//! `card_state.json`: everything the host must remember about the card.
//!
//! The card holds only keys. Which key backs which note, the note's BLS signature, and the
//! blinding keys for in-flight issuances live here, so the file is written (atomically, with
//! fsync) *before* anything is sent to the federation or the card.
//!
//! Slot lifecycle: a slot with a cached pubkey and no note record is `free`. A topup or change
//! output records a `pending` note on it; once the federation signed it, it is `live`; after the
//! federation reports the note spent it is `spent`. A spent or `dead` slot still holds the old
//! key and must be overwritten (GEN_KEY with P2 = 1) before it can carry another note.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteStatus {
    /// Signed by the federation, not yet seen spent.
    Live,
    /// Output submitted (or about to be); blind signature not yet combined.
    Pending,
    /// The federation reports the nonce spent.
    Spent,
    /// Never became money (rejected transaction) or abandoned. The slot may be overwritten.
    Dead,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NoteRec {
    pub slot: u8,
    pub amount_msat: u64,
    /// 33-byte compressed public key (even parity) of the slot key, hex.
    pub nonce: String,
    /// 48-byte BLS signature (consensus encoding), hex. Present once `live`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub status: NoteStatus,
    /// Blinding key (consensus encoding), hex. Kept while `pending`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blinding_key: Option<String>,
    /// Transaction that created the note, hex, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub txid: Option<String>,
    /// Index of the note's output in that transaction, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_idx: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlotRec {
    pub slot: u8,
    /// x-only public key, hex.
    pub pubkey: String,
    /// The card already held this key when we first saw the slot, so we do not know who else
    /// has seen it or what it was used for. Never used as a nonce; overwritten when needed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unknown: bool,
}

/// A slot overwrite that was started and may or may not have reached the card.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingOverwrite {
    pub slot: u8,
    pub old_pubkey: String,
}

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

/// A transaction the host built and (maybe) sent: the outputs it creates are tracked in `notes`
/// and `host_pending` under the same txid.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingTx {
    pub txid: String,
    /// Consensus encoding of the signed transaction. Resubmitting it is idempotent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_hex: Option<String>,
    /// Set (and saved) immediately before the first submission. If false after a restart the
    /// transaction never left this machine and the group is abandoned.
    #[serde(default)]
    pub submitted: bool,
    /// The federation accepted it; only collection of outputs remains.
    #[serde(default)]
    pub accepted: bool,
    /// Nonces (hex) of the card notes it spends.
    #[serde(default)]
    pub inputs: Vec<String>,
    /// The till service's `client_ref` for this sale. Written in the same save that creates the
    /// record, so a repeat of the ref can always find its transaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_ref: Option<String>,
    /// What the sale looked like when it was built, so a sale recovered later is complete.
    #[serde(default)]
    pub price_msat: u64,
    #[serde(default)]
    pub ts_unix_ms: u64,
    #[serde(default)]
    pub overpay_msat: u64,
    #[serde(default)]
    pub fees_msat: u64,
    /// (slot, msat) of the card notes spent and of the change notes created.
    #[serde(default)]
    pub spent: Vec<(u8, u64)>,
    #[serde(default)]
    pub change: Vec<(u8, u64)>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HostKind {
    Till,
    Cashout,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CardState {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub slots: Vec<SlotRec>,
    #[serde(default)]
    pub notes: Vec<NoteRec>,
    #[serde(default)]
    pub pending_overwrites: Vec<PendingOverwrite>,
    #[serde(default)]
    pub host_pending: Vec<HostPending>,
    #[serde(default)]
    pub pending_txs: Vec<PendingTx>,
    /// Sales whose `PendingTx` was removed by a process that had no sales ledger to record them
    /// in. The till service turns them into sales (and removes them) on its next sync.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tombstones: Vec<Tombstone>,
    /// Finished `issue` calls, keyed by the app's `client_ref`. Written in the save that records
    /// the notes, before the federation is charged, so a repeat of the ref gets the same answer
    /// even after a crash.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<IssueRec>,
}

/// One `POST /v1/cards/issue`, kept so its `client_ref` can be replayed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IssueRec {
    pub client_ref: String,
    /// The transaction that funds the notes. Empty until the wallet has built it.
    #[serde(default)]
    pub txid: String,
    /// `(slot, msat)` created for this ref.
    #[serde(default)]
    pub notes: Vec<(u8, u64)>,
    /// The amounts the caller asked for, so a repeat with a different body can be told apart
    /// from a genuine replay.
    #[serde(default)]
    pub amounts: Vec<u64>,
    #[serde(default)]
    pub ts_unix_ms: u64,
}

/// What is left of an accepted `PendingTx` that carried a `client_ref`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tombstone {
    pub client_ref: String,
    pub txid: String,
    pub price_msat: u64,
    pub accepted: bool,
    #[serde(default)]
    pub ts_unix_ms: u64,
    #[serde(default)]
    pub overpay_msat: u64,
    #[serde(default)]
    pub fees_msat: u64,
    #[serde(default)]
    pub spent: Vec<(u8, u64)>,
    #[serde(default)]
    pub change: Vec<(u8, u64)>,
}

impl CardState {
    pub fn slot_pubkey(&self, slot: u8) -> Option<&str> {
        self.slots.iter().find(|s| s.slot == slot).map(|s| s.pubkey.as_str())
    }

    /// Record a key found on the card that this state file never produced.
    pub fn set_unknown_slot(&mut self, slot: u8, pubkey: String) {
        self.set_slot_pubkey(slot, pubkey);
        if let Some(s) = self.slots.iter_mut().find(|s| s.slot == slot) {
            s.unknown = true;
        }
    }

    pub fn set_slot_pubkey(&mut self, slot: u8, pubkey: String) {
        match self.slots.iter_mut().find(|s| s.slot == slot) {
            Some(s) => {
                s.pubkey = pubkey;
                s.unknown = false;
            }
            None => {
                self.slots.push(SlotRec { slot, pubkey, unknown: false });
                self.slots.sort_by_key(|s| s.slot);
            }
        }
    }

    /// Note records for the slot's *current* key.
    pub fn notes_on_current_key(&self, slot: u8) -> impl Iterator<Item = &NoteRec> {
        let pk = self.slot_pubkey(slot).map(str::to_owned);
        self.notes.iter().filter(move |n| {
            n.slot == slot && pk.as_deref().is_some_and(|pk| nonce_matches_pubkey(&n.nonce, pk))
        })
    }

    /// Slots whose key has never carried a note: usable for a new note right now.
    pub fn free_slots(&self) -> Vec<u8> {
        self.slots
            .iter()
            .filter(|s| !s.unknown)
            .filter(|s| !self.notes_on_current_key(s.slot).any(|_| true))
            .filter(|s| !self.pending_overwrites.iter().any(|p| p.slot == s.slot))
            .map(|s| s.slot)
            .collect()
    }

    /// Slots whose current key carried a note that is now `spent` or `dead` (and nothing live or
    /// pending): the key is burned and the slot can be overwritten.
    pub fn overwritable_slots(&self) -> Vec<u8> {
        self.slots
            .iter()
            .filter(|s| {
                if s.unknown {
                    return true;
                }
                let mut any = false;
                for n in self.notes_on_current_key(s.slot) {
                    any = true;
                    if matches!(n.status, NoteStatus::Live | NoteStatus::Pending) {
                        return false;
                    }
                }
                any
            })
            .filter(|s| !self.pending_overwrites.iter().any(|p| p.slot == s.slot))
            .map(|s| s.slot)
            .collect()
    }

    pub fn live_notes(&self) -> Vec<&NoteRec> {
        self.notes.iter().filter(|n| n.status == NoteStatus::Live).collect()
    }

    pub fn live_total_msat(&self) -> u64 {
        self.live_notes().iter().map(|n| n.amount_msat).sum()
    }
}

/// 33-byte compressed key (hex) against an x-only key (hex).
pub fn nonce_matches_pubkey(nonce_hex: &str, xonly_hex: &str) -> bool {
    nonce_hex.len() == 66 && nonce_hex[2..] == *xonly_hex
}

/// A state file on disk. Every mutation goes through [`StateStore::save`].
pub struct StateStore {
    path: PathBuf,
    pub state: CardState,
}

impl StateStore {
    /// Load `path`, or start empty if it does not exist.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let state = match std::fs::read(&path) {
            Ok(bytes) => {
                let st: CardState = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?;
                if st.version > STATE_VERSION {
                    bail!("{} has version {}, this build understands {STATE_VERSION}", path.display(), st.version);
                }
                st
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                CardState { version: STATE_VERSION, ..Default::default() }
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path, state })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write to a temporary file in the same directory, fsync it, rename over the state file,
    /// then fsync the directory so the rename itself is durable.
    pub fn save(&self) -> Result<()> {
        let dir = self.path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        create_private_dir(dir)?;
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(&self.state)?;
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

    /// Append `line` to `cashouts.log` next to the state file (mode 0600) and fsync it. Used for
    /// ecash strings that must not be lost; call before deleting the records that produced them.
    pub fn append_cashout_log(&self, line: &str) -> Result<()> {
        let path = self.path.with_file_name("cashouts.log");
        let mut f = private_options().append(true).open(&path).with_context(|| format!("opening {}", path.display()))?;
        writeln!(f, "{line}")?;
        f.sync_all()?;
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(n: u8) -> String {
        format!("{:02x}", n).repeat(32)
    }
    fn note(slot: u8, key: u8, status: NoteStatus) -> NoteRec {
        NoteRec {
            slot,
            amount_msat: 1024,
            nonce: format!("02{}", pk(key)),
            signature: None,
            status,
            blinding_key: None,
            txid: None,
            out_idx: None,
        }
    }

    #[test]
    fn round_trip_and_atomic_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("card_state.json");
        let mut s = StateStore::open(&path).unwrap();
        assert_eq!(s.state.version, STATE_VERSION);
        s.state.set_slot_pubkey(0, pk(1));
        s.state.notes.push(note(0, 1, NoteStatus::Live));
        s.save().unwrap();
        assert!(!path.with_extension("json.tmp").exists(), "temp file renamed away");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            s.append_cashout_log("ecash-string").unwrap();
            let log = path.with_file_name("cashouts.log");
            assert_eq!(std::fs::metadata(&log).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::read_to_string(&log).unwrap(), "ecash-string\n");
        }
        let s2 = StateStore::open(&path).unwrap();
        assert_eq!(s2.state, s.state);
        // Overwrite again: still parses.
        s.state.notes[0].status = NoteStatus::Spent;
        s.save().unwrap();
        assert_eq!(StateStore::open(&path).unwrap().state.notes[0].status, NoteStatus::Spent);
    }

    #[test]
    fn slot_lifecycle() {
        let mut st = CardState::default();
        for i in 0..4 {
            st.set_slot_pubkey(i, pk(i + 1));
        }
        assert_eq!(st.free_slots(), vec![0, 1, 2, 3]);
        st.notes.push(note(0, 1, NoteStatus::Live));
        st.notes.push(note(1, 2, NoteStatus::Pending));
        st.notes.push(note(2, 3, NoteStatus::Spent));
        st.notes.push(note(3, 4, NoteStatus::Dead));
        assert!(st.free_slots().is_empty());
        assert_eq!(st.overwritable_slots(), vec![2, 3]);
        assert_eq!(st.live_total_msat(), 1024);
        // Overwrite slot 2: new key, old note record is history, slot is free again.
        st.set_slot_pubkey(2, pk(9));
        assert_eq!(st.free_slots(), vec![2]);
        assert_eq!(st.overwritable_slots(), vec![3]);
        // A pending overwrite blocks reuse until resolved.
        st.pending_overwrites.push(PendingOverwrite { slot: 2, old_pubkey: pk(3) });
        assert!(st.free_slots().is_empty());
    }

    #[test]
    fn rejects_newer_versions_and_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        std::fs::write(&path, br#"{"version": 99}"#).unwrap();
        assert!(StateStore::open(&path).is_err());
        std::fs::write(&path, b"not json").unwrap();
        assert!(StateStore::open(&path).is_err());
    }
}
