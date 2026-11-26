//! The LN-v1 gateway funds the till's published incoming offer. Settle it
//! with a separately funded client and exercise the same card tap as the POS.
use super::*;
use anyhow::{Result, ensure};
use fedimint_client::transaction::{ClientOutput, ClientOutputBundle};
use fedimint_core::core::IntoDynInstance;
use fedimint_core::module::Amounts;
use fedimint_core::secp256k1::{SECP256K1, SecretKey};
use fedimint_core::util::SafeUrl;
use fedimint_ln_client::LightningClientModule;
use fedimint_ln_client::api::LnFederationApi as _;
use fedimint_ln_common::gateway_endpoint_constants::GET_GATEWAY_ID_ENDPOINT;
use fedimint_ln_common::{LightningGateway, LightningGatewayAnnouncement, LightningOutput};
use lightning_invoice::{Bolt11Invoice, RoutingFees};
use tokio::process::Command;

async fn v1_topup(fee_bearing: bool) -> Result<()> {
    assert!(!Fixtures::is_real_test());
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit)
        .with_module(DummyClientInit, DummyInit)
        .with_module(fedimint_ln_client::LightningClientInit { gateway_conn: None }, fedimint_ln_server::LightningInit);
    let builder = fixtures.new_fed_builder(0);
    let builder = if fee_bearing { builder } else { builder.disable_mint_fees() };
    let fed = tokio::time::timeout(Duration::from_secs(60), builder.build()).await?;
    let payer = fed.new_client().await;
    fund(&payer, Amount::from_sats(10_000)).await?;
    let payer_wallet = Wallet::from_client(payer.clone()).await?;
    let (nominal, spendable) = payer_wallet.mint_balances_msat().await?;
    assert_eq!(nominal, payer_wallet.balance_msat().await?);
    assert_eq!(nominal > spendable, fee_bearing);
    assert!(nominal - spendable <= payer_wallet.uneconomical_change_bound_msat()?);

    let key = SecretKey::from_slice(&[17; 32])?.keypair(SECP256K1);
    let pubkey = key.public_key();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let api = SafeUrl::parse(&format!("http://{}", listener.local_addr()?))?;
    let gateway = axum::Router::new().route(GET_GATEWAY_ID_ENDPOINT, get(move || async move { Json(pubkey) }));
    tokio::spawn(async move { axum::serve(listener, gateway).await.unwrap() });
    let ln = payer.get_first_module::<LightningClientModule>()?;
    let announcement = LightningGatewayAnnouncement {
        info: LightningGateway {
            federation_index: 0,
            gateway_redeem_key: pubkey,
            node_pub_key: pubkey,
            lightning_alias: "fixture".into(),
            api,
            route_hints: vec![],
            // Outgoing routing fees are deliberately nonzero: they are not
            // charged on the invoice amount deposited in an incoming offer.
            fees: RoutingFees { base_msat: 3_000, proportional_millionths: 1_000 },
            gateway_id: pubkey,
            supports_private_payments: false,
        },
        vetted: true,
        ttl: Duration::from_secs(600),
        auth: None,
    };
    ln.api.register_gateway(&announcement).await?;

    let dir = tempfile::tempdir()?;
    let join = Command::new(env!("CARGO_BIN_EXE_card-sidecar"))
        .arg("--data-dir")
        .arg(dir.path())
        .args(["join", &fed.invite_code().to_string()])
        .kill_on_drop(true)
        .output()
        .await?;
    ensure!(join.status.success(), "till could not join the LN-v1 federation: {}", String::from_utf8_lossy(&join.stderr));
    let mut card = SoftCard::v4();
    let keys = (0..8).map(|i| card.gen_key(i)).collect::<Result<Vec<_>>>()?;
    fedimint_card_host_card::softkit::ready_for_pin(&mut card)?;
    fedimint_card_host_card::softkit::open_test_session(&mut card)?;
    let registry = Arc::new(FakeRegistry::new(fed.id().to_string(), keys.iter().map(ToString::to_string).collect(), 7));
    let url = serve_registry(registry.clone()).await;
    let addr = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?;
    let mut till = super::self_funded::start_till(dir.path(), &url, addr).await?;
    let (status, before) = call(addr, "GET", "/v1/status", None, None).await;
    assert_eq!(status, 200);
    assert_eq!(before["wallet_balance_msat"], 0);
    let face = 1u64 << 21;
    let req = json!({"amount_msat":face, "short_id":short_id_of(&keys[0]), "client_ref":"legacy-receive"});
    let (status, q) = create_quote(addr, req.clone()).await;
    assert_eq!(status, 200, "{q}");
    let invoice: Bolt11Invoice = q["invoice"].as_str().unwrap().parse()?;
    let id = q["topup_id"].as_str().unwrap();
    let gross = q["invoice_amount_msat"].as_u64().unwrap();
    let fee = q["topup_fee_msat"].as_u64().unwrap();
    assert_eq!(q["amount_msat"], face);
    assert_eq!(gross, face + fee);
    assert_eq!(invoice.amount_milli_satoshis(), Some(gross));
    assert_eq!(q["expires_at_ms"], invoice.expires_at().unwrap().as_secs() * 1000);
    assert_eq!(fee > 0, fee_bearing);
    // Use the same guard and offer-to-contract operation as an LN-v1 gateway.
    let (output, amount, _) =
        fedimint_ln_client::create_incoming_contract_output(&ln.api, *invoice.payment_hash(), Amount::from_msats(gross), &key).await?;
    assert_eq!(amount.msats, gross);
    let outputs =
        ClientOutputBundle::new_no_sm(vec![ClientOutput { amounts: Amounts::new_bitcoin(amount), output: LightningOutput::V0(output) }])
            .into_dyn(ln.id);
    let op = OperationId::new_random();
    let range = payer
        .finalize_and_submit_transaction(op, "test gateway funds offer", |_| (), TransactionBuilder::new().with_outputs(outputs))
        .await?;
    payer.await_primary_bitcoin_module_outputs(op, range.into_iter().collect()).await?;
    super::self_funded::settled(addr, id).await?;
    let (status, replay) = create_quote(addr, req).await;
    assert_eq!(status, 200);
    assert_eq!(replay["invoice"], q["invoice"]);
    let (link, phone) = open_link(addr, Arc::new(Mutex::new(card))).await;
    let path = format!("/v1/topup/{id}/load");
    let (status, load) = call(addr, "POST", &path, Some(&link), None).await;
    phone.abort();
    assert_eq!(status, 200, "{load}");
    assert_eq!(load["state"], "loaded");
    let (status, replay) = call(addr, "POST", &path, None, None).await;
    assert_eq!(status, 200);
    assert_eq!(replay["load_txid"], load["load_txid"]);
    {
        let loads = registry.loads.lock().unwrap();
        assert_eq!(loads.len(), 1);
        assert_eq!(loads[0]["notes"][0]["amount_msat"], face);
        assert_eq!(registry.outcomes.lock().unwrap().len(), 1);
    }
    let costs = &load["fee_accounting"];
    assert_eq!(costs["status"], "reconciled", "{load}");
    assert_eq!(replay["fee_accounting"], *costs);
    assert_eq!(costs["gateway_msat"], 0);
    let claim_cost = costs["claim_msat"].as_u64().unwrap();
    let load_cost = costs["load_msat"].as_u64().unwrap();
    let dead = costs["unspendable_change_delta_msat"].as_i64().unwrap();
    let retained = fee.checked_sub(claim_cost + load_cost).unwrap();
    assert_eq!(costs["retained_nominal_msat"], retained);
    assert_eq!(costs["retained_spendable_msat"], retained as i64 - dead);
    assert_eq!(claim_cost > 0, fee_bearing);
    assert_eq!(load_cost > 0, fee_bearing);
    // Reopen the actual wallet. The fee ledger must match both total funds
    // and notes that can fund a subsequent spend, after issuance completes.
    till.kill().await?;
    let wallet = Wallet::open(dir.path()).await?;
    let (nominal, spendable) = tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            let pair = wallet.mint_balances_msat().await?;
            if pair.0 == retained {
                return Ok::<_, anyhow::Error>(pair);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await??;
    assert_eq!(nominal as i64 - spendable as i64, dead);
    assert_eq!(costs["retained_spendable_msat"], spendable);
    println!(
        "LN v1: starting=0, invoice={gross}, card={face}, fee={fee}, claim={claim_cost}, load={load_cost}, retained_nominal={nominal}, retained_spendable={spendable} msat"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_lightning_full_card_credit_from_empty_wallet() -> Result<()> {
    v1_topup(true).await
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_lightning_zero_fee_invoice_equals_card_credit() -> Result<()> {
    v1_topup(false).await
}
