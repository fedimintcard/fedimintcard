/**
 * The sealed PIN, the host's side (docs/OFFLINE-PROTOCOL.md § 9; applet 4.0).
 * Every PIN command to a card whose
 * SELECT answered major 4 is sealed to the card's PIN key under a fresh card
 * nonce, and only after that key has been checked.
 *
 * This is Cashu Card 1.9's sealed PIN (cashucard, MIT: its applet's `unseal`,
 * `sealBlock` and `unblockPin`, and `tools/cardctl` `seal_block`), with our
 * labels and with the PIN key bound by the card's attestation key, which the
 * registry's certificate (§ 2) already certifies. Every primitive is one that
 * construction uses: secp256k1 ECDH (the X coordinate), SHA-256 blocks for the
 * keystream and the tag, and ECDSA P-256 with SHA-256 for the binding and the
 * certificate. The vectors every implementation passes are
 * `spec/vectors/sealed-pin.json` (`sealedPin.test.ts` runs them here).
 *
 * The type rule: an envelope is built only for a `CheckedPinKey`, which only
 * `CheckedPinKey.ask` makes, and only from a `TrustedCard`, which only
 * `TrustedCard.check` makes, from a certificate that verified under the
 * registry's certificate key and names the card's own slot 0. No PIN, key,
 * nonce or shared secret goes into a reason text.
 */
import {secp256k1} from '@noble/curves/secp256k1';
import {p256} from '@noble/curves/nist';
import {sha256} from '@noble/hashes/sha2';
import {secureRandomBytes} from './secureRandom';

const ascii = (s: string): Uint8Array => Uint8Array.from(s, c => c.charCodeAt(0));

/** The seal's label (18 ASCII). */
export const SEAL_LABEL = ascii('fedimintcard/seal1');
/** The binding's label (17 ASCII): the attestation key signs it ‖ the PIN key. */
export const BINDING_LABEL = ascii('fedimintcard/pin1');
/** The certificate's label (§ 2, `host/card/src/v3.rs` CERT_LABEL). */
export const CERT_LABEL = ascii('fedimintcard/cert1');

const CLA = 0xb0;
export const INS_GET_NONCE = 0x44;
export const INS_VERIFY_PIN = 0x40;
export const INS_SET_PIN = 0x41;
export const INS_CHANGE_PIN = 0x42;
const INS_GET_PUB = 0x11;
const INS_GET_CERT = 0x32;
/** P1 of every sealed PIN command and of GET_NONCE. */
export const P1_SEALED = 0x01;

export const NONCE_LEN = 16;
export const PIN_KEY_LEN = 33;
export const POINT_LEN = 65;
export const TAG_LEN = 16;
export const PIN_BLOCK_LEN = 9;
export const BINDING_MIN = 8;
export const BINDING_MAX = 72;
/** The certificate's length (§ 2): 01 ‖ slot 0 (32) ‖ attestation key, compressed (33) ‖ r ‖ s (64). */
export const CERT_LEN = 130;

type Bytes = readonly number[] | Uint8Array;
type Send = (apdu: readonly number[]) => Promise<{body: number[]; sw: number | null}>;

const SW_OK = 0x9000;

function concat(...parts: Bytes[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

function same(a: Bytes, b: Bytes): boolean {
  if (a.length !== b.length) {
    return false;
  }
  let d = 0;
  for (let i = 0; i < a.length; i++) {
    d |= a[i] ^ b[i];
  }
  return d === 0;
}

/** Zeroes what held a PIN, a key or a secret. */
export function wipe(...arrays: Uint8Array[]): void {
  for (const a of arrays) {
    a.fill(0);
  }
}

/** The PIN block: the PIN's length (1, 4 to 8) ‖ its ASCII digits ‖ zeros to 8 bytes; always 9 bytes. */
export function pinBlock(pin: string): Uint8Array {
  if (!/^[0-9]{4,8}$/.test(pin)) {
    throw new Error('pinBlock: the PIN must be 4 to 8 digits');
  }
  const block = new Uint8Array(PIN_BLOCK_LEN);
  block[0] = pin.length;
  for (let i = 0; i < pin.length; i++) {
    block[1 + i] = pin.charCodeAt(i);
  }
  return block;
}

/** block(i, m) = SHA-256(label ‖ i ‖ x ‖ E ‖ nonce ‖ INS ‖ m). */
export function sealBlock(
  i: number,
  x: Bytes,
  point: Bytes,
  nonce: Bytes,
  ins: number,
  m: Bytes,
  label: Bytes = SEAL_LABEL,
): Uint8Array {
  return sha256(concat(label, [i], x, point, nonce, [ins], m));
}

/** XORs `data` in place with block(1, ""), block(2, ""), … */
function keystream(data: Uint8Array, x: Bytes, point: Bytes, nonce: Bytes, ins: number, label: Bytes): void {
  for (let off = 0, i = 1; off < data.length; off += 32, i++) {
    const block = sealBlock(i, x, point, nonce, ins, [], label);
    for (let j = 0; j < 32 && off + j < data.length; j++) {
      data[off + j] ^= block[j];
    }
    wipe(block);
  }
}

/** A fresh secp256k1 scalar from the phone's secure source. */
function freshScalar(): Uint8Array {
  for (;;) {
    const k = secureRandomBytes(32);
    if (secp256k1.utils.isValidPrivateKey(k)) {
      return k;
    }
    wipe(k);
  }
}

/**
 * The envelope E ‖ ct ‖ tag for `clear` under `pinKey` (33 bytes, compressed)
 * and `nonce`, for instruction `ins`. `eph` is the sender's scalar for this
 * envelope only (a fresh one when absent; the vectors pass theirs); it, the
 * shared point and x are wiped before this returns. The caller owns `clear`.
 * Raw: it checks nothing about the key; `sealedPinApdu` is what hosts call.
 */
export function sealEnvelope(
  ins: number,
  pinKey: Bytes,
  nonce: Bytes,
  clear: Bytes,
  eph: Uint8Array = freshScalar(),
  label: Bytes = SEAL_LABEL,
): Uint8Array {
  const point = secp256k1.getPublicKey(eph, false);
  const shared = secp256k1.getSharedSecret(eph, Uint8Array.from(pinKey), false);
  const x = shared.slice(1, 33);
  const ct = Uint8Array.from(clear);
  keystream(ct, x, point, nonce, ins, label);
  const tag = sealBlock(0, x, point, nonce, ins, ct, label);
  const out = concat(point, ct, tag.subarray(0, TAG_LEN));
  wipe(eph, shared, x, tag);
  return out;
}

/**
 * The card's side of the seal: opens `envelope` (E ‖ ct ‖ tag) with the PIN
 * key's scalar `d`, or null when E is not a full point on the curve, the
 * envelope is shorter than its parts, or the tag does not match. For the
 * fakes and the vector test: the app itself never opens an envelope.
 */
export function openEnvelope(
  d: Uint8Array,
  nonce: Bytes,
  ins: number,
  envelope: Bytes,
  label: Bytes = SEAL_LABEL,
): Uint8Array | null {
  if (envelope.length < POINT_LEN + TAG_LEN || envelope[0] !== 0x04) {
    return null;
  }
  const point = Uint8Array.from(envelope.slice(0, POINT_LEN));
  let shared: Uint8Array;
  try {
    secp256k1.ProjectivePoint.fromHex(point).assertValidity();
    shared = secp256k1.getSharedSecret(d, point, false);
  } catch {
    return null;
  }
  const x = shared.slice(1, 33);
  const ct = Uint8Array.from(envelope.slice(POINT_LEN, envelope.length - TAG_LEN));
  const tag = sealBlock(0, x, point, nonce, ins, ct, label);
  const good = same(tag.subarray(0, TAG_LEN), envelope.slice(envelope.length - TAG_LEN));
  if (good) {
    keystream(ct, x, point, nonce, ins, label);
  }
  wipe(shared, x, tag);
  return good ? ct : null;
}

/** The PIN a block carries, or null when its length is not 4 to 8 or it is not 9 bytes of digits and zeros. */
export function unblockPin(block: Bytes): string | null {
  const n = block[0];
  if (block.length !== PIN_BLOCK_LEN || n < 4 || n > 8) {
    return null;
  }
  return String.fromCharCode(...Array.from(block.slice(1, 1 + n)));
}

/** ECDSA P-256 SHA-256 under a SEC1 key, high or low s (the card does not normalise s). */
function p256Verifies(sig: Uint8Array, message: Uint8Array, key: Uint8Array, format: 'der' | 'compact'): boolean {
  try {
    return p256.verify(sig, sha256(message), key, {lowS: false, prehash: false, format});
  } catch {
    return false;
  }
}

/** True when `key` is a compressed secp256k1 point on the curve. */
function compressedPoint(key: Bytes): boolean {
  if (key.length !== PIN_KEY_LEN || (key[0] !== 0x02 && key[0] !== 0x03)) {
    return false;
  }
  try {
    secp256k1.ProjectivePoint.fromHex(Uint8Array.from(key)).assertValidity();
    return true;
  } catch {
    return false;
  }
}

/** The binding's message: `fedimintcard/pin1` ‖ the PIN key's 33 bytes. */
export function bindingMessage(pinKey: Bytes): Uint8Array {
  return concat(BINDING_LABEL, pinKey);
}

/** The PIN key is a compressed secp256k1 point, and `der` verifies under `attestKey` over the binding message. */
export function bindingVerifies(attestKey: Bytes, pinKey: Bytes, der: Bytes): boolean {
  if (!compressedPoint(pinKey) || der.length < BINDING_MIN || der.length > BINDING_MAX) {
    return false;
  }
  return p256Verifies(Uint8Array.from(der), bindingMessage(pinKey), Uint8Array.from(attestKey), 'der');
}

/** GET_NONCE's answer split: nonce (16) ‖ PIN key (33) ‖ binding (DER, 8 to 72); null for any other length. */
export function parseNonceAnswer(
  body: Bytes,
): {nonce: Uint8Array; pinKey: Uint8Array; binding: Uint8Array} | null {
  const der = body.length - NONCE_LEN - PIN_KEY_LEN;
  if (der < BINDING_MIN || der > BINDING_MAX) {
    return null;
  }
  const b = Uint8Array.from(body);
  return {
    nonce: b.slice(0, NONCE_LEN),
    pinKey: b.slice(NONCE_LEN, NONCE_LEN + PIN_KEY_LEN),
    binding: b.slice(NONCE_LEN + PIN_KEY_LEN),
  };
}

/** The certificate's parts (§ 2), or null when it is not 130 bytes of version 1. */
export function parseCertificate(
  b: Bytes,
): {slot0: Uint8Array; attestKey: Uint8Array; signature: Uint8Array} | null {
  if (b.length !== CERT_LEN || b[0] !== 0x01) {
    return null;
  }
  const c = Uint8Array.from(b);
  return {slot0: c.slice(1, 33), attestKey: c.slice(33, 66), signature: c.slice(66, 130)};
}

/** The registry signed `fedimintcard/cert1` ‖ slot 0 ‖ the attestation key, raw r ‖ s, under `certKey` (65 bytes). */
export function certificateVerifies(cert: {slot0: Bytes; attestKey: Bytes; signature: Bytes}, certKey: Bytes): boolean {
  if (certKey.length !== POINT_LEN || certKey[0] !== 0x04) {
    return false;
  }
  return p256Verifies(
    Uint8Array.from(cert.signature),
    concat(CERT_LABEL, cert.slot0, cert.attestKey),
    Uint8Array.from(certKey),
    'compact',
  );
}

export type Refused = {readonly ok: false; readonly sw: number | null; readonly reason: string};

const hex = (sw: number | null) => (sw === null ? 'no status word' : sw.toString(16).toUpperCase().padStart(4, '0'));

/** What the person is told when this phone has no registry certificate key to check a card's PIN key with. */
export const NEEDS_CERT_KEY =
  "this phone needs the registry's certificate key to check the card before it sends a PIN; no PIN was sent";
/** What the person is told when the card's certificate or PIN key does not check. */
export const NOT_GENUINE = 'the card did not prove it is a genuine card of this registry; no PIN was sent';

/**
 * A card whose certificate verified under the registry's certificate key and
 * names the card's own slot 0, in this selection. Only `check` makes one.
 */
export class TrustedCard {
  private constructor(
    readonly slot0: Uint8Array,
    readonly attestKey: Uint8Array,
  ) {}

  /** GET_PUB 0, GET_CERT, the certificate checked under `certKey` and against slot 0. */
  static async check(
    send: Send,
    certKey: Bytes | null | undefined,
  ): Promise<{readonly ok: true; readonly trust: TrustedCard} | Refused> {
    if (!certKey || certKey.length !== POINT_LEN || certKey[0] !== 0x04) {
      return {ok: false, sw: null, reason: NEEDS_CERT_KEY};
    }
    const slot0 = await send([CLA, INS_GET_PUB, 0x00, 0x00, 0x00]);
    if (slot0.sw !== SW_OK || slot0.body.length !== 32) {
      return {ok: false, sw: slot0.sw, reason: `GET_PUB 0 answered ${hex(slot0.sw)}`};
    }
    const got = await send([CLA, INS_GET_CERT, 0x00, 0x00, 0x00]);
    if (got.sw !== SW_OK) {
      return {ok: false, sw: got.sw, reason: `GET_CERT answered ${hex(got.sw)}`};
    }
    const cert = parseCertificate(got.body);
    if (cert === null || !same(cert.slot0, slot0.body) || !certificateVerifies(cert, certKey)) {
      return {ok: false, sw: got.sw, reason: NOT_GENUINE};
    }
    return {ok: true, trust: new TrustedCard(cert.slot0, cert.attestKey)};
  }
}

/** A PIN key and its live nonce, checked under a `TrustedCard`. Only `ask` makes one. */
export class CheckedPinKey {
  private constructor(
    readonly pinKey: Uint8Array,
    readonly nonce: Uint8Array,
  ) {}

  /** GET_NONCE, then the PIN key and its binding checked under the trusted card's attestation key. */
  static async ask(send: Send, trust: TrustedCard): Promise<{readonly ok: true; readonly key: CheckedPinKey} | Refused> {
    const got = await send(getNonceApdu());
    if (got.sw !== SW_OK) {
      return {ok: false, sw: got.sw, reason: `GET_NONCE answered ${hex(got.sw)}`};
    }
    const answer = parseNonceAnswer(got.body);
    if (answer === null || !bindingVerifies(trust.attestKey, answer.pinKey, answer.binding)) {
      return {ok: false, sw: got.sw, reason: NOT_GENUINE};
    }
    return {ok: true, key: new CheckedPinKey(answer.pinKey, answer.nonce)};
  }
}

export function getNonceApdu(): number[] {
  return [CLA, INS_GET_NONCE, P1_SEALED, 0x00, 0x00];
}

/**
 * A sealed PIN command: `B0 ins 01 00 Lc` ‖ the envelope of `clear` (one PIN
 * block for VERIFY_PIN and SET_PIN, old ‖ new for CHANGE_PIN), with no Le.
 * `clear` is wiped. Build it once per tap and send the same bytes through the wait.
 */
export function sealedPinApdu(ins: number, key: CheckedPinKey, clear: Uint8Array, eph?: Uint8Array): number[] {
  const envelope = sealEnvelope(ins, key.pinKey, key.nonce, clear, eph);
  wipe(clear);
  return [CLA, ins, P1_SEALED, 0x00, envelope.length, ...envelope];
}
