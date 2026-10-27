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
    /// Kept so a replay returns the same body as the first answer.
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub warnings: Vec<String>,
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
    /// Sales that could not be written to disk. Replays are served from here, so a failing disk
    /// cannot turn a repeat into a second charge; the card state link rebuilds them after a
    /// restart.
    memo: Vec<SaleRec>,
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
        Ok(Self { path, file, memo: vec![] })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn find_ref(&self, client_ref: &str) -> Option<&SaleRec> {
        self.file.sales.iter().chain(&self.memo).find(|s| s.client_ref.as_deref() == Some(client_ref))
    }

    /// Record a sale and persist it. A sale already recorded (same `txid`, same `client_ref`)
    /// returns the existing record and writes nothing. A `txid` recorded under a different
    /// `client_ref`, or a `client_ref` under a different `txid`, is an error. If the write fails
    /// the sale is kept in memory for replays and the error is returned.
    pub fn append(&mut self, rec: SaleRec) -> Result<SaleRec> {
        if let Some(r) = self.file.sales.iter().find(|s| s.txid == rec.txid) {
            if r.client_ref != rec.client_ref {
                bail!("transaction {} is already recorded under another client_ref", rec.txid);
            }
            // Recorded while its notes were still being collected: bring the amounts up to date.
            if r.collection_pending && !rec.collection_pending {
                let (received, pending) = (rec.received_msat, rec.collection_pending);
                let r = self.file.sales.iter_mut().find(|s| s.txid == rec.txid).expect("just found");
                let old = (r.received_msat, r.collection_pending);
                r.received_msat = received;
                r.collection_pending = pending;
                let updated = r.clone();
                if let Err(e) = self.save() {
                    let r = self.file.sales.iter_mut().find(|s| s.txid == rec.txid).expect("just found");
                    (r.received_msat, r.collection_pending) = old;
                    return Err(e);
                }
                return Ok(updated);
            }
            return Ok(r.clone());
        }
        // Not on disk yet: a sale held back by an earlier failed write is written now.
        if let Some(pos) = self.memo.iter().position(|s| s.txid == rec.txid) {
            if self.memo[pos].client_ref != rec.client_ref {
                bail!("transaction {} is already recorded under another client_ref", rec.txid);
            }
            let held = self.memo.remove(pos);
            return self.commit(held);
        }
        if let Some(r) = rec.client_ref.as_deref().and_then(|c| self.find_ref(c)) {
            bail!("client_ref {:?} is already recorded for transaction {}", r.client_ref, r.txid);
        }
        self.commit(rec)
    }

    fn commit(&mut self, mut rec: SaleRec) -> Result<SaleRec> {
        rec.id = self.file.next_id;
        self.file.next_id += 1;
        self.file.sales.push(rec.clone());
        if let Err(e) = self.save() {
            self.file.sales.pop();
            self.file.next_id -= 1;
            self.memo.push(rec);
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
