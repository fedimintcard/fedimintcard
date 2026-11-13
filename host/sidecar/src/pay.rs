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
//! - `pay_failed`: certain the invoice was not paid. The value the card paid the till
//!   (`received_msat`: the price and any overpay of the charge) stays in the till's wallet, owed
//!   to the holder.
//! - `refunding`: saved before a refund load ([`refund_job`], `POST /v1/pay/{id}/refund`): the
//!   value owed goes back onto the card that paid, as a holder load (docs/TOPUP.md). A card with
//!   too few free slots takes part of it; the pay is `pay_failed` again, owed the rest
//!   (`refund_owed_msat`), and the next tap loads more.
//! - `refunded`: the value is back on the card (`refund_msat`, `refund_txid`), all but less than
//!   the smallest note (`refund_dust_msat`), which stays with the till.
//!
//! Before the card is charged, the wallet quotes the gateway's fee for the invoice
//! ([`PayLightning::quote`]): a pay no gateway can carry now (`lightning_unavailable`), or whose fee
//! reserve is below the gateway's fee (`fee_reserve_too_low`), is refused with nothing charged.
//!
//! A record left `charging`, `charged` or `paying` (a crash, a payment still in flight, a till
//! wallet not yet holding the amount, no gateway answering for a moment) is driven on by the
//! sweeper ([`sweep`]): a `charging` one is settled by what the charge left, resolving a charge
//! still undecided without a card (`TillHandle::settle_charge`), a `charged` or `paying` one first
//! asks the wallet what it did with the invoice ([`Wallet::pay_status`]) and pays only when it
//! never funded a payment of it, and a `paying` one with an operation id is followed until the
//! federation settles it. Nothing is ever paid twice, and nothing that could still be paid is
//! called failed.
//!
//! A refund never loads more than the till holds for the holder, and each of its loads happens at
//! most once. Every load is tagged `pay-refund:<pay_id>` in the loader's journal and kept there
//! once finished ([`crate::holder::Loader::keep_finished`]), so a refund cut short anywhere is
//! settled by the next call from the journal, never loaded twice: a load still in the journal is
//! finished first, the kept loads the federation took are what is on the card (their sum), and
//! only what is still owed is loaded. Only the card that paid takes it (its short id, read at the
//! charge's tap before the card is charged, and kept on the charge's sale too).
//!
//! A repeated `client_ref` for a pay the card paid (or may have) answers the record, or drives it
//! on, without the checks that guard the card (the invoice's time left, the price, ops, a PIN): a
//! finished pay replays its answer, one still settling answers `undecided`.
//!
//! The PIN, the invoice (it carries the payment secret) and preimages are never logged; the
//! preimage is answered only to the caller of a paid pay, in the answer to its `POST` (and that
//! `POST`'s replay), never by `GET`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use axum::Extension;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::HeaderMap;
use fedimint_card_host_card::{PIN_MAX, PIN_MIN};
use futures::future::BoxFuture;
use lightning_invoice::Currency;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::glue::on_card;
use crate::holder::{CardFull, Loader};
use crate::link::Link;
use crate::ops::{OpsClient, short_id_of};
use crate::pos::PayScope;
use crate::state::write_private_json;
use crate::till::{MAX_PRICE_MSAT, Reply, TillHandle, error_reply};
use crate::topup::Topups;
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
/// Most notes one refund load puts on the card (and at most the card's free slots): what is still
/// owed is rounded down to this many, never up, and the rest goes on at the next tap.
pub const REFUND_MAX_NOTES: u32 = 7;
/// The loader's tag of a pay's refund: `pay-refund:<pay_id>`.
pub const REFUND_TAG_PREFIX: &str = "pay-refund:";

/// The loader's tag of pay `pay_id`'s refund.
pub fn refund_tag(pay_id: u64) -> String {
    format!("{REFUND_TAG_PREFIX}{pay_id}")
}

/// The pay whose refund a loader's tag names.
pub fn refund_of_tag(tag: &str) -> Option<u64> {
    tag.strip_prefix(REFUND_TAG_PREFIX)?.parse().ok()
}

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
    /// A `pay_failed` pay whose value is being loaded back onto the card.
    Refunding,
    /// A `pay_failed` pay whose value is back on the card.
    Refunded,
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
            PayState::Refunding => "refunding",
            PayState::Refunded => "refunded",
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
    /// The card that was charged (its short id, read at the charge's tap): the only card a
    /// refund goes to.
    #[serde(default)]
    pub short_id: Option<String>,
    /// What the refund's loads put back on the card so far (`pay_failed`, `refunded`), or will
    /// once the load in flight lands (`refunding`). Never more than the price.
    #[serde(default)]
    pub refund_msat: Option<u64>,
    /// The refund's last load.
    #[serde(default)]
    pub refund_txid: Option<String>,
    /// What the card paid the till, from the charge's sale: the price and any overpay the charge
    /// could not avoid (up to `MAX_TILL_OVERPAY`). Set once the pay is `charged`; a record from
    /// before it was kept counts the price.
    #[serde(default)]
    pub received_msat: Option<u64>,
}

/// What a charge's sale says the card paid the till: its price and overpay, or what the till
/// collected when that is more.
pub fn paid_to_till(price_msat: u64, overpaid_msat: u64, received_msat: u64) -> u64 {
    received_msat.max(price_msat.saturating_add(overpaid_msat))
}

impl PayRec {
    /// What the card paid the till for this pay: all of it is the holder's while the invoice is
    /// not paid.
    pub fn held_msat(&self) -> u64 {
        self.received_msat.unwrap_or(self.price_msat)
    }

    /// What the till still holds for the holder of a pay whose invoice was not paid.
    pub fn refund_owed_msat(&self) -> u64 {
        self.held_msat().saturating_sub(self.refund_msat.unwrap_or(0))
    }

    /// What a refunded pay left with the till: less than the smallest note the card could take.
    pub fn refund_dust_msat(&self) -> u64 {
        self.refund_owed_msat()
    }

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
        if let Some(c) = &self.short_id {
            v["short_id"] = json!(c);
        }
        // What the card paid the till, and the part of it above the price.
        if let Some(r) = self.received_msat {
            v["received_msat"] = json!(r);
            v["overpaid_msat"] = json!(r.saturating_sub(self.price_msat));
        }
        if matches!(self.state, PayState::Refunding | PayState::Refunded) || self.refund_msat.is_some_and(|m| m > 0) {
            v["refund_msat"] = json!(self.refund_msat.unwrap_or(0));
        }
        match self.state {
            // Owed to the holder: the next refund tap loads it.
            PayState::PayFailed => v["refund_owed_msat"] = json!(self.refund_owed_msat()),
            PayState::Refunded => v["refund_dust_msat"] = json!(self.refund_dust_msat()),
            _ => {}
        }
        if let Some(t) = &self.refund_txid {
            v["refund_txid"] = json!(t);
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
    /// [`Wallet::quote_invoice`]: the gateway's fee for the invoice now; an error when no gateway
    /// can pay it now.
    fn quote(&self, bolt11: String) -> BoxFuture<'_, Result<u64>>;
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

    fn quote(&self, bolt11: String) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move { self.wallet.quote_invoice(&bolt11).await })
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
    /// The sale: id, transaction, the card it charged when the sale recorded it, and what the
    /// card paid the till ([`paid_to_till`]).
    Sold(u64, String, Option<String>, u64),
    /// A charge with this ref is unresolved.
    Pending,
    /// No charge with this ref, or one for another price (a `/v1/charge` that took the ref).
    None,
}

fn charge_of(h: &TillHandle, rec: &PayRec) -> Charge {
    let cref = charge_ref(&rec.client_ref);
    if let Some(s) = h.sales().lock().unwrap().find_ref(&cref) {
        return if s.price_msat == rec.price_msat {
            Charge::Sold(s.id, s.txid.clone(), s.short_id.clone(), paid_to_till(s.price_msat, s.overpaid_msat, s.received_msat))
        } else {
            Charge::None
        };
    }
    if h.has_pending_ref(&cref) { Charge::Pending } else { Charge::None }
}

/// The card the sale of `rec`'s charge recorded, if it did.
fn sale_card(h: &TillHandle, rec: &PayRec) -> Option<String> {
    h.sales().lock().unwrap().find_ref(&charge_ref(&rec.client_ref)).and_then(|s| s.short_id.clone())
}

/// An answer about `rec`, with the pay's fields beside the error.
fn about(mut r: Reply, rec: &PayRec) -> Reply {
    if let Some(b) = r.body.as_object_mut() {
        b.insert("pay_id".into(), json!(rec.pay_id));
        b.insert("state".into(), json!(rec.state.name()));
        b.insert("price_msat".into(), json!(rec.price_msat));
        b.insert("sale_id".into(), json!(rec.sale_id));
        // The card a refund goes to, and what it is still owed.
        if let Some(c) = &rec.short_id {
            b.insert("short_id".into(), json!(c));
        }
        if let Some(r) = rec.received_msat {
            b.insert("received_msat".into(), json!(r));
        }
        if rec.state == PayState::PayFailed {
            b.insert("refund_owed_msat".into(), json!(rec.refund_owed_msat()));
            if let Some(m) = rec.refund_msat.filter(|m| *m > 0) {
                b.insert("refund_msat".into(), json!(m));
            }
        }
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
                    "the invoice was not paid ({}); {} msat of the card's {} stay with the till, owed to the holder",
                    rec.detail.as_deref().unwrap_or("the payment failed"),
                    rec.refund_owed_msat(),
                    rec.held_msat()
                ),
            ),
            rec,
        ),
        PayState::Refunding | PayState::Refunded => about(
            Reply::fail(
                502,
                "pay_failed",
                format!(
                    "the invoice was not paid ({}); {}",
                    rec.detail.as_deref().unwrap_or("the payment failed"),
                    if rec.state == PayState::Refunded {
                        "the value is back on the card"
                    } else {
                        "its refund to the card is not finished: refund again with the same card"
                    }
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
///
/// A charge still unresolved is settled here, once per call, without a card
/// ([`TillHandle::settle_charge`]): the sweeper (and a repeat of the pay) is what drives an
/// undecided charge to an end, whether a POS ever asks about it again or not.
async fn advance(h: &TillHandle, pays: &Arc<Pays>, id: u64, mut tap: Option<Tap>, wait: Duration) -> Reply {
    let mut settled = false;
    loop {
        let Some(mut rec) = pays.get(id) else { return unknown_pay() };
        match rec.state {
            PayState::Paid | PayState::PayFailed | PayState::NotCharged | PayState::Refunding | PayState::Refunded => {
                return final_reply(&rec);
            }
            PayState::Charging => match tap.take() {
                None => match charge_of(h, &rec) {
                    Charge::Sold(sale_id, txid, card, received) => {
                        rec.state = PayState::Charged;
                        rec.sale_id = Some(sale_id);
                        rec.txid = Some(txid);
                        rec.received_msat = Some(received);
                        // The card that paid, from its sale: a crash before the charge's answer
                        // was recorded loses no refund.
                        rec.short_id = rec.short_id.take().or(card);
                        rec.detail = None;
                        pays.record(&mut rec);
                    }
                    // Resolve it now, with no card and no new spend; then read what it left.
                    Charge::Pending if !settled => {
                        settled = true;
                        let r = h.settle_charge(rec.price_msat, charge_ref(&rec.client_ref)).await;
                        if r.status != 200 {
                            tracing::info!("pay {id}: settling its charge: {} {}", r.status, r.body["error"]);
                        }
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
    // The card that tapped: the one a refund goes back to.
    if let Some(c) = r.body["short_id"].as_str() {
        rec.short_id = Some(c.to_owned());
    }
    let sold = match r.status {
        200 => Charge::Sold(
            r.body["sale_id"].as_u64().unwrap_or(0),
            r.body["txid"].as_str().unwrap_or_default().to_owned(),
            sale_card(h, &rec),
            paid_to_till(
                r.body["price_msat"].as_u64().unwrap_or(rec.price_msat),
                r.body["overpaid_msat"].as_u64().unwrap_or(0),
                r.body["received_msat"].as_u64().unwrap_or(0),
            ),
        ),
        // Undecided: the charge layer resolves it; the pay waits for its sale.
        202 => Charge::Pending,
        // Refused. Whether it left a charge behind, the sales and the card state say.
        _ => charge_of(h, &rec),
    };
    match sold {
        Charge::Sold(sale_id, txid, card, received) => {
            // A replay settled without the card answers no short id; its sale may have it.
            rec.short_id = rec.short_id.take().or(card);
            rec.state = PayState::Charged;
            rec.sale_id = Some(sale_id);
            rec.txid = Some(txid);
            rec.received_msat = Some(received);
            rec.detail = None;
            pays.record(&mut rec);
            None
        }
        Charge::Pending => {
            if rec.short_id.is_some() {
                pays.record(&mut rec);
            }
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
        // may still be being collected), or no gateway answers for a moment. Not a failure while
        // the invoice can still be paid: the sweeper tries again until it expires.
        WalletPay::Failed { reason, operation_id: None, insufficient_funds, retryable } if insufficient_funds || retryable => {
            if now_ms() >= rec.invoice_expires_at_ms {
                return fail(pays, rec, None, format!("the invoice expired before the till could pay it ({reason})"));
            }
            rec.state = PayState::Charged;
            rec.operation_id = None;
            let (detail, answer) = if insufficient_funds {
                (
                    format!("the till's wallet cannot pay the invoice yet: {reason}"),
                    "the till's wallet does not hold the amount yet; it pays the invoice once it does",
                )
            } else {
                (
                    format!("no Lightning gateway can pay the invoice now: {reason}"),
                    "no Lightning gateway answers now; the till pays the invoice once one does, before it expires",
                )
            };
            rec.detail = Some(detail);
            pays.record(&mut rec);
            undecided(&rec, answer)
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
    // `pay_failed` is what lets the value go back to the holder: it is answered only once it is on
    // disk. Otherwise the record stays as last saved, and the till settles it again from the wallet.
    if let Err(e) = pays.write_ahead(&mut rec) {
        tracing::warn!("pay {}: could not save pay_failed: {e:#}", rec.pay_id);
        let saved = pays.get(rec.pay_id).unwrap_or(rec);
        return undecided(&saved, "the till could not record the failure; it settles the pay again, and nothing is charged or paid twice");
    }
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

/// A pay-scope caller names its card (`X-Card-Link`): without one a pay or a refund would go to
/// whatever card is on the till's own relay or reader, which is not the app's to charge.
fn card_link_required() -> Reply {
    Reply::fail(
        400,
        "card_link_required",
        "a pay token pays only with the card behind its own card link (X-Card-Link); nothing was charged or loaded",
    )
}

/// `POST /v1/pay`: charge the card for the invoice, then pay it.
pub async fn pay(State(h): State<TillHandle>, scope: Option<Extension<PayScope>>, headers: HeaderMap, body: Bytes) -> Reply {
    if scope.is_some() && !headers.contains_key(crate::till::LINK_HEADER) {
        return card_link_required();
    }
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
    // Only the payment hash for now: it says which pay a repeated `client_ref` names. A pay the
    // card already paid (or may have) replays without the checks below, which guard the card.
    let payment_hash = match parse_bolt11(&bolt11) {
        Ok(i) => i.payment_hash().to_string(),
        Err(e) => return Reply::fail(400, "bad_invoice", format!("{e:#}")),
    };

    let admit = pays.admit.lock().await;
    let mut existing = pays.store.lock().unwrap().by_ref(&client_ref).cloned();
    // A pay whose card was refused, or whose charge a crash cut off: the sales and the card state
    // say whether its charge exists after all (an undecided charge that resolved later). Then it is
    // this pay's charge: the pay goes on from it, never refused for it, never charged again.
    let mut charge_pending = false;
    if let Some(rec) = existing.as_mut()
        && matches!(rec.state, PayState::NotCharged | PayState::Charging)
        && !pays.is_driven(rec.pay_id)
    {
        match charge_of(h, rec) {
            Charge::Sold(sale_id, txid, card, received) => {
                rec.state = PayState::Charged;
                rec.sale_id = Some(sale_id);
                rec.txid = Some(txid);
                rec.received_msat = Some(received);
                rec.short_id = rec.short_id.take().or(card);
                rec.detail = None;
                pays.record(rec);
            }
            Charge::Pending => {
                if rec.state == PayState::NotCharged {
                    // Settled from the charge below ([`advance`]), as any `charging` record.
                    rec.state = PayState::Charging;
                    rec.detail = Some("the card's charge is still being resolved".into());
                    pays.record(rec);
                }
                charge_pending = true;
            }
            Charge::None => {}
        }
    }
    let replay = existing.as_ref().filter(|r| charge_pending || !matches!(r.state, PayState::Charging | PayState::NotCharged));
    let (id, tap) = if let Some(rec) = replay {
        // The card paid for this pay, or may have: answer it, or drive it on, whatever the
        // invoice's time left, the till's settings, ops or a PIN say now.
        let asked_other_fee =
            req.max_fee_msat.is_some_and(|a| a.min(rec.fee_reserve_msat.max(pays.cfg.max_fee_msat)) != rec.fee_reserve_msat);
        if rec.payment_hash != payment_hash || asked_other_fee {
            return conflict(&client_ref, rec);
        }
        match rec.state {
            PayState::Paid | PayState::PayFailed | PayState::Refunding | PayState::Refunded => return final_reply(rec),
            // A charge still unresolved is driven below without the card: settled, then paid (it
            // took the card's money) or `not_charged` (it left nothing).
            _ if pays.is_driven(rec.pay_id) => {
                return undecided(rec, "an earlier attempt with this client_ref is in flight; nothing was charged or paid again");
            }
            _ => {}
        }
        // No card is touched: the link closes now.
        drop(tap);
        (rec.pay_id, None)
    } else {
        // A new pay, or one whose card is still to be charged: every check that guards the card.
        let now = now_ms();
        let (inv, fee_reserve_msat, margin_msat, price_msat) =
            match card_checks(h, &pays, &bolt11, req.max_fee_msat, req.pin.is_some(), now) {
                Ok(c) => c,
                Err(r) => return r,
            };
        tap.pin = req.pin.map(String::into_bytes);
        let id = match existing {
            Some(rec) => {
                if rec.payment_hash != inv.payment_hash || rec.fee_reserve_msat != fee_reserve_msat || rec.price_msat != price_msat {
                    return conflict(&client_ref, &rec);
                }
                if pays.is_driven(rec.pay_id) {
                    return undecided(&rec, "an earlier attempt with this client_ref is in flight; nothing was charged or paid again");
                }
                if rec.state == PayState::NotCharged {
                    if let Some(r) =
                        refuse_new(h, &pays, &rec.client_ref, &inv, rec.fee_reserve_msat, rec.price_msat, Some(rec.pay_id), now).await
                    {
                        return r;
                    }
                    let mut again = rec.clone();
                    again.state = PayState::Charging;
                    again.detail = None;
                    // The charge is made now: the daily cap counts it from now.
                    again.created_at_ms = now;
                    if let Err(e) = pays.write_ahead(&mut again) {
                        return Reply::fail(500, "internal", format!("could not record the pay, so the card was not charged: {e:#}"));
                    }
                }
                rec.pay_id
            }
            None => {
                if let Some(r) = refuse_new(h, &pays, &client_ref, &inv, fee_reserve_msat, price_msat, None, now).await {
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
                    short_id: None,
                    refund_msat: None,
                    refund_txid: None,
                    received_msat: None,
                };
                match pays.store.lock().unwrap().insert(rec) {
                    Ok(r) => r.pay_id,
                    Err(e) => return Reply::fail(500, "internal", format!("could not record the pay, so the card was not charged: {e:#}")),
                }
            }
        };
        (id, Some(tap))
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
        advance(&h, &pays, id, tap, wait).await
    });
    task.await.unwrap_or_else(|e| Reply::fail(500, "internal", format!("the pay stopped: {e}")))
}

fn conflict(client_ref: &str, rec: &PayRec) -> Reply {
    Reply::fail(409, "conflict", format!("client_ref {client_ref:?} was used for pay {} of another invoice or price", rec.pay_id))
}

/// Every check that guards the card, run before a pay charges it (a new pay, or one whose card is
/// still to be charged): the invoice ([`check_invoice`]), the price, ops and the PIN. Answers the
/// invoice, the fee reserve, the margin and the price.
fn card_checks(
    h: &TillHandle,
    pays: &Pays,
    bolt11: &str,
    max_fee_msat: Option<u64>,
    has_pin: bool,
    now: u64,
) -> Result<(InvoiceFacts, u64, u64, u64), Reply> {
    let inv = check_invoice(bolt11, &pays.currency, &pays.cfg, now)?;
    let fee_reserve_msat = pays.cfg.fee_reserve(max_fee_msat);
    let margin_msat = pays.cfg.margin(inv.amount_msat);
    let price_msat = match inv.amount_msat.checked_add(fee_reserve_msat).and_then(|p| p.checked_add(margin_msat)) {
        Some(p) if p <= MAX_PRICE_MSAT => p,
        _ => {
            return Err(Reply::fail(
                400,
                "over_limit",
                format!("the invoice, its fee reserve and margin come to more than {MAX_PRICE_MSAT} msat"),
            ));
        }
    };
    let (refused, pin_required) = {
        let conn = h.conn();
        let c = conn.lock().unwrap();
        (c.refusal(), c.ops.is_some())
    };
    if let Some(r) = refused {
        return Err(r);
    }
    if pin_required && !has_pin {
        return Err(Reply::fail(400, "pin_required", "this till charges only with the holder's PIN"));
    }
    Ok((inv, fee_reserve_msat, margin_msat, price_msat))
}

/// Why a pay of `inv` for `client_ref` may not start (a new one, or `pay_id` again after its card
/// was not charged): the invoice is another pay's or already paid by this wallet, the ref is taken
/// by a charge, the daily cap is reached, no gateway can pay the invoice now, or the gateway's
/// fee is more than `fee_reserve_msat`. All of it is known before the card is charged.
#[allow(clippy::too_many_arguments)]
async fn refuse_new(
    h: &TillHandle,
    pays: &Pays,
    client_ref: &str,
    inv: &InvoiceFacts,
    fee_reserve_msat: u64,
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
        Ok(None) => {}
        Ok(Some(_)) => return Some(Reply::fail(409, "invoice_in_use", "this till's wallet already paid this invoice, or is paying it")),
        Err(e) => return Some(Reply::fail(502, "lightning_error", format!("reading the wallet's payments of the invoice: {e:#}"))),
    }
    // The gateway's fee, asked now: a pay that could never start is refused before the card pays.
    match pays.ln.quote(inv.bolt11.clone()).await {
        Ok(fee) if fee > fee_reserve_msat => {
            let ask = if fee <= pays.cfg.max_fee_msat {
                format!("ask again with max_fee_msat of at least {fee}")
            } else {
                format!("this till's fee reserve is at most {} msat", pays.cfg.max_fee_msat)
            };
            let mut r = Reply::fail(
                400,
                "fee_reserve_too_low",
                format!(
                    "the gateway asks {fee} msat to pay this invoice, more than the fee reserve of {fee_reserve_msat} msat; {ask}; nothing was charged"
                ),
            );
            r.body["gateway_fee_msat"] = json!(fee);
            Some(r)
        }
        Ok(_) => None,
        Err(e) => Some(Reply::fail(
            503,
            "lightning_unavailable",
            format!("no Lightning gateway can pay this invoice now ({e:#}); nothing was charged, try again later"),
        )),
    }
}

#[derive(Deserialize)]
pub struct PayGetQuery {
    client_ref: Option<String>,
}

/// `GET /v1/pay/{pay_id}`: the record, without its preimage. A pay-scope caller names the pay's
/// `client_ref` too (`?client_ref=`), so it reads only the pays it made: any other is 404.
pub async fn pay_get(
    State(h): State<TillHandle>,
    scope: Option<Extension<PayScope>>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<PayGetQuery>,
) -> Reply {
    let Some(pays) = h.pays() else { return unavailable() };
    match id.parse::<u64>().ok().and_then(|id| pays.get(id)) {
        Some(rec) if scope.is_some() && q.client_ref.as_deref() != Some(rec.client_ref.as_str()) => unknown_pay(),
        // No preimage: it goes only to the caller, in the answer to its POST (and the POST's replay).
        Some(rec) => Reply::ok(rec.to_json(false)),
        None => unknown_pay(),
    }
}

/// `GET /v1/pays`: the newest [`LIST_MAX`] pays, oldest first; no preimages. The POS token only:
/// it is every holder's pays, which a pay token does not open.
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

// ------------------------------------------------------------------ refunds

fn not_refundable(rec: &PayRec) -> Reply {
    about(
        Reply::fail(409, "not_refundable", format!("only a pay_failed pay is refunded, and pay {} is {}", rec.pay_id, rec.state.name())),
        rec,
    )
}

/// `POST /v1/pay/{pay_id}/refund`, with the card that paid in the field (`X-Card-Link`, or the
/// relay): load a `pay_failed` pay's value back onto it.
pub async fn pay_refund(
    State(h): State<TillHandle>,
    scope: Option<Extension<PayScope>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> Reply {
    if scope.is_some() && !headers.contains_key(crate::till::LINK_HEADER) {
        return card_link_required();
    }
    let link = match crate::till::link_of(&h, &headers) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let r = refund_on(&h, &id, link.clone()).await;
    // The link lives for one job.
    if let Some(l) = link {
        l.close();
    }
    r
}

async fn refund_on(h: &TillHandle, id: &str, link: Option<Link>) -> Reply {
    let Some(pays) = h.pays() else { return unavailable() };
    let Some(rec) = id.parse::<u64>().ok().and_then(|id| pays.get(id)) else { return unknown_pay() };
    match rec.state {
        // Done: answered again without a card.
        PayState::Refunded => return Reply::ok(rec.to_json(false)),
        PayState::PayFailed | PayState::Refunding => {}
        _ => return not_refundable(&rec),
    }
    if rec.short_id.is_none() {
        return card_unknown(&rec);
    }
    // A refund is a holder load, recorded in the registry.
    let ops = {
        let conn = h.conn();
        let c = conn.lock().unwrap();
        if let Some(r) = c.refusal() {
            return about(r, &rec);
        }
        c.ops.clone()
    };
    let Some(ops) = ops else {
        return about(
            Reply::fail(
                503,
                "ops_not_connected",
                "a refund is a load onto the card, recorded in the registry, and this till is not on one",
            ),
            &rec,
        );
    };
    h.refund_pay(rec.pay_id, ops.client.url().to_owned(), link, pays.clone()).await
}

fn card_unknown(rec: &PayRec) -> Reply {
    about(
        Reply::fail(
            409,
            "card_unknown",
            "the till did not record which card paid, so no card can take this refund here; the value stays in the till's wallet, owed to the holder",
        ),
        rec,
    )
}

/// What the loader's journal holds of pay `id`'s refund.
struct RefundLoads {
    /// What the federation took, over every kept load of the refund: on the card.
    msat: u64,
    /// The last of those loads.
    txid: Option<String>,
    /// A load of the refund is still in the journal, its outcome not yet known.
    in_flight: bool,
}

fn refund_loads(loader: &Loader<'_>, id: u64) -> Result<RefundLoads> {
    let tag = refund_tag(id);
    let in_flight = loader.pending()?.iter().any(|l| l.tag.as_deref() == Some(tag.as_str()));
    let kept = loader.kept(&tag)?;
    let took: Vec<_> = kept.iter().filter(|k| k.accepted).collect();
    Ok(RefundLoads {
        msat: took.iter().flat_map(|k| k.notes.iter().map(|(_, m)| *m)).sum(),
        txid: took.last().map(|k| k.txid.clone()),
        in_flight,
    })
}

/// Record pay `id`'s refund as the journal has it. While a load of it is in flight the record
/// stays as it is (the next call finishes that load first). Otherwise `refund_msat` is what the
/// refund's loads put on the card, and the pay is `refunded` once what it still owes is below
/// `min_denom`, the smallest note (that dust stays with the till), else `pay_failed` again, owed
/// the rest. Only a pay a refund was started for (or one still `pay_failed`, whose `refunding`
/// save was lost) is touched.
fn settle_refund(pays: &Pays, id: u64, loads: &RefundLoads, min_denom: u64) {
    let Some(mut rec) = pays.get(id) else { return };
    if loads.in_flight || !matches!(rec.state, PayState::Refunding | PayState::PayFailed) {
        return;
    }
    let state = if rec.held_msat().saturating_sub(loads.msat) < min_denom { PayState::Refunded } else { PayState::PayFailed };
    let refund_msat = (loads.msat > 0).then_some(loads.msat);
    if (rec.state, rec.refund_msat, rec.refund_txid.as_deref()) == (state, refund_msat, loads.txid.as_deref()) {
        return;
    }
    rec.state = state;
    rec.refund_msat = refund_msat;
    rec.refund_txid = loads.txid.clone();
    pays.record(&mut rec);
    match state {
        PayState::Refunded => {
            tracing::info!("pay {id}: refunded {} msat to the card; {} msat of dust stay with the till", loads.msat, rec.refund_dust_msat())
        }
        _ => tracing::info!("pay {id}: {} msat back on the card so far, {} msat still owed", loads.msat, rec.refund_owed_msat()),
    }
}

/// The refund of pay `id`, in the till's actor (it shares the job slot with every card job).
///
/// 1. Finish every load an earlier run left (a refund's, or a top-up's, which is marked loaded),
///    and record each refund it touched from the journal ([`settle_refund`]).
/// 2. What this refund's kept loads put on the card counts: the pay is `refunded` once less than
///    the smallest note is still owed.
/// 3. Otherwise open the card, check it is the one that paid, and load the largest amount at or
///    below what is still owed that its free slots hold ([`REFUND_MAX_NOTES`] at most): never more
///    than the till holds for the holder. Save `refunding` first. A load cut short stays in the
///    journal (the pay stays `refunding` and the next call finishes it); one that left nothing, or
///    a partial refund (the card had too few free slots), puts the pay back to `pay_failed`, owed
///    the rest, for another tap.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn refund_job(
    wallet: &crate::wallet::Wallet,
    dir: &Path,
    open: impl FnOnce() -> Result<Box<dyn fedimint_card_host_card::Card>>,
    topups: Option<&Topups>,
    pays: &Pays,
    id: u64,
    ops_url: &str,
    till_ops: Option<OpsClient>,
) -> Reply {
    let min = wallet.min_denom();
    let loader = match Loader::new(wallet, ops_url, dir) {
        Ok(l) => l.with_tag(refund_tag(id)).keep_finished(),
        Err(e) => return error_reply(&e),
    };
    let loader = match till_ops.clone() {
        Some(k) => loader.with_keyer(k),
        None => loader,
    };
    match loader.resume_each().await {
        Ok(done) => {
            for f in done {
                let Some(t) = &f.load.tag else { continue };
                match refund_of_tag(t) {
                    Some(pid) => match refund_loads(&loader, pid) {
                        Ok(l) => settle_refund(pays, pid, &l, min),
                        Err(e) => return refund_error(pays, &loader, id, min, error_reply(&e)),
                    },
                    None => {
                        if let (Some(tp), Some(_)) = (topups, &f.live) {
                            crate::till::mark_loaded(tp, t, &f.load.txid, &f.load.short_id);
                        }
                    }
                }
            }
        }
        Err(e) => return refund_error(pays, &loader, id, min, error_reply(&e)),
    }
    let loads = match refund_loads(&loader, id) {
        Ok(l) => l,
        Err(e) => return refund_error(pays, &loader, id, min, error_reply(&e)),
    };
    settle_refund(pays, id, &loads, min);
    let Some(mut rec) = pays.get(id) else { return unknown_pay() };
    match rec.state {
        PayState::Refunded => return Reply::ok(rec.to_json(false)),
        PayState::PayFailed | PayState::Refunding => {}
        _ => return not_refundable(&rec),
    }
    let Some(want) = rec.short_id.clone() else { return card_unknown(&rec) };
    let mut card = match open() {
        Ok(c) => c,
        Err(e) => return about(error_reply(&e), &rec),
    };
    let got = match on_card(|| card.get_pub(0)) {
        Ok(k) => short_id_of(&k),
        Err(e) => return about(error_reply(&e), &rec),
    };
    if !got.eq_ignore_ascii_case(&want) {
        return about(
            Reply::fail(
                409,
                "wrong_card",
                format!("pay {} was paid by card {want}, not this one (card {got}); nothing was loaded", rec.pay_id),
            ),
            &rec,
        );
    }
    // What is still owed, rounded down to whole notes the card has room for.
    let slots = match till_ops.as_ref().map_or_else(|| OpsClient::holder(ops_url), |c| Ok(c.clone())) {
        Ok(c) => c.free_slots(&want).await,
        Err(e) => return about(error_reply(&e), &rec),
    };
    let room = match slots {
        // Unkeyed slots (split keying) count: the load keys them at this tap.
        Ok(f) => (f.free.len() + f.unkeyed_slots.len()) as u32,
        Err(e) => return about(Reply::fail(502, "ops_unreachable", format!("the card's free slots: {e}")), &rec),
    };
    if room == 0 {
        return about(
            Reply::fail(409, "card_full", "the card has no free slot: spend from it, then refund again; nothing was loaded"),
            &rec,
        );
    }
    let owed = rec.held_msat().saturating_sub(loads.msat);
    let cfg = &wallet.mint_cfg;
    let Some(amount) = crate::plan::card_amount_at_most(owed, room.min(REFUND_MAX_NOTES), min, |n| {
        cfg.tbs_pks.get(fedimint_core::Amount::from_msats(n)).is_some()
    }) else {
        return about(Reply::fail(422, "no_refund_amount", format!("the {owed} msat owed make no amount the card can hold")), &rec);
    };
    rec.state = PayState::Refunding;
    // The refund's total once this load is on the card.
    rec.refund_msat = Some(loads.msat + amount);
    rec.refund_txid = loads.txid.clone();
    if let Err(e) = pays.write_ahead(&mut rec) {
        let saved = pays.get(id).unwrap_or(rec);
        return about(Reply::fail(500, "internal", format!("could not record the refund, so nothing was loaded: {e:#}")), &saved);
    }
    match loader.load(card.as_mut(), &[amount]).await {
        Ok(r) => {
            tracing::info!("pay {id}: loaded {} msat of its refund in {}", r.notes.iter().map(|(_, m)| m).sum::<u64>(), r.txid);
            match refund_loads(&loader, id) {
                Ok(l) => settle_refund(pays, id, &l, min),
                // The load is kept in the journal: the next call records it, and loads nothing twice.
                Err(e) => {
                    let saved = pays.get(id).unwrap_or(rec);
                    return about(
                        Reply::fail(
                            500,
                            "internal",
                            format!(
                                "the refund's load is on the card, but the journal could not be read back ({e:#}); refund again to record it"
                            ),
                        ),
                        &saved,
                    );
                }
            }
            match pays.get(id) {
                Some(rec) => Reply::ok(rec.to_json(false)),
                None => unknown_pay(),
            }
        }
        Err(e) => {
            let r = match e.downcast_ref::<CardFull>() {
                Some(full) => Reply::fail(409, "card_full", format!("{full}: spend from the card, then refund again; nothing was loaded")),
                None => error_reply(&e),
            };
            refund_error(pays, &loader, id, min, r)
        }
    }
}

/// A refund that stopped with `r`: still `refunding` while a load of it is in the journal (the
/// next call finishes it, never loads it again), else recorded from what its kept loads put on
/// the card ([`settle_refund`]). A journal that cannot be read counts as in flight.
fn refund_error(pays: &Pays, loader: &Loader<'_>, id: u64, min_denom: u64, r: Reply) -> Reply {
    if let Ok(l) = refund_loads(loader, id) {
        settle_refund(pays, id, &l, min_denom);
    }
    match pays.get(id) {
        Some(rec) => about(r, &rec),
        None => r,
    }
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
            short_id: None,
            refund_msat: None,
            refund_txid: None,
            received_msat: None,
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

    /// A wallet side that is never asked.
    struct NoLn;
    impl PayLightning for NoLn {
        fn pay(&self, _: String, _: u64, _: Duration) -> BoxFuture<'_, Result<WalletPay>> {
            Box::pin(async { bail!("not asked") })
        }
        fn resolve(&self, _: String, _: Duration) -> BoxFuture<'_, Result<WalletPay>> {
            Box::pin(async { bail!("not asked") })
        }
        fn status(&self, _: String, _: Duration) -> BoxFuture<'_, Result<Option<WalletPay>>> {
            Box::pin(async { bail!("not asked") })
        }
        fn quote(&self, _: String) -> BoxFuture<'_, Result<u64>> {
            Box::pin(async { bail!("not asked") })
        }
    }

    #[test]
    fn a_failure_is_answered_only_once_it_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = PayStore::path_in(dir.path());
        let mut store = PayStore::open(&path).unwrap();
        let r = store.insert(rec(0, PayState::Paying, 1, 1_000)).unwrap();
        // A save that fails: pay_failed is neither answered nor kept in memory.
        store.path = dir.path().join("missing").join("x").join(PAYS_FILE);
        std::fs::write(dir.path().join("missing"), b"a file, not a directory").unwrap();
        let pays = Pays::new(store, Arc::new(NoLn), PayConfig::default(), Currency::Regtest);
        let reply = fail(&pays, r.clone(), None, "the gateway asks more".into());
        assert_eq!(
            (reply.status, reply.body["error"].as_str(), reply.body["state"].as_str()),
            (202, Some("undecided"), Some("paying")),
            "{}",
            reply.body
        );
        assert!(!reply.body.to_string().contains("owed"), "{}", reply.body);
        assert_eq!(pays.get(r.pay_id).unwrap().state, PayState::Paying);
        assert_eq!(PayStore::open(&path).unwrap().get(r.pay_id).unwrap().state, PayState::Paying);
        // Saved: answered.
        pays.store.lock().unwrap().path = path.clone();
        let reply = fail(&pays, r.clone(), None, "the gateway asks more".into());
        assert_eq!((reply.status, reply.body["error"].as_str()), (502, Some("pay_failed")), "{}", reply.body);
        assert_eq!(pays.get(r.pay_id).unwrap().state, PayState::PayFailed);
        assert_eq!(PayStore::open(&path).unwrap().get(r.pay_id).unwrap().state, PayState::PayFailed);
    }

    #[test]
    fn a_refund_is_tagged_by_its_pay_and_answered_as_a_failed_pay() {
        assert_eq!(refund_tag(42), "pay-refund:42");
        assert_eq!(refund_of_tag(&refund_tag(42)), Some(42));
        assert_eq!(refund_of_tag("0123456789abcdef"), None);
        assert_eq!(refund_of_tag("pay-refund:x"), None);

        let mut r = rec(5, PayState::PayFailed, 1, 1_100);
        r.short_id = Some("3d13135c".into());
        // pay_failed: the card it is owed to and all of the price, no refund yet.
        let v = r.to_json(false);
        assert_eq!(v["short_id"], "3d13135c");
        assert!(v.get("refund_msat").is_none() && v.get("refund_txid").is_none(), "{v}");
        assert_eq!(v["refund_owed_msat"], 1_100);
        let f = final_reply(&r);
        assert_eq!((f.body["short_id"].as_str(), f.body["refund_owed_msat"].as_u64()), (Some("3d13135c"), Some(1_100)), "{}", f.body);
        // Part of it back on the card (too few free slots): pay_failed, owed the rest.
        r.refund_msat = Some(1_024);
        r.refund_txid = Some("cd".repeat(32));
        let v = r.to_json(false);
        assert_eq!((v["refund_msat"].as_u64(), v["refund_owed_msat"].as_u64()), (Some(1_024), Some(76)), "{v}");
        let f = final_reply(&r);
        assert_eq!((f.body["refund_msat"].as_u64(), f.body["refund_owed_msat"].as_u64()), (Some(1_024), Some(76)), "{}", f.body);
        assert!(f.body["detail"].as_str().unwrap().contains("76 msat of the card's 1100"), "{}", f.body);
        for (state, name) in [(PayState::Refunding, "refunding"), (PayState::Refunded, "refunded")] {
            r.state = state;
            r.refund_txid = (state == PayState::Refunded).then(|| "ef".repeat(32));
            let v = r.to_json(true);
            assert_eq!((v["state"].as_str(), v["refund_msat"].as_u64()), (Some(name), Some(1_024)), "{v}");
            assert!(v.get("preimage").is_none() && v.get("fee_paid_msat").is_none(), "{v}");
            // Replaying the POST of a refunded pay still says its invoice was not paid.
            let f = final_reply(&r);
            assert_eq!((f.status, f.body["error"].as_str(), f.body["state"].as_str()), (502, Some("pay_failed"), Some(name)));
            assert!(!state.is_open());
            assert!(v.get("refund_owed_msat").is_none(), "{v}");
        }
        assert_eq!(r.to_json(false)["refund_txid"], "ef".repeat(32));
        // Refunded: what stayed with the till, below one note.
        assert_eq!(r.to_json(false)["refund_dust_msat"], 76);
    }

    #[test]
    fn a_refund_is_recorded_from_the_sum_of_its_loads_and_never_while_one_is_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PayStore::open(PayStore::path_in(dir.path())).unwrap();
        let id = store.insert(rec(0, PayState::Refunding, 1, (1 << 20) + (1 << 10) + 1)).unwrap().pay_id;
        let pays = Pays::new(store, Arc::new(NoLn), PayConfig::default(), Currency::Regtest);
        let loads = |msat: u64, in_flight: bool| RefundLoads { msat, txid: (msat > 0).then(|| format!("{msat:064x}")), in_flight };
        let state = |p: &Pays| {
            let r = p.get(id).unwrap();
            (r.state, r.refund_msat, r.refund_owed_msat())
        };
        // A load in flight: left as it is.
        settle_refund(&pays, id, &loads(1 << 20, true), 512);
        assert_eq!(state(&pays), (PayState::Refunding, None, (1 << 20) + (1 << 10) + 1));
        // Part of it on the card: pay_failed, owed the rest.
        settle_refund(&pays, id, &loads(1 << 20, false), 512);
        assert_eq!(state(&pays), (PayState::PayFailed, Some(1 << 20), (1 << 10) + 1));
        assert_eq!(pays.get(id).unwrap().refund_txid, Some(format!("{:064x}", 1u64 << 20)));
        // Less than the smallest note still owed: refunded, the dust with the till.
        settle_refund(&pays, id, &loads((1 << 20) + (1 << 10), false), 512);
        assert_eq!(state(&pays), (PayState::Refunded, Some((1 << 20) + (1 << 10)), 1));
        assert_eq!(pays.get(id).unwrap().refund_dust_msat(), 1);
        // On disk too; a refunded pay is not touched again.
        assert_eq!(PayStore::open(PayStore::path_in(dir.path())).unwrap().get(id).unwrap().state, PayState::Refunded);
        settle_refund(&pays, id, &loads(0, false), 512);
        assert_eq!(state(&pays).0, PayState::Refunded);
        // Nothing of a refund ever landed: pay_failed, owed all of it.
        let mut r = pays.get(id).unwrap();
        r.state = PayState::Refunding;
        pays.store.lock().unwrap().put(&r).unwrap();
        settle_refund(&pays, id, &loads(0, false), 512);
        assert_eq!(state(&pays), (PayState::PayFailed, None, (1 << 20) + (1 << 10) + 1));
    }

    #[test]
    fn the_holder_is_owed_what_the_card_paid_the_till_overpay_included() {
        // The sale's overpay counts; so does what the till collected when it is more.
        assert_eq!(paid_to_till(11_150, 2_048, 0), 13_198);
        assert_eq!(paid_to_till(11_150, 2_048, 13_198), 13_198);
        assert_eq!(paid_to_till(11_150, 0, 0), 11_150);
        assert_eq!(paid_to_till(11_150, 0, 11_200), 11_200);
        let mut r = rec(4, PayState::PayFailed, 1, 11_150);
        // A record from before the charge's amount was kept: the price.
        assert_eq!((r.held_msat(), r.refund_owed_msat()), (11_150, 11_150));
        assert!(r.to_json(false).get("received_msat").is_none());
        r.received_msat = Some(13_198);
        assert_eq!((r.held_msat(), r.refund_owed_msat()), (13_198, 13_198));
        let v = r.to_json(false);
        assert_eq!(
            (v["received_msat"].as_u64(), v["overpaid_msat"].as_u64(), v["refund_owed_msat"].as_u64()),
            (Some(13_198), Some(2_048), Some(13_198))
        );
        assert!(final_reply(&r).body["detail"].as_str().unwrap().contains("13198 msat of the card's 13198"), "{}", final_reply(&r).body);
        // Part of it back on the card: owed the rest of what the card paid, not of the price.
        r.refund_msat = Some(12_288);
        assert_eq!(r.refund_owed_msat(), 910);
        // A refund settles against what the card paid: 910 msat owed is not dust under a 512 note.
        let dir = tempfile::tempdir().unwrap();
        let mut store = PayStore::open(PayStore::path_in(dir.path())).unwrap();
        r.refund_msat = None;
        let id = store.insert(r).unwrap().pay_id;
        let pays = Pays::new(store, Arc::new(NoLn), PayConfig::default(), Currency::Regtest);
        settle_refund(&pays, id, &RefundLoads { msat: 12_288, txid: Some("ab".repeat(32)), in_flight: false }, 512);
        assert_eq!((pays.get(id).unwrap().state, pays.get(id).unwrap().refund_owed_msat()), (PayState::PayFailed, 910));
        settle_refund(&pays, id, &RefundLoads { msat: 12_288 + 512, txid: Some("cd".repeat(32)), in_flight: false }, 512);
        assert_eq!((pays.get(id).unwrap().state, pays.get(id).unwrap().refund_dust_msat()), (PayState::Refunded, 398));
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
