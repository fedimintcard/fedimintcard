/**
 * What a J3R180 receives, pinned. Every installed card is a J3R180 that speaks
 * SCP02, so a change to cardkit that moves one byte it sends that chip is a
 * change to every card in the field. These pins are sha256 over every APDU the
 * host sent (one hex line each) in three runs against the fake card, with the
 * host challenge and the clock fixed and a load file built here rather than
 * read from the CAP, so nothing but cardkit's own bytes can move them:
 *
 * - `provision` of a blank card on the factory keys, offered its own set and
 *   the factory set;
 * - `rotateKeys` from the factory set to the spike set on a fresh card;
 * - `factoryReset` of a card installed and rotated to its own set.
 *
 * They were taken at 4301b0a, before SCP03 was added beside SCP02. A pin that
 * moves is a J3R180 that would see different bytes: find out why before
 * touching the hash.
 */
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {describe, it} from 'node:test';

import {CARD_AID, FEDIMINT_PACKAGE_AID, OPENER_AID} from '../aids';
import {toHex} from '../bytes';
import {
  fedimintPlan,
  provision,
  rotateKeys,
  type LoadFileAsset,
  type RunOptions,
} from '../provision';
import {factoryReset} from '../reset';
import {FACTORY_KEYS, SPIKE_KEYS} from '../scp02';
import {FakeCard} from './fake-card';

/** The card's own set in these runs: the spike's public test set. */
const OWN = {
  enc: Array.from(SPIKE_KEYS.enc),
  mac: Array.from(SPIKE_KEYS.mac),
  dek: Array.from(SPIKE_KEYS.dek),
};

/** The host challenge all 5A, the clock at 0: nothing about a run varies but cardkit. */
function fixed(): RunOptions {
  return {random: n => new Array(n).fill(0x5a), now: () => 0};
}

/**
 * A load file of 600 bytes (three LOAD blocks) shaped as the fake card reads
 * one: C4, a two-byte length, a Header naming this package and an Applet
 * component naming the opener and the card applet, then filler. Built from
 * lengths here so a new CAP never moves a pin.
 */
function syntheticLoadFile(): LoadFileAsset {
  const component = (tag: number, body: number[]) => [
    tag,
    (body.length >> 8) & 0xff,
    body.length & 0xff,
    ...body,
  ];
  const header = component(0x01, [
    0xde,
    0xca,
    0xff,
    0xed, // magic
    0x01,
    0x02, // minor, major
    0x00, // flags
    0x00,
    0x03, // package minor, major
    FEDIMINT_PACKAGE_AID.length,
    ...FEDIMINT_PACKAGE_AID,
  ]);
  const applet = component(0x03, [
    0x02,
    OPENER_AID.length,
    ...OPENER_AID,
    0x00,
    0x00,
    CARD_AID.length,
    ...CARD_AID,
    0x00,
    0x00,
  ]);
  const size = 600;
  const body = size - 4;
  const file = [
    0xc4,
    0x82,
    (body >> 8) & 0xff,
    body & 0xff,
    ...header,
    ...applet,
  ];
  while (file.length < size) {
    file.push(0x30);
  }
  return {
    name: 'synthetic load file (scp02-pins)',
    size,
    sha256: createHash('sha256').update(Buffer.from(file)).digest('hex'),
    hex: toHex(file),
  };
}

const PLAN = fedimintPlan(syntheticLoadFile());

/** sha256 over every APDU the card received, one hex line each. */
function wire(card: FakeCard): string {
  return createHash('sha256')
    .update(card.log.map(a => toHex(a)).join('\n'))
    .digest('hex');
}

/** The next tap, with an empty log. */
function nextTap(card: FakeCard): void {
  card.reset();
  card.log.length = 0;
}

/**
 * What 4301b0a sent a J3R180 in each run. The rotation's is also cashucard's
 * `ROTATION_AT_01[2]` (its rotate.test.ts at 0229dc8): the two engines sent
 * that chip the same bytes before either had SCP03.
 */
const PINS = {
  provision: '7275dfcb9c22531bde80fda42d7a470b45cc36674604e684627e69efd87583bd',
  rotateKeys:
    'c0bb9ed8abc77f89055f561de3f08f2a4fcc6538fbbf79a0e46c28f4a0a9bfe3',
  factoryReset:
    '14fed11afaf2bb7c0047bf8843009fe6763cdbded90fd873826faef65913f3f1',
};

describe('SCP02 on the wire: what a J3R180 receives, pinned at 4301b0a', () => {
  it('provision of a blank card offered its own set and the factory set', async () => {
    const card = new FakeCard();
    const r = await provision(card.transceive, PLAN, {
      ...fixed(),
      keys: [OWN, FACTORY_KEYS],
    });
    assert.equal(r.ok, true, r.reason);
    assert.equal(r.keySet, 1);
    assert.equal(wire(card), PINS.provision);
  });

  it('rotateKeys from the factory set to the spike set, in place at version 01', async () => {
    const card = new FakeCard();
    const r = await rotateKeys(
      card.transceive,
      FACTORY_KEYS,
      SPIKE_KEYS,
      fixed(),
    );
    assert.equal(r.ok, true, r.reason);
    assert.equal(r.rotated, true);
    assert.equal(wire(card), PINS.rotateKeys);
  });

  it('factoryReset of a card installed and rotated to its own set', async () => {
    const card = new FakeCard();
    const installed = await provision(card.transceive, PLAN, {
      ...fixed(),
      keys: [OWN, FACTORY_KEYS],
    });
    assert.equal(installed.ok, true, installed.reason);
    nextTap(card);
    const rotated = await rotateKeys(
      card.transceive,
      FACTORY_KEYS,
      OWN,
      fixed(),
    );
    assert.equal(rotated.ok, true, rotated.reason);
    nextTap(card);
    const r = await factoryReset(card.transceive, OWN, fixed());
    assert.equal(r.ok, true, r.reason);
    assert.equal(r.found, 'own-present');
    assert.equal(wire(card), PINS.factoryReset);
  });
});
