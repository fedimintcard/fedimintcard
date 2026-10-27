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

use crate::glue::{BalanceReport, CardWallet, SpendFault, SpendReport, on_card};
use crate::plan::PlanError;
use crate::sales::{PendingRec, SaleRec, SalesStore, SlotMsat};
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
}

/// What handlers hold. Cheap to clone, `Send + Sync`.
#[derive(Clone)]
pub struct TillHandle {
    tx: mpsc::Sender<Job>,
    busy: Arc<AtomicBool>,
    /// The `client_ref` of the job that holds the busy flag, if it has one.
    inflight: Arc<Mutex<Option<String>>>,
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
    let federation_name = cw.wallet.client.config().await.global.meta.get("federation_name").cloned();
    let handle = TillHandle {
        tx,
        busy: busy.clone(),
        inflight: inflight.clone(),
        wallet: cw.wallet.clone(),
        relay,
        sales: sales.clone(),
        federation_name,
    };
    (handle, actor(cw, rx, busy, inflight, sales, opener))
}

async fn actor(
    mut cw: CardWallet,
    mut rx: mpsc::Receiver<Job>,
    busy: Arc<AtomicBool>,
    inflight: Arc<Mutex<Option<String>>>,
    sales: Arc<Mutex<SalesStore>>,
    opener: CardOpener,
) {
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
                                settle_pending(&mut cw, c, &sales).await
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
                        match cw.balance().await {
                            Ok(rep) => balance_reply(&rep),
                            Err(e) => error_reply(&e),
                        }
                    }
                    Err(e) => error_reply(&e),
                };
                // Ecash strings are never sent over the API; they are in cashouts.log.
                cw.recovered_cashouts.clear();
                (reply, r)
            }
        };
        cw.card = Box::new(NoCard);
        // Free the service before answering, so a client that retries at once is not told "busy".
        *inflight.lock().unwrap() = None;
        busy.store(false, Ordering::SeqCst);
        let _ = reply_tx.send(reply);
    }
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

fn replay(sales: &Mutex<SalesStore>, client_ref: Option<&str>) -> Option<Reply> {
    let rec = sales.lock().unwrap().find_ref(client_ref?)?.clone();
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

/// What a repeated `client_ref` means. `Some(reply)` is the final answer (the earlier sale, or
/// still undecided); `None` means the earlier attempt left nothing behind and a fresh attempt
/// may start. Does not need the card.
pub async fn settle_pending(
    cw: &mut CardWallet,
    client_ref: &str,
    sales: &Mutex<SalesStore>,
) -> Option<Reply> {
    if let Some(r) = replay(sales, Some(client_ref)) {
        return Some(r);
    }
    let pending = sales.lock().unwrap().find_pending(client_ref).cloned()?;
    // Finish what the earlier attempt left in flight (same path every command takes first).
    let rec = match cw.resolve_pending(Duration::from_secs(10)).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("resolving before a repeated client_ref: {e:#}");
            Default::default()
        }
    };
    cw.recovered_cashouts.clear();
    let still = cw.in_flight();
    let in_flight = match &pending.txid {
        Some(t) => still.contains(t),
        None => !still.is_empty(),
    };
    if in_flight {
        return Some(undecided_reply(pending.txid.as_deref()));
    }
    if let Some(t) = &pending.txid {
        if cw.tx_accepted(t, Duration::from_secs(5)).await {
            // Accepted. The per-input detail was not kept; the federation is the record.
            let sale = SaleRec {
                id: 0,
                ts_unix_ms: pending.ts_unix_ms,
                price_msat: pending.price_msat,
                received_msat: rec.till_received,
                overpaid_msat: 0,
                fees_msat: 0,
                txid: t.clone(),
                inputs: vec![],
                change: vec![],
                collection_pending: false,
                client_ref: Some(client_ref.to_owned()),
                duration_ms: 0,
                warnings: vec!["recovered after an undecided attempt; fees and slots were not recorded".into()],
            };
            let mut store = sales.lock().unwrap();
            return Some(match store.append(sale.clone()) {
                Ok(saved) => Reply::ok(charge_body(&saved, &saved.warnings, 0)),
                Err(e) => return Some(Reply::fail(500, "internal", format!("{e:#}"))),
            });
        }
    }
    // Nothing in flight and not accepted: the attempt failed before it mattered.
    let _ = sales.lock().unwrap().clear_pending(client_ref);
    None
}

/// The testable core of `POST /v1/charge`: spend `price` from `cw`'s card and record the sale.
///
/// A `client_ref` is written to `sales.json` as a pending record before anything is submitted.
/// A repeat never starts a second spend while the first may still count: it returns the earlier
/// sale, or `undecided`, or (if the first left nothing behind) starts afresh.
pub async fn charge_with(
    cw: &mut CardWallet,
    price: u64,
    client_ref: Option<String>,
    sales: &Mutex<SalesStore>,
) -> ChargeResponse {
    if let Some(bad) = validate_price(price) {
        return bad;
    }
    if let Some(c) = client_ref.as_deref() {
        if let Some(r) = settle_pending(cw, c, sales).await {
            return r;
        }
        let pending = PendingRec { client_ref: c.to_owned(), ts_unix_ms: now_ms(), price_msat: price, txid: None };
        if let Err(e) = sales.lock().unwrap().set_pending(pending) {
            return Reply::fail(500, "internal", format!("could not record the sale before charging: {e:#}"));
        }
    }
    let started = Instant::now();
    let result = cw.spend(price).await;
    // Whatever happened, recovered ecash is never part of an API answer (it is in cashouts.log).
    cw.recovered_cashouts.clear();
    let r: SpendReport = match result {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("charge of {price} msat failed: {e:#}");
            if let Some(c) = client_ref.as_deref() {
                let mut store = sales.lock().unwrap();
                // Something left in flight keeps the pending record, now with its txid.
                match cw.in_flight().into_iter().next() {
                    Some(t) => {
                        let _ = store.set_pending(PendingRec {
                            client_ref: c.to_owned(),
                            ts_unix_ms: now_ms(),
                            price_msat: price,
                            txid: Some(t),
                        });
                    }
                    None => {
                        let _ = store.clear_pending(c);
                    }
                }
            }
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
    // The money has moved: a ledger failure must not turn a paid sale into an error.
    match sales.lock().unwrap().append(rec.clone()) {
        Ok(saved) => rec = saved,
        Err(e) => rec.warnings.push(format!("sale not recorded in sales.json: {e:#}")),
    }
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
/// is in flight right now is answered `undecided`, never `busy`, and is not queued.
async fn run_job(h: &TillHandle, client_ref: Option<&str>, make: impl FnOnce(oneshot::Sender<Reply>) -> Job) -> Reply {
    {
        let mut inflight = h.inflight.lock().unwrap();
        if h.busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            if let Some(c) = client_ref {
                let same = inflight.as_deref() == Some(c) || h.sales.lock().unwrap().find_pending(c).is_some();
                if same {
                    return undecided_reply(None);
                }
            }
            return Reply::fail(409, "busy", "another charge or balance call is in flight");
        }
        *inflight = client_ref.map(str::to_owned);
    }
    let (tx, rx) = oneshot::channel();
    if h.tx.try_send(make(tx)).is_err() {
        *h.inflight.lock().unwrap() = None;
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
    if let Some(r) = replay(&h.sales, req.client_ref.as_deref()) {
        return r;
    }
    let cref = req.client_ref.clone();
    run_job(&h, cref.as_deref(), |reply| Job::Charge { price, client_ref: req.client_ref, reply }).await
}

async fn card_balance(State(h): State<TillHandle>) -> Reply {
    run_job(&h, None, |reply| Job::Balance { reply }).await
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
