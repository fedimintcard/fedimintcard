//! `fedimint-card-pos-core`: the offline charge a till and a POS phone share
//! (docs/POS-OFFLINE.md). Synchronous, with no tokio code, fedimint client, rocksdb or registry
//! client, so it builds for iOS and Android as well as for the till.

use anyhow::{Context as _, Result};
use fedimint_core::encoding::Decodable;
use fedimint_core::module::registry::ModuleRegistry;
use fedimint_core::secp256k1::{Keypair, Message, Parity, PublicKey, SECP256K1, SecretKey, XOnlyPublicKey};
use fedimint_mint_common::{Nonce, Note};
use tbs::{BlindedMessage, BlindingKey};

#[cfg(feature = "ffi")]
uniffi::setup_scaffolding!();

pub mod charge;
#[cfg(feature = "ffi")]
pub mod ffi;
pub mod files;
pub mod kit;
pub mod plan;
pub mod queue;
pub mod refusal;
pub mod store;

pub use refusal::{Refusal, class_of, refusal};

/// The nonce for a card key. Parity is fixed to even so the 33 bytes blinded at issue are the
/// 33 bytes the federation sees at spend.
pub fn nonce_for(key: XOnlyPublicKey) -> Nonce {
    Nonce(PublicKey::from_x_only_public_key(key, Parity::Even))
}

/// The 48 bytes a note's stored signature (consensus hex) is on the card.
pub fn sig48_of(hex_sig: &str) -> Result<[u8; 48]> {
    hex::decode(hex_sig).ok().and_then(|b| b.try_into().ok()).context("a note signature that is not 48 bytes")
}

/// A card's short id: the last 4 bytes of its slot-0 x-only key, lower-case hex.
pub fn short_id_of(slot0: &XOnlyPublicKey) -> String {
    let hex = slot0.to_string();
    hex[hex.len() - 8..].to_owned()
}

/// The blinded message for a nonce under a blinding key.
pub fn blind(nonce: Nonce, key: BlindingKey) -> BlindedMessage {
    tbs::blind_message(nonce.to_message(), key)
}


/// A scalar from a small integer: its consensus encoding is 32 bytes little-endian.
fn scalar<T: Decodable>(n: u64) -> T {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&n.to_le_bytes());
    T::consensus_decode_whole(&bytes, &ModuleRegistry::default()).expect("a canonical scalar")
}

/// The crypto the offline charge needs, run where it runs: a note signed by a one-share mint,
/// unblinded and checked as `Note::verify` checks a card's note, a tampered one refused, and a
/// BIP340 signature as the card makes them. True when all three hold.
#[cfg_attr(feature = "ffi", uniffi::export)]
pub fn self_check() -> bool {
    let sk: tbs::SecretKeyShare = scalar(0x1234_5678);
    let pk = tbs::AggregatePublicKey(tbs::derive_pk_share(&sk).0);
    let key = |b: u8| Keypair::from_secret_key(SECP256K1, &SecretKey::from_slice(&[b; 32]).expect("a key"));
    let nonce = Nonce(key(7).public_key());
    let bk: tbs::BlindingKey = scalar(99);
    let blinded = tbs::blind_message(nonce.to_message(), bk);
    let signature = tbs::unblind_signature(bk, tbs::BlindedSignature(tbs::sign_message(blinded, sk).0));
    let good = Note { nonce, signature }.verify(pk);
    let tampered = Note { nonce: Nonce(key(8).public_key()), signature }.verify(pk);
    let msg = Message::from_digest([3; 32]);
    let schnorr = SECP256K1.verify_schnorr(&SECP256K1.sign_schnorr(&msg, &key(7)), &msg, &key(7).x_only_public_key().0).is_ok();
    good && !tampered && schnorr
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_self_check_passes() {
        assert!(super::self_check());
    }
}
