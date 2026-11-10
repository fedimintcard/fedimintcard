//! The durable queue of offline charges (`offline_queue.json`), the same file on a till and on a
//! POS phone: each charge with every secret, saved before the card is told anything, and its signed
//! bytes saved before the point of sale is answered.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::files::{HostPending, LoadNote, write_private_json};
use crate::plan::FeeHeadroom;

pub const QUEUE_FILE: &str = "offline_queue.json";
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
    NoChangeRoom { detail: String, below_msat: Option<u64>, above_msat: Option<u64> },
}

impl OfflineError {
    /// The suggestion of [`OfflineError::NoChangeRoom`]: the nearer of its two prices, the
    /// higher one on a tie.
    pub fn suggested_msat(&self, price: u64) -> Option<u64> {
        let OfflineError::NoChangeRoom { below_msat, above_msat, .. } = self else { return None };
        match (below_msat, above_msat) {
            (Some(b), Some(a)) => Some(if price - b < a - price { *b } else { *a }),
            (b, a) => b.or(*a),
        }
    }
}

impl std::fmt::Display for OfflineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OfflineError::Unsupported(m) | OfflineError::NotReady(m) | OfflineError::NotGenuine(m) | OfflineError::Cap(m) => f.write_str(m),
            OfflineError::NoChangeRoom { detail, .. } => f.write_str(detail),
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
}

impl OfflineCharge {
    fn unsettled(&self) -> bool {
        matches!(self.state, ChargeState::Signing | ChargeState::Queued | ChargeState::Reported)
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
}

/// The durable queue: saved like the state file (0600, fsync, rename, fsync of the directory).
pub struct Queue {
    path: PathBuf,
    pub file: QueueFile,
}

impl Queue {
    pub fn open(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join(QUEUE_FILE);
        let file = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => QueueFile { version: 1, ..Default::default() },
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path, file })
    }

    pub fn save(&self) -> Result<()> {
        write_private_json(&self.path, &self.file)
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
        if let Some(c) = self.file.charges.iter_mut().find(|c| c.txid == txid) {
            f(c);
        }
        self.save()
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
