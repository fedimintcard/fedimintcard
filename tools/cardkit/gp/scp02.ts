/**
 * GlobalPlatform Secure Channel Protocol 02, the card-manager commands an
 * install needs, and the one way in to a secure channel, over an injected
 * transceiver (D27, D181). INITIALIZE UPDATE's reply names the protocol in
 * byte 11, so `openSession` reads it there and runs SCP02 (here, the J3R180)
 * or SCP03 (scp03.ts, the J3R452) behind one interface: the install, the key
 * rotation and the reset never choose. Everything else in this file is the
 * same for both: the commands, the whitelist and the rule that nothing goes
 * out after INITIALIZE UPDATE until the card has proved the keys.
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
  CardError,
  type KeySet,
  type Random,
  type Reply,
  type SessionKeys,
  type Transceiver,
  parseCommand,
  transmit,
} from './apdu';
import {
  type Bytes,
  bytes,
  concat,
  equal,
  equalConstantTime,
  fromHex,
  toHex,
} from './bytes';
import {desEcb, retailMac, tdesCbc, tdesCbcMac, tdesEcb} from './des';
import {
  Scp03Session,
  aesEcb,
  aesKcv,
  parseScp03Reply,
  scp03HostCryptogram,
  scp03KeysIfMatches,
} from './scp03';

export {
  CardError,
  TransportError,
  parseCommand,
  parseReply,
  transmit,
  type KeySet,
  type Random,
  type Reply,
  type SessionKeys,
  type Transceiver,
} from './apdu';

const K40 = fromHex('404142434445464748494A4B4C4D4E4F');

/** The keys the J3R180 ships with: 40..4F for all three, key set version 01. */
export const FACTORY_KEYS: KeySet = {enc: K40, mac: K40, dek: K40};

/**
 * The key set version the public default sits at: a J3R180 ships there, and a
 * J3R452 is moved there at first contact (D182). The reset puts the public
 * default back in place, so a card the tap provisioned returns to it here.
 */
export const FACTORY_KVN = 0x01;

/**
 * The key set version a J3R452 ships at (255). Its first rotation is first
 * contact: PUT KEY with P1 00 puts our set at FACTORY_KVN, and the card drops
 * the version-FF set (D182).
 */
export const SHIPPED_KVN = 0xff;

/** The spike's public test set for the rotation drill (never a card's real keys). */
export const SPIKE_KEYS: KeySet = {
  enc: fromHex('101112131415161718191A1B1C1D1E1F'),
  mac: fromHex('202122232425262728292A2B2C2D2E2F'),
  dek: fromHex('303132333435363738393A3B3C3D3E3F'),
};

/** The card's cryptogram does not match these keys. Nothing more was sent. */
export class WrongKeys extends Error {
  constructor(
    message = 'the card did not prove these keys; EXTERNAL AUTHENTICATE was not sent',
  ) {
    super(message);
    this.name = 'WrongKeys';
  }
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

/** What INITIALIZE UPDATE answers with: 28 bytes under SCP02, 32 under SCP03. */
export interface InitializeUpdateReply {
  diversification: number[];
  kvn: number;
  /** The protocol the card opens, byte 11: 02 or 03. */
  scp: number;
  /** SCP03's i parameter (byte 12); an SCP02 reply does not carry it. */
  i?: number;
  /** The sequence counter: 2 bytes under SCP02, 3 under SCP03. */
  seq: number[];
  /** 6 bytes under SCP02, 8 under SCP03. */
  cardChallenge: number[];
  cardCryptogram: number[];
}

/** One INITIALIZE UPDATE: what was sent, to which security domain, and what came back. */
export interface Handshake {
  hostChallenge: number[];
  /** The security domain selected for it; SCP03's card challenge is made over its AID. */
  sdAid: number[];
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

/**
 * INITIALIZE UPDATE's reply, by the protocol in byte 11: 28 bytes with 02 is
 * SCP02's; with 03 it is SCP03's, whose i (byte 12) sets its length and is
 * read first (parseScp03Reply), so a random challenge or S16 is refused for
 * that, and the one i taken, 10, gives 32 bytes. Anything else, an unknown
 * protocol or a length that does not go with the one named, is the card's
 * fault: a CardError, and nothing more is sent.
 */
export function parseInitializeUpdate(reply: Reply): InitializeUpdateReply {
  if (reply.sw !== 0x9000) {
    throw new CardError(reply.sw, 0x50);
  }
  const d = reply.data;
  const scp = d.length > 11 ? d[11] : -1;
  if (scp === 0x03) {
    return parseScp03Reply(reply);
  }
  if (scp !== 0x02) {
    throw new CardError(
      reply.sw,
      0x50,
      d.length === 28 || d.length === 32
        ? `the card opens SCP${toHex([scp])}, neither SCP02 nor SCP03`
        : `INITIALIZE UPDATE answered ${d.length} bytes, not 28 (SCP02) or 32 (SCP03)`,
    );
  }
  if (d.length !== 28) {
    throw new CardError(
      reply.sw,
      0x50,
      `INITIALIZE UPDATE answered ${d.length} bytes for SCP02, not 28`,
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

/**
 * Sends INITIALIZE UPDATE with a fresh host challenge to the security domain
 * `sdAid` names, which the caller has selected (the ISD by default).
 * Authenticates nothing.
 */
export async function initializeUpdate(
  tx: Transceiver,
  random: Random,
  kvn = 0,
  sdAid: Bytes = ISD_AID,
): Promise<Handshake> {
  const hostChallenge = bytes(random(8));
  const command = initializeUpdateCommand(hostChallenge, kvn);
  if (sdAid.length < 5 || sdAid.length > 16) {
    throw new RangeError('an AID is 5 to 16 bytes');
  }
  const aid = bytes(sdAid);
  return {
    hostChallenge,
    sdAid: aid,
    reply: parseInitializeUpdate(await transmit(tx, command)),
  };
}

/**
 * The session keys when the card proves `keys`, else null. Offline: sends
 * nothing. SCP02: the card cryptogram under S-ENC. SCP03: the pseudo-random
 * card challenge for the handshake's counter and security domain, and the
 * card cryptogram under S-MAC (scp03KeysIfMatches).
 */
export function sessionIfMatches(
  keys: KeySet,
  hs: Handshake,
): SessionKeys | null {
  if (hs.reply.scp === 0x03) {
    return scp03KeysIfMatches(keys, hs);
  }
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

/**
 * An open SCP02 channel at C-MAC level. Every command to the card manager
 * goes through `wrap` or `send`, in order; a command that is sent twice, or
 * not at all after being wrapped, breaks the chain, and the card ends the
 * session at the next one.
 */
export class Session {
  readonly scp = 2;
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
 * The channel `openSession` opens: SCP02's Session or SCP03's Scp03Session,
 * told apart by `scp`. Both wrap and send the same way, report the key
 * version they authenticated, and hold the DEK that `putKeys` encrypts under.
 */
export type SecureSession = Session | Scp03Session;

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

/**
 * EXTERNAL AUTHENTICATE at C-MAC level (84 82 01 00 under either protocol),
 * for keys `sessionIfMatches` has already proved. SCP03 ignores
 * `macOverModifiedApdu`: its C-MAC always covers the header as sent.
 */
export async function externalAuthenticate(
  tx: Transceiver,
  keys: SessionKeys,
  hs: Handshake,
  options: SessionOptions = {},
): Promise<SecureSession> {
  let session: SecureSession;
  let cryptogram: number[];
  if (hs.reply.scp === 0x03) {
    session = new Scp03Session(keys, hs.reply.kvn);
    cryptogram = scp03HostCryptogram(
      keys.mac,
      hs.hostChallenge,
      hs.reply.cardChallenge,
    );
  } else {
    session = new Session(keys, options, hs.reply.kvn);
    cryptogram = hostCryptogram(
      keys.enc,
      hs.reply.seq,
      hs.reply.cardChallenge,
      hs.hostChallenge,
    );
  }
  const reply = await session.send(tx, externalAuthenticateCommand(cryptogram));
  if (reply.sw !== 0x9000) {
    throw new CardError(reply.sw, 0x82);
  }
  return session;
}

export interface OpenSessionOptions extends SessionOptions {
  /** The key set INITIALIZE UPDATE names; 0, the default, lets the card choose. */
  kvn?: number;
  /** The security domain the caller has selected; the ISD by default. */
  sdAid?: Bytes;
}

/**
 * Opens a secure channel on the security domain the caller has selected:
 * INITIALIZE UPDATE, whose reply names SCP02 or SCP03; the card's proof of
 * the keys checked here, offline; then EXTERNAL AUTHENTICATE. Wrong keys, or
 * an SCP03 challenge made for another security domain, throw WrongKeys with
 * nothing sent after INITIALIZE UPDATE.
 */
export async function openSession(
  tx: Transceiver,
  keys: KeySet,
  random: Random,
  options: OpenSessionOptions = {},
): Promise<SecureSession> {
  const hs = await initializeUpdate(
    tx,
    random,
    options.kvn ?? 0,
    options.sdAid ?? ISD_AID,
  );
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
 * GET DATA 00E0, the key information template. Needs no key: gp -i sends it
 * with no secure channel, and the J3R180 (gp-info.txt) and the J3R452 answer
 * it after a SELECT of the card manager. First contact reads it (D182).
 */
export function getKeyInformation(): number[] {
  return [0x80, 0xca, 0x00, 0xe0, 0x00];
}

/** One key in a key information template. */
export interface KeyInformation {
  id: number;
  kvn: number;
  /** Type and length of each of the key's components (one for a DES3 or an AES key). */
  components: {type: number; length: number}[];
}

/** A BER-TLV of `data` at `at`: tag (one byte), length (one byte, or 81 and one), value. */
function keyInformationTlv(
  data: number[],
  at: number,
): {tag: number; value: number[]; end: number} {
  const tag = data[at];
  let length = data[at + 1];
  let start = at + 2;
  if (length === 0x81) {
    length = data[at + 2];
    start = at + 3;
  } else if (length !== undefined && length >= 0x80) {
    throw new Error(
      `a TLV at ${at} with a length form this reader does not take`,
    );
  }
  if (
    tag === undefined ||
    length === undefined ||
    start + length > data.length
  ) {
    throw new Error(`a TLV at ${at} that runs past the data`);
  }
  return {tag, value: data.slice(start, start + length), end: start + length};
}

/**
 * GET DATA 00E0's answer, the key information template, as gp -i reads it
 * (GPKeyInfo.parseTemplate): E0, then for each key C0 with the key ID, the
 * key set version and each component's type and length. Throws for anything
 * else; the extended form (FF in place of a type) is refused, not guessed at.
 */
export function parseKeyInformation(data: Bytes): KeyInformation[] {
  const d = bytes(data);
  const template = keyInformationTlv(d, 0);
  if (template.tag !== 0xe0 || template.end !== d.length) {
    throw new Error(`not a key information template: ${toHex(d)}`);
  }
  const keys: KeyInformation[] = [];
  for (let at = 0; at < template.value.length;) {
    const entry = keyInformationTlv(template.value, at);
    at = entry.end;
    const v = entry.value;
    if (entry.tag !== 0xc0 || v.length < 4 || v.length % 2 !== 0) {
      throw new Error(`not a key information entry: ${toHex(v)}`);
    }
    if (v[2] === 0xff) {
      throw new Error('extended key information is not read here');
    }
    const components: KeyInformation['components'] = [];
    for (let i = 2; i < v.length; i += 2) {
      components.push({type: v[i], length: v[i + 1]});
    }
    keys.push({id: v[0], kvn: v[1], components});
  }
  return keys;
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
 * ID 1, more to follow), the new set's version, then ENC, MAC and DEK, and
 * Le 00. The key type follows the protocol the session opened, never a
 * caller's flag. SCP02: each key 80 10 <the key under S-DEK, 3DES-ECB> 03
 * <its KCV>, as gp --lock sends it on a card whose set is version 01 (P1 01,
 * new version 01, in place). SCP03: each 88 11 10 <the key under the static
 * DEK, AES-ECB> 03 <its AES KCV>, the shape gp sent the J3R452.
 */
export function putKeys(
  keys: KeySet,
  session: SecureSession,
  options: {p1: number; newKvn: number},
): number[] {
  const dek = session.keys.dek;
  const data: number[] = [options.newKvn & 0xff];
  for (const key of [keys.enc, keys.mac, keys.dek]) {
    if (session.scp === 3) {
      if (key.length !== 16) {
        throw new RangeError('an AES-128 key is 16 bytes');
      }
      data.push(0x88, 0x11, 0x10, ...aesEcb(dek, key), 0x03, ...aesKcv(key));
    } else {
      if (key.length !== 16) {
        throw new RangeError('a DES3 key is 16 bytes');
      }
      data.push(0x80, 0x10, ...tdesEcb(dek, key), 0x03, ...kcv(key));
    }
  }
  return concat(
    [0x80, 0xd8, options.p1 & 0xff, 0x81, data.length],
    data,
    [0x00],
  );
}
