//! Refunding a pay whose invoice was not paid (`POST /v1/pay/{id}/refund`): the till loads the
//! value it holds for the holder back onto the card that paid, as a holder load (docs/TOPUP.md),
//! at most once. The till is on a fake registry answering the holder load routes (as in
//! `topup.rs`), with the in-process federation of `till_service.rs` and a phone link answering
//! from a `SoftCard`. The pays are records put in the till's store (`tests/pay.rs` covers how a
//! pay comes to be `pay_failed`).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use fedimint_card_host_card::{Card, SoftCard, StatusWord};
use fedimint_card_sidecar::glue::CardWallet;
use fedimint_card_sidecar::holder::JOURNAL_FILE;
use fedimint_card_sidecar::ops::{Ops, OpsConfig, short_id_of};
use fedimint_card_sidecar::pay::{PayConfig, PayLightning, PayRec, PayState, PayStore, Pays};
use fedimint_card_sidecar::plan;
use fedimint_card_sidecar::sales::SalesStore;
use fedimint_card_sidecar::state::StateStore;
use fedimint_card_sidecar::till::{self, Conn, OpsState};
use fedimint_card_sidecar::wallet::{Wallet, WalletPay};
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
use lightning_invoice::Currency;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

// ------------------------------------------------------------------ the fake registry

/// The holder load routes for one card, as in `topup.rs`: its free slots (the first `free` of
/// slots 1 to 7 no load took), and every load and outcome it was sent.
struct FakeRegistry {
    federation_id: String,
    keys: Vec<String>,
    free: AtomicUsize,
    loads: Mutex<Vec<Value>>,
    outcomes: Mutex<Vec<Value>>,
    /// Answer this many outcomes 503 first (a registry that fails after the load was sent).
    fail_outcomes: AtomicUsize,
    /// Answer this many loads 503 first, keeping none of them (a registry that may or may not
    /// have taken the load: the till's journal keeps it, and the federation never sees it).
    fail_loads: AtomicUsize,
    version: AtomicU64,
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
    let free: Vec<Value> =
        (1..r.keys.len() as u64).filter(|s| !used.contains(s)).take(n).map(|s| json!({"slot": s, "pubkey": r.keys[s as usize]})).collect();
    Json(json!({"version": r.version.load(Ordering::SeqCst), "federation_id": r.federation_id, "free": free, "unkeyed_slots": []}))
}

async fn load(
    State(r): State<Arc<FakeRegistry>>,
    Path(_id): Path<String>,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    if r.fail_loads.load(Ordering::SeqCst) > 0 {
        r.fail_loads.fetch_sub(1, Ordering::SeqCst);
        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(json!({"ok": false, "reason": "down for a moment"})));
    }
    r.loads.lock().unwrap().push(body);
    (axum::http::StatusCode::OK, Json(json!({"ok": true, "version": 2})))
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
        .route("/cards/{id}/load/outcome", post(outcome))
        .with_state(r);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

// ------------------------------------------------------------------ the till

/// A wallet side that is never asked: these pays are over.
struct NoLn;
impl PayLightning for NoLn {
    fn pay(&self, _: String, _: u64, _: Duration) -> BoxFuture<'_, anyhow::Result<WalletPay>> {
        Box::pin(async { anyhow::bail!("not asked") })
    }
    fn resolve(&self, _: String, _: Duration) -> BoxFuture<'_, anyhow::Result<WalletPay>> {
        Box::pin(async { anyhow::bail!("not asked") })
    }
    fn status(&self, _: String, _: Duration) -> BoxFuture<'_, anyhow::Result<Option<WalletPay>>> {
        Box::pin(async { anyhow::bail!("not asked") })
    }
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

struct Fx {
    addr: SocketAddr,
    pays: Arc<Pays>,
    registry: Arc<FakeRegistry>,
    card: Arc<Mutex<SoftCard>>,
    short_id: String,
    wallet: Wallet,
    dir: tempfile::TempDir,
    _fed: FederationTest,
}

/// A till on the fake registry whose wallet holds 10,000 sats (the value the failed pays left
/// there), a keyed card with seven free slots, and the actor to poll.
async fn start(on_ops: bool) -> anyhow::Result<(Fx, impl std::future::Future<Output = ()>)> {
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
    let registry = Arc::new(FakeRegistry {
        federation_id: wallet.federation_id().to_string(),
        keys: keys.iter().map(|k| k.to_string()).collect(),
        free: AtomicUsize::new(7),
        loads: Mutex::new(vec![]),
        outcomes: Mutex::new(vec![]),
        fail_outcomes: AtomicUsize::new(0),
        fail_loads: AtomicUsize::new(0),
        version: AtomicU64::new(1),
    });
    let url = serve_registry(registry.clone()).await;
    let conn = if on_ops {
        let ops = Ops::new(&OpsConfig { url: url.clone(), token: "till_test".into() }, dir.path())?;
        Conn::new(OpsState::Connected { url }, Some(ops))
    } else {
        Conn::new(OpsState::Local, None)
    };

    let store = StateStore::open(dir.path().join("card_state.json"))?;
    let cw = CardWallet::new(wallet.clone(), store, Box::new(till::NoCard));
    let sales = SalesStore::open(dir.path().join("sales.json"))?;
    let (handle, actor) =
        till::new_with_conn(cw, "http://127.0.0.1:1".into(), sales, Box::new(|| anyhow::bail!("no relay in this test")), conn).await;
    let pays = Pays::new(
        PayStore::open(PayStore::path_in(dir.path()))?,
        Arc::new(NoLn),
        PayConfig { enabled: true, ..PayConfig::default() },
        Currency::Regtest,
    );
    let handle = handle.with_pays(pays.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = till::router(handle).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Ok((Fx { addr, pays, registry, card, short_id, wallet, dir, _fed: fed }, actor))
}

/// A pay of `price_msat` in `state`, charged to card `short_id`, put in the till's store.
fn put_pay(pays: &Pays, client_ref: &str, state: PayState, price_msat: u64, short_id: Option<&str>) -> u64 {
    let rec = PayRec {
        pay_id: 0,
        client_ref: client_ref.into(),
        bolt11: "lnbcrt1".into(),
        payment_hash: format!("{:064x}", rand::random::<u64>()),
        invoice_msat: price_msat - 1_150,
        fee_reserve_msat: 1_000,
        margin_msat: 150,
        price_msat,
        invoice_expires_at_ms: u64::MAX,
        created_at_ms: 1,
        updated_at_ms: 1,
        state,
        sale_id: Some(1),
        txid: Some("ab".repeat(32)),
        operation_id: None,
        preimage: None,
        fee_paid_msat: None,
        detail: Some("no route".into()),
        short_id: short_id.map(str::to_owned),
        refund_msat: None,
        refund_txid: None,
    };
    pays.store.lock().unwrap().insert(rec).unwrap().pay_id
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

async fn call(addr: SocketAddr, method: &str, path: &str, link: Option<&str>) -> (u16, Value) {
    let http = reqwest::Client::new();
    let mut req = match method {
        "GET" => http.get(format!("http://{addr}{path}")),
        _ => http.post(format!("http://{addr}{path}")).json(&json!({})),
    }
    .timeout(Duration::from_secs(120));
    if let Some(l) = link {
        req = req.header("X-Card-Link", l);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `POST /v1/pay/{id}/refund`, with the card behind a phone link when `card` is given.
async fn refund(addr: SocketAddr, id: u64, card: Option<Arc<Mutex<SoftCard>>>) -> (u16, Value) {
    let path = format!("/v1/pay/{id}/refund");
    match card {
        Some(c) => {
            let (link, _phone) = open_link(addr, c).await;
            call(addr, "POST", &path, Some(&link)).await
        }
        None => call(addr, "POST", &path, None).await,
    }
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

/// What the till loads for a price with seven free slots: rounded down to whole notes, never up.
fn refund_for(wallet: &Wallet, price: u64) -> u64 {
    plan::card_amount_at_most(price, 7, wallet.min_denom(), |n| wallet.mint_cfg.tbs_pks.get(Amount::from_msats(n)).is_some()).unwrap()
}

/// The msat of every load the registry was sent.
fn loaded(registry: &FakeRegistry) -> Vec<u64> {
    registry
        .loads
        .lock()
        .unwrap()
        .iter()
        .map(|l| l["notes"].as_array().unwrap().iter().map(|n| n["amount_msat"].as_u64().unwrap()).sum())
        .collect()
}

/// 10,000 msat + a 1,000 msat fee reserve + 150 msat margin.
const PRICE: u64 = 11_150;

// ------------------------------------------------------------------ the tests

#[tokio::test(flavor = "multi_thread")]
async fn a_pay_failed_pay_goes_back_onto_its_card_once() -> anyhow::Result<()> {
    let (f, actor) = start(true).await?;
    let id = put_pay(&f.pays, "a", PayState::PayFailed, PRICE, Some(&f.short_id));
    let want = refund_for(&f.wallet, PRICE);
    // Never more than the till holds for the holder; what stays is less than the smallest note.
    let dust = PRICE - want;
    assert!(dust < f.wallet.min_denom(), "{want}");
    let (addr, pays, registry, card, dir) = (f.addr, f.pays.clone(), f.registry.clone(), f.card.clone(), f.dir.path().to_owned());
    with_actor(actor, async move {
        let (s, v) = refund(addr, id, Some(card.clone())).await;
        assert_eq!((s, v["state"].as_str(), v["refund_msat"].as_u64()), (200, Some("refunded"), Some(want)), "{v}");
        assert_eq!((v["refund_dust_msat"].as_u64(), v.get("refund_owed_msat")), (Some(dust), None), "{v}");
        let loads = registry.loads.lock().unwrap().clone();
        assert_eq!(loads.len(), 1);
        assert_eq!(v["refund_txid"], loads[0]["txid"]);
        let notes: u64 = loads[0]["notes"].as_array().unwrap().iter().map(|n| n["amount_msat"].as_u64().unwrap()).sum();
        assert_eq!(notes, want, "the notes loaded are the refund");
        assert_eq!(registry.outcomes.lock().unwrap().len(), 1);
        // Never a preimage or an invoice in the answer.
        assert!(v.get("preimage").is_none() && v.get("bolt11").is_none(), "{v}");

        // Again, with or without the card: the same refund, nothing loaded twice.
        let (s, again) = refund(addr, id, None).await;
        assert_eq!((s, again["state"].as_str(), &again["refund_txid"]), (200, Some("refunded"), &v["refund_txid"]), "{again}");
        let (s, again) = refund(addr, id, Some(card)).await;
        assert_eq!((s, again["state"].as_str()), (200, Some("refunded")), "{again}");
        assert_eq!(registry.loads.lock().unwrap().len(), 1, "no second load");

        // The record, in memory and on disk.
        let (s, g) = call(addr, "GET", &format!("/v1/pay/{id}"), None).await;
        assert_eq!((s, g["state"].as_str(), g["refund_msat"].as_u64()), (200, Some("refunded"), Some(want)), "{g}");
        let disk = PayStore::open(PayStore::path_in(&dir)).unwrap();
        assert_eq!(disk.get(id).unwrap().state, PayState::Refunded);
        assert_eq!(pays.get(id).unwrap().refund_txid, disk.get(id).unwrap().refund_txid);
        // A refund is not a payment: the pay still answers that its invoice was not paid.
        let (s, l) = call(addr, "GET", "/v1/pays", None).await;
        assert_eq!((s, l["total_price_msat"].as_u64(), l["pays"][0]["state"].as_str()), (200, Some(0), Some("refunded")), "{l}");
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_card_that_paid_and_only_a_pay_failed_pay_is_refunded() -> anyhow::Result<()> {
    let (f, actor) = start(true).await?;
    let other = short_id_of(&SoftCard::new().gen_key(0)?);
    assert_ne!(other, f.short_id);
    let theirs = put_pay(&f.pays, "theirs", PayState::PayFailed, PRICE, Some(&other));
    let paid = put_pay(&f.pays, "paid", PayState::Paid, PRICE, Some(&f.short_id));
    let open = put_pay(&f.pays, "open", PayState::Paying, PRICE, Some(&f.short_id));
    let unknown = put_pay(&f.pays, "unknown", PayState::PayFailed, PRICE, None);
    let (addr, pays, registry, card) = (f.addr, f.pays.clone(), f.registry.clone(), f.card.clone());
    with_actor(actor, async move {
        // Another card: refused before anything is loaded; the pay stays pay_failed.
        let (s, v) = refund(addr, theirs, Some(card.clone())).await;
        assert_eq!((s, v["error"].as_str(), v["state"].as_str()), (409, Some("wrong_card"), Some("pay_failed")), "{v}");
        assert_eq!(pays.get(theirs).unwrap().state, PayState::PayFailed);
        // Paid, or not settled yet: nothing to refund.
        for (id, state) in [(paid, "paid"), (open, "paying")] {
            let (s, v) = refund(addr, id, Some(card.clone())).await;
            assert_eq!((s, v["error"].as_str(), v["state"].as_str()), (409, Some("not_refundable"), Some(state)), "{v}");
        }
        assert_eq!((pays.get(paid).unwrap().state, pays.get(open).unwrap().state), (PayState::Paid, PayState::Paying));
        // A pay whose card was never recorded: not refunded to whichever card is presented.
        let (s, v) = refund(addr, unknown, Some(card.clone())).await;
        assert_eq!((s, v["error"].as_str()), (409, Some("card_unknown")), "{v}");
        // No such pay.
        assert_eq!(refund(addr, 99, None).await.0, 404);
        assert_eq!(call(addr, "POST", "/v1/pay/x/refund", None).await.0, 404);
        assert!(registry.loads.lock().unwrap().is_empty(), "nothing was loaded");
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_card_or_a_till_off_ops_leaves_the_pay_owed() -> anyhow::Result<()> {
    let (f, actor) = start(true).await?;
    let id = put_pay(&f.pays, "full", PayState::PayFailed, PRICE, Some(&f.short_id));
    let (addr, pays, registry, card) = (f.addr, f.pays.clone(), f.registry.clone(), f.card.clone());
    with_actor(actor, async move {
        // No free slot: refused before anything is built; pay_failed, not refunding.
        registry.free.store(0, Ordering::SeqCst);
        let (s, v) = refund(addr, id, Some(card.clone())).await;
        assert_eq!((s, v["error"].as_str(), v["state"].as_str()), (409, Some("card_full"), Some("pay_failed")), "{v}");
        assert_eq!(pays.get(id).unwrap().state, PayState::PayFailed);
        assert!(registry.loads.lock().unwrap().is_empty());
        // Room again: refunded.
        registry.free.store(7, Ordering::SeqCst);
        let (s, v) = refund(addr, id, Some(card)).await;
        assert_eq!((s, v["state"].as_str()), (200, Some("refunded")), "{v}");
    })
    .await;

    let (f, actor) = start(false).await?;
    let id = put_pay(&f.pays, "local", PayState::PayFailed, PRICE, Some(&f.short_id));
    let (addr, pays, card) = (f.addr, f.pays.clone(), f.card.clone());
    with_actor(actor, async move {
        let (s, v) = refund(addr, id, Some(card)).await;
        assert_eq!((s, v["error"].as_str(), v["state"].as_str()), (503, Some("ops_not_connected"), Some("pay_failed")), "{v}");
        assert_eq!(pays.get(id).unwrap().state, PayState::PayFailed);
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refund_cut_short_after_it_was_sent_is_finished_not_loaded_again() -> anyhow::Result<()> {
    let (f, actor) = start(true).await?;
    let id = put_pay(&f.pays, "cut", PayState::PayFailed, PRICE, Some(&f.short_id));
    let (addr, pays, registry, card) = (f.addr, f.pays.clone(), f.registry.clone(), f.card.clone());
    with_actor(actor, async move {
        // The load reaches the federation, then the registry fails before it has the outcome:
        // the refund stays `refunding`, its load in the journal.
        registry.fail_outcomes.store(1, Ordering::SeqCst);
        let (s, v) = refund(addr, id, Some(card)).await;
        assert_ne!(s, 200, "{v}");
        assert_eq!(v["state"], "refunding", "{v}");
        assert_eq!(pays.get(id).unwrap().state, PayState::Refunding);
        assert_eq!(registry.loads.lock().unwrap().len(), 1);

        // The next call finishes that load, without a card: refunded, nothing sent twice.
        let (s, v) = refund(addr, id, None).await;
        assert_eq!((s, v["state"].as_str()), (200, Some("refunded")), "{v}");
        let loads = registry.loads.lock().unwrap().clone();
        assert_eq!(loads.len(), 1, "no second load");
        assert_eq!(v["refund_txid"], loads[0]["txid"]);
        assert_eq!(registry.outcomes.lock().unwrap().len(), 1);
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refund_whose_result_was_never_saved_is_not_loaded_again() -> anyhow::Result<()> {
    let (f, actor) = start(true).await?;
    let id = put_pay(&f.pays, "crash", PayState::PayFailed, PRICE, Some(&f.short_id));
    let (addr, pays, registry, card, dir) = (f.addr, f.pays.clone(), f.registry.clone(), f.card.clone(), f.dir.path().to_owned());
    with_actor(actor, async move {
        let (s, v) = refund(addr, id, Some(card.clone())).await;
        assert_eq!((s, v["state"].as_str()), (200, Some("refunded")), "{v}");
        let txid = v["refund_txid"].clone();
        // The journal kept how the refund's load ended.
        let journal: Value = serde_json::from_slice(&std::fs::read(dir.join(JOURNAL_FILE)).unwrap()).unwrap();
        assert_eq!(journal["kept"][0]["tag"], format!("pay-refund:{id}"), "{journal}");
        assert_eq!((journal["kept"][0]["accepted"].as_bool(), &journal["kept"][0]["txid"]), (Some(true), &txid));

        // A crash after the load, before `refunded` was saved: the record says `refunding`.
        let mut rec = pays.get(id).unwrap();
        rec.state = PayState::Refunding;
        rec.refund_txid = None;
        pays.store.lock().unwrap().put(&rec).unwrap();
        let (s, v) = refund(addr, id, Some(card)).await;
        assert_eq!((s, v["state"].as_str(), &v["refund_txid"]), (200, Some("refunded"), &txid), "{v}");
        assert_eq!(registry.loads.lock().unwrap().len(), 1, "no second load");
        assert_eq!(PayStore::open(PayStore::path_in(&dir)).unwrap().get(id).unwrap().state, PayState::Refunded);
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refund_cut_short_before_its_load_loads_once_on_the_next_call() -> anyhow::Result<()> {
    let (f, actor) = start(true).await?;
    // A crash after `refunding` was saved and before the load was journalled: nothing moved.
    let id = put_pay(&f.pays, "early", PayState::Refunding, PRICE, Some(&f.short_id));
    let (addr, registry, card) = (f.addr, f.registry.clone(), f.card.clone());
    with_actor(actor, async move {
        let (s, v) = refund(addr, id, Some(card)).await;
        assert_eq!((s, v["state"].as_str()), (200, Some("refunded")), "{v}");
        assert_eq!(registry.loads.lock().unwrap().len(), 1);
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_card_with_too_few_slots_takes_the_refund_over_several_taps_and_never_more() -> anyhow::Result<()> {
    let (f, actor) = start(true).await?;
    // One free slot: a refund rounded up would load 2^21 for this price, twice what the till holds.
    let price = (1u64 << 20) + (1 << 10) + 1;
    let id = put_pay(&f.pays, "slots", PayState::PayFailed, price, Some(&f.short_id));
    let min = f.wallet.min_denom();
    assert!(min <= 1 << 10, "the test federation's smallest note is {min}");
    let (addr, pays, registry, card, dir) = (f.addr, f.pays.clone(), f.registry.clone(), f.card.clone(), f.dir.path().to_owned());
    with_actor(actor, async move {
        registry.free.store(1, Ordering::SeqCst);
        // The first tap loads the largest note at or below the price: pay_failed, owed the rest.
        let (s, v) = refund(addr, id, Some(card.clone())).await;
        assert_eq!((s, v["state"].as_str(), v["refund_msat"].as_u64()), (200, Some("pay_failed"), Some(1 << 20)), "{v}");
        assert_eq!(v["refund_owed_msat"].as_u64(), Some((1 << 10) + 1), "{v}");
        assert_eq!(loaded(&registry), vec![1 << 20]);
        assert_eq!(PayStore::open(PayStore::path_in(&dir)).unwrap().get(id).unwrap().refund_msat, Some(1 << 20));
        // The next taps load the rest, and never more than is owed: the last msat is dust.
        for _ in 0..4 {
            let (s, v) = refund(addr, id, Some(card.clone())).await;
            assert_eq!(s, 200, "{v}");
            let sum: u64 = loaded(&registry).iter().sum();
            assert!(sum <= price, "{sum} loaded for a price of {price}");
            assert_eq!(v["refund_msat"].as_u64(), Some(sum), "{v}");
            if v["state"] == "refunded" {
                break;
            }
            assert_eq!((v["state"].as_str(), v["refund_owed_msat"].as_u64()), (Some("pay_failed"), Some(price - sum)), "{v}");
        }
        let rec = pays.get(id).unwrap();
        let sum: u64 = loaded(&registry).iter().sum();
        assert_eq!((rec.state, rec.refund_msat), (PayState::Refunded, Some(sum)));
        assert!(price - sum < min, "{sum} of {price}");
        assert_eq!(loaded(&registry), vec![1 << 20, 1 << 10]);
        let (s, v) = call(addr, "GET", &format!("/v1/pay/{id}"), None).await;
        assert_eq!(
            (s, v["refund_dust_msat"].as_u64(), &v["refund_txid"]),
            (200, Some(1), &registry.loads.lock().unwrap()[1]["txid"]),
            "{v}"
        );
        // Done: no further load, with or without the card.
        let (s, v) = refund(addr, id, Some(card)).await;
        assert_eq!((s, v["state"].as_str()), (200, Some("refunded")), "{v}");
        assert_eq!(registry.loads.lock().unwrap().len(), 2);
        // The journal kept both loads, each taken by the federation.
        let journal: Value = serde_json::from_slice(&std::fs::read(dir.join(JOURNAL_FILE)).unwrap()).unwrap();
        let kept: Vec<&Value> = journal["kept"].as_array().unwrap().iter().filter(|k| k["tag"] == format!("pay-refund:{id}")).collect();
        assert_eq!(kept.iter().map(|k| k["accepted"].as_bool()).collect::<Vec<_>>(), vec![Some(true), Some(true)], "{journal}");
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refund_load_the_federation_never_took_is_kept_as_not_taken_and_loaded_once_again() -> anyhow::Result<()> {
    let (f, actor) = start(true).await?;
    let id = put_pay(&f.pays, "never", PayState::PayFailed, PRICE, Some(&f.short_id));
    let want = refund_for(&f.wallet, PRICE);
    let before = f.wallet.balance_msat().await?;
    let (addr, pays, registry, card, dir, wallet) =
        (f.addr, f.pays.clone(), f.registry.clone(), f.card.clone(), f.dir.path().to_owned(), f.wallet.clone());
    with_actor(actor, async move {
        // The registry does not answer the load: the build is dropped, never sent, and the load
        // stays in the journal, its outcome unknown to the till. The pay stays refunding.
        registry.fail_loads.store(1, Ordering::SeqCst);
        let (s, v) = refund(addr, id, Some(card.clone())).await;
        assert_ne!(s, 200, "{v}");
        assert_eq!(v["state"], "refunding", "{v}");
        assert_eq!(pays.get(id).unwrap().state, PayState::Refunding);
        let tag = format!("pay-refund:{id}");
        let journal: Value = serde_json::from_slice(&std::fs::read(dir.join(JOURNAL_FILE)).unwrap()).unwrap();
        let pending: Vec<&Value> = journal["loads"].as_array().unwrap().iter().filter(|l| l["tag"] == tag).collect();
        assert_eq!(pending.len(), 1, "{journal}");
        let first = pending[0]["txid"].clone();
        assert!(registry.loads.lock().unwrap().is_empty());

        // The next call finishes that load as not taken (it is not the refund), then loads once.
        let (s, v) = refund(addr, id, Some(card)).await;
        assert_eq!((s, v["state"].as_str(), v["refund_msat"].as_u64()), (200, Some("refunded"), Some(want)), "{v}");
        assert_ne!(v["refund_txid"], first);
        let journal: Value = serde_json::from_slice(&std::fs::read(dir.join(JOURNAL_FILE)).unwrap()).unwrap();
        assert!(journal["loads"].as_array().unwrap().is_empty(), "{journal}");
        let kept: Vec<&Value> = journal["kept"].as_array().unwrap().iter().filter(|k| k["tag"] == tag).collect();
        assert_eq!(
            kept.iter().map(|k| (k["txid"].clone(), k["accepted"].as_bool())).collect::<Vec<_>>(),
            vec![(first, Some(false)), (v["refund_txid"].clone(), Some(true))],
            "{journal}"
        );
        // Only one load reached the registry and the federation.
        assert_eq!(loaded(&registry), vec![want]);
        // The till's wallet paid for one refund, not two (allowing for the federation's fees and
        // change still being reissued).
        let mut spent = 0;
        for _ in 0..60 {
            spent = before - wallet.balance_msat().await.unwrap();
            if spent < want + want / 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!((want..want + want / 2).contains(&spent), "the wallet spent {spent} msat for a refund of {want}");
    })
    .await;
    Ok(())
}
