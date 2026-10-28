//! P0: the provisioning endpoints (`precheck`, `keySetup`, `issue`).
//!
//! The plan's done-line for P0 is these four behaviours, and each has a test named for it:
//!
//! * precheck on a blank, an installed and a foreign card;
//! * keySetup is idempotent;
//! * issue replays a `client_ref`;
//! * issue refuses a card already holding notes.
//!
//! They run against the same in-process federation fixture as the till tests, with [`SoftCard`]
//! standing in for the applet — including its PIN state machine, so `keySetup` is exercised through
//! `SET_PIN`/`VERIFY_PIN` and not against a stub that always says `9000`.
//!
//! `SoftCard` is not silicon: nothing here exercises a real reader, a real applet, or the relay.
//! What it does prove is that the endpoints' decisions come from card and state, in the order the
//! plan requires.

use std::cell::RefCell;
use std::rc::Rc;
use std::future::Future;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use fedimint_card_host_card::{Card, SoftCard};
use fedimint_card_sidecar::glue::CardWallet;
use fedimint_card_sidecar::sales::SalesStore;
use fedimint_card_sidecar::state::StateStore;
use fedimint_card_sidecar::till;
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
use serde_json::Value;
use tower::ServiceExt as _;

struct Fx {
    _fed: FederationTest,
    cw: CardWallet,
    _dir: tempfile::TempDir,
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

/// A wallet with `funds` behind it and a card the tests own. `card` is left as given, so a test
/// can hand in a blank, an installed or a pre-keyed card.
async fn fx_with(card: Box<dyn Card>, funds: Amount) -> anyhow::Result<Fx> {
    assert!(
        std::env::var_os("FM_TEST_USE_REAL_DAEMONS").is_none(),
        "FM_TEST_USE_REAL_DAEMONS is set: these tests need the in-process fake bitcoin backend"
    );
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit).with_module(DummyClientInit, DummyInit);
    let fed = tokio::time::timeout(Duration::from_secs(60), fixtures.new_fed_builder(0).disable_mint_fees().build())
        .await?;
    let client = fed.new_client().await;
    fund(&client, funds).await?;
    let wallet = Wallet::from_client(client).await?;
    let dir = tempfile::tempdir()?;
    let store = StateStore::open(dir.path().join("card_state.json"))?;
    Ok(Fx { _fed: fed, cw: CardWallet::new(wallet, store, card), _dir: dir })
}

/// A card shared between the wallet under test and the endpoint's opener, as `till serve` opens a
/// fresh one per job.
struct Shared(Rc<RefCell<SoftCard>>);

impl Card for Shared {
    fn gen_key(&mut self, slot: u8) -> anyhow::Result<fedimint_core::secp256k1::XOnlyPublicKey> {
        self.0.borrow_mut().gen_key(slot)
    }
    fn gen_key_overwrite(&mut self, slot: u8) -> anyhow::Result<fedimint_core::secp256k1::XOnlyPublicKey> {
        self.0.borrow_mut().gen_key_overwrite(slot)
    }
    fn get_pub(&mut self, slot: u8) -> anyhow::Result<fedimint_core::secp256k1::XOnlyPublicKey> {
        self.0.borrow_mut().get_pub(slot)
    }
    fn sign(
        &mut self,
        slot: u8,
        msg32: &[u8; 32],
    ) -> anyhow::Result<fedimint_core::secp256k1::schnorr::Signature> {
        self.0.borrow_mut().sign(slot, msg32)
    }
    fn apdu(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.0.borrow_mut().apdu(apdu)
    }
}

/// Build the till handle over `f`, and return it with the actor future the caller must poll.
/// The router is built per call (`till::router` clones only the handle), so one actor can serve
/// several requests in a test.
async fn serve(
    f: Fx,
    card: Rc<RefCell<SoftCard>>,
) -> (till::TillHandle, impl Future<Output = ()>) {
    let sales = SalesStore::open(f._dir.path().join("sales.json")).unwrap();
    let opener_card = card;
    till::new(
        f.cw,
        "http://127.0.0.1:1".into(),
        sales,
        Box::new(move || Ok(Box::new(Shared(opener_card.clone())) as Box<dyn Card>)),
    )
    .await
}

async fn call(app: axum::Router, method: &str, path: &str, body: &str) -> (u16, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Run one request with the actor polled alongside it. The actor future is not `Send`, so it is
/// driven on this task rather than spawned. Takes the `Pin<&mut _>` so one actor serves many calls.
macro_rules! call_with_actor {
    ($actor:expr, $h:expr, $method:expr, $path:expr, $body:expr) => {{
        let app = till::router(($h).clone());
        let fut = async move { call(app, $method, $path, $body).await };
        tokio::select! {
            r = fut => r,
            _ = &mut *$actor => panic!("actor ended early"),
        }
    }};
}

/// A blank card: applet to select, no keys, no PIN.
fn blank_card() -> (Rc<RefCell<SoftCard>>, Box<dyn Card>) {
    let card = Rc::new(RefCell::new(SoftCard::new()));
    (card.clone(), Box::new(Shared(card)))
}

/// An "installed" card as a provisioner sees it: keys in every slot, recorded by this wallet.
/// Generated through the wallet so the state file knows them.
fn installed_card(f: &mut Fx) {
    f.cw.card_setup(false).unwrap();
}

// ------------------------------------------------------------------ precheck

#[tokio::test(flavor = "multi_thread")]
async fn precheck_on_a_blank_card_says_blank_and_safe() -> anyhow::Result<()> {
    let (shared, card) = blank_card();
    let mut f = fx_with(card, Amount::from_sats(10_000)).await?;
    // A card whose applet is not installed at all: SELECT answers 6A82.
    shared.borrow_mut().visit_blank_applet();
    let (h, actor) = serve(f, shared.clone()).await;
    let mut actor = Box::pin(actor);

    let (code, body) = call_with_actor!(&mut actor, h, "POST", "/v1/cards/precheck", "");
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["kind"], "blank");
    assert_eq!(body["applet_present"], false, "no applet selected: {body}");
    assert_eq!(body["keyed_slots"], serde_json::json!([]));
    assert_eq!(body["live_msat"], 0);
    assert_eq!(body["safe_to_replace"], true);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn precheck_on_an_installed_card_reports_keys_and_refuses_replace() -> anyhow::Result<()> {
    let (shared, card) = blank_card();
    let mut f = fx_with(card, Amount::from_sats(10_000)).await?;
    installed_card(&mut f);
    let keys: Vec<u8> = (0..8).collect();
    let (h, actor) = serve(f, shared.clone()).await;
    let mut actor = Box::pin(actor);

    let (code, body) = call_with_actor!(&mut actor, h, "POST", "/v1/cards/precheck", "");
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["kind"], "installed");
    assert_eq!(body["applet_present"], true, "{body}");
    assert_eq!(body["keyed_slots"], serde_json::json!(keys));
    assert_eq!(body["safe_to_replace"], false, "a keyed card must ask before a sweep");
    assert_eq!(body["live_msat"], 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn key_setup_is_idempotent() -> anyhow::Result<()> {
    let (shared, card) = blank_card();
    let f = fx_with(card, Amount::from_sats(10_000)).await?;
    let (h, actor) = serve(f, shared.clone()).await;
    let mut actor = Box::pin(actor);

    let (code, first) = call_with_actor!(&mut actor, h, "POST", "/v1/cards/keySetup", r#"{"pin":"1234"}"#);
    assert_eq!(code, 200, "{first}");
    assert_eq!(first["keys"].as_array().unwrap().len(), 8);
    assert_eq!(first["pin_set"], true, "{first}");
    assert_eq!(first["pin"], "set_now");
    assert_eq!(first["generated"], serde_json::json!([0, 1, 2, 3, 4, 5, 6, 7]));
    let short = first["short_id"].as_str().unwrap().to_string();
    assert_eq!(short.len(), 8);

    // Second run: same keys, same short id, the PIN reported as already there and still verifying.
    let (code, second) = call_with_actor!(&mut actor, h, "POST", "/v1/cards/keySetup", r#"{"pin":"1234"}"#);
    assert_eq!(code, 200, "{second}");
    assert_eq!(second["short_id"], short, "the identity changed on a repeat");
    assert_eq!(second["keys"], first["keys"]);
    assert_eq!(second["generated"], serde_json::json!([]), "a repeat generated nothing");
    assert_eq!(second["pin"], "set");
    assert_eq!(second["pin_set"], true);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn key_setup_refuses_a_foreign_card() -> anyhow::Result<()> {
    let (shared, card) = blank_card();
    let mut f = fx_with(card, Amount::from_sats(10_000)).await?;
    // Keys this wallet never produced: as if another machine had set the card up.
    shared.borrow_mut().install_foreign_keys();
    let (h, actor) = serve(f, shared.clone()).await;
    let mut actor = Box::pin(actor);

    let (code, body) = call_with_actor!(&mut actor, h, "POST", "/v1/cards/keySetup", r#"{"pin":"1234"}"#);
    assert_eq!(code, 502, "{body}");
    assert_eq!(body["error"], "card_error");
    assert!(
        body["detail"].as_str().unwrap().contains("did not produce"),
        "the refusal must say why: {body}"
    );
    Ok(())
}

// ------------------------------------------------------------------ issue

#[tokio::test(flavor = "multi_thread")]
async fn issue_replays_a_client_ref() -> anyhow::Result<()> {
    let (shared, card) = blank_card();
    let mut f = fx_with(card, Amount::from_sats(100_000)).await?;
    installed_card(&mut f);
    let (h, actor) = serve(f, shared.clone()).await;
    let mut actor = Box::pin(actor);

    let body = r#"{"amounts_msat":[70000],"client_ref":"batch-1-card-1"}"#;
    let (code, first) = call_with_actor!(&mut actor, h, "POST", "/v1/cards/issue", body);
    assert_eq!(code, 200, "{first}");
    assert_eq!(first["replayed"], false);
    let txid = first["txid"].as_str().unwrap().to_string();
    assert!(!txid.is_empty(), "{first}");
    let notes = first["notes"].clone();

    let (code, second) = call_with_actor!(&mut actor, h, "POST", "/v1/cards/issue", body);
    assert_eq!(code, 200, "{second}");
    assert_eq!(second["replayed"], true, "a repeat must be answered from state");
    assert_eq!(second["txid"], txid);
    assert_eq!(second["notes"], notes);

    // The same ref with a different body is a different call, not a replay.
    let (code, third) =
        call_with_actor!(&mut actor, h, "POST", "/v1/cards/issue", r#"{"amounts_msat":[1000],"client_ref":"batch-1-card-1"}"#);
    assert_eq!(code, 409, "{third}");
    assert_eq!(third["error"], "ref_mismatch");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn issue_refuses_a_card_that_already_holds_notes() -> anyhow::Result<()> {
    let (shared, card) = blank_card();
    let mut f = fx_with(card, Amount::from_sats(100_000)).await?;
    installed_card(&mut f);
    // Put notes on it the way an earlier batch would have, without a client_ref.
    f.cw.topup(&[1 << 16]).await?;
    let (h, actor) = serve(f, shared.clone()).await;
    let mut actor = Box::pin(actor);

    let (code, body) =
        call_with_actor!(&mut actor, h, "POST", "/v1/cards/issue", r#"{"amounts_msat":[1000],"client_ref":"second-tap"}"#);
    assert_eq!(code, 409, "{body}");
    assert_eq!(body["error"], "card_holds_notes");
    assert!(
        body["detail"].as_str().unwrap().contains("double its value"),
        "the refusal must say the risk: {body}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn issue_rejects_a_bad_body_and_a_pin_outside_the_applets_bounds() -> anyhow::Result<()> {
    let (shared, card) = blank_card();
    let f = fx_with(card, Amount::from_sats(10_000)).await?;
    let (h, actor) = serve(f, shared.clone()).await;
    let mut actor = Box::pin(actor);

    let (code, body) = call_with_actor!(&mut actor, h, "POST", "/v1/cards/issue", r#"{"amounts_msat":[]}"#);
    assert_eq!(code, 400);
    assert_eq!(body["error"], "bad_request");

    // 3 digits and 9 digits are both outside PIN_MIN..PIN_MAX: refused before the card sees them,
    // so a malformed PIN can never spend a try.
    for pin in ["123", "123456789"] {
        let b = format!(r#"{{"pin":"{pin}"}}"#);
        let (code, body) = call_with_actor!(&mut actor, h, "POST", "/v1/cards/keySetup", &b);
        assert_eq!(code, 400, "pin {pin}: {body}");
        assert_eq!(body["error"], "bad_request");
    }
    Ok(())
}
