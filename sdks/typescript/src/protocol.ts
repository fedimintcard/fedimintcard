import { secp256k1 } from '@noble/curves/secp256k1';
import { p256 } from '@noble/curves/nist';
import { sha256 } from '@noble/hashes/sha2';

/** One ISO 7816 command and its response, including SW1/SW2. */
export type Transceive = (command: Uint8Array) => Promise<Uint8Array>;

export const FEDIMINT_CARD_AID = 'F0464D4341524401';

export class CardStatusError extends Error {
  constructor(readonly statusWord: number) {
    super('Card returned ' + statusWord.toString(16).padStart(4, '0').toUpperCase());
    this.name = 'CardStatusError';
  }
}

export class UnsupportedCardError extends Error {
  constructor() {
    super('This reader requires a Fedimint Card 4.x applet.');
    this.name = 'UnsupportedCardError';
  }
}

export class CardProtocolError extends Error {
  constructor(reason: string) {
    super(reason);
    this.name = 'CardProtocolError';
  }
}

function requireLength(data: Uint8Array, length: number, name: string): void {
  if (data.length !== length) throw new CardProtocolError('Invalid ' + name + ' length');
}

// All commands used here are read-only, short APDUs ending with Le.
export async function readResponse(send: Transceive, first: number[], maximum: number): Promise<Uint8Array> {
  let command = Uint8Array.from(first);
  const result: number[] = [];
  for (let round = 0; round < 16; round++) {
    const response = await send(command);
    if (!(response instanceof Uint8Array) || response.length < 2) {
      throw new CardProtocolError('Missing ISO 7816 status word');
    }
    const end = response.length - 2;
    const high = response[end];
    const low = response[end + 1];
    if (high === 0x6c) {
      if (end !== 0) throw new CardProtocolError('Unexpected body on a length correction');
      command = command.slice();
      command[command.length - 1] = low;
      continue;
    }
    if (high !== 0x61 && (high !== 0x90 || low !== 0)) throw new CardStatusError(high * 256 + low);
    if (result.length + end > maximum) throw new CardProtocolError('Card response exceeds the command limit');
    for (let i = 0; i < end; i++) result.push(response[i]);
    if (high === 0x90) return Uint8Array.from(result);
    command = Uint8Array.of(0x00, 0xc0, 0x00, 0x00, low);
  }
  throw new CardProtocolError('Too many ISO 7816 continuations');
}

const states = ['empty', 'keyed', 'note', 'change', 'spent'] as const;
export type SlotState = typeof states[number];
function slotState(value: number): SlotState {
  if (value >= states.length) throw new CardProtocolError('Unknown Fedimint Card slot state');
  return states[value];
}

function u64(data: Uint8Array, offset: number): bigint {
  let value = 0n;
  for (const byte of data.subarray(offset, offset + 8)) value = (value << 8n) | BigInt(byte);
  return value;
}

function dataView(data: Uint8Array): DataView {
  return new DataView(data.buffer, data.byteOffset, data.byteLength);
}

export function parseState(data: Uint8Array) {
  requireLength(data, 65, 'GET_STATE');
  const view = dataView(data);
  return {
    slots: Array.from(data.subarray(0, 32), slotState),
    headerTime: view.getUint32(32),
    windowStart: view.getUint32(36),
    dailyLimitMsat: u64(data, 40),
    spentThisWindowMsat: u64(data, 48),
    certificateLength: data[56],
    hasAttestationKey: data[57] !== 0,
    hasRegistryKey: data[58] !== 0,
    hasPin: data[59] !== 0,
    wrongPinAttempts: data[60],
    pinWaitRemaining: view.getUint16(61),
    anchored: data[63] !== 0,
    usesChipMultiplier: Boolean(data[64] & 1),
    nonceKeyInRam: Boolean(data[64] & 2),
  };
}

export function publicKey(data: Uint8Array): Uint8Array {
  requireLength(data, 32, 'x-only public key');
  try {
    secp256k1.ProjectivePoint.fromHex(Uint8Array.of(2, ...data)).assertValidity();
  } catch {
    throw new CardProtocolError('Invalid x-only public key');
  }
  return data.slice();
}

export function parseSlot(data: Uint8Array) {
  requireLength(data, 187, 'GET_SLOT');
  const state = slotState(data[0]);
  return {
    state,
    publicKey: state === 'empty' ? null : publicKey(data.subarray(1, 33)),
    amountMsat: u64(data, 33),
    mintSignature: state === 'note' ? data.slice(41, 89) : null,
    spentHash: state === 'spent' ? data.slice(89, 121) : null,
    changeOpening: state === 'change' ? {
      transactionId: data.slice(121, 153),
      outputIndex: dataView(data).getUint16(153),
    } : null,
  };
}

export function parseCertificate(data: Uint8Array) {
  requireLength(data, 130, 'card certificate');
  if (data[0] !== 1) throw new CardProtocolError('Unsupported card certificate version');
  const attestationKey = data.slice(33, 66);
  try {
    p256.ProjectivePoint.fromHex(attestationKey).assertValidity();
  } catch {
    throw new CardProtocolError('Invalid attestation public key');
  }
  return { slot0: publicKey(data.subarray(1, 33)), attestationKey, signature: data.slice(66, 130) };
}

export type CardCertificate = ReturnType<typeof parseCertificate>;
export type CardState = ReturnType<typeof parseState>;
export type CardSlot = ReturnType<typeof parseSlot>;

/** Checks the registry signature and its binding to the observed slot 0 public key. */
export function verifiesCertificate(cert: CardCertificate, slot0: Uint8Array, registryKey: Uint8Array): boolean {
  if (registryKey.length !== 65 || registryKey[0] !== 4 || slot0.length !== 32 ||
      cert.slot0.length !== 32 || cert.attestationKey.length !== 33 || cert.signature.length !== 64 ||
      !slot0.every((byte, i) => byte === cert.slot0[i])) return false;
  const label = Uint8Array.from('fedimintcard/cert1', (c) => c.charCodeAt(0));
  const digest = sha256(Uint8Array.of(...label, ...cert.slot0, ...cert.attestationKey));
  try {
    return p256.verify(cert.signature, digest, registryKey, { format: 'compact', prehash: false, lowS: false });
  } catch {
    return false;
  }
}
