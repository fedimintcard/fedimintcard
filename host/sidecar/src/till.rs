//! The till service: a small HTTP API (see `docs/TILL-API.md`) in front of the card wallet.
//!
//! [`CardWallet`] is not `Send` (it holds a `Box<dyn Card>` across awaits), so exactly one actor
//! future owns it and handlers talk to the actor through a channel. The actor is polled by the
//! task that calls [`serve`], on the same runtime as the CLI commands, so the blocking card calls
//! keep using `on_card` unchanged.
//!
//! At most one job (charge or card balance) runs at a time. The busy flag is set by the handler
//! that admits the job and cleared by the actor when the job is finished, so a client that
//! disconnects mid-charge cannot let a second charge start while the first is in flight.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use fedimint_card_host_card::{BridgeError, BridgeTransport, Card, CardClient, StatusWord};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::glue::{BalanceReport, CardWallet, PromotedSale, SpendFault, SpendReport, on_card};
use crate::plan::PlanError;
use crate::sales::{SaleRec, SalesStore, SlotMsat};
use crate::state::NoteStatus;
use crate::wallet::{Wallet, sales_path};

pub const DEFAULT_LISTEN: &str = "127.0.0.1:9877";
/// Largest price accepted (2^40 msat, about 11 BTC).
pub const MAX_PRICE_MSAT: u64 = 1 << 40;
const MAX_CLIENT_REF: usize = 128;
const RELAY_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// A finished API answer: HTTP status plus JSON body.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Value,
}

/// What `charge_with` returns.
pub type ChargeResponse = Reply;

impl IntoResponse for Reply {
    fn into_response(self) -> Response {
        (StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), Json(self.body)).into_response()
    }
}

impl Reply {
    fn ok(body: Value) -> Self {
        Reply { status: 200, body }
    }
    fn fail(status: u16, code: &str, detail: impl std::fmt::Display) -> Self {
        Reply { status, body: json!({"ok": false, "error": code, "detail": detail.to_string()}) }
    }
}

// ------------------------------------------------------------------ error classification

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Class {
    pub status: u16,
    pub code: &'static str,
}

/// Map an error to its API code and HTTP status by looking for typed errors in its cause chain.
pub fn classify(err: &anyhow::Error) -> Class {
    let c = |status, code| Class { status, code };
    for cause in err.chain() {
        if let Some(b) = cause.downcast_ref::<BridgeError>() {
            return match b {
                BridgeError::NoCard(_) => c(409, "no_card"),
                BridgeError::Unreachable(_) => c(502, "relay_unreachable"),
                BridgeError::Http(..) => c(502, "card_error"),
            };
        }
        if cause.downcast_ref::<StatusWord>().is_some() {
            return c(502, "card_error");
        }
        if let Some(p) = cause.downcast_ref::<PlanError>() {
            return match p {
                PlanError::Insufficient { .. } => c(402, "insufficient"),
                PlanError::NoExactPlan { .. } | PlanError::TooManyNotes(_) => c(422, "no_exact_plan"),
            };
        }
        if let Some(f) = cause.downcast_ref::<SpendFault>() {
            return match f {
                SpendFault::Declined(_) => c(402, "declined"),
                SpendFault::Undecided(_) => c(202, "undecided"),
                SpendFault::Unresolved(_) => c(503, "recovering"),
                SpendFault::WrongCard(_) => c(502, "card_error"),
            };
        }
    }
    c(500, "internal")
}

fn error_reply(err: &anyhow::Error) -> Reply {
    let class = classify(err);
    Reply::fail(class.status, class.code, format!("{err:#}"))
}

// ------------------------------------------------------------------ the card behind the relay

/// Placeholder for the time before the first job: `serve` starts without a card present.
pub struct NoCard;

impl Card for NoCard {
    fn gen_key(&mut self, _: u8) -> Result<fedimint_core::secp256k1::XOnlyPublicKey> {
        Err(anyhow!("no card connected"))
    }
    fn gen_key_overwrite(&mut self, _: u8) -> Result<fedimint_core::secp256k1::XOnlyPublicKey> {
        Err(anyhow!("no card connected"))
    }
    fn get_pub(&mut self, _: u8) -> Result<fedimint_core::secp256k1::XOnlyPublicKey> {
        Err(anyhow!("no card connected"))
    }
    fn sign(&mut self, _: u8, _: &[u8; 32]) -> Result<fedimint_core::secp256k1::schnorr::Signature> {
        Err(anyhow!("no card connected"))
    }
}

/// Open the card behind the phone relay at `relay` and select the applet. Blocking.
pub fn open_relay_card(relay: &str) -> Result<Box<dyn Card>> {
    let mut c = CardClient::new(BridgeTransport::new(relay));
    c.select().with_context(|| format!("card relay at {relay}"))?;
    Ok(Box::new(c))
}

/// How the actor gets a card for each job.
pub type CardOpener = Box<dyn Fn() -> Result<Box<dyn Card>>>;

// ------------------------------------------------------------------ jobs and the actor

enum Job {
    Charge { price: u64, client_ref: Option<String>, reply: oneshot::Sender<Reply> },
    Balance { reply: oneshot::Sender<Reply> },
    Precheck { reply: oneshot::Sender<Reply> },
    KeySetup { pin: Option<Vec<u8>>, reply: oneshot::Sender<Reply> },
    Issue { amounts: Vec<u64>, client_ref: Option<String>, reply: oneshot::Sender<Reply> },
}

/// What handlers hold. Cheap to clone, `Send + Sync`.
#[derive(Clone)]
pub struct TillHandle {
    tx: mpsc::Sender<Job>,
    busy: Arc<AtomicBool>,
    /// The `client_ref` of the job that holds the busy flag, if it has one.
    inflight: Arc<Mutex<Option<String>>>,
    /// `client_ref`s of unresolved transactions in card_state.json, refreshed by the actor after
    /// every job (a read-only snapshot, so handlers need not touch the wallet).
    pending_refs: Arc<Mutex<Vec<String>>>,
    wallet: Wallet,
    relay: String,
    sales: Arc<Mutex<SalesStore>>,
    federation_name: Option<String>,
}

/// Build the handle and the actor future. The caller must poll the future (it is not `Send`)
/// for as long as the handle's router is serving; it ends when every handle clone is dropped.
pub async fn new(
    cw: CardWallet,
    relay: String,
    sales: SalesStore,
    opener: CardOpener,
) -> (TillHandle, impl std::future::Future<Output = ()>) {
    let (tx, rx) = mpsc::channel(1);
    let busy = Arc::new(AtomicBool::new(false));
    let sales = Arc::new(Mutex::new(sales));
    let inflight = Arc::new(Mutex::new(None));
    let pending_refs = Arc::new(Mutex::new(refs_of(&cw)));
    let federation_name = cw.wallet.client.config().await.global.meta.get("federation_name").cloned();
    let handle = TillHandle {
        tx,
        busy: busy.clone(),
        inflight: inflight.clone(),
        pending_refs: pending_refs.clone(),
        wallet: cw.wallet.clone(),
        relay,
        sales: sales.clone(),
        federation_name,
    };
    (handle, actor(cw, rx, busy, inflight, pending_refs, sales, opener))
}

async fn actor(
    mut cw: CardWallet,
    mut rx: mpsc::Receiver<Job>,
    busy: Arc<AtomicBool>,
    inflight: Arc<Mutex<Option<String>>>,
    pending_refs: Arc<Mutex<Vec<String>>>,
    sales: Arc<Mutex<SalesStore>>,
    opener: CardOpener,
) {
    ensure_hook(&mut cw, &sales);
    while let Some(job) = rx.recv().await {
        let (reply_tx, reply) = match job {
            Job::Charge { price, client_ref, reply } => {
                let r = match on_card(|| opener()) {
                    Ok(card) => {
                        cw.card = card;
                        charge_with(&mut cw, price, client_ref, &sales).await
                    }
                    Err(e) => {
                        // A repeat of an earlier attempt can be answered without a card.
                        let settled = match client_ref.as_deref() {
                            Some(c) => {
                                cw.card = Box::new(NoCard);
                                let r = settle(&mut cw, c, price, &sales).await;
                                sync_sales(&mut cw, &sales);
                                r
                            }
                            None => None,
                        };
                        settled.unwrap_or_else(|| error_reply(&e))
                    }
                };
                (reply, r)
            }
            Job::Balance { reply } => {
                let r = match on_card(|| opener()) {
                    Ok(card) => {
                        cw.card = card;
                        balance_job(&mut cw, &sales).await
                    }
                    Err(e) => error_reply(&e),
                };
                (reply, r)
            }
            Job::Precheck { reply } => {
                let r = match on_card(|| opener()) {
                    Ok(card) => {
                        cw.card = card;
                        match crate::cards::precheck(&mut cw) {
                            Ok(p) => Reply::ok(p.to_json()),
                            Err(e) => error_reply(&e),
                        }
                    }
                    Err(e) => error_reply(&e),
                };
                (reply, r)
            }
            Job::KeySetup { pin, reply } => {
                let r = match on_card(|| opener()) {
                    Ok(card) => {
                        cw.card = card;
                        match crate::cards::key_setup(&mut cw, pin.as_deref()) {
                            Ok(o) => Reply::ok(o.to_json()),
                            Err(e) => error_reply(&e),
                        }
                    }
                    Err(e) => error_reply(&e),
                };
                (reply, r)
            }
            Job::Issue { amounts, client_ref, reply } => {
                let r = match on_card(|| opener()) {
                    Ok(card) => {
                        cw.card = card;
                        issue_job(&mut cw, &amounts, client_ref).await
                    }
                    Err(e) => error_reply(&e),
                };
                (reply, r)
            }
        };
        cw.card = Box::new(NoCard);
        *pending_refs.lock().unwrap() = refs_of(&cw);
        // Free the service before answering, so a client that retries at once is not told
        // "busy". Both are cleared under the lock admission takes, so no request can see "busy"
        // for a job that is already over.
        {
            let mut g = inflight.lock().unwrap();
            *g = None;
            busy.store(false, Ordering::SeqCst);
        }
        let _ = reply_tx.send(reply);
    }
}

/// `POST /v1/cards/issue`: put the batch's amounts on the card, exactly as `card topup` does.
///
/// The card must already be in `cw.card`. A `client_ref` that has finished is answered from
/// state without touching the card; one whose notes are pending is `undecided`, never a second
/// issue. A card already holding live notes on the keys this call would write to is refused —
/// issuing again would double its value with no record of having done so.
async fn issue_job(cw: &mut CardWallet, amounts: &[u64], client_ref: Option<String>) -> Reply {
    if let Some(cref) = client_ref.as_deref() {
        if let Some(rec) = cw.store.state.issues.iter().find(|r| r.client_ref == cref).cloned() {
            if rec.amounts != amounts {
                return Reply::fail(
                    409,
                    "ref_mismatch",
                    format!(
                        "client_ref {cref} was issued for amounts {:?}, not {amounts:?}",
                        rec.amounts
                    ),
                );
            }
            if rec.txid.is_empty() {
                return undecided_reply(None);
            }
            let live_msat = cw
                .store
                .state
                .notes
                .iter()
                .filter(|n| n.status == crate::state::NoteStatus::Live)
                .map(|n| n.amount_msat)
                .sum::<u64>()
                / 1000;
            return Reply::ok(
                crate::cards::IssueOutcome {
                    client_ref: Some(cref.to_owned()),
                    live_msat,
                    txid: rec.txid,
                    notes: rec.notes,
                    plan: json!({"notes": amounts.iter().flat_map(|a| crate::plan::binary(*a)).collect::<Vec<u64>>()}),
                    replayed: true,
                }
                .to_json(),
            );
        }
    }

    let preview = match crate::cards::issue_plan(cw, amounts).await {
        Ok(p) => p,
        Err(e) => return error_reply(&e),
    };
    let held = crate::cards::held_notes(cw);
    if !held.is_empty() {
        return Reply::fail(
            409,
            "card_holds_notes",
            format!(
                "this card already holds {} note(s) worth {} msat (slots {:?}); \
                 issuing again would double its value, and a copied card would hold it twice",
                held.len(),
                held.iter().map(|(_, a)| *a).sum::<u64>(),
                held.iter().map(|(s, _)| *s).collect::<Vec<u8>>()
            ),
        );
    }

    match cw.topup_ref(amounts, client_ref.as_deref()).await {
        Ok(notes) => {
            let txid = client_ref
                .as_deref()
                .and_then(|c| cw.store.state.issues.iter().find(|r| r.client_ref == c))
                .map(|r| r.txid.clone())
                .or_else(|| {
                    notes
                        .first()
                        .and_then(|(slot, _)| cw.store.state.notes_on_current_key(*slot).find_map(|n| n.txid.clone()))
                })
                .unwrap_or_default();
            let live_msat = cw
                .store
                .state
                .notes
                .iter()
                .filter(|n| n.status == crate::state::NoteStatus::Live)
                .map(|n| n.amount_msat)
                .sum::<u64>()
                / 1000;
            Reply::ok(
                crate::cards::IssueOutcome {
                    client_ref,
                    live_msat,
                    txid,
                    notes,
                    plan: preview,
                    replayed: false,
                }
                .to_json(),
            )
        }
        Err(e) => error_reply(&e),
    }
}

/// `POST /v1/card/balance`: check the card, reconcile, and record sales the reconciliation
/// found. The card must already be in `cw.card`.
pub async fn balance_job(cw: &mut CardWallet, sales: &Arc<Mutex<SalesStore>>) -> Reply {
    ensure_hook(cw, sales);
    let r = match cw.verify_card() {
        Ok(()) => match cw.balance().await {
            Ok(rep) => balance_reply(&rep),
            Err(e) => error_reply(&e),
        },
        Err(e) => error_reply(&e),
    };
    sync_sales(cw, sales);
    // Ecash strings are never sent over the API; they are in cashouts.log.
    cw.recovered_cashouts.clear();
    r
}

fn refs_of(cw: &CardWallet) -> Vec<String> {
    cw.store.state.pending_txs.iter().filter_map(|p| p.client_ref.clone()).collect()
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn status_str(s: NoteStatus) -> &'static str {
    match s {
        NoteStatus::Live => "live",
        NoteStatus::Pending => "pending",
        NoteStatus::Spent => "spent",
        NoteStatus::Dead => "dead",
    }
}

fn balance_reply(r: &BalanceReport) -> Reply {
    let notes: Vec<Value> = r
        .notes
        .iter()
        .map(|n| json!({"slot": n.slot, "msat": n.amount_msat, "status": status_str(n.status)}))
        .collect();
    Reply::ok(json!({"ok": true, "live_msat": r.live_msat, "notes": notes, "free_slots": r.free_slots}))
}

fn charge_body(s: &SaleRec, warnings: &[String], duration_ms: u64) -> Value {
    json!({
        "ok": true,
        "sale_id": s.id,
        "txid": s.txid,
        "price_msat": s.price_msat,
        "received_msat": s.received_msat,
        "overpaid_msat": s.overpaid_msat,
        "fees_msat": s.fees_msat,
        "inputs": s.inputs,
        "change": s.change,
        "collection_pending": s.collection_pending,
        "warnings": warnings,
        "duration_ms": duration_ms,
    })
}

fn validate_price(price: u64) -> Option<Reply> {
    if price == 0 || price > MAX_PRICE_MSAT {
        return Some(Reply::fail(400, "bad_request", format!("price_msat must be between 1 and {MAX_PRICE_MSAT}")));
    }
    None
}

fn conflict_reply(client_ref: &str, had: u64, now: u64) -> Reply {
    Reply::fail(
        409,
        "conflict",
        format!("client_ref {client_ref:?} was used for a sale of {had} msat, not {now} msat"),
    )
}

/// A completed sale for `client_ref`: the same body again, or `conflict` if the price differs.
fn replay(sales: &Mutex<SalesStore>, client_ref: Option<&str>, price: u64) -> Option<Reply> {
    let rec = sales.lock().unwrap().find_ref(client_ref?)?.clone();
    if rec.price_msat != price {
        return Some(conflict_reply(client_ref?, rec.price_msat, price));
    }
    Some(Reply::ok(charge_body(&rec, &rec.warnings, rec.duration_ms)))
}

fn undecided_reply(txid: Option<&str>) -> Reply {
    Reply::fail(
        202,
        "undecided",
        format!(
            "an earlier attempt with this client_ref is still being resolved{}; nothing was charged again",
            txid.map(|t| format!(" (transaction {t})")).unwrap_or_default()
        ),
    )
}

fn sale_from(p: &PromotedSale) -> SaleRec {
    SaleRec {
        id: 0,
        ts_unix_ms: p.ts_unix_ms,
        price_msat: p.price_msat,
        received_msat: p.received_msat,
        overpaid_msat: p.overpaid_msat,
        fees_msat: p.fees_msat,
        txid: p.txid.clone(),
        inputs: SlotMsat::list(&p.inputs),
        change: SlotMsat::list(&p.change),
        collection_pending: p.collection_pending,
        client_ref: Some(p.client_ref.clone()),
        duration_ms: 0,
        warnings: vec![],
    }
}

/// Make every resolution of a transaction that carries a `client_ref` write its sale to
/// `sales.json` before the link in `card_state.json` is removed.
pub fn ensure_hook(cw: &mut CardWallet, sales: &Arc<Mutex<SalesStore>>) {
    let sales = sales.clone();
    cw.on_promote = Some(Box::new(move |p| {
        sales.lock().unwrap().append(sale_from(p)).map(|_| ())
    }));
}

/// After a job: sales the federation accepted whose notes are still being collected show up in
/// the ledger too, so an app polling `/v1/sales` sees them; so do sales left in tombstones by
/// a process that had no ledger.
fn sync_sales(cw: &mut CardWallet, sales: &Mutex<SalesStore>) {
    for p in cw.accepted_refs() {
        if let Err(e) = sales.lock().unwrap().append(sale_from(&p)) {
            tracing::warn!("recording accepted sale {}: {e:#}", p.txid);
        }
    }
    for (sale, _) in cw.tombstoned_sales() {
        let done = sales.lock().unwrap().append(sale_from(&sale));
        match done.and_then(|_| cw.clear_tombstone(&sale.txid)) {
            Ok(()) => {}
            Err(e) => tracing::warn!("recording tombstoned sale {}: {e:#}", sale.txid),
        }
    }
}

/// What a repeated `client_ref` means. `Some(reply)` is the final answer; `None` means the
/// earlier attempt is proven to have left nothing behind and a fresh attempt may start.
/// Never needs the card, and never starts a spend.
pub async fn settle(
    cw: &mut CardWallet,
    client_ref: &str,
    price: u64,
    sales: &Arc<Mutex<SalesStore>>,
) -> Option<Reply> {
    ensure_hook(cw, sales);
    // A sale recorded while its notes were still being collected is finished first, so the
    // replay shows the final amounts.
    let collecting = sales.lock().unwrap().find_ref(client_ref).is_some_and(|r| r.collection_pending)
        && cw.pending_for_ref(client_ref).is_some();
    if !collecting {
        if let Some(r) = replay(sales, Some(client_ref), price) {
            return Some(r);
        }
    }
    // A sale whose record was removed by a process without a ledger: record it now.
    if let Some((sale, _)) = cw.tombstoned_sales().into_iter().find(|(s, _)| s.client_ref == client_ref) {
        if sale.price_msat != price {
            return Some(conflict_reply(client_ref, sale.price_msat, price));
        }
        let rec = match sales.lock().unwrap().append(sale_from(&sale)) {
            Ok(r) => {
                let _ = cw.clear_tombstone(&sale.txid);
                r
            }
            Err(_) => sale_from(&sale), // held in memory by the store; the tombstone stays
        };
        return Some(Reply::ok(charge_body(&rec, &rec.warnings, rec.duration_ms)));
    }
    let pending = cw.pending_for_ref(client_ref)?.clone();
    if pending.price_msat != price {
        return Some(conflict_reply(client_ref, pending.price_msat, price));
    }
    // Resolve it now. If the federation accepted it, the hook has recorded the sale.
    match cw.resolve_pending(Duration::from_secs(10)).await {
        Ok(rec) => {
            for e in &rec.errors {
                tracing::warn!("resolving before a repeated client_ref: {e}");
            }
        }
        Err(e) => tracing::warn!("resolving before a repeated client_ref: {e:#}"),
    }
    cw.recovered_cashouts.clear();
    if let Some(r) = replay(sales, Some(client_ref), price) {
        return Some(r);
    }
    match cw.pending_for_ref(client_ref).cloned() {
        // Accepted, the till notes are still being collected: it is a sale.
        Some(p) if p.accepted => {
            let sale = PromotedSale {
                client_ref: client_ref.to_owned(),
                txid: p.txid.clone(),
                ts_unix_ms: p.ts_unix_ms,
                price_msat: p.price_msat,
                received_msat: 0,
                overpaid_msat: p.overpay_msat,
                fees_msat: p.fees_msat,
                inputs: p.spent.clone(),
                change: p.change.clone(),
                collection_pending: true,
            };
            let rec = match sales.lock().unwrap().append(sale_from(&sale)) {
                Ok(r) => r,
                Err(_) => sale_from(&sale), // held in memory by the store; replays still work
            };
            Some(Reply::ok(charge_body(&rec, &rec.warnings, rec.duration_ms)))
        }
        // Still unknown: never a fresh spend.
        Some(p) => Some(undecided_reply(Some(&p.txid))),
        // Gone without a sale: proven abandoned.
        None => None,
    }
}

/// The testable core of `POST /v1/charge`: spend `price` from `cw`'s card and record the sale.
///
/// A `client_ref` is stored on the transaction record in `card_state.json` in the save that
/// creates it, before anything is signed or submitted, so a repeat can always find its
/// transaction. A repeat never starts a second spend while the first may still count.
pub async fn charge_with(
    cw: &mut CardWallet,
    price: u64,
    client_ref: Option<String>,
    sales: &Arc<Mutex<SalesStore>>,
) -> ChargeResponse {
    if let Some(bad) = validate_price(price) {
        return bad;
    }
    ensure_hook(cw, sales);
    if let Some(c) = client_ref.as_deref() {
        if let Some(r) = settle(cw, c, price, sales).await {
            sync_sales(cw, sales);
            return r;
        }
    }
    if let Err(e) = cw.verify_card() {
        return error_reply(&e);
    }
    let started = Instant::now();
    let result = cw.spend_ref(price, client_ref.as_deref()).await;
    // Whatever happened, recovered ecash is never part of an API answer (it is in cashouts.log).
    cw.recovered_cashouts.clear();
    let r: SpendReport = match result {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("charge of {price} msat failed: {e:#}");
            sync_sales(cw, sales);
            return error_reply(&e);
        }
    };
    let mut rec = SaleRec {
        id: 0,
        ts_unix_ms: now_ms(),
        price_msat: r.price_msat,
        received_msat: r.received.msats,
        overpaid_msat: r.overpay_msat,
        fees_msat: r.fees_msat,
        txid: r.txid.to_string(),
        inputs: SlotMsat::list(&r.inputs),
        change: SlotMsat::list(&r.change),
        collection_pending: r.collection_pending,
        client_ref,
        duration_ms: started.elapsed().as_millis() as u64,
        warnings: r.warnings.clone(),
    };
    // The money has moved: a ledger failure must not turn a paid sale into an error. The store
    // keeps the sale in memory, so a repeat still replays it.
    match sales.lock().unwrap().append(rec.clone()) {
        Ok(saved) => rec = saved,
        Err(e) => rec.warnings.push(format!("sale not recorded in sales.json: {e:#}")),
    }
    sync_sales(cw, sales);
    Reply::ok(charge_body(&rec, &rec.warnings, rec.duration_ms))
}

// ------------------------------------------------------------------ HTTP

pub fn router(h: TillHandle) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/status", get(status))
        .route("/v1/charge", post(charge))
        .route("/v1/sales", get(sales_list))
        .route("/v1/card/balance", post(card_balance))
        .route("/v1/cards/precheck", post(cards_precheck))
        .route("/v1/cards/keySetup", post(cards_key_setup))
        .route("/v1/cards/issue", post(cards_issue))
        .with_state(h)
}

async fn health() -> Reply {
    Reply::ok(json!({"ok": true}))
}

async fn status(State(h): State<TillHandle>) -> Reply {
    let balance = match h.wallet.balance_msat().await {
        Ok(b) => b,
        Err(e) => return error_reply(&e),
    };
    let relay = h.relay.clone();
    let probe = tokio::task::spawn_blocking(move || BridgeTransport::new(&relay).health());
    let relay_ok = matches!(tokio::time::timeout(RELAY_PROBE_TIMEOUT, probe).await, Ok(Ok(Ok(()))));
    Reply::ok(json!({
        "federation_id": h.wallet.federation_id().to_string(),
        "federation_name": h.federation_name,
        "wallet_balance_msat": balance,
        "card_relay_ok": relay_ok,
        "busy": h.busy.load(Ordering::SeqCst),
    }))
}

#[derive(Deserialize)]
struct ChargeReq {
    price_msat: Option<u64>,
    client_ref: Option<String>,
}

/// Admit a job (or say busy) and wait for the actor's answer. A repeat of the `client_ref` that
/// is in flight right now is answered `undecided`, never `busy`, and is not queued; one that
/// finished in the meantime gets its sale.
async fn run_job(
    h: &TillHandle,
    charge: Option<(&str, u64)>,
    make: impl FnOnce(oneshot::Sender<Reply>) -> Job,
) -> Reply {
    {
        let mut inflight = h.inflight.lock().unwrap();
        if h.busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            if let Some((c, price)) = charge {
                if let Some(r) = replay(&h.sales, Some(c), price) {
                    return r;
                }
                if inflight.as_deref() == Some(c) || h.pending_refs.lock().unwrap().iter().any(|r| r == c) {
                    return undecided_reply(None);
                }
            }
            return Reply::fail(409, "busy", "another charge or balance call is in flight");
        }
        *inflight = charge.map(|(c, _)| c.to_owned());
    }
    let (tx, rx) = oneshot::channel();
    if h.tx.try_send(make(tx)).is_err() {
        let mut g = h.inflight.lock().unwrap();
        *g = None;
        h.busy.store(false, Ordering::SeqCst);
        return Reply::fail(500, "internal", "the till service is shutting down");
    }
    rx.await.unwrap_or_else(|_| Reply::fail(500, "internal", "the till service stopped before answering"))
}

async fn charge(State(h): State<TillHandle>, body: Bytes) -> Reply {
    let req: ChargeReq = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return Reply::fail(400, "bad_request", format!("body must be JSON like {{\"price_msat\":N}}: {e}")),
    };
    let Some(price) = req.price_msat else {
        return Reply::fail(400, "bad_request", "price_msat is required");
    };
    if let Some(bad) = validate_price(price) {
        return bad;
    }
    if req.client_ref.as_deref().is_some_and(|c| c.is_empty() || c.len() > MAX_CLIENT_REF) {
        return Reply::fail(400, "bad_request", format!("client_ref must be 1 to {MAX_CLIENT_REF} bytes"));
    }
    // A repeat of a finished sale needs neither the card nor the busy flag.
    if let Some(r) = replay(&h.sales, req.client_ref.as_deref(), price) {
        return r;
    }
    let cref = req.client_ref.clone();
    run_job(&h, cref.as_deref().map(|c| (c, price)), |reply| Job::Charge { price, client_ref: req.client_ref, reply }).await
}

async fn card_balance(State(h): State<TillHandle>) -> Reply {
    run_job(&h, None, |reply| Job::Balance { reply }).await
}

#[derive(Deserialize)]
struct KeySetupReq {
    /// 4 to 8 ASCII digits, per the applet's `PIN_MIN`/`PIN_MAX`. Omitted means "leave the PIN
    /// alone" — a card can be keyed and issued before it is pinned.
    pin: Option<String>,
}

#[derive(Deserialize)]
struct PrecheckReq {
    /// Accepted and ignored: precheck asks the card, not the caller. Present so the app can post
    /// the batch it is about if it later wants the reply annotated.
    #[serde(default)]
    #[allow(dead_code)]
    batch_id: Option<String>,
}

/// `POST /v1/cards/precheck` — read-only. What is on this card, is the ISD open, does the applet
/// answer. Nothing is written, so the app can call it on every tap.
async fn cards_precheck(State(h): State<TillHandle>, body: Bytes) -> Reply {
    if !body.is_empty() {
        if let Err(e) = serde_json::from_slice::<PrecheckReq>(&body) {
            return Reply::fail(400, "bad_request", format!("body must be JSON or empty: {e}"));
        }
    }
    run_job(&h, None, |reply| Job::Precheck { reply }).await
}

/// `POST /v1/cards/keySetup` — `card setup`, then `SET_PIN`. Idempotent: keys already present are
/// kept and an existing PIN is reported, not reset.
async fn cards_key_setup(State(h): State<TillHandle>, body: Bytes) -> Reply {
    let req: KeySetupReq = if body.is_empty() {
        KeySetupReq { pin: None }
    } else {
        match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => return Reply::fail(400, "bad_request", format!("body must be JSON like {{\"pin\":\"1234\"}}: {e}")),
        }
    };
    let pin = match req.pin {
        None => None,
        Some(p) => {
            if !(4..=8).contains(&p.len()) || !p.bytes().all(|b| b.is_ascii_digit()) {
                return Reply::fail(400, "bad_request", "pin must be 4 to 8 digits");
            }
            Some(p.into_bytes())
        }
    };
    run_job(&h, None, |reply| Job::KeySetup { pin, reply }).await
}

/// `POST /v1/cards/issue` — the batch's amounts onto the card, replayable by `client_ref`.
async fn cards_issue(State(h): State<TillHandle>, body: Bytes) -> Reply {
    let req: crate::cards::IssueReq = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return Reply::fail(400, "bad_request", format!("body must be JSON like {{\"amount_msat\":[N]}}: {e}")),
    };
    if req.amounts_msat.is_empty() {
        return Reply::fail(400, "bad_request", "amount_msat must name at least one amount");
    }
    if req.client_ref.as_deref().is_some_and(|c| c.is_empty() || c.len() > MAX_CLIENT_REF) {
        return Reply::fail(400, "bad_request", format!("client_ref must be 1 to {MAX_CLIENT_REF} bytes"));
    }
    let cref = req.client_ref.clone();
    run_job(&h, cref.as_deref().map(|c| (c, 0)), |reply| Job::Issue { amounts: req.amounts_msat, client_ref: req.client_ref, reply })
        .await
}

async fn sales_list(State(h): State<TillHandle>) -> Reply {
    let s = h.sales.lock().unwrap();
    let list: Vec<Value> = s
        .file
        .sales
        .iter()
        .map(|r| {
            json!({
                "id": r.id, "ts_unix_ms": r.ts_unix_ms, "price_msat": r.price_msat,
                "received_msat": r.received_msat, "overpaid_msat": r.overpaid_msat,
                "fees_msat": r.fees_msat, "txid": r.txid, "inputs": r.inputs,
                "change": r.change, "client_ref": r.client_ref,
            })
        })
        .collect();
    Reply::ok(json!({
        "count": s.file.sales.len(),
        "total_price_msat": s.file.sales.iter().map(|r| r.price_msat).sum::<u64>(),
        "total_received_msat": s.file.sales.iter().map(|r| r.received_msat).sum::<u64>(),
        "sales": list,
    }))
}

// ------------------------------------------------------------------ serve

/// Run the till service until Ctrl-C. Owns the data directory (the sales file is stored next to
/// the state file). Starts without a card: each job opens the card behind `relay` afresh.
pub async fn serve(cw: CardWallet, listen: SocketAddr, relay: String) -> Result<()> {
    if !listen.ip().is_loopback() {
        eprintln!("warning: {listen} is not a loopback address; the till API has no authentication");
    }
    let dir = cw.store.path().parent().map(|p| p.to_owned()).unwrap_or_default();
    let sales = SalesStore::open(sales_path(&dir))?;
    let listener = tokio::net::TcpListener::bind(listen).await.with_context(|| format!("binding {listen}"))?;
    let relay_for_jobs = relay.clone();
    let opener: CardOpener = Box::new(move || open_relay_card(&relay_for_jobs));
    let mut cw = cw;
    cw.card = Box::new(NoCard);
    let (handle, actor) = new(cw, relay.clone(), sales, opener).await;
    eprintln!("till service listening on http://{} (card relay {relay})", listener.local_addr()?);
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let server = tokio::spawn(std::future::IntoFuture::into_future(
        axum::serve(listener, router(handle)).with_graceful_shutdown(shutdown),
    ));
    // The actor ends once the server has dropped the router (and with it every handle).
    let (served, ()) = tokio::join!(server, actor);
    served.context("till server task")??;
    Ok(())
}
