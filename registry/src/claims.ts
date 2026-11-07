/**
 * Till claims (docs/TILL-CLAIM.md): a till asks for its own till token and an operator approves
 * it from the provisioner app, so no operator password is typed on the till's server.
 *
 * - `POST /till-claims` (no auth) starts a claim: a name, a code to show, and a poll secret the
 *   registry keeps only as SHA-256.
 * - The operator reads it by code and approves it; the claim is then tied to that operator.
 * - The till polls with its secret; the approved poll creates the till exactly as `POST /tills`
 *   does and deletes the claim in the same transaction, so the token is answered once and the
 *   registry never holds it in plain text.
 *
 * A pending claim lives 10 minutes. Approval extends it to 7 days from the approval, so a till
 * that is off when the operator approves (a StartOS box restarting, a laptop asleep) still gets
 * its token when it next polls. The token itself is minted only at that poll, so the registry
 * holds nothing usable meanwhile. Claims are swept on every claim route. They are not in the
 * backup (`backup.ts` lists the tables it copies): a claim is useless after a restore.
 */
import {insertTill, newTillToken, RateLimited} from './auth';
import {BadRequest, Conflict, constantTimeEqual, fromHex, isHex, NotFound, randomHex, sha256Hex} from './http';

export const CLAIM_TTL_MS = 10 * 60 * 1000;
/** How long an approved claim waits for its till's poll, from the approval. */
export const APPROVED_CLAIM_TTL_MS = 7 * 24 * 60 * 60 * 1000;
/** Claims one client IP may start per window. */
export const CLAIMS_PER_IP = 10;
export const CLAIM_IP_WINDOW_MS = 60 * 60 * 1000;
/** Pending claims across the registry; past it every start answers 429. */
export const MAX_LIVE_CLAIMS = 200;

/** No 0/O/1/I/L: a code is read off a screen and may be typed. */
const ALPHABET = '23456789ABCDEFGHJKMNPQRSTUVWXYZ';
const CODE_LEN = 8;

type Sql = SqlStorage;

/** A code as typed or scanned (`k7qm3xrp`, `K7QM-3XRP`) in its stored form, or null. */
export function normalizeCode(raw: string): string | null {
  const code = raw.replace(/-/g, '').toUpperCase();
  if (code.length !== CODE_LEN) return null;
  for (const ch of code) {
    if (!ALPHABET.includes(ch)) return null;
  }
  return code;
}

/** `K7QM3XRP` → `K7QM-3XRP`, as shown and as the QR text carries it. */
export function formatCode(code: string): string {
  return `${code.slice(0, 4)}-${code.slice(4)}`;
}

function randomCode(): string {
  let out = '';
  // Rejection sampling, so every character is equally likely.
  const limit = 256 - (256 % ALPHABET.length);
  while (out.length < CODE_LEN) {
    for (const b of crypto.getRandomValues(new Uint8Array(16))) {
      if (b < limit && out.length < CODE_LEN) out += ALPHABET[b % ALPHABET.length];
    }
  }
  return out;
}

/** The till's name as the operator will see it: 1 to 64 characters, none of them control or format characters. */
export function claimName(value: unknown): string {
  const name = typeof value === 'string' ? value.trim() : '';
  if (name === '' || name.length > 64 || /\p{C}/u.test(name)) {
    throw new BadRequest('a till needs a name of 1 to 64 printable characters');
  }
  return name;
}

/** Drops expired claims and spent IP windows. */
export function sweepClaims(sql: Sql, now: number): void {
  sql.exec('DELETE FROM till_claims WHERE expires_at <= ?', now);
  sql.exec('DELETE FROM till_claim_budget WHERE window_start <= ?', now - CLAIM_IP_WINDOW_MS);
}

/**
 * `POST /till-claims`. `ip` is the caller's address (CF-Connecting-IP); only its hash is kept,
 * and only for the rate-limit window. A refused start still spends the IP's budget.
 */
export async function startClaim(
  storage: DurableObjectStorage,
  ip: string,
  body: Record<string, unknown> | null,
  now: number,
): Promise<{claim_id: string; code: string; poll_secret: string; expires_at: number}> {
  const name = claimName(body?.name);
  const ipHash = await sha256Hex(`till-claim-ip:${ip}`);
  const claimId = randomHex(16);
  const pollSecret = randomHex(32);
  const secretHash = await sha256Hex(pollSecret);
  const expiresAt = now + CLAIM_TTL_MS;
  const code = storage.transactionSync(() => {
    const sql = storage.sql;
    sweepClaims(sql, now);
    const row = sql
      .exec('SELECT window_start, claims FROM till_claim_budget WHERE ip_hash = ?', ipHash)
      .toArray()[0] as {window_start: number; claims: number} | undefined;
    const fresh = !row || now - Number(row.window_start) >= CLAIM_IP_WINDOW_MS;
    if (!fresh && Number(row.claims) >= CLAIMS_PER_IP) {
      throw new RateLimited('too many till claims from this address; try again within the hour');
    }
    sql.exec(
      `INSERT INTO till_claim_budget (ip_hash, window_start, claims) VALUES (?, ?, ?)
       ON CONFLICT(ip_hash) DO UPDATE SET window_start = excluded.window_start, claims = excluded.claims`,
      ipHash,
      fresh ? now : Number(row.window_start),
      fresh ? 1 : Number(row.claims) + 1,
    );
    const live = sql.exec('SELECT COUNT(*) AS n FROM till_claims WHERE approved_by IS NULL').one() as {n: number};
    if (Number(live.n) >= MAX_LIVE_CLAIMS) {
      throw new RateLimited('too many till claims are waiting; try again in a few minutes');
    }
    // Unique among live claims (expired ones were just swept); a collision draws again.
    for (;;) {
      const c = randomCode();
      if (sql.exec('SELECT 1 FROM till_claims WHERE code = ?', c).toArray().length > 0) continue;
      sql.exec(
        `INSERT INTO till_claims (claim_id, code, name, poll_secret_hash, created_at, expires_at, approved_by, approved_at)
         VALUES (?, ?, ?, ?, ?, ?, NULL, NULL)`,
        claimId, c, name, secretHash, now, expiresAt,
      );
      return c;
    }
  });
  return {claim_id: claimId, code: formatCode(code), poll_secret: pollSecret, expires_at: expiresAt};
}

interface ClaimRow {
  claim_id: string;
  code: string;
  name: string;
  poll_secret_hash: string;
  created_at: number;
  expires_at: number;
  approved_by: number | null;
}

function byCode(sql: Sql, rawCode: string, now: number): ClaimRow | null {
  const code = normalizeCode(rawCode);
  if (code === null) return null;
  sweepClaims(sql, now);
  return (sql.exec('SELECT * FROM till_claims WHERE code = ?', code).toArray()[0] as ClaimRow | undefined) ?? null;
}

/**
 * `GET /till-claims/:code`. A claim another operator approved answers as if there were none,
 * as other operators' cards do.
 */
export function readClaim(sql: Sql, operatorId: number, rawCode: string, now: number) {
  const c = byCode(sql, rawCode, now);
  if (!c || (c.approved_by !== null && Number(c.approved_by) !== operatorId)) throw new NotFound('no claim');
  return {
    name: String(c.name),
    created_at: Number(c.created_at),
    expires_at: Number(c.expires_at),
    status: c.approved_by === null ? 'pending' : 'approved',
  };
}

/**
 * `POST /till-claims/:code/approve`: ties a pending claim to this operator and keeps it for
 * [`APPROVED_CLAIM_TTL_MS`] from now, for a till that is not running to poll later.
 */
export function approveClaim(storage: DurableObjectStorage, operatorId: number, rawCode: string, now: number): void {
  storage.transactionSync(() => {
    const c = byCode(storage.sql, rawCode, now);
    if (!c) throw new NotFound('no claim');
    if (c.approved_by !== null) {
      if (Number(c.approved_by) !== operatorId) throw new NotFound('no claim');
      throw new Conflict('this claim is already approved');
    }
    storage.sql.exec(
      'UPDATE till_claims SET approved_by = ?, approved_at = ?, expires_at = ? WHERE claim_id = ?',
      operatorId, now, now + APPROVED_CLAIM_TTL_MS, c.claim_id,
    );
  });
}

/**
 * `POST /till-claims/:claim_id/poll`. Unknown, expired and a wrong secret all answer 404 alike.
 * Approved: the till is made and the claim deleted in one transaction, so a second poll (or two
 * at once) finds nothing.
 */
export async function pollClaim(
  storage: DurableObjectStorage,
  claimId: string,
  body: Record<string, unknown> | null,
  now: number,
): Promise<{status: 'pending'} | {status: 'approved'; till_id: number; name: string; token: string}> {
  const secret = body?.poll_secret;
  if (!isHex(claimId, 16) || !isHex(secret, 32)) throw new NotFound('no claim');
  const presented = fromHex(await sha256Hex(secret));
  const matches = (): ClaimRow | null => {
    sweepClaims(storage.sql, now);
    const c = storage.sql.exec('SELECT * FROM till_claims WHERE claim_id = ?', claimId).toArray()[0] as
      | ClaimRow
      | undefined;
    return c && constantTimeEqual(fromHex(String(c.poll_secret_hash)), presented) ? c : null;
  };
  const seen = matches();
  if (!seen) throw new NotFound('no claim');
  if (seen.approved_by === null) return {status: 'pending'};
  // Hashed outside the transaction (WebCrypto is async); checked again inside it.
  const minted = await newTillToken();
  return storage.transactionSync(() => {
    const c = matches();
    if (!c) throw new NotFound('no claim');
    if (c.approved_by === null) return {status: 'pending' as const};
    const till = insertTill(storage.sql, Number(c.approved_by), String(c.name), minted, now);
    storage.sql.exec('DELETE FROM till_claims WHERE claim_id = ?', c.claim_id);
    return {status: 'approved' as const, ...till};
  });
}
