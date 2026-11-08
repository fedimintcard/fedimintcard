/**
 * Operator accounts and till tokens, after cashucard's ops (`ops/src/auth.ts`,
 * `pwhash.ts`, `operator-setup.ts`), on the object's own SQLite and the
 * runtime's WebCrypto.
 *
 * - **Operators** are rows with a name and a PBKDF2-SHA256 password hash. They
 *   are created only by a setup command injected at deploy time
 *   (`OPERATOR_SETUP`, a secret), never over HTTP: `bootstrap` creates the
 *   first operator; `recover` resets a named operator's password and revokes
 *   their sessions. Each command carries a nonce and applies once.
 * - **Operator sessions** are 12 h bearer tokens (`op_…`), stored only as a
 *   SHA-256 hash, revocable by sign-out or by recovery.
 * - **Sign-in** is budgeted service-wide: 10 attempts a minute, reserved
 *   before the slow hash so concurrent attempts cannot all pass.
 * - **Till tokens** (`till_…`) are issued by an operator, stored hashed, never
 *   expire, and are revoked by the operator who issued them. A till can also ask
 *   for one and have an operator approve it (`claims.ts`).
 */
import {constantTimeEqual, fromHex, randomHex, sha256Hex, toHex} from './http';

export const SESSION_TTL_MS = 12 * 60 * 60 * 1000;
export const LOGIN_MAX_ATTEMPTS = 10;
export const LOGIN_WINDOW_MS = 60 * 1000;

/**
 * PBKDF2-SHA256 iterations. OWASP's 2023 floor is 600,000, which cashucard
 * uses; its app notes that sign-in then takes 8–12 s on the deployed Worker
 * (`apps/provisioner/src/api.ts` there). 100,000 is chosen for sign-in time,
 * with the login budget below limiting guesses. The stored string carries
 * the count, so raising it later leaves existing hashes verifying.
 */
export const PBKDF2_ITERATIONS = 100_000;
const KEY_BYTES = 32;

async function derive(
  password: string,
  salt: Uint8Array,
  iterations: number,
): Promise<Uint8Array> {
  const key = await crypto.subtle.importKey(
    'raw',
    new TextEncoder().encode(password),
    'PBKDF2',
    false,
    ['deriveBits'],
  );
  const bits = await crypto.subtle.deriveBits(
    {name: 'PBKDF2', hash: 'SHA-256', salt, iterations},
    key,
    KEY_BYTES * 8,
  );
  return new Uint8Array(bits);
}

/** `pbkdf2$<iterations>$<salt hex>$<hash hex>` */
export async function hashPassword(password: string): Promise<string> {
  const salt = crypto.getRandomValues(new Uint8Array(16));
  const hash = await derive(password, salt, PBKDF2_ITERATIONS);
  return `pbkdf2$${PBKDF2_ITERATIONS}$${toHex(salt)}$${toHex(hash)}`;
}

export async function verifyPassword(
  password: string,
  stored: string,
): Promise<boolean> {
  const parts = stored.split('$');
  if (parts.length !== 4 || parts[0] !== 'pbkdf2') {
    return false;
  }
  const iterations = Number(parts[1]);
  if (!Number.isSafeInteger(iterations) || iterations < 1) {
    return false;
  }
  const expected = fromHex(parts[3]!);
  const actual = await derive(password, fromHex(parts[2]!), iterations);
  return constantTimeEqual(expected, actual);
}

/* ------------------------------------------------------------------------- */
/* The setup command                                                         */
/* ------------------------------------------------------------------------- */

interface Setup {
  /** bootstrap: the first operator; recover: a new password; add: another operator (e.g. a store reviewer). */
  action: 'bootstrap' | 'recover' | 'add';
  nonce: string;
  name: string;
  password: string;
}

export function parseSetup(raw: string): Setup {
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    throw new Error('OPERATOR_SETUP is not JSON');
  }
  const s = value as Partial<Setup> | null;
  if (
    !s ||
    (s.action !== 'bootstrap' && s.action !== 'recover' && s.action !== 'add') ||
    typeof s.nonce !== 'string' ||
    !/^[A-Za-z0-9_-]{16,128}$/.test(s.nonce) ||
    typeof s.name !== 'string' ||
    !/^[A-Za-z0-9._@-]{1,64}$/.test(s.name) ||
    typeof s.password !== 'string' ||
    s.password.length < 12 ||
    s.password.length > 1024
  ) {
    // Never echo the command: it carries a password.
    throw new Error('OPERATOR_SETUP fields are invalid');
  }
  return s as Setup;
}

/**
 * Applies the deploy-injected command once per nonce. The caller runs this
 * inside `blockConcurrencyWhile`, so no request sees a half-applied command.
 * A reused nonce with a different action or name is refused rather than
 * ignored, and a changed password needs a new nonce.
 */
export async function applySetup(
  storage: DurableObjectStorage,
  raw: string | undefined,
  now: number,
): Promise<{operator_id: number; applied: boolean} | undefined> {
  if (raw === undefined || raw === '') {
    return undefined;
  }
  const setup = parseSetup(raw);
  const sql = storage.sql;
  const previous = () => {
    const row = sql
      .exec(
        'SELECT action, operator_id, name FROM operator_setup_commands WHERE nonce = ?',
        setup.nonce,
      )
      .toArray()[0] as {action: string; operator_id: number; name: string} | undefined;
    if (row && (row.action !== setup.action || row.name !== setup.name)) {
      throw new Error('OPERATOR_SETUP nonce was already used for another command');
    }
    return row;
  };
  const done = previous();
  if (done) {
    return {operator_id: Number(done.operator_id), applied: false};
  }
  const passwordHash = await hashPassword(setup.password);
  return storage.transactionSync(() => {
    const raced = previous();
    if (raced) {
      return {operator_id: Number(raced.operator_id), applied: false};
    }
    let id: number;
    if (setup.action === 'bootstrap' || setup.action === 'add') {
      if (setup.action === 'bootstrap' && sql.exec('SELECT id FROM operators LIMIT 1').toArray().length > 0) {
        throw new Error('bootstrap needs an empty operators table; use recover or add');
      }
      if (setup.action === 'add' && sql.exec('SELECT id FROM operators WHERE name = ?', setup.name).toArray().length > 0) {
        throw new Error('add needs a new operator name; use recover for an existing one');
      }
      id = Number(
        (
          sql
            .exec(
              'INSERT INTO operators (name, password_hash, created_at) VALUES (?, ?, ?) RETURNING id',
              setup.name,
              passwordHash,
              now,
            )
            .one() as {id: number}
        ).id,
      );
    } else {
      const row = sql
        .exec('SELECT id FROM operators WHERE name = ?', setup.name)
        .toArray()[0] as {id: number} | undefined;
      if (!row) {
        throw new Error('recover needs an existing operator of that name');
      }
      id = Number(row.id);
      sql.exec('UPDATE operators SET password_hash = ? WHERE id = ?', passwordHash, id);
      sql.exec(
        'UPDATE sessions SET revoked_at = ? WHERE operator_id = ? AND revoked_at IS NULL',
        now,
        id,
      );
    }
    sql.exec(
      'INSERT INTO operator_setup_commands (nonce, action, operator_id, name, applied_at) VALUES (?, ?, ?, ?, ?)',
      setup.nonce,
      setup.action,
      id,
      setup.name,
      now,
    );
    return {operator_id: id, applied: true};
  });
}

/* ------------------------------------------------------------------------- */
/* Operator sessions                                                         */
/* ------------------------------------------------------------------------- */

export class RateLimited extends Error {}

function reserveLoginAttempt(storage: DurableObjectStorage, now: number): void {
  storage.transactionSync(() => {
    const sql = storage.sql;
    const row = sql
      .exec('SELECT window_start, attempts FROM login_attempt_budget WHERE id = 1')
      .toArray()[0] as {window_start: number; attempts: number} | undefined;
    const reset = !row || now - Number(row.window_start) >= LOGIN_WINDOW_MS;
    if (!reset && Number(row.attempts) >= LOGIN_MAX_ATTEMPTS) {
      throw new RateLimited('too many sign-in attempts; try again in a minute');
    }
    sql.exec(
      `INSERT INTO login_attempt_budget (id, window_start, attempts) VALUES (1, ?, ?)
       ON CONFLICT(id) DO UPDATE SET window_start = excluded.window_start, attempts = excluded.attempts`,
      reset ? now : Number(row.window_start),
      reset ? 1 : Number(row.attempts) + 1,
    );
  });
}

/** A session token for the right name and password, or null. */
export async function signIn(
  storage: DurableObjectStorage,
  name: string,
  password: string,
  now: number,
): Promise<{operator_id: number; token: string} | null> {
  reserveLoginAttempt(storage, now);
  const sql = storage.sql;
  const operator = sql
    .exec('SELECT id, password_hash FROM operators WHERE name = ?', name)
    .toArray()[0] as {id: number; password_hash: string} | undefined;
  // An unknown name costs the same hash as a known one, so the time taken
  // does not say which names exist.
  const ok = await verifyPassword(
    password,
    operator?.password_hash ?? `pbkdf2$${PBKDF2_ITERATIONS}$00$00`,
  );
  if (!operator || !ok) {
    return null;
  }
  const token = `op_${randomHex(32)}`;
  const tokenHash = await sha256Hex(token);
  // The hash yields to other requests: a recovery that landed meanwhile must
  // win over this sign-in, so check the password is still the one verified.
  const issued = storage.transactionSync(() => {
    const current = sql
      .exec('SELECT password_hash FROM operators WHERE id = ?', operator.id)
      .toArray()[0] as {password_hash: string} | undefined;
    if (!current || current.password_hash !== operator.password_hash) {
      return false;
    }
    sql.exec(
      'INSERT INTO sessions (token_hash, operator_id, expires_at, revoked_at, created_at) VALUES (?, ?, ?, NULL, ?)',
      tokenHash,
      operator.id,
      now + SESSION_TTL_MS,
      now,
    );
    return true;
  });
  return issued ? {operator_id: Number(operator.id), token} : null;
}

/** The operator a live session token belongs to, or null. */
export async function operatorFor(
  storage: DurableObjectStorage,
  token: string,
  now: number,
): Promise<number | null> {
  if (!token.startsWith('op_')) {
    return null;
  }
  const row = storage.sql
    .exec(
      'SELECT operator_id, expires_at, revoked_at FROM sessions WHERE token_hash = ?',
      await sha256Hex(token),
    )
    .toArray()[0] as
    | {operator_id: number; expires_at: number; revoked_at: number | null}
    | undefined;
  if (!row || row.revoked_at !== null || Number(row.expires_at) <= now) {
    return null;
  }
  return Number(row.operator_id);
}

export async function signOut(
  storage: DurableObjectStorage,
  token: string,
  now: number,
): Promise<void> {
  storage.sql.exec(
    'UPDATE sessions SET revoked_at = ? WHERE token_hash = ? AND revoked_at IS NULL',
    now,
    await sha256Hex(token),
  );
}

/* ------------------------------------------------------------------------- */
/* Till tokens                                                               */
/* ------------------------------------------------------------------------- */

export interface Till {
  readonly till_id: number;
  readonly operator_id: number;
}

/** A new till token and its hash; the token is shown once and never stored. */
export async function newTillToken(): Promise<{token: string; hash: string}> {
  const token = `till_${randomHex(32)}`;
  return {token, hash: await sha256Hex(token)};
}

/** The till row for a token from [`newTillToken`]. Synchronous, so it can join a transaction. */
export function insertTill(
  sql: SqlStorage,
  operatorId: number,
  name: string,
  minted: {token: string; hash: string},
  now: number,
): {till_id: number; name: string; token: string} {
  const row = sql
    .exec(
      'INSERT INTO tills (operator_id, name, token_hash, created_at, revoked_at) VALUES (?, ?, ?, ?, NULL) RETURNING till_id',
      operatorId,
      name,
      minted.hash,
      now,
    )
    .one() as {till_id: number};
  return {till_id: Number(row.till_id), name, token: minted.token};
}

/** `POST /tills`; a till claim's approved poll makes its till the same way (`claims.ts`). */
export async function createTill(
  storage: DurableObjectStorage,
  operatorId: number,
  name: string,
  now: number,
): Promise<{till_id: number; name: string; token: string}> {
  return insertTill(storage.sql, operatorId, name, await newTillToken(), now);
}

export function listTills(storage: DurableObjectStorage, operatorId: number) {
  return storage.sql
    .exec(
      'SELECT till_id, name, created_at, revoked_at FROM tills WHERE operator_id = ? ORDER BY till_id',
      operatorId,
    )
    .toArray()
    .map(r => ({
      till_id: Number(r.till_id),
      name: String(r.name),
      created_at: Number(r.created_at),
      revoked_at: r.revoked_at === null ? null : Number(r.revoked_at),
    }));
}

/** Revokes one of this operator's tills; false when it is not theirs. */
export function revokeTill(
  storage: DurableObjectStorage,
  operatorId: number,
  tillId: number,
  now: number,
): boolean {
  const row = storage.sql
    .exec('SELECT operator_id FROM tills WHERE till_id = ?', tillId)
    .toArray()[0] as {operator_id: number} | undefined;
  if (!row || Number(row.operator_id) !== operatorId) {
    return false;
  }
  storage.sql.exec(
    'UPDATE tills SET revoked_at = COALESCE(revoked_at, ?) WHERE till_id = ?',
    now,
    tillId,
  );
  return true;
}

export async function tillFor(
  storage: DurableObjectStorage,
  token: string,
): Promise<Till | null> {
  if (!token.startsWith('till_')) {
    return null;
  }
  const row = storage.sql
    .exec(
      'SELECT till_id, operator_id, revoked_at FROM tills WHERE token_hash = ?',
      await sha256Hex(token),
    )
    .toArray()[0] as
    | {till_id: number; operator_id: number; revoked_at: number | null}
    | undefined;
  if (!row || row.revoked_at !== null) {
    return null;
  }
  return {till_id: Number(row.till_id), operator_id: Number(row.operator_id)};
}
