/**
 * The federation's mint keys, and the note check ops runs with them (OPS-PLAN § 6, decided in O4:
 * ops verifies each note's signature before it is live).
 *
 * A till reads the mint module's aggregate public key per note tier from the federation's client
 * config (`MintClientConfig::tbs_pks`) and posts them with the federation's name
 * (`POST /federations`). They are kept per operator and federation, first write wins: a
 * federation's keys never change, so a different set for the same federation is a 409, and a
 * till token cannot swap them after the fact. A card's notes are checked against the keys of
 * its batch's federation, under its operator.
 */
import {BadRequest, Conflict, isHex, Unprocessable} from './http';
import {parsePublicKey, verifyNotes, type NoteCheck} from './tbs';

type Sql = SqlStorage;

/** A federation has one tier per power of two in practice; this bounds the body, not the config. */
const MAX_TIERS = 128;

/**
 * Validates `{"<amount_msat>": "<96-byte G2 key, hex>"}` and answers it canonically (tiers in
 * numeric order). Amounts stay strings: the top tiers are past 2^53.
 */
function parseTiers(raw: unknown): string {
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) {
    throw new BadRequest('tbs_pks must map each tier (msat, a decimal string) to its key');
  }
  const entries = Object.entries(raw as Record<string, unknown>);
  if (entries.length === 0 || entries.length > MAX_TIERS) {
    throw new BadRequest(`tbs_pks must have 1 to ${MAX_TIERS} tiers`);
  }
  for (const [amount, pk] of entries) {
    if (!/^[1-9][0-9]{0,19}$/.test(amount) || BigInt(amount) >= 1n << 64n) {
      throw new BadRequest(`tier ${amount} is not an amount in msat`);
    }
    if (!isHex(pk, 96)) {
      throw new BadRequest(`the key of tier ${amount} must be 96 bytes, hex`);
    }
  }
  entries.sort(([a], [b]) => (BigInt(a) < BigInt(b) ? -1 : BigInt(a) > BigInt(b) ? 1 : 0));
  return JSON.stringify(Object.fromEntries(entries));
}

/**
 * Stores a federation's tier keys for an operator. The same set again answers `false` (nothing
 * new); a different set is a Conflict. Each key is checked to be a G2 point only on first write.
 */
export function putMintKeys(
  sql: Sql,
  operatorId: number,
  federationId: string,
  raw: unknown,
  now: number,
): boolean {
  const tiers = parseTiers(raw);
  const prior = sql
    .exec('SELECT tbs_pks FROM federation_keys WHERE operator_id = ? AND federation_id = ?', operatorId, federationId)
    .toArray()[0] as {tbs_pks: string} | undefined;
  if (prior) {
    if (prior.tbs_pks === tiers) {
      return false;
    }
    throw new Conflict(`federation ${federationId} already has other mint keys on record`);
  }
  for (const [amount, pk] of Object.entries(JSON.parse(tiers) as Record<string, string>)) {
    if (!parsePublicKey(pk)) {
      throw new BadRequest(`the key of tier ${amount} is not a point of G2`);
    }
  }
  sql.exec(
    'INSERT INTO federation_keys (operator_id, federation_id, tbs_pks, updated_at) VALUES (?, ?, ?, ?)',
    operatorId,
    federationId,
    tiers,
    now,
  );
  return true;
}

/** The card's federation and operator, through its batch; null for no such card. */
export function cardFederation(sql: Sql, shortId: string): {operator_id: number; federation_id: string} | null {
  const row = sql
    .exec(
      'SELECT b.operator_id, b.federation_id FROM cards c JOIN batches b ON b.batch_id = c.batch_id WHERE c.short_id = ?',
      shortId,
    )
    .toArray()[0] as {operator_id: number; federation_id: string} | undefined;
  return row ? {operator_id: Number(row.operator_id), federation_id: String(row.federation_id)} : null;
}

/** The tier keys of the card's federation; an Unprocessable when no till has reported them. */
export function mintKeysFor(sql: Sql, shortId: string): Map<string, string> {
  const fed = cardFederation(sql, shortId);
  const row = fed
    ? (sql
        .exec('SELECT tbs_pks FROM federation_keys WHERE operator_id = ? AND federation_id = ?', fed.operator_id, fed.federation_id)
        .toArray()[0] as {tbs_pks: string} | undefined)
    : undefined;
  if (!row) {
    throw new Unprocessable(
      `the mint keys of card ${shortId}'s federation are not on record yet; a till of its operator reports them at startup`,
    );
  }
  return new Map(Object.entries(JSON.parse(row.tbs_pks) as Record<string, string>));
}

/** Whether the card's federation has a note tier of `amountMsat`. */
export function isTier(keys: Map<string, string>, amountMsat: number): boolean {
  return keys.has(String(amountMsat));
}

/**
 * Throws Unprocessable unless every note's signature is the federation's, under the key of the
 * note's own tier. Nothing is written by the caller when this throws.
 */
export function checkNotes(
  sql: Sql,
  shortId: string,
  notes: readonly {nonce: string; amount_msat: number; signature: string}[],
): void {
  if (notes.length === 0) {
    return;
  }
  const keys = mintKeysFor(sql, shortId);
  const checks: NoteCheck[] = notes.map(n => {
    const hex = keys.get(String(n.amount_msat));
    const pk = hex === undefined ? null : parsePublicKey(hex);
    if (!pk) {
      throw new Unprocessable(`${n.amount_msat} msat is not a note tier of card ${shortId}'s federation`);
    }
    return {nonce: n.nonce, signature: n.signature, pk};
  });
  if (!verifyNotes(checks)) {
    throw new Unprocessable('a note signature does not verify under the federation’s key for its amount; nothing was recorded');
  }
}
