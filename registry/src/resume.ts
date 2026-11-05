/**
 * Finishing a card whose install tap was lifted after the card-manager
 * rotation, while its slot keys or its PIN were being written (OPS-PLAN O5).
 * The card is ours and on its own keys, with our applet and no PIN; the
 * phone re-keys all eight slots, sets a fresh PIN and posts the finished card
 * under its new short id.
 *
 *   POST /registry/resume-check {chip, short_id}   may the card on this chip, read as short_id, be finished?
 *   POST /registry/cards {..., supersedes}         the finished card; the old row closed in the same transaction
 *
 * The check is asked before anything is written on the card. `short_id` is
 * the one the card answers now (slot 0's key from the lifted tap); ops may
 * have no row for it at all, which is the usual case: the provisioner posts
 * only finished cards. The card may be finished only when ops holds nothing
 * under that id (no live or pending note, no open charge, no open load), any
 * row it has is this operator's, on this chip, with its PIN not set, and no
 * other open card is recorded on the chip.
 *
 * The post names the old id as `supersedes`. The same conditions are checked
 * again inside the post's transaction; then the old row, if ops has one, is
 * closed: `cards.replaced_by` (v6's column, unwritten until now) is set to the
 * new id, and the old id answers 410 to holder loads and till writes from
 * then on, as a reset id does (`index.ts`). A repeat of the post (an outbox
 * retry) finds the old row already closed by this new id and changes nothing.
 */
import {batchOwner, cardOwner, storedCard, type CardBody} from './cards';
import {BadRequest, Conflict, isShortId} from './http';
import {isChip} from './managerKeys';
import {holdingReasons, holdings, isEmpty, type CardHoldings} from './reset';

type Sql = SqlStorage;

export interface ResumeCheck extends CardHoldings {
  readonly short_id: string;
  readonly chip: string;
  /** Ops has a row under this short id. */
  readonly known: boolean;
  /** The card may be finished under a new short id. */
  readonly resumable: boolean;
  /** Why not, one line each; empty when resumable. */
  readonly reasons: readonly string[];
}

/**
 * Why the card on `chip`, answering `oldId`, may not be finished as `newId`
 * (null while only checking), one line each. Reads only.
 */
export function resumeReasons(
  sql: Sql,
  operatorId: number,
  chip: string,
  oldId: string,
  newId: string | null,
): {reasons: string[]; held: CardHoldings; known: boolean} {
  const reasons: string[] = [];
  const old = storedCard(sql, oldId);
  if (old) {
    if (cardOwner(sql, oldId) !== operatorId) {
      // Nothing about another operator's card is said beyond this.
      return {reasons: [`card ${oldId} is another operator’s`], held: zero(), known: true};
    }
    if (old.chip !== chip) {
      reasons.push(
        old.chip === null
          ? `ops recorded no chip for card ${oldId}`
          : `ops recorded card ${oldId} on another chip`,
      );
    }
    if (old.pin_set) {
      reasons.push(`ops records card ${oldId} with its PIN set`);
    }
    if (old.reset_at !== null) {
      reasons.push(`card ${oldId} was reset`);
    }
    if (old.replaced_by !== null && old.replaced_by !== newId) {
      reasons.push(`card ${oldId} was already finished as ${old.replaced_by}`);
    }
  }
  const held = holdings(sql, oldId);
  reasons.push(...holdingReasons(held));
  const escrow = sql
    .exec('SELECT operator_id FROM manager_keys WHERE chip = ?', chip)
    .toArray()[0] as {operator_id: number} | undefined;
  if (escrow && Number(escrow.operator_id) !== operatorId) {
    reasons.push('this chip’s manager keys belong to another operator');
  }
  // Another open card on the same chip: ops thinks the chip carries it, so
  // finishing this one would leave two live rows for one chip.
  const others = sql
    .exec(
      `SELECT short_id FROM cards
       WHERE chip = ? AND short_id != ? AND short_id != ? AND reset_at IS NULL AND replaced_by IS NULL`,
      chip,
      oldId,
      newId ?? oldId,
    )
    .toArray()
    .map(r => String(r.short_id));
  for (const id of others) {
    reasons.push(`ops records card ${id} on this chip`);
  }
  return {reasons, held, known: old !== null};
}

function zero(): CardHoldings {
  return {live_notes: 0, live_msat: 0, pending_notes: 0, pending_msat: 0, open_charges: 0, open_loads: 0};
}

/** `POST /registry/resume-check`. */
export function resumeCheck(sql: Sql, operatorId: number, r: Record<string, unknown>): ResumeCheck {
  if (!isChip(r.chip)) {
    throw new BadRequest('chip must be the card’s CPLC id, cplc-v1: and 24 lower-case hex characters');
  }
  if (!isShortId(r.short_id)) {
    throw new BadRequest('short_id must be 8 lower-case hex characters');
  }
  const {reasons, held, known} = resumeReasons(sql, operatorId, r.chip, r.short_id, null);
  return {
    short_id: r.short_id,
    chip: r.chip,
    known,
    ...held,
    resumable: reasons.length === 0 && isEmpty(held),
    reasons,
  };
}

/** The old short id a card row names, or null; checked against the row itself. */
export function parseSupersedes(r: Record<string, unknown>): string | null {
  const old = r.supersedes;
  if (old === undefined) {
    return null;
  }
  if (!isShortId(old)) {
    throw new BadRequest('supersedes must be the 8 lower-case hex short id the card answered before');
  }
  if (old === r.short_id) {
    throw new BadRequest('supersedes must be another short id than the card’s own');
  }
  return old;
}

/**
 * Inside `postCard`'s transaction, before the new row is written: refuses
 * unless the old id may be closed for this card, then closes it. Idempotent:
 * an old row already closed by this card stays as it is.
 */
export function closeSuperseded(sql: Sql, operatorId: number, card: CardBody, oldId: string): void {
  if (batchOwner(sql, card.batch_id) !== operatorId) {
    return; // postCard answers this as 404 itself
  }
  if (card.chip === null) {
    throw new BadRequest('a card that supersedes another must name its chip');
  }
  if (!card.pin_set) {
    throw new BadRequest('a card that supersedes another must have its PIN set');
  }
  const {reasons} = resumeReasons(sql, operatorId, card.chip, oldId, card.short_id);
  if (reasons.length > 0) {
    throw new Conflict(`card ${oldId} cannot be closed for ${card.short_id}: ${reasons.join('; ')}`);
  }
  sql.exec(
    'UPDATE cards SET replaced_by = ? WHERE short_id = ? AND replaced_by IS NULL',
    card.short_id,
    oldId,
  );
}

/** Why a short id takes nothing new, or null when it may. */
export function closedReason(sql: Sql, shortId: string): {reset: boolean; replacedBy: string | null} | null {
  const row = sql.exec('SELECT reset_at, replaced_by FROM cards WHERE short_id = ?', shortId).toArray()[0];
  if (!row) {
    return null;
  }
  const reset = row.reset_at !== null && row.reset_at !== undefined;
  const replacedBy = row.replaced_by === null || row.replaced_by === undefined ? null : String(row.replaced_by);
  return reset || replacedBy !== null ? {reset, replacedBy} : null;
}

/** Whether ops records the card's PIN as set; a card without one takes no value. */
export function pinIsSet(sql: Sql, shortId: string): boolean {
  const row = sql.exec('SELECT pin_set FROM cards WHERE short_id = ?', shortId).toArray()[0];
  return row !== undefined && Number(row.pin_set) === 1;
}
