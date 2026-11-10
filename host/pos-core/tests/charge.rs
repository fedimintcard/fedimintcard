//! The offline charge on a phone (docs/POS-OFFLINE.md): a 3.0 SoftCard behind an APDU transport,
//! an offline kit, and the phone's store, with no federation and no network.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use common::{DAY_MS, Fx, now_ms, slots_in, spent_hashes};
use fedimint_card_host_card::softkit::{PullTransport, SoftMint, SoftTransport};
use fedimint_card_host_card::{BridgeError, Card, CardClient, SlotState, TEST_PIN, Transport, open_pin_session_with};
use fedimint_card_pos_core::charge::{Env, Pin, Taken, TillOuts, charge};
use fedimint_card_pos_core::queue::{ChargeState, QUEUE_FILE, Queue};
use fedimint_card_pos_core::refusal;
use fedimint_card_pos_core::store::Store;
use fedimint_core::encoding::Decodable;
use fedimint_core::secp256k1::hashes::Hash as _;
use fedimint_core::secp256k1::{Message, SECP256K1, XOnlyPublicKey};
use fedimint_core::TransactionId;
use fedimint_core::transaction::{Transaction, TransactionSignature};
use serde_json::{Value, json};

/// Mainnet block 900,200's header (as `card/tests/v3_softcard.rs`), and its time, 2025-06-07.
const H900200: &str = "0060002058d02283d148685d2413fdf9a66317225589b86a56e2000000000000000000002c29575cba6c0fc3b8e366bf820d4d526ba300cd8915ee4ac075a32aafa6e9302e59446874370217546eb104";
const T: u64 = 1_749_309_742;
/// The genesis block: a real header, with far less work than the card's floor.
const GENESIS: &str = "0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4a29ab5f49ffff001d1dac2b7c";

/// The 32 bytes a card signs for a transaction (`SIGN`'s message, a SPENT slot's hash).
fn digest(txid: &str) -> [u8; 32] {
    use std::str::FromStr as _;
    TransactionId::from_str(txid).unwrap().to_byte_array()
}

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path()).unwrap();
    (dir, s)
}

fn client<T: Transport>(t: T) -> CardClient<T> {
    let mut c = CardClient::new(t);
    let _ = c.select();
    c
}

fn take(s: &Store, card: &SoftTransport, kit: &str, price: u64, cref: &str) -> anyhow::Result<Taken> {
    s.charge(&mut client(card.clone()), kit, price, cref, b"1234")
}

/// The card's clock (`GET_STATE`), seconds.
fn clock_of(card: &SoftTransport) -> u64 {
    u64::from(card.0.lock().unwrap().get_state().unwrap().now)
}

/// The card leaves the field at its `at`th SIGN (from 0): that SIGN and every APDU after it fail.
struct PullAtSign {
    inner: SoftTransport,
    at: usize,
    signs: usize,
    gone: bool,
}

impl PullAtSign {
    fn new(card: &SoftTransport, at: usize) -> Self {
        Self { inner: card.clone(), at, signs: 0, gone: false }
    }
}

impl Transport for PullAtSign {
    fn transmit(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        if apdu.starts_with(&[0xB0, 0x12]) {
            self.gone |= self.signs == self.at;
            self.signs += 1;
        }
        if self.gone {
            anyhow::bail!(BridgeError::NoCard("the card left the field".into()));
        }
        self.inner.transmit(apdu)
    }
}

fn code(r: &anyhow::Result<Taken>) -> &'static str {
    match r {
        Ok(_) => "ok",
        Err(e) => refusal(e).code,
    }
}

/// APDUs a charge sends to the card, counted.
fn take_counted(s: &Store, card: &SoftTransport, kit: &str, price: u64, cref: &str, pin: &[u8]) -> (anyhow::Result<Taken>, usize) {
    let n = Arc::new(Mutex::new(0));
    let mut c = CardClient::new(common::Counted { inner: card.clone(), n: n.clone() });
    let r = s.charge(&mut c, kit, price, cref, pin);
    let n = *n.lock().unwrap();
    (r, n)
}

/// The till's view of a charge: the signed bytes decode, each input's signature verifies over
/// the txid under its note's key, and each output is a kit pool entry or a change opening.
fn check_signed(c: &fedimint_card_pos_core::queue::OfflineCharge, kit: &fedimint_card_pos_core::kit::Kit) {
    let decoders = fedimint_card_pos_core::charge::mint_decoders(kit.mint_instance_id);
    let tx = Transaction::consensus_decode_hex(c.signed_hex.as_deref().unwrap(), &decoders).unwrap();
    assert_eq!(tx.tx_hash().to_string(), c.txid);
    let TransactionSignature::NaiveMultisig(sigs) = &tx.signatures else { panic!("multisig") };
    assert_eq!(sigs.len(), c.inputs.len());
    let msg = Message::from_digest(tx.tx_hash().to_byte_array());
    for (sig, input) in sigs.iter().zip(&c.inputs) {
        let key: XOnlyPublicKey = input.nonce[2..].parse().unwrap();
        SECP256K1.verify_schnorr(sig, &msg, &key).expect("the card's signature");
    }
    let pool: Vec<&String> = kit.pool.iter().flat_map(|t| &t.blinded).collect();
    assert_eq!(c.pool.len() + c.change.len(), tx.outputs.len());
    for p in &c.pool {
        assert!(pool.contains(&&p.blinded), "{p:?} is a kit entry");
        assert!(p.out_idx < c.pool.len() as u64, "the till's outputs come first");
    }
    assert!(c.host.is_empty(), "a phone holds no till secret");
    assert_eq!(c.pool.iter().map(|p| p.amount_msat).sum::<u64>(), c.till_msat);
}

// ------------------------------------------------------------------ paid offline

#[test]
fn a_card_pays_offline_from_a_kit() {
    for fees in [true, false] {
        let fx = Fx::new(fees);
        let (_dir, s) = store();
        let card = fx.card(32, &[1 << 16, 1 << 14]);
        let kit = fx.kit(s.phone_id(), 40, now_ms(), 7 * DAY_MS);
        let json = serde_json::to_string(&kit).unwrap();
        let t = take(&s, &card, &json, 50_000, "sale-1").unwrap();
        assert!(!t.replayed);
        let c = &t.charge;
        assert_eq!((c.state, c.price_msat, c.client_ref.as_deref()), (ChargeState::Queued, 50_000, Some("sale-1")));
        assert!(c.till_msat >= 50_000);
        check_signed(c, &kit);
        // The inputs are spent on the card, the change is owed to it.
        let spent: Vec<u8> = spent_hashes(&card)
            .iter()
            .map(|(s, h)| {
                assert_eq!(*h, digest(&c.txid), "slot {s} signed this charge");
                *s
            })
            .collect();
        assert_eq!(spent, c.inputs.iter().map(|i| i.slot).collect::<Vec<_>>());
        assert_eq!(slots_in(&card, SlotState::Change), c.change.iter().map(|ch| ch.slot).collect::<Vec<_>>());
        // On disk, as the till's queue, and counted as waiting.
        let st = s.stats();
        assert_eq!((st.waiting, st.waiting_msat), (1, c.till_msat));
        let q: Value = serde_json::from_slice(&std::fs::read(s.dir().join(QUEUE_FILE)).unwrap()).unwrap();
        assert_eq!(q["charges"][0]["state"], "queued");
        assert_eq!(q["charges"][0]["pool"].as_array().unwrap().len(), c.pool.len());
    }
}

/// The till's newest block header travels in the kit: the card's clock moves to it, and the charge
/// pays.
#[test]
fn a_kit_with_a_header_sets_the_cards_clock_and_pays() {
    let fx = Fx::new(true);
    let (_dir, s) = store();
    let card = fx.card(32, &[1 << 16]);
    assert_eq!(clock_of(&card), 0);
    let mut kit = fx.kit(s.phone_id(), 8, now_ms(), DAY_MS);
    kit.block_header = Some(H900200.into());
    let t = take(&s, &card, &serde_json::to_string(&kit).unwrap(), 20_000, "header").unwrap();
    assert_eq!(t.charge.state, ChargeState::Queued);
    assert_eq!(clock_of(&card), T, "the card's clock is the header's time");
    // A header that is not one refuses the kit before the card hears anything.
    for bad in [GENESIS, "00", "not hex"] {
        kit.block_header = Some(bad.into());
        let (r, apdus) = take_counted(&s, &card, &serde_json::to_string(&kit).unwrap(), 10_000, "bad-header", b"1234");
        assert_eq!((code(&r), apdus), ("offline_not_ready", 0), "{bad}");
    }
}

/// The kit's expiry, checked again by the card's clock once the card has seen the kit's header:
/// a phone whose clock is ten days behind takes no charge with a kit the card's clock says has
/// expired, and nothing is signed. By a card clock short of the expiry, the same charge pays.
#[test]
fn the_cards_clock_rechecks_the_kits_expiry() {
    let fx = Fx::new(true);
    let dir = tempfile::tempdir().unwrap();
    let queue = Mutex::new(Queue::open(dir.path()).unwrap());
    let phone = "ab".repeat(16);
    let kit = fx.kit(&phone, 8, (T - 20 * 86_400) * 1_000, 21 * DAY_MS);
    let keys = kit.decode(&phone).unwrap();
    let header: [u8; 80] = hex::decode(H900200).unwrap().try_into().unwrap();
    let env = |expires_at_ms: u64| Env {
        mint_id: kit.mint_instance_id,
        tbs_pks: &keys.tbs_pks,
        fee: &keys.fee,
        surplus_allowed: kit.surplus_allowed,
        decoders: &keys.decoders,
        cert_key: Some(&kit.cert_key),
        cfg: keys.cfg,
        header: Some(header),
        pin: Pin::Verify(b"1234".to_vec()),
        till_outs: TillOuts::Pool(&keys.pool),
        keyed_ok: None,
        now_ms: T * 1_000 - 10 * DAY_MS,
        expires_at_ms: Some(expires_at_ms),
    };
    let card = fx.card(32, &[1 << 16]);
    let r = charge(&mut client(card.clone()), &queue, &env(T * 1_000 - DAY_MS), 20_000, Some("late-by-the-card"));
    assert_eq!(code(&r), "offline_kit_expired", "{:?}", r.err());
    assert_eq!(clock_of(&card), T, "the card saw the header first");
    assert!(slots_in(&card, SlotState::Spent).is_empty(), "nothing signed");
    assert!(queue.lock().unwrap().file.charges.is_empty());
    let t = charge(&mut client(card.clone()), &queue, &env(T * 1_000 + DAY_MS), 20_000, Some("in-time")).unwrap();
    assert_eq!(t.charge.state, ChargeState::Queued);
}

/// The kit as the till answers it, `{"ok":true,"kit":…}`, is a kit too.
#[test]
fn the_tills_whole_answer_is_taken_as_the_kit() {
    let fx = Fx::new(true);
    let (_dir, s) = store();
    let card = fx.card(32, &[1 << 16]);
    let answer = serde_json::json!({"ok": true, "kit": fx.kit(s.phone_id(), 8, now_ms(), DAY_MS)}).to_string();
    assert!(take(&s, &card, &answer, 20_000, "whole").is_ok());
}

// ------------------------------------------------------------------ refusals

#[test]
fn an_expired_kit_is_refused_before_the_card_hears_anything() {
    let fx = Fx::new(true);
    let (_dir, s) = store();
    let card = fx.card(32, &[1 << 16]);
    let old = serde_json::to_string(&fx.kit(s.phone_id(), 8, now_ms() - 8 * DAY_MS, 7 * DAY_MS)).unwrap();
    let (r, apdus) = take_counted(&s, &card, &old, 20_000, "late", b"1234");
    assert_eq!((code(&r), apdus), ("offline_kit_expired", 0));
    // A phone clock set back is no help: the store has seen a later time.
    let good = serde_json::to_string(&fx.kit(s.phone_id(), 8, now_ms(), DAY_MS)).unwrap();
    take(&s, &card, &good, 20_000, "now").unwrap();
    let st = std::fs::read(s.dir().join(QUEUE_FILE)).unwrap();
    let hw = serde_json::from_slice::<Value>(&st).unwrap()["high_water_ms"].as_u64().unwrap();
    assert!(hw >= now_ms() - 60_000);
    // A kit that is young by its own issue time but which the store has seen expire.
    let mut stale = fx.kit(s.phone_id(), 8, hw - 2 * DAY_MS, DAY_MS);
    stale.issued_at_ms = hw - 2 * DAY_MS;
    let (r, apdus) = take_counted(&s, &card, &serde_json::to_string(&stale).unwrap(), 20_000, "stale", b"1234");
    assert_eq!((code(&r), apdus), ("offline_kit_expired", 0));
}

#[test]
fn a_kit_for_another_phone_or_garbled_is_not_ready() {
    let fx = Fx::new(true);
    let (_dir, s) = store();
    let card = fx.card(32, &[1 << 16]);
    let other = serde_json::to_string(&fx.kit(&"ab".repeat(16), 8, now_ms(), DAY_MS)).unwrap();
    for kit in [other.as_str(), "{}", "not json"] {
        let (r, apdus) = take_counted(&s, &card, kit, 20_000, "k", b"1234");
        assert_eq!((code(&r), apdus), ("offline_not_ready", 0), "{kit}");
    }
}

#[test]
fn the_caps_are_kept() {
    let fx = Fx::new(true);
    let (_dir, s) = store();
    let card = fx.card(32, &[1 << 16, 1 << 16, 1 << 16]);
    let mut kit = fx.kit(s.phone_id(), 40, now_ms(), DAY_MS);
    kit.cap_count = 1;
    kit.cap_msat = 60_000;
    let json = serde_json::to_string(&kit).unwrap();
    // Over the value cap: refused once the plan says what the till would get.
    assert_eq!(code(&take(&s, &card, &json, 70_000, "big")), "offline_cap_reached");
    take(&s, &card, &json, 20_000, "one").unwrap();
    // Over the count cap: refused before the card is asked anything.
    let (r, apdus) = take_counted(&s, &card, &json, 10_000, "two", b"1234");
    assert_eq!((code(&r), apdus), ("offline_cap_reached", 0));
    assert_eq!(s.stats().waiting, 1);
}

/// A kit out of the 32,768 msat tier: a price that needs one is refused with prices the pool
/// still pays, and one of them pays.
#[test]
fn a_pool_out_of_a_tier_refuses_with_a_price_it_pays() {
    let fx = Fx::new(true);
    let (_dir, s) = store();
    let card = fx.card(32, &[1 << 20]);
    let mut kit = fx.kit(s.phone_id(), 40, now_ms(), DAY_MS);
    kit.pool.retain(|t| t.amount_msat != 1 << 15);
    let json = serde_json::to_string(&kit).unwrap();
    let r = take(&s, &card, &json, 50_000, "needs-32k");
    let e = r.as_ref().err().expect("refused");
    let f = refusal(e);
    assert_eq!((f.status, f.code), (409, "offline_pool_exhausted"), "{}", f.detail);
    let suggested = f.suggested_msat.expect("a price the pool pays");
    assert!(f.below_msat == Some(suggested) || f.above_msat == Some(suggested));
    assert_eq!(slots_in(&card, SlotState::Spent), Vec::<u8>::new(), "nothing signed");
    let t = take(&s, &card, &json, suggested, "suggested").unwrap();
    assert!(t.charge.pool.iter().all(|p| p.amount_msat != 1 << 15));
}

/// No KEYED slot for change: the card pays only prices it can pay exactly.
#[test]
fn no_room_for_change_offline_is_said_with_prices_the_card_pays() {
    let fx = Fx::new(true);
    let (_dir, s) = store();
    // Two keyed slots, both holding notes.
    let card = fx.card(2, &[1 << 16, 1 << 14]);
    let json = fx.kit_json(s.phone_id());
    let r = take(&s, &card, &json, 50_000, "needs-change");
    let f = refusal(r.as_ref().err().unwrap());
    assert_eq!(f.code, "offline_no_change_room", "{}", f.detail);
    let p = f.suggested_msat.expect("a price");
    let t = take(&s, &card, &json, p, "exact").unwrap();
    assert!(t.charge.change.is_empty());
}

#[test]
fn a_note_that_does_not_check_is_refused_as_invalid() {
    let fx = Fx::new(true);
    let (_dir, s) = store();
    // A note from another mint: the kit's keys do not verify it.
    let card = fx.card(32, &[]);
    SoftMint::powers_of_two(32).load(&mut card.0.lock().unwrap(), 0, 1 << 16).unwrap();
    let r = take(&s, &card, &fx.kit_json(s.phone_id()), 20_000, "forged");
    let f = refusal(r.as_ref().err().unwrap());
    assert_eq!((f.status, f.code), (403, "offline_note_invalid"), "{}", f.detail);
    assert!(slots_in(&card, SlotState::Spent).is_empty());
}

#[test]
fn a_clone_without_the_certified_attestation_key_is_refused() {
    let fx = Fx::new(true);
    let (_dir, s) = store();
    let card = fx.card(32, &[1 << 16]);
    let clone = SoftTransport::new(card.0.lock().unwrap().extract_clone());
    let r = take(&s, &clone, &fx.kit_json(s.phone_id()), 20_000, "clone");
    assert_eq!(code(&r), "card_not_genuine");
    assert!(slots_in(&clone, SlotState::Spent).is_empty());
}

#[test]
fn a_wrong_pin_says_the_tries_left_and_a_malformed_one_spends_none() {
    let fx = Fx::new(true);
    let (_dir, s) = store();
    let card = fx.card(32, &[1 << 16]);
    let json = fx.kit_json(s.phone_id());
    // Malformed: no APDU at all, so no try spent.
    for (cref, pin) in [("", &b"1234"[..]), (&"x".repeat(129)[..], b"1234"), ("é", b"1234"), ("ok", b"12a4"), ("ok", b"123")] {
        let (r, apdus) = take_counted(&s, &card, &json, 20_000, cref, pin);
        assert_eq!((code(&r), apdus), ("bad_request", 0), "{cref:?} {pin:?}");
    }
    let (r, _) = take_counted(&s, &card, &json, 20_000, "pin", b"9999");
    let f = refusal(r.as_ref().err().unwrap());
    assert_eq!((f.code, f.tries_left), ("wrong_pin", Some(2)), "two tries left: none was spent before");
    let (r, _) = take_counted(&s, &card, &json, 20_000, "pin", b"9998");
    assert_eq!(refusal(r.as_ref().err().unwrap()).tries_left, Some(1));
    assert!(take(&s, &card, &json, 20_000, "pin").is_ok(), "the right PIN pays and resets the tries");
}

// ------------------------------------------------------------------ replays

#[test]
fn a_repeat_is_the_same_charge_and_another_price_is_a_conflict() {
    let fx = Fx::new(true);
    let (_dir, s) = store();
    let card = fx.card(32, &[1 << 16, 1 << 16]);
    let json = fx.kit_json(s.phone_id());
    let first = take(&s, &card, &json, 30_000, "r").unwrap();
    let (again, apdus) = take_counted(&s, &card, &json, 30_000, "r", b"1234");
    let again = again.unwrap();
    assert!(again.replayed && again.charge.txid == first.charge.txid);
    assert_eq!(apdus, 0, "answered from the queue");
    assert_eq!(code(&take(&s, &card, &json, 31_000, "r")), "conflict");
    assert_eq!(s.charges().len(), 1);
}

// ------------------------------------------------------------------ a card taken away, a write lost

/// A successful charge's APDUs, counted.
fn apdus_of_a_charge(fx: &Fx, notes: &[u64], price: u64) -> usize {
    let (_dir, s) = store();
    let card = fx.card(32, notes);
    let (r, n) = take_counted(&s, &card, &fx.kit_json(s.phone_id()), price, "count", b"1234");
    r.unwrap();
    n
}

/// After whatever happened, a rerun of the sale pays it, and the card and the queue hold exactly
/// one signed transaction for it: each input slot spent once, by that transaction.
fn exactly_one_charge(s: &Store, card: &SoftTransport, json: &str, price: u64, cref: &str, what: &str) {
    let t = take(s, card, json, price, cref).unwrap_or_else(|e| panic!("{what}: the rerun: {e:#}"));
    let charges: Vec<_> = s.charges().into_iter().filter(|c| c.client_ref.as_deref() == Some(cref)).collect();
    assert_eq!(charges.len(), 1, "{what}: {charges:?}");
    let c = &charges[0];
    assert_eq!((c.state, &c.txid), (ChargeState::Queued, &t.charge.txid), "{what}");
    let spent = spent_hashes(card);
    assert_eq!(spent.iter().map(|(s, _)| *s).collect::<Vec<_>>(), c.inputs.iter().map(|i| i.slot).collect::<Vec<_>>(), "{what}");
    for (slot, h) in spent {
        assert_eq!(h, digest(&c.txid), "{what}: slot {slot} signed the queued transaction");
    }
}

#[test]
fn a_card_pulled_at_any_apdu_then_tapped_again_pays_once() {
    let fx = Fx::new(true);
    let (notes, price) = ([1u64 << 16, 1 << 16, 1 << 16], 90_000);
    let n = apdus_of_a_charge(&fx, &notes, price);
    assert!(n > 10, "{n}");
    for at in 0..n {
        let (_dir, s) = store();
        let card = fx.card(32, &notes);
        let json = fx.kit_json(s.phone_id());
        let r = s.charge(&mut client(PullTransport::new(card.clone(), at)), &json, price, "pulled", b"1234");
        assert!(r.is_err(), "pulled at APDU {at} of {n}, yet paid");
        // A sale left signing is listed for the app, which retries it only here.
        let signing = s.charges().iter().any(|c| c.state == ChargeState::Signing);
        assert_eq!(s.signing_refs(), if signing { vec!["pulled".to_string()] } else { vec![] }, "pulled at {at}");
        exactly_one_charge(&s, &card, &json, price, "pulled", &format!("pulled at {at}"));
        assert!(s.signing_refs().is_empty(), "pulled at {at}: finished");
    }
}

/// A charge a card left signing whose other input slot has since paid another transaction (or
/// been renewed and loaded again) can never be sent: the retap abandons it, frees its sale's
/// `client_ref`, keeps its pool entries used, and takes the sale from the notes the card has. Once
/// card_error on every later tap of the card, for good.
#[test]
fn a_charge_left_signing_that_can_never_be_sent_is_abandoned_and_the_retap_pays() {
    let fx = Fx::new(true);
    for rekeyed in [false, true] {
        let what = if rekeyed { "re-keyed" } else { "spent elsewhere" };
        let (_dir, s) = store();
        let card = fx.card(32, &[1 << 16, 1 << 16, 1 << 16, 1 << 16]);
        let json = fx.kit_json(s.phone_id());
        // 90,000 msat takes two notes; the card leaves at the second SIGN.
        let r = s.charge(&mut CardClient::new(PullAtSign::new(&card, 1)), &json, 90_000, "half", b"1234");
        assert_eq!(code(&r), "no_card", "{what}");
        let left = s.charges().remove(0);
        assert_eq!((left.state, left.inputs.len()), (ChargeState::Signing, 2), "{what}");
        let (signed, unsigned) = (left.inputs[0].slot, left.inputs[1].slot);
        assert_eq!(spent_hashes(&card), vec![(signed, digest(&left.txid))], "{what}");
        // Elsewhere (another phone, or the till online) the unsigned input pays another
        // transaction; then, perhaps, its slot is renewed and loaded again.
        {
            let mut c = card.0.lock().unwrap();
            open_pin_session_with(&mut *c, TEST_PIN).unwrap();
            c.sign(unsigned, &[7; 32]).unwrap();
            if rekeyed {
                c.gen_key_overwrite(unsigned).unwrap();
                fx.mint.load(&mut c, unsigned, 1 << 16).unwrap();
            }
        }
        let t = take(&s, &card, &json, 90_000, "half").unwrap_or_else(|e| panic!("{what}: {e:#}"));
        assert!(!t.replayed && t.charge.txid != left.txid, "{what}");
        let charges = s.charges();
        let gone = charges.iter().find(|c| c.txid == left.txid).unwrap();
        assert_eq!((gone.state, gone.client_ref.as_deref()), (ChargeState::Abandoned, None), "{what}");
        let detail = gone.detail.as_deref().unwrap_or_default();
        assert!(detail.contains(&format!("input slot {unsigned}")) && detail.contains("\"half\""), "{what}: {detail}");
        // Its pool entries are used for good, and the new charge took none of them.
        let q: Value = serde_json::from_slice(&std::fs::read(s.dir().join(QUEUE_FILE)).unwrap()).unwrap();
        let used: Vec<&str> = q["pool_used"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert!(!left.pool.is_empty() && left.pool.iter().all(|p| used.contains(&p.blinded.as_str())), "{what}");
        assert!(t.charge.pool.iter().all(|p| !used.contains(&p.blinded.as_str())), "{what}");
        // Nothing more was signed for the abandoned charge: no slot but the first holds its hash.
        let spent = spent_hashes(&card);
        assert_eq!(spent.iter().filter(|(_, h)| *h == digest(&left.txid)).map(|(s, _)| *s).collect::<Vec<_>>(), vec![signed], "{what}");
        assert!(spent.iter().filter(|(_, h)| *h == digest(&t.charge.txid)).count() == t.charge.inputs.len(), "{what}");
        // The sale is the new charge from now on; the abandoned one waits for nothing.
        let again = take(&s, &card, &json, 90_000, "half").unwrap();
        assert!(again.replayed && again.charge.txid == t.charge.txid, "{what}");
        let st = s.stats();
        assert_eq!((st.waiting, st.signing), (1, 0), "{what}");
        assert!(s.signing_refs().is_empty(), "{what}");
        assert_eq!(s.drain(50).txids, vec![t.charge.txid.clone()], "{what}");
    }
}

/// The first charge's transport: at its first GET_SLOT of slot `from` or later (a KEYED slot, for
/// change: after the charge has chosen its pool entries and before it saves them) it says so and
/// waits for `release`, at most `wait`; then it notes how many APDUs `other` has carried.
struct Gate {
    inner: SoftTransport,
    from: u8,
    entered: Option<mpsc::Sender<()>>,
    release: mpsc::Receiver<()>,
    wait: Duration,
    other: Arc<AtomicUsize>,
    other_then: Arc<Mutex<Option<usize>>>,
}

impl Transport for Gate {
    fn transmit(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        if apdu.len() > 2 && apdu[..2] == [0xB0, 0x15] && apdu[2] >= self.from {
            if let Some(tx) = self.entered.take() {
                tx.send(()).unwrap();
                let _ = self.release.recv_timeout(self.wait);
                *self.other_then.lock().unwrap() = Some(self.other.load(Ordering::SeqCst));
            }
        }
        self.inner.transmit(apdu)
    }
}

/// The second charge's transport: it counts its APDUs, and at its first SIGN (its pool entries
/// chosen and saved) lets the first charge go on.
struct Second {
    inner: SoftTransport,
    sent: Arc<AtomicUsize>,
    release: Option<mpsc::Sender<()>>,
}

impl Transport for Second {
    fn transmit(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.sent.fetch_add(1, Ordering::SeqCst);
        if apdu.starts_with(&[0xB0, 0x12]) {
            if let Some(tx) = self.release.take() {
                let _ = tx.send(());
            }
        }
        self.inner.transmit(apdu)
    }
}

/// Two charges on one store at once (an app's retry while the first still waits on its card):
/// the second waits for the first, so the two never plan from the same free pool entries.
#[test]
fn two_charges_at_once_on_one_store_take_disjoint_pool_entries() {
    let fx = Fx::new(true);
    let (_dir, s) = store();
    let (a, b) = (fx.card(32, &[1 << 16, 1 << 16]), fx.card(32, &[1 << 16, 1 << 16]));
    // Two entries a tier: both charges can pay, each from its own.
    let json = serde_json::to_string(&fx.kit(s.phone_id(), 2, now_ms(), DAY_MS)).unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let b_sent = Arc::new(AtomicUsize::new(0));
    let b_sent_then = Arc::new(Mutex::new(None));
    let gate = Gate {
        inner: a.clone(),
        from: 2,
        entered: Some(entered_tx),
        release: release_rx,
        wait: Duration::from_secs(2),
        other: b_sent.clone(),
        other_then: b_sent_then.clone(),
    };
    let second = Second { inner: b.clone(), sent: b_sent.clone(), release: Some(release_tx) };
    let (first, second) = std::thread::scope(|sc| {
        let (s, json) = (&s, &json);
        let first = sc.spawn(move || s.charge(&mut CardClient::new(gate), json, 90_000, "first", b"1234"));
        entered_rx.recv().unwrap();
        let second = sc.spawn(move || s.charge(&mut CardClient::new(second), json, 90_000, "second", b"1234"));
        (first.join().unwrap(), second.join().unwrap())
    });
    let (first, second) = (first.unwrap().charge, second.unwrap().charge);
    assert_eq!(*b_sent_then.lock().unwrap(), Some(0), "the second charge spoke to its card while the first was between its plan and its save");
    assert!(!first.pool.is_empty() && !second.pool.is_empty());
    let mine: Vec<&String> = first.pool.iter().map(|p| &p.blinded).collect();
    assert!(second.pool.iter().all(|p| !mine.contains(&&p.blinded)), "a pool entry taken twice");
    assert_eq!(s.stats().waiting, 2);
}

#[test]
fn a_write_lost_at_any_point_then_a_restart_pays_once() {
    let fx = Fx::new(true);
    let (notes, price) = ([1u64 << 16, 1 << 16, 1 << 16], 90_000);
    // Two writes for a charge: `signing`, then `queued`; the third is past it.
    for at in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let card = fx.card(32, &notes);
        let json = {
            let s = Store::open(dir.path()).unwrap();
            let json = fx.kit_json(s.phone_id());
            s.fail_write_after(Some(at));
            let r = take(&s, &card, &json, price, "lost-write");
            assert_eq!(r.is_ok(), at >= 2, "write {at}: {:?}", r.err());
            json
        };
        // The process ends; the next one opens the store from disk.
        let s = Store::open(dir.path()).unwrap();
        exactly_one_charge(&s, &card, &json, price, "lost-write", &format!("write {at} lost"));
    }
}

// ------------------------------------------------------------------ the store

#[test]
fn the_store_survives_a_restart_refuses_a_newer_queue_and_holds_its_lock() {
    let fx = Fx::new(true);
    let dir = tempfile::tempdir().unwrap();
    let card = fx.card(32, &[1 << 16]);
    let id = {
        let s = Store::open(dir.path()).unwrap();
        assert!(Store::open(dir.path()).is_err(), "a second open of the same store is refused");
        take(&s, &card, &fx.kit_json(s.phone_id()), 20_000, "kept").unwrap();
        s.phone_id().to_owned()
    };
    let s = Store::open(dir.path()).unwrap();
    assert_eq!(s.phone_id(), id, "the phone keeps its id");
    assert_eq!(s.stats().waiting, 1);
    assert_eq!(s.drain(10).txids.len(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for f in [QUEUE_FILE, "phone-id"] {
            assert_eq!(std::fs::metadata(dir.path().join(f)).unwrap().permissions().mode() & 0o777, 0o600, "{f}");
        }
    }
    drop(s);
    let mut q: Value = serde_json::from_slice(&std::fs::read(dir.path().join(QUEUE_FILE)).unwrap()).unwrap();
    q["version"] = 2.into();
    std::fs::write(dir.path().join(QUEUE_FILE), q.to_string()).unwrap();
    let e = Store::open(dir.path()).err().expect("a newer queue is refused");
    assert!(format!("{e:#}").contains("version 2"), "{e:#}");
}

#[test]
fn sync_marks_what_the_till_took_and_keeps_what_it_refused() {
    let fx = Fx::new(true);
    let (_dir, s) = store();
    let card = fx.card(32, &[1 << 16, 1 << 16, 1 << 16, 1 << 16]);
    let json = fx.kit_json(s.phone_id());
    let ids: Vec<String> = ["a", "b", "c"].iter().map(|r| take(&s, &card, &json, 20_000, r).unwrap().charge.txid).collect();
    let d = s.drain(50);
    let body: Value = serde_json::from_str(&d.body).unwrap();
    assert_eq!((body["phone_id"].as_str(), body["charges"].as_array().unwrap().len()), (Some(s.phone_id()), 3));
    assert_eq!(s.drain(2).txids, ids[..2].to_vec());
    let answer = serde_json::json!({"ok": true, "results": [
        {"txid": ids[0], "status": "queued"},
        {"txid": ids[1], "status": "refused", "code": "pool_entry_reused", "retry": false},
        {"txid": ids[2], "status": "refused", "code": "internal", "retry": true},
    ]});
    let r = s.mark_synced(&answer.to_string()).unwrap();
    assert_eq!((r.synced, r.refused, r.retry), (1, 1, 1));
    let st = s.stats();
    assert_eq!((st.waiting, st.synced, st.refused), (1, 1, 1));
    let d = s.drain(50);
    assert_eq!(d.txids, vec![ids[2].clone()]);
    assert_eq!(d.refused.iter().map(|r| (r.txid.as_str(), r.code.as_str())).collect::<Vec<_>>(), vec![(ids[1].as_str(), "pool_entry_reused")]);
    // A charge the till took is a replay, still; one it refused is not a sale to answer again.
    assert!(take(&s, &card, &json, 20_000, "a").unwrap().replayed);
    let (r, apdus) = take_counted(&s, &card, &json, 20_000, "b", b"1234");
    assert_eq!((code(&r), apdus), ("conflict", 0), "{:?}", r.err());
    // Only a refused one is dismissed.
    assert!(!s.dismiss(&ids[0]).unwrap());
    assert!(s.dismiss(&ids[1]).unwrap());
    assert_eq!(s.stats().refused, 0);
    assert!(s.mark_synced(r#"{"ok": false, "error": "unauthorized"}"#).is_err());
    assert!(s.mark_synced("garbage").is_err());
}

#[test]
fn a_dismissed_charges_pool_entries_are_never_taken_again() {
    let fx = Fx::new(true);
    let (_dir, s) = store();
    let card = fx.card(32, &[1 << 16, 1 << 16]);
    // Two entries a tier, the first of each taken first: a second charge of the same price pays
    // from the other one, never from an entry the dismissed charge took.
    let kit = fx.kit(s.phone_id(), 2, now_ms(), DAY_MS);
    let json = serde_json::to_string(&kit).unwrap();
    let first = take(&s, &card, &json, 20_000, "first").unwrap().charge;
    assert!(!first.pool.is_empty());
    let answer = serde_json::json!({"ok": true, "results": [{"txid": first.txid, "status": "refused", "code": "pool_entry_unknown"}]});
    s.mark_synced(&answer.to_string()).unwrap();
    assert!(s.dismiss(&first.txid).unwrap());
    let t = take(&s, &card, &json, 20_000, "second").unwrap();
    assert_eq!(t.charge.pool.len(), first.pool.len(), "the same tiers");
    let used: Vec<&String> = first.pool.iter().map(|p| &p.blinded).collect();
    assert!(t.charge.pool.iter().all(|p| !used.contains(&&p.blinded)), "a dismissed charge's entries reused");
}

/// The phone's clock at a sync decides only what is old enough to leave the queue: a clock set a
/// year ahead once, then put right, leaves every kit good.
#[test]
fn a_clock_set_ahead_at_a_sync_does_not_expire_the_kits() {
    let fx = Fx::new(true);
    let (_dir, s) = store();
    let card = fx.card(32, &[1 << 16, 1 << 16]);
    let json = fx.kit_json(s.phone_id());
    let t = take(&s, &card, &json, 20_000, "before").unwrap();
    let high_water = || serde_json::from_slice::<Value>(&std::fs::read(s.dir().join(QUEUE_FILE)).unwrap()).unwrap()["high_water_ms"].as_u64();
    let kept = high_water();
    assert!(kept.is_some_and(|h| h <= now_ms()), "a charge keeps the time it was taken: {kept:?}");
    let answer = json!({"ok": true, "results": [{"txid": t.charge.txid, "status": "queued"}]});
    assert_eq!(s.mark_synced_at(&answer.to_string(), now_ms() + 365 * DAY_MS).unwrap().synced, 1);
    assert_eq!(high_water(), kept, "the sync's clock is not kept");
    let after = take(&s, &card, &json, 20_000, "after").unwrap_or_else(|e| panic!("the kit expired: {e:#}"));
    assert_eq!(after.charge.state, ChargeState::Queued);
}
