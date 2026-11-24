//! What applet 4.0 changed, on the `SoftCard` and through `CardClient`: the rule for every reader
//! (SELECT), GET_STATE's 65 bytes, the clock's weak rule and anchor with their host mirrors
//! (`v3::check_header`, `CardStatus::anchors`), notes signed by the registry (and the registry's
//! vectors, `registry/test/note-sig-vectors.json`), and SET_CHANGE behind the PIN.
//!
//! Headers for the clock are mined here against an easy floor the card is given
//! (`SoftCard::use_floor`), as the applet's tests do (`Headers.useFloor`).

use fedimint_card_host_card::v3::{self, Opening, SlotState};
use fedimint_card_host_card::*;
use secp256k1::{Keypair, SECP256K1};

const PIN: &[u8] = b"1234";
/// The tests' floor: a target of about 2^255.
const TEST_FLOOR: u32 = 0x207F_FFFF;
/// Just over twice the tests' floor's work: the least a card with no hardest takes.
const CLOCK: u32 = 0x203F_FFFF;
const T0: u32 = 1_700_000_000;

fn sw<T: std::fmt::Debug>(r: anyhow::Result<T>) -> u16 {
    match r {
        Ok(_) => 0x9000,
        Err(e) => status_word(&e).unwrap_or_else(|| panic!("not a status word: {e:#}")),
    }
}

/// A header with `time` and `bits` whose hash meets its own bits.
fn mine(time: u32, bits: u32, salt: u8) -> [u8; 80] {
    let mut h = [0u8; 80];
    h[0] = 4;
    h[4] = salt;
    h[68..72].copy_from_slice(&time.to_le_bytes());
    h[72..76].copy_from_slice(&bits.to_le_bytes());
    let target = v3::target_of(&bits.to_le_bytes()).unwrap();
    for nonce in 0u32.. {
        h[76..80].copy_from_slice(&nonce.to_le_bytes());
        if v3::header_work_hash(&h) <= target {
            return h;
        }
    }
    unreachable!()
}

/// A header carrying only `bits` (the weak rule reads nothing else).
fn with_bits(bits: u32) -> [u8; 80] {
    let mut h = [0u8; 80];
    h[72..76].copy_from_slice(&bits.to_le_bytes());
    h
}

/// A 4.0 card at the tests' floor.
fn card() -> SoftCard {
    let mut c = SoftCard::v4();
    c.use_floor(TEST_FLOOR.to_le_bytes());
    c
}

fn sig48() -> [u8; 48] {
    std::array::from_fn(|i| 0xA8 + i as u8)
}

fn reg() -> p256::ecdsa::SigningKey {
    v3::test_registry_key()
}

/// A 4.0 card with the test registry's key and slots 0..n keyed.
fn noted_card(n: u8) -> (SoftCard, Vec<[u8; 32]>) {
    let mut c = card();
    c.set_registry_key(&v3::registry_point(&reg())).unwrap();
    let xs = (0..n).map(|s| c.gen_key(s).unwrap().serialize()).collect();
    (c, xs)
}

fn set_note_raw(c: &mut SoftCard, slot: u8, amount: u64, sig48: &[u8; 48], der: &[u8]) -> u16 {
    let mut d = v3::note_bytes(amount, sig48).to_vec();
    d.extend_from_slice(der);
    sw(c.apdu(&v3_apdu(v3::INS_SET_NOTE, slot, &d)))
}

/// Answers scripted in turn; every APDU sent is kept.
struct Script {
    answers: Vec<Vec<u8>>,
    sent: Vec<Vec<u8>>,
}

impl Transport for Script {
    fn transmit(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.sent.push(apdu.to_vec());
        anyhow::ensure!(!self.answers.is_empty(), "nothing more scripted");
        Ok(self.answers.remove(0))
    }
}

fn scripted(answers: &[&[u8]]) -> CardClient<Script> {
    CardClient::new(Script { answers: answers.iter().map(|a| a.to_vec()).collect(), sent: vec![] })
}

/// Each APDU handed to a `SoftCard` as the applet would take it.
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

// ------------------------------------------------------------------ SELECT: the rule for every reader

#[test]
fn select_passes_only_9000_with_two_bytes_and_major_4() {
    for ok in [&[0x04, 0x00, 0x90, 0x00][..], &[0x04, 0x07, 0x90, 0x00]] {
        let mut c = scripted(&[ok]);
        c.select().unwrap();
        assert_eq!(c.selected(), Some(AppletVersion { major: 4, minor: ok[1] }));
        assert_eq!(c.applet_version().unwrap().major, 4);
    }
    // 2.x answers no data; 3.0 answers 03 00; anything but two bytes is no version.
    for old in
        [&[0x90, 0x00][..], &[0x03, 0x00, 0x90, 0x00], &[0x02, 0x01, 0x90, 0x00], &[0x04, 0x00, 0x00, 0x90, 0x00], &[0x04, 0x90, 0x00]]
    {
        let mut c = scripted(&[old]);
        let e = c.select().unwrap_err();
        assert!(is_old_card(&e), "{old:02x?}: {e:#}");
        assert!(e.to_string().starts_with(OLD_CARD_MESSAGE), "{e}");
        assert_eq!(c.selected(), None);
    }
    let mut newer = scripted(&[&[0x05, 0x00, 0x90, 0x00][..]]);
    let e = newer.select().unwrap_err();
    assert_eq!(e.downcast_ref::<UnsupportedApplet>(), Some(&UnsupportedApplet::Newer(AppletVersion { major: 5, minor: 0 })));
    assert!(!is_old_card(&e));
    // No applet: the card's own status word, not the old-card error.
    let mut none = scripted(&[&[0x6A, 0x82][..]]);
    let e = none.select().unwrap_err();
    assert_eq!((status_word(&e), is_old_card(&e)), (Some(0x6A82), false));
}

#[test]
fn a_select_that_fails_ends_what_the_last_one_passed() {
    let mut c = scripted(&[&[0x04, 0x00, 0x90, 0x00][..], &[0x03, 0x00, 0x90, 0x00], &[0x04, 0x00, 0x90, 0x00], &[0x6A, 0x82]]);
    c.select().unwrap();
    assert!(c.select().is_err());
    assert_eq!(c.selected(), None, "an old card's answer");
    c.select().unwrap();
    assert!(c.select().is_err());
    assert_eq!(c.selected(), None, "a refused SELECT");
    // A raw SELECT through `apdu` answers what the card said, and keeps the same rule.
    let mut raw = scripted(&[&[0x03, 0x00, 0x90, 0x00][..], &[0x04, 0x00, 0x90, 0x00]]);
    assert_eq!(raw.apdu(&SELECT_APDU).unwrap(), vec![3, 0]);
    assert_eq!(raw.selected(), None);
    raw.apdu(&SELECT_APDU).unwrap();
    assert_eq!(raw.selected(), Some(AppletVersion::V4_0));
}

#[test]
fn the_soft_cards_answer_select_by_their_version() {
    let mut c = CardClient::new(Direct(SoftCard::v4()));
    c.select().unwrap();
    assert_eq!(c.selected(), Some(AppletVersion::V4_0));
    assert!(is_old_card(&CardClient::new(Direct(SoftCard::v3())).select().unwrap_err()));
    assert!(is_old_card(&CardClient::new(Direct(SoftCard::new())).select().unwrap_err()));
}

// ------------------------------------------------------------------ GET_STATE

#[test]
fn get_state_is_65_bytes_and_says_the_pin_and_the_anchor() {
    let mut c = card();
    assert_eq!(c.apdu(&v3_apdu(v3::INS_GET_STATE, 0, &[])).unwrap().len(), v3::GET_STATE_LEN);
    let st = c.get_state().unwrap();
    assert!(!st.pin_set && !st.anchored && !st.registry_key && !st.chip_mul && !st.key_in_ram);
    assert_eq!((st.wrong_tries, st.owed), (0, 0));
    c.set_registry_key(&v3::registry_point(&reg())).unwrap();
    c.set_header(&mine(T0, CLOCK, 0)).unwrap();
    let st = c.get_state().unwrap();
    assert!(st.registry_key && st.anchored && st.anchors());
    c.set_pin(PIN).unwrap();
    let st = c.get_state().unwrap();
    assert!(st.pin_set && !st.anchors());
}

// ------------------------------------------------------------------ the clock: the weak rule and the anchor

#[test]
fn the_weak_rule_at_its_edges() {
    let floor = 0x207F_FFFEu32.to_le_bytes();
    let weak = |h: u32, hardest: Option<u32>| v3::header_is_weak_at(&with_bits(h), hardest.map(u32::to_le_bytes).as_ref(), &floor);
    // Before any hardest: work under twice the floor's is weak; exactly twice is not.
    assert_eq!(weak(0x207F_FFFE, None), Some(true), "the floor's own work");
    assert_eq!(weak(0x2040_0000, None), Some(true), "a hair under twice the floor's work");
    assert_eq!(weak(0x203F_FFFF, None), Some(false), "exactly twice the floor's work");
    // With a hardest: a target over twice the hardest's is weak; exactly twice is not.
    assert_eq!(weak(0x207F_FFFE, Some(0x203F_FFFF)), Some(false));
    assert_eq!(weak(0x207F_FFFF, Some(0x203F_FFFF)), Some(true));
    assert_eq!(weak(0x2000_0001, Some(0x203F_FFFF)), Some(false), "harder is never weak");
    assert_eq!(weak(0x2000_0001, Some(0x0380_0000)), None, "bits no header carries");
    // Mainnet at the built floor: block 900 000 is no weak header, alone or beside itself.
    let h = hex::decode("00a0ab20247d4d9f582f9750344cdf62c46d81d046be960340960100000000000000000070f96945530651135839d8adc3f40e595118ec74c7ad81a3d17bb022e554fb0c937f4268743702177ad05f92").unwrap();
    let h: [u8; 80] = h.try_into().unwrap();
    let bits: [u8; 4] = h[72..76].try_into().unwrap();
    assert_eq!(v3::header_is_weak(&h, None), Some(false));
    assert_eq!(v3::check_header(&h, Some(&bits)), Ok(false), "taken, and not harder than itself");
    assert_eq!(v3::check_header(&h, None), Ok(true));
}

#[test]
fn the_card_takes_exactly_what_check_header_takes() {
    let mut c = card();
    c.set_registry_key(&v3::registry_point(&reg())).unwrap();
    c.gen_key(0).unwrap();
    let floor = TEST_FLOOR.to_le_bytes();
    let mut hardest: Option<[u8; 4]> = None;
    let mut now = 0;
    let tiny = {
        // its hash cannot meet a target of 1
        let mut h = with_bits(0x0300_0001);
        h[68..72].copy_from_slice(&(T0 + 9000).to_le_bytes());
        h
    };
    let headers = [
        mine(T0, CLOCK, 0),
        mine(T0 + 600, TEST_FLOOR, 1),   // weak before and after a hardest
        mine(T0 + 1200, 0x201F_FFFF, 2), // harder
        mine(T0 + 1800, CLOCK, 3),       // weak beside the new hardest
        mine(T0 + 2400, 0x203F_FFFE, 4), // exactly twice the hardest's target
        tiny,
        with_bits(0x2180_0000),         // bits no header can carry
        mine(T0 + 100, 0x201F_FFFF, 5), // earlier: an anchor before a note, nothing after
    ];
    for round in 0..2 {
        if round == 1 {
            // A slot holding value: no anchors from here on, so the clock only moves forward.
            let x = c.get_pub(0).unwrap().serialize();
            c.set_note(0, 1000, &sig48(), &v3::sign_note(&reg(), &x, 1000, &sig48())).unwrap();
        }
        for (i, h) in headers.iter().enumerate() {
            let want = v3::check_header_at(h, hardest.as_ref(), &floor);
            let got = c.set_header(h);
            match want {
                Ok(harder) => {
                    // Round 0 every header taken is an anchor; round 1 only a later one moves the clock.
                    let t = v3::header_time(h);
                    now = if round == 0 { t } else { now.max(t) };
                    assert_eq!(got.unwrap(), now, "round {round}, header {i}");
                    if harder {
                        hardest = Some(h[72..76].try_into().unwrap());
                    }
                }
                Err(code) => assert_eq!(sw(got), code, "round {round}, header {i}"),
            }
        }
    }
}

#[test]
fn an_anchor_sets_the_clock_even_back_keeps_the_limit_and_marks_only_its_selection() {
    let mut c = card();
    c.set_limit(5000).unwrap();
    assert_eq!(c.set_header(&mine(T0 + 1000, CLOCK, 0)).unwrap(), T0 + 1000);
    let st = c.get_state().unwrap();
    assert!(st.anchored);
    assert_eq!((st.now, st.window, st.limit_msat, st.spent_msat), (T0 + 1000, 0, 5000, 0));
    assert_eq!(c.set_header(&mine(T0, CLOCK, 1)).unwrap(), T0, "an anchor may be earlier");
    c.apdu(&SELECT_APDU).unwrap();
    assert!(!c.get_state().unwrap().anchored, "a new selection has not anchored");
    assert_eq!(sw(c.set_header(&mine(T0 + 5, TEST_FLOOR, 2))), v3::SW_LITTLE_WORK, "a weak anchor");
    let st = c.get_state().unwrap();
    assert_eq!((st.anchored, st.now), (false, T0), "the weak anchor wrote nothing");
    // A slot holding value: a header is no anchor, an earlier one moves nothing.
    c.set_registry_key(&v3::registry_point(&reg())).unwrap();
    let x = c.gen_key(0).unwrap().serialize();
    c.set_note(0, 1000, &sig48(), &v3::sign_note(&reg(), &x, 1000, &sig48())).unwrap();
    assert!(!c.get_state().unwrap().anchors());
    assert_eq!(c.set_header(&mine(T0 - 500, CLOCK, 3)).unwrap(), T0);
    assert_eq!(c.set_header(&mine(T0 + 50, CLOCK, 4)).unwrap(), T0 + 50);
    assert!(!c.get_state().unwrap().anchored);
    // Change owed is value too.
    let mut c = card();
    c.gen_key(0).unwrap();
    c.set_change(0, 2048, &Opening { txid: [1; 32], out_idx: 0, blinding_key: [2; 32] }).unwrap();
    c.set_header(&mine(T0 + 10, CLOCK, 5)).unwrap();
    assert!(!c.get_state().unwrap().anchored);
    // After SET_PIN: no anchor either.
    let mut c = card();
    c.set_registry_key(&v3::registry_point(&reg())).unwrap();
    c.set_header(&mine(T0 + 5, CLOCK, 6)).unwrap();
    c.set_pin(PIN).unwrap();
    c.apdu(&SELECT_APDU).unwrap();
    assert_eq!(c.set_header(&mine(T0 + 10, CLOCK, 7)).unwrap(), T0 + 10);
    assert_eq!(c.set_header(&mine(T0, CLOCK, 8)).unwrap(), T0 + 10);
    assert!(!c.get_state().unwrap().anchored);
}

#[test]
fn an_anchor_never_lowers_the_hardest() {
    let mut c = card();
    c.set_header(&mine(T0, 0x201F_FFFF, 0)).unwrap();
    // Twice the hardest's target exactly: taken as an anchor, and not harder.
    assert_eq!(c.set_header(&mine(T0 + 1, 0x203F_FFFE, 1)).unwrap(), T0 + 1);
    // Were the hardest lowered to 0x203FFFFE this would be twice it; against 0x201FFFFF it is weak.
    assert_eq!(sw(c.set_header(&mine(T0 + 2, 0x207F_FFFC, 2))), v3::SW_LITTLE_WORK);
}

// ------------------------------------------------------------------ notes the registry signed

#[test]
fn set_note_refuses_in_the_applets_order_and_writes_nothing() {
    let (mut c, xs) = noted_card(3);
    let good = v3::sign_note(&reg(), &xs[0], 1000, &sig48());
    let before = c.get_slot(0).unwrap();
    // Length: 56 bytes and a DER of 8 to 72.
    assert_eq!(set_note_raw(&mut c, 0, 1000, &sig48(), &[]), 0x6700, "the unsigned form");
    assert_eq!(set_note_raw(&mut c, 0, 1000, &sig48(), &[0x30; 7]), 0x6700);
    assert_eq!(set_note_raw(&mut c, 0, 1000, &sig48(), &[0x30; 73]), 0x6700);
    let other = p256::ecdsa::SigningKey::random(&mut rand::thread_rng());
    let mut flipped = good.clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 1;
    let mut other_fed = sig48();
    other_fed[0] ^= 1;
    for (what, amount, fed, der) in [
        ("the shortest DER, r = s = 1", 1000, sig48(), vec![0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01]),
        ("another key", 1000, sig48(), v3::sign_note(&other, &xs[0], 1000, &sig48())),
        ("slot 1's note", 1000, sig48(), v3::sign_note(&reg(), &xs[1], 1000, &sig48())),
        ("a flipped byte", 1000, sig48(), flipped),
        ("another amount", 2000, sig48(), good.clone()),
        ("another federation signature", 1000, other_fed, good.clone()),
    ] {
        assert_eq!(set_note_raw(&mut c, 0, amount, &fed, &der), 0x6982, "{what}");
        assert_eq!(c.get_slot(0).unwrap(), before, "{what}: nothing written");
    }
    // State before the signature: an empty slot, a note already there.
    assert_eq!(set_note_raw(&mut c, 5, 1000, &sig48(), &good), 0x6985, "empty");
    // A zero amount, signed: refused after the state and the key.
    let zero = v3::sign_note(&reg(), &xs[2], 0, &sig48());
    assert_eq!(set_note_raw(&mut c, 2, 0, &sig48(), &zero), 0x6A80);
    c.set_note(0, 1000, &sig48(), &good).unwrap();
    let s = c.get_slot(0).unwrap();
    assert_eq!((s.state, s.amount_msat, s.sig48), (SlotState::Note, 1000, sig48()));
    assert_eq!(set_note_raw(&mut c, 0, 1000, &sig48(), &good), 0x6985, "a note is never overwritten");
    // A CHANGE slot: the amount owed is checked before the signature.
    let open = Opening { txid: [3; 32], out_idx: 1, blinding_key: [4; 32] };
    c.set_change(1, 4096, &open).unwrap();
    let owed = v3::sign_note(&reg(), &xs[1], 2048, &sig48());
    assert_eq!(set_note_raw(&mut c, 1, 2048, &sig48(), &owed), 0x6A80, "not the amount owed");
    assert_eq!(set_note_raw(&mut c, 1, 4096, &sig48(), &owed), 0x6982, "the amount owed, signed for another");
    assert_eq!(c.slot_state(1), Some(SlotState::Change));
    c.set_note(1, 4096, &sig48(), &v3::sign_note(&reg(), &xs[1], 4096, &sig48())).unwrap();
    let s = c.get_slot(1).unwrap();
    assert_eq!((s.state, s.amount_msat, s.opening), (SlotState::Note, 4096, Opening::from_bytes(&[0; 66])));
}

#[test]
fn no_registry_key_no_note_and_a_rekeyed_slot_refuses_its_old_note() {
    let mut c = card();
    let x = c.gen_key(0).unwrap().serialize();
    let good = v3::sign_note(&reg(), &x, 1000, &sig48());
    assert_eq!(set_note_raw(&mut c, 0, 1000, &sig48(), &[0x30; 7]), 0x6700, "length comes first");
    assert_eq!(set_note_raw(&mut c, 0, 1000, &sig48(), &good), 0x6985, "no registry key");
    let zero = v3::sign_note(&reg(), &x, 0, &sig48());
    assert_eq!(set_note_raw(&mut c, 0, 0, &sig48(), &zero), 0x6985, "the key before the amount");
    assert_eq!(c.slot_state(0), Some(SlotState::Keyed));
    // The slot's state before the key: change owed of another amount is 6A80 even with no key.
    c.gen_key(1).unwrap();
    c.set_change(1, 4096, &Opening { txid: [7; 32], out_idx: 0, blinding_key: [8; 32] }).unwrap();
    assert_eq!(set_note_raw(&mut c, 1, 2048, &sig48(), &good), 0x6A80);
    c.set_registry_key(&v3::registry_point(&reg())).unwrap();
    c.gen_key_overwrite(0).unwrap();
    assert_eq!(set_note_raw(&mut c, 0, 1000, &sig48(), &good), 0x6982, "signed for the old key");
    let x = c.get_pub(0).unwrap().serialize();
    c.set_note(0, 1000, &sig48(), &v3::sign_note(&reg(), &x, 1000, &sig48())).unwrap();
}

#[test]
fn either_half_of_s_is_taken() {
    let (mut c, xs) = noted_card(2);
    for (slot, x) in xs.iter().enumerate() {
        let sig = p256::ecdsa::Signature::from_der(&v3::sign_note(&reg(), x, 1000, &sig48())).unwrap();
        let (r, s) = sig.split_scalars();
        let flipped = p256::ecdsa::Signature::from_scalars(r.to_bytes(), (-s).to_bytes()).unwrap();
        let der = if slot == 0 { sig } else { flipped };
        c.set_note(slot as u8, 1000, &sig48(), der.to_der().as_bytes()).unwrap();
    }
    assert_eq!(c.get_state().unwrap().slots_in(SlotState::Note), vec![0, 1]);
}

#[test]
fn the_registrys_vectors() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../registry/test/note-sig-vectors.json");
    let file: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let hexed = |v: &serde_json::Value| hex::decode(v.as_str().unwrap()).unwrap();
    let point = hexed(&file["public_key"]);
    let key = p256::ecdsa::VerifyingKey::from_sec1_bytes(&point).unwrap();
    let cases = file["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 5);
    let mut on_card = 0;
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let x: [u8; 32] = hexed(&case["slot_xonly"]).try_into().unwrap();
        let amount = case["amount_msat"].as_u64().unwrap();
        let fed: [u8; 48] = hexed(&case["fed_sig"]).try_into().unwrap();
        let der = hexed(&case["reg_sig"]);
        let valid = case["valid"].as_bool().unwrap();
        assert_eq!(hexed(&case["amount_be"]), amount.to_be_bytes(), "{name}");
        assert_eq!(v3::note_message(&x, amount, &fed), hexed(&case["message"]), "{name}");
        assert_eq!(v3::verify_note(&key, &x, amount, &fed, &der), valid, "{name}");
        // Slot keys that are 1·G and 2·G go onto a card whose registry key is the vectors'.
        let secret = match hex::encode(x).as_str() {
            "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798" => 1u8,
            "c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5" => 2,
            _ => continue,
        };
        let mut sk = [0u8; 32];
        sk[31] = secret;
        let mut c = SoftCard::v4();
        c.set_registry_key(&point.clone().try_into().unwrap()).unwrap();
        c.key_slot_with(5, Keypair::from_seckey_slice(SECP256K1, &sk).unwrap()).unwrap();
        assert_eq!(c.get_pub(5).unwrap().serialize(), x);
        assert_eq!(set_note_raw(&mut c, 5, amount, &fed, &der), if valid { 0x9000 } else { 0x6982 }, "{name}");
        assert_eq!(c.slot_state(5), Some(if valid { SlotState::Note } else { SlotState::Keyed }));
        on_card += 1;
    }
    assert_eq!(on_card, 2, "valid-1 and valid-4 went onto a card");
}

#[test]
fn the_client_sends_the_note_and_its_signature_and_refuses_a_signature_of_the_wrong_size() {
    let mut c = scripted(&[&[0x90, 0x00][..]]);
    let der = vec![0x30; 70];
    c.set_note(3, 4096, &sig48(), &der).unwrap();
    let t = c.into_transport();
    let sent = &t.sent[0];
    assert_eq!(&sent[..5], &[0xB0, v3::INS_SET_NOTE, 3, 0, 56 + 70]);
    assert_eq!(&sent[5..13], &4096u64.to_be_bytes());
    assert_eq!(&sent[13..61], &sig48());
    assert_eq!(&sent[61..131], &der[..]);
    for n in [0, 7, 73] {
        let mut c = scripted(&[]);
        assert!(status_word(&c.set_note(3, 4096, &sig48(), &vec![0x30; n]).unwrap_err()).is_none(), "{n} bytes");
        assert!(c.into_transport().sent.is_empty(), "{n} bytes: nothing sent");
    }
}

// ------------------------------------------------------------------ SET_CHANGE behind the PIN

#[test]
fn set_change_after_set_pin_needs_the_pin_in_this_selection() {
    let mut c = card();
    for s in 0..4 {
        c.gen_key(s).unwrap();
    }
    let open = Opening { txid: [5; 32], out_idx: 0, blinding_key: [6; 32] };
    c.set_registry_key(&v3::registry_point(&reg())).unwrap();
    c.set_header(&mine(T0, CLOCK, 0)).unwrap();
    c.set_pin(PIN).unwrap();
    c.set_change(0, 1024, &open).unwrap();
    c.apdu(&SELECT_APDU).unwrap();
    // Without the PIN the gate comes first, before the length and the state.
    assert_eq!(sw(c.set_change(1, 1024, &open)), SW_PIN_REQUIRED);
    assert_eq!(sw(c.apdu(&v3_apdu(v3::INS_SET_CHANGE, 1, &[1, 2, 3]))), SW_PIN_REQUIRED);
    assert_eq!(c.slot_state(1), Some(SlotState::Keyed));
    c.verify_pin(PIN).unwrap();
    c.set_change(1, 1024, &open).unwrap();
    c.apdu(&SELECT_APDU).unwrap();
    assert_eq!(sw(c.set_change(2, 1024, &open)), SW_PIN_REQUIRED, "a new selection");
    c.verify_pin(PIN).unwrap();
    c.set_change(2, 1024, &open).unwrap();
    assert_eq!(c.get_state().unwrap().slots_in(SlotState::Change), vec![0, 1, 2]);
}

#[test]
fn an_iso_command_but_select_is_refused_for_its_class() {
    let mut c = SoftCard::v4();
    assert_eq!(sw(c.apdu(&[0x00, 0xB0, 0x00, 0x00, 0x00])), SW_CLA_NOT_SUPPORTED, "READ BINARY");
    assert_eq!(sw(c.apdu(&[0x00, 0x20, 0x00, 0x00, 0x00])), SW_CLA_NOT_SUPPORTED, "ISO VERIFY");
    assert_eq!(sw(c.apdu(&[0xB0, 0x7F, 0x00, 0x00, 0x00])), SW_INS_NOT_SUPPORTED, "its own class, no such INS");
}
