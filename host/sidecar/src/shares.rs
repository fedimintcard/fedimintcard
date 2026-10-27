//! Blind signature shares for mint v1 outputs: fetch a threshold of verified shares from the
//! guardians, aggregate, unblind, and check the finished note against the federation key.

use std::collections::BTreeMap;
use std::future::ready;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use fedimint_api_client::api::{FederationApiExt, SerdeOutputOutcome, ServerError};
use fedimint_api_client::query::FilterMapThreshold;
use fedimint_core::module::ApiRequestErased;
use fedimint_core::{Amount, NumPeersExt, OutPoint, PeerId, TransactionId};
use fedimint_mint_client::output::verify_blind_share;
// Deprecated upstream in favour of the batch endpoint, but the single-output one works against
// every federation version, which matters for an unknown demo federation.
#[allow(deprecated)]
use fedimint_core::endpoint_constants::AWAIT_OUTPUT_OUTCOME_ENDPOINT;
use fedimint_mint_common::{Nonce, Note};
use tbs::{BlindedMessage, BlindedSignature, BlindedSignatureShare, BlindingKey};

use crate::wallet::Wallet;

/// How long to wait for one output's shares during a normal flow.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// What the guardians must have signed: output `out_idx` of a transaction.
#[derive(Debug, Clone, Copy)]
pub struct OutSpec {
    pub out_idx: u64,
    pub amount: Amount,
    pub blinded: BlindedMessage,
}

/// Fetch a threshold of valid shares for one output and aggregate them into a blind signature.
pub async fn fetch_blind_signature(
    w: &Wallet,
    txid: TransactionId,
    spec: OutSpec,
    timeout: Duration,
) -> Result<BlindedSignature> {
    // `await_output_outcome` is a federation-wide endpoint, not a mint module one.
    let api = w.client.api();
    let decoder = w.client.decoders().get_expect(w.mint_id).clone();
    let peer_pks = w.mint_cfg.peer_tbs_pks.clone();
    let OutSpec { amount, blinded, out_idx } = spec;
    let shares: BTreeMap<PeerId, BlindedSignatureShare> = tokio::time::timeout(
        timeout,
        api.request_with_strategy_retry(
            FilterMapThreshold::new(
                move |peer, outcome: SerdeOutputOutcome| {
                    ready(
                        verify_blind_share(peer, &outcome, amount, blinded, &decoder, &peer_pks)
                            .map_err(|e| ServerError::InvalidResponse(e.to_string().into())),
                    )
                },
                api.all_peers().to_num_peers(),
            ),
            #[allow(deprecated)]
            AWAIT_OUTPUT_OUTCOME_ENDPOINT.to_owned(),
            ApiRequestErased::new(OutPoint { txid, out_idx }),
        ),
    )
    .await
    .with_context(|| format!("timed out waiting for signature shares of output {out_idx}"))?;
    Ok(tbs::aggregate_signature_shares(
        &shares.into_iter().map(|(p, s)| (p.to_usize() as u64, s)).collect(),
    ))
}

/// Unblind `sig` and check the resulting note against the federation's key for `amount`.
pub fn finish_note(
    w: &Wallet,
    amount: Amount,
    nonce: Nonce,
    blinding_key: BlindingKey,
    blinded: BlindedMessage,
    sig: BlindedSignature,
) -> Result<tbs::Signature> {
    let pk = *w.mint_cfg.tbs_pks.tier(&amount).map_err(|_| anyhow::anyhow!("{amount} is not a denomination of this federation"))?;
    if !tbs::verify_blinded_signature(blinded, sig, pk) {
        bail!("aggregated blind signature does not verify");
    }
    let signature = tbs::unblind_signature(blinding_key, sig);
    if !(Note { nonce, signature }).verify(pk) {
        bail!("unblinded note does not verify");
    }
    Ok(signature)
}

/// The blinded message for a nonce under a blinding key.
pub fn blind(nonce: Nonce, key: BlindingKey) -> BlindedMessage {
    tbs::blind_message(nonce.to_message(), key)
}
