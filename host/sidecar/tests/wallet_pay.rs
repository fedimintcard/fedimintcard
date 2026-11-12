//! Paying an invoice from the till wallet as a library call (`Wallet::pay_invoice`,
//! `resolve_pay`, `pay_status`), end to end against in-process federations:
//!
//! - lnv2 with a test gateway (the shape of fedimint's own lnv2 test mock): paid, refused over
//!   the fee cap, failed with the refund back in the wallet, and a gateway that never answers
//!   (unknown, never failed).
//! - ln (v1) between two wallets of the same federation, which needs no gateway: paid, paid
//!   again without paying twice, and resolved after the wallet is opened again. An invoice from
//!   outside the federation is refused with nothing spent.
//!
//! Nothing here reaches a real federation, gateway or Lightning node.

use std::sync::Arc;
use std::time::Duration;

use fedimint_api_client::api::{FederationApiExt as _, ServerError};
use fedimint_card_sidecar::wallet::{Wallet, WalletPay};
use fedimint_client::ClientHandleArc;
use fedimint_client::transaction::TransactionBuilder;
use fedimint_core::Amount;
use fedimint_core::OutPoint;
use fedimint_core::bitcoin::hashes::{Hash as _, sha256};
use fedimint_core::config::FederationId;
use fedimint_core::core::{ModuleInstanceId, OperationId};
use fedimint_core::module::{ApiAuth, ApiRequestErased};
use fedimint_core::secp256k1::rand::rngs::OsRng;
use fedimint_core::secp256k1::schnorr::Signature;
use fedimint_core::secp256k1::{Keypair, SECP256K1, SecretKey};
use fedimint_core::util::SafeUrl;
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
use futures::StreamExt as _;
use lightning_invoice::{Bolt11Invoice, Currency, InvoiceBuilder, PaymentSecret};

/// What the test gateway does with an invoice, chosen by its payment secret.
const PAYABLE: [u8; 32] = [211; 32];
const UNPAYABLE: [u8; 32] = [212; 32];
const UNANSWERED: [u8; 32] = [213; 32];
const PREIMAGE: [u8; 32] = [1; 32];
const INVOICE_MSAT: u64 = 1_000_000;
const WAIT: Duration = Duration::from_secs(60);

fn gateway_url() -> SafeUrl {
    SafeUrl::parse("https://gateway.test").expect("valid url")
}

/// The gateway's fee for `INVOICE_MSAT`.
fn gateway_fee_msat() -> u64 {
    PaymentFee::TRANSACTION_FEE_DEFAULT.fee(INVOICE_MSAT).msats
}

/// An lnv2 gateway that pays `PAYABLE` invoices, forfeits `UNPAYABLE` ones (so the contract is
/// refunded) and errors on `UNANSWERED` ones, which leaves the payment funded and in flight.
#[derive(Debug)]
struct TestGateway {
    keypair: Keypair,
}

impl TestGateway {
    fn new() -> Self {
        let keypair = SecretKey::from_slice(&[1; 32])
            .expect("valid key")
            .keypair(SECP256K1);
        Self { keypair }
    }
}

#[async_trait::async_trait]
impl GatewayConnection for TestGateway {
    async fn routing_info(
        &self,
        _gateway_api: SafeUrl,
        _federation_id: &FederationId,
    ) -> Result<Option<RoutingInfo>, ServerError> {
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
        Err(ServerError::InvalidRequest(
            "the test gateway does not receive".into(),
        ))
    }

    async fn send_payment(
        &self,
        _gateway_api: SafeUrl,
        _federation_id: FederationId,
        _outpoint: OutPoint,
        contract: OutgoingContract,
        invoice: LightningInvoice,
        _auth: Signature,
    ) -> Result<Result<[u8; 32], Signature>, ServerError> {
        let LightningInvoice::Bolt11(invoice) = invoice;
        match invoice.payment_secret().0 {
            UNANSWERED => Err(ServerError::InvalidRequest("the gateway is down".into())),
            UNPAYABLE => Ok(Err(self.keypair.sign_schnorr(contract.forfeit_message()))),
            _ => Ok(Ok(PREIMAGE)),
        }
    }
}

/// A regtest invoice for `INVOICE_MSAT` that the test gateway treats by `secret`. Each one is
/// signed by a new key, so two invoices are never the same.
fn invoice(secret: [u8; 32]) -> Bolt11Invoice {
    let key = SecretKey::new(&mut OsRng);
    InvoiceBuilder::new(Currency::Regtest)
        .description(String::new())
        .payment_hash(sha256::Hash::hash(&PREIMAGE))
        .payment_secret(PaymentSecret(secret))
        .current_timestamp()
        .min_final_cltv_expiry_delta(0)
        .amount_milli_satoshis(INVOICE_MSAT)
        .build_signed(|m| SECP256K1.sign_ecdsa_recoverable(m, &key))
        .expect("valid invoice")
}

async fn build(fixtures: Fixtures) -> anyhow::Result<FederationTest> {
    assert!(
        std::env::var_os("FM_TEST_USE_REAL_DAEMONS").is_none(),
        "FM_TEST_USE_REAL_DAEMONS is set: these tests need the in-process fake bitcoin backend"
    );
    Ok(tokio::time::timeout(Duration::from_secs(60), fixtures.new_fed_builder(0).build()).await?)
}

/// Give `client` ecash out of thin air through the dummy module; answers the funding operation.
async fn fund(client: &ClientHandleArc, amount: Amount) -> anyhow::Result<OperationId> {
    let dummy = client.get_first_module::<DummyClientModule>()?;
    let op = OperationId::new_random();
    let range = client
        .finalize_and_submit_transaction(
            op,
            "fund",
            |_| (),
            TransactionBuilder::new().with_inputs(dummy.create_input(amount)),
        )
        .await?;
    client
        .await_primary_bitcoin_module_outputs(op, range.into_iter().collect())
        .await?;
    Ok(op)
}

/// Have every guardian list the test gateway, as a guardian admin does with a real one.
async fn add_gateway(fed: &FederationTest, lnv2: ModuleInstanceId) -> anyhow::Result<()> {
    for peer in fed.online_peer_ids() {
        let api = fed.new_admin_api(peer).await?.with_module(lnv2);
        let _: bool = api
            .request_admin(
                ADD_GATEWAY_ENDPOINT,
                ApiRequestErased::new(gateway_url()),
                ApiAuth::new("pass".into()),
            )
            .await?;
    }
    Ok(())
}

/// Shut the client down so its database can be opened again.
async fn close(w: Wallet) {
    let client = w.client.clone();
    drop(w);
    if let Ok(handle) = Arc::try_unwrap(client) {
        handle.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn lnv2_pays_refuses_fails_for_certain_and_leaves_the_unanswered_open() -> anyhow::Result<()>
{
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit)
        .with_module(DummyClientInit, DummyInit)
        .with_module(
            fedimint_lnv2_client::LightningClientInit {
                gateway_conn: Some(Arc::new(TestGateway::new())),
                custom_meta_fn: Arc::new(|| serde_json::Value::Null),
            },
            fedimint_lnv2_server::LightningInit,
        );
    let fed = build(fixtures).await?;
    let client = fed.new_client().await;
    let fund_op = fund(&client, Amount::from_sats(10_000)).await?;
    let lnv2 = client
        .get_first_module::<fedimint_lnv2_client::LightningClientModule>()?
        .id;
    add_gateway(&fed, lnv2).await?;
    let w = Wallet::from_client(client).await?;
    assert!(w.lightning().is_err() && w.lightning_v2().is_ok());

    // Paid: the preimage and the fee its contract was funded with.
    let payable = invoice(PAYABLE).to_string();
    let before = w.balance_msat().await?;
    let paid = w.pay_invoice(&payable, gateway_fee_msat(), WAIT).await?;
    let WalletPay::Paid {
        preimage,
        fee_msat,
        operation_id,
    } = &paid
    else {
        panic!("{paid:?}")
    };
    assert_eq!(*preimage, hex::encode(PREIMAGE));
    assert_eq!(*fee_msat, gateway_fee_msat());
    let after = w.balance_msat().await?;
    assert!(
        before - after >= INVOICE_MSAT + gateway_fee_msat(),
        "{before} -> {after}"
    );
    // Resolved from the client's record, found by the invoice, and paid again: the same payment.
    assert_eq!(w.resolve_pay(operation_id, WAIT).await?, paid);
    assert_eq!(w.pay_status(&payable, WAIT).await?, Some(paid.clone()));
    assert_eq!(
        w.pay_invoice(&payable, gateway_fee_msat(), WAIT).await?,
        paid
    );
    assert_eq!(
        w.balance_msat().await?,
        after,
        "paying the invoice again spent nothing"
    );

    // A fee cap below the gateway's fee: refused before anything is funded.
    let capped = invoice(PAYABLE).to_string();
    let out = w.pay_invoice(&capped, gateway_fee_msat() - 1, WAIT).await?;
    let WalletPay::Failed {
        reason,
        operation_id: None,
        insufficient_funds: false,
    } = &out
    else {
        panic!("{out:?}")
    };
    assert!(reason.contains("fees"), "{reason}");
    assert_eq!(w.pay_status(&capped, WAIT).await?, None);
    assert_eq!(w.balance_msat().await?, after);

    // The gateway gives up: certain failure, and the refund is back in the wallet.
    let unpayable = invoice(UNPAYABLE).to_string();
    let out = w.pay_invoice(&unpayable, gateway_fee_msat(), WAIT).await?;
    let WalletPay::Failed {
        operation_id: Some(op),
        insufficient_funds: false,
        ..
    } = &out
    else {
        panic!("{out:?}")
    };
    let refunded = w.balance_msat().await?;
    assert!(
        after - refunded < INVOICE_MSAT,
        "{after} -> {refunded}: the refund came back"
    );
    assert_eq!(w.resolve_pay(op, WAIT).await?, out);
    assert_eq!(w.pay_status(&unpayable, WAIT).await?, Some(out.clone()));

    // The gateway never answers: funded and in flight. Unknown, with an id that resolves to
    // unknown again while it lasts; never a failure.
    let unanswered = invoice(UNANSWERED).to_string();
    let out = w
        .pay_invoice(&unanswered, gateway_fee_msat(), Duration::from_secs(3))
        .await?;
    let WalletPay::Unknown { operation_id, .. } = &out else {
        panic!("{out:?}")
    };
    let short = Duration::from_millis(500);
    assert!(matches!(
        w.resolve_pay(operation_id, short).await?,
        WalletPay::Unknown { operation_id: ref o, .. } if o == operation_id
    ));
    assert!(matches!(
        w.pay_status(&unanswered, short).await?,
        Some(WalletPay::Unknown { operation_id: ref o, .. }) if o == operation_id
    ));
    assert!(w.balance_msat().await? <= refunded - INVOICE_MSAT);

    // Ids that name no Lightning payment of this wallet.
    let err = w
        .resolve_pay(&hex::encode(fund_op.0), short)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not a Lightning payment"), "{err}");
    let err = w.resolve_pay(&"00".repeat(32), short).await.unwrap_err();
    assert!(err.to_string().contains("no operation"), "{err}");
    assert!(w.resolve_pay("not hex", short).await.is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn ln_pays_inside_the_federation_once_and_resolves_after_reopening() -> anyhow::Result<()> {
    use fedimint_ln_client::{LightningClientModule, LnReceiveState};

    let fixtures = Fixtures::new_primary(MintClientInit, MintInit)
        .with_module(DummyClientInit, DummyInit)
        .with_module(
            fedimint_ln_client::LightningClientInit { gateway_conn: None },
            fedimint_ln_server::LightningInit,
        );
    let fed = build(fixtures).await?;

    // The till's wallet, joined by invite code as on a real till, funded with ecash from a
    // fixture client.
    let funder = fed.new_client().await;
    fund(&funder, Amount::from_sats(50_000)).await?;
    let funder = Wallet::from_client(funder).await?;
    let (notes, _) = funder.export(20_000_000, true, false).await?;
    let dir = tempfile::tempdir()?;
    let w = Wallet::join(dir.path(), &fed.invite_code().to_string()).await?;
    assert!(w.lightning().is_ok() && w.lightning_v2().is_err());
    w.import_notes(notes).await?;

    // Someone else in the federation asks to be paid 1,000 sat.
    let receiver = fed.new_client().await;
    let ln = receiver.get_first_module::<LightningClientModule>()?;
    let description = lightning_invoice::Description::new("till pay test".into())?;
    let (receive_op, bolt11, _) = ln
        .create_bolt11_invoice(
            Amount::from_sats(1_000),
            lightning_invoice::Bolt11InvoiceDescription::Direct(description),
            None,
            (),
            None,
        )
        .await?;
    let mut receive = ln.subscribe_ln_receive(receive_op).await?.into_stream();
    while let Some(state) = receive.next().await {
        if matches!(state, LnReceiveState::WaitingForPayment { .. }) {
            break;
        }
    }
    let bolt11 = bolt11.to_string();

    // No gateway, and none needed: paid inside the federation, with no gateway fee.
    assert_eq!(w.pay_status(&bolt11, WAIT).await?, None);
    let before = w.balance_msat().await?;
    let paid = w.pay_invoice(&bolt11, 0, WAIT).await?;
    let WalletPay::Paid {
        fee_msat: 0,
        operation_id,
        ..
    } = &paid
    else {
        panic!("{paid:?}")
    };
    let after = w.balance_msat().await?;
    assert!(before - after >= 1_000_000, "{before} -> {after}");
    // Found by the invoice alone: the attempt ids derived here are the client's own.
    assert_eq!(w.pay_status(&bolt11, WAIT).await?, Some(paid.clone()));
    assert_eq!(w.pay_invoice(&bolt11, 0, WAIT).await?, paid);
    assert_eq!(
        w.balance_msat().await?,
        after,
        "paying the invoice again spent nothing"
    );
    let operation_id = operation_id.clone();
    close(w).await;

    // After a restart: the same answer from the wallet's database.
    let w = Wallet::open(dir.path()).await?;
    assert_eq!(w.resolve_pay(&operation_id, WAIT).await?, paid);
    assert_eq!(w.pay_status(&bolt11, WAIT).await?, Some(paid.clone()));

    // An invoice from outside the federation, with no gateway to pay it: refused, nothing spent.
    let key = SecretKey::new(&mut OsRng);
    let outside = InvoiceBuilder::new(Currency::Regtest)
        .description("outside".into())
        .payment_hash(sha256::Hash::hash(&rand::random::<[u8; 32]>()))
        .payment_secret(PaymentSecret([42; 32]))
        .current_timestamp()
        .min_final_cltv_expiry_delta(144)
        .amount_milli_satoshis(10_000)
        .build_signed(|m| SECP256K1.sign_ecdsa_recoverable(m, &key))?
        .to_string();
    let out = w.pay_invoice(&outside, 1_000, WAIT).await?;
    let WalletPay::Failed {
        reason,
        operation_id: None,
        insufficient_funds: false,
    } = &out
    else {
        panic!("{out:?}")
    };
    assert!(reason.contains("gateway"), "{reason}");
    assert_eq!(w.pay_status(&outside, WAIT).await?, None);
    assert_eq!(w.balance_msat().await?, after);
    close(w).await;
    Ok(())
}
