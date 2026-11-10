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
 * - the card's proof of the keys is checked before EXTERNAL AUTHENTICATE
 *   (scp02.ts, and scp03.ts for an SCP03 card);
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
  FACTORY_KVN,
  type FreeMemory,
  type KeyInformation,
  type KeySet,
  type Random,
  type Reply,
  SHIPPED_KVN,
  type SecureSession,
  type Transceiver,
  TransportError,
  WrongKeys,
  deleteAid,
  externalAuthenticate,
  getFreeMemory,
  getKeyInformation,
  initializeUpdate,
  installAndSelectable,
  installForLoad,
  openSession,
  parseFreeMemory,
  parseKeyInformation,
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
 * - SELECT answers `03 00` (FedimintCardApplet.VERSION_MAJOR/MINOR), so a
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
      {apdu: selectAid(CARD_AID), data: [0x03, 0x00]},
      // The slots the install tap keys, and the last one, so an applet that kept a key across a
      // reinstall is seen. Not all 32: each GET_PUB is a round trip in the install tap, and a
      // fresh instance builds every slot's key objects anew at its first GEN_KEY.
      ...[0, 1, 2, 3, 4, 5, 6, 7, 31].map(slot => ({
        apdu: [GET_PUB[0], GET_PUB[1], slot, 0x00, 0x00],
        sw: 0x6985,
      })),
      // And one past the last: a 32-slot build, not an older 8-slot one.
      {apdu: [GET_PUB[0], GET_PUB[1], 32, 0x00, 0x00], sw: 0x6b00},
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
  /**
   * The card manager's keys; the factory set by default. A list is the sets
   * the card may be on (a card's own set, and the factory set it had before
   * a rotation): one INITIALIZE UPDATE is checked against each offline, and
   * EXTERNAL AUTHENTICATE goes out only for the one the card proved, so a
   * card on either costs no failed login.
   */
  keys?: KeySet | readonly KeySet[];
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

export interface ProvisionResult extends RunResult<Phase> {
  /** Which of the `keys` the card proved, by index; 0 for a single set. */
  keySet?: number;
}

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
  /**
   * True when this run made first contact (D182): PUT KEY P1 00 moved an SCP03
   * card from SHIPPED_KVN to `to` at FACTORY_KVN, and the reopen proved it there.
   */
  firstContact?: boolean;
}

/** A refusal of this module's own, before the card is touched further. Shared with gp/reset.ts. */
export class Guard extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'Guard';
  }
}

export function expectOk(reply: Reply, ins: number): Reply {
  if (reply.sw !== 0x9000) {
    throw new CardError(reply.sw, ins);
  }
  return reply;
}

/** The bookkeeping of one run: the phase, the clock, the records, the events. Shared with gp/reset.ts. */
export class Run<P extends string> {
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
  const given = options.keys ?? FACTORY_KEYS;
  const candidates: readonly KeySet[] = Array.isArray(given)
    ? given
    : [given as KeySet];
  // The set the card proved; the later logins in this run use it alone.
  let keys: KeySet = candidates[0] ?? FACTORY_KEYS;
  let keySet: number | undefined;
  const sessionOptions = {macOverModifiedApdu: options.macOverModifiedApdu};
  let session: SecureSession | null = null;
  let blocks: number[][] = [];
  const open = (): SecureSession => {
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
      const info = expectOk(
        await run.plain([0xb0, 0x01, 0x00, 0x00, 0x00]),
        0x01,
      );
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
      const hs = await initializeUpdate(run.timed, options.random, 0);
      for (let i = 0; i < candidates.length; i++) {
        const proved = sessionIfMatches(candidates[i], hs);
        if (proved) {
          keys = candidates[i];
          keySet = i;
          session = await externalAuthenticate(
            run.timed,
            proved,
            hs,
            sessionOptions,
          );
          return;
        }
      }
      throw new WrongKeys(
        candidates.length > 1
          ? `the card proved none of the ${candidates.length} key sets; EXTERNAL AUTHENTICATE was not sent`
          : undefined,
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
    return {...run.finish({ok: true}), keySet};
  } catch (e) {
    if (e instanceof RangeError && blocks.length === 0) {
      return run.fail(
        new Guard(
          `the ${plan.loadFile.name} load file cannot be sent: ${e.message}`,
        ),
      );
    }
    return {...run.fail(e), keySet};
  }
}

export interface RotateOptions extends RunOptions {
  /**
   * PUT KEY's P1 and the new set's version. By default the active set's
   * version for both (in place, as gp --lock does). P1 00 stops guarded on
   * any card but one at first contact, and there this option is refused
   * whatever it holds: first contact sends P1 00 and FACTORY_KVN only, and a
   * run that finds the card at SHIPPED_KVN with `putKey` set stops guarded
   * before EXTERNAL AUTHENTICATE.
   */
  putKey?: {p1?: number; newKvn?: number};
}

/** The three keys PUT KEY put at first contact, as the key information lists them. */
const FIRST_CONTACT_KEYS: readonly KeyInformation[] = [1, 2, 3].map(id => ({
  id,
  kvn: FACTORY_KVN,
  components: [{type: 0x88, length: 16}],
}));

/** A key information listing in words, for a guard's reason. */
function describeKeys(keys: readonly KeyInformation[]): string {
  if (keys.length === 0) {
    return 'no key';
  }
  return keys
    .map(
      k =>
        `key ${toHex([k.id])} at version ${toHex([k.kvn])} (${k.components
          .map(c => `type ${toHex([c.type])}, ${c.length} bytes`)
          .join('; ')})`,
    )
    .join(', ');
}

/**
 * Throws Guard unless GET DATA E0's answer after first contact is what the
 * test card gave after its own (D182): 9000 and a key information template
 * that lists no key at SHIPPED_KVN and holds the three AES-128 keys PUT KEY
 * put at FACTORY_KVN, IDs 01 to 03, and nothing else.
 */
function checkFirstContactKeys(reply: Reply): void {
  if (reply.sw !== 0x9000) {
    throw new Guard(
      `after first contact the card answered GET DATA E0 (its key information) with ${swHex(reply.sw)}, not 9000: whether it still holds the version-FF set is unknown`,
    );
  }
  let keys: KeyInformation[];
  try {
    keys = parseKeyInformation(reply.data);
  } catch {
    throw new Guard(
      `after first contact the card's key information did not parse (${toHex(reply.data)}): whether it still holds the version-FF set is unknown`,
    );
  }
  if (keys.some(k => k.kvn === SHIPPED_KVN)) {
    throw new Guard(
      `after first contact the card still lists key version FF (${describeKeys(keys)}): its shipped set may still open it, beside ours at 01`,
    );
  }
  const expected = (k: KeyInformation) =>
    FIRST_CONTACT_KEYS.some(
      e =>
        e.id === k.id &&
        e.kvn === k.kvn &&
        k.components.length === 1 &&
        k.components[0].type === e.components[0].type &&
        k.components[0].length === e.components[0].length,
    );
  if (
    keys.length !== FIRST_CONTACT_KEYS.length ||
    !keys.every(expected) ||
    new Set(keys.map(k => k.id)).size !== keys.length
  ) {
    throw new Guard(
      `after first contact the card lists ${describeKeys(keys)}, where PUT KEY put keys 01 to 03 at version 01, AES-128, and nothing else`,
    );
  }
}

/**
 * Moves the card manager from `from` to `to`. One INITIALIZE UPDATE, whose
 * card cryptogram is checked offline against both sets: a PUT KEY whose reply
 * was lost leaves the card on `to`, and that is found without a failed login.
 * EXTERNAL AUTHENTICATE goes out only for a set the card proved; then PUT KEY,
 * and a fresh secure channel under `to` to show the card took it.
 *
 * An SCP03 card at SHIPPED_KVN (FF, a J3R452 as it ships) gets first contact
 * (D182): PUT KEY with P1 00 puts `to` at FACTORY_KVN (01), and the card
 * should drop the FF set, as the test card did. The reopen checks both: it
 * reads the key information with GET DATA E0, which needs no key, so no login
 * is counted, and must find what the test card listed after its first
 * contact: ours at 01 and nothing else, no key at FF among them. Then it must
 * prove `to` at 01. Any other answer to the first, or `to` at another
 * version, stops the run guarded, so a card that kept the FF set, or put ours
 * elsewhere, is not reported rotated by this run. A pull after the PUT KEY and
 * before those answers leaves the next run to find `to` at 01, as after any
 * lost reply, and that run does not read the key information: it cannot tell
 * the card from one rotated in place. Any other card has its set replaced in
 * place, with the APDUs this sent before. Each of these stops guarded with
 * nothing sent after INITIALIZE UPDATE: `to` proved at FF (only an in-place
 * rotation of a shipped card puts it there, and the reset refuses FF), FF
 * under SCP02 (never measured), a caller's `putKey` at first contact, and
 * P1 00 on any card but one at first contact (it would add a set beside the
 * one in place).
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
    const kvn = hs.reply.kvn;
    // a J3R452 as it ships: our set goes in at 01 and its FF set goes (D182)
    const first = hs.reply.scp === 3 && kvn === SHIPPED_KVN;
    const fromKeys = sessionIfMatches(from, hs);
    const toKeys = fromKeys ? null : sessionIfMatches(to, hs);
    if (!fromKeys && !toKeys) {
      throw new WrongKeys(
        'the card proved neither key set; EXTERNAL AUTHENTICATE was not sent',
      );
    }
    if (toKeys) {
      if (kvn === SHIPPED_KVN) {
        throw new Guard(
          'the card proved the new key set at version FF, where only an in-place rotation of a shipped card puts it and a reset refuses it: EXTERNAL AUTHENTICATE was not sent',
        );
      }
      await externalAuthenticate(run.timed, toKeys, hs, sessionOptions);
      run.enter('done');
      return {
        ...run.finish({ok: true}),
        active: 'to',
        rotated: false,
        keyVersion: kvn,
      };
    }
    if (first && options.putKey !== undefined) {
      throw new Guard(
        "first contact takes P1 00 and version 01 only, never a caller's putKey: EXTERNAL AUTHENTICATE was not sent",
      );
    }
    const p1 = first ? 0x00 : (options.putKey?.p1 ?? kvn);
    const newKvn = first ? FACTORY_KVN : (options.putKey?.newKvn ?? kvn);
    if (kvn === SHIPPED_KVN && !first) {
      throw new Guard(
        'the card is at key version FF under SCP02, where first contact was never measured: EXTERNAL AUTHENTICATE was not sent',
      );
    }
    if (p1 === 0x00 && !first) {
      throw new Guard(
        `PUT KEY with P1 00 would add a key set beside version ${toHex([kvn])}: EXTERNAL AUTHENTICATE was not sent`,
      );
    }
    const session = await externalAuthenticate(
      run.timed,
      fromKeys!,
      hs,
      sessionOptions,
    );
    run.enter('putKey');
    const command = putKeys(to, session, {p1, newKvn});
    expectOk(await session.send(run.timed, command), 0xd8);
    run.enter('reopen');
    expectOk(await run.plain(selectAid(ISD_AID)), 0xa4);
    if (first) {
      // the test card dropped its FF set when PUT KEY P1 00 added ours: GET
      // DATA E0, plain, then listed ours at 01 alone, where it had listed the
      // three at FF before; one that kept that set would still open to it
      checkFirstContactKeys(await run.plain(getKeyInformation()));
    }
    const reopened = await openSession(
      run.timed,
      to,
      options.random,
      sessionOptions,
    );
    if (first && reopened.keyVersion !== FACTORY_KVN) {
      throw new Guard(
        `after first contact the card proved the new key set at version ${toHex([reopened.keyVersion ?? 0])}; PUT KEY asked for 01`,
      );
    }
    run.enter('done');
    return {
      ...run.finish({ok: true}),
      active: 'to',
      rotated: true,
      keyVersion: reopened.keyVersion,
      ...(first ? {firstContact: true} : {}),
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
