//! Host side of the Fedimint spend-key card.
//!
//! The card holds secp256k1 keys in numbered slots and exposes three commands: `GEN_KEY`,
//! `GET_PUB` and `SIGN` (BIP340 over a 32-byte message). It never reveals a private key.
//!
//! * [`Card`] is the high-level API the federation tests use.
//! * [`CardClient`] implements it over any APDU [`Transport`] (bridge or PC/SC).
//! * [`SoftCard`] implements it in-process with real keys, for tests without a card.

use anyhow::{Context, Result, bail, ensure};
use secp256k1::schnorr::Signature;
use secp256k1::{Keypair, Message, SECP256K1, XOnlyPublicKey};

pub mod bridge;
#[cfg(feature = "pcsc")]
pub mod pcsc_backend;

pub use bridge::BridgeTransport;

/// Number of key slots on the applet.
pub const SLOTS: u8 = 8;
/// Status word the applet answers for an empty slot (or an occupied slot on GEN_KEY).
pub const SW_CONDITIONS_NOT_SATISFIED: u16 = 0x6985;

/// Upper bound on 61xx / 6Cxx follow-ups for one command.
const MAX_CHAIN: usize = 16;
const CLA: u8 = 0xB0;
const INS_GEN_KEY: u8 = 0x10;
const INS_GET_PUB: u8 = 0x11;
const INS_SIGN: u8 = 0x12;
/// `SELECT` by AID `F0 46 4D 43 41 52 44 01`.
pub const SELECT_APDU: [u8; 14] = [
    0x00, 0xA4, 0x04, 0x00, 0x08, 0xF0, 0x46, 0x4D, 0x43, 0x41, 0x52, 0x44, 0x01, 0x00,
];

/// Error carrying the card's status word, so callers can match on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusWord(pub u16);

impl std::fmt::Display for StatusWord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "card returned status word {:04X}", self.0)
    }
}
impl std::error::Error for StatusWord {}

/// Returns the status word if `err` came from the card refusing a command.
pub fn status_word(err: &anyhow::Error) -> Option<u16> {
    err.downcast_ref::<StatusWord>().map(|s| s.0)
}

pub trait Card {
    /// Generate a key in `slot`. Fails with `6985` if the slot is already used.
    fn gen_key(&mut self, slot: u8) -> Result<XOnlyPublicKey>;
    /// Wipe `slot` and generate a fresh key in it (GEN_KEY with P2 = 1). Works on a used or an
    /// empty slot; the old key is gone for good.
    ///
    /// Contract: an overwrite destroys the spend key. The caller must first prove that nothing
    /// is still locked to the slot's current pubkey, and record the pending overwrite together
    /// with the old pubkey before sending. On any failure (error, timeout, torn connection) call
    /// `get_pub` to learn the outcome: `6985` means the slot is empty; a different pubkey means
    /// the new key exists, so adopt it; the old pubkey means the command never ran. Never retry
    /// a P2 = 1 GEN_KEY blindly.
    fn gen_key_overwrite(&mut self, slot: u8) -> Result<XOnlyPublicKey>;
    /// x-only public key of the key in `slot`. Fails with `6985` if the slot is empty.
    fn get_pub(&mut self, slot: u8) -> Result<XOnlyPublicKey>;
    /// BIP340 signature over the raw 32 bytes.
    fn sign(&mut self, slot: u8, msg32: &[u8; 32]) -> Result<Signature>;

    /// Public key of `slot`, generating a key first if the slot is empty. Idempotent, and safe
    /// when several tests race for the same slot.
    fn get_or_gen(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        match self.get_pub(slot) {
            Ok(pk) => Ok(pk),
            Err(e) if status_word(&e) == Some(SW_CONDITIONS_NOT_SATISFIED) => {
                match self.gen_key(slot) {
                    Ok(pk) => Ok(pk),
                    // Lost a race with another opener of the same slot.
                    Err(e) if status_word(&e) == Some(SW_CONDITIONS_NOT_SATISFIED) => self.get_pub(slot),
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }
}

/// Sends one APDU and returns `data || SW1 SW2`.
pub trait Transport {
    fn transmit(&mut self, apdu: &[u8]) -> Result<Vec<u8>>;
}

pub struct CardClient<T: Transport> {
    transport: T,
}

impl<T: Transport> CardClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Select the applet. Needed on a real card; harmless on the bridge.
    pub fn select(&mut self) -> Result<()> {
        self.exchange(&SELECT_APDU).map(|_| ())
    }

    pub fn into_transport(self) -> T {
        self.transport
    }

    /// Send an APDU, follow `61xx` with GET RESPONSE and `6Cxx` by resending with Le = xx
    /// (every APDU we build ends in its Le byte), and split off the status word.
    fn exchange(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        let mut resp = self.transport.transmit(apdu)?;
        for _ in 0..MAX_CHAIN {
            ensure!(resp.len() >= 2, "response shorter than a status word");
            let (body, sw) = resp.split_at(resp.len() - 2);
            data.extend_from_slice(body);
            match (sw[0], sw[1]) {
                (0x90, 0x00) => return Ok(data),
                (0x61, n) => resp = self.transport.transmit(&[0x00, 0xC0, 0x00, 0x00, n])?,
                (0x6C, _) if apdu.len() >= 4 && apdu[1] == INS_GEN_KEY && apdu[3] == 0x01 => {
                    // Never resend an overwrite: the first attempt may already have replaced the key.
                    return Err(StatusWord(0x6C00 | u16::from(sw[1])).into());
                }
                (0x6C, n) => {
                    let mut again = apdu.to_vec();
                    *again.last_mut().expect("non-empty APDU") = n;
                    resp = self.transport.transmit(&again)?;
                }
                (a, b) => return Err(StatusWord(u16::from_be_bytes([a, b])).into()),
            }
        }
        bail!("card kept answering 61xx/6Cxx for {MAX_CHAIN} rounds")
    }

    fn pubkey_cmd(&mut self, ins: u8, slot: u8, p2: u8) -> Result<XOnlyPublicKey> {
        let out = self.exchange(&[CLA, ins, slot, p2, 0x00])?;
        XOnlyPublicKey::from_slice(&out).context("card returned an invalid x-only public key")
    }
}

impl<T: Transport> Card for CardClient<T> {
    fn gen_key(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        self.pubkey_cmd(INS_GEN_KEY, slot, 0x00)
    }

    fn gen_key_overwrite(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        self.pubkey_cmd(INS_GEN_KEY, slot, 0x01)
    }

    fn get_pub(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        self.pubkey_cmd(INS_GET_PUB, slot, 0x00)
    }

    fn sign(&mut self, slot: u8, msg32: &[u8; 32]) -> Result<Signature> {
        let mut apdu = vec![CLA, INS_SIGN, slot, 0x00, 0x20];
        apdu.extend_from_slice(msg32);
        apdu.push(0x00);
        let out = self.exchange(&apdu)?;
        Signature::from_slice(&out).context("card returned an invalid signature")
    }
}

/// In-process stand-in for the applet, with the same slot semantics (including `6985`).
#[derive(Default)]
pub struct SoftCard {
    slots: [Option<Keypair>; SLOTS as usize],
}

impl SoftCard {
    pub fn new() -> Self {
        Self::default()
    }

    fn slot(&self, slot: u8) -> Result<&Option<Keypair>> {
        self.slots.get(slot as usize).ok_or_else(|| StatusWord(0x6B00).into())
    }
}

impl Card for SoftCard {
    fn gen_key(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        if self.slot(slot)?.is_some() {
            return Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into());
        }
        let kp = Keypair::new(SECP256K1, &mut rand::thread_rng());
        self.slots[slot as usize] = Some(kp);
        Ok(kp.x_only_public_key().0)
    }

    fn gen_key_overwrite(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        self.slot(slot)?;
        let kp = Keypair::new(SECP256K1, &mut rand::thread_rng());
        self.slots[slot as usize] = Some(kp);
        Ok(kp.x_only_public_key().0)
    }

    fn get_pub(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        match self.slot(slot)? {
            Some(kp) => Ok(kp.x_only_public_key().0),
            None => Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into()),
        }
    }

    fn sign(&mut self, slot: u8, msg32: &[u8; 32]) -> Result<Signature> {
        match self.slot(slot)? {
            Some(kp) => Ok(SECP256K1.sign_schnorr(&Message::from_digest(*msg32), kp)),
            None => Err(StatusWord(SW_CONDITIONS_NOT_SATISFIED).into()),
        }
    }
}

/// Pick a backend from the environment.
///
/// * `CARD_BACKEND=soft` (default): [`SoftCard`].
/// * `CARD_BACKEND=bridge`: HTTP bridge at `CARD_BRIDGE_URL` (default `http://127.0.0.1:9876`).
/// * `CARD_BACKEND=pcsc`: first reader, or the one named by `CARD_PCSC_READER`; needs the `pcsc` feature.
pub fn open_from_env() -> Result<Box<dyn Card>> {
    let backend = std::env::var("CARD_BACKEND").unwrap_or_else(|_| "soft".into());
    match backend.as_str() {
        "soft" => Ok(Box::new(SoftCard::new())),
        "bridge" => {
            let url = bridge::url_from_env();
            let mut c = CardClient::new(BridgeTransport::new(&url));
            c.select().with_context(|| format!("bridge at {url}"))?;
            Ok(Box::new(c))
        }
        #[cfg(feature = "pcsc")]
        "pcsc" => {
            let mut c = CardClient::new(pcsc_backend::PcscTransport::connect(
                std::env::var("CARD_PCSC_READER").ok().as_deref(),
            )?);
            c.select()?;
            Ok(Box::new(c))
        }
        #[cfg(not(feature = "pcsc"))]
        "pcsc" => bail!("CARD_BACKEND=pcsc needs `--features pcsc`"),
        other => bail!("unknown CARD_BACKEND {other:?} (soft|bridge|pcsc)"),
    }
}
