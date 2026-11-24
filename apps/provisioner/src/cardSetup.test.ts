import {selectApdu as select4, verifyPinHeld} from './cardSetup';
import {CARD_AID} from './cardkit';
import {FakeGpCard} from './testing/fakeGpCard';
import {
  finishCard,
  finishVerified,
  INSTALL_TAP_KEY_SLOTS,
  NEEDS_INTERNET,
  OLD_APPLET,
  parseState,
  waitSeconds,
  pinApdu,
  keyApdu,
  probeApplet,
  probeCard,
  probeSlotCount,
  selectApdu,
} from './cardSetup';
import {shortIdOf} from './store';
import {H900000, NOW_MS, testOffline} from './testing/anchor';

/**
 * Applet 4.0's key, PIN, offline and clock commands as FedimintCardApplet.java
 * answers them: GET_PUB 6985 on an empty slot, GEN_KEY P2=00 refused on a used
 * slot, 6B00 for a slot past the last (`count`, 8 unless told), GET_STATE's 65
 * bytes, SET_HEADER an anchor before the PIN, SET_PIN once (6985 after) and
 * only in a selection that anchored (6A96), VERIFY_PIN 63CX on a wrong PIN.
 * `version` is what SELECT answers; `header` makes SET_HEADER refuse (6A93),
 * not anchor, or report another time.
 */
function applet(
  opts: {
    preset?: Record<number, number[]>;
    pinSet?: boolean;
    failGen?: number;
    count?: number;
    probeAnswer?: number;
    version?: number[];
    header?: 'refuse' | 'noAnchor' | 'otherTime';
  } = {},
) {
  const count = opts.count ?? 8;
  const slots = new Map<number, number[]>(Object.entries(opts.preset ?? {}).map(([k, v]) => [Number(k), v]));
  let pin: string | null = opts.pinSet ? '9999' : null;
  let selected = false;
  let anchored = false;
  let now = 0;
  let registryKey = false;
  const log: number[][] = [];
  const ok = (body: number[] = []) => [...body, 0x90, 0x00];
  const sw = (n: number) => [n >> 8, n & 0xff];
  const be = (t: number) => [(t >>> 24) & 0xff, (t >>> 16) & 0xff, (t >>> 8) & 0xff, t & 0xff];
  const tx = async (apdu: number[]) => {
    log.push(apdu);
    const [cla, ins, p1, p2] = apdu;
    const data = apdu.length > 5 ? apdu.slice(5, 5 + apdu[4]) : [];
    if (cla === 0x00 && ins === 0xa4) {
      const aid = apdu.slice(5, 5 + apdu[4]);
      selected = aid.length === CARD_AID.length && aid.every((b, i) => b === CARD_AID[i]);
      anchored = false;
      return selected ? ok(opts.version ?? [0x04, 0x00]) : sw(0x6a82);
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
      case 0x16: {
        const out: number[] = Array.from({length: 32}, (_, i) => (slots.has(i) ? 1 : 0));
        out.push(...be(now), ...new Array(20).fill(0), 0, 0, registryKey ? 1 : 0);
        out.push(pin === null ? 0 : 1, 0, 0, 0, anchored ? 1 : 0, 0);
        return ok(out);
      }
      case 0x18:
      case 0x31:
        return pin === null ? ok() : sw(0x6985);
      case 0x30:
        return ok([0x04, ...new Array(64).fill(7)]);
      case 0x34:
        registryKey = true;
        return ok();
      case 0x19: {
        if (data.length !== 80 || opts.header === 'refuse') {
          return sw(0x6a93);
        }
        const time = (data[68] | (data[69] << 8) | (data[70] << 16) | (data[71] << 24)) >>> 0;
        if (pin === null && opts.header !== 'noAnchor') {
          now = opts.header === 'otherTime' ? time - 1 : time;
          anchored = true;
        }
        return ok(be(now));
      }
      case 0x41: {
        if (pin !== null) {
          return sw(0x6985);
        }
        if (!anchored) {
          return sw(0x6a96);
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

const offline = testOffline();

test('the APDUs are the host card client\'s', () => {
  expect(selectApdu()).toEqual([0x00, 0xa4, 0x04, 0x00, 8, ...CARD_AID, 0x00]);
  expect(keyApdu(0x10, 3)).toEqual([0xb0, 0x10, 0x03, 0x00, 0x00]);
  expect(pinApdu(0x41, '1234')).toEqual([0xb0, 0x41, 0x00, 0x00, 4, 0x31, 0x32, 0x33, 0x34]);
});

test('a fresh applet gets its keys, the anchor and the PIN, and no VERIFY_PIN after it', async () => {
  const card = applet();
  const steps: string[] = [];
  const r = await finishCard(card.tx, '482916', s => steps.push(s), {offline});
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
  expect(steps).toEqual(['select', 'keys', 'attest', 'anchor', 'setPin']);
  // SET_PIN's selection has the PIN: it is the last APDU, and no VERIFY_PIN is sent.
  expect(card.log[card.log.length - 1].slice(0, 2)).toEqual([0xb0, 0x41]);
  expect(card.log.some(a => a[0] === 0xb0 && a[1] === 0x40)).toBe(false);
});

test('keys already on the card are kept, never regenerated', async () => {
  const kept = Array.from({length: 32}, () => 0xab);
  const card = applet({preset: {0: kept}});
  const r = await finishCard(card.tx, '482916', undefined, {offline});
  expect(r.ok && r.keys[0]).toEqual(kept);
  expect(r.ok && r.generated).toEqual([1, 2, 3, 4, 5, 6, 7]);
  expect(card.log.some(a => a[1] === 0x10 && a[3] === 0x01)).toBe(false);
});

test('a card that already has a PIN stops before SET_PIN: the limit is refused, no PIN is sent', async () => {
  const card = applet({pinSet: true});
  const r = await finishCard(card.tx, '482916', undefined, {offline});
  expect(r).toMatchObject({ok: false, step: 'attest', sw: 0x6985});
  expect(card.log.some(a => a[1] === 0x41 || a[1] === 0x40)).toBe(false);
});

test('a slot that will not key stops at keys, before any PIN is sent', async () => {
  const card = applet({failGen: 5});
  const r = await finishCard(card.tx, '482916', undefined, {offline});
  expect(r).toMatchObject({ok: false, step: 'keys', sw: 0x6a80});
  expect(card.pin()).toBeNull();
});

test('a card with no applet stops at select', async () => {
  const r = await finishCard(async () => [0x6a, 0x82], '482916', undefined, {offline});
  expect(r).toMatchObject({ok: false, step: 'select', sw: 0x6a82});
});

test('a PIN the applet would refuse is a caller bug, not a card answer', async () => {
  await expect(finishCard(applet().tx, '12', undefined, {offline})).rejects.toThrow('4 to 8 digits');
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
  const r = await finishCard(card.transceive, '135790', undefined, {rekey: true, offline});
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
  const r = await finishCard(card.tx, '482916', undefined, {offline});
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
  const r = await finishCard(card.tx, '482916', undefined, {offline});
  expect(r).toEqual(expect.objectContaining({ok: true, slotCount: 8, generated: range(8)}));
  expect(r.ok && r.keys).toHaveLength(8);
  // SELECT, the probe (GET_PUB 8), GET_PUB + GEN_KEY per slot, SET_LIMIT,
  // GET_STATE, ATTEST_INIT, SET_CERT, SET_REGISTRY_KEY, SET_HEADER, GET_STATE, SET_PIN.
  expect(card.log).toHaveLength(1 + 1 + 16 + 8);
});

test('an 8-slot card that was partly keyed keeps its keys and gets the rest', async () => {
  const kept = Array.from({length: 32}, () => 0xcd);
  const card = applet({count: 8, preset: {0: kept, 1: kept}});
  const r = await finishCard(card.tx, '482916', undefined, {offline});
  expect(r.ok && r.slotCount).toBe(8);
  expect(r.ok && r.generated).toEqual(range(8, 2));
});

test('the install-tap seam: keySlots 32 still keys every slot of a 32-slot card', async () => {
  const card = applet({count: 32});
  const r = await finishCard(card.tx, '482916', undefined, {keySlots: 32, offline});
  expect(r).toEqual(expect.objectContaining({ok: true, slotCount: 32, generated: range(32)}));
  expect(r.ok && r.keys).toHaveLength(32);
  expect(card.pin()).toBe('482916');
});

test('a card whose slot count is unknown stops at keys, nothing generated and no PIN', async () => {
  const card = applet({count: 64});
  const r = await finishCard(card.tx, '482916', undefined, {offline});
  expect(r).toMatchObject({ok: false, step: 'keys'});
  expect(card.log.some(a => a[1] === 0x10)).toBe(false);
  expect(card.pin()).toBeNull();
});

test('a re-key of a 32-slot card covers the install tap’s slots, 0-7', async () => {
  const card = new FakeGpCard({appletSlots: 32}).withLiftedApplet(8);
  const before = card.applet!.slots.slice(0, 8).map(s => s!.join(','));
  // A lifted card on its own keys: SELECT the applet and finish it as the resume does.
  const r = await finishCard(card.transceive, '135790', undefined, {rekey: true, offline});
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

describe('applet 4.0 set-up: the anchor before SET_PIN, in one selection', () => {
  const ins = (log: number[][]) => log.map(a => (a[0] === 0x00 ? 'SELECT' : a[1]));

  test('the order: keys, the offline steps, SET_HEADER with the checked header, GET_STATE, then SET_PIN', async () => {
    const card = applet();
    const r = await finishCard(card.tx, '482916', undefined, {offline});
    expect(r.ok).toBe(true);
    const after = ins(card.log).slice(1 + 1 + 16);
    expect(after).toEqual([0x18, 0x16, 0x30, 0x31, 0x34, 0x19, 0x16, 0x41]);
    // One selection: the only SELECT is the first APDU.
    expect(ins(card.log).filter(i => i === 'SELECT')).toHaveLength(1);
    const header = card.log.find(a => a[1] === 0x19)!;
    expect(header.slice(0, 5)).toEqual([0xb0, 0x19, 0x00, 0x00, 80]);
    expect(header.slice(5)).toEqual([...H900000]);
  });

  test('with no header, or no offline setup, nothing at all is sent and the person is told it needs the internet', async () => {
    for (const options of [{offline: testOffline({header: null})}, {}]) {
      const card = applet();
      const r = await finishCard(card.tx, '482916', undefined, options);
      expect(r).toMatchObject({ok: false, step: 'anchor', sw: null, reason: NEEDS_INTERNET, registry: true});
      expect(card.log).toEqual([]);
    }
  });

  test('a header that fails the phone’s own checks is never sent, and nothing else is either', async () => {
    const tampered = [...H900000];
    tampered[79] ^= 0x01; // the nonce: the hash no longer meets its target
    const cases = [
      testOffline({header: H900000.slice(0, 79)}),
      testOffline({header: tampered}),
      testOffline({now: () => NOW_MS + 8 * 24 * 3600 * 1000}),
      testOffline({now: () => NOW_MS - 4 * 3600 * 1000}),
    ];
    for (const o of cases) {
      const card = applet();
      const r = await finishCard(card.tx, '482916', undefined, {offline: o});
      expect(r).toMatchObject({ok: false, step: 'anchor', sw: null, registry: true});
      expect(!r.ok && r.reason.startsWith(NEEDS_INTERNET)).toBe(true);
      expect(card.log).toEqual([]);
    }
  });

  test('a card that refuses the header, does not anchor, or reports another time gets no SET_PIN', async () => {
    for (const header of ['refuse', 'noAnchor', 'otherTime'] as const) {
      const card = applet({header});
      const r = await finishCard(card.tx, '482916', undefined, {offline});
      expect(r).toMatchObject({ok: false, step: 'anchor'});
      expect(card.log.some(a => a[1] === 0x41)).toBe(false);
      expect(card.pin()).toBeNull();
    }
  });

  test('SET_PIN answering 6A96 is reported as the anchor not kept', async () => {
    const card = applet();
    const tx = async (apdu: number[]) => (apdu[1] === 0x41 && apdu[0] === 0xb0 ? [0x6a, 0x96] : card.tx(apdu));
    const r = await finishCard(tx, '482916', undefined, {offline});
    expect(r).toMatchObject({ok: false, step: 'setPin', sw: 0x6a96, reason: 'the card did not keep the block header as its anchor'});
  });

  test('a card whose SELECT is not exactly major 4 gets the old-card message, and nothing after the SELECT', async () => {
    for (const version of [[0x03, 0x00], [0x02, 0x00], [0x05, 0x00], [0x04], [0x04, 0x00, 0x00]]) {
      const card = applet({version});
      const r = await finishCard(card.tx, '482916', undefined, {offline});
      expect(r).toMatchObject({ok: false, step: 'select', reason: OLD_APPLET, oldApplet: true});
      expect(card.log).toHaveLength(1);
      const v = await finishVerified(applet({version, pinSet: true}).tx, '9999');
      expect(v).toMatchObject({ok: false, step: 'select', reason: OLD_APPLET, oldApplet: true});
    }
    expect(OLD_APPLET).toBe('This card has an older applet. Reset it and set it up again.');
  });

  test('probeCard: an older applet is `old` after its SELECT alone; on 4.0 the PIN state is GET_STATE’s, with no VERIFY_PIN', async () => {
    const old = applet({version: [0x03, 0x00], preset: {0: Array(32).fill(1)}});
    expect(await probeCard(old.tx)).toEqual({kind: 'old'});
    expect(old.log).toHaveLength(1);
    for (const pinSet of [true, false]) {
      const card = applet({pinSet, preset: {0: Array(32).fill(1)}});
      expect(await probeCard(card.tx)).toEqual({kind: 'keyed', slot0: Array(32).fill(1), pin: pinSet ? 'set' : 'notSet'});
      expect(card.log.map(a => a[1])).toEqual([0xa4, 0x11, 0x16]);
    }
  });

  test('parseState reads the PIN bytes and the anchor; any other length is not a 4.0 state', () => {
    const body = new Array(65).fill(0);
    body[32] = 0x68; body[33] = 0x43; body[34] = 0x00; body[35] = 0x01;
    body[59] = 1; body[60] = 3; body[61] = 0x01; body[62] = 0x02; body[63] = 1;
    expect(parseState(body)).toMatchObject({now: 0x68430001, pinSet: true, wrongTries: 3, owed: 0x0102, anchored: true});
    expect(parseState(body.slice(0, 59))).toBeNull();
    expect(parseState([...body, 0])).toBeNull();
    expect(waitSeconds(0)).toBe(0);
    expect(waitSeconds(16)).toBe(8);
  });
});

describe('applet 4.0: a wait served within the tap', () => {
  const PIN4 = '482916';
  const owing = (wrong: number, owed: number) => {
    const card = new FakeGpCard().withFinishedApplet(PIN4);
    card.applet!.wrong = wrong;
    card.applet!.owed = owed;
    return card;
  };

  test('finishVerified serves the wait the card owes, with the seconds left, and reads the keys', async () => {
    const card = owing(2, 2);
    const waits: number[] = [];
    const r = await finishVerified(card.transceive, PIN4, undefined, s => waits.push(s));
    expect(r).toEqual(expect.objectContaining({ok: true}));
    expect(waits).toEqual([1, 1, 0]);
    expect(card.applet!.owed).toBe(0);
  });

  test('verifyPinHeld returns a wrong try as the card answers it, after the wait', async () => {
    const card = owing(1, 1);
    await card.transceive(select4());
    const r = await verifyPinHeld(card.transceive, '1111', 1);
    expect(r.sw).toBe(0x63c2);
    expect(card.applet!.owed).toBe(2);
  });

  test('verifyPinHeld stops when the card asks past the count it gave', async () => {
    let n = 0;
    const always = async () => {
      n += 1;
      return [0x6a, 0x98];
    };
    expect((await verifyPinHeld(always, PIN4, 1)).sw).toBe(0x6a98);
    // the asking, then the one that would have tried the PIN
    expect(n).toBe(2);
  });
});
