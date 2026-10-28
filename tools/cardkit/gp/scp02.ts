/**
 * GlobalPlatform Secure Channel Protocol 02 and the card-manager commands an
 * install needs, over an injected transceiver (D27).
 *
 * The J3R180 offers SCP02 with i=15, 35, 55 and 75 and opens i=55 (the
 * owner's `gp -i -v` of the spare, 2026-10-17). The host cannot read i from
 * INITIALIZE UPDATE, which reports the protocol only, so nothing here depends
 * on it: three DES3 keys, C-MAC only (EXTERNAL AUTHENTICATE P1 01), no R-MAC,
 * the ICV encrypted from the second command on, and the card's pseudo-random
 * challenge taken as it comes. That is what GlobalPlatformPro v25.10.20 does
 * for every SCP02 i (its SCP02Wrapper sets icvEnc and macModifiedAPDU for
 * all), and gp is what has run on these cards. gp's GPSession.transmit also
 * sends every wrapped command with Le 00 (it rebuilds each as a CommandAPDU
 * with Ne 256), so every builder here for a command under the C-MAC ends in
 * 00, outside the MAC. The H0 transcripts (gp/test/fixtures) hold this file
 * to gp's bytes.
 *
 * No retries anywhere: a command sent twice breaks the MAC chain, and a
 * failed handshake counts towards the card manager's lock, which is for good
 * (FORK.md). So the card cryptogram is checked here, before EXTERNAL
 * AUTHENTICATE is sent, and wrong keys send nothing more.
 */
import {DELETABLE_AIDS, ISD_AID} from './aids';
import {
  type Bytes,
  bytes,
  concat,
  equal,
  equalConstantTime,
  fromHex,
  swHex,
  toHex,
} from './bytes';
import {desEcb, retailMac, tdesCbc, tdesCbcMac, tdesEcb} from './des';

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

const K40 = fromHex('404142434445464748494A4B4C4D4E4F');

/** The keys the J3R180 ships with: 40..4F for all three, key set version 01. */
export const FACTORY_KEYS: KeySet = {enc: K40, mac: K40, dek: K40};

/** The spike's public test set for the rotation drill (never a card's real keys). */
export const SPIKE_KEYS: KeySet = {
  enc: fromHex('101112131415161718191A1B1C1D1E1F'),
  mac: fromHex('202122232425262728292A2B2C2D2E2F'),
  dek: fromHex('303132333435363738393A3B3C3D3E3F'),
};

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

/** The card's cryptogram does not match these keys. Nothing more was sent. */
export class WrongKeys extends Error {
  constructor(
    message = 'the card did not prove these keys; EXTERNAL AUTHENTICATE was not sent',
  ) {
    super(message);
    this.name = 'WrongKeys';
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

/** SCP02 session keys: 3DES-CBC with a zero IV over constant ‖ counter ‖ twelve zeros. */
export function deriveSessionKeys(keys: KeySet, seq: Bytes): SessionKeys {
  if (seq.length !== 2) {
    throw new RangeError('the sequence counter is 2 bytes');
  }
  const derive = (key: Bytes, constant: number[]) =>
    tdesCbc(key, concat(constant, seq, new Array(12).fill(0)));
  return {
    enc: derive(keys.enc, [0x01, 0x82]),
    mac: derive(keys.mac, [0x01, 0x01]),
    dek: derive(keys.dek, [0x01, 0x81]),
  };
}

/** The card's cryptogram: the full 3DES MAC under S-ENC of host ‖ counter ‖ card challenge. */
export function cardCryptogram(
  sEnc: Bytes,
  hostChallenge: Bytes,
  seq: Bytes,
  cardChallenge: Bytes,
): number[] {
  return tdesCbcMac(sEnc, concat(hostChallenge, seq, cardChallenge));
}

/** The host's cryptogram: the full 3DES MAC under S-ENC of counter ‖ card ‖ host challenge. */
export function hostCryptogram(
  sEnc: Bytes,
  seq: Bytes,
  cardChallenge: Bytes,
  hostChallenge: Bytes,
): number[] {
  return tdesCbcMac(sEnc, concat(seq, cardChallenge, hostChallenge));
}

/** The 28 bytes INITIALIZE UPDATE answers with. */
export interface InitializeUpdateReply {
  diversification: number[];
  kvn: number;
  scp: number;
  seq: number[];
  cardChallenge: number[];
  cardCryptogram: number[];
}

/** One INITIALIZE UPDATE: what was sent and what came back. */
export interface Handshake {
  hostChallenge: number[];
  reply: InitializeUpdateReply;
}

/** INITIALIZE UPDATE: key set `kvn` (0 for the card's choice), key ID 0, with Le. */
export function initializeUpdateCommand(
  hostChallenge: Bytes,
  kvn = 0,
): number[] {
  if (hostChallenge.length !== 8) {
    throw new RangeError(
      'the host challenge is 8 bytes, not ' + hostChallenge.length,
    );
  }
  return concat([0x80, 0x50, kvn & 0xff, 0x00, 0x08], hostChallenge, [0x00]);
}

export function parseInitializeUpdate(reply: Reply): InitializeUpdateReply {
  if (reply.sw !== 0x9000) {
    throw new CardError(reply.sw, 0x50);
  }
  const d = reply.data;
  if (d.length !== 28) {
    throw new CardError(
      reply.sw,
      0x50,
      `INITIALIZE UPDATE answered ${d.length} bytes, not 28`,
    );
  }
  if (d[11] !== 0x02) {
    throw new CardError(
      reply.sw,
      0x50,
      `the card opens SCP0${d[11]}, not SCP02`,
    );
  }
  return {
    diversification: d.slice(0, 10),
    kvn: d[10],
    scp: d[11],
    seq: d.slice(12, 14),
    cardChallenge: d.slice(14, 20),
    cardCryptogram: d.slice(20, 28),
  };
}

/** Sends INITIALIZE UPDATE with a fresh host challenge. Authenticates nothing. */
export async function initializeUpdate(
  tx: Transceiver,
  random: Random,
  kvn = 0,
): Promise<Handshake> {
  const hostChallenge = bytes(random(8));
  const command = initializeUpdateCommand(hostChallenge, kvn);
  return {
    hostChallenge,
    reply: parseInitializeUpdate(await transmit(tx, command)),
  };
}

/** The session keys when the card's cryptogram proves `keys`, else null. Offline: sends nothing. */
export function sessionIfMatches(
  keys: KeySet,
  hs: Handshake,
): SessionKeys | null {
  const s = deriveSessionKeys(keys, hs.reply.seq);
  const expected = cardCryptogram(
    s.enc,
    hs.hostChallenge,
    hs.reply.seq,
    hs.reply.cardChallenge,
  );
  return equalConstantTime(expected, hs.reply.cardCryptogram) ? s : null;
}

export interface SessionOptions {
  /**
   * True: the C-MAC covers the header as sent, CLA|04 and Lc + 8. False: the
   * header and Lc as they were before wrapping. Either way the APDU sent
   * carries CLA|04 and Lc + 8. GlobalPlatformPro v25.10.20 MACs the modified
   * header for every SCP02 i, so that is the default; the H0 transcript test
   * pins it to the card's own answers.
   */
  macOverModifiedApdu?: boolean;
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

/**
 * An open secure channel at C-MAC level. Every command to the card manager
 * goes through `wrap` or `send`, in order; a command that is sent twice, or
 * not at all after being wrapped, breaks the chain, and the card ends the
 * session at the next one.
 */
export class Session {
  readonly keys: SessionKeys;
  readonly macOverModifiedApdu: boolean;
  private lastMac: number[] | null = null;

  constructor(
    keys: SessionKeys,
    options: SessionOptions = {},
    /** The version observed in the authenticated INITIALIZE UPDATE, if known. */
    readonly keyVersion?: number,
  ) {
    this.keys = {
      enc: bytes(keys.enc),
      mac: bytes(keys.mac),
      dek: bytes(keys.dek),
    };
    this.macOverModifiedApdu = options.macOverModifiedApdu ?? true;
  }

  /** The APDU with its C-MAC: CLA|04, Lc + 8, the MAC after the data, Le kept. */
  wrap(apdu: Bytes): number[] {
    const c = parseCommand(apdu);
    if (c.data.length > 255 - 8) {
      throw new RangeError(
        `${c.data.length} bytes of data leave no room for the C-MAC`,
      );
    }
    const cla = c.cla | 0x04;
    const lc = c.data.length + 8;
    const header = this.macOverModifiedApdu
      ? [cla, c.ins, c.p1, c.p2, lc]
      : [c.cla, c.ins, c.p1, c.p2, c.data.length];
    const icv =
      this.lastMac === null
        ? new Array(8).fill(0)
        : desEcb(this.keys.mac.slice(0, 8), this.lastMac);
    const mac = retailMac(this.keys.mac, concat(header, c.data), icv);
    this.lastMac = mac;
    return concat(
      [cla, c.ins, c.p1, c.p2, lc],
      c.data,
      mac,
      c.le === null ? [] : [c.le],
    );
  }

  /** Wraps and sends one command. */
  send(tx: Transceiver, apdu: Bytes): Promise<Reply> {
    return transmit(tx, this.wrap(apdu));
  }
}

/**
 * EXTERNAL AUTHENTICATE before its C-MAC: P1 01 (C-MAC only), the host
 * cryptogram, and Le 00, as gp sends it (GPSession.transmit).
 */
export function externalAuthenticateCommand(hostCryptogram: Bytes): number[] {
  if (hostCryptogram.length !== 8) {
    throw new RangeError(
      'the host cryptogram is 8 bytes, not ' + hostCryptogram.length,
    );
  }
  return concat([0x84, 0x82, 0x01, 0x00, 0x08], hostCryptogram, [0x00]);
}

/** EXTERNAL AUTHENTICATE at C-MAC level, for keys `sessionIfMatches` has already proved. */
export async function externalAuthenticate(
  tx: Transceiver,
  keys: SessionKeys,
  hs: Handshake,
  options: SessionOptions = {},
): Promise<Session> {
  const session = new Session(keys, options, hs.reply.kvn);
  const cryptogram = hostCryptogram(
    keys.enc,
    hs.reply.seq,
    hs.reply.cardChallenge,
    hs.hostChallenge,
  );
  const reply = await session.send(tx, externalAuthenticateCommand(cryptogram));
  if (reply.sw !== 0x9000) {
    throw new CardError(reply.sw, 0x82);
  }
  return session;
}

/**
 * Opens a secure channel: INITIALIZE UPDATE, the card's cryptogram checked
 * here, then EXTERNAL AUTHENTICATE. Wrong keys throw WrongKeys with nothing
 * sent after INITIALIZE UPDATE.
 */
export async function openSession(
  tx: Transceiver,
  keys: KeySet,
  random: Random,
  options: SessionOptions & {kvn?: number} = {},
): Promise<Session> {
  const hs = await initializeUpdate(tx, random, options.kvn ?? 0);
  const s = sessionIfMatches(keys, hs);
  if (!s) {
    throw new WrongKeys();
  }
  return externalAuthenticate(tx, s, hs, options);
}

// ── Commands ──────────────────────────────────────────────────────────────

/** SELECT by name, first or only occurrence, with Le. */
export function selectAid(aid: Bytes): number[] {
  if (aid.length < 5 || aid.length > 16) {
    throw new RangeError('an AID is 5 to 16 bytes');
  }
  return concat([0x00, 0xa4, 0x04, 0x00, aid.length], aid, [0x00]);
}

/** GET DATA FF21, the card's free resources. Needs no key (docs/FOXY-CARD-HARDWARE.md). */
export function getFreeMemory(): number[] {
  return [0x80, 0xca, 0xff, 0x21, 0x00];
}

export interface FreeMemory {
  /** Tag 81: installed applications, where the card says. */
  applications?: number;
  /** Tag 82: free non-volatile memory, bytes. */
  nonVolatile: number;
  /** Tag 83: free volatile memory, bytes. */
  volatile?: number;
}

/** FF21 10 81 02 nnnn 82 04 <free storage> 83 04 <free volatile>; null for anything else. */
export function parseFreeMemory(data: Bytes): FreeMemory | null {
  const d = bytes(data);
  if (d.length < 3 || d[0] !== 0xff || d[1] !== 0x21 || d[2] + 3 > d.length) {
    return null;
  }
  const out: {applications?: number; nonVolatile?: number; volatile?: number} =
    {};
  let at = 3;
  const end = 3 + d[2];
  while (at + 2 <= end) {
    const tag = d[at];
    const len = d[at + 1];
    if (at + 2 + len > end || len > 4) {
      return null;
    }
    let v = 0;
    for (let i = 0; i < len; i++) {
      v = v * 256 + d[at + 2 + i];
    }
    if (tag === 0x81) {
      out.applications = v;
    } else if (tag === 0x82) {
      out.nonVolatile = v;
    } else if (tag === 0x83) {
      out.volatile = v;
    }
    at += 2 + len;
  }
  return out.nonVolatile === undefined ? null : (out as FreeMemory);
}

/**
 * DELETE of one object (P2 00: not its dependents), with Le 00 as gp sends
 * it. Throws for the card manager and for any AID off the whitelist,
 * before anything is built: this module cannot be made to delete anything
 * else.
 */
export function deleteAid(aid: Bytes): number[] {
  if (equal(aid, ISD_AID)) {
    throw new RangeError('refusing to delete the card manager');
  }
  if (!DELETABLE_AIDS.some(allowed => equal(allowed, aid))) {
    throw new RangeError(
      `refusing to delete ${toHex(aid)}: it is not on the whitelist`,
    );
  }
  return concat(
    [0x80, 0xe4, 0x00, 0x00, aid.length + 2, 0x4f, aid.length],
    aid,
    [0x00],
  );
}

/** INSTALL [for load] into `domain` (the ISD): no load file hash, no parameters, an empty token, Le 00. */
export function installForLoad(
  packageAid: Bytes,
  domain: Bytes = ISD_AID,
): number[] {
  const data = concat(
    [packageAid.length],
    packageAid,
    [domain.length],
    domain,
    [0x00, 0x00, 0x00],
  );
  return concat([0x80, 0xe6, 0x02, 0x00, data.length], data, [0x00]);
}

/**
 * The load file as LOAD commands of `blockSize` bytes (247 leaves room for
 * the C-MAC in a short APDU), with Le as gp sends them. P1 80 marks the last
 * block and P2 counts from 0, so a file of more than 256 blocks cannot be sent.
 */
export function splitLoad(file: Bytes, blockSize = 247): number[][] {
  if (!Number.isInteger(blockSize) || blockSize < 1 || blockSize + 8 > 255) {
    throw new RangeError(
      `a block of ${blockSize} bytes leaves no room for the C-MAC`,
    );
  }
  const all = bytes(file);
  const count = Math.ceil(all.length / blockSize);
  if (count === 0) {
    throw new RangeError('an empty load file');
  }
  if (count > 256) {
    throw new RangeError(`${count} blocks: P2 counts 256 at most`);
  }
  const out: number[][] = [];
  for (let i = 0; i < count; i++) {
    const block = all.slice(i * blockSize, (i + 1) * blockSize);
    out.push(
      concat(
        [0x80, 0xe8, i === count - 1 ? 0x80 : 0x00, i, block.length],
        block,
        [0x00],
      ),
    );
  }
  return out;
}

/** INSTALL [for install and make selectable]: privileges 00, parameters C9 00, an empty token, Le 00. */
export function installAndSelectable(
  packageAid: Bytes,
  moduleAid: Bytes,
  instanceAid: Bytes,
  options: {privileges?: Bytes; parameters?: Bytes} = {},
): number[] {
  const privileges = options.privileges ?? [0x00];
  const parameters = options.parameters ?? [0xc9, 0x00];
  const data = concat(
    [packageAid.length],
    packageAid,
    [moduleAid.length],
    moduleAid,
    [instanceAid.length],
    instanceAid,
    [privileges.length],
    privileges,
    [parameters.length],
    parameters,
    [0x00],
  );
  return concat([0x80, 0xe6, 0x0c, 0x00, data.length], data, [0x00]);
}

/** A DES3 key's check value: the first three bytes of its encryption of eight zeros. */
export function kcv(key16: Bytes): number[] {
  return tdesEcb(key16, new Array(8).fill(0)).slice(0, 3);
}

/**
 * PUT KEY of a whole set: P1 the key set it replaces (0 adds one), P2 81 (key
 * ID 1, more to follow), the new set's version, then ENC, MAC and DEK, each
 * 80 10 <the key under S-DEK, 3DES-ECB> 03 <its KCV>, and Le 00. As gp --lock
 * sends it on a card whose set is version 01: P1 01, new version 01, in place.
 */
export function putKeys(
  keys: KeySet,
  sessionDek: Bytes,
  options: {p1: number; newKvn: number},
): number[] {
  const data: number[] = [options.newKvn & 0xff];
  for (const key of [keys.enc, keys.mac, keys.dek]) {
    if (key.length !== 16) {
      throw new RangeError('a DES3 key is 16 bytes');
    }
    data.push(0x80, 0x10, ...tdesEcb(sessionDek, key), 0x03, ...kcv(key));
  }
  return concat(
    [0x80, 0xd8, options.p1 & 0xff, 0x81, data.length],
    data,
    [0x00],
  );
}
