import {checkHeaderAt, sha256d} from './header';
import {AHEAD_MAX_S, BEHIND_MAX_S, checkHeader, headerHashHex, targetOf} from './header';
import {H900000, T900000} from './testing/anchor';

const at = (t: number) => t * 1000;

/** A header of these bits at this time whose hash meets its own target; `bits` near 2^255 needs a few tries. */
function mined(bits: number, time: number): number[] {
  for (let nonce = 0; ; nonce++) {
    const h = new Array(80).fill(0);
    [68, 72, 76].forEach((o, i) => {
      const v = [time, bits, nonce][i];
      for (let k = 0; k < 4; k++) h[o + k] = (v >>> (8 * k)) & 0xff;
    });
    const r = checkHeader(h, at(time));
    if (!(r.ok === false && r.reason.includes('its own work'))) {
      return h;
    }
  }
}

test('block 900 000 passes, and its time is the header’s', () => {
  expect(checkHeader(H900000, at(T900000 + 60))).toEqual({ok: true, time: T900000});
  expect(headerHashHex(H900000)).toMatch(/^0{19}/);
});

test('the time window: at most 2 h ahead of the phone, at most 7 days behind', () => {
  expect(checkHeader(H900000, at(T900000 - AHEAD_MAX_S)).ok).toBe(true);
  expect(checkHeader(H900000, at(T900000 - AHEAD_MAX_S - 1))).toMatchObject({ok: false, reason: expect.stringMatching(/ahead/)});
  expect(checkHeader(H900000, at(T900000 + BEHIND_MAX_S)).ok).toBe(true);
  expect(checkHeader(H900000, at(T900000 + BEHIND_MAX_S + 1))).toMatchObject({ok: false, reason: expect.stringMatching(/behind/)});
});

test('80 bytes exactly', () => {
  expect(checkHeader(H900000.slice(0, 79), at(T900000)).ok).toBe(false);
  expect(checkHeader([...H900000, 0], at(T900000)).ok).toBe(false);
  expect(checkHeader([...H900000.slice(0, 79), 256], at(T900000)).ok).toBe(false);
});

test('its own proof of work: a changed nonce fails', () => {
  const h = [...H900000];
  h[76] ^= 0x01;
  expect(checkHeader(h, at(T900000))).toMatchObject({ok: false, reason: expect.stringMatching(/its own work/)});
});

test('weak against twice the floor: a header with its own work at an easy target is refused', () => {
  // Work a phone can mine: it meets its own target, and is far under twice the floor's.
  expect(checkHeader(mined(0x207fffff, T900000), at(T900000))).toMatchObject({ok: false, reason: expect.stringMatching(/twice/)});
});

test('targetOf: compact bits as a 32-byte big-endian target', () => {
  const t = targetOf(0x17023774)!;
  expect(t.findIndex(b => b !== 0)).toBe(9);
  expect(t.slice(9, 12)).toEqual([0x02, 0x37, 0x74]);
  expect(targetOf(0x1d00ffff)!.slice(4, 6)).toEqual([0xff, 0xff]);
  expect(targetOf(0x17800000)).toBeNull(); // negative
  expect(targetOf(0x17000000)).toBeNull(); // zero
  expect(targetOf(0x22010000)).toBeNull(); // over 256 bits
});

describe('the weak rule at its edge: exactly twice the floor’s work passes', () => {
  // A test floor whose half is a compact target: 0x207ffffe is 7f ff fe 00…,
  // its half 3f ff ff 00… is 0x203fffff.
  const FLOOR = 0x207ffffe;
  const NOW = 1_800_000_000;
  /** A header at `bits` and NOW, mined until it has its own work. */
  function mined(bits: number): number[] {
    const le = (v: number) => [v & 0xff, (v >>> 8) & 0xff, (v >>> 16) & 0xff, (v >>> 24) & 0xff];
    for (let nonce = 0; nonce < 1 << 16; nonce++) {
      const h = [...le(0x20000000), ...new Array(64).fill(7), ...le(NOW), ...le(bits), ...le(nonce)];
      const hash = sha256d(h).reverse();
      const target = targetOfForTest(bits);
      if (hash.join(',') === target.join(',') || lessThan(hash, target)) {
        return h;
      }
    }
    throw new Error('not mined');
  }
  const lessThan = (a: number[], b: number[]) => {
    for (let i = 0; i < a.length; i++) {
      if (a[i] !== b[i]) {
        return a[i] < b[i];
      }
    }
    return false;
  };
  const targetOfForTest = (bits: number) => {
    const out = new Array(32).fill(0);
    const e = bits >>> 24;
    out[32 - e] = (bits >>> 16) & 0xff;
    out[33 - e] = (bits >>> 8) & 0xff;
    out[34 - e] = bits & 0xff;
    return out;
  };
  const at = (bits: number) => checkHeaderAt(mined(bits), NOW * 1000, FLOOR);

  test('just under twice the work (a larger target) is refused', () => {
    expect(at(0x20400000)).toEqual({ok: false, reason: 'the block header has less than twice the least work a card takes'});
  });
  test('exactly twice the work passes', () => {
    expect(at(0x203fffff)).toEqual({ok: true, time: NOW});
  });
  test('just over twice the work passes', () => {
    expect(at(0x203ffffe)).toEqual({ok: true, time: NOW});
  });
  test('an odd floor: its half is not compact, and the target just under it passes', () => {
    // 0x207fffff halves to 3f ff ff 80…; 0x203fffff (3f ff ff 00…) is under it, 0x20400000 over.
    expect(checkHeaderAt(mined(0x203fffff), NOW * 1000, 0x207fffff).ok).toBe(true);
    expect(checkHeaderAt(mined(0x20400000), NOW * 1000, 0x207fffff).ok).toBe(false);
  });
});
