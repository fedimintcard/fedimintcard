/**
 * The vendored DES against published vectors (FIPS 81, the classic worked
 * example, NIST SP 800-67's TDEA example) and against node:crypto, which shares
 * no code with it, on random keys and data.
 */
import assert from 'node:assert/strict';
import {randomBytes} from 'node:crypto';
import {describe, it} from 'node:test';

import {fromHex, toHex} from '../bytes';
import {
  DES,
  desEcb,
  pad80,
  retailMac,
  tdesCbc,
  tdesCbcMac,
  tdesEcb,
} from '../des';
import {
  nodeCbc,
  nodeEcb,
  nodeFullMac,
  nodeRetailMac,
} from './node-des';

const NOW_IS_THE_TIME = fromHex(
  '4E6F772069732074 68652074696D6520 666F7220616C6C20',
);

function rnd(n: number): number[] {
  return Array.from(randomBytes(n));
}

describe('DES: published vectors', () => {
  it('FIPS 81 Table B1: ECB under 0123456789ABCDEF', () => {
    const key = fromHex('0123456789ABCDEF');
    assert.equal(
      toHex(desEcb(key, NOW_IS_THE_TIME)),
      '3FA40E8A984D48156A271787AB8883F9893D51EC4B563B53',
    );
    assert.equal(
      toHex(
        desEcb(
          key,
          fromHex('3FA40E8A984D48156A271787AB8883F9893D51EC4B563B53'),
          true,
        ),
      ),
      toHex(NOW_IS_THE_TIME),
    );
  });

  it('FIPS 81 Table C1: CBC under 0123456789ABCDEF from IV 1234567890ABCDEF', () => {
    const key = fromHex('0123456789ABCDEF');
    const k3 = [...key, ...key, ...key];
    const iv = fromHex('1234567890ABCDEF');
    const ct = 'E5C7CDDE872BF27C43E934008C389C0F683788499A7C05F6';
    assert.equal(toHex(tdesCbc(k3, NOW_IS_THE_TIME, iv)), ct);
    assert.equal(
      toHex(tdesCbc(k3, fromHex(ct), iv, true)),
      toHex(NOW_IS_THE_TIME),
    );
  });

  it('the classic worked example: 133457799BBCDFF1 encrypts 0123456789ABCDEF to 85E813540F0AB405', () => {
    const des = new DES(fromHex('133457799BBCDFF1'));
    assert.equal(
      toHex(des.encryptBlock(fromHex('0123456789ABCDEF'))),
      '85E813540F0AB405',
    );
    assert.equal(
      toHex(des.decryptBlock(fromHex('85E813540F0AB405'))),
      '0123456789ABCDEF',
    );
  });

  it('NIST SP 800-67 TDEA example (three keys, "The qufck brown fox jump")', () => {
    const key = fromHex('0123456789ABCDEF 23456789ABCDEF01 456789ABCDEF0123');
    const pt = fromHex('5468652071756663 6B2062726F776E20 666F78206A756D70');
    const ct = 'A826FD8CE53B855FCCE21C8112256FE668D5C05DD9B6B900';
    assert.equal(toHex(tdesEcb(key, pt)), ct);
    assert.equal(toHex(tdesEcb(key, fromHex(ct), true)), toHex(pt));
  });
});

describe('DES: differential against node:crypto des-ede3', () => {
  it('single DES, both ways, 300 random keys and blocks', () => {
    for (let i = 0; i < 300; i++) {
      const key = rnd(8);
      const block = rnd(8);
      const des = new DES(key);
      const ct = des.encryptBlock(block);
      assert.deepEqual(
        ct,
        nodeEcb(key, block),
        `key ${toHex(key)} block ${toHex(block)}`,
      );
      assert.deepEqual(des.decryptBlock(ct), block);
      assert.deepEqual(desEcb(key, block), ct);
    }
  });

  it('two- and three-key triple DES in ECB, both ways', () => {
    for (let i = 0; i < 100; i++) {
      for (const n of [16, 24]) {
        const key = rnd(n);
        const data = rnd(8 * (1 + (i % 5)));
        const ct = tdesEcb(key, data);
        assert.deepEqual(ct, nodeEcb(key, data));
        assert.deepEqual(tdesEcb(key, ct, true), data);
        assert.deepEqual(tdesEcb(key, ct, true), nodeEcb(key, ct, true));
      }
    }
  });

  it('triple DES in CBC from a random IV, both ways', () => {
    for (let i = 0; i < 100; i++) {
      const key = rnd(16);
      const iv = rnd(8);
      const data = rnd(8 * (1 + (i % 7)));
      const ct = tdesCbc(key, data, iv);
      assert.deepEqual(ct, nodeCbc(key, data, iv));
      assert.deepEqual(tdesCbc(key, ct, iv, true), data);
    }
  });

  it('the retail MAC (ISO 9797-1 algorithm 3, padding method 2), from zero and random ICVs', () => {
    for (let i = 0; i < 200; i++) {
      const key = rnd(16);
      const data = rnd(i % 70);
      const icv = i % 2 ? rnd(8) : new Array(8).fill(0);
      assert.deepEqual(
        retailMac(key, data, icv),
        nodeRetailMac(key, data, icv),
        `length ${data.length}`,
      );
    }
  });

  it('the full triple-DES CBC MAC of SCP02 cryptograms', () => {
    for (let i = 0; i < 100; i++) {
      const key = rnd(16);
      const data = rnd(i % 40);
      assert.deepEqual(tdesCbcMac(key, data), nodeFullMac(key, data));
    }
  });
});

describe('DES: shape and inputs', () => {
  it('pads with 80 then zeros, always adding at least one byte', () => {
    assert.equal(toHex(pad80([])), '8000000000000000');
    assert.equal(toHex(pad80([1, 2, 3, 4, 5, 6, 7])), '0102030405060780');
    assert.equal(
      toHex(pad80(new Array(8).fill(0xaa))),
      'AAAAAAAAAAAAAAAA8000000000000000',
    );
  });

  it('never writes to its arguments', () => {
    const key = rnd(16);
    const data = rnd(24);
    const icv = rnd(8);
    const [k0, d0, i0] = [toHex(key), toHex(data), toHex(icv)];
    tdesEcb(key, data);
    tdesCbc(key, data, icv);
    tdesCbc(key, data, icv, true);
    retailMac(key, data, icv);
    tdesCbcMac(key, data, icv);
    pad80(data);
    new DES(key.slice(0, 8)).encryptBlock(data.slice(0, 8));
    assert.deepEqual([toHex(key), toHex(data), toHex(icv)], [k0, d0, i0]);
  });

  it('refuses keys and data of the wrong length', () => {
    assert.throws(() => new DES(rnd(7)), RangeError);
    assert.throws(() => new DES(rnd(16)), RangeError);
    assert.throws(() => tdesEcb(rnd(8), rnd(8)), RangeError);
    assert.throws(() => tdesEcb(rnd(16), rnd(9)), RangeError);
    assert.throws(() => tdesCbc(rnd(16), rnd(8), rnd(7)), RangeError);
    assert.throws(() => retailMac(rnd(24), rnd(8)), RangeError);
    assert.throws(() => new DES(rnd(8)).encryptBlock(rnd(7)), RangeError);
  });
});
