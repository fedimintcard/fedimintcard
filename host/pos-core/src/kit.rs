//! The offline kit (docs/POS-OFFLINE.md § The kit): what a POS phone needs from its till to take a
//! card with no network. The till issues it (`GET /v1/offline/kit`) and the phone passes it to
//! every charge; nothing in it can spend the merchant's money. It carries the federation's note
//! keys and fees, the registry's certificate key, the phone's caps, its validity, the newest block
//! header, and a pool of the till's outputs: blinded nonces whose spend keys and blinding keys stay
//! on the till.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result, bail, ensure};
use fedimint_card_host_card::v3::{HEADER_LEN, header_has_work};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::encoding::Decodable;
use fedimint_core::module::registry::{ModuleDecoderRegistry, ModuleRegistry};
use fedimint_core::{Amount, Tiered};
use fedimint_mint_common::config::FeeConsensus;
use serde::{Deserialize, Serialize};
use tbs::{AggregatePublicKey, BlindedMessage};

use crate::charge::{PoolEntry, mint_decoders};
use crate::plan::FeeHeadroom;
use crate::queue::{OfflineConfig, OfflineError};

/// The kit's format.
pub const KIT_VERSION: u32 = 1;
/// Most bytes a kit may take as JSON: the till lowers the pool's depth to stay under it.
pub const KIT_MAX_BYTES: usize = 128 * 1024;

/// An offline kit, as the till answers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kit {
    pub version: u32,
    /// 16 random bytes, hex.
    pub kit_id: String,
    /// The phone it was issued to (its store's `phone-id`).
    pub phone_id: String,
    pub federation_id: String,
    pub mint_instance_id: ModuleInstanceId,
    /// Tier in msat (decimal) to its `tbs::AggregatePublicKey` (96 bytes, compressed G2), hex.
    pub tbs_pks: BTreeMap<String, String>,
    /// The mint's `FeeConsensus`, consensus encoding, hex.
    pub fee_consensus: String,
    /// The fee schedule a charge keeps a margin for, where the federation takes a surplus.
    pub fee_headroom: FeeHeadroom,
    pub surplus_allowed: bool,
    /// The registry's certificate key (65 bytes, uncompressed P-256), hex.
    pub cert_key: String,
    /// The phone's caps on charges waiting to sync.
    pub cap_msat: u64,
    pub cap_count: usize,
    pub issued_at_ms: u64,
    /// No charge from then on.
    pub expires_at_ms: u64,
    /// The newest Bitcoin block header the till had (80 bytes, hex): shown to a card whose clock
    /// is older.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_header: Option<String>,
    /// The till's outputs, per tier.
    pub pool: Vec<PoolTier>,
}

/// The pool entries of one tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolTier {
    pub amount_msat: u64,
    /// Blinded nonces (48 bytes, hex), each the blinded message of one till note.
    pub blinded: Vec<String>,
}

/// A kit decoded and checked: what a charge works with.
pub struct KitKeys {
    pub tbs_pks: Tiered<AggregatePublicKey>,
    pub fee: FeeConsensus,
    pub decoders: ModuleDecoderRegistry,
    pub header: Option<[u8; HEADER_LEN]>,
    pub pool: Vec<PoolEntry>,
    pub cfg: OfflineConfig,
}

/// A kit the phone cannot use: refused as `offline_not_ready`, before the card is asked anything.
fn unusable(m: String) -> anyhow::Error {
    OfflineError::NotReady(format!("the offline kit cannot be used: {m}")).into()
}

impl Kit {
    /// Parse a kit as the till answered it: the kit itself, or the till's `{"ok":true,"kit":…}`.
    pub fn parse(json: &str) -> Result<Kit> {
        if json.len() > KIT_MAX_BYTES {
            return Err(unusable(format!("{} bytes is more than {KIT_MAX_BYTES}", json.len())));
        }
        let v: serde_json::Value = serde_json::from_str(json).map_err(|e| unusable(format!("not JSON ({e})")))?;
        let v = match v.get("kit") {
            Some(k) => k.clone(),
            None => v,
        };
        let kit: Kit = serde_json::from_value(v).map_err(|e| unusable(format!("{e}")))?;
        if kit.version > KIT_VERSION {
            return Err(unusable(format!("version {} (this build understands {KIT_VERSION})", kit.version)));
        }
        Ok(kit)
    }

    /// The kit's keys, every field checked, for the phone `phone_id`. Its expiry is the charge's
    /// to check (`Env::expires_at_ms`), against the phone's clock and the card's.
    pub fn decode(&self, phone_id: &str) -> Result<KitKeys> {
        self.check(phone_id).map_err(|e| unusable(format!("{e:#}")))
    }

    fn check(&self, phone_id: &str) -> Result<KitKeys> {
        ensure!(self.phone_id == phone_id, "it was issued to phone {}, not {phone_id}", self.phone_id);
        ensure!(self.issued_at_ms < self.expires_at_ms, "it expires before it was issued");
        ensure!(self.cap_msat > 0 && self.cap_count > 0, "its caps are zero");
        let reg = ModuleRegistry::default();
        let mut tiers = vec![];
        for (amount, pk) in &self.tbs_pks {
            let msat: u64 = amount.parse().with_context(|| format!("tier {amount:?} is not an amount"))?;
            let pk = AggregatePublicKey::consensus_decode_hex(pk, &reg).map_err(|e| anyhow::anyhow!("the key of tier {msat}: {e}"))?;
            tiers.push((Amount::from_msats(msat), pk));
        }
        ensure!(!tiers.is_empty(), "it has no note keys");
        let tbs_pks: Tiered<AggregatePublicKey> = tiers.into_iter().collect();
        let fee = FeeConsensus::consensus_decode_hex(&self.fee_consensus, &reg).map_err(|e| anyhow::anyhow!("its fee schedule: {e}"))?;
        let cert_key = hex::decode(&self.cert_key).context("its certificate key is not hex")?;
        ensure!(cert_key.len() == 65 && cert_key[0] == 0x04, "its certificate key is not an uncompressed P-256 point");
        let header = match &self.block_header {
            None => None,
            Some(h) => {
                let b: [u8; HEADER_LEN] =
                    hex::decode(h).ok().and_then(|b| b.try_into().ok()).context("its block header is not 80 bytes of hex")?;
                ensure!(header_has_work(&b), "its block header lacks the work it claims");
                Some(b)
            }
        };
        let mut pool = vec![];
        let mut seen = HashSet::new();
        for tier in &self.pool {
            ensure!(tier.amount_msat.is_power_of_two(), "pool tier {} msat is not a power of two", tier.amount_msat);
            ensure!(
                tbs_pks.get(Amount::from_msats(tier.amount_msat)).is_some(),
                "pool tier {} msat is not a note of the federation",
                tier.amount_msat
            );
            for b in &tier.blinded {
                if !seen.insert(b.as_str()) {
                    bail!("pool entry {b} is there twice");
                }
                let m = BlindedMessage::consensus_decode_hex(b, &reg).map_err(|e| anyhow::anyhow!("pool entry {b}: {e}"))?;
                pool.push(PoolEntry::new(tier.amount_msat, m));
            }
        }
        Ok(KitKeys {
            tbs_pks,
            fee,
            decoders: mint_decoders(self.mint_instance_id),
            header,
            pool,
            cfg: OfflineConfig { cap_msat: self.cap_msat, cap_count: self.cap_count, fee_headroom: self.fee_headroom },
        })
    }

    /// Entries per tier: `(amount_msat, count)`.
    pub fn depth(&self) -> Vec<(u64, usize)> {
        self.pool.iter().map(|t| (t.amount_msat, t.blinded.len())).collect()
    }
}
