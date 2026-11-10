//! The apps' interface (feature `ffi`): `PosCore` with an `ApduTransport` written in Rust, as the
//! Swift and Kotlin ones are, through charge, drain, mark_synced and dismiss.

#![cfg(feature = "ffi")]

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::{DAY_MS, Fx, now_ms};
use fedimint_card_host_card::Transport as _;
use fedimint_card_host_card::softkit::SoftTransport;
use fedimint_card_pos_core::ffi::{ApduTransport, ChargeOutcome, PosCore, PosCoreError, TransportError};
use serde_json::{Value, json};

/// The phone's NFC session: a SoftCard's answers, until the card is gone.
struct Nfc {
    card: Mutex<SoftTransport>,
    gone: AtomicBool,
    sent: AtomicUsize,
}

impl Nfc {
    fn new(card: &SoftTransport) -> Arc<Self> {
        Arc::new(Self { card: Mutex::new(card.clone()), gone: AtomicBool::new(false), sent: AtomicUsize::new(0) })
    }
}

impl ApduTransport for Nfc {
    fn transmit(&self, apdu: Vec<u8>) -> Result<Vec<u8>, TransportError> {
        self.sent.fetch_add(1, Ordering::SeqCst);
        if self.gone.load(Ordering::SeqCst) {
            return Err(TransportError::Lost { reason: "tag was lost".into() });
        }
        self.card.lock().unwrap().transmit(&apdu).map_err(|e| TransportError::Lost { reason: e.to_string() })
    }
}

fn open() -> (tempfile::TempDir, Arc<PosCore>) {
    let dir = tempfile::tempdir().unwrap();
    let pc = PosCore::open(dir.path().to_str().unwrap().to_owned()).unwrap();
    (dir, pc)
}

fn kit(fx: &Fx, pc: &PosCore) -> String {
    json!({"ok": true, "kit": fx.kit(&pc.phone_id(), 40, now_ms(), 7 * DAY_MS)}).to_string()
}

#[test]
fn a_phone_charges_then_drains_and_syncs() {
    let fx = Fx::new(true);
    let (_dir, pc) = open();
    let card = fx.card(32, &[1 << 16, 1 << 14]);
    let k = kit(&fx, &pc);
    let nfc = Nfc::new(&card);
    let paid = pc.charge(nfc.clone(), k.clone(), 50_000, "sale-1".into(), "1234".into());
    let ChargeOutcome::Paid { txid, price_msat, till_msat, inputs, change, replayed, .. } = paid else { panic!("{paid:?}") };
    assert_eq!((price_msat, replayed), (50_000, false));
    assert!(till_msat >= 50_000 && !inputs.is_empty() && !change.is_empty());
    assert!(nfc.sent.load(Ordering::SeqCst) > 5);
    let st = pc.stats();
    assert_eq!((st.waiting, st.waiting_msat, st.synced), (1, till_msat, 0));

    let d = pc.drain(50);
    assert_eq!(d.txids, vec![txid.clone()]);
    let body: Value = serde_json::from_str(&d.body).unwrap();
    assert_eq!(body["phone_id"], json!(pc.phone_id()));
    assert_eq!(body["charges"][0]["txid"], json!(txid));
    assert!(body["charges"][0]["signed_hex"].as_str().is_some_and(|s| !s.is_empty()));

    let r = pc.mark_synced(json!({"ok": true, "results": [{"txid": txid, "status": "queued"}]}).to_string()).unwrap();
    assert_eq!((r.synced, r.refused, r.retry), (1, 0, 0));
    assert_eq!((pc.stats().waiting, pc.stats().synced), (0, 1));
    assert!(pc.drain(50).txids.is_empty());

    // The same sale again is that sale, answered without a word to the card.
    let again = Nfc::new(&card);
    let replay = pc.charge(again.clone(), k, 50_000, "sale-1".into(), "1234".into());
    assert!(matches!(&replay, ChargeOutcome::Paid { txid: t, replayed: true, .. } if *t == txid), "{replay:?}");
    assert_eq!(again.sent.load(Ordering::SeqCst), 0);
}

#[test]
fn a_refusal_carries_its_code_tries_and_prices() {
    let fx = Fx::new(true);
    let (_dir, pc) = open();
    let card = fx.card(32, &[1 << 16]);
    let k = kit(&fx, &pc);
    let r = pc.charge(Nfc::new(&card), k.clone(), 20_000, "pin".into(), "9999".into());
    assert!(matches!(&r, ChargeOutcome::Refused { code, tries_left: Some(2), .. } if code == "wrong_pin"), "{r:?}");
    // The card leaves the field: no_card.
    let gone = Nfc::new(&card);
    gone.gone.store(true, Ordering::SeqCst);
    let r = pc.charge(gone, k.clone(), 20_000, "gone".into(), "1234".into());
    assert!(matches!(&r, ChargeOutcome::Refused { code, .. } if code == "no_card"), "{r:?}");
    // An expired kit: refused before the card hears anything.
    let old = json!({"ok": true, "kit": fx.kit(&pc.phone_id(), 4, now_ms() - 9 * DAY_MS, 7 * DAY_MS)}).to_string();
    let nfc = Nfc::new(&card);
    let r = pc.charge(nfc.clone(), old, 20_000, "late".into(), "1234".into());
    assert!(matches!(&r, ChargeOutcome::Refused { code, .. } if code == "offline_kit_expired"), "{r:?}");
    assert_eq!(nfc.sent.load(Ordering::SeqCst), 0);
    // A price that needs a tier the pool has run out of: the prices it pays instead.
    let mut short = fx.kit(&pc.phone_id(), 40, now_ms(), DAY_MS);
    short.pool.retain(|t| t.amount_msat != 1 << 14);
    let r = pc.charge(Nfc::new(&card), serde_json::to_string(&short).unwrap(), 20_000, "short".into(), "1234".into());
    let ChargeOutcome::Refused { code, suggested_msat, .. } = &r else { panic!("{r:?}") };
    assert_eq!(code, "offline_pool_exhausted", "{r:?}");
    assert!(suggested_msat.is_some(), "{r:?}");
}

#[test]
fn refused_charges_are_listed_until_dismissed() {
    let fx = Fx::new(true);
    let (_dir, pc) = open();
    let card = fx.card(32, &[1 << 16, 1 << 16]);
    let k = kit(&fx, &pc);
    let ChargeOutcome::Paid { txid, .. } = pc.charge(Nfc::new(&card), k, 20_000, "r".into(), "1234".into()) else { panic!() };
    let answer = json!({"ok": true, "results": [{"txid": txid, "status": "refused", "code": "pool_entry_reused", "retry": false}]});
    assert_eq!(pc.mark_synced(answer.to_string()).unwrap().refused, 1);
    let d = pc.drain(50);
    assert!(d.txids.is_empty());
    assert_eq!(d.refused.iter().map(|r| (r.txid.as_str(), r.code.as_str())).collect::<Vec<_>>(), vec![(txid.as_str(), "pool_entry_reused")]);
    assert_eq!(pc.stats().refused, 1);
    assert!(pc.dismiss(txid.clone()).unwrap());
    assert!(!pc.dismiss(txid).unwrap());
    assert!(matches!(pc.mark_synced("{".into()), Err(PosCoreError::Answer(_))));
}

#[test]
fn one_process_has_the_store() {
    let (dir, _pc) = open();
    assert!(matches!(PosCore::open(dir.path().to_str().unwrap().to_owned()), Err(PosCoreError::Store(_))));
}
