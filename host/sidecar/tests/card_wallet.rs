//! The card wallet against an in-process 4-guardian federation running the mint v1 module.
//!
//! Funding comes from the dummy module (it mints value for any key). The backend is chosen by
//! `CARD_BACKEND` like everywhere else (soft by default). Real cards persist keys across tests, so
//! run a pcsc pass with `--test-threads=1`.

use std::cell::RefCell;
use std::rc::Rc;
use std::str::FromStr;
use std::time::Duration;

use fedimint_card_host_card::{Card, SLOTS, SoftCard, open_from_env};
use fedimint_card_sidecar::glue::{CardWallet, CrashPoint, nonce_for, on_card};
use fedimint_card_sidecar::plan;
use fedimint_card_sidecar::state::{HostKind, NoteStatus, StateStore};
use fedimint_card_sidecar::wallet::Wallet;
use fedimint_client::ClientHandleArc;
use fedimint_client::transaction::TransactionBuilder;
use fedimint_core::Amount;
use fedimint_core::core::{IntoDynInstance, OperationId};
use fedimint_core::encoding::Decodable;
use fedimint_core::module::registry::ModuleRegistry;
use fedimint_core::secp256k1::hashes::Hash as _;
use fedimint_core::secp256k1::rand::{self, Rng};
use fedimint_core::secp256k1::{Keypair, Message, SECP256K1, XOnlyPublicKey};
use fedimint_core::transaction::{Transaction, TransactionError, TransactionSignature, TransactionSubmissionOutcome};
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

/// `cw.topup`, and on a 4.x card (the bridge's applet) the registry this suite does not have,
/// stood in for as `till_service.rs` `load` does: each new note signed under the test registry
/// key the card was set up with, then written to the card. A 2.1 card (the soft backend here)
/// takes its notes as before.
async fn topup(cw: &mut CardWallet, amounts: &[u64]) -> anyhow::Result<Vec<(u8, u64)>> {
    let done = cw.topup(amounts).await?;
    registry_signs(cw)?;
    Ok(done)
}

/// The registry this suite does not have, on a 4.x card: every note the card holds without the
/// registry's signature (a load's notes, and a spend's change) is signed under the test registry
/// key and written to the card, as ops does for a till (`till_service.rs` `load`).
fn registry_signs(cw: &mut CardWallet) -> anyhow::Result<()> {
    if on_card(|| cw.card.applet_version())?.is_current() {
        let key = fedimint_card_host_card::v3::test_registry_key();
        for n in cw.store.state.notes.iter_mut().filter(|n| n.reg_sig.is_none() && n.signature.is_some()) {
            let x: [u8; 32] = hex::decode(&n.nonce[2..])?.try_into().unwrap();
            let sig48 = fedimint_card_sidecar::glue::sig48_of(n.signature.as_deref().unwrap()).unwrap();
            n.reg_sig = Some(hex::encode(fedimint_card_host_card::v3::sign_note(&key, &x, n.amount_msat, &sig48)));
        }
        cw.store.save()?;
        cw.sync_card_notes();
    }
    Ok(())
}

/// A persistent card (`CARD_BACKEND` bridge or pcsc) for the fixture. The bridge's jCardSim card
/// is reset first, so every fixture starts from a blank applet, and a 4.0 card without a
/// certificate is then set up as the Setup app leaves one before its PIN
/// ([`softkit::ready_for_pin`](fedimint_card_host_card::softkit::ready_for_pin): certified by the
/// test registry, its key, the anchor); the fixture's `open_test_session` sets the PIN, sealed. A
/// pcsc card is taken as it is.
fn persistent_card() -> anyhow::Result<Box<dyn Card>> {
    if std::env::var("CARD_BACKEND").as_deref() == Ok("bridge") {
        fedimint_card_host_card::BridgeTransport::new(&fedimint_card_host_card::bridge::url_from_env()).reset()?;
    }
    let mut card = open_from_env()?;
    if card.applet_version()?.is_current() && card.get_cert().is_err() {
        fedimint_card_host_card::softkit::ready_for_pin(card.as_mut())?;
    }
    Ok(card)
}

/// Whether the fixture re-keys the card (`card_setup(true)`): only a pcsc card, which keeps keys
/// from earlier runs. The reset bridge card is blank, and a certified 4.x card refuses a re-key of
/// slot 0 by design.
fn rekey_backend() -> bool {
    std::env::var("CARD_BACKEND").as_deref() == Ok("pcsc")
}

async fn fx(fees: bool, funds: Amount) -> anyhow::Result<Fx> {
    let soft = std::env::var("CARD_BACKEND").map(|b| b == "soft").unwrap_or(true);
    fx_card(fees, funds, if soft { soft_v4()? } else { persistent_card()? }).await
}

/// The soft backend's card: a 4.0 SoftCard ready for its first PIN (certified by the test
/// registry, its key, the anchor). The till sends no PIN to a card older than 4.0.
fn soft_v4() -> anyhow::Result<Box<dyn Card>> {
    let mut card: Box<dyn Card> = Box::new(SoftCard::v4());
    fedimint_card_host_card::softkit::ready_for_pin(card.as_mut())?;
    Ok(card)
}

/// [`fx`] on a card of the caller's choosing, such as a soft 2.0 card with eight slots.
async fn fx_card(fees: bool, funds: Amount, card: Box<dyn Card>) -> anyhow::Result<Fx> {
    fx_card_on(Fixtures::new_primary(MintClientInit, MintInit), fees, funds, card).await
}

async fn fx_card_on(fixtures: Fixtures, fees: bool, funds: Amount, card: Box<dyn Card>) -> anyhow::Result<Fx> {
    assert!(
        std::env::var_os("FM_TEST_USE_REAL_DAEMONS").is_none(),
        "FM_TEST_USE_REAL_DAEMONS is set: these tests need the in-process fake bitcoin backend"
    );
    let fixtures = fixtures.with_module(DummyClientInit, DummyInit);
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
    let mut cw = CardWallet::new(wallet, store, card).with_cert_key(Some(fedimint_card_host_card::softkit::test_cert_key_hex()));
    on_card(|| ()); // keep the helper exercised on every backend
    // A persistent card (pcsc, bridge) already holds keys from earlier runs: re-key it so it
    // behaves like a new one.
    let soft = std::env::var("CARD_BACKEND").map(|b| b == "soft").unwrap_or(true);
    cw.card_setup(!soft && rekey_backend())?;
    // SIGN needs a PIN on the card and this selection's VERIFY_PIN; card_setup does not open one.
    // It is sealed: a card older than 4.0 gets the old-card refusal here, before any PIN.
    fedimint_card_host_card::softkit::open_test_session(cw.card.as_mut())?;
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
    let done = topup(&mut f.cw, &[(1 << 17) + (1 << 15)]).await?;
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
    topup(&mut f.cw, &[1 << 20]).await?;
    // On applet 4.0 slot 0 holds the certified attestation key and takes no load: the note is in
    // the first load slot.
    let in_slot = f.cw.store.state.live_notes()[0].slot;
    let old_slot_key = f.cw.store.state.slot_pubkey(in_slot).unwrap().to_owned();
    let till_before = f.cw.wallet.balance_msat().await?;

    let price = 100_000;
    let r = f.cw.spend(price).await?;
    let ins: u64 = r.inputs.iter().map(|(_, a)| a).sum();
    let change: u64 = r.change.iter().map(|(_, a)| a).sum();
    // Exact balance, with the fees recomputed here from the federation's fee consensus, one item
    // at a time, and not taken from the planner: every input and every output pays its own fee.
    let fc = &f.cw.wallet.mint_cfg.fee_consensus;
    let item = |a: u64| fc.fee(Amount::from_msats(a)).msats;
    let till_notes: Vec<u64> = (0..64).filter(|b| r.till_msat >> b & 1 == 1).map(|b| 1u64 << b).collect();
    let fees_here: u64 = r.inputs.iter().map(|(_, a)| item(*a)).sum::<u64>()
        + till_notes.iter().map(|a| item(*a)).sum::<u64>()
        + r.change.iter().map(|(_, a)| item(*a)).sum::<u64>();
    let n_items = (r.inputs.len() + till_notes.len() + r.change.len()) as u64;
    assert_eq!(fees_here, if fees { 100 * n_items } else { 0 });
    assert_eq!(ins, r.till_msat + change + fees_here, "{r:?}");
    assert_eq!(r.fees_msat, fees_here);
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
    assert_eq!(r.renewed_slots, vec![in_slot], "input slot renewed");
    assert_ne!(st.slot_pubkey(in_slot).unwrap(), old_slot_key, "the input slot has a fresh key");
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
async fn v2_card_load_purchase_and_change_can_be_redeemed_at_the_same_federation() -> anyhow::Result<()> {
    let fixtures = Fixtures::new_primary(fedimint_mintv2_client::MintClientInit, fedimint_mintv2_server::MintInit);
    let mut f = fx_card_on(fixtures, false, Amount::from_sats(10_000), soft_v4()?).await?;
    let amount = 1 << 19;
    let initial_balance = f.cw.wallet.balance_msat().await?;
    topup(&mut f.cw, &[amount]).await?;
    assert_eq!(f.cw.balance().await?.live_msat, amount);
    let loaded_balance = f.cw.wallet.balance_msat().await?;
    assert_eq!(loaded_balance + amount, initial_balance);

    let first = f.cw.spend(1 << 17).await?;
    assert_eq!(first.till_msat, 1 << 17);
    assert_eq!(first.received.msats, first.till_msat);
    registry_signs(&mut f.cw)?;
    let change = f.cw.balance().await?.live_msat;
    assert_eq!(change, amount - first.till_msat);

    let second = f.cw.spend(change).await?;
    assert_eq!(second.received.msats, change);
    assert_eq!(f.cw.balance().await?.live_msat, 0);
    assert_eq!(f.cw.wallet.balance_msat().await?, initial_balance);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn v2_card_purchase_and_cashout_at_ten_msat_settle_into_bitcoin_wallets() -> anyhow::Result<()> {
    let fixtures = Fixtures::new_primary(fedimint_mintv2_client::MintClientInit, fedimint_card_fed_tests::ten_msat_mint::TenMsatMint);
    let card = if std::env::var("CARD_BACKEND").as_deref() == Ok("bridge") { persistent_card()? } else { soft_v4()? };
    let mut f = fx_card_on(fixtures, true, Amount::from_sats(10_000), card).await?;
    let loaded = 1 << 20;
    topup(&mut f.cw, &[loaded]).await?;
    anyhow::ensure!(f.cw.wallet.fee(512) == 10);
    // Topup has delivered the card output; its own wallet change may still be in flight.
    let ops = f.cw.wallet.client.operation_log().paginate_operations_rev(10, None).await;
    let (key, _) = ops.iter().find(|(_, entry)| entry.operation_module_kind() == "card-topup").expect("recorded card load");
    tokio::time::timeout(Duration::from_secs(45), async {
        while f.cw.wallet.client.has_active_states(key.operation_id).await {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await?;
    let balance = f.cw.wallet.balance_msat().await?;
    let sale = f.cw.spend(100_000).await?;
    anyhow::ensure!(!sale.collection_pending && sale.received.msats == sale.till_msat);
    anyhow::ensure!(sale.till_msat >= 100_000 && sale.fees_msat > 0);
    let credited = f.cw.wallet.balance_msat().await?.checked_sub(balance).expect("sale credited the till wallet");
    anyhow::ensure!(
        credited > 0 && credited <= sale.till_msat,
        "merchant balance {balance}, net credit {credited}, gross {}",
        sale.till_msat
    );
    registry_signs(&mut f.cw)?;
    let change = f.cw.balance().await?.live_msat;
    anyhow::ensure!(loaded == sale.till_msat + change + sale.fees_msat);
    let (slot, _) = sale.change.iter().max_by_key(|(_, a)| *a).expect("sale returned change");
    let attempt = f.cw.cashout(*slot).await?;
    anyhow::ensure!(!attempt.collection_pending && attempt.exported_msat > 0 && attempt.exported_msat <= attempt.gained_msat);
    let recipient = Wallet::from_client(f.fed.new_client().await).await?;
    let imported = recipient.import(attempt.ecash.as_deref().expect("cashout exported v2 eCash")).await?;
    anyhow::ensure!(imported.face.msats == attempt.exported_msat);
    anyhow::ensure!(recipient.balance_msat().await? + imported.fee_msat == imported.face.msats);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn change_note_is_spendable() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    topup(&mut f.cw, &[1 << 17]).await?;
    let r1 = f.cw.spend(100_000).await?;
    registry_signs(&mut f.cw)?;
    let change1: u64 = r1.change.iter().map(|(_, a)| a).sum();
    assert!(change1 > 0);
    let live_before: Vec<String> = f.cw.store.state.live_notes().iter().map(|n| n.nonce.clone()).collect();

    // Spend again, from change only.
    let r2 = f.cw.spend(10_000).await?;
    assert!(r2.inputs.iter().all(|(slot, _)| r1.change.iter().any(|(s, _)| s == slot)), "{r2:?}");
    let change2: u64 = r2.change.iter().map(|(_, a)| a).sum();
    let ins2: u64 = r2.inputs.iter().map(|(_, a)| a).sum();
    assert_eq!(ins2, r2.till_msat + change2 + r2.fees_msat);

    // The first change notes that were used are now spent on the federation.
    for n in &f.cw.store.state.notes {
        if live_before.contains(&n.nonce) && r2.inputs.iter().any(|(s, a)| *s == n.slot && *a == n.amount_msat) {
            assert!(f.cw.is_spent(fedimint_mint_common::Nonce(fedimint_core::secp256k1::PublicKey::from_str(&n.nonce)?)).await?);
        }
    }
    assert_eq!(f.cw.store.state.live_total_msat(), change1 - ins2 + change2);
    drop(f.fed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn cashout_produces_ecash_the_stock_client_reissues() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    topup(&mut f.cw, &[1 << 18]).await?;
    let slot = f.cw.store.state.live_notes()[0].slot;

    let r = f.cw.cashout(slot).await?;
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    assert!(!r.collection_pending);
    let ecash = r.ecash.clone().expect("ecash string");
    // Nothing is left half-done on the host, and the note is gone from the card.
    assert_eq!(f.cw.store.state.live_total_msat(), 0);
    assert!(f.cw.store.state.host_pending.is_empty());
    assert!(f.cw.store.state.pending_txs.is_empty());

    // The string is made of exactly-denominated notes worth what the report says.
    let oob = OOBNotes::from_str(&ecash).map_err(|e| anyhow::anyhow!("{e}"))?;
    let face = oob.total_amount().msats;
    assert_eq!(face, r.exported_msat);
    assert!(face <= r.gained_msat && r.gained_msat - face < 2_000, "{face} of {}", r.gained_msat);
    assert!(face + 2 * 6_000 > 1 << 18, "most of the note arrives: {face}");

    // An independent client with a fresh database reissues the string with the stock code path.
    let other = f.fed.new_client().await;
    let other = Wallet::from_client(other).await?;
    other.mint()?.validate_notes(&oob)?;
    let imp = other.import(&ecash).await?;
    assert_eq!(imp.face.msats, face);
    // Independent lower bound on that client's own reissue fees: one input fee per note.
    let fc = &other.mint_cfg.fee_consensus;
    let input_fees: u64 = oob.notes().iter_items().map(|(a, _)| fc.fee(a).msats).sum();
    assert!(imp.fee_msat >= input_fees, "{} < {input_fees}", imp.fee_msat);
    // Its balance is the face value minus the fees the client itself reports, to the msat.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let b = other.balance_msat().await?;
        if b + imp.fee_msat == face {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "balance {b} + fees {} != face {face}", imp.fee_msat);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Replaying the string is not an error for the importer's second call but cannot pay twice.
    assert!(other.import(&ecash).await?.already);
    drop(f.fed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_key_signature_is_rejected() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    topup(&mut f.cw, &[1 << 18]).await?;
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

    // Another key on the card: applet 4.0 signs only with a slot holding its note, so the card
    // itself refuses (6985) and no wrong signature exists. A key the host made up: the federation
    // rejects it.
    let wrong_card = on_card(|| f.cw.card.sign(other_slot, &digest)).expect_err("a slot with no note does not sign");
    assert_eq!(fedimint_card_host_card::status_word(&wrong_card), Some(0x6985), "{wrong_card:#}");
    let wrong_host = SECP256K1.sign_schnorr(&Message::from_digest(digest), &throwaway);
    for bad in [wrong_host] {
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
    topup(&mut f.cw, &[1 << 18]).await?;
    f.cw.spend(30_000).await?;
    let path = f.cw.store.path().to_owned();
    let state = f.cw.store.state.clone();
    assert!(!state.notes.is_empty());

    // New process: same file, same card.
    let reopened = StateStore::open(&path)?;
    assert_eq!(reopened.state, state);
    // A pending overwrite left by a crash is resolved from the card's real state.
    f.cw.store
        .state
        .pending_overwrites
        .push(fedimint_card_sidecar::state::PendingOverwrite { slot: 5, old_pubkey: f.cw.store.state.slot_pubkey(5).unwrap().to_owned() });
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
    topup(&mut f.cw, &[1 << 16]).await?;
    let err = f.cw.spend(1 << 20).await.expect_err("insufficient");
    assert!(err.to_string().contains("insufficient"), "{err}");
    assert_eq!(f.cw.store.state.live_total_msat(), 1 << 16, "nothing was touched");
    drop(f.fed);
    Ok(())
}

/// Simulate a restart: new process, same state file, same card.
fn restart(f: &mut Fx) -> anyhow::Result<()> {
    f.cw.store = StateStore::open(f.cw.store.path().to_owned())?;
    f.cw.crash_point = None;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn spend_signed_but_not_submitted_is_abandoned() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    topup(&mut f.cw, &[1 << 20]).await?;
    let before = f.cw.store.state.live_total_msat();

    f.cw.crash_point = Some(CrashPoint::BeforeSubmit);
    assert!(f.cw.spend(100_000).await.is_err());
    let signed_slot = f.cw.store.state.live_notes()[0].slot;
    // The crash left records for a transaction that never left the host.
    let st = &f.cw.store.state;
    assert_eq!(st.pending_txs.len(), 1);
    assert!(!st.pending_txs[0].submitted);
    assert!(st.notes.iter().any(|n| n.status == NoteStatus::Pending));
    assert!(!st.host_pending.is_empty());

    restart(&mut f)?;
    // A new spend refuses to start on top of it until recovery ran; `balance` does the recovery.
    let b = f.cw.balance().await?;
    assert_eq!(b.recovered.abandoned, 1);
    let st = &f.cw.store.state;
    assert!(st.pending_txs.is_empty() && st.host_pending.is_empty());
    assert!(!st.notes.iter().any(|n| n.status == NoteStatus::Pending));
    assert_eq!(st.live_total_msat(), before, "the input note is untouched");
    // A 4.0 slot signs once: the slot that signed the abandoned transaction signs no other, so
    // its note cannot pay a new one, and the refusal records nothing.
    let err = f.cw.spend(100_000).await.expect_err("the slot signed another transaction");
    assert!(err.to_string().contains("has already signed another transaction"), "{err:#}");
    let st = &f.cw.store.state;
    assert!(st.pending_txs.is_empty() && st.host_pending.is_empty());
    assert_eq!(st.live_total_msat(), before);
    // And the card is usable again: it takes a new load, which pays.
    topup(&mut f.cw, &[1 << 18]).await?;
    let r = f.cw.spend(100_000).await?;
    assert!(r.inputs.iter().all(|(slot, _)| *slot != signed_slot), "{r:?}");
    drop(f.fed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn spend_accepted_but_not_collected_is_collected_by_out_idx() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    topup(&mut f.cw, &[1 << 20]).await?;
    let till_before = f.cw.wallet.balance_msat().await?;

    f.cw.crash_point = Some(CrashPoint::AfterSubmit);
    assert!(f.cw.spend(100_000).await.is_err());
    // Every pending output knows its index in the transaction.
    let st = &f.cw.store.state;
    assert!(st.pending_txs[0].submitted && st.pending_txs[0].accepted);
    assert!(st.host_pending.iter().all(|h| h.out_idx.is_some()));
    assert!(st.notes.iter().filter(|n| n.status == NoteStatus::Pending).all(|n| n.out_idx.is_some() && n.txid.is_some()));
    assert_eq!(st.pending_txs.len(), 1);

    restart(&mut f)?;
    let b = f.cw.balance().await?;
    assert!(b.unresolved.is_empty(), "{:?} {:?}", b.unresolved, b.recovered.errors);
    assert!(b.recovered.errors.is_empty(), "{:?}", b.recovered.errors);
    assert!(b.recovered.notes_live >= 1, "change collected");
    let st = &f.cw.store.state;
    assert!(st.pending_txs.is_empty() && st.host_pending.is_empty());
    assert_eq!(st.notes.iter().filter(|n| n.status == NoteStatus::Spent).count(), 1, "input marked spent");
    // The till notes reached the wallet: face value less the wallet's reported reissue fees.
    let after = f.cw.wallet.balance_msat().await?;
    assert_eq!(after - till_before + b.recovered.till_fee, b.recovered.till_received);
    assert!(b.recovered.till_received >= 100_000);
    drop(f.fed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn undecided_submission_is_resubmitted_idempotently() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    topup(&mut f.cw, &[1 << 20]).await?;
    f.cw.crash_point = Some(CrashPoint::AfterSubmit);
    assert!(f.cw.spend(100_000).await.is_err());
    // Pretend the answer never arrived: submitted, not known to be accepted.
    f.cw.store.state.pending_txs[0].accepted = false;
    f.cw.store.save()?;
    restart(&mut f)?;
    let b = f.cw.balance().await?;
    assert!(b.unresolved.is_empty(), "{:?}", b.recovered.errors);
    assert!(b.recovered.notes_live >= 1);
    assert!(b.recovered.till_received >= 100_000);
    drop(f.fed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn cashout_interrupted_after_submit_ends_in_the_wallet() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    topup(&mut f.cw, &[1 << 18]).await?;
    let slot = f.cw.store.state.live_notes()[0].slot;
    let wallet_before = f.cw.wallet.balance_msat().await?;

    f.cw.crash_point = Some(CrashPoint::AfterSubmit);
    assert!(f.cw.cashout(slot).await.is_err());
    // The secret keys of the host notes are on disk, not lost with the process.
    assert!(!f.cw.store.state.host_pending.is_empty());

    restart(&mut f)?;
    let b = f.cw.balance().await?;
    assert!(b.unresolved.is_empty(), "{:?}", b.recovered.errors);
    assert!(b.recovered.cashouts.is_empty(), "no string path any more");
    // Wallet balance rose by the face value minus its own reissue fees as reported by the client.
    let after = f.cw.wallet.balance_msat().await?;
    assert_eq!(after - wallet_before + b.recovered.till_fee, b.recovered.till_received);
    assert!(b.recovered.till_received > (1 << 18) - 1_500, "{}", b.recovered.till_received);
    assert_eq!(f.cw.store.state.live_total_msat(), 0);
    // The wallet can now hand the value out with the stock export.
    let amount = f.cw.wallet.max_exportable(after - wallet_before).await?;
    let (notes, face) = f.cw.wallet.export(amount, false, true).await?;
    assert_eq!(notes.total_amount(), face);
    drop(f.fed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_cashout_records_go_to_the_log_before_they_are_removed() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    topup(&mut f.cw, &[1 << 20]).await?;
    f.cw.crash_point = Some(CrashPoint::AfterSubmit);
    assert!(f.cw.spend(100_000).await.is_err());
    // Rewrite the host records the way an older build left them.
    for h in &mut f.cw.store.state.host_pending {
        h.kind = HostKind::Cashout;
    }
    f.cw.store.save()?;
    restart(&mut f)?;
    let b = f.cw.balance().await?;
    assert_eq!(b.recovered.cashouts.len(), 1);
    assert_eq!(f.cw.recovered_cashouts, b.recovered.cashouts, "the CLI prints these");
    let log = std::fs::read_to_string(f.cw.store.path().with_file_name("cashouts.log"))?;
    assert!(log.contains(&b.recovered.cashouts[0]));
    assert!(f.cw.store.state.host_pending.is_empty());
    // The logged string is real ecash.
    let other = Wallet::from_client(f.fed.new_client().await).await?;
    assert!(other.import(&b.recovered.cashouts[0]).await?.face.msats >= 100_000);
    drop(f.fed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn export_never_rounds_up_silently() -> anyhow::Result<()> {
    let f = fx(true, Amount::from_sats(10_000)).await?;
    let w = &f.cw.wallet;
    assert_eq!(w.min_denom(), 512);
    assert!(w.export(1_000, false, true).await.is_err(), "1000 is not a multiple of 512");
    let (notes, face) = w.export(1_000, true, true).await?;
    assert_eq!(face.msats, 1_024);
    assert_eq!(notes.total_amount().msats, 1_024);
    let (notes, face) = w.export(2_048, false, false).await?;
    assert_eq!(face.msats, 2_048);
    assert_eq!(notes.total_amount().msats, 2_048);
    drop(f.fed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn topup_rejects_dust_amounts() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    let err = topup(&mut f.cw, &[1000]).await.expect_err("not a multiple of 512");
    assert!(err.to_string().contains("multiples of 512"), "{err}");
    drop(f.fed);
    Ok(())
}

fn stored_tx(f: &Fx) -> anyhow::Result<Transaction> {
    let hex_tx = f.cw.store.state.pending_txs[0].tx_hex.clone().expect("stored bytes");
    Transaction::consensus_decode_hex(&hex_tx, f.cw.wallet.client.decoders()).map_err(|e| anyhow::anyhow!("{e}"))
}

fn store_tx(f: &mut Fx, tx: &Transaction) -> anyhow::Result<()> {
    use fedimint_core::encoding::Encodable;
    f.cw.store.state.pending_txs[0].tx_hex = Some(tx.consensus_encode_to_hex());
    f.cw.store.save()
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_resubmission_does_not_abandon_a_confirmed_original() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    topup(&mut f.cw, &[1 << 20]).await?;
    f.cw.crash_point = Some(CrashPoint::AfterSubmit);
    assert!(f.cw.spend(100_000).await.is_err());
    // The stored bytes now describe a different transaction (new txid, same input): resubmitting
    // them is rejected, as an older federation might reject a duplicate. The recorded txid is
    // the one the federation did accept, and that confirmation wins.
    let mut tx = stored_tx(&f)?;
    tx.nonce = [7; 8];
    store_tx(&mut f, &tx)?;
    f.cw.store.state.pending_txs[0].accepted = false;
    f.cw.store.save()?;
    restart(&mut f)?;
    let b = f.cw.balance().await?;
    assert_eq!(b.recovered.abandoned, 0);
    assert!(b.unresolved.is_empty(), "{:?}", b.recovered.errors);
    assert!(b.recovered.notes_live >= 1 && b.recovered.till_received >= 100_000);
    drop(f.fed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_resubmission_with_an_unspent_input_is_abandoned() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    topup(&mut f.cw, &[1 << 20]).await?;
    let before = f.cw.store.state.live_total_msat();
    f.cw.crash_point = Some(CrashPoint::BeforeSubmit);
    assert!(f.cw.spend(100_000).await.is_err());
    // Claim it was submitted, with the signatures stripped so the federation rejects it. It never
    // was accepted and the input is unspent, so it must be abandoned (after the 10 s wait).
    let mut tx = stored_tx(&f)?;
    tx.signatures = TransactionSignature::NaiveMultisig(vec![]);
    store_tx(&mut f, &tx)?;
    f.cw.store.state.pending_txs[0].submitted = true;
    f.cw.store.save()?;
    restart(&mut f)?;
    let b = f.cw.balance().await?;
    assert_eq!(b.recovered.abandoned, 1, "{:?}", b.recovered.errors);
    assert!(f.cw.store.state.pending_txs.is_empty());
    assert_eq!(f.cw.store.state.live_total_msat(), before);
    drop(f.fed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn spend_right_after_a_crash_does_not_replan_the_spent_note() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    topup(&mut f.cw, &[1 << 17]).await?;
    let spent_slot = f.cw.store.state.live_notes()[0].slot;
    f.cw.crash_point = Some(CrashPoint::AfterSubmit);
    assert!(f.cw.spend(100_000).await.is_err());
    restart(&mut f)?;
    // No `balance` first: the spend itself must recover and mark the old note spent. On a 4.0 card
    // the change it then holds waits for the registry's signature, so this spend finds nothing to
    // pay with: the spent note is not planned again.
    let err = f.cw.spend(10_000).await.expect_err("only change waiting for the registry");
    assert!(err.to_string().contains("insufficient funds on the card: have 0 msat"), "{err:#}");
    assert!(f.cw.store.state.pending_txs.is_empty(), "recovered");
    assert!(
        f.cw.store.state.notes.iter().filter(|n| n.slot == spent_slot).all(|n| n.status != NoteStatus::Live),
        "the spent note is not live"
    );
    // Once the registry has signed the change, the spend plans from the change only.
    registry_signs(&mut f.cw)?;
    let r = f.cw.spend(10_000).await?;
    assert!(r.inputs.iter().all(|(slot, _)| *slot != spent_slot), "{r:?}");
    assert!(f.cw.store.state.pending_txs.is_empty());
    drop(f.fed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn slot_limits_follow_the_card_size() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    assert_eq!(SLOTS, 32);
    // Loads take slots 1 to 23 (applet 4.0: slot 0 holds the certified attestation key); 24 to 31
    // are for offline change.
    assert_eq!((f.cw.store.state.free_slots().len(), f.cw.store.state.free_change_slots().len()), (23, 8));
    topup(&mut f.cw, &[(1 << 20) + (1 << 18) + (1 << 16)]).await?;
    assert_eq!((f.cw.store.state.free_slots().len(), f.cw.store.state.free_change_slots().len()), (20, 8));
    drop(f.fed);
    Ok(())
}

/// The card as the till's opener hands it over, shared with the test so it can read what the
/// card received.
struct Shared(Rc<RefCell<SoftCard>>);

impl Card for Shared {
    fn gen_key(&mut self, slot: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.0.borrow_mut().gen_key(slot)
    }
    fn gen_key_overwrite(&mut self, slot: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.0.borrow_mut().gen_key_overwrite(slot)
    }
    fn get_pub(&mut self, slot: u8) -> anyhow::Result<XOnlyPublicKey> {
        self.0.borrow_mut().get_pub(slot)
    }
    fn sign(&mut self, slot: u8, msg32: &[u8; 32]) -> anyhow::Result<fedimint_core::secp256k1::schnorr::Signature> {
        self.0.borrow_mut().sign(slot, msg32)
    }
    fn apdu(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.0.borrow_mut().apdu(apdu)
    }
    // What the card's SELECT answers, as a till's transport reads it.
    fn applet_version(&mut self) -> anyhow::Result<fedimint_card_host_card::AppletVersion> {
        self.0.borrow_mut().applet_version()
    }
}

/// A card older than 4.0 (a 2.0 card with eight slots, a 2.1 card, a 3.0 card), keyed and with
/// its PIN as such a card carries them: a top-up, the PIN session a sale opens and the sale itself
/// get the old-card refusal (409 `old_card`). The top-up sends the card no command and records no
/// note; no PIN command reaches the card, sealed or not; the refused sale records nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_older_card_is_refused_before_any_pin_command() -> anyhow::Result<()> {
    let f = fx(true, Amount::from_sats(10_000)).await?;
    for (which, old) in [("2.0 with eight slots", SoftCard::with_slots(8)), ("2.1", SoftCard::new()), ("3.0", SoftCard::v3())] {
        let card = Rc::new(RefCell::new(old));
        {
            let mut c = card.borrow_mut();
            for slot in 0..c.slot_len() {
                c.gen_key(slot)?;
            }
            if !c.version().sign_once() {
                fedimint_card_old_card_pin::open_pin_session(&mut *c)?;
            }
            c.forget_received();
        }
        let dir = tempfile::tempdir()?;
        let store = StateStore::open(dir.path().join("card_state.json"))?;
        let mut cw = CardWallet::new(f.cw.wallet.clone(), store, Box::new(Shared(card.clone())))
            .with_cert_key(Some(fedimint_card_host_card::softkit::test_cert_key_hex()));
        cw.pin = Some(b"1234".to_vec());
        let e = cw.verify_pin_session().expect_err(which);
        assert!(fedimint_card_host_card::is_old_card(&e), "{which}: {e:#}");
        cw.card_setup(false)?;
        card.borrow_mut().forget_received();
        let e = cw.topup(&[1 << 18]).await.expect_err(which);
        assert!(fedimint_card_host_card::is_old_card(&e), "{which}: {e:#}");
        assert!(card.borrow().received_ins().is_empty(), "{which}: the top-up sent {:02x?}", card.borrow().received_ins());
        assert!(cw.store.state.notes.is_empty(), "{which}: the top-up recorded no note");
        let live = cw.store.state.live_total_msat();
        let e = cw.spend(10_000).await.expect_err(which);
        assert!(fedimint_card_host_card::is_old_card(&e), "{which}: {e:#}");
        let c = card.borrow();
        assert!(!c.heard_a_pin_command(), "{which}: the card received {:02x?}", c.received_ins());
        assert!(cw.store.state.pending_txs.is_empty() && cw.store.state.live_total_msat() == live, "{which}: nothing recorded");
    }
    drop(f.fed);
    Ok(())
}

/// A fresh 4.0 card: 32 keys, and a 50,000-sat load (twelve notes) fits with room to spare.
#[tokio::test(flavor = "multi_thread")]
async fn a_50k_load_fits_a_fresh_32_slot_card() -> anyhow::Result<()> {
    let mut f = fx(false, Amount::from_sats(60_000)).await?;
    assert_eq!(f.cw.store.state.slots.len(), 32, "a key in every slot");
    assert_eq!(f.cw.store.state.free_slots().len(), 23, "slots 1 to 23 take loads");
    assert_eq!(plan::binary(50_000_000).len(), 12);
    let notes = topup(&mut f.cw, &[50_000_000]).await?;
    assert_eq!(notes.len(), 12);
    assert_eq!(f.cw.store.state.live_total_msat(), 50_000_000);
    assert_eq!(f.cw.store.state.free_slots().len(), 11);
    // And it spends: the change lands in slots past the eighth.
    let r = f.cw.spend(12_345_000).await?;
    assert_eq!(f.cw.store.state.live_total_msat(), 50_000_000 - 12_345_000 - r.overpay_msat, "{r:?}");
    drop(f.fed);
    Ok(())
}

fn slot_keys(cw: &CardWallet) -> Vec<String> {
    cw.store.state.slots.iter().map(|s| s.pubkey.clone()).collect()
}

/// `--fresh` replaces every key of a 4.0 card not yet certified, and refuses while the state file
/// holds a live note. A certified card keeps slot 0, whose key its certificate names: there
/// `--fresh` is refused and no key changes.
#[tokio::test(flavor = "multi_thread")]
async fn fresh_setup_rekeys_and_refuses_with_live_notes() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    let certified = slot_keys(&f.cw);
    let err = f.cw.card_setup(true).expect_err("a certified card keeps slot 0");
    assert!(err.to_string().contains("re-key slot 0"), "{err}");
    assert_eq!(slot_keys(&f.cw), certified, "no key changed");

    let dir = tempfile::tempdir()?;
    let mut cw = CardWallet::new(f.cw.wallet.clone(), StateStore::open(dir.path().join("card_state.json"))?, Box::new(SoftCard::v4()));
    cw.card_setup(false)?;
    let old = slot_keys(&cw);
    cw.card_setup(true)?;
    let new = slot_keys(&cw);
    assert_eq!(new.len(), SLOTS as usize);
    assert!(old.iter().zip(&new).all(|(a, b)| a != b), "every slot has a new key");
    assert_eq!(cw.store.state.free_slots().len() + cw.store.state.free_change_slots().len(), SLOTS as usize);

    // A live note in the state file: `--fresh` would destroy its key, so it is refused.
    topup(&mut f.cw, &[1 << 18]).await?;
    cw.store.state.notes = f.cw.store.state.notes.clone();
    let err = cw.card_setup(true).expect_err("live note");
    assert!(err.to_string().contains("live or pending"), "{err}");
    assert_eq!(slot_keys(&cw), new, "nothing was touched");
    drop(f.fed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn keys_of_unknown_provenance_are_renewed_lazily_never_used() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    // A card whose slots all hold keys this state file never saw.
    let soft = std::env::var("CARD_BACKEND").map(|b| b == "soft").unwrap_or(true);
    let mut card = if soft { soft_v4()? } else { persistent_card()? };
    let mut foreign = vec![];
    for slot in 0..SLOTS {
        foreign.push(on_card(|| card.get_or_gen(slot))?.to_string());
    }
    let dir = tempfile::tempdir()?;
    let store = StateStore::open(dir.path().join("card_state.json"))?;
    f.cw = CardWallet::new(f.cw.wallet.clone(), store, card).with_cert_key(Some(fedimint_card_host_card::softkit::test_cert_key_hex()));
    f.cw.card_setup(false)?;
    fedimint_card_host_card::softkit::open_test_session(f.cw.card.as_mut())?;
    assert!(f.cw.store.state.free_slots().is_empty(), "unknown keys are not free");
    assert_eq!(f.cw.store.state.overwritable_slots().len(), SLOTS as usize);

    topup(&mut f.cw, &[1 << 18]).await?;
    let used = f.cw.store.state.live_notes()[0].slot;
    assert_ne!(f.cw.store.state.slot_pubkey(used).unwrap(), foreign[used as usize], "the old key was replaced");
    assert!(f.cw.store.state.notes.iter().all(|n| !foreign.iter().any(|k| n.nonce.ends_with(k.as_str()))));
    drop(f.fed);
    Ok(())
}

/// A slot whose note this till holds as spent is re-keyed only once the federation reports that
/// note spent: until then a transaction the federation has not seen (signed offline, say) may
/// still need the key's note.
#[tokio::test(flavor = "multi_thread")]
async fn a_spent_slot_is_renewed_only_once_the_federation_reports_its_note_spent() -> anyhow::Result<()> {
    let mut f = fx(true, Amount::from_sats(10_000)).await?;
    let slot = topup(&mut f.cw, &[1 << 20]).await?[0].0;
    let key = f.cw.card.get_pub(slot)?;
    let held_spent = |f: &mut Fx, status: NoteStatus| {
        let n = f.cw.store.state.notes.iter_mut().find(|n| n.slot == slot && n.nonce == hex_of(nonce_for(key).0.serialize())).unwrap();
        n.status = status;
    };
    held_spent(&mut f, NoteStatus::Spent);
    assert_eq!(f.cw.renew_spent_slots().await?, Vec::<u8>::new(), "the federation has not seen the note spent");
    assert_eq!(f.cw.card.get_pub(slot)?, key, "the key is kept");

    // The federation spends the note: the charge's own renewal now re-keys the slot.
    held_spent(&mut f, NoteStatus::Live);
    let r = f.cw.spend(100_000).await?;
    assert_eq!(r.inputs, vec![(slot, 1 << 20)]);
    assert_ne!(f.cw.card.get_pub(slot)?, key, "renewed once the federation reports the note spent");
    drop(f.fed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn v2_issue_interrupted_on_either_side_of_wallet_commit_resolves_once() -> anyhow::Result<()> {
    let fixtures = Fixtures::new_primary(fedimint_mintv2_client::MintClientInit, fedimint_mintv2_server::MintInit);
    let mut f = fx_card_on(fixtures, false, Amount::from_sats(10_000), soft_v4()?).await?;
    let start = f.cw.wallet.balance_msat().await?;
    let amount = 1 << 17;
    for point in [CrashPoint::IssueBeforeCommit, CrashPoint::IssueAfterCommit] {
        f.cw.crash_point = Some(point);
        assert!(f.cw.topup_ref(&[amount], Some("same-order")).await.is_err());
        assert!(f.cw.store.state.issue_submission.is_some());
        // Reload the durable card state just as the service does on its next start.
        f.cw.store = StateStore::open(f._dir.path().join("card_state.json"))?;
        f.cw.crash_point = None;
        f.cw.balance().await?;
        assert!(f.cw.store.state.issue_submission.is_none());
        if point == CrashPoint::IssueBeforeCommit {
            assert!(f.cw.store.state.issues.is_empty());
            assert!(f.cw.store.state.notes.is_empty());
            assert_eq!(f.cw.wallet.balance_msat().await?, start);
        } else {
            assert_eq!(f.cw.store.state.issues.len(), 1);
            assert_eq!(f.cw.store.state.notes.len(), 1);
            let note = &f.cw.store.state.notes[0];
            assert!(note.signature.is_some());
            assert_eq!(note.txid.as_deref(), Some(f.cw.store.state.issues[0].txid.as_str()));
            assert_eq!(f.cw.wallet.balance_msat().await?, start - amount);
        }
    }
    Ok(())
}
