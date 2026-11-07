//! Applet 3.0 (docs/OFFLINE-PROTOCOL.md § 1–2): the commands a till needs to pay offline, the
//! shapes of their answers, the block-header clock, and the registry's certificate over a card.
//!
//! The applet (`FedimintCardApplet.java`) is the source of truth; this module mirrors it byte for
//! byte so [`crate::SoftCard`] can follow it and a till can read a real card's answers.

use anyhow::{Context, Result, bail, ensure};
use p256::ecdsa::signature::Verifier as _;
use secp256k1::XOnlyPublicKey;
use sha2::{Digest, Sha256};

pub const INS_SET_NOTE: u8 = 0x14;
pub const INS_GET_SLOT: u8 = 0x15;
pub const INS_GET_STATE: u8 = 0x16;
pub const INS_SET_CHANGE: u8 = 0x17;
pub const INS_SET_LIMIT: u8 = 0x18;
pub const INS_SET_HEADER: u8 = 0x19;
pub const INS_ATTEST_INIT: u8 = 0x30;
pub const INS_SET_CERT: u8 = 0x31;
pub const INS_GET_CERT: u8 = 0x32;
pub const INS_ATTEST: u8 = 0x33;

/// SIGN past the day's limit (3.0). Nothing was spent.
pub const SW_OVER_LIMIT: u16 = 0x6A95;
/// SET_HEADER with a header that lacks the work (its own bits, the floor, or a quarter of the
/// hardest the card has taken).
pub const SW_LITTLE_WORK: u16 = 0x6A93;
/// Wrong data: a zero amount, change finished with another amount, a malformed `bits`.
pub const SW_WRONG_DATA: u16 = 0x6A80;
/// GET_CERT on a card with no certificate.
pub const SW_RECORD_NOT_FOUND: u16 = 0x6A83;
pub const SW_WRONG_LENGTH: u16 = 0x6700;

/// A note on the card: amount (8, big-endian msat) then the federation's signature (48).
pub const NOTE_LEN: usize = 56;
/// Change owed to a slot: txid (32), output index (2, big-endian), blinding key (32).
pub const OPEN_LEN: usize = 66;
pub const GET_SLOT_LEN: usize = 187;
pub const GET_STATE_LEN: usize = 58;
pub const CERT_MAX: usize = 160;
pub const HEADER_LEN: usize = 80;
/// ATTEST's label, 16 ASCII bytes.
pub const ATTEST_LABEL: &[u8; 16] = b"fedimintcard/at1";
/// The certificate's label, 18 ASCII bytes.
pub const CERT_LABEL: &[u8; 18] = b"fedimintcard/cert1";
/// The least work a header must show, as `bits` little-endian (0x17087BC0, cashucard's floor).
pub const FLOOR_BITS: [u8; 4] = [0xC0, 0x7B, 0x08, 0x17];
pub const DAY_SECONDS: u32 = 86_400;

/// What `SELECT` answers: the applet's major and minor version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppletVersion {
    pub major: u8,
    pub minor: u8,
}

impl AppletVersion {
    pub const V2_0: Self = Self { major: 2, minor: 0 };
    pub const V2_1: Self = Self { major: 2, minor: 1 };
    pub const V3_0: Self = Self { major: 3, minor: 0 };

    /// 3.0 and later: each slot signs one transaction, ever, and only a slot holding its note
    /// (SET_NOTE) signs at all.
    pub fn sign_once(&self) -> bool {
        self.major >= 3
    }

    /// The slots a card of this version has, when the version says (3.0: 32). 2.x cards are
    /// walked (`slot_count`): 2.0 has 8, 2.1 has 32.
    pub fn slots(&self) -> Option<u8> {
        self.sign_once().then_some(32)
    }

    /// From the two data bytes `SELECT` answers.
    pub fn from_select(data: &[u8]) -> Option<Self> {
        match data {
            [major, minor] => Some(Self { major: *major, minor: *minor }),
            _ => None,
        }
    }
}

impl std::fmt::Display for AppletVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    Empty,
    /// A key, no note.
    Keyed,
    Note,
    /// A key owed change (SET_CHANGE); finished by SET_NOTE with the same amount.
    Change,
    /// Signed its one transaction (`spent_hash`).
    Spent,
}

impl SlotState {
    pub fn from_byte(b: u8) -> Result<Self> {
        Ok(match b {
            0 => Self::Empty,
            1 => Self::Keyed,
            2 => Self::Note,
            3 => Self::Change,
            4 => Self::Spent,
            other => bail!("unknown slot state {other}"),
        })
    }

    pub fn byte(self) -> u8 {
        match self {
            Self::Empty => 0,
            Self::Keyed => 1,
            Self::Note => 2,
            Self::Change => 3,
            Self::Spent => 4,
        }
    }
}

/// The opening of change a slot is owed: enough for any online till to finish the note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opening {
    pub txid: [u8; 32],
    pub out_idx: u16,
    pub blinding_key: [u8; 32],
}

impl Opening {
    pub fn to_bytes(&self) -> [u8; OPEN_LEN] {
        let mut b = [0u8; OPEN_LEN];
        b[..32].copy_from_slice(&self.txid);
        b[32..34].copy_from_slice(&self.out_idx.to_be_bytes());
        b[34..].copy_from_slice(&self.blinding_key);
        b
    }

    pub fn from_bytes(b: &[u8]) -> Self {
        Self {
            txid: b[..32].try_into().expect("32"),
            out_idx: u16::from_be_bytes([b[32], b[33]]),
            blinding_key: b[34..66].try_into().expect("32"),
        }
    }
}

/// `GET_SLOT`'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotInfo {
    pub state: SlotState,
    /// The slot's x-only key; `None` for an empty slot (the card answers zeros).
    pub xonly: Option<XOnlyPublicKey>,
    /// The note's amount (or the change owed), 0 when there is none.
    pub amount_msat: u64,
    /// The federation's signature (consensus encoding of `tbs::Signature`); zeros unless NOTE.
    pub sig48: [u8; 48],
    /// The hash a SPENT slot signed; zeros otherwise.
    pub spent_hash: [u8; 32],
    /// The opening of change owed (CHANGE); zeros otherwise.
    pub opening: Opening,
}

impl SlotInfo {
    pub fn parse(b: &[u8]) -> Result<Self> {
        ensure!(b.len() == GET_SLOT_LEN, "GET_SLOT answered {} bytes, not {GET_SLOT_LEN}", b.len());
        let state = SlotState::from_byte(b[0])?;
        let xonly = if state == SlotState::Empty {
            None
        } else {
            Some(XOnlyPublicKey::from_slice(&b[1..33]).context("GET_SLOT answered an invalid x-only key")?)
        };
        Ok(Self {
            state,
            xonly,
            amount_msat: u64::from_be_bytes(b[33..41].try_into().expect("8")),
            sig48: b[41..89].try_into().expect("48"),
            spent_hash: b[89..121].try_into().expect("32"),
            opening: Opening::from_bytes(&b[121..187]),
        })
    }
}

/// `GET_STATE`'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardStatus {
    pub states: Vec<SlotState>,
    /// The card's clock: the time of the latest header it took (0: none yet).
    pub now: u32,
    /// When the day's window began (0: not yet).
    pub window: u32,
    /// The day's limit in msat (0: none).
    pub limit_msat: u64,
    /// Spent in the current window.
    pub spent_msat: u64,
    pub cert_len: u8,
    pub attest_key: bool,
}

impl CardStatus {
    pub fn parse(b: &[u8]) -> Result<Self> {
        ensure!(b.len() == GET_STATE_LEN, "GET_STATE answered {} bytes, not {GET_STATE_LEN}", b.len());
        Ok(Self {
            states: b[..32].iter().map(|s| SlotState::from_byte(*s)).collect::<Result<_>>()?,
            now: u32::from_be_bytes(b[32..36].try_into().expect("4")),
            window: u32::from_be_bytes(b[36..40].try_into().expect("4")),
            limit_msat: u64::from_be_bytes(b[40..48].try_into().expect("8")),
            spent_msat: u64::from_be_bytes(b[48..56].try_into().expect("8")),
            cert_len: b[56],
            attest_key: b[57] != 0,
        })
    }

    pub fn slots_in(&self, state: SlotState) -> Vec<u8> {
        self.states.iter().enumerate().filter(|(_, s)| **s == state).map(|(i, _)| i as u8).collect()
    }

    /// Whether SIGN would take notes of `amounts` in turn without answering 6A95: the applet's
    /// `dayAfter`, applied to each in order.
    pub fn limit_allows(&self, amounts: &[u64]) -> bool {
        let mut window = self.window;
        let mut spent = self.spent_msat;
        for a in amounts {
            if self.now != 0 {
                if window == 0 {
                    window = self.now;
                } else if window.checked_add(DAY_SECONDS).is_some_and(|end| self.now >= end) {
                    window = self.now;
                    spent = 0;
                }
            }
            let (s, wrapped) = spent.overflowing_add(*a);
            if self.limit_msat != 0 && (wrapped || s > self.limit_msat) {
                return false;
            }
            spent = s;
        }
        true
    }
}

// ------------------------------------------------------------------ block headers

pub fn sha256d(b: &[u8]) -> [u8; 32] {
    Sha256::digest(Sha256::digest(b)).into()
}

/// A header's hash as Bitcoin shows it (byte-reversed double SHA-256), hex.
pub fn header_hash_hex(h: &[u8; HEADER_LEN]) -> String {
    let mut d = sha256d(h);
    d.reverse();
    hex::encode(d)
}

/// A header's time, seconds.
pub fn header_time(h: &[u8; HEADER_LEN]) -> u32 {
    u32::from_le_bytes(h[68..72].try_into().expect("4"))
}

/// The target `bits` (little-endian, as in a header) name, as a 32-byte big-endian number. A size
/// no header could carry or a negative mantissa is `None` (the applet answers 6A80).
pub fn target_of(bits: &[u8; 4]) -> Option<[u8; 32]> {
    let exponent = bits[3] as usize;
    if !(1..=32).contains(&exponent) || bits[2] & 0x80 != 0 {
        return None;
    }
    let mut out = [0u8; 32];
    for k in 0..3 {
        let idx = 32 - exponent + k;
        if idx < 32 {
            out[idx] = bits[2 - k];
        }
    }
    Some(out)
}

/// Four times a 32-byte big-endian number; too large to hold is the largest there is.
pub fn times_four(a: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut carry = 0u16;
    for i in (0..32).rev() {
        let v = ((a[i] as u16) << 2) | carry;
        out[i] = v as u8;
        carry = (v >> 8) & 0x03;
    }
    if carry != 0 { [0xFF; 32] } else { out }
}

/// The header's hash read as Bitcoin compares it: a big-endian number.
pub fn header_work_hash(h: &[u8; HEADER_LEN]) -> [u8; 32] {
    let mut d = sha256d(h);
    d.reverse();
    d
}

/// Whether `h` meets its own bits and the floor. The card adds "a quarter of the hardest it has
/// taken", which only the card knows.
pub fn header_has_work(h: &[u8; HEADER_LEN]) -> bool {
    let bits: [u8; 4] = h[72..76].try_into().expect("4");
    let (Some(target), Some(floor)) = (target_of(&bits), target_of(&FLOOR_BITS)) else { return false };
    header_work_hash(h) <= target && target <= floor
}

// ------------------------------------------------------------------ the certificate

/// The registry's certificate over a card (§ 2): `01` ‖ slot 0's x-only key ‖ the attestation
/// key, compressed ‖ the registry's signature, raw r ‖ s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Certificate {
    pub slot0: [u8; 32],
    pub attest_key: [u8; 33],
    pub signature: [u8; 64],
}

pub const CERT_LEN: usize = 130;

impl Certificate {
    pub fn parse(b: &[u8]) -> Result<Self> {
        ensure!(b.len() == CERT_LEN, "the certificate is {} bytes, not {CERT_LEN}", b.len());
        ensure!(b[0] == 1, "unknown certificate version {}", b[0]);
        Ok(Self {
            slot0: b[1..33].try_into().expect("32"),
            attest_key: b[33..66].try_into().expect("33"),
            signature: b[66..130].try_into().expect("64"),
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = vec![1u8];
        b.extend_from_slice(&self.slot0);
        b.extend_from_slice(&self.attest_key);
        b.extend_from_slice(&self.signature);
        b
    }

    /// What the registry signs: the label, slot 0's key, the compressed attestation key.
    pub fn message(slot0: &[u8; 32], attest_key: &[u8; 33]) -> Vec<u8> {
        let mut m = CERT_LABEL.to_vec();
        m.extend_from_slice(slot0);
        m.extend_from_slice(attest_key);
        m
    }

    /// Check the registry's signature with its certificate key (65 bytes, uncompressed).
    pub fn verify(&self, cert_key: &[u8]) -> Result<()> {
        let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(cert_key).context("the kept certificate key is not a P-256 point")?;
        let sig = p256::ecdsa::Signature::from_slice(&self.signature).context("the certificate's signature is malformed")?;
        vk.verify(&Self::message(&self.slot0, &self.attest_key), &sig)
            .map_err(|_| anyhow::anyhow!("the certificate is not signed by the registry's certificate key"))
    }

    /// Check an ATTEST answer (DER) over `challenge` against this certificate's attestation key.
    pub fn verify_attest(&self, challenge: &[u8; 32], der: &[u8]) -> Result<()> {
        let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(&self.attest_key).context("the certificate's attestation key is not a P-256 point")?;
        let sig = p256::ecdsa::Signature::from_der(der).context("the card's attestation is not a DER signature")?;
        vk.verify(&attest_message(challenge, &self.slot0), &sig)
            .map_err(|_| anyhow::anyhow!("the card's attestation does not verify under its certified key"))
    }

    /// Make a certificate as the registry does, with its P-256 signing key (tests and tools).
    pub fn sign(key: &p256::ecdsa::SigningKey, slot0: &[u8; 32], attest_pub: &[u8]) -> Result<Self> {
        use p256::ecdsa::signature::Signer as _;
        let point = p256::PublicKey::from_sec1_bytes(attest_pub).context("attestation key")?;
        use p256::elliptic_curve::sec1::ToEncodedPoint as _;
        let attest_key: [u8; 33] = point.to_encoded_point(true).as_bytes().try_into().expect("33");
        let sig: p256::ecdsa::Signature = key.sign(&Self::message(slot0, &attest_key));
        Ok(Self { slot0: *slot0, attest_key, signature: sig.to_bytes().into() })
    }
}

/// What ATTEST signs: the label, the till's challenge, slot 0's x-only key.
pub fn attest_message(challenge: &[u8; 32], slot0: &[u8; 32]) -> Vec<u8> {
    let mut m = ATTEST_LABEL.to_vec();
    m.extend_from_slice(challenge);
    m.extend_from_slice(slot0);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_and_the_floor() {
        let floor = target_of(&FLOOR_BITS).unwrap();
        // 0x17087BC0: mantissa 08 7B C0 at 23 bytes up from the bottom.
        assert_eq!(&floor[9..12], &[0x08, 0x7B, 0xC0]);
        assert!(floor[..9].iter().all(|b| *b == 0) && floor[12..].iter().all(|b| *b == 0));
        assert_eq!(target_of(&[0, 0, 0x80, 3]), None, "negative mantissa");
        assert_eq!(target_of(&[0, 0, 1, 33]), None, "too large");
        assert_eq!(times_four(&[0xFF; 32]), [0xFF; 32]);
        let mut one = [0u8; 32];
        one[31] = 1;
        assert_eq!(times_four(&one)[31], 4);
    }

    #[test]
    fn limit_is_the_applets_day_after() {
        let s = |now, window, limit, spent| CardStatus { states: vec![], now, window, limit_msat: limit, spent_msat: spent, cert_len: 0, attest_key: false };
        assert!(s(0, 0, 0, 0).limit_allows(&[u64::MAX]));
        assert!(s(0, 0, 2000, 0).limit_allows(&[1000, 1000]));
        assert!(!s(0, 0, 2000, 0).limit_allows(&[1000, 1000, 1]));
        assert!(!s(0, 0, 2000, 1500).limit_allows(&[1000]));
        // a day later the window starts again
        assert!(s(100_000, 10_000, 2000, 2000).limit_allows(&[2000]));
        assert!(!s(10_000 + 86_399, 10_000, 2000, 2000).limit_allows(&[1]));
    }
}
