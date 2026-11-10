//! The offline charge on a phone (docs/POS-OFFLINE.md): a 3.0 SoftCard behind an APDU transport,
//! an offline kit, and the phone's store, with no federation and no network.

mod common;

use std::sync::{Arc, Mutex};

use common::{DAY_MS, Fx, now_ms, slots_in, spent_hashes};
use fedimint_card_host_card::softkit::{PullTransport, SoftMint, SoftTransport};
use fedimint_card_host_card::{CardClient, SlotState, Transport};
use fedimint_card_pos_core::charge::Taken;
use fedimint_card_pos_core::queue::{ChargeState, QUEUE_FILE};
use fedimint_card_pos_core::refusal;
use fedimint_card_pos_core::store::Store;
use fedimint_core::encoding::Decodable;
use fedimint_core::secp256k1::hashes::Hash as _;
use fedimint_core::secp256k1::{Message, SECP256K1, XOnlyPublicKey};
use fedimint_core::TransactionId;
use fedimint_core::transaction::{Transaction, TransactionSignature};
use serde_json::Value;

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
        exactly_one_charge(&s, &card, &json, price, "pulled", &format!("pulled at {at}"));
    }
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
    // One entry per tier: a second charge needing the same tiers finds them used.
    let kit = fx.kit(s.phone_id(), 1, now_ms(), DAY_MS);
    let json = serde_json::to_string(&kit).unwrap();
    let first = take(&s, &card, &json, 20_000, "first").unwrap().charge;
    let answer = serde_json::json!({"ok": true, "results": [{"txid": first.txid, "status": "refused", "code": "pool_entry_unknown"}]});
    s.mark_synced(&answer.to_string()).unwrap();
    assert!(s.dismiss(&first.txid).unwrap());
    let r = take(&s, &card, &json, 20_000, "second");
    if let Ok(t) = &r {
        let used: Vec<&String> = first.pool.iter().map(|p| &p.blinded).collect();
        assert!(t.charge.pool.iter().all(|p| !used.contains(&&p.blinded)), "a dismissed charge's entries reused");
    }
}
