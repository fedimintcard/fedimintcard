//! The library's sealed PIN (docs/OFFLINE-PROTOCOL.md § 9) over the shared vectors
//! (`spec/vectors/sealed-pin.json`), and the SoftCard's sealed commands against a host: the
//! envelope, the binding, the one-envelope wait, and every refusal a host must meet.

use fedimint_card_host_card::sealed::{self, PinKey};
use fedimint_card_host_card::v3;
use fedimint_card_host_card::*;
use fedimint_card_old_card_pin::{open_pin_session_with, verify_pin_held_clear};
use secp256k1::{PublicKey, SecretKey};
use serde_json::Value;

const PIN: &[u8] = b"1234";
const H900000: &str = "00a0ab20247d4d9f582f9750344cdf62c46d81d046be960340960100000000000000000070f96945530651135839d8adc3f40e595118ec74c7ad81a3d17bb022e554fb0c937f4268743702177ad05f92";

fn vectors() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../spec/vectors/sealed-pin.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}
fn h(v: &Value) -> Vec<u8> {
    hex::decode(v.as_str().unwrap()).unwrap()
}
fn byte(v: &Value) -> u8 {
    u8::from_str_radix(v.as_str().unwrap(), 16).unwrap()
}
fn sw<T>(r: anyhow::Result<T>) -> u16 {
    match r {
        Ok(_) => 0x9000,
        Err(e) => status_word(&e).unwrap_or_else(|| panic!("not a status word: {e:#}")),
    }
}
fn anchor() -> [u8; 80] {
    hex::decode(H900000).unwrap().try_into().unwrap()
}

// ------------------------------------------------------------------ the vectors

#[test]
fn the_library_makes_every_envelope_of_the_vectors() {
    let v = vectors();
    let mut n = 0;
    for e in v["sealed"].as_array().unwrap() {
        // The envelopes a sender made as they stand (the others are those spoiled afterwards).
        if !matches!(e["spoil"].as_str().unwrap(), "" | "for_ins" | "stale_nonce" | "block_len") {
            continue;
        }
        let key = PublicKey::from_slice(&h(&e["pin_key"])).unwrap();
        let eph = SecretKey::from_slice(&h(&e["eph_scalar"])).unwrap();
        let env = sealed::seal_with(sealed::SEAL_LABEL, &eph, &key, &h(&e["nonce"]), byte(&e["sealed_for_ins"]), &h(&e["clear"])).unwrap();
        let apdu = sealed::sealed_apdu(byte(&e["ins"]), &env);
        assert_eq!(apdu.last(), Some(&0), "the library's APDUs end in Le");
        assert_eq!(hex::encode(&apdu[..apdu.len() - 1]), e["apdu"].as_str().unwrap(), "{}", e["name"]);
        n += 1;
    }
    assert_eq!(n, 4 + 3 * 3);
}

#[test]
fn the_cards_opening_agrees_with_every_entry() {
    let v = vectors();
    let d = SecretKey::from_slice(&h(&v["card"]["pin_scalar"])).unwrap();
    for e in v["sealed"].as_array().unwrap() {
        let data = &h(&e["apdu"])[5..];
        let opened = sealed::open_with(sealed::SEAL_LABEL, &d, &h(&e["card_nonce"]), byte(&e["ins"]), data);
        assert_eq!(opened.is_some(), e["opens"].as_bool().unwrap(), "{}", e["name"]);
        assert_eq!(opened.map(hex::encode).unwrap_or_default(), e["opened"].as_str().unwrap(), "{}", e["name"]);
    }
}

#[test]
fn the_pin_blocks_and_cashus_own_envelopes() {
    let v = vectors();
    for b in v["pin_blocks"].as_array().unwrap() {
        let pin = b["pin"].as_str().unwrap().as_bytes();
        assert_eq!(sealed::pin_block(pin).unwrap().to_vec(), h(&b["block"]));
        assert_eq!(sealed::unblock(&h(&b["block"])), Some(pin));
    }
    assert!(sealed::pin_block(b"123").is_err() && sealed::pin_block(b"123456789").is_err());
    let mut n = 0;
    for c in v["cashu"].as_array().unwrap().iter().filter(|c| c.get("apdu").is_some()) {
        let key = PublicKey::from_slice(&h(&c["pin_key"])).unwrap();
        let eph = SecretKey::from_slice(&h(&c["eph_scalar"])).unwrap();
        let env = sealed::seal_with(&h(&c["label"]), &eph, &key, &h(&c["nonce"]), byte(&c["ins"]), &h(&c["clear"])).unwrap();
        let apdu = sealed::sealed_apdu(byte(&c["ins"]), &env);
        assert_eq!(hex::encode(&apdu[..apdu.len() - 1]), c["apdu"].as_str().unwrap(), "{}", c["name"]);
        n += 1;
    }
    assert_eq!(n, 3, "Cashu's three envelopes, byte for byte");
}

#[test]
fn the_binding_is_checked_as_the_vectors_say() {
    let v = vectors();
    let attest = h(&v["card"]["attest_key"]);
    for a in v["get_nonce"].as_array().unwrap() {
        let pk = PinKey::parse(&h(&a["answer"])).unwrap();
        assert_eq!(pk.nonce.to_vec(), h(&a["nonce"]));
        assert_eq!(pk.key.serialize().to_vec(), h(&a["pin_key"]));
        pk.check(&attest).unwrap_or_else(|e| panic!("{}: {e:#}", a["name"]));
        assert_eq!(sealed::GET_NONCE_APDU.to_vec(), h(&a["apdu"]));
    }
    for b in v["binding"].as_array().unwrap() {
        assert_eq!(sealed::binding_message(&h(&b["pin_key"])), h(&b["message"]));
        let answer = [vec![0xA0; 16], h(&b["pin_key"]), h(&b["der"])].concat();
        let ok = PinKey::parse(&answer).and_then(|pk| pk.check(&attest)).is_ok();
        assert_eq!(ok, b["valid"].as_bool().unwrap(), "{}", b["name"]);
    }
    // Lengths and points: a DER past 72, a key that is no compressed point.
    let good = h(&v["get_nonce"][0]["answer"]);
    assert!(PinKey::parse(&[good.clone(), vec![0; 3]].concat()).is_err());
    let mut bad = good.clone();
    bad[16] = 0x04;
    assert!(PinKey::parse(&bad).is_err());
}

/// The vectors' card in the state each `sw` assumes: the PIN key `pin_scalar` and `card_nonce`
/// live, no wrong try. VERIFY_PIN: PIN 1234 set. SET_PIN: no PIN, anchored, the registry key.
/// CHANGE_PIN: PIN 1234 set and given in this selection.
#[test]
fn the_softcard_answers_every_entry_as_the_vectors_say() {
    let v = vectors();
    let d = SecretKey::from_slice(&h(&v["card"]["pin_scalar"])).unwrap();
    for e in v["sealed"].as_array().unwrap() {
        let ins = byte(&e["ins"]);
        let mut c = SoftCard::v4();
        c.use_pin_key(d);
        match ins {
            0x41 => {
                c.set_registry_key(&v3::registry_point(&v3::test_registry_key())).unwrap();
                c.set_header(&anchor()).unwrap();
            }
            _ => c.pinned_with(sealed::unblock(&h(&e["clear"])[..9]).unwrap_or(PIN)),
        }
        if ins == 0x42 {
            let n = [0x55; 16];
            c.use_nonce(n);
            let env = sealed::seal(&d.public_key(secp256k1::SECP256K1), &n, 0x40, &sealed::pin_block(PIN).unwrap()).unwrap();
            c.apdu(&sealed::sealed_apdu(0x40, &env)).unwrap();
        }
        c.use_nonce(h(&e["card_nonce"]).try_into().unwrap());
        let mut apdu = h(&e["apdu"]);
        apdu.push(0);
        let got = format!("{:04x}", sw(c.apdu(&apdu)));
        assert_eq!(got, e["sw"].as_str().unwrap(), "{}", e["name"]);
        assert!(!c.nonce_live(), "{}: the opening spent the nonce", e["name"]);
    }
}

// ------------------------------------------------------------------ against a host

/// A 4.0 card as set-up leaves it before SET_PIN: slot 0 keyed, the attestation key certified by
/// the test registry, the registry key and the anchor. And that registry's key.
fn certified() -> (SoftCard, [u8; 65]) {
    let reg = v3::test_registry_key();
    let mut c = SoftCard::v4();
    let k0 = c.gen_key(0).unwrap();
    let w = c.attest_init().unwrap();
    c.set_cert(&Certificate::sign(&reg, &k0.serialize(), &w).unwrap().to_bytes()).unwrap();
    c.set_registry_key(&v3::registry_point(&reg)).unwrap();
    c.set_header(&anchor()).unwrap();
    (c, v3::registry_point(&reg))
}

#[test]
fn a_host_sets_and_verifies_a_sealed_pin_and_the_clear_form_is_refused() {
    let (mut c, key) = certified();
    let t = trust_card(&mut c, &key).unwrap();
    assert!(verify_pin_held_clear(&mut c, PIN, &mut |_| {}).unwrap_err().downcast_ref::<ClearPinRefused>().is_some());
    assert!(open_pin_session_with(&mut c, PIN).unwrap_err().downcast_ref::<ClearPinRefused>().is_some());
    open_pin_session_held(&mut c, &t, PIN, &mut |_| {}).unwrap();
    assert!(c.get_state().unwrap().pin_set);
    for ins in [0x40, 0x41, 0x42] {
        assert_eq!(sw(c.apdu(&[0xB0, ins, 0x00, 0x00, 0x04, b'1', b'2', b'3', b'4', 0x00])), 0x6B00, "INS {ins:02x} with P1 00");
    }
    c.apdu(&SELECT_APDU).unwrap();
    assert_eq!(sw(verify_pin_held(&mut c, &t, b"9999", &mut |_| {})), 0x63C1);
    verify_pin_held(&mut c, &t, PIN, &mut |_| {}).unwrap();
    change_pin_sealed(&mut c, &t, PIN, b"567890").unwrap();
    assert_eq!(sw(change_pin_sealed(&mut c, &t, b"567890", b"1111")), 0x6A94, "a change ends the session");
    verify_pin_held(&mut c, &t, b"567890", &mut |_| {}).unwrap();
}

#[test]
fn one_nonce_opens_one_envelope_and_select_ends_it() {
    let (mut c, key) = certified();
    let t = trust_card(&mut c, &key).unwrap();
    open_pin_session_held(&mut c, &t, PIN, &mut |_| {}).unwrap();
    let block = sealed::pin_block(PIN).unwrap();
    let cmd = SealedCommand::new(&mut c, &t, 0x40, &block).unwrap();
    cmd.send(&mut c).unwrap();
    assert_eq!(sw(cmd.send(&mut c)), 0x6985, "sent twice");
    let cmd = SealedCommand::new(&mut c, &t, 0x40, &block).unwrap();
    c.apdu(&SELECT_APDU).unwrap();
    assert_eq!(sw(cmd.send(&mut c)), 0x6985, "a SELECT ends the nonce");
    // No GET_NONCE in this selection at all.
    assert_eq!(sw(cmd.send(&mut c)), 0x6985);
    assert_eq!(c.get_state().unwrap().wrong_tries, 0, "none of these was a try");
}

#[test]
fn an_envelope_that_does_not_open_is_a_wrong_try_and_ends_the_session() {
    let (mut c, key) = certified();
    let t = trust_card(&mut c, &key).unwrap();
    set_pin_sealed(&mut c, &t, PIN).unwrap();
    // The first SET_PIN gave this selection the PIN; a tag changed takes it away.
    let mut a = SealedCommand::new(&mut c, &t, 0x40, &sealed::pin_block(PIN).unwrap()).unwrap().bytes().to_vec();
    let at = a.len() - 2;
    a[at] ^= 1;
    assert_eq!(sw(c.apdu(&a)), 0x63C1);
    assert_eq!(sw(change_pin_sealed(&mut c, &t, PIN, b"5678")), 0x6A94, "the session ended");
    // E off the curve: a try too, after its wait (one asking).
    let mut a = SealedCommand::new(&mut c, &t, 0x40, &sealed::pin_block(PIN).unwrap()).unwrap().bytes().to_vec();
    a[5 + 1..5 + 65].fill(0);
    assert_eq!(sw(c.apdu(&a)), v3::SW_KEEP_HOLDING);
    assert_eq!(sw(c.apdu(&a)), 0x63C2);
    // Sealed for another instruction: a try.
    c.set_pin_marks(0, 0);
    let a = SealedCommand::new(&mut c, &t, 0x41, &sealed::pin_block(PIN).unwrap()).unwrap().bytes().to_vec();
    assert_eq!(sw(c.apdu(&[&[0xB0, 0x40], &a[2..]].concat())), 0x63C1);
    // A block of length 3: 6700, no try.
    c.set_pin_marks(0, 0);
    let mut bad = sealed::pin_block(PIN).unwrap();
    bad[0] = 3;
    let a = SealedCommand::new(&mut c, &t, 0x40, &bad).unwrap();
    assert_eq!(sw(a.send(&mut c)), 0x6700);
    assert_eq!(c.get_state().unwrap().wrong_tries, 0);
}

#[test]
fn a_set_pin_that_does_not_open_writes_nothing_and_spends_the_nonce() {
    let (mut c, key) = certified();
    let t = trust_card(&mut c, &key).unwrap();
    let mut a = SealedCommand::new(&mut c, &t, 0x41, &sealed::pin_block(PIN).unwrap()).unwrap().bytes().to_vec();
    a[5 + 65] ^= 1;
    assert_eq!(sw(c.apdu(&a)), 0x6700);
    assert!(!c.nonce_live());
    assert!(!c.get_state().unwrap().pin_set);
    set_pin_sealed(&mut c, &t, PIN).unwrap();
}

#[test]
fn a_nonce_asked_in_the_wait_begins_the_stretch_again_and_the_old_envelope_is_a_wrong_try() {
    let (mut c, key) = certified();
    let t = trust_card(&mut c, &key).unwrap();
    set_pin_sealed(&mut c, &t, PIN).unwrap();
    c.set_pin_marks(1, 1);
    let a = SealedCommand::new(&mut c, &t, 0x40, &sealed::pin_block(PIN).unwrap()).unwrap();
    assert_eq!(sw(a.send(&mut c)), v3::SW_KEEP_HOLDING);
    c.get_nonce().unwrap();
    assert_eq!(sw(a.send(&mut c)), v3::SW_KEEP_HOLDING, "served the wait again");
    assert_eq!(sw(a.send(&mut c)), 0x63C2, "then a wrong try under the new nonce");
    let st = c.get_state().unwrap();
    assert_eq!((st.wrong_tries, st.owed), (2, 2), "and the next wait doubled");
    // verify_pin_held seals once and sends the same bytes through a whole wait.
    let mut seen = vec![];
    verify_pin_held(&mut c, &t, PIN, &mut |left| seen.push(left)).unwrap();
    assert_eq!(seen, vec![2, 1, 0]);
}

#[test]
fn no_pin_is_sealed_to_a_key_the_host_has_not_checked() {
    // GET_NONCE answering a key its binding does not name.
    let (mut c, key) = certified();
    let t = trust_card(&mut c, &key).unwrap();
    c.spoof_pin_key();
    let e = SealedCommand::new(&mut c, &t, 0x41, &sealed::pin_block(PIN).unwrap()).err().unwrap();
    assert!(e.downcast_ref::<UntrustedCard>().is_some(), "{e:#}");
    assert!(!format!("{e:#}").contains("1234"));
    assert!(!c.get_state().unwrap().pin_set);
    // A certificate under another registry key, or none.
    let (mut c, _) = certified();
    let other = v3::registry_point(&p256::ecdsa::SigningKey::random(&mut rand::thread_rng()));
    assert!(trust_card(&mut c, &other).unwrap_err().downcast_ref::<UntrustedCard>().is_some());
    let mut bare = SoftCard::v4();
    bare.gen_key(0).unwrap();
    assert!(trust_card(&mut bare, &other).unwrap_err().downcast_ref::<UntrustedCard>().is_some());
    // GET_NONCE before SET_CERT, and with another P1.
    bare.attest_init().unwrap();
    assert_eq!(sw(bare.get_nonce()), 0x6985);
    let (mut c, _) = certified();
    assert_eq!(sw(c.apdu(&[0xB0, 0x44, 0x00, 0x00, 0x00])), 0x6B00);
}

#[test]
fn the_binding_is_made_once_and_only_the_nonce_changes() {
    let (mut c, key) = certified();
    let cert = trust_card(&mut c, &key).unwrap().certificate().clone();
    let a = PinKey::parse(&c.get_nonce().unwrap()).unwrap();
    let b = PinKey::parse(&c.get_nonce().unwrap()).unwrap();
    assert_eq!((a.key, &a.binding), (b.key, &b.binding));
    assert_ne!(a.nonce, b.nonce);
    a.check(&cert.attest_key).unwrap();
}
