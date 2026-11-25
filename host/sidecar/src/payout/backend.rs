//! The wallet side of a payout, as a trait so tests can stand in for a federation and a gateway;
//! [`TillBackend`] is the till's own wallet and stores.
//!
//! Every eCash operation a payout starts is tagged in the wallet's operation meta, which the
//! client records in the same database transaction as the operation itself:
//!
//! - an export's spend carries `{"fedimintcard_payout": "<tag>"}`, and its record holds the notes;
//! - a reclaim's reissue carries `{"fedimintcard_payout_reclaim": "<tag>"}`.
//!
//! A payout saved before either is therefore settled after any crash by the wallet's operation
//! log alone ([`find_tagged`]): the tagged operation is there, or the wallet never started it.
//! The outcome of a reclaim is its reissue operation's own final state, never an error's text.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail, ensure};
use fedimint_core::Amount;
use fedimint_core::core::OperationId;
use fedimint_mint_client::{OOBNotes, ReissueExternalNotesError, ReissueExternalNotesState};
use futures::StreamExt as _;
use futures::future::BoxFuture;
use lightning_invoice::Currency;
use serde_json::{Value, json};
use tokio::sync::OwnedRwLockWriteGuard;

use super::funds::{self, Snapshot};
use crate::pay::Pays;
use crate::till::TillHandle;
use crate::topup::Topups;
use crate::wallet::{MintVersion, Wallet, WalletPay, encode_v2_ecash};

/// The key of the tag in a payout export's operation meta.
pub const TAG_KEY: &str = "fedimintcard_payout";
/// The key of the tag in a payout reclaim's operation meta.
pub const RECLAIM_KEY: &str = "fedimintcard_payout_reclaim";
const LOG_PAGE: usize = 100;

/// A payout's operation, found in the wallet's log by its tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tagged {
    /// The export's spend, with its notes.
    Export { notes: String },
    /// The reclaim's reissue operation, hex.
    Reclaim { operation_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimStart {
    /// The reissue operation, hex.
    Started(String),
    /// The wallet has a reissue of these notes already (this payout's, found by its tag, or one
    /// made outside it).
    AlreadyReissued,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimState {
    /// The reissue finished: the value is in the wallet.
    Done,
    /// The federation rejected the reissue. Not proof the recipient redeemed them; the notes stay.
    Rejected(String),
    /// Still running.
    Pending,
}

pub trait Backend: Send + Sync {
    /// The wallet's admission guard, exclusive (`crate::wallet::SpendGuard`).
    fn exclusive(&self) -> BoxFuture<'_, OwnedRwLockWriteGuard<()>>;
    /// The balance, what of it is owed to others with the fees of paying it out, and the fee
    /// schedules, read in [`funds::snapshot`]'s order. An error refuses every payout.
    fn snapshot(&self) -> BoxFuture<'_, Result<Snapshot>>;
    /// The network of the invoices the wallet pays; `None` without a Lightning module.
    fn currency(&self) -> BoxFuture<'_, Option<Currency>>;
    /// The client's own quote of the federation fees of funding a contract of `contract_msat`
    /// now ([`Wallet::ln_send_fee_quote`]): a second opinion against the bound.
    fn ln_send_fee_quote(&self, contract_msat: u64) -> BoxFuture<'_, Result<u64>>;
    /// The gateway's fee for `bolt11` now ([`Wallet::quote_invoice`]); an error when no gateway can
    /// pay it now.
    fn quote_gateway_fee(&self, bolt11: String) -> BoxFuture<'_, Result<u64>>;
    /// The mint's quote of an export's fees ([`Wallet::export_fee_quote`]).
    fn export_fee_quote(&self, amount_msat: u64) -> BoxFuture<'_, Result<u64>>;
    /// eCash exports are multiples of this.
    fn min_note_msat(&self) -> u64;
    /// [`Wallet::pay_invoice`].
    fn pay(&self, bolt11: String, max_fee_msat: u64, wait: Duration) -> BoxFuture<'_, Result<WalletPay>>;
    /// [`Wallet::resolve_pay`].
    fn resolve_pay(&self, operation_id: String, wait: Duration) -> BoxFuture<'_, Result<WalletPay>>;
    /// [`Wallet::pay_status`]: `None` when the wallet never funded a payment of the invoice.
    fn pay_status(&self, bolt11: String, wait: Duration) -> BoxFuture<'_, Result<Option<WalletPay>>>;
    /// The federation fees an operation paid, from the wallet's record.
    fn operation_fees(&self, operation_id: String) -> BoxFuture<'_, Result<Option<u64>>>;
    /// Spend `amount_msat` as eCash tagged `tag`: the notes' text and their face value.
    fn export(&self, amount_msat: u64, tag: String) -> BoxFuture<'_, Result<(String, u64)>>;
    /// The operations tagged with any of `tags` in the wallet's log.
    fn find_tagged(&self, tags: Vec<String>) -> BoxFuture<'_, Result<HashMap<String, Tagged>>>;
    /// Start reissuing `notes` into the wallet, tagged `tag`.
    fn reclaim_start(&self, notes: String, tag: String) -> BoxFuture<'_, Result<ReclaimStart>>;
    /// The reissue operation's state, followed for up to `wait`.
    fn reclaim_status(&self, operation_id: String, wait: Duration) -> BoxFuture<'_, Result<ReclaimState>>;
}

/// The till's wallet, its top-ups and its pays (or their files, when the service is not running).
pub struct TillBackend {
    pub wallet: Wallet,
    pub topups: Option<Arc<Topups>>,
    pub pays: Option<Arc<Pays>>,
    pub dir: PathBuf,
    /// The till, for the evidence that a `not_charged` pay was never charged (its in-flight job,
    /// card state and sales). Without it every `not_charged` pay counts.
    pub till: Option<TillHandle>,
}

impl TillBackend {
    /// Holds a clone of the handle: build one per request or pass, never keep it.
    pub fn of(h: &TillHandle, dir: &Path) -> Self {
        Self { wallet: h.wallet().clone(), topups: h.topups(), pays: h.pays(), dir: dir.to_owned(), till: Some(h.clone()) }
    }
}

impl Backend for TillBackend {
    fn exclusive(&self) -> BoxFuture<'_, OwnedRwLockWriteGuard<()>> {
        Box::pin(self.wallet.spend.exclusive())
    }

    fn snapshot(&self) -> BoxFuture<'_, Result<Snapshot>> {
        Box::pin(async move {
            let evidence = self.till.as_ref().map(|h| move |p: &crate::pay::PayRec| crate::pay::never_charged(h, p));
            let f = evidence.as_ref().map(|f| f as &(dyn Fn(&crate::pay::PayRec) -> bool + Send + Sync));
            funds::snapshot(&self.wallet, &self.dir, self.topups.as_deref(), self.pays.as_deref(), f).await
        })
    }

    fn currency(&self) -> BoxFuture<'_, Option<Currency>> {
        Box::pin(self.wallet.invoice_currency())
    }

    fn ln_send_fee_quote(&self, contract_msat: u64) -> BoxFuture<'_, Result<u64>> {
        Box::pin(self.wallet.ln_send_fee_quote(contract_msat))
    }

    fn quote_gateway_fee(&self, bolt11: String) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move { self.wallet.quote_invoice(&bolt11).await })
    }

    fn export_fee_quote(&self, amount_msat: u64) -> BoxFuture<'_, Result<u64>> {
        Box::pin(self.wallet.export_fee_quote(amount_msat))
    }

    fn min_note_msat(&self) -> u64 {
        self.wallet.min_denom().max(1)
    }

    fn pay(&self, bolt11: String, max_fee_msat: u64, wait: Duration) -> BoxFuture<'_, Result<WalletPay>> {
        Box::pin(async move { self.wallet.pay_invoice(&bolt11, max_fee_msat, wait).await })
    }

    fn resolve_pay(&self, operation_id: String, wait: Duration) -> BoxFuture<'_, Result<WalletPay>> {
        Box::pin(async move { self.wallet.resolve_pay(&operation_id, wait).await })
    }

    fn pay_status(&self, bolt11: String, wait: Duration) -> BoxFuture<'_, Result<Option<WalletPay>>> {
        Box::pin(async move { self.wallet.pay_status(&bolt11, wait).await })
    }

    fn operation_fees(&self, operation_id: String) -> BoxFuture<'_, Result<Option<u64>>> {
        Box::pin(async move { self.wallet.operation_fees_msat(&operation_id).await })
    }

    fn export(&self, amount_msat: u64, tag: String) -> BoxFuture<'_, Result<(String, u64)>> {
        Box::pin(async move { export_tagged(&self.wallet, amount_msat, &tag).await })
    }

    fn find_tagged(&self, tags: Vec<String>) -> BoxFuture<'_, Result<HashMap<String, Tagged>>> {
        Box::pin(async move { find_tagged(&self.wallet, &tags).await })
    }

    fn reclaim_start(&self, notes: String, tag: String) -> BoxFuture<'_, Result<ReclaimStart>> {
        Box::pin(async move { reclaim_start(&self.wallet, &notes, &tag).await })
    }

    fn reclaim_status(&self, operation_id: String, wait: Duration) -> BoxFuture<'_, Result<ReclaimState>> {
        Box::pin(async move { reclaim_status(&self.wallet, &operation_id, wait).await })
    }
}

fn with_invite(w: &Wallet, notes: OOBNotes) -> OOBNotes {
    match &w.invite {
        Some(invite) => OOBNotes::new_with_invite(notes.notes().clone(), invite),
        None => notes,
    }
}

fn parse_op(text: &str) -> Result<OperationId> {
    let bytes: [u8; 32] = hex::decode(text.trim()).ok().and_then(|b| b.try_into().ok()).ok_or_else(|| anyhow!("not an operation id"))?;
    Ok(OperationId(bytes))
}

/// Spend exactly `amount_msat` as eCash of this federation (with its invite), the spend tagged
/// `tag` in the wallet's operation log. The notes are never logged.
pub async fn export_tagged(w: &Wallet, amount_msat: u64, tag: &str) -> Result<(String, u64)> {
    let amount = Amount::from_msats(amount_msat);
    ensure!(
        amount_msat > 0 && w.mint_cfg.fee_consensus.checked_round_up(amount) == Some(amount),
        "{amount_msat} msat is not a positive multiple of {} msat, the federation's smallest economical note",
        w.min_denom()
    );
    if w.mint_version == MintVersion::V2 {
        let (_, notes) = w.mint_v2()?.send(amount, json!({ TAG_KEY: tag }), true).await?;
        return Ok((encode_v2_ecash(&notes), notes.amount().msats));
    }
    let mint = w.mint()?;
    let notes = mint.send_oob_notes(amount, json!({ TAG_KEY: tag })).await?;
    // Spent either way: the caller records what the wallet handed out.
    let face = notes.total_amount().msats;
    Ok((with_invite(w, notes).to_string(), face))
}

/// The wallet's operations tagged with any of `tags` (exports and reclaims), read from its
/// operation log, newest first, every page. An error when the log cannot be read or a tagged
/// operation is not what its tag says: the caller keeps those payouts as they are.
pub async fn find_tagged(w: &Wallet, tags: &[String]) -> Result<HashMap<String, Tagged>> {
    use fedimint_mint_client::{MintOperationMeta, MintOperationMetaVariant};
    let mut found = HashMap::new();
    if tags.is_empty() {
        return Ok(found);
    }
    let log = w.client.operation_log();
    let mut last = None;
    loop {
        let page = log.paginate_operations_rev(LOG_PAGE, last).await;
        for (key, entry) in &page {
            if entry.operation_module_kind() == fedimint_mintv2_common::KIND.as_str() {
                use fedimint_mintv2_client::MintOperationMeta as V2Meta;
                // A Reissue makes change inside the wallet. Only Send debits the exported notes.
                let meta = entry.try_meta::<V2Meta>()?;
                let (custom, operation, tag_key) = match meta {
                    V2Meta::Send { ecash, custom_meta } => (custom_meta, Tagged::Export { notes: ecash }, TAG_KEY),
                    V2Meta::Receive { custom_meta, .. } => {
                        (custom_meta, Tagged::Reclaim { operation_id: hex::encode(key.operation_id.0) }, RECLAIM_KEY)
                    }
                    V2Meta::Reissue { .. } => continue,
                };
                if let Some(tag) = custom.get(tag_key).and_then(Value::as_str)
                    && tags.iter().any(|requested| requested == tag)
                {
                    found.insert(tag.to_owned(), operation);
                }
                continue;
            }
            if entry.operation_module_kind() != fedimint_mint_common::KIND.as_str() {
                continue;
            }
            let Ok(meta) = entry.try_meta::<MintOperationMeta>() else { continue };
            let export = meta.extra_meta.get(TAG_KEY).and_then(Value::as_str).map(str::to_owned);
            let reclaim = meta.extra_meta.get(RECLAIM_KEY).and_then(Value::as_str).map(str::to_owned);
            match (export, reclaim, meta.variant) {
                (Some(tag), _, MintOperationMetaVariant::SpendOOB { oob_notes, .. }) if tags.contains(&tag) => {
                    found.insert(tag, Tagged::Export { notes: with_invite(w, oob_notes).to_string() });
                }
                (_, Some(tag), MintOperationMetaVariant::Reissuance { .. }) if tags.contains(&tag) => {
                    found.insert(tag, Tagged::Reclaim { operation_id: hex::encode(key.operation_id.0) });
                }
                (Some(tag), _, _) | (_, Some(tag), _) if tags.contains(&tag) => {
                    bail!("the wallet operation tagged {tag} is not what its tag says");
                }
                _ => {}
            }
        }
        if page.len() < LOG_PAGE || found.len() == tags.len() {
            return Ok(found);
        }
        last = page.last().map(|(k, _)| *k);
    }
}

/// Start reissuing exported notes back into the wallet, the reissue tagged `tag`.
pub async fn reclaim_start(w: &Wallet, notes: &str, tag: &str) -> Result<ReclaimStart> {
    if w.mint_version == MintVersion::V2 {
        let ecash = w.decode_v2_ecash(notes)?;
        return match w.mint_v2()?.receive(ecash, json!({ RECLAIM_KEY: tag })).await {
            Ok(op) => Ok(ReclaimStart::Started(hex::encode(op.0))),
            Err(fedimint_mintv2_client::ReceiveECashError::AlreadyReceived) => Ok(ReclaimStart::AlreadyReissued),
            Err(e) => Err(e.into()),
        };
    }
    let oob = OOBNotes::from_str(notes).map_err(|e| anyhow!("not an eCash string: {e}"))?;
    let mint = w.mint()?;
    match mint.reissue_external_notes(oob, json!({ RECLAIM_KEY: tag })).await {
        Ok(op) => Ok(ReclaimStart::Started(hex::encode(op.0))),
        Err(ReissueExternalNotesError::AlreadyReissued) => Ok(ReclaimStart::AlreadyReissued),
        Err(e) => Err(e.into()),
    }
}

/// The reissue `operation_id`'s state, from the client's own state machine.
pub async fn reclaim_status(w: &Wallet, operation_id: &str, wait: Duration) -> Result<ReclaimState> {
    let op = parse_op(operation_id)?;
    if w.mint_version == MintVersion::V2 {
        return Ok(match tokio::time::timeout(wait, w.v2_received(op)).await {
            Ok(Ok(true)) => ReclaimState::Done,
            Ok(Ok(false)) => ReclaimState::Rejected("mint rejected the notes".into()),
            Ok(Err(e)) => return Err(e),
            Err(_) => ReclaimState::Pending,
        });
    }
    let mint = w.mint()?;
    let mut stream = mint.subscribe_reissue_external_notes(op).await?.into_stream();
    let follow = async {
        while let Some(state) = stream.next().await {
            match state {
                ReissueExternalNotesState::Done => return ReclaimState::Done,
                ReissueExternalNotesState::Failed(e) => return ReclaimState::Rejected(e.to_string()),
                ReissueExternalNotesState::Created | ReissueExternalNotesState::Issuing => {}
            }
        }
        ReclaimState::Pending
    };
    Ok(tokio::time::timeout(wait, follow).await.unwrap_or(ReclaimState::Pending))
}
