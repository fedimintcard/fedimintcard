/**
 * Fedimint's mint v1 note check, ported from `fedimint-tbs` and `fedimint-mint-common` at
 * v0.13.0-beta.1, so ops can refuse a note whose signature the federation did not make
 * (OPS-PLAN § 6, "how ops trusts a load").
 *
 * A note is `(nonce, signature)`. It verifies under the federation's aggregate key for its
 * amount tier when `e(H(nonce), pk) == e(signature, G2)`:
 *
 * - `nonce` is the 33-byte compressed secp256k1 key (`Nonce::to_message` hashes
 *   `PublicKey::serialize()`); a card's is `02 || slot key`.
 * - `signature` is a G1 point, 48 bytes compressed (zcash encoding, as `bls12_381` writes it);
 *   `pk` is a G2 point, 96 bytes compressed. Signatures in G1, keys in G2.
 * - `H` is not the standard hash-to-curve. `Message::from_bytes` seeds a ChaCha20 RNG with
 *   SHA3-256("TBS_BLS12-381_" || bytes) and draws `G1Projective::random` from it
 *   (`bls12_381` 0.8.0, `g1.rs`): 96 bytes as an Fp, a u32 whose low bit flips y, retry until
 *   x^3 + 4 is square, then clear the cofactor. Every step below follows that code, including
 *   how `Fp::from_u768` weighs the two halves of the 96 bytes.
 *
 * `test/tbs-vectors.json` holds vectors the Rust code produced (`host/sidecar/tests/tbs_vectors.rs`);
 * the registry tests check this port against them.
 */
import {bls12_381} from '@noble/curves/bls12-381.js';
import {sha3_256} from '@noble/hashes/sha3.js';

const Fp = bls12_381.fields.Fp;
const P = Fp.ORDER;
const G1 = bls12_381.G1.Point;
const G2 = bls12_381.G2.Point;
type G1Point = InstanceType<typeof G1>;
type G2Point = InstanceType<typeof G2>;

const HASH_TAG = new TextEncoder().encode('TBS_BLS12-381_');
/** (p + 1) / 4: p = 3 (mod 4), so this power is a square root when one exists (`Fp::sqrt`). */
const SQRT_EXP = (P + 1n) / 4n;
/** 1 - z for BLS12-381's z = -0xd201000000010000: `G1Projective::clear_cofactor`. */
const H_EFF = 0xd201000000010001n;
const B = 4n;

/* ------------------------------------------------------------------------- */
/* rand_chacha 0.3.1 ChaCha20Rng: key = seed, 64-bit block counter from 0,   */
/* stream 0; words are read out little-endian in keystream order.            */
/* ------------------------------------------------------------------------- */

function rotl(x: number, n: number): number {
  return ((x << n) | (x >>> (32 - n))) >>> 0;
}

function chachaBlock(key: Uint32Array, counter: number, out: Uint32Array): void {
  const s = new Uint32Array(16);
  s[0] = 0x61707865;
  s[1] = 0x3320646e;
  s[2] = 0x79622d32;
  s[3] = 0x6b206574;
  s.set(key, 4);
  // A 64-bit counter in words 12-13; a few blocks never carry into 13.
  s[12] = counter >>> 0;
  s[13] = 0;
  s[14] = 0;
  s[15] = 0;
  const x = Uint32Array.from(s);
  const qr = (a: number, b: number, c: number, d: number) => {
    x[a] = (x[a]! + x[b]!) >>> 0;
    x[d] = rotl(x[d]! ^ x[a]!, 16);
    x[c] = (x[c]! + x[d]!) >>> 0;
    x[b] = rotl(x[b]! ^ x[c]!, 12);
    x[a] = (x[a]! + x[b]!) >>> 0;
    x[d] = rotl(x[d]! ^ x[a]!, 8);
    x[c] = (x[c]! + x[d]!) >>> 0;
    x[b] = rotl(x[b]! ^ x[c]!, 7);
  };
  for (let i = 0; i < 10; i += 1) {
    qr(0, 4, 8, 12);
    qr(1, 5, 9, 13);
    qr(2, 6, 10, 14);
    qr(3, 7, 11, 15);
    qr(0, 5, 10, 15);
    qr(1, 6, 11, 12);
    qr(2, 7, 8, 13);
    qr(3, 4, 9, 14);
  }
  for (let i = 0; i < 16; i += 1) {
    out[i] = (x[i]! + s[i]!) >>> 0;
  }
}

class ChaChaRng {
  private readonly key = new Uint32Array(8);
  private readonly block = new Uint32Array(16);
  private counter = 0;
  private index = 16;

  constructor(seed: Uint8Array) {
    const view = new DataView(seed.buffer, seed.byteOffset, seed.byteLength);
    for (let i = 0; i < 8; i += 1) {
      this.key[i] = view.getUint32(i * 4, true);
    }
  }

  nextU32(): number {
    if (this.index === 16) {
      chachaBlock(this.key, this.counter, this.block);
      this.counter += 1;
      this.index = 0;
    }
    const w = this.block[this.index]!;
    this.index += 1;
    return w;
  }

  /** `fill_bytes` for a multiple of 4 bytes: whole words, little-endian. */
  fill(n: number): Uint8Array {
    const out = new Uint8Array(n);
    const view = new DataView(out.buffer);
    for (let i = 0; i < n; i += 4) {
      view.setUint32(i, this.nextU32(), true);
    }
    return out;
  }
}

/* ------------------------------------------------------------------------- */
/* Message::from_bytes                                                       */
/* ------------------------------------------------------------------------- */

function bigBE(bytes: Uint8Array): bigint {
  let n = 0n;
  for (const b of bytes) {
    n = (n << 8n) | BigInt(b);
  }
  return n;
}

/**
 * `Fp::random`: 96 bytes read as twelve big-endian u64 limbs, of which `from_u768` takes the
 * first 48 bytes at weight 1 and the last 48 at weight 2^384 (its `d0 * R2 + d1 * R3`).
 */
function fpRandom(rng: ChaChaRng): bigint {
  const bytes = rng.fill(96);
  return Fp.create(bigBE(bytes.subarray(0, 48)) + (bigBE(bytes.subarray(48, 96)) << 384n));
}

/** Plain double-and-add: the point is not in the subgroup yet, so no endomorphism shortcuts. */
function mulPlain(point: G1Point, k: bigint): G1Point {
  let acc = G1.ZERO;
  let add = point;
  while (k > 0n) {
    if (k & 1n) acc = acc.add(add);
    add = add.double();
    k >>= 1n;
  }
  return acc;
}

/** `G1Projective::random(ChaChaRng::from_seed(seed))`. */
function g1Random(rng: ChaChaRng): G1Point {
  for (;;) {
    const x = fpRandom(rng);
    const flip = rng.nextU32() % 2 !== 0;
    const rhs = Fp.add(Fp.mul(Fp.sqr(x), x), B);
    const y = Fp.pow(rhs, SQRT_EXP);
    if (!Fp.eql(Fp.sqr(y), rhs)) {
      continue;
    }
    const p = mulPlain(G1.fromAffine({x, y: flip ? Fp.neg(y) : y}), H_EFF);
    if (!p.is0()) {
      return p;
    }
  }
}

/** `tbs::Message::from_bytes`: the G1 point a note's nonce is signed as. */
export function messageOf(bytes: Uint8Array): G1Point {
  const seed = sha3_256(new Uint8Array([...HASH_TAG, ...bytes]));
  return g1Random(new ChaChaRng(seed));
}

/* ------------------------------------------------------------------------- */
/* Keys and notes                                                            */
/* ------------------------------------------------------------------------- */

/** A G2 public key from its 96-byte compressed hex, or null if it is not a point of the group. */
export function parsePublicKey(hex: string): G2Point | null {
  try {
    const p = G2.fromHex(hex);
    return p.is0() ? null : p;
  } catch {
    return null;
  }
}

function parseSignature(hex: string): G1Point | null {
  try {
    const p = G1.fromHex(hex);
    return p.is0() ? null : p;
  } catch {
    return null;
  }
}

function fromHex(hex: string): Uint8Array {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i += 1) {
    out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
}

/** A note to check: its nonce and signature (hex) and the aggregate key of its tier. */
export interface NoteCheck {
  readonly nonce: string;
  readonly signature: string;
  readonly pk: G2Point;
}

/**
 * `Note::verify` for every note at once: whether each `signature` (48 bytes, hex) is the
 * federation's signature on its `nonce` (33 bytes, hex) under its tier's key. Each note alone is
 * e(H, pk) == e(sig, G2); the batch weighs note i by a random 64-bit r_i and checks
 * prod e(r_i H_i, pk_i) * e(-sum r_i sig_i, G2) == 1, one Miller loop per note and one final
 * exponentiation. Random weights keep one note's error from cancelling another's.
 */
export function verifyNotes(notes: readonly NoteCheck[]): boolean {
  if (notes.length === 0) {
    return true;
  }
  const pairs: {g1: G1Point; g2: G2Point}[] = [];
  let sigSum = G1.ZERO;
  const weights = crypto.getRandomValues(new BigUint64Array(notes.length));
  for (const [i, n] of notes.entries()) {
    const sig = parseSignature(n.signature);
    if (!sig) {
      return false;
    }
    const r = weights[i]! | 1n;
    pairs.push({g1: messageOf(fromHex(n.nonce)).multiply(r), g2: n.pk});
    sigSum = sigSum.add(sig.multiply(r));
  }
  pairs.push({g1: sigSum.negate(), g2: G2.BASE});
  return bls12_381.fields.Fp12.eql(bls12_381.pairingBatch(pairs), bls12_381.fields.Fp12.ONE);
}

/** `Note::verify` for one note. */
export function verifyNote(nonce: string, signature: string, pk: G2Point): boolean {
  return verifyNotes([{nonce, signature, pk}]);
}
