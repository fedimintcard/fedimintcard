//! Per-federation Bitcoin mint selection and the stock v2 eCash format.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use fedimint_card_pos_core::mint::MintFee;
use fedimint_client::ClientHandleArc;
use fedimint_core::base32::{FEDIMINT_PREFIX, decode_prefixed, encode_prefixed};
use fedimint_core::config::ClientConfig;
use fedimint_core::core::{ModuleInstanceId, OperationId};
use fedimint_core::module::CommonModuleInit;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::{Amount, PeerId, Tiered};
use fedimint_mint_common::config::MintClientConfig;
use fedimint_mintv2_client::{ECash, FinalReceiveOperationState, MintClientModule, ReceiveECashError};
use fedimint_mintv2_common::config::MintClientConfig as V2Config;
use serde_json::Value;
use tbs::{AggregatePublicKey, PublicKeyShare};

use super::{Imported, MintVersion, Wallet};

/// The federation's own configurations cannot supply a supported Bitcoin mint.
#[derive(Debug)]
pub struct UnsupportedMint;

impl std::fmt::Display for UnsupportedMint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("This till needs a federation with a supported Bitcoin mint v1 or mint v2 configuration.")
    }
}

impl std::error::Error for UnsupportedMint {}

/// The federation advertises a supported Bitcoin mint, but its API could not be negotiated.
#[derive(Debug)]
pub struct MintUnavailable;

impl std::fmt::Display for MintUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The till could not connect to this federation’s Bitcoin mint. Check the guardian connection and retry.")
    }
}

impl std::error::Error for MintUnavailable {}

/// Keys and fees used by card planners. Both mint protocols expose one aggregate
/// key per denomination and one key share per guardian and denomination.
#[derive(Clone)]
pub struct CardMintConfig {
    pub tbs_pks: Tiered<AggregatePublicKey>,
    pub peer_tbs_pks: BTreeMap<PeerId, Tiered<PublicKeyShare>>,
    pub fee_consensus: MintFee,
}

impl CardMintConfig {
    pub(super) fn for_client(client: &ClientHandleArc, cfg: &ClientConfig) -> Result<(MintVersion, ModuleInstanceId, Self)> {
        let (version, id) = bitcoin_mint(cfg)?;
        // The SDK also omits modules when version discovery temporarily cannot reach quorum.
        let module = client.get_module_client_dyn(id).map_err(|_| MintUnavailable)?;
        let config = match version {
            MintVersion::V1 => {
                module
                    .as_any()
                    .downcast_ref::<fedimint_mint_client::MintClientModule>()
                    .context("the Bitcoin mint v1 client is unavailable")?;
                let v1 = cfg.get_module::<MintClientConfig>(id)?;
                Self {
                    tbs_pks: v1.tbs_pks.clone(),
                    peer_tbs_pks: v1.peer_tbs_pks.clone(),
                    fee_consensus: MintFee::V1(v1.fee_consensus.clone()),
                }
            }
            MintVersion::V2 => {
                module.as_any().downcast_ref::<MintClientModule>().context("the Bitcoin mint v2 client is unavailable")?;
                let v2 = cfg.get_module::<V2Config>(id)?;
                let mut peers: BTreeMap<PeerId, Vec<(Amount, PublicKeyShare)>> = BTreeMap::new();
                for (denom, keys) in &v2.tbs_pks {
                    ensure!(denom.0 < 42, "unrecognized mint v2 denomination");
                    for (peer, key) in keys {
                        peers.entry(*peer).or_default().push((denom.amount(), *key));
                    }
                }
                ensure!(v2.tbs_agg_pks.keys().all(|d| d.0 < 42), "unrecognized mint v2 denomination");
                Self {
                    tbs_pks: v2.tbs_agg_pks.iter().map(|(d, k)| (d.amount(), *k)).collect(),
                    peer_tbs_pks: peers.into_iter().map(|(p, ks)| (p, ks.into_iter().collect())).collect(),
                    fee_consensus: MintFee::V2(v2.fee_consensus.clone()),
                }
            }
        };
        Ok((version, id, config))
    }
}

/// Prefer the original v1 mint when both kinds are advertised. For mint v2
/// select a Bitcoin instance by its unit, since other asset mints can coexist.
pub(super) fn bitcoin_mint(cfg: &ClientConfig) -> Result<(MintVersion, ModuleInstanceId)> {
    if let Some((&id, module)) = cfg.modules.iter().find(|(_, m)| m.is_kind(&fedimint_mint_common::KIND)) {
        let decoders: ModuleDecoderRegistry =
            [(id, fedimint_mint_common::KIND, fedimint_mint_common::MintCommonInit::decoder())].into_iter().collect();
        let decoded = module.clone().redecode_raw(&decoders).map_err(|_| UnsupportedMint)?;
        decoded.cast::<MintClientConfig>().map_err(|_| UnsupportedMint)?;
        return Ok((MintVersion::V1, id));
    }
    for (&id, module) in &cfg.modules {
        if module.is_kind(&fedimint_mintv2_common::KIND) {
            // A preview and a config read directly from the DB both contain raw module data.
            let decoders: ModuleDecoderRegistry =
                [(id, fedimint_mintv2_common::KIND, fedimint_mintv2_common::MintCommonInit::decoder())].into_iter().collect();
            let m = module.clone().redecode_raw(&decoders).map_err(|_| UnsupportedMint)?;
            if m.cast::<V2Config>().map_err(|_| UnsupportedMint)?.amount_unit.is_bitcoin() {
                return Ok((MintVersion::V2, id));
            }
        }
    }
    Err(UnsupportedMint.into())
}

/// Canonical interoperable encoding of a mint v2 bearer token.
pub fn encode_v2_ecash(notes: &ECash) -> String {
    encode_prefixed(FEDIMINT_PREFIX, notes)
}

impl Wallet {
    pub fn mint_v2(&self) -> Result<&MintClientModule> {
        self.client.get_module_client_dyn(self.mint_id)?.as_any().downcast_ref::<MintClientModule>().context("no Bitcoin mint v2 module")
    }

    pub fn decode_v2_ecash(&self, text: &str) -> Result<ECash> {
        let notes: ECash = decode_prefixed(FEDIMINT_PREFIX, text.trim()).context("invalid mint v2 eCash")?;
        self.validate_v2_ecash(&notes)?;
        Ok(notes)
    }

    fn validate_v2_ecash(&self, ecash: &ECash) -> Result<Amount> {
        ensure!(self.mint_version == MintVersion::V2 && ecash.unit().is_bitcoin(), "eCash is not from the Bitcoin mint v2 module");
        ensure!(ecash.mint() == Some(self.federation_id()), "eCash is from another federation");
        let notes = ecash.notes();
        ensure!(!notes.is_empty(), "empty eCash");
        let mut seen = BTreeSet::new();
        let mut total = 0u64;
        for note in notes {
            ensure!(note.denomination.0 < 42, "unrecognized eCash denomination");
            let key = self.mint_cfg.tbs_pks.get(note.amount()).context("unknown eCash denomination")?;
            let nonce = note.keypair.public_key();
            ensure!(seen.insert(nonce), "duplicate eCash note");
            ensure!(
                fedimint_mintv2_common::verify_note(
                    fedimint_mintv2_common::Note { denomination: note.denomination, nonce, signature: note.signature },
                    *key
                ),
                "invalid eCash signature"
            );
            total = total.checked_add(note.amount().msats).context("eCash amount overflow")?;
        }
        Ok(Amount::from_msats(total))
    }

    /// The mint's receive state records transaction acceptance. A merchant
    /// balance is credited only after the change notes have finished issuance.
    pub async fn v2_received(&self, op: OperationId) -> Result<bool> {
        match self.mint_v2()?.await_final_receive_operation_state(op).await? {
            FinalReceiveOperationState::Rejected => return Ok(false),
            FinalReceiveOperationState::Success => {}
        }
        let entry = self.client.operation_log().get_operation(op).await.context("mint v2 receive operation is missing")?;
        match entry.try_meta::<fedimint_mintv2_client::MintOperationMeta>()? {
            fedimint_mintv2_client::MintOperationMeta::Receive { change_outpoint_range, .. } => {
                self.client.await_primary_bitcoin_module_outputs(op, change_outpoint_range.into_iter().collect()).await?;
                Ok(true)
            }
            _ => anyhow::bail!("operation is not a mint v2 receive"),
        }
    }

    pub async fn import_v2_notes(&self, notes: ECash) -> Result<Imported> {
        let face = self.validate_v2_ecash(&notes)?;
        let mint = self.mint_v2()?;
        let existing_op = OperationId::from_encodable(&notes);
        let (op, already) = match mint.receive(notes, Value::Null).await {
            Ok(id) => (id, false),
            Err(ReceiveECashError::AlreadyReceived) => (existing_op, true),
            Err(e) => return Err(e.into()),
        };
        let received = tokio::time::timeout(Duration::from_secs(120), self.v2_received(op)).await.context("reissue timed out")??;
        ensure!(received, "mint v2 reissue was rejected");
        let fee_msat = if already { 0 } else { self.client.get_operation_fees(op).await?.map(|a| a.get_bitcoin().msats).unwrap_or(0) };
        Ok(Imported { face, fee_msat, already })
    }

    /// Export the active module's portable bearer token.
    pub async fn export_ecash(&self, amount_msat: u64, allow_round_up: bool, include_invite: bool) -> Result<(String, Amount)> {
        if self.mint_version == MintVersion::V1 {
            let (notes, face) = self.export(amount_msat, allow_round_up, include_invite).await?;
            return Ok((notes.to_string(), face));
        }
        let rounded = amount_msat.checked_next_multiple_of(self.min_denom()).context("eCash amount overflow")?;
        ensure!(allow_round_up || rounded == amount_msat, "eCash must be a multiple of {} msat", self.min_denom());
        let amount = Amount::from_msats(rounded);
        let (_, notes) = self.mint_v2()?.send(amount, Value::Null, include_invite).await?;
        let face = notes.amount();
        ensure!(face == amount, "unexpected mint v2 export amount");
        Ok((encode_v2_ecash(&notes), face))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fedimint_core::config::{ClientModuleConfig, GlobalClientConfig};
    use fedimint_core::module::{AmountUnit, CoreConsensusVersion};

    #[test]
    fn selects_bitcoin_among_multiple_v2_assets_and_refuses_other_units() -> Result<()> {
        let mint = |id, unit| {
            ClientModuleConfig::from_typed(
                id,
                fedimint_mintv2_common::KIND,
                fedimint_mintv2_common::MODULE_CONSENSUS_VERSION,
                V2Config {
                    tbs_agg_pks: BTreeMap::new(),
                    tbs_pks: BTreeMap::new(),
                    fee_consensus: fedimint_mintv2_common::config::FeeConsensus::zero(),
                    amount_unit: unit,
                },
            )
        };
        let mut cfg = ClientConfig {
            global: GlobalClientConfig {
                api_endpoints: BTreeMap::new(),
                broadcast_public_keys: None,
                consensus_version: CoreConsensusVersion::new(2, 1),
                meta: BTreeMap::new(),
            },
            modules: BTreeMap::from([(2, mint(2, AmountUnit::new_custom(7)))]),
        };
        assert!(bitcoin_mint(&cfg).unwrap_err().is::<UnsupportedMint>());
        cfg.modules.insert(3, mint(3, AmountUnit::BITCOIN));
        assert_eq!(bitcoin_mint(&cfg)?, (MintVersion::V2, 3));
        Ok(())
    }
}
