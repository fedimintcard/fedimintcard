//! The sealed PIN's shared vectors (`docs/OFFLINE-PROTOCOL.md` § 9): the reference that makes
//! `spec/vectors/sealed-pin.json`, and the check that the file is what it makes.
//!
//! The construction is Cashu Card 1.9's sealed PIN as cashucard ships it at be4b129
//! (`CashuApplet.unseal`, `sealBlock`, `unblockPin`, `processGetPinKey`), with our two labels, and
//! our binding: ECDSA P-256 SHA-256 by the card's attestation key, the signature ATTEST already
//! makes, in place of Cashu's BIP-340 by its card key. Nothing else is ours.
//!
//! `cashu_vectors_reproduce` runs the same code under Cashu's label against three envelopes and
//! the PIN-key signature of Cashu's published transcript (cashucard `spec/vectors/transcript.json`
//! at be4b129, the `pinkey` and `sealed` entries) and must reproduce them byte for byte. The
//! vectors of ours are only trusted because that test passes.
//!
//! The applet, the library and the Setup app each read the JSON file and must
//! agree with every entry. To make it again after a change here:
//! `SEALED_PIN_WRITE=1 cargo test -p fedimint-card-host-card --test sealed_pin_vectors`.

use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use secp256k1::{Message, PublicKey, Scalar, Secp256k1, SecretKey, XOnlyPublicKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const SEAL_LABEL: &[u8] = b"fedimintcard/seal1";
const BIND_LABEL: &[u8] = b"fedimintcard/pin1";
const CASHU_SEAL_LABEL: &[u8] = b"CashuCard/seal";
const CASHU_PINKEY_LABEL: &[u8] = b"CashuCard/pinkey";
const CLA: u8 = 0xB0;
const INS_VERIFY: u8 = 0x40;
const INS_SET: u8 = 0x41;
const INS_CHANGE: u8 = 0x42;
const TAG_LEN: usize = 16;

/// The card's PIN key scalar in these vectors: Cashu's test scalar (`PIN_D`, CashuAppletTest.java).
const PIN_D: &str = "3141592653589793238462643383279502884197169399375105820974944592";
/// The card's attestation key scalar in these vectors.
const ATTEST_D: &str = "a77e57a77e57a77e57a77e57a77e57a77e57a77e57a77e57a77e57a77e57a77e";
/// Another P-256 key, for a binding that must be refused.
const OTHER_ATTEST_D: &str = "0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad";

fn h(s: &str) -> Vec<u8> {
    hex::decode(s).expect("hex")
}
fn x(b: &[u8]) -> String {
    hex::encode(b)
}
fn s32(s: &str) -> [u8; 32] {
    h(s).try_into().expect("32 bytes")
}
/// Cashu's one-message scalars (`EPH`, CashuAppletTest.java): 0x11…11, 0x22…22, …
fn eph(k: u8) -> [u8; 32] {
    [k * 0x11; 32]
}

/// The secp256k1 public key of a scalar, compressed (33).
fn pub33(d: &[u8; 32]) -> [u8; 33] {
    PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(d).unwrap()).serialize()
}

/// The sender's side: E = e·G (65, 04 ‖ X ‖ Y) and the x of e·P (32), P the card's PIN key.
fn agree_sender(e: &[u8; 32], pin_key: &[u8]) -> ([u8; 65], [u8; 32]) {
    let secp = Secp256k1::new();
    let big_e = PublicKey::from_secret_key(&secp, &SecretKey::from_slice(e).unwrap()).serialize_uncompressed();
    let shared =
        PublicKey::from_slice(pin_key).unwrap().mul_tweak(&secp, &Scalar::from_be_bytes(*e).unwrap()).unwrap().serialize_uncompressed();
    (big_e, shared[1..33].try_into().unwrap())
}

/// The card's side: the x of d·E, or None where E is not a point of the curve in the 04 form
/// (the card's `requireFullPoint`, then its key agreement).
fn agree_card(d: &[u8; 32], big_e: &[u8]) -> Option<[u8; 32]> {
    if big_e.len() != 65 || big_e[0] != 0x04 {
        return None;
    }
    let e = PublicKey::from_slice(big_e).ok()?;
    let shared = e.mul_tweak(&Secp256k1::new(), &Scalar::from_be_bytes(*d).ok()?).ok()?.serialize_uncompressed();
    Some(shared[1..33].try_into().unwrap())
}

/// block(i, m) = SHA-256(label ‖ i (1) ‖ x (32) ‖ E (65) ‖ nonce (16) ‖ INS (1) ‖ m)
fn block(label: &[u8], i: u8, sx: &[u8; 32], big_e: &[u8], nonce: &[u8], ins: u8, more: &[u8]) -> [u8; 32] {
    let mut d = Sha256::new();
    d.update(label);
    d.update([i]);
    d.update(sx);
    d.update(big_e);
    d.update(nonce);
    d.update([ins]);
    d.update(more);
    d.finalize().into()
}

struct Sealed {
    big_e: [u8; 65],
    sx: [u8; 32],
    keystream: Vec<[u8; 32]>,
    ct: Vec<u8>,
    block0: [u8; 32],
    data: Vec<u8>,
}

/// The envelope E ‖ ct ‖ tag: ct = clear XOR blocks 1, 2, …; tag = block 0 over ct, first 16 bytes.
fn seal(label: &[u8], pin_key: &[u8], e: &[u8; 32], nonce: &[u8], ins: u8, clear: &[u8]) -> Sealed {
    let (big_e, sx) = agree_sender(e, pin_key);
    let mut ct = clear.to_vec();
    let mut keystream = Vec::new();
    for (i, chunk) in ct.chunks_mut(32).enumerate() {
        let k = block(label, (i + 1) as u8, &sx, &big_e, nonce, ins, &[]);
        chunk.iter_mut().zip(k).for_each(|(b, kb)| *b ^= kb);
        keystream.push(k);
    }
    let block0 = block(label, 0, &sx, &big_e, nonce, ins, &ct);
    let mut data = big_e.to_vec();
    data.extend(&ct);
    data.extend(&block0[..TAG_LEN]);
    Sealed { big_e, sx, keystream, ct, block0, data }
}

/// The card's opening of `data` under its live nonce, for the instruction it was sent with:
/// the clear bytes, or None where the envelope does not open (no point, or not this tag).
fn open(label: &[u8], d: &[u8; 32], data: &[u8], nonce: &[u8], ins: u8) -> Option<Vec<u8>> {
    if data.len() < 65 + 1 + TAG_LEN {
        return None;
    }
    let (big_e, rest) = data.split_at(65);
    let (ct, tag) = rest.split_at(rest.len() - TAG_LEN);
    let sx = agree_card(d, big_e)?;
    if block(label, 0, &sx, big_e, nonce, ins, ct)[..TAG_LEN] != *tag {
        return None;
    }
    let mut clear = ct.to_vec();
    for (i, chunk) in clear.chunks_mut(32).enumerate() {
        let k = block(label, (i + 1) as u8, &sx, big_e, nonce, ins, &[]);
        chunk.iter_mut().zip(k).for_each(|(b, kb)| *b ^= kb);
    }
    Some(clear)
}

/// A sealed command: CLA B0, INS, P1 = 01, P2 = 00, Lc, the envelope; no Le.
fn sealed_apdu(ins: u8, data: &[u8]) -> Vec<u8> {
    let mut a = vec![CLA, ins, 0x01, 0x00, data.len() as u8];
    a.extend(data);
    a
}

/// The PIN block: the PIN's length (1), the PIN, zeros to eight: always 9 bytes.
fn pin_block(pin: &[u8]) -> Vec<u8> {
    let mut b = vec![pin.len() as u8];
    b.extend(pin);
    b.resize(9, 0);
    b
}

fn binding_message(pin_key: &[u8]) -> Vec<u8> {
    [BIND_LABEL, pin_key].concat()
}

fn attest_key(d: &str) -> SigningKey {
    SigningKey::from_slice(&h(d)).unwrap()
}

fn p256_pub(sk: &SigningKey, compressed: bool) -> Vec<u8> {
    VerifyingKey::from(sk).to_encoded_point(compressed).as_bytes().to_vec()
}

/// Whether `der` is the card's binding of `pin_key` under the attestation key `attest33`: the check
/// every host makes before it seals (§ 9). High or low s: a card's ECDSA does not normalise s.
fn binding_holds(attest33: &[u8], pin_key: &[u8], der: &[u8]) -> bool {
    let Ok(vk) = VerifyingKey::from_sec1_bytes(attest33) else { return false };
    let Ok(sig) = Signature::from_der(der) else { return false };
    (8..=72).contains(&der.len())
        && PublicKey::from_slice(pin_key).is_ok()
        && pin_key.len() == 33
        && vk.verify(&binding_message(pin_key), &sig).is_ok()
}

// One vector entry's fields, each named as the JSON names it.
#[allow(clippy::too_many_arguments)]
fn sealed_entry(
    name: &str,
    cmd: u8,
    s: &Sealed,
    sealed_ins: u8,
    nonce: &[u8],
    card_nonce: &[u8],
    e: &[u8; 32],
    clear: &[u8],
    data: &[u8],
    spoil: &str,
    sw: &str,
) -> Value {
    let d = s32(PIN_D);
    let opened = open(SEAL_LABEL, &d, data, card_nonce, cmd);
    json!({
        "name": name,
        "ins": format!("{cmd:02x}"),
        "sealed_for_ins": format!("{sealed_ins:02x}"),
        "pin_key": x(&pub33(&d)),
        "nonce": x(nonce),
        "card_nonce": x(card_nonce),
        "eph_scalar": x(e),
        "E": x(&s.big_e),
        "x": x(&s.sx),
        "clear": x(clear),
        "keystream": s.keystream.iter().map(|k| x(k)).collect::<Vec<_>>(),
        "ct": x(&s.ct),
        "block0": x(&s.block0),
        "tag": x(&s.block0[..TAG_LEN]),
        "spoil": spoil,
        "apdu": x(&sealed_apdu(cmd, data)),
        "opens": opened.is_some(),
        "opened": opened.map(|c| x(&c)).unwrap_or_default(),
        "sw": sw,
    })
}

fn nonce(k: u8) -> Vec<u8> {
    vec![0xA0 + k; 16]
}

/// Every vector of ours, from the reference above.
fn vectors() -> Value {
    let d = s32(PIN_D);
    let pin_key = pub33(&d);
    let att = attest_key(ATTEST_D);
    let att33 = p256_pub(&att, true);

    // ---- the binding, and GET_NONCE's answer, for DERs of 70, 71 and 72 bytes.
    let mut bindings = Vec::new();
    let mut answers = Vec::new();
    let mut add = |name: String, key: &[u8], scalar: &[u8], der: &[u8], high: bool, signer33: &[u8], n: &[u8]| {
        let valid = binding_holds(&att33, key, der);
        bindings.push(json!({
            "name": name, "pin_scalar": x(scalar), "pin_key": x(key), "attest_key": x(&att33),
            "signed_by": x(signer33), "message": x(&binding_message(key)), "der": x(der),
            "der_len": der.len(), "high_s": high, "valid": valid,
        }));
        if valid {
            let answer = [n, key, der].concat();
            answers.push(json!({
                "name": format!("GET_NONCE answer, a {}-byte DER", der.len()),
                "apdu": "b044010000", "nonce": x(n), "pin_key": x(key), "der": x(der),
                "answer": x(&answer), "answer_len": answer.len(), "sw": "9000",
            }));
        }
    };
    let sig: Signature = att.sign(&binding_message(&pin_key));
    let high = sig.normalize_s().is_some();
    let der = sig.to_der().as_bytes().to_vec();
    add("the vectors' card: its PIN key bound".into(), &pin_key, &d, &der, high, &att33, &nonce(0));
    let (r, s) = sig.split_scalars();
    let flipped = Signature::from_scalars(r, -s).unwrap();
    add(
        "the same binding with s replaced by n - s: still the card's".into(),
        &pin_key,
        &d,
        flipped.to_der().as_bytes(),
        !high,
        &att33,
        &nonce(0),
    );
    let mut want = vec![70usize, 71, 72];
    want.retain(|l| *l != der.len());
    for k in 1u8..=255 {
        if want.is_empty() {
            break;
        }
        let dk = [k; 32];
        let key = pub33(&dk);
        let sig: Signature = att.sign(&binding_message(&key));
        let der = sig.to_der().as_bytes().to_vec();
        if let Some(i) = want.iter().position(|l| *l == der.len()) {
            want.remove(i);
            add(
                format!("a PIN key of scalar {k:02x}…{k:02x}: a {}-byte DER", der.len()),
                &key,
                &dk,
                &der,
                sig.normalize_s().is_some(),
                &att33,
                &nonce(k),
            );
        }
    }
    assert!(want.is_empty(), "no PIN key below 256 gave DERs of {want:?} bytes");
    let other = attest_key(OTHER_ATTEST_D);
    let by_other: Signature = other.sign(&binding_message(&pin_key));
    add(
        "signed by another P-256 key: refused".into(),
        &pin_key,
        &d,
        by_other.to_der().as_bytes(),
        by_other.normalize_s().is_some(),
        &p256_pub(&other, true),
        &nonce(0),
    );
    add("the card's signature over another PIN key: refused".into(), &pub33(&[7u8; 32]), &[7u8; 32], &der, high, &att33, &nonce(0));

    // ---- the envelopes.
    let mut sealed = Vec::new();
    let cases: [(&str, u8, u8, Vec<u8>, &str); 4] = [
        ("SET_PIN sealed, PIN 1234", INS_SET, 1, pin_block(b"1234"), "9000"),
        ("VERIFY_PIN sealed, PIN 1234", INS_VERIFY, 2, pin_block(b"1234"), "9000"),
        ("VERIFY_PIN sealed, PIN 12345678", INS_VERIFY, 3, pin_block(b"12345678"), "9000"),
        (
            "CHANGE_PIN sealed, 1234 to 567890: old block then new block",
            INS_CHANGE,
            4,
            [pin_block(b"1234"), pin_block(b"567890")].concat(),
            "9000",
        ),
    ];
    for (name, ins, k, clear, sw) in &cases {
        let n = nonce(*k);
        let e = eph(*k);
        let s = seal(SEAL_LABEL, &pin_key, &e, &n, *ins, clear);
        sealed.push(sealed_entry(name, *ins, &s, *ins, &n, &n, &e, clear, &s.data, "", sw));
    }
    // Spoiled: what the card answers to each, first wrong try in a row on VERIFY_PIN and CHANGE_PIN.
    for (cmd, k, fail_sw, clear) in [
        (INS_VERIFY, 2u8, "63c1", pin_block(b"1234")),
        (INS_SET, 1, "6700", pin_block(b"1234")),
        (INS_CHANGE, 4, "63c1", [pin_block(b"1234"), pin_block(b"567890")].concat()),
    ] {
        let n = nonce(k);
        let e = eph(k);
        let good = seal(SEAL_LABEL, &pin_key, &e, &n, cmd, &clear);
        let mut tag = good.data.clone();
        *tag.last_mut().unwrap() ^= 1;
        let mut body = good.data.clone();
        body[65] ^= 1;
        let mut point = good.data.clone();
        point[1..65].fill(0);
        let other_ins = if cmd == INS_SET { INS_VERIFY } else { INS_SET };
        let for_other = seal(SEAL_LABEL, &pin_key, &e, &n, other_ins, &clear);
        let stale_n = nonce(k + 8);
        let stale = seal(SEAL_LABEL, &pin_key, &e, &stale_n, cmd, &clear);
        let mut bad_block = clear.clone();
        let at = if cmd == INS_CHANGE { 9 } else { 0 };
        bad_block[at] = 3;
        let bad = seal(SEAL_LABEL, &pin_key, &e, &n, cmd, &bad_block);
        let c = format!("{cmd:02x}");
        sealed.push(sealed_entry(
            &format!("INS {c}: the last byte of the tag changed"),
            cmd,
            &good,
            cmd,
            &n,
            &n,
            &e,
            &clear,
            &tag,
            "tag",
            fail_sw,
        ));
        sealed.push(sealed_entry(
            &format!("INS {c}: the first byte of ct changed"),
            cmd,
            &good,
            cmd,
            &n,
            &n,
            &e,
            &clear,
            &body,
            "body",
            fail_sw,
        ));
        sealed.push(sealed_entry(
            &format!("INS {c}: E replaced by 04 and zeros, no point"),
            cmd,
            &good,
            cmd,
            &n,
            &n,
            &e,
            &clear,
            &point,
            "point",
            fail_sw,
        ));
        sealed.push(sealed_entry(
            &format!("INS {c}: sealed for INS {other_ins:02x}"),
            cmd,
            &for_other,
            other_ins,
            &n,
            &n,
            &e,
            &clear,
            &for_other.data,
            "for_ins",
            fail_sw,
        ));
        sealed.push(sealed_entry(
            &format!("INS {c}: sealed under a nonce the card does not hold (a nonce asked again mid-wait)"),
            cmd,
            &stale,
            cmd,
            &stale_n,
            &n,
            &e,
            &clear,
            &stale.data,
            "stale_nonce",
            fail_sw,
        ));
        sealed.push(sealed_entry(
            &format!("INS {c}: opens to a PIN block of length 3: no try"),
            cmd,
            &bad,
            cmd,
            &n,
            &n,
            &e,
            &bad_block,
            &bad.data,
            "block_len",
            "6700",
        ));
    }

    let pins: Vec<Value> = [&b"1234"[..], b"567890", b"12345678"]
        .iter()
        .map(|p| json!({"pin": String::from_utf8(p.to_vec()).unwrap(), "block": x(&pin_block(p))}))
        .collect();

    json!({
        "about": "The sealed PIN (docs/OFFLINE-PROTOCOL.md § 9). Made by host/card/tests/sealed_pin_vectors.rs, never by hand; that test fails if this file differs from what it makes. Hex is lower case. `sw` is the card's answer to `apdu` on a card holding `card_nonce` live, with no wrong try before it; an `apdu` carries no Le, and a host may append Le 00.",
        "labels": {"seal": x(SEAL_LABEL), "binding": x(BIND_LABEL)},
        "card": {
            "pin_scalar": PIN_D, "pin_key": x(&pin_key),
            "attest_scalar": ATTEST_D, "attest_key": x(&att33), "attest_key_uncompressed": x(&p256_pub(&att, false)),
        },
        "pin_blocks": pins,
        "binding": bindings,
        "get_nonce": answers,
        "sealed": sealed,
        "refusals": refusals(),
        "cashu": cashu_entries(),
    })
}

/// Each command's checks, in the order the card makes them, with its answer (§ 9).
fn refusals() -> Value {
    json!({
        "44": [
            {"check": "P1 != 01 or P2 != 00", "sw": "6b00"},
            {"check": "no certificate on the card (before SET_CERT)", "sw": "6985"},
            {"check": "the answer: nonce (16) || PIN key (33) || binding (DER, 8..72); the nonce is live from here", "sw": "9000"},
        ],
        "40": [
            {"check": "no PIN set", "sw": "6984"},
            {"check": "P1 != 01 or P2 != 00 (the clear form, P1 = 00, included)", "sw": "6b00"},
            {"check": "Lc != 90", "sw": "6700"},
            {"check": "no live nonce (writes nothing)", "sw": "6985"},
            {"check": "a wait owed: the same command again", "sw": "6a98"},
            {"check": "fewer than Lc bytes received", "sw": "6700"},
            {"check": "the envelope does not open: the nonce is spent and it is a wrong try", "sw": "63cx"},
            {"check": "the PIN block's length not 4..8: the nonce is spent, no try", "sw": "6700"},
            {"check": "the PIN: wrong", "sw": "63cx"},
            {"check": "the PIN: right; this selection has the PIN", "sw": "9000"},
        ],
        "41": [
            {"check": "P1 != 01 or P2 != 00 (the clear form, P1 = 00, included)", "sw": "6b00"},
            {"check": "a PIN is already set", "sw": "6985"},
            {"check": "Lc != 90", "sw": "6700"},
            {"check": "the clock not anchored in this selection", "sw": "6a96"},
            {"check": "no registry key, or a slot holds value", "sw": "6985"},
            {"check": "no live nonce", "sw": "6985"},
            {"check": "the signer's engine check fails", "sw": "6f00"},
            {"check": "fewer than Lc bytes received", "sw": "6700"},
            {"check": "the envelope does not open: the nonce is spent, nothing written", "sw": "6700"},
            {"check": "the PIN block's length not 4..8: nothing written", "sw": "6700"},
            {"check": "the PIN is set; this selection has the PIN", "sw": "9000"},
        ],
        "42": [
            {"check": "no PIN set", "sw": "6984"},
            {"check": "P1 != 01 or P2 != 00 (P1 00 included)", "sw": "6b00"},
            {"check": "this selection does not have the PIN", "sw": "6a94"},
            {"check": "Lc != 99", "sw": "6700"},
            {"check": "no live nonce", "sw": "6985"},
            {"check": "fewer than Lc bytes received", "sw": "6700"},
            {"check": "the envelope does not open: the nonce is spent, a wrong try, the session ends", "sw": "63cx"},
            {"check": "either PIN block's length not 4..8: the nonce is spent, no try", "sw": "6700"},
            {"check": "the old PIN wrong: the session ends", "sw": "63cx"},
            {"check": "the old PIN right: the new PIN is set and the session ends", "sw": "9000"},
        ],
    })
}

// ---- Cashu's own vectors, from cashucard spec/vectors/transcript.json at be4b129.

/// The `pinkey` answer before the sealed SET_PIN ("asked again"): nonce ‖ PIN key ‖ BIP-340 signature.
const CASHU_PINKEY_ANSWER: &str = "5b16a08c7a0c5f1570f966ef5fb5d40002cf9bfbdca0c087fdea7097d5482d3a7b30b5fa1cb52b621f89c0a61619b782d47fb4580ccd7149a9ea7a362e270cd4c7a466c77b01ecc952633a8071c9962f50d8e17bd0dd9883e9630ddf4dbc14e026bada6202833b06e17b866151019fd348";
/// The card key of that transcript ("its key", GET_PUBKEY).
const CASHU_CARD_KEY: &str = "02fa05ed0a3649886270bacec5ddc1a3643009a759bee996f9a6ac060c9992a82e";
/// (name, nonce of the `pinkey` entry before it, EPH index, INS, clear, the command as recorded).
const CASHU_SEALED: [(&str, &str, u8, u8, &str, &str); 3] = [
    (
        "SET_PIN sealed, under the PIN given: the new PIN as a PIN block, under the second nonce",
        "5b16a08c7a0c5f1570f966ef5fb5d400",
        1,
        0x41,
        "043536373800000000",
        "b04101005a044f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aa385b6b1b8ead809ca67454d9683fcf2ba03456d6fe2c4abe2b07f0fbdbb2f1c1e9f9e4dc6104c7cd7d2ae6bd1c1ae547938a343849295b9fe0",
    ),
    (
        "VERIFY_PIN sealed, the right PIN",
        "b2f3a2d4fbb002d9f0b51258af43e98a",
        2,
        0x40,
        "043536373800000000",
        "b04001005a04466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276728176c3c6431f8eeda4538dc37c865e2784f3a9e77d044f33e407797e1278af19bf8993ec84ec104be76ea038bcda180c47ab21c6b6fa8d4",
    ),
    (
        "VERIFY_PIN sealed, a wrong PIN on a card with no owner: one wrong try in a row",
        "5257ae460342361c2199d380415c0907",
        2,
        0x40,
        "043939393900000000",
        "b04001005a04466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276728176c3c6431f8eeda4538dc37c865e2784f3a9e77d044f33e407797e1278a5b26b96c8c618f65b21de117f13c6ead54caea2b2bb355d613",
    ),
];

fn cashu_pin_key() -> Vec<u8> {
    h(CASHU_PINKEY_ANSWER)[16..49].to_vec()
}

fn cashu_entries() -> Value {
    let key = cashu_pin_key();
    let mut out: Vec<Value> = CASHU_SEALED
        .iter()
        .map(|(name, n, k, ins, clear, apdu)| {
            json!({"name": name, "label": x(CASHU_SEAL_LABEL), "pin_key": x(&key), "nonce": n,
                   "eph_scalar": x(&eph(*k)), "ins": format!("{ins:02x}"), "clear": clear, "apdu": apdu})
        })
        .collect();
    out.push(json!({"name": "GET_NONCE P1 = 1: the card key's BIP-340 signature over SHA-256(\"CashuCard/pinkey\" || PIN key)",
                    "label": x(CASHU_PINKEY_LABEL), "card_key": CASHU_CARD_KEY, "answer": CASHU_PINKEY_ANSWER}));
    Value::Array(out)
}

#[test]
fn cashu_vectors_reproduce() {
    let key = cashu_pin_key();
    for (name, n, k, ins, clear, apdu) in CASHU_SEALED {
        let s = seal(CASHU_SEAL_LABEL, &key, &eph(k), &h(n), ins, &h(clear));
        let mut got = vec![CLA, ins, 0x01, 0x00, s.data.len() as u8];
        got.extend(&s.data);
        assert_eq!(x(&got), apdu, "{name}: our reference does not make Cashu's envelope");
    }
    let a = h(CASHU_PINKEY_ANSWER);
    assert_eq!(a.len(), 113);
    let digest: [u8; 32] = Sha256::new().chain_update(CASHU_PINKEY_LABEL).chain_update(&a[16..49]).finalize().into();
    let sig = secp256k1::schnorr::Signature::from_slice(&a[49..]).unwrap();
    let card = XOnlyPublicKey::from_slice(&h(CASHU_CARD_KEY)[1..]).unwrap();
    Secp256k1::new()
        .verify_schnorr(&sig, &Message::from_digest(digest), &card)
        .expect("Cashu's PIN-key signature is BIP-340 by the card key over SHA-256(label || key)");
}

#[test]
fn the_vectors_are_what_the_reference_makes() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../spec/vectors/sealed-pin.json");
    let made = vectors();
    if std::env::var_os("SEALED_PIN_WRITE").is_some() {
        std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(&made).unwrap() + "\n").unwrap();
    }
    let kept: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("spec/vectors/sealed-pin.json")).unwrap();
    assert_eq!(kept, made, "spec/vectors/sealed-pin.json is not what the reference makes: SEALED_PIN_WRITE=1 makes it again");
}

#[test]
fn the_card_opens_what_the_sender_seals() {
    // Two ways to the shared x (e·P on the sender's side, d·E on the card's) must agree, and
    // every entry's `opens` must follow from its spoil.
    let v = vectors();
    for e in v["sealed"].as_array().unwrap() {
        let opens = e["opens"].as_bool().unwrap();
        match e["spoil"].as_str().unwrap() {
            "" | "block_len" => assert!(opens, "{}", e["name"]),
            _ => assert!(!opens, "{}", e["name"]),
        }
        if opens {
            assert_eq!(e["opened"], e["clear"], "{}", e["name"]);
        }
    }
    for b in v["binding"].as_array().unwrap() {
        let refused = b["name"].as_str().unwrap().ends_with("refused");
        assert_eq!(b["valid"].as_bool().unwrap(), !refused, "{}", b["name"]);
    }
    let lens: Vec<u64> = v["get_nonce"].as_array().unwrap().iter().map(|a| a["der"].as_str().unwrap().len() as u64 / 2).collect();
    for l in [70, 71, 72] {
        assert!(lens.contains(&l), "a GET_NONCE answer with a {l}-byte DER");
    }
}
