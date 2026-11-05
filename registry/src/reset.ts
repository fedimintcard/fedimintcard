/**
 * Resetting a card in the provisioner (OPS-PLAN O5): the operator's two calls
 * around the tap that deletes the card's applet and installs a fresh one.
 *
 *   GET  /registry/cards/:short_id/reset-check  what ops holds for the card, and whether it may go
 *   POST /registry/cards/:short_id/reset        the new card's row; the old one is marked reset
 *
 * The check refuses while ops shows value or work in flight on the card: a
 * live or pending note, an open charge, an open holder load. Deleting the
 * applet deletes the slot keys, so a note under them could never be spent
 * again. The holder's PIN, which the phone asks for on the card itself, is
 * the consent for what ops cannot see (a load made around it); this route
 * never sees the PIN.
 *
 * Recording is one transaction: the new card's row (in the old card's batch,
 * since a reset is not part of a batch the phone has open), the old row's
 * `reset_at` and `replaced_by`, and the chip's escrow row pointed at the new
 * short id. It does not refuse on value: by the time the phone calls it, the
 * card has already been wiped, and a refusal would only lose the new card's
 * record. Whatever ops still held under the old short id is answered as
 * `stranded`, for the phone to show.
 *
 * After a reset the old short id takes nothing new: holder routes and till
 * writes answer 410 (`index.ts`).
 */
import {insertCard, parseCard, storedCard, type StoredCard} from './cards';
import {BadRequest, Conflict, NotFound} from './http';
import {linkChip} from './managerKeys';

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
  readonly replaced_by: string | null;
}

function holdings(sql: Sql, shortId: string): CardHoldings {
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
    reasons.push(`already reset; the card is ${card.replaced_by} now`);
  }
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
  const empty =
    held.live_notes === 0 && held.pending_notes === 0 && held.open_charges === 0 && held.open_loads === 0;
  return {
    short_id: shortId,
    chip: card.chip,
    ...held,
    empty,
    resettable: empty && card.reset_at === null,
    reasons,
    reset_at: card.reset_at,
    replaced_by: card.replaced_by,
  };
}

/** Whether the card was reset: its short id takes no more value. */
export function wasReset(sql: Sql, shortId: string): boolean {
  const row = sql.exec('SELECT reset_at FROM cards WHERE short_id = ?', shortId).toArray()[0];
  return row !== undefined && row.reset_at !== null;
}

/**
 * The record of a finished reset. Idempotent: the same new row again answers
 * what was stored; a different new card for an old one already reset is a
 * Conflict.
 */
export function recordReset(
  storage: DurableObjectStorage,
  operatorId: number,
  oldShortId: string,
  r: Record<string, unknown>,
  now: number,
): {old_short_id: string; card: StoredCard; reset_at: number; stranded: CardHoldings} {
  const sql = storage.sql;
  const old = storedCard(sql, oldShortId);
  if (!old) {
    throw new NotFound(`no card ${oldShortId}`);
  }
  // The batch is the old card's: the phone's reset is not part of a batch.
  const card = parseCard({...r, batch_id: old.batch_id});
  if (card.slot_pubkeys.length === 0 || !card.pin_set) {
    throw new BadRequest('a reset card is recorded finished: its eight slot keys, and its PIN set');
  }
  if (card.short_id === oldShortId) {
    throw new BadRequest('the new card must have a new short id');
  }
  if (card.chip !== null && old.chip !== null && card.chip !== old.chip) {
    throw new Conflict(`card ${oldShortId} was on another chip`);
  }
  return storage.transactionSync(() => {
    if (old.replaced_by !== null && old.replaced_by !== card.short_id) {
      throw new Conflict(`card ${oldShortId} was already reset to ${old.replaced_by}`);
    }
    const stored = insertCard(sql, operatorId, card, now);
    const resetAt = old.reset_at ?? now;
    if (old.reset_at === null) {
      sql.exec('UPDATE cards SET reset_at = ?, replaced_by = ? WHERE short_id = ?', now, card.short_id, oldShortId);
    }
    if (card.chip !== null) {
      linkChip(sql, card.chip, card.short_id, operatorId);
    }
    return {old_short_id: oldShortId, card: stored, reset_at: resetAt, stranded: holdings(sql, oldShortId)};
  });
}
