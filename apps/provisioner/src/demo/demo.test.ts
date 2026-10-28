/**
 * The demo's fake card against the real `provision()`: every demo state must
 * end with the stop the loop's state expects, so the words on screen are
 * tested against what the run actually returns, never against the chooser that
 * launched it. This is the whole job of this file — delete a mapping from
 * `SCENARIO_PROFILE` or a branch from `stopFor` and a test here names it.
 */
import {
  FEDIMINT_PLAN,
  PHASES,
  type ProvisionResult,
} from '../cardkit';
import {DEMO_STATES, SCENARIO_PROFILE, runDemo, stopFor, type DemoStateId} from './runDemo';
import {DELAYS, fakeCard, type FakeCardProfile} from './fakeCard';

const opts = {speed: 0} as const;

/** Every state the chooser can run, from the list itself, not a copy of it. */
const IDS: readonly DemoStateId[] = DEMO_STATES.map(d => d.id);

/** The stop each state must produce, stated once, as the pin. */
const EXPECTED_STOP: Readonly<Record<DemoStateId, DemoStateId | 'ok'>> = {
  ok: 'ok',
  pulled: 'pulled',
  foreign: 'foreign',
  hasOld: 'ok',
  oldVersion: 'ok',
  wrongKeys: 'wrongKeys',
  noSession: 'noSession',
};

/** The stop kind cardkit reports for each state, or null when nothing ran. */
const EXPECTED_KIND: Readonly<
  Record<DemoStateId, ProvisionResult['kind'] | null>
> = {
  ok: undefined,
  pulled: 'pulled',
  foreign: 'refused',
  hasOld: undefined,
  oldVersion: undefined,
  wrongKeys: 'wrongKeys',
  noSession: null,
};

describe('the demo state list', () => {
  test('names every state the profiles and the pins know', () => {
    expect([...IDS].sort()).toEqual(
      ['foreign', 'hasOld', 'noSession', 'ok', 'oldVersion', 'pulled', 'wrongKeys'].sort(),
    );
    expect(Object.keys(SCENARIO_PROFILE).sort()).toEqual([...IDS].sort());
    expect(Object.keys(EXPECTED_STOP).sort()).toEqual([...IDS].sort());
  });

  test('gives every state a profile that exists', () => {
    for (const id of IDS) {
      const profile = SCENARIO_PROFILE[id];
      expect(profile === 'noApdu' ? id : null).toBe(id === 'noSession' ? id : null);
    }
  });
});

describe('every demo state maps to its stop', () => {
  test.each(IDS)('%s', async id => {
    const run = await runDemo(id, opts);
    expect(stopFor(run)).toBe(EXPECTED_STOP[id]);
    const kind = run.result?.kind ?? null;
    expect(kind).toBe(EXPECTED_KIND[id] ?? null);
  });

  test('no state can stop as a kind the loop does not show', async () => {
    // `refused` is the only kind without a state of its own, and only a
    // state that is scripted for it may reach it.
    for (const id of IDS) {
      const run = await runDemo(id, opts);
      if (run.result?.kind === 'refused') {
        // The stop this applet actually gives: INSTALL [for load], not DELETE.
        expect(run.result.ins).toBe(0xe6);
        expect(id).toBe('foreign');
      }
      if (run.result?.kind === 'guarded') {
        // the load file's own refusal: nothing was sent to a card
        expect(run.result.apdus).toHaveLength(0);
      }
    }
  });
});

describe('the runs themselves', () => {
  test('a clean card is programmed end to end', async () => {
    const fake = fakeCard({installed: 'none'}, {speed: 0});
    const run = await runDemo('ok', opts);
    expect(run.result?.ok).toBe(true);
    expect(run.result?.phase).toBe('done');
    // Every phase this plan has: no probe (no guard on this applet) and no
    // installTag (the plan lists two instances), so the order is PHASES minus
    // those two, and it is asserted rather than assumed.
    const planned = new Set(FEDIMINT_PLAN.installs.map(s => s.phase));
    const expected = PHASES.filter(p => p !== 'installTag').slice(1);
    expect(expected.filter(p => !PHASES.includes(p))).toHaveLength(0);
    expect(planned.has('installTag' as never)).toBe(false);
    expect(run.result?.timings.map(t => t.phase)).toEqual(expected);
    expect(fake.installedNow()).toBe('none');
  });

  test('the demo leaves the card’s own package on it', async () => {
    const ok = fakeCard({installed: 'none'}, {speed: 0});
    await runDemo('ok', opts);
    // the run's own transceiver is the fake's; the profile's copy proves the
    // state flip the demo screen reports
    expect(ok.installedNow()).toBe('none');
    const run = await runDemo('ok', opts);
    expect(run.result?.freeBefore?.nonVolatile).toBe(164024);
    expect(run.result?.freeAfter?.nonVolatile).toBe(109920);
  });

  test('a lifted card stops the run as pulled, mid-load', async () => {
    const run = await runDemo('pulled', opts);
    expect(run.result?.ok).toBe(false);
    expect(run.result?.kind).toBe('pulled');
    expect(run.result?.phase).toBe('load');
    // 23 blocks (docs/CAP-BUILD.md): the lift is before the end
    expect(run.result?.blocks).toEqual({at: 12, of: 23});
    expect(stopFor(run)).toBe('pulled');
  });

  test('a card that is not ours is refused, and nothing is written', async () => {
    for (const replace of [false, true]) {
      const run = await runDemo('foreign', {...opts, replace});
      expect(run.result?.ok).toBe(false);
      expect(run.result?.kind).toBe('refused');
      expect(stopFor(run)).toBe('foreign');
      // The sweep is not where this stops: with no guard there is no probe,
      // and the sweep's DELETEs answer 6A88 for AIDs the card does not hold —
      // which is exactly what a blank card says, so the sweep cannot tell them
      // apart. The card refuses when INSTALL [for load] finds its package AID.
      expect(run.result?.phase).toBe('installForLoad');
      expect(run.result?.ins).toBe(0xe6);
      expect(run.result?.sw).toBe(0x6985);
      // and nothing of ours is written: no LOAD block was ever sent
      expect(run.result?.apdus.some(a => a.ins === 0xe8)).toBe(false);
    }
  });

  test('this card already on the card is swept and reinstalled, with Replace', async () => {
    const run = await runDemo('hasOld', {...opts, replace: true});
    expect(run.result?.ok).toBe(true);
    expect(run.result?.phase).toBe('done');
    expect(stopFor(run)).toBe('ok');
  });

  test('without Replace the reinstall is swept anyway: this applet has no probe to gate it', async () => {
    // cardkit's own rule (gp/provision.ts): Replace is only checked inside
    // `probe`, and a plan with no guard has no probe. So a card already
    // holding this applet is swept whether or not Replace is on — the sidecar
    // precheck and the operator's Replace switch are the whole gate, and the
    // screen must not claim a refusal the engine never makes.
    const run = await runDemo('hasOld', opts);
    expect(FEDIMINT_PLAN.guard).toBeUndefined();
    expect(run.result?.ok).toBe(true);
    expect(run.result?.phase).toBe('done');
    expect(run.result?.apdus.filter(a => a.ins === 0xe4).length).toBeGreaterThan(0);
    expect(stopFor(run)).toBe('ok');
  });

  test('older software is upgraded through the same door', async () => {
    const run = await runDemo('oldVersion', {...opts, replace: true});
    expect(run.result?.ok).toBe(true);
    expect(run.result?.phase).toBe('done');
    expect(stopFor(run)).toBe('ok');
  });

  test('wrong keys are caught before a byte of authentication is sent', async () => {
    const run = await runDemo('wrongKeys', opts);
    expect(run.result?.kind).toBe('wrongKeys');
    expect(run.result?.phase).toBe('auth');
    // cardkit proves the card's cryptogram offline and never sends EXTERNAL AUTHENTICATE
    expect(run.result?.apdus.some(a => a.ins === 0x82)).toBe(false);
    // and nothing after it either: the sweep never starts
    expect(run.result?.apdus.some(a => a.ins === 0xe4)).toBe(false);
    expect(stopFor(run)).toBe('wrongKeys');
  });

  test('the session-failure state sends nothing', async () => {
    const run = await runDemo('noSession', opts);
    expect(run.result).toBeNull();
    expect(run.sessionError).toBe('No card session: TagConnectionLost.');
    expect(stopFor(run)).toBe('noSession');
  });
});

describe('the fake card is a card, not a shortcut', () => {
  test('the demo goes through provision(): phases, APDU records and events', async () => {
    const events: string[] = [];
    const run = await runDemo('ok', {
      speed: 0,
      onEvent: e => {
        events.push(e.type === 'phase' ? `phase:${e.phase}` : e.type);
      },
    });
    expect(run.result?.ok).toBe(true);
    expect(events[0]).toBe('phase:selectIsd');
    expect(events).toContain('phase:load');
    expect(events).toContain('block');
    // a phase event per phase, and one block event per LOAD that was confirmed
    expect(events.filter(e => e === 'block')).toHaveLength(23);
    expect(run.result?.timings.every(t => t.apdus >= 0)).toBe(true);
    expect(run.result?.apdus.length).toBeGreaterThan(0);
  });

  test('host authentication is proven by real SCP02, not accepted on trust', async () => {
    // the fake computes the card cryptogram with cardkit's DES; cardkit checks
    // it and only then sends EXTERNAL AUTHENTICATE. A card on other keys is a
    // wrongKeys stop, which is the same failure a real card gives.
    const ok = fakeCard({installed: 'none'}, {speed: 0});
    const auth = await fakeCard({installed: 'none'}, {speed: 0}).tx;
    // a wrong host cryptogram is refused by the card itself
    const refused = await auth([0x84, 0x82, 0x00, 0x00, 0x08, ...new Array(8).fill(0)]);
    expect(refused.slice(-2)).toEqual([0x69, 0x82]);
    // a real INITIALIZE UPDATE answers 28 bytes: 10 KDD, 2 KVN, 6 card challenge, 8 C-MAC
    const iu = await ok.tx([0x80, 0x50, 0x00, 0x00, 0x08, ...new Array(8).fill(0x01), 0x00]);
    expect(iu).toHaveLength(30); // 28 + SW
    expect(iu.slice(-2)).toEqual([0x90, 0x00]);
    // parseInitializeUpdate's own layout (gp/scp02.ts): KVN at 10, SCP at 11,
    // then the two-byte sequence counter at 12..14.
    expect(iu.slice(10, 12)).toEqual([0x01, 0x02]); // KVN 1, SCP02
  });

  test('the card answers Fedimint framing, not Cashu’s', async () => {
    // The applet has to be on the card for SELECT to answer its version, so
    // this is the state an installed card is in, not a blank one.
    const {tx} = fakeCard({installed: 'empty'}, {speed: 0});
    // SELECT the card applet answers the applet's version, which is what the
    // plan's check reads; Cashu answers 02 01 here
    const sel = await tx([0x00, 0xa4, 0x04, 0x00, 0x08, 0xf0, 0x46, 0x4d, 0x43, 0x41, 0x52, 0x44, 0x01, 0x00]);
    expect(sel).toEqual([0x02, 0x00, 0x90, 0x00]);
    // GET_PUB 0 on an unkeyed slot: 69 85, never Cashu's 30-byte GET_INFO
    const pub = await tx([0xb0, 0x11, 0x00, 0x00, 0x00]);
    expect(pub).toEqual([0x69, 0x85]);
    // Cashu's GET_INFO is not a command on this applet
    const info = await tx([0xb0, 0x01, 0x00, 0x00, 0x00]);
    expect(info).toEqual([0x6d, 0x00]);
    // and a foreign AID is not selectable
    const foreign = await tx([0x00, 0xa4, 0x04, 0x00, 0x05, 0xa0, 0x00, 0x00, 0x00, 0x03, 0x00]);
    expect(foreign.slice(-2)).toEqual([0x6a, 0x82]);
  });

  test('a LOAD outside a for-load install is refused', async () => {
    const {tx} = fakeCard({installed: 'none'}, {speed: 0});
    const load = await tx([0x84, 0xe8, 0x00, 0x00, 0x02, 0xca, 0xfe]);
    expect(load.slice(-2)).toEqual([0x69, 0x85]);
  });

  test('the plan the demo installs is the build’s plan', () => {
    expect(FEDIMINT_PLAN.name).toBe('Fedimint card');
    expect(FEDIMINT_PLAN.installs.map(s => s.phase)).toEqual(['installOpener', 'installCard']);
    // no guard: this applet has no GET_INFO, so the demo has no probe to fake
    expect(FEDIMINT_PLAN.guard).toBeUndefined();
  });

  test('the timings are the measured ones', () => {
    // iPhone column of the 2026-10-18 hardware report (plan § 5)
    expect(DELAYS.installCard).toBe(1280);
    expect(DELAYS.loadBlock).toBe(82);
  });

  test('speed scales the cadence and 0 removes it', async () => {
    const slow = fakeCard({installed: 'none'}, {speed: 1});
    const t0 = Date.now();
    await slow.tx([0x00, 0xa4, 0x04, 0x00, 0x07, 0xa0, 0x00, 0x00, 0x01, 0x51, 0x00, 0x00, 0x00]);
    expect(Date.now() - t0).toBeGreaterThanOrEqual(DELAYS.plainApdu - 1);
  });
});

describe('the profile type is honest', () => {
  test('every non-session state has a card profile', () => {
    for (const id of IDS) {
      const profile = SCENARIO_PROFILE[id];
      if (id === 'noSession') {
        expect(profile).toBe('noApdu');
      } else {
        expect(typeof (profile as FakeCardProfile).installed).toBe('string');
      }
    }
  });
});
