/**
 * The install, as a table of phases and one loop over them, and the key
 * rotation. Both run over an injected transceiver, with an injected clock and
 * random source, so the same code runs on the phone and against the fake card
 * in gp/test.
 *
 * Recovery is level-triggered: nothing is remembered between runs. Every run
 * probes the card, logs in, and sweeps whatever a torn run may have left (the
 * instances, newest first, then the package; "not there" is fine) before it
 * loads. Any error ends the run where it is; there is no automatic restart and
 * no retry. The next tap starts again from the probe.
 *
 * Guards, in the order they act:
 * - the load file's sha256 is checked before the first APDU;
 * - the card cryptogram is checked before EXTERNAL AUTHENTICATE (scp02.ts);
 * - DELETE names only the AIDs of the whitelist, never the ISD (scp02.ts).
 *
 * What this card does NOT have: a GET_INFO, so `fedimintPlan` sets no `guard`.
 * Nothing on the card tells a run whether the installed applet holds keys or a
 * PIN, so the sweep is gated only by the sidecar's precheck and by the
 * operator's Replace switch (docs/PROVISIONER-PLAN.md § 10). A plan with no
 * guard skips `probe` and starts at `selectIsd`.
 */
import {sha256} from '@noble/hashes/sha2';

import {CARD_AID, FEDIMINT_PACKAGE_AID, ISD_AID, OPENER_AID} from './aids';
import {type Bytes, equal, fromHex, swHex, toHex} from './bytes';
import {
  CardError,
  FACTORY_KEYS,
  type FreeMemory,
  type KeySet,
  type Random,
  type Reply,
  type Session,
  type Transceiver,
  TransportError,
  WrongKeys,
  deleteAid,
  externalAuthenticate,
  getFreeMemory,
  initializeUpdate,
  installAndSelectable,
  installForLoad,
  openSession,
  parseFreeMemory,
  putKeys,
  selectAid,
  sessionIfMatches,
  splitLoad,
  transmit,
} from './scp02';

export type Phase =
  | 'probe'
  | 'selectIsd'
  | 'auth'
  | 'sweep'
  | 'installForLoad'
  | 'load'
  | 'installOpener'
  | 'installCard'
  | 'installTag'
  | 'verify'
  | 'done';

/** The phases of an install, in order. A plan may skip some (a plan with no guard has no probe). */
export const PHASES: readonly Phase[] = [
  'probe',
  'selectIsd',
  'auth',
  'sweep',
  'installForLoad',
  'load',
  'installOpener',
  'installCard',
  'installTag',
  'verify',
  'done',
];

export type InstallPhase = 'installOpener' | 'installCard' | 'installTag';

export type RotatePhase = 'selectIsd' | 'auth' | 'putKey' | 'reopen' | 'done';

/**
 * Why a run stopped. `pulled`: the reply never came (the card moved, or the
 * phone gave up). `refused`: the card answered with an error. `wrongKeys`: the
 * card did not prove the keys, and nothing was sent after INITIALIZE UPDATE.
 * `guarded`: this module refused before the card was touched further.
 */
export type StopKind = 'pulled' | 'refused' | 'wrongKeys' | 'guarded';

/** A load file as tools/cardkit/make-assets.py writes it. */
export interface LoadFileAsset {
  readonly name: string;
  readonly size: number;
  readonly sha256: string;
  readonly hex: string;
}

export interface InstallStep {
  phase: InstallPhase;
  module: Bytes;
  instance: Bytes;
}

/** A plain command to an installed applet, and the answer it must get. */
export interface Check {
  apdu: Bytes;
  /** The data the reply must carry, exactly. */
  data?: Bytes;
  /** The length the reply's data must have. */
  length?: number;
  /** A status word other than 9000 that the reply must carry, with no data. */
  sw?: number;
}

export interface Plan {
  name: string;
  loadFile: LoadFileAsset;
  packageAid: Bytes;
  installs: InstallStep[];
  /** DELETE, in this order, at every sweep: the instances newest first, then the package. */
  sweep: Bytes[];
  /** The applet whose GET_INFO guards the sweep: a run never sweeps a card applet that holds anything. */
  guard?: Bytes;
  checks: Check[];
  /** Deleted again after the checks (the hello applet leaves nothing behind). */
  removeAfterChecks?: Bytes[];
  /** LOAD block size; 247 leaves room for the C-MAC in a short APDU. */
  blockSize?: number;
}

/** GET_PUB of slot 0: 32 bytes once the slot is keyed, 6985 while it is not. */
const GET_PUB = [0xb0, 0x11, 0x00, 0x00, 0x00];

/**
 * The card: the opener first, then the card's applet. No tag, no hello applet,
 * and no `guard`: there is no GET_INFO on this applet, so the sweep cannot be
 * gated from the card side (see the header).
 *
 * The checks prove an install the only ways this applet allows:
 * - SELECT answers `02 00` (FedimintCardApplet.VERSION_MAJOR/MINOR), so a
 *   selection that lands on the opener, which answers `90 00` with no data,
 *   fails here rather than at the first real command;
 * - GET_PUB of every slot answers `6985`: mint-fresh, no slot has a key.
 */
export function fedimintPlan(loadFile: LoadFileAsset): Plan {
  return {
    name: 'Fedimint card',
    loadFile,
    packageAid: FEDIMINT_PACKAGE_AID,
    installs: [
      {phase: 'installOpener', module: OPENER_AID, instance: OPENER_AID},
      {phase: 'installCard', module: CARD_AID, instance: CARD_AID},
    ],
    sweep: [CARD_AID, OPENER_AID, FEDIMINT_PACKAGE_AID],
    checks: [
      {apdu: selectAid(CARD_AID), data: [0x02, 0x00]},
      // Every slot, so an applet that kept a key across a reinstall is seen.
      ...[0, 1, 2, 3, 4, 5, 6, 7].map(slot => ({
        apdu: [GET_PUB[0], GET_PUB[1], slot, 0x00, 0x00],
        sw: 0x6985,
      })),
    ],
  };
}

/** One APDU in a readout: its INS, the length of its data, the SW and the time. Never its bytes. */
export interface ApduRecord {
  phase: string;
  ins: number;
  lc: number;
  sw?: number;
  rlen?: number;
  ms: number;
}

export interface PhaseTiming {
  phase: string;
  ms: number;
  apdus: number;
  maxMs: number;
}

export type RunEvent =
  | {type: 'phase'; phase: string}
  /** Emitted only after the LOAD response confirms this block. */
  | {type: 'block'; index: number; of: number}
  | {type: 'apdu'; record: ApduRecord};

export interface RunOptions {
  random: Random;
  /** Milliseconds, any origin. */
  now: () => number;
  /** Called synchronously, never awaited; whatever it throws is ignored. */
  onEvent?: (event: RunEvent) => void;
  macOverModifiedApdu?: boolean;
}

export interface ProvisionOptions extends RunOptions {
  /** Remove an installed card applet after its probe. Spare cards only. */
  replace?: boolean;
  /** The card manager's keys; the factory set by default. */
  keys?: KeySet;
}

export interface RunResult<P extends string> {
  ok: boolean;
  /** Where the run stopped: `done` when it finished. */
  phase: P;
  kind?: StopKind;
  sw?: number;
  ins?: number;
  reason?: string;
  timings: PhaseTiming[];
  apdus: ApduRecord[];
  freeBefore?: FreeMemory;
  freeAfter?: FreeMemory;
  /** The LOAD block last sent (1-based), of how many. */
  blocks?: {at: number; of: number};
}

export type ProvisionResult = RunResult<Phase>;

/** How a run ended, before the bookkeeping is added. */
interface Outcome {
  ok: boolean;
  kind?: StopKind;
  sw?: number;
  ins?: number;
  reason?: string;
}

export interface RotateResult extends RunResult<RotatePhase> {
  /** Confirmed active key version from the successful authentication. */
  keyVersion?: number;
  /** Which set the card proved. */
  active?: 'from' | 'to';
  /** False when the card was already on `to` (a PUT KEY whose reply was lost). */
  rotated?: boolean;
}

/** A refusal of this module's own, before the card is touched further. */
class Guard extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'Guard';
  }
}

function expectOk(reply: Reply, ins: number): Reply {
  if (reply.sw !== 0x9000) {
    throw new CardError(reply.sw, ins);
  }
  return reply;
}

/** The bookkeeping of one run: the phase, the clock, the records, the events. */
class Run<P extends string> {
  phase: P;
  readonly records: ApduRecord[] = [];
  readonly timings: PhaseTiming[] = [];
  freeBefore?: FreeMemory;
  freeAfter?: FreeMemory;
  blocks?: {at: number; of: number};
  private phaseStart: number;

  constructor(
    private readonly tx: Transceiver,
    private readonly options: RunOptions,
    first: P,
  ) {
    this.phase = first;
    this.phaseStart = options.now();
  }

  emit(event: RunEvent): void {
    const onEvent = this.options.onEvent;
    if (!onEvent) {
      return;
    }
    try {
      const r = onEvent(event) as unknown;
      if (r && typeof (r as Promise<unknown>).then === 'function') {
        (r as Promise<unknown>).then(undefined, () => undefined);
      }
    } catch {
      // A readout that throws is the readout's problem, not the card's.
    }
  }

  enter(phase: P): void {
    this.closePhase();
    this.phase = phase;
    this.phaseStart = this.options.now();
    this.timings.push({phase, ms: 0, apdus: 0, maxMs: 0});
    this.emit({type: 'phase', phase});
  }

  private closePhase(): void {
    const t = this.timings[this.timings.length - 1];
    if (t && t.phase === this.phase) {
      t.ms = this.options.now() - this.phaseStart;
    }
  }

  /** Every APDU goes through here: timed, and recorded as INS, Lc, SW and lengths. */
  readonly timed: Transceiver = async apdu => {
    const record: ApduRecord = {
      phase: this.phase,
      ins: apdu[1],
      lc: apdu.length > 5 ? apdu[4] : 0,
      ms: 0,
    };
    const t0 = this.options.now();
    try {
      const reply = await this.tx(apdu);
      if (reply && reply.length >= 2) {
        record.sw = (reply[reply.length - 2] << 8) | reply[reply.length - 1];
        record.rlen = reply.length - 2;
      }
      return reply;
    } finally {
      record.ms = this.options.now() - t0;
      this.records.push(record);
      const t = this.timings[this.timings.length - 1];
      if (t) {
        t.apdus++;
        t.maxMs = Math.max(t.maxMs, record.ms);
      }
      this.emit({type: 'apdu', record: {...record}});
    }
  };

  plain(apdu: Bytes): Promise<Reply> {
    return transmit(this.timed, Array.from(apdu));
  }

  finish(outcome: Outcome): RunResult<P> {
    this.closePhase();
    return {
      ...outcome,
      phase: this.phase,
      timings: this.timings,
      apdus: this.records,
      freeBefore: this.freeBefore,
      freeAfter: this.freeAfter,
      blocks: this.blocks,
    };
  }

  /** The result for an error, or the error again when it is neither the card's nor a guard's. */
  fail(e: unknown): RunResult<P> {
    if (e instanceof Guard) {
      return this.finish({ok: false, kind: 'guarded', reason: e.message});
    }
    if (e instanceof TransportError) {
      return this.finish({ok: false, kind: 'pulled', reason: e.message});
    }
    if (e instanceof WrongKeys) {
      return this.finish({ok: false, kind: 'wrongKeys', reason: e.message});
    }
    if (e instanceof CardError) {
      return this.finish({
        ok: false,
        kind: 'refused',
        sw: e.sw,
        ins: e.ins,
        reason: e.message,
      });
    }
    throw e;
  }
}

/**
 * Why GET_INFO's 30 bytes are not an empty card, or null when they are.
 *
 * This card's applet has no GET_INFO, so nothing here uses it. It is kept
 * because `provision`'s probe calls it for any plan that sets `guard`, and a
 * future applet with a GET_INFO of this shape would reuse it. Its field
 * offsets are Cashu's; they must be re-derived, not assumed, before a Fedimint
 * plan sets `guard`. See docs/PROVISIONER-PLAN.md § 10.
 */
export function notEmpty(info: Bytes): string | null {
  if (info.length < 30) {
    return `GET_INFO answered ${info.length} bytes, not 30`;
  }
  if (info[3] !== 0) {
    return `it holds ${info[3]} unspent pieces`;
  }
  if (info[4] !== 0) {
    return `it has ${info[4]} spent pieces not yet freed`;
  }
  if (info[16] !== 0) {
    return 'it has an owner';
  }
  if (info[7] !== 0) {
    return 'it has a PIN';
  }
  if (info[11] !== 0) {
    return 'it has a record';
  }
  return null;
}

function loadFileBytes(asset: LoadFileAsset): number[] {
  let file: number[];
  try {
    file = fromHex(asset.hex);
  } catch {
    throw new Guard(`the ${asset.name} load file is not hex: nothing was sent`);
  }
  const digest = toHex(sha256(Uint8Array.from(file)));
  if (file.length !== asset.size || digest !== asset.sha256.toUpperCase()) {
    throw new Guard(
      `the ${asset.name} load file is ${file.length} bytes, sha256 ${digest.slice(0, 8)}...; ` +
        `the build says ${asset.size} bytes, ${asset.sha256.slice(0, 8)}...: nothing was sent`,
    );
  }
  return file;
}

const INSTALL_PHASES: readonly Phase[] = [
  'installOpener',
  'installCard',
  'installTag',
];

/**
 * Whether a run of `plan` that stopped in `phase` may have left the applet its
 * probe guards on the card: that applet's INSTALL had gone out, and only its
 * reply may have been lost. The next run's probe then finds the applet and
 * stops `guarded` unless `replace` is on. Never true for a plan with no guard,
 * which is every plan for this card today.
 */
export function mayLeaveGuardedApplet(plan: Plan, phase: string): boolean {
  const guard = plan.guard;
  if (!guard) {
    return false;
  }
  const step = plan.installs.find(s => equal(s.instance, guard));
  if (!step) {
    return false;
  }
  const at = PHASES.indexOf(phase as Phase);
  return at >= 0 && at >= PHASES.indexOf(step.phase);
}

/** A plan runs the probe only when it has a card applet to guard, and only the INSTALLs it lists. */
function applies(plan: Plan, phase: Phase): boolean {
  if (phase === 'probe') {
    return plan.guard !== undefined;
  }
  if (INSTALL_PHASES.includes(phase)) {
    return plan.installs.some(s => s.phase === phase);
  }
  return true;
}

/**
 * Installs `plan` on the card in the field. Returns how the run ended; throws
 * only for a fault of the caller's (a bug), never for anything the card does.
 *
 * With no guard, whatever the card holds is swept: the package's name is the
 * only claim, and the sidecar's precheck and the operator's Replace switch are
 * the whole of the gate.
 */
export async function provision(
  tx: Transceiver,
  plan: Plan,
  options: ProvisionOptions,
): Promise<ProvisionResult> {
  const run = new Run<Phase>(tx, options, plan.guard ? 'probe' : 'selectIsd');
  const keys = options.keys ?? FACTORY_KEYS;
  const sessionOptions = {macOverModifiedApdu: options.macOverModifiedApdu};
  let session: Session | null = null;
  let blocks: number[][] = [];
  const open = (): Session => {
    if (!session) {
      throw new Error('no secure channel');
    }
    return session;
  };

  const install = async (phase: InstallPhase) => {
    const step = plan.installs.find(s => s.phase === phase)!;
    expectOk(
      await open().send(
        run.timed,
        installAndSelectable(plan.packageAid, step.module, step.instance),
      ),
      0xe6,
    );
  };

  const steps: Record<Phase, () => Promise<void>> = {
    probe: async () => {
      const guard = plan.guard!;
      const selected = await run.plain(selectAid(guard));
      if (selected.sw === 0x6a82) {
        return; // no card applet: nothing to guard
      }
      expectOk(selected, 0xa4);
      if (selected.data.length < 2 || selected.data[0] !== 0x02) {
        throw new Guard(
          `${toHex(guard)} answers ${toHex(selected.data)} ${swHex(selected.sw)}, not a 2.x card applet: the app will not remove it`,
        );
      }
      const info = expectOk(await run.plain([0xb0, 0x01, 0x00, 0x00, 0x00]), 0x01);
      const why = notEmpty(info.data);
      if (why) {
        throw new Guard(
          `the card applet is installed and ${why}: the app never removes a card that holds anything`,
        );
      }
      if (!options.replace) {
        throw new Guard(
          'the card applet is installed: turn on Replace to remove it first (spare card only)',
        );
      }
    },
    selectIsd: async () => {
      expectOk(await run.plain(selectAid(ISD_AID)), 0xa4);
      const free = await run.plain(getFreeMemory());
      run.freeBefore =
        free.sw === 0x9000
          ? (parseFreeMemory(free.data) ?? undefined)
          : undefined;
    },
    auth: async () => {
      session = await openSession(
        run.timed,
        keys,
        options.random,
        sessionOptions,
      );
    },
    sweep: async () => {
      for (const aid of plan.sweep) {
        const r = await open().send(run.timed, deleteAid(aid));
        if (r.sw !== 0x9000 && r.sw !== 0x6a88) {
          throw new CardError(r.sw, 0xe4);
        }
      }
    },
    installForLoad: async () => {
      expectOk(
        await open().send(run.timed, installForLoad(plan.packageAid)),
        0xe6,
      );
    },
    load: async () => {
      for (let i = 0; i < blocks.length; i++) {
        run.blocks = {at: i + 1, of: blocks.length};
        expectOk(await open().send(run.timed, blocks[i]), 0xe8);
        run.emit({type: 'block', index: i + 1, of: blocks.length});
      }
    },
    installOpener: () => install('installOpener'),
    installCard: () => install('installCard'),
    installTag: () => install('installTag'),
    verify: async () => {
      for (const check of plan.checks) {
        // A check may expect a refusal as its success: an unkeyed slot answers 6985.
        const r = await run.plain(check.apdu);
        const want = check.sw ?? 0x9000;
        if (r.sw !== want) {
          throw new CardError(
            r.sw,
            check.apdu[1],
            `INS ${toHex([check.apdu[1]])} answered ${swHex(r.sw)}, not ${swHex(want)}`,
          );
        }
        if (check.data && !equal(r.data, check.data)) {
          throw new CardError(
            r.sw,
            check.apdu[1],
            `INS ${toHex([check.apdu[1]])} answered ${toHex(r.data)}, not ${toHex(check.data)}`,
          );
        }
        if (check.length !== undefined && r.data.length !== check.length) {
          throw new CardError(
            r.sw,
            check.apdu[1],
            `INS ${toHex([check.apdu[1]])} answered ${r.data.length} bytes, not ${check.length}`,
          );
        }
      }
      if (plan.removeAfterChecks) {
        expectOk(await run.plain(selectAid(ISD_AID)), 0xa4);
        const again = await openSession(
          run.timed,
          keys,
          options.random,
          sessionOptions,
        );
        for (const aid of plan.removeAfterChecks) {
          expectOk(await again.send(run.timed, deleteAid(aid)), 0xe4);
        }
      }
      expectOk(await run.plain(selectAid(ISD_AID)), 0xa4);
      const free = await run.plain(getFreeMemory());
      run.freeAfter =
        free.sw === 0x9000
          ? (parseFreeMemory(free.data) ?? undefined)
          : undefined;
    },
    done: async () => undefined,
  };

  try {
    // the bytes first: a file that is not the build's sends nothing at all
    blocks = splitLoad(loadFileBytes(plan.loadFile), plan.blockSize);
    for (const phase of PHASES) {
      if (applies(plan, phase)) {
        run.enter(phase);
        await steps[phase]();
      }
    }
    return run.finish({ok: true});
  } catch (e) {
    if (e instanceof RangeError && blocks.length === 0) {
      return run.fail(
        new Guard(
          `the ${plan.loadFile.name} load file cannot be sent: ${e.message}`,
        ),
      );
    }
    return run.fail(e);
  }
}

export interface RotateOptions extends RunOptions {
  /** PUT KEY's P1 and the new set's version; by default the active set's version for both (in place, as gp --lock does). */
  putKey?: {p1?: number; newKvn?: number};
}

/**
 * Moves the card manager from `from` to `to`. One INITIALIZE UPDATE, whose
 * card cryptogram is checked offline against both sets: a PUT KEY whose reply
 * was lost leaves the card on `to`, and that is found without a failed login.
 * EXTERNAL AUTHENTICATE goes out only for a set the card proved; then PUT KEY,
 * and a fresh secure channel under `to` to show the card took it.
 */
export async function rotateKeys(
  tx: Transceiver,
  from: KeySet,
  to: KeySet,
  options: RotateOptions,
): Promise<RotateResult> {
  const run = new Run<RotatePhase>(tx, options, 'selectIsd');
  const sessionOptions = {macOverModifiedApdu: options.macOverModifiedApdu};
  try {
    run.enter('selectIsd');
    expectOk(await run.plain(selectAid(ISD_AID)), 0xa4);
    run.enter('auth');
    const hs = await initializeUpdate(run.timed, options.random, 0);
    const fromKeys = sessionIfMatches(from, hs);
    const toKeys = fromKeys ? null : sessionIfMatches(to, hs);
    if (!fromKeys && !toKeys) {
      throw new WrongKeys(
        'the card proved neither key set; EXTERNAL AUTHENTICATE was not sent',
      );
    }
    if (toKeys) {
      await externalAuthenticate(run.timed, toKeys, hs, sessionOptions);
      run.enter('done');
      return {
        ...run.finish({ok: true}),
        active: 'to',
        rotated: false,
        keyVersion: hs.reply.kvn,
      };
    }
    const session = await externalAuthenticate(
      run.timed,
      fromKeys!,
      hs,
      sessionOptions,
    );
    run.enter('putKey');
    const kvn = hs.reply.kvn;
    const command = putKeys(to, session.keys.dek, {
      p1: options.putKey?.p1 ?? kvn,
      newKvn: options.putKey?.newKvn ?? kvn,
    });
    expectOk(await session.send(run.timed, command), 0xd8);
    run.enter('reopen');
    expectOk(await run.plain(selectAid(ISD_AID)), 0xa4);
    const reopened = await openSession(
      run.timed,
      to,
      options.random,
      sessionOptions,
    );
    run.enter('done');
    return {
      ...run.finish({ok: true}),
      active: 'to',
      rotated: true,
      keyVersion: reopened.keyVersion,
    };
  } catch (e) {
    return run.fail(e);
  }
}

/** Confirm only this set, without PUT KEY or a fallback authentication. */
export async function confirmManagerKeys(
  tx: Transceiver,
  keys: KeySet,
  options: RunOptions,
): Promise<RotateResult> {
  const run = new Run<RotatePhase>(tx, options, 'selectIsd');
  try {
    run.enter('selectIsd');
    expectOk(await run.plain(selectAid(ISD_AID)), 0xa4);
    run.enter('auth');
    const session = await openSession(run.timed, keys, options.random, {
      macOverModifiedApdu: options.macOverModifiedApdu,
    });
    run.enter('done');
    return {
      ...run.finish({ok: true}),
      active: 'to',
      rotated: false,
      keyVersion: session.keyVersion,
    };
  } catch (e) {
    return run.fail(e);
  }
}
