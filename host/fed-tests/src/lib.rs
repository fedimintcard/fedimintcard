//! Glue for issuing ecash to a card's key and spending it with a card signature.
//!
//! The fedimint client assumes it holds the spend keys (its transaction builder signs with a
//! `Keypair`), so everything here builds `fedimint_core::transaction::Transaction` by hand and
//! submits it through the federation API. The client is used only for its API handle, decoders
//! and config.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use fedimint_api_client::api::{FederationApiExt, ServerError};
use fedimint_api_client::query::FilterMapThreshold;
use fedimint_card_host_card::Card;
use fedimint_client::ClientHandleArc;
use fedimint_core::core::{IntoDynInstance, ModuleInstanceId};
use fedimint_core::encoding::Encodable;
use fedimint_core::module::{AmountUnit, ApiRequestErased};
use fedimint_core::secp256k1::hashes::Hash as _;
use fedimint_core::secp256k1::rand::{self, Rng};
use fedimint_core::secp256k1::{Keypair, Message, Parity, PublicKey, SECP256K1, XOnlyPublicKey, schnorr};
use fedimint_core::transaction::{
    Transaction, TransactionError, TransactionSignature, TransactionSubmissionOutcome,
};
use fedimint_core::{IdxRange, NumPeersExt, OutPointRange, PeerId, TransactionId};
use fedimint_dummy_common::DummyInput;
use fedimint_dummy_common::config::DummyClientConfig;
use fedimint_mintv2_client::issuance::NoteIssuanceRequest;
use fedimint_mintv2_client::{ECash, FinalReceiveOperationState, MintClientModule, SpendableNote};
use fedimint_mintv2_common::config::MintClientConfig;
use fedimint_mintv2_common::endpoint_constants::SIGNATURE_SHARES_ENDPOINT;
use fedimint_mintv2_common::{Denomination, MintInput, MintOutput, Note, nonce_message, verify_note};
use fedimint_testing::federation::FederationTest;
use fedimint_testing::fixtures::Fixtures;
use tbs::{BlindedSignatureShare, BlindingKey};

pub use fedimint_card_host_card as card;

/// A running 4-guardian federation, one test client, and the ids/config the glue needs.
pub struct Env {
    /// Keep alive: dropping it stops the guardians.
    pub fed: FederationTest,
    pub client: ClientHandleArc,
    pub mint_id: ModuleInstanceId,
    pub dummy_id: ModuleInstanceId,
    pub mint_cfg: MintClientConfig,
}

/// Start a federation. `fees = false` uses `FeeConsensus::zero()` for exact-split spends.
pub async fn setup(fees: bool) -> Env {
    use fedimint_dummy_client::DummyClientInit;
    use fedimint_dummy_server::DummyInit;
    use fedimint_mintv2_client::MintClientInit;
    use fedimint_mintv2_server::MintInit;

    let fixtures = Fixtures::new_primary(MintClientInit, MintInit).with_module(DummyClientInit, DummyInit);
    let mut builder = fixtures.new_fed_builder(0);
    if !fees {
        builder = builder.disable_mint_fees();
    }
    let fed = builder.build().await;
    let client = fed.new_client().await;
    let cfg = client.config().await;
    let (mint_id, mint_cfg) = cfg
        .get_first_module_by_kind::<MintClientConfig>(fedimint_mintv2_common::KIND)
        .expect("mintv2 module");
    let mint_cfg = mint_cfg.clone();
    let (dummy_id, _) = cfg
        .get_first_module_by_kind::<DummyClientConfig>(fedimint_dummy_common::KIND)
        .expect("dummy module");
    Env { fed, client, mint_id, dummy_id, mint_cfg }
}

/// The nonce for a card key. Parity is fixed to even so the 33 bytes hashed into the BLS
/// message at issue are exactly the 33 bytes the federation sees at spend.
pub fn nonce_for(card_key: XOnlyPublicKey) -> PublicKey {
    PublicKey::from_x_only_public_key(card_key, Parity::Even)
}

/// Run a blocking card call from inside a multi-thread tokio test.
pub fn on_card<R>(f: impl FnOnce() -> R) -> R {
    tokio::task::block_in_place(f)
}

fn sign_with_key(tx: &mut Transaction, keys: &[Keypair]) {
    let msg = Message::from_digest(tx.tx_hash().to_byte_array());
    tx.signatures = TransactionSignature::NaiveMultisig(
        keys.iter().map(|k| SECP256K1.sign_schnorr(&msg, k)).collect(),
    );
}

pub async fn submit(env: &Env, tx: Transaction) -> Result<Result<TransactionId, TransactionError>> {
    let outcome = env
        .client
        .api()
        .submit_transaction(tx)
        .await
        .try_into_inner(env.client.decoders())
        .context("decoding submission outcome")?;
    let TransactionSubmissionOutcome(res) = outcome;
    Ok(res)
}

/// Fetch a threshold of verified signature shares for `n` outputs of `txid` (starting at output
/// 0), then combine them. `blinded[i]` is the (denomination, blinded message) of output `i`.
pub async fn fetch_blind_signatures(
    env: &Env,
    txid: TransactionId,
    blinded: Vec<(Denomination, tbs::BlindedMessage)>,
) -> Result<Vec<tbs::BlindedSignature>> {
    let n = blinded.len();
    let range = OutPointRange::new(txid, IdxRange::from(0..n as u64));
    let tbs_pks = env.mint_cfg.tbs_pks.clone();
    let check = blinded.clone();
    let mod_api = env.client.api().with_module(env.mint_id);
    let shares: BTreeMap<PeerId, Vec<BlindedSignatureShare>> = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        mod_api.request_with_strategy_retry(
            FilterMapThreshold::new(
                move |peer, shares: Vec<BlindedSignatureShare>| {
                    let res = if shares.len() != check.len() {
                        Err(ServerError::InvalidResponse("wrong number of shares".into()))
                    } else if check.iter().zip(&shares).all(|((d, m), s)| {
                        tbs::verify_signature_share(*m, *s, tbs_pks[d][&peer])
                    }) {
                        Ok(shares)
                    } else {
                        Err(ServerError::InvalidResponse("invalid signature share".into()))
                    };
                    std::future::ready(res)
                },
                mod_api.all_peers().to_num_peers(),
            ),
            SIGNATURE_SHARES_ENDPOINT.to_owned(),
            ApiRequestErased::new(range),
        ),
    )
    .await
    .context("timed out waiting for signature shares")?;
    Ok((0..n)
        .map(|i| {
            tbs::aggregate_signature_shares(
                &shares.iter().map(|(p, v)| (p.to_usize() as u64, v[i])).collect(),
            )
        })
        .collect())
}

/// Step 4: issue one note of `denom` whose nonce is the card's key.
///
/// Funded from nothing by a hand-built `[DummyInput] -> [MintOutput]` transaction (the dummy
/// module mints value from thin air and accepts any key).
pub async fn issue_to_card(env: &Env, card_key: XOnlyPublicKey, denom: Denomination) -> Result<Note> {
    let nonce = nonce_for(card_key);
    let blinding_key = BlindingKey::random();
    let blinded = tbs::blind_message(nonce_message(nonce), blinding_key);
    // The tweak only serves client-side recovery; the server does not check it.
    let output = MintOutput::new_v0(denom, blinded, [0; 16]);

    let amount = denom.amount() + env.mint_cfg.fee_consensus.fee(denom.amount())
        + fedimint_core::Amount::from_msats(1000);
    let throwaway = Keypair::new(SECP256K1, &mut rand::thread_rng());
    let input = DummyInput { amount, unit: AmountUnit::BITCOIN, pub_key: throwaway.public_key() };

    let mut tx = Transaction {
        inputs: vec![input.into_dyn(env.dummy_id)],
        outputs: vec![output.into_dyn(env.mint_id)],
        nonce: rand::thread_rng().r#gen(),
        signatures: TransactionSignature::NaiveMultisig(vec![]),
    };
    sign_with_key(&mut tx, &[throwaway]);

    let txid = submit(env, tx).await?.map_err(|e| anyhow!("issuance rejected: {e}"))?;
    let sigs = fetch_blind_signatures(env, txid, vec![(denom, blinded)]).await?;
    let note = Note { denomination: denom, nonce, signature: tbs::unblind_signature(blinding_key, sigs[0]) };
    if !verify_note(note, env.mint_cfg.tbs_agg_pks[&denom]) {
        bail!("issued note does not verify");
    }
    Ok(note)
}

/// A spend transaction ready for a signature, plus the host-owned outputs it creates.
pub struct SpendDraft {
    pub tx: Transaction,
    pub requests: Vec<NoteIssuanceRequest>,
}

impl SpendDraft {
    /// The 32 bytes the card must sign: the transaction id, with no extra tag.
    pub fn digest(&self) -> [u8; 32] {
        self.tx.tx_hash().to_byte_array()
    }

    /// Consensus encoding of the whole transaction (signatures included).
    pub fn encoded(tx: &Transaction) -> Vec<u8> {
        tx.consensus_encode_to_vec()
    }
}

/// Step 5 part 1: `[card note] -> [host notes of out_denoms]`, unsigned.
pub fn draft_spend(env: &Env, note: Note, out_denoms: &[u8]) -> SpendDraft {
    let mut rng = rand::thread_rng();
    let requests: Vec<NoteIssuanceRequest> = out_denoms
        .iter()
        .map(|d| NoteIssuanceRequest {
            denomination: Denomination(*d),
            tweak: rng.r#gen(),
            keypair: Keypair::new(SECP256K1, &mut rng),
            blinding_key: BlindingKey::random(),
        })
        .collect();
    let tx = Transaction {
        inputs: vec![MintInput::new_v0(note).into_dyn(env.mint_id)],
        outputs: requests.iter().map(|r| r.output().into_dyn(env.mint_id)).collect(),
        nonce: rng.r#gen(),
        signatures: TransactionSignature::NaiveMultisig(vec![]),
    };
    SpendDraft { tx, requests }
}

pub fn attach_signatures(tx: &mut Transaction, sigs: Vec<schnorr::Signature>) {
    tx.signatures = TransactionSignature::NaiveMultisig(sigs);
}

/// Ask the card to sign the transaction id and check the signature on the host.
pub fn card_sign(card: &mut dyn Card, slot: u8, draft: &SpendDraft, key: XOnlyPublicKey) -> Result<schnorr::Signature> {
    let digest = draft.digest();
    let sig = on_card(|| card.sign(slot, &digest))?;
    SECP256K1
        .verify_schnorr(&sig, &Message::from_digest(digest), &key)
        .context("card signature does not verify on the host")?;
    Ok(sig)
}

/// Step 5 part 2: after acceptance, fetch signatures for the host's new notes.
pub async fn finalize_outputs(env: &Env, txid: TransactionId, requests: &[NoteIssuanceRequest]) -> Result<Vec<SpendableNote>> {
    let sigs = fetch_blind_signatures(
        env,
        txid,
        requests.iter().map(|r| (r.denomination, r.blinded_message())).collect(),
    )
    .await?;
    let notes: Vec<SpendableNote> = requests.iter().zip(sigs).map(|(r, s)| r.finalize(s)).collect();
    Ok(notes)
}

/// Hand notes to the test client the normal way. Success means the notes are spendable.
pub async fn redeem(env: &Env, notes: Vec<SpendableNote>) -> Result<()> {
    let mint = env.client.get_first_module::<MintClientModule>()?;
    let ecash = ECash::new(env.client.federation_id(), notes);
    let op = mint.receive(ecash, serde_json::Value::Null).await?;
    let state = mint.await_final_receive_operation_state(op).await?;
    if state != FinalReceiveOperationState::Success {
        bail!("receive ended in {state:?}");
    }
    Ok(())
}

/// Wait until the client's balance reaches `at_least`. Receiving first spends the notes in a
/// transaction, then issues the client's own fresh notes, so the balance lags `Success`.
pub async fn wait_for_balance(env: &Env, at_least: fedimint_core::Amount) -> Result<fedimint_core::Amount> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let bal = env.client.get_balance_for_btc().await?;
        if bal >= at_least {
            return Ok(bal);
        }
        if std::time::Instant::now() > deadline {
            bail!("balance stuck at {bal}, wanted {at_least}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
