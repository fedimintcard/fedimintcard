//! The sealed PIN (docs/OFFLINE-PROTOCOL.md § 9, applet 4.x): every PIN
//! command is sealed. The card has a secp256k1 key of its own for nothing else, the PIN key, bound by its
//! attestation key; a host checks that binding under a certificate it has checked, then seals each
//! PIN command to the key under a fresh card nonce.
//!
//! This is Cashu Card 1.9's sealed PIN as cashucard ships it (MIT; at be4b129 its applet's
//! `unseal`, `sealBlock`, `unblockPin` and `processGetPinKey`, and `tools/cardctl` `seal_block`),
//! with our labels and our binding (ECDSA P-256 SHA-256 by the attestation key, the signature
//! ATTEST already makes). The shared vectors are `spec/vectors/sealed-pin.json`
//! (`tests/sealed_pin.rs` runs this code over them).
//!
//! The one way to seal is [`SealedCommand::new`], and it takes a [`TrustedCard`], which only
//! [`trust_card`] makes: no host seals a PIN to a key it has not checked.

use anyhow::{Context, Result, ensure};
use p256::ecdsa::signature::Verifier as _;
use secp256k1::{PublicKey, SECP256K1, Scalar, SecretKey, XOnlyPublicKey};
use sha2::{Digest, Sha256};

use crate::{Card, Certificate, status_word};

/// The seal's label (18 ASCII).
pub const SEAL_LABEL: &[u8] = b"fedimintcard/seal1";
/// The binding's label (17 ASCII): the attestation key signs it ‖ the PIN key.
pub const BIND_LABEL: &[u8] = b"fedimintcard/pin1";
/// `GET_NONCE`: nonce (16) ‖ PIN key, compressed (33) ‖ binding (DER, 8..72).
pub const INS_GET_NONCE: u8 = 0x44;
/// P1 of every sealed PIN command, and of GET_NONCE. A 4.x card answers `6B00` to any other.
pub const P1_SEALED: u8 = 0x01;
pub const NONCE_LEN: usize = 16;
pub const PIN_KEY_LEN: usize = 33;
pub const BINDING_MIN: usize = 8;
pub const BINDING_MAX: usize = 72;
/// E, the sender's one-message point, uncompressed.
pub const POINT_LEN: usize = 65;
pub const TAG_LEN: usize = 16;
/// The PIN block: length (1) ‖ PIN ‖ zeros, always 9 bytes.
pub const PIN_BLOCK_LEN: usize = 9;
/// A sealed VERIFY_PIN's or SET_PIN's Lc (65 + 9 + 16), and CHANGE_PIN's (65 + 18 + 16).
pub const SEALED_LC: usize = POINT_LEN + PIN_BLOCK_LEN + TAG_LEN;
pub const SEALED_CHANGE_LC: usize = POINT_LEN + 2 * PIN_BLOCK_LEN + TAG_LEN;
/// `B0 44 01 00 00`.
pub const GET_NONCE_APDU: [u8; 5] = [0xB0, INS_GET_NONCE, P1_SEALED, 0x00, 0x00];

/// The card is not one this host may seal a PIN to: its certificate, its slot 0 or its PIN key's
/// binding did not check. No PIN byte was sealed or sent. The text names what failed, never a
/// key or a PIN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrustedCard(pub String);

impl std::fmt::Display for UntrustedCard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "this card is not one the registry certified: {}", self.0)
    }
}

impl std::error::Error for UntrustedCard {}

fn untrusted(m: impl Into<String>) -> anyhow::Error {
    UntrustedCard(m.into()).into()
}

/// A P1 00 PIN command (P1 ≠ 01) for a card that SELECT showed as 4.x: refused before it is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClearPinRefused;

impl std::fmt::Display for ClearPinRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("applet 4.x takes only the sealed PIN commands (P1 = 01); nothing was sent")
    }
}

impl std::error::Error for ClearPinRefused {}

/// What a card that answers `6D00` to GET_NONCE is told: a 4.0 dev applet from before the sealed PIN.
pub const NO_SEALED_PIN_MESSAGE: &str = "This card's applet is older than the sealed PIN. Reset it and set it up again.";

/// The PIN block of `pin` (4 to 8 bytes): its length, the PIN, zeros to 9 bytes.
pub fn pin_block(pin: &[u8]) -> Result<[u8; PIN_BLOCK_LEN]> {
    ensure!((crate::PIN_MIN..=crate::PIN_MAX).contains(&pin.len()), "a PIN is {} to {} digits", crate::PIN_MIN, crate::PIN_MAX);
    let mut b = [0u8; PIN_BLOCK_LEN];
    b[0] = pin.len() as u8;
    b[1..1 + pin.len()].copy_from_slice(pin);
    Ok(b)
}

/// The PIN a block carries, or `None` for a length outside 4 to 8 (the card's `6700`, no try).
pub fn unblock(block: &[u8]) -> Option<&[u8]> {
    let n = *block.first()? as usize;
    ((crate::PIN_MIN..=crate::PIN_MAX).contains(&n) && block.len() > n).then(|| &block[1..1 + n])
}

/// block(i, m) = SHA-256(label ‖ i (1) ‖ x (32) ‖ E (65) ‖ nonce (16) ‖ INS (1) ‖ m).
fn block(label: &[u8], i: u8, x: &[u8; 32], big_e: &[u8], nonce: &[u8], ins: u8, m: &[u8]) -> [u8; 32] {
    let mut d = Sha256::new();
    d.update(label);
    d.update([i]);
    d.update(x);
    d.update(big_e);
    d.update(nonce);
    d.update([ins]);
    d.update(m);
    d.finalize().into()
}

/// XOR `data` in place with blocks 1, 2, … (32 bytes each).
fn keystream(label: &[u8], x: &[u8; 32], big_e: &[u8], nonce: &[u8], ins: u8, data: &mut [u8]) {
    for (i, chunk) in data.chunks_mut(32).enumerate() {
        let mut k = block(label, (i + 1) as u8, x, big_e, nonce, ins, &[]);
        chunk.iter_mut().zip(k.iter()).for_each(|(b, kb)| *b ^= kb);
        k.fill(0);
    }
}

/// The x of `k`·`p`.
fn shared_x(p: &PublicKey, k: &SecretKey) -> Result<[u8; 32]> {
    let mut s = p.mul_tweak(SECP256K1, &Scalar::from(*k)).context("the key agreement")?.serialize_uncompressed();
    let x: [u8; 32] = s[1..33].try_into().expect("32");
    s.fill(0);
    Ok(x)
}

/// The envelope E ‖ ct ‖ tag of `clear` under `label`, sent with `ins`, to `pin_key` under
/// `nonce`, with the sender's one-message key `e`. [`seal`] is this with [`SEAL_LABEL`] and a
/// fresh `e`; the label and `e` are parameters only so the vectors (ours and Cashu's) can be
/// reproduced.
pub fn seal_with(label: &[u8], e: &SecretKey, pin_key: &PublicKey, nonce: &[u8], ins: u8, clear: &[u8]) -> Result<Vec<u8>> {
    let big_e = PublicKey::from_secret_key(SECP256K1, e).serialize_uncompressed();
    let mut x = shared_x(pin_key, e)?;
    let mut out = Vec::with_capacity(POINT_LEN + clear.len() + TAG_LEN);
    out.extend_from_slice(&big_e);
    out.extend_from_slice(clear);
    keystream(label, &x, &big_e, nonce, ins, &mut out[POINT_LEN..]);
    let tag = block(label, 0, &x, &big_e, nonce, ins, &out[POINT_LEN..]);
    out.extend_from_slice(&tag[..TAG_LEN]);
    x.fill(0);
    Ok(out)
}

/// [`seal_with`] under [`SEAL_LABEL`] with a fresh sender key from the system's RNG, wiped after.
pub fn seal(pin_key: &PublicKey, nonce: &[u8], ins: u8, clear: &[u8]) -> Result<Vec<u8>> {
    let mut e = SecretKey::new(&mut rand::thread_rng());
    let r = seal_with(SEAL_LABEL, &e, pin_key, nonce, ins, clear);
    e.non_secure_erase();
    r
}

/// The card's side, for the [`SoftCard`](crate::SoftCard) and the vectors: the clear data of
/// `data` under the PIN key's scalar `d` and the live `nonce`, for the instruction it came with;
/// `None` where it does not open (E not a point of the curve in the `04` form, or not this tag).
pub fn open_with(label: &[u8], d: &SecretKey, nonce: &[u8], ins: u8, data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < POINT_LEN + TAG_LEN || data[0] != 0x04 {
        return None;
    }
    let (big_e, rest) = data.split_at(POINT_LEN);
    let (ct, tag) = rest.split_at(rest.len() - TAG_LEN);
    let mut x = shared_x(&PublicKey::from_slice(big_e).ok()?, d).ok()?;
    let ok = block(label, 0, &x, big_e, nonce, ins, ct)[..TAG_LEN] == *tag;
    let clear = ok.then(|| {
        let mut c = ct.to_vec();
        keystream(label, &x, big_e, nonce, ins, &mut c);
        c
    });
    x.fill(0);
    clear
}

/// A sealed PIN command: CLA B0, `ins`, P1 = 01, P2 = 00, Lc, the envelope, Le = 00 (every APDU
/// this library builds ends in its Le; the vectors' `apdu` is this without it).
pub fn sealed_apdu(ins: u8, envelope: &[u8]) -> Vec<u8> {
    let mut a = vec![0xB0, ins, P1_SEALED, 0x00, envelope.len() as u8];
    a.extend_from_slice(envelope);
    a.push(0x00);
    a
}

/// The binding's message: [`BIND_LABEL`] ‖ the PIN key (33).
pub fn binding_message(pin_key33: &[u8]) -> Vec<u8> {
    [BIND_LABEL, pin_key33].concat()
}

/// GET_NONCE's answer, parsed: the nonce, the PIN key and its binding. Not yet checked.
#[derive(Debug, Clone)]
pub struct PinKey {
    pub nonce: [u8; NONCE_LEN],
    pub key: PublicKey,
    pub binding: Vec<u8>,
}

impl PinKey {
    /// nonce (16) ‖ a compressed secp256k1 point (33) ‖ DER (8..72).
    pub fn parse(answer: &[u8]) -> Result<Self> {
        let n = answer.len();
        if !(NONCE_LEN + PIN_KEY_LEN + BINDING_MIN..=NONCE_LEN + PIN_KEY_LEN + BINDING_MAX).contains(&n) {
            return Err(untrusted(format!("GET_NONCE answered {n} bytes")));
        }
        let k = &answer[NONCE_LEN..NONCE_LEN + PIN_KEY_LEN];
        if !matches!(k[0], 0x02 | 0x03) {
            return Err(untrusted("the PIN key is not a compressed point"));
        }
        let key = PublicKey::from_slice(k).map_err(|_| untrusted("the PIN key is not a point of the curve"))?;
        Ok(Self { nonce: answer[..NONCE_LEN].try_into().expect("16"), key, binding: answer[NONCE_LEN + PIN_KEY_LEN..].to_vec() })
    }

    /// The binding verifies as ECDSA P-256 SHA-256 under `attest_key` (SEC1) over
    /// [`binding_message`], with a high or a low s (the card does not normalise s).
    pub fn check(&self, attest_key: &[u8]) -> Result<()> {
        let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(attest_key)
            .map_err(|_| untrusted("the certified attestation key is not a P-256 point"))?;
        let sig = p256::ecdsa::Signature::from_der(&self.binding).map_err(|_| untrusted("the PIN key's binding is not a DER signature"))?;
        vk.verify(&binding_message(&self.key.serialize()), &sig)
            .map_err(|_| untrusted("the PIN key's binding does not verify under the certified attestation key"))
    }
}

/// A card whose certificate this host checked: GET_PUB 0, GET_CERT, the certificate verified under
/// the registry's certificate key, and naming this card's slot 0. Only [`trust_card`] makes one,
/// and every sealed PIN call takes one.
#[derive(Debug, Clone)]
pub struct TrustedCard {
    cert: Certificate,
    slot0: XOnlyPublicKey,
}

impl TrustedCard {
    pub fn certificate(&self) -> &Certificate {
        &self.cert
    }
    pub fn slot0(&self) -> XOnlyPublicKey {
        self.slot0
    }
}

/// Check the card in the field against the registry's certificate key `cert_key` (SEC1 bytes):
/// GET_PUB 0, GET_CERT, [`Certificate::verify`], slot 0. A card that fails is [`UntrustedCard`];
/// a transport failure is returned as it is.
pub fn trust_card(card: &mut dyn Card, cert_key: &[u8]) -> Result<TrustedCard> {
    let slot0 = card.get_pub(0).context("reading the card's slot 0 key")?;
    let raw = match card.get_cert() {
        Ok(c) => c,
        Err(e) if status_word(&e).is_some() => return Err(untrusted(format!("the card has no certificate ({e:#})"))),
        Err(e) => return Err(e),
    };
    let cert = Certificate::parse(&raw).map_err(|e| untrusted(format!("{e:#}")))?;
    cert.verify(cert_key).map_err(|e| untrusted(format!("{e:#}")))?;
    if cert.slot0 != slot0.serialize() {
        return Err(untrusted("the certificate is for another card"));
    }
    Ok(TrustedCard { cert, slot0 })
}

/// One sealed PIN command for one tap: GET_NONCE, the PIN key checked under the trusted card's
/// certified attestation key, then sealed once. [`send`](Self::send) sends the same bytes every
/// time, which is what the card's wait asks for (`6A98`: send it again). A new tap, a new SELECT
/// or a `6985` needs a new one.
pub struct SealedCommand {
    apdu: Vec<u8>,
}

impl SealedCommand {
    /// `clear` is the PIN block (VERIFY_PIN, SET_PIN) or old ‖ new (CHANGE_PIN), wiped by the
    /// caller. The PIN key is refused ([`UntrustedCard`]) before anything is sealed unless it checks.
    pub fn new(card: &mut dyn Card, trust: &TrustedCard, ins: u8, clear: &[u8]) -> Result<Self> {
        let answer = card.get_nonce().map_err(|e| {
            if status_word(&e) == Some(crate::SW_INS_NOT_SUPPORTED) { e.context(NO_SEALED_PIN_MESSAGE) } else { e.context("GET_NONCE") }
        })?;
        let pk = PinKey::parse(&answer)?;
        pk.check(&trust.cert.attest_key)?;
        Ok(Self { apdu: sealed_apdu(ins, &seal(&pk.key, &pk.nonce, ins, clear)?) })
    }

    /// Send the sealed command. `Ok` is `9000`; a refusal carries its [`StatusWord`].
    pub fn send(&self, card: &mut dyn Card) -> Result<()> {
        card.apdu(&self.apdu).map(|_| ())
    }

    /// The command's bytes (ciphertext: nothing in it is the PIN's).
    pub fn bytes(&self) -> &[u8] {
        &self.apdu
    }
}
