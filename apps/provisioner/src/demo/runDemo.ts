/**
 * The demo's scripted states (docs/PROVISIONER-PLAN.md § 5, "Kept deliberately").
 * A demo run is the real thing: `provision()` against `fakeCard`, at the real
 * cadence, with the real events. Nothing here is reachable in a build that has
 * demo mode off.
 *
 * The states are named for what the operator sees on the card, and each one
 * pins to exactly one stop kind. `demo.test.ts` is the pin: delete a mapping
 * and the test names the state that no longer maps.
 *
 * Every state here is reachable on THIS card. Cashu's `hasValue` ("holds
 * value") and `oldVersion` ("old software") states are **not** ported: there is
 * no `GET_INFO` to read a holding from, and no second version to find — the
 * sweep is gated by the sidecar pre-check and the operator's Replace switch
 * (plan § 5, § 10), not by the card. A demo state that cannot happen is a
 * screen the operator will trust and the card will not honour. `foreign`
 * takes `hasValue`'s place: it is the stop that exercises the guard for real,
 * as a whitelisted-DELETE refusal.
 */
import {
  FEDIMINT_PLAN,
  provision,
  type ProvisionResult,
  type RunEvent,
} from '../cardkit';
import {fakeCard, type FakeCardProfile, type Installed} from './fakeCard';

export type DemoStateId =
  | 'ok'
  | 'pulled'
  | 'foreign'
  | 'hasOld'
  | 'oldVersion'
  | 'wrongKeys'
  | 'noSession';

export interface DemoState {
  id: DemoStateId;
  name: string;
  sub: string;
}

/** The chooser's list, in the order it reads best; the ids are what tests pin. */
export const DEMO_STATES: readonly DemoState[] = Object.freeze([
  {
    id: 'ok',
    name: 'A clean card, programmed',
    sub: 'The happy path — about ten seconds',
  },
  {
    id: 'pulled',
    name: 'Card lifted mid-write',
    sub: 'A torn run recovers on the next tap',
  },
  {
    id: 'foreign',
    name: 'A card that is not ours',
    sub: 'The stop that never programs',
  },
  {
    id: 'hasOld',
    name: 'A card with this card already on it',
    sub: 'Operator chooses to reuse it',
  },
  {
    id: 'oldVersion',
    name: 'A card running older software',
    sub: 'The tap reinstalls it',
  },
  {
    id: 'wrongKeys',
    name: 'The card manager refuses the keys',
    sub: 'Short words: a lock is for good',
  },
  {
    id: 'noSession',
    name: 'The phone loses the session',
    sub: 'Nothing reached the card',
  },
]);

/**
 * The profile each state runs against. `noApdu` is the one state with no card
 * at all: the reader never opened a session, so no run starts.
 */
export const SCENARIO_PROFILE: Readonly<
  Record<DemoStateId, FakeCardProfile | 'noApdu'>
> = {
  ok: {installed: 'none'},
  // 23 blocks at 247 bytes (docs/CAP-BUILD.md): lifted in the middle, not at the end
  pulled: {installed: 'none', pullAtBlock: 12},
  foreign: {installed: 'foreign'},
  hasOld: {installed: 'empty'},
  oldVersion: {installed: 'old'},
  wrongKeys: {installed: 'none', keys: 'spike'},
  noSession: 'noApdu',
};

export interface DemoRun {
  state: DemoStateId;
  /** null: the session-failure state sent nothing, so there is no run to report. */
  result: ProvisionResult | null;
  sessionError?: string;
}

/** What the demo shows after a run: the state's own stop, or `ok`. */
export type DemoStop = DemoStateId | 'ok';

const NO_SESSION = 'No card session: TagConnectionLost.';

/**
 * Runs one demo state. `replace` is the operator's answer to "Remove what is
 * on the card and reuse it?" — ignored by every state that does not reach the
 * sweep.
 */
export async function runDemo(
  state: DemoStateId,
  {
    replace = false,
    speed = 1,
    onEvent = () => undefined,
  }: {
    replace?: boolean;
    speed?: number;
    onEvent?: (e: RunEvent) => void;
  } = {},
): Promise<DemoRun> {
  const profile = SCENARIO_PROFILE[state];
  if (profile === 'noApdu') {
    return {state, result: null, sessionError: NO_SESSION};
  }
  const fake = fakeCard(profile, {speed});
  const result = await provision(fake.tx, FEDIMINT_PLAN, {
    replace,
    // a demo card must not draw from the phone's CSPRNG: fixed bytes keep the
    // run reproducible, and nothing here is ever sent to a real card
    random: length => new Array<number>(length).fill(0x5a),
    now: Date.now,
    onEvent,
  });
  return {state, result};
}

/**
 * Which stop the loop shows for a demo run. Total over `ProvisionResult`: a
 * stop kind cardkit can produce and this does not name is a TypeScript error,
 * so a new kind cannot slip past the demo's screens. `refused` has no state of
 * its own — the loop shows it as the run that ended, and only `foreign` is
 * scripted to reach it.
 */
export function stopFor(run: DemoRun): DemoStop {
  if (run.sessionError) {
    return 'noSession';
  }
  const r = run.result;
  if (!r) {
    return 'noSession';
  }
  if (r.ok) {
    return 'ok';
  }
  switch (r.kind) {
    case 'pulled':
      return 'pulled';
    case 'wrongKeys':
      return 'wrongKeys';
    case 'guarded':
      // no plan for this card sets a guard, so this is the load file's own
      // refusal, before a byte was sent: nothing reached the card
      return 'noSession';
    case 'refused':
      return run.state;
    case undefined:
      return 'noSession';
  }
}

/** Whether a demo run leaves this card's package on the card. */
export function programsCard(state: DemoStateId): boolean {
  const profile = SCENARIO_PROFILE[state];
  if (profile === 'noApdu') {
    return false;
  }
  const installed: Installed = profile.installed;
  return installed === 'none' || installed === 'empty' || installed === 'old';
}
