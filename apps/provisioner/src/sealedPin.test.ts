/**
 * The shared vectors, `spec/vectors/sealed-pin.json` (made by
 * `host/card/tests/sealed_pin_vectors.rs`), run against the app's own sealing:
 * the TS consumer of docs/OFFLINE-PROTOCOL.md § 9.
 */
import {readFileSync} from 'fs';
import {join} from 'path';
import {schnorr} from '@noble/curves/secp256k1';
import {sha256} from '@noble/hashes/sha2';
import {
  BINDING_LABEL,
  bindingMessage,
  bindingVerifies,
  openEnvelope,
  parseNonceAnswer,
  pinBlock,
  SEAL_LABEL,
  sealBlock,
  sealEnvelope,
  unblockPin,
} from './sealedPin';

const V = JSON.parse(readFileSync(join(__dirname, '../../../spec/vectors/sealed-pin.json'), 'utf8'));
const b = (h: string): Uint8Array => Uint8Array.from((h.match(/../g) ?? []).map(x => parseInt(x, 16)));
const h = (a: ArrayLike<number>): string => Array.from(a, x => x.toString(16).padStart(2, '0')).join('');

test('the labels', () => {
  expect(h(SEAL_LABEL)).toBe(V.labels.seal);
  expect(h(BINDING_LABEL)).toBe(V.labels.binding);
});

test('the PIN blocks, and back', () => {
  expect(V.pin_blocks.length).toBe(3);
  for (const p of V.pin_blocks) {
    expect(h(pinBlock(p.pin))).toBe(p.block);
    expect(unblockPin(b(p.block))).toBe(p.pin);
  }
  expect(() => pinBlock('123')).toThrow();
  expect(unblockPin(b('033132330000000000'))).toBeNull();
});

test('the bindings: the message, and each DER kept or refused, high s kept', () => {
  expect(V.binding.length).toBe(6);
  for (const e of V.binding) {
    expect(h(bindingMessage(b(e.pin_key)))).toBe(e.message);
    expect(b(e.der).length).toBe(e.der_len);
    expect(bindingVerifies(b(V.card.attest_key), b(e.pin_key), b(e.der))).toBe(e.valid);
  }
  expect(V.binding.some((e: {high_s: boolean; valid: boolean}) => e.high_s && e.valid)).toBe(true);
  // A key that is not a compressed point is refused before any signature is looked at.
  const good = V.binding[0];
  expect(bindingVerifies(b(V.card.attest_key), b('04' + good.pin_key.slice(2)), b(good.der))).toBe(false);
});

test('GET_NONCE answers split into nonce, PIN key and binding', () => {
  expect(V.get_nonce.length).toBe(4);
  for (const e of V.get_nonce) {
    const answer = b(e.answer);
    expect(answer.length).toBe(e.answer_len);
    const parsed = parseNonceAnswer(answer)!;
    expect(h(parsed.nonce)).toBe(e.nonce);
    expect(h(parsed.pinKey)).toBe(e.pin_key);
    expect(h(parsed.binding)).toBe(e.der);
    expect(bindingVerifies(b(V.card.attest_key), parsed.pinKey, parsed.binding)).toBe(true);
  }
  expect(parseNonceAnswer(new Uint8Array(16 + 33 + 7))).toBeNull();
  expect(parseNonceAnswer(new Uint8Array(16 + 33 + 73))).toBeNull();
});

test('every envelope: the sender’s arithmetic as the vectors have it, and the card’s side opens exactly the good ones', () => {
  expect(V.sealed.length).toBe(22);
  for (const e of V.sealed) {
    const ins = parseInt(e.sealed_for_ins, 16);
    const nonce = b(e.nonce);
    const point = b(e.E);
    const x = b(e.x);
    expect(e.keystream.map((k: string, i: number) => h(sealBlock(i + 1, x, point, nonce, ins, [])))).toEqual(e.keystream);
    expect(h(sealBlock(0, x, point, nonce, ins, b(e.ct)))).toBe(e.block0);
    expect(e.tag).toBe(e.block0.slice(0, 32));
    const envelope = sealEnvelope(ins, b(e.pin_key), nonce, b(e.clear), b(e.eph_scalar));
    expect(h(envelope)).toBe(e.E + e.ct + e.tag);
    const apdu = b(e.apdu);
    if (e.spoil === '' || e.spoil === 'for_ins' || e.spoil === 'nonce') {
      expect(h(apdu.slice(5))).toBe(h(envelope));
    }
    expect(h(apdu.slice(0, 5))).toBe(`b0${e.ins}0100${(apdu.length - 5).toString(16).padStart(2, '0')}`);
    const opened = openEnvelope(b(V.card.pin_scalar), b(e.card_nonce), parseInt(e.ins, 16), apdu.slice(5));
    expect(opened !== null).toBe(e.opens);
    expect(opened === null ? '' : h(opened)).toBe(e.opened);
  }
  const good = V.sealed.filter((e: {spoil: string}) => e.spoil === '');
  expect(good.map((e: {ins: string}) => e.ins)).toEqual(['41', '40', '40', '42']);
});

test('Cashu’s own envelopes, under its label, byte for byte, and its PIN-key signature', () => {
  expect(V.cashu.length).toBe(4);
  const sealed = V.cashu.filter((e: {apdu?: string}) => e.apdu);
  expect(sealed.length).toBe(3);
  for (const e of sealed) {
    const ins = parseInt(e.ins, 16);
    const envelope = sealEnvelope(ins, b(e.pin_key), b(e.nonce), b(e.clear), b(e.eph_scalar), b(e.label));
    expect(h(b(e.apdu).slice(5))).toBe(h(envelope));
  }
  const pinkey = V.cashu.find((e: {answer?: string}) => e.answer);
  const answer = b(pinkey.answer);
  const key = answer.slice(16, 49);
  const message = sha256(Uint8Array.from([...b(pinkey.label), ...key]));
  const cardKey = b(pinkey.card_key);
  expect(schnorr.verify(answer.slice(49, 113), message, cardKey.length === 33 ? cardKey.slice(1) : cardKey)).toBe(true);
});
