/**
 * The factory reset against the fake card (OPS-PLAN O5): a rotated, installed
 * card ends on the factory keys at version 01 with none of ours listed, proved
 * by a factory login; a pull at every APDU is finished by the next run; no
 * EXTERNAL AUTHENTICATE ever goes out for a set the card did not prove; a
 * foreign package beside ours is never named.
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
import {FACTORY_KEYS, SPIKE_KEYS} from '../scp02';
import {fedimintLoadFile} from './assets';
import {FakeCard, pullAt} from './fake-card';

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
async function installedCard(): Promise<FakeCard> {
  const card = new FakeCard();
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

describe('factory reset', () => {
  it('ends on the factory keys at version 01 with none of ours, proved by a factory login', async () => {
    const card = await installedCard();
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
    const full = await installedCard();
    await factoryReset(full.transceive, OWN, options());
    const total = full.log.length;
    for (let n = 0; n < total; n++) {
      for (const when of ['before', 'after'] as const) {
        const card = await installedCard();
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
    const card = new FakeCard({keys: stranger});
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
    const card = new FakeCard();
    assert.equal((await provision(card.transceive, PLAN, options())).ok, true);
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
    const card = new FakeCard({keys: {...OWN}, kvn: 0x02});
    const r = await factoryReset(card.transceive, OWN, options());
    assert.equal(r.ok, false);
    assert.equal(r.kind, 'guarded');
    assert.ok(!card.insLog().includes(0x82));
    assert.ok(isOwn(card));
  });

  it('beforeWrite throwing ends the run with nothing deleted', async () => {
    const card = await installedCard();
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
