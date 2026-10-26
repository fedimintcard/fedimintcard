//! The host's own Fedimint client: join by invite code, re-open, and the stock mint v1 flows
//! (`ecash import` / `ecash export`). Only the mint module is registered.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
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
    MintClientInit, MintClientModule, OOBNotes, ReissueExternalNotesState,
    ReissueExternalNotesError,
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
    /// Invite code saved by `join`, offered to the recipients of exported ecash.
    pub invite: Option<InviteCode>,
}

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
        let (mint_id, mint_cfg) = cfg
            .get_first_module_by_kind::<MintClientConfig>(fedimint_mint_common::KIND)
            .context("federation has no mint (v1) module")?;
        Ok(Self { client, mint_id, mint_cfg: mint_cfg.clone(), invite: None })
    }

    /// Join the federation behind `invite` (first run). Creates the mnemonic file if missing.
    pub async fn join(data_dir: &Path, invite: &str) -> Result<Self> {
        let invite_code = InviteCode::from_str(invite).context("not a valid invite code")?;
        crate::state::create_private_dir(data_dir)?;
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
        let mut w = Self::from_client(Arc::new(client)).await?;
        w.invite = Some(invite_code);
        Ok(w)
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
        let mut w = Self::from_client(Arc::new(client)).await?;
        w.invite = std::fs::read_to_string(data_dir.join(INVITE_FILE))
            .ok()
            .and_then(|s| InviteCode::from_str(s.trim()).ok());
        Ok(w)
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

    /// The federation's smallest economical note: below this a note's fees eat a quarter of it.
    pub fn min_denom(&self) -> u64 {
        self.mint_cfg.fee_consensus.min_economical_denomination().msats
    }

    /// `ecash import`: reissue an external OOBNotes string into this wallet and wait until done.
    pub async fn import(&self, notes: &str) -> Result<Imported> {
        let oob = OOBNotes::from_str(notes).map_err(|e| anyhow!("not an ecash string: {e}"))?;
        self.import_notes(oob).await
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
        let fee_msat = self
            .client
            .get_operation_fees(op)
            .await
            .ok()
            .flatten()
            .map(|a| a.get_bitcoin().msats)
            .unwrap_or(0);
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
        let rounded = self.mint_cfg.fee_consensus.round_up(want);
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
        let mint = self.mint()?;
        let m = self.min_denom();
        let mut a = available / m * m;
        while a > 0 {
            let fee = mint.send_fee_quote(Amount::from_msats(a)).await?.total().get_bitcoin().msats;
            if a + fee <= available {
                return Ok(a);
            }
            a -= m;
        }
        Ok(0)
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
