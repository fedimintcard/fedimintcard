//! Mint v1 glue between the card, the state file and the federation.
//!
//! The fedimint client assumes it holds spend keys. Here the keys are on the card, so spends are
//! built by hand: `Transaction { inputs, outputs, nonce, signatures }`, with the signatures
//! produced by the card over `tx_hash()`. Topups reuse the stock client (it funds the transaction
//! from the wallet) with an explicit output blinded to a card key.
//!
//! Ordering rule everywhere: write the state file first, then talk to the card or the federation.
//! A spend leaves a `PendingTx` plus, for every output, a record holding its blinding key (and
//! secret key for host notes) and its output index. Lifecycle of a `PendingTx`:
//!
//! 1. `submitted = false`: built and maybe signed, never sent. After a restart it is abandoned.
//! 2. `submitted = true`: the signed transaction is stored; recovery resubmits it (idempotent)
//!    until the federation answers accepted or rejected.
//! 3. `accepted = true`: only collection of the outputs remains; it is retried until it succeeds.

use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use fedimint_card_host_card::{
    Card, MIN_SLOTS, SLOTS, SW_CONDITIONS_NOT_SATISFIED, SW_NO_SPACE, SlotState, StatusWord, past_last_slot, status_word,
};
use fedimint_client::transaction::{ClientOutput, ClientOutputBundle, TransactionBuilder};
use fedimint_core::core::{IntoDynInstance, OperationId};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::Amounts;
use fedimint_core::module::registry::ModuleRegistry;
use fedimint_core::secp256k1::hashes::Hash as _;
use fedimint_core::secp256k1::rand::{self, Rng};
use fedimint_core::secp256k1::{Keypair, Message, PublicKey, SECP256K1, SecretKey, XOnlyPublicKey};
use fedimint_core::transaction::{Transaction, TransactionError, TransactionSignature, TransactionSubmissionOutcome};
use fedimint_core::{Amount, OutPoint, TieredMulti, TransactionId};
use fedimint_mint_client::api::MintFederationApi as _;
use fedimint_mint_client::{OOBNotes, SpendableNote};
use fedimint_mint_common::{BlindNonce, MintInput, MintOutput, Nonce, Note};
use tbs::BlindingKey;

use crate::ops::{OpsError, OpsLink};
use crate::plan::{self, Plan};
use crate::shares::{self, FETCH_TIMEOUT, OutSpec, blind};
use crate::state::{
    HostKind, HostPending, IssueRec, LoadNote, NoteRec, NoteStatus, OpsWrite, PendingOverwrite, PendingTx, StateStore, Tombstone,
};
use crate::wallet::Wallet;

// Moved to the core the POS phones share (docs/POS-OFFLINE.md); kept at the old paths.
pub use fedimint_card_pos_core::{nonce_for, sig48_of};

/// Short wait used when recovering old records, where the answer may simply not exist.
const RECOVER_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound for waiting on a transaction decision.
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(90);

/// Outcomes of a spend that callers (the till service) tell apart. The text is exactly what the
/// CLI has always printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpendFault {
    /// The federation rejected the transaction: nothing moved.
    Declined(String),
    /// Submitted, no decision yet; recovery resubmits it on the next command.
    Undecided(String),
    /// An earlier transaction is still unresolved, so nothing new was started.
    Unresolved(String),
    /// The card in the field is not the one this wallet was set up with.
    WrongCard(String),
}

impl std::fmt::Display for SpendFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpendFault::Declined(m) | SpendFault::Undecided(m) | SpendFault::Unresolved(m) | SpendFault::WrongCard(m) => f.write_str(m),
        }
    }
}
impl std::error::Error for SpendFault {}

/// A sale that became real (the federation accepted it), found while resolving a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotedSale {
    pub client_ref: String,
    pub txid: String,
    pub ts_unix_ms: u64,
    pub price_msat: u64,
    pub received_msat: u64,
    pub overpaid_msat: u64,
    pub fees_msat: u64,
    pub inputs: Vec<(u8, u64)>,
    pub change: Vec<(u8, u64)>,
    /// The federation accepted it but the till notes are not all in the wallet yet.
    pub collection_pending: bool,
    /// The card it was charged to ([`CardWallet::card_short_id`]).
    pub short_id: Option<String>,
}

impl PromotedSale {
    fn from_ptx(p: &PendingTx, received_msat: u64, collection_pending: bool, short_id: Option<String>) -> Option<Self> {
        Some(Self {
            client_ref: p.client_ref.clone()?,
            txid: p.txid.clone(),
            ts_unix_ms: p.ts_unix_ms,
            price_msat: p.price_msat,
            received_msat,
            overpaid_msat: p.overpay_msat,
            fees_msat: p.fees_msat,
            inputs: p.spent.clone(),
            change: p.change.clone(),
            collection_pending,
            short_id,
        })
    }
}

/// Called with a sale just before its `PendingTx` is removed. If it fails the record stays.
pub type PromoteHook = Box<dyn Fn(&PromotedSale) -> Result<()>>;

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Run a blocking card call from inside a multi-thread tokio runtime.
pub fn on_card<R>(f: impl FnOnce() -> R) -> R {
    tokio::task::block_in_place(f)
}

fn enc<T: Encodable>(t: &T) -> String {
    t.consensus_encode_to_hex()
}

/// Decode a stored value. The input may be secret, so it is never put in the error.
fn dec<T: Decodable>(s: &str) -> Result<T> {
    T::consensus_decode_hex(s, &ModuleRegistry::default()).map_err(|e| anyhow!("decoding a stored value failed: {e}"))
}

/// Mint outputs for card notes, `(msat, blinded nonce)` each, with no state machine: the caller
/// holds the blinding keys and collects the signatures itself. The stock client puts explicit
/// outputs first, so output i of the transaction is note i.
pub fn card_outputs(wallet: &Wallet, outs: impl Iterator<Item = (u64, tbs::BlindedMessage)>) -> ClientOutputBundle {
    ClientOutputBundle::new_no_sm(
        outs.map(|(amount, blinded)| ClientOutput {
            output: MintOutput::new_v0(Amount::from_msats(amount), BlindNonce(blinded)),
            amounts: Amounts::new_bitcoin(Amount::from_msats(amount)),
        })
        .collect(),
    )
    .into_dyn(wallet.mint_id)
}

fn nonce_hex(n: Nonce) -> String {
    hex::encode(n.0.serialize())
}

pub(crate) fn nonce_from_hex(s: &str) -> Result<Nonce> {
    Ok(Nonce(PublicKey::from_str(s).context("bad nonce hex")?))
}

fn xonly(hex_str: &str) -> Result<XOnlyPublicKey> {
    XOnlyPublicKey::from_str(hex_str).context("bad pubkey hex")
}

fn parse_txid(s: &str) -> Result<TransactionId> {
    TransactionId::from_str(s).map_err(|_| anyhow!("bad txid {s}"))
}

#[derive(Debug, Clone)]
pub struct NoteView {
    pub slot: u8,
    pub amount_msat: u64,
    pub status: NoteStatus,
    /// A live note this job holds back: its slot signed a transaction nobody here knows
    /// ([`crate::state::Held`]).
    pub held: bool,
}

/// What recovery of interrupted work found and did.
#[derive(Debug, Clone, Default)]
pub struct Recovered {
    /// Card notes that became live.
    pub notes_live: usize,
    /// Face value of host notes reissued into the wallet.
    pub till_received: u64,
    /// Federation fees the wallet paid for those reissues.
    pub till_fee: u64,
    /// Ecash strings from legacy `cashout` records (already appended to `cashouts.log`).
    pub cashouts: Vec<String>,
    /// Transactions that never left this machine, or were rejected, and were abandoned.
    pub abandoned: usize,
    /// Groups that could not be completed this time (each is retried on the next command).
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BalanceReport {
    /// Every note on the card's current keys with its status (history is in the state file).
    pub notes: Vec<NoteView>,
    /// What the card can pay: its live notes, less those held back.
    pub live_msat: u64,
    /// The live notes held back, until the registry releases them ([`crate::state::Held`]).
    pub stranded_msat: u64,
    /// When the last of them may be released (ms, the registry's clock); `None` when there is no
    /// date to wait for ([`crate::state::CardState::stranded_until_ms`]).
    pub stranded_until_ms: Option<u64>,
    pub free_slots: Vec<u8>,
    pub overwritable_slots: Vec<u8>,
    pub recovered: Recovered,
    /// Transactions still undecided or still collecting.
    pub unresolved: Vec<String>,
}

/// Bound on how many renewal rounds a spend may make before giving up on finding a plan.
const RENEW_ROUNDS: usize = 3;

#[derive(Debug, Clone)]
pub struct SpendReport {
    /// The federation accepted this transaction: the money has moved.
    pub txid: TransactionId,
    pub inputs: Vec<(u8, u64)>,
    pub price_msat: u64,
    pub till_msat: u64,
    pub overpay_msat: u64,
    pub change: Vec<(u8, u64)>,
    /// Face value of the till notes reissued into the wallet (0 while collection is pending).
    pub received: Amount,
    pub fees_msat: u64,
    pub renewed_slots: Vec<u8>,
    /// Some output is below the economical size because no dust-free exact balance existed.
    pub dust: bool,
    /// The spend is done but its outputs are not all collected yet; `card balance` finishes it.
    pub collection_pending: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CashoutReport {
    pub txid: TransactionId,
    pub slot: u8,
    pub note_msat: u64,
    /// Value that reached the wallet after the wallet's own reissue fees.
    pub gained_msat: u64,
    /// Ecash string for `exported_msat`, if it could be produced.
    pub ecash: Option<String>,
    pub exported_msat: u64,
    pub dust: bool,
    pub collection_pending: bool,
    pub warnings: Vec<String>,
}

/// Where a test can make a spend "crash". Not for production use.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// After signing, before the transaction is marked submitted. On ops: after ops has the
    /// signed bytes (so the charge counts as submitted), before it is sent.
    BeforeSubmit,
    /// After the federation accepted the transaction, before anything is collected.
    AfterSubmit,
    /// On ops: `submitted` gets no answer and is never posted.
    SubmittedUnreachable,
    /// On ops: `submitted` is posted and taken, but its answer is lost.
    SubmittedAnswerLost,
}

pub struct CardWallet {
    pub wallet: Wallet,
    pub store: StateStore,
    pub card: Box<dyn Card>,
    /// Ecash strings recovered from legacy records during this run; the CLI prints them.
    pub recovered_cashouts: Vec<String>,
    #[doc(hidden)]
    pub crash_point: Option<CrashPoint>,
    /// See [`PromoteHook`].
    pub on_promote: Option<PromoteHook>,
    /// Set for a job on a till configured with ops (`crate::ops`): the card side of `store` came
    /// from the registry, and every write to it goes back there.
    pub ops: Option<OpsLink>,
    /// The holder's PIN for this job (`POST /v1/charge` `pin`). When unset the PIN session uses
    /// `CARD_PIN`, which is for the CLI and the tests.
    pub pin: Option<Vec<u8>>,
    /// The newest block header the till keeps, shown to a 3.0 card whose clock is older
    /// (`crate::clock`). `None`: the card's clock is left alone.
    pub headers: Option<std::sync::Arc<crate::clock::Headers>>,
    /// Transactions `resolve_pending` abandoned (rejected, or never sent) during this wallet's
    /// life, oldest first. The offline sender reads it to tell a rejected offline charge.
    pub abandoned: Vec<String>,
    /// The spare reserve an online tap keeps a 3.0 card at ([`CardWallet::card_spares`]).
    pub spares: crate::ops::SpareConfig,
}

impl CardWallet {
    pub fn new(wallet: Wallet, store: StateStore, card: Box<dyn Card>) -> Self {
        Self {
            wallet,
            store,
            card,
            recovered_cashouts: vec![],
            crash_point: None,
            on_promote: None,
            ops: None,
            pin: None,
            headers: None,
            abandoned: vec![],
            spares: crate::ops::SpareConfig::default(),
        }
    }

    /// Open the card's PIN session for this selection.
    ///
    /// `SIGN`, and re-keying a slot that already holds a key, both refuse with `6A94` until the
    /// selection has verified the PIN. The applet clears that on every `select()`, so this has to
    /// run once after the card is opened and before anything signs. A `SoftCard` has no PIN state
    /// and answers the verify with success, so tests need no setup.
    ///
    /// The holder's PIN, when the job has one, is only verified: a card without a PIN is not
    /// pinned at a till with whatever the holder typed.
    pub fn verify_pin_session(&mut self) -> Result<()> {
        if let Some(pin) = self.pin.as_deref() {
            return on_card(|| self.card.verify_pin(pin)).context("verifying the holder's PIN");
        }
        let pin = fedimint_card_host_card::pin_from_env();
        on_card(|| fedimint_card_host_card::open_pin_session_with(self.card.as_mut(), &pin)).context("opening the PIN session")
    }

    fn fee_fn(&self) -> impl Fn(u64) -> u64 + use<> {
        let cfg = self.wallet.mint_cfg.fee_consensus.clone();
        move |a| cfg.fee(Amount::from_msats(a)).msats
    }

    fn require_tiers(&self, amounts: &[u64]) -> Result<()> {
        for a in amounts {
            ensure!(
                self.wallet.mint_cfg.tbs_pks.get(Amount::from_msats(*a)).is_some(),
                "{a} msat is not a note denomination of this federation"
            );
        }
        Ok(())
    }

    // ---------------------------------------------------------------- card setup

    /// Make sure every slot the card has (8 on a 2.0 card, 32 on 2.1; the walk stops at `6B00`)
    /// has a key, and cache the public keys.
    ///
    /// Plain setup keeps keys already on the card. A slot that has a key but no record in the
    /// state file is of unknown provenance: it is recorded as such, never used as a note nonce,
    /// and overwritten lazily when a topup or spend needs the slot.
    ///
    /// `fresh` is for a new wallet on a card that was used before: it replaces every key
    /// (GEN_KEY with P2 = 1) and records all slots as free. It refuses if the state file has any
    /// live or pending note, or any unresolved transaction, since that would destroy the keys
    /// those notes depend on.
    pub fn card_setup(&mut self, fresh: bool) -> Result<Vec<(u8, XOnlyPublicKey)>> {
        self.recover_overwrites()?;
        if fresh {
            let st = &self.store.state;
            ensure!(
                !st.notes.iter().any(|n| matches!(n.status, NoteStatus::Live | NoteStatus::Pending))
                    && st.pending_txs.is_empty()
                    && st.host_pending.is_empty(),
                "--fresh would destroy the keys of live or pending notes; cash them out first"
            );
        }
        let mut out = vec![];
        for slot in 0..SLOTS {
            let existing = match on_card(|| self.card.get_pub(slot)) {
                Ok(pk) => Some(pk),
                Err(e) if status_word(&e) == Some(SW_CONDITIONS_NOT_SATISFIED) => None,
                // Past the card's last slot: a 2.0 card ends at 8, a 2.1 card at 32. The card,
                // not a constant, says how many slots there are.
                Err(e) if past_last_slot(&e) => {
                    self.store.save()?;
                    ensure!(
                        slot >= MIN_SLOTS,
                        "the card has only {slot} slot(s) (status 6B00 for slot {slot}); flash the {SLOTS}-slot applet"
                    );
                    break;
                }
                Err(e) => return Err(e).with_context(|| format!("slot {slot}")),
            };
            let pk = match (existing, fresh) {
                (None, _) => match on_card(|| self.card.gen_key(slot)) {
                    Ok(pk) => pk,
                    Err(e) if status_word(&e) == Some(SW_NO_SPACE) => {
                        self.store.save()?;
                        bail!("the card is out of memory for slot {slot}'s key (status 6A84); it cannot be issued");
                    }
                    Err(e) => return Err(e).with_context(|| format!("slot {slot}")),
                },
                (Some(old), true) => {
                    let rec = PendingOverwrite { slot, old_pubkey: old.to_string() };
                    self.store.state.pending_overwrites.push(rec.clone());
                    self.store.save()?;
                    match on_card(|| self.card.gen_key_overwrite(slot)) {
                        Ok(pk) => {
                            self.store.state.pending_overwrites.retain(|q| q.slot != slot);
                            pk
                        }
                        Err(e) => {
                            self.resolve_overwrite(&rec).ok();
                            return Err(e).with_context(|| format!("re-keying slot {slot}"));
                        }
                    }
                }
                (Some(old), false) => old,
            };
            let hexpk = pk.to_string();
            if fresh || existing.is_none() {
                // A key this run created: free.
                self.store.state.set_slot_pubkey(slot, hexpk);
            } else {
                match self.store.state.slot_pubkey(slot) {
                    Some(rec) if rec == hexpk => {}
                    Some(rec) => {
                        // The card holds a different key than we recorded.
                        let busy =
                            self.store.state.notes_on_current_key(slot).any(|n| matches!(n.status, NoteStatus::Live | NoteStatus::Pending));
                        ensure!(
                            !busy,
                            "slot {slot} holds a different key than recorded ({rec}) and has live notes; \
                                        this is not the card the state file belongs to"
                        );
                        self.store.state.set_unknown_slot(slot, hexpk);
                    }
                    None => self.store.state.set_unknown_slot(slot, hexpk),
                }
            }
            out.push((slot, pk));
        }
        self.store.save()?;
        Ok(out)
    }

    // ---------------------------------------------------------------- slot overwrite

    /// Finish or roll back overwrites that were started and may not have completed.
    pub fn recover_overwrites(&mut self) -> Result<()> {
        let pending = self.store.state.pending_overwrites.clone();
        for p in pending {
            self.resolve_overwrite(&p)?;
        }
        Ok(())
    }

    fn resolve_overwrite(&mut self, p: &PendingOverwrite) -> Result<()> {
        let slot = p.slot;
        let now = match on_card(|| self.card.get_pub(slot)) {
            Ok(pk) => Some(pk),
            Err(e) if status_word(&e) == Some(SW_CONDITIONS_NOT_SATISFIED) => None,
            Err(e) => return Err(e).with_context(|| format!("checking slot {slot} after an interrupted overwrite")),
        };
        let new = match now {
            // Same key as before: the overwrite never ran.
            Some(pk) if pk.to_string() == p.old_pubkey => None,
            // A new key exists: adopt it.
            Some(pk) => Some(pk),
            // Slot is empty: the wipe happened, the new key did not. Create it (never an overwrite).
            None => Some(on_card(|| self.card.gen_key(slot))?),
        };
        if let Some(pk) = new {
            self.store.state.set_slot_pubkey(slot, pk.to_string());
            self.ops_enqueue(OpsWrite::Slot { slot, old_pubkey: p.old_pubkey.clone(), pubkey: pk.to_string() });
        }
        self.store.state.pending_overwrites.retain(|q| q.slot != slot);
        self.store.save()
    }

    /// Replace the key in `slot` (GEN_KEY with P2 = 1). Refuses unless every note on the slot's
    /// current key is spent or dead. Follows the contract of `Card::gen_key_overwrite`.
    pub fn overwrite_slot(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        ensure!(
            self.store.state.overwritable_slots().contains(&slot),
            "slot {slot} still carries live or pending notes, or has no spent note to retire"
        );
        let old = self.store.state.slot_pubkey(slot).expect("overwritable implies cached").to_owned();
        ensure!(self.may_renew(slot), "slot {slot}'s key names the card in ops and is never renewed");
        let rec = PendingOverwrite { slot, old_pubkey: old };
        self.store.state.pending_overwrites.push(rec.clone());
        self.store.save()?;
        match on_card(|| self.card.gen_key_overwrite(slot)) {
            Ok(pk) => {
                self.store.state.set_slot_pubkey(slot, pk.to_string());
                self.ops_enqueue(OpsWrite::Slot { slot, old_pubkey: rec.old_pubkey.clone(), pubkey: pk.to_string() });
                self.store.state.pending_overwrites.retain(|q| q.slot != slot);
                self.store.save()?;
                Ok(pk)
            }
            Err(e) => {
                // Learn what happened before reporting; never retry the overwrite blindly.
                self.resolve_overwrite(&rec).ok();
                Err(e).with_context(|| format!("overwriting slot {slot}"))
            }
        }
    }

    /// Overwrite every slot whose note is confirmed spent on the federation (or dead).
    /// Returns the slots renewed. Failures are reported, not fatal.
    pub async fn renew_spent_slots(&mut self) -> Result<Vec<u8>> {
        let mut done = vec![];
        let slots: Vec<u8> = self.store.state.overwritable_slots().into_iter().filter(|s| self.may_renew(*s)).collect();
        for slot in slots {
            if !self.renewable(slot).await? {
                continue;
            }
            match self.overwrite_slot(slot) {
                Ok(_) => done.push(slot),
                Err(e) => tracing::warn!("could not renew slot {slot}: {e:#}"),
            }
        }
        Ok(done)
    }

    /// Whether every spent note on `slot`'s current key is spent on the federation too, so the
    /// key holds nothing and may be overwritten.
    pub(crate) async fn renewable(&self, slot: u8) -> Result<bool> {
        let notes: Vec<NoteRec> = self.store.state.notes_on_current_key(slot).cloned().collect();
        for n in &notes {
            if n.status == NoteStatus::Spent && !self.is_spent(nonce_from_hex(&n.nonce)?).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    // ---------------------------------------------------------------- federation helpers

    pub async fn is_spent(&self, nonce: Nonce) -> Result<bool> {
        let api = self.wallet.client.api().with_module(self.wallet.mint_id);
        tokio::time::timeout(Duration::from_secs(10), api.check_note_spent(nonce))
            .await
            .context("asking the federation whether a note is spent timed out")?
            .map_err(Into::into)
    }

    async fn submit(&self, tx: Transaction) -> Result<Result<TransactionId, TransactionError>> {
        let client = &self.wallet.client;
        let outcome = tokio::time::timeout(SUBMIT_TIMEOUT, client.api().submit_transaction(tx))
            .await
            .context("transaction submission timed out; outcome unknown, run `card balance` to reconcile")?
            .try_into_inner(client.decoders())
            .context("decoding submission outcome")?;
        let TransactionSubmissionOutcome(res) = outcome;
        Ok(res)
    }

    // ---------------------------------------------------------------- topup

    /// Issue notes worth `amounts` to free card slots. Each amount becomes its binary
    /// decomposition (one note per set bit) and must be a multiple of the federation's smallest
    /// economical note. The wallet funds amount plus fees.
    pub async fn topup(&mut self, amounts: &[u64]) -> Result<Vec<(u8, u64)>> {
        self.topup_ref(amounts, None).await
    }

    /// [`topup`](Self::topup), recording `client_ref` in the same save that writes the pending
    /// notes — before anything is signed or submitted — so a repeat of the ref can be answered
    /// from state even if the process dies mid-call. The transaction id is filled in as soon as
    /// the wallet has built it.
    pub async fn topup_ref(&mut self, amounts: &[u64], client_ref: Option<&str>) -> Result<Vec<(u8, u64)>> {
        self.recover_overwrites()?;
        let notes: Vec<u64> = amounts.iter().flat_map(|a| plan::binary(*a)).collect();
        ensure!(!notes.is_empty(), "nothing to top up");
        let m = self.wallet.min_denom();
        ensure!(notes.iter().all(|a| *a >= m), "topup amounts must be multiples of {m} msat (the federation's smallest economical note)");
        self.require_tiers(&notes)?;
        let mut free = self.store.state.free_slots();
        if free.len() < notes.len() {
            self.renew_spent_slots().await.ok();
            free = self.store.state.free_slots();
        }
        if free.len() < notes.len() {
            // A split-keyed card on ops: key never-keyed slots while the card is here.
            self.ops_make_room(notes.len()).await?;
            free = self.store.state.free_slots();
        }
        if free.len() < notes.len() {
            return Err(plan::PlanError::NoFreeSlots { need: notes.len(), free: free.len() }.into());
        }
        let need: u64 = notes.iter().sum::<u64>() + notes.iter().map(|a| 2 * self.wallet.fee(*a)).sum::<u64>();
        let bal = self.wallet.balance_msat().await?;
        ensure!(bal >= need, "wallet holds {bal} msat but the topup needs about {need} msat including fees");

        struct Out {
            slot: u8,
            amount: u64,
            nonce: Nonce,
            key: BlindingKey,
            blinded: tbs::BlindedMessage,
        }
        let mut outs = vec![];
        for (amount, slot) in notes.iter().zip(free) {
            let pk = xonly(self.store.state.slot_pubkey(slot).expect("free implies cached"))?;
            let nonce = nonce_for(pk);
            let key = BlindingKey::random();
            let blinded = blind(nonce, key);
            outs.push(Out { slot, amount: *amount, nonce, key, blinded });
        }
        // Persist before the federation hears of it. The stock client puts explicit outputs
        // first, so output i of the transaction is note i.
        for (i, o) in outs.iter().enumerate() {
            self.store.state.notes.push(NoteRec {
                slot: o.slot,
                amount_msat: o.amount,
                nonce: nonce_hex(o.nonce),
                signature: None,
                status: NoteStatus::Pending,
                blinding_key: Some(enc(&o.key)),
                txid: None,
                out_idx: Some(i as u64),
            });
        }
        // The ref lands in this save or not at all: a replay can then never see a note that the
        // record does not cover, nor a record over notes that were never written.
        if let Some(cref) = client_ref {
            self.store.state.issues.retain(|r| r.client_ref != cref);
            self.store.state.issues.push(IssueRec {
                client_ref: cref.to_owned(),
                txid: String::new(),
                notes: outs.iter().map(|o| (o.slot, o.amount)).collect(),
                amounts: amounts.to_vec(),
                ts_unix_ms: now_ms(),
            });
        }
        self.store.save()?;

        let bundle = card_outputs(&self.wallet, outs.iter().map(|o| (o.amount, o.blinded)));
        let client = &self.wallet.client;
        let op = OperationId::new_random();
        let range =
            match client.finalize_and_submit_transaction(op, "card-topup", |_| (), TransactionBuilder::new().with_outputs(bundle)).await {
                Ok(r) => r,
                Err(e) => {
                    // Nothing was sent: forget the pending records and any ref pointing at them.
                    let nonces: Vec<String> = outs.iter().map(|o| nonce_hex(o.nonce)).collect();
                    self.store.state.notes.retain(|n| !(nonces.contains(&n.nonce) && n.status == NoteStatus::Pending));
                    if let Some(cref) = client_ref {
                        self.store.state.issues.retain(|r| r.client_ref != cref);
                    }
                    self.store.save()?;
                    return Err(anyhow!(e)).context("building the topup transaction");
                }
            };
        let txid = range.txid();
        for n in self.store.state.notes.iter_mut().filter(|n| n.status == NoteStatus::Pending && n.txid.is_none()) {
            if outs.iter().any(|o| nonce_hex(o.nonce) == n.nonce) {
                n.txid = Some(txid.to_string());
            }
        }
        if let Some(cref) = client_ref
            && let Some(r) = self.store.state.issues.iter_mut().find(|r| r.client_ref == cref)
        {
            r.txid = txid.to_string();
        }
        // On ops the notes are recorded once they are live (the stock client builds and sends in
        // one call, so there are no unsigned bytes to post first). The blinding keys are kept in
        // the queued write because the registry records each note with its key.
        self.ops_enqueue(OpsWrite::Load {
            txid: txid.to_string(),
            client_ref: client_ref.map_or_else(|| txid.to_string(), str::to_owned),
            notes: outs
                .iter()
                .enumerate()
                .map(|(i, o)| LoadNote {
                    slot: o.slot,
                    amount_msat: o.amount,
                    nonce: nonce_hex(o.nonce),
                    blinding_key: enc(&o.key),
                    out_idx: i as u64,
                })
                .collect(),
        });
        self.store.save()?;

        let client = &self.wallet.client;
        let accepted = tokio::time::timeout(SUBMIT_TIMEOUT, client.transaction_updates(op).await.await_tx_accepted(txid))
            .await
            .context("topup transaction undecided after 90 s; run `card balance` later to reconcile")?;
        if let Err(e) = accepted {
            for n in self.store.state.notes.iter_mut().filter(|n| n.txid.as_deref() == Some(&txid.to_string())) {
                n.status = NoteStatus::Dead;
                n.blinding_key = None;
            }
            self.store.save()?;
            bail!("federation rejected the topup: {e}");
        }
        for (i, o) in outs.iter().enumerate() {
            self.finalize_card_note(&nonce_hex(o.nonce), OutPoint { txid, out_idx: i as u64 }, FETCH_TIMEOUT).await?;
        }
        self.sync_card_notes();
        self.ops_sync().await;
        Ok(outs.iter().map(|o| (o.slot, o.amount)).collect())
    }

    /// Fetch the signature for a pending card note at a known outpoint and make it live.
    async fn finalize_card_note(&mut self, nonce_h: &str, outpoint: OutPoint, timeout: Duration) -> Result<()> {
        let idx = self
            .store
            .state
            .notes
            .iter()
            .position(|n| n.nonce == nonce_h && n.status == NoteStatus::Pending)
            .context("no pending note for this nonce")?;
        let rec = self.store.state.notes[idx].clone();
        let key: BlindingKey = dec(rec.blinding_key.as_deref().context("pending note without blinding key")?)?;
        let nonce = nonce_from_hex(&rec.nonce)?;
        let amount = Amount::from_msats(rec.amount_msat);
        let signature = shares::note_signature(&self.wallet, outpoint, amount, nonce, key, timeout).await?;
        let n = &mut self.store.state.notes[idx];
        n.signature = Some(enc(&signature));
        n.status = NoteStatus::Live;
        n.blinding_key = None;
        n.txid = Some(outpoint.txid.to_string());
        n.out_idx = Some(outpoint.out_idx);
        self.store.save()
    }

    // ---------------------------------------------------------------- recovery

    /// Fallback for records without an output index (older state files): ask the federation
    /// where the blind nonce was used. Fails until consensus has processed the transaction, and
    /// older federations do not have the endpoint at all.
    async fn outpoint_by_blind_nonce(&self, blinded: tbs::BlindedMessage) -> Option<OutPoint> {
        let api = self.wallet.client.api().with_module(self.wallet.mint_id);
        let ops = api.fetch_blind_nonce_outpoints(vec![BlindNonce(blinded)]).await.ok()?;
        ops.into_iter().next()
    }

    async fn outpoint_of(&self, txid: Option<&str>, out_idx: Option<u64>, blinded: tbs::BlindedMessage) -> Option<OutPoint> {
        match (txid, out_idx) {
            (Some(t), Some(i)) => Some(OutPoint { txid: parse_txid(t).ok()?, out_idx: i }),
            _ => self.outpoint_by_blind_nonce(blinded).await,
        }
    }

    /// Records of a transaction that never became money: its pending outputs die, the inputs
    /// (still live) are untouched.
    fn abandon_group(&mut self, txid: &str) {
        self.abandoned.push(txid.to_owned());
        // On ops: the charge is recorded rejected, so its change notes die there too.
        self.ops_enqueue(OpsWrite::Outcome { txid: txid.to_owned(), accepted: false, change: vec![] });
        let inputs: Vec<String> = self.store.state.pending_txs.iter().filter(|p| p.txid == txid).flat_map(|p| p.inputs.clone()).collect();
        for n in self
            .store
            .state
            .notes
            .iter_mut()
            .filter(|n| n.status == NoteStatus::Pending && n.txid.as_deref() == Some(txid) && !inputs.contains(&n.nonce))
        {
            n.status = NoteStatus::Dead;
            n.blinding_key = None;
        }
        self.store.state.host_pending.retain(|h| h.group != txid);
        self.store.state.pending_txs.retain(|p| p.txid != txid);
    }

    /// The federation accepted `txid`: wait for the global confirmation and mark the card notes it
    /// spent. Notes stay live (and are re-checked by `balance`) if the confirmation does not come.
    async fn confirm_inputs_spent(&mut self, txid: TransactionId, inputs: &[String], timeout: Duration) -> Result<()> {
        let confirmed = tokio::time::timeout(timeout, self.wallet.client.api().await_transaction(txid)).await.is_ok();
        for nonce_h in inputs {
            let spent = confirmed || self.is_spent(nonce_from_hex(nonce_h)?).await.unwrap_or(false);
            if spent {
                for r in self.store.state.notes.iter_mut().filter(|r| &r.nonce == nonce_h && r.status == NoteStatus::Live) {
                    r.status = NoteStatus::Spent;
                }
            }
        }
        self.store.save()
    }

    /// Collect every output of an accepted transaction. Returns whether all of it is done.
    async fn collect_group(&mut self, txid: &str, timeout: Duration, rec: &mut Recovered) -> Result<bool> {
        let mut all = true;
        // Card notes.
        // A note a charge spends is not one of its outputs, even where the registry marks it
        // pending under the charge's txid (an offline charge's inputs, `offline-charges`).
        let spending: Vec<String> = self.store.state.pending_txs.iter().flat_map(|p| p.inputs.clone()).collect();
        let pending: Vec<NoteRec> = self
            .store
            .state
            .notes
            .iter()
            .filter(|n| n.status == NoteStatus::Pending && n.txid.as_deref() == Some(txid))
            .filter(|n| !spending.contains(&n.nonce))
            .cloned()
            .collect();
        for n in pending {
            let (Some(bk), Ok(nonce)) = (n.blinding_key.as_deref(), nonce_from_hex(&n.nonce)) else { continue };
            let key: BlindingKey = dec(bk)?;
            let Some(op) = self.outpoint_of(n.txid.as_deref(), n.out_idx, blind(nonce, key)).await else {
                all = false;
                continue;
            };
            match self.finalize_card_note(&n.nonce, op, timeout).await {
                Ok(()) => rec.notes_live += 1,
                Err(e) => {
                    rec.errors.push(format!("{txid}: card note on slot {}: {e:#}", n.slot));
                    all = false;
                }
            }
        }
        // Host notes: all of them or none, because they travel as one reissue.
        let items: Vec<HostPending> = self.store.state.host_pending.iter().filter(|h| h.group == txid).cloned().collect();
        if !items.is_empty() {
            let mut notes = TieredMulti::<SpendableNote>::default();
            let mut complete = true;
            for h in &items {
                let kp = Keypair::from_secret_key(SECP256K1, &SecretKey::from_str(&h.secret_key).context("bad stored secret key")?);
                let key: BlindingKey = dec(&h.blinding_key)?;
                let nonce = Nonce(kp.public_key());
                let blinded = blind(nonce, key);
                let amount = Amount::from_msats(h.amount_msat);
                let Some(op) = self.outpoint_of(Some(txid), h.out_idx, blinded).await else {
                    complete = false;
                    break;
                };
                let got = async {
                    let sig =
                        shares::fetch_blind_signature(&self.wallet, op.txid, OutSpec { out_idx: op.out_idx, amount, blinded }, timeout)
                            .await?;
                    shares::finish_note(&self.wallet, amount, nonce, key, blinded, sig)
                }
                .await;
                match got {
                    Ok(signature) => notes.push(amount, SpendableNote { signature, spend_key: kp }),
                    Err(e) => {
                        rec.errors.push(format!("{txid}: host note {}: {e:#}", h.amount_msat));
                        complete = false;
                        break;
                    }
                }
            }
            if complete {
                let oob = OOBNotes::new(self.wallet.federation_id().to_prefix(), notes);
                match items[0].kind {
                    HostKind::Cashout => {
                        // Legacy record: the string must reach cashouts.log before the secret
                        // keys are forgotten.
                        let text = oob.to_string();
                        self.store.append_cashout_log(&text)?;
                        self.recovered_cashouts.push(text.clone());
                        rec.cashouts.push(text);
                    }
                    HostKind::Till => match self.wallet.import_notes(oob).await {
                        Ok(imp) => {
                            if !imp.already {
                                rec.till_received += imp.face.msats;
                                rec.till_fee += imp.fee_msat;
                            }
                        }
                        Err(e) => {
                            rec.errors.push(format!("{txid}: reissuing the host notes: {e:#}"));
                            complete = false;
                        }
                    },
                }
                if complete {
                    self.store.state.host_pending.retain(|h| h.group != txid);
                    self.store.save()?;
                }
            }
            all &= complete;
        }
        Ok(all)
    }

    /// Complete whatever interrupted commands left. Errors are collected per transaction, never
    /// propagated: one stuck group must not block the others.
    pub async fn resolve_pending(&mut self, timeout: Duration) -> Result<Recovered> {
        let mut rec = Recovered::default();
        let ptxs = self.store.state.pending_txs.clone();
        for ptx in ptxs {
            if let Err(e) = self.resolve_tx(ptx.clone(), timeout, &mut rec).await {
                rec.errors.push(format!("{}: {e:#}", ptx.txid));
            }
        }
        // Outputs with no PendingTx: topups, and records from older state files.
        let known: Vec<String> = self.store.state.pending_txs.iter().map(|p| p.txid.clone()).collect();
        let mut groups: Vec<String> = self.store.state.host_pending.iter().map(|h| h.group.clone()).collect();
        groups.extend(
            self.store
                .state
                .notes
                .iter()
                .filter(|n| n.status == NoteStatus::Pending)
                .filter(|n| !self.store.state.pending_txs.iter().any(|p| p.inputs.contains(&n.nonce)))
                .filter_map(|n| n.txid.clone()),
        );
        groups.sort();
        groups.dedup();
        for g in groups.into_iter().filter(|g| !known.contains(g)) {
            if let Err(e) = self.collect_group(&g, timeout, &mut rec).await {
                rec.errors.push(format!("{g}: {e:#}"));
            }
        }
        // Pending notes whose transaction id was never recorded (crash inside a topup).
        let blind_only: Vec<NoteRec> =
            self.store.state.notes.iter().filter(|n| n.status == NoteStatus::Pending && n.txid.is_none()).cloned().collect();
        for n in blind_only {
            let (Some(bk), Ok(nonce)) = (n.blinding_key.as_deref(), nonce_from_hex(&n.nonce)) else { continue };
            let Ok(key) = dec::<BlindingKey>(bk) else { continue };
            if let Some(op) = self.outpoint_by_blind_nonce(blind(nonce, key)).await {
                match self.finalize_card_note(&n.nonce, op, timeout).await {
                    Ok(()) => rec.notes_live += 1,
                    Err(e) => rec.errors.push(format!("slot {}: {e:#}", n.slot)),
                }
            }
        }
        Ok(rec)
    }

    async fn resolve_tx(&mut self, mut ptx: PendingTx, timeout: Duration, rec: &mut Recovered) -> Result<()> {
        let txid = ptx.txid.clone();
        let txid_parsed = parse_txid(&txid)?;
        if !ptx.submitted && ptx.ops_unconfirmed {
            // Signed bytes ops never answered for: post them again. A yes means send as normal; a
            // definitive no (resolved meanwhile) means it never left this till; no answer waits.
            if self.ops.is_none() {
                return Ok(());
            }
            let hex_tx = ptx.tx_hex.clone().context("unconfirmed charge without stored bytes")?;
            match self.ops_submitted(&txid, &hex_tx).await {
                Ok(()) => {
                    if let Some(p) = self.store.state.pending_txs.iter_mut().find(|p| p.txid == txid) {
                        p.submitted = true;
                        p.ops_unconfirmed = false;
                    }
                    self.store.save()?;
                    ptx.submitted = true;
                }
                Err(e) if crate::ops::unanswered(&e) => return Ok(()),
                Err(e) => {
                    tracing::warn!("charge {txid} was not taken by ops ({e:#}); it was never sent: abandoning");
                    self.abandon_group(&txid);
                    self.store.save()?;
                    rec.abandoned += 1;
                    return Ok(());
                }
            }
        }
        if !ptx.submitted {
            // Signed or not, it never left this machine.
            self.abandon_group(&txid);
            self.store.save()?;
            rec.abandoned += 1;
            return Ok(());
        }
        // Resubmit the stored bytes on every pass until the collection is complete: it is
        // idempotent, and a federation that lost or never fully processed the first submission
        // gets it again.
        let hex_tx = ptx.tx_hex.as_deref().context("submitted transaction without stored bytes")?;
        let tx = Transaction::consensus_decode_hex(hex_tx, self.wallet.client.decoders())
            .map_err(|e| anyhow!("decoding the stored transaction: {e}"))?;
        let mut confirmed = ptx.accepted;
        match tokio::time::timeout(timeout, self.submit(tx)).await {
            Err(_) if !ptx.accepted => return Ok(()), // still undecided
            Ok(Err(e)) if !ptx.accepted => return Err(e),
            Ok(Ok(Err(rejection))) if !ptx.accepted => {
                // A rejected resubmission does not mean the original failed: an older federation
                // may refuse a transaction it already included. Ask for the confirmation first.
                let included =
                    tokio::time::timeout(Duration::from_secs(10), self.wallet.client.api().await_transaction(txid_parsed)).await.is_ok();
                // Why abandoning below cannot hit a transaction the federation later accepts: the
                // federation's verdict on a transaction is final and the same on every submission,
                // so a rejection means it was never (and will never be) included. A transaction
                // that was in fact included is caught by `included` just above, or has spent its
                // inputs (the `bail!` below, which keeps the record). Only a rejected
                // transaction with a still-unspent input reaches the abandon.
                if included {
                    confirmed = true;
                } else {
                    let mut some_unspent = false;
                    for n in &ptx.inputs {
                        if !self.is_spent(nonce_from_hex(n)?).await? {
                            some_unspent = true;
                        }
                    }
                    if some_unspent {
                        tracing::warn!("transaction {txid} was rejected ({rejection}) and an input is unspent: abandoning");
                        self.abandon_group(&txid);
                        self.store.save()?;
                        rec.abandoned += 1;
                        return Ok(());
                    }
                    // Every input is spent but we cannot show it was by this transaction (the API
                    // cannot say which one spent a note): do not abandon, keep trying.
                    bail!("resubmission rejected ({rejection}), inputs are spent but the transaction is not confirmed");
                }
            }
            Ok(Ok(Ok(_))) => {}
            _ => {} // already confirmed earlier; the resubmission result no longer matters
        }
        if !confirmed {
            confirmed = tokio::time::timeout(timeout, self.wallet.client.api().await_transaction(txid_parsed)).await.is_ok();
        }
        if !confirmed {
            return Ok(());
        }
        if !ptx.accepted {
            if let Some(p) = self.store.state.pending_txs.iter_mut().find(|p| p.txid == txid) {
                p.accepted = true;
            }
            self.store.save()?;
        }
        let received_before = rec.till_received;
        let collected = self.collect_group(&txid, timeout, rec).await?;
        // The card's side is done once its change notes are live, whether or not this till's own
        // notes are collected yet: ops must not keep the card's charge open for the merchant's.
        self.ops_accepted(&txid);
        self.store.save()?;
        if collected {
            // Make sure the card notes this transaction spent are marked before the record goes.
            self.confirm_inputs_spent(txid_parsed, &ptx.inputs, RECOVER_TIMEOUT).await?;
            // A record that carries a client_ref is never removed unless its sale is recorded:
            // either the hook succeeded, or (no hook, e.g. the CLI without a ledger) a tombstone
            // is written in the same save that removes the record.
            if let Some(sale) = PromotedSale::from_ptx(&ptx, rec.till_received.saturating_sub(received_before), false, self.card_short_id())
            {
                match &self.on_promote {
                    Some(hook) => hook(&sale).context("recording the sale")?,
                    None => self.store.state.tombstones.push(Tombstone {
                        client_ref: sale.client_ref,
                        txid: sale.txid,
                        price_msat: sale.price_msat,
                        accepted: true,
                        ts_unix_ms: sale.ts_unix_ms,
                        overpay_msat: sale.overpaid_msat,
                        fees_msat: sale.fees_msat,
                        spent: sale.inputs,
                        change: sale.change,
                    }),
                }
            }
            self.store.state.pending_txs.retain(|p| p.txid != txid);
            self.store.save()?;
        }
        Ok(())
    }

    fn unresolved(&self) -> Vec<String> {
        self.store.state.pending_txs.iter().map(|p| p.txid.clone()).collect()
    }

    /// The unresolved transaction that belongs to `client_ref`, if any.
    pub fn pending_for_ref(&self, client_ref: &str) -> Option<&PendingTx> {
        self.store.state.pending_txs.iter().find(|p| p.client_ref.as_deref() == Some(client_ref))
    }

    /// Sales the federation has accepted whose records are still waiting for their till notes.
    pub fn accepted_refs(&self) -> Vec<PromotedSale> {
        let card = self.card_short_id();
        self.store
            .state
            .pending_txs
            .iter()
            .filter(|p| p.accepted)
            .filter_map(|p| PromotedSale::from_ptx(p, 0, true, card.clone()))
            .collect()
    }

    /// The short id of the card this wallet's state belongs to: the card a job attached on ops,
    /// else the one whose slot 0 key the local state holds. `None` before a card was set up.
    pub fn card_short_id(&self) -> Option<String> {
        if let Some(o) = &self.ops {
            return Some(o.short_id.clone());
        }
        let pk = XOnlyPublicKey::from_str(self.store.state.slot_pubkey(0)?).ok()?;
        Some(crate::ops::short_id_of(&pk))
    }

    /// Check that the card in the field is the one this wallet was set up with: the public key
    /// of the first cached slot without an interrupted overwrite must match the cached one (a
    /// slot mid-overwrite may legitimately hold either key). Skipped when there is no such slot.
    /// Only an actual mismatch, or an empty slot, is a different card; a transport failure keeps
    /// its own error.
    pub fn verify_card(&mut self) -> Result<()> {
        let st = &self.store.state;
        let Some(slot) = st.slots.iter().map(|s| s.slot).find(|s| !st.pending_overwrites.iter().any(|p| p.slot == *s)) else {
            return Ok(());
        };
        let want = st.slot_pubkey(slot).unwrap_or_default().to_owned();
        let got = match on_card(|| self.card.get_pub(slot)) {
            Ok(k) => k,
            Err(e) if status_word(&e) == Some(SW_CONDITIONS_NOT_SATISFIED) => {
                return Err(SpendFault::WrongCard(format!("different card presented (slot {slot} is empty)")).into());
            }
            Err(e) => return Err(e),
        };
        if got.to_string() != want {
            return Err(SpendFault::WrongCard("different card presented".into()).into());
        }
        Ok(())
    }

    /// Sales waiting in tombstones.
    pub fn tombstoned_sales(&self) -> Vec<(PromotedSale, Tombstone)> {
        let card = self.card_short_id();
        self.store
            .state
            .tombstones
            .iter()
            .map(|t| {
                let sale = PromotedSale {
                    client_ref: t.client_ref.clone(),
                    txid: t.txid.clone(),
                    ts_unix_ms: t.ts_unix_ms,
                    price_msat: t.price_msat,
                    // Not kept; the price is the best statement of what was received.
                    received_msat: t.price_msat,
                    overpaid_msat: t.overpay_msat,
                    fees_msat: t.fees_msat,
                    inputs: t.spent.clone(),
                    change: t.change.clone(),
                    collection_pending: false,
                    short_id: card.clone(),
                };
                (sale, t.clone())
            })
            .collect()
    }

    /// Drop a tombstone once its sale is in the ledger.
    pub fn clear_tombstone(&mut self, txid: &str) -> Result<()> {
        self.store.state.tombstones.retain(|t| t.txid != txid);
        self.store.save()
    }

    // ---------------------------------------------------------------- balance

    /// Reconcile and report: finish interrupted work, then ask the federation about every live
    /// note and mark the spent ones.
    pub async fn balance(&mut self) -> Result<BalanceReport> {
        self.recover_overwrites()?;
        let recovered = self.resolve_pending(RECOVER_TIMEOUT).await?;
        let live: Vec<(usize, Nonce)> = self
            .store
            .state
            .notes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.status == NoteStatus::Live)
            .map(|(i, n)| Ok((i, nonce_from_hex(&n.nonce)?)))
            .collect::<Result<_>>()?;
        for (i, nonce) in live {
            if self.is_spent(nonce).await? {
                self.store.state.notes[i].status = NoteStatus::Spent;
            }
        }
        self.store.save()?;
        let st = &self.store.state;
        let mut notes = vec![];
        for s in &st.slots {
            for n in st.notes_on_current_key(s.slot) {
                let held = n.status == NoteStatus::Live && st.is_held(n.slot, crate::state::HeldKind::Spent);
                notes.push(NoteView { slot: n.slot, amount_msat: n.amount_msat, status: n.status, held });
            }
        }
        Ok(BalanceReport {
            notes,
            live_msat: st.live_total_msat(),
            stranded_msat: st.stranded_total_msat(),
            stranded_until_ms: st.stranded_until_ms(),
            free_slots: st.free_slots(),
            overwritable_slots: st.overwritable_slots(),
            recovered,
            unresolved: self.unresolved(),
        })
    }

    // ---------------------------------------------------------------- spending

    /// Build, persist, sign and submit a spend of `inputs` (live note records) into `outs`.
    /// Returns the txid once the federation accepted it. Any error before the transaction is
    /// submitted removes the records of its outputs again.
    ///
    /// On ops (`self.ops`) the same order runs through the registry: the intent (the unsigned
    /// transaction) is recorded there after step 1, and the signed bytes before anything is sent.
    /// If ops does not take either, the charge is abandoned here and never sent.
    async fn sign_and_submit(&mut self, inputs: &[NoteRec], outs: Vec<PlannedOut>, template: PendingTx) -> Result<TransactionId> {
        // Writes this till still owes ops for the card (an earlier outcome, a renewed slot) go
        // first: the intent below is refused while ops thinks the card is mid-charge.
        if self.ops.is_some() {
            self.ops_push().await.context("delivering earlier writes for this card to ops")?;
        }
        let mint_id = self.wallet.mint_id;
        let mut tx_inputs = vec![];
        let mut keys = vec![];
        for n in inputs {
            let nonce = nonce_from_hex(&n.nonce)?;
            let signature = dec(n.signature.as_deref().context("live note without signature")?)?;
            tx_inputs.push(MintInput::new_v0(Amount::from_msats(n.amount_msat), Note { nonce, signature }).into_dyn(mint_id));
            keys.push((n.slot, XOnlyPublicKey::from(nonce.0)));
        }
        let mut tx = Transaction {
            inputs: tx_inputs,
            outputs: outs
                .iter()
                .map(|o| MintOutput::new_v0(Amount::from_msats(o.amount), BlindNonce(o.blinded)).into_dyn(mint_id))
                .collect(),
            nonce: rand::thread_rng().r#gen(),
            signatures: TransactionSignature::NaiveMultisig(vec![]),
        };
        let txid = tx.tx_hash();
        let group = txid.to_string();
        // 0. A 3.0 card signs only a slot that holds its note, and only within its day's limit:
        //    write any note it lacks (SET_NOTE) and check the limit before anything is recorded.
        if self.card_sign_once() {
            self.prepare_inputs_v3(inputs, txid.to_byte_array())?;
        }

        // 1. Persist every secret needed to collect the outputs, and where each one will be,
        //    before anything is signed.
        for (i, o) in outs.iter().enumerate() {
            match &o.owner {
                Owner::Card { slot, nonce, key } => self.store.state.notes.push(NoteRec {
                    slot: *slot,
                    amount_msat: o.amount,
                    nonce: nonce_hex(*nonce),
                    signature: None,
                    status: NoteStatus::Pending,
                    blinding_key: Some(enc(key)),
                    txid: Some(group.clone()),
                    out_idx: Some(i as u64),
                }),
                Owner::Host { kp, key, kind } => self.store.state.host_pending.push(HostPending {
                    amount_msat: o.amount,
                    secret_key: hex::encode(kp.secret_bytes()),
                    blinding_key: enc(key),
                    kind: *kind,
                    group: group.clone(),
                    out_idx: Some(i as u64),
                }),
            }
        }
        let input_nonces: Vec<String> = inputs.iter().map(|n| n.nonce.clone()).collect();
        let client_ref = template.client_ref.clone();
        self.store.state.pending_txs.push(PendingTx {
            txid: group.clone(),
            tx_hex: None,
            submitted: false,
            accepted: false,
            inputs: input_nonces.clone(),
            ..template
        });
        self.store.save()?;

        // 1b. On ops: the intent, before the card signs. Refused or unreachable: nothing was
        //     signed, so the records go (and ops is told, in case it took the intent after all).
        if self.ops.is_some() {
            let change: Vec<serde_json::Value> = outs
                .iter()
                .enumerate()
                .filter_map(|(i, o)| match &o.owner {
                    Owner::Card { slot, nonce, key } => Some(serde_json::json!({
                        "slot": slot, "amount_msat": o.amount, "nonce": nonce_hex(*nonce),
                        "blinding_key": enc(key), "out_idx": i,
                    })),
                    Owner::Host { .. } => None,
                })
                .collect();
            let unsigned = tx.consensus_encode_to_hex();
            if let Err(e) = self.ops_intent(&group, client_ref.as_deref(), &unsigned, &input_nonces, change).await {
                self.abandon_group(&group);
                self.store.save()?;
                return Err(e);
            }
        }

        // 2. Sign on the card. Failure here means nothing was sent: forget the records.
        let signed: Result<()> = (|| {
            // SIGN refuses with 6A94 unless this selection has verified a PIN, and the applet
            // drops that on every SELECT (`select()` calls `pin.reset()`). The till re-selects
            // between sales and outlives the one verify done when the wallet opened, so the
            // session is opened here, in the selection that is about to sign. One verify covers
            // every slot's signature below.
            self.verify_pin_session()?;
            let digest = txid.to_byte_array();
            let msg = Message::from_digest(digest);
            let mut sigs = vec![];
            for (slot, key) in &keys {
                let sig = on_card(|| self.card.sign(*slot, &digest)).with_context(|| format!("card signature, slot {slot}"))?;
                SECP256K1.verify_schnorr(&sig, &msg, key).context("card signature does not verify on the host")?;
                sigs.push(sig);
            }
            tx.signatures = TransactionSignature::NaiveMultisig(sigs);
            Ok(())
        })();
        if let Err(e) = signed {
            self.abandon_group(&group);
            self.store.save()?;
            return Err(e);
        }
        // 3. Record the signed bytes, then that we are about to send, then send.
        let signed_hex = tx.consensus_encode_to_hex();
        if let Some(p) = self.store.state.pending_txs.iter_mut().find(|p| p.txid == group) {
            p.tx_hex = Some(signed_hex.clone());
        }
        self.store.save()?;
        if self.ops.is_some() {
            // 3b. On ops the signed bytes go there first; only then may this till send. Once ops
            //     has them any till may resubmit them, so the charge counts as submitted from here
            //     on (before the crash point below), and recovery asks the federation, never
            //     abandons it.
            let posted = match self.crash_point {
                Some(CrashPoint::SubmittedUnreachable) => Err(OpsError::Unreachable("simulated: no answer".into()).into()),
                Some(CrashPoint::SubmittedAnswerLost) => match self.ops_submitted(&group, &signed_hex).await {
                    Ok(()) => Err(OpsError::Unreachable("simulated: answer lost".into()).into()),
                    e => e,
                },
                _ => self.ops_submitted(&group, &signed_hex).await,
            };
            if let Err(e) = posted {
                if crate::ops::unanswered(&e) {
                    // Ops may have the bytes. Not sent, not abandoned: the next job with this card
                    // posts them again and sends only on a yes (`resolve_tx`).
                    if let Some(p) = self.store.state.pending_txs.iter_mut().find(|p| p.txid == group) {
                        p.ops_unconfirmed = true;
                    }
                    self.store.save()?;
                    return Err(SpendFault::Undecided(format!(
                        "ops did not answer for the signed charge {group}; it was not sent, and the next job with this card settles it: {e:#}"
                    ))
                    .into());
                }
                self.abandon_group(&group);
                self.store.save()?;
                return Err(e.context("the charge was not sent"));
            }
            if let Some(p) = self.store.state.pending_txs.iter_mut().find(|p| p.txid == group) {
                p.submitted = true;
            }
            self.store.save()?;
        }
        if self.crash_point == Some(CrashPoint::BeforeSubmit) {
            bail!("simulated crash before submit");
        }
        if let Some(p) = self.store.state.pending_txs.iter_mut().find(|p| p.txid == group) {
            p.submitted = true;
        }
        self.store.save()?;
        match self.submit(tx).await {
            Ok(Ok(accepted)) => {
                ensure!(accepted == txid, "federation returned a different txid");
                // `accepted` is set only once the federation's own confirmation arrives; until
                // then recovery keeps resubmitting the stored bytes.
                if tokio::time::timeout(Duration::from_secs(30), self.wallet.client.api().await_transaction(txid)).await.is_ok() {
                    if let Some(p) = self.store.state.pending_txs.iter_mut().find(|p| p.txid == group) {
                        p.accepted = true;
                    }
                    self.store.save()?;
                }
                if self.crash_point == Some(CrashPoint::AfterSubmit) {
                    bail!("simulated crash after submit");
                }
                self.confirm_inputs_spent(txid, &input_nonces, Duration::from_secs(30)).await.ok();
                Ok(txid)
            }
            Ok(Err(e)) => {
                // Definitely rejected: the outputs never existed.
                self.abandon_group(&group);
                self.store.save()?;
                Err(SpendFault::Declined(format!("federation rejected the spend: {e}")).into())
            }
            // Undecided: the records stay, recovery resubmits the stored bytes.
            Err(e) => {
                Err(SpendFault::Undecided(format!("transaction {group} is undecided and will be retried by the next command: {e:#}"))
                    .into())
            }
        }
    }

    fn build_outs(&self, amounts: &[u64], owner: impl FnMut(usize) -> Result<OwnerKind>) -> Result<Vec<PlannedOut>> {
        let mut owner = owner;
        amounts
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let key = BlindingKey::random();
                Ok(match owner(i)? {
                    OwnerKind::Card(slot) => {
                        let pk = xonly(self.store.state.slot_pubkey(slot).context("slot without cached key")?)?;
                        let nonce = nonce_for(pk);
                        PlannedOut { amount: *a, blinded: blind(nonce, key), owner: Owner::Card { slot, nonce, key } }
                    }
                    OwnerKind::Host(kind) => {
                        let kp = Keypair::new(SECP256K1, &mut rand::thread_rng());
                        PlannedOut { amount: *a, blinded: blind(Nonce(kp.public_key()), key), owner: Owner::Host { kp, key, kind } }
                    }
                })
            })
            .collect()
    }

    /// Recover, and refuse to go on while any earlier transaction is unresolved.
    async fn start_clean(&mut self) -> Result<Recovered> {
        self.recover_overwrites()?;
        let rec = self.resolve_pending(RECOVER_TIMEOUT).await?;
        if let Some(t) = self.unresolved().first() {
            return Err(SpendFault::Unresolved(format!(
                "transaction {t} from an earlier command is not resolved yet ({}); run `card balance` until it is",
                rec.errors.first().map(String::as_str).unwrap_or("waiting for the federation")
            ))
            .into());
        }
        Ok(rec)
    }

    /// Pay `price_msat` to the till from the card. The till notes are reissued into the wallet;
    /// the change returns to free card slots. Once the federation has accepted the transaction
    /// this never returns an error: the report says what is still pending.
    pub async fn spend(&mut self, price_msat: u64) -> Result<SpendReport> {
        self.spend_ref(price_msat, None).await
    }

    /// [`spend`](Self::spend) for the till service: `client_ref` is stored on the `PendingTx`, in
    /// the save that creates it, before anything is signed or submitted.
    pub async fn spend_ref(&mut self, price_msat: u64, client_ref: Option<&str>) -> Result<SpendReport> {
        self.start_clean().await?;
        let fee = self.fee_fn();
        let min_denom = self.wallet.min_denom();

        // Room for change is the free keyed slots plus, on ops, the never-keyed slots this tap can
        // key (split keying): the plan is made over both, and only the slots the change needs (and
        // `KEY_AHEAD`) are keyed below. Unkeyed slots left over still count as room for the
        // planner's `ROOM_AFTER_CHARGE`, since the tap of a later load keys them the same way.
        let attempt = |me: &Self| -> Result<(Vec<NoteRec>, Plan)> {
            let live: Vec<NoteRec> = me.store.state.live_notes().into_iter().cloned().collect();
            let amounts: Vec<u64> = live.iter().map(|n| n.amount_msat).collect();
            let room = me.store.state.free_slots().len() + me.keyable_slots();
            let plan = plan::solve(&amounts, price_msat, room, &fee, min_denom)?;
            Ok((live, plan))
        };
        // A card can hold several slots whose keys are spent but not yet renewed, and renewing
        // them is what makes room for change. Renew until it stops returning slots, then plan once
        // more: a sale refused for want of a slot is a customer turned away at the counter, so it
        // is worth the extra round trips rather than trusting a single pass.
        let mut renewed = 0usize;
        let (live, plan) = loop {
            match attempt(self) {
                Ok(r) => break r,
                Err(e) if matches!(e.downcast_ref::<plan::PlanError>(), Some(plan::PlanError::NoExactPlan { .. })) => {
                    // Re-keying a slot needs this selection's PIN session, as signing does, and
                    // the till opens a fresh selection per job: without it every renewal here
                    // answers 6A94 and the sale is refused for want of a slot.
                    self.verify_pin_session()?;
                    let freed = self.renew_spent_slots().await?;
                    if freed.is_empty() || renewed >= RENEW_ROUNDS {
                        return Err(e);
                    }
                    renewed += 1;
                }
                Err(e) => return Err(e),
            }
        };
        let inputs: Vec<NoteRec> = plan.inputs.iter().map(|&i| live[i].clone()).collect();
        let amounts: Vec<u64> = plan.till.iter().chain(&plan.change).copied().collect();
        self.require_tiers(&amounts)?;
        // Before the intent: it names the change slots' keys, and ops refuses unkeyed ones.
        self.ops_make_room(plan.change.len()).await?;

        let mut free = self.store.state.free_slots().into_iter();
        let n_till = plan.till.len();
        let outs = self.build_outs(&amounts, |i| {
            if i < n_till {
                Ok(OwnerKind::Host(HostKind::Till))
            } else {
                Ok(OwnerKind::Card(free.next().context("planner used more slots than are free")?))
            }
        })?;
        let change: Vec<(u8, u64)> = outs
            .iter()
            .filter_map(|o| match &o.owner {
                Owner::Card { slot, .. } => Some((*slot, o.amount)),
                Owner::Host { .. } => None,
            })
            .collect();

        let fees_msat = inputs.iter().map(|n| fee(n.amount_msat)).sum::<u64>() + plan::fee_sum(&amounts, &fee);
        let template = PendingTx {
            client_ref: client_ref.map(str::to_owned),
            price_msat,
            ts_unix_ms: now_ms(),
            overpay_msat: plan.overpay,
            fees_msat,
            spent: inputs.iter().map(|n| (n.slot, n.amount_msat)).collect(),
            change: change.clone(),
            ..Default::default()
        };
        let txid = self.sign_and_submit(&inputs, outs, template).await?;

        // Accepted. From here on nothing is an error.
        let mut warnings = vec![];
        let rec = match self.resolve_pending(FETCH_TIMEOUT).await {
            Ok(r) => r,
            Err(e) => {
                warnings.push(format!("collecting outputs: {e:#}"));
                Recovered::default()
            }
        };
        warnings.extend(rec.errors.iter().cloned());
        // The change is live: on a 3.0 card it is written in this tap (SET_NOTE).
        self.sync_card_notes();
        let renewed = match self.renew_spent_slots().await {
            Ok(r) => r,
            Err(e) => {
                warnings.push(format!("renewing spent slots: {e:#}"));
                vec![]
            }
        };
        Ok(SpendReport {
            txid,
            inputs: inputs.iter().map(|n| (n.slot, n.amount_msat)).collect(),
            price_msat,
            till_msat: plan.till.iter().sum(),
            overpay_msat: plan.overpay,
            change,
            received: Amount::from_msats(rec.till_received),
            fees_msat,
            renewed_slots: renewed,
            dust: plan.dust,
            collection_pending: !self.unresolved().is_empty(),
            warnings,
        })
    }

    // ---------------------------------------------------------------- cashout

    /// Move the note in `slot` into the wallet, then hand out its value as ecash.
    ///
    /// The card's key cannot be exported, so the note is spent with the card's signature into
    /// fresh host-owned notes; those are reissued into the wallet like any till payment, and the
    /// ecash string comes from the stock `send_oob_notes`. Funds are therefore in the wallet at
    /// every point after the federation accepts the spend, and no secret that only exists in a
    /// string on screen is ever the sole copy.
    pub async fn cashout(&mut self, slot: u8) -> Result<CashoutReport> {
        self.start_clean().await?;
        let live: Vec<NoteRec> = self.store.state.live_notes().into_iter().filter(|n| n.slot == slot).cloned().collect();
        let note = match live.as_slice() {
            [n] => n.clone(),
            [] => bail!("slot {slot} holds no live note"),
            _ => bail!("slot {slot} holds several live notes; state file is inconsistent"),
        };
        ensure!(!self.is_spent(nonce_from_hex(&note.nonce)?).await?, "the federation reports this note already spent; run `card balance`");
        let fee = self.fee_fn();
        let (amounts, dust) = plan::cashout(note.amount_msat, &fee, self.wallet.min_denom())
            .with_context(|| format!("a {} msat note cannot pay its own fee", note.amount_msat))?;
        self.require_tiers(&amounts)?;
        let outs = self.build_outs(&amounts, |_| Ok(OwnerKind::Host(HostKind::Till)))?;
        let bal0 = self.wallet.balance_msat().await?;
        let txid = self.sign_and_submit(std::slice::from_ref(&note), outs, PendingTx::default()).await?;

        let mut warnings = vec![];
        let rec = match self.resolve_pending(FETCH_TIMEOUT).await {
            Ok(r) => r,
            Err(e) => {
                warnings.push(format!("collecting outputs: {e:#}"));
                Recovered::default()
            }
        };
        warnings.extend(rec.errors.iter().cloned());
        if let Err(e) = self.renew_spent_slots().await {
            warnings.push(format!("renewing spent slots: {e:#}"));
        }
        let mut report = CashoutReport {
            txid,
            slot,
            note_msat: note.amount_msat,
            gained_msat: 0,
            ecash: None,
            exported_msat: 0,
            dust,
            collection_pending: !self.unresolved().is_empty(),
            warnings,
        };
        if report.collection_pending {
            report.warnings.push(
                "the note is spent; its value reaches the wallet when `card balance` finishes the collection, then use `ecash export`"
                    .into(),
            );
            return Ok(report);
        }
        let gained = self.wallet.balance_msat().await?.saturating_sub(bal0);
        report.gained_msat = gained;
        match self.export_after_cashout(gained).await {
            Ok(Some((text, amount))) => {
                // The log comes before the screen.
                if let Err(e) = self.store.append_cashout_log(&text) {
                    report.warnings.push(format!("could not write cashouts.log: {e:#}"));
                }
                report.ecash = Some(text);
                report.exported_msat = amount;
            }
            Ok(None) => report.warnings.push("the value in the wallet is too small to export; it stays in the wallet".into()),
            Err(e) => report.warnings.push(format!("the value is in the wallet but exporting it failed: {e:#}; use `ecash export`")),
        }
        Ok(report)
    }

    async fn export_after_cashout(&self, gained: u64) -> Result<Option<(String, u64)>> {
        let amount = self.wallet.max_exportable(gained).await?;
        if amount == 0 {
            return Ok(None);
        }
        let (notes, face) = self.wallet.export(amount, false, true).await?;
        Ok(Some((notes.to_string(), face.msats)))
    }
}

// -------------------------------------------------------------------- applet 3.0

/// Write live notes to a 3.0 card that lacks them (`SET_NOTE`): each `(slot, x-only key,
/// msat, signature hex)` whose slot is KEYED, or CHANGE owed that amount, under that key. A slot
/// holding anything else, or another key, is left alone. Answers how many were written; a card
/// error stops it and is returned.
pub fn write_notes(card: &mut dyn Card, notes: &[(u8, XOnlyPublicKey, u64, String)]) -> Result<usize> {
    if notes.is_empty() {
        return Ok(0);
    }
    let st = on_card(|| card.get_state())?;
    let mut n = 0;
    for (slot, key, amount, sig) in notes {
        let Some(state) = st.states.get(*slot as usize) else { continue };
        if !matches!(state, SlotState::Keyed | SlotState::Change) {
            continue;
        }
        let info = on_card(|| card.get_slot(*slot))?;
        if info.xonly != Some(*key) || (info.state == SlotState::Change && info.amount_msat != *amount) {
            tracing::warn!("slot {slot}: the card holds another key or another change than the note; not written");
            continue;
        }
        let sig = sig48_of(sig)?;
        on_card(|| card.set_note(*slot, *amount, &sig)).with_context(|| format!("SET_NOTE on slot {slot}"))?;
        n += 1;
    }
    Ok(n)
}

impl CardWallet {
    /// Whether the card in the field is applet 3.0 (each slot signs once; notes on the card).
    /// A card that cannot say is taken for 2.x.
    pub fn card_sign_once(&mut self) -> bool {
        match on_card(|| self.card.applet_version()) {
            Ok(v) => v.sign_once(),
            Err(e) => {
                tracing::warn!("the card's version: {e:#}");
                false
            }
        }
    }

    /// Before SIGN on a 3.0 card: every input slot holds its note (a KEYED or CHANGE slot gets it
    /// with SET_NOTE), none has signed another transaction, and the day's limit takes them all.
    fn prepare_inputs_v3(&mut self, inputs: &[NoteRec], txid: [u8; 32]) -> Result<()> {
        let mut counted = vec![];
        for n in inputs {
            let key = XOnlyPublicKey::from(nonce_from_hex(&n.nonce)?.0);
            let info = on_card(|| self.card.get_slot(n.slot)).with_context(|| format!("GET_SLOT {}", n.slot))?;
            if info.xonly != Some(key) {
                return Err(SpendFault::WrongCard(format!("slot {} holds another key than its note's", n.slot)).into());
            }
            match info.state {
                SlotState::Note => {
                    ensure!(
                        info.amount_msat == n.amount_msat,
                        "slot {} holds a note of {} msat, not {}",
                        n.slot,
                        info.amount_msat,
                        n.amount_msat
                    );
                    counted.push(n.amount_msat);
                }
                SlotState::Keyed | SlotState::Change => {
                    let sig = sig48_of(n.signature.as_deref().context("live note without signature")?)?;
                    on_card(|| self.card.set_note(n.slot, n.amount_msat, &sig)).with_context(|| format!("SET_NOTE on slot {}", n.slot))?;
                    counted.push(n.amount_msat);
                }
                SlotState::Spent if info.spent_hash == txid => {}
                SlotState::Spent => bail!(
                    "slot {} has already signed another transaction (an offline charge not yet settled?); its note cannot pay this one",
                    n.slot
                ),
                SlotState::Empty => return Err(SpendFault::WrongCard(format!("slot {} is empty on the card", n.slot)).into()),
            }
        }
        let st = on_card(|| self.card.get_state())?;
        if !st.limit_allows(&counted) {
            return Err(anyhow::Error::new(StatusWord(fedimint_card_host_card::v3::SW_OVER_LIMIT))
                .context("the card's daily limit does not take this charge; nothing was signed"));
        }
        Ok(())
    }

    /// On a 3.0 card, write every live note of this state that the card lacks (a load or change
    /// finished at another tap, or by a till that could not reach the card). Never fails the job.
    pub fn sync_card_notes(&mut self) -> usize {
        if !self.card_sign_once() {
            return 0;
        }
        let st = &self.store.state;
        let notes: Vec<(u8, XOnlyPublicKey, u64, String)> = st
            .slots
            .iter()
            .flat_map(|s| st.notes_on_current_key(s.slot))
            .filter(|n| n.status == NoteStatus::Live)
            .filter_map(|n| {
                let key = XOnlyPublicKey::from(nonce_from_hex(&n.nonce).ok()?.0);
                Some((n.slot, key, n.amount_msat, n.signature.clone()?))
            })
            .collect();
        match write_notes(self.card.as_mut(), &notes) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("writing notes to the card: {e:#}");
                0
            }
        }
    }

    /// Show a 3.0 card the till's newest block header when the card's clock is older. Never
    /// fails the job.
    pub fn sync_card_clock(&mut self) {
        let Some(h) = self.headers.as_ref().and_then(|h| h.latest()) else { return };
        if !self.card_sign_once() {
            return;
        }
        let r: Result<()> = (|| {
            let st = on_card(|| self.card.get_state())?;
            if st.now < fedimint_card_host_card::v3::header_time(&h) {
                on_card(|| self.card.set_header(&h))?;
            }
            Ok(())
        })();
        if let Err(e) = r {
            tracing::warn!("the card's clock: {e:#}");
        }
    }

    /// What a tap does first on a 3.0 card: its clock, then the notes it lacks. Answers how many
    /// notes it wrote.
    pub fn card_tick(&mut self) -> usize {
        self.sync_card_clock();
        self.sync_card_notes()
    }
}

enum OwnerKind {
    Card(u8),
    Host(HostKind),
}

enum Owner {
    Card { slot: u8, nonce: Nonce, key: BlindingKey },
    Host { kp: Keypair, key: BlindingKey, kind: HostKind },
}

struct PlannedOut {
    amount: u64,
    blinded: tbs::BlindedMessage,
    owner: Owner,
}
