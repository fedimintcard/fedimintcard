//! Applet 4.0's PIN and SIGN on the `SoftCard` and through `CardClient`, against the applet's
//! own numbers (`FedimintCardApplet.java` `countTry`, `waitOwed`, `verifyPin`, `setPin`,
//! `changePin`, `sign`; `PinWaitTest`, `SetPinTest`, `ChangePinTest`, `SignatureCheckTest`): every
//! try counted with its wait before the compare, the wait served by resending VERIFY_PIN, never a
//! block, the first SET_PIN giving the PIN, CHANGE_PIN's length byte, SIGN's `6F00` with nothing
//! spent, and no PIN command on a session whose SELECT did not pass.

use fedimint_card_host_card::v3::{self, Opening, SlotState};
use fedimint_card_host_card::*;

const PIN: &[u8] = b"1234";
const WRONG: &[u8] = b"9999";
const H900000: &str = "00a0ab20247d4d9f582f9750344cdf62c46d81d046be960340960100000000000000000070f96945530651135839d8adc3f40e595118ec74c7ad81a3d17bb022e554fb0c937f4268743702177ad05f92";
const KEEP_HOLDING: u16 = v3::SW_KEEP_HOLDING;

fn sw<T: std::fmt::Debug>(r: anyhow::Result<T>) -> u16 {
    match r {
        Ok(_) => 0x9000,
        Err(e) => status_word(&e).unwrap_or_else(|| panic!("not a status word: {e:#}")),
    }
}

fn anchor() -> [u8; 80] {
    hex::decode(H900000).unwrap().try_into().unwrap()
}

/// What set-up sends before SET_PIN: the test registry's key and the anchor.
fn ready(c: &mut dyn Card) {
    c.set_registry_key(&v3::registry_point(&v3::test_registry_key())).unwrap();
    c.set_header(&anchor()).unwrap();
}

/// A 4.0 card with the PIN set, in a fresh selection (no PIN given).
fn pinned() -> SoftCard {
    let mut c = SoftCard::v4();
    ready(&mut c);
    c.set_pin(PIN).unwrap();
    c.apdu(&SELECT_APDU).unwrap();
    c
}

fn marks(c: &mut dyn Card) -> (u8, u16) {
    let st = c.get_state().unwrap();
    (st.wrong_tries, st.owed)
}

/// VERIFY_PIN resent while it answers 6A98, as a host does: (the 6A98s, the last answer).
fn asked(c: &mut dyn Card, pin: &[u8]) -> (u32, u16) {
    let mut n = 0;
    loop {
        match sw(c.verify_pin(pin)) {
            KEEP_HOLDING => n += 1,
            other => return (n, other),
        }
        assert!(n <= 20_000, "a wait goes down with the askings");
    }
}

fn load(c: &mut SoftCard, slot: u8, msat: u64) {
    let x = c.get_or_gen(slot).unwrap().serialize();
    let sig48 = [7u8; 48];
    c.set_note(slot, msat, &sig48, &v3::sign_note(&v3::test_registry_key(), &x, msat, &sig48)).unwrap();
}

fn raw(ins: u8, p1: u8, p2: u8, data: &[u8]) -> Vec<u8> {
    let mut a = vec![0xB0, ins, p1, p2, data.len() as u8];
    a.extend_from_slice(data);
    a.push(0);
    a
}

// ------------------------------------------------------------------ the wait (PinWaitTest)

#[test]
fn the_wait_doubles_and_every_try_is_counted_before_the_compare() {
    let mut c = pinned();
    assert_eq!(sw(c.verify_pin(WRONG)), 0x63C1, "the first wrong try waits for nothing");
    assert_eq!(marks(&mut c), (1, 1));
    for (k, want) in [1, 2, 4, 8, 16, 33].into_iter().enumerate() {
        let (n, last) = asked(&mut c, WRONG);
        assert_eq!(n, want, "the wait before wrong try {}", k + 2);
        assert_eq!(last, 0x63C0 | (k as u16 + 2));
        assert_eq!(marks(&mut c), (k as u8 + 2, 1 << (k + 1)), "after wrong try {}", k + 2);
        assert!(c.get_state().unwrap().pin_set, "set, never blocked");
    }
    // The right PIN waits its turn too, then clears both marks.
    // After seven wrong tries 64 are owed: four stretches, the last one's end is the try.
    assert_eq!(asked(&mut c, PIN), (64 + 3, 0x9000));
    assert_eq!(marks(&mut c), (0, 0));
    assert_eq!(sw(c.verify_pin(WRONG)), 0x63C1, "the next wrong try waits 1 again");
}

#[test]
fn the_wait_has_a_ceiling_and_nothing_blocks() {
    let mut c = pinned();
    c.set_pin_marks(14, 0);
    assert_eq!(sw(c.verify_pin(WRONG)), 0x63CF);
    assert_eq!(marks(&mut c), (15, PIN_OWED_MOST));
    c.set_pin_marks(15, 0);
    assert_eq!(sw(c.verify_pin(WRONG)), 0x63CF, "fifteen stays fifteen");
    assert_eq!(marks(&mut c), (15, 0x4000), "and the wait its ceiling");
    c.set_pin_marks(13, 0);
    assert_eq!(sw(c.verify_pin(WRONG)), 0x63CE);
    assert_eq!(marks(&mut c), (14, 0x2000));
    for i in 0..40 {
        c.set_pin_marks(15, 0);
        assert_eq!(sw(c.verify_pin(WRONG)), 0x63CF, "try {i}");
    }
    // The ceiling's wait, served in full: 16 384 askings and the 1 023 writes between stretches.
    assert_eq!(asked(&mut c, PIN), (0x4000 + 0x4000 / 16 - 1, 0x9000));
    assert_eq!(marks(&mut c), (0, 0));
    let mut c = pinned();
    // Reached the long way, fifteen wrong tries in a row put a card at the ceiling.
    for i in 1..=16u16 {
        assert_eq!(asked(&mut c, WRONG).1, 0x63C0 | i.min(15), "try {i}");
    }
    assert_eq!(marks(&mut c), (15, 0x4000));
}

#[test]
fn a_stretch_is_written_at_its_end_and_any_other_command_begins_it_again() {
    let mut c = pinned();
    c.set_pin_marks(6, 32);
    for _ in 0..10 {
        assert_eq!(sw(c.verify_pin(PIN)), KEEP_HOLDING);
    }
    assert_eq!(marks(&mut c), (6, 32), "ten askings of a stretch: nothing written");
    for k in 0..17 {
        assert_eq!(sw(c.verify_pin(PIN)), KEEP_HOLDING, "asking {}", k + 1);
    }
    assert_eq!(marks(&mut c), (6, 16), "a stretch ended: written");
    for _ in 0..5 {
        assert_eq!(sw(c.verify_pin(PIN)), KEEP_HOLDING);
    }
    c.get_state().unwrap();
    assert_eq!(asked(&mut c, PIN), (16, 0x9000), "GET_STATE between askings: the stretch from its first");
    let mut c = pinned();
    c.set_pin_marks(6, 32);
    for _ in 0..5 {
        assert_eq!(sw(c.verify_pin(PIN)), KEEP_HOLDING);
    }
    c.get_state().unwrap();
    assert_eq!(asked(&mut c, PIN), (33, 0x9000), "GET_STATE between askings: the stretch from its first");
    // SELECT, and the commands the trait sends without `apdu` (GET_PUB, GEN_KEY, SIGN), begin the
    // stretch again; a malformed VERIFY_PIN does not, and serves no asking.
    c.gen_key(0).unwrap();
    let hop: [&dyn Fn(&mut SoftCard); 4] = [
        &|c| {
            c.apdu(&SELECT_APDU).unwrap();
        },
        &|c| {
            c.get_pub(0).unwrap();
        },
        &|c| {
            let _ = c.gen_key(0);
        },
        &|c| {
            let _ = c.sign(0, &[1; 32]);
        },
    ];
    for (i, between) in hop.iter().enumerate() {
        c.set_pin_marks(1, 1);
        assert_eq!(sw(c.verify_pin(PIN)), KEEP_HOLDING);
        between(&mut c);
        assert_eq!(asked(&mut c, PIN), (1, 0x9000), "command {i}: from the first asking");
    }
    c.set_pin_marks(1, 1);
    assert_eq!(sw(c.verify_pin(PIN)), KEEP_HOLDING);
    assert_eq!(sw(c.apdu(&raw(INS_VERIFY_PIN, 1, 0, PIN))), SW_WRONG_P1P2);
    assert_eq!(sw(c.apdu(&raw(INS_VERIFY_PIN, 0, 0, b"123"))), 0x6700);
    assert_eq!(sw(c.verify_pin(PIN)), 0x9000, "the stretch went on: its second asking is the try");
}

#[test]
fn verify_pin_checks_its_frame_before_the_wait_and_a_short_field_after_it() {
    let mut c = SoftCard::v4();
    assert_eq!(sw(c.apdu(&raw(INS_VERIFY_PIN, 1, 0, b"12"))), SW_NO_PIN_SET, "no PIN outranks the frame");
    let mut c = pinned();
    c.set_pin_marks(2, 1);
    assert_eq!(sw(c.apdu(&raw(INS_VERIFY_PIN, 0, 0, b"123456789"))), 0x6700);
    assert_eq!(sw(c.apdu(&[0xB0, INS_VERIFY_PIN, 0, 0, 0x80, 0])), 0x6700, "an Lc of 128 and up");
    assert_eq!(sw(c.apdu(&raw(INS_VERIFY_PIN, 0, 1, PIN))), SW_WRONG_P1P2);
    assert_eq!(marks(&mut c), (2, 1), "a bad frame costs neither a try nor an asking");
    // Lc 4 with two bytes: the wait first, then the receive comes up short, at no try.
    assert_eq!(sw(c.apdu(&[0xB0, INS_VERIFY_PIN, 0, 0, 4, b'1', b'2'])), KEEP_HOLDING);
    assert_eq!(sw(c.apdu(&[0xB0, INS_VERIFY_PIN, 0, 0, 4, b'1', b'2'])), 0x6700);
    assert_eq!(marks(&mut c), (2, 0), "the wait was served, no try was made");
    assert_eq!(sw(c.verify_pin(PIN)), 0x9000);
}

#[test]
fn a_try_ends_the_session_and_only_a_right_pin_opens_it() {
    let mut c = SoftCard::v4();
    ready(&mut c);
    c.set_pin(PIN).unwrap();
    load(&mut c, 0, 1000);
    load(&mut c, 1, 1000);
    c.sign(0, &[1; 32]).unwrap();
    assert_eq!(sw(c.verify_pin(WRONG)), 0x63C1);
    assert_eq!(sw(c.sign(1, &[2; 32])), SW_PIN_REQUIRED, "a wrong try ends what SET_PIN gave");
    assert_eq!(asked(&mut c, PIN), (1, 0x9000));
    c.sign(1, &[2; 32]).unwrap();
    assert_eq!(sw(c.verify_pin(WRONG)), 0x63C1);
    assert_eq!(sw(c.sign(1, &[2; 32])), SW_PIN_REQUIRED, "and what VERIFY_PIN gave");
}

// ------------------------------------------------------------------ SET_PIN (SetPinTest)

#[test]
fn set_pin_refuses_in_the_applets_order_and_sets_nothing() {
    let mut c = SoftCard::v4();
    let set = |c: &mut SoftCard, p1: u8, pin: &[u8]| sw(c.apdu(&raw(INS_SET_PIN, p1, 0, pin)));
    assert_eq!(set(&mut c, 1, b"12"), SW_WRONG_P1P2, "P1/P2 first");
    assert_eq!(set(&mut c, 0, b"123"), 0x6700, "the length before the anchor");
    assert_eq!(set(&mut c, 0, PIN), v3::SW_NOT_ANCHORED, "no anchor, before the registry key");
    c.set_header(&anchor()).unwrap();
    assert_eq!(set(&mut c, 0, PIN), 0x6985, "no registry key");
    c.set_registry_key(&v3::registry_point(&v3::test_registry_key())).unwrap();
    load(&mut c, 0, 1000);
    assert_eq!(set(&mut c, 0, PIN), 0x6985, "a note on the card");
    let mut c = SoftCard::v4();
    ready(&mut c);
    c.gen_key(0).unwrap();
    c.set_change(0, 2048, &Opening { txid: [1; 32], out_idx: 0, blinding_key: [2; 32] }).unwrap();
    assert_eq!(set(&mut c, 0, PIN), 0x6985, "change owed");
    let mut c = SoftCard::v4();
    ready(&mut c);
    c.apdu(&SELECT_APDU).unwrap();
    assert_eq!(set(&mut c, 0, PIN), v3::SW_NOT_ANCHORED, "an anchor of an earlier selection does not count");
    assert_eq!(sw(c.apdu(&[0xB0, INS_SET_PIN, 0, 0, 6, b'1', b'2'])), v3::SW_NOT_ANCHORED);
    c.set_header(&anchor()).unwrap();
    assert_eq!(sw(c.apdu(&[0xB0, INS_SET_PIN, 0, 0, 6, b'1', b'2'])), 0x6700, "a short field");
    assert!(!c.get_state().unwrap().pin_set, "no refusal set a PIN");
    c.set_pin(PIN).unwrap();
    assert_eq!(set(&mut c, 0, b"5678"), 0x6985, "once");
    assert_eq!(set(&mut c, 0, b"123"), 0x6985, "a set PIN outranks the length");
    assert_eq!(set(&mut c, 2, PIN), SW_WRONG_P1P2);
}

#[test]
fn the_first_set_pin_gives_the_pin_to_its_selection_only() {
    let mut c = SoftCard::v4();
    ready(&mut c);
    c.set_pin(PIN).unwrap();
    load(&mut c, 0, 1000);
    load(&mut c, 1, 1000);
    c.gen_key(2).unwrap();
    c.set_change(2, 64, &Opening { txid: [3; 32], out_idx: 0, blinding_key: [4; 32] }).unwrap();
    c.sign(0, &[1; 32]).unwrap();
    c.gen_key_overwrite(0).unwrap();
    c.apdu(&SELECT_APDU).unwrap();
    assert_eq!(sw(c.sign(1, &[1; 32])), SW_PIN_REQUIRED, "a new selection");
    assert_eq!(sw(c.gen_key_overwrite(0)), SW_PIN_REQUIRED);
    assert_eq!(c.get_state().unwrap().states[1], SlotState::Note);
}

// ------------------------------------------------------------------ CHANGE_PIN (ChangePinTest)

#[test]
fn change_pin_takes_the_old_pins_length_and_needs_this_sessions_pin() {
    let mut c = SoftCard::v4();
    assert_eq!(sw(c.change_pin(PIN, b"5678")), SW_NO_PIN_SET);
    let mut c = pinned();
    let change = |c: &mut SoftCard, p1: u8, p2: u8, data: &[u8]| sw(c.apdu(&raw(INS_CHANGE_PIN, p1, p2, data)));
    for p1 in [1, 3, 9, 0x80, 0xFF] {
        assert_eq!(change(&mut c, p1, 0, b"12345678"), SW_WRONG_P1P2, "P1 {p1:#x}, before the session");
    }
    assert_eq!(change(&mut c, 4, 1, b"12345678"), SW_WRONG_P1P2);
    assert_eq!(sw(c.change_pin(PIN, b"5678")), SW_PIN_REQUIRED);
    c.verify_pin(PIN).unwrap();
    assert_eq!(change(&mut c, 0, 0, b"123456789"), 0x6700, "P1 = 0 wants an even Lc");
    assert_eq!(change(&mut c, 4, 0, b"1234567"), 0x6700, "a new PIN of 3");
    assert_eq!(change(&mut c, 4, 0, b"1234123456789"), 0x6700, "a new PIN of 9");
    assert_eq!(change(&mut c, 8, 0, b"12345678123456789"), 0x6700, "Lc 17");
    assert_eq!(change(&mut c, 8, 0, b"1234567"), 0x6700, "an old PIN longer than the field");
    assert_eq!(sw(c.apdu(&[0xB0, INS_CHANGE_PIN, 4, 0, 8, b'1', b'2', b'3', b'4'])), 0x6700, "a short field");
    assert_eq!(marks(&mut c), (0, 0), "the lengths cost no try");
    // P1 = 0: equal halves. The change ends the session.
    assert_eq!(change(&mut c, 0, 0, b"12345678"), 0x9000);
    assert_eq!(sw(c.change_pin(b"5678", b"4321")), SW_PIN_REQUIRED, "a change ends the session");
    assert_eq!(sw(c.verify_pin(PIN)), 0x63C1, "the old PIN is gone");
    assert_eq!(asked(&mut c, b"5678"), (1, 0x9000));
    // P1 = the old length: halves of different lengths.
    c.change_pin(b"5678", b"87654321").unwrap();
    c.verify_pin(b"87654321").unwrap();
    // A wrong old PIN is a try: counted with its wait, the session ended, the PIN kept.
    assert_eq!(sw(c.change_pin(b"0000", b"1111")), 0x63C1);
    assert_eq!(marks(&mut c), (1, 1));
    assert_eq!(sw(c.change_pin(b"87654321", b"1111")), SW_PIN_REQUIRED);
    assert_eq!(asked(&mut c, b"87654321"), (1, 0x9000));
}

#[test]
fn the_client_sends_the_old_pins_length_and_refuses_a_bad_pin_unsent() {
    let mut c = scripted(&[&[4, 0, 0x90, 0x00], &[0x90, 0x00]]);
    c.select().unwrap();
    c.change_pin(b"1234", b"123456").unwrap();
    assert!(c.change_pin(b"123", b"1234").is_err());
    assert!(c.change_pin(b"1234", b"123456789").is_err());
    let sent = c.into_transport().sent;
    assert_eq!(sent.len(), 2, "the bad PINs were never sent");
    assert_eq!(sent[1], [&[0xB0, INS_CHANGE_PIN, 4, 0, 10][..], b"1234123456", &[0]].concat());
}

// ------------------------------------------------------------------ the host's helpers

/// Each APDU handed to a `SoftCard` as the applet would take it; every APDU is kept.
struct Logged {
    card: SoftCard,
    sent: Vec<Vec<u8>>,
}

impl Transport for Logged {
    fn transmit(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.sent.push(apdu.to_vec());
        let c = &mut self.card;
        let slot = apdu[2];
        let r = match (apdu[0], apdu[1]) {
            (0xB0, 0x10) if apdu[3] == 0 => c.gen_key(slot).map(|k| k.serialize().to_vec()),
            (0xB0, 0x10) => c.gen_key_overwrite(slot).map(|k| k.serialize().to_vec()),
            (0xB0, 0x11) => c.get_pub(slot).map(|k| k.serialize().to_vec()),
            (0xB0, 0x12) => c.sign(slot, apdu[5..37].try_into().unwrap()).map(|s| s.serialize().to_vec()),
            _ => c.apdu(apdu),
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

fn client(card: SoftCard) -> CardClient<Logged> {
    let mut c = CardClient::new(Logged { card, sent: vec![] });
    c.select().unwrap();
    c
}

fn ins_sent(c: CardClient<Logged>) -> Vec<u8> {
    c.into_transport().sent.iter().map(|a| a[1]).collect()
}

#[test]
fn verify_pin_held_resends_with_nothing_between_and_counts_down() {
    let mut card = pinned();
    card.set_pin_marks(6, 32);
    let mut c = client(card);
    let mut seen = vec![];
    verify_pin_held(&mut c, PIN, &mut |left| seen.push(left)).unwrap();
    assert_eq!(seen.len(), 1 + 33, "once before, once per 6A98");
    assert_eq!((seen[0], *seen.last().unwrap()), (32, 0));
    assert!(seen.windows(2).all(|w| w[1] <= w[0]), "it only goes down: {seen:?}");
    let ins = ins_sent(c);
    assert_eq!(ins[..2], [0xA4, v3::INS_GET_STATE], "the count is read before the first asking");
    assert!(ins[2..].iter().all(|&i| i == INS_VERIFY_PIN), "nothing between the askings");
    assert_eq!(ins.len(), 2 + 34);
    // A wrong PIN comes back as its 63CX once its wait is served; no wait: one VERIFY_PIN.
    let mut card = pinned();
    card.set_pin_marks(1, 1);
    let mut c = client(card);
    assert_eq!(sw(verify_pin_held(&mut c, WRONG, &mut |_| {})), 0x63C2);
    assert_eq!(sw(verify_pin_held(&mut c, PIN, &mut |_| {})), 0x9000);
    let mut c = client(pinned());
    verify_pin_held(&mut c, PIN, &mut |_| {}).unwrap();
    assert_eq!(ins_sent(c), [0xA4, v3::INS_GET_STATE, INS_VERIFY_PIN]);
}

#[test]
fn open_pin_session_sets_the_pin_and_sends_no_second_verify() {
    let mut card = SoftCard::v4();
    card.set_registry_key(&v3::registry_point(&v3::test_registry_key())).unwrap();
    load(&mut card, 0, 1000);
    // A card holding value takes no anchor, so no first PIN: the refusal comes back.
    let mut c = client(card);
    c.set_header(&anchor()).unwrap();
    assert_eq!(sw(open_pin_session_with(&mut c, PIN)), v3::SW_NOT_ANCHORED);
    let mut c = client(SoftCard::v4());
    c.gen_key(0).unwrap();
    ready(&mut c);
    open_pin_session_with(&mut c, PIN).unwrap();
    let x = c.get_pub(0).unwrap().serialize();
    c.set_note(0, 1000, &[7; 48], &v3::sign_note(&v3::test_registry_key(), &x, 1000, &[7; 48])).unwrap();
    c.sign(0, &[1; 32]).unwrap();
    let ins = ins_sent(c);
    let pin_cmds: Vec<u8> = ins.iter().copied().filter(|i| [INS_VERIFY_PIN, INS_SET_PIN, INS_CHANGE_PIN].contains(i)).collect();
    assert_eq!(pin_cmds, [INS_VERIFY_PIN, INS_SET_PIN], "VERIFY (6984), SET_PIN, and no VERIFY after it");
    // An old 2.1 card's SET_PIN opens nothing, so it still gets its VERIFY.
    let mut old = SoftCard::new();
    old.gen_key(0).unwrap();
    open_pin_session_with(&mut old, PIN).unwrap();
    old.sign(0, &[1; 32]).unwrap();
}

// ------------------------------------------------------------------ the rule for every reader

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

fn no_pin_command_sent(sent: &[Vec<u8>]) -> bool {
    !sent.iter().any(|a| a[0] == 0xB0 && [INS_VERIFY_PIN, INS_SET_PIN, INS_CHANGE_PIN].contains(&a[1]))
}

#[test]
fn no_pin_command_reaches_a_card_whose_select_did_not_pass() {
    // Before any SELECT: refused, nothing sent.
    let mut c = scripted(&[]);
    for e in [c.verify_pin(PIN).unwrap_err(), c.set_pin(PIN).unwrap_err(), c.change_pin(PIN, b"5678").unwrap_err()] {
        assert!(e.downcast_ref::<NotSelected>().is_some(), "{e:#}");
    }
    assert!(c.apdu(&raw(INS_VERIFY_PIN, 0, 0, PIN)).is_err(), "not by the raw path either");
    assert!(c.into_transport().sent.is_empty());
    // An old card (3.0, 2.x): the old-card error, from SELECT and from every PIN command after.
    for old in [&[3u8, 0, 0x90, 0x00][..], &[0x90, 0x00]] {
        let mut c = scripted(&[old, &[0x6D, 0x00]]);
        assert!(is_old_card(&c.select().unwrap_err()));
        assert!(is_old_card(&c.verify_pin(PIN).unwrap_err()));
        assert!(is_old_card(&c.set_pin(PIN).unwrap_err()));
        assert!(is_old_card(&c.change_pin(PIN, b"5678").unwrap_err()));
        let e = open_pin_session_with(&mut c, PIN).unwrap_err();
        assert!(is_old_card(&e), "{e:#}");
        let sent = c.into_transport().sent;
        assert!(no_pin_command_sent(&sent), "{sent:x?}");
    }
    // A raw SELECT that answers an old card, or no applet, ends what the last one passed.
    let mut c = scripted(&[&[4, 0, 0x90, 0x00], &[3, 0, 0x90, 0x00], &[0x6A, 0x82]]);
    c.select().unwrap();
    c.apdu(&SELECT_APDU).unwrap();
    assert!(is_old_card(&c.verify_pin(PIN).unwrap_err()));
    assert!(c.apdu(&SELECT_APDU).is_err());
    assert!(c.verify_pin(PIN).unwrap_err().downcast_ref::<NotSelected>().is_some());
    assert!(no_pin_command_sent(&c.into_transport().sent));
    // Over a 4.0 SoftCard the PIN commands go through once SELECT passed.
    let mut c = CardClient::new(Logged { card: pinned(), sent: vec![] });
    assert!(c.verify_pin(PIN).is_err());
    c.select().unwrap();
    c.verify_pin(PIN).unwrap();
}

// ------------------------------------------------------------------ SIGN's 6F00 (SignatureCheckTest)

#[test]
fn a_signature_the_card_refuses_spends_nothing() {
    let mut c = SoftCard::v4();
    c.set_limit(1500).unwrap();
    ready(&mut c);
    c.set_pin(PIN).unwrap();
    load(&mut c, 0, 1000);
    load(&mut c, 1, 1000);
    let before = c.get_state().unwrap();
    c.fail_next_sign_check();
    let e = c.sign(0, &[1; 32]).unwrap_err();
    assert!(is_sign_check_failed(&e), "{e:#}");
    assert_eq!(status_word(&e), Some(v3::SW_CRYPTO_ERROR));
    let after = c.get_state().unwrap();
    assert_eq!(c.slot_state(0), Some(SlotState::Note), "the note is still there");
    assert_eq!((after.now, after.window, after.spent_msat), (before.now, before.window, before.spent_msat));
    assert_eq!(c.get_slot(0).unwrap().spent_hash, [0; 32]);
    // One SIGN only, and only one that reaches the signature: SPENT's answer from storage and the
    // limit's 6A95 come before it.
    c.sign(0, &[1; 32]).unwrap();
    c.fail_next_sign_check();
    c.sign(0, &[1; 32]).unwrap();
    assert_eq!(sw(c.sign(1, &[2; 32])), v3::SW_OVER_LIMIT);
    assert_eq!(c.slot_state(1), Some(SlotState::Note));
    let mut c = SoftCard::v4();
    ready(&mut c);
    c.set_pin(PIN).unwrap();
    load(&mut c, 0, 1000);
    c.fail_next_sign_check();
    assert_eq!(sw(c.sign(0, &[1; 32])), v3::SW_CRYPTO_ERROR);
    c.sign(0, &[2; 32]).unwrap();
    assert_eq!(c.slot_state(0), Some(SlotState::Spent), "tapped again, it spends");
}

#[test]
fn the_client_gives_sign_s_6f00_as_its_own_error() {
    let mut c = scripted(&[&[4, 0, 0x90, 0x00], &[0x6F, 0x00], &[0x6F, 0x00]]);
    c.select().unwrap();
    let e = c.sign(0, &[1; 32]).unwrap_err();
    assert!(is_sign_check_failed(&e), "{e:#}");
    assert_eq!(status_word(&e), Some(0x6F00));
    // 6F00 to anything else is only its status word.
    let e = c.get_state().unwrap_err();
    assert!(!is_sign_check_failed(&e));
    assert_eq!(status_word(&e), Some(0x6F00));
    // Over a SoftCard through the APDUs.
    let mut card = SoftCard::v4();
    ready(&mut card);
    card.set_pin(PIN).unwrap();
    load(&mut card, 0, 1000);
    card.fail_next_sign_check();
    let mut c = client(card);
    c.verify_pin(PIN).unwrap();
    assert!(is_sign_check_failed(&c.sign(0, &[1; 32]).unwrap_err()));
    assert_eq!(c.get_slot(0).unwrap().state, SlotState::Note);
}

// ------------------------------------------------------------------ SELECT of another application

/// SELECTs that a host may send through `apdu` and that select something other than this applet:
/// another application's AID, the card manager (no AID), a part of this AID, this AID with
/// P2 = 0C, this AID on another channel.
fn other_selects() -> Vec<Vec<u8>> {
    let mut p2_0c = SELECT_APDU[..13].to_vec();
    p2_0c[3] = 0x0C;
    let mut channel = SELECT_APDU.to_vec();
    channel[0] = 0x01;
    vec![
        vec![0x00, 0xA4, 0x04, 0x00, 0x07, 0xD2, 0x76, 0x00, 0x00, 0x85, 0x01, 0x01, 0x00],
        vec![0x00, 0xA4, 0x04, 0x00, 0x00],
        vec![0x00, 0xA4, 0x04, 0x00, 0x06, 0xF0, 0x46, 0x4D, 0x43, 0x41, 0x52, 0x00],
        p2_0c,
        channel,
    ]
}

#[test]
fn a_select_of_any_other_application_ends_what_the_last_one_passed() {
    for other in other_selects() {
        // The other application answers what this applet would: it still selects nothing of ours.
        let mut c = scripted(&[&[4, 0, 0x90, 0x00], &[4, 0, 0x90, 0x00]]);
        c.select().unwrap();
        c.apdu(&other).unwrap();
        assert_eq!(c.selected(), None, "{other:02x?}");
        for e in [c.verify_pin(PIN).unwrap_err(), c.set_pin(PIN).unwrap_err(), c.change_pin(PIN, b"5678").unwrap_err()] {
            assert!(e.downcast_ref::<NotSelected>().is_some(), "{other:02x?}: {e:#}");
        }
        assert!(c.apdu(&raw(INS_VERIFY_PIN, 0, 0, PIN)).is_err(), "not by the raw path either");
        assert!(no_pin_command_sent(&c.into_transport().sent), "{other:02x?}");
    }
    // A SELECT of the applet with its exact frame passes again.
    let mut c = scripted(&[&[4, 0, 0x90, 0x00], &[4, 0, 0x90, 0x00], &[0x90, 0x00]]);
    c.apdu(&other_selects()[0]).unwrap();
    c.apdu(&SELECT_APDU).unwrap();
    assert_eq!(c.selected(), Some(AppletVersion::V4_0));
    c.verify_pin(PIN).unwrap();
}

#[test]
fn the_version_after_a_select_that_did_not_pass_is_that_selects_error() {
    // An old card's SELECT: the old-card error, and no GET_STATE goes to it.
    let mut c = scripted(&[&[3, 0, 0x90, 0x00]]);
    assert!(c.select().is_err());
    let e = c.applet_version().unwrap_err();
    assert!(is_old_card(&e), "{e:#}");
    let e = verify_pin_held(&mut c, PIN, &mut |_| {}).unwrap_err();
    assert!(is_old_card(&e), "{e:#}");
    assert_eq!(c.into_transport().sent.len(), 1, "only the SELECT");
    // No applet, or another application selected: not selected, and nothing sent.
    let mut c = scripted(&[&[0x6A, 0x82]]);
    assert!(c.select().is_err());
    assert!(c.applet_version().unwrap_err().downcast_ref::<NotSelected>().is_some());
    assert_eq!(c.into_transport().sent.len(), 1);
    let mut c = scripted(&[&[4, 0, 0x90, 0x00], &[0x90, 0x00]]);
    c.select().unwrap();
    c.apdu(&other_selects()[1]).unwrap();
    assert!(c.applet_version().unwrap_err().downcast_ref::<NotSelected>().is_some());
    assert_eq!(c.into_transport().sent.len(), 2);
    // A client that never sent a SELECT (its transport selects) still asks GET_STATE.
    let mut c = scripted(&[&[0x6D, 0x00]]);
    assert_eq!(c.applet_version().unwrap(), AppletVersion::V2_1);
    assert_eq!(c.into_transport().sent.len(), 1);
}

/// SELECT answered 4.0, GET_STATE answered `state`, every VERIFY_PIN answered 6A98.
struct Holding {
    state: Vec<u8>,
    verifies: usize,
}

impl Transport for Holding {
    fn transmit(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        if apdu[1] == INS_VERIFY_PIN {
            self.verifies += 1;
            return Ok(KEEP_HOLDING.to_be_bytes().to_vec());
        }
        if apdu[1] == 0xA4 {
            return Ok(vec![4, 0, 0x90, 0x00]);
        }
        assert_eq!(apdu[1], v3::INS_GET_STATE);
        Ok(self.state.clone())
    }
}

#[test]
fn verify_pin_held_stops_at_a_6a98_the_wait_does_not_owe() {
    // (wrong tries, owed) -> the askings the card answers 6A98 before it tries the PIN.
    for (wrong, owed, waits) in [(0u8, 0u16, 0usize), (1, 1, 1), (6, 32, 33)] {
        let mut card = pinned();
        card.set_pin_marks(wrong, owed);
        let mut state = card.apdu(&v3_apdu(v3::INS_GET_STATE, 0, &[])).unwrap();
        state.extend_from_slice(&[0x90, 0x00]);
        let mut c = CardClient::new(Holding { state, verifies: 0 });
        c.select().unwrap();
        let e = verify_pin_held(&mut c, PIN, &mut |_| {}).unwrap_err();
        assert!(status_word(&e).is_none(), "{e:#}");
        assert_eq!(c.into_transport().verifies, waits + 1, "owed {owed}: stops at the first 6A98 past the wait");
    }
}
