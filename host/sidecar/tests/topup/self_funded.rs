//! Exercise the POS API against a fresh disk wallet, live test guardians and the packaged CLI's
//! normal server entry point. Only the simulated payer starts with funds.
use super::*;
use anyhow::{Result, ensure};
use fedimint_api_client::api::FederationApiExt;
use fedimint_card_fed_tests::ten_msat_mint::TenMsatMint;
use fedimint_card_sidecar::wallet::MintVersion;
use fedimint_client::transaction::{ClientOutput, ClientOutputBundle};
use fedimint_core::config::FederationId;
use fedimint_core::core::IntoDynInstance;
use fedimint_core::module::{Amounts, ApiAuth, ApiRequestErased};
use fedimint_core::secp256k1::{Keypair, SECP256K1, SecretKey};
use fedimint_core::util::SafeUrl;
use fedimint_lnv2_client::common::contracts::{IncomingContract, PaymentImage};
use fedimint_lnv2_client::common::endpoint_constants::{ADD_GATEWAY_ENDPOINT, CREATE_BOLT11_INVOICE_ENDPOINT, ROUTING_INFO_ENDPOINT};
use fedimint_lnv2_client::common::gateway_api::{CreateBolt11InvoicePayload, PaymentFee, RoutingInfo};
use fedimint_lnv2_client::common::{LightningOutput, LightningOutputV0};
use lightning_invoice::{Bolt11Invoice, Currency, InvoiceBuilder, PaymentSecret};
use std::path::Path as FsPath;
use tokio::process::{Child, Command};

const CARD_MSAT: u64 = 2_100_224;
const GATEWAY_FEE_MSAT: u64 = 2100;
const WAIT: Duration = Duration::from_secs(60);
const CREATE: &str = "/v1/topup/fee-inclusive";

struct Gateway {
    key: Keypair,
    federation_id: FederationId,
    contract: Mutex<Option<IncomingContract>>,
    invoices: AtomicUsize,
}

async fn terms(State(g): State<Arc<Gateway>>, Json(fed): Json<FederationId>) -> Json<Option<RoutingInfo>> {
    assert_eq!(g.federation_id, fed);
    let pk = g.key.public_key();
    Json(Some(RoutingInfo {
        lightning_public_key: pk,
        lightning_alias: None,
        module_public_key: pk,
        send_fee_default: PaymentFee::TRANSACTION_FEE_DEFAULT,
        send_fee_minimum: PaymentFee::TRANSACTION_FEE_DEFAULT,
        expiration_delta_default: 500,
        expiration_delta_minimum: 144,
        receive_enabled: true,
        receive_fee: PaymentFee { base: Amount::from_msats(GATEWAY_FEE_MSAT), parts_per_million: 0 },
    }))
}

async fn invoice(State(g): State<Arc<Gateway>>, Json(req): Json<CreateBolt11InvoicePayload>) -> Json<Bolt11Invoice> {
    assert_eq!(req.federation_id, g.federation_id);
    assert_eq!(req.contract.commitment.amount.msats + GATEWAY_FEE_MSAT, req.amount.msats);
    let PaymentImage::Hash(hash) = req.contract.commitment.payment_image else { panic!("expected BOLT11") };
    let result = InvoiceBuilder::new(Currency::Regtest)
        .description("First card top-up".into())
        .payment_hash(hash)
        .current_timestamp()
        .min_final_cltv_expiry_delta(0)
        .payment_secret(PaymentSecret([0; 32]))
        .amount_milli_satoshis(req.amount.msats)
        .expiry_time(Duration::from_secs(req.expiry_secs.into()))
        .build_signed(|m| SECP256K1.sign_ecdsa_recoverable(m, &g.key.secret_key()))
        .unwrap();
    assert!(g.contract.lock().unwrap().replace(req.contract).is_none());
    g.invoices.fetch_add(1, Ordering::SeqCst);
    Json(result)
}

pub(super) async fn start_till(dir: &FsPath, registry: &str, addr: SocketAddr) -> Result<Child> {
    let mut proc = Command::new(env!("CARGO_BIN_EXE_card-sidecar"))
        .arg("--data-dir")
        .arg(dir)
        .args([
            "till",
            "serve",
            "--listen",
            &addr.to_string(),
            "--relay",
            "http://127.0.0.1:1",
            "--ops-url",
            registry,
            "--ops-token",
            "till_test",
            "--esplora-url",
            "",
        ])
        .env_remove("CARD_PIN")
        .env_remove("CARD_BACKEND")
        .env_remove("OPS_TILL_TOKEN")
        .kill_on_drop(true)
        .spawn()?;
    let http = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(4)).build()?;
    tokio::time::timeout(WAIT, async {
        loop {
            ensure!(proc.try_wait()?.is_none(), "test till exited during startup");
            if let Ok(res) = http.get(format!("http://{addr}/v1/status")).send().await
                && res.status().is_success()
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await??;
    Ok(proc)
}

pub(super) async fn settled(addr: SocketAddr, id: &str) -> Result<()> {
    tokio::time::timeout(WAIT, async {
        loop {
            let (code, v) = call(addr, "GET", &format!("/v1/topup/{id}"), None, None).await;
            ensure!(code == 200, "{v}");
            match v["state"].as_str() {
                Some("paid") => return Ok::<_, anyhow::Error>(()),
                Some("waiting") => tokio::time::sleep(Duration::from_millis(100)).await,
                _ => anyhow::bail!("unexpected settlement {v}"),
            }
        }
    })
    .await?
}

async fn fund_contract(payer: &ClientHandleArc, ln_id: u16, contract: IncomingContract) -> Result<()> {
    let amount = contract.commitment.amount;
    let outputs = ClientOutputBundle::new_no_sm(vec![ClientOutput {
        amounts: Amounts::new_bitcoin(amount),
        output: LightningOutput::V0(LightningOutputV0::Incoming(contract)),
    }])
    .into_dyn(ln_id);
    let op = OperationId::new_random();
    let range =
        payer.finalize_and_submit_transaction(op, "gateway funding", |_| (), TransactionBuilder::new().with_outputs(outputs)).await?;
    payer.await_primary_bitcoin_module_outputs(op, range.into_iter().collect()).await?;
    Ok(())
}

async fn first_topup(mint: MintVersion, restart: bool, overlapping: bool) -> Result<()> {
    assert!(!Fixtures::is_real_test());
    let fx = match mint {
        MintVersion::V1 => Fixtures::new_primary(MintClientInit, MintInit),
        MintVersion::V2 => Fixtures::new_primary(fedimint_mintv2_client::MintClientInit, TenMsatMint),
    };
    let fed = tokio::time::timeout(
        WAIT,
        fx.with_module(DummyClientInit, DummyInit)
            .with_module(
                fedimint_lnv2_client::LightningClientInit { gateway_conn: None, custom_meta_fn: Arc::new(|| Value::Null) },
                fedimint_lnv2_server::LightningInit,
            )
            .new_fed_builder(0)
            .build(),
    )
    .await?;
    let payer = fed.new_client().await;
    fund(&payer, Amount::from_sats(10_000)).await?;
    let ln_id = payer.get_first_module::<fedimint_lnv2_client::LightningClientModule>()?.id;
    let gateway = Arc::new(Gateway {
        key: SecretKey::from_slice(&[19; 32])?.keypair(SECP256K1),
        federation_id: fed.id(),
        contract: Mutex::new(None),
        invoices: AtomicUsize::new(0),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = SafeUrl::parse(&format!("http://{}", listener.local_addr()?))?;
    let app = axum::Router::new()
        .route(ROUTING_INFO_ENDPOINT, post(terms))
        .route(CREATE_BOLT11_INVOICE_ENDPOINT, post(invoice))
        .with_state(gateway.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for peer in fed.online_peer_ids() {
        let api = fed.new_admin_api(peer).await?.with_module(ln_id);
        let _: bool = api.request_admin(ADD_GATEWAY_ENDPOINT, ApiRequestErased::new(url.clone()), ApiAuth::new("pass".into())).await?;
    }
    let dir = tempfile::tempdir()?;
    let join = Command::new(env!("CARGO_BIN_EXE_card-sidecar"))
        .arg("--data-dir")
        .arg(dir.path())
        .args(["join", &fed.invite_code().to_string()])
        .kill_on_drop(true)
        .output()
        .await?;
    ensure!(join.status.success(), "couldn't join test federation: {}", String::from_utf8_lossy(&join.stderr));
    let mut card = SoftCard::v4();
    let keys = (0..8).map(|idx| card.gen_key(idx)).collect::<Result<Vec<_>>>()?;
    fedimint_card_host_card::softkit::ready_for_pin(&mut card)?;
    fedimint_card_host_card::softkit::open_test_session(&mut card)?;
    let short_id = short_id_of(&keys[0]);
    let card = Arc::new(Mutex::new(card));
    let mut registry = FakeRegistry::new(fed.id().to_string(), keys.iter().map(ToString::to_string).collect(), 7);
    registry.mint_version = mint;
    let registry = Arc::new(registry);
    let registry_url = serve_registry(registry.clone()).await;
    let addr = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?;
    let mut server = start_till(dir.path(), &registry_url, addr).await?;
    let (_, before) = call(addr, "GET", "/v1/status", None, None).await;
    assert_eq!(before["wallet_balance_msat"], 0);
    let request = json!({"fee_quote_version":1,"amount_msat":2_100_000,"short_id":short_id,"client_ref":"first"});
    let (code, quote) = call(addr, "POST", CREATE, None, Some(request.clone())).await;
    assert_eq!(code, 200, "{quote}");
    let face = quote["amount_msat"].as_u64().unwrap();
    if mint == MintVersion::V2 {
        assert_eq!(face, CARD_MSAT);
    }
    let gross = quote["invoice_amount_msat"].as_u64().unwrap();
    let fee = quote["topup_fee_msat"].as_u64().unwrap();
    assert_eq!(gross, face + fee);
    assert!(fee >= GATEWAY_FEE_MSAT);
    assert_eq!(quote["fee_quote_version"], 1);
    assert_eq!(quote["invoice"].as_str().unwrap().parse::<Bolt11Invoice>()?.amount_milli_satoshis(), Some(gross));
    let id = quote["topup_id"].as_str().unwrap();
    let contract = gateway.contract.lock().unwrap().take().unwrap();
    let second = if overlapping {
        // Two independent customers can have payable invoices before either has paid.
        let request = json!({"fee_quote_version":1,"amount_msat":1_024_000,"short_id":short_id,"client_ref":"second"});
        let (status, other) = call(addr, "POST", CREATE, None, Some(request)).await;
        assert_eq!(status, 200, "{other}");
        assert_ne!(other["topup_id"], id);
        let contract = gateway.contract.lock().unwrap().take().unwrap();
        Some((other, contract))
    } else {
        None
    };
    fund_contract(&payer, ln_id, contract).await?;
    settled(addr, id).await?;
    if restart {
        server.kill().await?;
        server = start_till(dir.path(), &registry_url, addr).await?;
        settled(addr, id).await?;
    }
    let (s, replay) = call(addr, "POST", CREATE, None, Some(request)).await;
    assert_eq!(s, 200, "{replay}");
    assert_eq!(replay["invoice"], quote["invoice"]);
    assert_eq!(gateway.invoices.load(Ordering::SeqCst), if overlapping { 2 } else { 1 });
    let (link, phone) = open_link(addr, card.clone()).await;
    let load_url = format!("/v1/topup/{id}/load");
    let (s, load) = call(addr, "POST", &load_url, Some(&link), None).await;
    phone.abort();
    assert_eq!(s, 200, "{load}");
    assert_eq!(load["state"], "loaded");
    let (s, again) = call(addr, "POST", &load_url, None, None).await;
    assert_eq!(s, 200);
    assert_eq!(again["load_txid"], load["load_txid"]);
    {
        let loads = registry.loads.lock().unwrap();
        assert_eq!(loads.len(), 1);
        assert_eq!(loads[0]["notes"].as_array().unwrap().iter().map(|n| n["amount_msat"].as_u64().unwrap()).sum::<u64>(), face);
        let mut card = card.lock().unwrap();
        let mut card_balance = 0;
        for slot in 0..card.slot_len() {
            let info = card.get_slot(slot)?;
            if info.state == SlotState::Note {
                card_balance += info.amount_msat;
            }
        }
        assert_eq!(card_balance, face);
    }
    assert_eq!(registry.outcomes.lock().unwrap().len(), 1);
    let (_, after) = call(addr, "GET", "/v1/status", None, None).await;
    let left = after["wallet_balance_msat"].as_u64().unwrap();
    let costs = &again["fee_accounting"];
    assert_eq!(costs["status"], "reconciled", "{again}");
    assert_eq!(costs["gateway_msat"], GATEWAY_FEE_MSAT);
    let claim = costs["claim_msat"].as_u64().unwrap();
    let load_cost = costs["load_msat"].as_u64().unwrap();
    assert_eq!(costs["retained_nominal_msat"], fee - GATEWAY_FEE_MSAT - claim - load_cost);
    if mint == MintVersion::V2 {
        assert_eq!(costs["retained_nominal_msat"], left);
        assert_eq!(costs["retained_spendable_msat"], left);
    }
    if let Some((other, contract)) = second {
        // The first promise could be fulfilled even while this second invoice was unpaid.
        let other_id = other["topup_id"].as_str().unwrap();
        fund_contract(&payer, ln_id, contract).await?;
        settled(addr, other_id).await?;
        let (link, phone) = open_link(addr, card.clone()).await;
        let (status, other_load) = call(addr, "POST", &format!("/v1/topup/{other_id}/load"), Some(&link), None).await;
        phone.abort();
        assert_eq!(status, 200, "{other_load}");
        assert_eq!(other_load["fee_accounting"]["status"], "reconciled");
        assert_ne!(other_load["load_txid"], load["load_txid"]);
        let mut card = card.lock().unwrap();
        let mut total = 0;
        for slot in 0..card.slot_len() {
            let info = card.get_slot(slot)?;
            if info.state == SlotState::Note {
                total += info.amount_msat;
            }
        }
        assert_eq!(total, face + other["amount_msat"].as_u64().unwrap());
        assert_eq!(registry.loads.lock().unwrap().len(), 2);
        assert_eq!(registry.outcomes.lock().unwrap().len(), 2);
    }
    assert!(left <= fee - GATEWAY_FEE_MSAT);
    println!("mint={mint:?} restart={restart} starting=0 gross={gross} card={face} fee={fee} wallet_after={left} msat");
    server.kill().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn first_fee_bearing_v2_topup_loads_without_a_restart() -> Result<()> {
    first_topup(MintVersion::V2, false, false).await
}

#[tokio::test(flavor = "multi_thread")]
async fn first_fee_bearing_v2_topup_survives_restart_after_payment() -> Result<()> {
    first_topup(MintVersion::V2, true, false).await
}

#[tokio::test(flavor = "multi_thread")]
async fn first_v1_mint_topup_loads_from_an_empty_till() -> Result<()> {
    first_topup(MintVersion::V1, false, false).await
}

#[tokio::test(flavor = "multi_thread")]
async fn distinct_customer_invoices_can_overlap_from_zero() -> Result<()> {
    first_topup(MintVersion::V2, false, true).await
}
