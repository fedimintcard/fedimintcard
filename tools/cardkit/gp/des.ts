/**
 * DES and triple DES, table-driven, with no dependency: the cipher SCP02 runs
 * on (D27). Plain TypeScript, so it runs the same under Node and on the phone.
 *
 * The tables below are FIPS 46-3's, with bit 1 the most significant bit of the
 * first byte. The round function's table (an S-box followed by P, for each of
 * the eight boxes) and the byte tables of the initial and final permutations
 * are worked out from them once, when the module loads. The tests hold this
 * file to FIPS 81's vectors and to node:crypto on random keys and data.
 *
 * Not constant-time: the round function looks tables up by key-dependent
 * indices, as every table-driven DES does. The keys it handles are the card
 * manager's, on the phone that holds them.
 */
import {type Bytes, bytes, xor} from './bytes';

// prettier-ignore
const IP = [
  58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4,
  62, 54, 46, 38, 30, 22, 14, 6, 64, 56, 48, 40, 32, 24, 16, 8,
  57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3,
  61, 53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];

// prettier-ignore
const P = [
  16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10,
  2, 8, 24, 14, 32, 27, 3, 9, 19, 13, 30, 6, 22, 11, 4, 25,
];

// prettier-ignore
const PC1 = [
  57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18,
  10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60, 52, 44, 36,
  63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22,
  14, 6, 61, 53, 45, 37, 29, 21, 13, 5, 28, 20, 12, 4,
];

// prettier-ignore
const PC2 = [
  14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10,
  23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2,
  41, 52, 31, 37, 47, 55, 30, 40, 51, 45, 33, 48,
  44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];

const SHIFTS = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];

// The eight S-boxes, each four rows of sixteen.
// prettier-ignore
const S = [
  [
    14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7,
    0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12, 11, 9, 5, 3, 8,
    4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0,
    15, 12, 8, 2, 4, 9, 1, 7, 5, 11, 3, 14, 10, 0, 6, 13,
  ],
  [
    15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10,
    3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1, 10, 6, 9, 11, 5,
    0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15,
    13, 8, 10, 1, 3, 15, 4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
  ],
  [
    10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8,
    13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5, 14, 12, 11, 15, 1,
    13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7,
    1, 10, 13, 0, 6, 9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12,
  ],
  [
    7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15,
    13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2, 12, 1, 10, 14, 9,
    10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4,
    3, 15, 0, 6, 10, 1, 13, 8, 9, 4, 5, 11, 12, 7, 2, 14,
  ],
  [
    2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9,
    14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15, 10, 3, 9, 8, 6,
    4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14,
    11, 8, 12, 7, 1, 14, 2, 13, 6, 15, 0, 9, 10, 4, 5, 3,
  ],
  [
    12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11,
    10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13, 14, 0, 11, 3, 8,
    9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6,
    4, 3, 2, 12, 9, 5, 15, 10, 11, 14, 1, 7, 6, 0, 8, 13,
  ],
  [
    4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1,
    13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5, 12, 2, 15, 8, 6,
    1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2,
    6, 11, 13, 8, 1, 4, 10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
  ],
  [
    13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7,
    1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6, 11, 0, 14, 9, 2,
    7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8,
    2, 1, 14, 7, 4, 10, 8, 13, 15, 12, 9, 0, 3, 5, 6, 11,
  ],
];

/** The inverse of a permutation of 1..n given 1-based. */
function inverse(perm: number[]): number[] {
  const out = new Array<number>(perm.length);
  perm.forEach((from, to) => {
    out[from - 1] = to + 1;
  });
  return out;
}

const FP = inverse(IP);

/**
 * For a 64-bit permutation, the image of each byte value at each byte
 * position, as two 32-bit words: entry (pos * 256 + value) * 2 is the high
 * word, the next the low. A block's image is the OR of its eight bytes'.
 */
function byteTables(perm: number[]): Uint32Array {
  const t = new Uint32Array(8 * 256 * 2);
  for (let out = 0; out < 64; out++) {
    const from = perm[out] - 1;
    const pos = from >> 3;
    const mask = 0x80 >> (from & 7);
    for (let v = 0; v < 256; v++) {
      if (v & mask) {
        const at = (pos * 256 + v) * 2;
        if (out < 32) {
          t[at] |= 1 << (31 - out);
        } else {
          t[at + 1] |= 1 << (63 - out);
        }
      }
    }
  }
  return t;
}

const IP_TABLE = byteTables(IP);
const FP_TABLE = byteTables(FP);

/**
 * The round function's table: for box i and six input bits x, P applied to
 * the box's four output bits in their place among the 32.
 */
const SP = (() => {
  const t = new Uint32Array(8 * 64);
  for (let i = 0; i < 8; i++) {
    for (let x = 0; x < 64; x++) {
      const row = ((x >> 4) & 2) | (x & 1);
      const col = (x >> 1) & 0xf;
      const s = S[i][row * 16 + col] << (28 - 4 * i);
      let out = 0;
      for (let j = 0; j < 32; j++) {
        if ((s >>> (32 - P[j])) & 1) {
          out |= 1 << (31 - j);
        }
      }
      t[i * 64 + x] = out >>> 0;
    }
  }
  return t;
})();

function permute(table: Uint32Array, block: number[]): [number, number] {
  let hi = 0;
  let lo = 0;
  for (let i = 0; i < 8; i++) {
    const at = (i * 256 + block[i]) * 2;
    hi |= table[at];
    lo |= table[at + 1];
  }
  return [hi >>> 0, lo >>> 0];
}

function words(hi: number, lo: number): number[] {
  return [
    hi >>> 24,
    (hi >>> 16) & 0xff,
    (hi >>> 8) & 0xff,
    hi & 0xff,
    lo >>> 24,
    (lo >>> 16) & 0xff,
    (lo >>> 8) & 0xff,
    lo & 0xff,
  ];
}

/** The sixteen round keys, each as eight six-bit values. */
function schedule(key: number[]): Uint8Array {
  const bits = new Array<number>(64);
  for (let i = 0; i < 64; i++) {
    bits[i] = (key[i >> 3] >> (7 - (i & 7))) & 1;
  }
  let c = PC1.slice(0, 28).map(p => bits[p - 1]);
  let d = PC1.slice(28).map(p => bits[p - 1]);
  const ks = new Uint8Array(16 * 8);
  for (let r = 0; r < 16; r++) {
    const n = SHIFTS[r];
    c = c.slice(n).concat(c.slice(0, n));
    d = d.slice(n).concat(d.slice(0, n));
    const cd = c.concat(d);
    for (let i = 0; i < 8; i++) {
      let six = 0;
      for (let j = 0; j < 6; j++) {
        six = (six << 1) | cd[PC2[i * 6 + j] - 1];
      }
      ks[r * 8 + i] = six;
    }
  }
  return ks;
}

/** The round function: E, the round key, the S-boxes and P. */
function f(r: number, ks: Uint8Array, round: number): number {
  const k = round * 8;
  // Box 0 takes bits 32, 1, 2, 3, 4, 5: the top six of R rotated right by one.
  let out = SP[((((r >>> 1) | (r << 31)) >>> 26) & 0x3f) ^ ks[k]];
  for (let i = 1; i < 8; i++) {
    // Box i takes bits 4i to 4i + 5: the top six of R rotated left by 4i - 1.
    const n = 4 * i - 1;
    const six = (((r << n) | (r >>> (32 - n))) >>> 26) & 0x3f;
    out |= SP[i * 64 + (six ^ ks[k + i])];
  }
  return out >>> 0;
}

/** Single DES under one 8-byte key (parity bits are ignored, as DES does). */
export class DES {
  private readonly ks: Uint8Array;

  constructor(key: Bytes) {
    if (key.length !== 8) {
      throw new RangeError('a DES key is 8 bytes, not ' + key.length);
    }
    this.ks = schedule(bytes(key));
  }

  encryptBlock(block: Bytes): number[] {
    return this.crypt(block, false);
  }

  decryptBlock(block: Bytes): number[] {
    return this.crypt(block, true);
  }

  private crypt(block: Bytes, decrypt: boolean): number[] {
    if (block.length !== 8) {
      throw new RangeError('a DES block is 8 bytes, not ' + block.length);
    }
    let [l, r] = permute(IP_TABLE, bytes(block));
    for (let round = 0; round < 16; round++) {
      const next = (l ^ f(r, this.ks, decrypt ? 15 - round : round)) >>> 0;
      l = r;
      r = next;
    }
    const [hi, lo] = permute(FP_TABLE, words(r, l));
    return words(hi, lo);
  }
}

function blocksOf(data: Bytes): number[][] {
  if (data.length % 8 !== 0) {
    throw new RangeError(
      'data is not a whole number of 8-byte blocks: ' + data.length,
    );
  }
  const all = bytes(data);
  const out: number[][] = [];
  for (let i = 0; i < all.length; i += 8) {
    out.push(all.slice(i, i + 8));
  }
  return out;
}

/** The three DES keys of a 16-byte (K1 K2 K1) or 24-byte (K1 K2 K3) key. */
function tripleKeys(key: Bytes): [DES, DES, DES] {
  const k = bytes(key);
  if (k.length === 16) {
    const k1 = new DES(k.slice(0, 8));
    return [k1, new DES(k.slice(8, 16)), k1];
  }
  if (k.length === 24) {
    return [
      new DES(k.slice(0, 8)),
      new DES(k.slice(8, 16)),
      new DES(k.slice(16, 24)),
    ];
  }
  throw new RangeError('a triple-DES key is 16 or 24 bytes, not ' + k.length);
}

function ede(
  keys: [DES, DES, DES],
  block: number[],
  decrypt: boolean,
): number[] {
  const [k1, k2, k3] = keys;
  return decrypt
    ? k1.decryptBlock(k2.encryptBlock(k3.decryptBlock(block)))
    : k3.encryptBlock(k2.decryptBlock(k1.encryptBlock(block)));
}

/** Single DES in ECB mode. */
export function desEcb(key: Bytes, data: Bytes, decrypt = false): number[] {
  const des = new DES(key);
  const out: number[] = [];
  for (const b of blocksOf(data)) {
    out.push(...(decrypt ? des.decryptBlock(b) : des.encryptBlock(b)));
  }
  return out;
}

/** Triple DES (EDE) in ECB mode, under a 16- or 24-byte key. */
export function tdesEcb(key: Bytes, data: Bytes, decrypt = false): number[] {
  const keys = tripleKeys(key);
  const out: number[] = [];
  for (const b of blocksOf(data)) {
    out.push(...ede(keys, b, decrypt));
  }
  return out;
}

const ZERO8 = [0, 0, 0, 0, 0, 0, 0, 0];

/** Triple DES (EDE) in CBC mode, under a 16- or 24-byte key, from `iv` (zeros by default). */
export function tdesCbc(
  key: Bytes,
  data: Bytes,
  iv: Bytes = ZERO8,
  decrypt = false,
): number[] {
  if (iv.length !== 8) {
    throw new RangeError('a CBC IV is 8 bytes, not ' + iv.length);
  }
  const keys = tripleKeys(key);
  let chain = bytes(iv);
  const out: number[] = [];
  for (const b of blocksOf(data)) {
    if (decrypt) {
      out.push(...xor(ede(keys, b, true), chain));
      chain = b;
    } else {
      chain = ede(keys, xor(b, chain), false);
      out.push(...chain);
    }
  }
  return out;
}

/** ISO/IEC 9797-1 padding method 2: 80, then zeros to a multiple of 8. Always adds a byte. */
export function pad80(data: Bytes): number[] {
  const out = bytes(data);
  out.push(0x80);
  while (out.length % 8 !== 0) {
    out.push(0);
  }
  return out;
}

/**
 * The retail MAC: ISO/IEC 9797-1 MAC algorithm 3 with padding method 2 and no
 * truncation. Single DES under the key's first half chains every block but
 * the last; the last goes through K1 K2 K1. SCP02's C-MAC.
 */
export function retailMac(
  key16: Bytes,
  data: Bytes,
  icv: Bytes = ZERO8,
): number[] {
  if (key16.length !== 16) {
    throw new RangeError(
      'the retail MAC takes a 16-byte key, not ' + key16.length,
    );
  }
  if (icv.length !== 8) {
    throw new RangeError('an ICV is 8 bytes, not ' + icv.length);
  }
  const k = bytes(key16);
  const blocks = blocksOf(pad80(data));
  const k1 = new DES(k.slice(0, 8));
  let chain = bytes(icv);
  for (let i = 0; i < blocks.length - 1; i++) {
    chain = k1.encryptBlock(xor(blocks[i], chain));
  }
  return ede(tripleKeys(k), xor(blocks[blocks.length - 1], chain), false);
}

/** The full triple-DES CBC MAC over padded data (SCP02's card and host cryptograms). */
export function tdesCbcMac(
  key16: Bytes,
  data: Bytes,
  icv: Bytes = ZERO8,
): number[] {
  return tdesCbc(key16, pad80(data), icv).slice(-8);
}
