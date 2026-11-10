/**
 * The factory reset against the fake card (OPS-PLAN O5): a rotated, installed
 * card ends on the factory keys at version 01 with none of ours listed, proved
 * by a factory login; a pull at every APDU is finished by the next run; no
 * EXTERNAL AUTHENTICATE ever goes out for a set the card did not prove; a
 * foreign package beside ours is never named. All of it as a J3R180 (SCP02)
 * and as a J3R452 (SCP03) on the public default at 01.
 *
 * Then the J3R452's own cases: a card at version FF (as it ships), 00 or 02
 * stops guarded after INITIALIZE UPDATE, with nothing written; one that went
 * through the install tap's first contact resets in place at 01 with AES keys,
 * leaves the seller's applets where they were, and installs again as a J3R180
 * does (D182).
 */
import assert from 'node:assert/strict';
import {randomBytes} from 'node:crypto';
import {describe, it} from 'node:test';

import {FEDIMINT_PACKAGE_AID} from '../aids';
import {toHex} from '../bytes';
import {fedimintPlan, provision, rotateKeys} from '../provision';
import {
  FACTORY_KEY_VERSION,
  factoryReset,
  getStatusCommand,
  parseStatusEntries,
  type FactoryResetOptions,
} from '../reset';
import {FACTORY_KEYS, FACTORY_KVN, SHIPPED_KVN, SPIKE_KEYS} from '../scp02';
import {fedimintLoadFile} from './assets';
import {FakeCard, SELLER_APPLETS, pullAt} from './fake-card';

const PLAN = fedimintPlan(fedimintLoadFile());
const PKG = toHex(FEDIMINT_PACKAGE_AID);
const OWN = {
  enc: Array.from(SPIKE_KEYS.enc),
  mac: Array.from(SPIKE_KEYS.mac),
  dek: Array.from(SPIKE_KEYS.dek),
};
const FOREIGN_PKG = 'F043415348554341';
const FOREIGN_APP = 'F04341534855434101';

function options(
  extra: Partial<FactoryResetOptions> = {},
): FactoryResetOptions {
  let t = 0;
  return {random: n => randomBytes(n), now: () => (t += 7), ...extra};
}

const same = (a: ArrayLike<number>, b: ArrayLike<number>) =>
  toHex(a) === toHex(b);
const isFactory = (card: FakeCard) =>
  same(card.keys.enc, FACTORY_KEYS.enc) &&
  same(card.keys.mac, FACTORY_KEYS.mac) &&
  same(card.keys.dek, FACTORY_KEYS.dek);
const isOwn = (card: FakeCard) =>
  same(card.keys.enc, OWN.enc) &&
  same(card.keys.mac, OWN.mac) &&
  same(card.keys.dek, OWN.dek);
const ours = (card: FakeCard) =>
  card.instanceAids().filter(a => a.startsWith(PKG)).length +
  [...card.packages.keys()].filter(p => p === PKG).length;

/** A finished card: installed on factory keys, rotated to its own, a foreign applet beside it. */
async function installedCard(scp: 2 | 3 = 2): Promise<FakeCard> {
  const card = new FakeCard({scp});
  const installed = await provision(card.transceive, PLAN, options());
  assert.equal(installed.ok, true, installed.reason);
  card.packages.set(FOREIGN_PKG, {
    aid: FOREIGN_PKG,
    modules: [FOREIGN_APP],
    size: 100,
    instancesMade: 1,
  });
  card.instances.set(FOREIGN_APP, {
    aid: FOREIGN_APP,
    module: FOREIGN_APP,
    pkg: FOREIGN_PKG,
    state: {used: [], pubs: [], pinSet: false},
  });
  const rotated = await rotateKeys(
    card.transceive,
    FACTORY_KEYS,
    OWN,
    options(),
  );
  assert.equal(rotated.ok, true, rotated.reason);
  assert.ok(isOwn(card));
  card.reset();
  card.log.length = 0;
  return card;
}

/** EXTERNAL AUTHENTICATE count; each must have been for a set the card proved (no failures). */
const auths = (card: FakeCard) => card.insLog().filter(i => i === 0x82).length;

for (const scp of [2, 3] as const) {
  describe(`factory reset (SCP0${scp})`, () => {
    it('ends on the factory keys at version 01 with none of ours, proved by a factory login', async () => {
      const card = await installedCard(scp);
      let wrote = 0;
      const r = await factoryReset(
        card.transceive,
        OWN,
        options({beforeWrite: async () => void wrote++}),
      );
      assert.equal(r.ok, true, r.reason);
      assert.equal(r.found, 'own-present');
      assert.equal(r.keyVersion, FACTORY_KEY_VERSION);
      assert.equal(wrote, 1);
      assert.ok(isFactory(card));
      assert.equal(card.kvn, 0x01);
      assert.equal(ours(card), 0);
      // the foreign applet and its package are where they were
      assert.ok(card.instances.has(FOREIGN_APP));
      assert.ok(card.packages.has(FOREIGN_PKG));
      // DELETE named only ours, instances first
      const deleted = card.log
        .filter(a => a[1] === 0xe4)
        .map(a => toHex(a.slice(7, 7 + a[6])));
      assert.deepEqual(deleted, [
        'F0464D4341524401',
        'F0464D434152444D',
        'F0464D43415244',
      ]);
      // PUT KEY in place: P1 01, new version 01
      const put = card.log.find(a => a[1] === 0xd8)!;
      assert.deepEqual(put.slice(0, 4), [0x84, 0xd8, 0x01, 0x81]);
      assert.equal(put[5], 0x01);
      // two logins (own, then factory), no failed one
      assert.equal(auths(card), 2);
      assert.equal(card.failedAuths, 0);
      // a second run finds it factory and writes nothing
      card.reset();
      const again = await factoryReset(
        card.transceive,
        OWN,
        options({beforeWrite: async () => assert.fail('no write expected')}),
      );
      assert.equal(again.ok, true, again.reason);
      assert.equal(again.found, 'factory-absent');
      assert.equal(card.failedAuths, 0);
    });

    it('a pull at any APDU, before or after the card acted, is finished by the next run without a failed login', async () => {
      const full = await installedCard(scp);
      await factoryReset(full.transceive, OWN, options());
      const total = full.log.length;
      for (let n = 0; n < total; n++) {
        for (const when of ['before', 'after'] as const) {
          const card = await installedCard(scp);
          const torn = pullAt(card, n, when);
          const first = await factoryReset(torn.tx, OWN, options());
          assert.equal(first.ok, false, `pull at ${n} ${when}`);
          assert.equal(first.kind, 'pulled');
          card.reset();
          const r = await factoryReset(card.transceive, OWN, options());
          assert.equal(r.ok, true, `${n} ${when}: ${r.reason}`);
          assert.ok(isFactory(card), `${n} ${when}`);
          assert.equal(ours(card), 0);
          assert.ok(card.instances.has(FOREIGN_APP));
          assert.equal(card.failedAuths, 0, `${n} ${when}`);
          assert.equal(
            card.insLog().filter(i => i === 0xd8).length <= 1,
            true,
            'PUT KEY at most once after a pull',
          );
        }
      }
    });

    it('a card on neither set stops at INITIALIZE UPDATE: no EXTERNAL AUTHENTICATE, nothing written', async () => {
      const stranger = {
        enc: Array(16).fill(0x77),
        mac: Array(16).fill(0x77),
        dek: Array(16).fill(0x77),
      };
      const card = new FakeCard({scp, keys: stranger});
      const r = await factoryReset(
        card.transceive,
        OWN,
        options({beforeWrite: async () => assert.fail('no write expected')}),
      );
      assert.equal(r.ok, false);
      assert.equal(r.kind, 'wrongKeys');
      assert.equal(card.insLog().at(-1), 0x50);
      assert.equal(auths(card), 0);
      assert.equal(card.failedAuths, 0);
    });

    it('factory keys with our applet still on: stops guarded, deletes nothing', async () => {
      const card = new FakeCard({scp});
      assert.equal(
        (await provision(card.transceive, PLAN, options())).ok,
        true,
      );
      card.reset();
      card.log.length = 0;
      const r = await factoryReset(card.transceive, OWN, options());
      assert.equal(r.ok, false);
      assert.equal(r.kind, 'guarded');
      assert.match(r.reason!, /factory keys and still has the Fedimint applet/);
      assert.ok(!card.insLog().includes(0xe4));
      assert.ok(ours(card) > 0);
    });

    it('a key set at another version stops before anything is written', async () => {
      const card = new FakeCard({scp, keys: {...OWN}, kvn: 0x02});
      const r = await factoryReset(card.transceive, OWN, options());
      assert.equal(r.ok, false);
      assert.equal(r.kind, 'guarded');
      assert.ok(!card.insLog().includes(0x82));
      assert.ok(isOwn(card));
    });

    it('beforeWrite throwing ends the run with nothing deleted', async () => {
      const card = await installedCard(scp);
      await assert.rejects(
        factoryReset(
          card.transceive,
          OWN,
          options({
            beforeWrite: async () => {
              throw new Error('disk full');
            },
          }),
        ),
        /disk full/,
      );
      assert.ok(!card.insLog().includes(0xe4));
      assert.ok(isOwn(card));
    });
  });
}

describe('factory reset: GET STATUS', () => {
  it('GET STATUS is 80 F2 40|20 02|03 02 4F 00 00, and its entries parse', () => {
    assert.deepEqual(
      getStatusCommand('applications'),
      [0x80, 0xf2, 0x40, 0x02, 0x02, 0x4f, 0x00, 0x00],
    );
    assert.deepEqual(
      getStatusCommand('loadFiles', true),
      [0x80, 0xf2, 0x20, 0x03, 0x02, 0x4f, 0x00, 0x00],
    );
    const entry = [
      0xe3, 0x0e, 0x4f, 0x07, 0xf0, 0x46, 0x4d, 0x43, 0x41, 0x52, 0x44, 0x9f,
      0x70, 0x01, 0x01, 0xcc, 0x00,
    ];
    entry[1] = entry.length - 2;
    assert.deepEqual(parseStatusEntries(entry), [{aid: PKG, loadFile: null}]);
    assert.throws(() => parseStatusEntries([0xe3, 0x05, 0x4f]));
  });
});

/** A J3R452 as it ships on the public default: SCP03, key set version FF. */
const shipped = (sellerApplets = false) =>
  new FakeCard({scp: 3, kvn: SHIPPED_KVN, sellerApplets});

/**
 * The install tap's card-manager steps on a card: the install, logged in with
 * whichever of the card's own set and the factory set it proves, then the
 * rotation from the factory set to its own, which makes first contact on a
 * card at FF. The card is then lifted.
 */
async function throughTheInstallTap(card: FakeCard) {
  const installed = await provision(card.transceive, PLAN, {
    ...options(),
    keys: [OWN, FACTORY_KEYS],
  });
  assert.equal(installed.ok, true, installed.reason);
  const rotated = await rotateKeys(
    card.transceive,
    FACTORY_KEYS,
    OWN,
    options(),
  );
  assert.equal(rotated.ok, true, rotated.reason);
  card.reset();
  return rotated;
}

/** GET DATA E0 with no secure channel, as gp -i and first contact send it. */
const KEY_INFORMATION = [0x80, 0xca, 0x00, 0xe0, 0x00];

/** PUT KEY's header as sent (CLA 84, INS, P1, P2, Lc), every one so far. */
const putKeyHeaders = (card: FakeCard) =>
  card.log.filter(a => a[1] === 0xd8).map(a => a.slice(0, 5));

describe('factory reset off version 01, and of a J3R452 after first contact (D182)', () => {
  it('stops guarded at version FF, 00 or 02 under either protocol, after INITIALIZE UPDATE: no login, beforeWrite never called, the card as it was', async () => {
    for (const scp of [2, 3] as const) {
      for (const kvn of [SHIPPED_KVN, 0x00, 0x02]) {
        const where = `SCP0${scp} at ${toHex([kvn])}`;
        const card = new FakeCard({scp, kvn, keys: {...OWN}});
        let wrote = 0;
        const r = await factoryReset(
          card.transceive,
          OWN,
          options({beforeWrite: async () => void wrote++}),
        );
        assert.equal(r.ok, false, where);
        assert.equal(r.kind, 'guarded', where);
        assert.equal(r.phase, 'auth', where);
        assert.match(
          r.reason!,
          kvn === SHIPPED_KVN
            ? /version FF.*only the install tap's first contact/
            : new RegExp(`version ${kvn}, not 1`),
          where,
        );
        assert.equal(wrote, 0, where);
        assert.deepEqual(card.insLog(), [0xa4, 0x50], where);
        assert.equal(card.kvn, kvn, where);
        assert.ok(isOwn(card), where);
        assert.equal(card.failedAuths, 0, where);
      }
    }
  });

  it('stops guarded on a J3R452 as it ships, on the public default at FF: only first contact takes it off FF', async () => {
    const card = shipped();
    let wrote = 0;
    const r = await factoryReset(
      card.transceive,
      OWN,
      options({beforeWrite: async () => void wrote++}),
    );
    assert.equal(r.kind, 'guarded');
    assert.match(r.reason!, /first contact/);
    assert.equal(wrote, 0);
    assert.deepEqual(card.insLog(), [0xa4, 0x50]);
    assert.equal(card.kvn, SHIPPED_KVN);
    assert.ok(isFactory(card));
  });

  it('after the install tap made first contact, ends on the public default at 01: PUT KEY of AES keys in place, proved by a login at 01', async () => {
    const card = shipped();
    const rotated = await throughTheInstallTap(card);
    assert.equal(rotated.firstContact, true);
    assert.equal(card.kvn, FACTORY_KVN);
    assert.ok(isOwn(card));
    card.log.length = 0;
    let wrote = 0;
    const r = await factoryReset(
      card.transceive,
      OWN,
      options({beforeWrite: async () => void wrote++}),
    );
    assert.equal(r.ok, true, r.reason);
    assert.equal(r.found, 'own-present');
    assert.equal(r.keyVersion, FACTORY_KVN);
    assert.equal(wrote, 1);
    assert.ok(isFactory(card));
    assert.equal(card.kvn, FACTORY_KVN);
    assert.equal(ours(card), 0);
    // in place at 01, three entries 88 11 10 <AES key under the DEK> 03 <KCV>: Lc 4E with the MAC
    const put = card.log.find(a => a[1] === 0xd8)!;
    assert.deepEqual(
      put.slice(0, 9),
      [0x84, 0xd8, 0x01, 0x81, 0x4e, 0x01, 0x88, 0x11, 0x10],
    );
    assert.equal(auths(card), 2);
    assert.equal(card.failedAuths, 0);
    // what the card lists now: the public default's three AES keys at 01, nothing at FF
    card.reset();
    assert.equal(
      toHex(await card.transceive(KEY_INFORMATION)),
      'E012C00401018810C00402018810C00403018810' + '9000',
    );
  });

  it("with the seller's applets on the card, deletes only this card's objects and leaves the seller's where they were", async () => {
    const card = shipped(true);
    await throughTheInstallTap(card);
    card.log.length = 0;
    const r = await factoryReset(card.transceive, OWN, options());
    assert.equal(r.ok, true, r.reason);
    assert.ok(isFactory(card));
    assert.equal(ours(card), 0);
    assert.deepEqual(
      card.instanceAids(),
      SELLER_APPLETS.map(a => a.instance),
    );
    assert.deepEqual(
      [...card.packages.keys()],
      SELLER_APPLETS.map(a => a.pkg),
    );
    const deleted = card.log
      .filter(a => a[1] === 0xe4)
      .map(a => toHex(a.slice(7, 7 + a[6])));
    assert.deepEqual(deleted, [
      'F0464D4341524401',
      'F0464D434152444D',
      'F0464D43415244',
    ]);
  });

  it("D182's chain: first contact, the install, a reset, then a second install whose rotation is in place at 01, as on a J3R180", async () => {
    const card = shipped();
    const first = await throughTheInstallTap(card);
    assert.equal(first.firstContact, true);
    const reset = await factoryReset(card.transceive, OWN, options());
    assert.equal(reset.ok, true, reset.reason);
    assert.ok(isFactory(card));
    card.reset();
    const second = await throughTheInstallTap(card);
    assert.equal(second.rotated, true);
    assert.equal(second.firstContact, undefined);
    assert.equal(second.keyVersion, FACTORY_KVN);
    // first contact's PUT KEY (P1 00), the reset's, then the second rotation's, both in place at 01
    assert.deepEqual(putKeyHeaders(card), [
      [0x84, 0xd8, 0x00, 0x81, 0x4e],
      [0x84, 0xd8, 0x01, 0x81, 0x4e],
      [0x84, 0xd8, 0x01, 0x81, 0x4e],
    ]);
    assert.ok(isOwn(card));
    assert.equal(card.kvn, FACTORY_KVN);
    assert.equal(ours(card), 2 + 1);
    assert.equal(card.failedAuths, 0);
  });
});
