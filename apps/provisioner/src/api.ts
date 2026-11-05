import {LOAD_FILE_SHA256} from './cardkit';
import {decodeInvite} from './federation';
import type {
  CardOps,
  ManagerKeysReply,
  TillClaimInfo,
  TillOps,
  TillRow,
  ResetCheck,
  ResetCompletion,
  ResetRecorded,
  ResumeCheck,
} from './ops';

/**
 * The one service the app talks to (docs/OPS-PLAN.md § 3), plus the registry
 * outbox. Ported from cashucard `apps/provisioner/src/api.ts`: the transport
 * (timeout, typed errors, the service's own reason strings surfaced verbatim)
 * and the outbox are the same design; the paths and the operation names are
 * this repo's.
 *
 * **Hosted** (`RegistryClient`): operator sessions, batches and the idempotent
 * card rows. Paths are `POST /auth/operator/session`, `POST /registry/batches`,
 * `POST /registry/cards`, `GET /registry/cards?batch_id=`. Cashu's
 * `/provisioning/*` attempt and bind surface is NOT carried over: this app
 * installs the applet and keys the card from the phone. What is carried over
 * is its key issuing, keyed by chip rather than by attempt (OPS-PLAN O5):
 * `POST /registry/manager-keys` answers a card's own card-manager key set, and
 * `GET /registry/cards/:id/reset-check` and `POST /registry/cards/:id/reset`
 * bracket a reset (the second is its completion, once the card is proved
 * factory).
 *
 * There is no local client. The sidecar that keyed and loaded cards over a
 * relay left the operator's loop: the phone keys the card in the install tap
 * (`cardSetup.ts`), and the holder's wallet loads it (OPS-PLAN § 4, § 6).
 *
 * The bearer token is a 12 h secret held in memory only (the threat model's
 * confiscated-phone row assumes device loss of the token and nothing more); no
 * Keychain dependency in v1. A PIN is never in a body this module sends.
 */

/* ------------------------------------------------------------------------- */
/* Errors and transport.                                                      */
/* ------------------------------------------------------------------------- */

/** The hosted registry's own reasons, surfaced verbatim (plan § 3, S1: never improvised). */
export type ServiceOperation =
  | 'sign-in'
  | 'sign-out'
  | 'create-batch'
  | 'post-card'
  | 'list-cards'
  | 'list-federations'
  | 'manager-keys'
  | 'resume-check'
  | 'reset-check'
  | 'complete-reset'
  | 'list-tills'
  | 'revoke-till'
  | 'claim-info'
  | 'approve-claim';

export class ApiServiceError extends Error {
  constructor(
    readonly reason: string,
    readonly status?: number,
    /** The service's machine code, when it sent one; the registry sends `reason` only. */
    readonly code?: string,
    readonly operation: ServiceOperation | null = null,
  ) {
    super(reason);
    this.name = 'ApiServiceError';
  }
}

export class ApiUnreachableError extends Error {
  readonly cause?: unknown;
  constructor(cause?: unknown) {
    super('the record service cannot be reached');
    this.name = 'ApiUnreachableError';
    this.cause = cause;
  }
}

export class ApiCancelledError extends Error {
  constructor() {
    super('request-cancelled');
    this.name = 'ApiCancelledError';
  }
}

const TIMEOUT_MS = 10_000;
// Deployed 600k PBKDF2 verification takes 8–12s; retain a bounded login.
const SIGN_IN_TIMEOUT_MS = 30_000;

/** The transport, injectable so the tests never open a socket. */
export type Fetcher = (url: string, init?: RequestInit) => Promise<Response>;

export interface ClientOptions {
  readonly baseUrl: string;
  readonly fetcher?: Fetcher;
  readonly timeoutMs?: number;
  readonly signal?: AbortSignal;
}

interface CallOptions {
  readonly token?: string | null;
  readonly body?: unknown;
  readonly timeoutMs?: number;
  readonly signal?: AbortSignal;
  /** The operation this request is; an error names it, never the path. */
  readonly operation: ServiceOperation;
}

/** A parsed JSON body: an object, or `null` when the reply carried none. */
type JsonBody = Record<string, unknown> | null;

/**
 * The shared transport for both clients: one `AbortController` (the caller's
 * signal forwarded, a timer for the budget), a JSON in, JSON out, and the two
 * error classes the rest of the app branches on. A response that is not JSON
 * is still a response: its status becomes an `ApiServiceError`, never an
 * "unreachable". The body is parsed once, here, and every consumer reads its
 * named fields with `typeof`/`Array.isArray`/`Number.isSafeInteger`.
 */
abstract class HttpTransport {
  private readonly fetcher: Fetcher;
  private readonly timeoutMs: number;

  constructor(protected readonly options: ClientOptions) {
    this.fetcher = options.fetcher ?? ((u, i) => fetch(u, i));
    this.timeoutMs = options.timeoutMs ?? TIMEOUT_MS;
  }

  private async call(
    method: string,
    path: string,
    call: CallOptions,
  ): Promise<Record<string, unknown>> {
    const signal = call.signal ?? this.options.signal;
    // The per-operation constant is what this call needs at least; a caller's
    // narrower budget (a test's 20 ms, a screen's shorter deadline) wins, and
    // never widens it.
    const timeoutMs = Math.min(
      call.timeoutMs ?? this.timeoutMs,
      this.timeoutMs,
    );
    if (signal?.aborted) {
      throw new ApiCancelledError();
    }
    const controller = new AbortController();
    let timedOut = false;
    const forwardAbort = () => controller.abort();
    signal?.addEventListener('abort', forwardAbort, {once: true});
    // A cancellation has to settle even when the transport ignores its
    // signal (a hung socket on a phone), so it races the request below.
    let rejectCancellation: (error: ApiCancelledError) => void = () => undefined;
    const cancellation = new Promise<never>((_resolve, reject) => {
      rejectCancellation = reject;
    });
    cancellation.catch(() => undefined);
    const onAbort = () => rejectCancellation(new ApiCancelledError());
    controller.signal.addEventListener('abort', onAbort, {once: true});
    const timer = setTimeout(() => {
      timedOut = true;
      controller.abort();
    }, timeoutMs);
    try {
      const request = (async () => {
        const res = await this.fetcher(this.options.baseUrl + path, {
          method,
          headers: {
            ...(call.token ? {Authorization: `Bearer ${call.token}`} : {}),
            ...(call.body !== undefined
              ? {'Content-Type': 'application/json'}
              : {}),
          },
          body: call.body !== undefined ? JSON.stringify(call.body) : undefined,
          signal: controller.signal,
        } as RequestInit);
        let json: JsonBody = null;
        try {
          const parsed: unknown = await res.json();
          json = parseBody(parsed);
        } catch {
          json = null;
        }
        // A 2xx that says it failed is a failure. The sidecar answers 202 with
        // `ok: false` when it cannot say whether a card was credited; reading
        // that as success would hand back an issue with no txid and no notes.
        if (!res.ok || json?.ok === false) {
          throw new ApiServiceError(
            reasonOf(json, res.status),
            res.status,
            codeOf(json),
            call.operation,
          );
        }
        return json ?? {};
      })();
      return await Promise.race([request, cancellation]);
    } catch (error) {
      if (signal?.aborted) {
        throw new ApiCancelledError();
      }
      if (timedOut) {
        throw new ApiUnreachableError(
          new Error(`the request exceeded ${timeoutMs} ms`),
        );
      }
      if (error instanceof ApiServiceError) {
        throw error;
      }
      throw new ApiUnreachableError(error);
    } finally {
      clearTimeout(timer);
      signal?.removeEventListener('abort', forwardAbort);
      controller.signal.removeEventListener('abort', onAbort);
    }
  }

  protected get(path: string, call: CallOptions) {
    return this.call('GET', path, call);
  }

  protected post(path: string, call: CallOptions) {
    return this.call('POST', path, call);
  }

  protected delete(path: string, call: CallOptions) {
    return this.call('DELETE', path, call);
  }
}

/**
 * One JSON value as a body. Arrays and primitives are not bodies this app
 * reads (`Reply` always sends an object), so they become `null` — a success
 * with no fields, or a failure with a bare status.
 */
function parseBody(value: unknown): JsonBody {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

/**
 * The service's own words for a failure. The sidecar sends `message` and a
 * `code`; the registry sends `reason`. Nothing is synthesised beyond the HTTP
 * status, which is what the app already knows.
 */
function reasonOf(
  json: Record<string, unknown> | null,
  status: number,
): string {
  if (json) {
    if (typeof json.reason === 'string' && json.reason) {
      return json.reason;
    }
    if (typeof json.message === 'string' && json.message) {
      return json.message;
    }
    if (typeof json.code === 'string' && json.code) {
      return json.code;
    }
  }
  return `HTTP ${status}`;
}

function codeOf(json: Record<string, unknown> | null): string | undefined {
  return json && typeof json.code === 'string' ? json.code : undefined;
}

/* ------------------------------------------------------------------------- */
/* Hosted registry (plan § 4).                                                */
/* ------------------------------------------------------------------------- */

/**
 * One card's registry row. The plan (§ 4) adds two fields Cashu's row does not
 * have: `pin_set` and `pin_policy`, the batch's. It still holds no PIN, no key
 * and no ecash — only the fact that a card was provisioned and its state.
 *
 * `body()` is the wire body and the outbox's frozen bytes: one function, so a
 * field added here cannot be added to one of the two and not the other.
 */
export interface CardRegistryRow {
  readonly short_id: string;
  readonly batch_id: number;
  /** Epoch ms; the card's installed-at, as the loop recorded it. */
  readonly installed_at: number;
  /** Which build installed it: BUILD.loadSha256. */
  readonly load_sha256: string;
  /** Whether SET_PIN succeeded on this card. */
  readonly pin_set: boolean;
  /** The batch's policy: one PIN for all, or one per card. */
  readonly pin_policy: 'batch' | 'perCard';
  /** Notes issued onto the card, satoshi. Zero is a keyed, PIN'd, empty card. */
  readonly notes_issued: number;
  /**
   * The eight slot keys, x-only hex, slot 0 first (OPS-PLAN § 5): the public
   * half only, which a till needs to find the card's notes. Absent on a row
   * whose key setup did not finish.
   */
  readonly slot_pubkeys?: readonly string[];
  /**
   * The chip's CPLC id (cardkit `readChipIdentity`), which ops keys the card's
   * manager keys on. Absent on a row from before O5.
   */
  readonly chip?: string;
  /**
   * The short id the card answered before a resume tap re-keyed it (OPS-PLAN
   * O5); ops closes that row in the same transaction. Absent on every other row.
   */
  readonly supersedes?: string;
}

export interface OperatorSession {
  readonly operator_id: number;
  /** The bearer token, in memory only. */
  readonly token: string;
}

/** A batch as the registry stores it (plan § 4: federation id, PIN policy, count, load sha). */
export interface RegistryBatch {
  readonly batch_id: number;
  readonly federation_id: string;
  readonly count: number;
  readonly pin_policy: 'batch' | 'perCard';
  readonly load_sha256: string;
  /** Epoch ms, from the service's clock. */
  readonly created_at: number;
}

/** What the app sends to open a batch. Server-assigned fields are not in it. */
export interface BatchRequest {
  readonly federation_id: string;
  readonly count: number;
  readonly pin_policy: 'batch' | 'perCard';
  readonly load_sha256: string;
}

/** A card row as the registry answers it: what was stored, and when. */
export interface StoredCard extends CardRegistryRow {
  readonly received_at: number;
}

export class RegistryClient extends HttpTransport {
  /** POST /auth/operator/session — the S1 call. */
  async operatorSession(
    name: string,
    password: string,
    signal?: AbortSignal,
  ): Promise<OperatorSession> {
    const json = await this.post('/auth/operator/session', {
      body: {name, password},
      timeoutMs: SIGN_IN_TIMEOUT_MS,
      signal,
      operation: 'sign-in',
    });
    if (
      !Number.isSafeInteger(json.operator_id) ||
      (json.operator_id as number) <= 0 ||
      typeof json.token !== 'string' ||
      !json.token
    ) {
      throw new ApiServiceError(
        'the operator session is malformed',
        200,
        undefined,
        'sign-in',
      );
    }
    // The plan drops Cashu's `env` split (§ 3, S5): this applet has no deploy
    // keys to rotate, so a key environment is not part of a session here. A
    // service that still sends one is ignored rather than rejected.
    return {operator_id: json.operator_id as number, token: json.token};
  }

  /** POST /auth/operator/signout — the registry revokes this token. */
  async signOut(token: string): Promise<void> {
    await this.post('/auth/operator/signout', {
      token,
      body: {},
      operation: 'sign-out',
    });
  }

  /** POST /registry/batches. */
  async createBatch(
    token: string,
    request: BatchRequest,
  ): Promise<RegistryBatch> {
    const json = await this.post('/registry/batches', {
      token,
      body: request,
      operation: 'create-batch',
    });
    const batch: unknown = json.batch;
    if (
      typeof batch !== 'object' ||
      batch === null ||
      !Number.isSafeInteger((batch as RegistryBatch).batch_id)
    ) {
      throw new ApiServiceError(
        'the batch is malformed',
        200,
        undefined,
        'create-batch',
      );
    }
    return batch as unknown as RegistryBatch;
  }

  /**
   * POST /registry/cards — idempotent on the service: the identical body
   * answers 200, a changed field for the same `short_id` answers 409. That is
   * exactly why `RegistryOutbox` exists: it re-posts frozen bytes.
   */
  async postCard(token: string, row: CardRegistryRow): Promise<StoredCard> {
    const json = await this.post('/registry/cards', {
      token,
      body: row,
      operation: 'post-card',
    });
    // The service answers what it stored; a service that answers nothing but
    // 200 stored exactly what was sent, so the sent row is the record.
    const stored: unknown = json.stored;
    return (
      typeof stored === 'object' && stored !== null ? stored : row
    ) as StoredCard;
  }

  /**
   * GET /registry/federations — names the operator's tills read from each
   * federation's config. The phone cannot ask the guardians itself: an invite
   * may name only an iroh node (OPS-PLAN § 7, O3 design).
   */
  async federationNames(token: string): Promise<Record<string, string>> {
    const json = await this.get('/registry/federations', {
      token,
      operation: 'list-federations',
    });
    const names: Record<string, string> = {};
    if (Array.isArray(json.federations)) {
      for (const f of json.federations as unknown[]) {
        const r = f as {federation_id?: unknown; name?: unknown};
        if (typeof r?.federation_id === 'string' && typeof r.name === 'string') {
          names[r.federation_id] = r.name;
        }
      }
    }
    return names;
  }

  /**
   * POST /registry/manager-keys — the card's own card-manager key set,
   * derived and escrowed by ops on the first ask and the same bytes on every
   * repeat. The reply is checked here, so a malformed set never reaches a
   * PUT KEY; it is returned to the tap and never stored.
   */
  async managerKeys(
    token: string,
    chip: string,
    shortId?: string,
  ): Promise<ManagerKeysReply> {
    const json = await this.post('/registry/manager-keys', {
      token,
      body: shortId === undefined ? {chip} : {chip, short_id: shortId},
      operation: 'manager-keys',
    });
    const hex16 = (v: unknown) => typeof v === 'string' && /^[0-9a-f]{32}$/.test(v);
    if (
      json.chip !== chip ||
      !Number.isSafeInteger(json.key_version) ||
      !hex16(json.enc) ||
      !hex16(json.mac) ||
      !hex16(json.dek)
    ) {
      throw new ApiServiceError(
        'the card-manager keys are malformed',
        200,
        undefined,
        'manager-keys',
      );
    }
    return {
      chip,
      key_version: json.key_version as number,
      enc: json.enc as string,
      mac: json.mac as string,
      dek: json.dek as string,
    };
  }

  /**
   * POST /registry/resume-check — whether a card an install tap left keyed
   * with no PIN may be finished. A reply that does not add up is never read
   * as "resumable".
   */
  async resumeCheck(token: string, chip: string, shortId: string): Promise<ResumeCheck> {
    const json = await this.post('/registry/resume-check', {
      token,
      body: {chip, short_id: shortId},
      operation: 'resume-check',
    });
    if (json.short_id !== shortId || json.chip !== chip || typeof json.resumable !== 'boolean') {
      throw new ApiServiceError('the resume check is malformed', 200, undefined, 'resume-check');
    }
    return {
      short_id: shortId,
      chip,
      known: json.known === true,
      resumable: json.resumable,
      reasons: Array.isArray(json.reasons)
        ? json.reasons.filter((r): r is string => typeof r === 'string')
        : [],
    };
  }

  /**
   * GET /registry/cards/:id/reset-check?chip= — what ops holds for the card.
   * The chip lets ops find a card it knows only by its chip (a `needsReset`
   * install), and say whether the holder's PIN is needed at all.
   */
  async resetCheck(token: string, shortId: string, chip?: string): Promise<ResetCheck> {
    const query = chip === undefined ? '' : `?chip=${encodeURIComponent(chip)}`;
    const json = await this.get(
      `/registry/cards/${encodeURIComponent(shortId)}/reset-check${query}`,
      {token, operation: 'reset-check'},
    );
    const count = (v: unknown) => (Number.isSafeInteger(v) ? (v as number) : NaN);
    const check: ResetCheck = {
      short_id: String(json.short_id),
      chip: typeof json.chip === 'string' ? json.chip : null,
      known: json.known !== false,
      // Only an explicit false skips the PIN: an older registry's reply asks for it.
      needs_holder_pin: json.needs_holder_pin !== false,
      empty: json.empty === true,
      resettable: json.resettable === true,
      reasons: Array.isArray(json.reasons)
        ? json.reasons.filter((r): r is string => typeof r === 'string')
        : [],
      live_notes: count(json.live_notes),
      live_msat: count(json.live_msat),
      pending_notes: count(json.pending_notes),
      pending_msat: count(json.pending_msat),
      open_charges: count(json.open_charges),
      open_loads: count(json.open_loads),
    };
    if (
      check.short_id !== shortId ||
      (chip !== undefined && !check.needs_holder_pin && check.chip !== chip) ||
      [
        check.live_notes,
        check.pending_notes,
        check.open_charges,
        check.open_loads,
      ].some(Number.isNaN)
    ) {
      // A check that does not add up is never read as "empty".
      throw new ApiServiceError(
        'the reset check is malformed',
        200,
        undefined,
        'reset-check',
      );
    }
    return check;
  }

  /**
   * POST /registry/cards/:id/reset — the completion: the card is factory and
   * blank, proved on the card. Ops marks the old row reset and releases the
   * chip's escrowed keys. Idempotent on the service, so the outbox re-posts
   * the same body after a lost answer.
   */
  async completeReset(
    token: string,
    completion: ResetCompletion,
  ): Promise<ResetRecorded> {
    const json = await this.post(
      `/registry/cards/${encodeURIComponent(completion.short_id)}/reset`,
      {
        token,
        body: {
          short_id: completion.short_id,
          chip: completion.chip,
          gp_key_version: completion.gp_key_version,
          manager_keys: completion.manager_keys,
          package: completion.package,
          verified_at: completion.verified_at,
        },
        operation: 'complete-reset',
      },
    );
    const stranded = json.stranded as {live_notes?: unknown; pending_notes?: unknown} | undefined;
    const n = (v: unknown) => (Number.isSafeInteger(v) ? (v as number) : 0);
    return {
      short_id: completion.short_id,
      released: json.released === true,
      stranded_notes: n(stranded?.live_notes) + n(stranded?.pending_notes),
    };
  }

  /** GET /registry/cards?batch_id= — the summary's refresh. */
  async listCards(token: string, batchId: number): Promise<StoredCard[]> {
    const json = await this.get(
      `/registry/cards?batch_id=${encodeURIComponent(String(batchId))}`,
      {token, operation: 'list-cards'},
    );
    const cards = json.cards;
    if (!Array.isArray(cards)) {
      throw new ApiServiceError(
        'the card list is malformed',
        200,
        undefined,
        'list-cards',
      );
    }
    return cards as StoredCard[];
  }

  /** GET /tills — the operator's tills, revoked ones included. */
  async listTills(token: string): Promise<TillRow[]> {
    const json = await this.get('/tills', {token, operation: 'list-tills'});
    if (!Array.isArray(json.tills)) {
      throw new ApiServiceError('the till list is malformed', 200, undefined, 'list-tills');
    }
    const tills: TillRow[] = [];
    for (const t of json.tills as unknown[]) {
      const r = t as Partial<Record<keyof TillRow, unknown>> | null;
      if (
        r &&
        Number.isSafeInteger(r.till_id) &&
        typeof r.name === 'string' &&
        typeof r.created_at === 'number'
      ) {
        tills.push({
          till_id: r.till_id as number,
          name: r.name,
          created_at: r.created_at,
          revoked_at: typeof r.revoked_at === 'number' ? r.revoked_at : null,
        });
      }
    }
    return tills;
  }

  /** DELETE /tills/:id — that till's token stops working at once. */
  async revokeTill(token: string, tillId: number): Promise<void> {
    if (!Number.isSafeInteger(tillId) || tillId <= 0) {
      throw new Error('a till id is a positive integer');
    }
    await this.delete(`/tills/${tillId}`, {token, operation: 'revoke-till'});
  }

  /** GET /till-claims/:code — the claim a till started (docs/TILL-CLAIM.md). */
  async claimInfo(token: string, code: string): Promise<TillClaimInfo> {
    const json = await this.get(`/till-claims/${encodeURIComponent(claimPath(code))}`, {
      token,
      operation: 'claim-info',
    });
    if (
      typeof json.name !== 'string' ||
      typeof json.created_at !== 'number' ||
      typeof json.expires_at !== 'number' ||
      (json.status !== 'pending' && json.status !== 'approved')
    ) {
      throw new ApiServiceError('the till claim is malformed', 200, undefined, 'claim-info');
    }
    return {
      name: json.name,
      created_at: json.created_at,
      expires_at: json.expires_at,
      status: json.status,
    };
  }

  /** POST /till-claims/:code/approve — ties the claim to this operator. */
  async approveClaim(token: string, code: string): Promise<void> {
    await this.post(`/till-claims/${encodeURIComponent(claimPath(code))}/approve`, {
      token,
      body: {},
      operation: 'approve-claim',
    });
  }
}

/**
 * A claim code as the registry is asked it: `XXXX-XXXX`, upper case, the form
 * the till shows and its QR carries. Refuses anything else before it is sent.
 */
function claimPath(code: string): string {
  const normal = normalizeClaimCode(code);
  if (normal === null) {
    throw new Error('not a till claim code');
  }
  return normal;
}

/**
 * The code's alphabet (docs/TILL-CLAIM.md): no 0, O, 1, I or L, so what an
 * operator reads off a till cannot be misread.
 */
const CLAIM_CODE = /^[A-HJKMNP-Z2-9]{8}$/;

/**
 * A typed or scanned claim code as `XXXX-XXXX`: upper case, with or without
 * the dash and spaces. Null when it is not eight characters of the alphabet.
 */
export function normalizeClaimCode(input: string): string | null {
  const bare = input.toUpperCase().replace(/[\s-]/g, '');
  return CLAIM_CODE.test(bare) ? `${bare.slice(0, 4)}-${bare.slice(4)}` : null;
}

/**
 * The registry outbox (plan § 5: ~55 lines). Rows enter `pending: true` when
 * the POST fails and are re-posted on interval and app-foreground until the
 * service answers; **the body is frozen at first write**.
 *
 * The freeze is the whole point: the row is captured once, on `add`, and
 * every retry re-sends those bytes. A retry that re-read live state could
 * carry one field moved on (a PIN set late, a note count settled) against a
 * `short_id` the service already stored — a 409 with no way to tell the two
 * writes apart, which the bench cannot resolve. A 409 here therefore keeps
 * the row pending and visible rather than dropping it.
 */
export class RegistryOutbox {
  private readonly rows: {row: CardRegistryRow; pending: boolean}[] = [];

  constructor(private readonly client: RegistryClient) {}

  /** The row body is captured here, once; retries re-post exactly these bytes. */
  add(row: CardRegistryRow): void {
    // A shallow copy is enough: every field is a string, a number or a
    // boolean, so there is nothing below it to drift.
    this.rows.push({row: {...row}, pending: true});
  }

  get pendingRows(): readonly CardRegistryRow[] {
    return this.rows.filter(r => r.pending).map(r => r.row);
  }

  get all(): readonly {row: CardRegistryRow; pending: boolean}[] {
    return this.rows;
  }

  /**
   * One retry pass. Resolves when every row has been answered or exhausted its
   * pass; answered rows flip to `pending: false` in place. A 401 (the 12 h
   * token dead) aborts the pass with that error: no row can succeed behind it,
   * and the caller signs out on it instead of re-posting 401s forever.
   */
  async retryAll(
    token: string,
  ): Promise<{posted: number; stillPending: number}> {
    let posted = 0;
    for (const entry of this.rows) {
      if (!entry.pending) {
        continue;
      }
      try {
        // The frozen body: entry.row, never a re-read of live state.
        await this.client.postCard(token, entry.row);
        entry.pending = false;
        posted++;
      } catch (e) {
        if (e instanceof ApiServiceError && e.status === 401) {
          throw e;
        }
        // A 409, an unreachable service or a 5xx: next pass. The row stays
        // pending and visible; nothing a programmed card did is dropped
        // because a network or a conflict got in the way of reporting it.
        continue;
      }
    }
    return {
      posted,
      stillPending: this.rows.filter(r => r.pending).length,
    };
  }
}

/* ------------------------------------------------------------------------- */
/* The registry as the store's StoreApi                                        */
/* ------------------------------------------------------------------------- */

/**
 * The store's own shapes, which this adapter implements. A type-only import,
 * so no runtime edge: `store.tsx` does not import `api.ts`.
 */
type StoreRow = import('./store').RecordRow;
type StoreBatchConfig = import('./store').BatchConfig;
type StoreSession = import('./store').Session;

/**
 * `RegistryClient` as `store.tsx`'s `StoreApi`. The store names the four calls
 * it needs in its own vocabulary — a session, a batch config, a loop row — and
 * passes the session only where it has one to hand, so the token is held here
 * between sign-in and the rows that need it. It is never persisted: the adapter
 * lives as long as the app's tree, and a sign-out drops it with the session.
 *
 * The outbox holds the rows the registry could not take live, so a programmed
 * card's record survives a network that went away.
 */
export class RegistryStoreApi implements CardOps, TillOps {
  private readonly outbox: RegistryOutbox;
  /** The batch id the registry answered; every card row names it. */
  private batchId: number | null = null;
  /** The bearer token from the last sign-in, in memory only: never persisted. */
  private current: string | null = null;

  constructor(
    private readonly client: RegistryClient,
    private readonly origin: string,
  ) {
    this.outbox = new RegistryOutbox(client);
  }

  get serviceUrl(): string {
    return this.origin;
  }

  async signIn(
    name: string,
    password: string,
    options?: {signal?: AbortSignal},
  ): Promise<{operatorId: number; token: string}> {
    const s = await this.client.operatorSession(name, password, options?.signal);
    this.current = s.token;
    return {operatorId: s.operator_id, token: s.token};
  }

  /** Revokes the session on the registry, then forgets it here. */
  async signOut(): Promise<void> {
    const token = this.current;
    this.current = null;
    if (token !== null) {
      await this.client.signOut(token);
    }
  }

  /** Federation names by decoded id, as the operator's tills reported them. */
  async federationNames(): Promise<Record<string, string>> {
    return this.client.federationNames(this.token());
  }

  /** A card's own card-manager keys, for the tap that rotates or resets it. */
  managerKeys(chip: string, shortId?: string): Promise<ManagerKeysReply> {
    return this.client.managerKeys(this.token(), chip, shortId);
  }

  resetCheck(shortId: string, chip?: string): Promise<ResetCheck> {
    return this.client.resetCheck(this.token(), shortId, chip);
  }

  resumeCheck(chip: string, shortId: string): Promise<ResumeCheck> {
    return this.client.resumeCheck(this.token(), chip, shortId);
  }

  completeReset(completion: ResetCompletion): Promise<ResetRecorded> {
    return this.client.completeReset(this.token(), completion);
  }

  /** Async, so a call before sign-in rejects rather than throws at the caller. */
  async listTills(): Promise<TillRow[]> {
    return this.client.listTills(this.token());
  }

  async revokeTill(tillId: number): Promise<void> {
    return this.client.revokeTill(this.token(), tillId);
  }

  async claimInfo(code: string): Promise<TillClaimInfo> {
    return this.client.claimInfo(this.token(), code);
  }

  async approveClaim(code: string): Promise<void> {
    return this.client.approveClaim(this.token(), code);
  }

  /** Open the batch once; a card row cannot be posted before it. */
  async postBatch(config: StoreBatchConfig): Promise<void> {
    const token = this.token();
    const batch = await this.client.createBatch(token, {
      // The federation's id, decoded from the invite on the phone; never the
      // invite text, which also carries guardian addresses (OPS-PLAN § 9.4).
      federation_id: decodeInvite(config.federation)?.federationId ?? config.federation,
      count: config.count,
      pin_policy: config.pinPolicy,
      load_sha256: LOAD_FILE_SHA256,
    });
    this.batchId = batch.batch_id;
  }

  /** Post one card row, or hold it in the outbox when the registry is away. */
  async postCard(row: StoreRow): Promise<void> {
    const token = this.token();
    const body = this.row(row);
    try {
      await this.client.postCard(token, body);
    } catch (e) {
      // A 401 is not a reason to keep a row: the pass behind it cannot succeed
      // either, and the store signs out on that error.
      if (e instanceof ApiServiceError && e.status === 401) {
        throw e;
      }
      // Anything else leaves the row queued with exactly these bytes, so what
      // a programmed card did is never dropped because reporting it failed.
      this.outbox.add(body);
    }
  }

  /** One retry pass; resolves to the rows the registry has now answered. */
  async retryPending(
    rows: readonly StoreRow[],
    session: StoreSession,
  ): Promise<readonly StoreRow[]> {
    const {posted} = await this.outbox.retryAll(session.token);
    if (posted === 0) {
      return [];
    }
    const answered = new Set(
      this.outbox.all
        .filter(r => !r.pending)
        .map(r => r.row.short_id),
    );
    return rows.filter(r => answered.has(r.id));
  }

  /**
   * The session's token. `postBatch`/`postCard` take no session argument, so a
   * call before sign-in is a bug of the caller's and is refused loudly rather
   * than posting an empty bearer token at the registry.
   */
  private token(): string {
    if (this.current === null) {
      throw new Error('the registry was asked before an operator signed in');
    }
    return this.current;
  }

  /** The loop's row as the registry's body. A row with no batch is refused. */
  private row(row: StoreRow): CardRegistryRow {
    if (this.batchId === null) {
      throw new Error('a card cannot be recorded before its batch');
    }
    return {
      short_id: row.id,
      batch_id: this.batchId,
      installed_at: row.at,
      load_sha256: LOAD_FILE_SHA256,
      pin_set: row.pinState === 'set',
      pin_policy: row.pinPolicy,
      // the loop records each note it wrote; the registry stores the total
      notes_issued: row.notesIssued.reduce((sum, n) => sum + n, 0),
      ...(row.slotKeys && row.slotKeys.length === 8
        ? {slot_pubkeys: [...row.slotKeys]}
        : {}),
      ...(row.chip ? {chip: row.chip} : {}),
      ...(row.supersedes && row.supersedes !== row.id ? {supersedes: row.supersedes} : {}),
    };
  }
}

/* ------------------------------------------------------------------------- */
/* Where each registry endpoint lives                                           */
/* ------------------------------------------------------------------------- */

/**
 * The origin for each named registry deployment. `settings.tsx` stores the
 * endpoint as a name (`'test' | 'production'`) rather than a URL, precisely so
 * the URL lives in one place; this is that place.
 *
 * Both name the same deployment: the registry is one Worker, and `test` and
 * `production` exist as separate names so a build can be pointed at a stage
 * that is not live without editing the URL. `registryOrigin` returns null for
 * an empty entry, and the app then cannot sign in — visibly, with a message
 * naming the build — instead of posting an operator's credentials at a host
 * someone guessed.
 *
 * Deployed 2026-10-31: Worker `fedimintcard-registry`, SQLite-backed Durable
 * Object, custom domain `registry.fedimintcard.org` (TLS valid). `test` is the
 * staging Worker, with its own storage and operators. Operators come from the
 * `OPERATOR_SETUP` Worker secret on each; no password is in this tree.
 */
export const REGISTRY_ORIGINS: Readonly<Record<string, string>> = Object.freeze({
  // Staging has no custom domain (registry/wrangler.toml `[env.staging]`).
  test: 'https://fedimintcard-registry-staging.old-cell-30e7.workers.dev',
  production: 'https://registry.fedimintcard.org',
} as const);

/** The origin for `endpoint`, or null when this build has none configured. */
export function registryOrigin(endpoint: string): string | null {
  const origin = REGISTRY_ORIGINS[endpoint];
  return typeof origin === 'string' && origin !== '' ? origin : null;
}
