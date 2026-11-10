//! A kit for tests without a card reader or a federation (feature `softkit`): a [`SoftCard`]
//! behind an APDU [`Transport`], as a phone's NFC session or the relay carries a real card's
//! answers; a transport that loses the card at a chosen APDU; and a mint of one guardian whose
//! notes check with `Note::verify` exactly as a federation's do, to load onto a 3.0 SoftCard.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use fedimint_core::encoding::Encodable;
use secp256k1::XOnlyPublicKey;

use crate::{BridgeError, Card, SoftCard, StatusWord, Transport};

/// What the card answers to `apdu`, `data || SW`, as a reader returns it. Errors that carry no
/// status word answer `6F00`.
pub fn answer(card: &mut SoftCard, apdu: &[u8]) -> Vec<u8> {
    let r: Result<Vec<u8>> = match (apdu[0], apdu[1]) {
        (0xB0, 0x10) if apdu[3] == 1 => card.gen_key_overwrite(apdu[2]).map(|k| k.serialize().to_vec()),
        (0xB0, 0x10) => card.gen_key(apdu[2]).map(|k| k.serialize().to_vec()),
        (0xB0, 0x11) => card.get_pub(apdu[2]).map(|k| k.serialize().to_vec()),
        (0xB0, 0x12) => match apdu.get(5..37).and_then(|m| <[u8; 32]>::try_from(m).ok()) {
            Some(m) => card.sign(apdu[2], &m).map(|s| s.as_ref().to_vec()),
            None => Err(StatusWord(crate::v3::SW_WRONG_LENGTH).into()),
        },
        _ => card.apdu(apdu),
    };
    match r {
        Ok(mut d) => {
            d.extend_from_slice(&[0x90, 0x00]);
            d
        }
        Err(e) => e.downcast_ref::<StatusWord>().map_or(0x6F00, |s| s.0).to_be_bytes().to_vec(),
    }
}

/// A shared [`SoftCard`] behind a [`Transport`]: the test keeps a handle to look at the card.
#[derive(Clone)]
pub struct SoftTransport(pub Arc<Mutex<SoftCard>>);

impl SoftTransport {
    pub fn new(card: SoftCard) -> Self {
        Self(Arc::new(Mutex::new(card)))
    }
}

impl Transport for SoftTransport {
    fn transmit(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
        Ok(answer(&mut self.0.lock().unwrap(), apdu))
    }
}

/// A card taken out of the field: APDU number `at` (from 0) and every one after it fail with
/// [`BridgeError::NoCard`], as a phone's NFC session or the relay reports a lost card.
pub struct PullTransport<T> {
    inner: T,
    at: usize,
    sent: usize,
}

impl<T: Transport> PullTransport<T> {
    pub fn new(inner: T, at: usize) -> Self {
        Self { inner, at, sent: 0 }
    }

    /// APDUs asked of this transport so far, the failed ones included.
    pub fn sent(&self) -> usize {
        self.sent
    }

    /// Whether the card was pulled.
    pub fn pulled(&self) -> bool {
        self.sent > self.at
    }
}

impl<T: Transport> Transport for PullTransport<T> {
    fn transmit(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
        let n = self.sent;
        self.sent += 1;
        if n >= self.at {
            return Err(BridgeError::NoCard(format!("the card left the field at APDU {n}")).into());
        }
        self.inner.transmit(apdu)
    }
}

/// A mint of one guardian (threshold 1): per tier, its key share is the aggregate key, so a note
/// it signs is a note of a federation whose `tbs_pks` are [`SoftMint::tbs_pks`].
pub struct SoftMint {
    sks: BTreeMap<u64, tbs::SecretKeyShare>,
}

impl SoftMint {
    /// Keys for `tiers` (msat), random.
    pub fn new(tiers: impl IntoIterator<Item = u64>) -> Self {
        Self { sks: tiers.into_iter().map(|t| (t, tbs::SecretKeyShare(tbs::BlindingKey::random().0))).collect() }
    }

    /// Every power of two from 1 msat to 2^`max_exp` msat.
    pub fn powers_of_two(max_exp: u32) -> Self {
        Self::new((0..=max_exp).map(|e| 1u64 << e))
    }

    /// The aggregate key of each tier, msat to key.
    pub fn tbs_pks(&self) -> BTreeMap<u64, tbs::AggregatePublicKey> {
        self.sks.iter().map(|(t, sk)| (*t, tbs::AggregatePublicKey(tbs::derive_pk_share(sk).0))).collect()
    }

    /// The finished signature (48 bytes, consensus encoding) on the note whose nonce is `key`
    /// with even parity (the card's notes), made by the blind flow a federation runs.
    pub fn sign(&self, amount_msat: u64, key: &XOnlyPublicKey) -> Result<[u8; 48]> {
        let sk = *self.sks.get(&amount_msat).with_context(|| format!("{amount_msat} msat is not a tier of this mint"))?;
        let nonce = secp256k1::PublicKey::from_x_only_public_key(*key, secp256k1::Parity::Even);
        let bk = tbs::BlindingKey::random();
        let blinded = tbs::blind_message(tbs::Message::from_bytes(&nonce.serialize()), bk);
        let sig = tbs::unblind_signature(bk, tbs::BlindedSignature(tbs::sign_message(blinded, sk).0));
        sig.consensus_encode_to_vec().try_into().map_err(|_| anyhow::anyhow!("a signature is 48 bytes"))
    }

    /// Load a note of `amount_msat` onto `slot` of a 3.0 card (`SET_NOTE`), keying the slot first
    /// if it has no key. Answers the slot's key.
    pub fn load(&self, card: &mut SoftCard, slot: u8, amount_msat: u64) -> Result<XOnlyPublicKey> {
        let key = card.get_or_gen(slot)?;
        let sig = self.sign(amount_msat, &key)?;
        card.set_note(slot, amount_msat, &sig)?;
        Ok(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CardClient, SlotState, TEST_PIN, open_pin_session_with};

    #[test]
    fn a_soft_card_answers_through_a_transport_as_a_reader_would() {
        let t = SoftTransport::new(SoftCard::v3());
        let mut c = CardClient::new(t.clone());
        c.select().unwrap();
        assert!(c.applet_version().unwrap().sign_once());
        let key = c.get_or_gen(3).unwrap();
        assert_eq!(t.0.lock().unwrap().get_pub(3).unwrap(), key);
        // SIGN needs the PIN session; the card's refusal comes back as its status word.
        let e = c.sign(3, &[1; 32]).unwrap_err();
        assert_eq!(crate::status_word(&e), Some(crate::SW_NO_PIN_SET));
        open_pin_session_with(&mut c, TEST_PIN).unwrap();
        assert_eq!(c.get_slot(3).unwrap().state, SlotState::Keyed);
    }

    #[test]
    fn a_pulled_card_answers_nothing_from_the_chosen_apdu_on() {
        // SELECT, then GET_PUB and GEN_KEY of an empty slot: the fourth APDU is the first lost.
        let mut c = CardClient::new(PullTransport::new(SoftTransport::new(SoftCard::v3()), 3));
        c.select().unwrap();
        c.get_or_gen(0).unwrap();
        let e = c.get_slot(0).unwrap_err();
        assert!(matches!(e.downcast_ref::<BridgeError>(), Some(BridgeError::NoCard(_))), "{e:#}");
        assert!(c.get_slot(0).is_err(), "still gone");
        let t = c.into_transport();
        assert!(t.pulled());
        assert_eq!(t.sent(), 5);
    }

    #[test]
    fn the_soft_mint_signs_notes_its_keys_verify() {
        let mint = SoftMint::powers_of_two(20);
        let mut card = SoftCard::v3();
        let key = mint.load(&mut card, 5, 1 << 14).unwrap();
        let info = card.get_slot(5).unwrap();
        assert_eq!((info.state, info.amount_msat, info.xonly), (SlotState::Note, 1 << 14, Some(key)));
        let nonce = secp256k1::PublicKey::from_x_only_public_key(key, secp256k1::Parity::Even).serialize();
        let sig: tbs::Signature = fedimint_core::encoding::Decodable::consensus_decode_whole(
            &info.sig48,
            &fedimint_core::module::registry::ModuleRegistry::default(),
        )
        .unwrap();
        let pks = mint.tbs_pks();
        assert!(tbs::verify(tbs::Message::from_bytes(&nonce), sig, pks[&(1 << 14)]));
        assert!(!tbs::verify(tbs::Message::from_bytes(&nonce), sig, pks[&(1 << 13)]), "another tier's key");
        assert!(mint.sign(3, &key).is_err(), "3 msat is not a tier");
    }
}
