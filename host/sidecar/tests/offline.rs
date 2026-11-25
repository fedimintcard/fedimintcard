//! F3: the till with applet 3.0 cards (docs/OFFLINE-PROTOCOL.md § 4), online and offline, against
//! the in-process federation of `till_service.rs` and a fake registry that answers the routes a
//! till uses (`state`, `intent`, `submitted`, `outcome`, slot renewals, `cert-key`,
//! `offline-charges`, `stranded`, and the releases and stranded slots in the state) as
//! `registry/src/cards.ts` and `offline.ts` do. "Down" makes every route answer 502 with no body,
//! as an unreachable registry behind a proxy does.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use fedimint_card_host_card::{BridgeError, CHANGE_SLOTS_FROM, Card, Certificate, SlotState, SoftCard};
use fedimint_card_pos_core::kit::Kit;
use fedimint_card_pos_core::store::Store;
use fedimint_card_sidecar::glue::CardWallet;
use fedimint_card_sidecar::offline::{ChargeState, Offline, OfflineCharge, OfflineConfig};
use fedimint_card_sidecar::ops::{MAX_KEY_PER_TAP, Ops, OpsConfig, SPARE_TARGET, SpareConfig, short_id_of};
use fedimint_card_sidecar::pool::{self, TillKeys};
use fedimint_card_sidecar::sales::SalesStore;
use fedimint_card_sidecar::state::{NoteStatus, StateStore};
use fedimint_card_sidecar::till::{self, Conn, OpsState, card_balance_job, charge_job_with, send_offline_job};
use fedimint_card_sidecar::wallet::Wallet;
use fedimint_client::ClientHandleArc;
use fedimint_client::transaction::TransactionBuilder;
use fedimint_core::Amount;
use fedimint_core::core::OperationId;
use fedimint_core::secp256k1::XOnlyPublicKey;
use fedimint_core::secp256k1::schnorr::Signature;
use fedimint_dummy_client::{DummyClientInit, DummyClientModule};
use fedimint_dummy_server::DummyInit;
use fedimint_mint_client::MintClientInit;
use fedimint_mint_server::MintInit;
use fedimint_testing::federation::FederationTest;
use fedimint_testing::fixtures::Fixtures;
use serde_json::{Value, json};
use tower::ServiceExt as _;

// ------------------------------------------------------------------ the fake registry

#[derive(Default)]
struct FakeCard {
    version: u64,
    slots: Vec<(u8, String)>,
    notes: Vec<Value>,
    txs: Vec<Value>,
    releases: Vec<Value>,
    /// Split keying: the card's slot count, with every slot not in `slots` listed as unkeyed. 0:
    /// none listed.
    slot_count: u8,
    /// Registry v13's `strandings`: `{slot, kind, txid, first_seen}` per slot a till held.
    strandings: Vec<Value>,
    /// Txids of the card's `offline_reports` rows that count as the registry having seen the
    /// transaction: recorded or refused, but not the money gate's refusals.
    reported: Vec<Value>,
}

/// Registry v13's grace before a stranded slot may be released (`STRANDED_GRACE_MS`,
/// `registry/src/cards.ts`): an offline kit's 7 days plus its 30-day redeem grace.
const STRANDED_GRACE_MS: u64 = 37 * DAY_MS;

/// The note on `slot`'s current key, as the registry finds it (`'02' || pubkey`).
fn note_on_key(c: &FakeCard, slot: u8) -> Option<&Value> {
    let (_, key) = c.slots.iter().find(|(s, _)| *s == slot)?;
    let nonce = format!("02{key}");
    c.notes.iter().find(|n| n["nonce"] == nonce.as_str())
}

/// Whether the registry has seen transaction `t` (`txKnown`): a charge on record, or an offline
/// report of it other than the money gate's refusal.
fn tx_seen(c: &FakeCard, t: &Value) -> bool {
    c.txs.iter().any(|x| x["txid"] == *t) || c.reported.contains(t)
}

/// The federation said a note `t` spent is spent (`landed`): its change is real, never released.
fn landed(c: &FakeCard, t: &Value) -> bool {
    c.notes.iter().any(|n| n["txid"] == *t && n["status"] == "spent")
}

/// Whether a stranded row is still open (`openStrandings`): the registry has not seen its
/// transaction, and no release of its slot for it is signed.
fn strand_open(c: &FakeCard, s: &Value) -> bool {
    !tx_seen(c, &s["txid"]) && !c.releases.iter().any(|r| r["slot"] == s["slot"] && r["txid"] == s["txid"])
}

/// `GET /state`'s `stranded` at `now`, as `cardState` lists the open rows: a spent row with its
/// live note's amount, a change row with none, and when each may be released. A row is due once
/// its grace has run, but a change row of a transaction that landed never is: no report releases
/// it, so a till has nothing to post for it.
fn stranded_of(c: &FakeCard, now: u64) -> Vec<Value> {
    c.strandings
        .iter()
        .filter(|s| strand_open(c, s))
        .map(|s| {
            let since = s["first_seen"].as_u64().unwrap();
            let live = note_on_key(c, s["slot"].as_u64().unwrap() as u8).filter(|n| n["status"] == "live");
            let amount = match (s["kind"].as_str(), live) {
                (Some("spent"), Some(n)) => n["amount_msat"].clone(),
                _ => Value::Null,
            };
            json!({
                "slot": s["slot"], "kind": s["kind"], "txid": s["txid"], "amount_msat": amount, "since": since,
                "release_after": since + STRANDED_GRACE_MS,
                "due": now >= since + STRANDED_GRACE_MS && !(s["kind"] == "change" && landed(c, &s["txid"])),
            })
        })
        .collect()
}

/// Sign a release per slot for `txid`, as `registry/src/offline.ts` `recordReleases`.
fn add_releases(cert: &p256::ecdsa::SigningKey, c: &mut FakeCard, txid: &Value, slots: &[u8]) {
    let slot0: [u8; 32] = hex::decode(&c.slots.iter().find(|(s, _)| *s == 0).unwrap().1).unwrap().try_into().unwrap();
    let t: [u8; 32] = hex::decode(txid.as_str().unwrap()).unwrap().try_into().unwrap();
    for slot in slots {
        let sig = fedimint_card_host_card::v3::sign_release(cert, &slot0, *slot, &t);
        c.releases.push(json!({"slot": slot, "txid": txid, "sig": hex::encode(sig)}));
    }
}

struct FakeOps {
    cert: p256::ecdsa::SigningKey,
    cards: Mutex<HashMap<String, FakeCard>>,
    down: AtomicBool,
    /// Answer `GET /registry/cert-key` with no key, as a registry without `CERT_SIGNING_KEY`.
    no_cert: AtomicBool,
    /// `GET /registry/cert-key` alone answers 502 (the rest of the registry answers).
    cert_down: AtomicBool,
    /// Every `GET /registry/cert-key` asked, the probe's included.
    cert_fetches: AtomicUsize,
    outcomes: Mutex<Vec<Value>>,
    /// Every `POST /load/outcome` body (the holder's load route), in order.
    load_outcomes: Mutex<Vec<Value>>,
    reports: Mutex<Vec<Value>>,
    /// Every `POST /slot-keys` body, in order.
    key_posts: Mutex<Vec<Value>>,
    /// Every renewal taken (`PUT /slots/:n`), by slot, in order.
    renewals: Mutex<Vec<u8>>,
    /// Every `POST /stranded` body, in order, those a registry before strandings refused included.
    stranded_posts: Mutex<Vec<Value>>,
    /// A registry before v13: no `stranded` in the state, and `POST /stranded` answers 404.
    before_strandings: AtomicBool,
    /// The registry signs no note (its key is off): outcomes stand with `notes: []`.
    no_note_key: AtomicBool,
    /// The holder's load route answers 429 (the card's hourly load budget is spent).
    load_budget_spent: AtomicBool,
    /// Every `ALERT` line the registry would log, in order.
    alerts: Mutex<Vec<String>>,
    /// `/offline-charges` refuses every report: `change_used` as `registry/src/offline.ts` answers
    /// a change nonce that already carries a note (409 `double_spend`, the change slots released,
    /// the inputs live), any other value as a 422 with that reason.
    refuse_reports: Mutex<Option<&'static str>>,
}

type S = State<Arc<FakeOps>>;

fn fail(code: u16, reason: &str) -> Response {
    (StatusCode::from_u16(code).unwrap(), Json(json!({"ok": false, "reason": reason}))).into_response()
}

fn ok(v: Value) -> Response {
    (StatusCode::OK, Json(v)).into_response()
}

impl FakeOps {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            cert: p256::ecdsa::SigningKey::random(&mut rand::thread_rng()),
            cards: Mutex::new(HashMap::new()),
            down: AtomicBool::new(false),
            no_cert: AtomicBool::new(false),
            cert_down: AtomicBool::new(false),
            cert_fetches: AtomicUsize::new(0),
            outcomes: Mutex::new(vec![]),
            load_outcomes: Mutex::new(vec![]),
            reports: Mutex::new(vec![]),
            key_posts: Mutex::new(vec![]),
            renewals: Mutex::new(vec![]),
            stranded_posts: Mutex::new(vec![]),
            before_strandings: AtomicBool::new(false),
            no_note_key: AtomicBool::new(false),
            load_budget_spent: AtomicBool::new(false),
            alerts: Mutex::new(vec![]),
            refuse_reports: Mutex::new(None),
        })
    }

    /// As the registry's `/test/stranded-age {short_id, ms}`: the card's stranded rows, `ms`
    /// older.
    fn age_strandings(&self, id: &str, ms: u64) {
        for s in self.cards.lock().unwrap().get_mut(id).unwrap().strandings.iter_mut() {
            s["first_seen"] = json!(s["first_seen"].as_u64().unwrap() - ms);
        }
    }

    /// The card's stranded rows, open or not.
    fn strandings(&self, id: &str) -> Vec<Value> {
        self.cards.lock().unwrap()[id].strandings.clone()
    }

    /// The slots the card's releases name for `txid`.
    fn released(&self, id: &str, txid: &str) -> Vec<u64> {
        let mut v: Vec<u64> =
            self.cards.lock().unwrap()[id].releases.iter().filter(|r| r["txid"] == txid).map(|r| r["slot"].as_u64().unwrap()).collect();
        v.sort();
        v
    }

    fn cert_pub(&self) -> Vec<u8> {
        self.cert.verifying_key().to_encoded_point(false).as_bytes().to_vec()
    }

    fn down(&self) -> Option<Response> {
        self.down.load(Ordering::SeqCst).then(|| StatusCode::BAD_GATEWAY.into_response())
    }

    fn note_status(&self, id: &str, nonce: &str) -> Option<String> {
        self.cards.lock().unwrap()[id].notes.iter().find(|n| n["nonce"] == nonce).map(|n| n["status"].as_str().unwrap().to_owned())
    }

    fn set_note_status(&self, id: &str, nonce: &str, status: &str) {
        let mut g = self.cards.lock().unwrap();
        let n = g.get_mut(id).unwrap().notes.iter_mut().find(|n| n["nonce"] == nonce).unwrap();
        n["status"] = json!(status);
    }
}

async fn cert_key(State(r): S) -> Response {
    r.cert_fetches.fetch_add(1, Ordering::SeqCst);
    if let Some(d) = r.down() {
        return d;
    }
    if r.cert_down.load(Ordering::SeqCst) {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    if r.no_cert.load(Ordering::SeqCst) {
        return ok(json!({"ok": true, "key": null}));
    }
    ok(json!({"ok": true, "key": hex::encode(r.cert_pub())}))
}

async fn federations(State(r): S) -> Response {
    r.down().unwrap_or_else(|| ok(json!({"ok": true})))
}

async fn state(State(r): S, Path(id): Path<String>) -> Response {
    if let Some(d) = r.down() {
        return d;
    }
    let g = r.cards.lock().unwrap();
    let Some(c) = g.get(&id) else { return fail(404, "no such card") };
    let open: Vec<Value> = c
        .txs
        .iter()
        .filter(|t| t["status"] == "open")
        .map(|t| json!({"txid": t["txid"], "till_id": 1, "client_ref": t["client_ref"], "signed_hex": t["signed_hex"], "inputs": t["inputs"]}))
        .collect();
    let mut v = json!({
        "version": c.version,
        "slots": c.slots.iter().map(|(s, k)| json!({"slot": s, "pubkey": k})).collect::<Vec<_>>(),
        "unkeyed_slots": unkeyed_of(c),
        "notes": c.notes,
        "open_txs": open,
        "releases": c.releases,
    });
    if !r.before_strandings.load(Ordering::SeqCst) {
        v["stranded"] = json!(stranded_of(c, now_ms()));
    }
    ok(v)
}

fn unkeyed_of(c: &FakeCard) -> Vec<u8> {
    (0..c.slot_count).filter(|s| !c.slots.iter().any(|(k, _)| k == s)).collect()
}

/// As `registry/src/cards.ts` `slot-keys`: first keys for never-keyed slots; the same key again
/// replays.
async fn slot_keys(State(r): S, Path(id): Path<String>, Json(b): Json<Value>) -> Response {
    if let Some(d) = r.down() {
        return d;
    }
    let mut g = r.cards.lock().unwrap();
    let c = g.get_mut(&id).unwrap();
    let mut added = vec![];
    for k in b["keys"].as_array().unwrap() {
        let (slot, pk) = (k["slot"].as_u64().unwrap() as u8, k["pubkey"].as_str().unwrap().to_owned());
        match c.slots.iter().find(|(s, _)| *s == slot) {
            Some((_, had)) if *had == pk => {}
            Some(_) => return fail(409, "slot has another key"),
            None if slot < c.slot_count => {
                c.slots.push((slot, pk));
                c.strandings.retain(|x| x["slot"] != slot);
                added.push(slot);
            }
            None => return fail(409, "no such slot"),
        }
    }
    if !added.is_empty() {
        c.version += 1;
    }
    r.key_posts.lock().unwrap().push(b.clone());
    ok(json!({"ok": true, "version": c.version, "added": added, "replayed": added.is_empty(), "unkeyed_slots": unkeyed_of(c)}))
}

/// As the registry signs a note when it turns live (DESIGN § 6): each live note of `nonces`
/// whose nonce is its slot's current key, once. Answers them as an outcome's answer lists them.
fn sign_live(cert: &p256::ecdsa::SigningKey, c: &mut FakeCard, nonces: &[Value]) -> Vec<Value> {
    let mut out = vec![];
    for n in c.notes.iter_mut().filter(|n| nonces.contains(&n["nonce"]) && n["status"] == "live") {
        let slot = n["slot"].as_u64().unwrap() as u8;
        let nonce = n["nonce"].as_str().unwrap().to_owned();
        let Some((_, key)) = c.slots.iter().find(|(s, _)| *s == slot) else { continue };
        if nonce.get(2..) != Some(key.as_str()) {
            continue;
        }
        if n["reg_sig"].is_null() {
            let x: [u8; 32] = hex::decode(key).unwrap().try_into().unwrap();
            let sig48 = fedimint_card_sidecar::glue::sig48_of(n["signature"].as_str().unwrap()).unwrap();
            let amount = n["amount_msat"].as_u64().unwrap();
            n["reg_sig"] = json!(hex::encode(fedimint_card_host_card::v3::sign_note(cert, &x, amount, &sig48)));
        }
        out.push(json!({"slot": slot, "nonce": nonce, "reg_sig": n["reg_sig"]}));
    }
    out
}

fn add_change(c: &mut FakeCard, txid: &Value, change: &[Value]) {
    for ch in change {
        c.notes.push(json!({
            "slot": ch["slot"], "amount_msat": ch["amount_msat"], "nonce": ch["nonce"], "signature": null,
            "blinding_key": ch["blinding_key"], "status": "pending", "txid": txid, "out_idx": ch["out_idx"],
        }));
    }
}

async fn intent(State(r): S, Path(id): Path<String>, Json(b): Json<Value>) -> Response {
    if let Some(d) = r.down() {
        return d;
    }
    let mut g = r.cards.lock().unwrap();
    let c = g.get_mut(&id).unwrap();
    if c.txs.iter().any(|t| t["txid"] == b["txid"]) {
        return ok(json!({"ok": true, "version": c.version}));
    }
    if let Some(t) = c.txs.iter().find(|t| t["status"] == "open") {
        return fail(409, &format!("card has an open charge {}", t["txid"]));
    }
    for i in b["inputs"].as_array().unwrap() {
        if !c.notes.iter().any(|n| n["nonce"] == *i && n["status"] == "live") {
            return fail(409, &format!("input {i} is not live"));
        }
    }
    let change = b["change"].as_array().unwrap().clone();
    // As `recordIntent`: change into slots 24 to 31, or under one of their keys, is refused for a
    // charge; a top-up's is recorded, with the alert.
    let change_keys: Vec<&String> = c.slots.iter().filter(|(s, _)| *s >= CHANGE_SLOTS_FROM).map(|(_, k)| k).collect();
    let in_change_slot = |ch: &Value| {
        ch["slot"].as_u64().is_some_and(|s| s >= CHANGE_SLOTS_FROM as u64)
            || ch["nonce"].as_str().is_some_and(|n| n.len() == 66 && n.starts_with("02") && change_keys.iter().any(|k| n[2..] == **k))
    };
    if change.iter().any(in_change_slot) {
        if !b["inputs"].as_array().unwrap().is_empty() {
            return fail(409, "change into a change slot");
        }
        r.alerts.lock().unwrap().push(format!("load_in_change_slot {id}"));
    }
    // Registry v13: a CHANGE slot held as stranded takes no other change.
    for ch in &change {
        if c.strandings.iter().any(|s| s["slot"] == ch["slot"] && s["kind"] == "change" && strand_open(c, s)) {
            return fail(409, &format!("slot {} is held as stranded change", ch["slot"]));
        }
    }
    add_change(c, &b["txid"], &change);
    c.txs.push(json!({"txid": b["txid"], "client_ref": b["client_ref"], "inputs": b["inputs"], "change": change, "signed_hex": null, "status": "open"}));
    c.version += 1;
    ok(json!({"ok": true, "version": c.version}))
}

async fn submitted(State(r): S, Path(id): Path<String>, Json(b): Json<Value>) -> Response {
    if let Some(d) = r.down() {
        return d;
    }
    let mut g = r.cards.lock().unwrap();
    let c = g.get_mut(&id).unwrap();
    let Some(t) = c.txs.iter_mut().find(|t| t["txid"] == b["txid"]) else { return fail(404, "no such charge") };
    t["signed_hex"] = b["tx_hex"].clone();
    c.version += 1;
    ok(json!({"ok": true, "version": c.version}))
}

/// The holder's load route as a repeat reaches it: the notes of that load the registry has
/// signed (signing any still unsigned).
async fn load_outcome(State(r): S, Path(id): Path<String>, Json(b): Json<Value>) -> Response {
    if let Some(d) = r.down() {
        return d;
    }
    r.load_outcomes.lock().unwrap().push(b.clone());
    if r.load_budget_spent.load(Ordering::SeqCst) {
        return fail(429, "the card's load budget is spent");
    }
    let mut g = r.cards.lock().unwrap();
    let c = g.get_mut(&id).unwrap();
    let nonces: Vec<Value> = b["notes"].as_array().unwrap().iter().map(|n| n["nonce"].clone()).collect();
    let notes = if r.no_note_key.load(Ordering::SeqCst) { vec![] } else { sign_live(&r.cert, c, &nonces) };
    ok(json!({"ok": true, "version": c.version, "notes": notes}))
}

async fn outcome(State(r): S, Path(id): Path<String>, Json(b): Json<Value>) -> Response {
    if let Some(d) = r.down() {
        return d;
    }
    let mut g = r.cards.lock().unwrap();
    let c = g.get_mut(&id).unwrap();
    let Some(t) = c.txs.iter_mut().find(|t| t["txid"] == b["txid"]) else { return fail(404, "no such charge") };
    let change_nonces: Vec<Value> = t["change"].as_array().unwrap().iter().map(|ch| ch["nonce"].clone()).collect();
    if t["status"] != "open" {
        // A repeat: the registry replays the outcome it recorded when the same `accepted` comes
        // with some of its change, each `{nonce, signature}` among the recorded ones
        // (`cards.ts repeatMatches`), else 409.
        let pairs = |v: &Value| -> Vec<(Value, Value)> {
            v["change"].as_array().map(|a| a.iter().map(|c| (c["nonce"].clone(), c["signature"].clone())).collect()).unwrap_or_default()
        };
        let recorded = r.outcomes.lock().unwrap().iter().find(|o| o["txid"] == b["txid"]).cloned();
        if recorded.is_some_and(|o| o["accepted"] != b["accepted"] || !pairs(&b).iter().all(|p| pairs(&o).contains(p))) {
            return fail(409, "the transaction is resolved with another outcome");
        }
        // The notes it sent the registry has signed (signing any still unsigned).
        let sent: Vec<Value> = pairs(&b).into_iter().map(|(n, _)| n).filter(|n| change_nonces.contains(n)).collect();
        let notes = if r.no_note_key.load(Ordering::SeqCst) { vec![] } else { sign_live(&r.cert, c, &sent) };
        return ok(json!({"ok": true, "version": c.version, "notes": notes}));
    }
    let accepted = b["accepted"] == true;
    t["status"] = json!(if accepted { "accepted" } else { "rejected" });
    let (inputs, change) = (t["inputs"].clone(), t["change"].clone());
    for n in c.notes.iter_mut() {
        if accepted && inputs.as_array().unwrap().contains(&n["nonce"]) {
            n["status"] = json!("spent");
        }
        if change.as_array().unwrap().iter().any(|ch| ch["nonce"] == n["nonce"]) {
            if accepted {
                let sig = b["change"].as_array().unwrap().iter().find(|s| s["nonce"] == n["nonce"]).map(|s| s["signature"].clone());
                n["signature"] = sig.unwrap_or(Value::Null);
                n["status"] = json!("live");
            } else {
                n["status"] = json!("dead");
            }
        }
    }
    if !accepted {
        // The inputs of a rejected offline charge are live again.
        for n in c.notes.iter_mut().filter(|n| inputs.as_array().unwrap().contains(&n["nonce"]) && n["status"] == "pending") {
            n["status"] = json!("live");
        }
        // A release for every input's slot and every change slot (a load has none).
        let ins = inputs.as_array().unwrap();
        if !ins.is_empty() {
            let mut slots: Vec<u8> =
                c.notes.iter().filter(|n| ins.contains(&n["nonce"])).map(|n| n["slot"].as_u64().unwrap() as u8).collect();
            slots.extend(change.as_array().unwrap().iter().map(|ch| ch["slot"].as_u64().unwrap() as u8));
            slots.sort();
            slots.dedup();
            add_releases(&r.cert, c, &b["txid"], &slots);
        }
    }
    let notes = if accepted && !r.no_note_key.load(Ordering::SeqCst) { sign_live(&r.cert, c, &change_nonces) } else { vec![] };
    c.version += 1;
    r.outcomes.lock().unwrap().push(b.clone());
    ok(json!({"ok": true, "version": c.version, "notes": notes}))
}

async fn renew(State(r): S, Path((id, slot)): Path<(String, u8)>, Json(b): Json<Value>) -> Response {
    if let Some(d) = r.down() {
        return d;
    }
    let mut g = r.cards.lock().unwrap();
    let c = g.get_mut(&id).unwrap();
    let s = c.slots.iter_mut().find(|(s, _)| *s == slot).unwrap();
    if s.1 != b["old_pubkey"] {
        return fail(409, "another key");
    }
    s.1 = b["pubkey"].as_str().unwrap().to_owned();
    c.releases.retain(|x| x["slot"] != slot);
    c.strandings.retain(|x| x["slot"] != slot);
    r.renewals.lock().unwrap().push(slot);
    c.version += 1;
    ok(json!({"ok": true, "version": c.version}))
}

/// As `registry/src/offline.ts` `recordOfflineCharge`.
async fn offline_charges(State(r): S, Path(id): Path<String>, Json(b): Json<Value>) -> Response {
    if let Some(d) = r.down() {
        return d;
    }
    r.reports.lock().unwrap().push(b.clone());
    let mut g = r.cards.lock().unwrap();
    let c = g.get_mut(&id).unwrap();
    assert!(b["signed_hex"].as_str().is_some_and(|s| !s.is_empty()) && b["signed_at"].as_u64().is_some_and(|t| t > 0), "{b}");
    assert!(b["tx_hex"].as_str().is_some_and(|s| !s.is_empty()), "{b}");
    if let Some(why) = *r.refuse_reports.lock().unwrap() {
        if why != "change_used" {
            return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"ok": false, "reason": why}))).into_response();
        }
        let change = b["change"].as_array().unwrap();
        if !c.releases.iter().any(|x| x["txid"] == b["txid"]) {
            let slots: Vec<u8> = change.iter().map(|ch| ch["slot"].as_u64().unwrap() as u8).collect();
            add_releases(&r.cert, c, &b["txid"], &slots);
        }
        let conflicts: Vec<Value> = change.iter().map(|ch| json!({"nonce": ch["nonce"], "status": "change_used", "txid": null})).collect();
        return (StatusCode::CONFLICT, Json(json!({"ok": false, "reason": "double_spend", "conflicts": conflicts}))).into_response();
    }
    if c.txs.iter().any(|t| t["txid"] == b["txid"]) {
        return ok(json!({"ok": true, "version": c.version}));
    }
    // Registry v13's money gate: a transaction the registry released a slot for is never taken.
    // Its refusal does not count as the registry having seen the transaction, so the
    // transaction's other held slots keep to their own release.
    if c.releases.iter().any(|x| x["txid"] == b["txid"]) {
        let conflicts: Vec<Value> =
            b["inputs"].as_array().unwrap().iter().map(|i| json!({"nonce": i, "status": "released", "txid": b["txid"]})).collect();
        return (StatusCode::CONFLICT, Json(json!({"ok": false, "reason": "double_spend", "conflicts": conflicts}))).into_response();
    }
    c.reported.push(b["txid"].clone());
    let mut conflicts = vec![];
    for i in b["inputs"].as_array().unwrap() {
        match c.notes.iter().find(|n| n["nonce"] == *i) {
            None => conflicts.push(json!({"nonce": i, "status": "missing", "txid": null})),
            Some(n) if n["status"] != "live" => conflicts.push(json!({"nonce": i, "status": n["status"], "txid": n["txid"]})),
            _ => {}
        }
    }
    if !conflicts.is_empty() {
        // Releases for the change slots it named; its inputs stay spent.
        if !c.releases.iter().any(|x| x["txid"] == b["txid"]) {
            let slots: Vec<u8> = b["change"].as_array().unwrap().iter().map(|ch| ch["slot"].as_u64().unwrap() as u8).collect();
            add_releases(&r.cert, c, &b["txid"], &slots);
        }
        return (StatusCode::CONFLICT, Json(json!({"ok": false, "reason": "double_spend", "conflicts": conflicts}))).into_response();
    }
    let change = b["change"].as_array().unwrap().clone();
    for n in c.notes.iter_mut().filter(|n| b["inputs"].as_array().unwrap().contains(&n["nonce"])) {
        n["status"] = json!("pending");
        n["txid"] = b["txid"].clone();
    }
    add_change(c, &b["txid"], &change);
    c.txs.push(json!({"txid": b["txid"], "client_ref": b["client_ref"], "inputs": b["inputs"], "change": change, "signed_hex": b["signed_hex"], "status": "open"}));
    c.version += 1;
    ok(json!({"ok": true, "version": c.version}))
}

/// As `registry/src/offline.ts` `recordStranded` (registry v13, ENG-867): the slots a till held at
/// a tap. A slot is recorded once (`first_seen`), and ignored when the registry has seen its
/// transaction or released the slot for it, when the slot is held for another transaction, when
/// a spent slot has no live note under its current key, or when a change slot's key ever carried
/// a note. A spent slot marked `spent: true` (the federation says its note is spent) has its note
/// spent under the transaction and its row dropped, released never. Then each open row of the
/// card past the grace, unless its transaction landed: a spent row is released with its own
/// `unspent: true` in this report; a change row when a spent row of its transaction is
/// `unspent: true` here, when its transaction has no spent row (the card never signed it), or when
/// its transaction already has a release. No version bump.
async fn stranded(State(r): S, Path(id): Path<String>, Json(b): Json<Value>) -> Response {
    if let Some(d) = r.down() {
        return d;
    }
    r.stranded_posts.lock().unwrap().push(b.clone());
    if r.before_strandings.load(Ordering::SeqCst) {
        return fail(404, "no such route");
    }
    let mut g = r.cards.lock().unwrap();
    let c = g.get_mut(&id).unwrap();
    let rows = b["slots"].as_array().cloned().unwrap_or_default();
    let flag = |f: &Value| f.is_null() || f.is_boolean();
    let valid = |x: &Value| {
        x["slot"].as_u64().is_some_and(|s| s < 32)
            && matches!(x["kind"].as_str(), Some("spent" | "change"))
            && x["txid"].as_str().is_some_and(|t| t.len() == 64 && t.bytes().all(|h| h.is_ascii_digit() || (b'a'..=b'f').contains(&h)))
            && flag(&x["unspent"])
            && flag(&x["spent"])
            && !(x["unspent"] == true && x["spent"] == true)
    };
    let mut named: Vec<u64> = rows.iter().filter_map(|x| x["slot"].as_u64()).collect();
    named.sort();
    named.dedup();
    if !(1..=32).contains(&rows.len()) || !rows.iter().all(valid) || named.len() != rows.len() {
        return fail(400, "slots: 1 to 32 of {slot, kind, txid, unspent?, spent?}, each slot once");
    }
    let now = now_ms();
    let open: Vec<Value> = c.strandings.iter().filter(|s| strand_open(c, s)).cloned().collect();
    let (mut held, mut spent, mut ignored) = (vec![], vec![], vec![]);
    for x in &rows {
        let (slot, t) = (x["slot"].as_u64().unwrap() as u8, &x["txid"]);
        let note = note_on_key(c, slot).cloned();
        let takes = c.slots.iter().any(|(s, _)| *s == slot)
            && !tx_seen(c, t)
            && !c.releases.iter().any(|y| y["slot"] == x["slot"] && y["txid"] == *t)
            && !open.iter().any(|s| s["slot"] == x["slot"] && (s["txid"] != *t || s["kind"] != x["kind"]))
            && if x["kind"] == "spent" { note.as_ref().is_some_and(|n| n["status"] == "live") } else { note.is_none() };
        if !takes {
            ignored.push(slot);
            continue;
        }
        if !c.strandings.iter().any(|s| s["slot"] == x["slot"] && s["txid"] == *t) {
            c.strandings.push(json!({"slot": slot, "kind": x["kind"], "txid": t, "first_seen": now}));
        }
        if x["kind"] == "spent" && x["spent"] == true {
            let nonce = note.map(|n| n["nonce"].clone());
            for n in c.notes.iter_mut().filter(|n| Some(&n["nonce"]) == nonce.as_ref() && n["status"] == "live") {
                n["status"] = json!("spent");
                n["txid"] = t.clone();
            }
            c.strandings.retain(|s| !(s["slot"] == x["slot"] && s["txid"] == *t));
            spent.push(slot);
            continue;
        }
        held.push(x.clone());
    }
    let unspent: Vec<&Value> = held.iter().filter(|x| x["kind"] == "spent" && x["unspent"] == true).collect();
    let mut release: Vec<(Value, Vec<u8>)> = vec![];
    for s in c.strandings.iter().filter(|s| strand_open(c, s)) {
        let t = &s["txid"];
        if s["first_seen"].as_u64().unwrap() + STRANDED_GRACE_MS > now || landed(c, t) {
            continue;
        }
        let releasable = if s["kind"] == "spent" {
            unspent.iter().any(|e| e["slot"] == s["slot"] && e["txid"] == *t)
        } else {
            unspent.iter().any(|e| e["txid"] == *t)
                || !c.strandings.iter().any(|x| x["txid"] == *t && x["kind"] == "spent")
                || c.releases.iter().any(|x| x["txid"] == *t)
        };
        if releasable {
            let slot = s["slot"].as_u64().unwrap() as u8;
            match release.iter_mut().find(|(x, _)| x == t) {
                Some((_, slots)) => slots.push(slot),
                None => release.push((t.clone(), vec![slot])),
            }
        }
    }
    let mut released = vec![];
    for (t, slots) in release {
        add_releases(&r.cert, c, &t, &slots);
        released.extend(slots);
    }
    let held: Vec<Value> = held.iter().map(|x| x["slot"].clone()).collect();
    ok(json!({"ok": true, "held": held, "released": released, "spent": spent, "ignored": ignored}))
}

async fn serve(r: Arc<FakeOps>) -> String {
    let app = axum::Router::new()
        .route("/registry/cert-key", get(cert_key))
        .route("/federations", post(federations))
        .route("/cards/{id}/state", get(state))
        .route("/cards/{id}/intent", post(intent))
        .route("/cards/{id}/submitted", post(submitted))
        .route("/cards/{id}/outcome", post(outcome))
        .route("/cards/{id}/load/outcome", post(load_outcome))
        .route("/cards/{id}/slots/{slot}", put(renew))
        .route("/cards/{id}/slot-keys", post(slot_keys))
        .route("/cards/{id}/offline-charges", post(offline_charges))
        .route("/cards/{id}/stranded", post(stranded))
        .with_state(r);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

// ------------------------------------------------------------------ cards, federation, till

/// The physical card, handed to whichever till it is tapped on.
struct Shared(Rc<RefCell<SoftCard>>);

impl Card for Shared {
    fn gen_key(&mut self, slot: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.0.borrow_mut().gen_key(slot)
    }
    fn gen_key_overwrite(&mut self, slot: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.0.borrow_mut().gen_key_overwrite(slot)
    }
    fn get_pub(&mut self, slot: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.0.borrow_mut().get_pub(slot)
    }
    fn sign(&mut self, slot: u8, msg32: &[u8; 32]) -> anyhow::Result<Signature> {
        self.0.borrow_mut().sign(slot, msg32)
    }
    fn apdu(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.0.borrow_mut().apdu(apdu)
    }
}

/// Key every slot, certify a 3.0 card's attestation key and give it the registry key as the Setup
/// app does (before the PIN), set the PIN, and register the card with the fake registry.
fn provision(reg: &FakeOps, card: &Rc<RefCell<SoftCard>>) -> String {
    let n = card.borrow().slot_len();
    provision_slots(reg, card, n)
}

/// [`provision`] with only slots `0..n` keyed and registered.
fn provision_slots(reg: &FakeOps, card: &Rc<RefCell<SoftCard>>, n: u8) -> String {
    provision_keys(reg, card, &(0..n).collect::<Vec<u8>>())
}

/// [`provision`] with only `slots` (slot 0 first) keyed and registered.
fn provision_keys(reg: &FakeOps, card: &Rc<RefCell<SoftCard>>, slots: &[u8]) -> String {
    assert_eq!(slots.first(), Some(&0), "slot 0's key names the card");
    let mut c = card.borrow_mut();
    let keys: Vec<XOnlyPublicKey> = slots.iter().map(|s| c.gen_key(*s).unwrap()).collect();
    if c.version().sign_once() {
        let w = c.attest_init().unwrap();
        let cert = Certificate::sign(&reg.cert, &keys[0].serialize(), &w).unwrap();
        c.set_cert(&cert.to_bytes()).unwrap();
        c.set_registry_key(&reg.cert_pub().try_into().unwrap()).unwrap();
        // A 4.0 card takes its PIN only in the selection that anchored its clock.
        fedimint_card_host_card::softkit::ready_for_pin(&mut *c).unwrap();
    }
    if c.version().sign_once() {
        fedimint_card_host_card::softkit::open_test_session_under(&mut *c, &reg.cert_pub()).unwrap();
    } else {
        // A card older than 4.0 carries its PIN as such a card does; no till sends it one.
        fedimint_card_old_card_pin::open_pin_session(&mut *c).unwrap();
        c.forget_received();
    }
    let id = short_id_of(&keys[0]);
    reg.cards.lock().unwrap().insert(
        id.clone(),
        FakeCard { version: 1, slots: slots.iter().zip(&keys).map(|(s, k)| (*s, k.to_string())).collect(), ..Default::default() },
    );
    id
}

/// [`provision_slots`] for a 32-slot card registered split-keyed: the registry lists slots `n`
/// and up as unkeyed, and tills key them.
fn provision_split(reg: &FakeOps, card: &Rc<RefCell<SoftCard>>, n: u8) -> String {
    let id = provision_slots(reg, card, n);
    reg.cards.lock().unwrap().get_mut(&id).unwrap().slot_count = card.borrow().slot_len();
    id
}

async fn fund(client: &ClientHandleArc, amount: Amount) -> anyhow::Result<()> {
    let dummy = client.get_first_module::<DummyClientModule>()?;
    let op = OperationId::new_random();
    let range = client
        .finalize_and_submit_transaction(op, "fund", |_| (), TransactionBuilder::new().with_inputs(dummy.create_input(amount)))
        .await?;
    client.await_primary_bitcoin_module_outputs(op, range.into_iter().collect()).await?;
    Ok(())
}

async fn federation() -> anyhow::Result<FederationTest> {
    federation_fees(true).await
}

/// The in-process federation with fedimint's default mint fees (100 msat per input and output),
/// or with none.
async fn federation_fees(fees: bool) -> anyhow::Result<FederationTest> {
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit).with_module(DummyClientInit, DummyInit);
    let builder = fixtures.new_fed_builder(0);
    let builder = if fees { builder } else { builder.disable_mint_fees() };
    Ok(tokio::time::timeout(Duration::from_secs(60), builder.build()).await?)
}

struct Till {
    cw: CardWallet,
    ops: Ops,
    off: Arc<Offline>,
    sales: Arc<Mutex<SalesStore>>,
    reg: Arc<FakeOps>,
    dir: tempfile::TempDir,
    _fed: FederationTest,
    _no_alerts: NoAlerts,
}

/// Every test: the till never tops up into slots 24 to 31 (the registry would raise its alert).
struct NoAlerts(Arc<FakeOps>);

impl Drop for NoAlerts {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            assert_eq!(*self.0.alerts.lock().unwrap(), Vec::<String>::new(), "a top-up into a change slot");
        }
    }
}

impl Till {
    async fn new(cfg: OfflineConfig) -> anyhow::Result<Self> {
        Self::new_fees(cfg, true).await
    }

    async fn new_fees(cfg: OfflineConfig, fees: bool) -> anyhow::Result<Self> {
        let fed = federation_fees(fees).await?;
        let client = fed.new_client().await;
        fund(&client, Amount::from_sats(10_000)).await?;
        let wallet = Wallet::from_client(client).await?;
        let dir = tempfile::tempdir()?;
        let reg = FakeOps::new();
        let url = serve(reg.clone()).await;
        let ops = Ops::new(&OpsConfig { url, token: "till_test".into() }, dir.path())?;
        let off = Offline::open(dir.path(), cfg)?;
        let cw = CardWallet::new(wallet, StateStore::open(dir.path().join("card_state.json"))?, Box::new(till::NoCard));
        let sales = Arc::new(Mutex::new(SalesStore::open(dir.path().join("sales.json"))?));
        let mut t = Self { cw, ops, off, sales, _no_alerts: NoAlerts(reg.clone()), reg, dir, _fed: fed };
        // Online at start: the probe keeps the registry's certificate key.
        let (r, f) = t.off.reach.probe(Some(&t.ops), &t.cw.wallet, t.dir.path()).await;
        assert_eq!((r, f), (Some(true), true));
        assert!(fedimint_card_sidecar::claim::read_cert_key(t.dir.path())?.is_some(), "the cert key is kept");
        t.cw.headers = None;
        Ok(t)
    }

    fn tap(&mut self, card: &Rc<RefCell<SoftCard>>) {
        self.cw.card = Box::new(Shared(card.clone()));
    }

    async fn load(&mut self, card: &Rc<RefCell<SoftCard>>, amounts: &[u64]) -> anyhow::Result<()> {
        self.tap(card);
        let base = self.ops.attach(&mut self.cw).await?;
        let r = self.cw.topup(amounts).await;
        Ops::finish(&mut self.cw, base).await;
        r.map(|_| ())
    }

    fn set_down(&self, down: bool) {
        self.reg.down.store(down, Ordering::SeqCst);
        self.off.reach.reset();
    }

    async fn charge(&mut self, card: &Rc<RefCell<SoftCard>>, price: u64, cref: &str) -> (u16, Value) {
        self.tap(card);
        self.cw.pin = Some(b"1234".to_vec());
        let r = charge_job_with(&mut self.cw, Some(&self.ops), Some(&self.off), price, Some(cref.into()), &self.sales).await;
        self.cw.pin = None;
        (r.status, r.body)
    }

    /// An online tap that only attaches the card and finishes (as a charge does before it
    /// plans), with the holder's PIN.
    async fn attach_tap(&mut self, card: &Rc<RefCell<SoftCard>>) -> anyhow::Result<()> {
        self.tap(card);
        self.cw.pin = Some(b"1234".to_vec());
        let r = self.ops.attach(&mut self.cw).await;
        self.cw.pin = None;
        let base = r?;
        Ops::finish(&mut self.cw, base).await;
        Ok(())
    }

    async fn send(&mut self) -> Value {
        self.off.reach.reset();
        let r = send_offline_job(&mut self.cw, &self.ops, &self.off, &self.sales).await;
        assert_eq!(r.status, 200, "{}", r.body);
        r.body
    }

    fn sale(&self, cref: &str) -> fedimint_card_sidecar::sales::SaleRec {
        self.sales.lock().unwrap().find_ref(cref).cloned().expect("a sale")
    }
}

fn v3_card() -> Rc<RefCell<SoftCard>> {
    Rc::new(RefCell::new(SoftCard::v4()))
}

fn slot_states(card: &Rc<RefCell<SoftCard>>, slots: &[u64]) -> Vec<SlotState> {
    slots.iter().map(|s| card.borrow().slot_state(*s as u8).unwrap()).collect()
}

fn slots_of(body: &Value, key: &str) -> Vec<u64> {
    body[key].as_array().unwrap().iter().map(|c| c["slot"].as_u64().unwrap()).collect()
}

// ------------------------------------------------------------------ (a) online with a 3.0 card

#[tokio::test(flavor = "multi_thread")]
async fn a_3_0_card_pays_online_and_holds_its_change() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    let id = provision(&t.reg, &card);
    t.load(&card, &[1 << 16, 1 << 14]).await?;
    // The load is on the card: SET_NOTE in the same tap.
    let notes = card.borrow().slot_state(1).unwrap();
    assert_eq!(notes, SlotState::Note);
    let (status, body) = t.charge(&card, 50_000, "a").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("offline").is_none(), "{body}");
    let change = slots_of(&body, "change");
    assert!(!change.is_empty(), "{body}");
    assert!(
        slot_states(&card, &change).iter().all(|s| *s == SlotState::Note),
        "the change is on the card: {:?}",
        slot_states(&card, &change)
    );
    for s in slots_of(&body, "inputs") {
        // Spent, then renewed in the same tap (GEN_KEY P2=1 on a SPENT slot).
        assert!(matches!(card.borrow().slot_state(s as u8).unwrap(), SlotState::Spent | SlotState::Keyed));
    }
    let open = t.reg.cards.lock().unwrap()[&id].txs.iter().filter(|x| x["status"] == "open").count();
    assert_eq!(open, 0, "the outcome reached the registry");
    Ok(())
}

/// Forget the certificate key the till keeps, as a till set up before it had one.
fn forget_cert_key(dir: &std::path::Path) {
    let p = dir.join("till-ops.json");
    let mut v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
    v.as_object_mut().unwrap().remove("cert_key");
    std::fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
    assert_eq!(fedimint_card_sidecar::claim::read_cert_key(dir).unwrap(), None);
}

/// A till that keeps no certificate key asks the registry for it when a card is tapped, keeps it,
/// and seals the 4.0 card's PIN to the card checked under it.
#[tokio::test(flavor = "multi_thread")]
async fn a_till_without_the_certificate_key_fetches_and_keeps_it_at_the_tap() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    provision(&t.reg, &card);
    forget_cert_key(t.dir.path());
    t.tap(&card);
    t.cw.pin = Some(b"1234".to_vec());
    let base = t.ops.attach(&mut t.cw).await?;
    let want = Some(hex::encode(t.reg.cert_pub()));
    assert_eq!(t.cw.ops.as_ref().unwrap().cert_key, want, "the tap has the registry's key");
    assert_eq!(fedimint_card_sidecar::claim::read_cert_key(t.dir.path())?, want, "and the till keeps it");
    t.cw.verify_pin_session()?;
    Ops::finish(&mut t.cw, base).await;
    t.cw.pin = None;
    Ok(())
}

/// A till that keeps no certificate key and cannot get one sends a 4.0 card no PIN: nothing is
/// asked of the card's PIN (no nonce, no try), and the merchant is told to retry once the till
/// has reached the registry (`offline_not_ready`). Once the registry has a key, the next tap
/// fetches it and the PIN goes.
#[tokio::test(flavor = "multi_thread")]
async fn a_till_that_cannot_get_the_certificate_key_sends_no_pin() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    provision(&t.reg, &card);
    forget_cert_key(t.dir.path());
    t.reg.no_cert.store(true, Ordering::SeqCst);
    let tries = card.borrow_mut().get_state()?.wrong_tries;
    t.tap(&card);
    t.cw.pin = Some(b"1234".to_vec());
    let base = t.ops.attach(&mut t.cw).await?;
    assert_eq!(t.cw.ops.as_ref().unwrap().cert_key, None);
    let e = t.cw.verify_pin_session().unwrap_err();
    let class = fedimint_card_sidecar::till::classify(&e);
    assert_eq!((class.status, class.code), (503, "offline_not_ready"), "{e:#}");
    assert!(format!("{e:#}").contains("no PIN was sent"), "{e:#}");
    assert!(!card.borrow().nonce_live(), "no GET_NONCE was asked");
    assert_eq!(card.borrow_mut().get_state()?.wrong_tries, tries, "no try was spent");
    Ops::finish(&mut t.cw, base).await;
    t.cw.pin = None;
    // What the merchant's app is told: the API answer for this refusal.
    let reply = fedimint_card_sidecar::till::error_reply(&e);
    assert_eq!((reply.status, reply.body["error"].as_str()), (503, Some("offline_not_ready")), "{}", reply.body);
    assert_eq!(fedimint_card_sidecar::claim::read_cert_key(t.dir.path())?, None, "nothing kept");

    t.reg.no_cert.store(false, Ordering::SeqCst);
    t.attach_tap(&card).await?;
    assert_eq!(fedimint_card_sidecar::claim::read_cert_key(t.dir.path())?, Some(hex::encode(t.reg.cert_pub())));
    Ok(())
}

/// A note the registry has not signed stays off the card and out of every plan; the till asks for
/// its signature again at each tap, and once the registry gives it, that tap writes it and the
/// charge spends it.
#[tokio::test(flavor = "multi_thread")]
async fn a_note_the_registry_has_not_signed_waits_for_a_later_tap() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    let id = provision(&t.reg, &card);
    t.reg.no_note_key.store(true, Ordering::SeqCst);
    t.load(&card, &[1 << 16]).await?;
    assert_eq!(card.borrow().slot_state(0).unwrap(), SlotState::Keyed, "no signature, so no SET_NOTE");
    let (status, body) = t.charge(&card, 20_000, "w1").await;
    assert_ne!(status, 200, "a note that cannot reach the card pays nothing: {body}");
    assert_eq!(body["error"], "insufficient", "the plan leaves the waiting note out: {body}");
    assert_eq!(card.borrow().slot_state(0).unwrap(), SlotState::Keyed, "nothing signed");
    assert!(t.reg.cards.lock().unwrap()[&id].txs.iter().all(|x| x["inputs"].as_array().is_none_or(|a| a.is_empty())), "no charge recorded");

    // The registry's key is back: the next tap repeats the load's outcome, the answer signs the
    // note, the tap writes it, and it pays.
    t.reg.no_note_key.store(false, Ordering::SeqCst);
    let (status, body) = t.charge(&card, 20_000, "w2").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(slots_of(&body, "inputs"), vec![0]);
    Ok(())
}

/// A stranger with the card in hand but not the PIN can only GEN_KEY its EMPTY slots. A till on
/// ops adopts those keys the next time it needs the slots (they are the card's own keys): the
/// top-up registers them, lands its notes in them, and nothing is lost.
#[tokio::test(flavor = "multi_thread")]
async fn a_stranger_keying_the_free_slots_without_the_pin_costs_a_top_up_nothing() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    let id = provision_split(&t.reg, &card, 1);
    let n = card.borrow().slot_len();
    // The stranger: a fresh selection, no PIN, and every EMPTY slot keyed.
    {
        let mut c = card.borrow_mut();
        c.apdu(&fedimint_card_host_card::SELECT_APDU)?;
        for slot in 1..n {
            assert_eq!(c.slot_state(slot), Some(SlotState::Empty));
            c.gen_key(slot)?;
        }
    }
    let stranger: Vec<XOnlyPublicKey> = (1..n).map(|s| card.borrow_mut().get_pub(s).unwrap()).collect();

    t.load(&card, &[1 << 16, 1 << 14, 1 << 12]).await?;
    let noted: Vec<u8> = (0..n).filter(|s| card.borrow().slot_state(*s).unwrap() == SlotState::Note).collect();
    assert_eq!(noted.len(), 3, "every note is on the card: {noted:?}");
    assert!(noted.iter().any(|s| *s >= 1), "a stranger-keyed slot holds a note: {noted:?}");
    {
        let regs = t.reg.cards.lock().unwrap();
        let slots = &regs[&id].slots;
        for s in noted.iter().filter(|s| **s >= 1) {
            let key = slots.iter().find(|(k, _)| k == s).map(|(_, v)| v.clone());
            assert_eq!(key, Some(stranger[*s as usize - 1].to_string()), "slot {s}: the registry holds the key the card holds");
        }
        // The registry holds a note on each of those slots' keys, and together they are the top-up.
        let on_keys: u64 =
            noted.iter().map(|s| note_on_key(&regs[&id], *s).expect("a note on the slot's key")["amount_msat"].as_u64().unwrap()).sum();
        assert_eq!(on_keys, (1 << 16) + (1 << 14) + (1 << 12), "nothing lost");
    }
    // The notes pay.
    let (status, body) = t.charge(&card, 20_000, "s1").await;
    assert_eq!(status, 200, "{body}");
    Ok(())
}

/// A load the registry does not know as a transaction (a holder's) is asked at the load route,
/// which spends the card's hourly load budget: only after `/outcome` answers 404, and at most
/// once an hour for the card.
#[tokio::test(flavor = "multi_thread")]
async fn a_holders_load_is_asked_for_its_signatures_at_the_load_route_at_most_once_an_hour() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    let id = provision(&t.reg, &card);
    t.reg.no_note_key.store(true, Ordering::SeqCst);
    t.load(&card, &[1 << 16]).await?;
    t.reg.cards.lock().unwrap().get_mut(&id).unwrap().txs.retain(|x| x["inputs"].as_array().is_some_and(|a| !a.is_empty()));
    let (status, body) = t.charge(&card, 20_000, "w1").await;
    assert_ne!(status, 200, "{body}");
    assert_eq!(t.reg.load_outcomes.lock().unwrap().len(), 1, "asked at the load route after the 404");
    t.reg.no_note_key.store(false, Ordering::SeqCst);
    let (status, body) = t.charge(&card, 20_000, "w2").await;
    assert_ne!(status, 200, "not asked again within the hour: {body}");
    assert_eq!(t.reg.load_outcomes.lock().unwrap().len(), 1);
    Ok(())
}

/// A repeat the registry refuses (409: it recorded another outcome) is not taken to the load
/// route; the note waits.
#[tokio::test(flavor = "multi_thread")]
async fn a_repeat_refused_with_409_never_goes_to_the_load_route() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    provision(&t.reg, &card);
    t.reg.no_note_key.store(true, Ordering::SeqCst);
    t.load(&card, &[1 << 16]).await?;
    for o in t.reg.outcomes.lock().unwrap().iter_mut() {
        o["change"] = json!([{"nonce": "00", "signature": "00"}]);
    }
    t.reg.no_note_key.store(false, Ordering::SeqCst);
    let (status, body) = t.charge(&card, 20_000, "w1").await;
    assert_ne!(status, 200, "the note still waits: {body}");
    assert!(t.reg.load_outcomes.lock().unwrap().is_empty(), "no load route after a 409");
    assert_eq!(card.borrow().slot_state(0).unwrap(), SlotState::Keyed);
    Ok(())
}

/// A `429` from the holder's load route (anyone with the card's short id can spend its hourly
/// budget) ends the tap's asking like an unreachable registry: it is no miss, so it never pushes
/// the load's back-off out.
#[tokio::test(flavor = "multi_thread")]
async fn a_load_route_429_is_no_miss_for_the_back_off() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    let id = provision(&t.reg, &card);
    t.reg.no_note_key.store(true, Ordering::SeqCst);
    t.load(&card, &[1 << 16]).await?;
    t.reg.cards.lock().unwrap().get_mut(&id).unwrap().txs.retain(|x| x["inputs"].as_array().is_some_and(|a| !a.is_empty()));
    t.reg.load_budget_spent.store(true, Ordering::SeqCst);
    let (status, body) = t.charge(&card, 20_000, "w1").await;
    assert_ne!(status, 200, "{body}");
    assert_eq!(t.reg.load_outcomes.lock().unwrap().len(), 1, "asked at the load route");
    let asks = StateStore::open(t.ops.card_path(&id))?.state.sig_asks;
    assert!(!asks.is_empty() && asks.values().all(|a| a.misses == 0 && a.load), "{asks:?}");
    Ok(())
}

/// The change of an offline charge that the registry could not sign at its outcome waits, and a
/// later tap gets its signature. The registry files the charge's inputs under the same txid, so
/// the repeat must send only the notes still waiting, never every note with the txid.
#[tokio::test(flavor = "multi_thread")]
async fn an_offline_charges_change_left_unsigned_is_signed_at_a_later_tap() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    let id = provision(&t.reg, &card);
    t.load(&card, &[1 << 16, 1 << 14]).await?;
    t.set_down(true);
    let (status, body) = t.charge(&card, 50_000, "b").await;
    assert_eq!((status, &body["offline"]), (200, &json!(true)), "{body}");
    let change = slots_of(&body, "change");
    assert!(!change.is_empty(), "{body}");
    t.set_down(false);
    t.reg.no_note_key.store(true, Ordering::SeqCst);
    let sent = t.send().await;
    assert_eq!(sent["settled"].as_array().unwrap().len(), 1, "{sent}");
    t.tap(&card);
    let r = card_balance_job(&mut t.cw, Some(&t.ops), &t.sales).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert!(slot_states(&card, &change).iter().all(|s| *s == SlotState::Change), "unsigned change stays off the card");

    // The key is back: the next tap's repeat sends the waiting change, and the tap writes it.
    t.reg.no_note_key.store(false, Ordering::SeqCst);
    t.tap(&card);
    let r = card_balance_job(&mut t.cw, Some(&t.ops), &t.sales).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert!(slot_states(&card, &change).iter().all(|s| *s == SlotState::Note), "{:?}", slot_states(&card, &change));
    let file = StateStore::open(t.ops.card_path(&id))?.state;
    let waiting: Vec<u8> = file
        .notes
        .iter()
        .filter(|n| n.status == NoteStatus::Live && n.signature.is_some() && n.reg_sig.is_none())
        .map(|n| n.slot)
        .collect();
    assert!(waiting.is_empty(), "nothing waits: {waiting:?}");
    Ok(())
}

// ------------------------------------------------------------------ (b) offline, then the sender

#[tokio::test(flavor = "multi_thread")]
async fn an_offline_charge_is_queued_then_sent_and_its_change_lands_on_the_card() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    let id = provision(&t.reg, &card);
    t.load(&card, &[1 << 16, 1 << 14]).await?;
    let bal0 = t.cw.wallet.balance_msat().await?;

    t.set_down(true);
    let (status, body) = t.charge(&card, 50_000, "b").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["offline"], true, "{body}");
    assert_eq!(body["collection_pending"], true);
    let (inputs, change) = (slots_of(&body, "inputs"), slots_of(&body, "change"));
    assert!(slot_states(&card, &inputs).iter().all(|s| *s == SlotState::Spent));
    assert!(!change.is_empty() && slot_states(&card, &change).iter().all(|s| *s == SlotState::Change), "{body}");
    let st = t.off.stats();
    assert_eq!((st.queued, st.double_spent), (1, 0));
    assert!(st.queued_msat >= 50_000);
    assert!(t.sale("b").offline);
    let queue: Value = serde_json::from_slice(&std::fs::read(t.dir.path().join("offline_queue.json"))?)?;
    assert_eq!(queue["charges"][0]["state"], "queued");
    assert!(queue["charges"][0]["signed_hex"].as_str().is_some());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(t.dir.path().join("offline_queue.json"))?.permissions().mode() & 0o777, 0o600);
    }
    // A repeat of the ref while offline is the same sale, not a second charge.
    let (s2, b2) = t.charge(&card, 50_000, "b").await;
    assert_eq!((s2, b2["txid"].clone()), (200, body["txid"].clone()), "{b2}");
    assert_eq!(t.off.stats().queued, 1);

    // The registry is back: report, send, collect, outcome.
    t.set_down(false);
    let sent = t.send().await;
    assert_eq!(sent["reported"].as_array().unwrap().len(), 1, "{sent}");
    assert_eq!(sent["settled"].as_array().unwrap().len(), 1, "{sent}");
    assert_eq!(t.off.stats().queued, 0);
    let sale = t.sale("b");
    assert!(sale.offline && !sale.collection_pending && !sale.double_spent, "{sale:?}");
    assert!(t.cw.wallet.balance_msat().await? > bal0, "the till's notes are in its wallet");
    let txid = body["txid"].as_str().unwrap();
    assert!(t.reg.outcomes.lock().unwrap().iter().any(|o| o["txid"] == txid && o["accepted"] == true), "the outcome was posted");
    for n in t.reg.cards.lock().unwrap()[&id].notes.iter().filter(|n| n["txid"] == txid) {
        assert!(n["status"] == "live" || n["status"] == "spent", "{n}");
    }
    // Nothing is left pending under it in the card's file either: its inputs, which the report had
    // the registry mark `pending` under it, are spent once it is settled.
    let file = StateStore::open(t.ops.card_path(&id))?.state;
    let notes: Vec<(u8, NoteStatus)> = file.notes.iter().filter(|n| n.txid.as_deref() == Some(txid)).map(|n| (n.slot, n.status)).collect();
    assert!(!notes.is_empty() && notes.iter().all(|(_, s)| *s != NoteStatus::Pending), "{notes:?}");
    assert_eq!(t.reg.reports.lock().unwrap().len(), 1);

    // The next tap writes the change to the card.
    t.tap(&card);
    let r = card_balance_job(&mut t.cw, Some(&t.ops), &t.sales).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert!(slot_states(&card, &change).iter().all(|s| *s == SlotState::Note), "{:?}", slot_states(&card, &change));
    Ok(())
}

// ------------------------------------------------------------------ (c) a double spend

#[tokio::test(flavor = "multi_thread")]
async fn notes_spent_elsewhere_make_the_offline_sale_a_double_spend() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    let id = provision(&t.reg, &card);
    t.load(&card, &[1 << 16]).await?;
    t.set_down(true);
    let (status, body) = t.charge(&card, 20_000, "c").await;
    assert_eq!(status, 200, "{body}");
    // Meanwhile another till reported the same note spent.
    let nonce = t.off.queue.lock().unwrap().file.charges[0].inputs[0].nonce.clone();
    t.reg.set_note_status(&id, &nonce, "spent");
    t.set_down(false);
    let sent = t.send().await;
    assert_eq!(sent["double_spent"].as_array().unwrap().len(), 1, "{sent}");
    let st = t.off.stats();
    assert_eq!((st.queued, st.double_spent), (0, 1));
    assert!(t.sale("c").double_spent);
    assert_eq!(t.reg.note_status(&id, &nonce).as_deref(), Some("spent"));
    Ok(())
}

// ------------------------------------------------------------------ (d) a cloned card

#[tokio::test(flavor = "multi_thread")]
async fn a_clone_with_an_uncertified_attestation_key_is_refused() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    provision(&t.reg, &card);
    t.load(&card, &[1 << 16]).await?;
    let clone = Rc::new(RefCell::new(card.borrow().extract_clone()));
    // A copy of the card's slots has no certified PIN key (the PIN key never leaves the card), so
    // no PIN session opens on it: the sealed VERIFY has no checked key to be sealed to.
    assert!(fedimint_card_host_card::softkit::open_test_session_under(&mut *clone.borrow_mut(), &t.reg.cert_pub()).is_err());
    t.set_down(true);
    let (status, body) = t.charge(&clone, 20_000, "d").await;
    assert_eq!((status, body["error"].as_str()), (403, Some("card_not_genuine")), "{body}");
    assert_eq!(t.off.stats().queued, 0);
    assert!((0..32).all(|s| !matches!(clone.borrow().slot_state(s), Some(SlotState::Spent | SlotState::Change))), "nothing was signed");
    // The genuine card still pays.
    let (status, body) = t.charge(&card, 20_000, "d2").await;
    assert_eq!((status, body["offline"].clone()), (200, json!(true)), "{body}");
    Ok(())
}

// ------------------------------------------------------------------ (e) caps, (f) a 2.1 card

#[tokio::test(flavor = "multi_thread")]
async fn the_tills_caps_and_a_2_1_card_are_refused() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig { cap_msat: 60_000, cap_count: 1, ..OfflineConfig::default() }).await?;
    let card = v3_card();
    provision(&t.reg, &card);
    t.load(&card, &[1 << 16, 1 << 15]).await?;
    let old = Rc::new(RefCell::new(SoftCard::new()));
    provision(&t.reg, &old);
    // A top-up is refused like a sale: nothing goes on the card, and no PIN command reaches it.
    let e = t.load(&old, &[1 << 16]).await.expect_err("a 2.1 card takes no top-up");
    assert!(fedimint_card_host_card::is_old_card(&e), "{e:#}");
    assert!(t.cw.store.state.notes.is_empty(), "the refused top-up recorded no note");
    assert!(!old.borrow().heard_a_pin_command(), "the top-up: the card received {:02x?}", old.borrow().received_ins());
    t.set_down(true);

    let (status, body) = t.charge(&old, 10_000, "f").await;
    // Applet 4.0's hosts take no older card at all: the old-card message, nothing signed, and no
    // PIN command, sealed or not, reaches the card.
    assert_eq!((status, body["error"].as_str()), (409, Some("old_card")), "{body}");
    assert_eq!(body["detail"].as_str(), Some(fedimint_card_host_card::OLD_CARD_MESSAGE), "{body}");
    assert_eq!(spent_slots(&old), 0);
    assert!(!old.borrow().heard_a_pin_command(), "the card received {:02x?}", old.borrow().received_ins());
    t.set_down(false);
    let (status, body) = t.charge(&old, 10_000, "f-online").await;
    assert_eq!((status, body["error"].as_str()), (409, Some("old_card")), "online: {body}");
    assert!(!old.borrow().heard_a_pin_command(), "online: the card received {:02x?}", old.borrow().received_ins());
    t.set_down(true);

    let (status, body) = t.charge(&card, 70_000, "e-big").await;
    assert_eq!((status, body["error"].as_str()), (503, Some("offline_cap_reached")), "over the msat cap: {body}");
    let (status, body) = t.charge(&card, 10_000, "e1").await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = t.charge(&card, 10_000, "e2").await;
    assert_eq!((status, body["error"].as_str()), (503, Some("offline_cap_reached")), "over the count cap: {body}");
    assert_eq!(t.off.stats().queued, 1);
    Ok(())
}

// ------------------------------------------------------------------ (g) a restart with a queued charge

#[tokio::test(flavor = "multi_thread")]
async fn a_queued_charge_survives_a_restart_and_is_sent() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    provision(&t.reg, &card);
    t.load(&card, &[1 << 16]).await?;
    t.set_down(true);
    let (status, body) = t.charge(&card, 20_000, "g").await;
    assert_eq!(status, 200, "{body}");
    // The process ends; the next one opens the queue, the sales and the card state from disk.
    t.off = Offline::open(t.dir.path(), OfflineConfig::default())?;
    t.sales = Arc::new(Mutex::new(SalesStore::open(t.dir.path().join("sales.json"))?));
    t.cw.store = StateStore::open(t.dir.path().join("card_state.json"))?;
    assert_eq!(t.off.stats().queued, 1);
    // Still down: the sender keeps it.
    let sent = t.send().await;
    assert!(sent["settled"].as_array().unwrap().is_empty() && !sent["errors"].as_array().unwrap().is_empty(), "{sent}");
    assert_eq!(t.off.stats().queued, 1);
    t.set_down(false);
    let sent = t.send().await;
    assert_eq!(sent["settled"].as_array().unwrap().len(), 1, "{sent}");
    assert_eq!(t.off.stats().queued, 0);
    assert!(!t.sale("g").collection_pending);
    // A second pass has nothing left to do.
    let sent = t.send().await;
    assert!(sent["reported"].as_array().unwrap().is_empty() && sent["settled"].as_array().unwrap().is_empty(), "{sent}");
    Ok(())
}

// ------------------------------------------------------------------ a card taken away mid-SIGN

/// The card, taken out of the field at its `at`th SIGN (from 0): that SIGN and every command
/// after it fail as a phone's card link reports a lost card. With `answer_lost` the card makes
/// that signature first, and only its answer is lost.
struct PulledAt {
    card: Shared,
    at: usize,
    signs: usize,
    pulled: bool,
    answer_lost: bool,
}

impl PulledAt {
    fn new(card: &Rc<RefCell<SoftCard>>, at: usize) -> Self {
        Self { card: Shared(card.clone()), at, signs: 0, pulled: false, answer_lost: false }
    }

    /// The card signs at its `at`th SIGN, then leaves the field before it answers.
    fn answer_lost(card: &Rc<RefCell<SoftCard>>, at: usize) -> Self {
        Self { answer_lost: true, ..Self::new(card, at) }
    }

    fn here(&self) -> anyhow::Result<()> {
        if self.pulled {
            anyhow::bail!(BridgeError::NoCard("POST /apdu: HTTP 503: the card left the field".into()));
        }
        Ok(())
    }
}

impl Card for PulledAt {
    fn gen_key(&mut self, slot: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.here()?;
        self.card.gen_key(slot)
    }
    fn gen_key_overwrite(&mut self, slot: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.here()?;
        self.card.gen_key_overwrite(slot)
    }
    fn get_pub(&mut self, slot: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.here()?;
        self.card.get_pub(slot)
    }
    fn sign(&mut self, slot: u8, msg32: &[u8; 32]) -> anyhow::Result<Signature> {
        let last = self.signs == self.at;
        self.pulled |= last && !self.answer_lost;
        self.here()?;
        self.signs += 1;
        let sig = self.card.sign(slot, msg32)?;
        // With `answer_lost` the card has made this signature; its answer leaves with the card.
        self.pulled |= last;
        self.here().map(|()| sig)
    }
    fn apdu(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.here()?;
        self.card.apdu(apdu)
    }
}

impl Till {
    /// An offline charge on a card that leaves the field at its `at`th SIGN.
    async fn charge_pulled(&mut self, card: &Rc<RefCell<SoftCard>>, at: usize, price: u64, cref: &str) -> (u16, Value) {
        self.charge_with_gone(PulledAt::new(card, at), price, cref).await
    }

    /// A charge on the card `gone`, which leaves the field at one of its SIGNs.
    async fn charge_with_gone(&mut self, gone: PulledAt, price: u64, cref: &str) -> (u16, Value) {
        self.cw.card = Box::new(gone);
        self.cw.pin = Some(TEST_PIN.to_vec());
        let r = charge_job_with(&mut self.cw, Some(&self.ops), Some(&self.off), price, Some(cref.into()), &self.sales).await;
        self.cw.pin = None;
        (r.status, r.body)
    }
}

fn spent_slots(card: &Rc<RefCell<SoftCard>>) -> usize {
    (0..32).filter(|s| card.borrow().slot_state(*s) == Some(SlotState::Spent)).count()
}

/// A card taken away while it signs leaves a `signing` charge; tapped again for the same sale (the
/// same `client_ref`), the till finishes that charge from the card's stored signature and answers
/// it. Before ENG-842 the retap went on to sign a second transaction for the same sale from the
/// card's other notes: the holder paid twice.
#[tokio::test(flavor = "multi_thread")]
async fn a_retap_after_the_card_left_mid_sign_is_the_same_charge() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    provision(&t.reg, &card);
    // Four notes of 65,536 msat: a 90,000 msat charge takes two, and two are left for another.
    t.load(&card, &[1 << 16, 1 << 16, 1 << 16, 1 << 16]).await?;
    t.set_down(true);
    let (status, body) = t.charge_pulled(&card, 1, 90_000, "pulled").await;
    assert_eq!((status, body["error"].as_str()), (202, Some("undecided")), "{body}");
    let first = t.off.queue.lock().unwrap().file.charges.clone();
    assert_eq!(first.len(), 1);
    assert_eq!((first[0].state, first[0].inputs.len()), (ChargeState::Signing, 2));
    assert_eq!(spent_slots(&card), 1, "the card signed one input before it left");

    let (status, body) = t.charge(&card, 90_000, "pulled").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["txid"], json!(first[0].txid), "the charge the card began, not a second one: {body}");
    assert_eq!(body["offline"], true, "{body}");
    let q = t.off.queue.lock().unwrap().file.charges.clone();
    assert_eq!(
        q.iter().map(|c| (c.txid.as_str(), c.state)).collect::<Vec<_>>(),
        vec![(first[0].txid.as_str(), ChargeState::Queued)],
        "one charge for one sale"
    );
    assert_eq!(spent_slots(&card), 2, "two inputs, each signed once");
    assert!(t.sale("pulled").offline);
    // A third tap of the sale is the sale.
    let (status, again) = t.charge(&card, 90_000, "pulled").await;
    assert_eq!((status, &again["txid"]), (200, &body["txid"]), "{again}");
    assert_eq!((t.off.stats().queued, spent_slots(&card)), (1, 2));
    Ok(())
}

/// Another card tapped for a sale whose first card left while it signed is refused `undecided`:
/// the first card may have paid already, and only its next tap can say. Nothing is signed on the
/// second card; the first card's tap finishes the sale.
#[tokio::test(flavor = "multi_thread")]
async fn another_card_for_a_sale_a_card_left_signing_is_refused_undecided() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (first, second) = (v3_card(), v3_card());
    provision(&t.reg, &first);
    provision(&t.reg, &second);
    t.load(&first, &[1 << 16, 1 << 16]).await?;
    t.load(&second, &[1 << 16, 1 << 16]).await?;
    t.set_down(true);
    let (status, body) = t.charge_pulled(&first, 1, 90_000, "two-cards").await;
    assert_eq!((status, body["error"].as_str()), (202, Some("undecided")), "{body}");

    let (status, body) = t.charge(&second, 90_000, "two-cards").await;
    assert_eq!((status, body["error"].as_str()), (202, Some("undecided")), "{body}");
    assert_eq!(spent_slots(&second), 0, "nothing signed on the second card");
    assert_eq!(t.off.queue.lock().unwrap().file.charges.len(), 1);

    let (status, body) = t.charge(&first, 90_000, "two-cards").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!((t.off.stats().queued, spent_slots(&first), spent_slots(&second)), (1, 2, 0));
    Ok(())
}

/// A sale whose card left mid-SIGN offline, tapped again once the registry is back: the tap goes
/// the offline way, never the online charge (the card's other notes would pay the sale a second
/// time, and the half-signed charge would still be finished at the card's next offline tap).
/// Another card, 3.0 or 2.1, is `undecided` with nothing signed; the card that began the charge
/// finishes it, answered as that charge, and the next send reports and settles it. Until
/// ENG-842's review an online retap was `undecided` every time, and ringing the sale again
/// charged the card twice.
#[tokio::test(flavor = "multi_thread")]
async fn an_online_retap_of_a_sale_left_signing_offline_finishes_it() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (card, other, old) = (v3_card(), v3_card(), Rc::new(RefCell::new(SoftCard::new())));
    provision(&t.reg, &card);
    provision(&t.reg, &other);
    provision(&t.reg, &old);
    t.load(&card, &[1 << 16, 1 << 16, 1 << 16, 1 << 16]).await?;
    t.load(&other, &[1 << 16, 1 << 16]).await?;
    t.set_down(true);
    let (status, body) = t.charge_pulled(&card, 1, 90_000, "pulled").await;
    assert_eq!((status, body["error"].as_str()), (202, Some("undecided")), "{body}");
    let first = t.off.queue.lock().unwrap().file.charges.clone();
    assert_eq!((first.len(), first[0].state), (1, ChargeState::Signing));

    t.set_down(false);
    // Another card waits for the first; a card older than 4.0 is refused outright.
    for (which, c, want) in [("another 4.0 card", &other, (202, "undecided")), ("a 2.1 card", &old, (409, "old_card"))] {
        let (status, body) = t.charge(c, 90_000, "pulled").await;
        assert_eq!((status, body["error"].as_str()), (want.0, Some(want.1)), "{which}: {body}");
        assert_eq!(spent_slots(c), 0, "{which}: nothing signed");
    }
    assert!(!old.borrow().heard_a_pin_command(), "a 2.1 card hears no PIN command: {:02x?}", old.borrow().received_ins());
    assert!(t.sales.lock().unwrap().find_ref("pulled").is_none(), "no sale");
    assert_eq!(t.off.queue.lock().unwrap().file.charges, first, "the offline charge still waits for its card");

    // Still online: the card that began the charge finishes it, and nothing is charged online.
    let (status, body) = t.charge(&card, 90_000, "pulled").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["txid"], json!(first[0].txid), "the charge the card began, not a second one: {body}");
    assert_eq!(body["offline"], true, "{body}");
    let q = t.off.queue.lock().unwrap().file.charges.clone();
    assert_eq!(
        q.iter().map(|c| (c.txid.as_str(), c.state)).collect::<Vec<_>>(),
        vec![(first[0].txid.as_str(), ChargeState::Queued)],
        "one charge for one sale"
    );
    assert_eq!(spent_slots(&card), 2, "two inputs, each signed once");
    assert!(t.sale("pulled").offline);
    let sent = t.send().await;
    assert_eq!((&sent["reported"], &sent["settled"]), (&json!([first[0].txid]), &json!([first[0].txid])), "{sent}");
    assert_eq!(t.off.stats().queued, 0);
    assert!(!t.sale("pulled").collection_pending);
    // A tap of the sale from now on is the sale.
    let (status, again) = t.charge(&card, 90_000, "pulled").await;
    assert_eq!((status, &again["txid"]), (200, &body["txid"]), "{again}");
    assert_eq!(spent_slots(&card), 2);
    Ok(())
}

// ------------------------------------------------------------------ phones charging offline (ENG-842)

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 86_400_000;

impl Till {
    fn keys(&self) -> TillKeys {
        TillKeys::of(&self.cw.wallet)
    }

    /// The kit `GET /v1/offline/kit` issues `phone` at `now`.
    fn kit_at(&self, phone: &str, now: u64) -> anyhow::Result<Kit> {
        let cert = fedimint_card_sidecar::claim::read_cert_key(self.dir.path())?;
        pool::issue_kit(&self.off.pool, &self.keys(), &self.off.cfg, &self.off.kits, cert, None, phone, now)
    }

    /// `POST /v1/offline/charges` from `phone`, at `now`.
    fn ingest_at(&self, phone: &str, charges: Vec<Value>, now: u64) -> Vec<pool::Ingested> {
        pool::ingest(&self.off.queue, &self.off.pool, &self.keys(), &self.sales, phone, charges, now)
    }

    fn ingest(&self, phone: &str, charges: Vec<Value>) -> Vec<pool::Ingested> {
        self.ingest_at(phone, charges, now_ms())
    }

    fn ledger(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.dir.path().join(pool::POOL_FILE)).unwrap()).unwrap()
    }
}

/// A POS phone paired with the till: its store (the core's), charging cards offline.
struct Phone {
    _dir: tempfile::TempDir,
    store: Store,
}

impl Phone {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        Self { _dir: dir, store }
    }

    /// Another store with this phone's id: a phone restored from a backup (Risk 4).
    fn restored_copy(&self) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(self.store.dir().join("phone-id"), dir.path().join("phone-id")).unwrap();
        let store = Store::open(dir.path()).unwrap();
        Self { _dir: dir, store }
    }

    fn id(&self) -> String {
        self.store.phone_id().to_owned()
    }

    fn take(&self, card: &Rc<RefCell<SoftCard>>, kit: &Kit, price: u64, cref: &str) -> anyhow::Result<OfflineCharge> {
        let t = self.store.charge(&mut Shared(card.clone()), &serde_json::to_string(kit)?, price, cref, b"1234")?;
        Ok(t.charge)
    }

    /// What a sync posts.
    fn drained(&self) -> Vec<Value> {
        let d: Value = serde_json::from_str(&self.store.drain(50).body).unwrap();
        d["charges"].as_array().unwrap().clone()
    }
}

fn statuses(r: &[pool::Ingested]) -> Vec<(&str, Option<&str>)> {
    r.iter().map(|x| (x.status, x.code)).collect()
}

/// A loaded, certified card and a phone with a kit from the till.
async fn phone_and_card(t: &mut Till, notes: &[u64]) -> anyhow::Result<(Rc<RefCell<SoftCard>>, Phone, Kit)> {
    let card = v3_card();
    provision(&t.reg, &card);
    t.load(&card, notes).await?;
    let phone = Phone::new();
    let kit = t.kit_at(&phone.id(), now_ms())?;
    Ok((card, phone, kit))
}

#[tokio::test(flavor = "multi_thread")]
async fn kits_are_written_before_they_are_answered_given_again_capped_and_reclaimed() -> anyhow::Result<()> {
    let t = Till::new(OfflineConfig::default()).await?;
    let phone = "ab".repeat(16);
    let now = now_ms();
    let k1 = t.kit_at(&phone, now)?;
    // The kit: the till's caps and keys, a week, and a pool from 4 msat (fedimint's default fees)
    // to the largest note under the value cap, 2^26 msat.
    assert_eq!((k1.phone_id.as_str(), k1.cap_msat, k1.cap_count), (phone.as_str(), 100_000_000, 200));
    assert_eq!(k1.expires_at_ms - k1.issued_at_ms, 7 * DAY_MS);
    let tiers: Vec<u64> = k1.pool.iter().map(|t| t.amount_msat).collect();
    assert_eq!((tiers.first(), tiers.last(), tiers.len()), (Some(&4), Some(&(1 << 26)), 25), "{tiers:?}");
    assert!(k1.pool.iter().all(|t| t.blinded.len() == pool::KIT_DEPTH), "{:?}", k1.depth());
    assert!(serde_json::to_vec(&k1)?.len() < fedimint_card_pos_core::kit::KIT_MAX_BYTES);
    assert_eq!(k1.cert_key, hex::encode(t.reg.cert_pub()));
    // Written before it was answered: every entry issued, with its secrets, in a private file.
    let l = t.ledger();
    assert_eq!(l["kits"][0]["kit_id"], json!(k1.kit_id));
    let entries = l["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 25 * pool::KIT_DEPTH);
    assert!(entries.iter().all(|e| e["state"]["kind"] == "issued" && e["state"]["secret_key"].as_str().is_some_and(|s| s.len() == 64)));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(t.dir.path().join(pool::POOL_FILE))?.permissions().mode() & 0o777, 0o600);
    }
    // Asked again within the hour: the same kit. Later, a new one; at three live kits, the newest.
    assert_eq!(t.kit_at(&phone, now + 60_000)?, k1);
    let k2 = t.kit_at(&phone, now + HOUR_MS + 1)?;
    let k3 = t.kit_at(&phone, now + 2 * HOUR_MS + 2)?;
    assert!(k2.kit_id != k1.kit_id && k3.kit_id != k2.kit_id);
    assert_eq!(t.kit_at(&phone, now + 3 * HOUR_MS + 3)?.kit_id, k3.kit_id, "three live kits: the newest again");
    // Another phone gets its own.
    assert_ne!(t.kit_at(&"cd".repeat(16), now)?.kit_id, k1.kit_id);
    // Past the redeem window (a week and 30 days): the kits' unused entries are dropped, secrets
    // and all, and each kit's record stays as its tombstone.
    let later = now + 38 * DAY_MS;
    let k4 = t.kit_at(&phone, later)?;
    assert!(![&k1, &k2, &k3].iter().any(|k| k.kit_id == k4.kit_id));
    let l = t.ledger();
    for k in [&k1, &k2, &k3] {
        let kit = l["kits"].as_array().unwrap().iter().find(|x| x["kit_id"] == json!(k.kit_id)).unwrap();
        assert_eq!(kit["reclaimed"], true, "{kit}");
        assert!(l["entries"].as_array().unwrap().iter().all(|e| e["kit_id"] != json!(k.kit_id)), "kit {}'s entries are gone", k.kit_id);
    }
    // Only the live kit's entries are left: the other phone's kit is past its window too.
    assert_eq!(l["entries"].as_array().unwrap().len(), 25 * pool::KIT_DEPTH);
    // A till that never fetched the registry's certificate key issues none.
    let e = pool::issue_kit(&t.off.pool, &t.keys(), &t.off.cfg, &t.off.kits, None, None, &phone, now).unwrap_err();
    assert_eq!(till::classify(&e), till::Class { status: 503, code: "offline_not_ready" });
    Ok(())
}

/// A kit given again is good for a week from that fetch, and at the cap of three live kits the
/// newest is given again with its tiers topped up once one runs low, under the same id: a shop
/// fetching its kit every few minutes never runs on a kit about to expire or drained.
#[tokio::test(flavor = "multi_thread")]
async fn a_kit_given_again_is_good_from_that_fetch_and_topped_up_at_the_cap() -> anyhow::Result<()> {
    let t = Till::new(OfflineConfig::default()).await?;
    let phone = "ab".repeat(16);
    let now = now_ms();
    t.kit_at(&phone, now)?;
    t.kit_at(&phone, now + HOUR_MS + 1)?;
    let k3 = t.kit_at(&phone, now + 2 * HOUR_MS + 2)?;
    // Day 6, 23 h after the first fetch: three kits live, so the newest, good for a week from now.
    let at = now + 6 * DAY_MS + 23 * HOUR_MS;
    let again = t.kit_at(&phone, at)?;
    assert_eq!((again.kit_id.as_str(), &again.pool), (k3.kit_id.as_str(), &k3.pool), "the same kit, the same entries");
    assert_eq!((again.issued_at_ms, again.expires_at_ms), (k3.issued_at_ms, at + 7 * DAY_MS));
    // Written before it was answered, with its redeem window moved too.
    let rec = |l: &Value| l["kits"].as_array().unwrap().iter().find(|k| k["kit_id"] == json!(k3.kit_id)).unwrap().clone();
    let l = t.ledger();
    assert_eq!((rec(&l)["expires_at_ms"].as_u64(), rec(&l)["redeem_until_ms"].as_u64()), (Some(at + 7 * DAY_MS), Some(at + 37 * DAY_MS)));
    assert_eq!(l["entries"].as_array().unwrap().len(), 3 * 25 * pool::KIT_DEPTH, "no entry added");
    // Asked again ten minutes later: the kit as it is, and no write.
    let file = t.dir.path().join(pool::POOL_FILE);
    let bytes = std::fs::read(&file)?;
    assert_eq!(t.kit_at(&phone, at + 10 * 60_000)?, again);
    assert_eq!(std::fs::read(&file)?, bytes, "nothing written");

    // The newest kit drained at the cap: one tier is down to 2 entries.
    let tier = again.pool[3].clone();
    let drained: Vec<String> = tier.blinded[2..].to_vec();
    t.off.pool.lock().unwrap().commit(|f| {
        for e in f.entries.iter_mut().filter(|e| drained.contains(&e.blinded)) {
            e.state = pool::EntryState::Used { txid: "00".repeat(32) };
        }
    })?;
    let topped = t.kit_at(&phone, at + 20 * 60_000)?;
    assert_eq!(topped.kit_id, k3.kit_id);
    assert!(topped.pool.iter().all(|t| t.blinded.len() == pool::KIT_DEPTH), "{:?}", topped.depth());
    let same = topped.pool.iter().find(|t| t.amount_msat == tier.amount_msat).unwrap();
    assert!(tier.blinded[..2].iter().all(|b| same.blinded.contains(b)), "the entries it had are still there");
    assert!(same.blinded.iter().all(|b| !drained.contains(b)), "a used entry is never given again");
    let l = t.ledger();
    assert_eq!(l["entries"].as_array().unwrap().len(), 3 * 25 * pool::KIT_DEPTH + drained.len(), "only what was used is added");
    assert_eq!(l["kits"].as_array().unwrap().iter().filter(|k| k["phone_id"] == json!(phone)).count(), 3, "never a fourth live kit");
    Ok(())
}

/// The ledger keeps what it needs: a request's charges are one write of the queue and one of the
/// ledger, and once a kit is past its redeem window its unused entries go while the used ones stay,
/// so a charge sent again after the sender settled it is still `already`.
#[tokio::test(flavor = "multi_thread")]
async fn the_ledger_takes_a_request_in_one_write_and_keeps_only_the_used_entries_of_old_kits() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (card, phone, kit) = phone_and_card(&mut t, &[1 << 16, 1 << 16, 1 << 16, 1 << 16]).await?;
    let a = phone.take(&card, &kit, 20_000, "one")?;
    let b = phone.take(&card, &kit, 20_000, "two")?;
    // A fault armed for the second write of each file: one request writes each once.
    t.off.queue.lock().unwrap().fail_write_after(Some(1));
    t.off.pool.lock().unwrap().fail_write_after(Some(1));
    assert_eq!(statuses(&t.ingest(&phone.id(), phone.drained())), vec![("queued", None), ("queued", None)]);
    t.off.queue.lock().unwrap().fail_write_after(None);
    t.off.pool.lock().unwrap().fail_write_after(None);
    let used_by = |l: &Value, txid: &str| l["entries"].as_array().unwrap().iter().filter(|e| e["state"]["txid"] == json!(txid)).count();
    let l = t.ledger();
    assert_eq!((used_by(&l, &a.txid), used_by(&l, &b.txid)), (a.pool.len(), b.pool.len()), "both marked used on disk");
    assert_eq!(t.off.stats().queued, 2);
    // Settled: the sender takes them out of the queue.
    let sent = t.send().await;
    assert_eq!(sent["settled"].as_array().unwrap().len(), 2, "{sent}");
    assert!(t.off.queue.lock().unwrap().file.charges.is_empty());
    // Past the kit's redeem window, the next kit call drops its unused entries.
    let later = kit.expires_at_ms + 31 * DAY_MS;
    t.kit_at(&phone.id(), later)?;
    let l = t.ledger();
    let of_kit: Vec<&Value> = l["entries"].as_array().unwrap().iter().filter(|e| e["kit_id"] == json!(kit.kit_id)).collect();
    assert_eq!(of_kit.len(), a.pool.len() + b.pool.len(), "only the used entries are kept");
    assert!(of_kit.iter().all(|e| e["state"]["kind"] == "used"));
    // Sent again: the ledger knows them.
    assert_eq!(statuses(&t.ingest_at(&phone.id(), phone.drained(), later)), vec![("already", None), ("already", None)]);
    assert!(t.off.queue.lock().unwrap().file.charges.is_empty(), "nothing queued twice");
    Ok(())
}

/// A request whose queue write fails is `retry` for every charge it held, a charge sent twice in
/// it included: nothing is the till's, and no sale is recorded.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_the_queue_cannot_take_is_retry_for_each_charge_in_it() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (card, phone, kit) = phone_and_card(&mut t, &[1 << 16, 1 << 16]).await?;
    let taken = phone.take(&card, &kit, 20_000, "twice")?;
    let c = phone.drained().remove(0);
    let twice = vec![c.clone(), c];
    t.off.queue.lock().unwrap().fail_write_after(Some(0));
    let r = t.ingest(&phone.id(), twice.clone());
    assert_eq!(r.iter().map(|x| (x.status, x.code, x.retry)).collect::<Vec<_>>(), vec![("refused", Some("internal"), true); 2]);
    assert_eq!(t.off.stats().queued, 0);
    assert!(t.sales.lock().unwrap().find_ref("twice").is_none(), "no sale for a charge the till does not have");
    assert!(t.ledger()["entries"].as_array().unwrap().iter().all(|e| e["state"]["kind"] == "issued"));
    // Sent again: the first is taken, and the second is the first.
    assert_eq!(statuses(&t.ingest(&phone.id(), twice)), vec![("queued", None), ("already", None)]);
    assert_eq!((t.off.stats().queued, t.sale("twice").txid), (1, taken.txid));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_charge_is_taken_once_and_a_pool_entry_never_twice() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (card, phone, kit) = phone_and_card(&mut t, &[1 << 16, 1 << 16, 1 << 16]).await?;
    let taken = phone.take(&card, &kit, 30_000, "p1")?;
    let sent = phone.drained();
    assert_eq!(statuses(&t.ingest(&phone.id(), sent.clone())), vec![("queued", None)]);
    // The till's queue has it, with the till's secrets for the phone's pool outputs.
    {
        let q = t.off.queue.lock().unwrap();
        let c = q.file.charges.iter().find(|c| c.txid == taken.txid).expect("queued");
        assert_eq!(c.state, ChargeState::Queued);
        assert_eq!(c.host.len(), c.pool.len());
        assert!(
            c.host.iter().zip(&c.pool).all(|(h, p)| h.out_idx == Some(p.out_idx) && h.amount_msat == p.amount_msat && h.group == c.txid)
        );
        assert_eq!((c.till_msat, c.unsigned_hex.clone()), (taken.till_msat, taken.unsigned_hex.clone()));
        assert!(taken.inputs.iter().all(|i| q.file.spent.contains(&i.nonce)), "the till never takes those notes offline itself");
    }
    let s = t.sale("p1");
    assert!(s.offline && s.collection_pending && s.txid == taken.txid);
    // The same again (the answer was lost): `already`, nothing twice.
    assert_eq!(statuses(&t.ingest(&phone.id(), sent.clone())), vec![("already", None)]);
    assert_eq!(t.off.stats().queued, 1);
    let used = t.ledger()["entries"].as_array().unwrap().iter().filter(|e| e["state"]["kind"] == "used").count();
    assert_eq!(used, taken.pool.len());
    assert!(
        t.ledger()["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["state"]["kind"] == "used")
            .all(|e| e["state"]["secret_key"].is_null())
    );

    // The phone restored from a backup takes the same pool entries again: refused.
    let copy = phone.restored_copy();
    let card2 = v3_card();
    provision(&t.reg, &card2);
    t.load(&card2, &[1 << 16, 1 << 16]).await?;
    let again = copy.take(&card2, &kit, 30_000, "p-copy")?;
    assert!(again.pool.iter().any(|p| taken.pool.iter().any(|q| q.blinded == p.blinded)), "the copy reused an entry");
    assert_eq!(statuses(&t.ingest(&copy.id(), copy.drained())), vec![("refused", Some("pool_entry_reused"))]);

    // Another phone's kit entries under this phone's id: not issued to it.
    let other = Phone::new();
    let other_kit = t.kit_at(&other.id(), now_ms())?;
    other.take(&card2, &other_kit, 10_000, "p-other")?;
    assert_eq!(statuses(&t.ingest(&phone.id(), other.drained())), vec![("refused", Some("pool_entry_unknown"))]);
    assert_eq!(statuses(&t.ingest(&other.id(), other.drained())), vec![("queued", None)]);

    // Past its kit's redeem window: refused.
    let late = phone.take(&card, &kit, 20_000, "p-late")?;
    let body: Vec<Value> = phone.drained().into_iter().filter(|c| c["txid"] == json!(late.txid)).collect();
    assert_eq!(
        statuses(&t.ingest_at(&phone.id(), body.clone(), kit.expires_at_ms + 31 * DAY_MS)),
        vec![("refused", Some("pool_entry_expired"))]
    );
    assert_eq!(statuses(&t.ingest(&phone.id(), body)), vec![("queued", None)], "within it, taken");
    Ok(())
}

/// A copy of `charge` with its first input's note signature replaced by `sig` and the txid, the
/// declared input and the bytes made to agree: a note no federation signed.
fn forged_note(t: &Till, charge: &Value, sig: &str) -> Value {
    use fedimint_core::core::IntoDynInstance as _;
    use fedimint_core::encoding::{Decodable, Encodable};
    use fedimint_mint_common::{MintInput, Note};
    let decoders = fedimint_card_pos_core::charge::mint_decoders(t.cw.wallet.mint_id);
    let mut tx = fedimint_core::transaction::Transaction::consensus_decode_hex(charge["signed_hex"].as_str().unwrap(), &decoders).unwrap();
    let v0 = tx.inputs[0].as_any().downcast_ref::<MintInput>().unwrap().maybe_v0_ref().unwrap().clone();
    let signature = tbs::Signature::consensus_decode_hex(sig, &Default::default()).unwrap();
    tx.inputs[0] = MintInput::new_v0(v0.amount, Note { nonce: v0.note.nonce, signature }).into_dyn(t.cw.wallet.mint_id);
    let mut c = charge.clone();
    c["txid"] = json!(tx.tx_hash().to_string());
    c["inputs"][0]["signature"] = json!(sig);
    c["signed_hex"] = json!(tx.consensus_encode_to_hex());
    c
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_charge_with_a_bad_signature_or_note_is_refused() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (card, phone, kit) = phone_and_card(&mut t, &[1 << 16, 1 << 16, 1 << 14]).await?;
    phone.take(&card, &kit, 90_000, "two-inputs")?;
    let c = phone.drained().remove(0);
    // The card's signatures over another message: the txid (signatures aside) is the same.
    let mut spoiled = c.clone();
    spoiled["signed_hex"] = json!(spoil_signatures(&t, c["signed_hex"].as_str().unwrap()));
    assert_eq!(statuses(&t.ingest(&phone.id(), vec![spoiled])), vec![("refused", Some("bad_signature"))]);
    // A note signed by no one: its second input's signature on the first note.
    let forged = forged_note(&t, &c, c["inputs"][1]["signature"].as_str().unwrap());
    assert_eq!(statuses(&t.ingest(&phone.id(), vec![forged])), vec![("refused", Some("bad_note"))]);
    // Bytes that are not the declared charge.
    let mut lying = c.clone();
    lying["till_msat"] = json!(1);
    lying["pool"][0]["amount_msat"] = json!(1);
    assert_eq!(
        statuses(&t.ingest(&phone.id(), vec![lying, json!({"txid": "x"})])),
        vec![("refused", Some("bad_charge")), ("refused", Some("bad_charge"))]
    );
    assert_eq!(t.off.stats().queued, 0, "nothing refused was queued");
    // The charge itself is fine.
    assert_eq!(statuses(&t.ingest(&phone.id(), vec![c])), vec![("queued", None)]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_between_the_queue_and_the_ledger_is_made_good() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (card, phone, kit) = phone_and_card(&mut t, &[1 << 16, 1 << 16]).await?;
    let taken = phone.take(&card, &kit, 30_000, "crash")?;
    t.off.pool.lock().unwrap().fail_write_after(Some(0));
    // The queue is written, the ledger's write fails: the charge is the till's all the same.
    assert_eq!(statuses(&t.ingest(&phone.id(), phone.drained())), vec![("queued", None)]);
    assert!(t.ledger()["entries"].as_array().unwrap().iter().all(|e| e["state"]["kind"] == "issued"), "the ledger is behind");
    // The next process: the ledger is brought up to the queue.
    t.off = Offline::open(t.dir.path(), OfflineConfig::default())?;
    let l = t.ledger();
    let used: Vec<&Value> = l["entries"].as_array().unwrap().iter().filter(|e| e["state"]["kind"] == "used").collect();
    assert_eq!(used.len(), taken.pool.len());
    assert!(used.iter().all(|e| e["state"]["txid"] == json!(taken.txid)));
    assert_eq!(statuses(&t.ingest(&phone.id(), phone.drained())), vec![("already", None)]);
    Ok(())
}

/// Decision 6 (2026-11-09): the registry down puts the till offline; a phone's charge is taken
/// into the queue with no call to the federation and nothing in the wallet; the registry back, the
/// sender settles it and the wallet rises by exactly what the till was paid.
#[tokio::test(flavor = "multi_thread")]
async fn with_the_registry_down_a_phone_charge_waits_then_settles_exactly() -> anyhow::Result<()> {
    // No fees: what the till's notes are worth is what reaches the wallet.
    let mut t = Till::new_fees(OfflineConfig::default(), false).await?;
    let (card, phone, kit) = phone_and_card(&mut t, &[1 << 16, 1 << 14]).await?;
    t.set_down(true);
    let (reg, _) = t.off.reach.probe(Some(&t.ops), &t.cw.wallet, t.dir.path()).await;
    assert_eq!(reg, Some(false), "the till is offline");
    let bal0 = t.cw.wallet.balance_msat().await?;
    let taken = phone.take(&card, &kit, 50_000, "settle")?;
    // Ingest is the signed bytes checked against the kit's keys: nothing to ask the federation.
    assert_eq!(statuses(&t.ingest(&phone.id(), phone.drained())), vec![("queued", None)]);
    assert_eq!(t.cw.wallet.balance_msat().await?, bal0, "nothing in the wallet yet");
    assert!(t.reg.reports.lock().unwrap().is_empty());
    let sent = t.send().await;
    assert!(sent["settled"].as_array().unwrap().is_empty(), "still offline: {sent}");
    t.set_down(false);
    let sent = t.send().await;
    assert_eq!((sent["reported"].as_array().unwrap().len(), sent["settled"].as_array().unwrap().len()), (1, 1), "{sent}");
    assert_eq!(t.cw.wallet.balance_msat().await? - bal0, taken.till_msat, "the till's pool notes, whole");
    let sale = t.sale("settle");
    assert!(sale.offline && !sale.collection_pending && !sale.double_spent, "{sale:?}");
    assert_eq!(t.off.stats().queued, 0);
    // The card's change lands at its next tap.
    t.tap(&card);
    let r = card_balance_job(&mut t.cw, Some(&t.ops), &t.sales).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    let change: Vec<u64> = taken.change.iter().map(|c| c.slot as u64).collect();
    assert!(slot_states(&card, &change).iter().all(|s| *s == SlotState::Note), "{:?}", slot_states(&card, &change));
    Ok(())
}

/// The registry sees a phone's charge as it sees the till's own: the same report, accepted.
#[tokio::test(flavor = "multi_thread")]
async fn a_phones_charge_reports_as_the_tills_own() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (card, phone, kit) = phone_and_card(&mut t, &[1 << 16, 1 << 16, 1 << 16]).await?;
    t.set_down(true);
    let (status, body) = t.charge(&card, 20_000, "till-took").await;
    assert_eq!((status, body["offline"].clone()), (200, json!(true)), "{body}");
    let phone_took = phone.take(&card, &kit, 20_000, "phone-took")?;
    assert_eq!(statuses(&t.ingest(&phone.id(), phone.drained())), vec![("queued", None)]);
    let bodies: Vec<Value> = t.off.queue.lock().unwrap().file.charges.iter().map(|c| c.report_body()).collect();
    let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(bodies.len(), 2);
    assert_eq!(keys(&bodies[0]), keys(&bodies[1]));
    assert_eq!(keys(&bodies[0]["change"][0]), keys(&bodies[1]["change"][0]));
    t.set_down(false);
    let sent = t.send().await;
    assert_eq!((sent["reported"].as_array().unwrap().len(), sent["settled"].as_array().unwrap().len()), (2, 2), "{sent}");
    assert!(t.reg.reports.lock().unwrap().iter().any(|r| r["txid"] == json!(phone_took.txid) && r["client_ref"] == "phone-took"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_offline_routes_answer_through_the_tills_api() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    provision(&t.reg, &card);
    t.load(&card, &[1 << 16, 1 << 14]).await?;
    let phone = Phone::new();
    let conn = Conn::new(OpsState::Connected { url: t.ops.client.url().to_owned() }, Some(t.ops.clone()));
    let sales = SalesStore::open(t.dir.path().join("sales-http.json"))?;
    let off = t.off.clone();
    let (handle, _actor) = till::new_with_conn(t.cw, "http://127.0.0.1:1".into(), sales, Box::new(|| anyhow::bail!("no card")), conn).await;
    // The till keeps a block header (it has an Esplora by default): its kits carry it.
    let headers = fedimint_card_sidecar::clock::Headers::new(None, t.dir.path());
    assert!(headers.offer(hex::decode(H900200)?.try_into().unwrap())?);
    let app = till::router(handle.with_offline(off).with_headers(headers));
    let call = |req: axum::http::Request<axum::body::Body>| {
        let app = app.clone();
        async move {
            let resp = app.oneshot(req).await.unwrap();
            let (status, cache) = (resp.status().as_u16(), resp.headers().get("cache-control").map(|v| v.to_str().unwrap().to_owned()));
            let v: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 22).await.unwrap()).unwrap_or(Value::Null);
            (status, cache, v)
        }
    };
    let get = |q: &str| axum::http::Request::get(format!("/v1/offline/kit{q}")).body(axum::body::Body::empty()).unwrap();
    let (s, _, v) = call(get("")).await;
    assert_eq!((s, v["error"].as_str()), (400, Some("bad_request")), "{v}");
    let (s, cache, v) = call(get(&format!("?phone={}", phone.id()))).await;
    assert_eq!((s, cache.as_deref()), (200, Some("no-store")), "{v}");
    let kit = Kit::parse(&v.to_string())?;
    assert_eq!(kit.phone_id, phone.id());
    assert_eq!(kit.block_header.as_deref(), Some(H900200), "the till's newest header");
    kit.decode(&phone.id())?;
    phone.take(&card, &kit, 20_000, "http")?;
    let header_time = fedimint_card_host_card::v3::header_time(&hex::decode(H900200)?.try_into().unwrap());
    assert_eq!(card.borrow_mut().get_state()?.now, header_time, "the card's clock moved to the kit's header");
    let post = |body: String| {
        axum::http::Request::post("/v1/offline/charges")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap()
    };
    let (s, _, v) = call(post(phone.store.drain(50).body)).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["results"][0]["status"], "queued", "{v}");
    let r = phone.store.mark_synced(&v.to_string())?;
    assert_eq!(r.synced, 1);
    let (s, _, v) = call(post(json!({"phone_id": "zz", "charges": []}).to_string())).await;
    assert_eq!((s, v["error"].as_str()), (400, Some("bad_request")), "{v}");
    let many: Vec<Value> = (0..51).map(|_| json!({})).collect();
    let (s, _, _) = call(post(json!({"phone_id": phone.id(), "charges": many}).to_string())).await;
    assert_eq!(s, 400);
    let (s, _, _) = call(post("x".repeat(fedimint_card_sidecar::pos::MAX_CHARGES_BODY + 1))).await;
    assert_eq!(s, 413, "the body limit");
    Ok(())
}

// ------------------------------------------------------------------ the queue file from 4301b0a

/// A queue an earlier till (4301b0a) wrote, with a charge queued offline, loads unchanged: the
/// fields the phone charges added (`pool`, `synced`, `pool_used`, `high_water_ms`) are absent from
/// it and default.
#[test]
fn a_queue_from_4301b0a_loads() -> anyhow::Result<()> {
    let golden = std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/offline_queue_4301b0a.json"))?;
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("offline_queue.json"), &golden)?;
    let off = Offline::open(dir.path(), OfflineConfig::default())?;
    let q = off.queue.lock().unwrap();
    assert_eq!(q.file.version, 1);
    let c = &q.file.charges[0];
    assert_eq!((c.client_ref.as_deref(), c.state, c.price_msat, c.till_msat), (Some("golden-1"), ChargeState::Queued, 50_000, 51_736));
    assert_eq!((c.inputs.len(), c.change.len(), c.host.len()), (1, 3, 6));
    assert!(c.pool.is_empty() && !c.synced && q.file.pool_used.is_empty() && q.file.high_water_ms == 0);
    assert_eq!(q.file.spent, vec![c.inputs[0].nonce.clone()]);
    assert_eq!(q.stats().queued, 1);
    assert!(c.report_body()["signed_hex"].as_str().is_some_and(|s| !s.is_empty()));
    // Saved again, it reads back the same.
    drop(q);
    off.queue.lock().unwrap().save()?;
    let again = Offline::open(dir.path(), OfflineConfig::default())?;
    assert_eq!(again.queue.lock().unwrap().file.charges, off.queue.lock().unwrap().file.charges);
    Ok(())
}

// ------------------------------------------------------------------ status

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_reachability_and_the_offline_queue() -> anyhow::Result<()> {
    let t = Till::new(OfflineConfig { cap_msat: 5_000_000, cap_count: 7, ..OfflineConfig::default() }).await?;
    t.set_down(true);
    let conn = Conn::new(OpsState::Connected { url: t.ops.client.url().to_owned() }, Some(t.ops.clone()));
    let sales = SalesStore::open(t.dir.path().join("sales2.json"))?;
    let off = t.off.clone();
    let (handle, _actor) = till::new_with_conn(t.cw, "http://127.0.0.1:1".into(), sales, Box::new(|| anyhow::bail!("no card")), conn).await;
    let app = till::router(handle.with_offline(off));
    let req = axum::http::Request::get("/v1/status").body(axum::body::Body::empty())?;
    let resp = app.oneshot(req).await?;
    let v: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 20).await?)?;
    assert_eq!(v["registry_reachable"], false, "{v}");
    assert_eq!(v["federation_reachable"], true, "{v}");
    // The test federation runs core consensus 2.1: a surplus is allowed, so offline charges keep
    // a fee margin (none charged yet).
    assert_eq!(
        v["offline"],
        json!({
            "queued": 0, "queued_msat": 0, "double_spent": 0, "refused": 0, "rejected": 0, "cap_msat": 5_000_000, "cap_count": 7,
            "fee_margin": true, "fee_headroom_base_msat": 100, "fee_headroom_ppm": 0, "last_fee_margin_msat": null,
        })
    );
    Ok(())
}

// ------------------------------------------------------------------ the clock

const H900000: &str = "00a0ab20247d4d9f582f9750344cdf62c46d81d046be960340960100000000000000000070f96945530651135839d8adc3f40e595118ec74c7ad81a3d17bb022e554fb0c937f4268743702177ad05f92";
const H900200: &str = "0060002058d02283d148685d2413fdf9a66317225589b86a56e2000000000000000000002c29575cba6c0fc3b8e366bf820d4d526ba300cd8915ee4ac075a32aafa6e9302e59446874370217546eb104";

/// A fake Esplora answering one tip, which the test can move.
async fn esplora(tip: Arc<Mutex<&'static str>>) -> String {
    let t1 = tip.clone();
    let app = axum::Router::new()
        .route(
            "/blocks/tip/hash",
            get(move || {
                let t = t1.clone();
                async move {
                    let h: [u8; 80] = hex::decode(*t.lock().unwrap()).unwrap().try_into().unwrap();
                    fedimint_card_host_card::v3::header_hash_hex(&h)
                }
            }),
        )
        .route("/block/{hash}/header", get(move |Path(_h): Path<String>| async move { tip.lock().unwrap().to_string() }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tills_header_moves_the_cards_clock_and_an_outage_moves_nothing() -> anyhow::Result<()> {
    use fedimint_card_sidecar::clock::Headers;
    let dir = tempfile::tempdir()?;
    let tip = Arc::new(Mutex::new(H900000));
    let url = esplora(tip.clone()).await;
    let headers = Headers::new(Some(url), dir.path());
    assert!(headers.latest().is_none());
    headers.refresh().await?;
    let t0 = fedimint_card_host_card::v3::header_time(&headers.latest().unwrap());
    *tip.lock().unwrap() = H900200;
    headers.refresh().await?;
    let t1 = fedimint_card_host_card::v3::header_time(&headers.latest().unwrap());
    assert!(t1 > t0);
    // Kept across a restart, from the data directory.
    let again = Headers::new(None, dir.path());
    assert_eq!(again.latest(), headers.latest());
    // An unreachable Esplora is an error to log, never a panic, and keeps the header.
    let gone = Headers::new(Some("http://127.0.0.1:1".into()), dir.path());
    assert!(gone.refresh().await.is_err());
    assert_eq!(gone.latest(), headers.latest());
    // An older tip moves nothing.
    *tip.lock().unwrap() = H900000;
    headers.refresh().await?;
    assert_eq!(fedimint_card_host_card::v3::header_time(&headers.latest().unwrap()), t1);

    // On a tap the card's clock moves to the till's header.
    let fed = federation().await?;
    let wallet = Wallet::from_client(fed.new_client().await).await?;
    let card = v3_card();
    let mut cw = CardWallet::new(wallet, StateStore::open(dir.path().join("card_state.json"))?, Box::new(Shared(card.clone())));
    cw.card_tick();
    assert_eq!(card.borrow_mut().get_state()?.now, 0, "no header kept: the clock stays");
    cw.headers = Some(headers);
    cw.card_tick();
    assert_eq!(card.borrow_mut().get_state()?.now, t1);
    // A 2.1 card is left alone.
    cw.card = Box::new(SoftCard::new());
    cw.card_tick();
    Ok(())
}

// ------------------------------------------------------------------ releases (§ 6)

#[tokio::test(flavor = "multi_thread")]
async fn a_double_spend_releases_the_change_slots_at_the_next_online_tap() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    // Few keyed slots: 0 to 7 for loads, the change slots 24 to 31.
    let id = provision_keys(&t.reg, &card, &[0, 1, 2, 3, 4, 5, 6, 7, 24, 25, 26, 27, 28, 29, 30, 31]);
    t.load(&card, &[1 << 16]).await?;
    t.set_down(true);
    let (status, body) = t.charge(&card, 20_000, "r1").await;
    assert_eq!(status, 200, "{body}");
    let change = slots_of(&body, "change");
    assert!(!change.is_empty() && slot_states(&card, &change).iter().all(|s| *s == SlotState::Change), "{body}");
    assert!(change.iter().all(|s| *s >= 24), "{body}");
    let nonce = t.off.queue.lock().unwrap().file.charges[0].inputs[0].nonce.clone();
    t.reg.set_note_status(&id, &nonce, "spent");
    t.set_down(false);
    let sent = t.send().await;
    assert_eq!(sent["double_spent"].as_array().unwrap().len(), 1, "{sent}");
    let detail = t.off.queue.lock().unwrap().file.charges[0].detail.clone().unwrap();
    assert!(detail.contains("released at its next online tap"), "{detail}");
    let listed: Vec<u64> = t.reg.cards.lock().unwrap()[&id].releases.iter().map(|r| r["slot"].as_u64().unwrap()).collect();
    assert_eq!(listed, change, "a release per change slot, none for the input");

    // The next online tap: CHANGE -> KEYED. The load goes below 24, never into them.
    t.load(&card, &[1 << 15]).await?;
    assert!(slot_states(&card, &change).iter().all(|s| *s == SlotState::Keyed), "{:?}", slot_states(&card, &change));
    let input = slots_of(&body, "inputs")[0];
    assert_eq!(card.borrow().slot_state(input as u8), Some(SlotState::Spent), "the input was spent elsewhere: not released");
    assert!((24..32).all(|s| card.borrow().slot_state(s) != Some(SlotState::Note)), "no load in a change slot");
    // An offline charge with change, into the released slots again: change slots are taken
    // lowest first, and the released ones are the lowest.
    t.set_down(true);
    let (status, body) = t.charge(&card, 10_000, "r2").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["offline"], true, "{body}");
    let change2 = slots_of(&body, "change");
    assert!(!change2.is_empty() && change2.contains(&change[0]), "change into the released slots: {body}");
    assert!(slot_states(&card, &change2).iter().all(|s| *s == SlotState::Change), "{:?}", slot_states(&card, &change2));
    Ok(())
}

/// Replace a signed transaction's card signatures with ones over another message: the federation
/// refuses it, its inputs unspent.
fn spoil_signatures(t: &Till, signed_hex: &str) -> String {
    use fedimint_core::encoding::{Decodable, Encodable};
    use fedimint_core::secp256k1::{Keypair, Message, SECP256K1};
    use fedimint_core::transaction::{Transaction, TransactionSignature};
    let mut tx = Transaction::consensus_decode_hex(signed_hex, t.cw.wallet.client.decoders()).unwrap();
    let TransactionSignature::NaiveMultisig(sigs) = &tx.signatures else { panic!("multisig") };
    let kp = Keypair::new(SECP256K1, &mut rand::thread_rng());
    let other = sigs.iter().map(|_| SECP256K1.sign_schnorr(&Message::from_digest([7; 32]), &kp)).collect();
    tx.signatures = TransactionSignature::NaiveMultisig(other);
    tx.consensus_encode_to_hex()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_charge_the_federation_rejects_releases_its_note_and_change_and_the_note_pays_again() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    let id = provision(&t.reg, &card);
    t.load(&card, &[1 << 16]).await?;
    t.set_down(true);
    let (status, body) = t.charge(&card, 20_000, "f1").await;
    assert_eq!(status, 200, "{body}");
    let (inputs, change) = (slots_of(&body, "inputs"), slots_of(&body, "change"));
    assert_eq!(inputs.len(), 1);
    assert!(!change.is_empty());
    // The bytes the till sends no longer carry the card's signatures.
    {
        let signed = t.off.queue.lock().unwrap().file.charges[0].signed_hex.clone().unwrap();
        let spoiled = spoil_signatures(&t, &signed);
        let mut q = t.off.queue.lock().unwrap();
        q.file.charges[0].signed_hex = Some(spoiled);
        q.save()?;
    }
    t.set_down(false);
    let sent = t.send().await;
    assert_eq!(sent["rejected"].as_array().unwrap().len(), 1, "{sent}");
    let txid = body["txid"].as_str().unwrap();
    assert!(
        t.reg.outcomes.lock().unwrap().iter().any(|o| o["txid"] == txid && o["accepted"] == false),
        "the rejection reached the registry"
    );
    let detail = t.off.queue.lock().unwrap().file.charges[0].detail.clone().unwrap();
    assert!(detail.contains("released at its next online tap"), "{detail}");
    let mut want: Vec<u64> = inputs.iter().chain(&change).copied().collect();
    want.sort();
    let listed: Vec<u64> = t.reg.cards.lock().unwrap()[&id].releases.iter().map(|r| r["slot"].as_u64().unwrap()).collect();
    assert_eq!(listed, want);
    assert_eq!(slot_states(&card, &inputs), vec![SlotState::Spent]);

    // The next online tap releases the input: a note again. The change slots carry the rejected
    // sale's dead notes, so they stay CHANGE until a tap has checked the PIN.
    assert!(change.iter().all(|s| *s >= 24), "{body}");
    let old_keys: Vec<String> = change.iter().map(|s| card.borrow_mut().get_pub(*s as u8).unwrap().to_string()).collect();
    t.tap(&card);
    let r = card_balance_job(&mut t.cw, Some(&t.ops), &t.sales).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(slot_states(&card, &inputs), vec![SlotState::Note]);
    assert!(slot_states(&card, &change).iter().all(|s| *s == SlotState::Change), "{:?}", slot_states(&card, &change));
    // A wrong PIN releases nothing.
    t.tap(&card);
    t.cw.pin = Some(b"9999".to_vec());
    let r = t.ops.attach(&mut t.cw).await;
    t.cw.pin = None;
    assert!(r.is_err(), "the wrong PIN ends the tap");
    assert!(slot_states(&card, &change).iter().all(|s| *s == SlotState::Change), "{:?}", slot_states(&card, &change));
    // The right PIN: each change slot released and re-keyed in one step, with the spare reserve
    // off (it keys nothing): the unit is outside its budget. The registry hears the new keys.
    t.cw.spares = fedimint_card_sidecar::ops::SpareConfig { target: 12, max_per_tap: 0 };
    t.attach_tap(&card).await?;
    t.cw.spares = Default::default();
    assert!(slot_states(&card, &change).iter().all(|s| *s == SlotState::Keyed), "{:?}", slot_states(&card, &change));
    for (s, old) in change.iter().zip(&old_keys) {
        let new = card.borrow_mut().get_pub(*s as u8)?.to_string();
        assert_ne!(&new, old, "slot {s} re-keyed");
        let reg = t.reg.cards.lock().unwrap()[&id].slots.iter().find(|(x, _)| u64::from(*x) == *s).map(|(_, k)| k.clone());
        assert_eq!(reg.as_deref(), Some(new.as_str()), "the registry has slot {s}'s new key");
    }
    // And that note pays an online charge.
    let (status, body) = t.charge(&card, 20_000, "f2").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("offline").is_none(), "{body}");
    assert_eq!(slots_of(&body, "inputs"), inputs);
    Ok(())
}

/// A tap cut between a dead change slot's RELEASE and its re-key leaves the slot KEYED on the key
/// that carried the dead note. A tap without the PIN leaves it so; the next tap with the PIN
/// re-keys it.
#[tokio::test(flavor = "multi_thread")]
async fn a_change_slot_released_but_not_rekeyed_is_rekeyed_at_the_next_pin_tap() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    let id = provision(&t.reg, &card);
    t.load(&card, &[1 << 16]).await?;
    t.set_down(true);
    let (status, body) = t.charge(&card, 20_000, "t1").await;
    assert_eq!(status, 200, "{body}");
    let change = slots_of(&body, "change");
    assert!(!change.is_empty() && change.iter().all(|s| *s >= 24), "{body}");
    {
        let signed = t.off.queue.lock().unwrap().file.charges[0].signed_hex.clone().unwrap();
        let spoiled = spoil_signatures(&t, &signed);
        let mut q = t.off.queue.lock().unwrap();
        q.file.charges[0].signed_hex = Some(spoiled);
        q.save()?;
    }
    t.set_down(false);
    let sent = t.send().await;
    assert_eq!(sent["rejected"].as_array().unwrap().len(), 1, "{sent}");

    // The cut: RELEASE took, the re-key did not.
    let slot = change[0] as u8;
    let old = card.borrow_mut().get_pub(slot)?;
    let (txid, sig) = {
        let g = t.reg.cards.lock().unwrap();
        let r = g[&id].releases.iter().find(|r| r["slot"] == u64::from(slot)).unwrap().clone();
        let txid: [u8; 32] = hex::decode(r["txid"].as_str().unwrap())?.try_into().unwrap();
        (txid, hex::decode(r["sig"].as_str().unwrap())?)
    };
    card.borrow_mut().release(slot, &txid, &sig)?;
    assert_eq!(card.borrow().slot_state(slot), Some(SlotState::Keyed));

    // No PIN: left as it is.
    t.tap(&card);
    let r = card_balance_job(&mut t.cw, Some(&t.ops), &t.sales).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(card.borrow().slot_state(slot), Some(SlotState::Keyed));
    assert_eq!(card.borrow_mut().get_pub(slot)?, old, "no PIN: not re-keyed");

    // The PIN: re-keyed, the reserve off.
    t.cw.spares = fedimint_card_sidecar::ops::SpareConfig { target: 12, max_per_tap: 0 };
    t.attach_tap(&card).await?;
    t.cw.spares = Default::default();
    assert_eq!(card.borrow().slot_state(slot), Some(SlotState::Keyed));
    let new = card.borrow_mut().get_pub(slot)?;
    assert_ne!(new, old, "re-keyed");
    let reg = t.reg.cards.lock().unwrap()[&id].slots.iter().find(|(x, _)| *x == slot).map(|(_, k)| k.clone());
    assert_eq!(reg, Some(new.to_string()), "the registry has the new key");
    Ok(())
}

// ------------------------------------------------------------------ a sale settled at a tap

impl Till {
    /// The sender's first half only: queued charges reported to the registry and written into
    /// their cards' files, none sent to the federation.
    async fn report_only(&mut self) -> fedimint_card_sidecar::offline::Sent {
        self.off.reach.reset();
        let mut sent = fedimint_card_sidecar::offline::Sent::default();
        fedimint_card_sidecar::offline::report(&self.off, &self.ops, None, &mut sent).await;
        sent
    }
}

/// An offline sale of 20,000 msat from a card loaded with one note, reported but not sent: the
/// card's next online tap is the one that sends it. Answers the sale's body and its change slots.
async fn reported_not_sent(t: &mut Till, card: &Rc<RefCell<SoftCard>>, cref: &str) -> anyhow::Result<(Value, Vec<u64>)> {
    provision(&t.reg, card);
    t.load(card, &[1 << 16]).await?;
    t.set_down(true);
    let (status, body) = t.charge(card, 20_000, cref).await;
    assert_eq!((status, body["offline"].clone()), (200, json!(true)), "{body}");
    let change = slots_of(&body, "change");
    assert!(!change.is_empty() && slot_states(card, &change).iter().all(|s| *s == SlotState::Change), "{body}");
    t.set_down(false);
    let sent = t.report_only().await;
    assert_eq!(sent.reported.len(), 1, "{sent:?}");
    Ok((body, change))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_balance_tap_that_settles_an_offline_sale_writes_its_change() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    let (body, change) = reported_not_sent(&mut t, &card, "s2a").await?;
    t.balance(&card).await;
    let txid = body["txid"].as_str().unwrap();
    assert!(t.reg.outcomes.lock().unwrap().iter().any(|o| o["txid"] == txid && o["accepted"] == true), "the outcome was posted");
    assert!(slot_states(&card, &change).iter().all(|s| *s == SlotState::Note), "{:?}", slot_states(&card, &change));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_charge_after_an_offline_sale_spends_its_change() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    let (_, change) = reported_not_sent(&mut t, &card, "s2b").await?;
    // The card holds no other note: only the change can pay.
    let (status, body) = t.charge(&card, 2_000, "s2c").await;
    assert_eq!(status, 200, "{body}");
    let inputs = slots_of(&body, "inputs");
    assert!(!inputs.is_empty() && inputs.iter().all(|s| change.contains(s)), "{body}");
    Ok(())
}

/// The queue's entry for `txid`, as written to `offline_queue.json`.
fn queue_entry(t: &Till, txid: &str) -> anyhow::Result<Value> {
    let queue: Value = serde_json::from_slice(&std::fs::read(t.dir.path().join("offline_queue.json"))?)?;
    Ok(queue["charges"].as_array().unwrap().iter().find(|c| c["txid"] == txid).cloned().expect("in the queue"))
}

/// Nothing of the sale `txid` went to the federation or into the card's file, and its queue entry
/// is `entry`.
async fn never_sent(t: &Till, id: &str, txid: &str, inputs: &[String], entry: &Value, at: &str) -> anyhow::Result<()> {
    for n in inputs {
        assert!(!t.cw.is_spent(fedimint_mint_common::Nonce(n.parse()?)).await?, "{at}: input {n} is spent at the federation");
    }
    let file = StateStore::open(t.ops.card_path(id))?.state;
    assert!(!file.pending_txs.iter().any(|p| p.txid == txid), "{at}: the card's file holds the sale");
    assert_eq!(&queue_entry(t, txid)?, entry, "{at}: the queue entry changed");
    Ok(())
}

async fn refused_sale_never_sent(why: &'static str, by_phone: bool, one_note: bool) -> anyhow::Result<()> {
    let at = format!("{why}, {}", if by_phone { "a phone's sale" } else { "the till's own sale" });
    let mut t = Till::new(OfflineConfig::default()).await?;
    let notes: &[u64] = if one_note { &[1 << 16] } else { &[1 << 16, 1 << 16] };
    let (card, phone, kit) = phone_and_card(&mut t, notes).await?;
    let txid = if by_phone {
        let c = phone.take(&card, &kit, 20_000, "n1")?;
        assert_eq!(statuses(&t.ingest(&phone.id(), phone.drained())), vec![("queued", None)], "{at}");
        c.txid
    } else {
        t.set_down(true);
        let (status, body) = t.charge(&card, 20_000, "n1").await;
        assert_eq!((status, body["offline"].clone()), (200, json!(true)), "{at}: {body}");
        t.set_down(false);
        body["txid"].as_str().unwrap().to_owned()
    };
    let (id, inputs) = {
        let q = t.off.queue.lock().unwrap();
        let c = q.file.charges.iter().find(|c| c.txid == txid).expect("queued");
        assert!(!c.change.is_empty(), "{at}: a sale with change");
        (c.short_id.clone(), c.inputs.iter().map(|i| i.nonce.clone()).collect::<Vec<_>>())
    };
    *t.reg.refuse_reports.lock().unwrap() = Some(why);
    let sent = t.send().await;
    let (key, state) = if why == "change_used" { ("double_spent", ChargeState::DoubleSpent) } else { ("refused", ChargeState::Refused) };
    assert_eq!(sent[key], json!([txid]), "{at}: {sent}");
    assert_eq!(t.off.queue.lock().unwrap().file.charges.iter().find(|c| c.txid == txid).map(|c| c.state), Some(state), "{at}");
    let entry = queue_entry(&t, &txid)?;
    never_sent(&t, &id, &txid, &inputs, &entry, &format!("{at}, after the send")).await?;
    t.send().await;
    never_sent(&t, &id, &txid, &inputs, &entry, &format!("{at}, after a second send")).await?;
    t.balance(&card).await;
    never_sent(&t, &id, &txid, &inputs, &entry, &format!("{at}, after a balance tap")).await?;
    // The card signed the refused sale with its input: the till holds that note back and reports
    // it, so the registry can give it back. A charge the card's other notes pay goes ahead; on a
    // card whose only note it is, the charge is refused `notes_held`. Either way the sale stays
    // unsent.
    for i in 0..2 {
        let (status, body) = t.charge(&card, 2_000, &format!("n2-{i}")).await;
        if one_note {
            assert_eq!((status, body["error"].as_str()), (402, Some("notes_held")), "{at}: {body}");
            let detail = body["detail"].as_str().unwrap();
            assert!(detail.contains("held") && detail.contains("other notes cannot pay"), "{at}: {detail}");
        } else {
            assert_eq!(status, 200, "{at}, charge {i}: {body}");
        }
        never_sent(&t, &id, &txid, &inputs, &entry, &format!("{at}, after charge tap {i}")).await?;
    }
    let b = t.balance(&card).await;
    assert_eq!(b["stranded_msat"], json!(1 << 16), "{at}: {b}");
    let held = on_hold(&b);
    assert_eq!(held.len(), 1, "{at}: {b}");
    assert!(t.stranded_posts() >= 1, "{at}: the held input is reported");
    let row = t.stranded_post(0);
    assert!(row.iter().any(|x| x["slot"] == json!(held[0]) && x["kind"] == "spent" && x["txid"] == json!(txid)), "{at}: {row:?}");
    Ok(())
}

/// A card whose only note is the input of a sale the registry refused: the charge is refused as
/// `notes_held`, saying what is held, and the sale stays unsent.
#[tokio::test(flavor = "multi_thread")]
async fn a_charge_the_notes_held_for_a_refused_sale_would_pay_is_refused_as_held() -> anyhow::Result<()> {
    for by_phone in [false, true] {
        for why in ["change_used", "bad_signature"] {
            refused_sale_never_sent(why, by_phone, true).await?;
        }
    }
    Ok(())
}

/// A sale the registry refused, as `change_used` (which the registry now answers as a double
/// spend, releasing its change slots) or for another
/// reason, whether the till made it or a phone's sync brought it: its signed bytes never reach
/// the federation, at the send, a second send, a balance tap or a charge tap of its card. Its
/// input is held and reported, and the card's other note pays.
#[tokio::test(flavor = "multi_thread")]
async fn signed_bytes_of_a_sale_the_registry_refused_are_never_sent_to_the_federation() -> anyhow::Result<()> {
    for by_phone in [false, true] {
        for why in ["change_used", "bad_signature"] {
            refused_sale_never_sent(why, by_phone, false).await?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ spare slots (§ 10)

/// Keyed slots on the card (any state but EMPTY).
fn keyed_on(card: &Rc<RefCell<SoftCard>>) -> usize {
    let n = card.borrow().slot_len();
    (0..n).filter(|s| card.borrow().slot_state(*s) != Some(SlotState::Empty)).count()
}

/// Slots KEYED on the card whose key carried no note in the registry: room for change offline.
fn fresh_on(t: &Till, id: &str, card: &Rc<RefCell<SoftCard>>) -> usize {
    let g = t.reg.cards.lock().unwrap();
    let c = &g[id];
    c.slots
        .iter()
        .filter(|(s, k)| {
            card.borrow().slot_state(*s) == Some(SlotState::Keyed)
                && !c.notes.iter().any(|n| n["slot"] == *s && n["nonce"].as_str().is_some_and(|x| x.ends_with(k.as_str())))
        })
        .count()
}

/// The smallest whole-sat price from `from_sats` up that the card's notes pay only with change
/// (no exact plan without it), as the till's planner sees them.
fn price_with_change(t: &Till, card: &Rc<RefCell<SoftCard>>, from_sats: u64) -> u64 {
    use fedimint_card_sidecar::plan::{MAX_CARD_NOTES, solve};
    let n = card.borrow().slot_len();
    let notes: Vec<u8> = (0..n).filter(|s| card.borrow().slot_state(*s) == Some(SlotState::Note)).collect();
    let amounts: Vec<u64> = notes.into_iter().map(|s| card.borrow_mut().get_slot(s).unwrap().amount_msat).collect();
    let w = &t.cw.wallet;
    let fee = |a: u64| w.fee(a);
    (from_sats..from_sats + 500)
        .map(|s| s * 1_000)
        .find(|p| solve(&amounts, *p, 0, &fee, w.min_denom()).is_err() && solve(&amounts, *p, MAX_CARD_NOTES, &fee, w.min_denom()).is_ok())
        .expect("a price that needs change")
}

/// The F6 drill's card (OFFLINE-PLAN.md § 10): registered with keys 0-7 of 32, loaded with ten
/// notes, which keys five more below 24 (the shortfall plus KEY_AHEAD): 10 notes and 3 spare
/// slots below 24. With the spare reserve on, the load's tap also keys change slots 24 and 25.
async fn drill_card(t: &mut Till) -> anyhow::Result<(Rc<RefCell<SoftCard>>, String)> {
    let card = v3_card();
    let id = provision_split(&t.reg, &card, 8);
    t.load(&card, &[(1 << 20) - (1 << 10)]).await?;
    let keyed_below = |c: &Rc<RefCell<SoftCard>>| (0..24).filter(|s| c.borrow().slot_state(*s) != Some(SlotState::Empty)).count();
    assert_eq!(keyed_below(&card), 13);
    let change: Vec<u8> = if t.cw.spares.target == 0 { vec![] } else { vec![24, 25] };
    assert_eq!((24..32).filter(|s| card.borrow().slot_state(*s) == Some(SlotState::Keyed)).collect::<Vec<u8>>(), change);
    assert_eq!(fresh_on(t, &id, &card), 3 + change.len());
    Ok((card, id))
}

/// Before the reserve, the drill's third offline charge was refused for want of a slot, and the
/// POS read it as "card needs a top up". Now the refusal says what it is and what the card can pay.
#[tokio::test(flavor = "multi_thread")]
async fn without_spare_slots_an_offline_charge_needing_change_is_refused_with_a_price_that_fits() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    t.cw.spares = SpareConfig { target: 0, max_per_tap: 0 };
    let (card, _id) = drill_card(&mut t).await?;
    let price = price_with_change(&t, &card, 50);
    let (status, body) = t.charge(&card, price, "d1").await;
    assert_eq!(status, 200, "{body}");
    assert!(!slots_of(&body, "change").is_empty(), "{body}");
    t.set_down(true);
    let mut refused = None;
    let mut taken = 0;
    for i in 0..6 {
        let price = price_with_change(&t, &card, 10 + 10 * i);
        let (status, body) = t.charge(&card, price, &format!("d-off-{i}")).await;
        if status != 200 {
            refused = Some((price, status, body));
            break;
        }
        assert_eq!(body["offline"], true, "{body}");
        assert!(!slots_of(&body, "change").is_empty(), "{body}");
        taken += 1;
    }
    let (price, status, body) = refused.expect("the card runs out of slots for change");
    // Without the reserve no change slot (24 to 31) is keyed, so no offline charge takes change.
    assert_eq!(taken, 0, "without the reserve the card has no slot for change offline");
    assert_eq!((status, body["error"].as_str()), (409, Some("offline_no_change_room")), "{body}");
    let detail = body["detail"].as_str().unwrap();
    assert!(detail.contains("needs change") && detail.contains("offline"), "{detail}");
    let suggested = body["suggested_msat"].as_u64().expect("a suggestion");
    assert_ne!(suggested, price);
    assert_eq!(suggested % 1_000, 0, "whole sats: {body}");
    for k in ["suggest_below_msat", "suggest_above_msat"] {
        assert!(body[k].is_null() || body[k].as_u64().is_some(), "{body}");
    }
    assert!(body["suggest_below_msat"].as_u64() == Some(suggested) || body["suggest_above_msat"].as_u64() == Some(suggested), "{body}");
    // The suggested price pays offline, exactly: no change.
    let (status, body) = t.charge(&card, suggested, "d-suggested").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["offline"], true, "{body}");
    assert!(slots_of(&body, "change").is_empty(), "{body}");
    Ok(())
}

/// Online taps top a 3.0 card's fresh keyed slots up to the target, at most MAX_KEY_PER_TAP per
/// tap, renewing settled SPENT slots before keying new ones, and the registry has every key.
#[tokio::test(flavor = "multi_thread")]
async fn online_taps_keep_a_spare_reserve_that_offline_change_can_use() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    assert_eq!(t.cw.spares, SpareConfig { target: SPARE_TARGET, max_per_tap: MAX_KEY_PER_TAP });
    assert_eq!(SPARE_TARGET, 12);
    // The drill's card has 3 spare slots below 24 and change slots 24 and 25 after its load; a
    // target of 11 keys the other six change slots, two a tap. The rule is the default's.
    const TARGET: usize = 11;
    t.cw.spares = SpareConfig { target: TARGET, max_per_tap: MAX_KEY_PER_TAP };
    let (card, id) = drill_card(&mut t).await?;

    // Taps with nothing else to do: +2 three times to the target, then nothing.
    let mut keyed = vec![];
    for _ in 0..4 {
        let (before, posts) = (keyed_on(&card), t.reg.key_posts.lock().unwrap().len());
        t.attach_tap(&card).await?;
        let added = keyed_on(&card) - before;
        assert!(added <= MAX_KEY_PER_TAP, "{added} keys in one tap");
        assert_eq!(t.reg.key_posts.lock().unwrap().len(), posts + usize::from(added > 0), "one slot-keys post per tap that keyed");
        keyed.push(added);
    }
    assert_eq!(keyed, vec![2, 2, 2, 0]);
    assert_eq!(fresh_on(&t, &id, &card), TARGET);
    assert!((24..32).all(|s| card.borrow().slot_state(s) == Some(SlotState::Keyed)), "every change slot keyed first");
    {
        // The registry has the card's keys, and lists the rest as unkeyed.
        let g = t.reg.cards.lock().unwrap();
        for (slot, k) in &g[&id].slots {
            assert_eq!(*k, card.borrow_mut().get_pub(*slot)?.to_string(), "slot {slot}");
        }
        assert_eq!(unkeyed_of(&g[&id]), (13..24).collect::<Vec<u8>>());
    }
    assert!(t.reg.renewals.lock().unwrap().is_empty(), "nothing was spent yet");

    // The drill again: an online charge with change, then offline charges with change, where the
    // card used to run out after one.
    let price = price_with_change(&t, &card, 50);
    let (status, body) = t.charge(&card, price, "s1").await;
    assert_eq!(status, 200, "{body}");
    assert!(!slots_of(&body, "change").is_empty(), "{body}");
    t.set_down(true);
    // Each offline charge takes change into fresh slots only (often several notes), so the
    // reserve lasts a few charges; past it the refusal says why.
    let mut with_change = 0;
    for i in 0..4 {
        let price = price_with_change(&t, &card, 10 + 10 * i);
        let (status, body) = t.charge(&card, price, &format!("s-off-{i}")).await;
        if status != 200 {
            assert_eq!((status, body["error"].as_str()), (409, Some("offline_no_change_room")), "{body}");
            break;
        }
        assert_eq!(body["offline"], true, "{body}");
        assert!(!slots_of(&body, "change").is_empty(), "{body}");
        with_change += 1;
    }
    assert!(with_change >= 2, "the reserve takes more offline change than the drill's card did: {with_change}");

    // Back online: the sender settles them; their inputs are SPENT on the card, settled in the
    // registry and on the federation. The next taps renew those before keying new slots.
    t.set_down(false);
    let sent = t.send().await;
    assert_eq!(sent["settled"].as_array().unwrap().len(), with_change, "{sent}");
    let spent: Vec<u8> = (0..32u8).filter(|s| card.borrow().slot_state(*s) == Some(SlotState::Spent)).collect();
    assert!(!spent.is_empty());
    let unkeyed_before = unkeyed_of(&t.reg.cards.lock().unwrap()[&id]);
    let start = t.reg.renewals.lock().unwrap().len();
    let mut taps = 0;
    while fresh_on(&t, &id, &card) < TARGET && taps < 6 {
        let (before, renewals) = (fresh_on(&t, &id, &card), t.reg.renewals.lock().unwrap().len());
        t.attach_tap(&card).await?;
        let renewed = t.reg.renewals.lock().unwrap().len() - renewals;
        assert!(renewed <= MAX_KEY_PER_TAP, "{renewed} renewals in one tap");
        assert!(fresh_on(&t, &id, &card) >= before, "{before} fresh slots before the tap");
        taps += 1;
        if t.reg.renewals.lock().unwrap().len() - start >= spent.len() {
            break;
        }
    }
    let renewed: Vec<u8> = t.reg.renewals.lock().unwrap()[start..].to_vec();
    assert!(!renewed.is_empty() && renewed.iter().all(|s| spent.contains(s)), "SPENT slots renewed: {renewed:?} of {spent:?}");
    for s in &renewed {
        assert_eq!(card.borrow().slot_state(*s), Some(SlotState::Keyed), "slot {s}");
        let k = t.reg.cards.lock().unwrap()[&id].slots.iter().find(|(x, _)| x == s).unwrap().1.clone();
        assert_eq!(k, card.borrow_mut().get_pub(*s)?.to_string(), "the registry has slot {s}'s new key");
    }
    if renewed.len() < spent.len() {
        assert_eq!(unkeyed_of(&t.reg.cards.lock().unwrap()[&id]), unkeyed_before, "no new slot while a SPENT one could be renewed");
    }
    Ok(())
}

/// A card's first tap keys change slots 24 and 25 even when its reserve is already at the
/// target, and later taps key no more for it; its loads stay below 24.
#[tokio::test(flavor = "multi_thread")]
async fn a_cards_first_tap_keys_change_slots_24_and_25_whatever_its_budget() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    let id = provision_split(&t.reg, &card, 16);
    // The reserve's budget keys nothing on this card.
    const _: () = assert!(16 >= SPARE_TARGET);
    t.attach_tap(&card).await?;
    let keyed: Vec<u8> = (16..32).filter(|s| card.borrow().slot_state(*s) == Some(SlotState::Keyed)).collect();
    assert_eq!(keyed, vec![24, 25]);
    t.attach_tap(&card).await?;
    assert_eq!(t.reg.key_posts.lock().unwrap().len(), 1, "two change slots keyed and free: nothing more to key");
    t.load(&card, &[(1 << 17) - (1 << 10)]).await?;
    let notes: Vec<u8> = (0..32).filter(|s| card.borrow().slot_state(*s) == Some(SlotState::Note)).collect();
    assert_eq!(notes, (0..7).collect::<Vec<u8>>());
    let g = t.reg.cards.lock().unwrap();
    assert_eq!(unkeyed_of(&g[&id]), (16..24).chain(26..32).collect::<Vec<u8>>());
    Ok(())
}

/// A split card that needs keys for a load gets them below 24; the change slots are the reserve's.
#[tokio::test(flavor = "multi_thread")]
async fn a_load_keys_slots_below_24_only() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    t.cw.spares = SpareConfig { target: 0, max_per_tap: 0 };
    let card = v3_card();
    provision_split(&t.reg, &card, 20);
    // 20 notes fill slots 0 to 19; the next load's 4 need keys, and KEY_AHEAD asks for 3 more.
    t.load(&card, &[(1 << 20) - (1 << 10)]).await?;
    t.load(&card, &[(1 << 20) - (1 << 10)]).await?;
    t.load(&card, &[(1 << 14) - (1 << 10)]).await?;
    assert!((0..24).all(|s| card.borrow().slot_state(s) == Some(SlotState::Note)));
    assert!((24..32).all(|s| card.borrow().slot_state(s) == Some(SlotState::Empty)), "no change slot keyed for a load");
    Ok(())
}

/// On a fully keyed card the till tops up and takes online change below 24, and offline change
/// from 24 up.
#[tokio::test(flavor = "multi_thread")]
async fn loads_and_online_change_stay_below_24_and_offline_change_goes_from_24() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    provision(&t.reg, &card);
    t.load(&card, &[1 << 16, 1 << 16]).await?;
    let notes: Vec<u8> = (0..32).filter(|s| card.borrow().slot_state(*s) == Some(SlotState::Note)).collect();
    assert_eq!(notes, vec![0, 1]);
    let (status, body) = t.charge(&card, 20_000, "on").await;
    assert_eq!(status, 200, "{body}");
    let change = slots_of(&body, "change");
    assert!(!change.is_empty() && change.iter().all(|s| *s < 24), "{body}");
    t.set_down(true);
    let (status, body) = t.charge(&card, 20_000, "off").await;
    assert_eq!((status, &body["offline"]), (200, &json!(true)), "{body}");
    let change = slots_of(&body, "change");
    assert!(!change.is_empty() && change.iter().all(|s| (24..32).contains(s)), "{body}");
    Ok(())
}

/// The till's offline path keeps change in slots 24 to 31: a card keyed only below 24 takes no
/// change offline, however many of those slots are KEYED and free.
#[tokio::test(flavor = "multi_thread")]
async fn offline_change_never_goes_into_a_free_keyed_slot_below_24() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    provision_slots(&t.reg, &card, 24);
    t.load(&card, &[1 << 16]).await?;
    t.set_down(true);
    let (status, body) = t.charge(&card, 20_000, "low").await;
    assert_eq!((status, body["error"].as_str()), (409, Some("offline_no_change_room")), "{body}");
    assert!((1..24).all(|s| card.borrow().slot_state(s) == Some(SlotState::Keyed)), "nothing written");
    Ok(())
}

// ------------------------------------------------------------------ the fee margin

/// A federation with no fees, at core consensus 2.1: an offline charge keeps a margin for
/// fedimint's default fees (100 msat per input and output), burned when it is sent. The
/// federation takes the surplus; the till gets what an exact charge would have given it, and the
/// card's change is short by the margin.
#[tokio::test(flavor = "multi_thread")]
async fn an_offline_charge_with_a_fee_margin_is_sent_and_accepted() -> anyhow::Result<()> {
    use fedimint_card_sidecar::plan;
    let mut t = Till::new_fees(OfflineConfig::default(), false).await?;
    assert!(t.cw.wallet.surplus_allowed(), "the test federation runs core consensus {}", t.cw.wallet.consensus_version);
    assert_eq!(t.cw.wallet.fee(1 << 16), 0);
    let card = v3_card();
    provision(&t.reg, &card);
    let loaded = [1u64 << 16, 1 << 14];
    t.load(&card, &loaded).await?;
    let bal0 = t.cw.wallet.balance_msat().await?;

    let n_keyed = (0..card.borrow().slot_len()).filter(|s| card.borrow().slot_state(*s) == Some(SlotState::Keyed)).count();
    t.set_down(true);
    let price = 50_000;
    let (status, body) = t.charge(&card, price, "m").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["offline"], true, "{body}");
    let margin = body["fee_margin_msat"].as_u64().unwrap();
    let q: Value = serde_json::from_slice(&std::fs::read(t.dir.path().join("offline_queue.json"))?)?;
    let c = &q["charges"][0];
    assert_eq!(c["fee_margin_msat"].as_u64(), Some(margin));
    let sum = |v: &Value| v.as_array().unwrap().iter().map(|n| n["amount_msat"].as_u64().unwrap()).sum::<u64>();
    let (ins, n_in) = (sum(&c["inputs"]), c["inputs"].as_array().unwrap().len() as u64);
    let (change, n_change) = (sum(&c["change"]), c["change"].as_array().unwrap().len() as u64);
    let till_msat = c["till_msat"].as_u64().unwrap();
    let n_till = c["host"].as_array().unwrap().len() as u64;
    // The signed transaction pays more than its outputs (no fees here): the surplus is the
    // margin, the default fees of every input and output.
    assert_eq!(c["fees_msat"], 0);
    assert_eq!(ins, till_msat + change + margin, "{c}");
    assert!(margin >= 100 * (n_in + n_till + n_change), "{margin} for {n_in} in, {n_till} + {n_change} out");
    // The same charge planned exactly spends as much: the margin comes out of the change or the
    // till's overpay, and the till still gets at least the price.
    let exact = plan::solve(&loaded, price, n_keyed, &|_| 0, 1)?;
    let exact_till = exact.till.iter().sum::<u64>();
    assert!(!exact.change.is_empty() && exact.surplus == 0);
    assert!(till_msat >= price, "the till gets the price: {till_msat} < {price}");
    assert_eq!(till_msat + change + margin, exact_till + exact.change.iter().sum::<u64>(), "the margin is paid from change or overpay");

    t.set_down(false);
    let sent = t.send().await;
    assert_eq!(sent["settled"].as_array().unwrap().len(), 1, "{sent}");
    let st = t.off.stats();
    assert_eq!((st.queued, st.rejected, st.double_spent), (0, 0, 0), "the federation took the surplus: {sent}");
    let sale = t.sale("m");
    assert!(sale.offline && !sale.collection_pending && !sale.double_spent && !sale.rejected, "{sale:?}");
    assert_eq!(t.cw.wallet.balance_msat().await? - bal0, till_msat, "the till's notes, whole");
    let txid = body["txid"].as_str().unwrap();
    assert!(t.reg.outcomes.lock().unwrap().iter().any(|o| o["txid"] == txid && o["accepted"] == true));
    // The change, short by the margin, lands on the card at its next tap.
    t.tap(&card);
    let r = card_balance_job(&mut t.cw, Some(&t.ops), &t.sales).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["live_msat"].as_u64(), Some(loaded.iter().sum::<u64>() - ins + change), "{}", r.body);
    Ok(())
}

/// Fedimint's default fees on the federation: the margin for the default schedule is 0, and an
/// offline charge balances exactly, as before.
#[tokio::test(flavor = "multi_thread")]
async fn at_the_default_fees_an_offline_charge_has_no_margin() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    provision(&t.reg, &card);
    t.load(&card, &[1 << 16, 1 << 14]).await?;
    t.set_down(true);
    let (status, body) = t.charge(&card, 50_000, "z").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["fee_margin_msat"], 0, "{body}");
    Ok(())
}

// ------------------------------------------------------------------ stranded slots (ENG-867)

impl Till {
    /// A balance tap of `card` on ops, as `POST /v1/card/balance` runs it.
    async fn balance(&mut self, card: &Rc<RefCell<SoftCard>>) -> Value {
        self.tap(card);
        let r = card_balance_job(&mut self.cw, Some(&self.ops), &self.sales).await;
        assert_eq!(r.status, 200, "{}", r.body);
        r.body
    }

    /// The slots of the `n`th `POST /stranded`, ascending, with their kinds and flags.
    fn stranded_post(&self, n: usize) -> Vec<Value> {
        let mut rows = self.reg.stranded_posts.lock().unwrap()[n]["slots"].as_array().unwrap().clone();
        rows.sort_by_key(|x| x["slot"].as_u64());
        rows
    }

    fn stranded_posts(&self) -> usize {
        self.reg.stranded_posts.lock().unwrap().len()
    }
}

/// The slots of a balance answer's notes marked `stranded`, ascending.
fn on_hold(b: &Value) -> Vec<u64> {
    let mut v: Vec<u64> =
        b["notes"].as_array().unwrap().iter().filter(|n| n["stranded"] == true).map(|n| n["slot"].as_u64().unwrap()).collect();
    v.sort();
    v
}

/// What a phone's lost offline sales left on a card ([`wiped_phone`]).
struct Lost {
    card: Rc<RefCell<SoftCard>>,
    id: String,
    /// The phone, for a sale that turns up after all; its store still has the sales.
    phone: Phone,
    /// The sales as the phone took them.
    sales: Vec<OfflineCharge>,
    /// The slots they spent, ascending, and the one owed the third sale's change.
    spent: Vec<u64>,
    change: Vec<u64>,
}

/// Drill (d) of 2026-11-13 on zion (docs/POS-OFFLINE.md § Drill log), at test scale: a card with
/// three notes of 16,384 msat and one of 262,144, and a phone that sells offline from it, 15 sat
/// twice (a whole note each) and 7 sat (a note and one change note), then loses its store before it
/// syncs. The till and the registry never see the sales, and the registry keeps the three notes
/// live.
async fn wiped_phone(t: &mut Till) -> anyhow::Result<Lost> {
    let (card, phone, kit) = phone_and_card(t, &[1 << 14, 1 << 14, 1 << 14, 1 << 18]).await?;
    let id = short_id_of(&card.borrow_mut().get_pub(0)?);
    let mut sales = vec![];
    for (price, cref) in [(15_000, "lost-1"), (15_000, "lost-2"), (7_000, "lost-3")] {
        sales.push(phone.take(&card, &kit, price, cref)?);
    }
    let shape: Vec<(usize, usize)> = sales.iter().map(|s| (s.inputs.len(), s.change.len())).collect();
    assert_eq!(shape, vec![(1, 0), (1, 0), (1, 1)], "two whole sales and one with change");
    let mut spent: Vec<u64> = sales.iter().flat_map(|s| s.inputs.iter().map(|i| i.slot as u64)).collect();
    spent.sort();
    let change: Vec<u64> = sales.iter().flat_map(|s| s.change.iter().map(|c| c.slot as u64)).collect();
    assert_eq!(slot_states(&card, &spent), vec![SlotState::Spent; 3]);
    assert_eq!(slot_states(&card, &change), vec![SlotState::Change]);
    for s in &sales {
        assert_eq!(t.reg.note_status(&id, &s.inputs[0].nonce).as_deref(), Some("live"), "the registry never saw {}", s.txid);
    }
    Ok(Lost { card, id, phone, sales, spent, change })
}

/// Drill (d) itself. The registry keeps a lost sale's notes live; before ENG-867 the card's balance
/// counted them and the till's online planner picked them, so a 16 sat sale failed ("slot 11 has
/// already signed another transaction"). Now an online tap holds the slots nothing explains: the
/// balance counts what the card can pay and puts the rest on hold, the till reports the slots (a
/// spent row per input, a change row for the change), and the 16 sat sale pays from the other note.
#[tokio::test(flavor = "multi_thread")]
async fn a_wiped_phones_sales_are_held_and_reported_and_a_sale_pays_around_them() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let lost = wiped_phone(&mut t).await?;
    // Why the drill's sale failed: over every note the registry calls live (and the change slot,
    // which it calls free), the planner's best plan for 16 sat spends a lost note.
    {
        use fedimint_card_sidecar::plan::solve;
        let w = &t.cw.wallet;
        let fee = |a: u64| w.fee(a);
        let p = solve(&[1 << 14, 1 << 14, 1 << 14, 1 << 18], 16_000, 28, &fee, w.min_denom())?;
        assert!(p.inputs.iter().any(|i| *i < 3), "{p:?}");
    }

    let b = t.balance(&lost.card).await;
    assert_eq!(b["live_msat"], json!(1 << 18), "only what the card can pay: {b}");
    assert_eq!(b["stranded_msat"], json!(3 * (1 << 14)), "{b}");
    assert_eq!(b["stranded_until_ms"], Value::Null, "the registry has not dated them yet: {b}");
    assert_eq!(on_hold(&b), lost.spent, "{b}");
    assert!(b["notes"].as_array().unwrap().iter().filter(|n| n["stranded"] == true).all(|n| n["status"] == "live"), "{b}");
    assert!(!b["free_slots"].as_array().unwrap().contains(&json!(lost.change[0])), "the change slot is no room: {b}");

    // Reported once: a spent row per input and a change row for the change, under its sale's txid,
    // with no flag (nothing is due, so the federation is not asked).
    assert_eq!(t.stranded_posts(), 1);
    let rows: Vec<(u64, String, String)> = t
        .stranded_post(0)
        .iter()
        .map(|x| (x["slot"].as_u64().unwrap(), x["kind"].as_str().unwrap().to_owned(), x["txid"].as_str().unwrap().to_owned()))
        .collect();
    let mut want: Vec<(u64, String, String)> = lost
        .sales
        .iter()
        .flat_map(|s| {
            let ins = s.inputs.iter().map(|i| (i.slot as u64, "spent".to_owned(), s.txid.clone()));
            ins.chain(s.change.iter().map(|c| (c.slot as u64, "change".to_owned(), s.txid.clone())))
        })
        .collect();
    want.sort();
    assert_eq!(rows, want);
    assert!(t.stranded_post(0).iter().all(|x| x.get("unspent").is_none() && x.get("spent").is_none()));
    assert_eq!(t.reg.strandings(&lost.id).len(), 4, "the registry has them");

    // The next tap: the registry lists them, with their date; nothing new to report.
    let b = t.balance(&lost.card).await;
    let since = t.reg.strandings(&lost.id)[0]["first_seen"].as_u64().unwrap();
    assert_eq!(b["stranded_until_ms"], json!(since + STRANDED_GRACE_MS), "{b}");
    assert_eq!((on_hold(&b), t.stranded_posts()), (lost.spent.clone(), 1), "listed, not due: not posted again");

    // The 16 sat sale that failed in the drill pays from the 262,144 msat note.
    let (status, body) = t.charge(&lost.card, 16_000, "after-the-wipe").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("offline").is_none(), "{body}");
    assert!(slots_of(&body, "inputs").iter().all(|s| !lost.spent.contains(s)), "{body}");
    assert!(slots_of(&body, "change").iter().all(|s| !lost.change.contains(s)), "{body}");
    Ok(())
}

/// Past the grace the holder gets the notes back (decision 2). The tap that finds the rows due asks
/// the federation, which says the notes are unspent, and reports them `unspent: true`; the
/// registry releases every slot of the three sales, the change slot with its sale's input; the
/// next tap sends the releases (`RELEASE`): the spent slots hold their notes again and the change
/// slot is keyed. The balance is whole, and a released note pays.
#[tokio::test(flavor = "multi_thread")]
async fn past_the_grace_the_held_notes_are_released_and_pay_again() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let lost = wiped_phone(&mut t).await?;
    t.balance(&lost.card).await;
    t.reg.age_strandings(&lost.id, STRANDED_GRACE_MS);

    let b = t.balance(&lost.card).await;
    assert_eq!(b["stranded_msat"], json!(3 * (1 << 14)), "still held at this tap: {b}");
    assert_eq!(b["stranded_until_ms"], Value::Null, "due: no date: {b}");
    assert_eq!(t.stranded_posts(), 2);
    for x in t.stranded_post(1) {
        match x["kind"].as_str() {
            Some("spent") => assert_eq!((x.get("unspent"), x.get("spent")), (Some(&json!(true)), None), "{x}"),
            _ => assert!(x.get("unspent").is_none() && x.get("spent").is_none(), "{x}"),
        }
    }
    for s in &lost.sales {
        let mut want: Vec<u64> = s.inputs.iter().map(|i| i.slot as u64).chain(s.change.iter().map(|c| c.slot as u64)).collect();
        want.sort();
        assert_eq!(t.reg.released(&lost.id, &s.txid), want, "every slot of {}", s.txid);
    }

    // The next tap sends the releases before anything else.
    let b = t.balance(&lost.card).await;
    assert_eq!(slot_states(&lost.card, &lost.spent), vec![SlotState::Note; 3]);
    assert_eq!(slot_states(&lost.card, &lost.change), vec![SlotState::Keyed]);
    assert_eq!(b["live_msat"], json!(3 * (1 << 14) + (1 << 18)), "whole again: {b}");
    assert_eq!((b["stranded_msat"].clone(), on_hold(&b)), (json!(0), vec![]), "{b}");
    assert_eq!(t.stranded_posts(), 2, "nothing left to report");

    // A released note pays: 15 sat is one whole 16,384 msat note.
    let (status, body) = t.charge(&lost.card, 15_000, "released").await;
    assert_eq!(status, 200, "{body}");
    assert!(!slots_of(&body, "inputs").is_empty() && slots_of(&body, "inputs").iter().all(|s| lost.spent.contains(s)), "{body}");
    Ok(())
}

/// A sale in the till's own queue explains its slots: a phone's sale it took in
/// (`POST /v1/offline/charges`), or its own offline sale, that the sender (every 30 s) has not
/// reported yet. Such slots are neither held nor reported, and the sender settles them as before.
#[tokio::test(flavor = "multi_thread")]
async fn a_sale_in_the_tills_own_queue_is_neither_held_nor_reported() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (card, phone, kit) = phone_and_card(&mut t, &[1 << 14, 1 << 14, 1 << 18]).await?;
    let synced = phone.take(&card, &kit, 7_000, "synced")?;
    assert_eq!((synced.inputs.len(), synced.change.len()), (1, 1));
    assert_eq!(statuses(&t.ingest(&phone.id(), phone.drained())), vec![("queued", None)]);
    t.set_down(true);
    let (status, body) = t.charge(&card, 15_000, "own").await;
    assert_eq!((status, &body["offline"]), (200, &json!(true)), "{body}");
    t.set_down(false);
    assert_eq!(t.off.stats().queued, 2, "neither sent yet");

    let b = t.balance(&card).await;
    assert_eq!((b["stranded_msat"].clone(), on_hold(&b)), (json!(0), vec![]), "{b}");
    assert_eq!(t.stranded_posts(), 0, "nothing reported");
    let change: Vec<u64> = synced.change.iter().map(|c| c.slot as u64).collect();
    assert_eq!(slot_states(&card, &change), vec![SlotState::Change]);

    let sent = t.send().await;
    assert_eq!(sent["settled"].as_array().unwrap().len(), 2, "{sent}");
    let b = t.balance(&card).await;
    assert_eq!(slot_states(&card, &change), vec![SlotState::Note], "the change landed");
    assert_eq!((b["stranded_msat"].clone(), t.stranded_posts()), (json!(0), 0), "{b}");
    Ok(())
}

/// A charge the till left `signing`, then abandoned at the card's next offline tap because an input
/// slot of it paid another transaction since (here a phone's sale), is never sent: the note the
/// card signed for it and the slots it opened for its change are stranded, as a lost phone's are
/// (`ChargeState::Abandoned`). Its entry in the till's queue explains nothing, so the next online
/// tap holds those slots and reports them, and a sale pays around them. Until the review of ENG-867
/// the entry explained them: the balance counted the signed note, its CHANGE slots were listed as
/// room for change the card refuses, and a sale that picked the note failed.
#[tokio::test(flavor = "multi_thread")]
async fn a_charge_the_till_abandoned_is_held_and_reported_like_a_lost_phones() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    provision(&t.reg, &card);
    // Four notes of 65,536 msat: a 90,000 msat charge takes two, with change.
    t.load(&card, &[1 << 16, 1 << 16, 1 << 16, 1 << 16]).await?;
    t.set_down(true);
    let (status, body) = t.charge_pulled(&card, 1, 90_000, "pulled").await;
    assert_eq!((status, body["error"].as_str()), (202, Some("undecided")), "{body}");
    let left = t.off.queue.lock().unwrap().file.charges[0].clone();
    assert_eq!((left.state, left.inputs.len()), (ChargeState::Signing, 2));
    let (signed, unsigned) = (left.inputs[0].slot as u64, left.inputs[1].slot as u64);
    let change: Vec<u64> = left.change.iter().map(|c| c.slot as u64).collect();
    assert_eq!(slot_states(&card, &[signed, unsigned]), vec![SlotState::Spent, SlotState::Note]);
    assert!(!change.is_empty() && slot_states(&card, &change).iter().all(|s| *s == SlotState::Change), "{change:?}");
    // Elsewhere the input the card had not signed for it pays another sale.
    fedimint_card_host_card::softkit::open_test_session_under(&mut *card.borrow_mut(), &t.reg.cert_pub())?;
    card.borrow_mut().sign(unsigned as u8, &[9; 32])?;
    // The card's next offline tap here abandons the charge and takes its own sale.
    let (status, body) = t.charge(&card, 20_000, "next").await;
    assert_eq!((status, &body["offline"]), (200, &json!(true)), "{body}");
    let state = t.off.queue.lock().unwrap().file.charges.iter().find(|c| c.txid == left.txid).map(|c| c.state);
    assert_eq!(state, Some(ChargeState::Abandoned));
    t.set_down(false);
    let sent = t.send().await;
    assert_eq!(sent["settled"].as_array().unwrap().len(), 1, "the new sale only: {sent}");

    let b = t.balance(&card).await;
    let mut lost = vec![signed, unsigned];
    lost.sort();
    assert_eq!(on_hold(&b), lost, "the note signed for the abandoned charge, and the phone's: {b}");
    assert_eq!(b["stranded_msat"], json!(2 * (1 << 16)), "{b}");
    let free = b["free_slots"].as_array().unwrap();
    assert!(change.iter().all(|s| !free.contains(&json!(s))), "its CHANGE slots are no room: {b}");
    assert_eq!(t.stranded_posts(), 1);
    let rows = t.stranded_post(0);
    assert!(rows.contains(&json!({"slot": signed, "kind": "spent", "txid": left.txid})), "{rows:?}");
    for s in &change {
        assert!(rows.contains(&json!({"slot": s, "kind": "change", "txid": left.txid})), "{rows:?}");
    }

    // An online sale pays around them.
    let (status, body) = t.charge(&card, 20_000, "after").await;
    assert_eq!(status, 200, "{body}");
    assert!(slots_of(&body, "inputs").iter().all(|s| !lost.contains(s)), "{body}");
    assert!(slots_of(&body, "change").iter().all(|s| !change.contains(s)), "{body}");
    Ok(())
}

/// A registry before v13 lists no stranded slots and answers `POST /stranded` 404. The till holds
/// the slots all the same (the hold comes from the card), logs the refusal, and the sale pays
/// around them; unlisted, they are reported again at every tap.
#[tokio::test(flavor = "multi_thread")]
async fn with_a_registry_before_strandings_the_slots_are_held_all_the_same() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    t.reg.before_strandings.store(true, Ordering::SeqCst);
    let lost = wiped_phone(&mut t).await?;
    let b = t.balance(&lost.card).await;
    assert_eq!((b["live_msat"].clone(), b["stranded_msat"].clone()), (json!(1 << 18), json!(3 * (1 << 14))), "{b}");
    assert_eq!(on_hold(&b), lost.spent, "{b}");
    assert_eq!(t.stranded_posts(), 1, "reported, and refused 404");
    assert!(t.reg.strandings(&lost.id).is_empty());
    let (status, body) = t.charge(&lost.card, 16_000, "old-registry").await;
    assert_eq!(status, 200, "{body}");
    assert!(slots_of(&body, "inputs").iter().all(|s| !lost.spent.contains(s)), "{body}");
    assert_eq!(t.stranded_posts(), 2);
    Ok(())
}

/// The money gate. Once the registry released a lost sale's slots the holder has the notes back,
/// so the sale itself, should it reach a till after all (a phone that synced late), is refused as a
/// double spend: the merchant's loss, within the till's caps.
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_sale_reported_after_its_release_is_a_double_spend() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let lost = wiped_phone(&mut t).await?;
    t.balance(&lost.card).await;
    t.reg.age_strandings(&lost.id, STRANDED_GRACE_MS);
    t.balance(&lost.card).await;
    t.balance(&lost.card).await;
    assert_eq!(slot_states(&lost.card, &lost.spent), vec![SlotState::Note; 3], "released");

    // The phone was not wiped after all: it syncs.
    assert_eq!(statuses(&t.ingest(&lost.phone.id(), lost.phone.drained())), vec![("queued", None); 3]);
    let sent = t.send().await;
    let mut double: Vec<String> = sent["double_spent"].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_owned()).collect();
    double.sort();
    let mut want: Vec<String> = lost.sales.iter().map(|s| s.txid.clone()).collect();
    want.sort();
    assert_eq!(double, want, "{sent}");
    for cref in ["lost-1", "lost-2", "lost-3"] {
        assert!(t.sale(cref).double_spent, "{cref}");
    }
    // The holder keeps the notes.
    let b = t.balance(&lost.card).await;
    assert_eq!(b["live_msat"], json!(3 * (1 << 14) + (1 << 18)), "{b}");
    Ok(())
}

/// Send a phone's signed charge to the federation past every till and the registry, and wait until
/// its first input is spent there.
async fn land(t: &Till, c: &OfflineCharge) -> anyhow::Result<()> {
    use fedimint_core::encoding::Decodable as _;
    let client = &t.cw.wallet.client;
    let tx = fedimint_core::transaction::Transaction::consensus_decode_hex(c.signed_hex.as_deref().unwrap(), client.decoders())?;
    let _ = client.api().submit_transaction(tx).await;
    let nonce = fedimint_mint_common::Nonce(c.inputs[0].nonce.parse()?);
    for _ in 0..150 {
        if t.cw.is_spent(nonce).await? {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!("{} never landed", c.txid)
}

/// Decision 5: the federation says a held note is spent (its sale reached the federation some way
/// the registry never saw). The tap that finds its row due reports it `spent: true`, never
/// `unspent`: the registry makes the note spent under the sale and drops the row with no release,
/// the till's copy has it spent too, and the next charge with the PIN renews the slot. The sale
/// landed, so its change is real: its CHANGE slot is never released, and stays held.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_note_the_federation_has_spent_is_reported_spent_and_never_released() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    // The large note first, so the small one is not in slot 0, which names the card and is never
    // renewed on ops.
    let (card, phone, kit) = phone_and_card(&mut t, &[1 << 18, 1 << 14]).await?;
    let id = short_id_of(&card.borrow_mut().get_pub(0)?);
    let landed = phone.take(&card, &kit, 7_000, "landed")?;
    assert_eq!((landed.inputs.len(), landed.change.len()), (1, 1));
    land(&t, &landed).await?;
    let (slot, nonce, change) = (landed.inputs[0].slot, landed.inputs[0].nonce.clone(), landed.change[0].slot);
    assert_ne!(slot, 0);
    let spent_row = json!({"slot": slot, "kind": "spent", "txid": landed.txid});
    let change_row = json!({"slot": change, "kind": "change", "txid": landed.txid});

    t.balance(&card).await;
    assert_eq!(t.stranded_post(0), vec![spent_row.clone(), change_row.clone()], "held and reported");
    t.reg.age_strandings(&id, STRANDED_GRACE_MS);
    let b = t.balance(&card).await;
    let mut spent_flagged = spent_row.clone();
    spent_flagged["spent"] = json!(true);
    assert_eq!(t.stranded_post(1), vec![spent_flagged, change_row], "no flag on a change slot");
    assert_eq!(t.reg.note_status(&id, &nonce).as_deref(), Some("spent"));
    assert!(t.reg.released(&id, &landed.txid).is_empty(), "nothing released");
    let rows: Vec<(u64, String)> =
        t.reg.strandings(&id).iter().map(|s| (s["slot"].as_u64().unwrap(), s["kind"].as_str().unwrap().to_owned())).collect();
    assert_eq!(rows, vec![(change as u64, "change".to_owned())], "the spent row is gone; the change row stays");
    assert_eq!(b["stranded_msat"], json!(0), "{b}");
    let local = StateStore::open(t.ops.card_path(&id))?;
    let n = local.state.notes.iter().find(|n| n.nonce == nonce).unwrap();
    assert_eq!(n.status, fedimint_card_sidecar::state::NoteStatus::Spent);

    // The next charge with the PIN renews the spent slot. The change slot stays held as the
    // registry lists it, never due (nothing releases it), so it is not reported again.
    let (status, body) = t.charge(&card, 16_000, "renews").await;
    assert_eq!(status, 200, "{body}");
    assert!(!slots_of(&body, "change").contains(&(change as u64)), "{body}");
    assert!(t.reg.renewals.lock().unwrap().contains(&slot), "renewed: {:?}", t.reg.renewals.lock().unwrap());
    assert_eq!(card.borrow().slot_state(slot), Some(SlotState::Keyed));
    assert_eq!(t.stranded_posts(), 2, "a landed sale's change is never due: not posted again");
    assert!(t.reg.released(&id, &landed.txid).is_empty(), "never released");
    assert_eq!(card.borrow().slot_state(change), Some(SlotState::Change));
    Ok(())
}

/// A restore of the registry brings its notes back but not its strandings (docs/OFFLINE-PROTOCOL.md
/// § 7, "As built"), so decision 5's landed sale loses its change row. The sale's CHANGE slot still
/// waits on its txid, which the input note the federation spent now carries; a spent note's txid
/// explains nothing (such a sale can have landed outside every till and the registry), so the next
/// tap holds the slot again, out of the free slots, and reports it. Until the review of ENG-867 the
/// spent note explained it: the slot was listed as room, and a sale's change went where the card
/// refuses it.
#[tokio::test(flavor = "multi_thread")]
async fn after_a_registry_restore_a_landed_sales_change_slot_is_held_again() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (card, phone, kit) = phone_and_card(&mut t, &[1 << 18, 1 << 14]).await?;
    let id = short_id_of(&card.borrow_mut().get_pub(0)?);
    let landed = phone.take(&card, &kit, 7_000, "landed")?;
    assert_eq!((landed.inputs.len(), landed.change.len()), (1, 1));
    land(&t, &landed).await?;
    let change = landed.change[0].slot;
    // Decision 5 up to its due report: the input is spent under the sale, the change row stays.
    t.balance(&card).await;
    t.reg.age_strandings(&id, STRANDED_GRACE_MS);
    t.balance(&card).await;
    assert_eq!(t.reg.note_status(&id, &landed.inputs[0].nonce).as_deref(), Some("spent"));
    assert_eq!(t.reg.strandings(&id).len(), 1);
    assert_eq!(t.stranded_posts(), 2);

    t.reg.cards.lock().unwrap().get_mut(&id).unwrap().strandings.clear();
    let b = t.balance(&card).await;
    assert!(!b["free_slots"].as_array().unwrap().contains(&json!(change)), "{b}");
    assert_eq!(t.stranded_posts(), 3, "held, and reported again");
    assert_eq!(t.stranded_post(2), vec![json!({"slot": change, "kind": "change", "txid": landed.txid})]);

    // Recorded again and never due: a sale pays around the slot, which is not posted again.
    let (status, body) = t.charge(&card, 16_000, "after-restore").await;
    assert_eq!(status, 200, "{body}");
    assert!(!slots_of(&body, "change").contains(&(change as u64)), "{body}");
    assert_eq!(card.borrow().slot_state(change), Some(SlotState::Change));
    assert_eq!(t.stranded_posts(), 3);
    Ok(())
}

/// Decision 4 (docs/POS-OFFLINE.md risk 5): a card taken away between `SET_CHANGE` and `SIGN` owes
/// change for a transaction it never signed, and has no SPENT slot for it. The till holds the
/// CHANGE slot (it is no room for change) and reports it; past the grace it reports it again with
/// no flag, since the federation has nothing to say of a transaction nobody signed, and the
/// registry releases it. The next tap makes it a keyed slot again.
#[tokio::test(flavor = "multi_thread")]
async fn a_change_slot_with_no_spent_partner_is_held_then_released_without_a_flag() -> anyhow::Result<()> {
    use fedimint_card_host_card::v3::Opening;
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    let id = provision(&t.reg, &card);
    t.load(&card, &[1 << 18]).await?;
    let slot = (0..32u8).find(|s| card.borrow().slot_state(*s) == Some(SlotState::Keyed)).unwrap();
    let txid = [7u8; 32];
    card.borrow_mut().set_change(slot, 1 << 13, &Opening { txid, out_idx: 1, blinding_key: [9; 32] })?;
    let row = json!({"slot": slot, "kind": "change", "txid": hex::encode(txid)});

    let b = t.balance(&card).await;
    assert_eq!((b["stranded_msat"].clone(), b["live_msat"].clone()), (json!(0), json!(1 << 18)), "a change slot holds no money: {b}");
    assert!(!b["free_slots"].as_array().unwrap().contains(&json!(slot)), "{b}");
    assert_eq!(t.stranded_post(0), vec![row.clone()]);
    t.reg.age_strandings(&id, STRANDED_GRACE_MS);
    t.balance(&card).await;
    assert_eq!(t.stranded_post(1), vec![row], "no flag");
    assert_eq!(t.reg.released(&id, &hex::encode(txid)), vec![slot as u64]);
    let b = t.balance(&card).await;
    assert_eq!(card.borrow().slot_state(slot), Some(SlotState::Keyed));
    assert!(b["free_slots"].as_array().unwrap().contains(&json!(slot)), "room again: {b}");
    Ok(())
}

/// More unexplained slots than one tap asks the card about (8): the rest are held on doubt, out of
/// the plan and the balance with their transaction not read yet, and are reported at the next tap.
/// While a held note is not listed, the balance gives no date.
#[tokio::test(flavor = "multi_thread")]
async fn past_the_taps_checks_slots_are_held_on_doubt_and_reported_at_the_next_tap() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    provision(&t.reg, &card);
    // Ten notes, 1,024 to 524,288 msat, in slots 0 to 9; nine signed away for transactions that no
    // till or registry ever sees.
    t.load(&card, &[(1 << 20) - (1 << 10)]).await?;
    let amounts: Vec<u64> = (0..10u8).map(|s| card.borrow_mut().get_slot(s).unwrap().amount_msat).collect();
    assert!(amounts.iter().all(|a| *a > 0));
    fedimint_card_host_card::softkit::open_test_session_under(&mut *card.borrow_mut(), &t.reg.cert_pub())?;
    for s in 1..=9u8 {
        card.borrow_mut().sign(s, &[s; 32])?;
    }
    let posted = |t: &Till, n: usize| -> Vec<u64> { t.stranded_post(n).iter().map(|x| x["slot"].as_u64().unwrap()).collect() };

    let b = t.balance(&card).await;
    assert_eq!((b["live_msat"].clone(), b["stranded_msat"].clone()), (json!(amounts[0]), json!(amounts[1..].iter().sum::<u64>())), "{b}");
    assert_eq!(on_hold(&b), (1..=9).collect::<Vec<u64>>());
    assert_eq!(posted(&t, 0), (1..=8).collect::<Vec<u64>>(), "eight asked; slot 9 held on doubt");
    let b = t.balance(&card).await;
    assert_eq!(posted(&t, 1), (1..=9).collect::<Vec<u64>>());
    assert_eq!(b["stranded_until_ms"], Value::Null, "slot 9 was not listed yet: {b}");
    let b = t.balance(&card).await;
    assert_eq!(t.stranded_posts(), 2, "all listed, none due");
    assert!(b["stranded_until_ms"].as_u64().is_some(), "{b}");
    Ok(())
}

// ------------------------------------------------------------------ a repeated client_ref on ops (ENG-858)

use std::cell::Cell;

use fedimint_card_host_card::TEST_PIN;
use fedimint_card_sidecar::glue::CrashPoint;
use fedimint_card_sidecar::state::PendingTx;

/// The card in the field, with every command sent to it counted. A card that is `gone` has left
/// the field: each command fails as a phone's card link reports a lost card.
struct Watched {
    card: Shared,
    gone: bool,
    calls: Rc<Cell<usize>>,
}

impl Watched {
    /// The card, and the count of the commands it is sent.
    fn new(card: &Rc<RefCell<SoftCard>>, gone: bool) -> (Self, Rc<Cell<usize>>) {
        let calls = Rc::new(Cell::new(0));
        (Self { card: Shared(card.clone()), gone, calls: calls.clone() }, calls)
    }

    fn count(&self) -> anyhow::Result<()> {
        self.calls.set(self.calls.get() + 1);
        if self.gone {
            anyhow::bail!(BridgeError::NoCard("POST /apdu: HTTP 503: the card left the field".into()));
        }
        Ok(())
    }
}

impl Card for Watched {
    fn gen_key(&mut self, slot: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.count()?;
        self.card.gen_key(slot)
    }
    fn gen_key_overwrite(&mut self, slot: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.count()?;
        self.card.gen_key_overwrite(slot)
    }
    fn get_pub(&mut self, slot: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.count()?;
        self.card.get_pub(slot)
    }
    fn sign(&mut self, slot: u8, msg32: &[u8; 32]) -> anyhow::Result<Signature> {
        self.count()?;
        self.card.sign(slot, msg32)
    }
    fn apdu(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.count()?;
        self.card.apdu(apdu)
    }
}

impl Till {
    /// [`Till::charge`] with the card in the field watched.
    async fn charge_watched(&mut self, w: Watched, price: u64, cref: &str) -> (u16, Value) {
        self.cw.card = Box::new(w);
        self.cw.pin = Some(TEST_PIN.to_vec());
        let r = charge_job_with(&mut self.cw, Some(&self.ops), Some(&self.off), price, Some(cref.into()), &self.sales).await;
        self.cw.pin = None;
        (r.status, r.body)
    }

    /// The unresolved charges in card `id`'s file.
    fn pending(&self, id: &str) -> Vec<PendingTx> {
        StateStore::open(self.ops.card_path(id)).unwrap().state.pending_txs
    }

    /// The charges the registry holds open for card `id`.
    fn open_charges(&self, id: &str) -> usize {
        self.reg.cards.lock().unwrap()[id].txs.iter().filter(|x| x["status"] == "open").count()
    }

    fn sales_count(&self) -> usize {
        self.sales.lock().unwrap().file.sales.len()
    }
}

/// `POST /v1/charge` with `body` through the till's router, its actor polled beside the request
/// (the actor is not `Send`).
async fn post_charge<A: std::future::Future<Output = ()>>(
    app: &axum::Router,
    actor: &mut std::pin::Pin<Box<A>>,
    body: &Value,
) -> (u16, Value) {
    let req = axum::http::Request::post("/v1/charge")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let call = async {
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    };
    tokio::select! {
        r = call => r,
        _ = actor.as_mut() => panic!("the till's actor ended early"),
    }
}

/// A repeat of a sale already in the ledger is that sale, answered before the card is asked
/// anything: the same 200 with the card gone, and `conflict` at another price. Before ENG-858 a
/// till on ops read the card's slot 0 key first, and the repeat of a finished sale whose card had
/// left the field was `no_card`.
#[tokio::test(flavor = "multi_thread")]
async fn a_repeat_of_a_recorded_sale_is_answered_before_the_card_is_asked_anything() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    provision(&t.reg, &card);
    t.load(&card, &[1 << 16, 1 << 14]).await?;
    let (status, first) = t.charge(&card, 20_000, "r1").await;
    assert_eq!(status, 200, "{first}");

    let (gone, calls) = Watched::new(&card, true);
    let (status, body) = t.charge_watched(gone, 20_000, "r1").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!((&body["sale_id"], &body["txid"]), (&first["sale_id"], &first["txid"]), "the same sale: {body}");
    assert_eq!(calls.get(), 0, "the card was asked nothing");
    assert_eq!(t.sales_count(), 1);

    let (gone, calls) = Watched::new(&card, true);
    let (status, body) = t.charge_watched(gone, 30_000, "r1").await;
    assert_eq!((status, body["error"].as_str()), (409, Some("conflict")), "{body}");
    assert_eq!(calls.get(), 0, "the card was asked nothing");
    assert_eq!(t.sales_count(), 1);
    Ok(())
}

/// A charge the till died on after the registry took its signed bytes (`submitted`, never sent to
/// the federation): its repeat, with the card gone, is settled from the card's file and answered as
/// that charge, with no command to the card and nothing signed again. Before ENG-858 the repeat
/// read the card first and was `no_card`.
#[tokio::test(flavor = "multi_thread")]
async fn a_repeat_of_a_charge_the_till_died_on_is_settled_with_the_card_gone_and_charged_once() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    let id = provision(&t.reg, &card);
    t.load(&card, &[1 << 16, 1 << 14]).await?;
    t.cw.crash_point = Some(CrashPoint::BeforeSubmit);
    let (status, body) = t.charge(&card, 20_000, "r2").await;
    t.cw.crash_point = None;
    assert_eq!(status, 500, "{body}");
    let pending = t.pending(&id);
    assert!(pending.len() == 1 && pending[0].submitted && pending[0].client_ref.as_deref() == Some("r2"), "{pending:?}");
    assert_eq!(t.sales_count(), 0);
    let spent = spent_slots(&card);
    assert!(spent > 0, "the card signed the charge");

    let (gone, calls) = Watched::new(&card, true);
    let (status, body) = t.charge_watched(gone, 20_000, "r2").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["txid"], json!(pending[0].txid), "the charge the till died on: {body}");
    assert_eq!(calls.get(), 0, "the card was asked nothing");
    assert_eq!(t.sales_count(), 1);
    assert_eq!(t.open_charges(&id), 0, "the outcome reached the registry");
    assert_eq!(spent_slots(&card), spent, "nothing signed again");
    Ok(())
}

/// A charge whose signed bytes the registry never answered for (`ops_unconfirmed`, not sent):
/// while the registry is down, its repeat is `undecided`, with the card gone or in the field, and
/// nothing is taken offline; once the registry answers, the repeat sends that charge and is answered
/// as it. Before ENG-858 the probe sent the repeat the offline way first: a card gone was
/// `no_card`, and a card in the field paid the sale a second time.
#[tokio::test(flavor = "multi_thread")]
async fn a_repeat_with_the_registry_down_is_undecided_and_never_taken_offline() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    let id = provision(&t.reg, &card);
    t.load(&card, &[1 << 16, 1 << 16]).await?;
    t.cw.crash_point = Some(CrashPoint::SubmittedUnreachable);
    let (status, body) = t.charge(&card, 20_000, "r3").await;
    t.cw.crash_point = None;
    assert_eq!((status, body["error"].as_str()), (202, Some("undecided")), "{body}");
    let pending = t.pending(&id);
    assert!(pending.len() == 1 && pending[0].ops_unconfirmed && !pending[0].submitted, "{pending:?}");
    let spent = spent_slots(&card);

    t.set_down(true);
    for gone in [true, false] {
        let (w, calls) = Watched::new(&card, gone);
        let (status, body) = t.charge_watched(w, 20_000, "r3").await;
        assert_eq!((status, body["error"].as_str()), (202, Some("undecided")), "card gone: {gone}: {body}");
        assert_eq!(calls.get(), 0, "card gone: {gone}: the card was asked nothing");
    }
    assert!(t.off.queue.lock().unwrap().file.charges.is_empty(), "nothing taken offline");
    assert_eq!(spent_slots(&card), spent, "nothing signed again");
    assert_eq!(t.sales_count(), 0);

    t.set_down(false);
    let (w, calls) = Watched::new(&card, false);
    let (status, body) = t.charge_watched(w, 20_000, "r3").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["txid"], json!(pending[0].txid), "the charge the registry had no answer for: {body}");
    assert_eq!(calls.get(), 0, "the card was asked nothing");
    assert_eq!(t.sales_count(), 1);
    assert_eq!(spent_slots(&card), spent, "nothing signed again");
    Ok(())
}

/// Another card tapped for a sale whose first card's charge the till died on (`submitted`, never
/// sent) is not charged: the repeat is settled from the first card's file and answered as that
/// charge. Before ENG-858 the second card was attached, its own file had no charge for the ref,
/// and it paid the sale a second time.
#[tokio::test(flavor = "multi_thread")]
async fn another_card_for_a_sale_with_an_unresolved_charge_is_not_charged() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (a, b) = (v3_card(), v3_card());
    let id_a = provision(&t.reg, &a);
    provision(&t.reg, &b);
    t.load(&a, &[1 << 16, 1 << 14]).await?;
    t.load(&b, &[1 << 16, 1 << 14]).await?;
    t.cw.crash_point = Some(CrashPoint::BeforeSubmit);
    let (status, body) = t.charge(&a, 20_000, "r4").await;
    t.cw.crash_point = None;
    assert_eq!(status, 500, "{body}");
    let txid = t.pending(&id_a)[0].txid.clone();

    let (w, calls) = Watched::new(&b, false);
    let (status, body) = t.charge_watched(w, 20_000, "r4").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["txid"], json!(txid), "card A's charge, not a second one: {body}");
    assert_eq!(calls.get(), 0, "card B was asked nothing");
    assert_eq!(spent_slots(&b), 0, "nothing signed on card B");
    assert_eq!(t.sales_count(), 1);
    assert!(t.pending(&id_a).is_empty());
    Ok(())
}

/// Over HTTP, with no card in the field (the opener fails as the relay's 503 does) and the
/// registry down, a repeat of a charge whose signed bytes the registry never answered for is
/// `undecided`, never `no_card`, which the POS apps read as a definitive refusal. With the registry
/// back, the same request settles the charge and answers its sale. Before ENG-858 the till could
/// not load the card's file and answered the failed open, `no_card`.
#[tokio::test(flavor = "multi_thread")]
async fn a_repeat_over_http_with_no_card_and_the_registry_down_is_undecided() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    let id = provision(&t.reg, &card);
    t.load(&card, &[1 << 16, 1 << 14]).await?;
    t.cw.crash_point = Some(CrashPoint::SubmittedUnreachable);
    let (status, body) = t.charge(&card, 20_000, "r5").await;
    t.cw.crash_point = None;
    assert_eq!((status, body["error"].as_str()), (202, Some("undecided")), "{body}");
    let txid = t.pending(&id)[0].txid.clone();

    let (reg, off) = (t.reg.clone(), t.off.clone());
    let conn = Conn::new(OpsState::Connected { url: t.ops.client.url().to_owned() }, Some(t.ops.clone()));
    let sales = SalesStore::open(t.dir.path().join("sales.json"))?;
    let no_card: till::CardOpener = Box::new(|| anyhow::bail!(BridgeError::NoCard("GET DATA FF21: HTTP 503: no card in the field".into())));
    let (handle, actor) = till::new_with_conn(t.cw, "http://127.0.0.1:1".into(), sales, no_card, conn).await;
    let app = till::router(handle.with_offline(off.clone()));
    let mut actor = Box::pin(actor);
    let body = json!({"price_msat": 20_000, "client_ref": "r5", "pin": std::str::from_utf8(TEST_PIN)?});

    reg.down.store(true, Ordering::SeqCst);
    off.reach.reset();
    let (status, v) = post_charge(&app, &mut actor, &body).await;
    assert_eq!((status, v["error"].as_str()), (202, Some("undecided")), "{v}");

    reg.down.store(false, Ordering::SeqCst);
    off.reach.reset();
    let (status, v) = post_charge(&app, &mut actor, &body).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["txid"], json!(txid), "the charge the registry had no answer for: {v}");
    let sales = SalesStore::open(t.dir.path().join("sales.json"))?.file.sales;
    let recorded: Vec<(Option<&str>, &str)> = sales.iter().map(|s| (s.client_ref.as_deref(), s.txid.as_str())).collect();
    assert_eq!(recorded, vec![(Some("r5"), txid.as_str())], "one sale in sales.json");
    Ok(())
}

/// A first attempt that left no record (the card left before it signed: `no_card`, and the record
/// of the charge was removed) gives a repeat nothing to answer from: the repeat is a fresh attempt
/// with the same ref, and pays. It passed before ENG-858 and passes after: a first attempt that
/// left no record is not held back. A record that the repeat itself proves left nothing behind is
/// the case of the two tests after it.
#[tokio::test(flavor = "multi_thread")]
async fn a_repeat_after_a_charge_that_left_nothing_is_a_fresh_attempt() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    let id = provision(&t.reg, &card);
    t.load(&card, &[1 << 16, 1 << 14]).await?;
    let (status, body) = t.charge_pulled(&card, 0, 20_000, "f1").await;
    assert_eq!((status, body["error"].as_str()), (409, Some("no_card")), "{body}");
    assert!(t.pending(&id).is_empty(), "no record of the charge");
    assert!(t.off.queue.lock().unwrap().file.charges.is_empty(), "nothing queued");
    assert_eq!(spent_slots(&card), 0, "nothing signed");

    let (status, body) = t.charge(&card, 20_000, "f1").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("offline").is_none(), "{body}");
    assert_eq!(json!(t.sale("f1").txid), body["txid"], "{body}");
    Ok(())
}

/// A first attempt whose record the repeat proves left nothing behind (the till died after the
/// card signed, before it posted the signed bytes to the registry: never sent) frees the ref. The
/// repeat settles that record from its card's file, the rejection reaches the registry, and another
/// card in the field pays the sale as a fresh attempt. Before ENG-858 the other card paid, but the
/// first card's record stayed in its file and its charge open at the registry.
#[tokio::test(flavor = "multi_thread")]
async fn a_repeat_of_a_charge_proven_never_sent_is_a_fresh_attempt() -> anyhow::Result<()> {
    repeat_after_a_charge_proven_never_sent(false).await
}

/// [`a_repeat_of_a_charge_proven_never_sent_is_a_fresh_attempt`] with the first card in the field
/// again (the common retry): the record is settled before the card is attached, so the registry's
/// release of the slot the first attempt signed reaches the card in the same job, and the card pays
/// the sale once, from that slot. Before ENG-858 the record was settled after the attach, and the
/// retap was answered 500 `internal`: the slot had signed the first attempt and was not released
/// yet.
#[tokio::test(flavor = "multi_thread")]
async fn the_same_card_after_a_charge_proven_never_sent_pays_the_sale_once() -> anyhow::Result<()> {
    repeat_after_a_charge_proven_never_sent(true).await
}

/// A first attempt on card A that is never sent, then its repeat on card A (`same_card`) or on
/// card B.
async fn repeat_after_a_charge_proven_never_sent(same_card: bool) -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (a, b) = (v3_card(), v3_card());
    let id_a = provision(&t.reg, &a);
    let id_b = provision(&t.reg, &b);
    t.load(&a, &[1 << 16, 1 << 14]).await?;
    t.load(&b, &[1 << 16, 1 << 14]).await?;
    t.cw.crash_point = Some(CrashPoint::SubmittedUnreachable);
    let (status, body) = t.charge(&a, 20_000, "r7").await;
    t.cw.crash_point = None;
    assert_eq!((status, body["error"].as_str()), (202, Some("undecided")), "{body}");
    // The till died before it posted the signed bytes: never sent, so proven to have left nothing.
    let mut file = StateStore::open(t.ops.card_path(&id_a))?;
    let old = file.state.pending_txs[0].txid.clone();
    file.state.pending_txs[0].ops_unconfirmed = false;
    file.save()?;

    let (card, id) = if same_card { (&a, &id_a) } else { (&b, &id_b) };
    let (w, calls) = Watched::new(card, false);
    let (status, body) = t.charge_watched(w, 20_000, "r7").await;
    assert_eq!(status, 200, "{body}");
    assert_ne!(body["txid"], json!(old), "a fresh charge, not the first attempt: {body}");
    assert!(calls.get() > 0, "the card in the field was charged");
    assert!(t.pending(&id_a).is_empty(), "card A's record is settled");
    assert_eq!(t.open_charges(&id_a), 0, "its rejection reached the registry, and nothing of card A is left open there");
    assert_eq!(t.sales_count(), 1);
    let sale = t.sale("r7");
    assert_eq!((sale.short_id.as_deref(), json!(sale.txid)), (Some(id.as_str()), body["txid"].clone()), "the sale is the card's");
    if same_card {
        // Paid from the slot the first attempt signed, which the registry's release gave back.
        let released = t.reg.released(&id_a, &old);
        assert!(
            !released.is_empty() && sale.inputs.iter().all(|i| released.contains(&u64::from(i.slot))),
            "released {released:?}, inputs {:?}",
            sale.inputs
        );
    } else {
        assert!(spent_slots(&b) > 0, "card B signed");
    }
    Ok(())
}

// ------------------------------------------------------------------ a charge a card left signing, at an online tap (ENG-873)

/// How a card leaves the field mid-sale ([`PulledAt`]).
type Gone = fn(&Rc<RefCell<SoftCard>>) -> PulledAt;

/// A card with four notes of 65,536 msat, tapped for a 90,000 msat sale (`pulled`: two of the
/// notes, with change) while the registry is down, that leaves the field as `gone` says; then the
/// registry is back. Answers the card and the charge it left `signing` in the till's queue.
async fn left_signing_offline(t: &mut Till, gone: Gone) -> anyhow::Result<(Rc<RefCell<SoftCard>>, OfflineCharge)> {
    let card = v3_card();
    provision(&t.reg, &card);
    t.load(&card, &[1 << 16, 1 << 16, 1 << 16, 1 << 16]).await?;
    t.set_down(true);
    let (status, body) = t.charge_with_gone(gone(&card), 90_000, "pulled").await;
    assert_eq!((status, body["error"].as_str()), (202, Some("undecided")), "{body}");
    let q = t.off.queue.lock().unwrap().file.charges.clone();
    assert_eq!(q.len(), 1);
    assert_eq!((q[0].state, q[0].inputs.len()), (ChargeState::Signing, 2));
    assert!(!q[0].change.is_empty(), "a charge with change");
    t.set_down(false);
    Ok((card, q[0].clone()))
}

/// The slots of a balance answer's notes the card can pay: `live`, and not held as stranded.
fn live_slots(b: &Value) -> Vec<u64> {
    let mut v: Vec<u64> = b["notes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["status"] == "live" && n["stranded"] != true)
        .map(|n| n["slot"].as_u64().unwrap())
        .collect();
    v.sort();
    v
}

/// A card taken away mid-sale while the registry was down left a charge `signing` in the till's
/// queue; back online, the card is tapped for another sale. The `signing` charge counted as known,
/// so the tap held none of its slots: the planner picked the note the card had signed for it, which
/// the card will not sign again (500 `internal` until ENG-873), and could plan change into its
/// CHANGE slots. Now the online charge settles it first. The card signed one input of two, so the
/// till finishes the charge from the card's stored signature and a fresh one for the other input,
/// records its sale (`offline`) and reports it in the same tap. The new sale pays around its slots,
/// nothing is held, and the sender settles the charge.
#[tokio::test(flavor = "multi_thread")]
async fn an_online_sale_settles_a_charge_the_card_left_signing_and_pays_around_it() -> anyhow::Result<()> {
    settled_at_an_online_sale(|c| PulledAt::new(c, 1), 1).await
}

/// [`an_online_sale_settles_a_charge_the_card_left_signing_and_pays_around_it`] for a card that
/// signed both inputs and left before its last answer reached the till: the charge is finished from
/// the card's stored signatures alone.
#[tokio::test(flavor = "multi_thread")]
async fn an_online_sale_settles_a_charge_whose_last_answer_was_lost_and_pays_around_it() -> anyhow::Result<()> {
    settled_at_an_online_sale(|c| PulledAt::answer_lost(c, 1), 2).await
}

/// A charge the card left `signing` as `gone` says, `signed` of its two inputs signed, then an
/// online sale of that card under another `client_ref`.
async fn settled_at_an_online_sale(gone: Gone, signed: usize) -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (card, left) = left_signing_offline(&mut t, gone).await?;
    assert_eq!(spent_slots(&card), signed);
    let ins: Vec<u64> = left.inputs.iter().map(|i| u64::from(i.slot)).collect();
    let change: Vec<u64> = left.change.iter().map(|c| u64::from(c.slot)).collect();

    let (status, body) = t.charge(&card, 20_000, "next").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("offline").is_none(), "taken online: {body}");
    let q = t.off.queue.lock().unwrap().file.charges.clone();
    assert_eq!(
        q.iter().map(|c| (c.txid.as_str(), c.state)).collect::<Vec<_>>(),
        vec![(left.txid.as_str(), ChargeState::Reported)],
        "finished and reported in the same tap"
    );
    assert_eq!(t.reg.reports.lock().unwrap().len(), 1);
    let sale = t.sale("pulled");
    assert!(sale.offline && sale.txid == left.txid, "the sale the card began: {sale:?}");
    assert!(!t.sale("next").offline);
    assert!(slots_of(&body, "inputs").iter().all(|s| !ins.contains(s)), "{body}");
    assert!(slots_of(&body, "change").iter().all(|s| !ins.contains(s) && !change.contains(s)), "{body}");
    assert_eq!(t.stranded_posts(), 0, "nothing held");
    // The charge's inputs are spent in the card's file once it is settled. Its report has the
    // registry mark them `pending` under it, and left so they passed for outputs of the charge
    // still to collect: each held the sale up for a whole collection timeout.
    let file = StateStore::open(t.ops.card_path(&left.short_id))?.state;
    for i in &left.inputs {
        let status: Vec<NoteStatus> = file.notes.iter().filter(|n| n.nonce == i.nonce).map(|n| n.status).collect();
        assert_eq!(status, vec![NoteStatus::Spent], "the input on slot {}", i.slot);
    }
    assert!(body["duration_ms"].as_u64().is_some_and(|ms| ms < 60_000), "no collection timeout waited out: {body}");

    let sent = t.send().await;
    assert_eq!(sent["settled"], json!([left.txid]), "{sent}");
    let b = t.balance(&card).await;
    assert_eq!((b["stranded_msat"].clone(), on_hold(&b)), (json!(0), vec![]), "{b}");
    assert!(live_slots(&b).iter().all(|s| !ins.contains(s)), "{b}");
    assert!(slot_states(&card, &change).iter().all(|s| *s != SlotState::Change), "its change landed");
    assert_eq!(t.stranded_posts(), 0);
    Ok(())
}

/// A charge a card left `signing` whose other input has since paid another transaction can never
/// be finished. The card's online charge sets it aside first, as its next offline tap would
/// (`abandoned`, its `client_ref` freed, no sale), so the tap holds its slots as a lost phone's:
/// the note the card signed for it and its CHANGE slots, reported in the same tap, and the sale
/// pays around them. Until ENG-873 the `signing` charge explained them, and the sale that picked
/// the note the card had signed was answered 500 `internal`.
#[tokio::test(flavor = "multi_thread")]
async fn an_online_sale_sets_aside_a_charge_the_card_can_never_finish_and_holds_its_slots() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (card, left) = left_signing_offline(&mut t, |c| PulledAt::new(c, 1)).await?;
    let (signed, unsigned) = (u64::from(left.inputs[0].slot), u64::from(left.inputs[1].slot));
    let change: Vec<u64> = left.change.iter().map(|c| u64::from(c.slot)).collect();
    assert_eq!(slot_states(&card, &[signed, unsigned]), vec![SlotState::Spent, SlotState::Note]);
    // Elsewhere the input the card had not signed for it pays another transaction.
    fedimint_card_host_card::softkit::open_test_session_under(&mut *card.borrow_mut(), &t.reg.cert_pub())?;
    card.borrow_mut().sign(unsigned as u8, &[9; 32])?;

    let (status, body) = t.charge(&card, 20_000, "next").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("offline").is_none(), "{body}");
    let mut lost = vec![signed, unsigned];
    lost.sort();
    assert!(slots_of(&body, "inputs").iter().all(|s| !lost.contains(s)), "{body}");
    assert!(slots_of(&body, "change").iter().all(|s| !lost.contains(s) && !change.contains(s)), "{body}");
    let gone = t.off.queue.lock().unwrap().file.charges.iter().find(|c| c.txid == left.txid).cloned().expect("kept for the operator");
    assert_eq!((gone.state, gone.client_ref.as_deref()), (ChargeState::Abandoned, None));
    let detail = gone.detail.unwrap_or_default();
    assert!(detail.contains(&format!("input slot {unsigned}")), "{detail}");
    assert!(t.sales.lock().unwrap().find_ref("pulled").is_none(), "nothing was charged for it");
    assert_eq!(t.stranded_posts(), 1, "reported in the same tap");
    let rows = t.stranded_post(0);
    assert!(rows.contains(&json!({"slot": signed, "kind": "spent", "txid": left.txid})), "{rows:?}");
    for s in &change {
        assert!(rows.contains(&json!({"slot": s, "kind": "change", "txid": left.txid})), "{rows:?}");
    }

    let b = t.balance(&card).await;
    assert_eq!(on_hold(&b), lost, "the note signed for the set aside charge, and the other one: {b}");
    assert_eq!(b["stranded_msat"], json!(2 * (1 << 16)), "{b}");
    let free = b["free_slots"].as_array().unwrap();
    assert!(change.iter().all(|s| !free.contains(&json!(s))), "its CHANGE slots are no room: {b}");
    assert_eq!(t.stranded_posts(), 1, "listed now, so not reported again");
    Ok(())
}

/// A card taken away at the first SIGN of a charge, before it signed anything, leaves the charge
/// `signing` all the same: the till could not read the card to tell. The card's online charge
/// drops it first, as its next offline tap would (the card signed none of its inputs, so it can
/// never be sent: the entry and its spent marks go), so the CHANGE slots the card opened for it are
/// stranded: held, reported as change rows with no spent row, and no room for the sale's change.
/// Until ENG-873 the `signing` charge explained them, and the sale planned its change into them.
#[tokio::test(flavor = "multi_thread")]
async fn an_online_sale_drops_a_charge_no_input_of_which_the_card_signed() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (card, left) = left_signing_offline(&mut t, |c| PulledAt::new(c, 0)).await?;
    assert_eq!(spent_slots(&card), 0, "nothing signed");
    let change: Vec<u64> = left.change.iter().map(|c| u64::from(c.slot)).collect();
    assert!(slot_states(&card, &change).iter().all(|s| *s == SlotState::Change), "{change:?}");

    let (status, body) = t.charge(&card, 20_000, "next").await;
    assert_eq!(status, 200, "{body}");
    assert!(slots_of(&body, "change").iter().all(|s| !change.contains(s)), "the CHANGE slots are no room: {body}");
    let q = t.off.queue.lock().unwrap().file.clone();
    assert!(q.charges.iter().all(|c| c.txid != left.txid), "dropped: {:?}", q.charges);
    assert!(left.inputs.iter().all(|i| !q.spent.contains(&i.nonce)), "its spent marks are gone");
    assert!(t.sales.lock().unwrap().find_ref("pulled").is_none());
    assert_eq!(t.stranded_posts(), 1, "reported in the same tap");
    let rows: Vec<(u64, String)> = t
        .stranded_post(0)
        .iter()
        .filter(|x| x["txid"] == json!(left.txid))
        .map(|x| (x["slot"].as_u64().unwrap(), x["kind"].as_str().unwrap().to_owned()))
        .collect();
    assert_eq!(rows, change.iter().map(|s| (*s, "change".to_owned())).collect::<Vec<_>>(), "change rows, no spent row");
    Ok(())
}

/// A balance tap carries no PIN, and the card signs nothing without one, not even a stored
/// signature: the tap settles no charge the card left `signing`. The charge stays as it was and
/// still explains its slots, so nothing is held or reported, and the note the card signed for it
/// counts in `live_msat` until the card's next charge settles it.
#[tokio::test(flavor = "multi_thread")]
async fn a_balance_tap_leaves_a_charge_a_card_left_signing_alone() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (card, left) = left_signing_offline(&mut t, |c| PulledAt::new(c, 1)).await?;
    let b = t.balance(&card).await;
    assert_eq!(t.off.queue.lock().unwrap().file.charges, vec![left.clone()], "the charge is as it was");
    assert_eq!((b["stranded_msat"].clone(), on_hold(&b)), (json!(0), vec![]), "{b}");
    assert_eq!(t.stranded_posts(), 0, "nothing reported");
    assert!(live_slots(&b).contains(&u64::from(left.inputs[0].slot)), "the note it signed still counts: {b}");
    assert_eq!(spent_slots(&card), 1, "nothing signed");
    Ok(())
}

/// A wrong PIN at an online sale of a card that left a charge `signing`: the settle's VERIFY_PIN
/// is refused and the tap ends `wrong_pin` with one try spent, before anything is reported,
/// attached or signed. A tap that went on would verify the same PIN again to top up the card's
/// spare reserve or to sign (two tries for one mistyped PIN), or plan from the note the card
/// signed for the charge (500 `internal`). The charge is left as it was; the right PIN then
/// settles it and takes the sale.
#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_pin_at_an_online_sale_settles_nothing_and_spends_one_try() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (card, left) = left_signing_offline(&mut t, |c| PulledAt::new(c, 1)).await?;
    t.tap(&card);
    t.cw.pin = Some(b"9999".to_vec());
    let r = charge_job_with(&mut t.cw, Some(&t.ops), Some(&t.off), 20_000, Some("next".into()), &t.sales).await;
    t.cw.pin = None;
    assert_eq!((r.status, r.body["error"].as_str()), (403, Some("wrong_pin")), "{}", r.body);
    assert!(r.body["detail"].as_str().is_some_and(|d| d.contains("1 wrong in a row")), "one try spent: {}", r.body);
    assert_eq!(t.off.queue.lock().unwrap().file.charges, vec![left.clone()], "left as it was");
    assert!(t.reg.reports.lock().unwrap().is_empty(), "nothing reported");
    assert_eq!(t.stranded_posts(), 0, "nothing held");
    assert_eq!(spent_slots(&card), 1, "nothing signed");
    let (status, body) = t.charge(&card, 20_000, "next").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(t.sale("pulled").txid, left.txid);
    Ok(())
}

/// The settle talks to the card in the field only for that card's own charges left `signing`,
/// and only with the holder's PIN: a job with no PIN settles nothing (`CARD_PIN` is never
/// tried), another card hears no VERIFY_PIN (a wrong PIN typed for it is not refused, because it
/// is not sent), and another card's online sale leaves this card's `signing` charge as it was.
#[tokio::test(flavor = "multi_thread")]
async fn the_settle_needs_the_pin_and_leaves_another_cards_charge_alone() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (card, left) = left_signing_offline(&mut t, |c| PulledAt::new(c, 1)).await?;
    t.tap(&card);
    t.cw.pin = None;
    let r = fedimint_card_sidecar::offline::settle_left_signing(&mut t.cw, &t.off);
    assert!(r.finished.is_empty() && r.error.is_none(), "{r:?}");
    assert_eq!(t.off.queue.lock().unwrap().file.charges, vec![left.clone()], "no PIN: nothing settled");

    let other = v3_card();
    provision(&t.reg, &other);
    t.load(&other, &[1 << 16, 1 << 16]).await?;
    t.tap(&other);
    t.cw.pin = Some(b"9999".to_vec());
    let r = fedimint_card_sidecar::offline::settle_left_signing(&mut t.cw, &t.off);
    t.cw.pin = None;
    assert!(r.finished.is_empty() && r.error.is_none(), "another card: no VERIFY_PIN sent: {r:?}");
    let (status, body) = t.charge(&other, 20_000, "other").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(t.off.queue.lock().unwrap().file.charges, vec![left.clone()], "another card: untouched");
    assert_eq!(spent_slots(&card), 1);
    Ok(())
}

/// The holder mistypes the PIN at the settle three times: each wrong PIN ends its tap with one
/// try counted (`wrong_pin`, the count in a row rising, each next try's wait served first), and
/// none of the taps reports, holds or signs anything. Applet 4.0 never blocks: the right PIN,
/// after the wait its try owes, settles the charge and takes the sale.
#[tokio::test(flavor = "multi_thread")]
async fn wrong_pins_at_the_settle_end_each_tap_and_the_right_one_after_its_wait_settles() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (card, left) = left_signing_offline(&mut t, |c| PulledAt::new(c, 1)).await?;
    for n in 1..=3 {
        t.tap(&card);
        t.cw.pin = Some(b"9999".to_vec());
        let r = charge_job_with(&mut t.cw, Some(&t.ops), Some(&t.off), 20_000, Some("next".into()), &t.sales).await;
        t.cw.pin = None;
        assert_eq!((r.status, r.body["error"].as_str()), (403, Some("wrong_pin")), "{}", r.body);
        assert!(r.body["detail"].as_str().is_some_and(|d| d.contains(&format!("{n} wrong in a row"))), "{}", r.body);
    }
    assert_eq!(t.off.queue.lock().unwrap().file.charges, vec![left.clone()], "left as it was");
    assert!(t.reg.reports.lock().unwrap().is_empty(), "nothing reported");
    assert_eq!(t.stranded_posts(), 0, "nothing held");
    assert_eq!(spent_slots(&card), 1, "nothing signed");
    assert_eq!(card.borrow_mut().get_state()?.owed, 4, "the next try owes four askings");
    let (status, body) = t.charge(&card, 20_000, "next").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(t.sale("pulled").txid, left.txid);
    assert_eq!(card.borrow_mut().get_state()?.wrong_tries, 0);
    Ok(())
}

// ------------------------------------------------------------------ no certificate key: no 500 when no PIN was sent

/// A card that left a charge signing offline (taken away at its second SIGN), back online.
async fn left_signing(t: &mut Till) -> anyhow::Result<(Rc<RefCell<SoftCard>>, Vec<OfflineCharge>)> {
    let card = v3_card();
    provision(&t.reg, &card);
    t.load(&card, &[1 << 16, 1 << 16, 1 << 16, 1 << 16]).await?;
    t.set_down(true);
    let (status, body) = t.charge_pulled(&card, 1, 90_000, "pulled").await;
    assert_eq!((status, body["error"].as_str()), (202, Some("undecided")), "{body}");
    let first = t.off.queue.lock().unwrap().file.charges.clone();
    assert_eq!((first.len(), first[0].state), (1, ChargeState::Signing));
    t.set_down(false);
    Ok((card, first))
}

/// An online sale that first settles a charge the card left signing, at a till that keeps no
/// certificate key and whose registry answers without one, is `offline_not_ready` (503): no PIN is
/// sent, nothing is signed, the left charge still waits, no sale is recorded. With the key back, the same sale goes through.
#[tokio::test(flavor = "multi_thread")]
async fn settling_a_charge_left_signing_without_the_certificate_key_is_offline_not_ready() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (card, first) = left_signing(&mut t).await?;
    forget_cert_key(t.dir.path());
    t.cw.cert_key = None;
    t.reg.no_cert.store(true, Ordering::SeqCst);
    let tries = card.borrow_mut().get_state()?.wrong_tries;
    let spent = spent_slots(&card);
    let (status, body) = t.charge(&card, 10_000, "next sale").await;
    assert_eq!((status, body["error"].as_str()), (503, Some("offline_not_ready")), "{body}");
    assert!(!card.borrow().nonce_live(), "no GET_NONCE was asked");
    assert_eq!(card.borrow_mut().get_state()?.wrong_tries, tries, "no try was spent");
    assert_eq!(spent_slots(&card), spent, "nothing signed");
    assert_eq!(t.off.queue.lock().unwrap().file.charges, first, "the left charge still waits for its card");
    assert!(t.sales.lock().unwrap().find_ref("next sale").is_none(), "no sale");

    t.reg.no_cert.store(false, Ordering::SeqCst);
    t.off.reach.reset();
    let (status, body) = t.charge(&card, 10_000, "next sale").await;
    assert_eq!(status, 200, "{body}");
    assert!(t.off.queue.lock().unwrap().file.charges.iter().all(|c| c.state != ChargeState::Signing), "the left charge was settled");
    Ok(())
}

/// The same with an uncertified clone of the card in the field (its slot 0 key, so the left
/// charge is "its"): `card_not_genuine` (403), no PIN, nothing signed on the clone, the charge
/// still waits for the genuine card.
#[tokio::test(flavor = "multi_thread")]
async fn a_clone_tapped_for_a_charge_left_signing_is_not_genuine() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let (card, first) = left_signing(&mut t).await?;
    let clone = Rc::new(RefCell::new(card.borrow().extract_clone()));
    let spent = spent_slots(&clone);
    let (status, body) = t.charge(&clone, 10_000, "a clone's sale").await;
    assert_eq!((status, body["error"].as_str()), (403, Some("card_not_genuine")), "{body}");
    assert_eq!(spent_slots(&clone), spent, "nothing signed on the clone");
    assert_eq!(t.off.queue.lock().unwrap().file.charges, first, "the left charge still waits for its card");
    assert!(t.sales.lock().unwrap().find_ref("a clone's sale").is_none(), "no sale");
    Ok(())
}

/// The other PIN paths of an online sale (the spare reserve and the dead change slots at the
/// attach, then the sign) answer the same: a clone `card_not_genuine` (403), no key
/// `offline_not_ready` (503); neither a 500, nothing signed, no try spent.
#[tokio::test(flavor = "multi_thread")]
async fn an_online_sale_that_sends_no_pin_is_never_a_500() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    provision(&t.reg, &card);
    t.load(&card, &[1 << 16]).await?;
    let clone = Rc::new(RefCell::new(card.borrow().extract_clone()));
    let (status, body) = t.charge(&clone, 20_000, "clone").await;
    assert_eq!((status, body["error"].as_str()), (403, Some("card_not_genuine")), "{body}");
    assert_eq!(spent_slots(&clone), 0, "nothing signed on the clone");

    forget_cert_key(t.dir.path());
    t.cw.cert_key = None;
    t.reg.no_cert.store(true, Ordering::SeqCst);
    t.off.reach.reset();
    let tries = card.borrow_mut().get_state()?.wrong_tries;
    let (status, body) = t.charge(&card, 20_000, "no key").await;
    assert_eq!((status, body["error"].as_str()), (503, Some("offline_not_ready")), "{body}");
    assert_eq!((spent_slots(&card), card.borrow_mut().get_state()?.wrong_tries), (0, tries));
    Ok(())
}

// ------------------------------------------------------------------ a rotated certificate key

fn another_cert_key() -> String {
    hex::encode(FakeOps::new().cert_pub())
}

fn cert_fetches(t: &Till) -> usize {
    t.reg.cert_fetches.load(Ordering::SeqCst)
}

/// A till that runs on after the registry rotated its certificate key: the first card certified
/// under the new key fails under the kept one, so that tap asks the registry once, keeps the new
/// key (the job and `till-ops.json`) and the card's PIN goes. A card that verifies costs no fetch.
#[tokio::test(flavor = "multi_thread")]
async fn a_rotated_certificate_key_is_picked_up_at_the_first_card_that_fails_under_the_old() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    provision(&t.reg, &card);
    t.load(&card, &[1 << 16]).await?;
    let new = hex::encode(t.reg.cert_pub());
    let stale = another_cert_key();
    fedimint_card_sidecar::claim::write_cert_key(t.dir.path(), &stale)?;
    t.cw.cert_key = Some(stale);
    let n0 = cert_fetches(&t);
    t.attach_tap(&card).await?;
    assert_eq!(cert_fetches(&t) - n0, 1, "one fetch");
    assert_eq!(t.cw.cert_key.as_deref(), Some(new.as_str()), "the job has the new key");
    assert_eq!(fedimint_card_sidecar::claim::read_cert_key(t.dir.path())?.as_deref(), Some(new.as_str()), "and the till keeps it");
    t.attach_tap(&card).await?;
    assert_eq!(cert_fetches(&t) - n0, 1, "a card that verifies costs no fetch");
    let (status, body) = t.charge(&card, 20_000, "after the rotation").await;
    assert_eq!(status, 200, "{body}");
    Ok(())
}

/// A card certified by some other registry fails under the till's key at every tap: each tap asks
/// the registry once (a charge's tap: its probe and that one), the key stays, and its PIN is never
/// sent (`card_not_genuine`).
#[tokio::test(flavor = "multi_thread")]
async fn a_card_another_registry_certified_costs_one_fetch_a_tap_and_is_refused() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let other = FakeOps::new();
    let card = v3_card();
    let id = provision(&other, &card);
    let rec = other.cards.lock().unwrap().remove(&id).unwrap();
    t.reg.cards.lock().unwrap().insert(id, rec);
    let kept = fedimint_card_sidecar::claim::read_cert_key(t.dir.path())?;
    let n0 = cert_fetches(&t);
    for tap in 1..=3 {
        t.attach_tap(&card).await?;
        assert_eq!(cert_fetches(&t) - n0, tap, "one fetch a tap");
    }
    assert_eq!(fedimint_card_sidecar::claim::read_cert_key(t.dir.path())?, kept, "the till's key stays");
    t.off.reach.reset();
    let n1 = cert_fetches(&t);
    // It holds no note this till can see, so the sale is refused before any PIN.
    let (status, body) = t.charge(&card, 20_000, "fake").await;
    assert_eq!((status, body["error"].as_str()), (402, Some("insufficient")), "{body}");
    assert_eq!(cert_fetches(&t) - n1, 2, "a charge's tap: the probe and one check");
    t.tap(&card);
    t.cw.pin = Some(TEST_PIN.to_vec());
    let e = t.cw.verify_pin_session().unwrap_err();
    assert_eq!(fedimint_card_sidecar::till::classify(&e).code, "card_not_genuine", "{e:#}");
    t.cw.pin = None;
    assert!(!card.borrow().nonce_live(), "no GET_NONCE was asked");
    Ok(())
}

/// With the registry's key route not answering, the till keeps the key it has: a card certified
/// under a newer key is refused this tap (403, no PIN) and picked up at a tap once it answers. A
/// kept key that is not hex is no key (503), never a 500.
#[tokio::test(flavor = "multi_thread")]
async fn with_the_registry_unreachable_the_till_keeps_its_old_key() -> anyhow::Result<()> {
    let mut t = Till::new(OfflineConfig::default()).await?;
    let card = v3_card();
    provision(&t.reg, &card);
    t.load(&card, &[1 << 16]).await?;
    let stale = another_cert_key();
    fedimint_card_sidecar::claim::write_cert_key(t.dir.path(), &stale)?;
    t.cw.cert_key = Some(stale.clone());
    t.reg.cert_down.store(true, Ordering::SeqCst);
    t.attach_tap(&card).await?;
    assert_eq!(t.cw.cert_key.as_deref(), Some(stale.as_str()), "the old key stays");
    assert_eq!(fedimint_card_sidecar::claim::read_cert_key(t.dir.path())?.as_deref(), Some(stale.as_str()));
    t.tap(&card);
    t.cw.pin = Some(TEST_PIN.to_vec());
    let e = t.cw.verify_pin_session().unwrap_err();
    assert_eq!(fedimint_card_sidecar::till::classify(&e).code, "card_not_genuine", "{e:#}");
    t.cw.cert_key = Some("not hex".into());
    let e = t.cw.verify_pin_session().unwrap_err();
    assert_eq!(fedimint_card_sidecar::till::error_reply(&e).status, 503, "{e:#}");
    assert!(!card.borrow().nonce_live(), "no GET_NONCE was asked");
    t.cw.pin = None;

    t.reg.cert_down.store(false, Ordering::SeqCst);
    t.attach_tap(&card).await?;
    assert_eq!(t.cw.cert_key, Some(hex::encode(t.reg.cert_pub())), "picked up once the registry answers");
    Ok(())
}
