//! What the till and the phone write the same way: the till's outputs and the card notes a
//! transaction creates, and private JSON files saved durably (0600, fsync, rename, fsync of the
//! directory).

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// An output owned by the host (a till note or a cashout note) that is in flight.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostPending {
    pub amount_msat: u64,
    /// Secret key of the note (32 bytes), hex.
    pub secret_key: String,
    /// Blinding key (consensus encoding), hex.
    pub blinding_key: String,
    /// `till` notes are reissued into the wallet. `cashout` only exists in state files written
    /// by an earlier build, which exported such notes as a string.
    pub kind: HostKind,
    /// The transaction (hex txid) that creates the output.
    pub group: String,
    /// Index of the output in that transaction, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_idx: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HostKind {
    Till,
    Cashout,
}

/// A card note created by a load (the sidecar's `OpsWrite::Load`) or as a charge's change. The blinding key is kept because the registry
/// records each pending note with it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoadNote {
    pub slot: u8,
    pub amount_msat: u64,
    pub nonce: String,
    pub blinding_key: String,
    pub out_idx: u64,
}

/// `OpenOptions` for a file only the owner can read.
pub fn private_options() -> std::fs::OpenOptions {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o
}

/// Create `dir` (and parents) with mode 0700 where it does not exist yet.
/// Write `value` as JSON to `path` (mode 0600) durably: a temporary file in the same directory,
/// fsynced, renamed over `path`, then the directory fsynced so the rename itself is durable.
pub fn write_private_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_private(path, &serde_json::to_vec_pretty(value)?, "json.tmp")
}

/// [`write_private_json`] for a line of text (a phone's id).
pub fn write_private_text(path: &Path, text: &str) -> Result<()> {
    write_private(path, text.as_bytes(), "tmp")
}

fn write_private(path: &Path, bytes: &[u8], tmp_ext: &str) -> Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    create_private_dir(dir)?;
    let tmp = path.with_extension(tmp_ext);
    {
        let mut f = private_options().truncate(true).open(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

pub fn create_private_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    Ok(())
}
