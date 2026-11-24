//! The applet's command set since 3.0 (docs/OFFLINE-PROTOCOL.md § 1–2), as applet 4.0 answers it:
//! the commands a till needs to pay offline, the shapes of their answers, the block-header clock
//! with its weak rule and anchor, the registry's signatures over notes, and its certificate over a
//! card.
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
pub const INS_SET_REGISTRY_KEY: u8 = 0x34;
pub const INS_RELEASE: u8 = 0x35;

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
/// RELEASE or SET_NOTE with a signature the registry key does not verify.
pub const SW_SECURITY_STATUS_NOT_SATISFIED: u16 = 0x6982;
/// SET_PIN in a selection that has not anchored the clock (4.0).
pub const SW_NOT_ANCHORED: u16 = 0x6A96;
/// VERIFY_PIN while a wait is owed: the card served one asking and did not look at the PIN. Send
/// the same command again, with nothing between, until it answers something else (4.0).
pub const SW_KEEP_HOLDING: u16 = 0x6A98;
/// The card's check of its own work failed (SIGN: the signature did not verify), so it wrote
/// nothing: the slot keeps its note (4.0).
pub const SW_CRYPTO_ERROR: u16 = 0x6F00;

/// A note on the card: amount (8, big-endian msat) then the federation's signature (48).
pub const NOTE_LEN: usize = 56;
/// Change owed to a slot: txid (32), output index (2, big-endian), blinding key (32).
pub const OPEN_LEN: usize = 66;
pub const GET_SLOT_LEN: usize = 187;
/// GET_STATE of applet 4.0: byte 58 the registry key, 59..63 the PIN, 63 the anchor, 64 the signer.
pub const GET_STATE_LEN: usize = 65;
pub const CERT_MAX: usize = 160;
pub const HEADER_LEN: usize = 80;
/// ATTEST's label, 16 ASCII bytes.
pub const ATTEST_LABEL: &[u8; 16] = b"fedimintcard/at1";
/// The certificate's label, 18 ASCII bytes.
pub const CERT_LABEL: &[u8; 18] = b"fedimintcard/cert1";
/// RELEASE's label, 17 ASCII bytes.
pub const RELEASE_LABEL: &[u8; 17] = b"fedimintcard/rel1";
/// The shortest and longest DER signature RELEASE takes (the applet's bounds).
pub const RELEASE_SIG_MIN: usize = 8;
pub const RELEASE_SIG_MAX: usize = 72;
/// The label of the registry's signature over a note, 18 ASCII bytes (4.0).
pub const NOTE_LABEL: &[u8; 18] = b"fedimintcard/note1";
/// The shortest and longest DER signature SET_NOTE takes after the note (the applet's bounds).
pub const NOTE_SIG_MIN: usize = 8;
pub const NOTE_SIG_MAX: usize = 72;
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
    pub const V4_0: Self = Self { major: 4, minor: 0 };

    /// The only major this library speaks: 4. Anything else gets no PIN command.
    pub fn is_current(&self) -> bool {
        self.major == Self::V4_0.major
    }

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
    /// The registry's certificate key is set (SET_REGISTRY_KEY), so the card takes releases and
    /// notes.
    pub registry_key: bool,
    /// A PIN is set (byte 59).
    pub pin_set: bool,
    /// Wrong PIN tries in a row (byte 60; the card stops counting at 15).
    pub wrong_tries: u8,
    /// Askings owed before the next PIN try (bytes 61..63, big-endian): each is one VERIFY_PIN
    /// answered `6A98`.
    pub owed: u16,
    /// This selection anchored the clock (byte 63), which SET_PIN asks for.
    pub anchored: bool,
    /// The signer's product runs on the chip's RSA engine (byte 64, bit 0).
    pub chip_mul: bool,
    /// The signer's nonce key lives in RAM (byte 64, bit 1).
    pub key_in_ram: bool,
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
            registry_key: b[58] != 0,
            pin_set: b[59] != 0,
            wrong_tries: b[60],
            owed: u16::from_be_bytes([b[61], b[62]]),
            anchored: b[63] != 0,
            chip_mul: b[64] & 1 != 0,
            key_in_ram: b[64] & 2 != 0,
        })
    }

    /// Whether a header the card takes now would anchor its clock: no PIN set and no slot holding
    /// value (a note or change owed).
    pub fn anchors(&self) -> bool {
        anchors(self.pin_set, self.states.iter().any(|s| matches!(s, SlotState::Note | SlotState::Change)))
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

/// Twice a 32-byte big-endian number; too large to hold is the largest there is (the applet's
/// `twice`).
pub fn twice(a: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut carry = 0u16;
    for i in (0..32).rev() {
        let v = ((a[i] as u16) << 1) | carry;
        out[i] = v as u8;
        carry = (v >> 8) & 0x01;
    }
    if carry != 0 { [0xFF; 32] } else { out }
}

/// The weak rule (4.0): with a hardest taken (its `bits`), a header is weak when its target is over
/// twice the hardest's; before any hardest, when its target, doubled, is over the floor's (its
/// work is under twice the floor's). `None` for bits no header can carry (the card answers 6A80).
pub fn header_is_weak(h: &[u8; HEADER_LEN], hardest: Option<&[u8; 4]>) -> Option<bool> {
    header_is_weak_at(h, hardest, &FLOOR_BITS)
}

/// [`header_is_weak`] against another floor (tests mine headers for an easy one, as the applet's
/// tests do).
pub fn header_is_weak_at(h: &[u8; HEADER_LEN], hardest: Option<&[u8; 4]>, floor: &[u8; 4]) -> Option<bool> {
    let bits: [u8; 4] = h[72..76].try_into().expect("4");
    let target = target_of(&bits)?;
    Some(match hardest {
        Some(hb) => target > twice(&target_of(hb)?),
        None => twice(&target) > target_of(floor)?,
    })
}

/// The card's whole test of a header (SET_HEADER), in its order: bits it can read (6A80), its own
/// work, the floor, a quarter of the hardest, then the weak rule (each 6A93). `hardest` is the
/// card's hardest `bits`, `None` before any. `Ok(harder)` when the card takes the header, `harder`
/// saying whether it raises the hardest. A host sends a card only headers this takes.
pub fn check_header(h: &[u8; HEADER_LEN], hardest: Option<&[u8; 4]>) -> std::result::Result<bool, u16> {
    check_header_at(h, hardest, &FLOOR_BITS)
}

/// [`check_header`] against another floor (tests).
pub fn check_header_at(h: &[u8; HEADER_LEN], hardest: Option<&[u8; 4]>, floor: &[u8; 4]) -> std::result::Result<bool, u16> {
    let bits: [u8; 4] = h[72..76].try_into().expect("4");
    let target = target_of(&bits).ok_or(SW_WRONG_DATA)?;
    if header_work_hash(h) > target {
        return Err(SW_LITTLE_WORK);
    }
    if target > target_of(floor).ok_or(SW_WRONG_DATA)? {
        return Err(SW_LITTLE_WORK);
    }
    let hardest_target = match hardest {
        Some(hb) => Some(target_of(hb).ok_or(SW_WRONG_DATA)?),
        None => None,
    };
    if let Some(ht) = &hardest_target
        && target > times_four(ht)
    {
        return Err(SW_LITTLE_WORK);
    }
    if header_is_weak_at(h, hardest, floor).ok_or(SW_WRONG_DATA)? {
        return Err(SW_LITTLE_WORK);
    }
    Ok(hardest_target.is_none_or(|ht| target < ht))
}

/// Whether a header the card takes is an anchor (4.0): no PIN set and no slot holding value. An
/// anchor sets the clock to its time, even an earlier one, raises the hardest only where harder,
/// clears the day's window and spent count, and marks the selection anchored for SET_PIN.
pub fn anchors(pin_set: bool, holds_value: bool) -> bool {
    !pin_set && !holds_value
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
        let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(&self.attest_key)
            .context("the certificate's attestation key is not a P-256 point")?;
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

// ------------------------------------------------------------------ releases (§ 6)

/// What the registry signs for a release: the label, slot 0's x-only key, the slot, the txid.
pub fn release_message(slot0: &[u8; 32], slot: u8, txid: &[u8; 32]) -> Vec<u8> {
    let mut m = RELEASE_LABEL.to_vec();
    m.extend_from_slice(slot0);
    m.push(slot);
    m.extend_from_slice(txid);
    m
}

/// Check a release (DER) with the registry's certificate key: the applet's verify, on the host.
pub fn verify_release(registry_key: &p256::ecdsa::VerifyingKey, slot0: &[u8; 32], slot: u8, txid: &[u8; 32], der: &[u8]) -> bool {
    p256::ecdsa::Signature::from_der(der).is_ok_and(|sig| registry_key.verify(&release_message(slot0, slot, txid), &sig).is_ok())
}

/// Sign a release as the registry does, DER (tests and tools).
pub fn sign_release(key: &p256::ecdsa::SigningKey, slot0: &[u8; 32], slot: u8, txid: &[u8; 32]) -> Vec<u8> {
    use p256::ecdsa::signature::Signer as _;
    let sig: p256::ecdsa::Signature = key.sign(&release_message(slot0, slot, txid));
    sig.to_der().as_bytes().to_vec()
}

// ------------------------------------------------------------------ notes (4.0, DESIGN § 6)

/// SET_NOTE's 56 data bytes before the registry's signature: amount (8, big-endian) ‖ the
/// federation's signature (48).
pub fn note_bytes(amount_msat: u64, sig48: &[u8; 48]) -> [u8; NOTE_LEN] {
    let mut n = [0u8; NOTE_LEN];
    n[..8].copy_from_slice(&amount_msat.to_be_bytes());
    n[8..].copy_from_slice(sig48);
    n
}

/// What the registry signs for a note and the card checks: the label, the slot's x-only key as
/// the card holds it, the 56 note bytes (106 bytes).
pub fn note_message(slot_x: &[u8; 32], amount_msat: u64, sig48: &[u8; 48]) -> Vec<u8> {
    let mut m = NOTE_LABEL.to_vec();
    m.extend_from_slice(slot_x);
    m.extend_from_slice(&note_bytes(amount_msat, sig48));
    m
}

/// Check the registry's signature over a note (DER) as the card does: ECDSA P-256 with SHA-256,
/// either half of `s` taken (p256's verify takes both, as the card's does).
pub fn verify_note(registry_key: &p256::ecdsa::VerifyingKey, slot_x: &[u8; 32], amount_msat: u64, sig48: &[u8; 48], der: &[u8]) -> bool {
    p256::ecdsa::Signature::from_der(der).is_ok_and(|sig| registry_key.verify(&note_message(slot_x, amount_msat, sig48), &sig).is_ok())
}

/// The registry key the tests use: a TEST key, private key 1 (its public key is P-256's
/// generator), the applet tests' `TEST_REGISTRY_KEY`. Never a deployment key.
pub fn test_registry_key() -> p256::ecdsa::SigningKey {
    let mut one = [0u8; 32];
    one[31] = 1;
    p256::ecdsa::SigningKey::from_slice(&one).expect("1 is a scalar")
}

/// A registry key's point as SET_REGISTRY_KEY takes it: 65 bytes uncompressed.
pub fn registry_point(key: &p256::ecdsa::SigningKey) -> [u8; 65] {
    key.verifying_key().to_encoded_point(false).as_bytes().try_into().expect("65")
}

/// Sign a note as the registry does, DER (tests and tools).
pub fn sign_note(key: &p256::ecdsa::SigningKey, slot_x: &[u8; 32], amount_msat: u64, sig48: &[u8; 48]) -> Vec<u8> {
    use p256::ecdsa::signature::Signer as _;
    let sig: p256::ecdsa::Signature = key.sign(&note_message(slot_x, amount_msat, sig48));
    sig.to_der().as_bytes().to_vec()
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
        let s = |now, window, limit, spent| CardStatus {
            states: vec![],
            now,
            window,
            limit_msat: limit,
            spent_msat: spent,
            cert_len: 0,
            attest_key: false,
            registry_key: false,
            pin_set: false,
            wrong_tries: 0,
            owed: 0,
            anchored: false,
            chip_mul: false,
            key_in_ram: false,
        };
        assert!(s(0, 0, 0, 0).limit_allows(&[u64::MAX]));
        assert!(s(0, 0, 2000, 0).limit_allows(&[1000, 1000]));
        assert!(!s(0, 0, 2000, 0).limit_allows(&[1000, 1000, 1]));
        assert!(!s(0, 0, 2000, 1500).limit_allows(&[1000]));
        // a day later the window starts again
        assert!(s(100_000, 10_000, 2000, 2000).limit_allows(&[2000]));
        assert!(!s(10_000 + 86_399, 10_000, 2000, 2000).limit_allows(&[1]));
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;

    #[test]
    fn get_state_is_65_bytes_with_the_pin_the_anchor_and_the_signer() {
        let mut b = vec![0u8; GET_STATE_LEN];
        b[2] = SlotState::Keyed.byte();
        b[58] = 1;
        b[59] = 1;
        b[60] = 4;
        b[61] = 0x01;
        b[62] = 0x02;
        b[63] = 1;
        b[64] = 2;
        let st = CardStatus::parse(&b).unwrap();
        assert!(st.registry_key && st.pin_set && st.anchored && st.key_in_ram && !st.chip_mul);
        assert_eq!((st.wrong_tries, st.owed), (4, 0x0102));
        assert!(!st.anchors(), "a PIN is set");
        b[64] = 1;
        assert!(CardStatus::parse(&b).unwrap().chip_mul);
        b[59] = 0;
        assert!(CardStatus::parse(&b).unwrap().anchors(), "no PIN and a keyed slot holds no value");
        b[3] = SlotState::Change.byte();
        assert!(!CardStatus::parse(&b).unwrap().anchors(), "change owed is value");
        for n in [58, 59, 63, 64, 66] {
            assert!(CardStatus::parse(&vec![0u8; n]).is_err(), "{n} bytes is not a 4.0 card");
        }
    }

    #[test]
    fn twice_doubles_and_saturates() {
        let mut a = [0u8; 32];
        a[31] = 0x81;
        let t = twice(&a);
        assert_eq!((t[30], t[31]), (1, 2));
        let mut top = [0u8; 32];
        top[0] = 0x80;
        assert_eq!(twice(&top), [0xFF; 32]);
    }

    #[test]
    fn the_note_message_is_label_key_amount_and_federation_signature() {
        let m = note_message(&[7; 32], 1000, &[9; 48]);
        assert_eq!(m.len(), 106);
        assert_eq!(&m[..18], b"fedimintcard/note1");
        assert_eq!(&m[18..50], &[7; 32]);
        assert_eq!(&m[50..58], &1000u64.to_be_bytes());
        assert_eq!(&m[58..], &[9; 48]);
    }
}
