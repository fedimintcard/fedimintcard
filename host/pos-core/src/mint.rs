//! The mint wire formats used by till and phone card transactions.
use anyhow::{Result, ensure};
use fedimint_core::Amount;
use fedimint_core::core::{DynInput, DynOutput, IntoDynInstance as _, ModuleInstanceId};
use fedimint_core::module::CommonModuleInit as _;
use fedimint_core::module::registry::{ModuleDecoderRegistry, ModuleRegistry};
use fedimint_mint_common::{BlindNonce, Nonce};
use fedimint_mintv2_common::Denomination;
use rand::Rng as _;
use serde::{Deserialize, Serialize};
use tbs::{AggregatePublicKey, BlindedMessage, BlindingKey, Signature};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MintVersion {
    #[default]
    V1,
    V2,
}

/// The selected mint's own validated fee schedule. Both encode the pair (base, ppm)
/// in the same order, but historical v2 deployments support another base fee.
#[derive(Clone, Debug)]
pub enum MintFee {
    V1(fedimint_mint_common::config::FeeConsensus),
    V2(fedimint_mintv2_common::config::FeeConsensus),
}

impl MintFee {
    pub fn fee(&self, amount: Amount) -> Amount {
        match self {
            Self::V1(f) => f.fee(amount),
            Self::V2(f) => f.fee(amount),
        }
    }

    pub fn min_economical_denomination(&self) -> Amount {
        match self {
            Self::V1(fee) => fee.min_economical_denomination(),
            Self::V2(_) => {
                fedimint_mintv2_common::config::client_denominations().next().expect("mint v2 has client denominations").amount()
            }
        }
    }

    /// Mint v2 refuses any bearer bundle containing a note worth its base fee or less.
    pub fn output_floors(&self) -> crate::plan::OutputFloors {
        let minimum = match self {
            Self::V1(_) => 1,
            Self::V2(fee) => (fee.base_fee().msats + 1).next_power_of_two(),
        };
        crate::plan::OutputFloors { preferred: self.min_economical_denomination().msats, minimum }
    }

    pub fn checked_round_up(&self, amount: Amount) -> Option<Amount> {
        amount.msats.checked_next_multiple_of(self.min_economical_denomination().msats).map(Amount::from_msats)
    }

    pub fn consensus_hex(&self) -> String {
        use fedimint_core::encoding::Encodable as _;
        match self {
            Self::V1(fee) => fee.consensus_encode_to_hex(),
            Self::V2(fee) => fee.consensus_encode_to_hex(),
        }
    }

    pub fn from_hex(version: MintVersion, encoded: &str) -> Result<Self> {
        use fedimint_core::encoding::Decodable as _;
        let reg = ModuleRegistry::default();
        Ok(match version {
            MintVersion::V1 => Self::V1(fedimint_mint_common::config::FeeConsensus::consensus_decode_hex(encoded, &reg)?),
            MintVersion::V2 => Self::V2(fedimint_mintv2_common::config::FeeConsensus::consensus_decode_hex(encoded, &reg)?),
        })
    }
}

pub fn denomination(msat: u64) -> Result<Denomination> {
    ensure!(msat.is_power_of_two() && msat.trailing_zeros() < 42, "invalid mint v2 note denomination");
    Ok(Denomination(msat.trailing_zeros() as u8))
}

impl MintVersion {
    pub(crate) fn is_v1(&self) -> bool {
        *self == Self::V1
    }

    pub fn blind(self, nonce: Nonce, key: BlindingKey) -> BlindedMessage {
        tbs::blind_message(self.message(nonce), key)
    }

    fn message(self, nonce: Nonce) -> tbs::Message {
        match self {
            Self::V1 => nonce.to_message(),
            Self::V2 => fedimint_mintv2_common::nonce_message(nonce.0),
        }
    }

    pub fn verify(self, nonce: Nonce, signature: Signature, pk: AggregatePublicKey) -> bool {
        tbs::verify(self.message(nonce), signature, pk)
    }

    pub fn decoders(self, id: ModuleInstanceId) -> ModuleDecoderRegistry {
        let (kind, decoder) = match self {
            Self::V1 => (fedimint_mint_common::KIND, fedimint_mint_common::MintCommonInit::decoder()),
            Self::V2 => (fedimint_mintv2_common::KIND, fedimint_mintv2_common::MintCommonInit::decoder()),
        };
        ModuleRegistry::from_iter([(id, kind, decoder)])
    }

    pub fn input(self, id: ModuleInstanceId, msat: u64, nonce: Nonce, signature: Signature) -> Result<DynInput> {
        Ok(match self {
            Self::V1 => fedimint_mint_common::MintInput::new_v0(Amount::from_msats(msat), fedimint_mint_common::Note { nonce, signature })
                .into_dyn(id),
            Self::V2 => fedimint_mintv2_common::MintInput::new_v0(fedimint_mintv2_common::Note {
                denomination: denomination(msat)?,
                nonce: nonce.0,
                signature,
            })
            .into_dyn(id),
        })
    }

    pub fn output(self, id: ModuleInstanceId, msat: u64, blinded: BlindedMessage) -> Result<DynOutput> {
        Ok(match self {
            Self::V1 => fedimint_mint_common::MintOutput::new_v0(Amount::from_msats(msat), BlindNonce(blinded)).into_dyn(id),
            Self::V2 => fedimint_mintv2_common::MintOutput::new_v0(denomination(msat)?, blinded, rand::thread_rng().r#gen()).into_dyn(id),
        })
    }

    pub fn read_input(self, id: ModuleInstanceId, input: &DynInput) -> Option<(Amount, Nonce, Signature)> {
        if id != input.module_instance_id() {
            return None;
        }
        match self {
            Self::V1 => {
                let i = input.as_any().downcast_ref::<fedimint_mint_common::MintInput>()?.maybe_v0_ref()?;
                Some((i.amount, i.note.nonce, i.note.signature))
            }
            Self::V2 => {
                let i = input.as_any().downcast_ref::<fedimint_mintv2_common::MintInput>()?.maybe_v0_ref()?;
                (i.note.denomination.0 < 42).then(|| (i.note.amount(), Nonce(i.note.nonce), i.note.signature))
            }
        }
    }

    pub fn read_output(self, id: ModuleInstanceId, output: &DynOutput) -> Option<(Amount, BlindedMessage)> {
        if id != output.module_instance_id() {
            return None;
        }
        match self {
            Self::V1 => {
                let o = output.as_any().downcast_ref::<fedimint_mint_common::MintOutput>()?.maybe_v0_ref()?;
                Some((o.amount, o.blind_nonce.0))
            }
            Self::V2 => {
                let o = output.as_any().downcast_ref::<fedimint_mintv2_common::MintOutput>()?.maybe_v0_ref()?;
                (o.denomination.0 < 42).then(|| (o.amount(), o.nonce))
            }
        }
    }
}
