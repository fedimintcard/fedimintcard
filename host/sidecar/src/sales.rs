//! `sales.json`: the till service's convenience ledger of completed charges.
//!
//! The money truth is the federation and `card_state.json`; this file only lets the point-of-sale
//! app list sales and replay a `client_ref`. It is written like the state file: temp file with
//! mode 0600, fsync, rename, fsync of the directory.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::state::{create_private_dir, private_options};

pub const SALES_VERSION: u32 = 1;

/// One card slot and an amount, as shown in the API (`{"slot":0,"msat":N}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotMsat {
    pub slot: u8,
    pub msat: u64,
}

impl SlotMsat {
    pub fn list(v: &[(u8, u64)]) -> Vec<SlotMsat> {
        v.iter().map(|&(slot, msat)| SlotMsat { slot, msat }).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaleRec {
    /// Assigned by [`SalesStore::append`]; ignored on input.
    #[serde(default)]
    pub id: u64,
    pub ts_unix_ms: u64,
    pub price_msat: u64,
    pub received_msat: u64,
    pub overpaid_msat: u64,
    pub fees_msat: u64,
    pub txid: String,
    pub inputs: Vec<SlotMsat>,
    pub change: Vec<SlotMsat>,
    pub collection_pending: bool,
    #[serde(default)]
    pub client_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SalesFile {
    pub version: u32,
    pub next_id: u64,
    pub sales: Vec<SaleRec>,
}

impl Default for SalesFile {
    fn default() -> Self {
        Self { version: SALES_VERSION, next_id: 1, sales: vec![] }
    }
}

pub struct SalesStore {
    path: PathBuf,
    pub file: SalesFile,
}

impl SalesStore {
    /// Load `path`, or start empty if it does not exist.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let file = match std::fs::read(&path) {
            Ok(bytes) => {
                let f: SalesFile =
                    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
                if f.version > SALES_VERSION {
                    bail!("{} has version {}, this build understands {SALES_VERSION}", path.display(), f.version);
                }
                f
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => SalesFile::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path, file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn find_ref(&self, client_ref: &str) -> Option<&SaleRec> {
        self.file.sales.iter().find(|s| s.client_ref.as_deref() == Some(client_ref))
    }

    /// Record a sale and persist it. A sale whose `txid` or `client_ref` is already recorded
    /// returns the existing record and writes nothing. If the write fails the in-memory ledger
    /// is rolled back.
    pub fn append(&mut self, mut rec: SaleRec) -> Result<SaleRec> {
        if let Some(r) = rec.client_ref.as_deref().and_then(|c| self.find_ref(c)) {
            return Ok(r.clone());
        }
        if let Some(r) = self.file.sales.iter().find(|s| s.txid == rec.txid) {
            return Ok(r.clone());
        }
        rec.id = self.file.next_id;
        self.file.next_id += 1;
        self.file.sales.push(rec.clone());
        if let Err(e) = self.save() {
            self.file.sales.pop();
            self.file.next_id -= 1;
            return Err(e);
        }
        Ok(rec)
    }

    fn save(&self) -> Result<()> {
        let dir = self.path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        create_private_dir(dir)?;
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(&self.file)?;
        {
            let mut f = private_options().truncate(true).open(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(&json)?;
            f.write_all(b"\n")?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    }
}
