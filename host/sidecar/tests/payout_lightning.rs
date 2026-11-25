//! A Lightning payout end to end on an in-process federation with an lnv2 test gateway (the
//! gateway of `wallet_pay.rs`): the engine over the till's own wallet ([`TillBackend`]), with a
//! Lightning address service injected in place of the network whose invoice commits to the
//! address's metadata by its description hash.
//!
//! - refused, nothing funded: an invoice whose description hash is not the metadata's, and a
//!   gateway whose fee is above the cap;
//! - paid: the wallet funds the payment and the gateway pays it; the record holds the gateway's
//!   fee and the federation's, and the balance dropped by the payment;
//! - a replay of the request and a restart (a new engine over the same store) fund nothing more.
//!
//! Nothing here reaches a real federation, gateway or service.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use fedimint_api_client::api::{FederationApiExt as _, ServerError};
use fedimint_card_sidecar::payout::backend::TillBackend;
use fedimint_card_sidecar::payout::engine::{Engine, ManualLightning};
use fedimint_card_sidecar::payout::net::{Fetch, FetchError};
use fedimint_card_sidecar::payout::store::{Mode, Origin, PayoutStore, Record, Settings, State};
use fedimint_card_sidecar::till::Reply;
use fedimint_card_sidecar::wallet::{Wallet, WalletPay};
use fedimint_client::ClientHandleArc;
use fedimint_client::transaction::TransactionBuilder;
use fedimint_core::bitcoin::hashes::{Hash as _, sha256};
use fedimint_core::config::FederationId;
use fedimint_core::core::{ModuleInstanceId, OperationId};
use fedimint_core::module::{ApiAuth, ApiRequestErased};
use fedimint_core::secp256k1::rand::rngs::OsRng;
use fedimint_core::secp256k1::schnorr::Signature;
use fedimint_core::secp256k1::{Keypair, SECP256K1, SecretKey};
use fedimint_core::util::SafeUrl;
use fedimint_core::{Amount, OutPoint};
use fedimint_dummy_client::{DummyClientInit, DummyClientModule};
use fedimint_dummy_server::DummyInit;
use fedimint_lnv2_client::common::contracts::{IncomingContract, OutgoingContract};
use fedimint_lnv2_client::common::endpoint_constants::ADD_GATEWAY_ENDPOINT;
use fedimint_lnv2_client::common::gateway_api::{GatewayConnection, PaymentFee, RoutingInfo};
use fedimint_lnv2_client::common::{Bolt11InvoiceDescription, LightningInvoice};
use fedimint_mint_client::MintClientInit;
use fedimint_mint_server::MintInit;
use fedimint_testing::federation::FederationTest;
use fedimint_testing::fixtures::Fixtures;
use futures::future::BoxFuture;
use lightning_invoice::{Bolt11Invoice, Currency, InvoiceBuilder, PaymentSecret};
use serde_json::{Value, json};

const PREIMAGE: [u8; 32] = [1; 32];
const AMOUNT_SAT: u64 = 10_000;
const WAIT: Duration = Duration::from_secs(60);
const META: &str = r#"[["text/plain","Pay to shop"],["text/identifier","shop@example.com"]]"#;

fn gateway_url() -> SafeUrl {
    SafeUrl::parse("https://gateway.test").expect("valid url")
}

fn gateway_fee_msat(amount_msat: u64) -> u64 {
    PaymentFee::TRANSACTION_FEE_DEFAULT.fee(amount_msat).msats
}

/// Pays every invoice with `PREIMAGE`.
#[derive(Debug)]
struct TestGateway {
    keypair: Keypair,
    preimage: Arc<Mutex<[u8; 32]>>,
}

#[async_trait::async_trait]
impl GatewayConnection for TestGateway {
    async fn routing_info(&self, _gateway_api: SafeUrl, _federation_id: &FederationId) -> Result<Option<RoutingInfo>, ServerError> {
        Ok(Some(RoutingInfo {
            lightning_public_key: self.keypair.public_key(),
            lightning_alias: Some("test-gateway".into()),
            module_public_key: self.keypair.public_key(),
            send_fee_default: PaymentFee::TRANSACTION_FEE_DEFAULT,
            send_fee_minimum: PaymentFee::TRANSACTION_FEE_DEFAULT,
            expiration_delta_default: 500,
            expiration_delta_minimum: 144,
            receive_fee: PaymentFee::TRANSACTION_FEE_DEFAULT,
            receive_enabled: false,
        }))
    }

    async fn bolt11_invoice(
        &self,
        _gateway_api: SafeUrl,
        _federation_id: FederationId,
        _contract: IncomingContract,
        _amount: Amount,
        _description: Bolt11InvoiceDescription,
        _expiry_secs: u32,
    ) -> Result<Bolt11Invoice, ServerError> {
        Err(ServerError::InvalidRequest("the test gateway does not receive".into()))
    }

    async fn send_payment(
        &self,
        _gateway_api: SafeUrl,
        _federation_id: FederationId,
        _outpoint: OutPoint,
        _contract: OutgoingContract,
        _invoice: LightningInvoice,
        _auth: Signature,
    ) -> Result<Result<[u8; 32], Signature>, ServerError> {
        Ok(Ok(*self.preimage.lock().unwrap()))
    }
}

/// The address's service, injected: its well-known document, and an invoice for the amount the
/// callback is asked for, committing to `hash` (the metadata's, unless a test says otherwise).
struct Service {
    hash: Mutex<[u8; 32]>,
    payment_hash: Mutex<sha256::Hash>,
    issued: Mutex<Vec<String>>,
}

impl Fetch for Service {
    fn get_json(&self, url: reqwest::Url) -> BoxFuture<'_, Result<Value, FetchError>> {
        Box::pin(async move {
            if url.path() == "/.well-known/lnurlp/shop" {
                return Ok(json!({
                    "tag": "payRequest", "callback": "https://example.com/lnurlp/shop/cb",
                    "minSendable": 1000, "maxSendable": 1_000_000_000u64, "metadata": META,
                }));
            }
            let amount: u64 = url.query_pairs().find(|(k, _)| k == "amount").and_then(|(_, v)| v.parse().ok()).expect("an amount");
            let key = SecretKey::new(&mut OsRng);
            let pr = InvoiceBuilder::new(Currency::Regtest)
                .description_hash(sha256::Hash::from_byte_array(*self.hash.lock().unwrap()))
                .payment_hash(*self.payment_hash.lock().unwrap())
                .payment_secret(PaymentSecret(rand::random()))
                .current_timestamp()
                .min_final_cltv_expiry_delta(0)
                .amount_milli_satoshis(amount)
                .build_signed(|m| SECP256K1.sign_ecdsa_recoverable(m, &key))
                .expect("valid invoice")
                .to_string();
            self.issued.lock().unwrap().push(pr.clone());
            Ok(json!({"pr": pr}))
        })
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

async fn add_gateway(fed: &FederationTest, lnv2: ModuleInstanceId) -> anyhow::Result<()> {
    for peer in fed.online_peer_ids() {
        let api = fed.new_admin_api(peer).await?.with_module(lnv2);
        let _: bool = api.request_admin(ADD_GATEWAY_ENDPOINT, ApiRequestErased::new(gateway_url()), ApiAuth::new("pass".into())).await?;
    }
    Ok(())
}

/// The refusal's body; a payout that went through is a test failure.
fn err(r: Result<Record, Reply>) -> Value {
    match r {
        Ok(rec) => panic!("expected a refusal, the payout is {:?}", rec.state),
        Err(e) => e.body,
    }
}

fn settings(max_fee_ppm: u64) -> Settings {
    Settings { mode: Mode::Both, reserve_sat: 1_000, threshold_sat: 1, max_fee_sat: 1_000, max_fee_ppm, interval_minutes: 1_440 }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_engine_pays_a_lightning_address_through_the_gateway_and_refuses_with_nothing_funded() -> anyhow::Result<()> {
    assert!(std::env::var_os("FM_TEST_USE_REAL_DAEMONS").is_none(), "these tests need the in-process fake bitcoin backend");
    let keypair = SecretKey::from_slice(&[1; 32]).expect("valid key").keypair(SECP256K1);
    let preimage = Arc::new(Mutex::new(PREIMAGE));
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit).with_module(DummyClientInit, DummyInit).with_module(
        fedimint_lnv2_client::LightningClientInit {
            gateway_conn: Some(Arc::new(TestGateway { keypair, preimage: preimage.clone() })),
            custom_meta_fn: Arc::new(|| serde_json::Value::Null),
        },
        fedimint_lnv2_server::LightningInit,
    );
    let fed = tokio::time::timeout(Duration::from_secs(60), fixtures.new_fed_builder(0).build()).await?;
    let client = fed.new_client().await;
    fund(&client, Amount::from_sats(100_000)).await?;
    let lnv2 = client.get_first_module::<fedimint_lnv2_client::LightningClientModule>()?.id;
    add_gateway(&fed, lnv2).await?;
    let w = Wallet::from_client(client).await?;

    let dir = tempfile::tempdir()?;
    let backend = TillBackend { wallet: w.clone(), topups: None, pays: None, dir: dir.path().to_owned(), till: None };
    let meta_hash = sha256::Hash::hash(META.as_bytes()).to_byte_array();
    let service =
        Service { hash: Mutex::new([7; 32]), payment_hash: Mutex::new(sha256::Hash::hash(b"refused")), issued: Mutex::new(vec![]) };
    let e = Engine { dir: dir.path(), backend: &backend, fetch: &service };

    e.update_settings(settings(100_000), false).map_err(|r| anyhow::anyhow!("{}", r.body))?;
    let p = e.propose_destination("shop@example.com").await.map_err(|r| anyhow::anyhow!("{}", r.body))?;
    e.confirm_destination(&p.token).map_err(|r| anyhow::anyhow!("{}", r.body))?;
    let dest = PayoutStore::open(dir.path())?.file().destination.clone().expect("confirmed destination");
    let manual = |id: &str| {
        Some(ManualLightning {
            request_id: id.into(),
            expected_destination: dest.address.clone(),
            expected_confirmed_at_ms: dest.confirmed_at_ms,
        })
    };
    let before = w.balance_msat().await?;

    // An invoice that does not commit to the metadata: refused before anything is funded.
    let r = err(e.lightning(manual("hash"), Some(AMOUNT_SAT), WAIT).await);
    assert_eq!(r["error"], "invoice_rejected", "{r}");
    let bad = service.issued.lock().unwrap().last().cloned().unwrap();
    assert_eq!(w.pay_status(&bad, Duration::ZERO).await?, None);
    assert_eq!(w.balance_msat().await?, before);

    // A fee cap below the gateway's fee: the quote refuses it under the guard, nothing funded.
    *service.hash.lock().unwrap() = meta_hash;
    e.update_settings(settings(0), false).map_err(|r| anyhow::anyhow!("{}", r.body))?;
    let r = err(e.lightning(manual("cap"), Some(AMOUNT_SAT), WAIT).await);
    assert_eq!(r["error"], "fee_too_high", "{r}");
    let capped = service.issued.lock().unwrap().last().cloned().unwrap();
    assert_eq!(w.pay_status(&capped, Duration::ZERO).await?, None);
    assert_eq!(w.balance_msat().await?, before);

    // Paid: exactly the invoice the service gave for exactly the amount, through the gateway.
    *service.payment_hash.lock().unwrap() = sha256::Hash::hash(&PREIMAGE);
    e.update_settings(settings(100_000), false).map_err(|r| anyhow::anyhow!("{}", r.body))?;
    let rec = e.lightning(manual("pay"), Some(AMOUNT_SAT), WAIT).await.map_err(|r| anyhow::anyhow!("{}", r.body))?;
    assert_eq!(rec.state, State::Paid, "{:?}", rec.detail);
    let amount_msat = AMOUNT_SAT * 1000;
    assert_eq!(rec.fee_paid_msat, Some(gateway_fee_msat(amount_msat)));
    let paid_invoice = service.issued.lock().unwrap().last().cloned().unwrap();
    assert!(matches!(w.pay_status(&paid_invoice, WAIT).await?, Some(WalletPay::Paid { .. })));
    let after = w.balance_msat().await?;
    let fed_fee = rec.federation_fee_msat.unwrap_or(0);
    // The federation's fees the wallet's own record names are within the provable bound held back,
    // and the bound is the one the schedules give at that balance (no heuristic).
    assert!(rec.federation_fee_msat.is_some(), "the wallet's record names its fees");
    assert!(fed_fee <= rec.fee_allowance_msat, "{fed_fee} > {}", rec.fee_allowance_msat);
    let schedules = w.fee_schedules().await.map_err(anyhow::Error::msg)?;
    let bound =
        fedimint_card_sidecar::payout::fees::ln_send_fee_bound_over(&schedules, before, amount_msat, amount_msat + rec.fee_cap_msat)
            .map_err(anyhow::Error::msg)?;
    assert_eq!(rec.fee_allowance_msat, bound);
    assert!(w.ln_send_fee_quote(amount_msat).await? <= bound, "the client's own quote is within the bound");
    assert!(before - after >= amount_msat + gateway_fee_msat(amount_msat), "{before} -> {after}");
    assert!(
        before - after <= amount_msat + rec.fee_cap_msat + rec.fee_allowance_msat,
        "{before} -> {after}: more than the amount and the fees held back ({fed_fee} msat to the federation)"
    );
    assert!(PayoutStore::open(dir.path())?.file().halted.is_none());

    // The same request, and a restart: nothing more funded.
    let again = e.lightning(manual("pay"), Some(AMOUNT_SAT), WAIT).await.map_err(|r| anyhow::anyhow!("{}", r.body))?;
    assert_eq!(again.id, rec.id);
    let restarted = Engine { dir: dir.path(), backend: &backend, fetch: &service };
    restarted.sweep(Duration::from_secs(5)).await;
    assert_eq!(w.balance_msat().await?, after);
    assert_eq!(PayoutStore::open(dir.path())?.file().records.iter().filter(|r| r.state == State::Paid).count(), 1);
    // Advance only the persisted due date. A confirmed automatic withdrawal must use the real
    // gateway and preserve the configured reserve. A second tick and restart must not repay.
    let auto_preimage = [3; 32];
    *preimage.lock().unwrap() = auto_preimage;
    *service.payment_hash.lock().unwrap() = sha256::Hash::hash(&auto_preimage);
    let mut auto_settings = settings(100_000);
    auto_settings.mode = Mode::Auto;
    restarted.update_settings(auto_settings, false).map_err(|r| anyhow::anyhow!("{}", r.body))?;
    PayoutStore::open(dir.path())?.commit(|f| f.next_auto_at_ms = 0)?;
    restarted.sweep(WAIT).await;
    let store = PayoutStore::open(dir.path())?;
    let auto = store.file().records.iter().find(|r| r.origin == Origin::Auto).expect("automatic withdrawal recorded");
    assert_eq!(auto.state, State::Paid, "{:?}", auto.detail);
    assert!(auto.amount_msat > 0);
    let balance = w.balance_msat().await?;
    assert!(balance >= auto_settings.reserve_sat * 1000 && balance < after);
    let last = service.issued.lock().unwrap().last().cloned().unwrap();
    assert!(matches!(w.pay_status(&last, WAIT).await?, Some(WalletPay::Paid { .. })));
    restarted.sweep(WAIT).await;
    assert_eq!(w.balance_msat().await?, balance);
    Ok(())
}
