//! The reference holder loader (docs/OPS-PLAN.md § 6, phase O4): value comes from the holder's own
//! Fedimint wallet, and ops records it with no account and no token, only the card's short id.
//!
//! `card load` runs it with the data directory's wallet, which is a separate wallet from any
//! till's. The card needs no PIN: the loader reads keys (`GET_PUB`), it never signs.
//!
//! Order, the write-ahead order of `topup` with ops in the middle:
//!
//! 1. Read the short id off the card (slot 0) and the free slot keys from ops
//!    (`GET /cards/:id/slots`); check each chosen key against the card's own `GET_PUB`, so a wrong
//!    answer from ops cannot send value to keys the card does not hold. A loader run by a till
//!    ([`Loader::with_keyer`]) first keys never-keyed slots of a split-keyed card when the free
//!    ones are too few (`ops::key_slots`); a holder's own loader has no till token and cannot.
//! 2. Build the transaction inside a database transaction of the wallet's client
//!    (`finalize_and_submit_transaction_dbtx`): the txid is fixed, and nothing is sent or spent
//!    until that database transaction commits.
//! 3. Journal the notes with their blinding keys (`holder_loads.json`), then
//!    `POST /cards/:id/load`. If ops refuses, the database transaction is dropped: nothing left
//!    the wallet.
//! 4. Commit: the client submits. Wait for the federation, collect each note's signature, and
//!    `POST /cards/:id/load/outcome`; ops verifies every signature before the notes are live.
//!
//! A run that dies anywhere is finished by the next one ([`Loader::resume`]): a journalled load
//! whose operation is not in the client's log never committed, so it is reported rejected; one
//! that is gets its outcome from the federation, as a till's topup does.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use fedimint_card_host_card::Card;
use fedimint_client::transaction::TransactionBuilder;
use fedimint_core::core::OperationId;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::registry::ModuleRegistry;
use fedimint_core::secp256k1::XOnlyPublicKey;
use fedimint_core::{Amount, OutPoint, TransactionId};
use fedimint_mint_common::Nonce;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tbs::BlindingKey;

use crate::glue::{card_outputs, nonce_for, on_card};
use crate::ops::{OpsClient, OpsError, RemoteSlot, short_id_of};
use crate::plan;
use crate::shares::{self, FETCH_TIMEOUT, blind};
use crate::state::{LoadNote, write_private_json};
use crate::wallet::Wallet;

/// The journal of loads in flight, in the loader's data directory.
pub const JOURNAL_FILE: &str = "holder_loads.json";
/// Bound for waiting on the federation's decision.
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(90);

/// A load between `POST /load` and its outcome in ops.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingLoad {
    pub short_id: String,
    pub txid: String,
    /// The wallet client's operation, hex: present in its log exactly when the build committed.
    pub operation_id: String,
    pub notes: Vec<LoadNote>,
    /// Who asked for the load, if anyone did: a till's top-up id (docs/TOPUP.md), so the till
    /// can tell which top-up a load finished by a later run belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

/// The card has fewer free slots than the load has notes. Nothing was built or sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardFull {
    pub short_id: String,
    pub free: usize,
    pub needed: usize,
    /// Never-keyed slots the card also has, which only a till can key (at a tap).
    pub unkeyed: usize,
}

impl std::fmt::Display for CardFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "card {} has {} free slot(s) and this load needs {}", self.short_id, self.free, self.needed)?;
        if self.unkeyed > 0 {
            write!(f, " ({} more slot(s) have no key yet; a till keys them at a tap)", self.unkeyed)?;
        }
        Ok(())
    }
}

impl std::error::Error for CardFull {}

/// A load an earlier run left, as [`Loader::resume_each`] finished it.
#[derive(Debug, Clone)]
pub struct Finished {
    pub load: PendingLoad,
    /// `(slot, msat)` now live on the card; `None`: the load never became money.
    pub live: Option<Vec<(u8, u64)>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Journal {
    loads: Vec<PendingLoad>,
}

/// What a load put on the card.
#[derive(Debug, Clone)]
pub struct LoadReport {
    pub short_id: String,
    pub txid: String,
    /// `(slot, msat)` of each note now live on the card.
    pub notes: Vec<(u8, u64)>,
    /// Loads an earlier run left, finished by this one.
    pub resumed: usize,
}

/// Where a test can make a load "crash". Not for production use.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadCrash {
    /// Ops has the load, nothing was committed: the transaction never left the wallet.
    BeforeCommit,
    /// Committed (so submitted), before the outcome reaches ops.
    AfterCommit,
}

pub struct Loader<'w> {
    wallet: &'w Wallet,
    ops: OpsClient,
    journal: PathBuf,
    tag: Option<String>,
    /// A till's own registry client (with its token), for keying never-keyed slots.
    keyer: Option<OpsClient>,
    #[doc(hidden)]
    pub crash: Option<LoadCrash>,
}

impl<'w> Loader<'w> {
    /// A loader paying from `wallet`, against the registry at `ops_url`, journalling in `data_dir`.
    pub fn new(wallet: &'w Wallet, ops_url: &str, data_dir: &Path) -> Result<Self> {
        Ok(Self { wallet, ops: OpsClient::holder(ops_url)?, journal: data_dir.join(JOURNAL_FILE), tag: None, keyer: None, crash: None })
    }

    /// Mark this loader's loads with `tag` in the journal ([`PendingLoad::tag`]).
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    fn read_journal(&self) -> Result<Journal> {
        match std::fs::read(&self.journal) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", self.journal.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Journal::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.journal.display())),
        }
    }

    fn update_journal(&self, f: impl FnOnce(&mut Vec<PendingLoad>)) -> Result<()> {
        let mut j = self.read_journal()?;
        f(&mut j.loads);
        write_private_json(&self.journal, &j)
    }

    /// Let this loader key a split-keyed card's never-keyed slots when a load needs them, through
    /// `till`, a till's registry client: `POST /slot-keys` takes a till token, not a holder.
    pub fn with_keyer(mut self, till: OpsClient) -> Self {
        self.keyer = Some(till);
        self
    }

    /// Loads in flight from earlier runs.
    pub fn pending(&self) -> Result<Vec<PendingLoad>> {
        Ok(self.read_journal()?.loads)
    }

    /// Put `amounts` on the card in the field, one note per set bit of each, paid by the wallet.
    /// Finishes any load an earlier run left first.
    pub async fn load(&self, card: &mut dyn Card, amounts: &[u64]) -> Result<LoadReport> {
        let resumed = self.resume().await?;
        let notes: Vec<u64> = amounts.iter().flat_map(|a| plan::binary(*a)).collect();
        ensure!(!notes.is_empty(), "nothing to load");
        let m = self.wallet.min_denom();
        ensure!(notes.iter().all(|a| *a >= m), "amounts must be multiples of {m} msat (the federation's smallest economical note)");
        for a in &notes {
            ensure!(
                self.wallet.mint_cfg.tbs_pks.get(Amount::from_msats(*a)).is_some(),
                "{a} msat is not a note denomination of this federation"
            );
        }

        // 1. The card and its free slots.
        let slot0 = on_card(|| card.get_pub(0)).context("reading the card's slot 0 key")?;
        let short_id = short_id_of(&slot0);
        let mut free = self.ops.free_slots(&short_id).await.context("asking ops for the card's free slots")?;
        let ours = self.wallet.federation_id().to_string();
        ensure!(
            free.federation_id == ours,
            "card {short_id} belongs to federation {}, and this wallet is in {ours}",
            free.federation_id
        );
        if let Some(keyer) = &self.keyer {
            let n = crate::ops::slots_to_key(notes.len(), free.free.len(), free.unkeyed_slots.len());
            if n > 0 && free.free.len() + n >= notes.len() {
                let keyed = crate::ops::key_slots(card, keyer, &short_id, &free.unkeyed_slots[..n]).await?;
                free.version = keyed.answer.version;
                free.unkeyed_slots = keyed.answer.unkeyed_slots.clone();
                free.free.extend(keyed.keys.into_iter().map(|(slot, pubkey)| RemoteSlot { slot, pubkey }));
                free.free.sort_by_key(|s| s.slot);
            }
        }
        if free.free.len() < notes.len() {
            return Err(anyhow!(CardFull {
                short_id,
                free: free.free.len(),
                needed: notes.len(),
                unkeyed: free.unkeyed_slots.len()
            }));
        }
        let need: u64 = notes.iter().sum::<u64>() + notes.iter().map(|a| 2 * self.wallet.fee(*a)).sum::<u64>();
        let bal = self.wallet.balance_msat().await?;
        ensure!(bal >= need, "wallet holds {bal} msat but the load needs about {need} msat including fees");

        struct Out {
            slot: u8,
            amount: u64,
            nonce: Nonce,
            key: BlindingKey,
        }
        let mut outs = vec![];
        for (amount, s) in notes.iter().zip(&free.free) {
            let on_card_key = on_card(|| card.get_pub(s.slot)).with_context(|| format!("reading slot {}", s.slot))?;
            ensure!(
                on_card_key.to_string() == s.pubkey,
                "ops gave slot {} of card {short_id} a key the card does not hold; not loading",
                s.slot
            );
            let pk = XOnlyPublicKey::from_str(&s.pubkey).context("ops answered a bad key")?;
            outs.push(Out { slot: s.slot, amount: *amount, nonce: nonce_for(pk), key: BlindingKey::random() });
        }

        // 2. Built, not sent: the client submits only when this database transaction commits.
        let client = &self.wallet.client;
        let op = OperationId::new_random();
        let mut dbtx = client.db().begin_transaction().await;
        let bundle = card_outputs(self.wallet, outs.iter().map(|o| (o.amount, blind(o.nonce, o.key))));
        let range = client
            .finalize_and_submit_transaction_dbtx(
                &mut dbtx.to_ref_nc(),
                op,
                "card-holder-load",
                |_| (),
                TransactionBuilder::new().with_outputs(bundle),
            )
            .await
            .map_err(|e| anyhow!(e))
            .context("building the load transaction")?;
        let txid = range.txid();
        let pending = PendingLoad {
            short_id: short_id.clone(),
            txid: txid.to_string(),
            operation_id: hex::encode(op.0),
            notes: outs
                .iter()
                .enumerate()
                .map(|(i, o)| LoadNote {
                    slot: o.slot,
                    amount_msat: o.amount,
                    nonce: hex::encode(o.nonce.0.serialize()),
                    blinding_key: o.key.consensus_encode_to_hex(),
                    out_idx: i as u64,
                })
                .collect(),
            tag: self.tag.clone(),
        };

        // 3. Journal, then ops; a refusal drops the build and nothing has left the wallet.
        self.update_journal(|l| l.push(pending.clone()))?;
        let body = json!({"version": free.version, "txid": pending.txid, "notes": pending.notes});
        if let Err(e) = self.ops.load(&short_id, &body).await {
            drop(dbtx);
            if !matches!(e, OpsError::Unreachable(_)) {
                // A definite refusal: ops holds nothing. Without an answer it may, and the next
                // run tells it the load was never sent.
                self.update_journal(|l| l.retain(|p| p.txid != pending.txid))?;
            }
            return Err(e).context("recording the load in ops before submitting it");
        }
        if self.crash == Some(LoadCrash::BeforeCommit) {
            bail!("simulated crash before commit");
        }

        // 4. Send, and finish.
        dbtx.commit_tx_result().await.context("committing the load (it was not sent; the next run tells ops)")?;
        if self.crash == Some(LoadCrash::AfterCommit) {
            bail!("simulated crash after commit");
        }
        match self.finish(&pending).await? {
            Some(live) => Ok(LoadReport { short_id, txid: pending.txid, notes: live, resumed }),
            None => bail!("the federation did not take load {}; nothing moved", pending.txid),
        }
    }

    /// Finish every load an earlier run left, accepted or not. Returns how many were finished; one
    /// that cannot be finished yet stays in the journal and is an error.
    pub async fn resume(&self) -> Result<usize> {
        Ok(self.resume_each().await?.len())
    }

    /// [`Self::resume`], saying what became of each load.
    pub async fn resume_each(&self) -> Result<Vec<Finished>> {
        let mut done = vec![];
        for p in self.pending()? {
            let live =
                self.finish(&p).await.with_context(|| format!("finishing the earlier load {} to card {}", p.txid, p.short_id))?;
            done.push(Finished { load: p, live });
        }
        Ok(done)
    }

    /// The federation's answer for `p`, recorded in ops, and the journal entry removed. `None`:
    /// the load never became money.
    async fn finish(&self, p: &PendingLoad) -> Result<Option<Vec<(u8, u64)>>> {
        let client = &self.wallet.client;
        let op = OperationId(
            hex::decode(&p.operation_id).ok().and_then(|b| b.try_into().ok()).context("bad operation id in the journal")?,
        );
        let txid = TransactionId::from_str(&p.txid).map_err(|_| anyhow!("bad txid in the journal"))?;
        let outcome = if client.operation_log().get_operation(op).await.is_none() {
            // The build never committed: the federation never saw it.
            json!({"txid": p.txid, "accepted": false})
        } else {
            let decided = tokio::time::timeout(SUBMIT_TIMEOUT, client.transaction_updates(op).await.await_tx_accepted(txid))
                .await
                .context("the load is still undecided after 90 s; run again to finish it")?;
            match decided {
                Err(e) => {
                    tracing::warn!("the federation rejected load {}: {e}", p.txid);
                    json!({"txid": p.txid, "accepted": false})
                }
                Ok(()) => {
                    let mut sigs = vec![];
                    for n in &p.notes {
                        let nonce = Nonce(fedimint_core::secp256k1::PublicKey::from_str(&n.nonce).context("bad nonce in the journal")?);
                        let key = BlindingKey::consensus_decode_hex(&n.blinding_key, &ModuleRegistry::default())
                            .map_err(|_| anyhow!("bad blinding key in the journal"))?;
                        let sig = shares::note_signature(
                            self.wallet,
                            OutPoint { txid, out_idx: n.out_idx },
                            Amount::from_msats(n.amount_msat),
                            nonce,
                            key,
                            FETCH_TIMEOUT,
                        )
                        .await
                        .with_context(|| format!("collecting the signature for slot {}", n.slot))?;
                        sigs.push(json!({"nonce": n.nonce, "signature": sig.consensus_encode_to_hex()}));
                    }
                    json!({"txid": p.txid, "notes": sigs})
                }
            }
        };
        let accepted = outcome["accepted"] != json!(false);
        match self.ops.load_outcome(&p.short_id, &outcome).await {
            Ok(_) => {}
            // Rejected and ops never had it, or already ended it: nothing to record.
            Err(OpsError::UnknownCard(_)) if !accepted => {}
            Err(e) => return Err(e).context("recording the load's outcome in ops"),
        }
        self.update_journal(|l| l.retain(|q| q.txid != p.txid))?;
        Ok(accepted.then(|| p.notes.iter().map(|n| (n.slot, n.amount_msat)).collect()))
    }
}

/// The body ops takes at `POST /cards/:id/load/outcome` for an accepted load, from `notes`
/// (nonce, signature hex). For tests that tamper with it.
#[doc(hidden)]
pub fn outcome_body(txid: &str, notes: &[(String, String)]) -> Value {
    json!({"txid": txid, "notes": notes.iter().map(|(n, s)| json!({"nonce": n, "signature": s})).collect::<Vec<_>>()})
}
