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
 *              GET  /registry/cert-key           the certificate key's public point (65 bytes hex)
 *              POST /auth/operator/session       {name, password} -> {operator_id, token}
 *              POST /till-claims                 {name} -> {claim_id, code, poll_secret, expires_at}
 *                                                (docs/TILL-CLAIM.md; per-IP and global limits, 429)
 *   the till   POST /till-claims/:claim_id/poll  {poll_secret} -> {status: pending} or, once,
 *   that asked                                    {status: approved, till_id, token} (claims.ts)
 *   a holder   GET  /cards/:short_id/slots       free slot keys and version, nothing else
 *   (no token) POST /cards/:short_id/load        notes from the holder's wallet, before it submits
 *              POST /cards/:short_id/load/outcome  their signatures, each verified (loads.ts)
 *   operator   POST /auth/operator/signout
 *              POST /registry/batches            GET /registry/batches
 *              POST /registry/cards              GET /registry/cards?batch_id=
 *              POST /registry/manager-keys       {chip, short_id?} -> the card's escrowed GP keys (managerKeys.ts)
 *              GET  /registry/cards/:id/reset-check   POST /registry/cards/:id/reset   (reset.ts)
 *                                                the reset's completion: card factory, chip's escrow released
 *              POST /registry/resume-check       {chip, short_id}: may a lifted install be finished (resume.ts);
 *                                                the finished row then names `supersedes`, closing the old one
 *              POST /tills {name} -> {till_id, token}   GET /tills   DELETE /tills/:id
 *              GET  /till-claims/:code           a till's claim: name, status (pending, approved)
 *              POST /till-claims/:code/approve   ties it to this operator; its till is made at the poll
 *              GET  /registry/federations        names tills reported
 *              GET  /registry/backup-status      last backup; 503 if none or stale
 *              POST /registry/cards/:id/attestation  {attest_pub} -> {cert}: the card's certificate
 *              GET  /registry/offline-reports?short_id=  offline charges reported, newest first
 *   operator   GET  /cards/:short_id/state
 *   or till    POST /cards/:short_id/slots/:n/key   {pubkey}: the first key of a never-keyed slot
 *              POST /cards/:short_id/slot-keys      {keys: [{slot, pubkey}]}: the same, several at once
 *                                                (a 32-slot card registered with keys for 0-7; cards.ts addSlotKeys)
 *   till       POST /cards/:short_id/intent      the charge, unsigned, before the card signs;
 *                                                 with no inputs, a load from the till's wallet
 *              POST /cards/:short_id/submitted   the signed bytes, before the till sends them
 *              POST /cards/:short_id/outcome     the federation's answer
 *              POST /cards/:short_id/offline-charges  a charge the card signed offline, reported after the
 *                                                fact: {intent fields, signed_hex, signed_at}; 409 double_spend
 *                                                with conflicts (offline.ts, docs/OFFLINE-PROTOCOL.md § 3)
 *              POST /federations                 {federation_id, name, tbs_pks} from the federation's config
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
  addSlotKeys,
  BadRequest,
  cardOwner,
  cardState,
  Conflict,
  createBatch,
  listBatches,
  listCards,
  listFederationNames,
  NotFound,
  parseSlotKeys,
  postCard,
  putFederationName,
  recordIntent,
  recordOutcome,
  recordSubmitted,
  renewSlot,
} from './cards';
import {backupStatus, nextBackupAt, runBackup} from './backup';
import {approveClaim, pollClaim, readClaim, startClaim} from './claims';
import {migrate} from './db';
import {
  bearer,
  fail,
  isPositiveInt,
  isShortId,
  json,
  objectBody,
  ServiceUnavailable,
  Unprocessable,
} from './http';
import {expireLoads, freeSlots, recordLoad, recordLoadOutcome, spendBudget} from './loads';
import {
  certPublicKey,
  chargeRejected,
  listOfflineReports,
  recordAttestation,
  recordOfflineCharge,
  recordReleases,
  reportRefused,
} from './offline';
import {isChip, issueManagerKeys} from './managerKeys';
import {completeReset, resetCheck, resetOwner} from './reset';
import {closedReason, closeSuperseded, parseSupersedes, pinIsSet, resumeCheck} from './resume';

export interface Env {
  readonly REGISTRY: DurableObjectNamespace;
  /**
   * The operator setup command, a secret, as JSON:
   * {"action":"bootstrap"|"recover"|"add","nonce":"…16+ chars…","name":"…","password":"…12+ chars…"}
   * (add: another operator with a name not yet used, e.g. an app-store reviewer on staging)
   * Applied once per nonce when the object starts. `wrangler secret put OPERATOR_SETUP`.
   */
  readonly OPERATOR_SETUP?: string;
  /** R2 bucket for the nightly encrypted backup (`backup.ts`). */
  readonly BACKUPS?: R2Bucket;
  /** The age recipient (age1…) backups are encrypted to; its identity lives outside Cloudflare. */
  readonly BACKUP_RECIPIENT?: string;
  /**
   * The master every card's card-manager keys derive from (`managerKeys.ts`):
   * 32 bytes, hex. A secret: `wrangler secret put CARD_MANAGER_MASTER`. Without
   * it, `/registry/manager-keys` answers 503 and nothing else changes.
   */
  readonly CARD_MANAGER_MASTER?: string;
  /**
   * The certificate key (`offline.ts`): a P-256 private key, PKCS#8 DER, base64. A secret:
   * `wrangler secret put CERT_SIGNING_KEY`. Without it the certificate routes answer 503.
   */
  readonly CERT_SIGNING_KEY?: string;
}

type Caller = {kind: 'operator'; operatorId: number} | {kind: 'till'; till: Till};

export class Registry {
  private readonly ctx: DurableObjectState;
  private readonly env: Env;
  /** Why the object cannot serve, when the setup command was refused. */
  private broken: string | null = null;
  /** Test-only (`/test/cert-key`): act as a deployment without `CERT_SIGNING_KEY` for releases. */
  private releaseKeyOff = false;

  constructor(ctx: DurableObjectState, env: Env) {
    this.ctx = ctx;
    this.env = env;
    ctx.blockConcurrencyWhile(async () => {
      migrate(ctx.storage);
      // The nightly backup runs on the object's alarm; arm it if nothing is.
      if ((await ctx.storage.getAlarm()) === null) {
        await ctx.storage.setAlarm(nextBackupAt(Date.now()));
      }
      try {
        await applySetup(ctx.storage, env.OPERATOR_SETUP, Date.now());
      } catch (e) {
        // The message never carries the command; it names what was wrong.
        this.broken = e instanceof Error ? e.message : 'OPERATOR_SETUP failed';
      }
    });
  }

  /** The nightly backup. A failure is recorded (backup-status) and retried in an hour. */
  async alarm(): Promise<void> {
    const now = Date.now();
    try {
      await runBackup(this.ctx.storage, this.env.BACKUPS, this.env.BACKUP_RECIPIENT, now);
      await this.ctx.storage.setAlarm(nextBackupAt(now));
    } catch {
      await this.ctx.storage.setAlarm(now + 60 * 60 * 1000);
    }
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname.startsWith('/test/')) {
      return this.testHook(url.pathname, request);
    }
    // Reached only through the object's own stub: the Worker refuses /internal/.
    if (url.pathname === '/internal/backup' && request.method === 'POST') {
      const done = await runBackup(this.ctx.storage, this.env.BACKUPS, this.env.BACKUP_RECIPIENT, Date.now());
      return json(200, {ok: true, ...done});
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
      if (e instanceof Unprocessable) return fail(422, e.message);
      if (e instanceof RateLimited) return fail(429, e.message);
      if (e instanceof ServiceUnavailable) return fail(503, e.message);
      throw e;
    }
  }

  /** The certificate key releases are signed with (`recordReleases`). */
  private certSecret(): string | undefined {
    return this.releaseKeyOff ? undefined : this.env.CERT_SIGNING_KEY;
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

    // A till asking for its token has nothing yet: these answer without one (claims.ts).
    if (path === '/till-claims') {
      if (method !== 'POST') return fail(405, 'use POST');
      const ip = request.headers.get('CF-Connecting-IP') ?? 'unknown';
      return json(200, {ok: true, ...(await startClaim(storage, ip, await objectBody(request), now))});
    }
    const poll = /^\/till-claims\/([^/]+)\/poll$/.exec(path);
    if (poll) {
      if (method !== 'POST') return fail(405, 'use POST');
      return json(200, {ok: true, ...(await pollClaim(storage, poll[1]!, await objectBody(request), now))});
    }

    // A holder has the card and nothing else: these answer without a token.
    const holder = /^\/cards\/([^/]+)\/(slots|load|load\/outcome)$/.exec(path);
    if (holder) {
      return this.holderRoute(request, holder[1]!, holder[2]!, now);
    }

    if (path === '/registry/cert-key') {
      if (method !== 'GET') return fail(405, 'use GET');
      return json(200, {ok: true, key: await certPublicKey(this.env.CERT_SIGNING_KEY)});
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
      // A resumed card names the short id it answered before; that row is
      // closed in the same transaction, or nothing is stored (resume.ts).
      const supersedes = parseSupersedes(body);
      const stored = postCard(storage, op, body, now, card => {
        if (supersedes !== null) closeSuperseded(sql, op, card, supersedes);
      });
      return json(200, {ok: true, stored});
    }
    if (path === '/registry/resume-check') {
      if (method !== 'POST') return fail(405, 'use POST');
      const body = await objectBody(request);
      if (!body) return fail(400, 'expected {chip, short_id}');
      return json(200, {ok: true, ...resumeCheck(sql, op, body)});
    }
    if (path === '/registry/manager-keys') {
      if (method !== 'POST') return fail(405, 'use POST');
      const body = await objectBody(request);
      if (!body) return fail(400, 'expected {chip}');
      // Read per request, so a secret set or removed applies at once.
      const keys = issueManagerKeys(storage, op, this.env.CARD_MANAGER_MASTER, body, now, id => cardOwner(sql, id));
      return json(200, {ok: true, ...keys});
    }
    const reset = /^\/registry\/cards\/([^/]+)\/(reset-check|reset)$/.exec(path);
    if (reset) {
      const shortId = reset[1]!;
      if (!isShortId(shortId)) return fail(400, 'short_id must be 8 lower-case hex characters');
      const checking = reset[2] === 'reset-check';
      if (method !== (checking ? 'GET' : 'POST')) return fail(405, checking ? 'use GET' : 'use POST');
      // The chip the phone read: on the check as ?chip=, in the completion's body.
      const body = checking ? null : await objectBody(request);
      if (!checking && !body) return fail(400, 'the completion must be an object');
      const chip = checking ? url.searchParams.get('chip') : body!.chip;
      if (checking && chip !== null && !isChip(chip)) {
        return fail(400, 'chip must be the card’s CPLC id, cplc-v1: and 24 lower-case hex characters');
      }
      // Another operator's card, or chip, answers as if it did not exist, as the card routes do.
      if (resetOwner(sql, shortId, isChip(chip) ? chip : null) !== op) return fail(404, `no card ${shortId}`);
      expireLoads(storage, shortId, now);
      if (checking) {
        return json(200, {ok: true, ...resetCheck(sql, op, shortId, isChip(chip) ? chip : null)});
      }
      return json(200, {ok: true, ...completeReset(storage, op, shortId, body!, now)});
    }
    const attest = /^\/registry\/cards\/([^/]+)\/attestation$/.exec(path);
    if (attest) {
      if (method !== 'POST') return fail(405, 'use POST');
      if (!isShortId(attest[1])) return fail(400, 'short_id must be 8 lower-case hex characters');
      const body = await objectBody(request);
      if (!body) return fail(400, 'expected {attest_pub}');
      return json(200, {ok: true, ...(await recordAttestation(storage, this.env.CERT_SIGNING_KEY, op, attest[1], body))});
    }
    if (path === '/registry/offline-reports' && method === 'GET') {
      const id = url.searchParams.get('short_id');
      if (id !== null && !isShortId(id)) return fail(400, 'short_id must be 8 lower-case hex characters');
      return json(200, {ok: true, reports: listOfflineReports(sql, op, id)});
    }
    if (path === '/registry/backup-status' && method === 'GET') {
      // 503 until a backup succeeded, and when the last success is over 26 hours old.
      const status = backupStatus(sql, Boolean(this.env.BACKUPS && this.env.BACKUP_RECIPIENT), now);
      return json(status.stale ? 503 : 200, {ok: !status.stale, ...status});
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
    const claim = /^\/till-claims\/([^/]+)(\/approve)?$/.exec(path);
    if (claim) {
      if (claim[2]) {
        if (method !== 'POST') return fail(405, 'use POST');
        approveClaim(storage, op, claim[1]!, now);
        return json(200, {ok: true});
      }
      if (method !== 'GET') return fail(405, 'use GET');
      return json(200, {ok: true, ...readClaim(sql, op, claim[1]!, now)});
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
    expireLoads(storage, shortId, now);
    if (rest === '/state' && method === 'GET') {
      return json(200, {ok: true, ...cardState(storage.sql, shortId)});
    }
    const firstKey = /^\/slots\/(\d{1,2})\/key$/.exec(rest);
    const keying = method === 'POST' && (firstKey !== null || rest === '/slot-keys');
    if (caller.kind !== 'till' && !keying) {
      return fail(403, 'only a till records charges and renews slot keys');
    }
    const closed = closedReason(storage.sql, shortId);
    if (closed?.reset) {
      // Its slot keys were deleted with the applet: nothing new may land under them.
      return fail(410, `card ${shortId} was reset; it takes no more charges`);
    }
    if (closed) {
      // Its slot keys were replaced on the card when it was finished.
      return fail(410, `card ${shortId} was finished as ${closed.replacedBy}; it takes no more charges`);
    }
    if (!pinIsSet(storage.sql, shortId)) {
      // Nothing may be spent from it, so nothing may be put on it.
      return fail(409, `card ${shortId} has no PIN set; it takes no charges or loads`);
    }
    const body = await objectBody(request);
    if (!body) return fail(400, 'expected a JSON object');
    if (keying) {
      const keys = parseSlotKeys(body, firstKey ? Number(firstKey[1]) : undefined);
      return json(200, {ok: true, ...addSlotKeys(storage, shortId, keys)});
    }
    // Refused above already; repeated so the type knows the caller is a till.
    if (caller.kind !== 'till') {
      return fail(403, 'only a till records charges and renews slot keys');
    }
    if (rest === '/intent' && method === 'POST') {
      return json(200, {ok: true, ...recordIntent(storage, shortId, caller.till.till_id, body, now)});
    }
    if (rest === '/submitted' && method === 'POST') {
      return json(200, {ok: true, ...recordSubmitted(storage, shortId, body)});
    }
    if (rest === '/outcome' && method === 'POST') {
      const {release, ...done} = recordOutcome(storage, shortId, body, now);
      if (release) {
        await recordReleases(storage, this.certSecret(), shortId, release.txid, release.slots, chargeRejected(release.txid), now);
      }
      return json(200, {ok: true, ...done});
    }
    if (rest === '/offline-charges' && method === 'POST') {
      const done = recordOfflineCharge(storage, shortId, caller.till.till_id, body, now);
      if (!done.recorded) {
        if (done.release) {
          const {txid, slots} = done.release;
          await recordReleases(storage, this.certSecret(), shortId, txid, slots, reportRefused(txid, shortId), now);
        }
        return json(409, {ok: false, reason: 'double_spend', conflicts: done.conflicts});
      }
      return json(200, {ok: true, version: done.version});
    }
    const slot = /^\/slots\/(\d{1,2})$/.exec(rest);
    if (slot && method === 'PUT') {
      return json(200, {ok: true, ...renewSlot(storage, shortId, Number(slot[1]), body)});
    }
    return fail(404, 'no such route');
  }

  /** The holder routes (`loads.ts`). Any card in the registry, whichever operator's. */
  private async holderRoute(request: Request, shortId: string, what: string, now: number): Promise<Response> {
    const storage = this.ctx.storage;
    if (!isShortId(shortId)) {
      return fail(400, 'short_id must be 8 lower-case hex characters');
    }
    if (cardOwner(storage.sql, shortId) === null) {
      return fail(404, `no card ${shortId}`);
    }
    const closed = closedReason(storage.sql, shortId);
    if (closed?.reset) {
      // A load under deleted keys could never be spent: the card is blank now.
      return fail(410, `card ${shortId} was reset; load the card's new id`);
    }
    if (closed) {
      // The card was finished under new keys: these are gone from it.
      return fail(410, `card ${shortId} was finished as ${closed.replacedBy}; load that id`);
    }
    if (!pinIsSet(storage.sql, shortId)) {
      // A card with no PIN is one the provisioner did not finish: nothing may be loaded on it.
      return fail(409, `card ${shortId} has no PIN set; it takes no loads`);
    }
    expireLoads(storage, shortId, now);
    const method = request.method;
    if (what === 'slots') {
      if (method !== 'GET') return fail(405, 'use GET');
      return json(200, {ok: true, ...freeSlots(storage.sql, shortId)});
    }
    if (method !== 'POST') return fail(405, 'use POST');
    const body = await objectBody(request);
    if (!body) return fail(400, 'expected a JSON object');
    spendBudget(storage, shortId, now);
    if (what === 'load') {
      return json(200, {ok: true, ...recordLoad(storage, shortId, body, now)});
    }
    return json(200, {ok: true, ...recordLoadOutcome(storage, shortId, body, now)});
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
        'tills', 'sessions', 'login_attempt_budget', 'federations', 'backup_runs',
        'federation_keys', 'holder_loads', 'holder_budget', 'manager_keys', 'card_resets',
        'till_claims', 'till_claim_budget', 'offline_reports', 'releases',
      ]) {
        sql.exec(`DELETE FROM ${table}`);
      }
      this.releaseKeyOff = false;
      return json(200, {ok: true});
    }
    if (path === '/test/reset-notes') {
      sql.exec('DELETE FROM notes');
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
    if (path === '/test/cert-key') {
      const b = (await objectBody(request)) ?? {};
      this.releaseKeyOff = b.off === true;
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
    if (url.pathname.startsWith('/test/') || url.pathname.startsWith('/internal/')) {
      return fail(404, 'no such route');
    }
    // One global object: every request lands on the same writer.
    const stub = env.REGISTRY.get(env.REGISTRY.idFromName('registry'));
    return stub.fetch(new Request(`https://registry.internal${url.pathname}${url.search}`, request));
  },
};
