//! Paying a Lightning invoice with a card (`POST /v1/pay`): the till charges the card through the
//! charge path (PIN, `client_ref` idempotency, ops, the card's daily limit; never offline), then
//! pays the invoice from its own wallet with what it took ([`Wallet::pay_invoice`]).
//!
//! `pays.json` is the write-ahead record of every pay (written like `sales.json`: 0600, temp
//! file, rename). Each state is saved before the step it guards:
//!
//! - `charging`: saved before the card is charged. The charge carries the `client_ref`
//!   `pay:<client_ref>`, so its sale (`sales.json`) or its unresolved transaction (card state)
//!   says afterwards whether the card paid.
//! - `not_charged`: the charge was refused and left nothing behind. A retry with the same
//!   `client_ref` starts it again.
//! - `charged`: the card paid the till; no payment has started.
//! - `paying`: saved before the payment starts; `operation_id` once the wallet named it.
//! - `paid`: the preimage and the gateway's fee.
//! - `pay_failed`: certain the invoice was not paid. The value stays in the till's wallet, owed to
//!   the holder.
//!
//! A record left `charging`, `charged` or `paying` (a crash, a payment still in flight, a till
//! wallet not yet holding the amount) is driven on by the sweeper ([`sweep`]): a `charging` one is
//! settled by what the charge left, a `charged` or `paying` one first asks the wallet what it did
//! with the invoice ([`Wallet::pay_status`]) and pays only when it never funded a payment of it, and
//! a `paying` one with an operation id is followed until the federation settles it. Nothing is ever
//! paid twice, and nothing that could still be paid is called failed.
//!
//! The PIN, the invoice (it carries the payment secret) and preimages are never logged; the
//! preimage is answered only for a paid pay.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::HeaderMap;
use fedimint_card_host_card::{PIN_MAX, PIN_MIN};
use futures::future::BoxFuture;
use lightning_invoice::Currency;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::link::Link;
use crate::state::write_private_json;
use crate::till::{MAX_PRICE_MSAT, Reply, TillHandle};
use crate::wallet::{Wallet, WalletPay, parse_bolt11};

pub const PAYS_FILE: &str = "pays.json";
const PAYS_VERSION: u32 = 1;
/// The card's charge for a pay carries `pay:<client_ref>`.
pub const CHARGE_REF_PREFIX: &str = "pay:";
/// Longest `client_ref` of a pay: its charge's `client_ref` must fit the charge's limit.
pub const MAX_CLIENT_REF: usize = crate::till::MAX_CLIENT_REF - CHARGE_REF_PREFIX.len();
/// How often the sweeper drives the records left open.
pub const SWEEP_EVERY: Duration = Duration::from_secs(30);
/// How long the sweeper follows one payment in flight before it moves on.
pub const SWEEP_WAIT: Duration = Duration::from_secs(10);
/// How long a look at what the wallet already did with an invoice follows a payment in flight.
const STATUS_WAIT: Duration = Duration::from_secs(5);
const DAY_MS: u64 = 86_400_000;
/// Most pays `GET /v1/pays` lists, newest last.
pub const LIST_MAX: usize = 200;

/// The charge `client_ref` of the pay `client_ref`.
pub fn charge_ref(client_ref: &str) -> String {
    format!("{CHARGE_REF_PREFIX}{client_ref}")
}

// ------------------------------------------------------------------ settings

/// The till's settings for paying invoices (`till serve --pay-…`, `TILL_PAY_…`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayConfig {
    /// `POST /v1/pay` is refused unless set. The records already there are resolved either way.
    pub enabled: bool,
    /// Largest invoice paid, msat.
    pub max_invoice_msat: u64,
    /// The cap on the fee reserve: a caller's `max_fee_msat` above it is lowered to it.
    pub max_fee_msat: u64,
    /// The till's margin on each pay: this many msat ...
    pub margin_msat: u64,
    /// ... plus this many basis points of the invoice.
    pub margin_bps: u64,
    /// Most value the till charges for pays in any 24 hours (pays not charged or failed do not
    /// count).
    pub daily_cap_msat: u64,
    /// Least time an invoice must have left before it expires, seconds.
    pub min_expiry_secs: u64,
    /// How long `POST /v1/pay` follows the payment before it answers `undecided`.
    pub wait: Duration,
}

impl PayConfig {
    pub const DEFAULT_MAX_INVOICE_MSAT: u64 = 100_000_000;
    pub const DEFAULT_MAX_FEE_MSAT: u64 = 1_000_000;
    pub const DEFAULT_DAILY_CAP_MSAT: u64 = 1_000_000_000;
    pub const DEFAULT_MIN_EXPIRY_SECS: u64 = 120;
    pub const DEFAULT_WAIT: Duration = Duration::from_secs(60);

    /// The fee reserve for a caller's `max_fee_msat`: the till's cap when it asks for none or more.
    pub fn fee_reserve(&self, asked: Option<u64>) -> u64 {
        asked.map_or(self.max_fee_msat, |a| a.min(self.max_fee_msat))
    }

    /// The till's margin on an invoice of `invoice_msat`.
    pub fn margin(&self, invoice_msat: u64) -> u64 {
        let bps = (invoice_msat as u128 * self.margin_bps as u128 / 10_000).min(u64::MAX as u128) as u64;
        self.margin_msat.saturating_add(bps)
    }
}

impl Default for PayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_invoice_msat: Self::DEFAULT_MAX_INVOICE_MSAT,
            max_fee_msat: Self::DEFAULT_MAX_FEE_MSAT,
            margin_msat: 0,
            margin_bps: 0,
            daily_cap_msat: Self::DEFAULT_DAILY_CAP_MSAT,
            min_expiry_secs: Self::DEFAULT_MIN_EXPIRY_SECS,
            wait: Self::DEFAULT_WAIT,
        }
    }
}

// ------------------------------------------------------------------ the record

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayState {
    Charging,
    NotCharged,
    Charged,
    Paying,
    Paid,
    PayFailed,
}

impl PayState {
    pub fn name(self) -> &'static str {
        match self {
            PayState::Charging => "charging",
            PayState::NotCharged => "not_charged",
            PayState::Charged => "charged",
            PayState::Paying => "paying",
            PayState::Paid => "paid",
            PayState::PayFailed => "pay_failed",
        }
    }

    /// Still to be driven: the card may have paid, and the invoice is not settled.
    pub fn is_open(self) -> bool {
        matches!(self, PayState::Charging | PayState::Charged | PayState::Paying)
    }

    /// Counts toward the daily cap: the card paid (or may have) and the value is not owed back.
    fn counts_to_cap(self) -> bool {
        self.is_open() || self == PayState::Paid
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayRec {
    pub pay_id: u64,
    pub client_ref: String,
    /// The invoice, as it is paid. It carries the payment secret: never in an answer or a log.
    pub bolt11: String,
    /// Hex.
    pub payment_hash: String,
    pub invoice_msat: u64,
    /// The most the gateway may take; what it does not take stays in the till's wallet.
    pub fee_reserve_msat: u64,
    pub margin_msat: u64,
    /// What the card is charged: invoice, fee reserve and margin.
    pub price_msat: u64,
    pub invoice_expires_at_ms: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub state: PayState,
    #[serde(default)]
    pub sale_id: Option<u64>,
    #[serde(default)]
    pub txid: Option<String>,
    /// The wallet's payment, hex: what resolves an unknown outcome.
    #[serde(default)]
    pub operation_id: Option<String>,
    #[serde(default)]
    pub preimage: Option<String>,
    #[serde(default)]
    pub fee_paid_msat: Option<u64>,
    #[serde(default)]
    pub detail: Option<String>,
}

impl PayRec {
    /// The API's view. No invoice and no operation id; the preimage only of a paid pay, and only
    /// when `preimage` is set.
    pub fn to_json(&self, preimage: bool) -> Value {
        let mut v = json!({
            "ok": true,
            "pay_id": self.pay_id,
            "client_ref": self.client_ref,
            "state": self.state.name(),
            "payment_hash": self.payment_hash,
            "invoice_msat": self.invoice_msat,
            "fee_reserve_msat": self.fee_reserve_msat,
            "margin_msat": self.margin_msat,
            "price_msat": self.price_msat,
            "sale_id": self.sale_id,
            "created_at_ms": self.created_at_ms,
            "updated_at_ms": self.updated_at_ms,
            "invoice_expires_at_ms": self.invoice_expires_at_ms,
        });
        if self.state == PayState::Paid {
            let fee = self.fee_paid_msat.unwrap_or(0);
            v["fee_paid_msat"] = json!(fee);
            v["fee_refund_msat"] = json!(self.fee_reserve_msat.saturating_sub(fee));
            if preimage {
                v["preimage"] = json!(self.preimage);
            }
        }
        if let Some(t) = &self.txid {
            v["txid"] = json!(t);
        }
        if let Some(d) = &self.detail {
            v["detail"] = json!(d);
        }
        v
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PayFile {
    version: u32,
    next_id: u64,
    pays: Vec<PayRec>,
}

impl Default for PayFile {
    fn default() -> Self {
        Self { version: PAYS_VERSION, next_id: 1, pays: vec![] }
    }
}

/// `pays.json`. A change is saved before it counts: when a save fails, memory is left as the file
/// is, and the caller decides ([`Self::put`], [`Self::keep`]).
pub struct PayStore {
    path: PathBuf,
    file: PayFile,
}

impl PayStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let file = match std::fs::read(&path) {
            Ok(b) => {
                let f: PayFile = serde_json::from_slice(&b).with_context(|| format!("parsing {}", path.display()))?;
                if f.version > PAYS_VERSION {
                    bail!("{} has version {}, this build understands {PAYS_VERSION}", path.display(), f.version);
                }
                f
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => PayFile::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path, file })
    }

    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join(PAYS_FILE)
    }

    pub fn get(&self, id: u64) -> Option<&PayRec> {
        self.file.pays.iter().find(|p| p.pay_id == id)
    }

    pub fn by_ref(&self, client_ref: &str) -> Option<&PayRec> {
        self.file.pays.iter().find(|p| p.client_ref == client_ref)
    }

    pub fn all(&self) -> &[PayRec] {
        &self.file.pays
    }

    /// Record a new pay under the next id and save it; nothing is kept when the save fails.
    pub fn insert(&mut self, mut rec: PayRec) -> Result<PayRec> {
        rec.pay_id = self.file.next_id;
        self.file.next_id += 1;
        self.file.pays.push(rec.clone());
        if let Err(e) = self.save() {
            self.file.pays.pop();
            self.file.next_id -= 1;
            return Err(e);
        }
        Ok(rec)
    }

    /// Replace the record with `rec.pay_id` and save. When the save fails, the record stays as it
    /// was, in memory too.
    pub fn put(&mut self, rec: &PayRec) -> Result<()> {
        let Some(i) = self.file.pays.iter().position(|p| p.pay_id == rec.pay_id) else { bail!("no pay {}", rec.pay_id) };
        let old = std::mem::replace(&mut self.file.pays[i], rec.clone());
        if let Err(e) = self.save() {
            self.file.pays[i] = old;
            return Err(e);
        }
        Ok(())
    }

    /// Replace the record in memory only: an outcome that already happened, whose save failed.
    /// Restart finds the record as last saved and settles it again from the wallet.
    pub fn keep(&mut self, rec: &PayRec) {
        if let Some(p) = self.file.pays.iter_mut().find(|p| p.pay_id == rec.pay_id) {
            *p = rec.clone();
        }
    }

    /// What pays created since `since_ms` charged, or may have, and have not failed.
    pub fn charged_since(&self, since_ms: u64) -> u64 {
        self.file.pays.iter().filter(|p| p.created_at_ms >= since_ms && p.state.counts_to_cap()).map(|p| p.price_msat).sum()
    }

    fn save(&self) -> Result<()> {
        write_private_json(&self.path, &self.file)
    }
}

// ------------------------------------------------------------------ the Lightning side

/// The wallet's side of a pay, so tests can stand in for a federation and a gateway. Each call
/// promises what its [`Wallet`] method promises.
pub trait PayLightning: Send + Sync {
    /// [`Wallet::pay_invoice`].
    fn pay(&self, bolt11: String, max_fee_msat: u64, wait: Duration) -> BoxFuture<'_, Result<WalletPay>>;
    /// [`Wallet::resolve_pay`].
    fn resolve(&self, operation_id: String, wait: Duration) -> BoxFuture<'_, Result<WalletPay>>;
    /// [`Wallet::pay_status`]: `None` when the wallet never funded a payment of the invoice.
    fn status(&self, bolt11: String, wait: Duration) -> BoxFuture<'_, Result<Option<WalletPay>>>;
}

/// The till's own wallet.
pub struct WalletPayer {
    pub wallet: Wallet,
}

impl PayLightning for WalletPayer {
    fn pay(&self, bolt11: String, max_fee_msat: u64, wait: Duration) -> BoxFuture<'_, Result<WalletPay>> {
        Box::pin(async move { self.wallet.pay_invoice(&bolt11, max_fee_msat, wait).await })
    }

    fn resolve(&self, operation_id: String, wait: Duration) -> BoxFuture<'_, Result<WalletPay>> {
        Box::pin(async move { self.wallet.resolve_pay(&operation_id, wait).await })
    }

    fn status(&self, bolt11: String, wait: Duration) -> BoxFuture<'_, Result<Option<WalletPay>>> {
        Box::pin(async move { self.wallet.pay_status(&bolt11, wait).await })
    }
}

// ------------------------------------------------------------------ the invoice

/// What the till takes from an invoice it accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvoiceFacts {
    pub bolt11: String,
    pub payment_hash: String,
    pub amount_msat: u64,
    pub expires_at_ms: u64,
}

/// Step 1, before the card is touched: the invoice decodes, has an amount within the till's
/// limit, is for `currency`, and has at least `cfg.min_expiry_secs` left at `now_ms`.
pub fn check_invoice(text: &str, currency: &Currency, cfg: &PayConfig, now_ms: u64) -> Result<InvoiceFacts, Reply> {
    let invoice = parse_bolt11(text).map_err(|e| Reply::fail(400, "bad_invoice", format!("{e:#}")))?;
    let amount_msat = invoice.amount_milli_satoshis().unwrap_or(0);
    if amount_msat == 0 {
        return Err(Reply::fail(400, "bad_invoice", "the invoice is for 0; ask for one with an amount"));
    }
    if invoice.currency() != *currency {
        return Err(Reply::fail(
            400,
            "wrong_network",
            format!("the invoice is for {:?}; this till pays {currency:?} invoices", invoice.currency()),
        ));
    }
    let expires_at_ms = invoice.expires_at().map_or(u64::MAX, |d| d.as_millis().min(u64::MAX as u128) as u64);
    if expires_at_ms <= now_ms {
        return Err(Reply::fail(400, "invoice_expired", "the invoice has expired; ask for a new one"));
    }
    if expires_at_ms - now_ms < cfg.min_expiry_secs.saturating_mul(1000) {
        return Err(Reply::fail(
            400,
            "invoice_expired",
            format!("the invoice expires in less than {} s; ask for a new one", cfg.min_expiry_secs),
        ));
    }
    if amount_msat > cfg.max_invoice_msat {
        return Err(Reply::fail(
            400,
            "over_limit",
            format!("the invoice is for {amount_msat} msat; this till pays at most {} msat", cfg.max_invoice_msat),
        ));
    }
    Ok(InvoiceFacts { bolt11: invoice.to_string(), payment_hash: invoice.payment_hash().to_string(), amount_msat, expires_at_ms })
}

// ------------------------------------------------------------------ the pays

/// Where a test stops the flow, as a crash there would.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayCrash {
    /// `charging` saved; the card not charged yet.
    AfterRecord,
    /// The card charged; `charged` not saved.
    AfterCharge,
    /// `charged` saved; `paying` not.
    AfterCharged,
    /// `paying` saved; the payment not started.
    AfterPaying,
    /// The payment answered; its outcome not saved.
    AfterPay,
}

/// The till's pays: the store, the wallet's side, the settings, shared by the handlers and the
/// sweeper.
pub struct Pays {
    pub store: Mutex<PayStore>,
    pub ln: Arc<dyn PayLightning>,
    pub cfg: PayConfig,
    /// The invoices this till pays.
    pub currency: Currency,
    /// Serialises the admission of pays, so a repeated `client_ref` or invoice never makes two.
    admit: tokio::sync::Mutex<()>,
    /// The pays being driven right now, by a request or the sweeper.
    driving: Mutex<HashSet<u64>>,
    #[doc(hidden)]
    pub crash: Mutex<Option<PayCrash>>,
}

/// One pay being driven; dropping it lets another driver take the pay.
struct Driving {
    pays: Arc<Pays>,
    id: u64,
}

impl Drop for Driving {
    fn drop(&mut self) {
        self.pays.driving.lock().unwrap().remove(&self.id);
    }
}

/// The card a pay is charged with: the holder's PIN and the POS phone's link, for one charge.
struct Tap {
    pin: Option<Vec<u8>>,
    link: Option<Link>,
}

impl Drop for Tap {
    fn drop(&mut self) {
        // The link lives for one job.
        if let Some(l) = &self.link {
            l.close();
        }
    }
}

impl Pays {
    pub fn new(store: PayStore, ln: Arc<dyn PayLightning>, cfg: PayConfig, currency: Currency) -> Arc<Self> {
        Arc::new(Self {
            store: Mutex::new(store),
            ln,
            cfg,
            currency,
            admit: tokio::sync::Mutex::new(()),
            driving: Mutex::new(HashSet::new()),
            crash: Mutex::new(None),
        })
    }

    /// The pays of a till whose wallet can pay invoices (`pays.json` in `dir`); `None` without a
    /// Lightning module.
    pub async fn for_wallet(dir: &Path, wallet: &Wallet, cfg: PayConfig) -> Result<Option<Arc<Self>>> {
        let Some(currency) = wallet.invoice_currency().await else { return Ok(None) };
        let store = PayStore::open(PayStore::path_in(dir))?;
        Ok(Some(Self::new(store, Arc::new(WalletPayer { wallet: wallet.clone() }), cfg, currency)))
    }

    pub fn get(&self, id: u64) -> Option<PayRec> {
        self.store.lock().unwrap().get(id).cloned()
    }

    /// Take `id` to drive it; `None` while someone else drives it.
    fn take(self: &Arc<Self>, id: u64) -> Option<Driving> {
        self.driving.lock().unwrap().insert(id).then(|| Driving { pays: self.clone(), id })
    }

    fn is_driven(&self, id: u64) -> bool {
        self.driving.lock().unwrap().contains(&id)
    }

    fn crash_at(&self, at: PayCrash) -> Option<Reply> {
        let mut c = self.crash.lock().unwrap();
        (*c == Some(at)).then(|| {
            *c = None;
            Reply::fail(500, "internal", format!("crash point {at:?}"))
        })
    }

    /// Save a step before it is taken: on failure the step must not be taken.
    fn write_ahead(&self, rec: &mut PayRec) -> Result<()> {
        rec.updated_at_ms = now_ms();
        self.store.lock().unwrap().put(rec)
    }

    /// Save what already happened. When the save fails the record is kept in memory, so this
    /// process answers it; a restart settles it again from the wallet or the sales.
    fn record(&self, rec: &mut PayRec) {
        rec.updated_at_ms = now_ms();
        let mut s = self.store.lock().unwrap();
        if let Err(e) = s.put(rec) {
            tracing::warn!("pay {}: could not save {}: {e:#}", rec.pay_id, rec.state.name());
            s.keep(rec);
        }
    }
}

/// `pays.json`'s answer about whether the card paid for `rec`: from the sales and the card state.
enum Charge {
    /// The sale: id and transaction.
    Sold(u64, String),
    /// A charge with this ref is unresolved.
    Pending,
    /// No charge with this ref, or one for another price (a `/v1/charge` that took the ref).
    None,
}

fn charge_of(h: &TillHandle, rec: &PayRec) -> Charge {
    let cref = charge_ref(&rec.client_ref);
    if let Some(s) = h.sales().lock().unwrap().find_ref(&cref) {
        return if s.price_msat == rec.price_msat { Charge::Sold(s.id, s.txid.clone()) } else { Charge::None };
    }
    if h.has_pending_ref(&cref) { Charge::Pending } else { Charge::None }
}

/// An answer about `rec`, with the pay's fields beside the error.
fn about(mut r: Reply, rec: &PayRec) -> Reply {
    if let Some(b) = r.body.as_object_mut() {
        b.insert("pay_id".into(), json!(rec.pay_id));
        b.insert("state".into(), json!(rec.state.name()));
        b.insert("price_msat".into(), json!(rec.price_msat));
        b.insert("sale_id".into(), json!(rec.sale_id));
    }
    r
}

fn undecided(rec: &PayRec, detail: impl std::fmt::Display) -> Reply {
    about(Reply::fail(202, "undecided", detail), rec)
}

/// The answer for a pay that is over.
fn final_reply(rec: &PayRec) -> Reply {
    match rec.state {
        PayState::Paid => Reply::ok(rec.to_json(true)),
        PayState::PayFailed => about(
            Reply::fail(
                502,
                "pay_failed",
                format!(
                    "the invoice was not paid ({}); the card's {} msat stay with the till, owed to the holder",
                    rec.detail.as_deref().unwrap_or("the payment failed"),
                    rec.price_msat
                ),
            ),
            rec,
        ),
        PayState::NotCharged => {
            about(Reply::fail(409, "not_charged", rec.detail.clone().unwrap_or_else(|| "the card was not charged".into())), rec)
        }
        _ => {
            undecided(rec, rec.detail.clone().unwrap_or_else(|| "the pay is still being settled; nothing was charged or paid again".into()))
        }
    }
}

/// Drive pay `id` as far as it goes now, and answer where it stands. `tap` is the card for a
/// record still `charging`; without one (the sweeper) such a record is only settled by what an
/// earlier charge left.
async fn advance(h: &TillHandle, pays: &Arc<Pays>, id: u64, mut tap: Option<Tap>, wait: Duration) -> Reply {
    loop {
        let Some(mut rec) = pays.get(id) else { return unknown_pay() };
        match rec.state {
            PayState::Paid | PayState::PayFailed | PayState::NotCharged => return final_reply(&rec),
            PayState::Charging => match tap.take() {
                None => match charge_of(h, &rec) {
                    Charge::Sold(sale_id, txid) => {
                        rec.state = PayState::Charged;
                        rec.sale_id = Some(sale_id);
                        rec.txid = Some(txid);
                        rec.detail = None;
                        pays.record(&mut rec);
                    }
                    Charge::Pending => {
                        return undecided(&rec, "the card's charge is still being resolved; nothing was charged again");
                    }
                    Charge::None => {
                        rec.state = PayState::NotCharged;
                        rec.detail = Some("the card was not charged".into());
                        pays.record(&mut rec);
                        return final_reply(&rec);
                    }
                },
                Some(t) => {
                    if let Some(r) = pays.crash_at(PayCrash::AfterRecord) {
                        return r;
                    }
                    let (pin, link) = (t.pin.clone(), t.link.clone());
                    let r = h.charge_card(rec.price_msat, charge_ref(&rec.client_ref), pin, link, true).await;
                    drop(t);
                    if let Some(r) = pays.crash_at(PayCrash::AfterCharge) {
                        return r;
                    }
                    if let Some(r) = charged(h, pays, rec, r) {
                        return r;
                    }
                }
            },
            PayState::Charged => return pay_step(pays, rec, wait).await,
            PayState::Paying if rec.operation_id.is_none() => return pay_step(pays, rec, wait).await,
            PayState::Paying => {
                let op = rec.operation_id.clone().unwrap_or_default();
                return match pays.ln.resolve(op, wait).await {
                    Ok(o) => apply(pays, rec, o),
                    Err(e) => hold(pays, rec, format!("asking the federation about the payment: {e:#}")),
                };
            }
        }
    }
}

/// After the card's charge answered `r`: `None` once the record is `charged` (go on and pay),
/// else the answer.
fn charged(h: &TillHandle, pays: &Pays, mut rec: PayRec, r: Reply) -> Option<Reply> {
    let sold = match r.status {
        200 => Charge::Sold(r.body["sale_id"].as_u64().unwrap_or(0), r.body["txid"].as_str().unwrap_or_default().to_owned()),
        // Undecided: the charge layer resolves it; the pay waits for its sale.
        202 => Charge::Pending,
        // Refused. Whether it left a charge behind, the sales and the card state say.
        _ => charge_of(h, &rec),
    };
    match sold {
        Charge::Sold(sale_id, txid) => {
            rec.state = PayState::Charged;
            rec.sale_id = Some(sale_id);
            rec.txid = Some(txid);
            rec.detail = None;
            pays.record(&mut rec);
            None
        }
        Charge::Pending => {
            let detail = r.body["detail"].as_str().unwrap_or("the card's charge is still being resolved").to_owned();
            Some(undecided(&rec, format!("{detail}; the invoice is paid once the charge is settled")))
        }
        Charge::None => {
            rec.state = PayState::NotCharged;
            rec.detail = r.body["detail"].as_str().map(str::to_owned);
            pays.record(&mut rec);
            Some(about(r, &rec))
        }
    }
}

/// Pay `rec`'s invoice (`charged`, or `paying` before the wallet named an operation). First asks
/// the wallet what it already did with the invoice, so a payment a crash cut off is followed, not
/// started again.
async fn pay_step(pays: &Pays, mut rec: PayRec, wait: Duration) -> Reply {
    match pays.ln.status(rec.bolt11.clone(), STATUS_WAIT.min(wait)).await {
        Ok(Some(o)) => return apply(pays, rec, o),
        Ok(None) => {}
        Err(e) => return hold(pays, rec, format!("reading the wallet's payments of the invoice: {e:#}")),
    }
    // Nothing was funded: an invoice past its expiry can no longer be paid.
    if now_ms() >= rec.invoice_expires_at_ms {
        return fail(pays, rec, None, "the invoice expired before the till could pay it".into());
    }
    if let Some(r) = pays.crash_at(PayCrash::AfterCharged) {
        return r;
    }
    rec.state = PayState::Paying;
    rec.operation_id = None;
    rec.detail = None;
    if let Err(e) = pays.write_ahead(&mut rec) {
        tracing::warn!("pay {}: could not save paying: {e:#}", rec.pay_id);
        let rec = pays.get(rec.pay_id).unwrap_or(rec);
        return undecided(&rec, "the till could not record the payment before starting it, so it did not start; it tries again");
    }
    if let Some(r) = pays.crash_at(PayCrash::AfterPaying) {
        return r;
    }
    let outcome = pays.ln.pay(rec.bolt11.clone(), rec.fee_reserve_msat, wait).await;
    if let Some(r) = pays.crash_at(PayCrash::AfterPay) {
        return r;
    }
    match outcome {
        Ok(o) => apply(pays, rec, o),
        // Nothing was funded (`Wallet::pay_invoice`); the next pass asks the wallet first.
        Err(e) => hold(pays, rec, format!("the payment did not start: {e:#}")),
    }
}

/// Record what the wallet answered about `rec`'s payment, and answer it.
fn apply(pays: &Pays, mut rec: PayRec, outcome: WalletPay) -> Reply {
    match outcome {
        WalletPay::Paid { preimage, fee_msat, operation_id } => {
            rec.state = PayState::Paid;
            rec.preimage = Some(preimage);
            rec.fee_paid_msat = Some(fee_msat);
            rec.operation_id = Some(operation_id);
            rec.detail = None;
            pays.record(&mut rec);
            tracing::info!("pay {}: paid, gateway fee {fee_msat} msat", rec.pay_id);
            final_reply(&rec)
        }
        // Nothing was funded: the till's wallet does not hold the amount yet (the charge's notes
        // may still be being collected). Not a failure while the invoice can still be paid.
        WalletPay::Failed { reason, operation_id: None, insufficient_funds: true } => {
            if now_ms() >= rec.invoice_expires_at_ms {
                return fail(pays, rec, None, format!("the invoice expired before the till's wallet could pay it ({reason})"));
            }
            rec.state = PayState::Charged;
            rec.operation_id = None;
            rec.detail = Some(format!("the till's wallet cannot pay the invoice yet: {reason}"));
            pays.record(&mut rec);
            undecided(&rec, "the till's wallet does not hold the amount yet; it pays the invoice once it does")
        }
        WalletPay::Failed { reason, operation_id, .. } => fail(pays, rec, operation_id, reason),
        WalletPay::Unknown { reason, operation_id } => {
            rec.state = PayState::Paying;
            rec.operation_id = Some(operation_id);
            rec.detail = Some(reason);
            pays.record(&mut rec);
            undecided(&rec, "the payment is in flight; the till follows it until it settles, and nothing is paid again")
        }
    }
}

fn fail(pays: &Pays, mut rec: PayRec, operation_id: Option<String>, reason: String) -> Reply {
    rec.state = PayState::PayFailed;
    if operation_id.is_some() {
        rec.operation_id = operation_id;
    }
    rec.detail = Some(reason);
    pays.record(&mut rec);
    tracing::warn!("pay {}: the invoice was not paid; {} msat owed to the holder", rec.pay_id, rec.price_msat);
    final_reply(&rec)
}

/// Leave `rec` in its state, with `detail`, for the next pass.
fn hold(pays: &Pays, mut rec: PayRec, detail: String) -> Reply {
    tracing::warn!("pay {}: {detail}", rec.pay_id);
    rec.detail = Some(detail);
    pays.record(&mut rec);
    undecided(&rec, "the pay is not settled yet; the till goes on with it, and nothing is charged or paid twice")
}

/// One pass over the pays left open: each one not being driven is driven as far as it goes.
pub async fn sweep(h: &TillHandle) {
    let Some(pays) = h.pays() else { return };
    let open: Vec<u64> = pays.store.lock().unwrap().all().iter().filter(|p| p.state.is_open()).map(|p| p.pay_id).collect();
    for id in open {
        // Taken under the admission lock: a request that has just recorded a pay takes it before
        // letting go of that lock, so the sweeper never settles a `charging` record whose card is
        // about to be charged.
        let driving = {
            let _admit = pays.admit.lock().await;
            pays.take(id)
        };
        let Some(_driving) = driving else { continue };
        let r = advance(h, &pays, id, None, SWEEP_WAIT).await;
        if r.status != 200 && r.status != 202 {
            tracing::warn!("pay {id}: {}", r.body["error"]);
        }
    }
}

/// [`sweep`] now and every [`SWEEP_EVERY`]. The task holds a handle: abort it when the server
/// stops, or the till's actor never ends.
pub fn spawn_sweeper(h: TillHandle) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            sweep(&h).await;
            tokio::time::sleep(SWEEP_EVERY).await;
        }
    })
}

// ------------------------------------------------------------------ HTTP

#[derive(Deserialize)]
struct PayReq {
    bolt11: Option<String>,
    max_fee_msat: Option<u64>,
    client_ref: Option<String>,
    /// The holder's PIN, typed at the till. Never logged, stored or echoed.
    pin: Option<String>,
}

fn unavailable() -> Reply {
    Reply::fail(503, "lightning_unavailable", "this till pays no invoices (the federation has no Lightning module it can use)")
}

fn unknown_pay() -> Reply {
    Reply::fail(404, "unknown_pay", "no such pay on this till")
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `POST /v1/pay`: charge the card for the invoice, then pay it.
pub async fn pay(State(h): State<TillHandle>, headers: HeaderMap, body: Bytes) -> Reply {
    let link = match crate::till::link_of(&h, &headers) {
        Ok(l) => l,
        Err(r) => return r,
    };
    // Closes the link on every answer that never reaches the card.
    let tap = Tap { pin: None, link };
    pay_on(&h, tap, body).await
}

async fn pay_on(h: &TillHandle, mut tap: Tap, body: Bytes) -> Reply {
    let Some(pays) = h.pays() else { return unavailable() };
    if !pays.cfg.enabled {
        return Reply::fail(503, "pay_disabled", "this till does not pay invoices (TILL_PAY_ENABLED is off)");
    }
    let req: PayReq = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return Reply::fail(400, "bad_request", format!("body must be JSON like {{\"bolt11\":\"…\",\"client_ref\":\"…\"}}: {e}"));
        }
    };
    let Some(client_ref) = req.client_ref.filter(|c| !c.is_empty() && c.len() <= MAX_CLIENT_REF) else {
        return Reply::fail(400, "bad_request", format!("client_ref is required, 1 to {MAX_CLIENT_REF} bytes"));
    };
    // Checked here, so a malformed PIN never reaches the card and costs no try. Never echoed.
    if req.pin.as_deref().is_some_and(|p| !(PIN_MIN..=PIN_MAX).contains(&p.len()) || !p.bytes().all(|b| b.is_ascii_digit())) {
        return Reply::fail(400, "bad_request", format!("pin must be {PIN_MIN} to {PIN_MAX} digits"));
    }
    let Some(bolt11) = req.bolt11 else { return Reply::fail(400, "bad_request", "bolt11 is required") };
    let now = now_ms();
    let inv = match check_invoice(&bolt11, &pays.currency, &pays.cfg, now) {
        Ok(i) => i,
        Err(r) => return r,
    };
    let fee_reserve_msat = pays.cfg.fee_reserve(req.max_fee_msat);
    let margin_msat = pays.cfg.margin(inv.amount_msat);
    let price_msat = match inv.amount_msat.checked_add(fee_reserve_msat).and_then(|p| p.checked_add(margin_msat)) {
        Some(p) if p <= MAX_PRICE_MSAT => p,
        _ => {
            return Reply::fail(
                400,
                "over_limit",
                format!("the invoice, its fee reserve and margin come to more than {MAX_PRICE_MSAT} msat"),
            );
        }
    };
    let (refused, pin_required) = {
        let conn = h.conn();
        let c = conn.lock().unwrap();
        (c.refusal(), c.ops.is_some())
    };
    if let Some(r) = refused {
        return r;
    }
    if pin_required && req.pin.is_none() {
        return Reply::fail(400, "pin_required", "this till charges only with the holder's PIN");
    }
    tap.pin = req.pin.map(String::into_bytes);

    let admit = pays.admit.lock().await;
    let existing = pays.store.lock().unwrap().by_ref(&client_ref).cloned();
    let id = match existing {
        Some(rec) => {
            if rec.payment_hash != inv.payment_hash || rec.fee_reserve_msat != fee_reserve_msat || rec.price_msat != price_msat {
                return Reply::fail(
                    409,
                    "conflict",
                    format!("client_ref {client_ref:?} was used for pay {} of another invoice or price", rec.pay_id),
                );
            }
            match rec.state {
                PayState::Paid | PayState::PayFailed => return final_reply(&rec),
                _ if pays.is_driven(rec.pay_id) => {
                    return undecided(&rec, "an earlier attempt with this client_ref is in flight; nothing was charged or paid again");
                }
                PayState::NotCharged => {
                    if let Some(r) = refuse_new(h, &pays, &rec.client_ref, &inv, rec.price_msat, Some(rec.pay_id), now).await {
                        return r;
                    }
                    let mut again = rec.clone();
                    again.state = PayState::Charging;
                    again.detail = None;
                    if let Err(e) = pays.write_ahead(&mut again) {
                        return Reply::fail(500, "internal", format!("could not record the pay, so the card was not charged: {e:#}"));
                    }
                    rec.pay_id
                }
                _ => rec.pay_id,
            }
        }
        None => {
            if let Some(r) = refuse_new(h, &pays, &client_ref, &inv, price_msat, None, now).await {
                return r;
            }
            let rec = PayRec {
                pay_id: 0,
                client_ref,
                bolt11: inv.bolt11.clone(),
                payment_hash: inv.payment_hash.clone(),
                invoice_msat: inv.amount_msat,
                fee_reserve_msat,
                margin_msat,
                price_msat,
                invoice_expires_at_ms: inv.expires_at_ms,
                created_at_ms: now,
                updated_at_ms: now,
                state: PayState::Charging,
                sale_id: None,
                txid: None,
                operation_id: None,
                preimage: None,
                fee_paid_msat: None,
                detail: None,
            };
            match pays.store.lock().unwrap().insert(rec) {
                Ok(r) => r.pay_id,
                Err(e) => return Reply::fail(500, "internal", format!("could not record the pay, so the card was not charged: {e:#}")),
            }
        }
    };
    let Some(driving) = pays.take(id) else {
        let rec = pays.get(id);
        return rec.map_or_else(unknown_pay, |r| undecided(&r, "an earlier attempt with this client_ref is in flight"));
    };
    drop(admit);
    // Its own task: a client that goes away mid-pay cannot stop it between the charge and the
    // payment.
    let (h, pays, wait) = (h.clone(), pays.clone(), pays.cfg.wait);
    let task = tokio::spawn(async move {
        let _driving = driving;
        advance(&h, &pays, id, Some(tap), wait).await
    });
    task.await.unwrap_or_else(|e| Reply::fail(500, "internal", format!("the pay stopped: {e}")))
}

/// Why a pay of `inv` for `client_ref` may not start (a new one, or `pay_id` again after its card
/// was not charged): the invoice is another pay's or already paid by this wallet, the ref is taken
/// by a charge, or the daily cap is reached.
async fn refuse_new(
    h: &TillHandle,
    pays: &Pays,
    client_ref: &str,
    inv: &InvoiceFacts,
    price_msat: u64,
    pay_id: Option<u64>,
    now: u64,
) -> Option<Reply> {
    {
        let s = pays.store.lock().unwrap();
        if let Some(other) =
            s.all().iter().find(|p| Some(p.pay_id) != pay_id && p.payment_hash == inv.payment_hash && p.state != PayState::NotCharged)
        {
            return Some(Reply::fail(409, "invoice_in_use", format!("pay {} is for this invoice", other.pay_id)));
        }
        let charged = s.charged_since(now.saturating_sub(DAY_MS));
        if charged.saturating_add(price_msat) > pays.cfg.daily_cap_msat {
            return Some(Reply::fail(
                403,
                "daily_cap",
                format!(
                    "this till pays at most {} msat of invoices in 24 hours and {charged} msat are taken; nothing was charged",
                    pays.cfg.daily_cap_msat
                ),
            ));
        }
    }
    let cref = charge_ref(client_ref);
    if h.sales().lock().unwrap().find_ref(&cref).is_some() || h.has_pending_ref(&cref) {
        return Some(Reply::fail(409, "conflict", format!("client_ref {cref:?} is taken by a charge that is not this pay")));
    }
    // An invoice this wallet already paid (or is paying) outside this pay: charging the card for it
    // would take the holder's money for nothing.
    match pays.ln.status(inv.bolt11.clone(), Duration::ZERO).await {
        Ok(None) => None,
        Ok(Some(_)) => Some(Reply::fail(409, "invoice_in_use", "this till's wallet already paid this invoice, or is paying it")),
        Err(e) => Some(Reply::fail(502, "lightning_error", format!("reading the wallet's payments of the invoice: {e:#}"))),
    }
}

/// `GET /v1/pay/{pay_id}`.
pub async fn pay_get(State(h): State<TillHandle>, UrlPath(id): UrlPath<String>) -> Reply {
    let Some(pays) = h.pays() else { return unavailable() };
    match id.parse::<u64>().ok().and_then(|id| pays.get(id)) {
        Some(rec) => Reply::ok(rec.to_json(true)),
        None => unknown_pay(),
    }
}

/// `GET /v1/pays`: the newest [`LIST_MAX`] pays, oldest first; no preimages.
pub async fn pays_list(State(h): State<TillHandle>) -> Reply {
    let Some(pays) = h.pays() else { return unavailable() };
    let s = pays.store.lock().unwrap();
    let all = s.all();
    let list: Vec<Value> = all[all.len().saturating_sub(LIST_MAX)..].iter().map(|p| p.to_json(false)).collect();
    let paid = || all.iter().filter(|p| p.state == PayState::Paid);
    Reply::ok(json!({
        "count": all.len(),
        "total_price_msat": paid().map(|p| p.price_msat).sum::<u64>(),
        "total_invoice_msat": paid().map(|p| p.invoice_msat).sum::<u64>(),
        "pays": list,
    }))
}

#[cfg(test)]
mod tests {
    use fedimint_core::bitcoin::hashes::{Hash as _, sha256};
    use fedimint_core::secp256k1::{SECP256K1, SecretKey};
    use lightning_invoice::{InvoiceBuilder, PaymentSecret};

    use super::*;

    fn invoice(currency: Currency, msat: u64, expiry: Duration) -> String {
        let key = SecretKey::from_slice(&[7u8; 32]).unwrap();
        InvoiceBuilder::new(currency)
            .description("pay test".into())
            .payment_hash(sha256::Hash::hash(&rand::random::<[u8; 32]>()))
            .payment_secret(PaymentSecret([42u8; 32]))
            .current_timestamp()
            .min_final_cltv_expiry_delta(144)
            .expiry_time(expiry)
            .amount_milli_satoshis(msat)
            .build_signed(|m| SECP256K1.sign_ecdsa_recoverable(m, &key))
            .unwrap()
            .to_string()
    }

    fn rec(id: u64, state: PayState, created: u64, price: u64) -> PayRec {
        PayRec {
            pay_id: id,
            client_ref: format!("r{id}-{created}-{price}"),
            bolt11: "lnbcrt1".into(),
            payment_hash: format!("{:064x}", created ^ (price << 20)),
            invoice_msat: price,
            fee_reserve_msat: 0,
            margin_msat: 0,
            price_msat: price,
            invoice_expires_at_ms: u64::MAX,
            created_at_ms: created,
            updated_at_ms: created,
            state,
            sale_id: None,
            txid: None,
            operation_id: None,
            preimage: None,
            fee_paid_msat: None,
            detail: None,
        }
    }

    #[test]
    fn an_invoice_is_checked_before_anything_else() {
        let cfg = PayConfig { max_invoice_msat: 50_000, ..PayConfig::default() };
        let now = now_ms();
        let ok = invoice(Currency::Regtest, 10_000, Duration::from_secs(3600));
        let facts = check_invoice(&format!("lightning:{ok}"), &Currency::Regtest, &cfg, now).unwrap();
        assert_eq!(facts.amount_msat, 10_000);
        assert_eq!(facts.bolt11, ok);
        assert_eq!(facts.payment_hash.len(), 64);
        assert!(facts.expires_at_ms > now + 3_500_000);

        let code = |r: Result<InvoiceFacts, Reply>| {
            let r = r.unwrap_err();
            (r.status, r.body["error"].as_str().unwrap().to_owned())
        };
        assert_eq!(code(check_invoice("lnbc1nope", &Currency::Regtest, &cfg, now)), (400, "bad_invoice".into()));
        assert_eq!(code(check_invoice(&ok, &Currency::Bitcoin, &cfg, now)), (400, "wrong_network".into()));
        let big = invoice(Currency::Regtest, 50_001, Duration::from_secs(3600));
        assert_eq!(code(check_invoice(&big, &Currency::Regtest, &cfg, now)), (400, "over_limit".into()));
        let short = invoice(Currency::Regtest, 10_000, Duration::from_secs(119));
        assert_eq!(code(check_invoice(&short, &Currency::Regtest, &cfg, now)), (400, "invoice_expired".into()));
        assert_eq!(code(check_invoice(&ok, &Currency::Regtest, &cfg, now + 3_601_000)), (400, "invoice_expired".into()));
        // No amount.
        let key = SecretKey::from_slice(&[7u8; 32]).unwrap();
        let open = InvoiceBuilder::new(Currency::Regtest)
            .description("no amount".into())
            .payment_hash(sha256::Hash::hash(b"x"))
            .payment_secret(PaymentSecret([42u8; 32]))
            .current_timestamp()
            .min_final_cltv_expiry_delta(144)
            .build_signed(|m| SECP256K1.sign_ecdsa_recoverable(m, &key))
            .unwrap()
            .to_string();
        assert_eq!(code(check_invoice(&open, &Currency::Regtest, &cfg, now)), (400, "bad_invoice".into()));
    }

    #[test]
    fn the_fee_reserve_is_capped_and_the_margin_adds_up() {
        let cfg = PayConfig { max_fee_msat: 5_000, margin_msat: 100, margin_bps: 150, ..PayConfig::default() };
        assert_eq!(cfg.fee_reserve(None), 5_000);
        assert_eq!(cfg.fee_reserve(Some(2_000)), 2_000);
        assert_eq!(cfg.fee_reserve(Some(9_000)), 5_000);
        // 1.5 % of 1,000,000 msat is 15,000.
        assert_eq!(cfg.margin(1_000_000), 15_100);
        assert_eq!(PayConfig::default().margin(1_000_000), 0);
        assert_eq!(PayConfig { margin_bps: 10_000, ..PayConfig::default() }.margin(u64::MAX), u64::MAX);
    }

    #[test]
    fn the_store_saves_each_step_and_keeps_a_failed_save_out_of_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = PayStore::path_in(dir.path());
        let mut s = PayStore::open(&path).unwrap();
        let a = s.insert(rec(0, PayState::Charging, 1, 10)).unwrap();
        let b = s.insert(rec(0, PayState::Charging, 2, 20)).unwrap();
        assert_eq!((a.pay_id, b.pay_id), (1, 2));
        let mut paid = a.clone();
        paid.state = PayState::Paid;
        s.put(&paid).unwrap();
        let again = PayStore::open(&path).unwrap();
        assert_eq!(again.get(1).unwrap().state, PayState::Paid);
        assert_eq!(again.by_ref(&b.client_ref).unwrap().pay_id, 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }

        // A save that fails leaves memory as the file is; `keep` is for what already happened.
        let mut broken = PayStore { path: dir.path().join("missing").join("x").join(PAYS_FILE), file: again.file.clone() };
        std::fs::write(dir.path().join("missing"), b"a file, not a directory").unwrap();
        let mut failed = broken.get(2).unwrap().clone();
        failed.state = PayState::Paying;
        assert!(broken.put(&failed).is_err());
        assert_eq!(broken.get(2).unwrap().state, PayState::Charging);
        assert!(broken.insert(rec(0, PayState::Charging, 3, 30)).is_err());
        assert_eq!((broken.all().len(), broken.file.next_id), (2, 3));
        broken.keep(&failed);
        assert_eq!(broken.get(2).unwrap().state, PayState::Paying);
    }

    #[test]
    fn the_daily_cap_counts_what_the_cards_paid_and_was_not_owed_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = PayStore::open(PayStore::path_in(dir.path())).unwrap();
        for (state, created, price) in [
            (PayState::Paid, 1_000, 1),
            (PayState::Charging, 1_000, 10),
            (PayState::Charged, 1_000, 100),
            (PayState::Paying, 1_000, 1_000),
            (PayState::NotCharged, 1_000, 10_000),
            (PayState::PayFailed, 1_000, 100_000),
            (PayState::Paid, 999, 1_000_000),
        ] {
            s.insert(rec(0, state, created, price)).unwrap();
        }
        assert_eq!(s.charged_since(1_000), 1_111);
        assert_eq!(s.charged_since(0), 1_001_111);
    }

    #[test]
    fn a_record_answers_its_preimage_only_when_paid_and_asked() {
        let mut r = rec(3, PayState::Paying, 1, 1_000);
        r.fee_reserve_msat = 300;
        r.preimage = Some("ab".repeat(32));
        r.operation_id = Some("cd".repeat(32));
        let v = r.to_json(true);
        assert!(v.get("preimage").is_none() && v.get("fee_paid_msat").is_none(), "{v}");
        assert!(!v.to_string().contains(&r.bolt11) && !v.to_string().contains("cdcd"), "{v}");
        r.state = PayState::Paid;
        r.fee_paid_msat = Some(120);
        let v = r.to_json(true);
        assert_eq!((v["fee_paid_msat"].as_u64(), v["fee_refund_msat"].as_u64()), (Some(120), Some(180)));
        assert_eq!(v["preimage"], "ab".repeat(32));
        assert!(r.to_json(false).get("preimage").is_none());
        assert_eq!(final_reply(&r).status, 200);
        r.state = PayState::PayFailed;
        let f = final_reply(&r);
        assert_eq!((f.status, f.body["error"].as_str(), f.body["pay_id"].as_u64()), (502, Some("pay_failed"), Some(3)));
    }
}
