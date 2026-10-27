//! The till service: charge core, error classification, relay failures and the HTTP router.
//!
//! The federation fixture is copied from `card_wallet.rs`.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::rc::Rc;
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use fedimint_card_host_card::{BridgeError, Card, SoftCard, StatusWord, open_from_env};
use fedimint_card_sidecar::glue::{CardWallet, SpendFault};
use fedimint_card_sidecar::plan::PlanError;
use fedimint_card_sidecar::sales::SalesStore;
use fedimint_card_sidecar::state::StateStore;
use fedimint_card_sidecar::till::{self, Class, charge_with, classify, open_relay_card};
use fedimint_card_sidecar::wallet::Wallet;
use fedimint_client::ClientHandleArc;
use fedimint_client::transaction::TransactionBuilder;
use fedimint_core::core::OperationId;
use fedimint_core::secp256k1::XOnlyPublicKey;
use fedimint_core::secp256k1::schnorr::Signature;
use fedimint_core::Amount;
use fedimint_dummy_client::{DummyClientInit, DummyClientModule};
use fedimint_dummy_server::DummyInit;
use fedimint_mint_client::MintClientInit;
use fedimint_mint_server::MintInit;
use fedimint_testing::federation::FederationTest;
use fedimint_testing::fixtures::Fixtures;
use serde_json::Value;
use tower::ServiceExt as _;

struct Fx {
    /// Keep alive: dropping it stops the guardians.
    fed: FederationTest,
    cw: CardWallet,
    _dir: tempfile::TempDir,
}

/// Give `client` ecash out of thin air through the dummy module.
async fn fund(client: &ClientHandleArc, amount: Amount) -> anyhow::Result<()> {
    let dummy = client.get_first_module::<DummyClientModule>()?;
    let op = OperationId::new_random();
    let range = client
        .finalize_and_submit_transaction(op, "fund", |_| (), TransactionBuilder::new().with_inputs(dummy.create_input(amount)))
        .await?;
    client.await_primary_bitcoin_module_outputs(op, range.into_iter().collect()).await?;
    Ok(())
}

async fn fx(fees: bool, funds: Amount) -> anyhow::Result<Fx> {
    assert!(
        std::env::var_os("FM_TEST_USE_REAL_DAEMONS").is_none(),
        "FM_TEST_USE_REAL_DAEMONS is set: these tests need the in-process fake bitcoin backend"
    );
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit).with_module(DummyClientInit, DummyInit);
    let mut builder = fixtures.new_fed_builder(0);
    if !fees {
        builder = builder.disable_mint_fees();
    }
    let fed = tokio::time::timeout(Duration::from_secs(60), builder.build()).await?;
    let client = fed.new_client().await;
    fund(&client, funds).await?;
    let wallet = Wallet::from_client(client).await?;
    let dir = tempfile::tempdir()?;
    let store = StateStore::open(dir.path().join("card_state.json"))?;
    let card = open_from_env()?;
    let mut cw = CardWallet::new(wallet, store, card);
    // A persistent card (pcsc, bridge) already holds keys from earlier runs: re-key it so it
    // behaves like a new one.
    let soft = std::env::var("CARD_BACKEND").map(|b| b == "soft").unwrap_or(true);
    cw.card_setup(!soft)?;
    Ok(Fx { fed, cw, _dir: dir })
}


fn sales_of(f: &Fx) -> Mutex<SalesStore> {
    Mutex::new(SalesStore::open(f._dir.path().join("sales.json")).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn charge_happy_path_records_a_sale() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    f.cw.topup(&[1 << 16, 1 << 14]).await?;
    let sales = sales_of(&f);
    let r = charge_with(&mut f.cw, 50_000, Some("ref-1".into()), &sales).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    let b = &r.body;
    assert_eq!(b["ok"], true);
    assert_eq!(b["sale_id"], 1);
    assert_eq!(b["price_msat"], 50_000);
    assert!(b["overpaid_msat"].as_u64().unwrap() <= fedimint_card_sidecar::plan::MAX_TILL_OVERPAY);
    assert_eq!(b["txid"].as_str().unwrap().len(), 64);
    assert!(b["fees_msat"].as_u64().unwrap() >= 200);
    let ins: u64 = b["inputs"].as_array().unwrap().iter().map(|i| i["msat"].as_u64().unwrap()).sum();
    let chg: u64 = b["change"].as_array().unwrap().iter().map(|i| i["msat"].as_u64().unwrap()).sum();
    assert_eq!(
        ins,
        b["price_msat"].as_u64().unwrap() + b["overpaid_msat"].as_u64().unwrap() + chg + b["fees_msat"].as_u64().unwrap()
    );
    assert_eq!(b["collection_pending"], false);
    assert!(b["received_msat"].as_u64().unwrap() > 0);
    assert!(b["warnings"].as_array().unwrap().is_empty());
    assert!(!b.to_string().contains("OOBNotes") && b.get("ecash").is_none());

    let path = f._dir.path().join("sales.json");
    let file: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(file["sales"].as_array().unwrap().len(), 1);
    assert_eq!(file["sales"][0]["client_ref"], "ref-1");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_prices_are_rejected_before_the_card_is_touched() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(1000)).await?;
    let sales = sales_of(&f);
    for p in [0, (1u64 << 40) + 1] {
        let r = charge_with(&mut f.cw, p, None, &sales).await;
        assert_eq!((r.status, r.body["error"].as_str()), (400, Some("bad_request")));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn insufficient_leaves_card_state_unchanged() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    f.cw.topup(&[1 << 14]).await?;
    let before = std::fs::read(f._dir.path().join("card_state.json"))?;
    let sales = sales_of(&f);
    let r = charge_with(&mut f.cw, 5_000_000, None, &sales).await;
    assert_eq!((r.status, r.body["error"].as_str()), (402, Some("insufficient")), "{:?}", r.body);
    assert_eq!(r.body["ok"], false);
    assert!(r.body["detail"].as_str().unwrap().contains("insufficient"));
    assert_eq!(std::fs::read(f._dir.path().join("card_state.json"))?, before);
    assert!(sales.lock().unwrap().file.sales.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sales_persist_and_a_repeated_client_ref_does_not_charge_twice() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    f.cw.topup(&[1 << 16, 1 << 14]).await?;
    let sales = sales_of(&f);
    let first = charge_with(&mut f.cw, 20_000, Some("same".into()), &sales).await;
    assert_eq!(first.status, 200, "{:?}", first.body);
    let live = f.cw.store.state.live_total_msat();

    let again = charge_with(&mut f.cw, 20_000, Some("same".into()), &sales).await;
    assert_eq!(again.status, 200);
    assert_eq!(again.body["sale_id"], first.body["sale_id"]);
    assert_eq!(again.body["txid"], first.body["txid"]);
    assert_eq!(f.cw.store.state.live_total_msat(), live, "the card must not be charged again");
    assert_eq!(sales.lock().unwrap().file.sales.len(), 1);

    // A second, different sale gets the next id; both survive a reopen.
    let second = charge_with(&mut f.cw, 10_000, Some("other".into()), &sales).await;
    assert_eq!((second.status, &second.body["sale_id"]), (200, &Value::from(2)), "{:?}", second.body);
    let reopened = SalesStore::open(f._dir.path().join("sales.json"))?;
    assert_eq!(reopened.file.sales.len(), 2);
    assert_eq!(reopened.file.next_id, 3);
    assert_eq!(reopened.find_ref("same").unwrap().txid, first.body["txid"].as_str().unwrap());
    Ok(())
}

#[test]
fn classify_pins_codes_and_statuses() {
    let c = |e: anyhow::Error| classify(&e);
    let k = |status, code| Class { status, code };
    let wrap = |e: anyhow::Error| e.context("outer").context("outermost");
    assert_eq!(c(wrap(BridgeError::NoCard("POST /apdu: HTTP 503: x".into()).into())), k(409, "no_card"));
    assert_eq!(c(wrap(BridgeError::Unreachable("POST /apdu: refused".into()).into())), k(502, "relay_unreachable"));
    assert_eq!(c(wrap(BridgeError::Http(500, "POST /apdu: HTTP 500: x".into()).into())), k(502, "card_error"));
    assert_eq!(c(wrap(StatusWord(0x6985).into())), k(502, "card_error"));
    assert_eq!(c(wrap(PlanError::Insufficient { have: 1, need: 2 }.into())), k(402, "insufficient"));
    assert_eq!(c(wrap(PlanError::NoExactPlan { price: 1, free_slots: 0 }.into())), k(422, "no_exact_plan"));
    assert_eq!(c(wrap(PlanError::TooManyNotes(9).into())), k(422, "no_exact_plan"));
    assert_eq!(c(wrap(SpendFault::Declined("d".into()).into())), k(402, "declined"));
    assert_eq!(c(wrap(SpendFault::Undecided("u".into()).into())), k(202, "undecided"));
    assert_eq!(c(wrap(SpendFault::Unresolved("r".into()).into())), k(503, "recovering"));
    assert_eq!(c(anyhow::anyhow!("something else")), k(500, "internal"));
    // Display text is unchanged from the old anyhow strings.
    assert_eq!(SpendFault::Declined("federation rejected the spend: e".into()).to_string(), "federation rejected the spend: e");
}

/// A relay that answers every request with `status`.
fn mock_relay(status: &'static str, body: &'static str) -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    std::thread::spawn(move || {
        for s in l.incoming() {
            let mut s = s.unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        }
    });
    url
}

fn closed_port_url() -> String {
    let p = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    format!("http://127.0.0.1:{p}")
}

#[test]
fn relay_failures_map_to_no_card_and_relay_unreachable() {
    let err = open_relay_card(&mock_relay("503 Service Unavailable", "no card")).err().unwrap();
    assert_eq!(classify(&err), Class { status: 409, code: "no_card" }, "{err:#}");
    let err = open_relay_card(&closed_port_url()).err().unwrap();
    assert_eq!(classify(&err), Class { status: 502, code: "relay_unreachable" }, "{err:#}");
}

/// A card shared with the test, whose signature takes a while.
struct SlowCard(Rc<RefCell<SoftCard>>);
impl Card for SlowCard {
    fn gen_key(&mut self, s: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.0.borrow_mut().gen_key(s)
    }
    fn gen_key_overwrite(&mut self, s: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.0.borrow_mut().gen_key_overwrite(s)
    }
    fn get_pub(&mut self, s: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.0.borrow_mut().get_pub(s)
    }
    fn sign(&mut self, s: u8, m: &[u8; 32]) -> anyhow::Result<Signature> {
        std::thread::sleep(Duration::from_millis(1500));
        self.0.borrow_mut().sign(s, m)
    }
}

async fn call(app: axum::Router, method: &str, uri: &str, body: &str) -> (u16, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn router_serves_one_charge_at_a_time_and_reports_status() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    // Replace the card with one shared with the opener below.
    let shared = Rc::new(RefCell::new(SoftCard::new()));
    f.cw.card = Box::new(SlowCard(shared.clone()));
    f.cw.card_setup(false)?;
    f.cw.topup(&[1 << 16, 1 << 14]).await?;
    let sales = SalesStore::open(f._dir.path().join("sales.json"))?;
    let opener_card = shared.clone();
    let dead_relay = closed_port_url();
    let (handle, actor) = till::new(
        f.cw,
        dead_relay,
        sales,
        Box::new(move || Ok(Box::new(SlowCard(opener_card.clone())) as Box<dyn Card>)),
    )
    .await;
    let app = till::router(handle);

    let a = tokio::spawn(call(app.clone(), "POST", "/v1/charge", r#"{"price_msat":20000,"client_ref":"a"}"#));
    let a2 = app.clone();
    let rest = async move {
        // Let the first charge get admitted and into the (slow) card call.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let b = tokio::spawn(call(a2.clone(), "POST", "/v1/charge", r#"{"price_msat":10000,"client_ref":"b"}"#)).await.unwrap();
        let bal = tokio::spawn(call(a2.clone(), "POST", "/v1/card/balance", "")).await.unwrap();
        let st = tokio::spawn(call(a2.clone(), "GET", "/v1/status", "")).await.unwrap();
        let first = a.await.unwrap();
        (b, bal, st, first)
    };
    // The actor future is not Send: poll it here, on the test's own task, next to the requests.
    let rest = tokio::spawn(rest);
    let mut actor = Box::pin(actor);
    let (b, bal, st, first) = tokio::select! {
        r = rest => r.unwrap(),
        _ = &mut actor => panic!("actor ended early"),
    };
    assert_eq!(b.0, 409, "{:?}", b.1);
    assert_eq!(b.1["error"], "busy");
    assert_eq!(bal.0, 409);
    assert_eq!(first.0, 200, "{:?}", first.1);

    // 7: /v1/status with a dead relay.
    assert_eq!(st.0, 200);
    assert_eq!(st.1["card_relay_ok"], false);
    assert_eq!(st.1["busy"], true);
    assert!(st.1["federation_id"].as_str().unwrap().len() >= 16);
    assert!(st.1["wallet_balance_msat"].as_u64().is_some());
    assert!(st.1.get("federation_name").is_some());

    // Validation and malformed bodies.
    assert_eq!(call(app.clone(), "POST", "/v1/charge", "{}").await.0, 400);
    assert_eq!(call(app.clone(), "POST", "/v1/charge", "not json").await.0, 400);
    assert_eq!(call(app.clone(), "GET", "/v1/health", "").await, (200, serde_json::json!({"ok": true})));

    let (code, s) = call(app, "GET", "/v1/sales", "").await;
    assert_eq!(code, 200);
    assert_eq!(s["count"], 1);
    assert_eq!(s["total_price_msat"], 20_000);
    assert_eq!(s["sales"][0]["client_ref"], "a");
    Ok(())
}
