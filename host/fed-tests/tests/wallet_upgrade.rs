//! Native database upgrade coverage. The optional historical-binary tests run the previous
//! release to create its own RocksDB and seed against an in-process test federation.
use fedimint_card_fed_tests::ten_msat_mint::TenMsatMint;

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use fedimint_card_sidecar::payout::backend::{
    ReclaimStart, ReclaimState, Tagged, export_tagged, find_tagged, reclaim_start, reclaim_status,
};
use fedimint_card_sidecar::wallet::{DB_DIR, INVITE_FILE, MNEMONIC_FILE, Wallet};
use fedimint_client::ClientHandleArc;
use fedimint_client::transaction::TransactionBuilder;
use fedimint_core::Amount;
use fedimint_core::base32::{FEDIMINT_PREFIX, encode_prefixed};
use fedimint_core::core::OperationId;
use fedimint_dummy_client::{DummyClientInit, DummyClientModule};
use fedimint_dummy_server::DummyInit;
use fedimint_meta_client::{MetaClientModule, common::MetaKey};
use fedimint_testing::federation::FederationTest;
use fedimint_testing::fixtures::Fixtures;
use fedimint_walletv2_client::WalletClientModule;

const TEST_MSAT: u64 = 64_000;
const STEP: Duration = Duration::from_secs(90);

async fn federation(fixtures: Fixtures) -> Result<FederationTest> {
    ensure!(!Fixtures::is_real_test(), "this test requires the in-process fake Bitcoin backend");
    Ok(tokio::time::timeout(STEP, fixtures.with_module(DummyClientInit, DummyInit).new_fed_builder(0).disable_mint_fees().build()).await?)
}

fn v2_fixtures(exact_fee: bool) -> Fixtures {
    let primary = if exact_fee {
        Fixtures::new_primary(fedimint_mintv2_client::MintClientInit, TenMsatMint)
    } else {
        Fixtures::new_primary(fedimint_mintv2_client::MintClientInit, fedimint_mintv2_server::MintInit)
    };
    primary
        .with_server_only_module(fedimint_lnv2_server::LightningInit)
        .with_server_only_module(fedimint_meta_server::MetaInit)
        .with_server_only_module(fedimint_walletv2_server::WalletInit)
}

async fn funded_client(fed: &FederationTest) -> Result<ClientHandleArc> {
    let client = fed.new_client().await;
    let op = OperationId::new_random();
    let input = client.get_first_module::<DummyClientModule>()?.create_input(Amount::from_sats(1024));
    let range = client.finalize_and_submit_transaction(op, "test funding", |_| (), TransactionBuilder::new().with_inputs(input)).await?;
    client.await_primary_bitcoin_module_outputs(op, range.into_iter().collect()).await?;
    Ok(client)
}

async fn close(wallet: Wallet) -> Result<()> {
    let client = wallet.client.clone();
    drop(wallet);
    Arc::try_unwrap(client).map_err(|_| anyhow::anyhow!("test leaked a client handle"))?.shutdown().await;
    Ok(())
}

async fn check_v2_modules(wallet: &Wallet) -> Result<()> {
    let gateways = wallet.lightning_v2()?.list_gateways(None).await?;
    ensure!(gateways.is_empty(), "fixture unexpectedly has a registered gateway");
    let meta = wallet.client.get_first_module::<MetaClientModule>()?;
    ensure!(meta.get_consensus_value_rev(MetaKey(73)).await?.is_none(), "empty federation already contains metadata");
    wallet.client.get_first_module::<WalletClientModule>()?.block_count().await?;
    Ok(())
}

async fn wait_balance(wallet: &Wallet, expected: u64) -> Result<()> {
    tokio::time::timeout(STEP, async {
        while wallet.balance_msat().await? != expected {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

async fn fund_v2_wallet(fed: &FederationTest, wallet: &Wallet, amount_msat: u64) -> Result<u64> {
    let funder = funded_client(fed).await?;
    let mint = funder.get_first_module::<fedimint_mintv2_client::MintClientModule>()?;
    let (_, ecash) = mint.send(Amount::from_msats(amount_msat), serde_json::Value::Null, true).await?;
    let imported = wallet.import(&encode_prefixed(FEDIMINT_PREFIX, &ecash)).await?;
    ensure!(imported.face.msats == amount_msat && !imported.already);
    let balance = wallet.balance_msat().await?;
    if wallet.fee(amount_msat) == 0 {
        ensure!(balance == amount_msat);
    } else {
        // The SDK records both the per-note fee and any amount below its smallest change note.
        ensure!(imported.fee_msat > 0 && balance > 0);
        ensure!(balance + imported.fee_msat == amount_msat, "credited balance and actual fee must account for the full import");
    }
    Ok(balance)
}

async fn receive_v2_and_reopen(dir: &Path, fed: &FederationTest, wallet: Wallet) -> Result<u64> {
    ensure!(wallet.federation_id() == fed.id());
    check_v2_modules(&wallet).await?;
    ensure!(wallet.balance_msat().await? == 0);

    ensure!(wallet.fee(512) == 10 && wallet.fee(1_000_000) == 10);
    let received = fund_v2_wallet(fed, &wallet, TEST_MSAT).await?;
    close(wallet).await?;
    let reopened = Wallet::open(dir).await?;
    ensure!(reopened.federation_id() == fed.id());
    ensure!(reopened.balance_msat().await? == received, "mint-v2 notes did not survive reopening");
    check_v2_modules(&reopened).await?;
    close(reopened).await?;
    Ok(received)
}

#[tokio::test(flavor = "multi_thread")]
async fn fresh_v2_wallet_joins_receives_and_reopens_with_all_modules() -> Result<()> {
    let fed = federation(v2_fixtures(true)).await?;
    let dir = tempfile::tempdir()?;
    tokio::time::timeout(STEP * 2, async {
        let wallet = Wallet::join(dir.path(), &fed.invite_code().to_string()).await?;
        receive_v2_and_reopen(dir.path(), &fed, wallet).await?;
        Ok(())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread")]
async fn saved_wallet_refreshes_an_incomplete_api_cache_even_with_the_current_marker() -> Result<()> {
    use fedimint_client::db::CachedApiVersionSetKey;
    use fedimint_core::db::IDatabaseTransactionOpsCoreTyped as _;

    let fed = federation(v2_fixtures(true)).await?;
    let dir = tempfile::tempdir()?;
    tokio::time::timeout(STEP * 2, async {
        let wallet = Wallet::join(dir.path(), &fed.invite_code().to_string()).await?;
        let received = fund_v2_wallet(&fed, &wallet, TEST_MSAT).await?;
        let original = OriginalWallet::read(dir.path())?;
        let db = wallet.client.db().clone();
        let mint_id = wallet.mint_id;
        close(wallet).await?;

        // Only the disposable SDK version cache is stale. Leave all marker and wallet files in place.
        ensure!(dir.path().join("api-versions-card40-mintv2").is_file());
        let mut tx = db.begin_transaction().await;
        let mut cached = tx.get_value(&CachedApiVersionSetKey).await.context("missing initial API cache")?;
        ensure!(cached.0.modules.remove(&mint_id).is_some());
        tx.insert_entry(&CachedApiVersionSetKey, &cached).await;
        tx.commit_tx_result().await?;
        drop(db);

        let reopened = Wallet::open(dir.path()).await?;
        ensure!(reopened.federation_id() == fed.id() && reopened.balance_msat().await? == received);
        let mut tx = reopened.client.db().begin_transaction_nc().await;
        ensure!(tx.get_value(&CachedApiVersionSetKey).await.context("cache was not refreshed")?.0.modules.contains_key(&mint_id));
        drop(tx);
        check_v2_modules(&reopened).await?;
        close(reopened).await?;
        original.verify(dir.path())
    })
    .await?
}

/// Snapshot identity and original wallet files without emitting any wallet material on failure.
struct OriginalWallet {
    mnemonic: Vec<u8>,
    invite: Vec<u8>,
    db_identity: Vec<u8>,
}
impl OriginalWallet {
    fn read(dir: &Path) -> Result<Self> {
        Ok(Self {
            mnemonic: std::fs::read(dir.join(MNEMONIC_FILE))?,
            invite: std::fs::read(dir.join(INVITE_FILE))?,
            db_identity: std::fs::read(dir.join(DB_DIR).join("IDENTITY"))?,
        })
    }
    fn verify(&self, dir: &Path) -> Result<()> {
        let now = Self::read(dir)?;
        ensure!(self.mnemonic == now.mnemonic, "upgrade replaced existing mnemonic");
        ensure!(self.invite == now.invite, "upgrade replaced existing invite");
        ensure!(self.db_identity == now.db_identity, "upgrade replaced existing RocksDB");
        Ok(())
    }
}

async fn legacy(dir: &Path, args: &[&str]) -> Result<Output> {
    let binary = PathBuf::from(std::env::var_os("CARD40_LEGACY_SIDECAR").context("set CARD40_LEGACY_SIDECAR to the 0.1.0:10 executable")?);
    let mut command = tokio::process::Command::new(binary);
    command.arg("--data-dir").arg(dir).args(args).kill_on_drop(true);
    Ok(tokio::time::timeout(STEP, command.output()).await??)
}

/// Run the normal packaged entry point against the previous binary's database.
async fn check_admin_restart(dir: &Path, fed: &FederationTest, expected_balance: u64) -> Result<()> {
    let binary = std::env::var_os("CARD40_UPDATED_SIDECAR").context("set CARD40_UPDATED_SIDECAR to the updated card-sidecar binary")?;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let addr = listener.local_addr()?;
    let password = OperationId::new_random().fmt_full().to_string();
    let password_file = dir.join("fixture-admin-password");
    std::fs::write(&password_file, &password)?;
    drop(listener);
    // Override every external address. The federation also lives entirely on loopback.
    let mut child = tokio::process::Command::new(binary)
        .arg("--data-dir")
        .arg(dir)
        .args(["till", "admin", "--listen", "127.0.0.1:0", "--admin-listen", &addr.to_string(), "--admin-password-file"])
        .arg(password_file)
        .args(["--ops-url", "http://127.0.0.1:1", "--relay", "http://127.0.0.1:1", "--esplora-url", ""])
        .env_remove("OPS_TILL_TOKEN")
        .env_remove("TILL_ADMIN_PASSWORD")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let http = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(10)).build()?;
    let health = format!("http://{addr}/healthz");
    let status = format!("http://{addr}/admin/status");
    tokio::time::timeout(STEP, async {
        loop {
            ensure!(child.try_wait()?.is_none(), "updated till exited before it could serve the previous wallet");
            if let Ok(response) = http.get(&health).send().await
                && response.status().is_success()
            {
                let state: serde_json::Value = http.get(&status).bearer_auth(&password).send().await?.error_for_status()?.json().await?;
                if state["joined"] == true {
                    ensure!(state["federation_id"] == fed.id().to_string());
                    ensure!(state["wallet_balance_msat"] == expected_balance);
                    break Ok::<_, anyhow::Error>(());
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await??;
    child.kill().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires CARD40_LEGACY_SIDECAR (599f8f053217) and CARD40_UPDATED_SIDECAR"]
async fn committed_v2_wallet_from_previous_till_opens_in_place() -> Result<()> {
    let fed = federation(v2_fixtures(true)).await?;
    let dir = tempfile::tempdir()?;
    let old_join = legacy(dir.path(), &["join", &fed.invite_code().to_string()]).await?;
    ensure!(!old_join.status.success());
    ensure!(String::from_utf8_lossy(&old_join.stderr).contains("Module kind mint not found"), "old till did not reach its mint-v1 check");
    let original = OriginalWallet::read(dir.path())?;
    let balance = tokio::time::timeout(STEP * 2, async {
        let wallet = Wallet::open(dir.path()).await?;
        receive_v2_and_reopen(dir.path(), &fed, wallet).await
    })
    .await??;
    check_admin_restart(dir.path(), &fed, balance).await?;
    original.verify(dir.path())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires CARD40_LEGACY_SIDECAR built from 599f8f053217"]
async fn funded_v1_wallet_from_previous_till_opens_in_place() -> Result<()> {
    let fed = federation(Fixtures::new_primary(fedimint_mint_client::MintClientInit, fedimint_mint_server::MintInit)).await?;
    let dir = tempfile::tempdir()?;
    ensure!(legacy(dir.path(), &["join", &fed.invite_code().to_string()]).await?.status.success(), "previous till failed to join mint v1");
    tokio::time::timeout(STEP * 2, async {
        let funder = funded_client(&fed).await?;
        let mint = funder.get_first_module::<fedimint_mint_client::MintClientModule>()?;
        let notes = mint.send_oob_notes(Amount::from_msats(TEST_MSAT), ()).await?;
        ensure!(legacy(dir.path(), &["ecash", "import", &notes.to_string()]).await?.status.success(), "old till failed to redeem v1 notes");
        let original = OriginalWallet::read(dir.path())?;
        let wallet = Wallet::open(dir.path()).await?;
        ensure!(wallet.federation_id() == fed.id());
        ensure!(wallet.balance_msat().await? == TEST_MSAT, "mint-v1 balance changed");
        let (exported, amount) = wallet.export(TEST_MSAT, false, false).await?;
        ensure!(amount.msats == TEST_MSAT);
        let recipient = Wallet::from_client(funder).await?;
        ensure!(recipient.import(&exported.to_string()).await?.face.msats == TEST_MSAT, "recipient could not redeem upgraded v1 notes");
        close(wallet).await?;
        original.verify(dir.path())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread")]
async fn v2_payout_recovers_the_export_after_making_change_and_settles_each_reclaim_once() -> Result<()> {
    let fed = federation(v2_fixtures(false)).await?;
    let dir = tempfile::tempdir()?;
    let invite = fed.invite_code().to_string();
    tokio::time::timeout(STEP * 2, async {
        let wallet = Wallet::join(dir.path(), &invite).await?;
        fund_v2_wallet(&fed, &wallet, 1 << 17).await?;
        // Leave only the largest denomination; a send for half that denomination must reissue.
        let mint = wallet.mint_v2()?;
        let counts = mint.get_count_by_denomination().await;
        let (largest, count) = counts.last_key_value().context("no funded notes")?;
        let initial = largest.amount().msats * count;
        for (tier, quantity) in counts.range(..largest) {
            for _ in 0..*quantity {
                mint.send(tier.amount(), serde_json::Value::Null, false).await?;
            }
        }
        ensure!(mint.get_count_by_denomination().await.len() == 1, "fixture retained smaller notes");
        ensure!(wallet.balance_msat().await? == initial);
        let face = largest.amount().msats / 2;
        let tags = ["fedimintcard-payout:fixture-1".to_owned(), "fedimintcard-payout-reclaim:fixture-1".to_owned()];
        let (notes, amount) = export_tagged(&wallet, face, &tags[0]).await?;
        ensure!(amount == face);
        let page = wallet.client.operation_log().paginate_operations_rev(10, None).await;
        ensure!(
            page.iter().any(|(_, entry)| matches!(entry.try_meta::<fedimint_mintv2_client::MintOperationMeta>(),
            Ok(fedimint_mintv2_client::MintOperationMeta::Reissue { custom_meta, .. }) if custom_meta["fedimintcard_payout"] == tags[0])),
            "fixture did not exercise tagged change-making"
        );
        close(wallet).await?;

        let wallet = Wallet::open(dir.path()).await?;
        wait_balance(&wallet, initial - face).await?;
        let found = find_tagged(&wallet, &tags).await?;
        ensure!(found.len() == 1);
        ensure!(found.get(&tags[0]) == Some(&Tagged::Export { notes: notes.clone() }));
        let ReclaimStart::Started(op) = reclaim_start(&wallet, &notes, &tags[1]).await? else {
            anyhow::bail!("first reclaim was not started")
        };
        close(wallet).await?;
        let wallet = Wallet::open(dir.path()).await?;
        ensure!(find_tagged(&wallet, &tags).await?.get(&tags[1]) == Some(&Tagged::Reclaim { operation_id: op.clone() }));
        ensure!(reclaim_status(&wallet, &op, STEP).await? == ReclaimState::Done);
        wait_balance(&wallet, initial).await?;
        ensure!(reclaim_start(&wallet, &notes, &tags[1]).await? == ReclaimStart::AlreadyReissued);
        ensure!(wallet.balance_msat().await? == initial);

        // Notes the recipient already redeemed cannot be reclaimed by this till.
        let (taken, _) = export_tagged(&wallet, face, "fedimintcard-payout:fixture-2").await?;
        let recipient_dir = tempfile::tempdir()?;
        let recipient = Wallet::join(recipient_dir.path(), &invite).await?;
        recipient.import(&taken).await?;
        wait_balance(&recipient, face).await?;
        let ReclaimStart::Started(op) = reclaim_start(&wallet, &taken, "fedimintcard-payout-reclaim:fixture-2").await? else {
            anyhow::bail!("reclaim of spent export has no outcome to check")
        };
        ensure!(matches!(reclaim_status(&wallet, &op, STEP).await?, ReclaimState::Rejected(_)));
        wait_balance(&wallet, initial - face).await?;
        close(recipient).await?;
        close(wallet).await
    })
    .await?
}
