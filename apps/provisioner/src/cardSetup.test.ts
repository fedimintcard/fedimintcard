import {CARD_AID} from './cardkit';
import {FakeGpCard} from './testing/fakeGpCard';
import {
  finishCard,
  finishVerified,
  INSTALL_TAP_KEY_SLOTS,
  pinApdu,
  keyApdu,
  probeApplet,
  probeCard,
  probeSlotCount,
  selectApdu,
} from './cardSetup';
import {shortIdOf} from './store';

/**
 * The applet's key and PIN commands as FedimintCardApplet.java answers them:
 * GET_PUB 6985 on an empty slot, GEN_KEY P2=00 refused on a used slot,
 * SET_PIN once (6985 after), VERIFY_PIN 63CX on a wrong PIN, and 6B00 for a
 * slot past the last (`count`, 8 unless told).
 */
function applet(
  opts: {preset?: Record<number, number[]>; pinSet?: boolean; failGen?: number; count?: number; probeAnswer?: number} = {},
) {
  const count = opts.count ?? 8;
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
    if ((ins === 0x10 || ins === 0x11) && p1 >= count) {
      return sw(opts.probeAnswer ?? 0x6b00);
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

test('a fresh 8-slot applet gets eight keys and the PIN, verified', async () => {
  const card = applet();
  const steps: string[] = [];
  const r = await finishCard(card.tx, '482916', s => steps.push(s));
  expect(r.ok).toBe(true);
  if (!r.ok) {
    return;
  }
  expect(r.keys).toHaveLength(8);
  expect(r.slotCount).toBe(8);
  expect(r.generated).toEqual([0, 1, 2, 3, 4, 5, 6, 7]);
  // The count came from the card: one GET_PUB past slot 7, and no APDU for a slot past it after.
  expect(card.log.filter(a => a[0] === 0xb0 && a[2] >= 8)).toEqual([[0xb0, 0x11, 8, 0x00, 0x00]]);
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

const range = (n: number, from = 0) => Array.from({length: n - from}, (_, i) => from + i);

test('probeSlotCount reads 8 or 32 from where the card first answers 6B00, writing nothing', async () => {
  /** The applet, selected, as probeSlotCount finds it inside finishCard. */
  const selected = async (opts: Parameters<typeof applet>[0]) => {
    const card = applet(opts);
    await card.tx(selectApdu());
    return card;
  };
  expect(await probeSlotCount((await selected({count: 8})).tx)).toEqual({ok: true, count: 8});
  const card = await selected({count: 32});
  expect(await probeSlotCount(card.tx)).toEqual({ok: true, count: 32});
  expect(card.log.slice(1).map(a => [a[1], a[2]])).toEqual([[0x11, 8], [0x11, 32]]);
  expect(card.slots.size).toBe(0);
  // A count this app does not know (64 slots), or an answer that is no slot's, is not guessed at.
  expect(await probeSlotCount((await selected({count: 64})).tx)).toMatchObject({
    ok: false,
    reason: expect.stringContaining('more than this app knows'),
  });
  expect(await probeSlotCount((await selected({count: 8, probeAnswer: 0x6a80})).tx)).toMatchObject({ok: false, sw: 0x6a80});
});

test('split keying: a fresh 32-slot applet gets slots 0-7 keyed before the PIN, and 8-31 left empty', async () => {
  expect(INSTALL_TAP_KEY_SLOTS).toBe(8);
  const card = applet({count: 32});
  const r = await finishCard(card.tx, '482916');
  expect(r.ok).toBe(true);
  if (!r.ok) {
    return;
  }
  expect(r.slotCount).toBe(32);
  expect(r.keys).toHaveLength(8);
  expect(r.generated).toEqual(range(8));
  expect(r.keys[7]).toEqual(card.slots.get(7));
  expect([...card.slots.keys()].sort((a, b) => a - b)).toEqual(range(8));
  // Nothing is generated past slot 7; slot 8 and 32 are touched only by the count probe.
  expect(card.log.some(a => a[1] === 0x10 && a[2] >= 8)).toBe(false);
  const setPin = card.log.findIndex(a => a[1] === 0x41);
  expect(setPin).toBeGreaterThan(card.log.findIndex(a => a[1] === 0x10 && a[2] === 7));
  expect(card.pin()).toBe('482916');
});

test('an 8-slot card is keyed exactly as before split keying: all 8 slots, then the PIN', async () => {
  const card = applet({count: 8});
  const r = await finishCard(card.tx, '482916');
  expect(r).toEqual(expect.objectContaining({ok: true, slotCount: 8, generated: range(8)}));
  expect(r.ok && r.keys).toHaveLength(8);
  // SELECT, the probe (GET_PUB 8), GET_PUB + GEN_KEY per slot, SET_PIN, VERIFY_PIN.
  expect(card.log).toHaveLength(1 + 1 + 16 + 2);
});

test('an 8-slot card that was partly keyed keeps its keys and gets the rest', async () => {
  const kept = Array.from({length: 32}, () => 0xcd);
  const card = applet({count: 8, preset: {0: kept, 1: kept}});
  const r = await finishCard(card.tx, '482916');
  expect(r.ok && r.slotCount).toBe(8);
  expect(r.ok && r.generated).toEqual(range(8, 2));
});

test('the install-tap seam: keySlots 32 still keys every slot of a 32-slot card', async () => {
  const card = applet({count: 32});
  const r = await finishCard(card.tx, '482916', undefined, {keySlots: 32});
  expect(r).toEqual(expect.objectContaining({ok: true, slotCount: 32, generated: range(32)}));
  expect(r.ok && r.keys).toHaveLength(32);
  expect(card.pin()).toBe('482916');
});

test('a card whose slot count is unknown stops at keys, nothing generated and no PIN', async () => {
  const card = applet({count: 64});
  const r = await finishCard(card.tx, '482916');
  expect(r).toMatchObject({ok: false, step: 'keys'});
  expect(card.log.some(a => a[1] === 0x10)).toBe(false);
  expect(card.pin()).toBeNull();
});

test('a re-key of a 32-slot card covers the install tap’s slots, 0-7', async () => {
  const card = new FakeGpCard({appletSlots: 32}).withLiftedApplet(8);
  const before = card.applet!.slots.slice(0, 8).map(s => s!.join(','));
  // A lifted card on its own keys: SELECT the applet and finish it as the resume does.
  const r = await finishCard(card.transceive, '135790', undefined, {rekey: true});
  expect(r).toEqual(expect.objectContaining({ok: true, slotCount: 32, generated: range(8)}));
  expect(card.applet!.slots.slice(0, 8).every((s, i) => s!.join(',') !== before[i])).toBe(true);
  expect(card.applet!.slots.slice(8).every(s => s === null)).toBe(true);
});

test('finishVerified reads back slots 0-7 of a finished 32-slot card, generating none', async () => {
  const card = new FakeGpCard({appletSlots: 32}).withFinishedApplet('2468');
  const keys = card.applet!.slots.slice(0, 8).map(s => s!);
  const r = await finishVerified(card.transceive, '2468');
  expect(r).toEqual(expect.objectContaining({ok: true, slotCount: 32, generated: []}));
  expect(r.ok && r.keys).toEqual(keys);
  expect(card.applet!.slots.slice(8).every(s => s === null)).toBe(true);
});
