//! A POS phone's store (docs/POS-OFFLINE.md § The phone's store): in one directory of the app's
//! protected storage, the offline queue (`offline_queue.json`, the till's format), the phone's id
//! (`phone-id`, 16 random bytes, hex, made once) and a lock (`store.lock`) so two processes never
//! write the queue at once.
//!
//! The store must not be restored from a backup (an old queue would take pool entries again and
//! strand payments): the apps keep it out of the platform's backups.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow, bail};
use fedimint_card_host_card::Card;
use serde::Deserialize;
use serde_json::json;

use crate::charge::{Env, Pin, Taken, TillOuts, charge};
use crate::files::{create_private_dir, private_options};
use crate::kit::Kit;
use crate::queue::{ChargeState, OfflineCharge, OfflineError, Queue};

pub const PHONE_ID_FILE: &str = "phone-id";
pub const LOCK_FILE: &str = "store.lock";
/// Most charges one sync sends: the till takes at most this many in one request.
pub const MAX_SYNC: usize = 50;
/// How long a charge the till has stays in the queue, so a repeat of its `client_ref` is still
/// that charge (30 days).
pub const KEEP_SYNCED_MS: u64 = 30 * 86_400_000;

pub struct Store {
    dir: PathBuf,
    queue: Mutex<Queue>,
    /// Held for the whole of a charge: two charges at once (an app's retry while the first still
    /// waits on its card) would plan from the same free pool entries.
    charging: Mutex<()>,
    phone_id: String,
    /// Held while the store is open: `store.lock`, locked.
    _lock: File,
}

/// What the store holds, for the app's "N payments waiting to sync".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhoneStats {
    /// Taken offline, signed, not yet on the till.
    pub waiting: usize,
    pub waiting_msat: u64,
    /// Left signing by a card taken away mid-tap: that card's next tap finishes them.
    pub signing: usize,
    /// Refused by the till at sync (`dismiss` once the app has shown them).
    pub refused: usize,
    /// On the till, kept for repeats of their `client_ref`.
    pub synced: usize,
}

/// A charge the till refused, for the app to list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub txid: String,
    pub code: String,
    pub price_msat: u64,
    pub ts_unix_ms: u64,
}

/// What [`Store::drain`] answers: the body of `POST /v1/offline/charges`, the charges in it, and
/// the charges the till refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drain {
    pub body: String,
    pub txids: Vec<String>,
    pub refused: Vec<Refused>,
}

/// What [`Store::mark_synced`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Now on the till (`queued`, or `already` there).
    pub synced: usize,
    /// Refused for good: kept, with the code, until dismissed.
    pub refused: usize,
    /// Refused for now (`retry`): still waiting.
    pub retry: usize,
}

#[derive(Deserialize)]
struct Answer {
    ok: bool,
    #[serde(default)]
    results: Vec<Result1>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct Result1 {
    txid: String,
    status: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    retry: bool,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl Store {
    /// Open (or make) the store in `dir`. Fails if another process has it open.
    pub fn open(dir: &Path) -> Result<Self> {
        create_private_dir(dir)?;
        let lock = private_options().open(dir.join(LOCK_FILE)).with_context(|| format!("opening {}", dir.join(LOCK_FILE).display()))?;
        // flock(2) through `fs4`: std's `File::try_lock` is unsupported on Android (it answers
        // "try_lock() not supported"), which left every Android phone without an offline store.
        if !fs4::fs_std::FileExt::try_lock_exclusive(&lock).context("locking the offline store")? {
            bail!("the offline store in {} is open elsewhere", dir.display());
        }
        let phone_id = phone_id(dir)?;
        let queue = Queue::open(dir)?;
        Ok(Self { dir: dir.to_owned(), queue: Mutex::new(queue), charging: Mutex::new(()), phone_id, _lock: lock })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The id the till issues this phone's kits to.
    pub fn phone_id(&self) -> &str {
        &self.phone_id
    }

    fn q(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take `price` from `card` offline with the kit `kit_json` (as the till answered it), the
    /// holder's `pin`, for the sale `client_ref`. The kit is checked before the card is asked
    /// anything; its expiry against the latest of the phone's clock, the kit's issue and the
    /// latest time this store has seen, and again against the card's clock. A repeat of a sale the
    /// till refused at sync is `conflict`: that sale was not paid, and a new one needs a new
    /// `client_ref`. Charges on one store run one at a time: a second waits for the first.
    pub fn charge(&self, card: &mut dyn Card, kit_json: &str, price: u64, client_ref: &str, pin: &[u8]) -> Result<Taken> {
        // A panic never leaves the queue half-changed (`Queue::commit`), so a poisoned lock is safe.
        let _one = self.charging.lock().unwrap_or_else(|e| e.into_inner());
        let t = self.charge_inner(card, kit_json, price, client_ref, pin)?;
        if t.replayed && t.charge.state == ChargeState::Refused {
            bail!(OfflineError::Conflict(format!(
                "the till refused sale {client_ref:?} at sync ({}); start it again with a new client_ref",
                t.charge.detail.as_deref().unwrap_or("refused")
            )));
        }
        Ok(t)
    }

    fn charge_inner(&self, card: &mut dyn Card, kit_json: &str, price: u64, client_ref: &str, pin: &[u8]) -> Result<Taken> {
        let kit = Kit::parse(kit_json)?;
        let keys = kit.decode(&self.phone_id)?;
        let now = now_ms().max(kit.issued_at_ms).max(self.q().file.high_water_ms);
        let env = Env {
            mint_id: kit.mint_instance_id,
            tbs_pks: &keys.tbs_pks,
            fee: &keys.fee,
            surplus_allowed: kit.surplus_allowed,
            decoders: &keys.decoders,
            cert_key: Some(&kit.cert_key),
            cfg: keys.cfg,
            header: keys.header,
            pin: Pin::Verify(pin.to_vec()),
            till_outs: TillOuts::Pool(&keys.pool),
            keyed_ok: None,
            now_ms: now,
            expires_at_ms: Some(kit.expires_at_ms),
        };
        charge(card, &self.queue, &env, price, Some(client_ref))
    }

    pub fn stats(&self) -> PhoneStats {
        let q = self.q();
        let mut s = PhoneStats::default();
        for c in &q.file.charges {
            match c.state {
                ChargeState::Queued if c.synced => s.synced += 1,
                ChargeState::Queued => {
                    s.waiting += 1;
                    s.waiting_msat += c.till_msat;
                }
                ChargeState::Signing => s.signing += 1,
                ChargeState::Refused => s.refused += 1,
                _ => {}
            }
        }
        s
    }

    /// The charges waiting for the till, oldest first, at most `max` (and [`MAX_SYNC`]), as the
    /// body of `POST /v1/offline/charges`; and the charges the till refused, to list.
    pub fn drain(&self, max: usize) -> Drain {
        let q = self.q();
        let waiting: Vec<&OfflineCharge> =
            q.file.charges.iter().filter(|c| c.state == ChargeState::Queued && !c.synced).take(max.min(MAX_SYNC)).collect();
        let refused = q
            .file
            .charges
            .iter()
            .filter(|c| c.state == ChargeState::Refused)
            .map(|c| Refused {
                txid: c.txid.clone(),
                code: c.detail.clone().unwrap_or_default(),
                price_msat: c.price_msat,
                ts_unix_ms: c.ts_unix_ms,
            })
            .collect();
        Drain {
            body: json!({"phone_id": self.phone_id, "charges": waiting}).to_string(),
            txids: waiting.iter().map(|c| c.txid.clone()).collect(),
            refused,
        }
    }

    /// The `client_ref`s of the charges a card taken away mid-tap left `signing`. Only that card's
    /// next tap here finishes one, so the app retries such a sale with this store's `charge` only,
    /// never on the till's `POST /v1/charge`: the card may have signed part of a transaction, and
    /// an online charge would take the sale a second time (docs/POS-OFFLINE.md).
    pub fn signing_refs(&self) -> Vec<String> {
        self.q().file.charges.iter().filter(|c| c.state == ChargeState::Signing).filter_map(|c| c.client_ref.clone()).collect()
    }

    /// Take the till's answer to a drain's body: `queued` and `already` are the till's now,
    /// `refused` is kept with its code (unless `retry`, which waits for the next sync). Charges the
    /// till has had, and abandoned ones, leave the queue after [`KEEP_SYNCED_MS`]; their pool
    /// entries stay used.
    pub fn mark_synced(&self, answer_json: &str) -> Result<SyncReport> {
        self.mark_synced_at(answer_json, now_ms())
    }

    /// [`Store::mark_synced`] with the phone's clock at `now`. The clock only decides what is old
    /// enough to leave the queue: it is not kept as the store's latest time, which only a charge
    /// moves (and never past its kit's expiry), so a clock set ahead once cannot expire every kit
    /// for good.
    #[doc(hidden)]
    pub fn mark_synced_at(&self, answer_json: &str, now: u64) -> Result<SyncReport> {
        let a: Answer = serde_json::from_str(answer_json).map_err(|e| anyhow!("the till's answer is not one: {e}"))?;
        if !a.ok {
            bail!("the till refused the sync: {}", a.error.as_deref().unwrap_or("no reason given"));
        }
        let mut report = SyncReport::default();
        self.q().commit(|f| {
            for r in &a.results {
                let Some(c) = f.charges.iter_mut().find(|c| c.txid == r.txid && c.state == ChargeState::Queued) else { continue };
                match (r.status.as_str(), r.retry) {
                    ("queued" | "already", _) => {
                        c.synced = true;
                        report.synced += 1;
                    }
                    ("refused", false) => {
                        c.state = ChargeState::Refused;
                        c.detail = Some(r.code.clone().unwrap_or_else(|| "refused".into()));
                        report.refused += 1;
                    }
                    _ => report.retry += 1,
                }
            }
            let clock = now.max(f.high_water_ms);
            let old =
                |c: &OfflineCharge| (c.synced || c.state == ChargeState::Abandoned) && c.ts_unix_ms.saturating_add(KEEP_SYNCED_MS) < clock;
            let gone: Vec<String> = f.charges.iter().filter(|c| old(c)).flat_map(|c| c.pool.iter().map(|p| p.blinded.clone())).collect();
            f.charges.retain(|c| !old(c));
            f.pool_used.extend(gone);
        })?;
        Ok(report)
    }

    /// Remove a charge the till refused, once the app has shown it. Its notes stay spent here and
    /// its pool entries used. Answers whether there was one.
    pub fn dismiss(&self, txid: &str) -> Result<bool> {
        self.q().commit(|f| {
            let Some(i) = f.charges.iter().position(|c| c.txid == txid && c.state == ChargeState::Refused) else { return false };
            let c = f.charges.remove(i);
            f.pool_used.extend(c.pool.into_iter().map(|p| p.blinded));
            true
        })
    }

    /// Tests: fail the queue's write after the next `n`.
    #[doc(hidden)]
    pub fn fail_write_after(&self, n: Option<usize>) {
        self.q().fail_write_after(n);
    }

    /// The queue as it is (tests, diagnostics).
    pub fn charges(&self) -> Vec<OfflineCharge> {
        self.q().file.charges.clone()
    }
}

/// The phone's id: made once (16 random bytes, hex, 0600) and kept.
fn phone_id(dir: &Path) -> Result<String> {
    let path = dir.join(PHONE_ID_FILE);
    match std::fs::read_to_string(&path) {
        Ok(s) if s.trim().len() == 32 && s.trim().bytes().all(|b| b.is_ascii_hexdigit()) => return Ok(s.trim().to_ascii_lowercase()),
        Ok(_) => bail!("{} is not 16 bytes of hex", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    }
    let id = hex::encode(rand::random::<[u8; 16]>());
    crate::files::write_private_text(&path, &id)?;
    Ok(id)
}
