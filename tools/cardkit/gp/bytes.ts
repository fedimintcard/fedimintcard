/**
 * Byte helpers. Every function here returns a new array and leaves its
 * arguments as they were: a command or a key handed to this module is never
 * written to.
 */

/** Bytes as the cardkit passes them: any array-like of 0..255. */
export type Bytes = ArrayLike<number>;

/** A copy of `b` as a plain array, each value checked to be a byte. */
export function bytes(b: Bytes): number[] {
  const out = new Array<number>(b.length);
  for (let i = 0; i < b.length; i++) {
    const v = b[i];
    if (!Number.isInteger(v) || v < 0 || v > 0xff) {
      throw new RangeError(`not a byte at ${i}: ${v}`);
    }
    out[i] = v;
  }
  return out;
}

/** The parts, end to end, as a new array. */
export function concat(...parts: Bytes[]): number[] {
  const out: number[] = [];
  for (const p of parts) {
    for (let i = 0; i < p.length; i++) {
      out.push(p[i]);
    }
  }
  return out;
}

/** Upper-case hex, no separators. */
export function toHex(b: Bytes): string {
  let s = '';
  for (let i = 0; i < b.length; i++) {
    s += (b[i] & 0xff).toString(16).padStart(2, '0');
  }
  return s.toUpperCase();
}

/** Hex (spaces allowed) to bytes; throws on anything else. */
export function fromHex(hex: string): number[] {
  const clean = hex.replace(/\s+/g, '');
  if (clean.length % 2 !== 0 || /[^0-9a-fA-F]/.test(clean)) {
    throw new RangeError('not hex');
  }
  const out = new Array<number>(clean.length / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
}

/** True when a and b hold the same bytes. */
export function equal(a: Bytes, b: Bytes): boolean {
  if (a.length !== b.length) {
    return false;
  }
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) {
      return false;
    }
  }
  return true;
}

/** Equal in time that does not depend on where the first difference is. */
export function equalConstantTime(a: Bytes, b: Bytes): boolean {
  if (a.length !== b.length) {
    return false;
  }
  let diff = 0;
  for (let i = 0; i < a.length; i++) {
    diff |= a[i] ^ b[i];
  }
  return diff === 0;
}

/** a XOR b, which must be the same length. */
export function xor(a: Bytes, b: Bytes): number[] {
  if (a.length !== b.length) {
    throw new RangeError('xor of different lengths');
  }
  const out = new Array<number>(a.length);
  for (let i = 0; i < a.length; i++) {
    out[i] = (a[i] ^ b[i]) & 0xff;
  }
  return out;
}

/** A status word as four hex digits, e.g. `6A88`. */
export function swHex(sw: number): string {
  return sw.toString(16).toUpperCase().padStart(4, '0');
}
