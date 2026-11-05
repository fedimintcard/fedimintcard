/**
 * The batch state machine: phases, stop kinds, the PIN policy and the loop
 * index (docs/PROVISIONER-PLAN.md § 5). The only place card state lives.
 *
 * Screens read from here and dispatch actions; none of them computes a card's
 * state, a count, a plan sum or a stop's meaning. That is the contract § 5
 * sets, and it is what makes the loop's counter and the summary's numbers one
 * fact each rather than two that must be kept in step.
 *
 * What this store deliberately does NOT hold:
 *
 * - **A PIN value.** Not in state, not in an action payload, not in a ref that
 *   outlives a batch. A batch carries its PIN *policy name* (§ 5: "recorded in
 *   the registry as a policy name only"), and the value travels from `pin.ts`
 *   (the draft in BatchSetup, the reveal slot in the loop) straight into the
 *   `SET_PIN` APDU in `session.ts`. The only PIN-adjacent fact recorded here is
 *   whether the card took it: `PinState`.
 * - **Anything on disk.** This module imports no storage. The keys this app
 *   writes at all are `prov.settings.v1` (settings.tsx) and api.ts's session
 *   and outbox records; none takes a PIN. A batch in flight is memory-only,
 *   and the summary says so when the app was relaunched (§ 6 P5).
 *
 * The shape is Cashu's `ProvisioningStore.tsx` — a provider with a phase, a
 * card-active flag and one action per card — trimmed to this applet, plus the
 * phases and stops this card adds (§ 5 "Stops", § 3 S3).
 */
import React, {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react';
import {AppState} from 'react-native';

import type {StopKind} from './cardkit';
import type {CardOps} from './ops';
import {
  useOptionalBatchPin,
  usePinReveal,
  type PinPolicy,
  type PinRevealContext,
} from './pin';

// ---------------------------------------------------------------------------
// The card path: phases and stops
// ---------------------------------------------------------------------------

/**
 * The phases one card goes through, in order. The first eight are cardkit's
 * `FEDIMINT_PLAN` (probe → selectIsd → auth → sweep → installForLoad → load →
 * installOpener/installCard → verify), folded to what the operator sees;
 * `rotate` moves the card manager to the card's own keys (OPS-PLAN O5), and
 * `keySetup` and `setPin` are this plan's additions (§ 3 S3), because on this
 * card a programmed card is not a usable one.
 */
export const CARD_PHASES = [
  'probe',
  'selectIsd',
  'auth',
  'sweep',
  'load',
  'install',
  'verify',
  'rotate',
  'keySetup',
  'setPin',
  'done',
] as const;

export type CardPhase = (typeof CARD_PHASES)[number];

/** Where a phase sits in the run; `phaseIndex('setPin')` is the PIN gate's line. */
export function phaseIndex(phase: CardPhase): number {
  return CARD_PHASES.indexOf(phase);
}

/**
 * The next phase after `phase`, for a loop that advances only by what the
 * engine reported — never by a timer. `done` stays `done`.
 */
export function nextPhase(phase: CardPhase): CardPhase {
  return CARD_PHASES[phaseIndex(phase) + 1] ?? 'done';
}

/**
 * Cardkit's four stops are carried over unchanged — `StopKind` is imported,
 * never redefined — and the rest are this card's: two from § 5's "Stops"
 * table, `noManagerKeys` (OPS-PLAN O5), when ops did not answer the card's
 * own keys and nothing was written, and `needsReset`, a keyed card with a PIN
 * this phone cannot prove, which only Settings → Reset a card recovers.
 *
 * A tap lifted while the card's keys or PIN were being written is not a stop
 * at all: the loop keeps the batch's place for the same card's next tap,
 * which finishes it (`installTap.ts`, `holdsPlace`).
 */
export type CardStop =
  | StopKind
  | 'differentCard'
  | 'pinNotSet'
  | 'noManagerKeys'
  | 'needsReset';

/** Every stop, in § 5's table order. */
export const CARD_STOPS: readonly CardStop[] = [
  'pulled',
  'refused',
  'wrongKeys',
  'guarded',
  'differentCard',
  'pinNotSet',
  'noManagerKeys',
  'needsReset',
];

/**
 * The stops that hold the batch until the operator acknowledges them. § 5:
 * `wrongKeys` is "blocks the batch until acknowledged"; `pinNotSet` is listed
 * as one of the two new stops whose card cannot be handed out, so it holds the
 * batch for the same decision.
 */
const BLOCKS_BATCH: Record<CardStop, boolean> = {
  pulled: false,
  refused: false,
  wrongKeys: true,
  guarded: false,
  differentCard: false,
  pinNotSet: true,
  // nothing was written: the next tap asks ops again
  noManagerKeys: false,
  // nothing more was tried against the PIN: the card waits for a reset
  needsReset: false,
};

/** The batch can also end on something that is not one card's: unanswered rows. */
export type BatchStop = CardStop | 'outboxPending';

/**
 * Whether a recorded run's PIN is on the card. `pinNotSet` is the only case
 * where `SET_PIN` succeeded and `VERIFY_PIN` did not; a run that stopped before
 * `setPin` has no PIN state to report, and `unknown` says that instead of
 * claiming `notSet`.
 */
export type PinState = 'set' | 'notSet' | 'unknown';

export function pinStateOf(
  outcome: Pick<CardOutcome, 'phase' | 'stop' | 'ok'>,
): PinState {
  if (outcome.stop === 'pinNotSet') {
    return 'notSet';
  }
  if (outcome.ok) {
    return 'set';
  }
  return phaseIndex(outcome.phase) > phaseIndex('setPin') ? 'set' : 'unknown';
}

/**
 * Whether a recorded card can be handed out. The registry's "programmed-only"
 * distinction (§ 5) is this predicate and nothing else: a `pinNotSet` card is
 * programmed and unspendable.
 */
export function isSpendable(
  outcome: Pick<CardOutcome, 'ok' | 'stop' | 'pinState'>,
): boolean {
  return outcome.ok && outcome.stop === undefined && outcome.pinState === 'set';
}

// ---------------------------------------------------------------------------
// The batch
// ---------------------------------------------------------------------------

export interface BatchConfig {
  /** The federation invite; the picker in Settings chose it. Never a PIN. */
  federation: string;
  /** The loop's denominator. */
  count: number;
  /** One PIN for the batch, or one generated per card: a policy, never a value. */
  pinPolicy: PinPolicy;
}

/** The outcome of one card's run, as the engine reports it. */
export interface CardOutcome {
  /** `true` only for a card installed, keyed and PIN-verified. */
  ok: boolean;
  /** Where the run stopped: `done` when it finished. */
  phase: CardPhase;
  /** The stop, when the run did not finish. */
  stop?: CardStop;
  /** The policy the batch used; the registry row names this, never a value. */
  pinPolicy: PinPolicy;
  /** Whether the card's PIN is set and verified. */
  pinState: PinState;
  /**
   * Notes the card received, in sat. Always empty: the provisioner loads no
   * value (docs/OPS-PLAN.md § 6). Kept because the registry row carries it.
   */
  notesIssued: readonly number[];
  /** The card's status word, when the card refused one. */
  sw?: number;
  /** The engine's own reason. Never a PIN, never a key. */
  reason?: string;
  /** The chip's CPLC id, once read; the registry row carries it (OPS-PLAN O5). */
  chip?: string;
}

/** One card's row on the summary and in the registry (§ 3 S4). */
export interface RecordRow extends CardOutcome {
  /** The card's short id: the last bytes of its applet public key, hex. */
  id: string;
  /** Epoch ms, when the row was written. */
  at: number;
  /** A registry write is outstanding; the summary shows it and retry re-posts. */
  pending: boolean;
  /**
   * The card's eight slot keys, x-only hex, slot 0 first, when key setup ran.
   * The registry keeps them so a till can find the card's notes (OPS-PLAN § 5).
   */
  slotKeys?: readonly string[];
  /** The short id this card answered before a resume tap re-keyed it. */
  supersedes?: string;
}

/** The loop's live view: the next card's number and what the card is doing. */
export interface LoopState {
  /** The card being programmed now, 1-based; `count + 1` once the batch is full. */
  index: number;
  /** The phase of the card on the reader, or `done` between cards. */
  phase: CardPhase;
  /** The card is on the reader and its run has not ended. */
  cardActive: boolean;
  /** The last LOAD block the card acknowledged; never a timed estimate. */
  loadProgress: {index: number; of: number} | null;
}

/** A stop as the summary groups it: one row per kind, not one per card. */
export interface StopView {
  kind: CardStop;
  cards: number;
  /** The operator has seen this stop; the loop may continue. */
  acknowledged: boolean;
  /** Nothing may be programmed until this is acknowledged. */
  blocksBatch: boolean;
}

export type PhasesState = 'idle' | 'running' | 'summary' | 'ended';

export interface BatchSummary {
  /** Cards tapped: each one produced a record, ok or not. */
  attempted: number;
  /** The batch's own denominator, so "ended early" is a visible difference. */
  target: number;
  /** Installed, keyed and PIN-verified. */
  spendable: number;
  /** Programmed but the PIN did not verify (`pinNotSet`): must be zero (§ 10). */
  unspendable: number;
  /** Stopped before the install completed. */
  failed: number;
  /** Registry writes outstanding. */
  pending: number;
  policy: PinPolicy;
}

/**
 * What the loop hands the store when one card's run ends: the engine's outcome
 * and the one fact the store cannot derive — the card's key, for its short id.
 */
export interface CardResult extends CardOutcome {
  /** A successful run's card key. Absent when the run failed before KEY_SETUP. */
  pubkey?: readonly number[];
  /** All eight slot keys, slot 0 first, from a finished key setup. */
  slotKeys?: readonly (readonly number[])[];
  /**
   * The short id the card answered before a resume tap re-keyed it; the
   * registry closes that row when this one is posted (OPS-PLAN O5).
   */
  supersedes?: string;
}

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

export interface BatchActions {
  /** Start a batch. The config carries a PIN policy; it can never carry a value. */
  start(config: BatchConfig): void;
  /** Record one card: the only way card state enters the store. */
  recordCard(result: CardResult): void;
  /** The loop's own transition; screens never set a phase or the index. */
  setLoop(
    update: Partial<Pick<LoopState, 'phase' | 'cardActive' | 'loadProgress'>>,
  ): void;
  /** The operator saw this stop. Clears the gate; the batch may continue. */
  acknowledgeStop(kind: CardStop): void;
  /** An acknowledged gate reopened: the loop holds again. */
  reopenStop(kind: CardStop): void;
  /** Close the batch: the summary's numbers freeze and the PIN slot is wiped. */
  end(): void;
}

export interface BatchView {
  config: BatchConfig;
  phase: PhasesState;
  records: readonly RecordRow[];
  /** Cards tapped; equal to `records.length`, named for the screen's sentence. */
  done: number;
  summary: BatchSummary;
  stops: readonly StopView[];
  pendingCount: number;
  /** The batch has fewer records than its count: the summary says so. */
  stopped: boolean;
  /** A gate the operator has not acknowledged holds the loop. */
  blockedBy: CardStop | null;
  /** The registry has rows the service has not accepted yet. */
  outboxPending: boolean;
  actions: BatchActions;
}

/** Group the batch's stopped rows by kind, in § 5's table order. */
function stopsOf(
  records: readonly RecordRow[],
  acknowledged: ReadonlySet<CardStop>,
): StopView[] {
  const counts = new Map<CardStop, number>();
  for (const r of records) {
    if (r.stop) {
      counts.set(r.stop, (counts.get(r.stop) ?? 0) + 1);
    }
  }
  // Every kind that has occurred is listed, blocking or not: the summary's
  // record must show a `pulled` as plainly as a `wrongKeys`.
  return CARD_STOPS.filter(k => counts.has(k)).map(kind => ({
    kind,
    cards: counts.get(kind) ?? 0,
    acknowledged: acknowledged.has(kind),
    blocksBatch: BLOCKS_BATCH[kind],
  }));
}

/** The summary's numbers, from the rows. One pass, one source of truth. */
export function summarize(
  config: BatchConfig,
  records: readonly RecordRow[],
): BatchSummary {
  let spendable = 0;
  let unspendable = 0;
  let pending = 0;
  for (const r of records) {
    if (r.stop === 'pinNotSet') {
      unspendable++;
    } else if (isSpendable(r)) {
      spendable++;
    }
    if (r.pending) {
      pending++;
    }
  }
  return {
    attempted: records.length,
    target: config.count,
    spendable,
    unspendable,
    failed: records.length - spendable - unspendable,
    pending,
    policy: config.pinPolicy,
  };
}

// ---------------------------------------------------------------------------
// The API seam
// ---------------------------------------------------------------------------

/** The signed-in operator. The token is memory-only (api.ts owns its 12 h life). */
export interface Session {
  readonly operatorId: number;
  readonly token: string;
}

/**
 * The registry seam. `store.tsx` does not import api.ts, so the batch machine
 * runs with no network module loaded — which is what demo mode and the store's
 * tests need. `App.tsx` passes the real client in, and api.ts owns the
 * session's storage; nothing here is persisted.
 */
export interface StoreApi extends Partial<CardOps> {
  readonly serviceUrl: string;
  /**
   * Resolves with the operator's session. Rejects with whatever api.ts throws:
   * `ApiUnreachableError` (transport), `ApiCancelledError` (aborted signal),
   * `ApiServiceError` (the service's refusal). This module passes them through
   * untouched so `SignInScreen` can branch on them.
   */
  signIn(
    name: string,
    password: string,
    options?: {signal?: AbortSignal},
  ): Promise<{operatorId: number; token: string}>;
  /** The batch record: federation, count, PIN policy name, load sha (§ 4). */
  postBatch(config: BatchConfig): Promise<void>;
  /** One card row, with its PIN state and policy name — never a PIN value. */
  postCard(row: RecordRow): Promise<void>;
  /** One retry pass; resolves to the rows the service answered. */
  retryPending(
    rows: readonly RecordRow[],
    session: Session,
  ): Promise<readonly RecordRow[]>;
  /** Revokes the session on the service; best effort, never blocks sign-out. */
  signOut?(): Promise<void>;
  /** Federation names by decoded id, from the operator's tills. */
  federationNames?(): Promise<Record<string, string>>;
}

export interface StoreShape {
  signedIn: boolean;
  session: Session | null;
  serviceUrl: string;
  signIn(
    name: string,
    password: string,
    options?: {signal?: AbortSignal},
  ): Promise<void>;
  signOut(): void;
  /** Federation names by decoded id, read after sign-in; empty until then. */
  federationNames: Readonly<Record<string, string>>;
  /**
   * Ops' card lifecycle calls (manager keys, reset), for the signed-in
   * operator. Each rejects when this build has no registry to ask, which a
   * tap reports as a stop rather than going on without the answer.
   */
  readonly ops: CardOps;
  /**
   * Start a batch. Exposed on the store as well as on `batch.actions` because
   * `batch` is null until a batch starts: BatchSetup has to call this before
   * there is anything to read an action from.
   */
  startBatch(config: BatchConfig): void;
  batch: BatchView | null;
  /** The live loop, or null when no batch is running. */
  loop: LoopState | null;
  /**
   * The one PIN slot (`pin.ts`'s `usePinReveal`), exposed so the loop and the
   * summary reach it without a second provider in their tree. The store hands
   * the interface through and never holds a value: `grant`, `current`,
   * `reveal` and `wipe` all read the slot's own ref.
   */
  readonly pinSlot: PinRevealContext;
}

const Ctx = createContext<StoreShape | null>(null);

/** The outbox's retry cadence on an open batch (§ 4's retry contract). */
const RETRY_INTERVAL_MS = 30_000;

// ---------------------------------------------------------------------------
// The provider
// ---------------------------------------------------------------------------

interface BatchState {
  config: BatchConfig;
  phase: PhasesState;
  records: RecordRow[];
  acknowledged: Set<CardStop>;
  loop: LoopState;
}

/** The loop's starting point: the first card, not yet on the reader. */
const OPENING_LOOP: LoopState = {
  index: 1,
  phase: 'probe',
  cardActive: false,
  loadProgress: null,
};

/**
 * The provider. The batch machine reads nothing from settings.
 *
 * `api` is the registry seam: absent in demo mode and in tests, where the batch
 * machine runs with its rows left pending, which is exactly what the summary's
 * pending warning is for.
 */
export function StoreProvider({
  children,
  api,
  initialSession = null,
}: {
  children: React.ReactNode;
  /** The registry client; absent in demo mode and in the store's tests. */
  api?: StoreApi;
  /** A session api.ts restored, when it has one. */
  initialSession?: Session | null;
}) {
  const [session, setSession] = useState<Session | null>(initialSession);
  const [federationNames, setFederationNames] = useState<Readonly<Record<string, string>>>({});
  const [batch, setBatch] = useState<BatchState | null>(null);
  /**
   * The one PIN slot (see `pin.ts`). The store reads no PIN from it: it only
   * hands the interface on, and calls `wipe()` where § 5 says a PIN must not
   * outlive its batch — `start`, `end`, `signOut`. The provider that holds the
   * value is `pin.ts`'s, one level above the app's screens.
   */
  const pinSlot = usePinReveal();
  // The batch PIN's holder, if the tree has one. Absent, `useBatchPin` throws,
  // and sign-out still has to complete — so this is read more carefully than
  // `pinSlot`, whose slot degrades to inert.
  const batchPin = useOptionalBatchPin();
  const batchRef = useRef<BatchState | null>(batch);
  batchRef.current = batch;

  // `api` is passed once by the app; a ref keeps the actions stable across the
  // parent's re-renders without depending on the object's identity.
  const apiRef = useRef<StoreApi | undefined>(api);
  apiRef.current = api;

  const signIn = useCallback<StoreShape['signIn']>(
    async (name, password, options) => {
      const client = apiRef.current;
      if (!client) {
        throw new Error('no registry client configured for this build');
      }
      const s = await client.signIn(name, password, options);
      setSession({operatorId: s.operatorId, token: s.token});
    },
    [],
  );

  const signOut = useCallback((): void => {
    // The service forgets the token too; signing out here never waits on it.
    apiRef.current?.signOut?.().catch(() => undefined);
    // The outbox's frozen bodies outlive the session on purpose (api.ts); the
    // batch does not. Ending it drops the one PIN the app holds.
    pinSlot.wipe();
    batchPin?.wipe();
    setBatch(null);
    setSession(null);
  }, [pinSlot, batchPin]);

  // A new client means another registry (Settings → Registry): the session
  // was issued by the old one and the new client holds no token, so every
  // call would be refused on the phone. Sign out rather than look signed in.
  const firstApi = useRef(api);
  useEffect(() => {
    if (api === firstApi.current) {
      return;
    }
    firstApi.current = api;
    pinSlot.wipe();
    batchPin?.wipe();
    setBatch(null);
    setSession(null);
  }, [api, pinSlot, batchPin]);

  // Names come from the registry, once per session; a failure leaves the short
  // id on screen, which is what the operator saw before any till reported one.
  const signedIn = session !== null;
  useEffect(() => {
    if (!signedIn) {
      setFederationNames({});
      return;
    }
    let live = true;
    apiRef.current
      ?.federationNames?.()
      .then(names => live && setFederationNames(names))
      .catch(() => undefined);
    return () => {
      live = false;
    };
  }, [signedIn]);

  const recordCard = useCallback((result: CardResult): void => {
    const live = batchRef.current;
    if (!live || live.phase !== 'running') {
      throw new Error('recordCard outside a running batch');
    }
    // § 8.5, "PIN: mandatory": every card this app provisions gets a PIN, so a
    // recorded card always carries a resolved PIN state. A result claiming
    // success without a verified PIN is a caller's bug: refuse it rather than
    // let an unspendable card be counted as provisioned.
    const pinState = pinStateOf(result);
    if (result.ok && pinState !== 'set') {
      throw new Error(
        'recordCard: a successful run must carry a verified PIN (plan § 8.5); ' +
          'record the card as stopped instead',
      );
    }
    if (!result.ok && result.stop === undefined) {
      throw new Error('recordCard: a failed run must name its stop');
    }
    if (result.ok && !result.pubkey) {
      throw new Error('recordCard: a successful run must carry the card key');
    }
    // The row is built once, so the post and the summary name the same
    // (id, at). A run that never got a key has no short id; the row says so
    // rather than inventing one.
    const at = Date.now();
    const row: RecordRow = {
      ...result,
      pinState,
      id: result.pubkey ? shortIdOf(result.pubkey) : `unkeyed-${at}`,
      at,
      // A run that stopped before the card had keys has no short id, so the
      // registry has nothing to record: the row is the batch's own, never
      // posted, and never shown as waiting on the registry.
      pending: result.pubkey !== undefined,
      slotKeys: result.slotKeys?.map(k =>
        k.map(b => b.toString(16).padStart(2, '0')).join(''),
      ),
    };
    setBatch(b => {
      if (!b || b.phase !== 'running') {
        return b;
      }
      const records = [...b.records, row];
      const full = records.length >= b.config.count;
      return {
        ...b,
        records,
        phase: full ? 'summary' : b.phase,
        loop: {
          ...b.loop,
          index: b.loop.index + 1,
          phase: 'done',
          cardActive: false,
          loadProgress: null,
        },
      };
    });
    if (!row.pending) {
      return;
    }
    apiRef.current?.postCard(row).then(
      () => {
        setBatch(b =>
          b
            ? {
                ...b,
                records: b.records.map(r =>
                  r.id === row.id && r.at === row.at
                    ? {...r, pending: false}
                    : r,
                ),
              }
            : b,
        );
      },
      () => undefined, // still pending: the interval pass and the summary retry
    );
  }, []);

  const actions = useMemo<BatchActions>(
    () => ({
      start(config: BatchConfig): void {
        if (!Number.isSafeInteger(config.count) || config.count < 1) {
          throw new Error('a batch is at least one card');
        }
        // A new batch starts empty and takes its PIN policy from the config.
        // Any PIN left from a previous batch goes first.
        pinSlot.wipe();
        batchPin?.wipe();
        setBatch({
          config,
          phase: 'running',
          records: [],
          acknowledged: new Set<CardStop>(),
          loop: OPENING_LOOP,
        });
        apiRef.current?.postBatch(config).catch(() => undefined);
      },
      recordCard,
      setLoop(update: Parameters<BatchActions['setLoop']>[0]): void {
        setBatch(b => {
          if (!b || b.phase !== 'running') {
            return b;
          }
          return {...b, loop: {...b.loop, ...update}};
        });
      },
      acknowledgeStop(kind: CardStop): void {
        setBatch(b => {
          if (!b) {
            return b;
          }
          const acknowledged = new Set(b.acknowledged);
          acknowledged.add(kind);
          return {...b, acknowledged};
        });
      },
      reopenStop(kind: CardStop): void {
        setBatch(b => {
          if (!b) {
            return b;
          }
          const acknowledged = new Set(b.acknowledged);
          acknowledged.delete(kind);
          return {...b, acknowledged};
        });
      },
      end(): void {
        // § 5: a generated PIN "is not retrievable after the batch ends", and
        // the batch PIN has no card left to be sent to.
        pinSlot.wipe();
        batchPin?.wipe();
        setBatch(b =>
          b ? {...b, phase: 'ended', loop: {...b.loop, cardActive: false}} : b,
        );
      },
    }),
    [recordCard, pinSlot, batchPin],
  );

  // The outbox's cadence: one pass on an interval while a batch has pending
  // rows, and one on foreground. Only rows still pending are posted, and the
  // answer flips exactly those rows.
  const pendingKey = batch
    ? batch.records
        .filter(r => r.pending)
        .map(r => `${r.id}@${r.at}`)
        .join(',')
    : '';
  const retrying = session !== null && batch !== null && pendingKey !== '';
  useEffect(() => {
    const client = apiRef.current;
    const sess = session;
    if (!retrying || !client || !sess || pendingKey === '') {
      return;
    }
    const pass = (): void => {
      const live = batchRef.current;
      if (!live) {
        return;
      }
      const rows = live.records.filter(r => r.pending);
      if (rows.length === 0) {
        return;
      }
      client.retryPending(rows, sess).then(answered => {
        if (answered.length === 0) {
          return;
        }
        const key = new Set(answered.map(r => `${r.id}@${r.at}`));
        setBatch(b =>
          b
            ? {
                ...b,
                records: b.records.map(r =>
                  r.pending && key.has(`${r.id}@${r.at}`)
                    ? {...r, pending: false}
                    : r,
                ),
              }
            : b,
        );
      }, () => undefined);
    };
    const timer = setInterval(pass, RETRY_INTERVAL_MS);
    const sub = AppState.addEventListener('change', state => {
      if (state === 'active') {
        pass();
      }
    });
    return () => {
      clearInterval(timer);
      sub.remove();
    };
  }, [session, retrying, pendingKey]);

  const view = useMemo<BatchView | null>(
    () => (batch ? viewOf(batch, actions) : null),
    [batch, actions],
  );

  const ops = useMemo<CardOps>(() => {
    const missing = (call: string) =>
      Promise.reject(new Error(`${call} needs the registry, and this build has none`));
    return {
      managerKeys: (chip, shortId) =>
        apiRef.current?.managerKeys?.(chip, shortId) ?? missing('the card-manager keys'),
      resumeCheck: (chip, shortId) =>
        apiRef.current?.resumeCheck?.(chip, shortId) ?? missing('the resume check'),
      resetCheck: (shortId, chip) =>
        apiRef.current?.resetCheck?.(shortId, chip) ?? missing('the reset check'),
      completeReset: completion =>
        apiRef.current?.completeReset?.(completion) ?? missing('recording a reset'),
    };
  }, []);

  const value = useMemo<StoreShape>(
    () => ({
      signedIn: session !== null,
      session,
      serviceUrl: apiRef.current?.serviceUrl ?? '',
      signIn,
      signOut,
      federationNames,
      ops,
      startBatch: actions.start,
      batch: view,
      loop: batch && batch.phase === 'running' ? batch.loop : null,
      pinSlot,
    }),
    [
      session,
      signIn,
      signOut,
      federationNames,
      ops,
      actions,
      view,
      batch,
      pinSlot,
    ],
  );

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

function viewOf(batch: BatchState, actions: BatchActions): BatchView {
  const stops = stopsOf(batch.records, batch.acknowledged);
  const summary = summarize(batch.config, batch.records);
  const gate = stops.find(s => s.blocksBatch && !s.acknowledged);
  return {
    config: batch.config,
    phase: batch.phase,
    records: batch.records,
    done: batch.records.length,
    summary,
    stops,
    pendingCount: summary.pending,
    stopped: batch.records.length < batch.config.count,
    blockedBy: gate ? gate.kind : null,
    outboxPending: summary.pending > 0,
    actions,
  };
}

export function useStore(): StoreShape {
  const s = useContext(Ctx);
  if (!s) {
    throw new Error('useStore outside StoreProvider');
  }
  return s;
}

/** Federation names, or none outside a provider (component tests). */
export function useFederationNames(): Readonly<Record<string, string>> {
  return useContext(Ctx)?.federationNames ?? NO_NAMES;
}
const NO_NAMES: Readonly<Record<string, string>> = {};

/** The open batch, or null. Every screen that reads card state goes through this. */
export function useBatch(): BatchView | null {
  return useStore().batch;
}

/** The live loop, or null when no batch is running. */
export function useLoop(): LoopState | null {
  return useStore().loop;
}

/**
 * The card's short id: the last 4 bytes of its slot-0 x-only key, lower-case
 * hex, which is the registry's rule (`registry/src/index.ts`, `^[0-9a-f]{8}$`).
 */
export function shortIdOf(pubkey: readonly number[]): string {
  return pubkey
    .slice(-4)
    .map(b => b.toString(16).padStart(2, '0'))
    .join('');
}

/** The short id, uppercased for a row. The registry keeps the lowercase form. */
export function formatShortId(id: string): string {
  return id.toUpperCase();
}
