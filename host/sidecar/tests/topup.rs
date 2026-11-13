//! Top-up at the POS by Lightning (docs/TOPUP.md): the till's routes, with a fake gateway (the
//! `TopupLightning` trait), a fake registry answering the holder load routes, the in-process
//! federation of `till_service.rs`, and a phone link answering from a `SoftCard` (as in
//! `pos_link.rs`).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use fedimint_card_host_card::{Card, SoftCard, StatusWord};
use fedimint_card_sidecar::glue::CardWallet;
use fedimint_card_sidecar::holder::{CardFull, Loader};
use fedimint_card_sidecar::ops::{Ops, OpsClient, OpsConfig, short_id_of};
use fedimint_card_sidecar::sales::SalesStore;
use fedimint_card_sidecar::state::StateStore;
use fedimint_card_sidecar::till::{self, Conn, OpsState};
use fedimint_card_sidecar::topup::{LnOutcome, TopupLightning, TopupStore, Topups};
use fedimint_card_sidecar::wallet::Wallet;
use fedimint_client::ClientHandleArc;
use fedimint_client::transaction::TransactionBuilder;
use fedimint_core::Amount;
use fedimint_core::core::OperationId;
use fedimint_dummy_client::{DummyClientInit, DummyClientModule};
use fedimint_dummy_server::DummyInit;
use fedimint_mint_client::MintClientInit;
use fedimint_mint_server::MintInit;
use fedimint_testing::federation::FederationTest;
use fedimint_testing::fixtures::Fixtures;
use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;

// ------------------------------------------------------------------ the fake gateway

#[derive(Default)]
struct FakeLn {
    n: AtomicU64,
    senders: Mutex<HashMap<String, oneshot::Sender<LnOutcome>>>,
    receivers: Mutex<HashMap<String, oneshot::Receiver<LnOutcome>>>,
    invoices: AtomicUsize,
}

impl FakeLn {
    fn settle(&self, op: &str, outcome: LnOutcome) {
        let tx = self.senders.lock().unwrap().remove(op).expect("an invoice for that operation");
        tx.send(outcome).unwrap();
    }
}

impl TopupLightning for FakeLn {
    fn invoice(&self, amount_msat: u64, _expiry_secs: u64) -> BoxFuture<'_, anyhow::Result<(String, String)>> {
        Box::pin(async move {
            let op = format!("{:064x}", self.n.fetch_add(1, Ordering::SeqCst));
            let (tx, rx) = oneshot::channel();
            self.senders.lock().unwrap().insert(op.clone(), tx);
            self.receivers.lock().unwrap().insert(op.clone(), rx);
            self.invoices.fetch_add(1, Ordering::SeqCst);
            Ok((op, format!("lnbcrt{amount_msat}fake")))
        })
    }

    fn wait(&self, operation_id: String, _expires_at_ms: u64) -> BoxFuture<'static, anyhow::Result<LnOutcome>> {
        let rx = self.receivers.lock().unwrap().remove(&operation_id);
        Box::pin(async move {
            match rx {
                Some(rx) => Ok(rx.await.unwrap_or(LnOutcome::Failed("dropped".into()))),
                None => Ok(LnOutcome::Failed("unknown operation".into())),
            }
        })
    }
}

// ------------------------------------------------------------------ the fake registry

/// The holder load routes, answering for one card: its free slots (the first `free` of slots 1 up
/// to the card's last registered key, which is `keys.len() - 1`, then any slot a till keyed), its
/// never-keyed slots (`unkeyed`, split keying), and every load, outcome and slot-keys post it was
/// sent.
struct FakeRegistry {
    federation_id: String,
    keys: Vec<String>,
    free: AtomicUsize,
    loads: Mutex<Vec<Value>>,
    outcomes: Mutex<Vec<Value>>,
    /// Answer this many outcomes 503 first (a registry that fails after the load was sent).
    fail_outcomes: AtomicUsize,
    /// Slots with no key on record, ascending.
    unkeyed: Mutex<Vec<u8>>,
    /// Keys a till added (`POST /slot-keys`), in order.
    keyed: Mutex<Vec<(u8, String)>>,
    /// Every `POST /slot-keys` body that was answered 200.
    key_posts: Mutex<Vec<Value>>,
    /// Answer this many slot-keys posts 503 first, without recording them.
    fail_keys: AtomicUsize,
    /// Bumped by each slot-keys post that added a key; loads must name it.
    version: AtomicU64,
}

impl FakeRegistry {
    fn new(federation_id: String, keys: Vec<String>, free: usize) -> Self {
        Self {
            federation_id,
            keys,
            free: AtomicUsize::new(free),
            loads: Mutex::new(vec![]),
            outcomes: Mutex::new(vec![]),
            fail_outcomes: AtomicUsize::new(0),
            unkeyed: Mutex::new(vec![]),
            keyed: Mutex::new(vec![]),
            key_posts: Mutex::new(vec![]),
            fail_keys: AtomicUsize::new(0),
            version: AtomicU64::new(1),
        }
    }
}

async fn slots(State(r): State<Arc<FakeRegistry>>, Path(_id): Path<String>) -> Json<Value> {
    let n = r.free.load(Ordering::SeqCst);
    let used: Vec<u64> = r
        .loads
        .lock()
        .unwrap()
        .iter()
        .flat_map(|l| l["notes"].as_array().unwrap().iter().map(|n| n["slot"].as_u64().unwrap()).collect::<Vec<_>>())
        .collect();
    let mut free: Vec<Value> =
        (1..r.keys.len() as u64).filter(|s| !used.contains(s)).take(n).map(|s| json!({"slot": s, "pubkey": r.keys[s as usize]})).collect();
    free.extend(r.keyed.lock().unwrap().iter().filter(|(s, _)| !used.contains(&(*s as u64))).map(|(s, k)| json!({"slot": s, "pubkey": k})));
    let unkeyed = r.unkeyed.lock().unwrap().clone();
    Json(json!({"version": r.version.load(Ordering::SeqCst), "federation_id": r.federation_id, "free": free, "unkeyed_slots": unkeyed}))
}

async fn slot_keys(
    State(r): State<Arc<FakeRegistry>>,
    Path(_id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    if r.fail_keys.load(Ordering::SeqCst) > 0 {
        r.fail_keys.fetch_sub(1, Ordering::SeqCst);
        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(json!({"ok": false, "reason": "down for a moment"})));
    }
    assert_eq!(headers.get("authorization").and_then(|h| h.to_str().ok()), Some("Bearer till_test"), "a till's token");
    let mut added = vec![];
    for k in body["keys"].as_array().unwrap() {
        let (slot, pubkey) = (k["slot"].as_u64().unwrap() as u8, k["pubkey"].as_str().unwrap().to_owned());
        let mut keyed = r.keyed.lock().unwrap();
        match keyed.iter().find(|(s, _)| *s == slot) {
            Some((_, had)) => assert_eq!(had, &pubkey, "another key on a keyed slot"),
            None => {
                assert!(r.unkeyed.lock().unwrap().contains(&slot), "slot {slot} is not unkeyed");
                keyed.push((slot, pubkey));
                added.push(slot);
            }
        }
    }
    r.unkeyed.lock().unwrap().retain(|s| !added.contains(s));
    if !added.is_empty() {
        r.version.fetch_add(1, Ordering::SeqCst);
    }
    r.key_posts.lock().unwrap().push(body);
    let unkeyed = r.unkeyed.lock().unwrap().clone();
    (
        axum::http::StatusCode::OK,
        Json(
            json!({"ok": true, "version": r.version.load(Ordering::SeqCst), "added": added, "replayed": added.is_empty(), "unkeyed_slots": unkeyed}),
        ),
    )
}

async fn load(State(r): State<Arc<FakeRegistry>>, Path(_id): Path<String>, Json(body): Json<Value>) -> Json<Value> {
    r.loads.lock().unwrap().push(body);
    Json(json!({"ok": true, "version": 2}))
}

async fn outcome(
    State(r): State<Arc<FakeRegistry>>,
    Path(_id): Path<String>,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    if r.fail_outcomes.load(Ordering::SeqCst) > 0 {
        r.fail_outcomes.fetch_sub(1, Ordering::SeqCst);
        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(json!({"ok": false, "reason": "down for a moment"})));
    }
    r.outcomes.lock().unwrap().push(body);
    (axum::http::StatusCode::OK, Json(json!({"ok": true, "version": 3})))
}

async fn serve_registry(r: Arc<FakeRegistry>) -> String {
    let app = axum::Router::new()
        .route("/cards/{id}/slots", get(slots))
        .route("/cards/{id}/load", post(load))
        .route("/cards/{id}/slot-keys", post(slot_keys))
        .route("/cards/{id}/load/outcome", post(outcome))
        .with_state(r);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

// ------------------------------------------------------------------ the till

async fn fund(client: &ClientHandleArc, amount: Amount) -> anyhow::Result<()> {
    let dummy = client.get_first_module::<DummyClientModule>()?;
    let op = OperationId::new_random();
    let range = client
        .finalize_and_submit_transaction(op, "fund", |_| (), TransactionBuilder::new().with_inputs(dummy.create_input(amount)))
        .await?;
    client.await_primary_bitcoin_module_outputs(op, range.into_iter().collect()).await?;
    Ok(())
}

struct Fx {
    addr: SocketAddr,
    ln: Arc<FakeLn>,
    topups: Arc<Topups>,
    registry: Arc<FakeRegistry>,
    card: Arc<Mutex<SoftCard>>,
    short_id: String,
    _fed: FederationTest,
    _dir: tempfile::TempDir,
}

/// How the till is set up: on ops (the fake registry), on no registry, or with no Lightning.
#[derive(Clone, Copy, PartialEq)]
enum Setup {
    OnOps,
    Local,
    NoLightning,
}

/// A till whose wallet holds 10,000 sats (standing in for what the gateway paid in), a keyed
/// card with seven free slots, and the actor to poll.
async fn start(setup: Setup) -> anyhow::Result<(Fx, impl std::future::Future<Output = ()>)> {
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit).with_module(DummyClientInit, DummyInit);
    let fed = tokio::time::timeout(Duration::from_secs(60), fixtures.new_fed_builder(0).build()).await?;
    let client = fed.new_client().await;
    fund(&client, Amount::from_sats(10_000)).await?;
    let wallet = Wallet::from_client(client).await?;
    let dir = tempfile::tempdir()?;

    let card = Arc::new(Mutex::new(SoftCard::new()));
    let mut keys = vec![];
    for s in 0..8 {
        keys.push(card.lock().unwrap().gen_key(s)?);
    }
    let short_id = short_id_of(&keys[0]);
    let registry = Arc::new(FakeRegistry::new(wallet.federation_id().to_string(), keys.iter().map(|k| k.to_string()).collect(), 7));
    let url = serve_registry(registry.clone()).await;
    let conn = match setup {
        Setup::Local => Conn::new(OpsState::Local, None),
        _ => {
            let ops = Ops::new(&OpsConfig { url: url.clone(), token: "till_test".into() }, dir.path())?;
            Conn::new(OpsState::Connected { url }, Some(ops))
        }
    };

    let store = StateStore::open(dir.path().join("card_state.json"))?;
    let cw = CardWallet::new(wallet, store, Box::new(till::NoCard));
    let sales = SalesStore::open(dir.path().join("sales.json"))?;
    let (handle, actor) =
        till::new_with_conn(cw, "http://127.0.0.1:1".into(), sales, Box::new(|| anyhow::bail!("no relay in this test")), conn).await;
    let ln = Arc::new(FakeLn::default());
    let topups = Topups::new(TopupStore::open(TopupStore::path_in(dir.path()))?, ln.clone());
    let handle = if setup == Setup::NoLightning { handle } else { handle.with_topups(topups.clone()) };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = till::router(handle).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Ok((Fx { addr, ln, topups, registry, card, short_id, _fed: fed, _dir: dir }, actor))
}

/// Run `test` next to the actor (which is not `Send`) on this task.
async fn with_actor<T: Send + 'static>(
    actor: impl std::future::Future<Output = ()>,
    test: impl std::future::Future<Output = T> + Send + 'static,
) -> T {
    let t = tokio::spawn(test);
    let mut actor = Box::pin(actor);
    tokio::select! {
        r = t => r.unwrap(),
        _ = &mut actor => panic!("actor ended early"),
    }
}

async fn call(addr: SocketAddr, method: &str, path: &str, link: Option<&str>, body: Option<Value>) -> (u16, Value) {
    let http = reqwest::Client::new();
    let mut req = match method {
        "GET" => http.get(format!("http://{addr}{path}")),
        _ => http.post(format!("http://{addr}{path}")).json(&body.unwrap_or(json!({}))),
    }
    .timeout(Duration::from_secs(120));
    if let Some(l) = link {
        req = req.header("X-Card-Link", l);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// The applet's APDU answers (`data || SW`), from a soft card (as in `pos_link.rs`).
fn answer(card: &Mutex<SoftCard>, apdu: &[u8]) -> Vec<u8> {
    let mut c = card.lock().unwrap();
    let r: anyhow::Result<Vec<u8>> = match (apdu[0], apdu[1]) {
        (0xB0, 0x10) if apdu[3] == 1 => c.gen_key_overwrite(apdu[2]).map(|k| k.serialize().to_vec()),
        (0xB0, 0x10) => c.gen_key(apdu[2]).map(|k| k.serialize().to_vec()),
        (0xB0, 0x11) => c.get_pub(apdu[2]).map(|k| k.serialize().to_vec()),
        (0xB0, 0x12) => {
            let m: [u8; 32] = apdu[5..37].try_into().unwrap();
            c.sign(apdu[2], &m).map(|s| s.as_ref().to_vec())
        }
        _ => c.apdu(apdu),
    };
    match r {
        Ok(mut d) => {
            d.extend_from_slice(&[0x90, 0x00]);
            d
        }
        Err(e) => {
            let sw = e.downcast_ref::<StatusWord>().map_or(0x6F00, |s| s.0);
            sw.to_be_bytes().to_vec()
        }
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Open a phone's card link; the task answers APDUs until the till closes it.
async fn open_link(addr: SocketAddr, card: Arc<Mutex<SoftCard>>) -> (String, tokio::task::JoinHandle<()>) {
    let (mut ws, _): (Ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/card/link")).await.unwrap();
    let ready: Value = serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    let id = ready["link"].as_str().unwrap().to_owned();
    let task = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws.next().await {
            let Message::Text(t) = msg else { continue };
            let v: Value = serde_json::from_str(t.as_str()).unwrap();
            let apdu = hex::decode(v["apdu"].as_str().unwrap()).unwrap();
            let frame = json!({"type": "rapdu", "id": v["id"], "rapdu": hex::encode(answer(&card, &apdu))}).to_string();
            if ws.send(Message::Text(frame.into())).await.is_err() {
                break;
            }
        }
    });
    (id, task)
}

/// Poll `GET /v1/topup/{id}` until its state is `want`.
async fn until_state(addr: SocketAddr, id: &str, want: &str) -> Value {
    for _ in 0..100 {
        let (s, v) = call(addr, "GET", &format!("/v1/topup/{id}"), None, None).await;
        assert_eq!(s, 200, "{v}");
        if v["state"] == want {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("top-up {id} never reached {want}");
}

fn op_of(topups: &Topups, id: &str) -> String {
    topups.get(id).unwrap().operation_id
}

// ------------------------------------------------------------------ the tests

#[tokio::test(flavor = "multi_thread")]
async fn a_paid_top_up_loads_the_card_once() -> anyhow::Result<()> {
    let (f, actor) = start(Setup::OnOps).await?;
    let (addr, ln, topups, registry, card, short_id) =
        (f.addr, f.ln.clone(), f.topups.clone(), f.registry.clone(), f.card.clone(), f.short_id.clone());
    with_actor(actor, async move {
        // 100 sats rounds to the nearest card amount, 100.352 sats in three notes.
        let (s, created) = call(addr, "POST", "/v1/topup", None, Some(json!({"amount_msat": 100_000, "client_ref": "t1"}))).await;
        assert_eq!(s, 200, "{created}");
        assert_eq!(created["amount_msat"], 100_352);
        assert_eq!(created["state"], "waiting");
        assert_eq!(created["invoice"], "lnbcrt100352fake");
        assert!(created["expires_at_ms"].as_u64().unwrap() > 0);
        assert!(created.get("operation_id").is_none(), "{created}");
        let id = created["topup_id"].as_str().unwrap().to_owned();

        // The same client_ref answers the same top-up, with no second invoice; another amount is a conflict.
        let (s, again) = call(addr, "POST", "/v1/topup", None, Some(json!({"amount_msat": 100_000, "client_ref": "t1"}))).await;
        assert_eq!((s, again["topup_id"].as_str()), (200, Some(id.as_str())), "{again}");
        assert_eq!(ln.invoices.load(Ordering::SeqCst), 1);
        let (s, conflict) = call(addr, "POST", "/v1/topup", None, Some(json!({"amount_msat": 50_000, "client_ref": "t1"}))).await;
        assert_eq!((s, conflict["error"].as_str()), (409, Some("client_ref_conflict")), "{conflict}");

        // Unpaid: no load, and the card is not touched.
        let (s, v) = call(addr, "POST", &format!("/v1/topup/{id}/load"), None, None).await;
        assert_eq!((s, v["error"].as_str()), (409, Some("not_paid")), "{v}");

        ln.settle(&op_of(&topups, &id), LnOutcome::Paid);
        until_state(addr, &id, "paid").await;

        let (link, phone) = open_link(addr, card.clone()).await;
        let (s, loaded) = call(addr, "POST", &format!("/v1/topup/{id}/load"), Some(&link), None).await;
        assert_eq!(s, 200, "{loaded}");
        assert_eq!(loaded["state"], "loaded");
        assert_eq!(loaded["amount_msat"], 100_352);
        let txid = loaded["load_txid"].as_str().unwrap().to_owned();
        tokio::time::timeout(Duration::from_secs(5), phone).await.expect("the till closes the link").unwrap();

        // Ops saw one load of three notes, and its signed outcome.
        let loads = registry.loads.lock().unwrap().clone();
        assert_eq!(loads.len(), 1);
        let amounts: Vec<u64> = loads[0]["notes"].as_array().unwrap().iter().map(|n| n["amount_msat"].as_u64().unwrap()).collect();
        assert_eq!(amounts.iter().sum::<u64>(), 100_352);
        assert_eq!(amounts.len(), 3);
        assert_eq!(loads[0]["txid"], txid);
        let outcomes = registry.outcomes.lock().unwrap().clone();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0]["notes"].as_array().unwrap().len(), 3, "{}", outcomes[0]);

        // Loaded is final: asking again answers the same load without a card.
        let (s, v) = call(addr, "POST", &format!("/v1/topup/{id}/load"), None, None).await;
        assert_eq!((s, v["load_txid"].as_str()), (200, Some(txid.as_str())), "{v}");
        assert_eq!(registry.loads.lock().unwrap().len(), 1, "no second load");
        let v = until_state(addr, &id, "loaded").await;
        assert_eq!(v["load_txid"], txid);
        assert_eq!(topups.get(&id).unwrap().short_id.as_deref(), Some(short_id.as_str()));
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_card_keeps_the_top_up_paid_until_it_has_room() -> anyhow::Result<()> {
    let (f, actor) = start(Setup::OnOps).await?;
    let (addr, ln, topups, registry, card) = (f.addr, f.ln.clone(), f.topups.clone(), f.registry.clone(), f.card.clone());
    with_actor(actor, async move {
        registry.free.store(2, Ordering::SeqCst);
        let (_, created) = call(addr, "POST", "/v1/topup", None, Some(json!({"amount_msat": 100_000}))).await;
        let id = created["topup_id"].as_str().unwrap().to_owned();
        ln.settle(&op_of(&topups, &id), LnOutcome::Paid);
        until_state(addr, &id, "paid").await;

        let (link, _phone) = open_link(addr, card.clone()).await;
        let (s, v) = call(addr, "POST", &format!("/v1/topup/{id}/load"), Some(&link), None).await;
        assert_eq!((s, v["error"].as_str()), (409, Some("card_full")), "{v}");
        assert!(registry.loads.lock().unwrap().is_empty(), "nothing was sent");
        until_state(addr, &id, "paid").await;

        registry.free.store(7, Ordering::SeqCst);
        let (link, _phone) = open_link(addr, card).await;
        let (s, v) = call(addr, "POST", &format!("/v1/topup/{id}/load"), Some(&link), None).await;
        assert_eq!((s, v["state"].as_str()), (200, Some("loaded")), "{v}");
    })
    .await;
    Ok(())
}

/// A top-up for a named card is rounded to fit that card's free slots, and a card with none is
/// refused before any invoice exists.
#[tokio::test(flavor = "multi_thread")]
async fn a_top_up_for_a_card_fits_its_free_slots() -> anyhow::Result<()> {
    let (f, actor) = start(Setup::OnOps).await?;
    let (addr, registry) = (f.addr, f.registry.clone());
    with_actor(actor, async move {
        registry.free.store(2, Ordering::SeqCst);
        let (s, v) = call(addr, "POST", "/v1/topup", None, Some(json!({"amount_msat": 100_000, "short_id": "0011aabb"}))).await;
        assert_eq!(s, 200, "{v}");
        let amount = v["amount_msat"].as_u64().unwrap();
        assert!(amount.count_ones() <= 2, "{amount} needs more than 2 notes");
        assert_eq!(amount, 98_304, "nearest 2-note amount to 100,000 msat");

        registry.free.store(0, Ordering::SeqCst);
        let (s, v) = call(addr, "POST", "/v1/topup", None, Some(json!({"amount_msat": 100_000, "short_id": "0011aabb"}))).await;
        assert_eq!((s, v["error"].as_str()), (409, Some("card_full")), "{v}");

        let (s, v) = call(addr, "POST", "/v1/topup", None, Some(json!({"amount_msat": 100_000, "short_id": "nothex!!"}))).await;
        assert_eq!((s, v["error"].as_str()), (400, Some("bad_request")), "{v}");
    })
    .await;
    Ok(())
}

/// Paid top-ups not yet on a card are listed for the card they were made for and for any card when
/// made for none; another card's, and waiting, expired or loaded ones, are not.
#[tokio::test(flavor = "multi_thread")]
async fn waiting_top_ups_are_offered_to_their_card() -> anyhow::Result<()> {
    let (f, actor) = start(Setup::OnOps).await?;
    let (addr, ln, topups, card) = (f.addr, f.ln.clone(), f.topups.clone(), f.card.clone());
    with_actor(actor, async move {
        let mine = "0011aabb";
        let create = |body: Value| async move {
            let (s, v) = call(addr, "POST", "/v1/topup", None, Some(body)).await;
            assert_eq!(s, 200, "{v}");
            v["topup_id"].as_str().unwrap().to_owned()
        };
        let a = create(json!({"amount_msat": 100_000, "short_id": mine})).await;
        let b = create(json!({"amount_msat": 50_000})).await;
        let c = create(json!({"amount_msat": 20_000, "short_id": "FFFF0000"})).await;
        let unpaid = create(json!({"amount_msat": 30_000, "short_id": mine})).await;
        let expired = create(json!({"amount_msat": 40_000, "short_id": mine})).await;
        for id in [&a, &b, &c] {
            ln.settle(&op_of(&topups, id), LnOutcome::Paid);
            until_state(addr, id, "paid").await;
        }
        ln.settle(&op_of(&topups, &expired), LnOutcome::Expired);
        until_state(addr, &expired, "expired").await;

        let ids =
            |v: &Value| v["topups"].as_array().unwrap().iter().map(|t| t["topup_id"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        let (s, v) = call(addr, "GET", &format!("/v1/topups/waiting?short_id={mine}"), None, None).await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(ids(&v), vec![a.clone(), b.clone()], "{v}");
        let first = &v["topups"][0];
        assert_eq!(first["amount_msat"].as_u64(), Some(100_352));
        assert_eq!(first["for_card"].as_str(), Some(mine));
        assert!(first["created_at_ms"].as_u64().is_some());
        assert!(v["topups"][1]["for_card"].is_null());

        let (_, v) = call(addr, "GET", "/v1/topups/waiting?short_id=ffff0000", None, None).await;
        assert_eq!(ids(&v), vec![b.clone(), c.clone()], "{v}");
        let (_, v) = call(addr, "GET", "/v1/topups/waiting", None, None).await;
        assert_eq!(ids(&v), vec![a.clone(), b.clone(), c.clone()], "{v}");
        assert!(!ids(&v).contains(&unpaid));

        let (s, v) = call(addr, "GET", "/v1/topups/waiting?short_id=nothex!!", None, None).await;
        assert_eq!((s, v["error"].as_str()), (400, Some("bad_request")), "{v}");

        // Loaded: no longer offered.
        let (link, _phone) = open_link(addr, card).await;
        let (s, v) = call(addr, "POST", &format!("/v1/topup/{a}/load"), Some(&link), None).await;
        assert_eq!((s, v["state"].as_str()), (200, Some("loaded")), "{v}");
        let (_, v) = call(addr, "GET", &format!("/v1/topups/waiting?short_id={mine}"), None, None).await;
        assert_eq!(ids(&v), vec![b.clone()], "{v}");
    })
    .await;
    Ok(())
}

/// A topups.json written before `for_card` existed (the zion till has one) still loads; its
/// top-ups belong to no card.
#[test]
fn old_records_without_for_card_load() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = TopupStore::path_in(dir.path());
    std::fs::write(
        &path,
        r#"{"version":1,"topups":[{"topup_id":"bbbbbbbbbbbbbbbb","client_ref":null,"requested_msat":100000,
        "amount_msat":100352,"invoice":"lnbc1","operation_id":"00","created_at_ms":5,"expires_at_ms":6,
        "state":"paid","load_txid":null,"short_id":null,"detail":null}]}"#,
    )?;
    let store = TopupStore::open(&path)?;
    let t = store.get("bbbbbbbbbbbbbbbb").expect("loaded");
    assert_eq!(t.for_card, None);
    assert_eq!(store.waiting_for(Some("0011aabb")).len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_and_failed_top_ups_do_not_load() -> anyhow::Result<()> {
    let (f, actor) = start(Setup::OnOps).await?;
    let (addr, ln, topups) = (f.addr, f.ln.clone(), f.topups.clone());
    with_actor(actor, async move {
        let (_, a) = call(addr, "POST", "/v1/topup", None, Some(json!({"amount_msat": 21_000}))).await;
        assert_eq!(a["amount_msat"], 20_992, "{a}");
        let a = a["topup_id"].as_str().unwrap().to_owned();
        ln.settle(&op_of(&topups, &a), LnOutcome::Expired);
        until_state(addr, &a, "expired").await;
        let (s, v) = call(addr, "POST", &format!("/v1/topup/{a}/load"), None, None).await;
        assert_eq!((s, v["error"].as_str()), (410, Some("expired")), "{v}");

        let (_, b) = call(addr, "POST", "/v1/topup", None, Some(json!({"amount_msat": 50_000}))).await;
        let b = b["topup_id"].as_str().unwrap().to_owned();
        ln.settle(&op_of(&topups, &b), LnOutcome::Failed("gateway gave up".into()));
        let v = until_state(addr, &b, "failed").await;
        assert_eq!(v["detail"], "gateway gave up");
        let (s, v) = call(addr, "POST", &format!("/v1/topup/{b}/load"), None, None).await;
        assert_eq!((s, v["error"].as_str()), (409, Some("topup_failed")), "{v}");

        let (s, v) = call(addr, "GET", "/v1/topup/0000000000000000", None, None).await;
        assert_eq!((s, v["error"].as_str()), (404, Some("unknown_topup")), "{v}");
        let (s, v) = call(addr, "POST", "/v1/topup/0000000000000000/load", None, None).await;
        assert_eq!((s, v["error"].as_str()), (404, Some("unknown_topup")), "{v}");
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_requests_are_refused_before_any_invoice() -> anyhow::Result<()> {
    let (f, actor) = start(Setup::OnOps).await?;
    let (addr, ln) = (f.addr, f.ln.clone());
    with_actor(actor, async move {
        for body in [json!({}), json!({"amount_msat": 0}), json!({"amount_msat": 100_000_001})] {
            let (s, v) = call(addr, "POST", "/v1/topup", None, Some(body.clone())).await;
            assert_eq!((s, v["error"].as_str()), (400, Some("bad_amount")), "{body}: {v}");
        }
        let (s, v) = call(addr, "POST", "/v1/topup", None, Some(json!({"amount_msat": 1000, "client_ref": "x".repeat(65)}))).await;
        assert_eq!((s, v["error"].as_str()), (400, Some("bad_request")), "{v}");
        let (s, v) = call(addr, "POST", "/v1/topup", None, Some(json!({"amount_msat": 1000, "client_ref": ""}))).await;
        assert_eq!((s, v["error"].as_str()), (400, Some("bad_request")), "{v}");
        assert_eq!(ln.invoices.load(Ordering::SeqCst), 0);
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_till_off_ops_or_without_lightning_takes_no_top_ups() -> anyhow::Result<()> {
    let (f, actor) = start(Setup::Local).await?;
    let addr = f.addr;
    with_actor(actor, async move {
        let (s, v) = call(addr, "POST", "/v1/topup", None, Some(json!({"amount_msat": 100_000}))).await;
        assert_eq!((s, v["error"].as_str()), (503, Some("ops_not_connected")), "{v}");
    })
    .await;

    let (f, actor) = start(Setup::NoLightning).await?;
    let addr = f.addr;
    with_actor(actor, async move {
        let (s, v) = call(addr, "POST", "/v1/topup", None, Some(json!({"amount_msat": 100_000}))).await;
        assert_eq!((s, v["error"].as_str()), (503, Some("lightning_unavailable")), "{v}");
        let (s, v) = call(addr, "GET", "/v1/topup/abc", None, None).await;
        assert_eq!((s, v["error"].as_str()), (503, Some("lightning_unavailable")), "{v}");
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_till_watches_its_unpaid_invoices() -> anyhow::Result<()> {
    // The store from a till that stopped with one invoice unpaid: a new `Topups` over it watches
    // that invoice again.
    let dir = tempfile::tempdir()?;
    let ln = Arc::new(FakeLn::default());
    let (op, _) = ln.invoice(100_352, 600).await?;
    {
        let mut store = TopupStore::open(TopupStore::path_in(dir.path()))?;
        store.insert(fedimint_card_sidecar::topup::TopupRec {
            topup_id: "aaaaaaaaaaaaaaaa".into(),
            client_ref: None,
            requested_msat: 100_000,
            amount_msat: 100_352,
            invoice: "lnbcrt100352fake".into(),
            operation_id: op.clone(),
            created_at_ms: 1,
            expires_at_ms: u64::MAX,
            state: fedimint_card_sidecar::topup::TopupState::Waiting,
            load_txid: None,
            short_id: None,
            detail: None,
            for_card: None,
        })?;
    }
    let mode = std::fs::metadata(TopupStore::path_in(dir.path()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(mode.permissions().mode() & 0o777, 0o600);
    }
    let topups = Topups::new(TopupStore::open(TopupStore::path_in(dir.path()))?, ln.clone());
    topups.watch_waiting();
    ln.settle(&op, LnOutcome::Paid);
    for _ in 0..100 {
        if topups.get("aaaaaaaaaaaaaaaa").unwrap().state == fedimint_card_sidecar::topup::TopupState::Paid {
            // And it is on disk.
            let again = TopupStore::open(TopupStore::path_in(dir.path()))?;
            assert_eq!(again.get("aaaaaaaaaaaaaaaa").unwrap().state, fedimint_card_sidecar::topup::TopupState::Paid);
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the restarted watcher never recorded the payment");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_load_cut_short_after_it_was_sent_is_finished_not_repeated() -> anyhow::Result<()> {
    let (f, actor) = start(Setup::OnOps).await?;
    let (addr, ln, topups, registry, card) = (f.addr, f.ln.clone(), f.topups.clone(), f.registry.clone(), f.card.clone());
    with_actor(actor, async move {
        let (_, created) = call(addr, "POST", "/v1/topup", None, Some(json!({"amount_msat": 100_000}))).await;
        let id = created["topup_id"].as_str().unwrap().to_owned();
        ln.settle(&op_of(&topups, &id), LnOutcome::Paid);
        until_state(addr, &id, "paid").await;

        // The load reaches the federation, then the registry fails before it has the outcome.
        registry.fail_outcomes.store(1, Ordering::SeqCst);
        let (link, _phone) = open_link(addr, card.clone()).await;
        let (s, v) = call(addr, "POST", &format!("/v1/topup/{id}/load"), Some(&link), None).await;
        assert_ne!(s, 200, "{v}");
        until_state(addr, &id, "paid").await;
        assert_eq!(registry.loads.lock().unwrap().len(), 1);

        // The next tap finishes that load: the top-up is loaded and nothing is sent twice.
        let (link, _phone) = open_link(addr, card).await;
        let (s, v) = call(addr, "POST", &format!("/v1/topup/{id}/load"), Some(&link), None).await;
        assert_eq!((s, v["state"].as_str()), (200, Some("loaded")), "{v}");
        let loads = registry.loads.lock().unwrap().clone();
        assert_eq!(loads.len(), 1, "no second load");
        assert_eq!(v["load_txid"], loads[0]["txid"]);
        assert_eq!(registry.outcomes.lock().unwrap().len(), 1);
    })
    .await;
    Ok(())
}

// ------------------------------------------------------------------ the holder load, by card size

/// A holder's wallet holding `funds` sats (no mint fees, as on the production federation), and a
/// fake registry for `card`, keyed in every slot it has, with every slot but 0 free.
async fn holder_on(card: &mut SoftCard, funds: u64) -> anyhow::Result<(FederationTest, Wallet, String, tempfile::TempDir)> {
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit).with_module(DummyClientInit, DummyInit);
    let fed = tokio::time::timeout(Duration::from_secs(60), fixtures.new_fed_builder(0).disable_mint_fees().build()).await?;
    let client = fed.new_client().await;
    fund(&client, Amount::from_sats(funds)).await?;
    let wallet = Wallet::from_client(client).await?;
    let keys: Vec<String> = (0..card.slot_len()).map(|s| card.gen_key(s).map(|k| k.to_string())).collect::<anyhow::Result<_>>()?;
    let free = keys.len() - 1;
    let registry = Arc::new(FakeRegistry::new(wallet.federation_id().to_string(), keys, free));
    let url = serve_registry(registry).await;
    Ok((fed, wallet, url, tempfile::tempdir()?))
}

/// 50,000 sats is twelve notes: a fresh 32-slot card takes them in one load.
#[tokio::test(flavor = "multi_thread")]
async fn a_50k_holder_load_fits_a_fresh_32_slot_card() -> anyhow::Result<()> {
    let mut card = SoftCard::new();
    assert_eq!(card.slot_len(), 32);
    let (_fed, wallet, url, dir) = holder_on(&mut card, 60_000).await?;
    let r = Loader::new(&wallet, &url, dir.path())?.load(&mut card, &[50_000_000]).await?;
    assert_eq!(r.notes.len(), 12, "{:?}", r.notes);
    assert_eq!(r.notes.iter().map(|(_, a)| a).sum::<u64>(), 50_000_000);
    assert!(r.notes.iter().all(|(s, _)| (1..32).contains(s)), "{:?}", r.notes);
    Ok(())
}

/// The same load on a 2.0 card (eight slots, seven free) is refused before anything is built, as
/// it always was.
#[tokio::test(flavor = "multi_thread")]
async fn a_50k_holder_load_is_refused_on_an_eight_slot_card() -> anyhow::Result<()> {
    let mut card = SoftCard::with_slots(8);
    let (_fed, wallet, url, dir) = holder_on(&mut card, 60_000).await?;
    let before = wallet.balance_msat().await?;
    let err = Loader::new(&wallet, &url, dir.path())?.load(&mut card, &[50_000_000]).await.expect_err("twelve notes, seven slots");
    let full = err.downcast_ref::<CardFull>().unwrap_or_else(|| panic!("{err:#}"));
    assert_eq!((full.free, full.needed), (7, 12));
    assert_eq!(wallet.balance_msat().await?, before, "nothing was paid");
    // A load that fits still loads.
    let r = Loader::new(&wallet, &url, dir.path())?.load(&mut card, &[(1 << 20) + (1 << 18) + (1 << 16)]).await?;
    assert_eq!(r.notes.len(), 3);
    assert!(r.notes.iter().all(|(s, _)| (1..8).contains(s)), "{:?}", r.notes);
    Ok(())
}

// ------------------------------------------------------------------ split keying

/// The card's key in `slot`, hex, as `GET_PUB` reads it.
fn card_key(card: &Mutex<SoftCard>, slot: u8) -> Option<String> {
    card.lock().unwrap().get_pub(slot).ok().map(|k| k.to_string())
}

/// A 32-slot card registered with keys for slots 0-7 only: the load's tap keys the shortfall plus
/// `KEY_AHEAD` of the lowest unkeyed slots, records them in one post, and loads against the
/// version that post answered. The next top-up fits the slots keyed ahead and keys nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_top_up_load_keys_a_split_cards_slots_at_the_tap() -> anyhow::Result<()> {
    let (f, actor) = start(Setup::OnOps).await?;
    let (addr, ln, topups, registry, card) = (f.addr, f.ln.clone(), f.topups.clone(), f.registry.clone(), f.card.clone());
    with_actor(actor, async move {
        registry.free.store(1, Ordering::SeqCst);
        *registry.unkeyed.lock().unwrap() = (8..32).collect();
        assert_eq!(card_key(&card, 8), None, "slot 8 is empty on the card");

        let (_, created) = call(addr, "POST", "/v1/topup", None, Some(json!({"amount_msat": 100_000}))).await;
        let id = created["topup_id"].as_str().unwrap().to_owned();
        ln.settle(&op_of(&topups, &id), LnOutcome::Paid);
        until_state(addr, &id, "paid").await;
        let (link, _phone) = open_link(addr, card.clone()).await;
        let (s, v) = call(addr, "POST", &format!("/v1/topup/{id}/load"), Some(&link), None).await;
        assert_eq!((s, v["state"].as_str()), (200, Some("loaded")), "{v}");

        // Three notes, one free slot: 2 short, plus KEY_AHEAD.
        let want = 2 + fedimint_card_sidecar::ops::KEY_AHEAD;
        let posts = registry.key_posts.lock().unwrap().clone();
        assert_eq!(posts.len(), 1, "one post for all the keys");
        let keyed = registry.keyed.lock().unwrap().clone();
        assert_eq!(keyed.iter().map(|(s, _)| *s).collect::<Vec<_>>(), (8..8 + want as u8).collect::<Vec<_>>());
        for (slot, k) in &keyed {
            assert_eq!(card_key(&card, *slot).as_deref(), Some(k.as_str()), "slot {slot}: the card's own key");
        }
        assert_eq!(card_key(&card, 8 + want as u8), None, "nothing keyed beyond the margin");
        let loads = registry.loads.lock().unwrap().clone();
        assert_eq!(loads[0]["version"], 2, "the load names the version the keys answered");
        let slots: Vec<u64> = loads[0]["notes"].as_array().unwrap().iter().map(|n| n["slot"].as_u64().unwrap()).collect();
        assert_eq!(slots, vec![1, 8, 9]);

        // The next top-up fits the slots keyed ahead: no keying at that tap.
        let (_, created) = call(addr, "POST", "/v1/topup", None, Some(json!({"amount_msat": 100_000}))).await;
        let id = created["topup_id"].as_str().unwrap().to_owned();
        ln.settle(&op_of(&topups, &id), LnOutcome::Paid);
        until_state(addr, &id, "paid").await;
        let (link, _phone) = open_link(addr, card.clone()).await;
        let (s, v) = call(addr, "POST", &format!("/v1/topup/{id}/load"), Some(&link), None).await;
        assert_eq!((s, v["state"].as_str()), (200, Some("loaded")), "{v}");
        assert_eq!(registry.key_posts.lock().unwrap().len(), 1, "no second keying");
    })
    .await;
    Ok(())
}

/// A top-up named for a split-keyed card counts its unkeyed slots as room, since the load's tap
/// keys them; a card with neither free nor unkeyed slots is still refused before any invoice.
#[tokio::test(flavor = "multi_thread")]
async fn a_top_up_for_a_split_card_counts_its_unkeyed_slots_as_room() -> anyhow::Result<()> {
    let (f, actor) = start(Setup::OnOps).await?;
    let (addr, registry) = (f.addr, f.registry.clone());
    with_actor(actor, async move {
        let create = |body: Value| async move { call(addr, "POST", "/v1/topup", None, Some(body)).await };
        registry.free.store(0, Ordering::SeqCst);
        *registry.unkeyed.lock().unwrap() = (8..32).collect();
        let (s, v) = create(json!({"amount_msat": 100_000, "short_id": "0011aabb"})).await;
        assert_eq!((s, v["amount_msat"].as_u64()), (200, Some(100_352)), "three notes fit: {v}");

        *registry.unkeyed.lock().unwrap() = vec![31];
        let (s, v) = create(json!({"amount_msat": 100_000, "short_id": "0011aabb"})).await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["amount_msat"].as_u64().unwrap().count_ones(), 1, "one unkeyed slot, one note: {v}");

        registry.unkeyed.lock().unwrap().clear();
        let (s, v) = create(json!({"amount_msat": 100_000, "short_id": "0011aabb"})).await;
        assert_eq!((s, v["error"].as_str()), (409, Some("card_full")), "{v}");
        assert!(registry.key_posts.lock().unwrap().is_empty(), "making an invoice keys nothing");
    })
    .await;
    Ok(())
}

/// The registry does not answer the keys: the load is refused (nothing sent, the top-up stays
/// paid), and the keys the tap made stay on the card. The next tap finds them with `GET_PUB`,
/// records those same keys, and loads.
#[tokio::test(flavor = "multi_thread")]
async fn keys_a_tap_made_but_never_recorded_are_recorded_by_the_next_tap() -> anyhow::Result<()> {
    let (f, actor) = start(Setup::OnOps).await?;
    let (addr, ln, topups, registry, card) = (f.addr, f.ln.clone(), f.topups.clone(), f.registry.clone(), f.card.clone());
    with_actor(actor, async move {
        registry.free.store(0, Ordering::SeqCst);
        *registry.unkeyed.lock().unwrap() = (8..32).collect();
        registry.fail_keys.store(1, Ordering::SeqCst);
        let (_, created) = call(addr, "POST", "/v1/topup", None, Some(json!({"amount_msat": 100_000}))).await;
        let id = created["topup_id"].as_str().unwrap().to_owned();
        ln.settle(&op_of(&topups, &id), LnOutcome::Paid);
        until_state(addr, &id, "paid").await;

        let (link, _phone) = open_link(addr, card.clone()).await;
        let (s, v) = call(addr, "POST", &format!("/v1/topup/{id}/load"), Some(&link), None).await;
        assert_eq!((s, v["error"].as_str()), (502, Some("ops_unreachable")), "{v}");
        until_state(addr, &id, "paid").await;
        assert!(registry.loads.lock().unwrap().is_empty(), "nothing was loaded");
        assert!(registry.keyed.lock().unwrap().is_empty(), "nothing was recorded");
        let want = 3 + fedimint_card_sidecar::ops::KEY_AHEAD;
        let made: Vec<String> = (8..8 + want as u8).map(|s| card_key(&card, s).expect("keyed on the card")).collect();

        let (link, _phone) = open_link(addr, card.clone()).await;
        let (s, v) = call(addr, "POST", &format!("/v1/topup/{id}/load"), Some(&link), None).await;
        assert_eq!((s, v["state"].as_str()), (200, Some("loaded")), "{v}");
        let keyed: Vec<String> = registry.keyed.lock().unwrap().iter().map(|(_, k)| k.clone()).collect();
        assert_eq!(keyed, made, "the keys the first tap made, not new ones");
        assert_eq!(card_key(&card, 8 + want as u8), None);
    })
    .await;
    Ok(())
}

/// A holder's wallet holding `funds` sats (no mint fees) and a fake registry for a fresh 32-slot
/// `card` registered with keys for slots 0-7 only (split keying), 1-7 free and 8-31 unkeyed.
async fn holder_on_split(
    card: &mut SoftCard,
    funds: u64,
) -> anyhow::Result<(FederationTest, Wallet, Arc<FakeRegistry>, String, tempfile::TempDir)> {
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit).with_module(DummyClientInit, DummyInit);
    let fed = tokio::time::timeout(Duration::from_secs(60), fixtures.new_fed_builder(0).disable_mint_fees().build()).await?;
    let client = fed.new_client().await;
    fund(&client, Amount::from_sats(funds)).await?;
    let wallet = Wallet::from_client(client).await?;
    let keys: Vec<String> = (0..8).map(|s| card.gen_key(s).map(|k| k.to_string())).collect::<anyhow::Result<_>>()?;
    let registry = Arc::new(FakeRegistry::new(wallet.federation_id().to_string(), keys, 7));
    *registry.unkeyed.lock().unwrap() = (8..card.slot_len()).collect();
    let url = serve_registry(registry.clone()).await;
    Ok((fed, wallet, registry, url, tempfile::tempdir()?))
}

/// 50,000 sats (twelve notes) onto a split-keyed card: a holder's own loader cannot key, so it is
/// refused and says why; the till's loader keys slots 8-15 (5 short plus `KEY_AHEAD`) and loads.
#[tokio::test(flavor = "multi_thread")]
async fn a_50k_load_onto_a_split_card_needs_a_till_to_key_it() -> anyhow::Result<()> {
    let mut card = SoftCard::new();
    let (_fed, wallet, registry, url, dir) = holder_on_split(&mut card, 60_000).await?;
    let err = Loader::new(&wallet, &url, dir.path())?.load(&mut card, &[50_000_000]).await.expect_err("seven keyed slots");
    let full = err.downcast_ref::<CardFull>().unwrap_or_else(|| panic!("{err:#}"));
    assert_eq!((full.free, full.needed, full.unkeyed), (7, 12, 24));
    assert!(full.to_string().contains("a till keys them"), "{full}");
    assert!(registry.key_posts.lock().unwrap().is_empty());

    let till = OpsClient::new(&OpsConfig { url: url.clone(), token: "till_test".into() })?;
    let r = Loader::new(&wallet, &url, dir.path())?.with_keyer(till).load(&mut card, &[50_000_000]).await?;
    assert_eq!(r.notes.len(), 12, "{:?}", r.notes);
    let slots: Vec<u8> = r.notes.iter().map(|(s, _)| *s).collect();
    assert_eq!(slots, (1..13).collect::<Vec<u8>>());
    let keyed: Vec<u8> = registry.keyed.lock().unwrap().iter().map(|(s, _)| *s).collect();
    assert_eq!(keyed, (8..16).collect::<Vec<u8>>());
    assert_eq!(*registry.unkeyed.lock().unwrap(), (16..32).collect::<Vec<u8>>());
    assert!(card.get_pub(16).is_err(), "slot 16 stays empty on the card");
    Ok(())
}

/// A load that cannot fit even with every unkeyed slot keyed keys nothing; one that fits the free
/// slots keys nothing either.
#[tokio::test(flavor = "multi_thread")]
async fn a_till_keys_only_when_keying_makes_the_load_fit() -> anyhow::Result<()> {
    let mut card = SoftCard::new();
    let (_fed, wallet, registry, url, dir) = holder_on_split(&mut card, 60_000).await?;
    let till = OpsClient::new(&OpsConfig { url: url.clone(), token: "till_test".into() })?;
    *registry.unkeyed.lock().unwrap() = vec![8, 9];
    let err =
        Loader::new(&wallet, &url, dir.path())?.with_keyer(till.clone()).load(&mut card, &[50_000_000]).await.expect_err("7 + 2 < 12");
    assert!(err.downcast_ref::<CardFull>().is_some(), "{err:#}");
    assert!(registry.key_posts.lock().unwrap().is_empty());
    assert!(card.get_pub(8).is_err());

    let r = Loader::new(&wallet, &url, dir.path())?.with_keyer(till).load(&mut card, &[(1 << 20) + (1 << 18)]).await?;
    assert_eq!(r.notes.len(), 2);
    assert!(registry.key_posts.lock().unwrap().is_empty(), "two notes fit seven free slots");
    Ok(())
}

/// The keying arithmetic: nothing when the notes fit, else the shortfall plus `KEY_AHEAD`, never
/// more slots than are unkeyed. An 8-slot card has none unkeyed, so it never keys.
#[test]
fn slots_to_key_is_the_shortfall_plus_the_margin() {
    use fedimint_card_sidecar::ops::{KEY_AHEAD, slots_to_key};
    assert_eq!(slots_to_key(3, 3, 24), 0);
    assert_eq!(slots_to_key(3, 7, 24), 0);
    assert_eq!(slots_to_key(3, 1, 24), 2 + KEY_AHEAD);
    assert_eq!(slots_to_key(12, 7, 24), 5 + KEY_AHEAD);
    assert_eq!(slots_to_key(12, 7, 6), 6);
    assert_eq!(slots_to_key(12, 7, 0), 0);
}
