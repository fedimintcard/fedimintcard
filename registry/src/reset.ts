/**
 * Resetting a card in the provisioner (OPS-PLAN O5): the operator's two calls
 * around the tap that takes the card back to its factory state.
 *
 *   GET  /registry/cards/:short_id/reset-check  what ops holds for the card, and whether it may go
 *   POST /registry/cards/:short_id/reset        the completion: the card is factory, proved
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
 *   till writes after, `index.ts`);
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
import {storedCard} from './cards';
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
  /** The chip ops recorded for the card; null for a card from before O5. */
  readonly chip: string | null;
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

/** The check, for a card the caller has already found to be this operator's. */
export function resetCheck(sql: Sql, shortId: string): ResetCheck {
  const card = storedCard(sql, shortId);
  if (!card) {
    throw new NotFound(`no card ${shortId}`);
  }
  const held = holdings(sql, shortId);
  const reasons: string[] = [];
  if (card.reset_at !== null) {
    reasons.push('already reset: the card is blank, on its factory keys');
  }
  if (card.replaced_by !== null) {
    reasons.push(`closed: the card was finished as ${card.replaced_by}; reset that id instead`);
  }
  reasons.push(...holdingReasons(held));
  const empty = isEmpty(held);
  return {
    short_id: shortId,
    chip: card.chip,
    ...held,
    empty,
    resettable: empty && card.reset_at === null && card.replaced_by === null,
    reasons,
    reset_at: card.reset_at,
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
    if (!old) {
      throw new NotFound(`no card ${shortId}`);
    }
    if (old.chip !== null && old.chip !== done.chip) {
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
    if (escrow) {
      if (Number(escrow.operator_id) !== operatorId) {
        throw new Conflict('this chip’s manager keys belong to another operator');
      }
      const linked = escrow.short_id === null ? null : String(escrow.short_id);
      if (linked !== null && linked !== shortId && !wasReset(sql, linked)) {
        throw new Conflict(`this chip carries card ${linked} now, not ${shortId}`);
      }
      sql.exec('DELETE FROM manager_keys WHERE chip = ?', done.chip);
    }
    if (old.reset_at === null) {
      sql.exec('UPDATE cards SET reset_at = ? WHERE short_id = ?', now, shortId);
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
    return {reset: record, stranded: holdings(sql, shortId), released: escrow !== undefined};
  });
}
