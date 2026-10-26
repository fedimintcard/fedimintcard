//! The card wallet against an in-process 4-guardian federation running the mint v1 module.
//!
//! Funding comes from the dummy module (it mints value for any key). The backend is chosen by
//! `CARD_BACKEND` like everywhere else (soft by default). Real cards persist keys across tests, so
//! run a pcsc pass with `--test-threads=1`.

use std::str::FromStr;
use std::time::Duration;

use fedimint_card_host_card::open_from_env;
use fedimint_card_sidecar::glue::{CardWallet, nonce_for, on_card};
use fedimint_card_sidecar::plan;
use fedimint_card_sidecar::state::{NoteStatus, StateStore};
use fedimint_card_sidecar::wallet::Wallet;
use fedimint_client::ClientHandleArc;
use fedimint_client::transaction::TransactionBuilder;
use fedimint_core::core::{IntoDynInstance, OperationId};
use fedimint_core::encoding::Decodable;
use fedimint_core::module::registry::ModuleRegistry;
use fedimint_core::secp256k1::hashes::Hash as _;
use fedimint_core::secp256k1::rand::{self, Rng};
use fedimint_core::secp256k1::{Keypair, Message, SECP256K1, XOnlyPublicKey};
use fedimint_core::transaction::{Transaction, TransactionError, TransactionSignature, TransactionSubmissionOutcome};
use fedimint_core::Amount;
use fedimint_dummy_client::{DummyClientInit, DummyClientModule};
use fedimint_dummy_server::DummyInit;
use fedimint_mint_client::{MintClientInit, OOBNotes};
use fedimint_mint_common::{MintInput, MintOutput, Note};
use fedimint_mint_server::MintInit;
use fedimint_testing::federation::FederationTest;
use fedimint_testing::fixtures::Fixtures;

struct Fx {
    /// Keep alive: dropping it stops the guardians.
    fed: FederationTest,
    cw: CardWallet,
    _dir: tempfile::TempDir,
}

/// Give `client` ecash out of thin air through the dummy module.
async fn fund(client: &ClientHandleArc, amount: Amount) -> anyhow::Result<()> {
    let dummy = client.get_first_module::<DummyClientModule>()?;
    let op = OperationId::new_random();
    let range = client
        .finalize_and_submit_transaction(op, "fund", |_| (), TransactionBuilder::new().with_inputs(dummy.create_input(amount)))
        .await?;
    client.await_primary_bitcoin_module_outputs(op, range.into_iter().collect()).await?;
    Ok(())
}

async fn fx(fees: bool, funds: Amount) -> anyhow::Result<Fx> {
    assert!(
        std::env::var_os("FM_TEST_USE_REAL_DAEMONS").is_none(),
        "FM_TEST_USE_REAL_DAEMONS is set: these tests need the in-process fake bitcoin backend"
    );
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit).with_module(DummyClientInit, DummyInit);
    let mut builder = fixtures.new_fed_builder(0);
    if !fees {
        builder = builder.disable_mint_fees();
    }
    let fed = tokio::time::timeout(Duration::from_secs(60), builder.build()).await?;
    let client = fed.new_client().await;
    fund(&client, funds).await?;
    let wallet = Wallet::from_client(client).await?;
    let dir = tempfile::tempdir()?;
    let store = StateStore::open(dir.path().join("card_state.json"))?;
    let card = open_from_env()?;
    let mut cw = CardWallet::new(wallet, store, card);
    on_card(|| ()); // keep the helper exercised on every backend
    cw.card_setup()?;
    Ok(Fx { fed, cw, _dir: dir })
}

fn expected_fee(fees: bool) -> u64 {
    if fees { 100 } else { 0 }
}

#[tokio::test(flavor = "multi_thread")]
async fn federation_fee_configs_are_what_the_tests_assume() -> anyhow::Result<()> {
    for fees in [false, true] {
        let f = fx(fees, Amount::from_sats(1000)).await?;
        assert_eq!(f.cw.wallet.fee(1 << 20), expected_fee(fees));
        assert_eq!(f.cw.wallet.fee(1), expected_fee(fees));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn topup_to_card_key_verifies() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    let before = f.cw.wallet.balance_msat().await?;
    // 0b1010: two notes, 2^3 and 2^1 ... use real sizes: 2^17 + 2^15.
    let done = f.cw.topup(&[(1 << 17) + (1 << 15)]).await?;
    assert_eq!(done.len(), 2);

    let st = &f.cw.store.state;
    assert_eq!(st.live_notes().len(), 2);
    for n in st.live_notes() {
        // The note's nonce is the slot key with even parity, and the note verifies under the
        // federation's aggregate key for its denomination.
        let key = XOnlyPublicKey::from_str(st.slot_pubkey(n.slot).unwrap())?;
        assert_eq!(hex_of(nonce_for(key).0.serialize()), n.nonce);
        let sig = fedimint_core::encoding::Decodable::consensus_decode_hex(n.signature.as_deref().unwrap(), &ModuleRegistry::default())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let note = Note { nonce: nonce_for(key), signature: sig };
        let pk = *f.cw.wallet.mint_cfg.tbs_pks.get(Amount::from_msats(n.amount_msat)).expect("tier");
        assert!(note.verify(pk), "note on slot {} verifies", n.slot);
        // Not spent yet.
        assert!(!f.cw.is_spent(note.nonce).await?);
    }
    assert_eq!(st.live_total_msat(), (1 << 17) + (1 << 15));
    // The wallet paid the notes plus fees (100 per output, and the wallet's own inputs/change).
    let after = f.cw.wallet.balance_msat().await?;
    assert!(before - after >= (1 << 17) + (1 << 15) + 200, "paid {} msat", before - after);
    assert!(before - after <= (1 << 17) + (1 << 15) + 5_000, "paid {} msat", before - after);
    drop(f.fed);
    Ok(())
}

fn hex_of(b: [u8; 33]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// One spend with the card signature; checks the accounting identity to the msat.
async fn spend_exact(fees: bool) -> anyhow::Result<()> {
    let mut f = fx(fees, Amount::from_sats(10_000)).await?;
    f.cw.topup(&[1 << 20]).await?;
    let old_slot_key = f.cw.store.state.slot_pubkey(0).unwrap().to_owned();
    let till_before = f.cw.wallet.balance_msat().await?;

    let price = 100_000;
    let r = f.cw.spend(price).await?;
    let ins: u64 = r.inputs.iter().map(|(_, a)| a).sum();
    let change: u64 = r.change.iter().map(|(_, a)| a).sum();
    // Exact balance: inputs == till + change + fees, to the msat. The federation enforces it.
    assert_eq!(ins, r.till_msat + change + r.fees_msat, "{r:?}");
    assert!(r.overpay_msat <= plan::MAX_TILL_OVERPAY);
    assert_eq!(r.till_msat, price + r.overpay_msat);
    if fees {
        assert!(r.fees_msat >= 200);
    } else {
        assert_eq!(r.fees_msat, 0);
    }

    // The till got its notes into the wallet (less the wallet's own reissue fees).
    let till_after = f.cw.wallet.balance_msat().await?;
    assert!(till_after > till_before + price - 2_000, "till {till_before} -> {till_after}");
    assert!(r.received.msats == r.till_msat);

    // Change landed on the card as live notes, and the spent slot was renewed.
    let st = &f.cw.store.state;
    assert_eq!(st.live_total_msat(), change);
    assert_eq!(st.live_notes().len(), r.change.len());
    assert_eq!(r.renewed_slots, vec![0], "input slot renewed");
    assert_ne!(st.slot_pubkey(0).unwrap(), old_slot_key, "slot 0 has a fresh key");
    let spent = st.notes.iter().filter(|n| n.status == NoteStatus::Spent).count();
    assert_eq!(spent, 1);

    // `balance` agrees with the federation about every note.
    let b = f.cw.balance().await?;
    assert_eq!(b.live_msat, change);
    drop(f.fed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn spend_with_card_signature_is_exact_fees_on() -> anyhow::Result<()> {
    spend_exact(true).await
}

#[tokio::test(flavor = "multi_thread")]
async fn spend_with_card_signature_is_exact_fees_off() -> anyhow::Result<()> {
    spend_exact(false).await
}

#[tokio::test(flavor = "multi_thread")]
async fn change_note_is_spendable() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    f.cw.topup(&[1 << 20]).await?;
    let r1 = f.cw.spend(100_000).await?;
    let change1: u64 = r1.change.iter().map(|(_, a)| a).sum();
    assert!(change1 > 0);
    let live_before: Vec<String> = f.cw.store.state.live_notes().iter().map(|n| n.nonce.clone()).collect();

    // Spend again, from change only.
    let r2 = f.cw.spend(250_000).await?;
    assert!(r2.inputs.iter().all(|(slot, _)| r1.change.iter().any(|(s, _)| s == slot)), "{r2:?}");
    let change2: u64 = r2.change.iter().map(|(_, a)| a).sum();
    let ins2: u64 = r2.inputs.iter().map(|(_, a)| a).sum();
    assert_eq!(ins2, r2.till_msat + change2 + r2.fees_msat);

    // The first change notes that were used are now spent on the federation.
    for n in &f.cw.store.state.notes {
        if live_before.contains(&n.nonce) && r2.inputs.iter().any(|(s, a)| *s == n.slot && *a == n.amount_msat) {
            assert!(f.cw.is_spent(fedimint_mint_common::Nonce(
                fedimint_core::secp256k1::PublicKey::from_str(&n.nonce)?
            ))
            .await?);
        }
    }
    assert_eq!(f.cw.store.state.live_total_msat(), change1 - ins2 + change2);
    drop(f.fed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn cashout_produces_ecash_the_stock_client_reissues() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    f.cw.topup(&[1 << 18]).await?;
    let slot = f.cw.store.state.live_notes()[0].slot;

    let r = f.cw.cashout(slot).await?;
    // 2^18 minus the input fee and the output fees, exactly.
    let fee = |a: u64| f.cw.wallet.fee(a);
    let oob = OOBNotes::from_str(&r.ecash).map_err(|e| anyhow::anyhow!("{e}"))?;
    let face = oob.total_amount().msats;
    let outs: Vec<u64> = oob.notes().iter_items().map(|(a, _)| a.msats).collect();
    assert_eq!(face, r.face_msat);
    assert_eq!(1u64 << 18, face + fee(1 << 18) + plan::fee_sum(&outs, &fee));
    assert_eq!(f.cw.store.state.live_total_msat(), 0);
    assert!(f.cw.store.state.host_pending.is_empty());

    // An independent client with a fresh database reissues the string with the stock code path.
    let other = f.fed.new_client().await;
    let other = Wallet::from_client(other).await?;
    other.mint()?.validate_notes(&oob)?;
    let got = other.import(&r.ecash).await?;
    assert_eq!(got.msats, face);
    // Balance lags Done slightly; it ends within the reissue fees of the face value.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let b = other.balance_msat().await?;
        // The stock reissue pays an input fee per note and drops dust below the economical size.
        if b + 8_000 >= face {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "balance stuck at {b}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Replaying the string fails: the notes are spent.
    assert!(other.import(&r.ecash).await.is_err());
    drop(f.fed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_key_signature_is_rejected() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    f.cw.topup(&[1 << 18]).await?;
    let note_rec = f.cw.store.state.live_notes()[0].clone();
    let other_slot = f.cw.store.state.free_slots()[0];
    let mint_id = f.cw.wallet.mint_id;

    // [card note] -> [one note]; fee on both sides is 100 msat.
    let amount = Amount::from_msats(note_rec.amount_msat);
    let nonce = fedimint_mint_common::Nonce(fedimint_core::secp256k1::PublicKey::from_str(&note_rec.nonce)?);
    let sig = Decodable::consensus_decode_hex(note_rec.signature.as_deref().unwrap(), &ModuleRegistry::default())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let out_amount = Amount::from_msats((1 << 17) + (1 << 16));
    let throwaway = Keypair::new(SECP256K1, &mut rand::thread_rng());
    let key = tbs::BlindingKey::random();
    let blinded = tbs::blind_message(fedimint_mint_common::Nonce(throwaway.public_key()).to_message(), key);
    let mut tx = Transaction {
        inputs: vec![MintInput::new_v0(amount, Note { nonce, signature: sig }).into_dyn(mint_id)],
        outputs: vec![MintOutput::new_v0(out_amount, fedimint_mint_common::BlindNonce(blinded)).into_dyn(mint_id)],
        nonce: rand::thread_rng().r#gen(),
        signatures: TransactionSignature::NaiveMultisig(vec![]),
    };
    let digest = tx.tx_hash().to_byte_array();

    // Signed by another key on the card, and by a key the host made up: both rejected.
    let wrong_card = on_card(|| f.cw.card.sign(other_slot, &digest))?;
    let wrong_host = SECP256K1.sign_schnorr(&Message::from_digest(digest), &throwaway);
    for bad in [wrong_card, wrong_host] {
        tx.signatures = TransactionSignature::NaiveMultisig(vec![bad]);
        let client = &f.cw.wallet.client;
        let TransactionSubmissionOutcome(res) = client.api().submit_transaction(tx.clone()).await.try_into_inner(client.decoders())?;
        let err = res.expect_err("must be rejected");
        assert!(matches!(err, TransactionError::InvalidSignature { .. }), "{err}");
    }

    // The rejection did not burn the note: the card can still spend it normally.
    assert!(!f.cw.is_spent(nonce).await?);
    let r = f.cw.spend(50_000).await?;
    assert_eq!(r.inputs, vec![(note_rec.slot, note_rec.amount_msat)]);
    drop(f.fed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn state_file_survives_a_restart() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    f.cw.topup(&[1 << 18]).await?;
    f.cw.spend(30_000).await?;
    let path = f.cw.store.path().to_owned();
    let state = f.cw.store.state.clone();
    assert!(!state.notes.is_empty());

    // New process: same file, same card.
    let reopened = StateStore::open(&path)?;
    assert_eq!(reopened.state, state);
    // A pending overwrite left by a crash is resolved from the card's real state.
    f.cw.store.state.pending_overwrites.push(fedimint_card_sidecar::state::PendingOverwrite {
        slot: 5,
        old_pubkey: f.cw.store.state.slot_pubkey(5).unwrap().to_owned(),
    });
    f.cw.store.save()?;
    f.cw.recover_overwrites()?; // card still has the old key: "never ran", record dropped
    assert!(f.cw.store.state.pending_overwrites.is_empty());
    assert_eq!(f.cw.store.state.slot_pubkey(5), state.slot_pubkey(5));
    drop(f.fed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn spend_fails_cleanly_when_card_cannot_cover_it() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    f.cw.topup(&[1 << 16]).await?;
    let err = f.cw.spend(1 << 20).await.expect_err("insufficient");
    assert!(err.to_string().contains("insufficient"), "{err}");
    assert_eq!(f.cw.store.state.live_total_msat(), 1 << 16, "nothing was touched");
    drop(f.fed);
    Ok(())
}
