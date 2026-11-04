import {describe, expect, it} from 'vitest';

import {fromHex, toHex} from '../src/http';
import {messageOf, parsePublicKey, verifyNote, verifyNotes} from '../src/tbs';
import vectors from './tbs-vectors.json';

/**
 * OPS-PLAN O4: the registry's note check is a port of Fedimint's, so it is held to vectors the
 * federation's own Rust code produced (`host/sidecar/tests/tbs_vectors.rs`): the hash to G1, and
 * notes made by the blind flow, each also tampered in the ways a forged load would be.
 */

describe('Message::from_bytes', () => {
  it('maps bytes to the same G1 point as tbs', () => {
    for (const v of vectors.messages) {
      expect(toHex(messageOf(fromHex(v.bytes)).toBytes()), v.bytes).toBe(v.message);
    }
  });
});

describe('Note::verify', () => {
  /** Flip one bit of a hex string. */
  const flip = (hex: string, byte: number) =>
    hex.slice(0, byte * 2) + ((parseInt(hex.slice(byte * 2, byte * 2 + 2), 16) ^ 1).toString(16).padStart(2, '0')) + hex.slice(byte * 2 + 2);

  it('accepts every note the federation code signed', () => {
    for (const n of vectors.notes) {
      expect(toHex(messageOf(fromHex(n.nonce)).toBytes())).toBe(n.message);
      expect(verifyNote(n.nonce, n.signature, parsePublicKey(n.pk)!)).toBe(true);
    }
  });

  it('refuses the wrong tier, another nonce, a flipped bit and a non-point', () => {
    const [a, b] = vectors.notes;
    expect(verifyNote(a!.nonce, a!.signature, parsePublicKey(a!.other_tier_pk)!)).toBe(false);
    expect(verifyNote(b!.nonce, a!.signature, parsePublicKey(a!.pk)!)).toBe(false);
    expect(verifyNote(flip(a!.nonce, 32), a!.signature, parsePublicKey(a!.pk)!)).toBe(false);
    // A flipped bit in the signature is another point or no point at all; either fails.
    expect(verifyNote(a!.nonce, flip(a!.signature, 47), parsePublicKey(a!.pk)!)).toBe(false);
    expect(verifyNote(a!.nonce, 'c'.repeat(96), parsePublicKey(a!.pk)!)).toBe(false);
  });

  it('parses only real G2 points as keys', () => {
    expect(parsePublicKey('c'.repeat(192))).toBeNull();
    expect(parsePublicKey(vectors.notes[0]!.pk)).not.toBeNull();
  });
});

describe('a batch of notes', () => {
  it('passes when every note verifies, and fails when any one does not', () => {
    const all = vectors.notes.map(n => ({nonce: n.nonce, signature: n.signature, pk: parsePublicKey(n.pk)!}));
    expect(verifyNotes(all)).toBe(true);
    expect(verifyNotes([])).toBe(true);
    // Two signatures swapped between notes of one tier: each is a real signature, on the other note.
    const swapped = all.map((n, i) => (i < 2 ? {...n, signature: all[1 - i]!.signature} : n));
    expect(verifyNotes(swapped)).toBe(false);
    expect(verifyNotes(all.map((n, i) => (i === 3 ? {...n, pk: all[0]!.pk} : n)))).toBe(false);
  });

  it('checks eight notes, a full card, in one batch', () => {
    const eight = [...vectors.notes, ...vectors.notes].map(n => ({nonce: n.nonce, signature: n.signature, pk: parsePublicKey(n.pk)!}));
    expect(verifyNotes(eight)).toBe(true);
  });
});
