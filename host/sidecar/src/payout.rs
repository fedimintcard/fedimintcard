//! The merchant's payouts from the till's wallet (`PAYOUT-API.md`): a confirmed Lightning address
//! paid automatically on a cadence and/or on request, and manual eCash exports of this
//! federation that can be reclaimed.
//!
//! - [`store`]: `payout.json`, the one durable record.
//! - [`engine`]: the decisions, made one at a time under the admin payout lock, each step saved
//!   before it is taken, and every open payout settled from the wallet's own records.
//! - [`funds`]: what in the wallet is owed to holders (top-ups, pays), so it is never paid out.
//! - [`lnurl`], [`net`]: Lightning address resolution, the exact-invoice check, and the only
//!   network access (public HTTPS hosts, pinned DNS, no redirects).
//! - [`backend`]: the wallet side, including tagged eCash exports recoverable from the operation
//!   log.
//!
//! Every route is under `/admin/payout` and the admin router wraps them with the admin bearer.
//! Invoices, operation ids and eCash notes are never logged.

pub mod backend;
pub mod engine;
pub mod fees;
pub mod funds;
pub mod lnurl;
pub mod net;
pub mod store;

use std::future::Future;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::routing::{get, post, put};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::admin::AdminState;
use crate::till::{Reply, TillHandle};
use backend::TillBackend;
use engine::{Engine, ManualLightning};
use net::SafeFetch;
use store::{Mode, PayoutStore, Record};

/// How often the worker runs.
pub const SWEEP_EVERY: Duration = Duration::from_secs(60);
/// How long the worker follows one payment in flight.
pub const SWEEP_WAIT: Duration = Duration::from_secs(10);
/// How long a manual request follows its payment before it answers `202`.
pub const REQUEST_WAIT: Duration = Duration::from_secs(30);
const MAX_REQUEST_ID: usize = 64;

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/admin/payout", get(status))
        .route("/admin/payout/settings", put(settings))
        .route("/admin/payout/destination", post(propose).delete(remove))
        .route("/admin/payout/destination/confirm", post(confirm))
        .route("/admin/payout/lightning", post(lightning))
        .route("/admin/payout/ecash", post(ecash))
        .route("/admin/payout/records/{id}", get(record))
        .route("/admin/payout/records/{id}/reclaim", post(reclaim))
        .route("/admin/payout/records/{id}/delivered", post(delivered))
}

/// The worker: every [`SWEEP_EVERY`], once the till is ready, settle open payouts and start a due
/// automatic one, under the payout lock. Never returns. The till's handle is held for one pass at
/// a time, so it never keeps the till's actor alive past shutdown.
pub async fn sweeper(state: AdminState) {
    loop {
        let one = state.payout_lock().lock().await;
        if let Ok(h) = state.till().await {
            // The backend holds a clone of the handle (the evidence for `not_charged` pays) for
            // this pass only.
            let b = TillBackend::of(&h, state.dir());
            drop(h);
            Engine { dir: state.dir(), backend: &b, fetch: &SafeFetch }.sweep(SWEEP_WAIT).await;
        }
        drop(one);
        tokio::time::sleep(SWEEP_EVERY).await;
    }
}

/// Run `f` under the payout lock in its own task: a request whose client goes away does not cut
/// a payout short between two of its steps.
async fn locked<F, Fut>(s: AdminState, f: F) -> Reply
where
    F: FnOnce(AdminState, TillBackend) -> Fut + Send + 'static,
    Fut: Future<Output = Reply> + Send + 'static,
{
    let h: TillHandle = match s.till().await {
        Ok(h) => h,
        Err(r) => return r,
    };
    let b = TillBackend::of(&h, s.dir());
    drop(h);
    let task = tokio::spawn(async move {
        let _one = s.payout_lock().lock().await;
        f(s.clone(), b).await
    });
    task.await.unwrap_or_else(|_| Reply::fail(500, "internal", "the payout task stopped; the worker settles any payout it left"))
}

fn body<T: DeserializeOwned>(b: &Bytes, example: &str) -> Result<T, Reply> {
    serde_json::from_slice(b).map_err(|e| Reply::fail(400, "bad_request", format!("the body must be JSON like {example}: {e}")))
}

fn check_request_id(r: &str) -> Result<(), Reply> {
    if r.is_empty() || r.len() > MAX_REQUEST_ID || !r.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err(Reply::fail(
            400,
            "bad_request",
            format!("request_id is 1 to {MAX_REQUEST_ID} characters of A-Z, a-z, 0-9, '-' and '_'"),
        ));
    }
    Ok(())
}

/// A payout as answered: its JSON, and for eCash whose notes it shows, a QR of them made here
/// (`qr_svg`, or `null` with `qr_error` when the notes do not fit one QR code; the `notes` text is
/// there to copy either way).
fn payout_json(rec: &Record) -> serde_json::Value {
    let mut v = rec.to_json();
    if let Some(notes) = v.get("notes").and_then(|n| n.as_str()).map(str::to_owned) {
        match crate::admin::qr_svg(&notes) {
            Ok(svg) => v["qr_svg"] = json!(svg),
            Err(_) => {
                v["qr_svg"] = serde_json::Value::Null;
                v["qr_error"] = json!("too_long");
            }
        }
    }
    v
}

/// A payout's answer: `202` while it is still settling.
fn payout_reply(r: Result<Record, Reply>) -> Reply {
    match r {
        Ok(rec) => {
            Reply { status: if rec.state.needs_drive() { 202 } else { 200 }, body: json!({"ok": true, "payout": payout_json(&rec)}) }
        }
        Err(r) => r,
    }
}

fn ok_or(r: Result<serde_json::Value, Reply>) -> Reply {
    r.map(Reply::ok).unwrap_or_else(|e| e)
}

async fn status(State(s): State<AdminState>) -> Reply {
    let h = match s.till().await {
        Ok(h) => h,
        Err(r) => return r,
    };
    let b = TillBackend::of(&h, s.dir());
    drop(h);
    ok_or(Engine { dir: s.dir(), backend: &b, fetch: &SafeFetch }.status().await)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsPatch {
    mode: Option<String>,
    reserve_sat: Option<u64>,
    threshold_sat: Option<u64>,
    max_fee_sat: Option<u64>,
    max_fee_ppm: Option<u64>,
    interval_minutes: Option<u64>,
    /// Clear a halt (a fee overrun) after reviewing it.
    resume: Option<bool>,
}

async fn settings(State(s): State<AdminState>, b: Bytes) -> Reply {
    let p: SettingsPatch = match body(&b, r#"{"mode":"both","reserve_sat":10000}"#) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mode = match p.mode.as_deref().map(Mode::parse) {
        Some(None) => return Reply::fail(400, "bad_settings", "mode is auto, manual, both or off"),
        Some(m) => m,
        None => None,
    };
    locked(s, move |s, b| async move {
        let e = Engine { dir: s.dir(), backend: &b, fetch: &SafeFetch };
        let mut cur = match PayoutStore::open(s.dir()) {
            Ok(st) => st.file().settings,
            Err(err) => return Reply::fail(503, "payout_store", format!("{err:#}")),
        };
        if let Some(m) = mode {
            cur.mode = m;
        }
        cur.reserve_sat = p.reserve_sat.unwrap_or(cur.reserve_sat);
        cur.threshold_sat = p.threshold_sat.unwrap_or(cur.threshold_sat);
        cur.max_fee_sat = p.max_fee_sat.unwrap_or(cur.max_fee_sat);
        cur.max_fee_ppm = p.max_fee_ppm.unwrap_or(cur.max_fee_ppm);
        cur.interval_minutes = p.interval_minutes.unwrap_or(cur.interval_minutes);
        if let Err(r) = e.update_settings(cur, p.resume == Some(true)) {
            return r;
        }
        ok_or(e.status().await)
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Propose {
    address: String,
}

async fn propose(State(s): State<AdminState>, b: Bytes) -> Reply {
    let p: Propose = match body(&b, r#"{"address":"name@domain"}"#) {
        Ok(p) => p,
        Err(r) => return r,
    };
    locked(s, move |s, b| async move {
        let e = Engine { dir: s.dir(), backend: &b, fetch: &SafeFetch };
        match e.propose_destination(&p.address).await {
            Ok(pending) => Reply::ok(json!({"ok": true, "pending_destination": pending.to_json()})),
            Err(r) => r,
        }
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirm {
    confirm_token: String,
}

async fn confirm(State(s): State<AdminState>, b: Bytes) -> Reply {
    let c: Confirm = match body(&b, r#"{"confirm_token":"…"}"#) {
        Ok(c) => c,
        Err(r) => return r,
    };
    locked(s, move |s, b| async move {
        let e = Engine { dir: s.dir(), backend: &b, fetch: &SafeFetch };
        match e.confirm_destination(c.confirm_token.trim()) {
            Ok(()) => ok_or(e.status().await),
            Err(r) => r,
        }
    })
    .await
}

async fn remove(State(s): State<AdminState>) -> Reply {
    locked(s, |s, b| async move {
        let e = Engine { dir: s.dir(), backend: &b, fetch: &SafeFetch };
        match e.remove_destination() {
            Ok(()) => ok_or(e.status().await),
            Err(r) => r,
        }
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LightningReq {
    request_id: String,
    expected_destination: String,
    expected_confirmed_at_ms: u64,
    amount_sat: Option<u64>,
}

async fn lightning(State(s): State<AdminState>, b: Bytes) -> Reply {
    let r: LightningReq =
        match body(&b, r#"{"request_id":"…","amount_sat":25000,"expected_destination":"shop@example.com","expected_confirmed_at_ms":123}"#)
        {
            Ok(r) => r,
            Err(r) => return r,
        };
    if let Err(e) = check_request_id(&r.request_id) {
        return e;
    }
    locked(s, move |s, b| async move {
        let e = Engine { dir: s.dir(), backend: &b, fetch: &SafeFetch };
        let manual = ManualLightning {
            request_id: r.request_id,
            expected_destination: r.expected_destination,
            expected_confirmed_at_ms: r.expected_confirmed_at_ms,
        };
        payout_reply(e.lightning(Some(manual), r.amount_sat, REQUEST_WAIT).await)
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EcashReq {
    request_id: String,
    amount_sat: u64,
}

async fn ecash(State(s): State<AdminState>, b: Bytes) -> Reply {
    let r: EcashReq = match body(&b, r#"{"request_id":"…","amount_sat":25000}"#) {
        Ok(r) => r,
        Err(r) => return r,
    };
    if let Err(e) = check_request_id(&r.request_id) {
        return e;
    }
    locked(s, move |s, b| async move {
        let e = Engine { dir: s.dir(), backend: &b, fetch: &SafeFetch };
        payout_reply(e.ecash(r.request_id, r.amount_sat, REQUEST_WAIT).await)
    })
    .await
}

async fn record(State(s): State<AdminState>, UrlPath(id): UrlPath<String>) -> Reply {
    if let Err(r) = s.till().await {
        return r;
    }
    match PayoutStore::open(s.dir()) {
        Ok(st) => match st.get(&id) {
            Some(rec) => Reply::ok(json!({"ok": true, "payout": payout_json(rec)})),
            None => Reply::fail(404, "unknown_payout", "no such payout"),
        },
        Err(e) => Reply::fail(503, "payout_store", format!("{e:#}")),
    }
}

async fn reclaim(State(s): State<AdminState>, UrlPath(id): UrlPath<String>) -> Reply {
    locked(s, move |s, b| async move {
        let e = Engine { dir: s.dir(), backend: &b, fetch: &SafeFetch };
        payout_reply(e.reclaim(&id, REQUEST_WAIT).await)
    })
    .await
}

async fn delivered(State(s): State<AdminState>, UrlPath(id): UrlPath<String>) -> Reply {
    locked(s, move |s, b| async move {
        let e = Engine { dir: s.dir(), backend: &b, fetch: &SafeFetch };
        payout_reply(e.delivered(&id))
    })
    .await
}

#[cfg(test)]
mod tests;
