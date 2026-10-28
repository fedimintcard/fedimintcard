import {LOAD_FILE_SHA256} from './cardkit';

/**
 * The two services the app talks to (docs/PROVISIONER-PLAN.md § 4), split by
 * what they touch, plus the registry outbox. Ported from cashucard
 * `apps/provisioner/src/api.ts`: the transport (timeout, typed errors, the
 * service's own reason strings surfaced verbatim) and the outbox are the same
 * design; the paths and the operation names are this repo's.
 *
 * **Hosted** (`RegistryClient`): operator sessions, batches and the idempotent
 * card rows. Paths are § 4's table — `POST /auth/operator/session`,
 * `POST /registry/batches`, `POST /registry/cards`,
 * `GET /registry/cards?batch_id=`. Cashu's `/provisioning/*` and `/keys/issue`
 * are NOT carried over: this app installs the applet from the phone and keys
 * the card through the sidecar, so the hosted service has no attempt, bind,
 * key-issuing or disposition surface to call.
 *
 * **Local** (`LocalClient`): the sidecar over loopback. Paths and bodies are
 * read from `host/sidecar/src/` itself, not assumed:
 *
 *   GET  /v1/health            till.rs  router(): {"ok": true}
 *   GET  /v1/status            till.rs  status():  {federation_id, federation_name,
 *                                wallet_balance_msat, card_relay_ok, busy}
 *   POST /v1/card/balance      till.rs  card_balance(): no body ->
 *                                {ok, live_msat, notes[{slot, msat, status}], free_slots}
 *   POST /v1/cards/precheck    § 4 "new" — not in till.rs yet
 *   POST /v1/cards/keySetup    § 4 "new" — not in till.rs yet
 *   POST /v1/cards/issue       § 4 "new" — not in till.rs yet
 *
 * The three "new" routes are P0's to add (`router()` at till.rs:532 is the
 * place). Their bodies and replies follow what the sidecar already computes —
 * `glue.rs` `CardWallet::topup` for the issue plan, `till.rs`
 * `balance_reply()` for the precheck's note list, `classify()` for the error
 * codes — so P0 implements against this file and the screen code is written
 * once. The `:line` numbers are deliberately absent: they move.
 *
 * Errors mirror the sidecar's own vocabulary: every failure body is
 * `{"ok": false, "code": "...", "message": "..."}` (`Reply::fail`), where
 * `code` is exactly the string `classify()` returns — `bad_request`,
 * `no_card`, `relay_unreachable`, `card_error`, `insufficient`,
 * `no_exact_plan`, `declined`, `undecided`, `recovering`, `busy`, `internal`.
 * The client never invents one.
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
  | 'create-batch'
  | 'post-card'
  | 'list-cards';

/** The sidecar's own operations. */
export type LocalOperation =
  | 'health'
  | 'status'
  | 'balance'
  | 'precheck'
  | 'key-setup'
  | 'issue';

export class ApiServiceError extends Error {
  constructor(
    readonly reason: string,
    readonly status?: number,
    /**
     * The service's machine code, when it sent one. The sidecar always does
     * (`code`); the registry sends `reason` only, so there it stays undefined.
     */
    readonly code?: string,
    readonly operation: ServiceOperation | LocalOperation | null = null,
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

/**
 * The sidecar's HTTP codes for failures the app must not present as a
 * transport problem: every one of them is a card or a federation answering,
 * not the wire. Kept as the strings `classify()` emits, so a new code the
 * sidecar adds shows up in a report verbatim instead of being mapped away.
 */
export const LOCAL_CODES = [
  'bad_request',
  'no_card',
  'relay_unreachable',
  'card_error',
  'insufficient',
  'no_exact_plan',
  'declined',
  'undecided',
  'recovering',
  'busy',
  'internal',
] as const;

export type LocalCode = (typeof LOCAL_CODES)[number];

const TIMEOUT_MS = 10_000;
// Deployed 600k PBKDF2 verification takes 8–12s; retain a bounded login.
const SIGN_IN_TIMEOUT_MS = 30_000;
// The issue step submits a transaction and waits for acceptance (glue.rs:
// 90 s for acceptance, plus the fetch of each output), so it gets the longest
// bounded budget in the app. A timeout here is not a failure that may be
// retried with a fresh key: the notes may already be the federation's, so the
// loop treats it as `undecided` and reconciles with `balance` first.
const ISSUE_TIMEOUT_MS = 120_000;

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
  readonly operation: ServiceOperation | LocalOperation;
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
export class RegistryStoreApi {
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

  /** Open the batch once; a card row cannot be posted before it. */
  async postBatch(config: StoreBatchConfig): Promise<void> {
    const token = this.token();
    const batch = await this.client.createBatch(token, {
      federation_id: config.federation,
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
 * Both are empty because neither deployment exists yet: P1c builds the
 * registry, and this repo records no host for it. `registryOrigin` returns
 * null for an unset one, and the app then cannot sign in — visibly, with a
 * message naming the build — instead of posting an operator's credentials at a
 * host someone guessed.
 */
export const REGISTRY_ORIGINS: Readonly<Record<string, string>> = Object.freeze({
  test: '',
  production: '',
} as const);

/** The origin for `endpoint`, or null when this build has none configured. */
export function registryOrigin(endpoint: string): string | null {
  const origin = REGISTRY_ORIGINS[endpoint];
  return typeof origin === 'string' && origin !== '' ? origin : null;
}

/* ------------------------------------------------------------------------- */
/* Local sidecar (host/sidecar/src/till.rs, § 4).                             */
/* ------------------------------------------------------------------------- */

/** GET /v1/status — till.rs `status()`. */
export interface SidecarStatus {
  readonly federation_id: string;
  readonly federation_name: string | null;
  readonly wallet_balance_msat: number;
  /** False means the phone's relay at :9876 is not answering (README: adb forward). */
  readonly card_relay_ok: boolean;
  /** A till job is in flight right now; a card command now is answered `busy`. */
  readonly busy: boolean;
}

/** POST /v1/card/balance — till.rs `balance_reply()` and `status_str()`. */
export interface CardBalance {
  readonly live_msat: number;
  readonly notes: readonly {
    readonly slot: number;
    readonly msat: number;
    readonly status: 'live' | 'pending' | 'spent' | 'dead';
  }[];
  readonly free_slots: readonly number[];
}

/**
 * POST /v1/cards/precheck — read-only, nothing written. The protection
 * against wiping a used card is host-side (plan § 5, handoff § 5: the applet
 * has no GET_INFO, so cardkit cannot ask), and this reply is what the loop
 * shows before the sweep.
 */
export interface CardPrecheck {
  /** True when the applet answered SELECT with 0200; false when it answered 6A82. */
  readonly applet_present: boolean;
  /** The card manager (ISD) says it is there and unlocked for INSTALL. */
  readonly isd_open: boolean;
  /** Slots that hold a key: what the sweep would destroy. */
  readonly keyed_slots: readonly number[];
  /** Live notes, satoshi. Not zero plus a successful install means a replace. */
  readonly live_msat: number;
  /** False for a used card: the loop must ask before it sweeps. */
  readonly safe_to_replace: boolean;
}

/** POST /v1/cards/keySetup — `card setup` then SET_PIN, one job, one card. */
export interface KeySetupRequest {
  /**
   * The batch PIN, or the per-card one. Never logged, never stored, never in
   * a registry body (§ 5 "PIN handling in the app"); the sidecar forwards it
   * to the card as `41 SET_PIN` and keeps nothing.
   */
  readonly pin: string;
  /** Replace the keys of a card the precheck said was used. The operator's explicit answer. */
  readonly replace: boolean;
}

export interface KeySetupResult {
  /** Slot, and the x-only public key `card_setup` generated into it, hex. */
  readonly keys: readonly {readonly slot: number; readonly pubkey: string}[];
  /**
   * The card's short id, as the sidecar computed it from the keys it just
   * generated. The app's own rule is `shortIdOf` in `store.tsx` (the last two
   * bytes of the first slot's key); this field is what the sidecar reported
   * for the same card, and the loop uses it only to label the run and key the
   * one-time PIN reveal.
   */
  readonly short_id: string;
  /** True only when SET_PIN answered 9000. */
  readonly pin_set: boolean;
}

/** POST /v1/cards/issue — `CardWallet::topup` (glue.rs:452), one job, replay-safe. */
export interface IssueRequest {
  /** Notes to create, msat, as `card topup` takes them. The plan previews this exact list. */
  readonly amounts_msat: readonly number[];
  /**
   * The idempotency key. A repeat with the same key returns the same result
   * (the sidecar writes the `PendingTx` before it signs); a repeat with a
   * different key for a spent card is a loss, so the key is minted once per
   * tap and never reused for a second attempt.
   */
  readonly client_ref: string;
}

/**
 * A successful issue. An undecided one is not this shape: the sidecar answers
 * it as a 202 with code `undecided` (`undecided_reply`, till.rs:335), which
 * arrives as an `ApiServiceError` — the loop reads `code` and the summary
 * shows the notes as pending, never as issued.
 */
export interface IssueResult {
  readonly client_ref: string;
  readonly txid: string;
  readonly notes: readonly {readonly slot: number; readonly msat: number}[];
  readonly live_msat: number;
}

/**
 * The sidecar, loopback only. No token: `till serve` says out loud that the
 * API has no authentication (till.rs:657) and the app reaches it on 127.0.0.1,
 * the same way the till does.
 */
export class LocalClient extends HttpTransport {
  /**
   * The sidecar's failures keep their own `code`, so the loop can tell a
   * pulled card (`no_card`) from a federation refusal (`insufficient`). A
   * relay that is not there (`relay_unreachable`) is reported as
   * `ApiUnreachableError` instead: nothing else can be tried until the cable
   * or `adb forward` is back, and the operator's next move is the same.
   */
  private async sidecar(
    method: 'GET' | 'POST',
    path: string,
    call: CallOptions,
  ): Promise<Record<string, unknown>> {
    try {
      return method === 'GET'
        ? await this.get(path, call)
        : await this.post(path, call);
    } catch (error) {
      if (
        error instanceof ApiServiceError &&
        (error.code === 'relay_unreachable' || error.status === 502)
      ) {
        throw new ApiUnreachableError(error);
      }
      throw error;
    }
  }

  /** GET /v1/health — the sign-in screen's "is the sidecar running" probe. */
  async health(signal?: AbortSignal): Promise<boolean> {
    const json = await this.sidecar('GET', '/v1/health', {
      signal,
      operation: 'health',
    });
    return json.ok === true;
  }

  /** GET /v1/status. */
  async status(signal?: AbortSignal): Promise<SidecarStatus> {
    const json = await this.sidecar('GET', '/v1/status', {
      signal,
      operation: 'status',
    });
    return {
      federation_id: String(json.federation_id ?? ''),
      federation_name:
        typeof json.federation_name === 'string'
          ? json.federation_name
          : null,
      wallet_balance_msat: Number(json.wallet_balance_msat ?? 0),
      card_relay_ok: json.card_relay_ok === true,
      busy: json.busy === true,
    };
  }

  /** POST /v1/card/balance — reconcile; needs the card on the relay, no body. */
  async balance(signal?: AbortSignal): Promise<CardBalance> {
    const json = await this.sidecar('POST', '/v1/card/balance', {
      signal,
      operation: 'balance',
    });
    return {
      live_msat: Number(json.live_msat ?? 0),
      notes: Array.isArray(json.notes)
        ? (json.notes as CardBalance['notes'])
        : [],
      free_slots: Array.isArray(json.free_slots)
        ? (json.free_slots as number[])
        : [],
    };
  }

  /** POST /v1/cards/precheck — read-only; a used card is refused, never swept. */
  async precheck(
    replace: boolean,
    signal?: AbortSignal,
  ): Promise<CardPrecheck> {
    const json = await this.sidecar('POST', '/v1/cards/precheck', {
      body: {replace},
      signal,
      operation: 'precheck',
    });
    return {
      applet_present: json.applet_present === true,
      isd_open: json.isd_open === true,
      keyed_slots: Array.isArray(json.keyed_slots)
        ? (json.keyed_slots as number[])
        : [],
      live_msat: Number(json.live_msat ?? 0),
      safe_to_replace: json.safe_to_replace === true,
    };
  }

  /**
   * POST /v1/cards/keySetup. The PIN goes in this body and nowhere else: the
   * request is the only copy, and nothing here or in the caller logs it.
   */
  async keySetup(
    request: KeySetupRequest,
    signal?: AbortSignal,
  ): Promise<KeySetupResult> {
    const json = await this.sidecar('POST', '/v1/cards/keySetup', {
      body: request,
      signal,
      operation: 'key-setup',
    });
    return {
      keys: Array.isArray(json.keys)
        ? (json.keys as KeySetupResult['keys'])
        : [],
      short_id: String(json.short_id ?? ''),
      pin_set: json.pin_set === true,
    };
  }

  /**
   * POST /v1/cards/issue. The longest budget in the app, because it waits for
   * a federation to accept a transaction. A 202 (`undecided`) is thrown as an
   * `ApiServiceError` with that code, like any other service answer.
   */
  async issue(
    request: IssueRequest,
    signal?: AbortSignal,
  ): Promise<IssueResult> {
    const json = await this.sidecar('POST', '/v1/cards/issue', {
      body: request,
      timeoutMs: ISSUE_TIMEOUT_MS,
      signal,
      operation: 'issue',
    });
    return {
      client_ref: String(json.client_ref ?? request.client_ref),
      txid: String(json.txid ?? ''),
      notes: Array.isArray(json.notes)
        ? (json.notes as IssueResult['notes'])
        : [],
      live_msat: Number(json.live_msat ?? 0),
    };
  }
}
