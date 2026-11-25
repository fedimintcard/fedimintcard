//! The till's persistent Fedimint client, Bitcoin eCash, and Lightning operations.

mod mint;
pub use fedimint_card_pos_core::mint::MintVersion;
pub use mint::{CardMintConfig, MintUnavailable, UnsupportedMint, encode_v2_ecash};

pub mod pay;
pub use pay::WalletPay;

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use fedimint_bip39::{Bip39RootSecretStrategy, Mnemonic};
use fedimint_client::secret::RootSecretStrategy;
use fedimint_client::{Client, ClientHandleArc, RootSecret};
use fedimint_connectors::ConnectorRegistry;
use fedimint_core::Amount;
use fedimint_core::config::ClientConfig;
use fedimint_core::config::FederationId;
use fedimint_core::core::{ModuleInstanceId, OperationId};
use fedimint_core::db::Database;
use fedimint_core::invite_code::InviteCode;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::module::{CommonModuleInit, CoreConsensusVersion};
use fedimint_core::secp256k1::rand::thread_rng;
use fedimint_ln_client::{InternalPayState, LnPayState, PayBolt11InvoiceError};
use fedimint_ln_common::config::FeeToAmount as _;
use fedimint_mint_client::{MintClientInit, MintClientModule, OOBNotes, ReissueExternalNotesError, ReissueExternalNotesState};
use futures::StreamExt as _;
use lightning_invoice::Bolt11Invoice;
use serde::Serialize;

/// File names inside the data directory.
pub const MNEMONIC_FILE: &str = "mnemonic";
pub const INVITE_FILE: &str = "invite";
pub const DB_DIR: &str = "client.db";
pub const STATE_FILE: &str = "card_state.json";
pub const SALES_FILE: &str = "sales.json";

pub fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join(STATE_FILE)
}

pub fn sales_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SALES_FILE)
}

/// A joined federation client plus what the card glue needs from the mint module.
/// Written once the API versions were refetched with the ln module registered (`Wallet::open`).
const LN_VERSIONS_MARKER: &str = "api-versions-ln";
/// The same for lnv2, on a federation without ln (the only kind that registers it).
const LNV2_VERSIONS_MARKER: &str = "api-versions-lnv2";
const CARD40_MODULES_MARKER: &str = "api-versions-card40-mintv2";

/// How long `pay_bolt11` follows a payment before it answers `Pending`.
pub const LN_PAY_WAIT: Duration = Duration::from_secs(90);

/// The outcome of `Wallet::pay_bolt11`. Only `Refused` is certain to have spent nothing; after
/// `Pending` the payment goes on in the client and ends as paid or refunded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LnPayOutcome {
    /// The gateway paid the invoice. `fee_msat` is the gateway's fee (the federation's own
    /// transaction fees are not included).
    Paid { preimage: String, fee_msat: u64, operation_id: String },
    /// Still in flight after `LN_PAY_WAIT`.
    Pending { operation_id: String },
    /// The payment did not go through and the eCash came back to the wallet.
    Refunded { reason: String, operation_id: String },
    /// The client reported an error it could not settle; check the wallet balance.
    Failed { reason: String, operation_id: String },
    /// Not started: nothing left the wallet. `retryable` when nothing about the invoice or the
    /// fee stops it, only that no gateway answers now: the same payment can go through later.
    Refused { reason: String, insufficient_funds: bool, retryable: bool },
}

impl LnPayOutcome {
    fn refused(reason: impl Into<String>) -> Self {
        Self::Refused { reason: reason.into(), insufficient_funds: false, retryable: false }
    }

    /// Refused only because no gateway answers now.
    fn no_gateway(reason: impl Into<String>) -> Self {
        Self::Refused { reason: reason.into(), insufficient_funds: false, retryable: true }
    }
}

/// Parse a BOLT11 invoice as a wallet would take it (a `lightning:` prefix and surrounding
/// whitespace are dropped). An invoice without an amount is refused: the till pays exact amounts.
pub fn parse_bolt11(text: &str) -> Result<Bolt11Invoice> {
    let t = text.trim();
    let t = match t.get(..10) {
        Some(p) if p.eq_ignore_ascii_case("lightning:") => &t[10..],
        _ => t,
    };
    let invoice = Bolt11Invoice::from_str(t).map_err(|e| anyhow!("not a BOLT11 invoice: {e}"))?;
    ensure!(invoice.amount_milli_satoshis().is_some(), "the invoice has no amount; ask for one with an amount");
    Ok(invoice)
}

/// How long one guardian gets to list the gateways in the fallback below.
const GATEWAY_PEER_TIMEOUT: Duration = Duration::from_secs(15);

/// The lnv1 gateway to pay through. The stock refresh (`update_gateway_cache`) needs every guardian
/// to answer, so one unreachable guardian (a poor connection, a guardian down) stops every payment.
/// When it fails, use the gateways saved by an earlier refresh, then whatever guardians answer on
/// their own. That is safe for a payout: a gateway claims an lnv1 contract only by paying the
/// invoice, and a contract it never claims times out and returns to the wallet.
async fn lnv1_gateway(
    ln: &fedimint_client::module::ClientModuleInstance<'_, fedimint_ln_client::LightningClientModule>,
    invoice: &Bolt11Invoice,
) -> std::result::Result<fedimint_ln_common::LightningGateway, String> {
    let refresh = ln.update_gateway_cache().await;
    match ln.select_available_gateway(None, Some(invoice.clone())).await {
        Ok(gw) => return Ok(gw),
        Err(e) if refresh.is_ok() => return Err(format!("no Lightning gateway answers: {e:?}")),
        Err(_) => {}
    }
    let refresh_err = refresh.err().map(|e| format!("{e:?}")).unwrap_or_default();
    tracing::warn!("gateway refresh failed ({refresh_err}); asking the guardians one by one");
    gateway_from_any_peer(&ln.api)
        .await
        .ok_or_else(|| format!("reading the federation's gateways: {refresh_err}; no guardian listed a gateway on its own either"))
}

/// Ask each guardian for its gateway list on its own and pick a live gateway, vetted ones first.
async fn gateway_from_any_peer(api: &fedimint_api_client::api::DynModuleApi) -> Option<fedimint_ln_common::LightningGateway> {
    use fedimint_api_client::api::FederationApiExt as _;
    use fedimint_core::module::ApiRequestErased;
    use fedimint_ln_common::LightningGatewayAnnouncement;
    let asks = api.all_peers().iter().map(|peer| {
        let peer = *peer;
        async move {
            tokio::time::timeout(
                GATEWAY_PEER_TIMEOUT,
                api.request_single_peer::<Vec<LightningGatewayAnnouncement>>(
                    fedimint_ln_common::federation_endpoint_constants::LIST_GATEWAYS_ENDPOINT.to_string(),
                    ApiRequestErased::default(),
                    peer,
                ),
            )
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default()
        }
    });
    let mut all: Vec<LightningGatewayAnnouncement> = futures::future::join_all(asks).await.into_iter().flatten().collect();
    all.retain(|a| !a.ttl.is_zero());
    all.sort_by_key(|a| !a.vetted);
    all.into_iter().next().map(|a| a.info)
}

#[derive(Clone, Copy)]
struct FederationModules {
    lnv2: bool,
    meta: bool,
    walletv2: bool,
}

impl FederationModules {
    fn for_config(cfg: &ClientConfig, mint: MintVersion) -> Self {
        // Prefer the original Lightning module on mixed federations.
        let has_ln = cfg.modules.values().any(|m| m.is_kind(&fedimint_ln_common::KIND));
        Self {
            lnv2: !has_ln && Self::register::<fedimint_lnv2_client::common::LightningCommonInit>(cfg, mint),
            meta: Self::register::<fedimint_meta_client::common::MetaCommonInit>(cfg, mint),
            walletv2: Self::register::<fedimint_walletv2_client::common::WalletCommonInit>(cfg, mint),
        }
    }

    fn register<M: CommonModuleInit>(cfg: &ClientConfig, mint: MintVersion) -> bool {
        let mut advertised = cfg.modules.iter().filter(|(_, module)| module.is_kind(&M::KIND)).peekable();
        if advertised.peek().is_none() {
            return false;
        }
        // SDK decoders apply to every instance of a kind. For a v1 wallet, enable an
        // optional client when it can read all instances. V2 wallets use SDK negotiation.
        mint == MintVersion::V2
            || advertised.all(|(&id, module)| {
                let decoders: ModuleDecoderRegistry = [(id, M::KIND, M::decoder())].into_iter().collect();
                module.version.major == M::CONSENSUS_VERSION.major
                    && module.clone().redecode_raw(&decoders).is_ok_and(|cfg| cfg.cast::<M::ClientConfig>().is_ok())
            })
    }
}

/// The wallet's admission guard: every clone of a [`Wallet`] shares one.
///
/// - **Shared** ([`Self::debit`]): a wallet debit together with the record transition that goes
///   with it (a top-up's load and `loaded`, a pay's funding and its outcome, a refund load, a
///   card issue, an offline send), and the insert of a new liability (a pay's `charging`, a
///   top-up's `waiting`). Many at once.
/// - **Exclusive** ([`Self::exclusive`]): a merchant payout's final read of the balance and the
///   obligations together with its own funding, so no debit or liability change lands between
///   them.
///
/// Lock order: an outer lock of a caller (the admin payout lock, a pay's admission, a top-up's
/// create lock, the till's actor) comes first, this guard next, and the stores' `std` mutexes
/// last (held only without an await). Never acquire it while holding it: tokio's lock is fair,
/// so a nested shared acquire behind a waiting payout deadlocks. It is taken at the lowest level,
/// around the wallet call itself, and nothing awaited under it acquires it again.
#[derive(Clone, Debug, Default)]
pub struct SpendGuard(Arc<tokio::sync::RwLock<()>>);

impl SpendGuard {
    pub async fn debit(&self) -> tokio::sync::OwnedRwLockReadGuard<()> {
        self.0.clone().read_owned().await
    }

    pub async fn exclusive(&self) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        self.0.clone().write_owned().await
    }
}

#[derive(Clone)]
pub struct Wallet {
    pub client: ClientHandleArc,
    pub mint_id: ModuleInstanceId,
    pub mint_version: MintVersion,
    pub mint_cfg: CardMintConfig,
    /// Invite code saved by `join`, offered to the recipients of exported ecash.
    pub invite: Option<InviteCode>,
    /// The federation's core consensus version, from its client config.
    pub consensus_version: CoreConsensusVersion,
    /// Shared by every clone ([`SpendGuard`]).
    pub spend: SpendGuard,
}

/// The first core consensus version whose guardians take a transaction that pays more than its
/// outputs and fees (`fedimint-server/src/consensus/transaction.rs`, `OVERPAY_MIN_VERSION`).
pub const SURPLUS_MIN_VERSION: CoreConsensusVersion = CoreConsensusVersion::new(2, 1);

/// Result of reissuing external notes into the wallet.
#[derive(Debug, Clone, Copy)]
pub struct Imported {
    pub face: Amount,
    /// Federation fees the wallet paid for the reissue, as reported by the client.
    pub fee_msat: u64,
    /// The notes had been reissued before: nothing to do.
    pub already: bool,
}

impl Wallet {
    /// Wrap an already-built client (tests build theirs from the in-process federation).
    pub async fn from_client(client: ClientHandleArc) -> Result<Self> {
        let cfg = client.config().await;
        let (mint_version, mint_id, mint_cfg) = CardMintConfig::for_client(&client, &cfg)?;
        Ok(Self {
            client,
            mint_id,
            mint_version,
            mint_cfg,
            invite: None,
            consensus_version: cfg.global.consensus_version,
            spend: SpendGuard::default(),
        })
    }

    /// The federation's fee schedules, read from its modules' own fee functions
    /// (`crate::payout::fees`). `Err` when the mint's cannot be bounded: nothing may be spent on a
    /// bound then. A Lightning schedule that cannot be bounded is `ln: None` with the reason in
    /// `ln_problem`: Lightning funding is refused for it.
    pub async fn fee_schedules(&self) -> std::result::Result<crate::payout::fees::Schedules, String> {
        use crate::payout::fees::{Linear, LnSchedule, Schedules};
        let mint = Linear::from_fee_fn(|x| self.fee(x)).map_err(|e| format!("the mint's fees cannot be bounded: {e}"))?;
        let cfg = self.client.config().await;
        let (ln, ln_problem) = if self.lightning().is_ok() {
            match cfg.get_first_module_by_kind::<fedimint_ln_common::config::LightningClientConfig>(fedimint_ln_common::KIND) {
                Ok((_, c)) => (
                    Some(LnSchedule::V1 {
                        contract_input_msat: c.fee_consensus.contract_input.msats,
                        contract_output_msat: c.fee_consensus.contract_output.msats,
                    }),
                    None,
                ),
                Err(_) => (None, Some("the ln module's configuration cannot be read".to_owned())),
            }
        } else if self.lightning_v2().is_ok() {
            match cfg
                .get_first_module_by_kind::<fedimint_lnv2_client::common::config::LightningClientConfig>(fedimint_lnv2_client::common::KIND)
            {
                Ok((_, c)) => {
                    let f = c.fee_consensus.clone();
                    match Linear::from_fee_fn(|x| f.fee(Amount::from_msats(x)).msats) {
                        Ok(l) => (Some(LnSchedule::V2(l)), None),
                        Err(e) => (None, Some(format!("the lnv2 module's fees cannot be bounded: {e}"))),
                    }
                }
                Err(_) => (None, Some("the lnv2 module's configuration cannot be read".to_owned())),
            }
        } else {
            (None, None)
        };
        let min_change_msat = match self.mint_version {
            MintVersion::V1 => 1,
            MintVersion::V2 => self.min_denom(),
        };
        Ok(Schedules { mint, min_change_msat, ln, ln_problem })
    }

    /// The client's own quote of the federation fees of funding an outgoing contract worth
    /// `contract_msat` now (`send_fee_quote` of ln, else lnv2: a dry run over the inventory).
    pub async fn ln_send_fee_quote(&self, contract_msat: u64) -> Result<u64> {
        let amount = Amount::from_msats(contract_msat);
        if let Ok(ln) = self.lightning() {
            return Ok(ln.send_fee_quote(amount).await?.total().get_bitcoin().msats);
        }
        if let Ok(ln) = self.lightning_v2() {
            return Ok(ln.send_fee_quote(amount).await?.total().get_bitcoin().msats);
        }
        bail!("this federation has no Lightning module the till can use")
    }

    /// What the ln receive `operation_id` (hex) came to, from the client's own state machine,
    /// followed for up to `wait`: `Some(false)` when it ended canceled (terminal: nothing was or
    /// will be credited), `Some(true)` when it was claimed (the value is in the wallet), `None`
    /// while it could still be paid or cannot be told.
    pub async fn ln_receive_settled(&self, operation_id: &str, wait: Duration) -> Result<Option<bool>> {
        use fedimint_ln_client::LnReceiveState;
        let bytes: [u8; 32] = hex::decode(operation_id.trim()).ok().and_then(|b| b.try_into().ok()).context("not an operation id")?;
        if self.lightning().is_err() {
            use fedimint_lnv2_client::FinalReceiveOperationState;
            let ln = self.lightning_v2()?;
            let result = tokio::time::timeout(wait, ln.await_final_receive_operation_state(OperationId(bytes))).await;
            return Ok(match result {
                Ok(Ok(FinalReceiveOperationState::Claimed)) => Some(true),
                Ok(Ok(FinalReceiveOperationState::Expired | FinalReceiveOperationState::Uneconomical)) => Some(false),
                Ok(Ok(FinalReceiveOperationState::Failure)) => bail!("lnv2 could not settle the incoming payment"),
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => None,
            });
        }
        let ln = self.lightning()?;
        let mut stream = ln.subscribe_ln_receive(OperationId(bytes)).await?.into_stream();
        let mut last = None;
        let _ = tokio::time::timeout(wait, async {
            while let Some(state) = stream.next().await {
                let end = matches!(state, LnReceiveState::Canceled { .. } | LnReceiveState::Claimed);
                last = Some(state);
                if end {
                    break;
                }
            }
        })
        .await;
        Ok(match last {
            Some(LnReceiveState::Canceled { .. }) => Some(false),
            Some(LnReceiveState::Claimed) => Some(true),
            _ => None,
        })
    }

    /// The mint's own quote of the fees `send_oob_notes` pays for `amount_msat` now.
    pub async fn export_fee_quote(&self, amount_msat: u64) -> Result<u64> {
        let amount = Amount::from_msats(amount_msat);
        let quote = match self.mint_version {
            MintVersion::V1 => self.mint()?.send_fee_quote(amount).await?,
            MintVersion::V2 => self.mint_v2()?.send_fee_quote(amount).await?,
        };
        Ok(quote.total().get_bitcoin().msats)
    }

    /// The federation fees operation `operation_id` (hex) paid, from the client's own record.
    pub async fn operation_fees_msat(&self, operation_id: &str) -> Result<Option<u64>> {
        let bytes: [u8; 32] = hex::decode(operation_id.trim()).ok().and_then(|b| b.try_into().ok()).context("not an operation id")?;
        Ok(self.client.get_operation_fees(OperationId(bytes)).await.ok().flatten().map(|a| a.get_bitcoin().msats))
    }

    /// Join the federation behind `invite` (first run). Creates the mnemonic file if missing.
    pub async fn join(data_dir: &Path, invite: &str) -> Result<Self> {
        let invite_code = InviteCode::from_str(invite).context("not a valid invite code")?;
        ensure!(!Self::joined(data_dir).await?, "this wallet has already joined a federation");
        let endpoints = connectors().await;
        let mut preview = Client::builder()
            .await
            .with_module_for_join()
            .preview(endpoints.clone(), &invite_code)
            .await
            .context("downloading federation config from the invite code")?;
        let (mint, _) = mint::bitcoin_mint(preview.config())?;
        let modules = FederationModules::for_config(preview.config(), mint);
        if mint == MintVersion::V2 || modules.lnv2 || modules.meta || modules.walletv2 {
            preview = Client::builder()
                .await
                .with_module_for_join()
                .with_federation_modules(mint, modules)
                .preview(endpoints, &invite_code)
                .await
                .context("downloading federation config from the invite code")?;
        }
        crate::state::create_private_dir(data_dir)?;
        let mnemonic = load_or_create_mnemonic(data_dir)?;
        let db = open_db(data_dir).await?;
        let client = preview.join(db, root_secret(&mnemonic)).await.context("joining the federation")?;
        std::fs::write(data_dir.join(INVITE_FILE), invite)?;
        // Joined with every module registered: the cached API versions already cover them.
        let _ = std::fs::write(data_dir.join(LN_VERSIONS_MARKER), b"ln module registered at join\n");
        let _ = std::fs::write(data_dir.join(LNV2_VERSIONS_MARKER), b"lnv2 decided at join\n");
        let mut w = Self::from_client(Arc::new(client)).await?;
        let _ = std::fs::write(data_dir.join(CARD40_MODULES_MARKER), b"current client modules negotiated\n");
        w.invite = Some(invite_code);
        Ok(w)
    }

    /// A failed join can leave a mnemonic and an empty RocksDB. Only a stored federation
    /// configuration means a join committed. An established installation missing its seed or
    /// database needs repair, so refuse to initialize a replacement wallet in its place.
    pub(crate) async fn joined(data_dir: &Path) -> Result<bool> {
        let database = data_dir.join(DB_DIR).try_exists()?;
        let invite = data_dir.join(INVITE_FILE).try_exists()?;
        if !database {
            ensure!(!invite, "the federation invite exists but the wallet database is missing");
            return Ok(false);
        }
        ensure!(data_dir.join(MNEMONIC_FILE).try_exists()?, "the wallet database exists but the mnemonic is missing");
        let db = open_db(data_dir).await?;
        let joined = Client::get_config_from_db(&db).await.is_some();
        ensure!(joined || !invite, "the federation invite exists but the wallet database has no federation config");
        Ok(joined)
    }

    /// Re-open a client joined earlier.
    pub async fn open(data_dir: &Path) -> Result<Self> {
        let mnemonic =
            read_mnemonic(data_dir).with_context(|| format!("no wallet in {}: run `join <invite>` first", data_dir.display()))?;
        let db = open_db(data_dir).await?;
        let cfg = Client::get_config_from_db(&db).await.context("wallet database has no federation config")?;
        let (mint, mint_id) = mint::bitcoin_mint(&cfg)?;
        let modules = FederationModules::for_config(&cfg, mint);
        // A wallet that joined before the ln module was registered has a cached API version set
        // without it, and the client skips any module missing from that set. Drop the cache once
        // (marker file) so the client fetches the versions afresh, ln included, as it does at join.
        // The same once more for lnv2, on the federations that get it.
        let marker = data_dir.join(LN_VERSIONS_MARKER);
        let marker_v2 = data_dir.join(LNV2_VERSIONS_MARKER);
        let current_modules = data_dir.join(CARD40_MODULES_MARKER);
        let uses_new_modules = mint == MintVersion::V2 || modules.meta || modules.walletv2;
        let refetch = !marker.exists() || (modules.lnv2 && !marker_v2.exists()) || (uses_new_modules && !current_modules.exists());
        {
            use fedimint_client::db::CachedApiVersionSetKey;
            use fedimint_core::db::IDatabaseTransactionOpsCoreTyped as _;
            let mut dbtx = db.begin_transaction().await;
            if let Some(cached) = dbtx.get_value(&CachedApiVersionSetKey).await
                && (refetch || !cached.0.modules.contains_key(&mint_id))
            {
                dbtx.remove_entry(&CachedApiVersionSetKey).await;
                dbtx.commit_tx_result().await.context("clearing the cached API versions")?;
            }
        }
        let builder = Client::builder().await.with_module_for_join().with_federation_modules(mint, modules);
        let client = builder.open(connectors().await, db, root_secret(&mnemonic)).await.context("opening the client database")?;
        if !marker.exists() {
            let _ = std::fs::write(&marker, b"ln module registered; API versions refetched once\n");
        }
        if modules.lnv2 && !marker_v2.exists() {
            let _ = std::fs::write(&marker_v2, b"lnv2 module registered; API versions refetched once\n");
        }
        let mut w = Self::from_client(Arc::new(client)).await?;
        if uses_new_modules && !current_modules.exists() {
            let _ = std::fs::write(current_modules, b"current client modules negotiated\n");
        }
        w.invite = std::fs::read_to_string(data_dir.join(INVITE_FILE)).ok().and_then(|s| InviteCode::from_str(s.trim()).ok());
        Ok(w)
    }

    pub fn federation_id(&self) -> FederationId {
        self.client.federation_id()
    }

    pub async fn federation_name(&self) -> Option<String> {
        let cfg = self.client.config().await;
        let fallback = cfg.global.meta.get("federation_name").cloned();
        if !cfg.modules.values().any(|m| m.is_kind(&fedimint_meta_client::common::KIND)) {
            return fallback;
        }
        let field = self.client.meta_service().get_field::<String>(self.client.db(), "federation_name");
        tokio::time::timeout(Duration::from_millis(500), field).await.ok().flatten().and_then(|m| m.value).or(fallback)
    }

    /// The federation's ln module: the till's top-up invoices (docs/TOPUP.md).
    pub fn lightning(&self) -> Result<fedimint_client::module::ClientModuleInstance<'_, fedimint_ln_client::LightningClientModule>> {
        Ok(self.client.get_first_module::<fedimint_ln_client::LightningClientModule>()?)
    }

    /// The federation's lnv2 module, registered only where there is no ln module.
    pub fn lightning_v2(&self) -> Result<fedimint_client::module::ClientModuleInstance<'_, fedimint_lnv2_client::LightningClientModule>> {
        Ok(self.client.get_first_module::<fedimint_lnv2_client::LightningClientModule>()?)
    }

    /// `ln pay`: pay `bolt11` from the wallet's balance through a gateway of the federation's ln
    /// module, else of its lnv2 module, and follow it for up to `LN_PAY_WAIT`. With
    /// `max_fee_msat`, a gateway asking a larger fee for this invoice is refused before anything is
    /// spent. Paying an invoice again answers an earlier payment that is paid or not settled, and
    /// never funds a second one while one could still pay; only after a certain failure (refunded)
    /// does it start a new attempt. Errors for an invoice that does not parse or has no amount,
    /// and when the wallet's earlier payments of it cannot be read.
    pub async fn pay_bolt11(&self, bolt11: &str, max_fee_msat: Option<u64>) -> Result<LnPayOutcome> {
        self.pay_bolt11_within(bolt11, max_fee_msat, LN_PAY_WAIT).await
    }

    /// `pay_bolt11`, following the payment for up to `wait` before it answers `Pending`.
    async fn pay_bolt11_within(&self, bolt11: &str, max_fee_msat: Option<u64>, wait: Duration) -> Result<LnPayOutcome> {
        let invoice = parse_bolt11(bolt11)?;
        if self.lightning().is_ok() {
            // ln refuses a new attempt only while the last one is paid or running; one that ended
            // in an error it could not settle leaves a funded contract, and a second attempt would
            // fund another for the same payment hash.
            if let Some(earlier) = self.earlier_v1(&invoice, wait).await? {
                return Ok(earlier);
            }
            return Ok(self.pay_v1(invoice, max_fee_msat, wait).await);
        }
        if self.lightning_v2().is_ok() {
            return Ok(self.pay_v2(invoice, max_fee_msat, wait).await);
        }
        Ok(LnPayOutcome::refused("this federation has no Lightning module the till can use"))
    }

    async fn pay_v1(&self, invoice: Bolt11Invoice, max_fee_msat: Option<u64>, wait: Duration) -> LnPayOutcome {
        let Ok(ln) = self.lightning() else { return LnPayOutcome::refused("no ln module") };
        let amount_msat = invoice.amount_milli_satoshis().unwrap_or(0);
        // Without a gateway only an invoice of this federation can be paid: the client pays it
        // inside the federation with no gateway fee, and refuses any other invoice
        // (`NoLnGatewayAvailable`) before anything is funded.
        let (gw, no_gateway) = match lnv1_gateway(&ln, &invoice).await {
            Ok(gw) => (Some(gw), None),
            Err(reason) => (None, Some(reason)),
        };
        let fee_msat = gw.as_ref().map_or(0, |gw| gw.fees.to_amount(&Amount::from_msats(amount_msat)).msats);
        if let Some(r) = refuse_fee_or_balance(fee_msat, max_fee_msat, amount_msat, self.balance_msat().await.ok()) {
            return r;
        }
        let out = match ln.pay_bolt11_invoice(gw, invoice, ()).await {
            Ok(out) => out,
            Err(PayBolt11InvoiceError::PreviousPaymentAttemptStillInProgress { operation_id }) => {
                return self.follow_v1(operation_id, wait).await.unwrap_or_else(|e| unsettled(operation_id, e));
            }
            Err(PayBolt11InvoiceError::NoLnGatewayAvailable) => {
                return LnPayOutcome::no_gateway(no_gateway.unwrap_or_else(|| "no Lightning gateway".into()));
            }
            Err(e) => {
                let insufficient_funds =
                    matches!(e, PayBolt11InvoiceError::Transaction(fedimint_client::module::TransactionSubmitError::InsufficientFunds(_)));
                return LnPayOutcome::Refused { reason: e.to_string(), insufficient_funds, retryable: false };
            }
        };
        let op = out.payment_type.operation_id();
        self.follow_v1(op, wait).await.unwrap_or_else(|e| unsettled(op, e))
    }

    /// Follow the ln payment `op` until it ends or `wait` passes. The fee and the kind of payment
    /// (through a gateway, or inside the federation) come from the operation's own record, so a
    /// payment picked up again (an earlier attempt, after a restart) answers what it was funded
    /// with. Errors when `op` is not an ln payment of this wallet.
    async fn follow_v1(&self, op: OperationId, wait: Duration) -> Result<LnPayOutcome> {
        let operation_id = hex::encode(op.0);
        let ln = self.lightning()?;
        let details = ln
            .get_ln_pay_details_for(op)
            .await
            .map_err(|e| anyhow!("operation {operation_id} is not a Lightning payment of this wallet: {e}"))?;
        let fee_msat = details.fee.msats;
        let failed = |reason: String| LnPayOutcome::Failed { reason, operation_id: operation_id.clone() };
        let refunded = |reason: String| LnPayOutcome::Refunded { reason, operation_id: operation_id.clone() };
        let follow = async {
            if !details.is_internal_payment {
                let mut stream = match ln.subscribe_ln_pay(op).await {
                    Ok(s) => s.into_stream(),
                    Err(e) => return failed(e.to_string()),
                };
                while let Some(state) = stream.next().await {
                    match state {
                        LnPayState::Success { preimage } => {
                            return LnPayOutcome::Paid { preimage, fee_msat, operation_id: operation_id.clone() };
                        }
                        LnPayState::Refunded { gateway_error } => return refunded(gateway_error.to_string()),
                        LnPayState::Canceled => return refunded("canceled: the federation did not fund the payment".into()),
                        LnPayState::UnexpectedError { error_message } => return failed(error_message),
                        LnPayState::Created
                        | LnPayState::Funded { .. }
                        | LnPayState::WaitingForRefund { .. }
                        | LnPayState::AwaitingChange => {}
                    }
                }
            } else {
                let mut stream = match ln.subscribe_internal_pay(op).await {
                    Ok(s) => s.into_stream(),
                    Err(e) => return failed(e.to_string()),
                };
                while let Some(state) = stream.next().await {
                    match state {
                        InternalPayState::Preimage(p) => {
                            return LnPayOutcome::Paid { preimage: hex::encode(p.0), fee_msat, operation_id: operation_id.clone() };
                        }
                        InternalPayState::RefundSuccess { error, .. } => return refunded(format!("{error:?}")),
                        InternalPayState::FundingFailed { error } => {
                            return refunded(format!("the federation did not fund the payment: {error:?}"));
                        }
                        InternalPayState::RefundError { error_message, .. } | InternalPayState::UnexpectedError(error_message) => {
                            return failed(error_message);
                        }
                        InternalPayState::Funding => {}
                    }
                }
            }
            failed("the payment ended without an outcome".into())
        };
        Ok(tokio::time::timeout(wait, follow).await.unwrap_or_else(|_| LnPayOutcome::Pending { operation_id: operation_id.clone() }))
    }

    async fn pay_v2(&self, invoice: Bolt11Invoice, max_fee_msat: Option<u64>, wait: Duration) -> LnPayOutcome {
        use fedimint_lnv2_client::{SendPaymentError, SendWithTermsError};
        let Ok(ln) = self.lightning_v2() else { return LnPayOutcome::refused("no lnv2 module") };
        let amount_msat = invoice.amount_milli_satoshis().unwrap_or(0);
        let (gateway, routing) = match ln.select_gateway(Some(invoice.clone())).await {
            Ok(g) => g,
            Err(e) => return LnPayOutcome::no_gateway(format!("no Lightning gateway answers: {e}")),
        };
        let (send_fee, expiration_delta) = routing.send_parameters(&invoice);
        let fee_msat = send_fee.fee(amount_msat).msats;
        if let Some(r) = refuse_fee_or_balance(fee_msat, max_fee_msat, amount_msat, self.balance_msat().await.ok()) {
            return r;
        }
        // `send_with_terms`: the contract carries the fee just checked, or nothing is funded.
        let op = match ln.send_with_terms(invoice, gateway, send_fee, expiration_delta, serde_json::Value::Null).await {
            Ok(op) => op,
            Err(SendWithTermsError::Send(SendPaymentError::DuplicatePaymentAttempt(op))) => op,
            Err(e) => {
                let insufficient_funds = matches!(e, SendWithTermsError::Send(SendPaymentError::InsufficientFunds(_)));
                return LnPayOutcome::Refused { reason: e.to_string(), insufficient_funds, retryable: false };
            }
        };
        self.follow_v2(op, wait).await.unwrap_or_else(|e| unsettled(op, e))
    }

    /// Follow the lnv2 payment `op` until it ends or `wait` passes, with the gateway fee its
    /// contract was funded with. Errors when `op` is not an lnv2 payment of this wallet.
    async fn follow_v2(&self, op: OperationId, wait: Duration) -> Result<LnPayOutcome> {
        use fedimint_lnv2_client::{FinalSendOperationState, LightningOperationMeta};
        let operation_id = hex::encode(op.0);
        let ln = self.lightning_v2()?;
        let meta = self.client.operation_log().get_operation(op).await.map(|e| e.try_meta::<LightningOperationMeta>());
        let Some(Ok(LightningOperationMeta::Send(meta))) = meta else {
            bail!("operation {operation_id} is not a Lightning payment of this wallet");
        };
        let fee_msat = meta.gateway_fee().msats;
        Ok(match tokio::time::timeout(wait, ln.await_final_send_operation_state(op)).await {
            Err(_) => LnPayOutcome::Pending { operation_id },
            Ok(Err(e)) => LnPayOutcome::Failed { reason: e.to_string(), operation_id },
            Ok(Ok(FinalSendOperationState::Success(preimage))) => {
                LnPayOutcome::Paid { preimage: hex::encode(preimage), fee_msat, operation_id }
            }
            Ok(Ok(FinalSendOperationState::Refunded)) => {
                LnPayOutcome::Refunded { reason: "the gateway could not pay; refunded".into(), operation_id }
            }
            // The refund was not accepted and no preimage turned up: where the value is, the
            // client cannot say. Never a certain failure.
            Ok(Ok(FinalSendOperationState::Failure)) => {
                LnPayOutcome::Failed { reason: "the federation reported a failed payment".into(), operation_id }
            }
        })
    }

    /// What paying `bolt11` would cost in gateway fees now, asked before anything is funded or
    /// charged: the fee [`Self::pay_bolt11`] checks against its cap, through the gateway it would
    /// pick, and 0 for an invoice of this federation without a gateway (ln pays it inside the
    /// federation). Errors when no gateway answers for the invoice now, or the federation has no
    /// Lightning module the till can use: the invoice cannot be paid now. A gateway may still
    /// change its fee before the payment, which the payment's own check refuses.
    pub async fn quote_invoice(&self, bolt11: &str) -> Result<u64> {
        let invoice = parse_bolt11(bolt11)?;
        let amount_msat = invoice.amount_milli_satoshis().unwrap_or(0);
        if let Ok(ln) = self.lightning() {
            return match lnv1_gateway(&ln, &invoice).await {
                Ok(gw) => Ok(gw.fees.to_amount(&Amount::from_msats(amount_msat)).msats),
                // As `pay_v1` without a gateway: only an invoice of this federation is paid.
                Err(reason) => {
                    let markers = self.client.get_internal_payment_markers().map_err(|e| anyhow!("{e}"))?;
                    let last_hop =
                        invoice.route_hints().first().and_then(|rh| rh.0.last()).map(|hop| (hop.src_node_id, hop.short_channel_id));
                    let internal = last_hop == Some(markers)
                        || ln.list_gateways().await.iter().any(|g| last_hop == Some((g.info.node_pub_key, g.info.federation_index)));
                    if internal { Ok(0) } else { Err(anyhow!(reason)) }
                }
            };
        }
        if let Ok(ln) = self.lightning_v2() {
            let (_, routing) = ln.select_gateway(Some(invoice.clone())).await.map_err(|e| anyhow!("no Lightning gateway answers: {e}"))?;
            return Ok(routing.send_parameters(&invoice).0.fee(amount_msat).msats);
        }
        bail!("this federation has no Lightning module the till can use")
    }

    pub fn mint(&self) -> Result<fedimint_client::module::ClientModuleInstance<'_, MintClientModule>> {
        Ok(self.client.get_first_module::<MintClientModule>()?)
    }

    /// Federation fee for one input or output of `msat`, in msat.
    pub fn fee(&self, msat: u64) -> u64 {
        self.mint_cfg.fee_consensus.fee(Amount::from_msats(msat)).msats
    }

    pub async fn balance_msat(&self) -> Result<u64> {
        Ok(self.client.get_balance_for_btc().await?.msats)
    }

    /// Whether the federation takes a surplus (core consensus 2.1 or later): what lets an offline
    /// charge carry a fee margin (`plan::solve_with_margin`).
    pub fn surplus_allowed(&self) -> bool {
        self.consensus_version >= SURPLUS_MIN_VERSION
    }

    /// The smallest denomination that the selected mint's wallet issues.
    pub fn min_denom(&self) -> u64 {
        self.mint_cfg.fee_consensus.min_economical_denomination().msats
    }

    /// `ecash import`: reissue an external OOBNotes string into this wallet and wait until done.
    pub async fn import(&self, notes: &str) -> Result<Imported> {
        match self.mint_version {
            MintVersion::V1 => {
                let oob = OOBNotes::from_str(notes).map_err(|e| anyhow!("not an ecash string: {e}"))?;
                self.import_notes(oob).await
            }
            MintVersion::V2 => {
                let ecash = fedimint_core::base32::decode_prefixed(fedimint_core::base32::FEDIMINT_PREFIX, notes.trim())
                    .context("invalid mint v2 eCash")?;
                self.import_v2_notes(ecash).await
            }
        }
    }

    /// Reissue notes into the wallet. Notes that were already reissued count as done.
    pub async fn import_notes(&self, oob: OOBNotes) -> Result<Imported> {
        let mint = self.mint()?;
        let face = oob.total_amount();
        let op = match mint.reissue_external_notes(oob, ()).await {
            Ok(op) => op,
            Err(ReissueExternalNotesError::AlreadyReissued) => {
                return Ok(Imported { face, fee_msat: 0, already: true });
            }
            Err(e) => return Err(e.into()),
        };
        let mut stream = mint.subscribe_reissue_external_notes(op).await?.into_stream();
        let wait = async {
            while let Some(state) = stream.next().await {
                match state {
                    ReissueExternalNotesState::Done => return Ok(()),
                    ReissueExternalNotesState::Failed(e) => bail!("reissue failed: {e}"),
                    ReissueExternalNotesState::Created | ReissueExternalNotesState::Issuing => {}
                }
            }
            bail!("reissue stream ended early")
        };
        tokio::time::timeout(Duration::from_secs(120), wait).await.context("reissue timed out")??;
        let fee_msat = self.client.get_operation_fees(op).await.ok().flatten().map(|a| a.get_bitcoin().msats).unwrap_or(0);
        Ok(Imported { face, fee_msat, already: false })
    }

    /// `ecash export`: take notes worth exactly `amount_msat` out of the wallet with the stock
    /// `send_oob_notes` (it reissues the wallet exact denominations when needed). The stock call
    /// rounds the amount up to a multiple of the smallest economical note; this refuses to do
    /// that silently unless `allow_round_up`.
    ///
    /// Note: at this pin `send_oob_notes` records the spend with no timeout, so unlike
    /// `spend_notes_with_selector(.., try_cancel_after, ..)` there is no automatic refund if the
    /// recipient never reissues. Keep the printed string until it is redeemed.
    pub async fn export(&self, amount_msat: u64, allow_round_up: bool, include_invite: bool) -> Result<(OOBNotes, Amount)> {
        let mint = self.mint()?;
        let want = Amount::from_msats(amount_msat);
        let rounded = self.mint_cfg.fee_consensus.checked_round_up(want).context("eCash amount overflow")?;
        if rounded != want && !allow_round_up {
            bail!(
                "{amount_msat} msat is not a multiple of {} msat, the federation's smallest economical note; \
                 the export would be {} msat. Ask for exactly that, or pass --round-up",
                self.min_denom(),
                rounded.msats
            );
        }
        let notes = mint.send_oob_notes(rounded, ()).await?;
        let face = notes.total_amount();
        ensure!(face == rounded, "wallet handed out {} msat for a request of {} msat", face.msats, rounded.msats);
        let notes = match (&self.invite, include_invite) {
            (Some(invite), true) => OOBNotes::new_with_invite(notes.notes().clone(), invite),
            _ => notes,
        };
        Ok((notes, face))
    }

    /// Largest amount, a multiple of the smallest economical note, that `send_oob_notes` can pay
    /// out of `available` msat once its own fees are counted.
    pub async fn max_exportable(&self, available: u64) -> Result<u64> {
        let m = self.min_denom();
        let mut a = available / m * m;
        while a > 0 {
            let fee = self.export_fee_quote(a).await?;
            if fee <= available - a {
                return Ok(a);
            }
            a -= m;
        }
        Ok(0)
    }
}

/// A payment the wallet funded (or found in flight) whose record could not be read back: its
/// outcome cannot be settled from here, so it is never reported as refused.
fn unsettled(op: OperationId, e: anyhow::Error) -> LnPayOutcome {
    LnPayOutcome::Failed { reason: format!("{e:#}"), operation_id: hex::encode(op.0) }
}

/// The refusals checked before a payment is funded: a gateway fee over `max_fee_msat`, or a
/// balance (when known) below the invoice plus that fee. The federation's own fees come on top, so
/// a balance just above that can still be refused by the client.
fn refuse_fee_or_balance(fee_msat: u64, max_fee_msat: Option<u64>, amount_msat: u64, balance_msat: Option<u64>) -> Option<LnPayOutcome> {
    if let Some(max) = max_fee_msat
        && fee_msat > max
    {
        return Some(LnPayOutcome::refused(format!("the gateway asks {fee_msat} msat in fees, more than --max-fee-msat {max}")));
    }
    let need = amount_msat.saturating_add(fee_msat);
    match balance_msat {
        Some(b) if b < need => Some(LnPayOutcome::Refused {
            reason: format!("the wallet holds {b} msat; the invoice and the gateway's fee need {need} msat"),
            insufficient_funds: true,
            retryable: false,
        }),
        _ => None,
    }
}

/// Register the original mint and Lightning modules, then the modules selected for this federation.
trait BuilderExt {
    fn with_module_for_join(self) -> fedimint_client::ClientBuilder;
    fn with_federation_modules(self, mint: MintVersion, modules: FederationModules) -> fedimint_client::ClientBuilder;
}
impl BuilderExt for fedimint_client::ClientBuilder {
    fn with_module_for_join(mut self) -> fedimint_client::ClientBuilder {
        self.with_module(MintClientInit);
        self.with_module(fedimint_ln_client::LightningClientInit::default());
        self
    }

    fn with_federation_modules(mut self, mint: MintVersion, modules: FederationModules) -> fedimint_client::ClientBuilder {
        if mint == MintVersion::V2 {
            self.with_module(fedimint_mintv2_client::MintClientInit);
        }
        if modules.lnv2 {
            self.with_module(fedimint_lnv2_client::LightningClientInit::default());
        }
        if modules.meta {
            self.with_module(fedimint_meta_client::MetaClientInit);
            let source: fedimint_meta_client::MetaModuleMetaSourceWithFallback = Default::default();
            self.with_meta_service(fedimint_client::meta::MetaService::new(source));
        }
        if modules.walletv2 {
            self.with_module(fedimint_walletv2_client::WalletClientInit);
        }
        self
    }
}

async fn connectors() -> ConnectorRegistry {
    ConnectorRegistry::build_from_client_defaults().bind().await
}

async fn open_db(data_dir: &Path) -> Result<Database> {
    Ok(fedimint_rocksdb::RocksDb::build(data_dir.join(DB_DIR)).open().await.context("opening RocksDB")?.into())
}

fn root_secret(mnemonic: &Mnemonic) -> RootSecret {
    RootSecret::StandardDoubleDerive(Bip39RootSecretStrategy::<12>::to_root_secret(mnemonic))
}

fn parse_mnemonic(words: &str) -> Result<Mnemonic> {
    Mnemonic::from_str(words.trim()).context("mnemonic file is not a valid BIP39 phrase")
}

/// Read the mnemonic, or create it if (and only if) the file does not exist. An existing file
/// that cannot be read or parsed is an error: it is never overwritten.
fn load_or_create_mnemonic(data_dir: &Path) -> Result<Mnemonic> {
    let path = data_dir.join(MNEMONIC_FILE);
    match std::fs::read_to_string(&path) {
        Ok(words) => return parse_mnemonic(&words).with_context(|| path.display().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    }
    let m = Bip39RootSecretStrategy::<12>::random(&mut thread_rng());
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&path).with_context(|| format!("creating {}", path.display()))?;
    std::io::Write::write_all(&mut f, m.to_string().as_bytes())?;
    f.sync_all()?;
    Ok(m)
}

fn read_mnemonic(data_dir: &Path) -> Result<Mnemonic> {
    let words = std::fs::read_to_string(data_dir.join(MNEMONIC_FILE))?;
    parse_mnemonic(&words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_modules_preflight_every_instance_for_v1() {
        use fedimint_core::config::{ClientModuleConfig, GlobalClientConfig};
        use fedimint_core::encoding::DynRawFallback;
        use fedimint_lnv2_client::common::LightningCommonInit;
        use fedimint_meta_client::common::{MetaCommonInit, config::MetaClientConfig};

        let mut config = ClientConfig {
            global: GlobalClientConfig {
                api_endpoints: Default::default(),
                broadcast_public_keys: None,
                consensus_version: CoreConsensusVersion::new(2, 1),
                meta: Default::default(),
            },
            modules: [(4, ClientModuleConfig::from_typed(4, MetaCommonInit::KIND, MetaCommonInit::CONSENSUS_VERSION, MetaClientConfig))]
                .into_iter()
                .collect(),
        };
        assert!(FederationModules::for_config(&config, MintVersion::V1).meta);
        config.modules.insert(
            5,
            ClientModuleConfig {
                kind: MetaCommonInit::KIND,
                version: MetaCommonInit::CONSENSUS_VERSION,
                config: DynRawFallback::Raw { module_instance_id: 5, raw: vec![1] },
            },
        );
        config.modules.insert(
            6,
            ClientModuleConfig {
                kind: LightningCommonInit::KIND,
                version: LightningCommonInit::CONSENSUS_VERSION,
                config: DynRawFallback::Raw { module_instance_id: 6, raw: vec![] },
            },
        );
        let modules = FederationModules::for_config(&config, MintVersion::V1);
        assert!(!modules.meta && !modules.lnv2);
        let strict = FederationModules::for_config(&config, MintVersion::V2);
        assert!(strict.meta && strict.lnv2);
    }

    #[test]
    fn bolt11_parsing_refuses_garbage_and_amountless_invoices() {
        assert!(parse_bolt11("").unwrap_err().to_string().contains("not a BOLT11 invoice"));
        assert!(parse_bolt11("lnbc1notaninvoice").unwrap_err().to_string().contains("not a BOLT11 invoice"));
        let ecash = "AwEEnUrjgAD7A9WAX0W3SV4UHHmTPOTRPIEoIM5xs6jZLyxj";
        assert!(parse_bolt11(ecash).unwrap_err().to_string().contains("not a BOLT11 invoice"));
    }

    #[test]
    fn fee_and_balance_are_checked_before_paying() {
        assert_eq!(refuse_fee_or_balance(1_000, Some(1_000), 10_000, Some(11_000)), None);
        assert_eq!(refuse_fee_or_balance(1_000, None, 10_000, None), None);
        let Some(LnPayOutcome::Refused { reason, insufficient_funds: false, retryable: false }) =
            refuse_fee_or_balance(1_001, Some(1_000), 10_000, Some(1 << 30))
        else {
            panic!("a fee over the cap is refused")
        };
        assert!(reason.contains("--max-fee-msat 1000"), "{reason}");
        assert!(matches!(
            refuse_fee_or_balance(1_000, None, 10_000, Some(10_999)),
            Some(LnPayOutcome::Refused { insufficient_funds: true, .. })
        ));
    }

    #[test]
    fn mnemonic_is_created_once_with_private_mode_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create_mnemonic(dir.path()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(MNEMONIC_FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(load_or_create_mnemonic(dir.path()).unwrap().to_string(), first.to_string());

        // A file that does not parse is an error and is left exactly as it was.
        let path = dir.path().join(MNEMONIC_FILE);
        std::fs::write(&path, "not a mnemonic").unwrap();
        assert!(load_or_create_mnemonic(dir.path()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not a mnemonic");
    }
}
