//! The host's own Fedimint client: join by invite code, re-open, and the stock mint v1 flows
//! (`ecash import` / `ecash export`). Only the mint module is registered.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use fedimint_bip39::{Bip39RootSecretStrategy, Mnemonic};
use fedimint_client::secret::RootSecretStrategy;
use fedimint_client::{Client, ClientHandleArc, RootSecret};
use fedimint_connectors::ConnectorRegistry;
use fedimint_core::config::FederationId;
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::Database;
use fedimint_core::invite_code::InviteCode;
use fedimint_core::secp256k1::rand::thread_rng;
use fedimint_core::Amount;
use fedimint_mint_client::{
    MintClientInit, MintClientModule, OOBNotes, ReissueExternalNotesState, SelectNotesWithAtleastAmount,
    SelectNotesWithExactAmount, SpendOOBState,
};
use fedimint_mint_common::config::MintClientConfig;
use futures::StreamExt as _;

/// File names inside the data directory.
pub const MNEMONIC_FILE: &str = "mnemonic";
pub const INVITE_FILE: &str = "invite";
pub const DB_DIR: &str = "client.db";
pub const STATE_FILE: &str = "card_state.json";

pub fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join(STATE_FILE)
}

/// A joined federation client plus what the card glue needs from the mint module.
#[derive(Clone)]
pub struct Wallet {
    pub client: ClientHandleArc,
    pub mint_id: ModuleInstanceId,
    pub mint_cfg: MintClientConfig,
}

impl Wallet {
    /// Wrap an already-built client (tests build theirs from the in-process federation).
    pub async fn from_client(client: ClientHandleArc) -> Result<Self> {
        let cfg = client.config().await;
        let (mint_id, mint_cfg) = cfg
            .get_first_module_by_kind::<MintClientConfig>(fedimint_mint_common::KIND)
            .context("federation has no mint (v1) module")?;
        Ok(Self { client, mint_id, mint_cfg: mint_cfg.clone() })
    }

    /// Join the federation behind `invite` (first run). Creates the mnemonic file if missing.
    pub async fn join(data_dir: &Path, invite: &str) -> Result<Self> {
        let invite_code = InviteCode::from_str(invite).context("not a valid invite code")?;
        std::fs::create_dir_all(data_dir)?;
        let mnemonic = load_or_create_mnemonic(data_dir)?;
        let db = open_db(data_dir).await?;
        let client = Client::builder()
            .await
            .with_module_for_join()
            .preview(connectors().await, &invite_code)
            .await
            .context("downloading federation config from the invite code")?
            .join(db, root_secret(&mnemonic))
            .await
            .context("joining the federation")?;
        std::fs::write(data_dir.join(INVITE_FILE), invite)?;
        Self::from_client(Arc::new(client)).await
    }

    /// Re-open a client joined earlier.
    pub async fn open(data_dir: &Path) -> Result<Self> {
        let mnemonic = read_mnemonic(data_dir)
            .with_context(|| format!("no wallet in {}: run `join <invite>` first", data_dir.display()))?;
        let db = open_db(data_dir).await?;
        let client = Client::builder()
            .await
            .with_module_for_join()
            .open(connectors().await, db, root_secret(&mnemonic))
            .await
            .context("opening the client database")?;
        Self::from_client(Arc::new(client)).await
    }

    pub fn federation_id(&self) -> FederationId {
        self.client.federation_id()
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

    /// `ecash import`: reissue an external OOBNotes string into this wallet and wait until done.
    /// Returns the face value of the notes.
    pub async fn import(&self, notes: &str) -> Result<Amount> {
        let oob = OOBNotes::from_str(notes).map_err(|e| anyhow!("not an ecash string: {e}"))?;
        self.import_notes(oob).await
    }

    pub async fn import_notes(&self, oob: OOBNotes) -> Result<Amount> {
        let mint = self.mint()?;
        let face = oob.total_amount();
        let op = mint.reissue_external_notes(oob, ()).await?;
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
        Ok(face)
    }

    /// `ecash export`: spend wallet notes into an OOBNotes string. Tries an exact amount first,
    /// then falls back to the smallest set of notes worth at least `amount`.
    pub async fn export(&self, amount_msat: u64) -> Result<(OOBNotes, Amount)> {
        let mint = self.mint()?;
        let amount = Amount::from_msats(amount_msat);
        let ttl = Some(Duration::from_secs(3600));
        let (op, notes) = match mint.spend_notes_with_selector(&SelectNotesWithExactAmount, amount, ttl, false, ()).await {
            Ok(r) => r,
            Err(_) => mint.spend_notes_with_selector(&SelectNotesWithAtleastAmount, amount, ttl, false, ()).await?,
        };
        let mut stream = mint.subscribe_spend_notes(op).await?.into_stream();
        // First state is `Created`: the notes are out of the wallet and in our hands.
        match tokio::time::timeout(Duration::from_secs(30), stream.next()).await {
            Ok(Some(SpendOOBState::Created)) | Ok(None) | Err(_) => {}
            Ok(Some(other)) => bail!("unexpected spend state {other:?}"),
        }
        let face = notes.total_amount();
        Ok((notes, face))
    }
}

/// `Client::builder` with only the mint module registered.
trait BuilderExt {
    fn with_module_for_join(self) -> fedimint_client::ClientBuilder;
}
impl BuilderExt for fedimint_client::ClientBuilder {
    fn with_module_for_join(mut self) -> fedimint_client::ClientBuilder {
        self.with_module(MintClientInit);
        self
    }
}

async fn connectors() -> ConnectorRegistry {
    ConnectorRegistry::build_from_client_defaults().bind().await
}

async fn open_db(data_dir: &Path) -> Result<Database> {
    Ok(fedimint_rocksdb::RocksDb::build(data_dir.join(DB_DIR))
        .open()
        .await
        .context("opening RocksDB")?
        .into())
}

fn root_secret(mnemonic: &Mnemonic) -> RootSecret {
    RootSecret::StandardDoubleDerive(Bip39RootSecretStrategy::<12>::to_root_secret(mnemonic))
}

fn read_mnemonic(data_dir: &Path) -> Result<Mnemonic> {
    let words = std::fs::read_to_string(data_dir.join(MNEMONIC_FILE))?;
    Mnemonic::from_str(words.trim()).context("mnemonic file is not a valid BIP39 phrase")
}

fn load_or_create_mnemonic(data_dir: &Path) -> Result<Mnemonic> {
    if let Ok(m) = read_mnemonic(data_dir) {
        return Ok(m);
    }
    let m = Bip39RootSecretStrategy::<12>::random(&mut thread_rng());
    let path = data_dir.join(MNEMONIC_FILE);
    std::fs::write(&path, m.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(m)
}
