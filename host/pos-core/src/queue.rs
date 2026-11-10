//! The durable queue of offline charges (`offline_queue.json`), the same file on a till and on a
//! POS phone: each charge with every secret, saved before the card is told anything, and its signed
//! bytes saved before the point of sale is answered.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::files::{HostPending, LoadNote, write_private_json};
use crate::plan::FeeHeadroom;

pub const QUEUE_FILE: &str = "offline_queue.json";
/// The queue file's format. A file of a later one is refused, not misread.
pub const QUEUE_VERSION: u32 = 1;
/// The till's caps on what it holds unsettled (OFFLINE-PLAN.md § 8 defaults).
pub const DEFAULT_CAP_MSAT: u64 = 100_000_000;
pub const DEFAULT_CAP_COUNT: usize = 200;

/// Why an offline charge was refused. The till service maps these to its error codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfflineError {
    /// A 2.x card: it can sign a note twice, so it is never taken offline.
    Unsupported(String),
    /// The till cannot check a card offline: no certificate key kept, or not on ops.
    NotReady(String),
    /// The card's certificate or attestation does not check: not a card the registry certified.
    NotGenuine(String),
    /// The till's unsettled offline total or count would pass its cap.
    Cap(String),
    /// The price needs change and the card has no fresh KEYED slot (or too few) to take it
    /// offline; an exact plan would exist with room. The nearest prices the card pays with the
    /// slots it has, in whole sats (msat), below and above the asked one, when there are any.
    NoChangeRoom { detail: String, price_msat: u64, below_msat: Option<u64>, above_msat: Option<u64> },
    /// A phone's offline kit is past its expiry, by the phone's clock or the card's: it charges
    /// offline again once it has a new kit from the till.
    KitExpired(String),
    /// A phone's offline kit has no till note left of a tier the price needs (the card could pay
    /// it with every tier). The nearest prices the pool still pays, as for `NoChangeRoom`.
    PoolExhausted { detail: String, price_msat: u64, below_msat: Option<u64>, above_msat: Option<u64> },
    /// No plan from the card's notes that check, and some did not: a note of no denomination of
    /// the federation's, or whose federation signature does not verify.
    NoteInvalid(String),
    /// The request itself: a price, `client_ref` or PIN of the wrong shape. Nothing was asked of
    /// the card, so no PIN try was spent.
    BadRequest(String),
    /// This `client_ref` was used for a charge of another price.
    Conflict(String),
    /// An earlier attempt with this `client_ref` is waiting for its card, pulled while it signed:
    /// only that card's next tap can finish it, and nothing was charged again.
    Undecided(String),
}

impl OfflineError {
    /// The till API's HTTP status and code for this refusal (docs/TILL-API.md).
    pub fn class(&self) -> (u16, &'static str) {
        match self {
            OfflineError::Unsupported(_) => (409, "offline_unsupported_card"),
            OfflineError::NotReady(_) => (503, "offline_not_ready"),
            OfflineError::NotGenuine(_) => (403, "card_not_genuine"),
            OfflineError::Cap(_) => (503, "offline_cap_reached"),
            OfflineError::NoChangeRoom { .. } => (409, "offline_no_change_room"),
            OfflineError::KitExpired(_) => (403, "offline_kit_expired"),
            OfflineError::PoolExhausted { .. } => (409, "offline_pool_exhausted"),
            OfflineError::NoteInvalid(_) => (403, "offline_note_invalid"),
            OfflineError::BadRequest(_) => (400, "bad_request"),
            OfflineError::Conflict(_) => (409, "conflict"),
            OfflineError::Undecided(_) => (202, "undecided"),
        }
    }

    /// The prices of [`OfflineError::NoChangeRoom`] and [`OfflineError::PoolExhausted`]:
    /// `(suggested, below, above)`, the suggestion the nearer of the two, the higher on a tie.
    pub fn suggestions(&self) -> Option<(Option<u64>, Option<u64>, Option<u64>)> {
        let (OfflineError::NoChangeRoom { price_msat, below_msat, above_msat, .. }
        | OfflineError::PoolExhausted { price_msat, below_msat, above_msat, .. }) = self
        else {
            return None;
        };
        let suggested = match (below_msat, above_msat) {
            (Some(b), Some(a)) => Some(if price_msat - b < a - price_msat { *b } else { *a }),
            (b, a) => b.or(*a),
        };
        Some((suggested, *below_msat, *above_msat))
    }
}

impl std::fmt::Display for OfflineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OfflineError::Unsupported(m)
            | OfflineError::NotReady(m)
            | OfflineError::NotGenuine(m)
            | OfflineError::Cap(m)
            | OfflineError::KitExpired(m)
            | OfflineError::NoteInvalid(m)
            | OfflineError::BadRequest(m)
            | OfflineError::Conflict(m)
            | OfflineError::Undecided(m) => f.write_str(m),
            OfflineError::NoChangeRoom { detail, .. } | OfflineError::PoolExhausted { detail, .. } => f.write_str(detail),
        }
    }
}
impl std::error::Error for OfflineError {}

#[derive(Debug, Clone, Copy)]
pub struct OfflineConfig {
    pub cap_msat: u64,
    pub cap_count: usize,
    /// The fee schedule an offline charge keeps a margin for (OFFLINE-PLAN.md § 10, "Fee
    /// margin"), on a federation that takes a surplus; [`FeeHeadroom::OFF`] for none.
    pub fee_headroom: FeeHeadroom,
}

impl Default for OfflineConfig {
    fn default() -> Self {
        Self { cap_msat: DEFAULT_CAP_MSAT, cap_count: DEFAULT_CAP_COUNT, fee_headroom: FeeHeadroom::DEFAULT }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChargeState {
    /// Saved with every secret, the card not yet (or not known to have) signed.
    Signing,
    /// Signed; waiting for the registry.
    Queued,
    /// The registry took it; written into the card's file, waiting for the federation.
    Reported,
    /// The registry found an input already spent by another charge: a loss, kept for the operator.
    DoubleSpent,
    /// The registry took it and the federation refused it: an input spent where the registry did
    /// not see it (a copied card), or a fee rise its fee margin (if any) does not cover. A loss, kept for
    /// the operator; the federation's answer is not more specific than that here.
    Rejected,
    /// The registry refused the report for another reason (`detail`); kept for the operator.
    Refused,
    /// Left `signing` by a card taken away, and never finishable: one of its input slots was
    /// since spent by another transaction or re-keyed. Nothing was charged for it, and it no
    /// longer answers for its `client_ref` (`detail` names it); the notes the card did sign for
    /// it are stranded. Kept for the operator; its pool entries are never taken again.
    Abandoned,
}

/// A card note an offline charge spends, as the card held it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfflineInput {
    pub slot: u8,
    pub amount_msat: u64,
    /// 33 bytes, hex (`02 ‖ x-only key`).
    pub nonce: String,
    /// 48 bytes, hex.
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfflineCharge {
    pub txid: String,
    pub short_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_ref: Option<String>,
    pub state: ChargeState,
    pub unsigned_hex: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_hex: Option<String>,
    #[serde(default)]
    pub signed_at: u64,
    pub inputs: Vec<OfflineInput>,
    /// The card's change: the intent's `change` entries.
    pub change: Vec<LoadNote>,
    /// The till's own outputs, with their secrets.
    pub host: Vec<HostPending>,
    pub price_msat: u64,
    pub till_msat: u64,
    pub overpay_msat: u64,
    pub fees_msat: u64,
    /// The fee margin the inputs pay beyond the outputs and `fees_msat` (burned by the
    /// federation; OFFLINE-PLAN.md § 10). 0: an exact balance.
    #[serde(default)]
    pub fee_margin_msat: u64,
    pub ts_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// A phone's charge: the till's outputs, taken from the offline kit's pool (the till keeps
    /// their secrets). Empty on a till's own charge, whose `host` has them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pool: Vec<PoolRef>,
    /// A phone's charge the till has (its answer at sync was `queued` or `already`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub synced: bool,
}

/// One of a phone charge's outputs to the till: a pool entry of the offline kit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolRef {
    pub amount_msat: u64,
    /// The blinded nonce, 48 bytes, hex.
    pub blinded: String,
    /// Its output's index in the transaction.
    pub out_idx: u64,
}

impl OfflineCharge {
    /// Waiting to settle, and still this store's to send: a phone's charge the till has is the
    /// till's.
    fn unsettled(&self) -> bool {
        matches!(self.state, ChargeState::Signing | ChargeState::Queued | ChargeState::Reported) && !self.synced
    }

    /// The body of `POST /offline-charges`: the intent's fields, the signed bytes and when.
    pub fn report_body(&self) -> Value {
        json!({
            "txid": self.txid,
            "client_ref": self.client_ref.as_deref().unwrap_or(&self.txid),
            "tx_hex": self.unsigned_hex,
            "inputs": self.inputs.iter().map(|i| i.nonce.clone()).collect::<Vec<_>>(),
            "change": self.change.iter().map(|c| json!({
                "slot": c.slot, "amount_msat": c.amount_msat, "nonce": c.nonce,
                "blinding_key": c.blinding_key, "out_idx": c.out_idx,
            })).collect::<Vec<_>>(),
            "signed_hex": self.signed_hex,
            "signed_at": self.signed_at,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueFile {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub charges: Vec<OfflineCharge>,
    /// Nonces (hex) of every note this till took offline: never taken offline again.
    #[serde(default)]
    pub spent: Vec<String>,
    /// A phone's pool entries (blinded, hex) that a charge no longer in `charges` took: never
    /// taken again, signed or not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pool_used: Vec<String>,
    /// The latest time a phone has seen (ms): a clock set back cannot make an expired kit young.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub high_water_ms: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// The durable queue: saved like the state file (0600, fsync, rename, fsync of the directory).
/// Every change goes through [`Queue::commit`], so what is in memory is never ahead of the disk.
pub struct Queue {
    path: PathBuf,
    pub file: QueueFile,
    /// Tests: writes left before one fails ([`Queue::fail_write_after`]).
    fault: Option<usize>,
}

impl Queue {
    pub fn open(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join(QUEUE_FILE);
        let file: QueueFile = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => QueueFile { version: QUEUE_VERSION, ..Default::default() },
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        if file.version > QUEUE_VERSION {
            bail!("{} has version {}, this build understands {QUEUE_VERSION}", path.display(), file.version);
        }
        Ok(Self { path, file, fault: None })
    }

    /// Save the file as it is in memory.
    pub fn save(&mut self) -> Result<()> {
        let file = std::mem::take(&mut self.file);
        let r = self.write(&file);
        self.file = file;
        r
    }

    /// Change the file with `f` and save it: `f` edits a copy, which replaces what is in memory
    /// only once it is on disk. A failed write leaves memory as the disk has it.
    pub fn commit<R>(&mut self, f: impl FnOnce(&mut QueueFile) -> R) -> Result<R> {
        let mut next = self.file.clone();
        let r = f(&mut next);
        self.write(&next)?;
        self.file = next;
        Ok(r)
    }

    /// Tests: fail the write after the next `n` (0: the next one), once.
    #[doc(hidden)]
    pub fn fail_write_after(&mut self, n: Option<usize>) {
        self.fault = n;
    }

    fn write(&mut self, file: &QueueFile) -> Result<()> {
        if let Some(n) = self.fault {
            if n == 0 {
                self.fault = None;
                bail!("writing {} failed (an injected fault)", self.path.display());
            }
            self.fault = Some(n - 1);
        }
        write_private_json(&self.path, file)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn stats(&self) -> Stats {
        let open = self.file.charges.iter().filter(|c| c.unsettled());
        Stats {
            queued: open.clone().count(),
            queued_msat: open.map(|c| c.till_msat).sum(),
            double_spent: self.file.charges.iter().filter(|c| c.state == ChargeState::DoubleSpent).count(),
            refused: self.file.charges.iter().filter(|c| c.state == ChargeState::Refused).count(),
            rejected: self.file.charges.iter().filter(|c| c.state == ChargeState::Rejected).count(),
        }
    }

    /// Change the charge `txid` and save.
    pub fn set(&mut self, txid: &str, f: impl FnOnce(&mut OfflineCharge)) -> Result<()> {
        self.commit(|file| {
            if let Some(c) = file.charges.iter_mut().find(|c| c.txid == txid) {
                f(c);
            }
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Charges taken offline and not yet settled.
    pub queued: usize,
    /// What the till is owed for them (its outputs), msat.
    pub queued_msat: u64,
    pub double_spent: usize,
    pub refused: usize,
    pub rejected: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn charge(txid: &str, state: ChargeState, till: u64) -> OfflineCharge {
        OfflineCharge {
            txid: txid.into(),
            short_id: "abcd0123".into(),
            client_ref: None,
            state,
            unsigned_hex: "00".into(),
            signed_hex: Some("01".into()),
            signed_at: 1,
            inputs: vec![OfflineInput { slot: 1, amount_msat: 4096, nonce: "02aa".into(), signature: "bb".into() }],
            change: vec![],
            host: vec![],
            price_msat: till,
            till_msat: till,
            overpay_msat: 0,
            fees_msat: 0,
            fee_margin_msat: 0,
            ts_unix_ms: 0,
            detail: None,
            pool: vec![],
            synced: false,
        }
    }

    #[test]
    fn the_queue_is_durable_private_and_counts_what_is_unsettled() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = Queue::open(dir.path()).unwrap();
        q.file.charges.push(charge("a", ChargeState::Queued, 1000));
        q.file.charges.push(charge("b", ChargeState::Reported, 2000));
        q.file.charges.push(charge("c", ChargeState::DoubleSpent, 4000));
        q.file.charges.push(charge("d", ChargeState::Signing, 8000));
        q.file.spent.push("02aa".into());
        q.save().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(q.path()).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let q = Queue::open(dir.path()).unwrap();
        assert_eq!(q.stats(), Stats { queued: 3, queued_msat: 11_000, double_spent: 1, refused: 0, rejected: 0 });
        assert_eq!(q.file.spent, vec!["02aa".to_string()]);
    }

    #[test]
    fn a_newer_queue_is_refused_not_misread() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(QUEUE_FILE), br#"{"version": 2, "charges": [], "spent": []}"#).unwrap();
        let e = Queue::open(dir.path()).err().expect("refused");
        assert!(format!("{e:#}").contains("version 2"), "{e:#}");
        std::fs::write(dir.path().join(QUEUE_FILE), br#"{"version": 1, "charges": [], "spent": []}"#).unwrap();
        assert!(Queue::open(dir.path()).is_ok());
    }

    #[test]
    fn a_failed_write_leaves_memory_as_the_disk_has_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = Queue::open(dir.path()).unwrap();
        q.commit(|f| f.charges.push(charge("a", ChargeState::Queued, 1000))).unwrap();
        // The second write from now fails: the first goes through, the second changes nothing.
        q.fail_write_after(Some(1));
        q.commit(|f| f.charges.push(charge("b", ChargeState::Queued, 2000))).unwrap();
        let e = q.set("a", |c| c.state = ChargeState::Reported).unwrap_err();
        assert!(format!("{e:#}").contains("injected"), "{e:#}");
        assert_eq!(q.file.charges.iter().map(|c| (c.txid.as_str(), c.state)).collect::<Vec<_>>(), vec![("a", ChargeState::Queued), ("b", ChargeState::Queued)]);
        // Once: the next write goes through, and the disk has what memory has.
        q.set("a", |c| c.state = ChargeState::Reported).unwrap();
        let on_disk = Queue::open(dir.path()).unwrap();
        assert_eq!(on_disk.file.charges, q.file.charges);
        assert_eq!(on_disk.file.charges[0].state, ChargeState::Reported);
    }

    #[test]
    fn a_phone_charge_the_till_has_no_longer_counts_against_the_caps() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = Queue::open(dir.path()).unwrap();
        let mut synced = charge("s", ChargeState::Queued, 4000);
        synced.synced = true;
        q.commit(|f| f.charges.extend([charge("a", ChargeState::Queued, 1000), synced])).unwrap();
        assert_eq!((q.stats().queued, q.stats().queued_msat), (1, 1000));
    }

    #[test]
    fn the_report_body_is_the_intent_plus_the_signed_bytes() {
        let mut c = charge("t", ChargeState::Queued, 1000);
        c.change.push(LoadNote { slot: 4, amount_msat: 1024, nonce: "02cc".into(), blinding_key: "dd".into(), out_idx: 1 });
        let b = c.report_body();
        assert_eq!(b["client_ref"], "t", "a charge without the app's ref is named by its txid");
        assert_eq!(b["inputs"], json!(["02aa"]));
        assert_eq!(b["change"][0], json!({"slot": 4, "amount_msat": 1024, "nonce": "02cc", "blinding_key": "dd", "out_idx": 1}));
        assert_eq!((b["tx_hex"].as_str(), b["signed_hex"].as_str(), b["signed_at"].as_u64()), (Some("00"), Some("01"), Some(1)));
    }
}
