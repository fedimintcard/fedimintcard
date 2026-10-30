//! Step 2: generate a key on the card, get a signature, and let `secp256k1` verify it.
//!
//! * SoftCard and APDU framing (`CardClient` over an in-memory APDU responder) always run.
//! * The bridge test runs only when `CARD_BRIDGE_URL` is set.
//! * `card_from_env` runs against whatever `CARD_BACKEND` selects (soft by default).

use fedimint_card_host_card::*;
use secp256k1::{Message, SECP256K1, XOnlyPublicKey};

/// Tests that touch the shared bridge card (one resets it, one uses it) must not interleave.
static BRIDGE_CARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn bridge_lock() -> std::sync::MutexGuard<'static, ()> {
    BRIDGE_CARD.lock().unwrap_or_else(|e| e.into_inner())
}

/// Provision a PIN on a card that has none, then open a session with it: `SIGN` needs both, as
/// on the applet.
fn open_session(card: &mut dyn Card) {
    const PIN: &[u8] = b"1234";
    if status_word(&card.verify_pin(PIN).unwrap_err()) == Some(SW_NO_PIN_SET) {
        card.set_pin(PIN).expect("SET_PIN on a fresh card");
    }
    card.verify_pin(PIN).expect("VERIFY_PIN");
}

fn roundtrip(card: &mut dyn Card, slot: u8) {
    let pk = card.get_or_gen(slot).expect("key");
    // get_or_gen runs before the session so a fresh card can still be provisioned: GEN_KEY on an
    // empty slot needs no PIN, and SIGN (which does) comes after.
    open_session(card);
    assert_eq!(card.get_pub(slot).unwrap(), pk, "GET_PUB is stable");
    let msg = [0x5Au8; 32];
    let sig = card.sign(slot, &msg).expect("sign");
    SECP256K1
        .verify_schnorr(&sig, &Message::from_digest(msg), &pk)
        .expect("signature verifies against the card's key");
    // A different message must not verify under the same signature.
    assert!(
        SECP256K1
            .verify_schnorr(&sig, &Message::from_digest([0x5B; 32]), &pk)
            .is_err()
    );
}

#[test]
fn soft_card_roundtrip() {
    roundtrip(&mut SoftCard::new(), 0);
}

/// `SIGN` is what spends a note, so the stand-in must refuse it exactly as the applet does:
/// nothing before a PIN exists, nothing in a selection that has not verified, and nothing once
/// the tries are gone — the applet's three status words, in the applet's precedence.
#[test]
fn soft_card_sign_needs_a_pin_in_this_session() {
    const MSG: [u8; 32] = [0x42; 32];
    let mut c = SoftCard::new();
    c.gen_key(0).unwrap();

    assert_eq!(status_word(&c.sign(0, &MSG).unwrap_err()), Some(SW_NO_PIN_SET), "no PIN yet");
    c.set_pin(b"1234").unwrap();
    assert_eq!(status_word(&c.sign(0, &MSG).unwrap_err()), Some(SW_PIN_REQUIRED), "not verified");
    c.verify_pin(b"1234").unwrap();
    c.sign(0, &MSG).expect("verified");

    // A session is a selection: the applet clears the flag on SELECT.
    assert_eq!(c.apdu(&SELECT_APDU).unwrap(), Vec::<u8>::new(), "SELECT answers no data");
    assert_eq!(status_word(&c.sign(0, &MSG).unwrap_err()), Some(SW_PIN_REQUIRED), "after SELECT");

    // A wrong PIN leaves nothing open. The first two spend a try and say how many are left; the
    // third leaves none, so the card is blocked from then on.
    for left in [2, 1] {
        let e = c.verify_pin(b"9999").unwrap_err();
        assert_eq!(status_word(&e), Some(0x6300 | left), "tries left in the status word");
        assert_eq!(status_word(&c.sign(0, &MSG).unwrap_err()), Some(SW_PIN_REQUIRED));
    }
    assert_eq!(status_word(&c.verify_pin(b"9999").unwrap_err()), Some(0x6300), "no tries left");
    assert_eq!(status_word(&c.sign(0, &MSG).unwrap_err()), Some(SW_PIN_BLOCKED), "terminal");
    assert_eq!(status_word(&c.verify_pin(b"1234").unwrap_err()), Some(SW_PIN_BLOCKED), "no unblock");
}

/// An overwrite destroys the key the slot's notes are locked to, so it takes the same PIN as
/// spending them — but only once the card has a PIN: before SET_PIN there is nothing to spend.
#[test]
fn soft_card_overwrite_needs_the_pin_only_once_one_is_set() {
    let mut c = SoftCard::new();
    c.gen_key(0).unwrap();
    c.gen_key_overwrite(0).expect("no PIN yet, provisioning may re-key");
    c.set_pin(b"1234").unwrap();
    assert_eq!(status_word(&c.gen_key_overwrite(0).unwrap_err()), Some(SW_PIN_REQUIRED));
    c.verify_pin(b"1234").unwrap();
    c.gen_key_overwrite(0).expect("verified");
}

#[test]
fn soft_card_slot_semantics() {
    let mut c = SoftCard::new();
    assert_eq!(status_word(&c.get_pub(0).unwrap_err()), Some(0x6985), "empty slot");
    let a = c.gen_key(0).unwrap();
    assert_eq!(status_word(&c.gen_key(0).unwrap_err()), Some(0x6985), "no overwrite");
    let b = c.gen_key(1).unwrap();
    assert_ne!(a, b);
    assert_eq!(status_word(&c.gen_key(8).unwrap_err()), Some(0x6B00), "slot out of range");
    assert_eq!(status_word(&c.gen_key_overwrite(8).unwrap_err()), Some(0x6B00), "overwrite out of range");
    assert!(c.gen_key(7).is_ok(), "slot 7 exists");
}

#[test]
fn soft_card_overwrite() {
    let mut c = SoftCard::new();
    let e = c.gen_key_overwrite(3).unwrap();
    assert_eq!(c.get_pub(3).unwrap(), e, "overwrite on an empty slot works");
    let a = c.gen_key(0).unwrap();
    // The card now has a PIN, so an overwrite needs the PIN session it would need to spend.
    c.set_pin(b"1234").unwrap();
    assert_eq!(status_word(&c.gen_key_overwrite(0).unwrap_err()), Some(SW_PIN_REQUIRED));
    c.verify_pin(b"1234").unwrap();
    let b = c.gen_key_overwrite(0).unwrap();
    assert_ne!(a, b);
    assert_eq!(c.get_pub(0).unwrap(), b);
    let msg = [7u8; 32];
    let sig = c.sign(0, &msg).unwrap();
    let m = Message::from_digest(msg);
    assert!(SECP256K1.verify_schnorr(&sig, &m, &b).is_ok());
    assert!(SECP256K1.verify_schnorr(&sig, &m, &a).is_err(), "old key no longer signs");
    assert_eq!(status_word(&c.gen_key(0).unwrap_err()), Some(0x6985), "plain GEN_KEY still refuses");
}

/// Applet-shaped APDU responder over a `SoftCard`, to test `CardClient` framing offline.
struct FakeApplet {
    card: SoftCard,
    /// Deliver responses as `61xx` + GET RESPONSE, as T=0 cards do.
    chain: bool,
    pending: Option<Vec<u8>>,
    log: Vec<Vec<u8>>,
}

impl Transport for FakeApplet {
    fn transmit(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.log.push(apdu.to_vec());
        if apdu[1] == 0xC0 {
            let mut d = self.pending.take().expect("GET RESPONSE without pending data");
            d.extend_from_slice(&[0x90, 0x00]);
            return Ok(d);
        }
        if apdu[1] == 0xA4 {
            return Ok(vec![0x90, 0x00]);
        }
        assert_eq!(apdu[0], 0xB0, "CLA");
        let slot = apdu[2];
        if apdu[1] != 0x10 {
            assert_eq!(apdu[3], 0, "P2");
        }
        let res = match apdu[1] {
            0x10 => {
                assert_eq!(apdu.len(), 5, "GEN_KEY: header + Le only");
                match apdu[3] {
                    0 => self.card.gen_key(slot),
                    1 => self.card.gen_key_overwrite(slot),
                    _ => return Ok(vec![0x6B, 0x00]),
                }
                .map(|k| k.serialize().to_vec())
            }
            0x11 => {
                assert_eq!(apdu.len(), 5, "GET_PUB: header + Le only");
                self.card.get_pub(slot).map(|k| k.serialize().to_vec())
            }
            0x12 => {
                assert_eq!(apdu[4], 0x20, "Lc");
                assert_eq!(apdu.len(), 5 + 32 + 1, "SIGN: header, 32 bytes, Le");
                let m: [u8; 32] = apdu[5..37].try_into().unwrap();
                self.card.sign(slot, &m).map(|s| s.serialize().to_vec())
            }
            // The PIN commands the roundtrip needs to open a session. Forwarded whole to the
            // card's own APDU layer, so the status words are the applet's, not a re-derivation.
            0x40 | 0x41 => {
                assert_eq!(apdu.len(), 5 + apdu[4] as usize + 1, "PIN command: Lc bytes, Le");
                self.card.apdu(apdu)
            }
            _ => return Ok(vec![0x6D, 0x00]),
        };
        match res {
            Ok(data) if self.chain => {
                let n = data.len() as u8;
                self.pending = Some(data);
                Ok(vec![0x61, n])
            }
            Ok(mut data) => {
                data.extend_from_slice(&[0x90, 0x00]);
                Ok(data)
            }
            Err(e) => Ok(status_word(&e).unwrap().to_be_bytes().to_vec()),
        }
    }
}

fn fake(chain: bool) -> CardClient<FakeApplet> {
    CardClient::new(FakeApplet { card: SoftCard::new(), chain, pending: None, log: vec![] })
}

#[test]
fn client_apdu_framing_roundtrip() {
    let mut c = fake(false);
    c.select().unwrap();
    roundtrip(&mut c, 2);
    let log = &c.into_transport().log;
    assert_eq!(log[0], SELECT_APDU);
    assert!(log.iter().any(|a| a[1] == 0x12), "SIGN was sent");
}

#[test]
fn client_follows_get_response() {
    let mut c = fake(true);
    roundtrip(&mut c, 1);
    assert!(c.into_transport().log.iter().any(|a| a[1] == 0xC0));
}

#[test]
fn client_gen_key_overwrite_sends_p2_1() {
    let mut c = fake(false);
    let a = c.gen_key(5).unwrap();
    let b = c.gen_key_overwrite(5).unwrap();
    assert_ne!(a, b);
    assert_eq!(c.get_pub(5).unwrap(), b);
    let log = c.into_transport().log;
    assert_eq!(log[0], [0xB0, 0x10, 5, 0, 0], "plain GEN_KEY has P2 = 0");
    assert_eq!(log[1], [0xB0, 0x10, 5, 1, 0], "overwrite has P2 = 1");
    assert_eq!(log[2][3], 0, "GET_PUB keeps P2 = 0");
}

struct Wrong6C {
    log: Vec<Vec<u8>>,
}

impl Transport for Wrong6C {
    fn transmit(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.log.push(apdu.to_vec());
        Ok(vec![0x6C, 0x20])
    }
}

#[test]
fn overwrite_is_never_resent_on_6c() {
    let mut c = CardClient::new(Wrong6C { log: vec![] });
    let e = c.gen_key_overwrite(1).unwrap_err();
    assert_eq!(status_word(&e), Some(0x6C20));
    assert_eq!(c.into_transport().log.len(), 1, "no resend");
    // Other commands still follow 6Cxx.
    let mut c = CardClient::new(Wrong6C { log: vec![] });
    let _ = c.gen_key(1);
    let log = c.into_transport().log;
    assert!(log.len() > 1, "plain GEN_KEY is resent with the corrected Le");
    assert_eq!(*log[1].last().unwrap(), 0x20);
}

#[test]
fn client_surfaces_status_words() {
    let mut c = fake(false);
    let e = c.get_pub(0).unwrap_err();
    assert_eq!(status_word(&e), Some(0x6985));
    c.gen_key(0).unwrap();
    assert_eq!(status_word(&c.gen_key(0).unwrap_err()), Some(0x6985));
}

#[test]
fn bridge_roundtrip_when_configured() {
    let _guard = bridge_lock();
    let Ok(url) = std::env::var("CARD_BRIDGE_URL") else {
        eprintln!("CARD_BRIDGE_URL not set: skipping bridge test");
        return;
    };
    let t = BridgeTransport::new(&url);
    t.health().expect("bridge /health");
    t.reset().expect("bridge /reset");
    let mut c = CardClient::new(t);
    c.select().unwrap();
    // Overwrite rather than GEN_KEY: a real card behind the relay keeps its keys across /reset.
    let pk: XOnlyPublicKey = c.gen_key_overwrite(0).unwrap();
    assert_eq!(status_word(&c.gen_key(0).unwrap_err()), Some(0x6985));
    roundtrip(&mut c, 0);
    assert_eq!(c.get_pub(0).unwrap(), pk);
}

#[test]
fn card_from_env() {
    let _guard = bridge_lock();
    let mut card = open_from_env().expect("backend from CARD_BACKEND");
    roundtrip(card.as_mut(), 3);
}

/// Scripted transport: returns canned responses (the last one forever) and records what was sent.
struct Script {
    replies: std::collections::VecDeque<Vec<u8>>,
    sent: Vec<Vec<u8>>,
}

impl Transport for Script {
    fn transmit(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.sent.push(apdu.to_vec());
        Ok(if self.replies.len() > 1 { self.replies.pop_front().unwrap() } else { self.replies[0].clone() })
    }
}

fn script(replies: Vec<Vec<u8>>) -> CardClient<Script> {
    CardClient::new(Script { replies: replies.into(), sent: vec![] })
}

#[test]
fn client_resends_with_corrected_le_on_6c() {
    let key = SoftCard::new().gen_key(0).unwrap().serialize();
    let mut ok = key.to_vec();
    ok.extend_from_slice(&[0x90, 0x00]);
    let mut c = script(vec![vec![0x6C, 0x20], ok]);
    assert_eq!(c.get_pub(0).unwrap().serialize(), key);
    let sent = c.into_transport().sent;
    assert_eq!(sent, vec![vec![0xB0, 0x11, 0x00, 0x00, 0x00], vec![0xB0, 0x11, 0x00, 0x00, 0x20]]);
}

#[test]
fn client_gives_up_on_endless_61_and_6c() {
    let mut c = script(vec![vec![0x61, 0x20]]);
    let e = c.get_pub(0).unwrap_err();
    assert!(e.to_string().contains("61xx/6Cxx"), "{e}");
    assert_eq!(c.into_transport().sent.len(), 1 + 16, "first send plus at most 16 follow-ups");

    let mut c = script(vec![vec![0x6C, 0x20]]);
    assert!(c.get_pub(0).is_err());
    assert_eq!(c.into_transport().sent.len(), 1 + 16);
}
