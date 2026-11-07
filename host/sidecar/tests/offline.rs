//! F3: the till with applet 3.0 cards (docs/OFFLINE-PROTOCOL.md § 4), online and offline, against
//! the in-process federation of `till_service.rs` and a fake registry that answers the routes a
//! till uses (`state`, `intent`, `submitted`, `outcome`, slot renewals, `cert-key`,
//! `offline-charges`, and the releases in the state) as `registry/src/cards.ts` and `offline.ts` do. "Down" makes every route
//! answer 502 with no body, as an unreachable registry behind a proxy does.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use fedimint_card_host_card::{Card, Certificate, SlotState, SoftCard, open_pin_session};
use fedimint_card_sidecar::glue::CardWallet;
use fedimint_card_sidecar::offline::{Offline, OfflineConfig};
use fedimint_card_sidecar::ops::{MAX_KEY_PER_TAP, Ops, OpsConfig, SPARE_TARGET, SpareConfig, short_id_of};
use fedimint_card_sidecar::sales::SalesStore;
use fedimint_card_sidecar::state::StateStore;
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
    outcomes: Mutex<Vec<Value>>,
    reports: Mutex<Vec<Value>>,
    /// Every `POST /slot-keys` body, in order.
    key_posts: Mutex<Vec<Value>>,
    /// Every renewal taken (`PUT /slots/:n`), by slot, in order.
    renewals: Mutex<Vec<u8>>,
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
            outcomes: Mutex::new(vec![]),
            reports: Mutex::new(vec![]),
            key_posts: Mutex::new(vec![]),
            renewals: Mutex::new(vec![]),
        })
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
    if let Some(d) = r.down() {
        return d;
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
    ok(json!({
        "version": c.version,
        "slots": c.slots.iter().map(|(s, k)| json!({"slot": s, "pubkey": k})).collect::<Vec<_>>(),
        "unkeyed_slots": unkeyed_of(c),
        "notes": c.notes,
        "open_txs": open,
        "releases": c.releases,
    }))
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

async fn outcome(State(r): S, Path(id): Path<String>, Json(b): Json<Value>) -> Response {
    if let Some(d) = r.down() {
        return d;
    }
    let mut g = r.cards.lock().unwrap();
    let c = g.get_mut(&id).unwrap();
    let Some(t) = c.txs.iter_mut().find(|t| t["txid"] == b["txid"]) else { return fail(404, "no such charge") };
    if t["status"] != "open" {
        return ok(json!({"ok": true, "version": c.version}));
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
            let mut slots: Vec<u8> = c.notes.iter().filter(|n| ins.contains(&n["nonce"])).map(|n| n["slot"].as_u64().unwrap() as u8).collect();
            slots.extend(change.as_array().unwrap().iter().map(|ch| ch["slot"].as_u64().unwrap() as u8));
            slots.sort();
            slots.dedup();
            add_releases(&r.cert, c, &b["txid"], &slots);
        }
    }
    c.version += 1;
    r.outcomes.lock().unwrap().push(b.clone());
    ok(json!({"ok": true, "version": c.version}))
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
    if c.txs.iter().any(|t| t["txid"] == b["txid"]) {
        return ok(json!({"ok": true, "version": c.version}));
    }
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

async fn serve(r: Arc<FakeOps>) -> String {
    let app = axum::Router::new()
        .route("/registry/cert-key", get(cert_key))
        .route("/federations", post(federations))
        .route("/cards/{id}/state", get(state))
        .route("/cards/{id}/intent", post(intent))
        .route("/cards/{id}/submitted", post(submitted))
        .route("/cards/{id}/outcome", post(outcome))
        .route("/cards/{id}/slots/{slot}", put(renew))
        .route("/cards/{id}/slot-keys", post(slot_keys))
        .route("/cards/{id}/offline-charges", post(offline_charges))
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
    let mut c = card.borrow_mut();
    let keys: Vec<XOnlyPublicKey> = (0..n).map(|s| c.gen_key(s).unwrap()).collect();
    if c.version().sign_once() {
        let w = c.attest_init().unwrap();
        let cert = Certificate::sign(&reg.cert, &keys[0].serialize(), &w).unwrap();
        c.set_cert(&cert.to_bytes()).unwrap();
        c.set_registry_key(&reg.cert_pub().try_into().unwrap()).unwrap();
    }
    open_pin_session(&mut *c).unwrap();
    let id = short_id_of(&keys[0]);
    reg.cards.lock().unwrap().insert(
        id.clone(),
        FakeCard { version: 1, slots: keys.iter().enumerate().map(|(i, k)| (i as u8, k.to_string())).collect(), ..Default::default() },
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
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit).with_module(DummyClientInit, DummyInit);
    Ok(tokio::time::timeout(Duration::from_secs(60), fixtures.new_fed_builder(0).build()).await?)
}

struct Till {
    cw: CardWallet,
    ops: Ops,
    off: Arc<Offline>,
    sales: Arc<Mutex<SalesStore>>,
    reg: Arc<FakeOps>,
    dir: tempfile::TempDir,
    _fed: FederationTest,
}

impl Till {
    async fn new(cfg: OfflineConfig) -> anyhow::Result<Self> {
        let fed = federation().await?;
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
        let mut t = Self { cw, ops, off, sales, reg, dir, _fed: fed };
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
    Rc::new(RefCell::new(SoftCard::v3()))
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
    assert!(slot_states(&card, &change).iter().all(|s| *s == SlotState::Note), "the change is on the card: {:?}", slot_states(&card, &change));
    for s in slots_of(&body, "inputs") {
        // Spent, then renewed in the same tap (GEN_KEY P2=1 on a SPENT slot).
        assert!(matches!(card.borrow().slot_state(s as u8).unwrap(), SlotState::Spent | SlotState::Keyed));
    }
    let open = t.reg.cards.lock().unwrap()[&id].txs.iter().filter(|x| x["status"] == "open").count();
    assert_eq!(open, 0, "the outcome reached the registry");
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
    clone.borrow_mut().verify_pin(b"1234")?;
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
    let mut t = Till::new(OfflineConfig { cap_msat: 60_000, cap_count: 1 }).await?;
    let card = v3_card();
    provision(&t.reg, &card);
    t.load(&card, &[1 << 16, 1 << 15]).await?;
    let old = Rc::new(RefCell::new(SoftCard::new()));
    provision(&t.reg, &old);
    t.load(&old, &[1 << 16]).await?;
    t.set_down(true);

    let (status, body) = t.charge(&old, 10_000, "f").await;
    assert_eq!((status, body["error"].as_str()), (409, Some("offline_unsupported_card")), "{body}");

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

// ------------------------------------------------------------------ status

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_reachability_and_the_offline_queue() -> anyhow::Result<()> {
    let t = Till::new(OfflineConfig { cap_msat: 5_000_000, cap_count: 7 }).await?;
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
    assert_eq!(v["offline"], json!({"queued": 0, "queued_msat": 0, "double_spent": 0, "refused": 0, "rejected": 0, "cap_msat": 5_000_000, "cap_count": 7}));
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
    // Few keyed slots, so the released ones are needed again.
    let id = provision_slots(&t.reg, &card, 8);
    t.load(&card, &[1 << 16]).await?;
    t.set_down(true);
    let (status, body) = t.charge(&card, 20_000, "r1").await;
    assert_eq!(status, 200, "{body}");
    let change = slots_of(&body, "change");
    assert!(!change.is_empty() && slot_states(&card, &change).iter().all(|s| *s == SlotState::Change), "{body}");
    let nonce = t.off.queue.lock().unwrap().file.charges[0].inputs[0].nonce.clone();
    t.reg.set_note_status(&id, &nonce, "spent");
    t.set_down(false);
    let sent = t.send().await;
    assert_eq!(sent["double_spent"].as_array().unwrap().len(), 1, "{sent}");
    let detail = t.off.queue.lock().unwrap().file.charges[0].detail.clone().unwrap();
    assert!(detail.contains("released at its next online tap"), "{detail}");
    let listed: Vec<u64> = t.reg.cards.lock().unwrap()[&id].releases.iter().map(|r| r["slot"].as_u64().unwrap()).collect();
    assert_eq!(listed, change, "a release per change slot, none for the input");

    // The next online tap: CHANGE -> KEYED, and the slots take notes again.
    t.load(&card, &[1 << 15]).await?;
    assert!(slot_states(&card, &change).iter().all(|s| matches!(s, SlotState::Keyed | SlotState::Note)), "{:?}", slot_states(&card, &change));
    let input = slots_of(&body, "inputs")[0];
    assert_eq!(card.borrow().slot_state(input as u8), Some(SlotState::Spent), "the input was spent elsewhere: not released");
    // Free slots are taken lowest first, and the released ones are the lowest.
    let loaded: Vec<u64> = change.iter().copied().filter(|s| card.borrow().slot_state(*s as u8) == Some(SlotState::Note)).collect();
    assert_eq!(loaded.len(), 1, "the load went to a released slot: {:?}", slot_states(&card, &change));
    // A charge with change, into released slots again.
    let (status, body) = t.charge(&card, 10_000, "r2").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("offline").is_none(), "{body}");
    let change2 = slots_of(&body, "change");
    assert!(!change2.is_empty() && change2.iter().all(|s| change.contains(s)), "change into the released slots: {body}");
    assert!(slot_states(&card, &change2).iter().all(|s| *s == SlotState::Note), "{:?}", slot_states(&card, &change2));
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
    assert!(t.reg.outcomes.lock().unwrap().iter().any(|o| o["txid"] == txid && o["accepted"] == false), "the rejection reached the registry");
    let detail = t.off.queue.lock().unwrap().file.charges[0].detail.clone().unwrap();
    assert!(detail.contains("released at its next online tap"), "{detail}");
    let mut want: Vec<u64> = inputs.iter().chain(&change).copied().collect();
    want.sort();
    let listed: Vec<u64> = t.reg.cards.lock().unwrap()[&id].releases.iter().map(|r| r["slot"].as_u64().unwrap()).collect();
    assert_eq!(listed, want);
    assert_eq!(slot_states(&card, &inputs), vec![SlotState::Spent]);

    // The next online tap releases: the input is a note again, the change slots are keyed.
    t.tap(&card);
    let r = card_balance_job(&mut t.cw, Some(&t.ops), &t.sales).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(slot_states(&card, &inputs), vec![SlotState::Note]);
    assert!(slot_states(&card, &change).iter().all(|s| *s == SlotState::Keyed), "{:?}", slot_states(&card, &change));
    // And that note pays an online charge.
    let (status, body) = t.charge(&card, 20_000, "f2").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("offline").is_none(), "{body}");
    assert_eq!(slots_of(&body, "inputs"), inputs);
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
/// notes, which keys five more (the shortfall plus KEY_AHEAD): 10 notes and 3 spare slots.
async fn drill_card(t: &mut Till) -> anyhow::Result<(Rc<RefCell<SoftCard>>, String)> {
    let card = v3_card();
    let id = provision_split(&t.reg, &card, 8);
    t.load(&card, &[(1 << 20) - (1 << 10)]).await?;
    assert_eq!(keyed_on(&card), 13);
    assert_eq!(fresh_on(t, &id, &card), 3);
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
    // As in the drill: one offline charge with change, then none.
    assert_eq!(taken, 1, "without the reserve the card runs out of slots for change after one");
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
    // The drill's numbers (13 keyed after the load, 3 of them spare) are a target of 6; at the
    // default of 12 the load's own tap already keys spares. The rule is the same.
    const TARGET: usize = 6;
    t.cw.spares = SpareConfig { target: TARGET, max_per_tap: MAX_KEY_PER_TAP };
    let (card, id) = drill_card(&mut t).await?;

    // Taps with nothing else to do: +2, then +1 to the target, then nothing.
    let mut keyed = vec![];
    for _ in 0..3 {
        let (before, posts) = (keyed_on(&card), t.reg.key_posts.lock().unwrap().len());
        t.attach_tap(&card).await?;
        let added = keyed_on(&card) - before;
        assert!(added <= MAX_KEY_PER_TAP, "{added} keys in one tap");
        assert_eq!(t.reg.key_posts.lock().unwrap().len(), posts + usize::from(added > 0), "one slot-keys post per tap that keyed");
        keyed.push(added);
    }
    assert_eq!(keyed, vec![2, 1, 0]);
    assert_eq!(fresh_on(&t, &id, &card), TARGET);
    {
        // The registry has the card's keys, and lists the rest as unkeyed.
        let g = t.reg.cards.lock().unwrap();
        for (slot, k) in &g[&id].slots {
            assert_eq!(*k, card.borrow_mut().get_pub(*slot)?.to_string(), "slot {slot}");
        }
        assert_eq!(unkeyed_of(&g[&id]), (16..32).collect::<Vec<u8>>());
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
