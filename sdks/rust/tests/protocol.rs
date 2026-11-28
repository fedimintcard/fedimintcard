use fedimintcard::{Balance, Reader, ReaderError, SlotState};
use serde_json::{json, Value};
use std::{cell::RefCell, collections::VecDeque, io, rc::Rc};

fn vectors() -> Value {
    serde_json::from_str(include_str!("reader-vectors.json")).unwrap()
}
fn bytes(v: &Value) -> Vec<u8> {
    hex::decode(v.as_str().unwrap()).unwrap()
}

type Step = (Vec<u8>, Vec<u8>);
type Pending = Rc<RefCell<VecDeque<Step>>>;
type Transport = Box<dyn FnMut(&[u8]) -> io::Result<Vec<u8>>>;
fn step(v: &Value) -> Step {
    (bytes(&v["command"]), bytes(&v["response"]))
}
fn stream(steps: Vec<Step>) -> (Transport, Pending) {
    let queue = Rc::new(RefCell::new(VecDeque::from(steps)));
    let outstanding = Rc::clone(&queue);
    (
        Box::new(move |actual: &[u8]| {
            let (command, response) = queue.borrow_mut().pop_front().expect("unexpected APDU");
            assert_eq!(command, actual);
            Ok(response)
        }),
        outstanding,
    )
}
#[test]
fn accepts_current_minor_and_rejects_unsupported_or_broken_cards_at_select() {
    let v = vectors();
    for (response, version) in [
        (&v["select"]["response"], (4, 0)),
        (&v["futureMinor"]["response"], (4, 1)),
    ] {
        let (tx, q) = stream(vec![(bytes(&v["select"]["command"]), bytes(response))]);
        assert_eq!(Reader::select(tx).unwrap().version(), version);
        assert!(q.borrow().is_empty());
    }
    for response in [
        &v["legacy"]["response"],
        &v["malformedSelect"]["response"],
        &v["absent"]["response"],
        &v["malformedResponse"],
        &json!("05009000"),
    ] {
        let (tx, q) = stream(vec![(bytes(&v["select"]["command"]), bytes(response))]);
        assert!(Reader::select(tx).is_err());
        assert!(q.borrow().is_empty());
    }
}
#[test]
fn reads_host_vectors_and_preserves_integer_precision() {
    let v = vectors();
    let (tx, q) = stream(vec![
        step(&v["select"]),
        step(&v["state"]),
        step(&v["identity"]),
        (vec![0xb0, 0x32, 0, 0, 0], vec![0x6a, 0x83]),
    ]);
    let mut c = Reader::select(tx).unwrap();
    let s = c.state().unwrap();
    assert_eq!(s.limit_msat, 9_007_199_254_741_157);
    assert_eq!(s.spent_msat, 7001);
    assert_eq!(s.now, 1_730_000_000);
    assert_eq!(s.window, 1_729_999_000);
    assert_eq!(s.owed, 33);
    assert_eq!(s.wrong_tries, 2);
    assert!(s.anchored && s.chip_multiply && s.nonce_key_in_ram && s.pin_set);
    assert_eq!(
        s.slots[..4],
        [
            SlotState::Keyed,
            SlotState::Note,
            SlotState::Spent,
            SlotState::Change
        ]
    );
    assert_eq!(
        c.public_key(0).unwrap().serialize().as_slice(),
        bytes(&v["identity"]["xOnly"])
    );
    assert_eq!(c.certificate().unwrap(), None);
    assert!(matches!(
        c.public_key(32),
        Err(ReaderError::InvalidSlot(32))
    ));
    assert!(q.borrow().is_empty());
}
fn slot(v: &Value, index: u8, state: u8, amount: u64) -> Step {
    let mut b = vec![0u8; 187];
    b[0] = state;
    b[1..33].copy_from_slice(&bytes(&v["identity"]["xOnly"]));
    b[33..41].copy_from_slice(&amount.to_be_bytes());
    b.extend([0x90, 0]);
    (vec![0xb0, 0x15, index, 0, 0], b)
}
#[test]
fn balances_notes_and_owed_change_separately_and_detects_changes() {
    let v = vectors();
    for last_state in [3, 4] {
        let (tx, q) = stream(vec![
            step(&v["select"]),
            step(&v["state"]),
            slot(&v, 1, 2, u64::MAX),
            slot(&v, 3, last_state, 7),
        ]);
        let amount = Reader::select(tx).unwrap().balance();
        if last_state == 3 {
            assert_eq!(
                amount.unwrap(),
                Balance {
                    stored_msat: u128::from(u64::MAX),
                    owed_msat: 7
                }
            );
        } else {
            assert!(amount.is_err());
        }
        assert!(q.borrow().is_empty());
    }
}
#[test]
fn follows_get_response_and_corrects_its_length() {
    let v = vectors();
    let (tx, q) = stream(vec![
        (bytes(&v["select"]["command"]), vec![4, 0x61, 2]),
        (vec![0, 0xc0, 0, 0, 2], vec![0x6c, 1]),
        (vec![0, 0xc0, 0, 0, 1], vec![0, 0x90, 0]),
    ]);
    assert_eq!(Reader::select(tx).unwrap().version(), (4, 0));
    assert!(q.borrow().is_empty());
}
#[test]
fn failed_transport_ends_reader() {
    let mut calls = 0;
    let mut c = Reader::select(|_: &[u8]| {
        calls += 1;
        if calls == 1 {
            Ok(vec![4, 0, 0x90, 0])
        } else {
            Err(io::Error::other("card removed"))
        }
    })
    .unwrap();
    assert!(matches!(c.state(), Err(ReaderError::Transport(_))));
    assert!(matches!(c.state(), Err(ReaderError::SessionEnded)));
    assert_eq!(calls, 2);
}
#[test]
fn checks_issuer_signature_and_observed_identity() {
    let v: Value = serde_json::from_str(include_str!("certificate-vector.json")).unwrap();
    let cmds = vectors();
    let (tx, q) = stream(vec![
        step(&cmds["select"]),
        step(&cmds["identity"]),
        step(&v),
    ]);
    let mut reader = Reader::select(tx).unwrap();
    let identity = reader.public_key(0).unwrap();
    let mut certificate = reader.certificate().unwrap().unwrap();
    let issuer = bytes(&v["registryPublicKey"]);
    assert!(fedimintcard::verify_certificate(
        &certificate,
        &issuer,
        &identity
    ));
    assert!(!fedimintcard::verify_certificate(
        &certificate,
        &bytes(&v["otherRegistryPublicKey"]),
        &identity
    ));
    certificate[2] ^= 0x01;
    assert!(!fedimintcard::verify_certificate(
        &certificate,
        &issuer,
        &identity
    ));
    certificate[2] ^= 0x01;
    certificate[100] ^= 0x01;
    assert!(!fedimintcard::verify_certificate(
        &certificate,
        &issuer,
        &identity
    ));
    assert!(q.borrow().is_empty());
}

#[test]
fn reads_only_the_public_change_reference() {
    let v = vectors();
    let mut change = slot(&v, 3, 3, 7);
    change.1[121..153].fill(0x11);
    change.1[153..155].copy_from_slice(&500u16.to_be_bytes());
    change.1[155..187].fill(0xab);
    let (tx, q) = stream(vec![step(&v["select"]), change]);
    let slot = Reader::select(tx).unwrap().slot(3).unwrap();
    let change = slot.change_output.unwrap();
    assert_eq!(change.transaction_id, [0x11; 32]);
    assert_eq!(change.output_index, 500);
    assert!(q.borrow().is_empty());
}
