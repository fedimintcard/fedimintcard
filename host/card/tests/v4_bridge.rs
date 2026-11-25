//! Every applet 4.0 path the library offers, driven through `CardClient` on a fresh card: on the
//! `SoftCard` always, and on the real applet behind the bridge when `CARD_BRIDGE_URL` is set. Each
//! step's answer is checked against the applet's, and the bridge's answers must be the SoftCard's,
//! step for step: this is what makes the SoftCard a stand-in for the card in the hosts' tests.
//!
//! Paths: set-up (registry key and anchor in SET_PIN's selection), a weak header, GEN_KEY and its
//! overwrite (never of the certified slot 0, even spent and under the PIN), a registry-signed note and a wrongly signed one, the change gate, a right PIN, a
//! wrong PIN and the wait it owes served by `verify_pin_held`, SIGN and SIGN again, GET_STATE's
//! 65 bytes, CHANGE_PIN with the old PIN's length.

use fedimint_card_host_card::v3::{self, Opening};
use fedimint_card_host_card::*;
use fedimint_card_old_card_pin::ClearPin;
use secp256k1::{Message, SECP256K1};

const PIN: &[u8] = b"1234";
const NEW_PIN: &[u8] = b"567890";
const WRONG: &[u8] = b"9999";
const H900000: &str = "00a0ab20247d4d9f582f9750344cdf62c46d81d046be960340960100000000000000000070f96945530651135839d8adc3f40e595118ec74c7ad81a3d17bb022e554fb0c937f4268743702177ad05f92";

/// Each APDU handed to a `SoftCard` as a reader would carry it to the applet.
struct Soft(SoftCard);

impl Transport for Soft {
    fn transmit(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        let (c, slot) = (&mut self.0, apdu[2]);
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
            Err(e) => status_word(&e).unwrap_or(0x6F00).to_be_bytes().to_vec(),
        })
    }
}

fn sw<T>(r: anyhow::Result<T>) -> String {
    match r {
        Ok(_) => "9000".into(),
        Err(e) => match status_word(&e) {
            Some(s) => format!("{s:04X}"),
            None => format!("error: {e:#}"),
        },
    }
}

/// A header with `time` and `bits` whose hash meets its own bits.
fn mine(time: u32, bits: u32) -> [u8; 80] {
    let mut h = [0u8; 80];
    h[0] = 4;
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

/// The 6A98s `verify_pin_held` served, and its answer.
fn held(c: &mut dyn Card, t: &TrustedCard, pin: &[u8]) -> String {
    let mut calls = 0;
    let r = verify_pin_held(c, t, pin, &mut |_| calls += 1);
    format!("{} after {} waits", sw(r), calls - 1)
}

fn state(c: &mut dyn Card) -> v3::CardStatus {
    c.get_state().expect("GET_STATE parses")
}

/// Every path, in order, on a fresh card; one line per step.
fn paths<T: Transport>(c: &mut CardClient<T>) -> Vec<String> {
    let mut log = vec![];
    let mut step = |name: &str, answer: String| log.push(format!("{name}: {answer}"));
    let registry = v3::test_registry_key();
    let other_registry = p256::ecdsa::SigningKey::from_slice(&[0x22; 32]).unwrap();
    let anchor: [u8; 80] = hex::decode(H900000).unwrap().try_into().unwrap();
    let sig48 = [0x48u8; 48];

    step("select", sw(c.select()));
    step("version", format!("{:?}", c.selected()));
    let raw = c.apdu(&v3_apdu(v3::INS_GET_STATE, 0, &[])).unwrap();
    step("GET_STATE fresh, bytes", raw.len().to_string());
    let st = state(c);
    step("fresh: pin, anchored, registry", format!("{} {} {}", st.pin_set, st.anchored, st.registry_key));

    // The sealed PIN (§ 9): a PIN key bound by a certified attestation key, before any PIN.
    let x0 = c.gen_key(0).unwrap();
    step("GET_NONCE before the certificate", sw(c.get_nonce()));
    let w = c.attest_init().unwrap();
    step("SET_CERT", sw(c.set_cert(&Certificate::sign(&registry, &x0.serialize(), &w).unwrap().to_bytes())));
    let t = trust_card(c, &v3::registry_point(&registry)).unwrap();
    let clear = c.set_pin(PIN).unwrap_err().downcast_ref::<ClearPinRefused>().is_some();
    step("SET_PIN with P1 00", if clear { "refused unsent" } else { "sent" }.to_string());

    // Set-up: SET_PIN needs this selection's anchor, then the registry key.
    step("SET_PIN before the anchor", sw(set_pin_sealed(c, &t, PIN)));
    step("SET_HEADER weak, before any anchor", sw(c.set_header(&mine(1_700_000_000, 0x207F_FFFF))));
    step("SET_HEADER anchor", c.set_header(&anchor).map(|now| now.to_string()).unwrap_or_else(|e| sw::<()>(Err(e))));
    step("SET_PIN before the registry key", sw(set_pin_sealed(c, &t, PIN)));
    step("SET_REGISTRY_KEY", sw(c.set_registry_key(&v3::registry_point(&registry))));
    step("SET_HEADER weak, after the anchor", sw(c.set_header(&mine(1_800_000_000, 0x207F_FFFF))));
    step("GEN_KEY 0 again", sw(c.gen_key(0)));
    c.gen_key(1).unwrap();
    c.gen_key(2).unwrap();
    let x3 = c.gen_key(3).unwrap().serialize();
    step("SET_PIN", sw(set_pin_sealed(c, &t, PIN)));
    step("SET_PIN again", sw(set_pin_sealed(c, &t, PIN)));
    let st = state(c);
    step("after SET_PIN: pin, anchored, wrong, owed", format!("{} {} {} {}", st.pin_set, st.anchored, st.wrong_tries, st.owed));

    // Notes: only the registry's signature loads one.
    let x = x0.serialize();
    step("SET_NOTE signed by another key", sw(c.set_note(0, 1000, &sig48, &v3::sign_note(&other_registry, &x, 1000, &sig48))));
    step("SET_NOTE over another amount", sw(c.set_note(0, 1001, &sig48, &v3::sign_note(&registry, &x, 1000, &sig48))));
    step("slot 0 after the refusals", format!("{:?}", c.get_slot(0).unwrap().state));
    step("SET_NOTE", sw(c.set_note(0, 1000, &sig48, &v3::sign_note(&registry, &x, 1000, &sig48))));
    let s0 = c.get_slot(0).unwrap();
    step("slot 0", format!("{:?} {}", s0.state, s0.amount_msat));
    // A NOTE slot other than 0, so the refusal is the NOTE state's and not slot 0's certificate.
    step("SET_NOTE slot 3", sw(c.set_note(3, 500, &sig48, &v3::sign_note(&registry, &x3, 500, &sig48))));
    step("GEN_KEY overwrite of a NOTE slot", sw(c.gen_key_overwrite(3)));
    step("slot 3 after the refusal", format!("{:?} {}", c.get_slot(3).unwrap().state, c.get_pub(3).unwrap().serialize() == x3));

    // A new selection: no PIN in it, so the change gate and SIGN refuse.
    let opening = Opening { txid: [3; 32], out_idx: 1, blinding_key: [4; 32] };
    step("select again", sw(c.select()));
    step("SET_CHANGE without the PIN", sw(c.set_change(1, 2000, &opening)));
    step("SIGN without the PIN", sw(c.sign(0, &[0x5A; 32])));
    step("GEN_KEY overwrite without the PIN", sw(c.gen_key_overwrite(2)));

    // The right PIN, then a wrong one and the waits it owes.
    step("VERIFY_PIN right", held(c, &t, PIN));
    step("VERIFY_PIN wrong", sw(verify_pin_held(c, &t, WRONG, &mut |_| {})));
    let st = state(c);
    step("after one wrong: wrong, owed", format!("{} {}", st.wrong_tries, st.owed));
    step("VERIFY_PIN wrong, held", held(c, &t, WRONG));
    step("SIGN after a wrong PIN", sw(c.sign(0, &[0x5A; 32])));
    step("VERIFY_PIN right, held", held(c, &t, PIN));
    let st = state(c);
    step("after the right PIN: wrong, owed", format!("{} {}", st.wrong_tries, st.owed));

    // With the PIN: the change gate opens, GEN_KEY may overwrite a KEYED slot, SIGN signs once.
    step("SET_CHANGE", sw(c.set_change(1, 2000, &opening)));
    let s1 = c.get_slot(1).unwrap();
    step("slot 1", format!("{:?} {} {}", s1.state, s1.amount_msat, s1.opening == opening));
    let before = c.get_pub(2).unwrap();
    let after = c.gen_key_overwrite(2);
    step(
        "GEN_KEY overwrite of a KEYED slot",
        format!(
            "{} new key {}",
            sw(after.as_ref().map(|_| ()).map_err(|e| anyhow::anyhow!("{e:#}"))),
            after.map(|k| k != before).unwrap_or(false)
        ),
    );
    // The PIN session survives the commands between (only SELECT and a try end it).
    let msg = [0x5Au8; 32];
    let sig = c.sign(0, &msg);
    let ok = sig.as_ref().map(|s| SECP256K1.verify_schnorr(s, &Message::from_digest(msg), &x0).is_ok()).unwrap_or(false);
    step("SIGN", format!("{} verifies {ok}", sw(sig.map(|_| ()))));
    let again = c.sign(0, &msg);
    let ok = again.as_ref().map(|s| SECP256K1.verify_schnorr(s, &Message::from_digest(msg), &x0).is_ok()).unwrap_or(false);
    step("SIGN the same hash again", format!("{} verifies {ok}", sw(again.map(|_| ()))));
    step("SIGN another hash", sw(c.sign(0, &[0x5B; 32])));
    let s0 = c.get_slot(0).unwrap();
    step("slot 0 after SIGN", format!("{:?} spent hash is the message {}", s0.state, s0.spent_hash == msg));
    // Slot 0 is spent and the PIN is held, which would let any other slot re-key; the certificate
    // names slot 0's key, so its re-key form is refused before anything is written.
    step("GEN_KEY overwrite of the certified slot 0, spent, with the PIN", sw(c.gen_key_overwrite(0)));
    let s0 = c.get_slot(0).unwrap();
    step("slot 0 after the refusal: state, same key", format!("{:?} {}", s0.state, c.get_pub(0).unwrap() == x0));
    let raw = c.apdu(&v3_apdu(v3::INS_GET_STATE, 0, &[])).unwrap();
    step("GET_STATE bytes", raw.len().to_string());
    let st = state(c);
    step("GET_STATE slots 0..3", format!("{:?}", &st.states[..3]));

    // CHANGE_PIN: P1 is the old PIN's length; it needs this session's PIN and ends it.
    step("CHANGE_PIN", sw(change_pin_sealed(c, &t, PIN, NEW_PIN)));
    step("SIGN after CHANGE_PIN", sw(c.sign(0, &msg)));
    step("CHANGE_PIN without the session", sw(change_pin_sealed(c, &t, NEW_PIN, PIN)));
    step("VERIFY_PIN the old PIN", held(c, &t, PIN));
    step("VERIFY_PIN the new PIN", held(c, &t, NEW_PIN));
    let st = state(c);
    step("at the end: pin, wrong, owed", format!("{} {} {}", st.pin_set, st.wrong_tries, st.owed));
    step("GET_STATE at the end", format!("{st:?}"));
    log
}

/// What the applet answers (FedimintCardApplet.java), step for step; the bridge run of
/// `every_4_0_path_on_the_bridge_matches_the_soft_card` gave the same.
const EXPECTED: &[&str] = &[
    "select: 9000",
    "version: Some(AppletVersion { major: 4, minor: 0 })",
    "GET_STATE fresh, bytes: 65",
    "fresh: pin, anchored, registry: false false false",
    "GET_NONCE before the certificate: 6985",
    "SET_CERT: 9000",
    "SET_PIN with P1 00: refused unsent",
    "SET_PIN before the anchor: 6A96",
    "SET_HEADER weak, before any anchor: 6A93",
    "SET_HEADER anchor: 1749188499",
    "SET_PIN before the registry key: 6985",
    "SET_REGISTRY_KEY: 9000",
    "SET_HEADER weak, after the anchor: 6A93",
    "GEN_KEY 0 again: 6985",
    "SET_PIN: 9000",
    "SET_PIN again: 6985",
    "after SET_PIN: pin, anchored, wrong, owed: true true 0 0",
    "SET_NOTE signed by another key: 6982",
    "SET_NOTE over another amount: 6982",
    "slot 0 after the refusals: Keyed",
    "SET_NOTE: 9000",
    "slot 0: Note 1000",
    "SET_NOTE slot 3: 9000",
    "GEN_KEY overwrite of a NOTE slot: 6985",
    "slot 3 after the refusal: Note true",
    "select again: 9000",
    "SET_CHANGE without the PIN: 6A94",
    "SIGN without the PIN: 6A94",
    "GEN_KEY overwrite without the PIN: 6A94",
    "VERIFY_PIN right: 9000 after 0 waits",
    "VERIFY_PIN wrong: 63C1",
    "after one wrong: wrong, owed: 1 1",
    "VERIFY_PIN wrong, held: 63C2 after 1 waits",
    "SIGN after a wrong PIN: 6A94",
    "VERIFY_PIN right, held: 9000 after 2 waits",
    "after the right PIN: wrong, owed: 0 0",
    "SET_CHANGE: 9000",
    "slot 1: Change 2000 true",
    "GEN_KEY overwrite of a KEYED slot: 9000 new key true",
    "SIGN: 9000 verifies true",
    "SIGN the same hash again: 9000 verifies true",
    "SIGN another hash: 6985",
    "slot 0 after SIGN: Spent spent hash is the message true",
    "GEN_KEY overwrite of the certified slot 0, spent, with the PIN: 6985",
    "slot 0 after the refusal: state, same key: Spent true",
    "GET_STATE bytes: 65",
    "GET_STATE slots 0..3: [Spent, Change, Keyed]",
    "CHANGE_PIN: 9000",
    "SIGN after CHANGE_PIN: 6A94",
    "CHANGE_PIN without the session: 6A94",
    "VERIFY_PIN the old PIN: 63C1 after 0 waits",
    "VERIFY_PIN the new PIN: 9000 after 1 waits",
    "at the end: pin, wrong, owed: true 0 0",
    "GET_STATE at the end: CardStatus { states: [Spent, Change, Keyed, Note, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty], now: 1749188499, window: 1749188499, limit_msat: 0, spent_msat: 1000, cert_len: 130, attest_key: true, registry_key: true, pin_set: true, wrong_tries: 0, owed: 0, anchored: false, chip_mul: false, key_in_ram: false }",
];

#[test]
fn every_4_0_path_on_the_soft_card() {
    let log = paths(&mut CardClient::new(Soft(SoftCard::v4())));
    assert_eq!(log, EXPECTED);
}

#[test]
fn every_4_0_path_on_the_bridge_matches_the_soft_card() {
    let Ok(url) = std::env::var("CARD_BRIDGE_URL") else {
        eprintln!("CARD_BRIDGE_URL not set: skipping bridge test");
        return;
    };
    let t = BridgeTransport::new(&url);
    t.health().expect("bridge /health");
    t.reset().expect("bridge /reset");
    let bridge = paths(&mut CardClient::new(t));
    let soft = paths(&mut CardClient::new(Soft(SoftCard::v4())));
    for (i, (b, s)) in bridge.iter().zip(&soft).enumerate() {
        eprintln!("{} {i:2} {b}", if b == s { "  " } else { "!!" });
        if b != s {
            eprintln!("      soft: {s}");
        }
    }
    assert_eq!(bridge, soft, "the applet and the SoftCard answered differently");
}
