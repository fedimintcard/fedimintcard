//! Costs of a particular paid receipt, derived from accepted wallet transactions.
use std::collections::{BTreeMap, BTreeSet};

use fedimint_client::db::TransactionFeesKey;
use fedimint_client::module::sm::IState;
use fedimint_client::transaction::{TxSubmissionStates, TxSubmissionStatesSM};
use fedimint_core::core::OperationId;
use fedimint_core::db::IDatabaseTransactionOpsCoreTyped as _;
use fedimint_core::transaction::Transaction;
use fedimint_ln_client::{LightningOperationMeta, LightningOperationMetaVariant};
use fedimint_lnv2_client::common::LightningInvoice;

use super::{MintVersion, Wallet};
use crate::topup::RealizedTopupCosts;

#[derive(Clone, Copy)]
enum Leg {
    Receive,
    Load,
}

fn operation(hex_id: &str) -> Option<OperationId> {
    Some(OperationId(hex::decode(hex_id).ok()?.try_into().ok()?))
}

impl Wallet {
    /// V1 can reissue a note worth less than its spend fee as wallet change, or
    /// consolidate such a note it already held. The card's explicit load notes
    /// are economical by construction, so only wallet change can be counted.
    fn change_in_unspendable_face(&self, tx: &Transaction) -> Option<i64> {
        if self.mint_version == MintVersion::V2 {
            return Some(0);
        }
        let mut delta = 0i128;
        for input in tx.inputs.iter().filter(|i| i.module_instance_id() == self.mint_id) {
            let face = input.as_any().downcast_ref::<fedimint_mint_common::MintInput>()?.maybe_v0_ref()?.amount.msats;
            if face <= self.fee(face) {
                delta -= i128::from(face);
            }
        }
        for output in tx.outputs.iter().filter(|o| o.module_instance_id() == self.mint_id) {
            let face = output.as_any().downcast_ref::<fedimint_mint_common::MintOutput>()?.maybe_v0_ref()?.amount.msats;
            if face <= self.fee(face) {
                delta += i128::from(face);
            }
        }
        delta.try_into().ok()
    }

    /// Include the full fee recorded by finalize_transaction (input face less
    /// output face includes both explicit fees and denomination rounding).
    /// A receive can also have an accepted offer; that alone proves no claim.
    async fn accepted_topup_cost(&self, op: OperationId, leg: Leg) -> Option<(u64, i64)> {
        let (active, inactive) = self.client.executor().get_operation_states(op).await;
        let mut transactions = BTreeMap::new();
        let mut accepted = BTreeSet::new();
        for state in active.iter().map(|(s, _)| s).chain(inactive.iter().map(|(s, _)| s)) {
            if let Some(submit) = state.as_any().downcast_ref::<TxSubmissionStatesSM>() {
                match &submit.state {
                    TxSubmissionStates::Created(tx) => {
                        transactions.insert(tx.tx_hash(), tx);
                    }
                    TxSubmissionStates::Accepted(id) => {
                        accepted.insert(*id);
                    }
                    _ => {}
                }
            }
        }
        let mut dbtx = self.client.db().begin_transaction_nc().await;
        let mut fee = 0u64;
        let mut delta = 0i64;
        let mut target = false;
        for id in accepted {
            let tx = *transactions.get(&id)?;
            target |= match leg {
                Leg::Receive => tx.inputs.iter().any(|input| {
                    input.as_any().is::<fedimint_ln_common::LightningInput>()
                        || input.as_any().is::<fedimint_lnv2_client::common::LightningInput>()
                }),
                Leg::Load => {
                    tx.inputs.iter().any(|i| i.module_instance_id() == self.mint_id)
                        && tx.outputs.iter().any(|o| o.module_instance_id() == self.mint_id)
                }
            };
            fee = fee.checked_add(dbtx.get_value(&TransactionFeesKey(id)).await?.get_bitcoin().msats)?;
            delta = delta.checked_add(self.change_in_unspendable_face(tx)?)?;
        }
        target.then_some((fee, delta))
    }

    async fn received_topup_cost(&self, op: OperationId) -> Option<(u64, u64, i64)> {
        let entry = self.client.operation_log().get_operation(op).await?;
        let gateway_fee = match entry.operation_module_kind() {
            kind if kind == fedimint_ln_common::KIND.as_str() => match entry.try_meta::<LightningOperationMeta>().ok()?.variant {
                LightningOperationMetaVariant::Receive { .. } => 0,
                _ => return None,
            },
            kind if kind == fedimint_lnv2_client::common::KIND.as_str() => {
                let fedimint_lnv2_client::LightningOperationMeta::Receive(receive) = entry.try_meta().ok()? else { return None };
                let LightningInvoice::Bolt11(invoice) = receive.invoice;
                invoice.amount_milli_satoshis()?.checked_sub(receive.contract.commitment.amount.msats)?
            }
            _ => return None,
        };
        let (fee, delta) = self.accepted_topup_cost(op, Leg::Receive).await?;
        Some((gateway_fee, fee, delta))
    }

    pub(crate) async fn topup_costs(&self, receive_op: &str, load_op: Option<&str>) -> RealizedTopupCosts {
        let receive = match operation(receive_op) {
            Some(op) => self.received_topup_cost(op).await,
            None => None,
        };
        let load = match load_op.and_then(operation) {
            Some(op)
                if self.client.operation_log().get_operation(op).await.is_some_and(|e| e.operation_module_kind() == "card-holder-load") =>
            {
                self.accepted_topup_cost(op, Leg::Load).await
            }
            _ => None,
        };
        RealizedTopupCosts {
            gateway_msat: receive.map(|(gateway, _, _)| gateway),
            claim_msat: receive.map(|(_, fee, _)| fee),
            load_msat: load.map(|(fee, _)| fee),
            unspendable_change_delta_msat: receive.zip(load).and_then(|((_, _, claim), (_, load))| claim.checked_add(load)),
        }
    }
}
