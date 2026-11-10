/**
 * What every secure channel shares: the injected transceiver and random
 * source, the key types, the two errors a card can cause, the reply and the
 * short APDU's parts. scp02.ts re-exports all of it, so its importers are
 * unchanged, and a second secure channel can build on these without
 * importing scp02.ts.
 */
import {type Bytes, bytes, swHex} from './bytes';

/** One APDU out, the card's data and SW back. Throws when the card is gone. */
export type Transceiver = (apdu: number[]) => Promise<number[]>;

/** `length` random bytes (the host challenge). */
export type Random = (length: number) => ArrayLike<number>;

/** A card manager's static keys, 16 bytes each (key IDs 1, 2 and 3). */
export interface KeySet {
  readonly enc: Bytes;
  readonly mac: Bytes;
  readonly dek: Bytes;
}

/** The session keys of one secure channel. */
export interface SessionKeys {
  readonly enc: number[];
  readonly mac: number[];
  readonly dek: number[];
}

/** The card left the field, or the transceiver failed: the reply never came. */
export class TransportError extends Error {
  readonly cause: unknown;

  constructor(message: string, cause?: unknown) {
    super(message);
    this.name = 'TransportError';
    this.cause = cause;
  }
}

/** The card answered, with a status word or a reply the command does not take. */
export class CardError extends Error {
  readonly sw: number;
  readonly ins: number;

  constructor(sw: number, ins: number, detail?: string) {
    super(
      detail ??
        `the card answered ${swHex(sw)} to INS ${ins.toString(16).toUpperCase().padStart(2, '0')}`,
    );
    this.name = 'CardError';
    this.sw = sw;
    this.ins = ins;
  }
}

export interface Reply {
  data: number[];
  sw: number;
}

/** Data and SW of a raw reply; a reply under two bytes is no reply. */
export function parseReply(raw: Bytes): Reply {
  if (!raw || raw.length < 2) {
    throw new TransportError(`a reply of ${raw ? raw.length : 0} bytes`);
  }
  const b = bytes(raw);
  return {data: b.slice(0, -2), sw: (b[b.length - 2] << 8) | b[b.length - 1]};
}

/** Sends one APDU. A transceiver that throws, or a short reply, is a TransportError. */
export async function transmit(
  tx: Transceiver,
  apdu: number[],
): Promise<Reply> {
  let raw: number[];
  try {
    raw = await tx(apdu);
  } catch (e) {
    throw new TransportError(
      e instanceof Error && e.message ? e.message : 'the card did not answer',
      e,
    );
  }
  return parseReply(raw);
}

interface Command {
  cla: number;
  ins: number;
  p1: number;
  p2: number;
  data: number[];
  le: number | null;
}

/** A short APDU's parts: case 1 (header), 2 (Le), 3 (Lc data) or 4 (Lc data Le). */
export function parseCommand(apdu: Bytes): Command {
  const a = bytes(apdu);
  if (a.length < 4) {
    throw new RangeError('an APDU is at least 4 bytes');
  }
  const [cla, ins, p1, p2] = a;
  if (a.length === 4) {
    return {cla, ins, p1, p2, data: [], le: null};
  }
  if (a.length === 5) {
    return {cla, ins, p1, p2, data: [], le: a[4]};
  }
  const lc = a[4];
  if (lc > 0 && a.length === 5 + lc) {
    return {cla, ins, p1, p2, data: a.slice(5), le: null};
  }
  if (lc > 0 && a.length === 6 + lc) {
    return {cla, ins, p1, p2, data: a.slice(5, 5 + lc), le: a[5 + lc]};
  }
  throw new RangeError(`an APDU of ${a.length} bytes with Lc ${lc}`);
}
