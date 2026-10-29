/**
 * The hosted provisioning registry — plan § 4, P1c.
 *
 * What this service is: the operator's record of what was provisioned. It
 * never holds a key, an ecash note or a PIN value, only the fact that a card
 * exists, its short id, and its state. The wallet and the issue step stay on
 * the operator's machine (§ 4), so nothing reachable here can spend anything.
 *
 * Shape: one Worker in front of one SQLite-backed Durable Object. The object
 * is the only writer, which is what makes `/registry/cards` idempotent across
 * concurrent callers without a transaction dance — two posts for the same
 * `short_id` queue behind the same instance and the second sees the first.
 *
 * The app (`apps/provisioner/src/api.ts`) is the contract: its `RegistryClient`
 * names every path and reads `reason` off a failure, its `CardRegistryRow` is
 * the card body, and its outbox depends on 409 meaning "you already stored a
 * different row for this short_id".
 */

/** Cards this service is asked about. Anything else is a 404, not a 400. */
const PATHS = {
  session: '/auth/operator/session',
  batches: '/registry/batches',
  cards: '/registry/cards',
} as const;

/** Session lifetime. The app treats a 401 as "sign in again" (§ api.ts). */
const SESSION_TTL_MS = 12 * 60 * 60 * 1000;

/** The default operator id. One operator account today; the column exists so a
 * second does not need a migration. */
const DEFAULT_OPERATOR_ID = 1;

export interface Env {
  readonly REGISTRY: DurableObjectNamespace;
  /** Set with `wrangler secret put OPERATOR_PASSWORD`. */
  readonly OPERATOR_PASSWORD?: string;
}

/* ------------------------------------------------------------------------- */
/* Bodies                                                                    */
/* ------------------------------------------------------------------------- */

/** A stored card row: what the app sent plus when we first took it. */
export interface StoredCard {
  readonly short_id: string;
  readonly batch_id: number;
  readonly installed_at: number;
  readonly load_sha256: string;
  readonly pin_set: boolean;
  readonly pin_policy: 'batch' | 'perCard';
  readonly notes_issued: number;
  readonly received_at: number;
}

export interface RegistryBatch {
  readonly batch_id: number;
  readonly federation_id: string;
  readonly count: number;
  readonly pin_policy: 'batch' | 'perCard';
  readonly load_sha256: string;
  readonly created_at: number;
}

/**
 * The wire body of a card, exactly as `CardRegistryRow` declares it. Kept
 * separate from `StoredCard` so the validated shape is what goes in, and
 * `received_at` is something the server adds rather than something it accepts.
 */
interface CardBody {
  readonly short_id: string;
  readonly batch_id: number;
  readonly installed_at: number;
  readonly load_sha256: string;
  readonly pin_set: boolean;
  readonly pin_policy: 'batch' | 'perCard';
  readonly notes_issued: number;
}

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: {
      // The app reads JSON on every status, including failures: a non-JSON
      // body becomes a bare status with no reason (§ api.ts `reasonOf`).
      'Content-Type': 'application/json; charset=utf-8',
      'Cache-Control': 'no-store',
    },
  });
}

/** A failure the app can show: it reads `reason` first (§ api.ts `reasonOf`). */
function fail(status: number, reason: string): Response {
  return json(status, {ok: false, reason});
}

/* ------------------------------------------------------------------------- */
/* Validation                                                                */
/* ------------------------------------------------------------------------- */

function isShortId(value: unknown): value is string {
  // A card's short id is the last 4 bytes of its slot-0 key, lower-case hex
  // (§ 5: "short id = last 4 bytes of slot-0 x-only key"). Eight hex
  // characters, no prefix — anything else is a bug upstream, not a variant.
  return typeof value === 'string' && /^[0-9a-f]{8}$/.test(value);
}

function isPinPolicy(value: unknown): value is 'batch' | 'perCard' {
  return value === 'batch' || value === 'perCard';
}

function isCount(value: unknown): value is number {
  return Number.isSafeInteger(value) && (value as number) >= 0;
}

function isSha256(value: unknown): value is string {
  return typeof value === 'string' && /^[0-9a-f]{64}$/.test(value);
}

/**
 * The card body, or a reason it is not one. Unknown keys are ignored rather
 * than rejected: the app sends `body()`'s fields and a future field must not
 * make an older server refuse a good row. A *missing or wrong-typed* field is
 * refused, because storing a row with a silent default would be a lie about
 * the card.
 */
function parseCardBody(raw: unknown): {card: CardBody} | {error: string} {
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) {
    return {error: 'the card row must be an object'};
  }
  const r = raw as Record<string, unknown>;
  if (!isShortId(r.short_id)) {
    return {error: 'short_id must be 8 lower-case hex characters'};
  }
  if (!Number.isSafeInteger(r.batch_id) || (r.batch_id as number) <= 0) {
    return {error: 'batch_id must be a positive integer'};
  }
  if (!Number.isSafeInteger(r.installed_at) || (r.installed_at as number) <= 0) {
    return {error: 'installed_at must be epoch milliseconds'};
  }
  if (!isSha256(r.load_sha256)) {
    return {error: 'load_sha256 must be 64 lower-case hex characters'};
  }
  if (typeof r.pin_set !== 'boolean') {
    return {error: 'pin_set must be a boolean'};
  }
  if (!isPinPolicy(r.pin_policy)) {
    return {error: "pin_policy must be 'batch' or 'perCard'"};
  }
  if (!isCount(r.notes_issued)) {
    return {error: 'notes_issued must be a non-negative integer'};
  }
  return {
    card: {
      short_id: r.short_id,
      batch_id: r.batch_id as number,
      installed_at: r.installed_at as number,
      load_sha256: r.load_sha256,
      pin_set: r.pin_set,
      pin_policy: r.pin_policy,
      notes_issued: r.notes_issued as number,
    },
  };
}

function parseBatchBody(raw: unknown): {batch: Omit<RegistryBatch, 'batch_id' | 'created_at'>} | {error: string} {
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) {
    return {error: 'the batch must be an object'};
  }
  const r = raw as Record<string, unknown>;
  if (typeof r.federation_id !== 'string' || !r.federation_id) {
    return {error: 'federation_id is required'};
  }
  if (!Number.isSafeInteger(r.count) || (r.count as number) <= 0) {
    return {error: 'count must be a positive integer'};
  }
  if (!isPinPolicy(r.pin_policy)) {
    return {error: "pin_policy must be 'batch' or 'perCard'"};
  }
  if (!isSha256(r.load_sha256)) {
    return {error: 'load_sha256 must be 64 lower-case hex characters'};
  }
  return {
    batch: {
      federation_id: r.federation_id,
      count: r.count as number,
      pin_policy: r.pin_policy,
      load_sha256: r.load_sha256,
    },
  };
}

/* ------------------------------------------------------------------------- */
/* Sessions                                                                  */
/* ------------------------------------------------------------------------- */

/**
 * A token is `v1.<expires>.<mac>` where the MAC covers the expiry and the
 * operator id. Stateless on purpose: a Worker restart, a rollout or a new
 * isolate all verify the same token, so an operator is not signed out by a
 * deploy. The password is the key, so rotating it revokes every session.
 */
async function signToken(
  secret: string,
  operatorId: number,
  expiresAt: number,
): Promise<string> {
  const payload = `v1.${expiresAt}.${operatorId}`;
  const key = await crypto.subtle.importKey(
    'raw',
    new TextEncoder().encode(secret),
    {name: 'HMAC', hash: 'SHA-256'},
    false,
    ['sign'],
  );
  const mac = await crypto.subtle.sign('HMAC', key, new TextEncoder().encode(payload));
  return `${payload}.${toHex(new Uint8Array(mac))}`;
}

/** The operator id a token proves, or null. Constant-time MAC comparison. */
async function verifyToken(
  secret: string,
  token: string,
  now: number,
): Promise<number | null> {
  const parts = token.split('.');
  if (parts.length !== 4 || parts[0] !== 'v1') {
    return null;
  }
  const [, expiresRaw, operatorRaw, mac] = parts as [string, string, string, string];
  const expiresAt = Number(expiresRaw);
  if (!Number.isSafeInteger(expiresAt) || expiresAt <= now) {
    return null;
  }
  const operatorId = Number(operatorRaw);
  if (!Number.isSafeInteger(operatorId) || operatorId <= 0) {
    return null;
  }
  const expected = await signToken(secret, operatorId, expiresAt);
  const expectedMac = expected.slice(expected.lastIndexOf('.') + 1);
  return constantTimeEqual(expectedMac, mac) ? operatorId : null;
}

function constantTimeEqual(a: string, b: string): boolean {
  if (a.length !== b.length) {
    return false;
  }
  let diff = 0;
  for (let i = 0; i < a.length; i += 1) {
    diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  }
  return diff === 0;
}

function toHex(bytes: Uint8Array): string {
  let out = '';
  for (const byte of bytes) {
    out += byte.toString(16).padStart(2, '0');
  }
  return out;
}

/** The password, compared without leaking its length through timing. */
function passwordMatches(expected: string, given: string): boolean {
  // Hash both first so the comparison is over equal-length inputs; the length
  // of the *password* then never influences the time taken.
  return constantTimeEqual(
    toHex(new Uint8Array(new TextEncoder().encode(expected))),
    toHex(new Uint8Array(new TextEncoder().encode(given))),
  );
}

/* ------------------------------------------------------------------------- */
/* The Durable Object                                                        */
/* ------------------------------------------------------------------------- */

/**
 * Every write goes through one object, so "has this short_id been seen" and
 * "insert it" cannot interleave with another request. The storage is the
 * object's own SQLite: rows survive a restart because they are on disk, which
 * is the P1c done-line's "a row survives a Worker restart".
 */
export class Registry {
  private readonly ctx: DurableObjectState;

  constructor(ctx: DurableObjectState) {
    this.ctx = ctx;
    ctx.blockConcurrencyWhile(async () => {
      const sql = ctx.storage.sql;
      sql.exec(
        `CREATE TABLE IF NOT EXISTS batches (
           batch_id      INTEGER PRIMARY KEY AUTOINCREMENT,
           federation_id TEXT    NOT NULL,
           count         INTEGER NOT NULL,
           pin_policy    TEXT    NOT NULL,
           load_sha256   TEXT    NOT NULL,
           created_at    INTEGER NOT NULL
         )`,
      );
      sql.exec(
        `CREATE TABLE IF NOT EXISTS cards (
           short_id      TEXT    PRIMARY KEY,
           batch_id      INTEGER NOT NULL,
           installed_at  INTEGER NOT NULL,
           load_sha256   TEXT    NOT NULL,
           pin_set       INTEGER NOT NULL,
           pin_policy    TEXT    NOT NULL,
           notes_issued  INTEGER NOT NULL,
           received_at   INTEGER NOT NULL
         )`,
      );
    });
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    // Test-only hooks. They are reachable only through the object's own stub
    // (`env.REGISTRY.get(...)`), never through the Worker's routes, so a
    // deployment cannot call them: `default.fetch` forwards three paths only.
    if (url.pathname.startsWith('/test/')) {
      return this.testHook(url.pathname);
    }
    switch (url.pathname) {
      case PATHS.batches:
        return this.createBatch(request);
      case PATHS.cards:
        return request.method === 'GET'
          ? this.listCards(url)
          : this.postCard(request);
      default:
        return fail(404, 'no such route');
    }
  }

  /** Test-only. `reset` wipes rows; `evict` drops everything held in memory. */
  private async testHook(path: string): Promise<Response> {
    const sql = this.ctx.storage.sql;
    if (path === '/test/reset') {
      sql.exec('DELETE FROM cards');
      sql.exec('DELETE FROM batches');
      return json(200, {ok: true});
    }
    if (path === '/test/count') {
      // Proves the rows are read from storage rather than a value the object
      // happens to hold, without destroying the instance to show it.
      const row = sql.exec('SELECT COUNT(*) AS n FROM cards').one() as {n: number};
      return json(200, {ok: true, cards: Number(row.n)});
    }
    return fail(404, 'no such test hook');
  }

  /** POST /registry/batches — a new batch, id and clock from here. */
  private async createBatch(request: Request): Promise<Response> {
    const raw: unknown = await request.json().catch(() => null);
    const parsed = parseBatchBody(raw);
    if ('error' in parsed) {
      return fail(400, parsed.error);
    }
    const now = Date.now();
    const cursor = this.ctx.storage.sql
      .exec(
        `INSERT INTO batches (federation_id, count, pin_policy, load_sha256, created_at)
         VALUES (?, ?, ?, ?, ?)
         RETURNING batch_id`,
        parsed.batch.federation_id,
        parsed.batch.count,
        parsed.batch.pin_policy,
        parsed.batch.load_sha256,
        now,
      )
      .one() as {batch_id: number};
    const batch: RegistryBatch = {
      batch_id: Number(cursor.batch_id),
      ...parsed.batch,
      created_at: now,
    };
    return json(200, {ok: true, batch});
  }

  /**
   * POST /registry/cards — the idempotent upsert the done-line names.
   *
   * The identical body answers 200 with the row as stored. A body for a
   * `short_id` we already hold that differs in any field answers 409 and
   * stores nothing: the app's outbox re-posts frozen bytes, so a 409 means two
   * genuinely different accounts of one card, which a human has to settle.
   *
   * `received_at` is deliberately not compared: it is ours, and the first
   * sighting is the truth.
   */
  private async postCard(request: Request): Promise<Response> {
    const raw: unknown = await request.json().catch(() => null);
    const parsed = parseCardBody(raw);
    if ('error' in parsed) {
      return fail(400, parsed.error);
    }
    const card = parsed.card;
    const existing = this.ctx.storage.sql
      .exec(`SELECT * FROM cards WHERE short_id = ?`, card.short_id)
      .toArray()[0] as Record<string, unknown> | undefined;

    if (existing) {
      const stored = rowToCard(existing);
      if (sameCard(stored, card)) {
        return json(200, {ok: true, stored});
      }
      return fail(
        409,
        `card ${card.short_id} is already stored with different contents`,
      );
    }

    const receivedAt = Date.now();
    this.ctx.storage.sql.exec(
      `INSERT INTO cards
         (short_id, batch_id, installed_at, load_sha256, pin_set, pin_policy, notes_issued, received_at)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?)`,
      card.short_id,
      card.batch_id,
      card.installed_at,
      card.load_sha256,
      card.pin_set ? 1 : 0,
      card.pin_policy,
      card.notes_issued,
      receivedAt,
    );
    return json(200, {ok: true, stored: {...card, received_at: receivedAt}});
  }

  /** GET /registry/cards?batch_id= — the summary's refresh. */
  private async listCards(url: URL): Promise<Response> {
    const raw = url.searchParams.get('batch_id');
    if (raw === null) {
      return fail(400, 'batch_id is required');
    }
    const batchId = Number(raw);
    if (!Number.isSafeInteger(batchId) || batchId <= 0) {
      return fail(400, 'batch_id must be a positive integer');
    }
    const rows = this.ctx.storage.sql
      .exec(`SELECT * FROM cards WHERE batch_id = ? ORDER BY received_at`, batchId)
      .toArray();
    return json(200, {ok: true, cards: rows.map(rowToCard)});
  }
}

function rowToCard(row: Record<string, unknown>): StoredCard {
  return {
    short_id: String(row.short_id),
    batch_id: Number(row.batch_id),
    installed_at: Number(row.installed_at),
    load_sha256: String(row.load_sha256),
    pin_set: Number(row.pin_set) === 1,
    pin_policy: String(row.pin_policy) as 'batch' | 'perCard',
    notes_issued: Number(row.notes_issued),
    received_at: Number(row.received_at),
  };
}

/** Every app-owned field equal. `received_at` is excluded on purpose. */
function sameCard(stored: StoredCard, card: CardBody): boolean {
  return (
    stored.batch_id === card.batch_id &&
    stored.installed_at === card.installed_at &&
    stored.load_sha256 === card.load_sha256 &&
    stored.pin_set === card.pin_set &&
    stored.pin_policy === card.pin_policy &&
    stored.notes_issued === card.notes_issued
  );
}

/* ------------------------------------------------------------------------- */
/* The Worker                                                                */
/* ------------------------------------------------------------------------- */

function objectFor(env: Env): DurableObjectStub {
  // One global registry: the id is a constant, so every request lands on the
  // same object and the idempotency guarantee is real rather than per-shard.
  return env.REGISTRY.get(env.REGISTRY.idFromName('registry'));
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const secret = env.OPERATOR_PASSWORD;

    if (url.pathname === '/health') {
      return json(200, {ok: true});
    }

    if (!secret) {
      // A deployment without the secret would accept any password if we fell
      // through to a default; say so loudly instead.
      return fail(503, 'the registry has no operator password configured');
    }

    if (url.pathname === PATHS.session) {
      if (request.method !== 'POST') {
        return fail(405, 'use POST');
      }
      const raw: unknown = await request.json().catch(() => null);
      if (typeof raw !== 'object' || raw === null) {
        return fail(400, 'expected {name, password}');
      }
      const {name, password} = raw as Record<string, unknown>;
      if (typeof name !== 'string' || typeof password !== 'string') {
        return fail(400, 'expected {name, password}');
      }
      // One operator account: the name is checked for shape, the password is
      // the credential. A name mismatch and a password mismatch answer the
      // same 401 so the service does not confirm which half was wrong.
      if (!name || !passwordMatches(secret, password)) {
        return fail(401, 'the operator name or password is not correct');
      }
      const expiresAt = Date.now() + SESSION_TTL_MS;
      const token = await signToken(secret, DEFAULT_OPERATOR_ID, expiresAt);
      return json(200, {ok: true, operator_id: DEFAULT_OPERATOR_ID, token});
    }

    if (url.pathname === PATHS.batches || url.pathname === PATHS.cards) {
      const header = request.headers.get('Authorization') ?? '';
      const bearer = header.startsWith('Bearer ') ? header.slice(7).trim() : '';
      // Unauthenticated is 401 whatever the token says: the app signs out on
      // it rather than retrying (§ api.ts `retryAll`).
      const operatorId =
        bearer === '' ? null : await verifyToken(secret, bearer, Date.now());
      if (operatorId === null) {
        return fail(401, 'sign in first');
      }
      const forwarded = new Request(`https://registry.internal${url.pathname}${url.search}`, request);
      return objectFor(env).fetch(forwarded);
    }

    return fail(404, 'no such route');
  },
};
