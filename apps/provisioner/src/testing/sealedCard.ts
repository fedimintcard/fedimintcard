/**
 * The card's side of the sealed PIN for the fakes (docs/OFFLINE-PROTOCOL.md
 * § 9), and a test registry that signs real certificates (§ 2), so the app's
 * checks before it seals run against the fakes exactly as against a card.
 * Test-only: fixed scalars, a nonce from a counter.
 */
import {p256} from '@noble/curves/nist';
import {secp256k1} from '@noble/curves/secp256k1';
import {sha256} from '@noble/hashes/sha2';
import {
  bindingMessage,
  CERT_LABEL,
  NONCE_LEN,
  openEnvelope,
  PIN_BLOCK_LEN,
  unblockPin,
} from '../sealedPin';

const scalar = (seed: string): Uint8Array => sha256(Uint8Array.from(seed, c => c.charCodeAt(0)));

const REGISTRY_SCALAR = scalar('fakes: the registry certificate key');
/** The test registry's certificate key, 65 bytes uncompressed (`GET /registry/cert-key`). */
export const TEST_CERT_KEY: readonly number[] = Array.from(p256.getPublicKey(REGISTRY_SCALAR, false));

/** The registry's certificate over `attestPub` (65 bytes) and slot 0: 01 ‖ slot 0 ‖ attest key compressed ‖ r ‖ s. */
export function testCertify(attestPub: readonly number[], slot0: readonly number[], key = REGISTRY_SCALAR): number[] {
  const attest = p256.ProjectivePoint.fromHex(Uint8Array.from(attestPub)).toRawBytes(true);
  const message = Uint8Array.from([...CERT_LABEL, ...slot0, ...attest]);
  const sig = p256.sign(sha256(message), key, {prehash: false});
  return [0x01, ...slot0, ...attest, ...sig.toCompactRawBytes()];
}

/** A certificate signed by another key than the test registry's. */
export const otherCertify = (attestPub: readonly number[], slot0: readonly number[]) =>
  testCertify(attestPub, slot0, scalar('fakes: some other key'));

/**
 * The card's PIN key, its attestation key, the binding and the nonce, as the
 * applet keeps them (FedimintCardApplet): both keys made at each
 * ATTEST_INIT, the binding at the first GET_NONCE, the nonce live from
 * GET_NONCE to the first opening, a new GET_NONCE or a SELECT.
 */
export class SealedPinCard {
  private pinScalar: Uint8Array | null = null;
  private attestScalar: Uint8Array | null = null;
  private binding: Uint8Array | null = null;
  private nonce: Uint8Array | null = null;
  private made = 0;
  private asked = 0;
  /** A test sets this to make the card answer a binding by another key (a card that is not genuine). */
  forgeBinding = false;

  constructor(private readonly seed: string) {}

  /** ATTEST_INIT: a new PIN key and attestation key; answers the attestation key, 65 bytes. */
  attestInit(): number[] {
    this.made += 1;
    this.pinScalar = scalar(`${this.seed} pin ${this.made}`);
    this.attestScalar = scalar(`${this.seed} attest ${this.made}`);
    this.binding = null;
    return Array.from(p256.getPublicKey(this.attestScalar, false));
  }

  /** GET_NONCE's answer, or null before ATTEST_INIT. */
  getNonce(): number[] | null {
    if (!this.pinScalar || !this.attestScalar) {
      return null;
    }
    const pinKey = secp256k1.getPublicKey(this.pinScalar, true);
    if (!this.binding) {
      const signer = this.forgeBinding ? scalar('fakes: not the card') : this.attestScalar;
      this.binding = p256.sign(sha256(bindingMessage(pinKey)), signer, {prehash: false}).toDERRawBytes();
    }
    this.asked += 1;
    this.nonce = scalar(`${this.seed} nonce ${this.asked}`).slice(0, NONCE_LEN);
    return [...this.nonce, ...pinKey, ...this.binding];
  }

  /** True while a nonce is live. */
  get live(): boolean {
    return this.nonce !== null;
  }

  /** SELECT, a reset, a tear: the nonce is gone. */
  forget(): void {
    this.nonce = null;
  }

  /** The opening: spends the nonce, opened or not; the clear bytes, or null. */
  open(ins: number, envelope: readonly number[]): Uint8Array | null {
    const nonce = this.nonce;
    this.nonce = null;
    if (!nonce || !this.pinScalar) {
      return null;
    }
    return openEnvelope(this.pinScalar, nonce, ins, envelope);
  }

  /** The PINs the opened blocks carry, or null for a block whose length is not 4 to 8. */
  static pins(clear: Uint8Array): string[] | null {
    const out: string[] = [];
    for (let at = 0; at < clear.length; at += PIN_BLOCK_LEN) {
      const pin = unblockPin(clear.slice(at, at + PIN_BLOCK_LEN));
      if (pin === null) {
        return null;
      }
      out.push(pin);
    }
    return out;
  }
}
