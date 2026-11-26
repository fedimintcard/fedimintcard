//! Top-up at the POS by Lightning (docs/TOPUP.md): the till makes an invoice through the
//! federation's gateway, the holder pays it from their own wallet, and the till loads the eCash it
//! received onto the card with the holder load ([`crate::holder::Loader`]).
//!
//! `topups.json` keeps each top-up and its state (written like `sales.json`: 0600, temp file,
//! rename). The Lightning side is the [`TopupLightning`] trait, so tests can stand in for a
//! gateway.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::state::write_private_json;
use crate::wallet::Wallet;

pub const TOPUPS_FILE: &str = "topups.json";
const TOPUPS_VERSION: u32 = 1;
pub const FEE_QUOTE_VERSION: u32 = 1;
/// A top-up is at most this many notes, so it fits a card's free slots (docs/TOPUP.md).
pub const MAX_TOPUP_NOTES: u32 = 3;
/// 100,000 sats.
pub const MAX_TOPUP_MSAT: u64 = 100_000_000;
pub const INVOICE_EXPIRY_SECS: u64 = 600;
/// How long past the invoice's expiry a watcher waits for a payment it never saw arrive.
const EXPIRY_GRACE: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TopupState {
    /// The invoice is unpaid.
    Waiting,
    /// The eCash is in the till's wallet, not on a card yet.
    Paid,
    Loaded,
    /// Unpaid at expiry.
    Expired,
    /// `detail` says why. Received eCash, if any, stays in the till's wallet.
    Failed,
}

impl TopupState {
    pub fn name(self) -> &'static str {
        match self {
            TopupState::Waiting => "waiting",
            TopupState::Paid => "paid",
            TopupState::Loaded => "loaded",
            TopupState::Expired => "expired",
            TopupState::Failed => "failed",
        }
    }
}

/// Amounts fixed when a fee-inclusive invoice is created. Older records have no quote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopupQuote {
    pub version: u32,
    pub invoice_amount_msat: u64,
    pub topup_fee_msat: u64,
    pub load_fee_bound_msat: u64,
}

impl TopupQuote {
    pub fn validate(&self, face_msat: u64) -> Result<()> {
        ensure!(self.version == FEE_QUOTE_VERSION, "unsupported top-up fee quote version {}", self.version);
        ensure!(
            face_msat > 0 && face_msat.checked_add(self.topup_fee_msat) == Some(self.invoice_amount_msat),
            "invalid top-up invoice total"
        );
        ensure!(self.load_fee_bound_msat <= self.topup_fee_msat, "the top-up fee cannot cover its card load");
        Ok(())
    }
}

/// Per-operation costs from the wallet's accepted transactions. Unknown evidence is never zero.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealizedTopupCosts {
    pub gateway_msat: Option<u64>,
    /// Federation fees, including overpayment below the smallest change denomination.
    pub claim_msat: Option<u64>,
    pub load_msat: Option<u64>,
    /// Unspendable wallet change created minus such notes consumed, across claim and load.
    pub unspendable_change_delta_msat: Option<i64>,
}

impl RealizedTopupCosts {
    fn complete(&self) -> bool {
        self.gateway_msat.is_some() && self.claim_msat.is_some() && self.load_msat.is_some() && self.unspendable_change_delta_msat.is_some()
    }

    fn summary(&self, fee_msat: u64, state: TopupState) -> Value {
        let nominal = (|| {
            let net = i128::from(fee_msat) - i128::from(self.gateway_msat?) - i128::from(self.claim_msat?) - i128::from(self.load_msat?);
            i64::try_from(net).ok()
        })();
        let retained = nominal.and_then(|n| n.checked_sub(self.unspendable_change_delta_msat?));
        let status = match state {
            TopupState::Waiting => "awaiting_payment",
            TopupState::Expired | TopupState::Failed => "no_confirmed_receipt",
            TopupState::Paid => "awaiting_card_load",
            TopupState::Loaded if self.complete() => "reconciled",
            TopupState::Loaded => "evidence_incomplete",
        };
        json!({"status":status,"gateway_msat":self.gateway_msat,"claim_msat":self.claim_msat,"load_msat":self.load_msat,
            "unspendable_change_delta_msat":self.unspendable_change_delta_msat,
            "retained_nominal_msat":nominal,"retained_spendable_msat":retained})
    }
}

pub struct TopupInvoice {
    /// Earlier of the signed invoice expiry and its incoming contract's funding deadline.
    pub expires_at_ms: u64,
    pub operation_id: String,
    pub invoice: String,
    pub invoice_amount_msat: u64,
    pub topup_fee_msat: u64,
    pub load_fee_bound_msat: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopupRec {
    pub topup_id: String,
    #[serde(default)]
    pub client_ref: Option<String>,
    /// What the POS asked for, before rounding to a card amount.
    pub requested_msat: u64,
    /// The face value promised to the card, including on records from earlier tills.
    pub amount_msat: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<TopupQuote>,
    pub invoice: String,
    /// The Lightning receive operation in the till's wallet, hex.
    pub operation_id: String,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub state: TopupState,
    #[serde(default)]
    pub load_txid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realized_costs: Option<RealizedTopupCosts>,
    #[serde(default)]
    pub short_id: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    /// The card the top-up was made for (`short_id` in `POST /v1/topup`); `None` for one made
    /// without it, which any card may take (docs/TOPUP.md § Waiting top-ups).
    #[serde(default)]
    pub for_card: Option<String>,
}

impl TopupRec {
    pub(crate) fn payment_window_closed(&self) -> bool {
        self.quote.is_some() && self.state == TopupState::Waiting && now_ms() >= self.expires_at_ms
    }

    /// The API's view: no operation id, nothing secret.
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "ok": true,
            "topup_id": self.topup_id,
            "amount_msat": self.amount_msat,
            "invoice": if self.payment_window_closed() { "" } else { &self.invoice },
            "expires_at_ms": self.expires_at_ms,
            "state": self.state.name(),
        });
        if let Some(q) = &self.quote {
            v["fee_quote_version"] = json!(q.version);
            v["invoice_amount_msat"] = json!(q.invoice_amount_msat);
            v["topup_fee_msat"] = json!(q.topup_fee_msat);
            v["fee_accounting"] = self.realized_costs.clone().unwrap_or_default().summary(q.topup_fee_msat, self.state);
        }
        if let Some(t) = &self.load_txid {
            v["load_txid"] = json!(t);
        }
        if let Some(d) = &self.detail {
            v["detail"] = json!(d);
        }
        v
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TopupFile {
    version: u32,
    topups: Vec<TopupRec>,
}

pub struct TopupStore {
    path: PathBuf,
    file: TopupFile,
}

impl TopupStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let file = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => TopupFile { version: TOPUPS_VERSION, topups: vec![] },
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        ensure!(file.version <= TOPUPS_VERSION, "unsupported top-up store version {}", file.version);
        for t in &file.topups {
            if let Some(q) = &t.quote {
                q.validate(t.amount_msat).context("invalid saved top-up quote")?;
            }
        }
        Ok(Self { path, file })
    }

    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join(TOPUPS_FILE)
    }

    pub fn get(&self, id: &str) -> Option<&TopupRec> {
        self.file.topups.iter().find(|t| t.topup_id == id)
    }

    pub fn by_ref(&self, client_ref: &str) -> Option<&TopupRec> {
        self.file.topups.iter().find(|t| t.client_ref.as_deref() == Some(client_ref))
    }

    /// The paid top-ups not yet loaded that `card` may take: made for it or for no card; every one
    /// when `card` is `None`. Oldest first.
    pub fn waiting_for(&self, card: Option<&str>) -> Vec<&TopupRec> {
        let mut v: Vec<&TopupRec> = self
            .file
            .topups
            .iter()
            .filter(|t| t.state == TopupState::Paid)
            .filter(|t| match (card, t.for_card.as_deref()) {
                (None, _) | (_, None) => true,
                (Some(c), Some(f)) => c.eq_ignore_ascii_case(f),
            })
            .collect();
        v.sort_by_key(|t| t.created_at_ms);
        v
    }

    pub fn all(&self) -> &[TopupRec] {
        &self.file.topups
    }

    pub fn insert(&mut self, rec: TopupRec) -> Result<()> {
        if let Some(q) = &rec.quote {
            q.validate(rec.amount_msat)?;
        }
        let mut next = self.file.clone();
        next.topups.push(rec);
        self.save(next)
    }

    /// Change one top-up and save. `false` when there is no such top-up.
    pub fn update(&mut self, id: &str, f: impl FnOnce(&mut TopupRec)) -> Result<bool> {
        let mut next = self.file.clone();
        let Some(t) = next.topups.iter_mut().find(|t| t.topup_id == id) else { return Ok(false) };
        f(t);
        self.save(next)?;
        Ok(true)
    }

    fn save(&mut self, next: TopupFile) -> Result<()> {
        write_private_json(&self.path, &next)?;
        self.file = next;
        Ok(())
    }
}

/// How an invoice ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LnOutcome {
    /// The eCash is in the wallet.
    Paid,
    Expired,
    Failed(String),
}

/// The Lightning side of a top-up.
pub trait TopupLightning: Send + Sync {
    /// An invoice for exactly `amount_msat`: `(operation id hex, bolt11)`.
    fn invoice(&self, amount_msat: u64, expiry_secs: u64) -> BoxFuture<'_, Result<(String, String)>>;
    /// Issue an invoice whose fees cover claiming the receipt and loading the promised card amount.
    fn fee_invoice(&self, face_msat: u64, expiry_secs: u64) -> BoxFuture<'_, Result<TopupInvoice>>;
    /// Wait until the invoice behind `operation_id` is paid (and its eCash claimed), or will not
    /// be. `expires_at_ms`: the invoice's expiry, for a watcher that never hears a timeout.
    fn wait(&self, operation_id: String, expires_at_ms: u64) -> BoxFuture<'static, Result<LnOutcome>>;
    /// A previous watcher can have stopped before the SDK finally claimed the receipt.
    fn settled(&self, operation_id: String) -> BoxFuture<'_, Result<Option<bool>>>;
    fn realized(&self, receive_op: String, load_op: Option<String>) -> BoxFuture<'_, RealizedTopupCosts>;
}

/// The federation's Lightning module through the till's wallet.
pub struct WalletLightning {
    pub wallet: Wallet,
}

impl WalletLightning {
    pub async fn invoice_v2(&self, amount_msat: u64, expiry_secs: u64, gateway: Option<&str>) -> Result<(String, String)> {
        use fedimint_lnv2_client::common::Bolt11InvoiceDescription;
        let ln = self.wallet.lightning_v2()?;
        let gateway = match gateway {
            Some(url) => Some(ln.list_gateways(None).await?.into_iter().find(|g| g.as_str() == url).context("unknown Lightning gateway")?),
            None => None,
        };
        let (invoice, op) = ln
            .receive(
                fedimint_core::Amount::from_msats(amount_msat),
                expiry_secs.try_into()?,
                Bolt11InvoiceDescription::Direct("fedimintcard top-up".into()),
                gateway,
                serde_json::Value::Null,
            )
            .await?;
        Ok((hex::encode(op.0), invoice.to_string()))
    }
}

impl TopupLightning for WalletLightning {
    fn realized(&self, receive_op: String, load_op: Option<String>) -> BoxFuture<'_, RealizedTopupCosts> {
        Box::pin(async move { self.wallet.topup_costs(&receive_op, load_op.as_deref()).await })
    }

    fn fee_invoice(&self, face_msat: u64, expiry_secs: u64) -> BoxFuture<'_, Result<TopupInvoice>> {
        Box::pin(self.fee_inclusive_invoice(face_msat, expiry_secs))
    }

    fn invoice(&self, amount_msat: u64, expiry_secs: u64) -> BoxFuture<'_, Result<(String, String)>> {
        Box::pin(async move {
            if self.wallet.lightning().is_err() {
                return self.invoice_v2(amount_msat, expiry_secs, None).await;
            }
            let ln = self.wallet.lightning()?;
            ln.update_gateway_cache().await.map_err(|e| anyhow!("reading the federation's gateways: {e:?}"))?;
            let gw = ln.select_available_gateway(None, None).await.map_err(|e| anyhow!("no Lightning gateway answers: {e:?}"))?;
            let desc =
                lightning_invoice::Bolt11InvoiceDescription::Direct(lightning_invoice::Description::new("fedimintcard top-up".into())?);
            let (op, invoice, _) = ln
                .create_bolt11_invoice(fedimint_core::Amount::from_msats(amount_msat), desc, Some(expiry_secs), (), Some(gw))
                .await
                .map_err(|e| anyhow!("the gateway made no invoice: {e:?}"))?;
            Ok((hex::encode(op.0), invoice.to_string()))
        })
    }

    fn settled(&self, operation_id: String) -> BoxFuture<'_, Result<Option<bool>>> {
        Box::pin(async move { self.wallet.ln_receive_settled(&operation_id, Duration::from_millis(500)).await })
    }

    fn wait(&self, operation_id: String, expires_at_ms: u64) -> BoxFuture<'static, Result<LnOutcome>> {
        let wallet = self.wallet.clone();
        Box::pin(async move {
            use fedimint_ln_client::{LnReceiveState, receive::LightningReceiveError};
            use futures::StreamExt as _;
            let op = fedimint_core::core::OperationId(
                hex::decode(&operation_id).ok().and_then(|b| b.try_into().ok()).context("bad operation id")?,
            );
            let entry = wallet.client.operation_log().get_operation(op).await.context("unknown Lightning receive operation")?;
            if entry.operation_module_kind() == fedimint_lnv2_client::common::KIND.as_str() {
                use fedimint_lnv2_client::FinalReceiveOperationState;
                return Ok(match wallet.lightning_v2()?.await_final_receive_operation_state(op).await? {
                    FinalReceiveOperationState::Claimed => LnOutcome::Paid,
                    FinalReceiveOperationState::Expired => LnOutcome::Expired,
                    FinalReceiveOperationState::Failure => LnOutcome::Failed("Lightning payment claim failed".into()),
                    FinalReceiveOperationState::Uneconomical => LnOutcome::Failed("Lightning payment was uneconomical to claim".into()),
                });
            }
            let ln = wallet.lightning()?;
            let mut updates = ln.subscribe_ln_receive(op).await?.into_stream();
            let mut funded = false;
            loop {
                // A local timeout leaves a claim unresolved until the SDK reports a final state.
                let left = Duration::from_millis(expires_at_ms.saturating_sub(now_ms())).saturating_add(EXPIRY_GRACE);
                let next = if funded { Ok(updates.next().await) } else { tokio::time::timeout(left, updates.next()).await };
                match next {
                    Err(_) => return Err(anyhow!("waiting for the wallet to confirm the invoice outcome")),
                    Ok(None) => return Err(anyhow!("the receive ended without an outcome")),
                    Ok(Some(LnReceiveState::Claimed)) => return Ok(LnOutcome::Paid),
                    Ok(Some(LnReceiveState::Canceled { reason: LightningReceiveError::Timeout })) => return Ok(LnOutcome::Expired),
                    Ok(Some(LnReceiveState::Canceled { reason })) => return Ok(LnOutcome::Failed(reason.to_string())),
                    Ok(Some(LnReceiveState::Funded | LnReceiveState::AwaitingFunds)) => funded = true,
                    Ok(Some(_)) => {}
                }
            }
        })
    }
}

/// The till's top-ups: the store and the Lightning side, shared by the handlers, the watchers and
/// the actor.
pub struct Topups {
    pub store: Mutex<TopupStore>,
    pub ln: Arc<dyn TopupLightning>,
    /// Serialises `POST /v1/topup`, so a repeated `client_ref` never makes two invoices.
    pub create: tokio::sync::Mutex<()>,
}

impl Topups {
    pub fn new(store: TopupStore, ln: Arc<dyn TopupLightning>) -> Arc<Self> {
        Arc::new(Self { store: Mutex::new(store), ln, create: tokio::sync::Mutex::new(()) })
    }

    pub fn get(&self, id: &str) -> Option<TopupRec> {
        self.store.lock().unwrap().get(id).cloned()
    }

    /// Resume pending operations and check terminal records written by earlier watchers. An
    /// earlier local timeout could have been recorded while the SDK was still claiming.
    pub fn watch_waiting(self: &Arc<Self>) {
        let rows = self.store.lock().unwrap().all().to_vec();
        let mut unresolved = Vec::new();
        let mut account = Vec::new();
        for t in rows {
            match t.state {
                TopupState::Waiting => self.watch(&t),
                TopupState::Expired | TopupState::Failed => unresolved.push(t),
                TopupState::Paid | TopupState::Loaded if t.quote.is_some() => account.push(t.topup_id),
                _ => {}
            }
        }
        let me = self.clone();
        tokio::spawn(async move {
            for id in account {
                me.reconcile(&id).await;
            }
            for t in unresolved {
                match me.ln.settled(t.operation_id.clone()).await {
                    Ok(Some(true)) => me.save_outcome(&t.topup_id, TopupState::Paid, None).await,
                    Ok(Some(false)) => {}
                    // Resolve the same operation; never create a replacement invoice.
                    Ok(None) | Err(_) => me.watch(&t),
                }
            }
        });
    }

    pub fn watch(self: &Arc<Self>, t: &TopupRec) {
        let me = self.clone();
        let (id, op, expiry) = (t.topup_id.clone(), t.operation_id.clone(), t.expires_at_ms);
        tokio::spawn(async move {
            let mut retry = 2;
            loop {
                let (state, detail) = match me.ln.wait(op.clone(), expiry).await {
                    Ok(LnOutcome::Paid) => (TopupState::Paid, None),
                    Ok(LnOutcome::Expired) => (TopupState::Expired, None),
                    Ok(LnOutcome::Failed(why)) => (TopupState::Failed, Some(why)),
                    Err(e) => {
                        tracing::warn!("could not resolve top-up {id}: {e:#}; retrying its wallet operation");
                        tokio::time::sleep(Duration::from_secs(retry)).await;
                        retry = (2 * retry).min(60);
                        continue;
                    }
                };
                me.save_outcome(&id, state, detail).await;
                return;
            }
        });
    }

    async fn save_outcome(&self, id: &str, state: TopupState, detail: Option<String>) {
        // If the write fails after the SDK claimed, retry that exact result instead of losing the
        // holder's paid state or creating a different operation.
        loop {
            let saved = self.store.lock().unwrap().update(id, |t| {
                if !matches!(t.state, TopupState::Loaded | TopupState::Paid) {
                    t.state = state;
                    t.detail = detail.clone();
                }
            });
            match saved {
                Ok(_) => {
                    if state == TopupState::Paid {
                        self.reconcile(id).await;
                    }
                    return;
                }
                Err(e) => tracing::warn!("could not record top-up {id} {}: {e:#}; retrying", state.name()),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    /// Reconciliation reads local SDK evidence only. Missing legacy metadata or an interrupted
    /// accounting write never withholds the paid credit or reruns the financial operation.
    pub async fn reconcile(&self, id: &str) {
        let Some(row) = self.get(id) else { return };
        if row.quote.is_none() || !matches!(row.state, TopupState::Paid | TopupState::Loaded) {
            return;
        }
        let before = row.realized_costs.clone().unwrap_or_default();
        if before.complete() || (row.state == TopupState::Paid && before.gateway_msat.is_some() && before.claim_msat.is_some()) {
            return;
        }
        let observed = self.ln.realized(row.operation_id.clone(), row.load_operation_id.clone()).await;
        let costs = RealizedTopupCosts {
            gateway_msat: observed.gateway_msat.or(before.gateway_msat),
            claim_msat: observed.claim_msat.or(before.claim_msat),
            load_msat: observed.load_msat.or(before.load_msat),
            unspendable_change_delta_msat: observed.unspendable_change_delta_msat.or(before.unspendable_change_delta_msat),
        };
        if row.realized_costs.as_ref() == Some(&costs) {
            return;
        }
        // Another request may have observed the load while this one was reading the claim.
        let mut store = self.store.lock().unwrap();
        if let Some(cur) = store.get(id)
            && cur.state == row.state
            && cur.load_operation_id == row.load_operation_id
            && cur.realized_costs == row.realized_costs
            && let Err(e) = store.update(id, |t| t.realized_costs = Some(costs))
        {
            tracing::warn!("could not save top-up {id} fee accounting: {e:#}");
        }
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A new top-up id: 16 hex characters.
pub fn new_id() -> String {
    hex::encode(rand::random::<[u8; 8]>())
}
