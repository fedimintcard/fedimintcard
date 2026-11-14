/**
 * Resetting a card in the provisioner (OPS-PLAN O5): the operator's two calls
 * around the tap that takes the card back to its factory state.
 *
 *   GET  /registry/cards/:short_id/reset-check?chip=  what ops holds for the card, and whether it may go
 *   POST /registry/cards/:short_id/reset        the completion: the card is factory, proved
 *
 * **By chip.** The phone sends the chip's CPLC id with the short id it read
 * (`?chip=` on the check; the completion always named it). A card whose
 * install stopped after the PIN was set, with the phone no longer holding
 * that PIN (`needsReset`), answers a short id ops may have no row for, or
 * only a stopped one (PIN not set). Such a card is found by its chip: it is
 * this operator's when the chip's `manager_keys` escrow is, and another
 * operator's chip answers 404 as an unknown short id does. It may be reset
 * **without the holder's PIN** (`needs_holder_pin: false`) only when all of:
 * the escrow is the caller's; no open row on the chip (nor the short id's
 * own) is a finished card with its PIN set; and ops holds nothing for any
 * open row on the chip. That card never reached a holder, so nobody has a
 * PIN to consent with. Every other card keeps the rules below.
 *
 * The check refuses while ops shows value or work in flight on the card: a
 * live or pending note, an open charge, an open holder load. Deleting the
 * applet deletes the slot keys, so a note under them could never be spent
 * again. The holder's PIN, which the phone asks for on the card itself, is
 * the consent for what ops cannot see (a load made around it); this route
 * never sees the PIN.
 *
 * The completion comes after the phone has proved, on the card, that the
 * Fedimint package is gone and that the card manager answers a full login
 * under the chip maker's keys: `{chip, short_id, gp_key_version,
 * manager_keys: 'factory', package: 'absent', verified_at}`. One transaction:
 *
 * - the old row's `reset_at` (its short id answers 410 to holder routes and
 *   till writes after, `index.ts`), and the same for every other open row of
 *   this operator's on the chip whose PIN was never set (a stopped install);
 *   ops may have no row for the old short id at all;
 * - the chip's escrow row in `manager_keys` is **deleted**: the card carries
 *   the factory keys now, so no card holds these bytes and there is nothing
 *   left to escrow. Whoever installs the chip next (any operator) asks
 *   `/registry/manager-keys` as for a new chip and becomes its owner; the
 *   derivation is deterministic, so the same master gives the same bytes. A
 *   "released" flag would keep a key set no card carries in every backup, for
 *   no reader: the completion row below is the audit trail;
 * - a `card_resets` row: who, when, the chip, the old short id and the proof.
 *
 * Only the operator who owns the card may complete it (another gets 404, as
 * the card routes answer), and only while the chip's escrow, if any, is that
 * operator's and not linked to another live card (409). It does not refuse on
 * value: by the time the phone calls it the card is already blank, and a
 * refusal would only lose the record. Whatever ops still held under the old
 * short id is answered as `stranded`, for the phone to show. A repeat of a
 * completion answers the stored one and changes nothing, so an outbox retry
 * can never release an escrow the chip took since (a new install).
 */
import {cardOwner, storedCard} from './cards';
import {BadRequest, Conflict, isPositiveInt, NotFound} from './http';
import {isChip} from './managerKeys';

type Sql = SqlStorage;

export interface CardHoldings {
  readonly live_notes: number;
  readonly live_msat: number;
  readonly pending_notes: number;
  readonly pending_msat: number;
  readonly open_charges: number;
  readonly open_loads: number;
}

export interface ResetCheck extends CardHoldings {
  readonly short_id: string;
  /** The chip the phone read, else the one ops recorded; null for a card from before O5. */
  readonly chip: string | null;
  /** Ops has a row under this short id. */
  readonly known: boolean;
  /**
   * The holder's PIN (or the blocked-PIN path) is the consent. False only for
   * a card that never reached a holder: see "By chip" above.
   */
  readonly needs_holder_pin: boolean;
  /** Nothing ops can see on the card: no note live or pending, nothing open. */
  readonly empty: boolean;
  /** Empty, and not already reset. */
  readonly resettable: boolean;
  /** Why not, one line each; empty when resettable. */
  readonly reasons: readonly string[];
  readonly reset_at: number | null;
}

export function holdings(sql: Sql, shortId: string): CardHoldings {
  const notes = (status: string) =>
    sql
      .exec(
        'SELECT COUNT(*) AS n, COALESCE(SUM(amount_msat), 0) AS msat FROM notes WHERE short_id = ? AND status = ?',
        shortId,
        status,
      )
      .one() as {n: number; msat: number};
  const count = (query: string) => Number((sql.exec(query, shortId).one() as {n: number}).n);
  const live = notes('live');
  const pending = notes('pending');
  return {
    live_notes: Number(live.n),
    live_msat: Number(live.msat),
    pending_notes: Number(pending.n),
    pending_msat: Number(pending.msat),
    open_charges: count("SELECT COUNT(*) AS n FROM card_txs WHERE short_id = ? AND status = 'open'"),
    open_loads: count("SELECT COUNT(*) AS n FROM holder_loads WHERE short_id = ? AND status = 'open'"),
  };
}

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

interface ChipRow {
  readonly short_id: string;
  readonly pin_set: boolean;
  readonly operator_id: number | null;
}

/** The open rows (neither reset nor replaced) ops records on a chip, other than `shortId`. */
function openOnChip(sql: Sql, chip: string, shortId: string): ChipRow[] {
  return sql
    .exec(
      `SELECT c.short_id, c.pin_set, b.operator_id FROM cards c LEFT JOIN batches b ON b.batch_id = c.batch_id
       WHERE c.chip = ? AND c.short_id != ? AND c.reset_at IS NULL AND c.replaced_by IS NULL`,
      chip,
      shortId,
    )
    .toArray()
    .map(r => ({
      short_id: String(r.short_id),
      pin_set: Number(r.pin_set) === 1,
      operator_id: r.operator_id === null ? null : Number(r.operator_id),
    }));
}

/** The operator whose escrow the chip is under, or null. */
export function escrowOwner(sql: Sql, chip: string): number | null {
  const row = sql.exec('SELECT operator_id FROM manager_keys WHERE chip = ?', chip).toArray()[0];
  return row ? Number(row.operator_id) : null;
}

/** The operator who completed a reset of this short id, or null. */
export function resetBy(sql: Sql, shortId: string): number | null {
  const row = sql.exec('SELECT operator_id FROM card_resets WHERE short_id = ?', shortId).toArray()[0];
  return row ? Number(row.operator_id) : null;
}

/**
 * Who a reset route answers for: the short id's owner when ops has its row,
 * else whoever completed its reset (a repeat), else the chip's escrow owner.
 */
export function resetOwner(sql: Sql, shortId: string, chip: string | null): number | null {
  const owner = cardOwner(sql, shortId) ?? resetBy(sql, shortId);
  if (owner !== null) {
    return owner;
  }
  return chip === null ? null : escrowOwner(sql, chip);
}

function sumHoldings(all: readonly CardHoldings[]): CardHoldings {
  return all.reduce<CardHoldings>(
    (a, h) => ({
      live_notes: a.live_notes + h.live_notes,
      live_msat: a.live_msat + h.live_msat,
      pending_notes: a.pending_notes + h.pending_notes,
      pending_msat: a.pending_msat + h.pending_msat,
      open_charges: a.open_charges + h.open_charges,
      open_loads: a.open_loads + h.open_loads,
    }),
    {live_notes: 0, live_msat: 0, pending_notes: 0, pending_msat: 0, open_charges: 0, open_loads: 0},
  );
}

/**
 * The check, for a card the caller has already found to be this operator's
 * (`resetOwner`). `chip` is the one the phone read, or null from a phone
 * that sent none: then only the short id's row is looked at, as before.
 */
export function resetCheck(sql: Sql, operatorId: number, shortId: string, chip: string | null = null): ResetCheck {
  const card = storedCard(sql, shortId);
  if (!card && chip === null) {
    throw new NotFound(`no card ${shortId}`);
  }
  const own = holdings(sql, shortId);
  const reasons: string[] = [];
  if (card && card.reset_at !== null) {
    reasons.push('already reset: the card is blank, on its factory keys');
  }
  if (card && card.replaced_by !== null) {
    reasons.push(`closed: the card was finished as ${card.replaced_by}; reset that id instead`);
  }
  if (chip !== null && card && card.chip !== null && card.chip !== chip) {
    reasons.push(`ops recorded card ${shortId} on another chip`);
  }
  reasons.push(...holdingReasons(own));
  let held = own;
  let pinFree = false;
  if (chip !== null) {
    const others = chip === card?.chip || !card || card.chip === null ? openOnChip(sql, chip, shortId) : [];
    for (const row of others) {
      if (row.pin_set) {
        // Ops thinks the chip carries this finished card: resetting would close it.
        reasons.push(`ops records card ${row.short_id} on this chip`);
      }
      const theirs = holdings(sql, row.short_id);
      reasons.push(...holdingReasons(theirs).map(r => `card ${row.short_id}: ${r}`));
      held = sumHoldings([held, theirs]);
    }
    pinFree =
      escrowOwner(sql, chip) === operatorId &&
      (card === null || (card.chip === chip && !card.pin_set)) &&
      reasons.length === 0 &&
      isEmpty(held);
    if (!card && !pinFree && reasons.length === 0) {
      reasons.push(`ops has no record of card ${shortId}, so it has no holder PIN to check`);
    }
  }
  const empty = isEmpty(held);
  return {
    short_id: shortId,
    chip: chip ?? card?.chip ?? null,
    known: card !== null,
    needs_holder_pin: !pinFree,
    ...held,
    empty,
    resettable: empty && reasons.length === 0,
    reasons,
    reset_at: card?.reset_at ?? null,
  };
}

/** Nothing ops can see under the short id: no note live or pending, nothing open. */
export function isEmpty(held: CardHoldings): boolean {
  return held.live_notes === 0 && held.pending_notes === 0 && held.open_charges === 0 && held.open_loads === 0;
}

/** What ops holds under a short id, one line each; empty when it holds nothing. */
export function holdingReasons(held: CardHoldings): string[] {
  const reasons: string[] = [];
  if (held.live_notes > 0) {
    reasons.push(`${plural(held.live_notes, 'live note', 'live notes')}, ${held.live_msat} msat`);
  }
  if (held.pending_notes > 0) {
    reasons.push(`${plural(held.pending_notes, 'pending note', 'pending notes')}, ${held.pending_msat} msat`);
  }
  if (held.open_charges > 0) {
    reasons.push(`${plural(held.open_charges, 'open charge', 'open charges')} at a till`);
  }
  if (held.open_loads > 0) {
    reasons.push(`${plural(held.open_loads, 'open load', 'open loads')} from a holder’s wallet`);
  }
  return reasons;
}

/** Whether the card was reset: its short id takes no more value. */
export function wasReset(sql: Sql, shortId: string): boolean {
  const row = sql.exec('SELECT reset_at FROM cards WHERE short_id = ?', shortId).toArray()[0];
  return row !== undefined && row.reset_at !== null;
}

/** The proof a reset ends on, as the phone saw it on the card. */
export interface ResetCompletion {
  /** The old card's short id: the one the reset took off the chip. */
  readonly short_id: string;
  readonly chip: string;
  readonly operator_id: number;
  /** The key set version the factory login reported (01 on a J3R180). */
  readonly gp_key_version: number;
  readonly manager_keys: 'factory';
  readonly package: 'absent';
  /** When the phone proved it, epoch ms. */
  readonly verified_at: number;
  /** When ops recorded it. */
  readonly completed_at: number;
}

function parseCompletion(shortId: string, r: Record<string, unknown>) {
  if (r.short_id !== shortId) {
    throw new BadRequest('short_id must be the card being reset, as in the path');
  }
  if (!isChip(r.chip)) {
    throw new BadRequest('chip must be the card’s CPLC id, cplc-v1: and 24 lower-case hex characters');
  }
  const kvn = r.gp_key_version;
  if (!Number.isInteger(kvn) || (kvn as number) < 1 || (kvn as number) > 0x7f) {
    throw new BadRequest('gp_key_version must be the factory key set version the card reported, 1 to 127');
  }
  if (r.manager_keys !== 'factory') {
    throw new BadRequest("manager_keys must be 'factory': a reset ends on the chip maker's keys");
  }
  if (r.package !== 'absent') {
    throw new BadRequest("package must be 'absent': a reset ends with the Fedimint package deleted");
  }
  if (!isPositiveInt(r.verified_at)) {
    throw new BadRequest('verified_at must be epoch milliseconds');
  }
  return {chip: r.chip, gp_key_version: kvn as number, verified_at: r.verified_at};
}

function storedCompletion(sql: Sql, shortId: string): ResetCompletion | null {
  const row = sql.exec('SELECT * FROM card_resets WHERE short_id = ?', shortId).toArray()[0];
  if (!row) {
    return null;
  }
  return {
    short_id: String(row.short_id),
    chip: String(row.chip),
    operator_id: Number(row.operator_id),
    gp_key_version: Number(row.gp_key_version),
    manager_keys: 'factory',
    package: 'absent',
    verified_at: Number(row.verified_at),
    completed_at: Number(row.completed_at),
  };
}

/**
 * `POST /registry/cards/:short_id/reset`, for a card the caller has already
 * found to be this operator's. Idempotent: the same chip again answers what
 * was stored and changes nothing; another chip for a card already reset is a
 * Conflict.
 */
export function completeReset(
  storage: DurableObjectStorage,
  operatorId: number,
  shortId: string,
  r: Record<string, unknown>,
  now: number,
): {reset: ResetCompletion; stranded: CardHoldings; released: boolean} {
  const sql = storage.sql;
  const done = parseCompletion(shortId, r);
  return storage.transactionSync(() => {
    const old = storedCard(sql, shortId);
    if (old && old.chip !== null && old.chip !== done.chip) {
      throw new Conflict(`card ${shortId} was installed on another chip`);
    }
    const before = storedCompletion(sql, shortId);
    if (before) {
      if (before.chip !== done.chip) {
        throw new Conflict(`card ${shortId} was already reset on another chip`);
      }
      return {reset: before, stranded: holdings(sql, shortId), released: false};
    }
    const escrow = sql
      .exec('SELECT operator_id, short_id FROM manager_keys WHERE chip = ?', done.chip)
      .toArray()[0] as {operator_id: number; short_id: string | null} | undefined;
    if (!old && (!escrow || Number(escrow.operator_id) !== operatorId)) {
      // Known by its chip only, and the chip is not this operator's (any more).
      throw new NotFound(`no card ${shortId}`);
    }
    // Every other open row on the chip: a finished one means the chip carries
    // that card now; this operator's stopped ones (PIN never set) close here.
    const others = openOnChip(sql, done.chip, shortId);
    const finished = others.find(row => row.pin_set);
    if (finished) {
      throw new Conflict(`this chip carries card ${finished.short_id} now, not ${shortId}`);
    }
    const closing = others.filter(row => row.operator_id === operatorId).map(row => row.short_id);
    if (escrow) {
      if (Number(escrow.operator_id) !== operatorId) {
        throw new Conflict('this chip’s manager keys belong to another operator');
      }
      const linked = escrow.short_id === null ? null : String(escrow.short_id);
      if (linked !== null && linked !== shortId && !wasReset(sql, linked) && !closing.includes(linked)) {
        throw new Conflict(`this chip carries card ${linked} now, not ${shortId}`);
      }
      sql.exec('DELETE FROM manager_keys WHERE chip = ?', done.chip);
    }
    if (old && old.reset_at === null) {
      sql.exec('UPDATE cards SET reset_at = ? WHERE short_id = ?', now, shortId);
    }
    for (const id of closing) {
      sql.exec('UPDATE cards SET reset_at = ? WHERE short_id = ? AND reset_at IS NULL', now, id);
    }
    // The applet and its slots are gone: no release can fit them again, and nothing is held on them.
    for (const id of [shortId, ...closing]) {
      sql.exec('DELETE FROM releases WHERE short_id = ?', id);
      sql.exec('DELETE FROM strandings WHERE short_id = ?', id);
    }
    const record: ResetCompletion = {
      short_id: shortId,
      chip: done.chip,
      operator_id: operatorId,
      gp_key_version: done.gp_key_version,
      manager_keys: 'factory',
      package: 'absent',
      verified_at: done.verified_at,
      completed_at: now,
    };
    sql.exec(
      `INSERT INTO card_resets
         (short_id, chip, operator_id, gp_key_version, manager_keys, package, verified_at, completed_at)
       VALUES (?, ?, ?, ?, 'factory', 'absent', ?, ?)`,
      shortId,
      done.chip,
      operatorId,
      done.gp_key_version,
      done.verified_at,
      now,
    );
    const stranded = sumHoldings([shortId, ...closing].map(id => holdings(sql, id)));
    return {reset: record, stranded, released: escrow !== undefined};
  });
}
