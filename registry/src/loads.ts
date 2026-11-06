/**
 * Holder loading (OPS-PLAN § 6, phase O4): value comes from the holder's own Fedimint wallet,
 * which needs no account here, only the card's short id.
 *
 *   GET  /cards/:short_id/slots         the free slot keys to load into, and `version`
 *   POST /cards/:short_id/load          the notes, pending, with their blinding keys, before the
 *                                       wallet submits; version-checked
 *   POST /cards/:short_id/load/outcome  the federation's signatures; each is verified before the
 *                                       notes are live. {accepted: false}: the federation refused
 *
 * Why a stranger cannot hurt the card: a note is spent only by the card, behind its PIN, so a
 * record here can steal nothing. What a bogus record could do is show money the card does not
 * have, or sit on its slots. The first is closed by the signature check (`mint.ts`): a note is
 * live only once the federation's key for its tier verifies it. The second by expiry: an open
 * load lasts `LOAD_TTL_MS`, after which its notes are gone and the slots free; by one open load
 * per card; and by a per-card budget of holder writes.
 *
 * Every note must sit under the card's current key for its slot (`02 || key`, the sidecar's
 * `nonce_for`): a note the card cannot sign for would be worthless and hold the slot. A slot is
 * free while its current key has never carried a note, the rule the tills keep (`cards.ts`).
 *
 * A load that expired or was refused leaves no note row behind, so its keys stay free rather than
 * burned: the federation never signed (refused), or ops never heard that it did (expired). A
 * wallet that did submit and comes back late can still post the outcome while the slots are
 * untouched; once another load used one of them, it cannot.
 */
import {bump, requireVersion, slotCount, versionOf} from './cards';
import {RateLimited} from './auth';
import {BadRequest, Conflict, isCount, isHex, isPositiveInt, NotFound, Unprocessable} from './http';
import {cardFederation, checkNotes, isTier, mintKeysFor} from './mint';

type Sql = SqlStorage;

/** How long a load may stay open before its slots are free again. */
export const LOAD_TTL_MS = 30 * 60 * 1000;
/** Holder writes (load and outcome) per card per window. */
export const HOLDER_MAX_WRITES = 12;
export const HOLDER_WINDOW_MS = 60 * 60 * 1000;

interface LoadNote {
  readonly slot: number;
  readonly amount_msat: number;
  readonly nonce: string;
  readonly blinding_key: string;
  readonly out_idx: number;
}

interface LoadRow {
  readonly txid: string;
  readonly short_id: string;
  readonly body: string;
  readonly status: 'open' | 'accepted' | 'rejected' | 'expired';
  readonly outcome: string | null;
  readonly expires_at: number;
}

/** The card's slots, by number, as currently keyed. */
function currentKeys(sql: Sql, shortId: string): Map<number, string> {
  return new Map(
    sql
      .exec('SELECT slot, pubkey FROM card_slots WHERE short_id = ?', shortId)
      .toArray()
      .map(r => [Number(r.slot), String(r.pubkey)] as const),
  );
}

/** Whether a note was ever recorded under `nonce` on this card, in any state. */
function keyUsed(sql: Sql, shortId: string, nonce: string): boolean {
  return sql.exec('SELECT 1 FROM notes WHERE short_id = ? AND nonce = ?', shortId, nonce).toArray().length > 0;
}

/**
 * Ends the card's open loads whose time is up: their pending notes are deleted and the load is
 * marked expired. Not a version bump: it only frees slots, and a till whose read still showed them
 * held merely kept off them.
 */
export function expireLoads(storage: DurableObjectStorage, shortId: string, now: number): void {
  const sql = storage.sql;
  storage.transactionSync(() => {
    const due = sql
      .exec("SELECT txid FROM holder_loads WHERE short_id = ? AND status = 'open' AND expires_at <= ?", shortId, now)
      .toArray();
    for (const {txid} of due) {
      sql.exec("DELETE FROM notes WHERE short_id = ? AND txid = ? AND status = 'pending'", shortId, txid);
      sql.exec("UPDATE holder_loads SET status = 'expired', resolved_at = ? WHERE txid = ?", now, txid);
    }
  });
}

/**
 * Spends one of the card's holder writes for this window, or throws RateLimited. Its own
 * transaction, so a write that is then refused (a forged signature, a stale version) still counts.
 */
export function spendBudget(storage: DurableObjectStorage, shortId: string, now: number): void {
  storage.transactionSync(() => spendIn(storage.sql, shortId, now));
}

function spendIn(sql: Sql, shortId: string, now: number): void {
  const row = sql
    .exec('SELECT window_start, writes FROM holder_budget WHERE short_id = ?', shortId)
    .toArray()[0] as {window_start: number; writes: number} | undefined;
  const reset = !row || now - Number(row.window_start) >= HOLDER_WINDOW_MS;
  if (!reset && Number(row.writes) >= HOLDER_MAX_WRITES) {
    throw new RateLimited(`too many loads for card ${shortId}; try again within the hour`);
  }
  sql.exec(
    `INSERT INTO holder_budget (short_id, window_start, writes) VALUES (?, ?, ?)
     ON CONFLICT(short_id) DO UPDATE SET window_start = excluded.window_start, writes = excluded.writes`,
    shortId,
    reset ? now : Number(row.window_start),
    reset ? 1 : Number(row.writes) + 1,
  );
}

/**
 * `GET /cards/:short_id/slots`: the slots a wallet may load into. Only slots whose current key
 * never carried a note (so none pending either). No balances and no notes: anyone with the short
 * id may ask.
 */
export function freeSlots(sql: Sql, shortId: string) {
  const fed = cardFederation(sql, shortId);
  if (!fed) {
    throw new NotFound(`no card ${shortId}`);
  }
  const free = [...currentKeys(sql, shortId)]
    .filter(([, pubkey]) => !keyUsed(sql, shortId, '02' + pubkey))
    .sort(([a], [b]) => a - b)
    .map(([slot, pubkey]) => ({slot, pubkey}));
  return {version: versionOf(sql, shortId), federation_id: fed.federation_id, free};
}

function parseLoad(r: Record<string, unknown>, slots: number): {txid: string; notes: LoadNote[]} {
  if (!isHex(r.txid, 32)) {
    throw new BadRequest('txid must be 32 bytes, hex');
  }
  if (!Array.isArray(r.notes) || r.notes.length === 0 || r.notes.length > slots) {
    throw new BadRequest(`notes must list 1 to ${slots} notes`);
  }
  const notes: LoadNote[] = r.notes.map(n => {
    const o = n as Record<string, unknown>;
    if (
      typeof o !== 'object' ||
      o === null ||
      !Number.isSafeInteger(o.slot) ||
      (o.slot as number) < 0 ||
      (o.slot as number) >= slots ||
      !isPositiveInt(o.amount_msat) ||
      !isHex(o.nonce, 33) ||
      !isHex(o.blinding_key, 32) ||
      !isCount(o.out_idx)
    ) {
      throw new BadRequest(`each note needs slot 0-${slots - 1}, amount_msat, nonce, blinding_key and out_idx`);
    }
    return {slot: o.slot as number, amount_msat: o.amount_msat, nonce: o.nonce, blinding_key: o.blinding_key, out_idx: o.out_idx};
  });
  if (new Set(notes.map(n => n.slot)).size !== notes.length) {
    throw new BadRequest('notes name a slot twice');
  }
  if (new Set(notes.map(n => n.out_idx)).size !== notes.length) {
    throw new BadRequest('notes name an output twice');
  }
  return {txid: r.txid, notes};
}

/**
 * `POST /cards/:short_id/load`: the wallet's notes, recorded pending before it submits. A replay
 * of the open load answers its version; anything else needs the current version, free slots under
 * the card's own keys, tiers the federation has, and no other open load.
 */
export function recordLoad(
  storage: DurableObjectStorage,
  shortId: string,
  r: Record<string, unknown>,
  now: number,
): {version: number; replayed: boolean; expires_at: number} {
  const sql = storage.sql;
  const load = parseLoad(r, slotCount(sql, shortId));
  const body = JSON.stringify(load.notes);
  return storage.transactionSync(() => {
    const prior = sql.exec('SELECT * FROM holder_loads WHERE txid = ?', load.txid).toArray()[0] as LoadRow | undefined;
    if (prior) {
      if (prior.short_id === shortId && prior.body === body && prior.status === 'open') {
        return {version: versionOf(sql, shortId), replayed: true, expires_at: Number(prior.expires_at)};
      }
      throw new Conflict(`load ${load.txid} is already recorded (${prior.status})`);
    }
    requireVersion(sql, shortId, r.version);
    const open = sql.exec("SELECT txid FROM holder_loads WHERE short_id = ? AND status = 'open'", shortId).toArray();
    if (open.length > 0) {
      throw new Conflict(`card ${shortId} has a load in flight; it ends by ${LOAD_TTL_MS / 60000} minutes after it began`);
    }
    const keys = mintKeysFor(sql, shortId);
    const slots = currentKeys(sql, shortId);
    for (const n of load.notes) {
      const pubkey = slots.get(n.slot);
      if (pubkey === undefined) {
        throw new Conflict(`card ${shortId} has no key in slot ${n.slot}`);
      }
      if (n.nonce !== '02' + pubkey) {
        throw new Conflict(`the note for slot ${n.slot} is not under the card's key for that slot`);
      }
      if (keyUsed(sql, shortId, n.nonce)) {
        throw new Conflict(`slot ${n.slot} of card ${shortId} is not free; read /slots again`);
      }
      if (!isTier(keys, n.amount_msat)) {
        throw new Unprocessable(`${n.amount_msat} msat is not a note tier of card ${shortId}'s federation`);
      }
    }
    const expiresAt = now + LOAD_TTL_MS;
    sql.exec(
      `INSERT INTO holder_loads (txid, short_id, body, status, outcome, created_at, expires_at, resolved_at)
       VALUES (?, ?, ?, 'open', NULL, ?, ?, NULL)`,
      load.txid,
      shortId,
      body,
      now,
      expiresAt,
    );
    for (const n of load.notes) {
      sql.exec(
        `INSERT INTO notes (short_id, nonce, slot, amount_msat, signature, blinding_key, status, txid, out_idx)
         VALUES (?, ?, ?, ?, NULL, ?, 'pending', ?, ?)`,
        shortId,
        n.nonce,
        n.slot,
        n.amount_msat,
        n.blinding_key,
        load.txid,
        n.out_idx,
      );
    }
    return {version: bump(sql, shortId), replayed: false, expires_at: expiresAt};
  });
}

/**
 * `POST /cards/:short_id/load/outcome`. With signatures (accepted, the default): every one is
 * checked against the federation's key for its note's tier, and only if all verify do the notes
 * become live; one bad signature is a 422 and changes nothing. `{accepted: false}`: the federation
 * refused the transaction, the notes are deleted and their slots free. A repeat of the recorded
 * outcome answers 200; a different one is a 409.
 *
 * A load that expired is still finished by its signatures while each of its slots holds the same
 * key and no note has been recorded under it since.
 */
export function recordLoadOutcome(
  storage: DurableObjectStorage,
  shortId: string,
  r: Record<string, unknown>,
  now: number,
): {version: number; replayed: boolean} {
  if (!isHex(r.txid, 32)) {
    throw new BadRequest('txid must be 32 bytes, hex');
  }
  if (r.accepted !== undefined && typeof r.accepted !== 'boolean') {
    throw new BadRequest('accepted must be a boolean');
  }
  const accepted = r.accepted !== false;
  const signatures = new Map<string, string>();
  if (accepted) {
    if (!Array.isArray(r.notes)) {
      throw new BadRequest('an accepted load needs notes: [{nonce, signature}]');
    }
    for (const n of r.notes) {
      const o = n as Record<string, unknown>;
      if (typeof o !== 'object' || o === null || !isHex(o.nonce, 33) || !isHex(o.signature, 48)) {
        throw new BadRequest('each note needs nonce (33 bytes) and signature (48 bytes), hex');
      }
      signatures.set(o.nonce, o.signature);
    }
  }
  const outcome = JSON.stringify({accepted, notes: [...signatures].sort()});
  const sql = storage.sql;
  return storage.transactionSync(() => {
    const load = sql.exec('SELECT * FROM holder_loads WHERE txid = ?', r.txid).toArray()[0] as LoadRow | undefined;
    if (!load || load.short_id !== shortId) {
      throw new NotFound(`no load ${String(r.txid)} on card ${shortId}`);
    }
    if (load.status === 'accepted' || load.status === 'rejected') {
      if (load.outcome === outcome) {
        return {version: versionOf(sql, shortId), replayed: true};
      }
      throw new Conflict(`load ${String(r.txid)} is already resolved differently`);
    }
    const notes = JSON.parse(load.body) as LoadNote[];
    if (!accepted) {
      if (load.status === 'expired') {
        // Its notes went with the expiry; record what the wallet says, nothing else changes.
        sql.exec("UPDATE holder_loads SET status = 'rejected', outcome = ?, resolved_at = ? WHERE txid = ?", outcome, now, r.txid);
        return {version: versionOf(sql, shortId), replayed: false};
      }
      sql.exec("DELETE FROM notes WHERE short_id = ? AND txid = ? AND status = 'pending'", shortId, r.txid);
      sql.exec("UPDATE holder_loads SET status = 'rejected', outcome = ?, resolved_at = ? WHERE txid = ?", outcome, now, r.txid);
      return {version: bump(sql, shortId), replayed: false};
    }
    if (signatures.size !== notes.length || !notes.every(n => signatures.has(n.nonce))) {
      throw new BadRequest('an accepted load needs a signature for every note it recorded, and no others');
    }
    checkNotes(sql, shortId, notes.map(n => ({nonce: n.nonce, amount_msat: n.amount_msat, signature: signatures.get(n.nonce)!})));
    if (load.status === 'expired') {
      const slots = currentKeys(sql, shortId);
      for (const n of notes) {
        if (slots.get(n.slot) === undefined || '02' + slots.get(n.slot) !== n.nonce || keyUsed(sql, shortId, n.nonce)) {
          throw new Conflict(`load ${String(r.txid)} expired and slot ${n.slot} has been used since`);
        }
      }
      for (const n of notes) {
        sql.exec(
          `INSERT INTO notes (short_id, nonce, slot, amount_msat, signature, blinding_key, status, txid, out_idx)
           VALUES (?, ?, ?, ?, ?, NULL, 'live', ?, ?)`,
          shortId,
          n.nonce,
          n.slot,
          n.amount_msat,
          signatures.get(n.nonce)!,
          r.txid,
          n.out_idx,
        );
      }
    } else {
      for (const n of notes) {
        sql.exec(
          "UPDATE notes SET status = 'live', signature = ?, blinding_key = NULL WHERE short_id = ? AND nonce = ?",
          signatures.get(n.nonce)!,
          shortId,
          n.nonce,
        );
      }
    }
    sql.exec("UPDATE holder_loads SET status = 'accepted', outcome = ?, resolved_at = ? WHERE txid = ?", outcome, now, r.txid);
    return {version: bump(sql, shortId), replayed: false};
  });
}
