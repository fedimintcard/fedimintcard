/**
 * The block header a set-up sends a 4.0 card as its anchor (DESIGN § 4,
 * "Header source: the registry"). The registry serves the tip
 * (`GET /v1/chain/tip`); the phone checks it itself before any card sees it:
 * 80 bytes, a hash at or under its own target, work at least twice the
 * floor's (the card refuses less as weak), and a time no more than 2 h ahead
 * of the phone and no more than 7 days behind it.
 *
 * Plain byte arithmetic, no BigInt: a target is 32 bytes, big-endian.
 */
import {sha256} from '@noble/hashes/sha2';

export const HEADER_LEN = 80;
/** The applet's floor (FedimintCardApplet.FLOOR_BITS), as the header's `bits` field reads. */
export const FLOOR_BITS = 0x17087bc0;
export const AHEAD_MAX_S = 2 * 60 * 60;
export const BEHIND_MAX_S = 7 * 24 * 60 * 60;

export type HeaderCheck =
  | {readonly ok: true; readonly time: number}
  | {readonly ok: false; readonly reason: string};

function le32(b: readonly number[], at: number): number {
  return (b[at] | (b[at + 1] << 8) | (b[at + 2] << 16) | (b[at + 3] << 24)) >>> 0;
}

/** Double SHA-256 of the header, as Bitcoin hashes one. */
export function sha256d(bytes: readonly number[]): number[] {
  return Array.from(sha256(sha256(Uint8Array.from(bytes))));
}

/** The block hash in display order (big-endian hex), as explorers and the registry write it. */
export function headerHashHex(header: readonly number[]): string {
  return sha256d(header)
    .reverse()
    .map(b => b.toString(16).padStart(2, '0'))
    .join('');
}

/** The 32-byte big-endian target of a compact `bits`; null for a negative, zero or overflowing one. */
export function targetOf(bits: number): number[] | null {
  const exponent = bits >>> 24;
  const mantissa = bits & 0x007fffff;
  if ((bits & 0x00800000) !== 0 || mantissa === 0) {
    return null;
  }
  const out: number[] = new Array(32).fill(0);
  const m = [(mantissa >> 16) & 0xff, (mantissa >> 8) & 0xff, mantissa & 0xff];
  for (let i = 0; i < 3; i++) {
    const at = 32 - exponent + i;
    if (at < 0) {
      if (m[i] !== 0) {
        return null;
      }
    } else if (at < 32) {
      out[at] = m[i];
    }
  }
  return out.every(b => b === 0) ? null : out;
}

function cmp(a: readonly number[], b: readonly number[]): number {
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) {
      return a[i] < b[i] ? -1 : 1;
    }
  }
  return 0;
}

/** Half a target, rounded down: the target of twice its work. */
function half(t: readonly number[]): number[] {
  const out: number[] = new Array(t.length).fill(0);
  let carry = 0;
  for (let i = 0; i < t.length; i++) {
    out[i] = (t[i] >> 1) | carry;
    carry = (t[i] & 1) << 7;
  }
  return out;
}


/**
 * The phone's checks on a header before it is sent as an anchor. `nowMs` is
 * the phone's clock. On success, the header's time, which the card must then
 * report as its clock.
 */
export function checkHeader(header: readonly number[], nowMs: number): HeaderCheck {
  return checkHeaderAt(header, nowMs, FLOOR_BITS);
}

/**
 * `checkHeader` against any floor, given as compact bits (the card's
 * `header_is_weak_at`): a header whose target is more than half the floor's,
 * less than twice the floor's work, is refused; exactly twice passes.
 */
export function checkHeaderAt(header: readonly number[], nowMs: number, floorBits: number): HeaderCheck {
  const floor = targetOf(floorBits);
  if (floor === null) {
    throw new Error('checkHeaderAt: the floor has no valid target');
  }
  if (header.length !== HEADER_LEN || header.some(b => !Number.isInteger(b) || b < 0 || b > 255)) {
    return {ok: false, reason: `the block header is ${header.length} bytes, not ${HEADER_LEN}`};
  }
  const target = targetOf(le32(header, 72));
  if (target === null) {
    return {ok: false, reason: 'the block header has no valid target'};
  }
  if (cmp(sha256d(header).reverse(), target) > 0) {
    return {ok: false, reason: 'the block header does not have its own work'};
  }
  if (cmp(target, half(floor)) > 0) {
    return {ok: false, reason: 'the block header has less than twice the least work a card takes'};
  }
  const time = le32(header, 68);
  const now = Math.floor(nowMs / 1000);
  if (time > now + AHEAD_MAX_S) {
    return {ok: false, reason: 'the block header is more than 2 hours ahead of this phone’s clock'};
  }
  if (time < now - BEHIND_MAX_S) {
    return {ok: false, reason: 'the block header is more than 7 days behind this phone’s clock'};
  }
  return {ok: true, time};
}
