import {FACTORY_KEYS, fromHex, toHex, type KeySet} from './cardkit';
import {
  holdsPlace,
  installTap,
  NEITHER_SET,
  NOT_OWN,
  type SeenTap,
  type TapOptions,
  type TapResult,
} from './installTap';
import type {CardPhase} from './store';
import type {ManagerKeysReply} from './ops';
import {FakeGpCard, J3R180_CHIP, J3R452_CHIP, J3R452_CPLC} from './testing/fakeGpCard';

/**
 * The install tap against a card that keeps its card-manager keys across taps
 * (OPS-PLAN O5): a blank card installed and rotated to its own keys, a tap
 * broken after PUT KEY and finished by the next, one broken before it, a card
 * on keys nobody holds, and ops not answering.
 */

const OWN: ManagerKeysReply = {
  chip: J3R180_CHIP,
  key_version: 1,
  enc: '0f1e2d3c4b5a69788796a5b4c3d2e1f0',
  mac: '1f2e3d4c5b6a79889706b5c4d3e2f101',
  dek: '2f3e4d5c6b7a8998a716c5d4e3f20112',
};
const ownKeys = () => ({
  enc: fromHex(OWN.enc),
  mac: fromHex(OWN.mac),
  dek: fromHex(OWN.dek),
});
const PUT_KEY = 0xd8;
const INIT_UPDATE = 0x50;
const EXT_AUTH = 0x82;

function options(over: Partial<TapOptions> = {}) {
  const managerKeys = jest.fn(async (chip: string) => ({...OWN, chip}));
  let t = 0;
  return {
    managerKeys,
    opts: {
      pin: '482916',
      managerKeys,
      random: (n: number) =>
        Array.from({length: n}, (_, i) => (i * 37 + 11) & 0xff),
      now: () => (t += 5),
      ...over,
    } as TapOptions,
  };
}

// The card-manager tests, against a J3R180 (SCP02) and against a J3R452 on
// the public default at version 01 (SCP03), with the same assertions.
describe.each([2, 3] as const)('SCP0%i', scp => {
  test('a blank card: installed on the factory keys, rotated to its own, keyed and PINned in one tap', async () => {
    const card = new FakeGpCard({scp});
    const {opts, managerKeys} = options();
    const phases: string[] = [];
    const r = await installTap(card.transceive, {
      ...opts,
      onPhase: p => phases.push(p),
    });
    expect(r.installed).toEqual({ok: true});
    expect(r.setup).toEqual(
      expect.objectContaining({ok: true, generated: [0, 1, 2, 3, 4, 5, 6, 7]}),
    );
    expect(r.chip).toBe(J3R180_CHIP);
    expect(r.rotated).toBe(true);
    expect(managerKeys).toHaveBeenCalledWith(J3R180_CHIP);
    expect(card.sameKeys(ownKeys())).toBe(true);
    expect(card.ins().filter(i => i === PUT_KEY)).toHaveLength(1);
    expect(card.failedAuths).toBe(0);
    expect(card.applet?.pin).toBe('482916');
    expect(phases).toEqual(['managerKeys', 'rotate']);
    // the order: CPLC before the first login, PUT KEY after the install's last INSTALL and before GEN_KEY
    const at = (ins: number, from = 0) => card.ins().indexOf(ins, from);
    expect(at(0xca)).toBeLessThan(at(INIT_UPDATE));
    expect(card.ins().lastIndexOf(0xe6)).toBeLessThan(at(PUT_KEY));
    expect(at(PUT_KEY)).toBeLessThan(at(0x10));
  });

  test('a blank card with the 32-slot applet: slots 0-7 keyed in the install tap, then the PIN; 8-31 left for a till', async () => {
    const card = new FakeGpCard({scp, appletSlots: 32});
    const r = await installTap(card.transceive, options().opts);
    expect(r.installed).toEqual({ok: true});
    expect(r.setup).toEqual(
      expect.objectContaining({ok: true, slotCount: 32, generated: Array.from({length: 8}, (_, i) => i)}),
    );
    expect(r.setup?.ok && r.setup.keys).toHaveLength(8);
    expect(card.applet!.slots.slice(0, 8).every(s => s !== null)).toBe(true);
    expect(card.applet!.slots.slice(8).every(s => s === null)).toBe(true);
    expect(card.applet!.pin).toBe('482916');
    // SET_PIN only after slot 7's key.
    const b0 = card.log.filter(a => a[0] === 0xb0);
    expect(b0.findIndex(a => a[1] === 0x41)).toBeGreaterThan(b0.findIndex(a => a[1] === 0x10 && a[2] === 7));
  });

  test('a 32-slot card whose PIN answer was lost: one VERIFY, its 8 keys read back', async () => {
    const card = new FakeGpCard({scp, keys: ownKeys(), appletSlots: 32}).withLiftedApplet(8, '135792');
    const keys = card.applet!.slots.slice(0, 8).map(s => toHex(s!));
    const r = await installTap(
      card.transceive,
      options({
        resumeCheck: async (chip: string, shortId: string) => ({short_id: shortId, chip, known: false, resumable: true, reasons: []}),
        attemptedPin: chip => (chip === J3R180_CHIP ? '135792' : null),
      }).opts,
    );
    expect(r.finishedWithAttemptedPin).toBe(true);
    expect(r.setup?.ok && r.setup.keys.map(toHex)).toEqual(keys);
    expect(r.setup).toEqual(expect.objectContaining({generated: [], slotCount: 32}));
  });

  test('a tap broken after PUT KEY: the next tap finds the card on its own keys and finishes it', async () => {
    const card = new FakeGpCard({scp});
    card.pull(apdu => apdu[1] === PUT_KEY, 'after');
    const first = options();
    const r1 = await installTap(card.transceive, first.opts);
    expect(r1.installed).toEqual(
      expect.objectContaining({ok: false, kind: 'pulled', phase: 'rotate'}),
    );
    expect(card.sameKeys(ownKeys())).toBe(true);

    card.reset();
    const second = options();
    const r2 = await installTap(card.transceive, second.opts);
    expect(r2.installed).toEqual({ok: true});
    expect(r2.setup?.ok).toBe(true);
    expect(r2.rotated).toBe(false);
    // ops answered the same set twice; PUT KEY went once; no login failed
    expect(second.managerKeys).toHaveBeenCalledWith(J3R180_CHIP);
    expect(card.ins().filter(i => i === PUT_KEY)).toHaveLength(1);
    expect(card.failedAuths).toBe(0);
  });

  test('a tap broken before PUT KEY, or mid-load: the factory keys still work and the next tap rotates', async () => {
    for (const [test, when] of [
      [(a: number[]) => a[1] === PUT_KEY, 'before'],
      [(a: number[]) => a[1] === 0xe8 && a[3] === 4, 'after'],
    ] as const) {
      const card = new FakeGpCard({scp});
      card.pull(test, when);
      const r1 = await installTap(card.transceive, options().opts);
      expect(r1.installed).toEqual(
        expect.objectContaining({ok: false, kind: 'pulled'}),
      );
      expect(card.sameKeys(FACTORY_KEYS)).toBe(true);
      card.reset();
      const r2 = await installTap(card.transceive, options().opts);
      expect(r2.installed).toEqual({ok: true});
      expect(r2.rotated).toBe(true);
      expect(card.sameKeys(ownKeys())).toBe(true);
      expect(card.failedAuths).toBe(0);
    }
  });

  test('a card on keys neither ops nor the factory has: one INITIALIZE UPDATE, no EXTERNAL AUTHENTICATE, a stop', async () => {
    const stranger = {
      enc: fromHex('77'.repeat(16)),
      mac: fromHex('77'.repeat(16)),
      dek: fromHex('77'.repeat(16)),
    };
    const card = new FakeGpCard({scp, keys: stranger});
    const r = await installTap(card.transceive, options().opts);
    expect(r.installed).toEqual(
      expect.objectContaining({
        ok: false,
        kind: 'wrongKeys',
        reason: NEITHER_SET,
      }),
    );
    expect(r.setup).toBeNull();
    expect(card.ins().filter(i => i === INIT_UPDATE)).toHaveLength(1);
    expect(card.ins()).not.toContain(EXT_AUTH);
    expect(card.failedAuths).toBe(0);
    expect(card.instances.size).toBe(0);
  });

  test('ops without an answer (unreachable, another operator’s chip, no master): nothing written', async () => {
    const card = new FakeGpCard({scp});
    const {opts} = options({
      managerKeys: async () => {
        throw new Error('this card’s manager keys belong to another operator');
      },
    });
    const r = await installTap(card.transceive, opts);
    expect(r.installed).toEqual(
      expect.objectContaining({
        ok: false,
        kind: 'noManagerKeys',
        reason: 'this card’s manager keys belong to another operator',
      }),
    );
    expect(r.chip).toBe(J3R180_CHIP);
    expect(card.ins()).not.toContain(INIT_UPDATE);
    expect(card.sameKeys(FACTORY_KEYS)).toBe(true);
  });

  test('a blank card already on its own keys installs with them and needs no rotation', async () => {
    const card = new FakeGpCard({scp, keys: ownKeys()});
    const r = await installTap(card.transceive, options().opts);
    expect(r.installed).toEqual({ok: true});
    expect(r.rotated).toBe(false);
    expect(card.ins()).not.toContain(PUT_KEY);
    expect(card.ins().filter(i => i === INIT_UPDATE)).toHaveLength(1);
  });

  test('a keyed card is never installed over, and its chip is not even read', async () => {
    const card = new FakeGpCard({scp}).withFinishedApplet();
    const r = await installTap(card.transceive, options().opts);
    expect(r.installed).toEqual(
      expect.objectContaining({ok: false, kind: 'guarded'}),
    );
    expect(card.ins()).not.toContain(0xca);
  });

  test('ops’ keys are checked: a reply for another chip is no answer', async () => {
    const card = new FakeGpCard({scp});
    const {opts} = options({
      managerKeys: async () => ({...OWN, chip: 'cplc-v1:' + '0'.repeat(24)}),
    });
    const r = await installTap(card.transceive, opts);
    expect(r.installed).toEqual(expect.objectContaining({kind: 'noManagerKeys'}));
    expect(toHex(card.keys.enc)).toBe(toHex(FACTORY_KEYS.enc));
  });

  /* ------------------------------------------------------------------------- */
  /* A tap lifted during GEN_KEY, SET_PIN or VERIFY_PIN, and its resume        */
  /* ------------------------------------------------------------------------- */

  const GEN_KEY = 0x10;
  const SET_PIN = 0x41;
  const VERIFY_PIN = 0x40;
  const PIN = '482916';

  /** VERIFY_PINs that could spend a try: a PIN's length, not the free 3-byte status read. */
  const pinTries = (card: FakeGpCard, from = 0) =>
    card.log.slice(from).filter(a => a[0] === 0xb0 && a[1] === VERIFY_PIN && a[4] >= 4).length;

  const shortIdOfSlot0 = (card: FakeGpCard) => toHex(card.applet!.slots[0]!.slice(-4)).toLowerCase();

  /** Options with ops' resume check: nothing held for the card, unless told otherwise. */
  function resumeOptions(
    over: Partial<TapOptions> = {},
    answer: Partial<{known: boolean; resumable: boolean; reasons: string[]}> = {},
  ) {
    const resumeCheck = jest.fn(async (chip: string, shortId: string) => ({
      short_id: shortId,
      chip,
      known: false,
      resumable: true,
      reasons: [] as string[],
      ...answer,
    }));
    const base = options({resumeCheck, ...over});
    return {...base, resumeCheck};
  }

  // An install puts applet 2.1 on the card, so a freshly installed card always has 32 slots.
  test.each([32])('a tap lifted at any APDU of GEN_KEY, SET_PIN or VERIFY_PIN (%i slots): the next tap finishes the card, no failed login, at most one PIN try', async slots => {
    // The finish's APDUs on the wire, after the rotation: SELECT, then the B0 commands.
    const clean = new FakeGpCard({scp, appletSlots: slots});
    await installTap(clean.transceive, resumeOptions().opts);
    const put = clean.ins().indexOf(PUT_KEY);
    const finish = clean.log.slice(put).filter(a => a[0] === 0xb0).length;
    // The slot-count probe (GET_PUB past slot 7, and past 31 on a 32-slot card),
    // GET_PUB + GEN_KEY per slot the install tap keys (0-7), SET_PIN, VERIFY_PIN.
    expect(finish).toBe((slots === 8 ? 1 : 2) + 8 * 2 + 2);

    for (let k = 0; k < finish; k++) {
      for (const when of ['before', 'after'] as const) {
        const card = new FakeGpCard({scp, appletSlots: slots});
        let rotated = false;
        let seen = 0;
        card.pull(apdu => {
          if (apdu[1] === PUT_KEY) {
            rotated = true;
          }
          return rotated && apdu[0] === 0xb0 && seen++ === k;
        }, when);
        await expect(installTap(card.transceive, resumeOptions().opts)).rejects.toThrow('Tag was lost');
        const label = `${when} B0 APDU ${k}`;
        expect([label, card.sameKeys(ownKeys())]).toEqual([label, true]);
        const pinWent = card.log.some(a => a[0] === 0xb0 && a[1] === SET_PIN);
        const keyedBefore = card.applet!.slots[0] !== null;
        const oldSlots = card.applet!.slots.map(s => (s ? toHex(s) : null));
        const oldId = keyedBefore ? shortIdOfSlot0(card) : null;
        const pinBefore = card.applet!.pin;

        card.reset();
        const from = card.log.length;
        const {opts, resumeCheck} = resumeOptions({
          // The loop keeps the PIN only when SET_PIN went out, for that chip.
          attemptedPin: chip => (pinWent && chip === J3R180_CHIP ? PIN : null),
          pin: '730194',
        });
        const r = await installTap(card.transceive, opts);
        expect([label, r.installed]).toEqual([label, {ok: true}]);
        expect([label, r.setup?.ok]).toEqual([label, true]);
        expect([label, card.failedAuths]).toEqual([label, 0]);
        expect([label, pinTries(card, from) <= 1]).toEqual([label, true]);
        expect([label, card.applet!.tries]).toEqual([label, 3]);
        expect(card.log.slice(from).filter(a => a[1] === EXT_AUTH && a[0] === 0x84).length).toBeLessThanOrEqual(1);
        expect(card.log.slice(from).filter(a => a[1] === PUT_KEY)).toHaveLength(0);
        if (!keyedBefore) {
          // Lost before slot 0 had a key: an unkeyed applet, keyed as on any first tap.
          expect([label, r.resumedFrom]).toEqual([label, undefined]);
          expect(card.applet!.pin).toBe('730194');
        } else if (pinBefore === null) {
          // No PIN: every slot made again, a fresh PIN, a new short id.
          expect([label, r.resumedFrom]).toEqual([label, oldId]);
          expect(r.setup).toEqual(expect.objectContaining({generated: Array.from({length: 8}, (_, i) => i), slotCount: slots}));
          expect(card.applet!.slots.slice(0, 8).every((s, i) => toHex(s!) !== oldSlots[i])).toBe(true);
          expect(card.applet!.slots.slice(8).every(s => s === null)).toBe(true);
          expect(shortIdOfSlot0(card)).not.toBe(oldId);
          expect(card.applet!.pin).toBe('730194');
          expect(resumeCheck).toHaveBeenCalledWith(J3R180_CHIP, oldId);
        } else {
          // SET_PIN took and its answer was lost: one VERIFY with the PIN sent, the keys kept.
          expect([label, r.finishedWithAttemptedPin]).toEqual([label, true]);
          expect(r.resumedFrom).toBeUndefined();
          expect(r.setup).toEqual(expect.objectContaining({generated: [], slotCount: slots}));
          expect(card.applet!.slots.map(s => (s ? toHex(s) : null))).toEqual(oldSlots);
          expect(card.applet!.pin).toBe(PIN);
          expect(pinTries(card, from)).toBe(1);
        }
      }
    }
  });

  test('a resume lifted itself, mid re-key: the next tap re-keys again and finishes', async () => {
    const card = new FakeGpCard({scp, keys: ownKeys()}).withLiftedApplet(5);
    const first = shortIdOfSlot0(card);
    let gens = 0;
    card.pull(a => a[0] === 0xb0 && a[1] === GEN_KEY && ++gens === 4, 'after');
    await expect(installTap(card.transceive, resumeOptions().opts)).rejects.toThrow('Tag was lost');
    const second = shortIdOfSlot0(card);
    expect(second).not.toBe(first);
    card.reset();
    const r = await installTap(card.transceive, resumeOptions().opts);
    expect(r.installed).toEqual({ok: true});
    expect(r.resumedFrom).toBe(second);
    expect(card.applet!.pin).toBe(PIN);
    expect(card.failedAuths).toBe(0);
  });

  test('a PIN set and its answer lost, with the PIN this phone sent: one VERIFY, and the card is finished', async () => {
    const card = new FakeGpCard({scp, keys: ownKeys()}).withLiftedApplet(8, '135792');
    const keys = card.applet!.slots.map(s => toHex(s!));
    const {opts} = resumeOptions({attemptedPin: chip => (chip === J3R180_CHIP ? '135792' : null)});
    const r = await installTap(card.transceive, opts);
    expect(r.installed).toEqual({ok: true});
    expect(r.finishedWithAttemptedPin).toBe(true);
    expect(r.setup).toEqual(expect.objectContaining({ok: true, generated: []}));
    expect(card.applet!.slots.map(s => toHex(s!))).toEqual(keys);
    expect(pinTries(card)).toBe(1);
    expect(card.applet!.tries).toBe(3);
    expect(card.ins()).not.toContain(GEN_KEY);
  });

  test('a PIN set and its answer lost, without that PIN: needsReset, no PIN tried, ops’ keys not even asked', async () => {
    const card = new FakeGpCard({scp, keys: ownKeys()}).withLiftedApplet(8, '135792');
    const {opts, managerKeys} = resumeOptions();
    const r = await installTap(card.transceive, opts);
    expect(r.installed).toEqual(expect.objectContaining({ok: false, kind: 'needsReset'}));
    expect((r.installed as {reason: string}).reason).toContain('Settings → Reset a card');
    expect(pinTries(card)).toBe(0);
    expect(card.applet!.tries).toBe(3);
    expect(managerKeys).not.toHaveBeenCalled();
    expect(card.ins()).not.toContain(INIT_UPDATE);
    expect(r.resumable).toBeUndefined();
  });

  test('a remembered PIN the card refuses: one try spent, never a second, and needsReset', async () => {
    const card = new FakeGpCard({scp, keys: ownKeys()}).withLiftedApplet(8, '135792');
    const {opts} = resumeOptions({attemptedPin: () => '246801'});
    const r = await installTap(card.transceive, opts);
    expect(r.installed).toEqual(expect.objectContaining({ok: false, kind: 'needsReset', phase: 'verifyPin'}));
    expect(pinTries(card)).toBe(1);
    expect(card.applet!.tries).toBe(2);
  });

  test('a finished card ops knows is guarded: no PIN tried, no keys asked, nothing written', async () => {
    const card = new FakeGpCard({scp, keys: ownKeys()}).withFinishedApplet();
    const {opts, managerKeys} = resumeOptions({}, {known: true, resumable: false, reasons: ['ops records card x with its PIN set']});
    const r = await installTap(card.transceive, opts);
    expect(r.installed).toEqual(expect.objectContaining({ok: false, kind: 'guarded'}));
    expect(managerKeys).not.toHaveBeenCalled();
    expect(pinTries(card)).toBe(0);
    expect(card.ins()).not.toContain(INIT_UPDATE);
  });

  test('a card ops holds something for, or ops unreachable, is never re-keyed', async () => {
    const card = new FakeGpCard({scp, keys: ownKeys()}).withLiftedApplet(3);
    const before = card.applet!.slots.map(s => (s ? toHex(s) : null));
    const held = await installTap(
      card.transceive,
      resumeOptions({}, {known: true, resumable: false, reasons: ['1 live note, 1000 msat']}).opts,
    );
    expect(held.installed).toEqual(expect.objectContaining({ok: false, kind: 'guarded'}));
    expect((held.installed as {reason: string}).reason).toContain('1 live note');
    card.reset();
    const away = await installTap(
      card.transceive,
      resumeOptions({
        resumeCheck: async () => {
          throw new Error('the registry is unreachable');
        },
      }).opts,
    );
    expect(away.installed).toEqual(expect.objectContaining({ok: false, kind: 'noManagerKeys'}));
    expect(away.resumable).toBe(true);
    expect(card.ins()).not.toContain(GEN_KEY);
    expect(card.ins()).not.toContain(INIT_UPDATE);
    expect(card.applet!.slots.map(s => (s ? toHex(s) : null))).toEqual(before);
  });

  test('a keyed card not on the keys ops holds for it: one INITIALIZE UPDATE, no login, not re-keyed', async () => {
    const card = new FakeGpCard({scp}).withLiftedApplet(4); // still on the factory keys
    const before = card.applet!.slots.map(s => (s ? toHex(s) : null));
    const r = await installTap(card.transceive, resumeOptions().opts);
    expect(r.installed).toEqual(expect.objectContaining({ok: false, kind: 'wrongKeys', reason: NOT_OWN}));
    expect(card.ins().filter(i => i === INIT_UPDATE)).toHaveLength(1);
    expect(card.ins()).not.toContain(EXT_AUTH);
    expect(card.failedAuths).toBe(0);
    expect(card.applet!.slots.map(s => (s ? toHex(s) : null))).toEqual(before);
  });
});

test('the batch keeps its place for a card the next tap can finish, and only for one', () => {
  const lost = (phase: CardPhase, resuming = false): SeenTap => ({lost: true, phase, resuming});
  const result = (r: Partial<TapResult>): SeenTap => ({
    lost: false,
    result: {installed: {ok: true}, setup: null, ...r},
  });
  expect(holdsPlace(lost('keySetup'))).toBe(true);
  expect(holdsPlace(lost('setPin'))).toBe(true);
  expect(holdsPlace(lost('rotate', true))).toBe(true);
  expect(holdsPlace(lost('load'))).toBe(false);
  expect(holdsPlace(lost('rotate'))).toBe(false);
  expect(holdsPlace(result({installed: {ok: false, kind: 'noManagerKeys', phase: 'managerKeys'}, resumable: true}))).toBe(true);
  expect(holdsPlace(result({installed: {ok: false, kind: 'noManagerKeys', phase: 'managerKeys'}}))).toBe(false);
  expect(holdsPlace(result({installed: {ok: false, kind: 'guarded', phase: 'probe'}}))).toBe(false);
  expect(holdsPlace(result({installed: {ok: false, kind: 'needsReset', phase: 'verifyPin'}}))).toBe(false);
  expect(holdsPlace(result({setup: {ok: false, step: 'keys', sw: 0x6f00, reason: 'x'}}))).toBe(true);
  expect(holdsPlace(result({setup: {ok: false, step: 'verifyPin', sw: 0x6f00, reason: 'x'}}))).toBe(false);
  expect(holdsPlace(result({setup: {ok: true, keys: [], generated: [], slotCount: 8}}))).toBe(false);
});

/* ------------------------------------------------------------------------- */
/* A J3R452 as it ships: SCP03 at key set version FF, and first contact      */
/* ------------------------------------------------------------------------- */

describe('a J3R452 as it ships, at key set version FF', () => {
  /** On the public default, unless given the seller's own keys. */
  const shipped = (keys?: KeySet) =>
    new FakeGpCard({scp: 3, kvn: 0xff, cplc: J3R452_CPLC, ...(keys ? {keys} : {})});
  /** GET DATA E0 with no secure channel: first contact's read of the key information. */
  const KEY_INFORMATION = [0x80, 0xca, 0x00, 0xe0, 0x00];

  test('on the public default: installed, moved to version 01 under its own keys by first contact, keyed and given its PIN in one tap', async () => {
    const card = shipped();
    const {opts, managerKeys} = options();
    const r = await installTap(card.transceive, opts);
    expect(r.installed).toEqual({ok: true});
    expect(r.setup).toEqual(
      expect.objectContaining({ok: true, generated: [0, 1, 2, 3, 4, 5, 6, 7]}),
    );
    expect(r.chip).toBe(J3R452_CHIP);
    expect(r.rotated).toBe(true);
    expect(managerKeys).toHaveBeenCalledWith(J3R452_CHIP);
    expect(card.kvn).toBe(0x01);
    expect(card.sameKeys(ownKeys())).toBe(true);
    expect(card.putKeysTaken).toBe(1);
    expect(card.failedAuths).toBe(0);
    expect(card.applet?.pin).toBe('482916');
    // PUT KEY with P1 00 and new version 01, three AES entries (Lc 4E with the
    // MAC); then SELECT, GET DATA E0 and a login under the new set, before GEN_KEY
    const put = card.ins().indexOf(PUT_KEY);
    expect(card.log[put].slice(0, 6)).toEqual([0x84, 0xd8, 0x00, 0x81, 0x4e, 0x01]);
    expect(card.ins().slice(put, put + 5)).toEqual([PUT_KEY, 0xa4, 0xca, INIT_UPDATE, EXT_AUTH]);
    expect(card.log[put + 2]).toEqual(KEY_INFORMATION);
    expect(put).toBeLessThan(card.ins().indexOf(0x10));
  });

  test('a tap lifted at any APDU of first contact, before or after the card acted, is finished by the next: one PUT KEY taken, no failed login', async () => {
    // the rotation on the wire: SELECT, INITIALIZE UPDATE, EXTERNAL AUTHENTICATE,
    // PUT KEY, then SELECT, GET DATA E0, INITIALIZE UPDATE, EXTERNAL AUTHENTICATE
    const clean = shipped();
    await installTap(clean.transceive, options().opts);
    const from = clean.ins().indexOf(PUT_KEY) - 3;
    expect(clean.ins().slice(from, from + 8)).toEqual([
      0xa4, INIT_UPDATE, EXT_AUTH, PUT_KEY, 0xa4, 0xca, INIT_UPDATE, EXT_AUTH,
    ]);
    for (let n = from; n < from + 8; n++) {
      for (const when of ['before', 'after'] as const) {
        const label = `${when} APDU ${n - from} of first contact`;
        const card = shipped();
        card.pull((_a, i) => i === n, when);
        const r1 = await installTap(card.transceive, options().opts);
        expect([label, r1.installed]).toEqual([
          label,
          expect.objectContaining({ok: false, kind: 'pulled', phase: 'rotate'}),
        ]);
        card.reset();
        const r2 = await installTap(card.transceive, options().opts);
        expect([label, r2.installed]).toEqual([label, {ok: true}]);
        expect([label, r2.setup?.ok]).toEqual([label, true]);
        expect([label, card.kvn]).toEqual([label, 0x01]);
        expect([label, card.sameKeys(ownKeys())]).toEqual([label, true]);
        expect([label, card.putKeysTaken]).toEqual([label, 1]);
        expect([label, card.failedAuths]).toEqual([label, 0]);
      }
    }
  });

  test('on the seller’s own keys: one INITIALIZE UPDATE, no login, wrongKeys, and the card as it came', async () => {
    const seller = {
      enc: fromHex('a5'.repeat(16)),
      mac: fromHex('5a'.repeat(16)),
      dek: fromHex('c3'.repeat(16)),
    };
    const card = shipped(seller);
    const {opts, managerKeys} = options();
    const r = await installTap(card.transceive, opts);
    expect(r.installed).toEqual(
      expect.objectContaining({ok: false, kind: 'wrongKeys', reason: NEITHER_SET}),
    );
    expect(r.setup).toBeNull();
    // ops was asked for the chip's keys before the card proved anything: the
    // cost of a card no batch should hold until the seller's keys can be imported
    expect(managerKeys).toHaveBeenCalledWith(J3R452_CHIP);
    expect(card.ins().filter(i => i === INIT_UPDATE)).toHaveLength(1);
    expect(card.ins()).not.toContain(EXT_AUTH);
    expect(card.failedAuths).toBe(0);
    expect(card.kvn).toBe(0xff);
    expect(card.sameKeys(seller)).toBe(true);
    expect(card.instances.size).toBe(0);
  });
});
