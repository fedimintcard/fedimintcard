//! Steps 5 and 6: spend a card note with a card signature, and negative checks.
//!
//! Runs against `CARD_BACKEND` (soft by default; bridge and pcsc without code changes).

use fedimint_card_fed_tests::*;
use fedimint_card_host_card::{Card, open_from_env};
use fedimint_core::Amount;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::secp256k1::XOnlyPublicKey;
use fedimint_core::transaction::{Transaction, TransactionError};
use fedimint_mintv2_common::{Denomination, Note};

const SLOT: u8 = 0;
const OTHER_SLOT: u8 = 1;

struct Fx {
    env: Env,
    card: Box<dyn Card>,
    key: XOnlyPublicKey,
    note: Note,
}

async fn fx(fees: bool, denom: u8) -> anyhow::Result<Fx> {
    let env = setup(fees).await;
    let mut card = open_from_env()?;
    let key = on_card(|| card.get_or_gen(SLOT))?;
    let note = issue_to_card(&env, key, Denomination(denom)).await?;
    Ok(Fx { env, card, key, note })
}

fn rejected(res: anyhow::Result<Result<fedimint_core::TransactionId, TransactionError>>) -> TransactionError {
    res.expect("submission call").expect_err("federation should reject")
}

fn input_err_contains(e: &TransactionError, needle: &str) {
    assert!(matches!(e, TransactionError::Input(_)), "expected Input error, got {e}");
    assert!(e.to_string().contains(needle), "{e}");
}

/// Step 5, fees on: input 2^20 msat, outputs 2^19+2^18+2^17+2^16, the rest is overpaid fee.
#[tokio::test(flavor = "multi_thread")]
async fn card_note_is_spent_with_card_signature_fees_on() -> anyhow::Result<()> {
    let mut f = fx(true, 20).await?;
    let mut draft = draft_spend(&f.env, f.note, &[19, 18, 17, 16]);
    let sig = card_sign(f.card.as_mut(), SLOT, &draft, f.key)?;
    attach_signatures(&mut draft.tx, vec![sig]);

    // Finding for SPIKE.md: the exact bytes the federation receives.
    let bytes = draft.tx.consensus_encode_to_vec();
    println!("SPEND_TX_BYTES {}", bytes.len());
    println!("SPEND_TX_HEX {}", hex(&bytes));
    println!("SPEND_TXID {}", hex(&draft.digest()));
    let back = Transaction::consensus_decode_whole(&bytes, f.env.client.decoders())?;
    assert_eq!(back.tx_hash(), draft.tx.tx_hash(), "encoding round-trips");

    let txid = submit(&f.env, draft.tx.clone()).await?.map_err(|e| anyhow::anyhow!("rejected: {e}"))?;
    assert_eq!(txid, draft.tx.tx_hash());

    let notes = finalize_outputs(&f.env, txid, &draft.requests).await?;
    assert_eq!(notes.len(), 4);
    let before = f.env.client.get_balance_for_btc().await?;
    redeem(&f.env, notes).await?;
    // Fees on: the client pays input and output fees (100 msat each) to reissue the notes.
    let after = wait_for_balance(&f.env, Amount::from_msats(1)).await?;
    assert!(after > before, "new host notes are spendable: {before} -> {after}");
    assert!(after <= Amount::from_msats((1 << 19) + (1 << 18) + (1 << 17) + (1 << 16)));
    Ok(())
}

/// Step 5, fees off: the input splits exactly in two and the balance matches to the msat.
#[tokio::test(flavor = "multi_thread")]
async fn card_note_is_spent_exact_split_fees_off() -> anyhow::Result<()> {
    let mut f = fx(false, 20).await?;
    let mut draft = draft_spend(&f.env, f.note, &[19, 19]);
    let sig = card_sign(f.card.as_mut(), SLOT, &draft, f.key)?;
    attach_signatures(&mut draft.tx, vec![sig]);
    let txid = submit(&f.env, draft.tx.clone()).await?.map_err(|e| anyhow::anyhow!("rejected: {e}"))?;
    let notes = finalize_outputs(&f.env, txid, &draft.requests).await?;
    redeem(&f.env, notes).await?;
    assert_eq!(wait_for_balance(&f.env, Amount::from_msats(1 << 20)).await?, Amount::from_msats(1 << 20));
    Ok(())
}

/// Step 6: a signature from another key is rejected.
#[tokio::test(flavor = "multi_thread")]
async fn signature_from_another_key_is_rejected() -> anyhow::Result<()> {
    let mut f = fx(true, 20).await?;
    let other_key = on_card(|| f.card.get_or_gen(OTHER_SLOT))?;
    assert_ne!(other_key, f.key);
    let mut draft = draft_spend(&f.env, f.note, &[19, 18]);
    let bad = card_sign(f.card.as_mut(), OTHER_SLOT, &draft, other_key)?;
    attach_signatures(&mut draft.tx, vec![bad]);
    let e = rejected(submit(&f.env, draft.tx.clone()).await);
    assert!(matches!(e, TransactionError::InvalidSignature { .. }), "{e}");

    // Rejection must not burn the note: the right key can still spend it.
    let good = card_sign(f.card.as_mut(), SLOT, &draft, f.key)?;
    attach_signatures(&mut draft.tx, vec![good]);
    submit(&f.env, draft.tx).await?.map_err(|e| anyhow::anyhow!("rejected: {e}"))?;
    Ok(())
}

/// Step 6: no signature, or too many, is rejected.
#[tokio::test(flavor = "multi_thread")]
async fn missing_or_extra_signature_is_rejected() -> anyhow::Result<()> {
    let mut f = fx(true, 20).await?;
    let mut draft = draft_spend(&f.env, f.note, &[19, 18]);
    let sig = card_sign(f.card.as_mut(), SLOT, &draft, f.key)?;

    attach_signatures(&mut draft.tx, vec![]);
    let e = rejected(submit(&f.env, draft.tx.clone()).await);
    assert!(matches!(e, TransactionError::InvalidWitnessLength), "{e}");

    attach_signatures(&mut draft.tx, vec![sig, sig]);
    let e = rejected(submit(&f.env, draft.tx.clone()).await);
    assert!(matches!(e, TransactionError::InvalidWitnessLength), "{e}");
    Ok(())
}

/// Step 6: changing an output (or the nonce) after signing is rejected.
#[tokio::test(flavor = "multi_thread")]
async fn changing_the_transaction_after_signing_is_rejected() -> anyhow::Result<()> {
    let mut f = fx(true, 20).await?;
    let mut draft = draft_spend(&f.env, f.note, &[19, 18]);
    let sig = card_sign(f.card.as_mut(), SLOT, &draft, f.key)?;
    attach_signatures(&mut draft.tx, vec![sig]);

    // Swap output 0 for a different one (an attacker redirecting the value).
    let thief = draft_spend(&f.env, f.note, &[19]);
    let mut swapped = draft.tx.clone();
    swapped.outputs[0] = thief.tx.outputs[0].clone();
    let e = rejected(submit(&f.env, swapped).await);
    assert!(matches!(e, TransactionError::InvalidSignature { .. }), "swap: {e}");

    // Append an output.
    let mut extra = draft.tx.clone();
    extra.outputs.push(thief.tx.outputs[0].clone());
    let e = rejected(submit(&f.env, extra).await);
    assert!(matches!(e, TransactionError::InvalidSignature { .. }), "extra: {e}");

    // Change the nonce.
    let mut renonced = draft.tx.clone();
    renonced.nonce[0] ^= 1;
    let e = rejected(submit(&f.env, renonced).await);
    assert!(matches!(e, TransactionError::InvalidSignature { .. }), "nonce: {e}");

    // Control: the untouched transaction is accepted, so the three failures above were about
    // the tampering and not about the note.
    submit(&f.env, draft.tx).await?.map_err(|e| anyhow::anyhow!("rejected: {e}"))?;
    Ok(())
}

/// Step 6: a second spend of the same note is rejected, even as a fresh, validly signed tx.
#[tokio::test(flavor = "multi_thread")]
async fn second_spend_of_the_same_note_is_rejected() -> anyhow::Result<()> {
    let mut f = fx(true, 20).await?;
    let mut first = draft_spend(&f.env, f.note, &[19, 18]);
    let sig = card_sign(f.card.as_mut(), SLOT, &first, f.key)?;
    attach_signatures(&mut first.tx, vec![sig]);
    submit(&f.env, first.tx.clone()).await?.map_err(|e| anyhow::anyhow!("rejected: {e}"))?;

    // Trap: resubmitting the identical transaction is idempotent and returns Ok(txid).
    let again = submit(&f.env, first.tx.clone()).await?;
    assert_eq!(again, Ok(first.tx.tx_hash()), "identical resubmission is idempotent");

    // A new transaction (fresh nonce, fresh outputs) spending the same note is a double spend.
    let mut second = draft_spend(&f.env, f.note, &[19, 18]);
    let sig = card_sign(f.card.as_mut(), SLOT, &second, f.key)?;
    attach_signatures(&mut second.tx, vec![sig]);
    assert_ne!(second.tx.tx_hash(), first.tx.tx_hash());
    let e = rejected(submit(&f.env, second.tx).await);
    input_err_contains(&e, "already spent");
    Ok(())
}

/// Controls: the same checks fire for the reasons we think they do.
#[tokio::test(flavor = "multi_thread")]
async fn controls_bad_bls_signature_unbalanced_and_wrong_denomination() -> anyhow::Result<()> {
    let mut f = fx(true, 20).await?;

    // Bad BLS signature: right key and denomination, signature copied from another note.
    let other_key = on_card(|| f.card.get_or_gen(OTHER_SLOT))?;
    let other_note = issue_to_card(&f.env, other_key, Denomination(20)).await?;
    let forged = Note { signature: other_note.signature, ..f.note };
    let mut d = draft_spend(&f.env, forged, &[19, 18]);
    let sig = card_sign(f.card.as_mut(), SLOT, &d, f.key)?;
    attach_signatures(&mut d.tx, vec![sig]);
    input_err_contains(&rejected(submit(&f.env, d.tx).await), "invalid signature");

    // Wrong denomination: not one the federation issues.
    let bad_denom = Note { denomination: Denomination(200), ..f.note };
    let mut d = draft_spend(&f.env, bad_denom, &[19, 18]);
    let sig = card_sign(f.card.as_mut(), SLOT, &d, f.key)?;
    attach_signatures(&mut d.tx, vec![sig]);
    input_err_contains(&rejected(submit(&f.env, d.tx).await), "invalid amount");

    // Unbalanced: outputs worth more than the input.
    let mut d = draft_spend(&f.env, f.note, &[20, 20]);
    let sig = card_sign(f.card.as_mut(), SLOT, &d, f.key)?;
    attach_signatures(&mut d.tx, vec![sig]);
    let e = rejected(submit(&f.env, d.tx).await);
    assert!(matches!(e, TransactionError::UnbalancedTransaction { .. }), "{e}");
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
