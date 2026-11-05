import {FACTORY_KEYS, fromHex, toHex} from './cardkit';
import {installTap, NEITHER_SET, type TapOptions} from './installTap';
import type {ManagerKeysReply} from './ops';
import {FakeGpCard, J3R180_CHIP} from './testing/fakeGpCard';

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

test('a blank card: installed on the factory keys, rotated to its own, keyed and PINned in one tap', async () => {
  const card = new FakeGpCard();
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

test('a tap broken after PUT KEY: the next tap finds the card on its own keys and finishes it', async () => {
  const card = new FakeGpCard();
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
    const card = new FakeGpCard();
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
  const card = new FakeGpCard({keys: stranger});
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
  const card = new FakeGpCard();
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
  const card = new FakeGpCard({keys: ownKeys()});
  const r = await installTap(card.transceive, options().opts);
  expect(r.installed).toEqual({ok: true});
  expect(r.rotated).toBe(false);
  expect(card.ins()).not.toContain(PUT_KEY);
  expect(card.ins().filter(i => i === INIT_UPDATE)).toHaveLength(1);
});

test('a keyed card is never installed over, and its chip is not even read', async () => {
  const card = new FakeGpCard().withFinishedApplet();
  const r = await installTap(card.transceive, options().opts);
  expect(r.installed).toEqual(
    expect.objectContaining({ok: false, kind: 'guarded'}),
  );
  expect(card.ins()).not.toContain(0xca);
});

test('ops’ keys are checked: a reply for another chip is no answer', async () => {
  const card = new FakeGpCard();
  const {opts} = options({
    managerKeys: async () => ({...OWN, chip: 'cplc-v1:' + '0'.repeat(24)}),
  });
  const r = await installTap(card.transceive, opts);
  expect(r.installed).toEqual(expect.objectContaining({kind: 'noManagerKeys'}));
  expect(toHex(card.keys.enc)).toBe(toHex(FACTORY_KEYS.enc));
});
