//! Applet 3.0 on the `SoftCard` (docs/OFFLINE-PROTOCOL.md § 1), mirroring the applet's
//! `OfflineTest.java` case by case, with the same real mainnet headers; and the 3.0 commands
//! through `CardClient`'s APDU framing.

use fedimint_card_host_card::v3::{self, Certificate};
use fedimint_card_host_card::*;
use secp256k1::{Message, SECP256K1};

// Real mainnet headers: 900000, then 900200 (121 243 s later) and 900201 (553 s after that).
const H900000: &str = "00a0ab20247d4d9f582f9750344cdf62c46d81d046be960340960100000000000000000070f96945530651135839d8adc3f40e595118ec74c7ad81a3d17bb022e554fb0c937f4268743702177ad05f92";
const H900200: &str = "0060002058d02283d148685d2413fdf9a66317225589b86a56e2000000000000000000002c29575cba6c0fc3b8e366bf820d4d526ba300cd8915ee4ac075a32aafa6e9302e59446874370217546eb104";
const H900201: &str = "0000c020561cc99a795c39bfe4de51c4e1621a0643bef06c858900000000000000000000a60ddeb0320b09b52ddaf21ad7c492da26a49212e6e1848f22432b27ac2f8711575b4468743702177836e139";
/// The genesis block: real, but with far less work than the floor.
const GENESIS: &str = "0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4a29ab5f49ffff001d1dac2b7c";

fn header(h: &str) -> [u8; 80] {
    hex::decode(h).unwrap().try_into().unwrap()
}

fn h(v: u8) -> [u8; 32] {
    [v; 32]
}

fn sig48() -> [u8; 48] {
    std::array::from_fn(|i| 0xA0 + 8 + i as u8)
}

fn sw<T: std::fmt::Debug>(r: anyhow::Result<T>) -> u16 {
    match r {
        Ok(_) => 0x9000,
        Err(e) => status_word(&e).unwrap_or_else(|| panic!("not a status word: {e:#}")),
    }
}

const PIN: &[u8] = b"1234";

/// A card with slots 0..n-1 keyed and holding notes of `msat`, a PIN set and verified.
fn card(n: u8, msat: u64, limit: u64) -> SoftCard {
    let mut c = SoftCard::v3();
    if limit != 0 {
        c.set_limit(limit).unwrap();
    }
    for s in 0..n {
        c.gen_key(s).unwrap();
        c.set_note(s, msat, &sig48()).unwrap();
    }
    c.set_pin(PIN).unwrap();
    c.verify_pin(PIN).unwrap();
    c
}

fn verifies(c: &mut SoftCard, slot: u8, msg: &[u8; 32], sig: &secp256k1::schnorr::Signature) -> bool {
    SECP256K1.verify_schnorr(sig, &Message::from_digest(*msg), &c.get_pub(slot).unwrap()).is_ok()
}

fn change(msat: u64) -> Opening {
    let b: Vec<u8> = (8u8..74).collect();
    let _ = msat;
    Opening::from_bytes(&b)
}

#[test]
fn a_slot_signs_one_transaction_and_only_that_one_again() {
    let mut c = card(1, 1000, 0);
    let first = c.sign(0, &h(1)).unwrap();
    assert!(verifies(&mut c, 0, &h(1), &first));
    assert_eq!(c.get_slot(0).unwrap().state, SlotState::Spent);
    // the same transaction again: a lost answer, the stored signature byte for byte
    let again = c.sign(0, &h(1)).unwrap();
    assert_eq!(first, again);
    // any other transaction is the same note spent twice
    assert_eq!(sw(c.sign(0, &h(2))), 0x6985);
    let slot = c.get_slot(0).unwrap();
    assert_eq!(slot.state, SlotState::Spent);
    assert_eq!(slot.spent_hash, h(1));
}

#[test]
fn only_a_note_is_signed() {
    let mut c = SoftCard::v3();
    c.gen_key(0).unwrap();
    c.gen_key(1).unwrap();
    c.set_change(1, 4096, &change(4096)).unwrap();
    c.set_pin(PIN).unwrap();
    c.verify_pin(PIN).unwrap();
    assert_eq!(sw(c.sign(0, &h(1))), 0x6985, "a key with no note");
    assert_eq!(sw(c.sign(1, &h(1))), 0x6985, "a key owed change");
    assert_eq!(sw(c.sign(2, &h(1))), 0x6985, "an empty slot");
}

#[test]
fn a_key_that_holds_value_is_never_replaced() {
    let mut c = SoftCard::v3();
    for s in 0..3 {
        c.gen_key(s).unwrap();
    }
    c.set_note(0, 1000, &sig48()).unwrap();
    c.set_change(1, 2048, &change(2048)).unwrap();
    assert_eq!(sw(c.gen_key_overwrite(0)), 0x6985, "a note");
    assert_eq!(sw(c.gen_key_overwrite(1)), 0x6985, "change owed");
    // a KEYED slot (no value yet) may be re-keyed
    let before = c.get_pub(2).unwrap();
    assert_ne!(before, c.gen_key_overwrite(2).unwrap());
}

#[test]
fn a_spent_slot_is_re_keyed_and_forgets_its_transaction() {
    let mut c = card(1, 1000, 0);
    c.sign(0, &h(1)).unwrap();
    let before = c.get_pub(0).unwrap();
    c.gen_key_overwrite(0).unwrap();
    let slot = c.get_slot(0).unwrap();
    assert_eq!(slot.state, SlotState::Keyed);
    assert_ne!(before, c.get_pub(0).unwrap());
    assert_eq!(slot.spent_hash, [0; 32], "the spent hash is gone");
    c.set_note(0, 1000, &sig48()).unwrap();
    assert_eq!(sw(c.sign(0, &h(2))), 0x9000, "the new note signs its own transaction");
}

#[test]
fn set_note_takes_only_a_keyed_or_change_slot() {
    let mut c = SoftCard::v3();
    assert_eq!(sw(c.set_note(0, 1000, &sig48())), 0x6985, "empty");
    c.gen_key(0).unwrap();
    assert_eq!(sw(c.set_note(0, 0, &sig48())), 0x6A80, "no amount");
    assert_eq!(sw(c.apdu(&v3_apdu(v3::INS_SET_NOTE, 0, &[0; 55]))), 0x6700);
    c.set_note(0, 1000, &sig48()).unwrap();
    assert_eq!(sw(c.set_note(0, 2000, &sig48())), 0x6985, "a note is not overwritten");
    let slot = c.get_slot(0).unwrap();
    assert_eq!(slot.state, SlotState::Note);
    assert_eq!(slot.xonly, Some(c.get_pub(0).unwrap()));
    assert_eq!((slot.amount_msat, slot.sig48), (1000, sig48()));
}

#[test]
fn change_is_kept_then_finished_with_its_own_amount() {
    let mut c = SoftCard::v3();
    c.gen_key(3).unwrap();
    let open = change(4096);
    c.set_change(3, 4096, &open).unwrap();
    let slot = c.get_slot(3).unwrap();
    assert_eq!((slot.state, slot.amount_msat, slot.opening), (SlotState::Change, 4096, open));
    assert_eq!(sw(c.set_change(3, 4096, &open)), 0x6985, "only a keyed slot");
    assert_eq!(sw(c.set_note(3, 2048, &sig48())), 0x6A80, "not the amount it is owed");
    c.set_note(3, 4096, &sig48()).unwrap();
    let slot = c.get_slot(3).unwrap();
    assert_eq!(slot.state, SlotState::Note);
    assert_eq!(slot.opening.to_bytes(), [0; 66], "the opening is let go");
}

#[test]
fn the_limit_is_set_only_before_the_pin() {
    let mut c = SoftCard::v3();
    c.set_limit(5000).unwrap();
    c.set_pin(PIN).unwrap();
    c.verify_pin(PIN).unwrap();
    assert_eq!(sw(c.set_limit(1 << 40)), 0x6985);
    assert_eq!(c.get_state().unwrap().limit_msat, 5000);
}

#[test]
fn with_no_header_the_limit_holds_for_good_and_an_over_spend_burns_nothing() {
    let mut c = card(3, 1000, 2000);
    c.sign(0, &h(1)).unwrap();
    c.sign(1, &h(1)).unwrap();
    assert_eq!(sw(c.sign(2, &h(1))), 0x6A95);
    assert_eq!(c.get_slot(2).unwrap().state, SlotState::Note, "refused: the note is still there");
    assert_eq!(c.get_state().unwrap().spent_msat, 2000);
    // a spent slot's own transaction again costs nothing
    c.sign(0, &h(1)).unwrap();
}

#[test]
fn a_real_header_a_day_later_starts_a_new_day() {
    let mut c = card(4, 1000, 2000);
    c.set_header(&header(H900000)).unwrap();
    c.sign(0, &h(1)).unwrap();
    c.sign(1, &h(1)).unwrap();
    assert_eq!(sw(c.sign(2, &h(1))), 0x6A95);
    assert!(!c.get_state().unwrap().limit_allows(&[1000]), "the till's pre-check agrees");
    // 900200 is 121 243 s later: a new day
    assert_eq!(c.set_header(&header(H900200)).unwrap(), 1_749_309_742);
    assert!(c.get_state().unwrap().limit_allows(&[1000]));
    c.sign(2, &h(2)).unwrap();
    // 900201 is the same day: 3000 would pass 2000
    c.set_header(&header(H900201)).unwrap();
    c.sign(3, &h(3)).unwrap();
    let st = c.get_state().unwrap();
    assert_eq!(st.spent_msat, 2000);
    assert_eq!(st.window, 1_749_309_742, "the window began at 900200");
}

#[test]
fn headers_without_the_work_are_refused_and_an_older_one_moves_nothing() {
    let mut c = SoftCard::v3();
    assert_eq!(sw(c.set_header(&header(GENESIS))), 0x6A93, "under the floor");
    let mut forged = header(H900200);
    forged[79] ^= 1; // another nonce: the hash no longer meets the target
    assert_eq!(sw(c.set_header(&forged)), 0x6A93);
    assert!(v3::header_has_work(&header(H900200)) && !v3::header_has_work(&forged) && !v3::header_has_work(&header(GENESIS)));
    assert_eq!(c.set_header(&header(H900200)).unwrap(), 1_749_309_742);
    assert_eq!(c.set_header(&header(H900000)).unwrap(), 1_749_309_742, "the clock never goes back");
    assert_eq!(sw(c.apdu(&v3_apdu(v3::INS_SET_HEADER, 0, &[0; 79]))), 0x6700);
    assert_eq!(v3::header_time(&header(H900200)), 1_749_309_742);
}

#[test]
fn the_card_attests_over_the_challenge_and_its_own_key() {
    let mut c = SoftCard::v3();
    let slot0 = c.gen_key(0).unwrap().serialize();
    assert_eq!(sw(c.attest(&h(7))), 0x6985, "no key, no certificate");
    let w = c.attest_init().unwrap();
    assert_eq!(w.len(), 65);
    assert_eq!(sw(c.attest(&h(7))), 0x6985, "no certificate yet");
    // The registry's certificate, as § 2 makes it.
    let registry = p256::ecdsa::SigningKey::random(&mut rand::thread_rng());
    let cert = Certificate::sign(&registry, &slot0, &w).unwrap();
    c.set_cert(&cert.to_bytes()).unwrap();
    assert_eq!(c.get_cert().unwrap(), cert.to_bytes());
    let challenge: [u8; 32] = rand::random();
    let der = c.attest(&challenge).unwrap();
    let got = Certificate::parse(&c.get_cert().unwrap()).unwrap();
    let reg_pub = registry.verifying_key().to_encoded_point(false);
    got.verify(reg_pub.as_bytes()).unwrap();
    got.verify_attest(&challenge, &der).unwrap();
    assert!(got.verify_attest(&h(8), &der).is_err(), "another challenge");
    let other = p256::ecdsa::SigningKey::random(&mut rand::thread_rng());
    assert!(got.verify(other.verifying_key().to_encoded_point(false).as_bytes()).is_err(), "another registry");
    // provisioning only: no second certificate, no new key once a PIN is set
    assert_eq!(sw(c.set_cert(&cert.to_bytes())), 0x6985);
    assert_eq!(sw(c.attest_init()), 0x6985);
    let st = c.get_state().unwrap();
    assert_eq!((st.cert_len, st.attest_key), (130, true));
    // a clone of the keys has no certified attestation key of its own
    let mut clone = c.extract_clone();
    let der = clone.attest(&challenge).unwrap();
    assert!(got.verify_attest(&challenge, &der).is_err());
}

#[test]
fn attestation_key_is_not_made_after_the_pin() {
    let mut c = SoftCard::v3();
    c.set_pin(PIN).unwrap();
    assert_eq!(sw(c.attest_init()), 0x6985);
}

#[test]
fn versions() {
    assert_eq!(SoftCard::new().applet_version().unwrap(), AppletVersion::V2_1);
    assert!(!SoftCard::new().applet_version().unwrap().sign_once());
    assert!(SoftCard::v3().applet_version().unwrap().sign_once());
    // A 2.1 card refuses the 3.0 commands as unknown instructions.
    assert_eq!(sw(SoftCard::new().get_state()), 0x6D00);
    assert_eq!(SoftCard::v3().apdu(&SELECT_APDU).unwrap(), vec![3, 0]);
    assert!(SoftCard::new().apdu(&SELECT_APDU).unwrap().is_empty());
    // 2.1: any keyed slot signs, any number of times.
    let mut c = SoftCard::new();
    c.gen_key(0).unwrap();
    c.set_pin(PIN).unwrap();
    c.verify_pin(PIN).unwrap();
    c.sign(0, &h(1)).unwrap();
    c.sign(0, &h(2)).unwrap();
}

/// The 3.0 commands as APDUs, through `CardClient`: a transport that hands each APDU to a
/// `SoftCard` the way the applet would take it.
struct Direct(SoftCard);

impl Transport for Direct {
    fn transmit(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        let slot = apdu[2];
        let r = match (apdu[0], apdu[1]) {
            (0xB0, 0x10) if apdu[3] == 0 => self.0.gen_key(slot).map(|k| k.serialize().to_vec()),
            (0xB0, 0x10) => self.0.gen_key_overwrite(slot).map(|k| k.serialize().to_vec()),
            (0xB0, 0x11) => self.0.get_pub(slot).map(|k| k.serialize().to_vec()),
            (0xB0, 0x12) => self.0.sign(slot, apdu[5..37].try_into().unwrap()).map(|s| s.serialize().to_vec()),
            _ => self.0.apdu(apdu),
        };
        Ok(match r {
            Ok(mut d) => {
                d.extend_from_slice(&[0x90, 0x00]);
                d
            }
            Err(e) => status_word(&e).unwrap().to_be_bytes().to_vec(),
        })
    }
}

#[test]
fn client_speaks_3_0() {
    let mut c = CardClient::new(Direct(SoftCard::v3()));
    // Before SELECT the version comes from GET_STATE; after it, from SELECT's answer.
    assert_eq!(c.applet_version().unwrap(), AppletVersion::V3_0);
    c.select().unwrap();
    assert_eq!(c.applet_version().unwrap(), AppletVersion::V3_0);
    c.gen_key(0).unwrap();
    c.gen_key(1).unwrap();
    c.set_note(0, 4096, &sig48()).unwrap();
    let open = change(1);
    c.set_change(1, 1024, &open).unwrap();
    let s0 = c.get_slot(0).unwrap();
    assert_eq!((s0.state, s0.amount_msat, s0.xonly), (SlotState::Note, 4096, Some(c.get_pub(0).unwrap())));
    let s1 = c.get_slot(1).unwrap();
    assert_eq!((s1.state, s1.amount_msat, s1.opening), (SlotState::Change, 1024, open));
    let st = c.get_state().unwrap();
    assert_eq!(st.states[..3], [SlotState::Note, SlotState::Change, SlotState::Empty]);
    assert_eq!(c.set_header(&header(H900000)).unwrap(), v3::header_time(&header(H900000)));
    c.set_pin(PIN).unwrap();
    c.verify_pin(PIN).unwrap();
    c.sign(0, &h(9)).unwrap();
    assert_eq!(c.get_slot(0).unwrap().state, SlotState::Spent);
    assert_eq!(sw(c.get_cert()), 0x6A83);

    let mut old = CardClient::new(Direct(SoftCard::new()));
    assert_eq!(old.applet_version().unwrap(), AppletVersion::V2_1, "a 2.1 card refuses GET_STATE");
    old.select().unwrap();
    assert_eq!(old.applet_version().unwrap(), AppletVersion::V2_1);
}

// ------------------------------------------------------------------ releases (§ 6), as ReleaseTest.java

fn registry() -> p256::ecdsa::SigningKey {
    p256::ecdsa::SigningKey::random(&mut rand::thread_rng())
}

fn point(k: &p256::ecdsa::SigningKey) -> [u8; 65] {
    k.verifying_key().to_encoded_point(false).as_bytes().try_into().unwrap()
}

fn release(k: &p256::ecdsa::SigningKey, slot0: &[u8; 32], slot: u8, txid: &[u8; 32]) -> Vec<u8> {
    v3::sign_release(k, slot0, slot, txid)
}

/// A card with the registry key, slot 0 its identity, slot 1 a note, slot 2 keyed; PIN verified.
fn released_card(k: &p256::ecdsa::SigningKey) -> (SoftCard, [u8; 32]) {
    let mut c = SoftCard::v3();
    let slot0 = c.gen_key(0).unwrap().serialize();
    c.gen_key(1).unwrap();
    c.gen_key(2).unwrap();
    c.set_note(1, 2048, &sig48()).unwrap();
    c.set_registry_key(&point(k)).unwrap();
    c.set_pin(PIN).unwrap();
    c.verify_pin(PIN).unwrap();
    (c, slot0)
}

fn opening(txid: [u8; 32]) -> Opening {
    Opening { txid, out_idx: 0, blinding_key: [0; 32] }
}

#[test]
fn a_released_input_is_a_note_again_and_signs_another_transaction() {
    let k = registry();
    let (mut c, slot0) = released_card(&k);
    let (t1, t2) = (h(0x31), h(0x32));
    c.sign(1, &t1).unwrap();
    assert_eq!(sw(c.sign(1, &t2)), 0x6985, "spent on t1");
    assert_eq!(c.release(1, &t1, &release(&k, &slot0, 1, &t1)).unwrap(), SlotState::Note);
    let slot = c.get_slot(1).unwrap();
    assert_eq!((slot.state, slot.amount_msat, slot.spent_hash), (SlotState::Note, 2048, [0; 32]));
    let sig = c.sign(1, &t2).unwrap();
    assert!(verifies(&mut c, 1, &t2, &sig), "the note signs its new transaction");
    // the old release cannot undo the new spend
    assert_eq!(sw(c.release(1, &t1, &release(&k, &slot0, 1, &t1))), 0x6985);
}

#[test]
fn a_released_change_slot_is_keyed_again() {
    let k = registry();
    let (mut c, slot0) = released_card(&k);
    let t = h(0x44);
    c.set_change(2, 1024, &opening(t)).unwrap();
    assert_eq!(sw(c.gen_key_overwrite(2)), 0x6985, "change owed: never re-keyed");
    assert_eq!(c.release(2, &t, &release(&k, &slot0, 2, &t)).unwrap(), SlotState::Keyed);
    let slot = c.get_slot(2).unwrap();
    assert_eq!(slot.opening, Opening::from_bytes(&[0; 66]), "the opening is gone");
    assert_eq!(slot.amount_msat, 0);
    c.set_note(2, 4096, &sig48()).unwrap();
    assert_eq!(c.get_slot(2).unwrap().state, SlotState::Note, "the slot takes a new note");
}

#[test]
fn only_the_registrys_signature_over_this_card_slot_and_transaction() {
    let (k, other) = (registry(), registry());
    let (mut c, slot0) = released_card(&k);
    let t = h(0x55);
    c.sign(1, &t).unwrap();
    assert_eq!(sw(c.release(1, &t, &release(&other, &slot0, 1, &t))), 0x6982, "another key");
    assert_eq!(sw(c.release(1, &t, &release(&k, &h(7), 1, &t))), 0x6982, "another card");
    assert_eq!(sw(c.release(1, &t, &release(&k, &slot0, 2, &t))), 0x6982, "signed for another slot");
    assert_eq!(sw(c.release(2, &t, &release(&k, &slot0, 2, &t))), 0x6985, "slot 2 never spent on t");
    assert_eq!(sw(c.release(1, &h(0x56), &release(&k, &slot0, 1, &h(0x56)))), 0x6985, "another transaction");
    let mut bad = release(&k, &slot0, 1, &t);
    let last = bad.len() - 1;
    bad[last] ^= 1;
    assert_eq!(sw(c.release(1, &t, &bad)), 0x6982);
    assert_eq!(c.slot_state(1), Some(SlotState::Spent), "nothing was released");
}

#[test]
fn the_registry_key_is_set_before_the_pin_and_without_it_nothing_is_released() {
    let k = registry();
    let mut c = SoftCard::v3();
    let slot0 = c.gen_key(0).unwrap().serialize();
    c.gen_key(1).unwrap();
    c.set_note(1, 2048, &sig48()).unwrap();
    assert!(!c.get_state().unwrap().registry_key);
    c.set_pin(PIN).unwrap();
    c.verify_pin(PIN).unwrap();
    assert_eq!(sw(c.set_registry_key(&point(&k))), 0x6985, "only before the PIN");
    let t = h(0x66);
    c.sign(1, &t).unwrap();
    assert_eq!(sw(c.release(1, &t, &release(&k, &slot0, 1, &t))), 0x6985, "no registry key");
    assert_eq!(c.apdu(&v3_apdu(v3::INS_GET_STATE, 0, &[])).unwrap().len(), 59);
}

#[test]
fn a_malformed_key_or_release_is_refused() {
    let mut c = SoftCard::v3();
    assert_eq!(sw(c.apdu(&v3_apdu(v3::INS_SET_REGISTRY_KEY, 0, &[0; 64]))), 0x6700);
    let mut compressed = [0u8; 65];
    compressed[0] = 2;
    assert_eq!(sw(c.set_registry_key(&compressed)), 0x6A80);
    let mut off_curve = [0u8; 65];
    off_curve[0] = 4;
    assert_eq!(sw(c.set_registry_key(&off_curve)), 0x6A80);
    let k = registry();
    c.set_registry_key(&point(&k)).unwrap();
    assert!(c.get_state().unwrap().registry_key);
    assert_eq!(sw(c.apdu(&v3_apdu(v3::INS_RELEASE, 0, &[0; 33]))), 0x6700);
    assert!(c.release(0, &h(1), &[0; 7]).is_err(), "refused before the card sees it");
}

#[test]
fn client_speaks_release() {
    let k = registry();
    let mut c = CardClient::new(Direct(SoftCard::v3()));
    c.select().unwrap();
    let slot0 = c.gen_key(0).unwrap().serialize();
    c.gen_key(1).unwrap();
    c.set_note(1, 2048, &sig48()).unwrap();
    c.set_registry_key(&point(&k)).unwrap();
    assert!(c.get_state().unwrap().registry_key);
    c.set_pin(PIN).unwrap();
    c.verify_pin(PIN).unwrap();
    c.sign(1, &h(3)).unwrap();
    assert_eq!(c.release(1, &h(3), &release(&k, &slot0, 1, &h(3))).unwrap(), SlotState::Note);
}
