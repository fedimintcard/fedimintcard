//! Vectors for the registry's port of Fedimint's note check (`registry/src/tbs.ts`, OPS-PLAN O4).
//!
//! The registry verifies each note a holder loads with the federation's aggregate key for the
//! note's tier. That code is TypeScript, so it is checked against what this crate's `tbs` and
//! `fedimint-mint-common` (the federation's own code) produce: `Message::from_bytes` for a few
//! inputs, and notes made by the full blind flow (blind, sign, unblind) under fixed keys, with
//! tampered copies that must fail.
//!
//! The vectors are checked in at `registry/test/tbs-vectors.json`. This test fails if the file
//! differs from what the code produces; `TBS_VECTORS_WRITE=1` rewrites it.

use fedimint_card_sidecar::glue::nonce_for;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::registry::ModuleRegistry;
use fedimint_core::secp256k1::{Keypair, SECP256K1, SecretKey};
use fedimint_mint_common::{Nonce, Note};
use serde_json::{Value, json};
use tbs::{AggregatePublicKey, BlindedSignature, BlindingKey, Message, SecretKeyShare};

/// A scalar from a small integer: the consensus encoding is 32 bytes little-endian.
fn scalar<T: Decodable>(n: u64) -> T {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&n.to_le_bytes());
    T::consensus_decode_whole(&bytes, &ModuleRegistry::default()).expect("a canonical scalar")
}

/// A card-style nonce: slot key `seed`, even parity, as `glue::nonce_for` makes them.
fn nonce(seed: u8) -> Nonce {
    let sk = SecretKey::from_slice(&[seed; 32]).expect("a valid key");
    nonce_for(Keypair::from_secret_key(SECP256K1, &sk).x_only_public_key().0)
}

fn hexof<T: Encodable>(t: &T) -> String {
    t.consensus_encode_to_hex()
}

fn vectors() -> Value {
    let messages: Vec<Value> = [b"".to_vec(), b"note".to_vec(), nonce(1).0.serialize().to_vec(), vec![0xff; 33]]
        .iter()
        .map(|bytes| json!({"bytes": hex::encode(bytes), "message": hexof(&Message::from_bytes(bytes))}))
        .collect();

    // Two tiers, each with its own key, as a federation's config has one per amount.
    let tiers: Vec<(u64, SecretKeyShare)> = vec![(1024, scalar(0x1234_5678)), (2048, scalar(0x9abc_def0))];
    let mut notes = vec![];
    for (i, (amount, sk)) in tiers.iter().enumerate() {
        let pk = AggregatePublicKey(tbs::derive_pk_share(sk).0);
        let other = AggregatePublicKey(tbs::derive_pk_share(&tiers[1 - i].1).0);
        for seed in [3u8, 4] {
            let n = nonce(seed + i as u8 * 10);
            let key: BlindingKey = scalar(1000 + seed as u64);
            let blinded = tbs::blind_message(n.to_message(), key);
            let blind_sig = BlindedSignature(tbs::sign_message(blinded, *sk).0);
            let signature = tbs::unblind_signature(key, blind_sig);
            let note = Note { nonce: n, signature };
            assert!(note.verify(pk), "the federation's own check passes");
            assert!(!note.verify(other), "and fails under the other tier's key");
            notes.push(json!({
                "amount_msat": amount,
                "nonce": hex::encode(n.0.serialize()),
                "signature": hexof(&signature),
                "pk": hexof(&pk),
                "other_tier_pk": hexof(&other),
                "message": hexof(&n.to_message()),
            }));
        }
    }
    json!({
        "source": "host/sidecar/tests/tbs_vectors.rs (fedimint v0.13.0-beta.1 tbs, mint-common Note::verify)",
        "messages": messages,
        "notes": notes,
    })
}

#[test]
fn registry_vectors_match_the_federation_code() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../registry/test/tbs-vectors.json");
    let want = serde_json::to_string_pretty(&vectors()).unwrap() + "\n";
    if std::env::var_os("TBS_VECTORS_WRITE").is_some() {
        std::fs::write(&path, &want).unwrap();
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(have == want, "{} is stale: run with TBS_VECTORS_WRITE=1", path.display());
}
