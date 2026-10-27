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
    let federation_name = cw.wallet.client.config().await.global.meta.get("federation_name").cloned();
    let handle = TillHandle {
        tx,
        busy: busy.clone(),
        wallet: cw.wallet.clone(),
        relay,
        sales: sales.clone(),
        federation_name,
    };
    (handle, actor(cw, rx, busy, sales, opener))
}

async fn actor(
    mut cw: CardWallet,
    mut rx: mpsc::Receiver<Job>,
    busy: Arc<AtomicBool>,
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
                    Err(e) => error_reply(&e),
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
    Some(Reply::ok(charge_body(&rec, &[], 0)))
}

/// The testable core of `POST /v1/charge`: spend `price` from `cw`'s card and record the sale.
/// A `client_ref` seen before returns the earlier sale without touching the card.
pub async fn charge_with(
    cw: &mut CardWallet,
    price: u64,
    client_ref: Option<String>,
    sales: &Mutex<SalesStore>,
) -> ChargeResponse {
    if let Some(bad) = validate_price(price) {
        return bad;
    }
    if let Some(r) = replay(sales, client_ref.as_deref()) {
        return r;
    }
    let started = Instant::now();
    let result = cw.spend(price).await;
    // Whatever happened, recovered ecash is never part of an API answer (it is in cashouts.log).
    cw.recovered_cashouts.clear();
    let r: SpendReport = match result {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("charge of {price} msat failed: {e:#}");
            return error_reply(&e);
        }
    };
    let rec = SaleRec {
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
    };
    let mut warnings = r.warnings.clone();
    // The money has moved: a ledger failure must not turn a paid sale into an error.
    let rec = match sales.lock().unwrap().append(rec.clone()) {
        Ok(saved) => saved,
        Err(e) => {
            warnings.push(format!("sale not recorded in sales.json: {e:#}"));
            rec
        }
    };
    Reply::ok(charge_body(&rec, &warnings, started.elapsed().as_millis() as u64))
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

/// Admit a job (or say busy) and wait for the actor's answer.
async fn run_job(h: &TillHandle, make: impl FnOnce(oneshot::Sender<Reply>) -> Job) -> Reply {
    if h.busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return Reply::fail(409, "busy", "another charge or balance call is in flight");
    }
    let (tx, rx) = oneshot::channel();
    if h.tx.try_send(make(tx)).is_err() {
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
    run_job(&h, |reply| Job::Charge { price, client_ref: req.client_ref, reply }).await
}

async fn card_balance(State(h): State<TillHandle>) -> Reply {
    run_job(&h, |reply| Job::Balance { reply }).await
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
