//! Blind signature shares for mint outputs: fetch a threshold of verified shares from the
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
use crate::wallet::MintVersion;
#[allow(deprecated)]
use fedimint_core::endpoint_constants::AWAIT_OUTPUT_OUTCOME_ENDPOINT;
use fedimint_mint_common::Nonce;
use tbs::{BlindedMessage, BlindedSignature, BlindedSignatureShare, BlindingKey};

use crate::wallet::Wallet;

// Moved to the core the POS phones share (docs/POS-OFFLINE.md); kept at the old path.
pub use fedimint_card_pos_core::blind;

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
pub async fn fetch_blind_signature(w: &Wallet, txid: TransactionId, spec: OutSpec, timeout: Duration) -> Result<BlindedSignature> {
    if w.mint_version == MintVersion::V2 {
        let range = fedimint_core::OutPointRange::new(
            txid,
            fedimint_core::IdxRange::from(spec.out_idx..spec.out_idx.checked_add(1).context("output index exceeds range")?),
        );
        let api = w.client.api().with_module(w.mint_id);
        let peer_pks = w.mint_cfg.peer_tbs_pks.clone();
        let shares: BTreeMap<PeerId, BlindedSignatureShare> = tokio::time::timeout(
            timeout,
            api.request_with_strategy_retry(
                FilterMapThreshold::new(
                    move |peer, shares: Vec<BlindedSignatureShare>| {
                        ready(match shares.as_slice() {
                            [share]
                                if peer_pks
                                    .get(&peer)
                                    .and_then(|tiers| tiers.get(spec.amount))
                                    .is_some_and(|pk| tbs::verify_signature_share(spec.blinded, *share, *pk)) =>
                            {
                                Ok(*share)
                            }
                            _ => Err(ServerError::InvalidResponse("invalid mint signature share".into())),
                        })
                    },
                    api.all_peers().to_num_peers(),
                ),
                fedimint_mintv2_common::endpoint_constants::SIGNATURE_SHARES_ENDPOINT.to_owned(),
                ApiRequestErased::new(range),
            ),
        )
        .await
        .context("timed out waiting for mint v2 signature shares")?;
        return Ok(tbs::aggregate_signature_shares(&shares.into_iter().map(|(p, s)| (p.to_usize() as u64, s)).collect()));
    }
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
                            .map_err(|e| ServerError::InvalidResponse(e.to_string())),
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
    Ok(tbs::aggregate_signature_shares(&shares.into_iter().map(|(p, s)| (p.to_usize() as u64, s)).collect()))
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
    if !w.mint_version.verify(nonce, signature, pk) {
        bail!("unblinded note does not verify");
    }
    Ok(signature)
}

/// The finished signature of a card note at `outpoint`: fetch the guardians' shares for its
/// blinded nonce, aggregate, unblind and verify. Both the till's `topup` and the holder loader
/// (`holder.rs`) end a load with this.
pub async fn note_signature(
    w: &Wallet,
    outpoint: OutPoint,
    amount: Amount,
    nonce: Nonce,
    key: BlindingKey,
    timeout: Duration,
) -> Result<tbs::Signature> {
    let blinded = w.mint_version.blind(nonce, key);
    let sig = fetch_blind_signature(w, outpoint.txid, OutSpec { out_idx: outpoint.out_idx, amount, blinded }, timeout).await?;
    finish_note(w, amount, nonce, key, blinded, sig)
}
