import {CARD_AID} from './cardkit';
import {FakeGpCard} from './testing/fakeGpCard';
import {finishCard, pinApdu, keyApdu, probeApplet, probeCard, selectApdu, SLOT_COUNT} from './cardSetup';
import {shortIdOf} from './store';

/**
 * The applet's key and PIN commands as FedimintCardApplet.java answers them:
 * GET_PUB 6985 on an empty slot, GEN_KEY P2=00 refused on a used slot,
 * SET_PIN once (6985 after), VERIFY_PIN 63CX on a wrong PIN.
 */
function applet(opts: {preset?: Record<number, number[]>; pinSet?: boolean; failGen?: number} = {}) {
  const slots = new Map<number, number[]>(Object.entries(opts.preset ?? {}).map(([k, v]) => [Number(k), v]));
  let pin: string | null = opts.pinSet ? '9999' : null;
  let selected = false;
  const log: number[][] = [];
  const ok = (body: number[] = []) => [...body, 0x90, 0x00];
  const sw = (n: number) => [n >> 8, n & 0xff];
  const tx = async (apdu: number[]) => {
    log.push(apdu);
    const [cla, ins, p1, p2] = apdu;
    if (cla === 0x00 && ins === 0xa4) {
      const aid = apdu.slice(5, 5 + apdu[4]);
      selected = aid.length === CARD_AID.length && aid.every((b, i) => b === CARD_AID[i]);
      return selected ? ok([0x02, 0x00]) : sw(0x6a82);
    }
    if (!selected || cla !== 0xb0) {
      return sw(0x6e00);
    }
    switch (ins) {
      case 0x11:
        return slots.has(p1) ? ok(slots.get(p1)) : sw(0x6985);
      case 0x10: {
        if (p1 === opts.failGen) {
          return sw(0x6a80);
        }
        if (slots.has(p1) && p2 === 0) {
          return sw(0x6985);
        }
        const key = Array.from({length: 32}, (_, i) => (p1 * 31 + i) & 0xff);
        slots.set(p1, key);
        return ok(key);
      }
      case 0x41: {
        if (pin !== null) {
          return sw(0x6985);
        }
        pin = String.fromCharCode(...apdu.slice(5));
        return ok();
      }
      case 0x40:
        return String.fromCharCode(...apdu.slice(5)) === pin ? ok() : sw(0x63c2);
      default:
        return sw(0x6d00);
    }
  };
  return {tx, log, slots, pin: () => pin};
}

test('the APDUs are the host card client\'s', () => {
  expect(selectApdu()).toEqual([0x00, 0xa4, 0x04, 0x00, 8, ...CARD_AID, 0x00]);
  expect(keyApdu(0x10, 3)).toEqual([0xb0, 0x10, 0x03, 0x00, 0x00]);
  expect(pinApdu(0x41, '1234')).toEqual([0xb0, 0x41, 0x00, 0x00, 4, 0x31, 0x32, 0x33, 0x34]);
});

test('a fresh applet gets eight keys and the PIN, verified', async () => {
  const card = applet();
  const steps: string[] = [];
  const r = await finishCard(card.tx, '482916', s => steps.push(s));
  expect(r.ok).toBe(true);
  if (!r.ok) {
    return;
  }
  expect(r.keys).toHaveLength(SLOT_COUNT);
  expect(r.generated).toEqual([0, 1, 2, 3, 4, 5, 6, 7]);
  expect(r.keys[0]).toEqual(card.slots.get(0));
  expect(card.pin()).toBe('482916');
  expect(steps).toEqual(['select', 'keys', 'setPin', 'verifyPin']);
  // The last APDU proves the PIN: VERIFY_PIN with the same digits.
  expect(card.log[card.log.length - 1].slice(0, 2)).toEqual([0xb0, 0x40]);
});

test('keys already on the card are kept, never regenerated', async () => {
  const kept = Array.from({length: 32}, () => 0xab);
  const card = applet({preset: {0: kept}});
  const r = await finishCard(card.tx, '482916');
  expect(r.ok && r.keys[0]).toEqual(kept);
  expect(r.ok && r.generated).toEqual([1, 2, 3, 4, 5, 6, 7]);
  expect(card.log.some(a => a[1] === 0x10 && a[3] === 0x01)).toBe(false);
});

test('a card that already has a PIN stops at setPin', async () => {
  const r = await finishCard(applet({pinSet: true}).tx, '482916');
  expect(r).toMatchObject({ok: false, step: 'setPin', sw: 0x6985, reason: 'the card already has a PIN'});
});

test('a slot that will not key stops at keys, before any PIN is sent', async () => {
  const card = applet({failGen: 5});
  const r = await finishCard(card.tx, '482916');
  expect(r).toMatchObject({ok: false, step: 'keys', sw: 0x6a80});
  expect(card.pin()).toBeNull();
});

test('a card with no applet stops at select', async () => {
  const r = await finishCard(async () => [0x6a, 0x82], '482916');
  expect(r).toMatchObject({ok: false, step: 'select', sw: 0x6a82});
});

test('a PIN the applet would refuse is a caller bug, not a card answer', async () => {
  await expect(finishCard(applet().tx, '12')).rejects.toThrow('4 to 8 digits');
});

test('the short id is the registry\'s: last 4 bytes of the slot-0 key', () => {
  const key = [...Array(28).fill(0), 0x0a, 0xbc, 0xde, 0xf1];
  expect(shortIdOf(key)).toBe('0abcdef1');
  expect(shortIdOf(key)).toMatch(/^[0-9a-f]{8}$/);
});

test('probeApplet tells a blank card, an unkeyed applet and a keyed one apart', async () => {
  expect(await probeApplet(async () => [0x6a, 0x82])).toBe('none');
  expect(await probeApplet(applet().tx)).toBe('unkeyed');
  expect(await probeApplet(applet({preset: {0: Array(32).fill(1)}}).tx)).toBe('keyed');
});

test('a re-key is refused by a card with a PIN, and probeCard reads the PIN state without a try', async () => {
  const card = new FakeGpCard().withFinishedApplet('2468');
  const before = card.applet!.slots.map(s => s!.join(','));
  const probe = await probeCard(card.transceive);
  expect(probe).toEqual(expect.objectContaining({kind: 'keyed', pin: 'set'}));
  const r = await finishCard(card.transceive, '135790', undefined, {rekey: true});
  expect(r).toEqual(expect.objectContaining({ok: false, step: 'keys', sw: 0x6a94}));
  expect(card.applet!.slots.map(s => s!.join(','))).toEqual(before);
  expect(card.applet!.tries).toBe(3);
  expect(await probeCard(new FakeGpCard().withLiftedApplet(2).transceive)).toEqual(
    expect.objectContaining({kind: 'keyed', pin: 'notSet'}),
  );
});
