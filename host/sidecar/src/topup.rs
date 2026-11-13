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

use anyhow::{Context, Result, anyhow};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::state::write_private_json;
use crate::wallet::Wallet;

pub const TOPUPS_FILE: &str = "topups.json";
const TOPUPS_VERSION: u32 = 1;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopupRec {
    pub topup_id: String,
    #[serde(default)]
    pub client_ref: Option<String>,
    /// What the POS asked for, before rounding to a card amount.
    pub requested_msat: u64,
    /// The card amount: what the invoice is for and what loads.
    pub amount_msat: u64,
    pub invoice: String,
    /// The Lightning receive operation in the till's wallet, hex.
    pub operation_id: String,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub state: TopupState,
    #[serde(default)]
    pub load_txid: Option<String>,
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
    /// The API's view: no operation id, nothing secret.
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "ok": true,
            "topup_id": self.topup_id,
            "amount_msat": self.amount_msat,
            "invoice": self.invoice,
            "expires_at_ms": self.expires_at_ms,
            "state": self.state.name(),
        });
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
        self.file.topups.push(rec);
        self.save()
    }

    /// Change one top-up and save. `false` when there is no such top-up.
    pub fn update(&mut self, id: &str, f: impl FnOnce(&mut TopupRec)) -> Result<bool> {
        let Some(t) = self.file.topups.iter_mut().find(|t| t.topup_id == id) else { return Ok(false) };
        f(t);
        self.save()?;
        Ok(true)
    }

    fn save(&self) -> Result<()> {
        write_private_json(&self.path, &self.file)
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
    /// Wait until the invoice behind `operation_id` is paid (and its eCash claimed), or will not
    /// be. `expires_at_ms`: the invoice's expiry, for a watcher that never hears a timeout.
    fn wait(&self, operation_id: String, expires_at_ms: u64) -> BoxFuture<'static, Result<LnOutcome>>;
}

/// The federation's `ln` module through the till's wallet.
pub struct WalletLightning {
    pub wallet: Wallet,
}

impl TopupLightning for WalletLightning {
    fn invoice(&self, amount_msat: u64, expiry_secs: u64) -> BoxFuture<'_, Result<(String, String)>> {
        Box::pin(async move {
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

    fn wait(&self, operation_id: String, expires_at_ms: u64) -> BoxFuture<'static, Result<LnOutcome>> {
        let wallet = self.wallet.clone();
        Box::pin(async move {
            use fedimint_ln_client::{LnReceiveState, receive::LightningReceiveError};
            use futures::StreamExt as _;
            let op = fedimint_core::core::OperationId(
                hex::decode(&operation_id).ok().and_then(|b| b.try_into().ok()).context("bad operation id")?,
            );
            let ln = wallet.lightning()?;
            let mut updates = ln.subscribe_ln_receive(op).await?.into_stream();
            let mut funded = false;
            loop {
                // Past the expiry (and a grace) with no payment seen: the gateway can no longer pay.
                let left = Duration::from_millis(expires_at_ms.saturating_sub(now_ms())) + EXPIRY_GRACE;
                let next = if funded { Ok(updates.next().await) } else { tokio::time::timeout(left, updates.next()).await };
                match next {
                    Err(_) => return Ok(LnOutcome::Expired),
                    Ok(None) => return Ok(LnOutcome::Failed("the receive ended without an outcome".into())),
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

    /// Watch every unpaid invoice (at startup).
    pub fn watch_waiting(self: &Arc<Self>) {
        let waiting: Vec<_> = self.store.lock().unwrap().all().iter().filter(|t| t.state == TopupState::Waiting).cloned().collect();
        for t in waiting {
            self.watch(&t);
        }
    }

    /// Follow one invoice until it is paid or will not be, and record that.
    pub fn watch(self: &Arc<Self>, t: &TopupRec) {
        let me = self.clone();
        let (id, fut) = (t.topup_id.clone(), self.ln.wait(t.operation_id.clone(), t.expires_at_ms));
        tokio::spawn(async move {
            let (state, detail) = match fut.await {
                Ok(LnOutcome::Paid) => (TopupState::Paid, None),
                Ok(LnOutcome::Expired) => (TopupState::Expired, None),
                Ok(LnOutcome::Failed(why)) => (TopupState::Failed, Some(why)),
                Err(e) => (TopupState::Failed, Some(format!("{e:#}"))),
            };
            let r = me.store.lock().unwrap().update(&id, |t| {
                if t.state == TopupState::Waiting {
                    t.state = state;
                    t.detail = detail;
                }
            });
            match r {
                Ok(_) => eprintln!("top-up {id}: {}", state.name()),
                Err(e) => eprintln!("top-up {id}: could not record {}: {e:#}", state.name()),
            }
        });
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A new top-up id: 16 hex characters.
pub fn new_id() -> String {
    hex::encode(rand::random::<[u8; 8]>())
}
