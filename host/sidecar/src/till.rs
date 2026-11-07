//! The till service: a small HTTP API (see `docs/TILL-API.md`) in front of the card wallet.
//!
//! [`CardWallet`] is not `Send` (it holds a `Box<dyn Card>` across awaits), so exactly one actor
//! future owns it and handlers talk to the actor through a channel. The actor is polled by the
//! task that calls [`serve`], on the same runtime as the CLI commands, so the blocking card calls
//! keep using `on_card` unchanged.
//!
//! With ops configured (`serve --ops-url`), a card job (charge, balance, issue) first reads the
//! card's short id and switches the wallet to that card's file, loaded from the registry; the
//! writes it owes the registry go out at the end of the job (`crate::ops`). The sales ledger and
//! the till's own outputs stay local either way.
//!
//! A till may also be on ops without a token yet (`docs/TILL-CLAIM.md`): it then serves
//! `/v1/status` saying so and refuses card jobs with `ops_not_connected`, while a claim poller
//! waits for an operator's approval and puts the till on ops without a restart ([`Conn`]).
//!
//! At most one job (charge or card balance) runs at a time. The busy flag is set by the handler
//! that admits the job and cleared by the actor when the job is finished, so a client that
//! disconnects mid-charge cannot let a second charge start while the first is in flight.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Query, State};
use axum::extract::ws::WebSocketUpgrade;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use fedimint_card_host_card::{
    BridgeError, BridgeTransport, Card, CardClient, PIN_MAX, PIN_MIN, SW_PIN_BLOCKED, StatusWord, Transport, status_word,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::link::{Link, Links};
use crate::glue::{BalanceReport, CardWallet, PromotedSale, SpendFault, SpendReport, on_card};
use crate::claim::{Claim, Poll};
use crate::ops::{FederationReport, Ops, OpsConfig, OpsError};
use crate::plan::PlanError;
use crate::sales::{SaleRec, SalesStore, SlotMsat};
use crate::state::NoteStatus;
use crate::topup::{TopupRec, TopupState, Topups};
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
        if let Some(o) = cause.downcast_ref::<crate::offline::OfflineError>() {
            use crate::offline::OfflineError as E;
            return match o {
                E::Unsupported(_) => c(409, "offline_unsupported_card"),
                E::NotReady(_) => c(503, "offline_not_ready"),
                E::NotGenuine(_) => c(403, "card_not_genuine"),
                E::Cap(_) => c(503, "offline_cap_reached"),
            };
        }
        if let Some(o) = cause.downcast_ref::<OpsError>() {
            return match o {
                OpsError::Changed(_) => c(409, "card_changed"),
                OpsError::UnknownCard(_) => c(404, "unknown_card"),
                OpsError::Unreachable(_) => c(502, "ops_unreachable"),
                OpsError::Refused(..) => c(502, "ops_refused"),
            };
        }
        if let Some(StatusWord(sw)) = cause.downcast_ref::<StatusWord>() {
            match *sw {
                0x63C0..=0x63CF => return c(403, "wrong_pin"),
                SW_PIN_BLOCKED => return c(423, "pin_blocked"),
                fedimint_card_host_card::v3::SW_OVER_LIMIT => return c(402, "card_limit"),
                _ => {}
            }
        }
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
                PlanError::NoExactPlan { .. }
                | PlanError::TooManyNotes(_)
                | PlanError::TooManyInputs { .. }
                | PlanError::NoFreeSlots { .. } => {
                    c(422, "no_exact_plan")
                }
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
    let detail = match class.code {
        // Said plainly, for the holder: the status word is the card's, the tries left are its low
        // nibble (`63 CX`).
        "wrong_pin" => {
            let left = err.chain().find_map(|c| c.downcast_ref::<StatusWord>()).map_or(0, |s| s.0 & 0x0F);
            format!("wrong PIN; {left} tries left before the card blocks")
        }
        "pin_blocked" => "the card's PIN is blocked after too many wrong tries; the card cannot pay".into(),
        _ => format!("{err:#}"),
    };
    Reply::fail(class.status, class.code, detail)
}

// ------------------------------------------------------------------ the till and the registry

/// Where the till stands with the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpsState {
    /// No registry configured: card state in `card_state.json`, as before ops.
    Local,
    /// On ops: card jobs load each card from the registry.
    Connected { url: String },
    /// No token yet; a claim is waiting for an operator's approval.
    Waiting { url: String, code: String, expires_at_ms: u64 },
    /// No token, and the claim expired (or the registry no longer knows it).
    ClaimExpired { url: String, code: String },
    /// No token and no claim.
    NotConnected { url: Option<String> },
}

impl OpsState {
    pub fn name(&self) -> &'static str {
        match self {
            OpsState::Local => "local",
            OpsState::Connected { .. } => "connected",
            OpsState::Waiting { .. } => "waiting_for_approval",
            OpsState::ClaimExpired { .. } => "claim_expired",
            OpsState::NotConnected { .. } => "not_connected",
        }
    }

    /// The state a claim file gives a till with no token.
    pub fn of_claim(claim: Option<&Claim>, url: Option<String>, now_ms: u64) -> Self {
        match claim {
            None => OpsState::NotConnected { url },
            Some(c) if c.expired(now_ms) => OpsState::ClaimExpired { url: c.ops_url.clone(), code: c.code.clone() },
            Some(c) => OpsState::Waiting { url: c.ops_url.clone(), code: c.code.clone(), expires_at_ms: c.expires_at },
        }
    }

    fn url(&self) -> Option<&str> {
        match self {
            OpsState::Local => None,
            OpsState::Connected { url } | OpsState::Waiting { url, .. } | OpsState::ClaimExpired { url, .. } => Some(url),
            OpsState::NotConnected { url } => url.as_deref(),
        }
    }
}

/// The till's registry connection, shared by the handlers, the actor and the claim poller.
pub struct Conn {
    pub state: OpsState,
    /// Set exactly when `state` is `Connected`.
    pub ops: Option<Ops>,
    /// A newer claim waiting while the till is already connected (`till claim --replace`).
    pub new_claim: Option<(String, u64)>,
    /// `--ops-url`, for a till with neither token nor claim.
    pub default_url: Option<String>,
}

pub type SharedConn = Arc<Mutex<Conn>>;

impl Conn {
    pub fn new(state: OpsState, ops: Option<Ops>) -> SharedConn {
        let default_url = state.url().map(str::to_owned);
        Arc::new(Mutex::new(Conn { state, ops, new_claim: None, default_url }))
    }

    /// The refusal for a card job while a till that belongs on ops has no token.
    fn refusal(&self) -> Option<Reply> {
        let detail = match &self.state {
            OpsState::Local | OpsState::Connected { .. } => return None,
            OpsState::Waiting { code, .. } => {
                format!("this till is waiting for an operator to approve claim {code} in the provisioner app")
            }
            OpsState::ClaimExpired { code, .. } => format!(
                "claim {code} expired before an operator approved it; start a new one (`till claim`, or Connect to registry on StartOS)"
            ),
            OpsState::NotConnected { .. } => {
                "this till is not connected to a registry; connect it with `till claim` (Connect to registry on StartOS)".into()
            }
        };
        Some(Reply::fail(503, "ops_not_connected", detail))
    }

    fn status_json(&self) -> Value {
        let (code, expires) = match &self.state {
            OpsState::Waiting { code, expires_at_ms, .. } => (Some(code.clone()), Some(*expires_at_ms)),
            OpsState::ClaimExpired { code, .. } => (Some(code.clone()), None),
            _ => match &self.new_claim {
                Some((c, e)) => (Some(c.clone()), Some(*e)),
                None => (None, None),
            },
        };
        json!({
            "connected": matches!(self.state, OpsState::Connected { .. }),
            "ops_state": self.state.name(),
            "ops_url": self.state.url(),
            "claim_code": code,
            "claim_expires_at_ms": expires,
        })
    }
}

/// One look at the claim file, and at the registry about it. On approval: keep the token
/// (`till-ops.json`), report the federation, and put the till on ops. `dead` remembers claims the
/// registry answered 404, so they are not asked about again.
///
/// The registry decides whether a claim is still live, not the claim file's `expires_at`: an
/// approved claim waits for its till for days (registry `APPROVED_CLAIM_TTL_MS`), so a till that
/// was not running when the operator approved still collects its token on its first poll.
pub async fn claim_tick(conn: &SharedConn, dir: &Path, report: Option<&FederationReport>, dead: &mut HashSet<String>) {
    let claim = match crate::claim::read_claim(dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: {e:#}");
            return;
        }
    };
    let now = crate::claim::now_ms();
    let set = |state: OpsState, new_claim: Option<(String, u64)>| {
        let mut g = conn.lock().unwrap();
        if g.ops.is_some() {
            g.new_claim = new_claim;
        } else {
            g.state = state;
        }
    };
    let default_url = conn.lock().unwrap().default_url.clone();
    let Some(c) = claim else {
        // A till serving without a registry stays that way until a claim file appears.
        if !matches!(conn.lock().unwrap().state, OpsState::Local) {
            set(OpsState::NotConnected { url: default_url }, None);
        }
        return;
    };
    let expired = OpsState::ClaimExpired { url: c.ops_url.clone(), code: c.code.clone() };
    if dead.contains(&c.claim_id) {
        set(expired, None);
        return;
    }
    let waiting = OpsState::of_claim(Some(&c), None, now);
    match crate::claim::poll(&c).await {
        Ok(Poll::Pending) => set(waiting, Some((c.code.clone(), c.expires_at))),
        Ok(Poll::Gone) => {
            dead.insert(c.claim_id.clone());
            set(expired, None);
        }
        Ok(Poll::Approved { till_id, token }) => {
            // The registry answers the token once: if it cannot be saved, use it anyway.
            let cfg = match crate::claim::adopt(dir, &c, token.clone()) {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("warning: could not save the till token ({e:#}); the till is on ops until it restarts");
                    OpsConfig { url: c.ops_url.clone(), token }
                }
            };
            let ops = match Ops::new(&cfg, dir) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("warning: {e:#}");
                    return;
                }
            };
            if let Some(r) = report {
                crate::ops::report_federation(&ops.client, r).await;
            }
            let mut g = conn.lock().unwrap();
            g.ops = Some(ops);
            g.state = OpsState::Connected { url: cfg.url.clone() };
            g.new_claim = None;
            eprintln!("claim {} approved: on ops at {} as till {till_id}", c.code, cfg.url);
        }
        Err(e) => {
            // Unreachable for now; the claim is still live.
            eprintln!("warning: polling claim {}: {e:#}", c.code);
            set(waiting, Some((c.code.clone(), c.expires_at)));
        }
    }
}

/// [`claim_tick`] every `every`, for as long as the till serves.
pub async fn claim_poller(conn: SharedConn, dir: PathBuf, report: Option<FederationReport>, every: Duration) {
    let mut dead = HashSet::new();
    loop {
        claim_tick(&conn, &dir, report.as_ref(), &mut dead).await;
        tokio::time::sleep(every).await;
    }
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
///
/// The probe comes first: `GET DATA FF21` distinguishes "no card in the field" (the relay answers
/// 503, a transport error) from "card present, applet absent" (a status word).
///
/// The SELECT is then attempted, and `6A82` is tolerated — a blank card answers it, and that is
/// the normal first state of a card in a batch, not a failure. Skipping the SELECT altogether was
/// a bug: every applet command after it (`GET_PUB`, `SIGN`, `VERIFY_PIN`) went to the **card
/// manager** instead of the applet, which answers `6E00` (CLA not supported) — so a charge from the
/// phone died on its first `GET_PUB` while the same charge from a laptop worked, because the laptop
/// path had selected the applet when it opened.
pub fn open_relay_card(relay: &str) -> Result<Box<dyn Card>> {
    open_card_over(BridgeTransport::new(relay), &format!("card relay at {relay}"))
}

/// The card behind a POS phone's link (`docs/POS-LINK.md`), opened as [`open_relay_card`] opens
/// the relay's. Blocking.
pub fn open_link_card(link: &Link) -> Result<Box<dyn Card>> {
    open_card_over(link.transport(), &format!("card link {}", link.id))
}

/// [`open_relay_card`] over any transport: probe, then SELECT (a `6A82` is tolerated).
pub fn open_card_over<T: Transport + 'static>(transport: T, what: &str) -> Result<Box<dyn Card>> {
    let mut c = CardClient::new(transport);
    c.get_data_ff21()
        .or_else(|e| match status_word(&e) {
            Some(_) => Ok(vec![]),
            None => Err(e),
        })
        .with_context(|| what.to_owned())?;
    // A blank card, or one whose applet is absent, answers a status word; that is `precheck`'s
    // subject, not a reason to refuse to hand the card over.
    let _ = c.select();
    Ok(Box::new(c))
}

/// How the actor gets a card for each job.
pub type CardOpener = Box<dyn Fn() -> Result<Box<dyn Card>>>;

// ------------------------------------------------------------------ jobs and the actor

enum Job {
    /// `pin` is the holder's, typed at the till. Never logged, stored or sent to ops.
    /// `link`: the POS phone's card link (`X-Card-Link`), used instead of the opener.
    Charge {
        price: u64,
        client_ref: Option<String>,
        pin: Option<Vec<u8>>,
        link: Option<Link>,
        reply: oneshot::Sender<Reply>,
    },
    Balance { link: Option<Link>, reply: oneshot::Sender<Reply> },
    /// Send charges taken offline (docs/OFFLINE-PROTOCOL.md § 4.5).
    SendOffline { reply: oneshot::Sender<Reply> },
    Precheck { reply: oneshot::Sender<Reply> },
    KeySetup { pin: Option<Vec<u8>>, reply: oneshot::Sender<Reply> },
    Issue { amounts: Vec<u64>, client_ref: Option<String>, reply: oneshot::Sender<Reply> },
    /// Load a paid top-up's eCash onto the card (docs/TOPUP.md).
    TopupLoad {
        topup_id: String,
        amount_msat: u64,
        ops_url: String,
        link: Option<Link>,
        topups: Arc<Topups>,
        reply: oneshot::Sender<Reply>,
    },
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
    /// The registry connection. On ops every card has a holder PIN, so a charge without one would
    /// verify `CARD_PIN` instead and spend one of the holder's tries.
    conn: SharedConn,
    /// Open card links from POS phones (`GET /v1/card/link`).
    links: Arc<Links>,
    /// The POS token: with one, `/v1/*` needs it as a bearer (from loopback too, if set so).
    pos_auth: crate::pos::PosAuth,
    /// Top-ups by Lightning ([`Self::with_topups`]); without them the top-up routes answer 503.
    topups: Option<Arc<Topups>>,
    /// Offline charges ([`Self::with_offline`]); shared with the actor.
    offline: SharedOffline,
}

/// The till's offline charges, once [`TillHandle::with_offline`] set them.
pub type SharedOffline = Arc<Mutex<Option<Arc<crate::offline::Offline>>>>;

impl TillHandle {
    /// Take 3.0 cards offline when the registry or the federation does not answer, and send what
    /// was taken once they do (docs/OFFLINE-PROTOCOL.md § 4).
    pub fn with_offline(self, off: Arc<crate::offline::Offline>) -> Self {
        *self.offline.lock().unwrap() = Some(off);
        self
    }

    /// Send the charges taken offline now, as the background sender does: one job, answered with
    /// what it did.
    pub async fn send_offline(&self) -> Reply {
        run_job(self, None, |reply| Job::SendOffline { reply }).await
    }

    /// Whether a job is in flight.
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    /// Require `token` as a bearer on every `/v1/*` request not from loopback.
    pub fn with_pos_token(mut self, token: Option<String>) -> Self {
        self.pos_auth = crate::pos::PosAuth { token: token.map(Arc::from), everywhere: false };
        self
    }

    /// Require `token` as a bearer on every `/v1/*` request, from loopback too
    /// (`till serve --require-pos-token`, for a till behind a proxy).
    pub fn with_pos_token_everywhere(mut self, token: String) -> Self {
        self.pos_auth = crate::pos::PosAuth { token: Some(Arc::from(token)), everywhere: true };
        self
    }

    /// Take top-ups by Lightning (docs/TOPUP.md), and watch the invoices still unpaid.
    pub fn with_topups(mut self, topups: Arc<Topups>) -> Self {
        topups.watch_waiting();
        self.topups = Some(topups);
        self
    }

    /// The open card links.
    pub fn links(&self) -> Arc<Links> {
        self.links.clone()
    }

    /// The registry connection.
    pub fn conn(&self) -> SharedConn {
        self.conn.clone()
    }
}

/// Build the handle and the actor future. The caller must poll the future (it is not `Send`)
/// for as long as the handle's router is serving; it ends when every handle clone is dropped.
pub async fn new(
    cw: CardWallet,
    relay: String,
    sales: SalesStore,
    opener: CardOpener,
) -> (TillHandle, impl std::future::Future<Output = ()>) {
    new_with_ops(cw, relay, sales, opener, None).await
}

/// [`new`] for a till on ops: every card job loads the card's state from the registry.
pub async fn new_with_ops(
    cw: CardWallet,
    relay: String,
    sales: SalesStore,
    opener: CardOpener,
    ops: Option<Ops>,
) -> (TillHandle, impl std::future::Future<Output = ()>) {
    let state = match &ops {
        Some(o) => OpsState::Connected { url: o.client.url().to_owned() },
        None => OpsState::Local,
    };
    new_with_conn(cw, relay, sales, opener, Conn::new(state, ops)).await
}

/// [`new`] with a registry connection that can change while the till runs ([`claim_poller`]).
pub async fn new_with_conn(
    cw: CardWallet,
    relay: String,
    sales: SalesStore,
    opener: CardOpener,
    conn: SharedConn,
) -> (TillHandle, impl std::future::Future<Output = ()>) {
    let ops = conn.lock().unwrap().ops.clone();
    let (tx, rx) = mpsc::channel(1);
    let busy = Arc::new(AtomicBool::new(false));
    let sales = Arc::new(Mutex::new(sales));
    let inflight = Arc::new(Mutex::new(None));
    let pending_refs = Arc::new(Mutex::new(refs_now(&cw, ops.as_ref())));
    let federation_name = cw.wallet.client.config().await.global.meta.get("federation_name").cloned();
    let offline: SharedOffline = Arc::new(Mutex::new(None));
    let handle = TillHandle {
        tx,
        busy: busy.clone(),
        inflight: inflight.clone(),
        pending_refs: pending_refs.clone(),
        wallet: cw.wallet.clone(),
        relay,
        sales: sales.clone(),
        federation_name,
        conn: conn.clone(),
        links: Arc::new(Links::default()),
        pos_auth: crate::pos::PosAuth::default(),
        topups: None,
        offline: offline.clone(),
    };
    (handle, actor(cw, rx, busy, inflight, pending_refs, sales, opener, conn, offline))
}

/// `client_ref`s of unresolved charges: in the wallet's file, or on ops in every card file.
fn refs_now(cw: &CardWallet, ops: Option<&Ops>) -> Vec<String> {
    match ops {
        Some(o) => o.pending_refs(),
        None => refs_of(cw),
    }
}

#[allow(clippy::too_many_arguments)]
async fn actor(
    mut cw: CardWallet,
    mut rx: mpsc::Receiver<Job>,
    busy: Arc<AtomicBool>,
    inflight: Arc<Mutex<Option<String>>>,
    pending_refs: Arc<Mutex<Vec<String>>>,
    sales: Arc<Mutex<SalesStore>>,
    opener: CardOpener,
    conn: SharedConn,
    offline: SharedOffline,
) {
    ensure_hook(&mut cw, &sales);
    // The data directory: the holder loader journals top-up loads there.
    let dir = cw.store.path().parent().map(Path::to_owned).unwrap_or_default();
    while let Some(job) = rx.recv().await {
        // Read per job: the claim poller may have put the till on ops since the last one.
        let ops = conn.lock().unwrap().ops.clone();
        let off = offline.lock().unwrap().clone();
        let (reply_tx, reply) = match job {
            Job::SendOffline { reply } => {
                let r = match (ops.as_ref(), off.as_ref()) {
                    (Some(o), Some(f)) => send_offline_job(&mut cw, o, f, &sales).await,
                    _ => Reply::ok(json!({"ok": true, "reported": [], "settled": [], "double_spent": [], "refused": [], "errors": []})),
                };
                (reply, r)
            }
            Job::Charge { price, client_ref, pin, link, reply } => {
                let r = match on_card(|| open_for(&opener, link.as_ref())) {
                    Ok(card) => {
                        cw.card = card;
                        cw.pin = pin;
                        charge_job_with(&mut cw, ops.as_ref(), off.as_deref(), price, client_ref, &sales).await
                    }
                    Err(e) => {
                        // A repeat of an earlier attempt can be answered without a card.
                        let settled = match client_ref.as_deref() {
                            Some(c) => {
                                cw.card = Box::new(NoCard);
                                settle_without_card(&mut cw, ops.as_ref(), c, price, &sales).await
                            }
                            None => None,
                        };
                        settled.unwrap_or_else(|| error_reply(&e))
                    }
                };
                (reply, r)
            }
            Job::Balance { link, reply } => {
                let r = match on_card(|| open_for(&opener, link.as_ref())) {
                    Ok(card) => {
                        cw.card = card;
                        card_balance_job(&mut cw, ops.as_ref(), &sales).await
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
                        match ops.as_ref() {
                            None => issue_job(&mut cw, &amounts, client_ref).await,
                            Some(o) => match o.attach(&mut cw).await {
                                Ok(base) => {
                                    let r = issue_job(&mut cw, &amounts, client_ref).await;
                                    Ops::finish(&mut cw, base).await;
                                    r
                                }
                                Err(e) => error_reply(&e),
                            },
                        }
                    }
                    Err(e) => error_reply(&e),
                };
                (reply, r)
            }
            Job::TopupLoad { topup_id, amount_msat, ops_url, link, topups, reply } => {
                let keyer = ops.as_ref().map(|o| o.client.clone());
                let r = topup_load_job(&cw.wallet, &dir, &opener, link.as_ref(), &topups, &topup_id, amount_msat, &ops_url, keyer).await;
                (reply, r)
            }
        };
        cw.card = Box::new(NoCard);
        // The holder's PIN lives for one job.
        cw.pin = None;
        *pending_refs.lock().unwrap() = refs_now(&cw, ops.as_ref());
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

/// Load top-up `id` onto the card. First finish any load an earlier run left (a top-up's own load
/// cut short is among them, found by its tag), so a top-up is never loaded twice; then load.
#[allow(clippy::too_many_arguments)]
async fn topup_load_job(
    wallet: &Wallet,
    dir: &Path,
    opener: &CardOpener,
    link: Option<&Link>,
    topups: &Topups,
    id: &str,
    amount_msat: u64,
    ops_url: &str,
    keyer: Option<crate::ops::OpsClient>,
) -> Reply {
    let loader = match crate::holder::Loader::new(wallet, ops_url, dir) {
        // With the till's own client, a split-keyed card's unkeyed slots are keyed at this tap.
        Ok(l) => match keyer {
            Some(k) => l.with_tag(id).with_keyer(k),
            None => l.with_tag(id),
        },
        Err(e) => return error_reply(&e),
    };
    match loader.resume_each().await {
        Ok(done) => {
            for f in done {
                if let (Some(tag), Some(_)) = (&f.load.tag, &f.live) {
                    mark_loaded(topups, tag, &f.load.txid, &f.load.short_id);
                }
            }
        }
        Err(e) => return error_reply(&e),
    }
    match topups.get(id) {
        Some(t) if t.state == TopupState::Loaded => return Reply::ok(t.to_json()),
        Some(t) if t.state == TopupState::Paid => {}
        Some(t) => return topup_not_loadable(&t).unwrap_or_else(|| Reply::ok(t.to_json())),
        None => return unknown_topup(),
    }
    let mut card = match on_card(|| open_for(opener, link)) {
        Ok(c) => c,
        Err(e) => return error_reply(&e),
    };
    match loader.load(card.as_mut(), &[amount_msat]).await {
        Ok(r) => {
            mark_loaded(topups, id, &r.txid, &r.short_id);
            match topups.get(id) {
                Some(t) => Reply::ok(t.to_json()),
                None => unknown_topup(),
            }
        }
        Err(e) => match e.downcast_ref::<crate::holder::CardFull>() {
            Some(full) => Reply::fail(
                409,
                "card_full",
                format!("{full}: spend on the card, then tap again; the top-up stays paid"),
            ),
            None => error_reply(&e),
        },
    }
}

fn mark_loaded(topups: &Topups, id: &str, txid: &str, short_id: &str) {
    let r = topups.store.lock().unwrap().update(id, |t| {
        t.state = TopupState::Loaded;
        t.load_txid = Some(txid.to_owned());
        t.short_id = Some(short_id.to_owned());
        t.detail = None;
    });
    if let Err(e) = r {
        eprintln!("top-up {id}: loaded in {txid} but could not record it: {e:#}");
    }
}

fn unknown_topup() -> Reply {
    Reply::fail(404, "unknown_topup", "no such top-up on this till")
}

/// Why a top-up in this state cannot be loaded; `None` for `paid` and `loaded`.
fn topup_not_loadable(t: &TopupRec) -> Option<Reply> {
    match t.state {
        TopupState::Paid | TopupState::Loaded => None,
        TopupState::Waiting => Some(Reply::fail(409, "not_paid", "the invoice is not paid yet")),
        TopupState::Expired => Some(Reply::fail(410, "expired", "the invoice expired unpaid; make a new one")),
        TopupState::Failed => Some(Reply::fail(
            409,
            "topup_failed",
            t.detail.clone().unwrap_or_else(|| "the Lightning payment did not complete".into()),
        )),
    }
}

/// A job's card: the POS phone's link when the job came with one, else the opener (the relay).
fn open_for(opener: &CardOpener, link: Option<&Link>) -> Result<Box<dyn Card>> {
    match link {
        Some(l) => open_link_card(l),
        None => opener(),
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
        Ok(()) => match {
            if cw.ops.is_none() {
                cw.card_tick();
            }
            cw.balance().await
        } {
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

/// `POST /v1/charge` with the card in `cw.card`: on ops, the card's state is loaded from the
/// registry first and the writes it owes go back after; without ops this is [`charge_with`].
pub async fn charge_job(
    cw: &mut CardWallet,
    ops: Option<&Ops>,
    price: u64,
    client_ref: Option<String>,
    sales: &Arc<Mutex<SalesStore>>,
) -> Reply {
    charge_job_with(cw, ops, None, price, client_ref, sales).await
}

/// [`charge_job`] for a till that takes cards offline (`off`): when the registry or the
/// federation does not answer within 2 s, a 3.0 card is charged offline
/// ([`crate::offline::charge`]); while both answer, charges taken offline for any card are reported
/// first, so the registry knows which notes they spent.
pub async fn charge_job_with(
    cw: &mut CardWallet,
    ops: Option<&Ops>,
    off: Option<&crate::offline::Offline>,
    price: u64,
    client_ref: Option<String>,
    sales: &Arc<Mutex<SalesStore>>,
) -> Reply {
    let Some(ops) = ops else { return charge_with(cw, price, client_ref, sales).await };
    if let Some(bad) = validate_price(price) {
        return bad;
    }
    if let Some(off) = off {
        let (reg, fed) = off.reach.probe(Some(ops), &cw.wallet, &off.dir).await;
        if reg != Some(true) || !fed {
            return offline_charge(cw, ops, off, price, client_ref, sales).await;
        }
        let mut sent = crate::offline::Sent::default();
        crate::offline::report(off, ops, None, &mut sent).await;
        mark_offline_sales(off, sales, &sent);
    }
    let base = match ops.attach(cw).await {
        Ok(b) => b,
        Err(e) => {
            if let (Some(off), true) = (off, crate::ops::unanswered(&e)) {
                // The probe said up a moment ago; the registry went quiet since.
                off.reach.registry_down();
                return offline_charge(cw, ops, off, price, client_ref, sales).await;
            }
            return error_reply(&e);
        }
    };
    let r = charge_with(cw, price, client_ref, sales).await;
    Ops::finish(cw, base).await;
    r
}

/// The offline path of a charge: take it, record the sale (`offline`), answer 200 with
/// `"offline": true`.
async fn offline_charge(
    cw: &mut CardWallet,
    ops: &Ops,
    off: &crate::offline::Offline,
    price: u64,
    client_ref: Option<String>,
    sales: &Arc<Mutex<SalesStore>>,
) -> Reply {
    // A repeat of a sale already taken (offline or not) is that sale.
    if let Some(r) = replay(sales, client_ref.as_deref(), price) {
        return r;
    }
    let started = Instant::now();
    let taken = match crate::offline::charge(cw, off, ops, price, client_ref.as_deref()).await {
        Ok(t) => t.charge,
        Err(e) => {
            tracing::warn!("offline charge of {price} msat refused: {e:#}");
            return error_reply(&e);
        }
    };
    let mut rec = SaleRec {
        id: 0,
        ts_unix_ms: taken.ts_unix_ms,
        price_msat: taken.price_msat,
        received_msat: 0,
        overpaid_msat: taken.overpay_msat,
        fees_msat: taken.fees_msat,
        txid: taken.txid.clone(),
        inputs: taken.inputs.iter().map(|i| SlotMsat { slot: i.slot, msat: i.amount_msat }).collect(),
        change: taken.change.iter().map(|c| SlotMsat { slot: c.slot, msat: c.amount_msat }).collect(),
        collection_pending: true,
        client_ref,
        duration_ms: started.elapsed().as_millis() as u64,
        warnings: vec![],
        offline: true,
        double_spent: false,
        rejected: false,
    };
    match sales.lock().unwrap().append(rec.clone()) {
        Ok(saved) => rec = saved,
        Err(e) => rec.warnings.push(format!("sale not recorded in sales.json: {e:#}")),
    }
    Reply::ok(charge_body(&rec, &rec.warnings, rec.duration_ms))
}

/// Bring the sales of offline charges in line with what the sender found.
fn mark_offline_sales(off: &crate::offline::Offline, sales: &Mutex<SalesStore>, sent: &crate::offline::Sent) {
    let mut s = sales.lock().unwrap();
    for t in &sent.double_spent {
        if let Err(e) = s.update(t, |r| r.double_spent = true) {
            tracing::warn!("marking sale {t} double spent: {e:#}");
        }
    }
    for t in &sent.rejected {
        if let Err(e) = s.update(t, |r| r.rejected = true) {
            tracing::warn!("marking sale {t} rejected: {e:#}");
        }
    }
    for t in &sent.settled {
        let r = s.update(t, |r| {
            if r.collection_pending {
                r.collection_pending = false;
                if r.received_msat == 0 {
                    r.received_msat = r.price_msat + r.overpaid_msat;
                }
            }
        });
        if let Err(e) = r {
            tracing::warn!("settling sale {t}: {e:#}");
        }
    }
    let _ = off;
}

/// The sender as one job: report queued charges, then settle reported ones through the
/// federation (docs/OFFLINE-PROTOCOL.md § 4.5).
pub async fn send_offline_job(cw: &mut CardWallet, ops: &Ops, off: &crate::offline::Offline, sales: &Arc<Mutex<SalesStore>>) -> Reply {
    ensure_hook(cw, sales);
    let mut sent = crate::offline::Sent::default();
    crate::offline::report(off, ops, None, &mut sent).await;
    crate::offline::settle(cw, off, ops, &mut sent).await;
    sync_sales(cw, sales);
    cw.recovered_cashouts.clear();
    mark_offline_sales(off, sales, &sent);
    Reply::ok(json!({
        "ok": true,
        "reported": sent.reported,
        "settled": sent.settled,
        "double_spent": sent.double_spent,
        "refused": sent.refused,
        "rejected": sent.rejected,
        "errors": sent.errors,
    }))
}

/// `POST /v1/card/balance` with the card in `cw.card`, through ops when it is configured.
pub async fn card_balance_job(cw: &mut CardWallet, ops: Option<&Ops>, sales: &Arc<Mutex<SalesStore>>) -> Reply {
    let Some(ops) = ops else { return balance_job(cw, sales).await };
    let base = match ops.attach(cw).await {
        Ok(b) => b,
        Err(e) => return error_reply(&e),
    };
    let mut r = balance_job(cw, sales).await;
    // The card's id lets a top-up started from this screen fit the card's free slots.
    if let (Some(link), Some(body)) = (cw.ops.as_ref(), r.body.as_object_mut()) {
        if r.status == 200 {
            body.insert("short_id".into(), json!(link.short_id));
            // Slots with no key yet (split keying): room too, keyed by the tap that needs them.
            body.insert("unkeyed_slots".into(), json!(link.unkeyed));
        }
    }
    Ops::finish(cw, base).await;
    r
}

/// A repeated `client_ref` while no card is in the field. On ops, the card is the one whose file
/// holds the ref; its state is loaded from the registry by short id.
async fn settle_without_card(
    cw: &mut CardWallet,
    ops: Option<&Ops>,
    client_ref: &str,
    price: u64,
    sales: &Arc<Mutex<SalesStore>>,
) -> Option<Reply> {
    let Some(ops) = ops else {
        let r = settle(cw, client_ref, price, sales).await;
        sync_sales(cw, sales);
        return r;
    };
    let id = ops.card_for_ref(client_ref)?;
    let base = match ops.attach_id(cw, &id).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("settling {client_ref} without the card: {e:#}");
            return None;
        }
    };
    let r = settle(cw, client_ref, price, sales).await;
    sync_sales(cw, sales);
    Ops::finish(cw, base).await;
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
    let mut b = charge_body_plain(s, warnings, duration_ms);
    if s.offline {
        b["offline"] = json!(true);
    }
    if s.double_spent {
        b["double_spent"] = json!(true);
    }
    if s.rejected {
        b["rejected"] = json!(true);
    }
    b
}

fn charge_body_plain(s: &SaleRec, warnings: &[String], duration_ms: u64) -> Value {
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
        offline: false,
        double_spent: false,
        rejected: false,
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
    // On ops the attach did this already.
    if cw.ops.is_none() {
        cw.card_tick();
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
        offline: false,
        double_spent: false,
        rejected: false,
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
    let auth = h.pos_auth.clone();
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/status", get(status))
        .route("/v1/charge", post(charge))
        .route("/v1/sales", get(sales_list))
        .route("/v1/card/balance", post(card_balance))
        .route("/v1/cards/precheck", post(cards_precheck))
        .route("/v1/cards/keySetup", post(cards_key_setup))
        .route("/v1/cards/issue", post(cards_issue))
        .route("/v1/card/link", get(card_link))
        .route("/v1/topup", post(topup_create))
        .route("/v1/topup/{topup_id}", get(topup_get))
        .route("/v1/topups/waiting", get(topups_waiting))
        .route("/v1/topup/{topup_id}/load", post(topup_load))
        .with_state(h)
        .layer(axum::middleware::from_fn_with_state(auth, crate::pos::auth))
}

/// `GET /v1/card/link`: a POS phone's card link (docs/POS-LINK.md).
async fn card_link(State(h): State<TillHandle>, ws: WebSocketUpgrade) -> Response {
    let links = h.links.clone();
    ws.on_upgrade(move |socket| crate::link::run(socket, links))
}

const LINK_HEADER: &str = "x-card-link";

/// The link a job names in `X-Card-Link`: `Ok(None)` without the header, `Err(409)` for an id
/// that is unknown or closed.
fn link_of(h: &TillHandle, headers: &HeaderMap) -> Result<Option<Link>, Reply> {
    let Some(v) = headers.get(LINK_HEADER) else { return Ok(None) };
    let id = v.to_str().unwrap_or_default().trim();
    match h.links.get(id) {
        Some(l) => Ok(Some(l)),
        None => Err(Reply::fail(409, "card_link_gone", "the card link is closed or unknown; tap the card again")),
    }
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
    let mut body = json!({
        "federation_id": h.wallet.federation_id().to_string(),
        "federation_name": h.federation_name,
        "wallet_balance_msat": balance,
        "card_relay_ok": relay_ok,
        "busy": h.busy.load(Ordering::SeqCst),
    });
    if let (Some(b), Value::Object(c)) = (body.as_object_mut(), h.conn.lock().unwrap().status_json()) {
        b.extend(c);
    }
    // Reachability (probed with 2 s timeouts, kept 10 s) and the offline queue.
    let ops = h.conn.lock().unwrap().ops.clone();
    let off = h.offline.lock().unwrap().clone();
    let (reg, fed) = match &off {
        Some(o) => o.reach.probe(ops.as_ref(), &h.wallet, &o.dir).await,
        None => crate::offline::Reach::default().probe(ops.as_ref(), &h.wallet, Path::new("")).await,
    };
    body["registry_reachable"] = json!(reg);
    body["federation_reachable"] = json!(fed);
    body["offline"] = match &off {
        Some(o) => o.status_json(),
        None => Value::Null,
    };
    Reply::ok(body)
}

#[derive(Deserialize)]
struct ChargeReq {
    price_msat: Option<u64>,
    client_ref: Option<String>,
    /// The holder's PIN, typed at the till. Replaces `CARD_PIN` for this charge.
    pin: Option<String>,
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

async fn charge(State(h): State<TillHandle>, headers: HeaderMap, body: Bytes) -> Reply {
    let link = match link_of(&h, &headers) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let r = charge_on(&h, link.clone(), body).await;
    // The link lives for one job: it ends once the job's reply exists.
    if let Some(l) = link {
        l.close();
    }
    r
}

async fn charge_on(h: &TillHandle, link: Option<Link>, body: Bytes) -> Reply {
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
    // Checked here, so a malformed PIN never reaches the card and costs no try. Never echoed.
    if req.pin.as_deref().is_some_and(|p| !(PIN_MIN..=PIN_MAX).contains(&p.len()) || !p.bytes().all(|b| b.is_ascii_digit())) {
        return Reply::fail(400, "bad_request", format!("pin must be {PIN_MIN} to {PIN_MAX} digits"));
    }
    // A repeat of a finished sale needs neither the card nor the busy flag.
    if let Some(r) = replay(&h.sales, req.client_ref.as_deref(), price) {
        return r;
    }
    let (refused, pin_required) = {
        let c = h.conn.lock().unwrap();
        (c.refusal(), c.ops.is_some())
    };
    if let Some(r) = refused {
        return r;
    }
    if pin_required && req.pin.is_none() {
        return Reply::fail(400, "pin_required", "this till charges only with the holder's PIN");
    }
    let cref = req.client_ref.clone();
    let pin = req.pin.map(String::into_bytes);
    run_job(h, cref.as_deref().map(|c| (c, price)), |reply| Job::Charge { price, client_ref: req.client_ref, pin, link, reply })
        .await
}

async fn card_balance(State(h): State<TillHandle>, headers: HeaderMap) -> Reply {
    let link = match link_of(&h, &headers) {
        Ok(l) => l,
        Err(r) => return r,
    };
    if let Some(r) = h.conn.lock().unwrap().refusal() {
        if let Some(l) = link {
            l.close();
        }
        return r;
    }
    let r = run_job(&h, None, |reply| Job::Balance { link: link.clone(), reply }).await;
    if let Some(l) = link {
        l.close();
    }
    r
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
    if let Some(r) = h.conn.lock().unwrap().refusal() {
        return r;
    }
    let cref = req.client_ref.clone();
    run_job(&h, cref.as_deref().map(|c| (c, 0)), |reply| Job::Issue { amounts: req.amounts_msat, client_ref: req.client_ref, reply })
        .await
}

// ------------------------------------------------------------------ top-ups (docs/TOPUP.md)

const TOPUP_MAX_CLIENT_REF: usize = 64;

#[derive(Deserialize)]
struct TopupReq {
    amount_msat: Option<u64>,
    client_ref: Option<String>,
    /// The card the top-up is for (`short_id` from `/v1/card/balance`): the amount is then rounded
    /// to fit that card's free slots, and a card with none is refused before any invoice exists.
    short_id: Option<String>,
}

fn topups_unavailable() -> Reply {
    Reply::fail(503, "lightning_unavailable", "this till takes no Lightning top-ups (the federation's ln module is not available)")
}

/// The registry's URL, or the refusal: a top-up is a holder load, recorded in ops.
fn topup_ops_url(h: &TillHandle) -> Result<String, Reply> {
    let c = h.conn.lock().unwrap();
    if let Some(r) = c.refusal() {
        return Err(r);
    }
    match &c.ops {
        Some(o) => Ok(o.client.url().to_owned()),
        None => Err(Reply::fail(503, "ops_not_connected", "top-ups are recorded in the registry, and this till is not on one")),
    }
}

#[derive(Deserialize)]
struct WaitingQuery {
    short_id: Option<String>,
}

/// `GET /v1/topups/waiting?short_id=<id>`: paid top-ups not yet on a card that this card may take
/// (docs/TOPUP.md § Waiting top-ups), oldest first.
async fn topups_waiting(State(h): State<TillHandle>, Query(q): Query<WaitingQuery>) -> Reply {
    let Some(t) = h.topups.clone() else { return topups_unavailable() };
    if let Some(id) = q.short_id.as_deref() {
        if id.len() != 8 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Reply::fail(400, "bad_request", "short_id must be 8 hex characters");
        }
    }
    let store = t.store.lock().unwrap();
    let list: Vec<Value> = store
        .waiting_for(q.short_id.as_deref())
        .into_iter()
        .map(|r| json!({"topup_id": r.topup_id, "amount_msat": r.amount_msat, "for_card": r.for_card, "created_at_ms": r.created_at_ms}))
        .collect();
    Reply::ok(json!({"ok": true, "topups": list}))
}

async fn topup_create(State(h): State<TillHandle>, body: Bytes) -> Reply {
    let Some(t) = h.topups.clone() else { return topups_unavailable() };
    let req: TopupReq = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return Reply::fail(400, "bad_request", format!("body must be JSON like {{\"amount_msat\":N,\"client_ref\":\"…\"}}: {e}")),
    };
    if req.client_ref.as_deref().is_some_and(|c| c.is_empty() || c.len() > TOPUP_MAX_CLIENT_REF) {
        return Reply::fail(400, "bad_request", format!("client_ref must be 1 to {TOPUP_MAX_CLIENT_REF} bytes"));
    }
    let Some(requested) = req.amount_msat.filter(|a| *a > 0) else {
        return Reply::fail(400, "bad_amount", "amount_msat must be a positive number of msat");
    };
    if requested > crate::topup::MAX_TOPUP_MSAT {
        return Reply::fail(400, "bad_amount", format!("a top-up is at most {} msat", crate::topup::MAX_TOPUP_MSAT));
    }
    if let Err(r) = topup_ops_url(&h) {
        return r;
    }
    let _one = t.create.lock().await;
    if let Some(cref) = req.client_ref.as_deref() {
        if let Some(had) = t.store.lock().unwrap().by_ref(cref).cloned() {
            if had.requested_msat != requested {
                return Reply::fail(
                    409,
                    "client_ref_conflict",
                    format!("client_ref {cref} was a top-up of {} msat, not {requested}", had.requested_msat),
                );
            }
            return Reply::ok(had.to_json());
        }
    }
    let mut max_notes = crate::topup::MAX_TOPUP_NOTES;
    if let Some(id) = req.short_id.as_deref() {
        if id.len() != 8 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Reply::fail(400, "bad_request", "short_id must be 8 hex characters");
        }
        let ops = h.conn.lock().unwrap().ops.clone();
        let Some(ops) = ops else { return Reply::fail(503, "ops_not_connected", "this till is not connected to a registry") };
        // Unkeyed slots (split keying) count as room: the load's tap keys them (`holder.rs`).
        let free = match ops.client.free_slots(id).await {
            Ok(f) => (f.free.len() + f.unkeyed_slots.len()) as u32,
            Err(e) => return Reply::fail(503, "ops_unreachable", format!("the card's free slots: {e}")),
        };
        if free == 0 {
            return Reply::fail(409, "card_full", "this card has no free slot: spend from it, then top up");
        }
        max_notes = max_notes.min(free);
    }
    let cfg = &h.wallet.mint_cfg;
    let amount = match crate::plan::nearest_card_amount(requested, max_notes, h.wallet.min_denom(), |n| {
        cfg.tbs_pks.get(fedimint_core::Amount::from_msats(n)).is_some()
    }) {
        Some(a) if a <= crate::topup::MAX_TOPUP_MSAT => a,
        _ => return Reply::fail(400, "bad_amount", format!("{requested} msat rounds to no amount a card can hold")),
    };
    let (operation_id, invoice) = match t.ln.invoice(amount, crate::topup::INVOICE_EXPIRY_SECS).await {
        Ok(x) => x,
        Err(e) => return Reply::fail(502, "invoice_failed", format!("{e:#}")),
    };
    let now = crate::topup::now_ms();
    let rec = TopupRec {
        topup_id: crate::topup::new_id(),
        client_ref: req.client_ref,
        requested_msat: requested,
        amount_msat: amount,
        invoice,
        operation_id,
        created_at_ms: now,
        expires_at_ms: now + crate::topup::INVOICE_EXPIRY_SECS * 1000,
        state: TopupState::Waiting,
        load_txid: None,
        short_id: None,
        detail: None,
        for_card: req.short_id.as_ref().map(|c| c.to_ascii_lowercase()),
    };
    if let Err(e) = t.store.lock().unwrap().insert(rec.clone()) {
        return error_reply(&e);
    }
    t.watch(&rec);
    Reply::ok(rec.to_json())
}

async fn topup_get(State(h): State<TillHandle>, UrlPath(id): UrlPath<String>) -> Reply {
    let Some(t) = h.topups.clone() else { return topups_unavailable() };
    match t.get(&id) {
        Some(rec) => Reply::ok(rec.to_json()),
        None => unknown_topup(),
    }
}

async fn topup_load(State(h): State<TillHandle>, UrlPath(id): UrlPath<String>, headers: HeaderMap) -> Reply {
    let link = match link_of(&h, &headers) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let r = topup_load_on(&h, &id, link.clone()).await;
    if let Some(l) = link {
        l.close();
    }
    r
}

async fn topup_load_on(h: &TillHandle, id: &str, link: Option<Link>) -> Reply {
    let Some(t) = h.topups.clone() else { return topups_unavailable() };
    let Some(rec) = t.get(id) else { return unknown_topup() };
    if rec.state == TopupState::Loaded {
        return Reply::ok(rec.to_json());
    }
    if let Some(r) = topup_not_loadable(&rec) {
        return r;
    }
    let ops_url = match topup_ops_url(h) {
        Ok(u) => u,
        Err(r) => return r,
    };
    run_job(h, None, |reply| Job::TopupLoad {
        topup_id: rec.topup_id.clone(),
        amount_msat: rec.amount_msat,
        ops_url,
        link,
        topups: t.clone(),
        reply,
    })
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
                "offline": r.offline, "double_spent": r.double_spent,
                "collection_pending": r.collection_pending,
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
///
/// With `ops`, each card job loads that card's state from the registry (see the module docs), and
/// the federation's name and mint keys are reported to it once at startup. Without it, see
/// [`serve_with`].
///
/// With `require_pos_token`, every `/v1/*` request needs the POS token, loopback included, and
/// the token is made now if there is none (`till pair` shows it). This is for a till behind a
/// proxy, where a phone's request can arrive from a local address.
pub async fn serve(
    cw: CardWallet,
    listen: SocketAddr,
    relay: String,
    ops: Option<OpsConfig>,
    require_pos_token: bool,
) -> Result<()> {
    let (url, token) = match ops {
        Some(c) => (Some(c.url), Some(c.token)),
        None => (None, None),
    };
    serve_with(cw, listen, relay, url, token, require_pos_token).await
}

/// [`serve`], with the registry settings as `till serve` takes them (docs/TILL-CLAIM.md):
///
/// - `ops_token` (with `ops_url`): on ops with that token.
/// - else `<data_dir>/till-ops.json`, the token an approved claim left: on ops with it, at the
///   URL it was issued by; a claim file next to it (`till claim --replace`) is polled too.
/// - else, with `ops_url` or a claim file: on ops but not connected. `/v1/status` says so, card
///   jobs answer `ops_not_connected`, and the claim is polled every few seconds; on approval the
///   till goes on ops without a restart.
/// - else local: card state in `card_state.json`, as before ops.
pub async fn serve_with(
    cw: CardWallet,
    listen: SocketAddr,
    relay: String,
    ops_url: Option<String>,
    ops_token: Option<String>,
    require_pos_token: bool,
) -> Result<()> {
    serve_opts(cw, listen, relay, ops_url, ops_token, require_pos_token, ServeOptions::default()).await
}

/// What `till serve` adds for offline payments (docs/OFFLINE-PROTOCOL.md § 4).
#[derive(Debug, Clone, Default)]
pub struct ServeOptions {
    /// The Esplora API block headers come from (`--esplora-url`); `None`: the card's clock only
    /// moves with a header already kept in the data directory.
    pub esplora_url: Option<String>,
    /// The till's caps on unsettled offline charges.
    pub offline: crate::offline::OfflineConfig,
}

/// [`serve_with`], with the offline options.
pub async fn serve_opts(
    cw: CardWallet,
    listen: SocketAddr,
    relay: String,
    ops_url: Option<String>,
    ops_token: Option<String>,
    require_pos_token: bool,
    opts: ServeOptions,
) -> Result<()> {
    let dir = cw.store.path().parent().map(|p| p.to_owned()).unwrap_or_default();
    // Off loopback the API needs the POS token (docs/POS-LINK.md); `till pair` makes it.
    let pos_token = if require_pos_token {
        let (t, made) = crate::pos::ensure_token(&dir)?;
        if made {
            eprintln!("made a POS token in {}: pair phones with `till pair`", crate::pos::token_path(&dir).display());
        }
        Some(t)
    } else {
        crate::pos::read_token(&dir)?
    };
    if !listen.ip().is_loopback() && pos_token.is_none() {
        anyhow::bail!(
            "refusing to listen on {listen} without a POS token: run `card-sidecar till pair` first ({} is missing)",
            crate::pos::token_path(&dir).display()
        );
    }
    let sales = SalesStore::open(sales_path(&dir))?;
    let claim = crate::claim::read_claim(&dir)?;
    let (cfg, poll) = match (ops_url.clone(), ops_token) {
        (Some(url), Some(token)) => (Some(OpsConfig { url, token }), false),
        (None, Some(_)) => anyhow::bail!("--ops-token needs --ops-url"),
        (_, None) => match crate::claim::read_ops(&dir)? {
            Some(cfg) => {
                if let Some(u) = ops_url.as_deref() {
                    if crate::claim::origin(u).ok() != crate::claim::origin(&cfg.url).ok() {
                        eprintln!("note: using {} from {}, the registry that issued this till's token, not {u}", cfg.url, crate::claim::OPS_FILE);
                    }
                }
                (Some(cfg), true)
            }
            // No registry yet: watch for a claim anyway, so `till claim` works against a till
            // that is already serving.
            None => (None, true),
        },
    };
    let report = if cfg.is_some() || poll { Some(crate::ops::FederationReport::of(&cw.wallet).await) } else { None };
    let conn = match cfg {
        Some(cfg) => {
            let o = Ops::new(&cfg, &dir)?;
            crate::ops::report_federation(&o.client, report.as_ref().expect("made above")).await;
            eprintln!("card state from ops at {}", cfg.url);
            Conn::new(OpsState::Connected { url: cfg.url.clone() }, Some(o))
        }
        None if ops_url.is_some() || claim.is_some() => {
            let state = OpsState::of_claim(claim.as_ref(), ops_url.clone(), crate::claim::now_ms());
            eprintln!("not connected to a registry yet ({}): card jobs are refused until a claim is approved", state.name());
            let c = Conn::new(state, None);
            c.lock().unwrap().default_url = ops_url.clone();
            c
        }
        None => Conn::new(OpsState::Local, None),
    };
    let poller = poll.then(|| tokio::spawn(claim_poller(conn.clone(), dir.clone(), report, crate::claim::POLL_EVERY)));
    let listener = tokio::net::TcpListener::bind(listen).await.with_context(|| format!("binding {listen}"))?;
    let relay_for_jobs = relay.clone();
    let opener: CardOpener = Box::new(move || open_relay_card(&relay_for_jobs));
    let mut cw = cw;
    cw.card = Box::new(NoCard);
    // The card's clock: the newest block header, refreshed in the background.
    let headers = crate::clock::Headers::new(opts.esplora_url.clone(), &dir);
    let header_task = headers.spawn(crate::clock::REFRESH_EVERY);
    cw.headers = Some(headers);
    let wallet = cw.wallet.clone();
    let offline = crate::offline::Offline::open(&dir, opts.offline)?;
    let (handle, actor) = new_with_conn(cw, relay.clone(), sales, opener, conn.clone()).await;
    let handle = handle.with_offline(offline.clone());
    // The sender: while on ops, every so often, send what was taken offline (one job, never
    // while another is in flight).
    let sender = {
        let h = handle.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(crate::offline::SEND_EVERY).await;
                let on_ops = conn.lock().unwrap().ops.is_some();
                let waiting = offline.queue.lock().unwrap().file.charges.iter().any(|c| {
                    matches!(c.state, crate::offline::ChargeState::Queued | crate::offline::ChargeState::Reported)
                });
                if on_ops && waiting && !h.is_busy() {
                    let r = h.send_offline().await;
                    if r.status != 200 && r.body["error"] != "busy" {
                        tracing::warn!("offline sender: {}", r.body);
                    }
                }
            }
        })
    };
    // Top-ups by Lightning, when the federation's ln module is there (docs/TOPUP.md).
    let handle = match wallet.lightning() {
        Ok(_) => {
            let store = crate::topup::TopupStore::open(crate::topup::TopupStore::path_in(&dir))?;
            handle.with_topups(Topups::new(store, Arc::new(crate::topup::WalletLightning { wallet })))
        }
        Err(e) => {
            eprintln!("no Lightning top-ups: {e:#}");
            handle
        }
    };
    let (auth, handle) = match pos_token {
        Some(t) if require_pos_token => ("POS token required on every request", handle.with_pos_token_everywhere(t)),
        Some(t) => ("POS token required off loopback", handle.with_pos_token(Some(t))),
        None => ("no POS token: loopback only", handle.with_pos_token(None)),
    };
    eprintln!("till service listening on http://{} (card relay {relay}; {auth})", listener.local_addr()?);
    // Ctrl-C, or SIGTERM from a service manager (StartOS, systemd): stop taking requests and
    // let the ones in flight finish.
    let shutdown = async {
        #[cfg(unix)]
        {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("installing the SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    };
    let server = tokio::spawn(std::future::IntoFuture::into_future(
        axum::serve(listener, router(handle).into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(shutdown),
    ));
    // The actor ends once the server has dropped the router (and with it every handle).
    // The sender holds a handle, so it stops with the server, or the actor would never end.
    let server = async {
        let r = server.await;
        sender.abort();
        r
    };
    let (served, ()) = tokio::join!(server, actor);
    if let Some(p) = poller {
        p.abort();
    }
    if let Some(t) = header_task {
        t.abort();
    }
    served.context("till server task")??;
    Ok(())
}
