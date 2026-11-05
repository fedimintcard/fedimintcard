/**
 * Batches, card rows and card state (OPS-PLAN § 5).
 *
 * Card state is what the sidecar's `card_state.json` held
 * (`host/sidecar/src/state.rs`), per card instead of one file per data dir:
 * the eight slot keys, the notes behind them, and each charge a till made.
 * None of it is spendable here. A note is spent only with a BIP340 signature
 * from its slot key, which never leaves the card, behind the card's PIN. The
 * blinding keys of pending change notes unblind a signature; the note still
 * needs the card. The secret keys of a till's own outputs never come here.
 *
 * Writes follow the sidecar's write-ahead order (`glue.rs` `sign_and_submit`):
 * a till records its intent (the unsigned transaction; a txid does not cover
 * signatures) before the card signs, the signed bytes before it sends them,
 * and the outcome after the federation answers. Every write names the card `version` it read, and a
 * stale one is a 409, so two tills cannot both write against the same notes.
 * The federation's spent set stops a double spend whatever this says.
 */
import {
  BadRequest,
  Conflict,
  isCount,
  isHex,
  isPinPolicy,
  isPositiveInt,
  isShortId,
  NotFound,
} from './http';
import {isChip, linkChip} from './managerKeys';
import {checkNotes, putMintKeys} from './mint';

export const SLOTS = 8;
/** A transaction's consensus encoding, hex; a charge's is a few KB. */
const MAX_TX_HEX = 64 * 1024;

export {BadRequest, Conflict, NotFound};

type Sql = SqlStorage;

/* ------------------------------------------------------------------------- */
/* Batches                                                                   */
/* ------------------------------------------------------------------------- */

export interface RegistryBatch {
  readonly batch_id: number;
  readonly federation_id: string;
  readonly count: number;
  readonly pin_policy: 'batch' | 'perCard';
  readonly load_sha256: string;
  readonly created_at: number;
}

export function createBatch(
  sql: Sql,
  operatorId: number,
  r: Record<string, unknown>,
  now: number,
): RegistryBatch {
  // The federation's id, hex, as the app decodes it from the invite (OPS-PLAN
  // § 9.4). The first deployment took the whole invite; that is refused now.
  if (!isHex(r.federation_id, 32)) {
    throw new BadRequest('federation_id must be the 32-byte federation id, hex');
  }
  if (!isPositiveInt(r.count)) {
    throw new BadRequest('count must be a positive integer');
  }
  if (!isPinPolicy(r.pin_policy)) {
    throw new BadRequest("pin_policy must be 'batch' or 'perCard'");
  }
  if (!isHex(r.load_sha256, 32)) {
    throw new BadRequest('load_sha256 must be 64 lower-case hex characters');
  }
  const row = sql
    .exec(
      `INSERT INTO batches (federation_id, count, pin_policy, load_sha256, created_at, operator_id)
       VALUES (?, ?, ?, ?, ?, ?) RETURNING batch_id`,
      r.federation_id,
      r.count,
      r.pin_policy,
      r.load_sha256,
      now,
      operatorId,
    )
    .one() as {batch_id: number};
  return {
    batch_id: Number(row.batch_id),
    federation_id: r.federation_id,
    count: r.count,
    pin_policy: r.pin_policy,
    load_sha256: r.load_sha256,
    created_at: now,
  };
}

export function listBatches(sql: Sql, operatorId: number): RegistryBatch[] {
  return sql
    .exec(
      `SELECT batch_id, federation_id, count, pin_policy, load_sha256, created_at
       FROM batches WHERE operator_id = ? ORDER BY batch_id`,
      operatorId,
    )
    .toArray()
    .map(r => ({
      batch_id: Number(r.batch_id),
      federation_id: String(r.federation_id),
      count: Number(r.count),
      pin_policy: String(r.pin_policy) as 'batch' | 'perCard',
      load_sha256: String(r.load_sha256),
      created_at: Number(r.created_at),
    }));
}

export function batchOwner(sql: Sql, batchId: number): number | null {
  const row = sql
    .exec('SELECT operator_id FROM batches WHERE batch_id = ?', batchId)
    .toArray()[0] as {operator_id: number} | undefined;
  return row ? Number(row.operator_id) : null;
}

/** The operator a card belongs to, through its batch; null for no such card. */
export function cardOwner(sql: Sql, shortId: string): number | null {
  const row = sql
    .exec(
      'SELECT b.operator_id FROM cards c JOIN batches b ON b.batch_id = c.batch_id WHERE c.short_id = ?',
      shortId,
    )
    .toArray()[0] as {operator_id: number} | undefined;
  return row ? Number(row.operator_id) : null;
}

/* ------------------------------------------------------------------------- */
/* Card rows                                                                 */
/* ------------------------------------------------------------------------- */

export interface CardBody {
  readonly short_id: string;
  readonly batch_id: number;
  readonly installed_at: number;
  readonly load_sha256: string;
  readonly pin_set: boolean;
  readonly pin_policy: 'batch' | 'perCard';
  readonly notes_issued: number;
  /** The eight slot keys as provisioned; empty for a row from before O2. */
  readonly slot_pubkeys: readonly string[];
  /**
   * The chip's CPLC id (`managerKeys.ts`), which keys its card-manager set;
   * null on a row from before O5.
   */
  readonly chip: string | null;
}

export interface StoredCard extends CardBody {
  readonly received_at: number;
  /** When the provisioner took the card back to factory (`reset.ts`); its short id takes nothing new after. */
  readonly reset_at: number | null;
  /**
   * The short id the card was finished under when a resume tap re-keyed it
   * (`resume.ts`); this short id takes nothing new after.
   */
  readonly replaced_by: string | null;
}

/**
 * The card body, or why it is not one. Unknown keys are ignored, so a newer
 * app does not break an older server. `slot_pubkeys` is optional for rows an
 * older app queued before it sent them; when present it must be all eight
 * x-only keys, and slot 0's last 4 bytes must be the short id.
 */
export function parseCard(r: Record<string, unknown>): CardBody {
  if (!isShortId(r.short_id)) {
    throw new BadRequest('short_id must be 8 lower-case hex characters');
  }
  if (!isPositiveInt(r.batch_id)) {
    throw new BadRequest('batch_id must be a positive integer');
  }
  if (!isPositiveInt(r.installed_at)) {
    throw new BadRequest('installed_at must be epoch milliseconds');
  }
  if (!isHex(r.load_sha256, 32)) {
    throw new BadRequest('load_sha256 must be 64 lower-case hex characters');
  }
  if (typeof r.pin_set !== 'boolean') {
    throw new BadRequest('pin_set must be a boolean');
  }
  if (!isPinPolicy(r.pin_policy)) {
    throw new BadRequest("pin_policy must be 'batch' or 'perCard'");
  }
  if (!isCount(r.notes_issued)) {
    throw new BadRequest('notes_issued must be a non-negative integer');
  }
  let slots: string[] = [];
  if (r.slot_pubkeys !== undefined) {
    const keys = r.slot_pubkeys;
    if (!Array.isArray(keys) || keys.length !== SLOTS || !keys.every(k => isHex(k, 32))) {
      throw new BadRequest('slot_pubkeys must be 8 x-only keys, 64 lower-case hex each');
    }
    if ((keys[0] as string).slice(-8) !== r.short_id) {
      throw new BadRequest('short_id must be the last 4 bytes of slot 0’s key');
    }
    slots = keys as string[];
  }
  if (r.chip !== undefined && !isChip(r.chip)) {
    throw new BadRequest('chip must be the card’s CPLC id, cplc-v1: and 24 lower-case hex characters');
  }
  return {
    short_id: r.short_id,
    batch_id: r.batch_id,
    installed_at: r.installed_at,
    load_sha256: r.load_sha256,
    pin_set: r.pin_set,
    pin_policy: r.pin_policy,
    notes_issued: r.notes_issued,
    slot_pubkeys: slots,
    chip: r.chip === undefined ? null : r.chip,
  };
}

export function storedCard(sql: Sql, shortId: string): StoredCard | null {
  const row = sql.exec('SELECT * FROM cards WHERE short_id = ?', shortId).toArray()[0];
  if (!row) {
    return null;
  }
  // The keys as provisioned, kept on the row; `card_slots` holds the current
  // ones, which change when a till renews a spent slot.
  const slots =
    row.slot_pubkeys === null || row.slot_pubkeys === undefined
      ? []
      : (JSON.parse(String(row.slot_pubkeys)) as string[]);
  return {
    short_id: String(row.short_id),
    batch_id: Number(row.batch_id),
    installed_at: Number(row.installed_at),
    load_sha256: String(row.load_sha256),
    pin_set: Number(row.pin_set) === 1,
    pin_policy: String(row.pin_policy) as 'batch' | 'perCard',
    notes_issued: Number(row.notes_issued),
    slot_pubkeys: slots,
    chip: row.chip === null || row.chip === undefined ? null : String(row.chip),
    received_at: Number(row.received_at),
    reset_at: row.reset_at === null || row.reset_at === undefined ? null : Number(row.reset_at),
    replaced_by:
      row.replaced_by === null || row.replaced_by === undefined ? null : String(row.replaced_by),
  };
}

export function sameCard(a: CardBody, b: CardBody): boolean {
  return (
    a.chip === b.chip &&
    a.batch_id === b.batch_id &&
    a.installed_at === b.installed_at &&
    a.load_sha256 === b.load_sha256 &&
    a.pin_set === b.pin_set &&
    a.pin_policy === b.pin_policy &&
    a.notes_issued === b.notes_issued &&
    a.slot_pubkeys.length === b.slot_pubkeys.length &&
    a.slot_pubkeys.every((k, i) => k === b.slot_pubkeys[i])
  );
}

/**
 * The idempotent upsert: an identical body answers the stored row, a
 * different one for a stored short id is a Conflict and stores nothing. The
 * batch must be this operator's. `received_at` is ours and never compared;
 * slot keys are compared as provisioned, so a renewal does not make the
 * outbox's re-post a conflict.
 */
export function postCard(
  storage: DurableObjectStorage,
  operatorId: number,
  r: Record<string, unknown>,
  now: number,
  /**
   * Runs in the same transaction, before the row is written: a resumed card
   * closes the row it supersedes here (`resume.ts`), and a throw stores nothing.
   */
  alongside?: (card: CardBody) => void,
): StoredCard {
  const card = parseCard(r);
  const sql = storage.sql;
  const owner = batchOwner(sql, card.batch_id);
  if (owner !== operatorId) {
    throw new NotFound(`no batch ${card.batch_id} for this operator`);
  }
  return storage.transactionSync(() => {
    alongside?.(card);
    return insertCard(sql, operatorId, card, now);
  });
}

/**
 * The upsert's body, inside the caller's transaction: the stored row for an
 * identical body, a Conflict for a different one, else the new row with its
 * slots and version. A row that names its chip records it against that
 * chip's escrowed manager keys.
 */
export function insertCard(sql: Sql, operatorId: number, card: CardBody, now: number): StoredCard {
  const existing = storedCard(sql, card.short_id);
  if (existing) {
    if (sameCard(existing, card)) {
      return existing;
    }
    throw new Conflict(`card ${card.short_id} is already stored with different contents`);
  }
  sql.exec(
    `INSERT INTO cards
       (short_id, batch_id, installed_at, load_sha256, pin_set, pin_policy, notes_issued, received_at, slot_pubkeys, chip)
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
    card.short_id,
    card.batch_id,
    card.installed_at,
    card.load_sha256,
    card.pin_set ? 1 : 0,
    card.pin_policy,
    card.notes_issued,
    now,
    card.slot_pubkeys.length > 0 ? JSON.stringify(card.slot_pubkeys) : null,
    card.chip,
  );
  card.slot_pubkeys.forEach((pubkey, slot) => {
    sql.exec(
      'INSERT INTO card_slots (short_id, slot, pubkey) VALUES (?, ?, ?)',
      card.short_id,
      slot,
      pubkey,
    );
  });
  sql.exec(
    'INSERT INTO card_versions (short_id, version) VALUES (?, 0)',
    card.short_id,
  );
  if (card.chip !== null) {
    linkChip(sql, card.chip, card.short_id, operatorId);
  }
  return {...card, received_at: now, reset_at: null, replaced_by: null};
}

export function listCards(
  sql: Sql,
  operatorId: number,
  batchId: number,
): StoredCard[] {
  if (batchOwner(sql, batchId) !== operatorId) {
    throw new NotFound(`no batch ${batchId} for this operator`);
  }
  return sql
    .exec('SELECT short_id FROM cards WHERE batch_id = ? ORDER BY received_at', batchId)
    .toArray()
    .map(r => storedCard(sql, String(r.short_id))!);
}

/* ------------------------------------------------------------------------- */
/* Card state                                                                */
/* ------------------------------------------------------------------------- */

export function versionOf(sql: Sql, shortId: string): number {
  const row = sql
    .exec('SELECT version FROM card_versions WHERE short_id = ?', shortId)
    .toArray()[0] as {version: number} | undefined;
  return row ? Number(row.version) : 0;
}

export function bump(sql: Sql, shortId: string): number {
  const next = versionOf(sql, shortId) + 1;
  sql.exec(
    `INSERT INTO card_versions (short_id, version) VALUES (?, ?)
     ON CONFLICT(short_id) DO UPDATE SET version = excluded.version`,
    shortId,
    next,
  );
  return next;
}

export function requireVersion(sql: Sql, shortId: string, version: unknown): void {
  if (!Number.isSafeInteger(version)) {
    throw new BadRequest('version must be the integer the state answered');
  }
  const current = versionOf(sql, shortId);
  if (version !== current) {
    throw new Conflict(`card ${shortId} is at version ${current}, not ${version}; read it again`);
  }
}

export interface NoteView {
  readonly slot: number;
  readonly amount_msat: number;
  readonly nonce: string;
  readonly signature: string | null;
  readonly blinding_key: string | null;
  readonly status: 'live' | 'pending' | 'spent' | 'dead';
  readonly txid: string | null;
  readonly out_idx: number | null;
}

export function cardState(sql: Sql, shortId: string) {
  const slots = sql
    .exec('SELECT slot, pubkey FROM card_slots WHERE short_id = ? ORDER BY slot', shortId)
    .toArray()
    .map(r => ({slot: Number(r.slot), pubkey: String(r.pubkey)}));
  // Live and pending notes, and the spent or dead ones still on a slot's current
  // key: that key is burned until a till renews the slot, and a till that never
  // saw the card learns it only from here (the sidecar's `overwritable_slots`).
  // A holder's load in flight shows as pending notes without txid or blinding key:
  // a till keeps off those slots but does not try to collect them (`loads.ts`).
  const notes: NoteView[] = sql
    .exec(
      `SELECT slot, amount_msat, nonce, signature, status, out_idx,
         CASE WHEN held THEN NULL ELSE txid END AS txid,
         CASE WHEN held THEN NULL ELSE blinding_key END AS blinding_key
       FROM (SELECT *, status = 'pending' AND txid IN (SELECT txid FROM holder_loads) AS held FROM notes)
       WHERE short_id = ? AND (status IN ('live', 'pending')
         OR nonce IN (SELECT '02' || pubkey FROM card_slots WHERE short_id = ?))
       ORDER BY slot`,
      shortId,
      shortId,
    )
    .toArray()
    .map(r => ({
      slot: Number(r.slot),
      amount_msat: Number(r.amount_msat),
      nonce: String(r.nonce),
      signature: r.signature === null ? null : String(r.signature),
      blinding_key: r.blinding_key === null ? null : String(r.blinding_key),
      status: String(r.status) as NoteView['status'],
      txid: r.txid === null ? null : String(r.txid),
      out_idx: r.out_idx === null ? null : Number(r.out_idx),
    }));
  const open = sql
    .exec(
      "SELECT txid, till_id, client_ref, body, signed_hex, created_at FROM card_txs WHERE short_id = ? AND status = 'open'",
      shortId,
    )
    .toArray()
    .map(r => ({
      txid: String(r.txid),
      till_id: Number(r.till_id),
      client_ref: String(r.client_ref),
      // Null: the charge never left its till, so it can be recorded rejected.
      signed_hex: r.signed_hex === null ? null : String(r.signed_hex),
      created_at: Number(r.created_at),
      ...(JSON.parse(String(r.body)) as {tx_hex: string | null; inputs: string[]; change: ChangeOut[]}),
    }));
  return {
    short_id: shortId,
    version: versionOf(sql, shortId),
    slots,
    notes,
    open_txs: open,
  };
}

interface ChangeOut {
  readonly slot: number;
  readonly amount_msat: number;
  readonly nonce: string;
  readonly blinding_key: string;
  readonly out_idx: number;
}

function parseIntent(r: Record<string, unknown>) {
  if (!isHex(r.txid, 32)) {
    throw new BadRequest('txid must be 32 bytes, hex');
  }
  if (typeof r.client_ref !== 'string' || r.client_ref.length < 1 || r.client_ref.length > 128) {
    throw new BadRequest('client_ref must be 1 to 128 characters');
  }
  if (!Array.isArray(r.inputs) || !r.inputs.every(n => isHex(n, 33))) {
    throw new BadRequest('inputs must be the spent notes’ nonces, 33 bytes hex each');
  }
  // No inputs is a load: notes a till's own wallet issued to the card. The
  // wallet funds and signs it, so there are no unsigned bytes for anyone to
  // resubmit, and tx_hex is optional; a charge must carry them.
  const load = r.inputs.length === 0;
  if (
    !(load && r.tx_hex === undefined) &&
    (typeof r.tx_hex !== 'string' ||
      r.tx_hex.length === 0 ||
      r.tx_hex.length > MAX_TX_HEX ||
      r.tx_hex.length % 2 !== 0 ||
      !/^[0-9a-f]*$/.test(r.tx_hex))
  ) {
    throw new BadRequest('tx_hex must be the unsigned transaction, lower-case hex');
  }
  if (new Set(r.inputs).size !== r.inputs.length) {
    throw new BadRequest('inputs name a note twice');
  }
  if (!Array.isArray(r.change)) {
    throw new BadRequest('change must be a list');
  }
  const change: ChangeOut[] = r.change.map(c => {
    const o = c as Record<string, unknown>;
    if (
      typeof o !== 'object' ||
      o === null ||
      !Number.isSafeInteger(o.slot) ||
      (o.slot as number) < 0 ||
      (o.slot as number) >= SLOTS ||
      !isPositiveInt(o.amount_msat) ||
      !isHex(o.nonce, 33) ||
      !isHex(o.blinding_key, 32) ||
      !isCount(o.out_idx)
    ) {
      throw new BadRequest('each change output needs slot 0-7, amount_msat, nonce, blinding_key and out_idx');
    }
    return {
      slot: o.slot as number,
      amount_msat: o.amount_msat,
      nonce: o.nonce,
      blinding_key: o.blinding_key,
      out_idx: o.out_idx,
    };
  });
  if (new Set(change.map(c => c.slot)).size !== change.length) {
    throw new BadRequest('change names a slot twice');
  }
  if (load && change.length === 0) {
    throw new BadRequest('a load (no inputs) needs at least one note');
  }
  return {
    txid: r.txid,
    client_ref: r.client_ref,
    body: {tx_hex: (r.tx_hex as string | undefined) ?? null, inputs: r.inputs as string[], change},
  };
}

/**
 * A till's charge, recorded before the card signs. Replaying the same intent
 * answers the current version; a different body under the same txid, a
 * stale version, a second open charge, an input that is not a live note of
 * this card, or change into an occupied or unknown slot is refused.
 */
export function recordIntent(
  storage: DurableObjectStorage,
  shortId: string,
  tillId: number,
  r: Record<string, unknown>,
  now: number,
): {version: number; replayed: boolean} {
  const intent = parseIntent(r);
  const sql = storage.sql;
  return storage.transactionSync(() => {
    const prior = sql
      .exec('SELECT short_id, client_ref, body FROM card_txs WHERE txid = ?', intent.txid)
      .toArray()[0] as {short_id: string; client_ref: string; body: string} | undefined;
    if (prior) {
      if (
        prior.short_id === shortId &&
        prior.client_ref === intent.client_ref &&
        prior.body === JSON.stringify(intent.body)
      ) {
        return {version: versionOf(sql, shortId), replayed: true};
      }
      throw new Conflict(`transaction ${intent.txid} is already recorded differently`);
    }
    requireVersion(sql, shortId, r.version);
    const open = sql
      .exec("SELECT txid FROM card_txs WHERE short_id = ? AND status = 'open'", shortId)
      .toArray();
    if (open.length > 0) {
      throw new Conflict(`card ${shortId} has an open charge ${String(open[0]!.txid)}; resolve it first`);
    }
    for (const nonce of intent.body.inputs) {
      const note = sql
        .exec('SELECT status FROM notes WHERE short_id = ? AND nonce = ?', shortId, nonce)
        .toArray()[0] as {status: string} | undefined;
      if (!note || note.status !== 'live') {
        throw new Conflict(`input ${nonce} is not a live note of card ${shortId}`);
      }
    }
    const slots = new Set(
      sql
        .exec('SELECT slot FROM card_slots WHERE short_id = ?', shortId)
        .toArray()
        .map(s => Number(s.slot)),
    );
    const spentSlots = new Set(
      intent.body.inputs.map(n =>
        Number(
          (sql.exec('SELECT slot FROM notes WHERE short_id = ? AND nonce = ?', shortId, n).one() as {slot: number}).slot,
        ),
      ),
    );
    for (const c of intent.body.change) {
      if (!slots.has(c.slot)) {
        throw new Conflict(`card ${shortId} has no key in slot ${c.slot}`);
      }
      const held = sql
        .exec(
          "SELECT nonce FROM notes WHERE short_id = ? AND slot = ? AND status IN ('live', 'pending')",
          shortId,
          c.slot,
        )
        .toArray();
      if (held.length > 0 || spentSlots.has(c.slot)) {
        throw new Conflict(`slot ${c.slot} of card ${shortId} already backs a note`);
      }
      // A key carries one note in its life: a spent or dead note's key is
      // renewed (PUT /slots/:n) before the slot takes another.
      const used = sql
        .exec('SELECT status FROM notes WHERE short_id = ? AND nonce = ?', shortId, c.nonce)
        .toArray();
      if (used.length > 0) {
        throw new Conflict(`the key behind change nonce ${c.nonce} already carried a note; renew slot ${c.slot} first`);
      }
    }
    sql.exec(
      `INSERT INTO card_txs (txid, short_id, till_id, client_ref, body, status, outcome, created_at, resolved_at)
       VALUES (?, ?, ?, ?, ?, 'open', NULL, ?, NULL)`,
      intent.txid,
      shortId,
      tillId,
      intent.client_ref,
      JSON.stringify(intent.body),
      now,
    );
    for (const c of intent.body.change) {
      sql.exec(
        `INSERT INTO notes (short_id, nonce, slot, amount_msat, signature, blinding_key, status, txid, out_idx)
         VALUES (?, ?, ?, ?, NULL, ?, 'pending', ?, ?)`,
        shortId,
        c.nonce,
        c.slot,
        c.amount_msat,
        c.blinding_key,
        intent.txid,
        c.out_idx,
      );
    }
    return {version: bump(sql, shortId), replayed: false};
  });
}

/**
 * What the federation decided. Accepted: the inputs are spent and each change
 * note becomes live with its unblinded signature. Rejected: the change notes
 * are dead and the inputs stay live. Any till of the operator may record it,
 * so a charge whose till died is finished by the next one to see the card.
 * A repeat of the recorded outcome answers 200; a different one is a 409.
 */
export function recordOutcome(
  storage: DurableObjectStorage,
  shortId: string,
  r: Record<string, unknown>,
  now: number,
): {version: number; replayed: boolean} {
  if (!isHex(r.txid, 32)) {
    throw new BadRequest('txid must be 32 bytes, hex');
  }
  if (typeof r.accepted !== 'boolean') {
    throw new BadRequest('accepted must be a boolean');
  }
  const signatures = new Map<string, string>();
  if (r.accepted) {
    if (!Array.isArray(r.change)) {
      throw new BadRequest('an accepted charge needs change: [{nonce, signature}]');
    }
    for (const c of r.change) {
      const o = c as Record<string, unknown>;
      if (typeof o !== 'object' || o === null || !isHex(o.nonce, 33) || !isHex(o.signature, 48)) {
        throw new BadRequest('each change note needs nonce (33 bytes) and signature (48 bytes), hex');
      }
      signatures.set(o.nonce, o.signature);
    }
  }
  const outcome = JSON.stringify({accepted: r.accepted, change: [...signatures].sort()});
  const sql = storage.sql;
  return storage.transactionSync(() => {
    const tx = sql
      .exec('SELECT short_id, body, status, outcome FROM card_txs WHERE txid = ?', r.txid)
      .toArray()[0] as {short_id: string; body: string; status: string; outcome: string | null} | undefined;
    if (!tx || tx.short_id !== shortId) {
      throw new NotFound(`no charge ${String(r.txid)} on card ${shortId}`);
    }
    if (tx.status !== 'open') {
      if (tx.outcome === outcome) {
        return {version: versionOf(sql, shortId), replayed: true};
      }
      throw new Conflict(`charge ${String(r.txid)} is already resolved differently`);
    }
    const body = JSON.parse(tx.body) as {inputs: string[]; change: ChangeOut[]};
    if (r.accepted) {
      if (
        signatures.size !== body.change.length ||
        !body.change.every(c => signatures.has(c.nonce))
      ) {
        throw new BadRequest('an accepted charge needs a signature for every change note, and no others');
      }
      // Every live note in ops carries a signature the federation made (O4): a till's change
      // and loads are checked as a holder's load is.
      checkNotes(
        sql,
        shortId,
        body.change.map(c => ({nonce: c.nonce, amount_msat: c.amount_msat, signature: signatures.get(c.nonce)!})),
      );
      for (const nonce of body.inputs) {
        sql.exec(
          "UPDATE notes SET status = 'spent' WHERE short_id = ? AND nonce = ?",
          shortId,
          nonce,
        );
      }
      for (const c of body.change) {
        sql.exec(
          "UPDATE notes SET status = 'live', signature = ? WHERE short_id = ? AND nonce = ?",
          signatures.get(c.nonce)!,
          shortId,
          c.nonce,
        );
      }
    } else {
      for (const c of body.change) {
        sql.exec(
          "UPDATE notes SET status = 'dead' WHERE short_id = ? AND nonce = ?",
          shortId,
          c.nonce,
        );
      }
    }
    sql.exec(
      'UPDATE card_txs SET status = ?, outcome = ?, resolved_at = ? WHERE txid = ?',
      r.accepted ? 'accepted' : 'rejected',
      outcome,
      now,
      r.txid,
    );
    return {version: bump(sql, shortId), replayed: false};
  });
}

/**
 * The signed bytes of an open charge, posted after the card signs and before
 * the till sends them. A till sends only after this succeeds; a charge
 * resolved meanwhile (another till recorded it rejected) is a 409, and the
 * till must not send. Repeating the same bytes answers 200.
 */
export function recordSubmitted(
  storage: DurableObjectStorage,
  shortId: string,
  r: Record<string, unknown>,
): {version: number; replayed: boolean} {
  if (!isHex(r.txid, 32)) {
    throw new BadRequest('txid must be 32 bytes, hex');
  }
  if (
    typeof r.tx_hex !== 'string' ||
    r.tx_hex.length === 0 ||
    r.tx_hex.length > MAX_TX_HEX ||
    r.tx_hex.length % 2 !== 0 ||
    !/^[0-9a-f]*$/.test(r.tx_hex)
  ) {
    throw new BadRequest('tx_hex must be the signed transaction, lower-case hex');
  }
  const sql = storage.sql;
  return storage.transactionSync(() => {
    const tx = sql
      .exec('SELECT short_id, status, signed_hex FROM card_txs WHERE txid = ?', r.txid)
      .toArray()[0] as {short_id: string; status: string; signed_hex: string | null} | undefined;
    if (!tx || tx.short_id !== shortId) {
      throw new NotFound(`no charge ${String(r.txid)} on card ${shortId}`);
    }
    if (tx.signed_hex !== null) {
      if (tx.signed_hex === r.tx_hex) {
        return {version: versionOf(sql, shortId), replayed: true};
      }
      throw new Conflict(`charge ${String(r.txid)} already has other signed bytes`);
    }
    if (tx.status !== 'open') {
      throw new Conflict(`charge ${String(r.txid)} is already resolved; do not send it`);
    }
    sql.exec('UPDATE card_txs SET signed_hex = ? WHERE txid = ?', r.tx_hex, r.txid);
    return {version: bump(sql, shortId), replayed: false};
  });
}

/* ------------------------------------------------------------------------- */
/* Federation names                                                          */
/* ------------------------------------------------------------------------- */

/**
 * What a till read from the federation's config, kept per operator: its name, and the mint's
 * key per note tier (`tbs_pks`, see `mint.ts`). Either may be absent, not both: a federation
 * without a name still has keys.
 */
export function putFederationName(
  sql: Sql,
  operatorId: number,
  r: Record<string, unknown>,
  now: number,
): void {
  if (!isHex(r.federation_id, 32)) {
    throw new BadRequest('federation_id must be the decoded federation id, 32 bytes hex');
  }
  if (r.name === undefined && r.tbs_pks === undefined) {
    throw new BadRequest('expected a name, the mint keys (tbs_pks), or both');
  }
  if (r.name !== undefined) {
    const name = typeof r.name === 'string' ? r.name.trim() : '';
    if (name === '' || name.length > 128) {
      throw new BadRequest('name must be 1 to 128 characters');
    }
    sql.exec(
      `INSERT INTO federations (operator_id, federation_id, name, updated_at) VALUES (?, ?, ?, ?)
       ON CONFLICT (operator_id, federation_id) DO UPDATE SET name = excluded.name, updated_at = excluded.updated_at`,
      operatorId,
      r.federation_id,
      name,
      now,
    );
  }
  if (r.tbs_pks !== undefined) {
    putMintKeys(sql, operatorId, r.federation_id, r.tbs_pks, now);
  }
}

export function listFederationNames(
  sql: Sql,
  operatorId: number,
): {federation_id: string; name: string; updated_at: number}[] {
  return sql
    .exec(
      'SELECT federation_id, name, updated_at FROM federations WHERE operator_id = ? ORDER BY federation_id',
      operatorId,
    )
    .toArray()
    .map(r => ({
      federation_id: String(r.federation_id),
      name: String(r.name),
      updated_at: Number(r.updated_at),
    }));
}

/**
 * A slot's new key after the till renewed it (GEN_KEY with P2 = 01): only a
 * slot that backs no live or pending note, and only from the key on record.
 */
export function renewSlot(
  storage: DurableObjectStorage,
  shortId: string,
  slot: number,
  r: Record<string, unknown>,
): {version: number} {
  if (!Number.isSafeInteger(slot) || slot < 0 || slot >= SLOTS) {
    throw new BadRequest('slot must be 0 to 7');
  }
  // Slot 0's key names the card: its short id is the key's last 4 bytes, so
  // a renewed slot 0 would leave every till unable to find the card.
  if (slot === 0) {
    throw new Conflict('slot 0’s key is the card’s short id and is never renewed');
  }
  if (!isHex(r.old_pubkey, 32) || !isHex(r.pubkey, 32)) {
    throw new BadRequest('old_pubkey and pubkey must be x-only keys, 64 lower-case hex');
  }
  const sql = storage.sql;
  return storage.transactionSync(() => {
    requireVersion(sql, shortId, r.version);
    const row = sql
      .exec('SELECT pubkey FROM card_slots WHERE short_id = ? AND slot = ?', shortId, slot)
      .toArray()[0] as {pubkey: string} | undefined;
    if (!row) {
      throw new NotFound(`card ${shortId} has no slot ${slot} on record`);
    }
    if (row.pubkey !== r.old_pubkey) {
      throw new Conflict(`slot ${slot} of card ${shortId} does not hold that key`);
    }
    const held = sql
      .exec(
        "SELECT nonce FROM notes WHERE short_id = ? AND slot = ? AND status IN ('live', 'pending')",
        shortId,
        slot,
      )
      .toArray();
    if (held.length > 0) {
      throw new Conflict(`slot ${slot} of card ${shortId} still backs a note`);
    }
    sql.exec(
      'UPDATE card_slots SET pubkey = ? WHERE short_id = ? AND slot = ?',
      r.pubkey,
      shortId,
      slot,
    );
    return {version: bump(sql, shortId)};
  });
}
