import {FACTORY_KEYS, fromHex} from './cardkit';
import type {ManagerKeysReply, ResetCheck} from './ops';
import {
  identifyCard,
  readJournal,
  recordReset,
  RESET_JOURNAL_KEY,
  resetTap,
  type JournalStorage,
  type ResetTapOptions,
} from './reset';
import {shortIdOf} from './store';
import {
  CASHU_APPLET,
  CASHU_PACKAGE,
  FakeGpCard,
  J3R180_CHIP,
  J3R180_CPLC,
} from './testing/fakeGpCard';

/**
 * Reset a card (OPS-PLAN O5) against a card that keeps its keys, applets and
 * PIN across taps, and an ops that answers what the test says: refused with
 * value or an open charge, a wrong PIN, the blocked-PIN path, a reset broken
 * after DELETE and finished by the next tap, and the Cashu applet beside ours
 * never touched.
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
const HOLDER_PIN = '2468';
const NEW_PIN = '813579';
const DELETE = 0xe4;
const VERIFY = 0x40;

function memoryJournal(): JournalStorage & {map: Map<string, string>} {
  const map = new Map<string, string>();
  return {
    map,
    getItem: async k => map.get(k) ?? null,
    setItem: async (k, v) => void map.set(k, v),
    removeItem: async k => void map.delete(k),
  };
}

function check(shortId: string, over: Partial<ResetCheck> = {}): ResetCheck {
  return {
    short_id: shortId,
    chip: J3R180_CHIP,
    empty: true,
    resettable: true,
    reasons: [],
    live_notes: 0,
    live_msat: 0,
    pending_notes: 0,
    pending_msat: 0,
    open_charges: 0,
    open_loads: 0,
    replaced_by: null,
    ...over,
  };
}

function setup(
  card: FakeGpCard,
  over: Partial<ResetTapOptions> = {},
  checked?: Partial<ResetCheck>,
) {
  const oldId = card.applet ? shortIdOf(card.applet.slots[0]!) : '00000000';
  const journal = memoryJournal();
  const ops = {
    resetCheck: jest.fn(async (id: string) => check(id, checked)),
    managerKeys: jest.fn(async (chip: string) => ({...OWN, chip})),
    recordReset: jest.fn(async (old: string, row: {short_id: string}) => ({
      old_short_id: old,
      short_id: row.short_id,
      stranded_notes: 0,
    })),
  };
  let t = 1_770_000_000_000;
  const options: ResetTapOptions = {
    operatorId: 1,
    expected: oldId,
    holderPin: HOLDER_PIN,
    blockedConfirmed: false,
    newPin: NEW_PIN,
    ops,
    journal,
    random: n => Array.from({length: n}, (_, i) => (i * 29 + 3) & 0xff),
    now: () => (t += 1000),
    ...over,
  };
  return {oldId, journal, ops, options};
}

describe('identify', () => {
  it('reads the short id and the PIN’s state without spending a try', async () => {
    const card = new FakeGpCard().withFinishedApplet();
    const id = await identifyCard(card.transceive);
    expect(id).toEqual({
      ok: true,
      shortId: shortIdOf(card.applet!.slots[0]!),
      pin: 'set',
    });
    expect(card.applet!.tries).toBe(3);
    card.applet!.blocked = true;
    expect(await identifyCard(card.transceive)).toEqual(
      expect.objectContaining({ok: true, pin: 'blocked'}),
    );
    expect(await identifyCard(new FakeGpCard().transceive)).toEqual(
      expect.objectContaining({ok: false, why: 'noApplet'}),
    );
  });
});

describe('reset', () => {
  it('resets a card: deleted, reinstalled, rotated, new keys and PIN; the Cashu applet untouched', async () => {
    const card = new FakeGpCard().withCashu().withFinishedApplet(HOLDER_PIN);
    const {oldId, journal, ops, options} = setup(card);
    const r = await resetTap(card.transceive, options);
    expect(r.ok).toBe(true);
    if (!r.ok) return;
    expect(r.card.short_id).not.toBe(oldId);
    expect(r.card.short_id).toBe(shortIdOf(card.applet!.slots[0]!));
    expect(r.card.chip).toBe(J3R180_CHIP);
    expect(card.applet!.pin).toBe(NEW_PIN);
    expect(card.sameKeys(ownKeys())).toBe(true);
    expect(card.failedAuths).toBe(0);
    // only this card's package and instances were deleted; the other applet is where it was
    expect(card.packages.has(CASHU_PACKAGE)).toBe(true);
    expect(card.instances.has(CASHU_APPLET)).toBe(true);
    const deleted = card.log
      .filter(a => a[1] === DELETE)
      .map(a =>
        a
          .slice(7, 7 + a[6])
          .map(b => b.toString(16).padStart(2, '0'))
          .join(''),
      );
    expect(deleted).toEqual([
      'f0464d4341524401',
      'f0464d434152444d',
      'f0464d43415244',
    ]);
    expect(ops.resetCheck).toHaveBeenCalledWith(oldId);
    expect(ops.managerKeys).toHaveBeenCalledWith(J3R180_CHIP, oldId);

    // the journal names the finished card and holds no PIN and no manager key
    const raw = journal.map.get(RESET_JOURNAL_KEY)!;
    for (const secret of [HOLDER_PIN, NEW_PIN, OWN.enc, OWN.mac, OWN.dek]) {
      expect(raw).not.toContain(secret);
    }
    expect(await readJournal(journal)).toEqual(
      expect.objectContaining({
        stage: 'recording',
        old_short_id: oldId,
        card: r.card,
      }),
    );
    // ops records it, then the journal goes
    expect(await recordReset(ops, journal, oldId, r.card)).toEqual({
      old_short_id: oldId,
      short_id: r.card.short_id,
      stranded_notes: 0,
    });
    expect(ops.recordReset).toHaveBeenCalledWith(oldId, r.card);
    expect(journal.map.size).toBe(0);
  });

  it('refuses while ops shows value or an open charge: no PIN asked, nothing deleted, no journal', async () => {
    for (const reasons of [
      ['1 live note, 1024000 msat'],
      ['1 open charge at a till'],
    ]) {
      const card = new FakeGpCard().withFinishedApplet(HOLDER_PIN);
      const {journal, options} = setup(
        card,
        {},
        {resettable: false, empty: false, reasons},
      );
      const r = await resetTap(card.transceive, options);
      expect(r).toEqual({
        ok: false,
        stop: {kind: 'notResettable', reasons},
        resumable: false,
      });
      expect(card.ins()).not.toContain(VERIFY);
      expect(card.ins()).not.toContain(DELETE);
      expect(journal.map.size).toBe(0);
    }
  });

  it('a wrong PIN stops with the tries left, once, and touches nothing', async () => {
    const card = new FakeGpCard().withFinishedApplet(HOLDER_PIN);
    const {journal, options} = setup(card, {holderPin: '1111'});
    const r = await resetTap(card.transceive, options);
    expect(r).toEqual({
      ok: false,
      stop: {kind: 'wrongPin', triesLeft: 2},
      resumable: false,
    });
    expect(card.ins().filter(i => i === VERIFY)).toHaveLength(1);
    expect(card.ins()).not.toContain(DELETE);
    expect(card.ins()).not.toContain(0xca);
    expect(journal.map.size).toBe(0);
  });

  it('a different card than the one identified is refused before anything', async () => {
    const card = new FakeGpCard().withFinishedApplet(HOLDER_PIN);
    const {options, ops} = setup(card, {expected: 'deadbeef'});
    const r = await resetTap(card.transceive, options);
    expect(r).toEqual(
      expect.objectContaining({
        ok: false,
        stop: expect.objectContaining({kind: 'differentCard'}),
      }),
    );
    expect(ops.resetCheck).not.toHaveBeenCalled();
  });

  it('a blocked PIN: refused until the operator confirms, then reset when ops shows the card empty', async () => {
    const card = new FakeGpCard().withFinishedApplet(HOLDER_PIN);
    card.applet!.blocked = true;
    const first = setup(card, {holderPin: null});
    expect(await resetTap(card.transceive, first.options)).toEqual({
      ok: false,
      stop: {kind: 'pinBlocked', pin: 'blocked'},
      resumable: false,
    });
    expect(card.ins()).not.toContain(DELETE);

    // not empty at ops: the confirmation does not override the refusal
    card.reset();
    const full = setup(
      card,
      {holderPin: null, blockedConfirmed: true},
      {resettable: false, empty: false, reasons: ['1 pending note, 1000 msat']},
    );
    expect(await resetTap(card.transceive, full.options)).toEqual(
      expect.objectContaining({
        stop: expect.objectContaining({kind: 'notResettable'}),
      }),
    );

    // a holder PIN typed at a blocked card says so rather than spending anything
    card.reset();
    const typed = setup(card, {holderPin: HOLDER_PIN});
    expect(await resetTap(card.transceive, typed.options)).toEqual(
      expect.objectContaining({stop: {kind: 'pinBlocked', pin: 'blocked'}}),
    );

    card.reset();
    const confirmed = setup(card, {holderPin: null, blockedConfirmed: true});
    const r = await resetTap(card.transceive, confirmed.options);
    expect(r.ok).toBe(true);
    expect(card.applet!.blocked).toBe(false);
    expect(card.applet!.pin).toBe(NEW_PIN);
  });

  it('the blocked path is not a way around a PIN that still works', async () => {
    const card = new FakeGpCard().withFinishedApplet(HOLDER_PIN);
    const {options} = setup(card, {holderPin: null, blockedConfirmed: true});
    expect(await resetTap(card.transceive, options)).toEqual({
      ok: false,
      stop: {kind: 'pinRequired'},
      resumable: false,
    });
    expect(card.ins()).not.toContain(DELETE);
  });

  it('a reset broken after DELETE is finished by the next tap: same chip, no PIN, rotated keys', async () => {
    const card = new FakeGpCard().withCashu().withFinishedApplet(HOLDER_PIN);
    const {oldId, journal, options} = setup(card);
    card.pull(a => a[1] === DELETE, 'after');
    const r1 = await resetTap(card.transceive, options);
    expect(r1).toEqual(
      expect.objectContaining({
        ok: false,
        resumable: true,
        stop: expect.objectContaining({kind: 'pulled'}),
      }),
    );
    expect(card.instances.has('F0464D4341524401')).toBe(false);
    expect(await readJournal(journal)).toEqual(
      expect.objectContaining({
        stage: 'erasing',
        old_short_id: oldId,
        chip: J3R180_CHIP,
      }),
    );

    // The next tap: the card has no applet now, so no PIN and no ops check; the journal says which chip.
    card.reset();
    const before = card.log.length;
    const r2 = await resetTap(card.transceive, {
      ...options,
      expected: null,
      holderPin: null,
    });
    expect(r2.ok).toBe(true);
    // the only VERIFY_PIN is the new PIN's, the tap's last APDU: the holder was not asked again
    const after = card.log.slice(before).map(a => a[1]);
    expect(after.filter(i => i === VERIFY)).toHaveLength(1);
    expect(after.at(-1)).toBe(VERIFY);
    expect(options.ops.resetCheck).toHaveBeenCalledTimes(1);
    expect(card.sameKeys(ownKeys())).toBe(true);
    expect(card.applet!.pin).toBe(NEW_PIN);
    expect(card.instances.has(CASHU_APPLET)).toBe(true);
    expect(await readJournal(journal)).toEqual(
      expect.objectContaining({stage: 'recording', old_short_id: oldId}),
    );
  });

  it('a reset broken after the card took its own keys is finished on those keys, without a failed login', async () => {
    const card = new FakeGpCard().withFinishedApplet(HOLDER_PIN);
    const {options} = setup(card);
    card.pull(a => a[1] === 0xd8, 'after');
    expect((await resetTap(card.transceive, options)).ok).toBe(false);
    card.reset();
    const r = await resetTap(card.transceive, {
      ...options,
      expected: null,
      holderPin: null,
    });
    expect(r.ok).toBe(true);
    expect(card.ins().filter(i => i === 0xd8)).toHaveLength(1);
    expect(card.failedAuths).toBe(0);
  });

  it('an unfinished reset is finished only on its own chip, and only by its operator', async () => {
    const card = new FakeGpCard().withFinishedApplet(HOLDER_PIN);
    const {journal, options} = setup(card);
    card.pull(a => a[1] === DELETE, 'after');
    await resetTap(card.transceive, options);
    const otherCplc = [...J3R180_CPLC];
    otherCplc[18] ^= 0x01; // another serial
    const other = new FakeGpCard({cplc: otherCplc});
    const r = await resetTap(other.transceive, {
      ...options,
      expected: null,
      holderPin: null,
    });
    expect(r).toEqual(
      expect.objectContaining({
        ok: false,
        stop: expect.objectContaining({kind: 'differentCard'}),
      }),
    );
    expect(other.ins()).not.toContain(DELETE);
    card.reset();
    expect(
      await resetTap(card.transceive, {...options, operatorId: 2}),
    ).toEqual({ok: false, stop: {kind: 'otherOperator'}, resumable: false});
    expect(journal.map.size).toBe(1);
  });

  it('a finished card waiting on ops is never reported as a new reset', async () => {
    const card = new FakeGpCard().withFinishedApplet(HOLDER_PIN);
    const {options} = setup(card);
    expect((await resetTap(card.transceive, options)).ok).toBe(true);
    card.reset();
    expect(await resetTap(card.transceive, options)).toEqual({
      ok: false,
      stop: {kind: 'recordPending'},
      resumable: true,
    });
  });

  it('a card on keys ops does not hold is stopped without a failed login, the journal kept', async () => {
    const stranger = {
      enc: fromHex('77'.repeat(16)),
      mac: fromHex('77'.repeat(16)),
      dek: fromHex('77'.repeat(16)),
    };
    const card = new FakeGpCard({keys: stranger}).withFinishedApplet(
      HOLDER_PIN,
    );
    const {options} = setup(card);
    const r = await resetTap(card.transceive, options);
    expect(r).toEqual(
      expect.objectContaining({
        ok: false,
        stop: expect.objectContaining({kind: 'wrongKeys'}),
      }),
    );
    expect(card.failedAuths).toBe(0);
    expect(card.ins()).not.toContain(DELETE);
    expect(card.sameKeys(FACTORY_KEYS)).toBe(false);
  });
});
