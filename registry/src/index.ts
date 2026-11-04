/**
 * The hosted registry, which OPS-PLAN calls ops: operator accounts, batches,
 * card rows, and card state for tills (docs/OPS-PLAN.md § 5).
 *
 * It never holds a seed, a PIN, a slot secret or a till's output secret.
 * What it holds about a card (keys, notes, charges) can be spent only with
 * the card itself, behind its PIN.
 *
 * Shape: one Worker in front of one SQLite-backed Durable Object, which is the
 * only writer. Every check-then-write runs in one synchronous transaction on
 * that object, which is what makes the idempotent upserts and the card
 * version checks hold across concurrent callers.
 *
 * Routes, by who may call them:
 *
 *   anyone     GET  /health
 *              POST /auth/operator/session       {name, password} -> {operator_id, token}
 *   operator   POST /auth/operator/signout
 *              POST /registry/batches            GET /registry/batches
 *              POST /registry/cards              GET /registry/cards?batch_id=
 *              POST /tills {name} -> {till_id, token}   GET /tills   DELETE /tills/:id
 *              GET  /registry/federations        names tills reported
 *   operator   GET  /cards/:short_id/state
 *   or till
 *   till       POST /cards/:short_id/intent      the charge, unsigned, before the card signs
 *              POST /cards/:short_id/submitted   the signed bytes, before the till sends them
 *              POST /cards/:short_id/outcome     the federation's answer
 *              POST /federations                 {federation_id, name} from the federation's config
 *              PUT  /cards/:short_id/slots/:n    a renewed slot key
 *
 * An operator, and that operator's tills, see only their own batches and
 * cards. cashucard records the operator on a batch but does not filter on it;
 * this service does, because it holds balances (OPS-PLAN § 9.5).
 */
import {
  applySetup,
  createTill,
  hashPassword,
  listTills,
  operatorFor,
  RateLimited,
  revokeTill,
  signIn,
  signOut,
  tillFor,
  type Till,
} from './auth';
import {
  BadRequest,
  cardOwner,
  cardState,
  Conflict,
  createBatch,
  listBatches,
  listCards,
  listFederationNames,
  NotFound,
  postCard,
  putFederationName,
  recordIntent,
  recordOutcome,
  recordSubmitted,
  renewSlot,
} from './cards';
import {migrate} from './db';
import {bearer, fail, isPositiveInt, isShortId, json, objectBody} from './http';

export interface Env {
  readonly REGISTRY: DurableObjectNamespace;
  /**
   * The operator setup command, a secret, as JSON:
   * {"action":"bootstrap"|"recover","nonce":"…16+ chars…","name":"…","password":"…12+ chars…"}
   * Applied once per nonce when the object starts. `wrangler secret put OPERATOR_SETUP`.
   */
  readonly OPERATOR_SETUP?: string;
}

type Caller = {kind: 'operator'; operatorId: number} | {kind: 'till'; till: Till};

export class Registry {
  private readonly ctx: DurableObjectState;
  /** Why the object cannot serve, when the setup command was refused. */
  private broken: string | null = null;

  constructor(ctx: DurableObjectState, env: Env) {
    this.ctx = ctx;
    ctx.blockConcurrencyWhile(async () => {
      migrate(ctx.storage);
      try {
        await applySetup(ctx.storage, env.OPERATOR_SETUP, Date.now());
      } catch (e) {
        // The message never carries the command; it names what was wrong.
        this.broken = e instanceof Error ? e.message : 'OPERATOR_SETUP failed';
      }
    });
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname.startsWith('/test/')) {
      return this.testHook(url.pathname, request);
    }
    if (this.broken) {
      return fail(503, `the registry's operator setup was refused: ${this.broken}`);
    }
    try {
      return await this.route(request, url);
    } catch (e) {
      if (e instanceof BadRequest) return fail(400, e.message);
      if (e instanceof NotFound) return fail(404, e.message);
      if (e instanceof Conflict) return fail(409, e.message);
      if (e instanceof RateLimited) return fail(429, e.message);
      throw e;
    }
  }

  private async caller(request: Request): Promise<Caller | null> {
    const token = bearer(request);
    const storage = this.ctx.storage;
    const operatorId = await operatorFor(storage, token, Date.now());
    if (operatorId !== null) {
      return {kind: 'operator', operatorId};
    }
    const till = await tillFor(storage, token);
    return till ? {kind: 'till', till} : null;
  }

  private async route(request: Request, url: URL): Promise<Response> {
    const storage = this.ctx.storage;
    const sql = storage.sql;
    const path = url.pathname;
    const method = request.method;
    const now = Date.now();

    if (path === '/auth/operator/session') {
      if (method !== 'POST') return fail(405, 'use POST');
      const body = await objectBody(request);
      const name = body?.name;
      const password = body?.password;
      if (typeof name !== 'string' || typeof password !== 'string') {
        return fail(400, 'expected {name, password}');
      }
      if (sql.exec('SELECT id FROM operators LIMIT 1').toArray().length === 0) {
        return fail(503, 'the registry has no operator yet; set OPERATOR_SETUP');
      }
      const session = await signIn(storage, name, password, now);
      // A wrong name and a wrong password answer alike.
      return session
        ? json(200, {ok: true, ...session})
        : fail(401, 'the operator name or password is not correct');
    }

    const caller = await this.caller(request);
    if (!caller) {
      // The app signs out on a 401 rather than retrying.
      return fail(401, 'sign in first');
    }

    const card = /^\/cards\/([^/]+)(\/.*)?$/.exec(path);
    if (card) {
      return this.cardRoute(request, caller, card[1]!, card[2] ?? '', now);
    }

    if (path === '/federations' && method === 'POST') {
      if (caller.kind !== 'till') return fail(403, 'only a till reports a federation name');
      const body = await objectBody(request);
      if (!body) return fail(400, 'expected {federation_id, name}');
      putFederationName(sql, caller.till.operator_id, body, now);
      return json(200, {ok: true});
    }

    if (caller.kind !== 'operator') {
      return fail(403, 'a till token cannot do that');
    }
    const op = caller.operatorId;

    if (path === '/auth/operator/signout' && method === 'POST') {
      await signOut(storage, bearer(request), now);
      return json(200, {ok: true});
    }
    if (path === '/registry/batches') {
      if (method === 'GET') return json(200, {ok: true, batches: listBatches(sql, op)});
      if (method !== 'POST') return fail(405, 'use GET or POST');
      const body = await objectBody(request);
      if (!body) return fail(400, 'the batch must be an object');
      return json(200, {ok: true, batch: createBatch(sql, op, body, now)});
    }
    if (path === '/registry/cards') {
      if (method === 'GET') {
        const batchId = Number(url.searchParams.get('batch_id'));
        if (!isPositiveInt(batchId)) return fail(400, 'batch_id must be a positive integer');
        return json(200, {ok: true, cards: listCards(sql, op, batchId)});
      }
      if (method !== 'POST') return fail(405, 'use GET or POST');
      const body = await objectBody(request);
      if (!body) return fail(400, 'the card row must be an object');
      return json(200, {ok: true, stored: postCard(storage, op, body, now)});
    }
    if (path === '/registry/federations' && method === 'GET') {
      return json(200, {ok: true, federations: listFederationNames(sql, op)});
    }
    if (path === '/tills') {
      if (method === 'GET') return json(200, {ok: true, tills: listTills(storage, op)});
      if (method !== 'POST') return fail(405, 'use GET or POST');
      const body = await objectBody(request);
      const name = body?.name;
      if (typeof name !== 'string' || name.trim() === '' || name.length > 64) {
        return fail(400, 'a till needs a name of 1 to 64 characters');
      }
      // The token is shown once, here; only its hash is kept.
      return json(200, {ok: true, ...(await createTill(storage, op, name.trim(), now))});
    }
    const till = /^\/tills\/(\d+)$/.exec(path);
    if (till && method === 'DELETE') {
      return revokeTill(storage, op, Number(till[1]), now)
        ? json(200, {ok: true})
        : fail(404, 'no such till for this operator');
    }
    return fail(404, 'no such route');
  }

  private async cardRoute(
    request: Request,
    caller: Caller,
    shortId: string,
    rest: string,
    now: number,
  ): Promise<Response> {
    const storage = this.ctx.storage;
    if (!isShortId(shortId)) {
      return fail(400, 'short_id must be 8 lower-case hex characters');
    }
    // A card another operator owns answers as if it did not exist.
    const owner = cardOwner(storage.sql, shortId);
    const mine = caller.kind === 'operator' ? caller.operatorId : caller.till.operator_id;
    if (owner === null || owner !== mine) {
      return fail(404, `no card ${shortId}`);
    }
    const method = request.method;
    if (rest === '/state' && method === 'GET') {
      return json(200, {ok: true, ...cardState(storage.sql, shortId)});
    }
    if (caller.kind !== 'till') {
      return fail(403, 'only a till records charges and slot keys');
    }
    const body = await objectBody(request);
    if (!body) return fail(400, 'expected a JSON object');
    if (rest === '/intent' && method === 'POST') {
      return json(200, {ok: true, ...recordIntent(storage, shortId, caller.till.till_id, body, now)});
    }
    if (rest === '/submitted' && method === 'POST') {
      return json(200, {ok: true, ...recordSubmitted(storage, shortId, body)});
    }
    if (rest === '/outcome' && method === 'POST') {
      return json(200, {ok: true, ...recordOutcome(storage, shortId, body, now)});
    }
    const slot = /^\/slots\/(\d)$/.exec(rest);
    if (slot && method === 'PUT') {
      return json(200, {ok: true, ...renewSlot(storage, shortId, Number(slot[1]), body)});
    }
    return fail(404, 'no such route');
  }

  /**
   * Test-only. Reachable only through the object's own stub
   * (`env.REGISTRY.get(...)`); the Worker refuses every `/test/` path, so a
   * deployment cannot call them.
   */
  private async testHook(path: string, request: Request): Promise<Response> {
    const sql = this.ctx.storage.sql;
    if (path === '/test/reset') {
      for (const table of [
        'cards', 'batches', 'card_slots', 'card_versions', 'notes', 'card_txs',
        'tills', 'sessions', 'login_attempt_budget', 'federations',
      ]) {
        sql.exec(`DELETE FROM ${table}`);
      }
      return json(200, {ok: true});
    }
    if (path === '/test/count') {
      const row = sql.exec('SELECT COUNT(*) AS n FROM cards').one() as {n: number};
      return json(200, {ok: true, cards: Number(row.n)});
    }
    if (path === '/test/operator') {
      // A second operator for the scoping tests, created the way recovery
      // writes one: through the same hash.
      const body = (await objectBody(request)) ?? {};
      sql.exec(
        `INSERT INTO operators (name, password_hash, created_at) VALUES (?, ?, ?)
         ON CONFLICT(name) DO UPDATE SET password_hash = excluded.password_hash`,
        String(body.name),
        await hashPassword(String(body.password)),
        Date.now(),
      );
      return json(200, {ok: true});
    }
    if (path === '/test/note') {
      // A live note, as a holder's load will write one (O4).
      const b = (await objectBody(request)) ?? {};
      sql.exec(
        `INSERT INTO notes (short_id, nonce, slot, amount_msat, signature, blinding_key, status, txid, out_idx)
         VALUES (?, ?, ?, ?, ?, NULL, 'live', NULL, NULL)`,
        b.short_id, b.nonce, b.slot, b.amount_msat, b.signature,
      );
      return json(200, {ok: true});
    }
    return fail(404, 'no such test hook');
  }
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === '/health') {
      return json(200, {ok: true});
    }
    if (url.pathname.startsWith('/test/')) {
      return fail(404, 'no such route');
    }
    // One global object: every request lands on the same writer.
    const stub = env.REGISTRY.get(env.REGISTRY.idFromName('registry'));
    return stub.fetch(new Request(`https://registry.internal${url.pathname}${url.search}`, request));
  },
};
