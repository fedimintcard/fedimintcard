import {FACTORY_KEYS, fromHex} from './cardkit';
import type {ManagerKeysReply, ResetCheck, ResetCompletion} from './ops';
import {
  flushOutbox,
  identifyCard,
  readJournal,
  readOutbox,
  RESET_JOURNAL_KEY,
  RESET_OUTBOX_KEY,
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
 * PIN across taps, and an ops that answers what the test says: the card ends
 * factory (our package gone, the factory keys back, proved by a factory
 * login); refused with value or an open charge, a wrong PIN, the blocked-PIN
 * path; a tap lifted at every APDU and finished by the next; never an
 * EXTERNAL AUTHENTICATE with keys the card did not prove; the Cashu applet
 * beside ours never touched; and ops unreachable after the card is done.
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
const DELETE = 0xe4;
const VERIFY = 0x40;
const PUT_KEY = 0xd8;
const EXTERNAL_AUTH = 0x82;
const PKG = 'F0464D43415244';

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
    completeReset: jest.fn(async (c: ResetCompletion) => ({
      short_id: c.short_id,
      released: true,
      stranded_notes: 0,
    })),
  };
  let t = 1_770_000_000_000;
  const options: ResetTapOptions = {
    operatorId: 1,
    expected: oldId,
    holderPin: HOLDER_PIN,
    blockedConfirmed: false,
    ops,
    journal,
    random: n => Array.from({length: n}, (_, i) => (i * 29 + 3) & 0xff),
    now: () => (t += 1000),
    ...over,
  };
  return {oldId, journal, ops, options};
}

/** A finished card on its own manager keys, as the install tap leaves it, with a Cashu applet beside. */
const ownCard = () =>
  new FakeGpCard({keys: ownKeys()}).withCashu().withFinishedApplet(HOLDER_PIN);

/** Factory and blank: the factory keys at version 01, nothing of ours on it, the Cashu applet where it was. */
function expectFactory(card: FakeGpCard) {
  expect(card.sameKeys(FACTORY_KEYS)).toBe(true);
  expect(card.kvn).toBe(0x01);
  expect(card.packages.has(PKG)).toBe(false);
  expect([...card.instances.keys()].filter(a => a.startsWith(PKG))).toEqual([]);
  expect(card.packages.has(CASHU_PACKAGE)).toBe(true);
  expect(card.instances.has(CASHU_APPLET)).toBe(true);
  expect(card.failedAuths).toBe(0);
}

const resume = (options: ResetTapOptions): ResetTapOptions => ({
  ...options,
  expected: null,
  holderPin: null,
});

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
  it('takes the card to factory: ours deleted, factory keys back at 01, proved by a factory login; the Cashu applet untouched', async () => {
    const card = ownCard();
    const {oldId, journal, ops, options} = setup(card);
    const r = await resetTap(card.transceive, options);
    expect(r).toEqual({
      ok: true,
      completion: {
        short_id: oldId,
        chip: J3R180_CHIP,
        gp_key_version: 1,
        manager_keys: 'factory',
        package: 'absent',
        verified_at: expect.any(Number),
      },
    });
    expectFactory(card);
    // only this card's two instances and its package, instances first
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
    // PUT KEY in place, P1 01, new version 01; then a factory login, after it, that the card accepted
    const put = card.log.findIndex(a => a[1] === PUT_KEY);
    expect(card.log[put].slice(0, 4)).toEqual([0x84, 0xd8, 0x01, 0x81]);
    expect(card.log[put][5]).toBe(0x01);
    const auths = card.log
      .map((a, i) => [a[1], i] as const)
      .filter(([ins]) => ins === EXTERNAL_AUTH);
    expect(auths).toHaveLength(2);
    expect(auths[1][1]).toBeGreaterThan(put);
    // GET STATUS after the factory login: the proof that ours is absent
    expect(card.log.slice(auths[1][1]).some(a => a[1] === 0xf2)).toBe(true);
    // no new PIN, no keys made: nothing is installed
    expect(card.ins()).not.toContain(0x10);
    expect(card.ins()).not.toContain(0xe8);
    expect(ops.resetCheck).toHaveBeenCalledWith(oldId);
    expect(ops.managerKeys).toHaveBeenCalledWith(J3R180_CHIP, oldId);

    // the journal is gone; the outbox holds the proof, and no PIN or key
    expect(journal.map.has(RESET_JOURNAL_KEY)).toBe(false);
    const raw = journal.map.get(RESET_OUTBOX_KEY)!;
    for (const secret of [HOLDER_PIN, OWN.enc, OWN.mac, OWN.dek]) {
      expect(raw).not.toContain(secret);
    }
    expect(await readOutbox(journal)).toEqual([
      {...(r.ok ? r.completion : {}), operator_id: 1},
    ]);
    // ops gets it, then the outbox is empty
    const flushed = await flushOutbox(ops, journal, 1);
    expect(flushed).toEqual({
      recorded: [{short_id: oldId, released: true, stranded_notes: 0}],
      failed: null,
      left: [],
    });
    expect(ops.completeReset).toHaveBeenCalledWith(r.ok && r.completion);
    expect(journal.map.size).toBe(0);
  });

  it('refuses while ops shows value or an open charge: no PIN asked, nothing deleted, no journal', async () => {
    for (const reasons of [
      ['1 live note, 1024000 msat'],
      ['1 open charge at a till'],
    ]) {
      const card = ownCard();
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
    const card = ownCard();
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
    const card = ownCard();
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

  it('a blocked PIN: refused until the operator confirms, then reset to factory when ops shows the card empty', async () => {
    const card = ownCard();
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
    expectFactory(card);
  });

  it('the blocked path is not a way around a PIN that still works', async () => {
    const card = ownCard();
    const {options} = setup(card, {holderPin: null, blockedConfirmed: true});
    expect(await resetTap(card.transceive, options)).toEqual({
      ok: false,
      stop: {kind: 'pinRequired'},
      resumable: false,
    });
    expect(card.ins()).not.toContain(DELETE);
  });

  it('own keys, ours still there (lifted at the first DELETE): the next tap carries on deleting, no PIN asked', async () => {
    const card = ownCard();
    const {oldId, journal, options} = setup(card);
    card.pull(a => a[1] === DELETE, 'before');
    expect(await resetTap(card.transceive, options)).toEqual(
      expect.objectContaining({
        ok: false,
        resumable: true,
        stop: expect.objectContaining({kind: 'pulled'}),
      }),
    );
    expect(card.packages.has(PKG)).toBe(true);
    expect(await readJournal(journal)).toEqual(
      expect.objectContaining({
        schema: 2,
        old_short_id: oldId,
        chip: J3R180_CHIP,
      }),
    );
    // the journal holds no PIN and no key
    const raw = journal.map.get(RESET_JOURNAL_KEY)!;
    for (const secret of [HOLDER_PIN, OWN.enc, OWN.mac, OWN.dek]) {
      expect(raw).not.toContain(secret);
    }

    // The applet's instance is still there here, with the old short id: the
    // next tap would start over with consent. Lift it after the applet went.
    card.reset();
    card.pull(a => a[1] === DELETE && a[6] === 7, 'before'); // the package's DELETE
    const second = await resetTap(card.transceive, options);
    expect(second).toEqual(expect.objectContaining({ok: false}));
    expect(card.instances.has('F0464D4341524401')).toBe(false);
    expect(card.packages.has(PKG)).toBe(true);

    card.reset();
    const before = card.log.length;
    const r = await resetTap(card.transceive, resume(options));
    expect(r.ok).toBe(true);
    const after = card.log.slice(before).map(a => a[1]);
    expect(after).not.toContain(VERIFY);
    expect(after.filter(i => i === DELETE).length).toBeGreaterThan(0);
    expect(options.ops.resetCheck).toHaveBeenCalledTimes(2);
    expectFactory(card);
    expect(journal.map.has(RESET_JOURNAL_KEY)).toBe(false);
  });

  it('own keys, ours gone (lifted at PUT KEY): the next tap restores the factory keys without deleting', async () => {
    const card = ownCard();
    const {journal, options} = setup(card);
    card.pull(a => a[1] === PUT_KEY, 'before');
    expect((await resetTap(card.transceive, options)).ok).toBe(false);
    expect(card.sameKeys(ownKeys())).toBe(true);
    expect(card.packages.has(PKG)).toBe(false);
    card.reset();
    const before = card.log.length;
    const r = await resetTap(card.transceive, resume(options));
    expect(r.ok).toBe(true);
    const after = card.log.slice(before).map(a => a[1]);
    expect(after).not.toContain(DELETE);
    expect(after.filter(i => i === PUT_KEY)).toHaveLength(1);
    expectFactory(card);
    expect(journal.map.has(RESET_JOURNAL_KEY)).toBe(false);
  });

  it('factory keys, ours gone (PUT KEY’s reply lost): the next tap writes nothing and only saves the completion', async () => {
    const card = ownCard();
    const {oldId, journal, options} = setup(card);
    card.pull(a => a[1] === PUT_KEY, 'after');
    expect((await resetTap(card.transceive, options)).ok).toBe(false);
    expect(card.sameKeys(FACTORY_KEYS)).toBe(true);
    expect(await readOutbox(journal)).toEqual([]);
    card.reset();
    const before = card.log.length;
    const r = await resetTap(card.transceive, resume(options));
    expect(r.ok).toBe(true);
    const after = card.log.slice(before).map(a => a[1]);
    expect(after).not.toContain(DELETE);
    expect(after).not.toContain(PUT_KEY);
    expect(after.filter(i => i === EXTERNAL_AUTH)).toHaveLength(1);
    expectFactory(card);
    expect((await readOutbox(journal)).map(p => p.short_id)).toEqual([oldId]);
  });

  it('a tap lifted at any APDU, before or after the card acted, is finished by tapping again; never a failed login', async () => {
    const full = ownCard();
    await resetTap(full.transceive, setup(full).options);
    const total = full.log.length;
    for (let n = 0; n < total; n++) {
      for (const when of ['before', 'after'] as const) {
        const card = ownCard();
        const original = shortIdOf(card.applet!.slots[0]!);
        const {journal, ops, options} = setup(card);
        card.pull((_a, i) => i === n, when);
        const first = await resetTap(card.transceive, options).catch(
          () => null,
        );
        expect(first?.ok ?? false).toBe(false);
        card.reset();
        // Anything written to the card is covered by the journal.
        const touched =
          !card.sameKeys(ownKeys()) ||
          !card.packages.has(PKG) ||
          !card.instances.has('F0464D4341524401');
        if (touched) {
          expect(await readJournal(journal)).not.toBeNull();
        }
        let r = await resetTap(
          card.transceive,
          (await readJournal(journal)) ? resume(options) : options,
        );
        if (!r.ok && r.stop.kind === 'pinRequired') {
          // the journalled card still had its applet: the operator starts over with the PIN
          card.reset();
          await journal.removeItem(RESET_JOURNAL_KEY);
          r = await resetTap(card.transceive, options);
        }
        expect({n, when, ok: r.ok}).toEqual({n, when, ok: true});
        expectFactory(card);
        expect(journal.map.has(RESET_JOURNAL_KEY)).toBe(false);
        const outbox = await readOutbox(journal);
        expect(outbox.map(p => p.short_id)).toEqual([original]);
        expect((await flushOutbox(ops, journal, 1)).left).toEqual([]);
      }
    }
  });

  it('never sends EXTERNAL AUTHENTICATE for keys the card did not prove: a card on keys ops does not hold stops, untouched', async () => {
    const stranger = {
      enc: fromHex('77'.repeat(16)),
      mac: fromHex('77'.repeat(16)),
      dek: fromHex('77'.repeat(16)),
    };
    const card = new FakeGpCard({keys: stranger}).withFinishedApplet(
      HOLDER_PIN,
    );
    const {journal, options} = setup(card);
    const r = await resetTap(card.transceive, options);
    expect(r).toEqual({
      ok: false,
      stop: expect.objectContaining({kind: 'wrongKeys'}),
      resumable: false,
    });
    expect(card.ins()).not.toContain(EXTERNAL_AUTH);
    expect(card.failedAuths).toBe(0);
    expect(card.ins()).not.toContain(DELETE);
    expect(journal.map.size).toBe(0);
  });

  it('factory keys with our applet still on stops safely: nothing deleted, no journal', async () => {
    const card = new FakeGpCard().withCashu().withFinishedApplet(HOLDER_PIN);
    const {journal, options} = setup(card);
    const r = await resetTap(card.transceive, options);
    expect(r).toEqual({
      ok: false,
      stop: expect.objectContaining({
        kind: 'guarded',
        reason: expect.stringMatching(
          /factory keys and still has the Fedimint applet/,
        ),
      }),
      resumable: false,
    });
    expect(card.ins()).not.toContain(DELETE);
    expect(card.ins()).not.toContain(PUT_KEY);
    expect(card.packages.has(PKG)).toBe(true);
    expect(card.failedAuths).toBe(0);
    expect(journal.map.size).toBe(0);
  });

  it('a journal that cannot be written stops before the first DELETE', async () => {
    const card = ownCard();
    const {journal, options} = setup(card);
    journal.setItem = async () => {
      throw new Error('disk full');
    };
    expect(await resetTap(card.transceive, options)).toEqual({
      ok: false,
      stop: {kind: 'journal', reason: 'disk full'},
      resumable: false,
    });
    expect(card.ins()).not.toContain(DELETE);
    expect(card.sameKeys(ownKeys())).toBe(true);
  });

  it('an unfinished reset is finished only on its own chip, and only by its operator', async () => {
    const card = ownCard();
    const {journal, options} = setup(card);
    card.pull(a => a[1] === PUT_KEY, 'before');
    await resetTap(card.transceive, options);
    const otherCplc = [...J3R180_CPLC];
    otherCplc[18] ^= 0x01; // another serial
    const other = new FakeGpCard({cplc: otherCplc});
    const r = await resetTap(other.transceive, resume(options));
    expect(r).toEqual(
      expect.objectContaining({
        ok: false,
        stop: expect.objectContaining({kind: 'differentCard'}),
      }),
    );
    expect(other.ins()).not.toContain(EXTERNAL_AUTH);
    card.reset();
    expect(
      await resetTap(card.transceive, {...options, operatorId: 2}),
    ).toEqual({ok: false, stop: {kind: 'otherOperator'}, resumable: false});
    expect(journal.map.has(RESET_JOURNAL_KEY)).toBe(true);
  });

  it('ops unreachable after the card is factory: the card is done, the completion waits and is sent later', async () => {
    const card = ownCard();
    const {oldId, journal, ops, options} = setup(card);
    expect((await resetTap(card.transceive, options)).ok).toBe(true);
    ops.completeReset.mockRejectedValueOnce(new Error('network down'));
    const failed = await flushOutbox(ops, journal, 1);
    expect(failed.failed).toEqual({short_id: oldId, reason: 'network down'});
    expect(failed.left.map(p => p.short_id)).toEqual([oldId]);
    // the card needs nothing more: the next reset of another card is not blocked by this one
    expect(journal.map.has(RESET_JOURNAL_KEY)).toBe(false);
    // another operator's waiting completion is not sent with this one's
    const later = await flushOutbox(ops, journal, 2);
    expect(later.recorded).toEqual([]);
    const sent = await flushOutbox(ops, journal, 1);
    expect(sent.recorded).toEqual([
      {short_id: oldId, released: true, stranded_notes: 0},
    ]);
    expect(sent.left).toEqual([]);
    expect(journal.map.size).toBe(0);
  });
});
