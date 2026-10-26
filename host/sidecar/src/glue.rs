//! Mint v1 glue between the card, the state file and the federation.
//!
//! The fedimint client assumes it holds spend keys. Here the keys are on the card, so spends are
//! built by hand: `Transaction { inputs, outputs, nonce, signatures }`, with the signatures
//! produced by the card over `tx_hash()`. Topups reuse the stock client (it funds the transaction
//! from the wallet) with an explicit output blinded to a card key.
//!
//! Ordering rule everywhere: write the state file first, then talk to the card or the federation.
//! A crash at any point leaves records (`pending` notes, `host_pending` outputs, `pending_overwrites`)
//! from which the next command recovers.

use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use fedimint_card_host_card::{Card, SLOTS, SW_CONDITIONS_NOT_SATISFIED, status_word};
use fedimint_client::transaction::{ClientOutput, ClientOutputBundle, TransactionBuilder};
use fedimint_core::core::{IntoDynInstance, OperationId};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::Amounts;
use fedimint_core::module::registry::ModuleRegistry;
use fedimint_core::secp256k1::hashes::Hash as _;
use fedimint_core::secp256k1::rand::{self, Rng};
use fedimint_core::secp256k1::{Keypair, Message, Parity, PublicKey, SECP256K1, SecretKey, XOnlyPublicKey};
use fedimint_core::transaction::{
    Transaction, TransactionError, TransactionSignature, TransactionSubmissionOutcome,
};
use fedimint_core::{Amount, OutPoint, TransactionId, TieredMulti};
use fedimint_mint_client::api::MintFederationApi as _;
use fedimint_mint_client::{OOBNotes, SpendableNote};
use fedimint_mint_common::{BlindNonce, MintInput, MintOutput, Nonce, Note};
use tbs::BlindingKey;

use crate::plan::{self, Plan};
use crate::shares::{self, FETCH_TIMEOUT, OutSpec, blind};
use crate::state::{HostKind, HostPending, NoteRec, NoteStatus, PendingOverwrite, StateStore};
use crate::wallet::Wallet;

/// Short wait used when recovering old records, where the answer may simply not exist.
const RECOVER_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound for waiting on a transaction decision.
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(90);

/// Run a blocking card call from inside a multi-thread tokio runtime.
pub fn on_card<R>(f: impl FnOnce() -> R) -> R {
    tokio::task::block_in_place(f)
}

fn enc<T: Encodable>(t: &T) -> String {
    t.consensus_encode_to_hex()
}

fn dec<T: Decodable>(s: &str) -> Result<T> {
    T::consensus_decode_hex(s, &ModuleRegistry::default()).map_err(|e| anyhow!("decoding {s:.20}: {e}"))
}

/// The nonce for a card key. Parity is fixed to even so the 33 bytes blinded at issue are the
/// 33 bytes the federation sees at spend.
pub fn nonce_for(key: XOnlyPublicKey) -> Nonce {
    Nonce(PublicKey::from_x_only_public_key(key, Parity::Even))
}

fn nonce_hex(n: Nonce) -> String {
    hex::encode(n.0.serialize())
}

fn nonce_from_hex(s: &str) -> Result<Nonce> {
    Ok(Nonce(PublicKey::from_str(s).context("bad nonce hex")?))
}

fn xonly(hex_str: &str) -> Result<XOnlyPublicKey> {
    XOnlyPublicKey::from_str(hex_str).context("bad pubkey hex")
}

#[derive(Debug, Clone)]
pub struct NoteView {
    pub slot: u8,
    pub amount_msat: u64,
    pub status: NoteStatus,
}

#[derive(Debug, Clone)]
pub struct BalanceReport {
    /// Every note on the card's current keys with its status (history is in the state file).
    pub notes: Vec<NoteView>,
    pub live_msat: u64,
    pub free_slots: Vec<u8>,
    pub overwritable_slots: Vec<u8>,
    /// Ecash exported by recovery of an interrupted cashout.
    pub recovered_cashouts: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SpendReport {
    pub txid: TransactionId,
    pub inputs: Vec<(u8, u64)>,
    pub price_msat: u64,
    pub till_msat: u64,
    pub overpay_msat: u64,
    pub change: Vec<(u8, u64)>,
    /// Face value of the till notes reissued into the wallet.
    pub received: Amount,
    pub fees_msat: u64,
    pub renewed_slots: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct CashoutReport {
    pub txid: TransactionId,
    pub slot: u8,
    pub note_msat: u64,
    pub ecash: String,
    pub face_msat: u64,
}

/// What interrupted operations turned out to have done.
#[derive(Debug, Clone, Default)]
pub struct Recovered {
    pub notes_live: usize,
    pub till_received: u64,
    pub cashouts: Vec<String>,
}

pub struct CardWallet {
    pub wallet: Wallet,
    pub store: StateStore,
    pub card: Box<dyn Card>,
}

impl CardWallet {
    pub fn new(wallet: Wallet, store: StateStore, card: Box<dyn Card>) -> Self {
        Self { wallet, store, card }
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

    /// GEN_KEY on every slot that is empty and cache all public keys. Keys already on the card
    /// are kept. Stops at the first slot the applet does not have (a 4-slot applet answers 6B00).
    pub fn card_setup(&mut self) -> Result<Vec<(u8, XOnlyPublicKey)>> {
        self.recover_overwrites()?;
        let mut out = vec![];
        for slot in 0..SLOTS {
            let pk = match on_card(|| self.card.get_or_gen(slot)) {
                Ok(pk) => pk,
                Err(e) if status_word(&e) == Some(0x6B00) => {
                    self.store.save()?;
                    bail!("the card has only {slot} slot(s) (status 6B00 for slot {slot}); flash the 16-slot applet");
                }
                Err(e) => return Err(e).with_context(|| format!("slot {slot}")),
            };
            let hexpk = pk.to_string();
            match self.store.state.slot_pubkey(slot) {
                Some(old) if old == hexpk => {}
                Some(old) => {
                    // The card holds a different key than we recorded. Safe only if the old key
                    // carried no money we still care about.
                    let busy = self
                        .store
                        .state
                        .notes_on_current_key(slot)
                        .any(|n| matches!(n.status, NoteStatus::Live | NoteStatus::Pending));
                    ensure!(!busy, "slot {slot} holds a different key than recorded ({old}) and has live notes; \
                                    this is not the card the state file belongs to");
                    self.store.state.set_slot_pubkey(slot, hexpk);
                }
                None => self.store.state.set_slot_pubkey(slot, hexpk),
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
        let rec = PendingOverwrite { slot, old_pubkey: old };
        self.store.state.pending_overwrites.push(rec.clone());
        self.store.save()?;
        match on_card(|| self.card.gen_key_overwrite(slot)) {
            Ok(pk) => {
                self.store.state.set_slot_pubkey(slot, pk.to_string());
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
        for slot in self.store.state.overwritable_slots() {
            let notes: Vec<NoteRec> = self.store.state.notes_on_current_key(slot).cloned().collect();
            let mut safe = true;
            for n in &notes {
                if n.status == NoteStatus::Spent && !self.is_spent(nonce_from_hex(&n.nonce)?).await? {
                    safe = false;
                }
            }
            if !safe {
                continue;
            }
            match self.overwrite_slot(slot) {
                Ok(_) => done.push(slot),
                Err(e) => tracing::warn!("could not renew slot {slot}: {e:#}"),
            }
        }
        Ok(done)
    }

    // ---------------------------------------------------------------- federation helpers

    pub async fn is_spent(&self, nonce: Nonce) -> Result<bool> {
        let api = self.wallet.client.api().with_module(self.wallet.mint_id);
        Ok(api.check_note_spent(nonce).await?)
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
    /// decomposition (one note per set bit). The wallet funds amount plus fees.
    pub async fn topup(&mut self, amounts: &[u64]) -> Result<Vec<(u8, u64)>> {
        self.recover_overwrites()?;
        let notes: Vec<u64> = amounts.iter().flat_map(|a| plan::binary(*a)).collect();
        ensure!(!notes.is_empty(), "nothing to top up");
        self.require_tiers(&notes)?;
        let mut free = self.store.state.free_slots();
        if free.len() < notes.len() {
            self.renew_spent_slots().await.ok();
            free = self.store.state.free_slots();
        }
        ensure!(
            free.len() >= notes.len(),
            "{} note(s) need {} free slot(s) but only {} are free (run `card setup`, or spend/cash out to free slots)",
            notes.len(),
            notes.len(),
            free.len()
        );
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
        // Persist before the federation hears of it.
        for o in &outs {
            self.store.state.notes.push(NoteRec {
                slot: o.slot,
                amount_msat: o.amount,
                nonce: nonce_hex(o.nonce),
                signature: None,
                status: NoteStatus::Pending,
                blinding_key: Some(enc(&o.key)),
                txid: None,
            });
        }
        self.store.save()?;

        let bundle = ClientOutputBundle::new_no_sm(
            outs.iter()
                .map(|o| ClientOutput {
                    output: MintOutput::new_v0(Amount::from_msats(o.amount), BlindNonce(o.blinded)),
                    amounts: Amounts::new_bitcoin(Amount::from_msats(o.amount)),
                })
                .collect(),
        )
        .into_dyn(self.wallet.mint_id);
        let client = &self.wallet.client;
        let op = OperationId::new_random();
        let range = match client
            .finalize_and_submit_transaction(op, "card-topup", |_| (), TransactionBuilder::new().with_outputs(bundle))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // Nothing was sent: forget the pending records.
                let nonces: Vec<String> = outs.iter().map(|o| nonce_hex(o.nonce)).collect();
                self.store.state.notes.retain(|n| !(nonces.contains(&n.nonce) && n.status == NoteStatus::Pending));
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
        self.store.save()?;

        let accepted = tokio::time::timeout(
            SUBMIT_TIMEOUT,
            client.transaction_updates(op).await.await_tx_accepted(txid),
        )
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
        // The stock client puts explicit outputs first, so ours are out_idx 0..n.
        for (i, o) in outs.iter().enumerate() {
            self.finalize_card_note(&nonce_hex(o.nonce), OutPoint { txid, out_idx: i as u64 }, FETCH_TIMEOUT).await?;
        }
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
        let blinded = blind(nonce, key);
        let sig = shares::fetch_blind_signature(
            &self.wallet,
            outpoint.txid,
            OutSpec { out_idx: outpoint.out_idx, amount, blinded },
            timeout,
        )
        .await?;
        let signature = shares::finish_note(&self.wallet, amount, nonce, key, blinded, sig)?;
        let n = &mut self.store.state.notes[idx];
        n.signature = Some(enc(&signature));
        n.status = NoteStatus::Live;
        n.blinding_key = None;
        n.txid = Some(outpoint.txid.to_string());
        self.store.save()
    }

    // ---------------------------------------------------------------- recovery

    async fn outpoint_of(&self, blinded: tbs::BlindedMessage) -> Option<OutPoint> {
        let api = self.wallet.client.api().with_module(self.wallet.mint_id);
        let ops = api.fetch_blind_nonce_outpoints(vec![BlindNonce(blinded)]).await.ok()?;
        ops.into_iter().next()
    }

    /// Complete whatever an interrupted command left: pending card notes whose transaction was
    /// accepted become live, and in-flight host outputs are collected (till notes are reissued
    /// into the wallet, cashout notes are returned as ecash strings).
    pub async fn resolve_pending(&mut self, timeout: Duration) -> Result<Recovered> {
        let mut rec = Recovered::default();
        let pending: Vec<NoteRec> =
            self.store.state.notes.iter().filter(|n| n.status == NoteStatus::Pending).cloned().collect();
        for n in pending {
            let (Some(bk), Ok(nonce)) = (n.blinding_key.as_deref(), nonce_from_hex(&n.nonce)) else { continue };
            let Ok(key) = dec::<BlindingKey>(bk) else { continue };
            let blinded = blind(nonce, key);
            let Some(op) = self.outpoint_of(blinded).await else { continue };
            if self.finalize_card_note(&n.nonce, op, timeout).await.is_ok() {
                rec.notes_live += 1;
            }
        }

        // Host outputs, group by group.
        let mut groups: Vec<String> = self.store.state.host_pending.iter().map(|h| h.group.clone()).collect();
        groups.sort();
        groups.dedup();
        for g in groups {
            let items: Vec<HostPending> =
                self.store.state.host_pending.iter().filter(|h| h.group == g).cloned().collect();
            let mut notes = TieredMulti::<SpendableNote>::default();
            let mut complete = true;
            for h in &items {
                let kp = Keypair::from_secret_key(SECP256K1, &SecretKey::from_str(&h.secret_key)?);
                let key: BlindingKey = dec(&h.blinding_key)?;
                let nonce = Nonce(kp.public_key());
                let blinded = blind(nonce, key);
                let amount = Amount::from_msats(h.amount_msat);
                let Some(op) = self.outpoint_of(blinded).await else {
                    complete = false;
                    break;
                };
                let got = async {
                    let sig = shares::fetch_blind_signature(
                        &self.wallet,
                        op.txid,
                        OutSpec { out_idx: op.out_idx, amount, blinded },
                        timeout,
                    )
                    .await?;
                    shares::finish_note(&self.wallet, amount, nonce, key, blinded, sig)
                }
                .await;
                match got {
                    Ok(signature) => notes.push(amount, SpendableNote { signature, spend_key: kp }),
                    Err(_) => {
                        complete = false;
                        break;
                    }
                }
            }
            if !complete {
                continue;
            }
            let kind = items[0].kind;
            let oob = OOBNotes::new(self.wallet.federation_id().to_prefix(), notes);
            match kind {
                HostKind::Cashout => rec.cashouts.push(oob.to_string()),
                HostKind::Till => rec.till_received += self.wallet.import_notes(oob).await?.msats,
            }
            self.store.state.host_pending.retain(|h| h.group != g);
            self.store.save()?;
        }
        Ok(rec)
    }

    // ---------------------------------------------------------------- balance

    /// Reconcile and report: finish interrupted work, then ask the federation about every live
    /// note and mark the spent ones.
    pub async fn balance(&mut self) -> Result<BalanceReport> {
        self.recover_overwrites()?;
        let rec = self.resolve_pending(RECOVER_TIMEOUT).await?;
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
                notes.push(NoteView { slot: n.slot, amount_msat: n.amount_msat, status: n.status });
            }
        }
        Ok(BalanceReport {
            notes,
            live_msat: st.live_total_msat(),
            free_slots: st.free_slots(),
            overwritable_slots: st.overwritable_slots(),
            recovered_cashouts: rec.cashouts,
        })
    }

    // ---------------------------------------------------------------- spending

    /// Build, persist, sign and submit a spend of `inputs` (live note records) into `outs`.
    /// Returns the txid once the federation accepted it.
    async fn sign_and_submit(
        &mut self,
        inputs: &[NoteRec],
        outs: Vec<PlannedOut>,
    ) -> Result<TransactionId> {
        let mint_id = self.wallet.mint_id;
        let mut tx_inputs = vec![];
        let mut keys = vec![];
        for n in inputs {
            let nonce = nonce_from_hex(&n.nonce)?;
            let signature = dec(n.signature.as_deref().context("live note without signature")?)?;
            tx_inputs.push(
                MintInput::new_v0(Amount::from_msats(n.amount_msat), Note { nonce, signature }).into_dyn(mint_id),
            );
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

        // 1. Persist every secret needed to collect the outputs, before anything is signed.
        let group = txid.to_string();
        for o in &outs {
            match &o.owner {
                Owner::Card { slot, nonce, key } => self.store.state.notes.push(NoteRec {
                    slot: *slot,
                    amount_msat: o.amount,
                    nonce: nonce_hex(*nonce),
                    signature: None,
                    status: NoteStatus::Pending,
                    blinding_key: Some(enc(key)),
                    txid: Some(group.clone()),
                }),
                Owner::Host { kp, key, kind } => self.store.state.host_pending.push(HostPending {
                    amount_msat: o.amount,
                    secret_key: hex::encode(kp.secret_bytes()),
                    blinding_key: enc(key),
                    kind: *kind,
                    group: group.clone(),
                }),
            }
        }
        self.store.save()?;

        // 2. Sign on the card.
        let digest = txid.to_byte_array();
        let msg = Message::from_digest(digest);
        let mut sigs = vec![];
        for (slot, key) in &keys {
            let sig = on_card(|| self.card.sign(*slot, &digest)).with_context(|| format!("card signature, slot {slot}"))?;
            SECP256K1.verify_schnorr(&sig, &msg, key).context("card signature does not verify on the host")?;
            sigs.push(sig);
        }
        tx.signatures = TransactionSignature::NaiveMultisig(sigs);

        // 3. Submit.
        match self.submit(tx).await? {
            Ok(accepted) => {
                ensure!(accepted == txid, "federation returned a different txid");
                for n in inputs {
                    if let Some(r) = self
                        .store
                        .state
                        .notes
                        .iter_mut()
                        .find(|r| r.nonce == n.nonce && r.status == NoteStatus::Live)
                    {
                        r.status = NoteStatus::Spent;
                    }
                }
                self.store.save()?;
                Ok(txid)
            }
            Err(e) => {
                // Definitely rejected: the outputs never existed.
                for r in self.store.state.notes.iter_mut().filter(|r| r.status == NoteStatus::Pending && r.txid.as_deref() == Some(group.as_str())) {
                    r.status = NoteStatus::Dead;
                    r.blinding_key = None;
                }
                self.store.state.host_pending.retain(|h| h.group != group);
                self.store.save()?;
                Err(anyhow!("federation rejected the spend: {e}"))
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
                        PlannedOut {
                            amount: *a,
                            blinded: blind(Nonce(kp.public_key()), key),
                            owner: Owner::Host { kp, key, kind },
                        }
                    }
                })
            })
            .collect()
    }

    /// Pay `price_msat` to the till from the card. The till notes are reissued into the wallet;
    /// the change returns to free card slots.
    pub async fn spend(&mut self, price_msat: u64) -> Result<SpendReport> {
        self.recover_overwrites()?;
        // Finish earlier interrupted work so its change counts.
        self.resolve_pending(RECOVER_TIMEOUT).await.ok();
        let fee = self.fee_fn();

        let attempt = |me: &Self| -> Result<(Vec<NoteRec>, Plan)> {
            let live: Vec<NoteRec> = me.store.state.live_notes().into_iter().cloned().collect();
            let amounts: Vec<u64> = live.iter().map(|n| n.amount_msat).collect();
            let plan = plan::solve(&amounts, price_msat, me.store.state.free_slots().len(), &fee)?;
            Ok((live, plan))
        };
        let (live, plan) = match attempt(self) {
            Ok(r) => r,
            Err(e) if matches!(e.downcast_ref::<plan::PlanError>(), Some(plan::PlanError::NoExactPlan { .. })) => {
                // More slots may be free once spent keys are renewed.
                if self.renew_spent_slots().await?.is_empty() {
                    return Err(e);
                }
                attempt(self)?
            }
            Err(e) => return Err(e),
        };
        let inputs: Vec<NoteRec> = plan.inputs.iter().map(|&i| live[i].clone()).collect();
        let amounts: Vec<u64> = plan.till.iter().chain(&plan.change).copied().collect();
        self.require_tiers(&amounts)?;

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

        let txid = self.sign_and_submit(&inputs, outs).await?;
        let rec = self.resolve_pending(FETCH_TIMEOUT).await?;
        let renewed = self.renew_spent_slots().await.unwrap_or_default();

        let fees_msat = inputs.iter().map(|n| fee(n.amount_msat)).sum::<u64>() + plan::fee_sum(&amounts, &fee);
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
        })
    }

    // ---------------------------------------------------------------- cashout

    /// Move the note in `slot` out of the card into ecash the stock client can reissue.
    /// The card note is spent with the card's signature into fresh host-owned notes (the card's
    /// own key cannot be exported, so the string carries new keys).
    pub async fn cashout(&mut self, slot: u8) -> Result<CashoutReport> {
        self.recover_overwrites()?;
        let rec = self.resolve_pending(RECOVER_TIMEOUT).await.unwrap_or_default();
        let live: Vec<NoteRec> = self
            .store
            .state
            .live_notes()
            .into_iter()
            .filter(|n| n.slot == slot)
            .cloned()
            .collect();
        let note = match live.as_slice() {
            [n] => n.clone(),
            [] => bail!("slot {slot} holds no live note{}", if rec.cashouts.is_empty() { "" } else { " (an interrupted cashout was recovered; see the ecash printed above)" }),
            _ => bail!("slot {slot} holds several live notes; state file is inconsistent"),
        };
        ensure!(!self.is_spent(nonce_from_hex(&note.nonce)?).await?, "the federation reports this note already spent; run `card balance`");
        let fee = self.fee_fn();
        let amounts = plan::cashout(note.amount_msat, &fee)
            .with_context(|| format!("a {} msat note cannot pay its own fee", note.amount_msat))?;
        self.require_tiers(&amounts)?;
        let outs = self.build_outs(&amounts, |_| Ok(OwnerKind::Host(HostKind::Cashout)))?;
        let txid = self.sign_and_submit(std::slice::from_ref(&note), outs).await?;
        let rec = self.resolve_pending(FETCH_TIMEOUT).await?;
        let ecash = rec.cashouts.into_iter().next().context("cashout notes not yet signed; run `card balance` to collect them")?;
        let face = OOBNotes::from_str(&ecash).map(|o| o.total_amount().msats).unwrap_or(0);
        self.renew_spent_slots().await.ok();
        Ok(CashoutReport { txid, slot, note_msat: note.amount_msat, ecash, face_msat: face })
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
