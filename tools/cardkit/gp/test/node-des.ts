/**
 * DES, triple DES and the retail MAC computed with node:crypto, which shares
 * no code with gp/des.ts: the reference the tests and the fake card hold the
 * vendored cipher to. OpenSSL 3 keeps single DES (`des-ecb`) in its legacy
 * provider, so single DES is `des-ede3` under K‖K‖K, which is E_K(D_K(E_K(x)))
 * = E_K(x).
 */
import {createCipheriv, createDecipheriv} from 'node:crypto';

function run(
  alg: string,
  key: Buffer,
  iv: Buffer | null,
  data: Buffer,
  decrypt: boolean,
): Buffer {
  const c = decrypt
    ? createDecipheriv(alg, key, iv)
    : createCipheriv(alg, key, iv);
  c.setAutoPadding(false);
  return Buffer.concat([c.update(data), c.final()]);
}

function key24(key: ArrayLike<number>): Buffer {
  const k = Buffer.from(Array.from(key));
  if (k.length === 8) {
    return Buffer.concat([k, k, k]);
  }
  if (k.length === 16) {
    return Buffer.concat([k, k.subarray(0, 8)]);
  }
  if (k.length === 24) {
    return k;
  }
  throw new RangeError('key of ' + k.length + ' bytes');
}

/** ECB under an 8 (single DES), 16 (K1 K2 K1) or 24-byte key. */
export function nodeEcb(
  key: ArrayLike<number>,
  data: ArrayLike<number>,
  decrypt = false,
): number[] {
  return Array.from(
    run(
      'des-ede3-ecb',
      key24(key),
      null,
      Buffer.from(Array.from(data)),
      decrypt,
    ),
  );
}

/** CBC under an 8, 16 or 24-byte key. */
export function nodeCbc(
  key: ArrayLike<number>,
  data: ArrayLike<number>,
  iv: ArrayLike<number> = new Array(8).fill(0),
  decrypt = false,
): number[] {
  return Array.from(
    run(
      'des-ede3-cbc',
      key24(key),
      Buffer.from(Array.from(iv)),
      Buffer.from(Array.from(data)),
      decrypt,
    ),
  );
}

/** ISO 9797-1 padding method 2: 80, then zeros to a multiple of 8. */
export function nodePad(data: ArrayLike<number>): number[] {
  const out = Array.from(data);
  out.push(0x80);
  while (out.length % 8 !== 0) {
    out.push(0);
  }
  return out;
}

/**
 * ISO 9797-1 MAC algorithm 3 (the retail MAC) with padding method 2: single
 * DES CBC under K1 over every block but the last, then the last block in CBC
 * under K1 K2 K1 from there.
 */
export function nodeRetailMac(
  key16: ArrayLike<number>,
  data: ArrayLike<number>,
  icv: ArrayLike<number> = new Array(8).fill(0),
): number[] {
  const k = Array.from(key16);
  const padded = nodePad(data);
  let chain = Array.from(icv);
  if (padded.length > 8) {
    const head = nodeCbc(k.slice(0, 8), padded.slice(0, -8), chain);
    chain = head.slice(-8);
  }
  return nodeCbc(k, padded.slice(-8), chain);
}

/** Full triple-DES CBC MAC (SCP02's cryptograms): the last block of CBC over the padded data. */
export function nodeFullMac(
  key16: ArrayLike<number>,
  data: ArrayLike<number>,
  icv: ArrayLike<number> = new Array(8).fill(0),
): number[] {
  return nodeCbc(key16, nodePad(data), icv).slice(-8);
}
