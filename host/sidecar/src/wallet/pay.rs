//! Paying an invoice from the till wallet as a library call, for a caller that pays with someone
//! else's money (the till's pay route). It is `ln pay` underneath (`Wallet::pay_bolt11`), and
//! every answer is one of three, which such a caller must tell apart:
//!
//! - `Paid`: the invoice is paid; here are the preimage and the gateway's fee.
//! - `Failed`: certain the invoice was not paid, and the value is in the wallet: nothing was
//!   funded (refused), the federation rejected the funding, or the payment failed and its refund
//!   came back.
//! - `Unknown`: it could still be paid, or the client cannot say where the value is. The
//!   operation id resolves it later ([`Wallet::resolve_pay`]), also after a restart. Never
//!   treat it as failed: no refund, no second attempt.
//!
//! A caller that records a payment before it starts and crashes before it learns the operation id
//! asks [`Wallet::pay_status`] with the invoice instead: `None` there means this wallet never
//! funded a payment of it.

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use fedimint_core::bitcoin::hashes::{Hash as _, sha256};
use fedimint_core::core::OperationId;
use fedimint_lnv2_client::InvoiceSendStatus;
use lightning_invoice::Bolt11Invoice;
use serde::Serialize;

use super::{LnPayOutcome, Wallet, parse_bolt11};

/// What a payment from the till wallet came to. See the module docs for what each case promises.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WalletPay {
    /// The invoice is paid. `fee_msat` is the gateway's fee: 0 for an invoice of this federation,
    /// paid inside it. The federation's own transaction fees are not included.
    Paid { preimage: String, fee_msat: u64, operation_id: String },
    /// Certainly not paid, and the value is in the wallet. `operation_id` is set when a payment
    /// was funded and came back; `insufficient_funds` when the balance could not cover it.
    Failed { reason: String, operation_id: Option<String>, insufficient_funds: bool },
    /// Not settled: resolve it later with `operation_id`.
    Unknown { reason: String, operation_id: String },
}

impl WalletPay {
    /// The payment's operation id, when one was funded.
    pub fn operation_id(&self) -> Option<&str> {
        match self {
            Self::Paid { operation_id, .. } | Self::Unknown { operation_id, .. } => Some(operation_id),
            Self::Failed { operation_id, .. } => operation_id.as_deref(),
        }
    }
}

impl From<LnPayOutcome> for WalletPay {
    fn from(outcome: LnPayOutcome) -> Self {
        match outcome {
            LnPayOutcome::Paid { preimage, fee_msat, operation_id } => Self::Paid { preimage, fee_msat, operation_id },
            LnPayOutcome::Refused { reason, insufficient_funds } => Self::Failed { reason, operation_id: None, insufficient_funds },
            LnPayOutcome::Refunded { reason, operation_id } => {
                Self::Failed { reason, operation_id: Some(operation_id), insufficient_funds: false }
            }
            LnPayOutcome::Pending { operation_id } => Self::Unknown { reason: "the payment is still in flight".into(), operation_id },
            // The client could not settle it: the value may be with the gateway or on its way
            // back, so this is not a failure anybody may refund.
            LnPayOutcome::Failed { reason, operation_id } => Self::Unknown { reason, operation_id },
        }
    }
}

impl Wallet {
    /// Pay `bolt11` from the wallet's balance, refusing (before anything is funded) a gateway
    /// that asks more than `max_fee_msat` for it, and follow the payment for up to `wait`.
    /// Paying an invoice again answers an earlier payment that is paid or not settled and never
    /// funds a second one while one could still pay. Only after a certain failure does it start
    /// a new attempt. Errors for an invoice that does not parse or has no amount, and when the
    /// wallet's earlier payments of it cannot be read; nothing is funded then.
    pub async fn pay_invoice(&self, bolt11: &str, max_fee_msat: u64, wait: Duration) -> Result<WalletPay> {
        // The guard against a second attempt lives under `ln pay` (`pay_bolt11_within`), so the
        // operator's `ln pay` has it too: for ln `earlier_v1` answers an earlier attempt that is
        // paid or not settled; lnv2 never starts a second attempt at an invoice
        // (`DuplicatePaymentAttempt`, answered by following that attempt).
        Ok(self.pay_bolt11_within(bolt11, Some(max_fee_msat), wait).await?.into())
    }

    /// The currency of the invoices this wallet can pay: its Lightning module's network (ln, else
    /// lnv2). `None` without a Lightning module.
    pub async fn invoice_currency(&self) -> Option<lightning_invoice::Currency> {
        let cfg = self.client.config().await;
        if let Ok((_, c)) = cfg.get_first_module_by_kind::<fedimint_ln_common::config::LightningClientConfig>(fedimint_ln_common::KIND) {
            return Some(c.network.0.into());
        }
        if let Ok((_, c)) =
            cfg.get_first_module_by_kind::<fedimint_lnv2_client::common::config::LightningClientConfig>(fedimint_lnv2_client::common::KIND)
        {
            return Some(c.network.into());
        }
        None
    }

    /// What became of the payment `operation_id` (hex, as [`WalletPay`] gives it), from the
    /// federation client's own record, following it for up to `wait` if it is still in flight.
    /// Errors when the id does not name a Lightning payment of this wallet.
    pub async fn resolve_pay(&self, operation_id: &str, wait: Duration) -> Result<WalletPay> {
        let op = parse_operation_id(operation_id)?;
        let entry =
            self.client.operation_log().get_operation(op).await.ok_or_else(|| anyhow!("this wallet has no operation {operation_id}"))?;
        let kind = entry.operation_module_kind();
        let outcome = if kind == fedimint_ln_common::KIND.as_str() {
            self.follow_v1(op, wait).await?
        } else if kind == fedimint_lnv2_client::common::KIND.as_str() {
            self.follow_v2(op, wait).await?
        } else {
            bail!("operation {operation_id} is a {kind} operation, not a Lightning payment");
        };
        Ok(outcome.into())
    }

    /// What became of this wallet's payments of `bolt11`, found by the invoice alone: `None` when
    /// the wallet never funded one, else the settled answer over every attempt (a paid one wins,
    /// then one that is not settled). Follows an attempt in flight for up to `wait`.
    pub async fn pay_status(&self, bolt11: &str, wait: Duration) -> Result<Option<WalletPay>> {
        let invoice = parse_bolt11(bolt11)?;
        let mut found = Vec::new();
        for op in self.pay_operations(&invoice).await? {
            found.push(self.resolve_pay(&hex::encode(op.0), wait).await?);
        }
        Ok(settle(found))
    }

    /// This wallet's earlier ln attempt at `invoice` that a new attempt must not go around: a
    /// paid one, else one that is not settled (in flight, or ended in an error the client could
    /// not settle, which leaves the contract funded and claimable by the gateway). `None` when
    /// there is no earlier attempt or every one failed for certain (refunded). Fedimint's ln
    /// client starts a new attempt after any attempt that is neither paid nor running, including
    /// the unsettled ones, so this check runs before every ln payment.
    pub(super) async fn earlier_v1(&self, invoice: &Bolt11Invoice, wait: Duration) -> Result<Option<LnPayOutcome>> {
        let mut found = Vec::new();
        for op in self.ln_pay_operations(invoice).await? {
            found.push(self.follow_v1(op, wait).await?);
        }
        let paid = found.iter().find(|o| matches!(o, LnPayOutcome::Paid { .. }));
        let open = found.iter().find(|o| matches!(o, LnPayOutcome::Pending { .. } | LnPayOutcome::Failed { .. }));
        Ok(paid.or(open).cloned())
    }

    /// The ln operations this wallet funded to pay `invoice`, oldest first; none without ln.
    async fn ln_pay_operations(&self, invoice: &Bolt11Invoice) -> Result<Vec<OperationId>> {
        let mut ops = Vec::new();
        if let Ok(ln) = self.lightning() {
            // ln numbers the attempts at an invoice from 1 and keeps the last number; an attempt
            // whose transaction was never submitted has no operation.
            let hash = invoice.payment_hash();
            let last = {
                let mut dbtx = ln.db.begin_transaction_nc().await;
                ln.get_prev_payment_result(hash, &mut dbtx).await.index
            };
            for index in 1..=last {
                let op = ln_pay_operation_id(hash, index);
                if self.client.operation_log().get_operation(op).await.is_some() {
                    ops.push(op);
                }
            }
        }
        Ok(ops)
    }

    /// The operations this wallet funded to pay `invoice`, oldest first.
    async fn pay_operations(&self, invoice: &Bolt11Invoice) -> Result<Vec<OperationId>> {
        let mut ops = self.ln_pay_operations(invoice).await?;
        if let Ok(ln) = self.lightning_v2() {
            match ln.get_invoice_send_status(invoice).await.context("reading the wallet's lnv2 payments")? {
                InvoiceSendStatus::NotAttempted => {}
                InvoiceSendStatus::InFlight(op) | InvoiceSendStatus::Succeeded(op) | InvoiceSendStatus::Failed(op) => ops.push(op),
            }
        }
        Ok(ops)
    }
}

/// The operation id ln gives attempt `index` at paying the invoice with `payment_hash`: the
/// sha256 of the hash and the index (u16, little endian). A copy of
/// `LightningClientModule::get_payment_operation_id`, which is private; the integration tests
/// check it against the ids the client hands out.
fn ln_pay_operation_id(payment_hash: &sha256::Hash, index: u16) -> OperationId {
    let mut bytes = [0u8; 34];
    bytes[0..32].copy_from_slice(&payment_hash.to_byte_array());
    bytes[32..34].copy_from_slice(&index.to_le_bytes());
    OperationId(sha256::Hash::hash(&bytes).to_byte_array())
}

fn parse_operation_id(text: &str) -> Result<OperationId> {
    let bytes = hex::decode(text.trim()).map_err(|e| anyhow!("not an operation id: {e}"))?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| anyhow!("not an operation id: it is 32 bytes of hex"))?;
    Ok(OperationId(bytes))
}

/// One answer over several attempts at the same invoice: a paid one settles it, then one that is
/// not settled (it could still pay), then the last failure.
fn settle(found: Vec<WalletPay>) -> Option<WalletPay> {
    let pick = |want: fn(&WalletPay) -> bool| found.iter().find(|p| want(p)).cloned();
    pick(|p| matches!(p, WalletPay::Paid { .. }))
        .or_else(|| pick(|p| matches!(p, WalletPay::Unknown { .. })))
        .or_else(|| found.last().cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paid(op: &str) -> WalletPay {
        WalletPay::Paid { preimage: "00".repeat(32), fee_msat: 7, operation_id: op.into() }
    }

    fn unknown(op: &str) -> WalletPay {
        WalletPay::Unknown { reason: "in flight".into(), operation_id: op.into() }
    }

    fn failed(op: Option<&str>) -> WalletPay {
        WalletPay::Failed { reason: "refunded".into(), operation_id: op.map(Into::into), insufficient_funds: false }
    }

    #[test]
    fn only_a_refusal_or_a_refund_is_a_certain_failure() {
        let op = "ab".repeat(32);
        let refused = LnPayOutcome::Refused { reason: "fee".into(), insufficient_funds: true };
        assert_eq!(WalletPay::from(refused), WalletPay::Failed { reason: "fee".into(), operation_id: None, insufficient_funds: true });
        let refunded = LnPayOutcome::Refunded { reason: "gateway".into(), operation_id: op.clone() };
        assert_eq!(
            WalletPay::from(refunded),
            WalletPay::Failed { reason: "gateway".into(), operation_id: Some(op.clone()), insufficient_funds: false }
        );
        // In flight, and an error the client could not settle: both stay open, with their id.
        let pending = WalletPay::from(LnPayOutcome::Pending { operation_id: op.clone() });
        assert!(matches!(&pending, WalletPay::Unknown { operation_id, .. } if *operation_id == op));
        let unsettled = WalletPay::from(LnPayOutcome::Failed { reason: "refund rejected".into(), operation_id: op.clone() });
        assert_eq!(unsettled, WalletPay::Unknown { reason: "refund rejected".into(), operation_id: op.clone() });
        let paid = WalletPay::from(LnPayOutcome::Paid { preimage: "11".repeat(32), fee_msat: 3, operation_id: op.clone() });
        assert_eq!(paid.operation_id(), Some(op.as_str()));
        assert_eq!(serde_json::to_value(&paid).unwrap()["status"], "paid");
        assert_eq!(serde_json::to_value(&pending).unwrap()["status"], "unknown");
    }

    #[test]
    fn several_attempts_settle_paid_first_then_open_then_the_last_failure() {
        assert_eq!(settle(vec![]), None);
        assert_eq!(settle(vec![failed(Some("a")), paid("b"), unknown("c")]), Some(paid("b")));
        assert_eq!(settle(vec![failed(Some("a")), unknown("c")]), Some(unknown("c")));
        assert_eq!(settle(vec![failed(Some("a")), failed(Some("b"))]), Some(failed(Some("b"))));
    }

    #[test]
    fn operation_ids_parse_from_32_bytes_of_hex() {
        let op = OperationId([0xab; 32]);
        assert_eq!(parse_operation_id(&"ab".repeat(32)).unwrap(), op);
        assert_eq!(parse_operation_id(&format!(" {} ", "AB".repeat(32))).unwrap(), op);
        assert!(parse_operation_id("zz").is_err());
        assert!(parse_operation_id(&"ab".repeat(31)).is_err());
    }

    #[test]
    fn ln_attempt_ids_hash_the_payment_hash_and_the_index() {
        let hash = sha256::Hash::hash(b"invoice");
        let mut bytes = hash.to_byte_array().to_vec();
        bytes.extend_from_slice(&[2, 0]);
        let want = OperationId(sha256::Hash::hash(&bytes).to_byte_array());
        assert_eq!(ln_pay_operation_id(&hash, 2), want);
        assert_ne!(ln_pay_operation_id(&hash, 1), want);
    }
}
