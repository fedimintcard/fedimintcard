import {fromHex, toHex} from './cardkit';
import {limitApdu, DEFAULT_DAILY_LIMIT_SAT, type OfflineSetup} from './cardSetup';

const VILLAGE_LIMIT_SAT = 10_000;

test('a batch has no daily limit unless one is set', () => {
  expect(DEFAULT_DAILY_LIMIT_SAT).toBe(0);
});
import {LOOP, registryHeldWhat} from './strings';
import {holdsPlace, installTap, type TapOptions} from './installTap';
import {shortIdOf} from './store';
import {FakeGpCard, J3R180_CHIP} from './testing/fakeGpCard';
import type {ManagerKeysReply} from './ops';

/**
 * Applet 3.0's offline steps in the install tap (docs/OFFLINE-PROTOCOL.md § 5):
 * after the keys and before the PIN, SET_LIMIT, ATTEST_INIT, the registry's
 * certificate, SET_CERT; and each of them lifted, then finished by the next tap.
 */

const OWN: ManagerKeysReply = {
  chip: J3R180_CHIP,
  key_version: 1,
  enc: '0f1e2d3c4b5a69788796a5b4c3d2e1f0',
  mac: '1f2e3d4c5b6a79889706b5c4d3e2f101',
  dek: '2f3e4d5c6b7a8998a716c5d4e3f20112',
};
const ownKeys = () => ({enc: fromHex(OWN.enc), mac: fromHex(OWN.mac), dek: fromHex(OWN.dek)});
const PIN = '482916';
const GET_STATE = 0x16;
const SET_LIMIT = 0x18;
const ATTEST_INIT = 0x30;
const SET_CERT = 0x31;
const SET_REGISTRY_KEY = 0x34;
const SET_PIN = 0x41;
const GEN_KEY = 0x10;

/** The registry's certificate for a key, in the protocol's shape: 01 ‖ slot 0 ‖ compressed attest ‖ r‖s. */
const certFor = (attestPub: readonly number[], slot0: readonly number[]) => [
  0x01,
  ...slot0,
  0x02 + (attestPub[64] & 1),
  ...attestPub.slice(1, 33),
  ...new Array(64).fill(0xab),
];

/** The registry's certificate key: 04 and a 64-byte body. */
const REGISTRY_KEY = [0x04, ...Array.from({length: 64}, (_, i) => (i * 5 + 3) & 0xff)];

function setup(over: Partial<OfflineSetup> = {}) {
  const certify = jest.fn(async (attestPub: readonly number[], slot0: readonly number[]) =>
    certFor(attestPub, slot0),
  );
  // a village batch's limit; the default is none (DEFAULT_DAILY_LIMIT_SAT, checked below)
  const offline: OfflineSetup = {limitMsat: VILLAGE_LIMIT_SAT * 1000, certify, registryKey: REGISTRY_KEY, ...over};
  return {certify, offline};
}

function tapOptions(offline: OfflineSetup, over: Partial<TapOptions> = {}): TapOptions {
  let t = 0;
  return {
    pin: PIN,
    managerKeys: async chip => ({...OWN, chip}),
    resumeCheck: async (chip, shortId) => ({short_id: shortId, chip, known: false, resumable: true, reasons: []}),
    random: n => Array.from({length: n}, (_, i) => (i * 37 + 11) & 0xff),
    now: () => (t += 5),
    offline,
    ...over,
  };
}

const b0 = (card: FakeGpCard) => card.log.filter(a => a[0] === 0xb0).map(a => a[1]);
const slot0Id = (card: FakeGpCard) => shortIdOf(card.applet!.slots[0]!);

test('SET_LIMIT is 8 bytes of msat, big-endian', () => {
  expect(limitApdu(10_000_000)).toEqual([0xb0, 0x18, 0, 0, 8, 0, 0, 0, 0, 0, 0x98, 0x96, 0x80]);
  expect(limitApdu(0)).toEqual([0xb0, 0x18, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0]);
  expect(limitApdu(2 ** 40 + 5).slice(5)).toEqual([0, 0, 1, 0, 0, 0, 0, 5]);
  expect(() => limitApdu(-1)).toThrow();
  expect(() => limitApdu(1.5)).toThrow();
});

test('a blank card: keys, then limit, attestation, certificate, and only then the PIN, in the one tap', async () => {
  const card = new FakeGpCard();
  const {offline, certify} = setup();
  const steps: string[] = [];
  const r = await installTap(card.transceive, tapOptions(offline, {onSetupStep: s => steps.push(s)}));
  expect(r.installed).toEqual({ok: true});
  expect(r.setup?.ok).toBe(true);
  expect(steps).toEqual(['select', 'keys', 'attest', 'setPin', 'verifyPin']);
  const a = card.applet!;
  expect(a.limit).toEqual([0, 0, 0, 0, 0, 0x98, 0x96, 0x80]);
  expect(a.attest).toHaveLength(65);
  expect(a.cert).toHaveLength(130);
  expect(a.pin).toBe(PIN);
  // the registry was asked for this card's attestation key and slot 0
  expect(certify).toHaveBeenCalledTimes(1);
  expect(certify).toHaveBeenCalledWith(a.attest, a.slots[0]);
  expect(a.cert).toEqual(certFor(a.attest!, a.slots[0]!));
  // the order on the wire
  const ins = b0(card);
  const at = (i: number) => ins.indexOf(i);
  expect(ins.lastIndexOf(GEN_KEY)).toBeLessThan(at(SET_LIMIT));
  expect(at(SET_LIMIT)).toBeLessThan(at(ATTEST_INIT));
  expect(at(ATTEST_INIT)).toBeLessThan(at(SET_CERT));
  expect(at(SET_CERT)).toBeLessThan(at(SET_REGISTRY_KEY));
  expect(at(SET_REGISTRY_KEY)).toBeLessThan(at(SET_PIN));
  expect(a.registryKey).toEqual(REGISTRY_KEY);
});

test('no limit is 0 msat', async () => {
  const card = new FakeGpCard();
  const r = await installTap(card.transceive, tapOptions(setup({limitMsat: 0}).offline));
  expect(r.setup?.ok).toBe(true);
  expect(card.applet!.limit).toEqual(new Array(8).fill(0));
});

test('a build with no offline setup, and a 2.0 card, get none of the new commands', async () => {
  const plain = new FakeGpCard();
  await installTap(plain.transceive, tapOptions(setup().offline, {offline: undefined}));
  expect(b0(plain)).not.toContain(SET_LIMIT);
  expect(b0(plain)).not.toContain(ATTEST_INIT);
  expect(plain.applet!.cert ?? null).toBeNull();

  // a lifted 8-slot applet answers SELECT 02 00: resumed and PINned the old way
  const old = new FakeGpCard({keys: ownKeys()}).withLiftedApplet(8);
  const {offline, certify} = setup();
  const r = await installTap(old.transceive, tapOptions(offline));
  expect(r.setup?.ok).toBe(true);
  expect(certify).not.toHaveBeenCalled();
  expect(b0(old)).not.toContain(SET_LIMIT);
  expect(b0(old)).not.toContain(GET_STATE);
  expect(old.applet!.pin).toBe(PIN);
});

describe('a tap lifted at each new APDU is finished by the next', () => {
  const cases: [string, number, 'before' | 'after'][] = [
    ['SET_LIMIT, before it acted', SET_LIMIT, 'before'],
    ['SET_LIMIT, answer lost', SET_LIMIT, 'after'],
    ['ATTEST_INIT, before it acted', ATTEST_INIT, 'before'],
    ['ATTEST_INIT, answer lost', ATTEST_INIT, 'after'],
    ['SET_CERT, before it acted', SET_CERT, 'before'],
    ['SET_CERT, answer lost', SET_CERT, 'after'],
  ];
  test.each(cases)('%s', async (_label, ins, when) => {
    const card = new FakeGpCard();
    card.pull(a => a[0] === 0xb0 && a[1] === ins, when);
    const first = setup();
    await expect(installTap(card.transceive, tapOptions(first.offline))).rejects.toThrow('Tag was lost');
    // keyed, no PIN yet
    expect(card.applet!.slots[0]).not.toBeNull();
    expect(card.applet!.pin).toBeNull();
    const certAfterLift = card.applet!.cert ?? null;
    const idAfterLift = slot0Id(card);

    card.reset();
    const second = setup();
    const r = await installTap(card.transceive, tapOptions(second.offline));
    expect(r.installed).toEqual({ok: true});
    expect(r.setup?.ok).toBe(true);
    const a = card.applet!;
    expect(a.pin).toBe(PIN);
    expect(a.limit).toEqual([0, 0, 0, 0, 0, 0x98, 0x96, 0x80]);
    // whatever the card holds is a certificate over the keys it ends with
    expect(a.cert).toEqual(certFor(a.attest!, a.slots[0]!));
    if (certAfterLift) {
      // SET_CERT had taken: slot 0 is kept (the certificate is over it), nothing asked again
      expect(slot0Id(card)).toBe(idAfterLift);
      expect(second.certify).not.toHaveBeenCalled();
      expect(r.resumedFrom).toBe(idAfterLift);
    } else {
      expect(second.certify).toHaveBeenCalledTimes(1);
    }
    expect(card.failedAuths).toBe(0);
  });
});

test('a lift after SET_CERT does not re-key: the certificate stays over slot 0', async () => {
  const card = new FakeGpCard();
  card.pull(a => a[0] === 0xb0 && a[1] === SET_PIN, 'before');
  await expect(installTap(card.transceive, tapOptions(setup().offline))).rejects.toThrow('Tag was lost');
  const keys = card.applet!.slots.map(s => (s ? toHex(s) : null));
  const cert = card.applet!.cert;
  card.reset();
  const second = setup();
  const r = await installTap(card.transceive, tapOptions(second.offline));
  expect(r.setup).toEqual(expect.objectContaining({ok: true, generated: []}));
  expect(card.applet!.slots.map(s => (s ? toHex(s) : null))).toEqual(keys);
  expect(card.applet!.cert).toEqual(cert);
  expect(b0(card).filter(i => i === ATTEST_INIT)).toHaveLength(1);
  expect(card.applet!.pin).toBe(PIN);
});

test('ATTEST_INIT may run again before SET_CERT: the registry certifies the key the card holds now', async () => {
  const card = new FakeGpCard();
  const keys: (readonly number[])[] = [];
  const failing = setup({
    certify: async attestPub => {
      keys.push(attestPub);
      throw new Error('the registry is away');
    },
  });
  const r1 = await installTap(card.transceive, tapOptions(failing.offline));
  expect(r1.setup).toEqual(
    expect.objectContaining({ok: false, step: 'attest', reason: expect.stringMatching(/registry is away/)}),
  );
  // the batch keeps its place; no PIN was sent
  expect(holdsPlace({lost: false, result: r1})).toBe(true);
  expect(card.applet!.pin).toBeNull();
  expect(card.applet!.cert ?? null).toBeNull();

  card.reset();
  const ok = setup({
    certify: async (attestPub, slot0) => {
      keys.push(attestPub);
      return certFor(attestPub, slot0);
    },
  });
  const r2 = await installTap(card.transceive, tapOptions(ok.offline));
  expect(r2.setup?.ok).toBe(true);
  expect(keys).toHaveLength(2);
  expect(keys[1]).toEqual(card.applet!.attest);
  expect(keys[1]).not.toEqual(keys[0]);
  expect(card.applet!.pin).toBe(PIN);
});

test('a registry answer the card cannot hold, or a card refusing the certificate, stops at the attest step', async () => {
  const card = new FakeGpCard();
  const long = setup({certify: async () => new Array(161).fill(1)});
  const r = await installTap(card.transceive, tapOptions(long.offline));
  expect(r.setup).toEqual(expect.objectContaining({ok: false, step: 'attest'}));
  expect(card.applet!.pin).toBeNull();
  expect(b0(card)).not.toContain(SET_CERT);
});

test('a certificate for another key is not taken for this card’s', async () => {
  const other = new FakeGpCard();
  // lift after SET_CERT, then swap the certificate's slot 0 for someone else's
  other.pull(a => a[0] === 0xb0 && a[1] === SET_PIN, 'before');
  await expect(installTap(other.transceive, tapOptions(setup().offline))).rejects.toThrow('Tag was lost');
  other.applet!.cert = [1, ...new Array(32).fill(9), ...other.applet!.cert!.slice(33)];
  other.reset();
  const r = await installTap(other.transceive, tapOptions(setup().offline));
  expect(r.setup).toEqual(expect.objectContaining({ok: false, step: 'attest'}));
  expect(r.setup && !r.setup.ok && r.setup.reason).toMatch(/another key/);
  expect(other.applet!.pin).toBeNull();
});

describe('SET_REGISTRY_KEY', () => {
  test('is sent once, as CLA B0 INS 34 with the 65-byte key, after SET_CERT and before SET_PIN', async () => {
    const card = new FakeGpCard();
    await installTap(card.transceive, tapOptions(setup().offline));
    expect(card.log.filter(a => a[0] === 0xb0 && a[1] === SET_REGISTRY_KEY)).toEqual([
      [0xb0, 0x34, 0, 0, 65, ...REGISTRY_KEY],
    ]);
  });

  test.each([['before'], ['after']] as const)('a lift %s it is finished by the next tap', async when => {
    const card = new FakeGpCard();
    card.pull(a => a[0] === 0xb0 && a[1] === SET_REGISTRY_KEY, when);
    await expect(installTap(card.transceive, tapOptions(setup().offline))).rejects.toThrow('Tag was lost');
    expect(card.applet!.pin).toBeNull();
    const keyed = card.applet!.registryKey ?? null;
    expect(keyed === null).toBe(when === 'before');
    const idAfterLift = slot0Id(card);

    card.reset();
    const second = setup();
    const r = await installTap(card.transceive, tapOptions(second.offline));
    expect(r.setup?.ok).toBe(true);
    expect(card.applet!.registryKey).toEqual(REGISTRY_KEY);
    expect(card.applet!.pin).toBe(PIN);
    // the certificate was already taken: not asked for again, and slot 0 is kept
    expect(second.certify).not.toHaveBeenCalled();
    expect(slot0Id(card)).toBe(idAfterLift);
    // the lifted APDU is in the log too; a key the card already holds is not sent again
    expect(b0(card).filter(i => i === SET_REGISTRY_KEY)).toHaveLength(when === 'before' ? 2 : 1);
  });

  test('a card that already holds the key is not sent it again', async () => {
    const card = new FakeGpCard();
    card.pull(a => a[0] === 0xb0 && a[1] === SET_PIN, 'before');
    await expect(installTap(card.transceive, tapOptions(setup().offline))).rejects.toThrow('Tag was lost');
    expect(card.applet!.registryKey).toEqual(REGISTRY_KEY);
    card.reset();
    const before = b0(card).filter(i => i === SET_REGISTRY_KEY).length;
    const r = await installTap(card.transceive, tapOptions(setup().offline));
    expect(r.setup?.ok).toBe(true);
    expect(b0(card).filter(i => i === SET_REGISTRY_KEY)).toHaveLength(before);
  });

  test('an older 3.0 card (58-byte GET_STATE) skips it and is finished', async () => {
    const card = new FakeGpCard();
    card.stateLength = 58;
    const r = await installTap(card.transceive, tapOptions(setup().offline));
    expect(r.setup?.ok).toBe(true);
    expect(b0(card)).not.toContain(SET_REGISTRY_KEY);
    expect(card.applet!.cert).not.toBeNull();
    expect(card.applet!.pin).toBe(PIN);
  });

  test('a key that is not 04 and 64 bytes is never sent to the card', async () => {
    const card = new FakeGpCard();
    const bad = setup({registryKey: [0x02, ...new Array(64).fill(1)]});
    const r = await installTap(card.transceive, tapOptions(bad.offline));
    expect(r.setup).toEqual(expect.objectContaining({ok: false, step: 'attest', registry: true}));
    expect(b0(card)).not.toContain(SET_REGISTRY_KEY);
    expect(card.applet!.pin).toBeNull();
    expect(holdsPlace({lost: false, result: r})).toBe(true);
  });
});

describe('a registry failure is not reported as a lifted card', () => {
  test('a refusal of the certificate carries the registry flag and its own reason', async () => {
    const card = new FakeGpCard();
    const refusing = setup({
      certify: async () => {
        throw new Error('the card has live notes under another key');
      },
    });
    const r = await installTap(card.transceive, tapOptions(refusing.offline));
    expect(r.setup).toEqual(
      expect.objectContaining({
        ok: false,
        step: 'attest',
        registry: true,
        reason: 'the card has live notes under another key',
      }),
    );
    const held = r.setup && !r.setup.ok ? {reason: r.setup.reason, registry: r.setup.registry === true} : {};
    expect(registryHeldWhat(held)).toBe(
      "The registry refused the card's certificate: the card has live notes under another key",
    );
  });

  test('a card-side failure (and a really lifted tap) keeps the lifted-card text', () => {
    expect(registryHeldWhat({})).toBeNull();
    expect(registryHeldWhat({reason: 'SET_LIMIT answered 6985'})).toBeNull();
    expect(LOOP.heldWhat).toMatch(/left the phone/);
  });
});
