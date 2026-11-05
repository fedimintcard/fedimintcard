/**
 * The chip identity ops keys card-manager keys on: the parse against a real
 * J3R180's CPLC, which fields count, the refusals, and the two APDUs the read
 * sends. Adapted from cashucard's gp/test/identity.test.ts (see ../../NOTICE);
 * the capture is the owner's `gp -i` of a J3R180 (IC serial 09336320, the
 * card O1 finished on Max), from cashucard's gp/test/fixtures/gp-info.txt.
 */
import assert from 'node:assert/strict';
import {describe, it} from 'node:test';

import {fromHex, toHex} from '../bytes';
import {CHIP_ID_PATTERN, parseCplc, readChipIdentity} from '../identity';
import {CardError, TransportError, type Transceiver} from '../scp02';

const J3R180_CPLC = fromHex(
  '9F7F2A4790D321470000000000304109336320554700000000000000000432A939333336330000000000000000',
);
const J3R180_ID = 'cplc-v1:4790d3213041093363205547';

describe('parseCplc', () => {
  it('names a J3R180 by fabricator, IC type, fabrication date, serial and batch', () => {
    const chip = parseCplc(J3R180_CPLC);
    assert.equal(chip.id, J3R180_ID);
    assert.equal(chip.icSerialNumber, '09336320');
    assert.match(chip.id, CHIP_ID_PATTERN);
  });

  it('ignores the OS and personalisation fields, which a reset may not keep', () => {
    const changed = J3R180_CPLC.slice();
    changed[7] ^= 1; // OS identifier
    changed[30] ^= 1; // personaliser fields
    assert.equal(parseCplc(changed).id, J3R180_ID);
    for (const offset of [3, 5, 13, 15, 19]) {
      const other = J3R180_CPLC.slice();
      other[offset] ^= 1;
      assert.notEqual(parseCplc(other).id, J3R180_ID, `byte ${offset} counts`);
    }
  });

  it('refuses a wrong tag, a wrong length and an unset serial', () => {
    const wrongTag = J3R180_CPLC.slice();
    wrongTag[1] = 0x7e;
    const wrongLength = J3R180_CPLC.slice();
    wrongLength[2] = 41;
    const zeros = J3R180_CPLC.slice();
    zeros.fill(0, 15, 19);
    const ones = J3R180_CPLC.slice();
    ones.fill(0xff, 15, 19);
    for (const bad of [
      [],
      J3R180_CPLC.slice(3),
      wrongTag,
      wrongLength,
      zeros,
      ones,
    ]) {
      assert.throws(() => parseCplc(bad), RangeError);
    }
  });
});

describe('readChipIdentity', () => {
  it('sends SELECT of the card manager and GET DATA 9F7F, nothing else', async () => {
    const sent: string[] = [];
    const tx: Transceiver = async apdu => {
      sent.push(toHex(apdu));
      return apdu[1] === 0xa4 ? [0x90, 0x00] : [...J3R180_CPLC, 0x90, 0x00];
    };
    assert.equal((await readChipIdentity(tx)).id, J3R180_ID);
    assert.deepEqual(sent, ['00A4040008A00000015100000000', '80CA9F7F00']);
  });

  it('reports a refusal as the card’s, a bad CPLC as a refusal, and a lost reply as transport', async () => {
    const refusing: Transceiver = async apdu =>
      apdu[1] === 0xa4 ? [0x90, 0x00] : [0x6a, 0x88];
    await assert.rejects(
      readChipIdentity(refusing),
      (e: unknown) => e instanceof CardError && e.sw === 0x6a88,
    );
    const garbled: Transceiver = async apdu =>
      apdu[1] === 0xa4 ? [0x90, 0x00] : [0x9f, 0x7f, 0x01, 0x00, 0x90, 0x00];
    await assert.rejects(readChipIdentity(garbled), CardError);
    const gone: Transceiver = async () => {
      throw new Error('tag lost');
    };
    await assert.rejects(readChipIdentity(gone), TransportError);
  });
});
