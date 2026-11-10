/**
 * AES-128, AES-CMAC and SCP03's KDF computed with node:crypto, sharing no
 * code with gp/scp03.ts: the reference the tests and the fake card hold
 * cardkit's SCP03 to, as node-des.ts is for SCP02. node:crypto has no CMAC,
 * so it is built here on OpenSSL's AES-128-CBC: the subkeys by BigInt
 * arithmetic, the MAC as the last block of one CBC pass from a zero IV.
 */
import {createCipheriv, createDecipheriv} from 'node:crypto';

function key16(key: ArrayLike<number>): Buffer {
  const k = Buffer.from(Array.from(key));
  if (k.length !== 16) {
    throw new RangeError('an AES-128 key of ' + k.length + ' bytes');
  }
  return k;
}

/** AES-128-ECB over whole blocks, no padding. */
export function nodeAesEcb(
  key: ArrayLike<number>,
  data: ArrayLike<number>,
  decrypt = false,
): number[] {
  const c = decrypt
    ? createDecipheriv('aes-128-ecb', key16(key), null)
    : createCipheriv('aes-128-ecb', key16(key), null);
  c.setAutoPadding(false);
  return Array.from(
    Buffer.concat([c.update(Buffer.from(Array.from(data))), c.final()]),
  );
}

const MASK = (1n << 128n) - 1n;

/** SP 800-38B's subkey step: the 128-bit value shifted left, reduced by 87 on overflow. */
function shift(v: bigint): bigint {
  return ((v << 1n) & MASK) ^ (v >> 127n ? 0x87n : 0n);
}

function toBigInt(b: Buffer): bigint {
  return BigInt('0x' + (b.toString('hex') || '0'));
}

function fromBigInt(v: bigint): Buffer {
  return Buffer.from(v.toString(16).padStart(32, '0'), 'hex');
}

/** AES-CMAC (NIST SP 800-38B), all 16 bytes. */
export function nodeCmac(
  key: ArrayLike<number>,
  data: ArrayLike<number>,
): number[] {
  const k = key16(key);
  const m = Buffer.from(Array.from(data));
  const l = toBigInt(Buffer.from(nodeAesEcb(k, Buffer.alloc(16))));
  const k1 = shift(l);
  const k2 = shift(k1);
  const complete = m.length > 0 && m.length % 16 === 0;
  const padded = complete
    ? Buffer.from(m)
    : Buffer.concat([
        m,
        Buffer.from([0x80]),
        Buffer.alloc(15 - (m.length % 16)),
      ]);
  const at = padded.length - 16;
  const last = toBigInt(padded.subarray(at)) ^ (complete ? k1 : k2);
  fromBigInt(last).copy(padded, at);
  const c = createCipheriv('aes-128-cbc', k, Buffer.alloc(16));
  c.setAutoPadding(false);
  const out = Buffer.concat([c.update(padded), c.final()]);
  return Array.from(out.subarray(out.length - 16));
}

/** Amendment D's KDF: CMAC over eleven zeros, the constant, 00, L, the counter 01 and the context, cut to L bits. */
export function nodeScp03Kdf(
  key: ArrayLike<number>,
  constant: number,
  lengthBits: number,
  context: ArrayLike<number>,
): number[] {
  const input = Buffer.concat([
    Buffer.alloc(11),
    Buffer.from([constant, 0x00, lengthBits >> 8, lengthBits & 0xff, 0x01]),
    Buffer.from(Array.from(context)),
  ]);
  return nodeCmac(key, input).slice(0, lengthBits / 8);
}
