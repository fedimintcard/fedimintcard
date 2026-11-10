/**
 * GlobalPlatform Secure Channel Protocol 03 (Card Specification 2.3,
 * Amendment D) at security level 01, C-MAC only, with AES-128 keys: what
 * GlobalPlatformPro v25.10.20 runs on the J3R452 (JCOP 4.5), whose
 * INITIALIZE UPDATE answers i=10 (a pseudo-random card challenge, no R-MAC).
 * The hub in scp02.ts reads the protocol from byte 11 of that reply and comes
 * here for 03; nothing outside it needs to know which protocol a card speaks.
 *
 * The rule is SCP02's: nothing is sent after INITIALIZE UPDATE unless the
 * card has proved the keys offline, because a failed EXTERNAL AUTHENTICATE
 * counts towards a lock that is for good. Under SCP03 the card proves them
 * twice: its challenge is the KDF of the static ENC key over the sequence
 * counter and the security domain's AID, and its cryptogram the KDF of S-MAC
 * over both challenges. Both are checked here, in constant time. A reply that
 * reports a random challenge (i without 10) is refused: its cryptogram would
 * still prove the MAC key, which is all EXTERNAL AUTHENTICATE uses at level 01,
 * but only the pseudo-random challenge proves the ENC key and the security
 * domain, and no card here opens i=00. So is S16 (i with 01), whose 16-byte
 * challenges, cryptograms and MACs are not written here.
 *
 * At level 01 the card's replies carry data and SW only: no R-MAC is expected,
 * stripped or checked. S-ENC is derived (constant 04) as the specification
 * has it, but nothing at level 01 uses it, so no capture has proved it.
 *
 * AES is @noble/ciphers' (D181); CMAC (NIST SP 800-38B) and the KDF (NIST SP
 * 800-108 in counter mode, as Amendment D uses it) are written here and held
 * to NIST's vectors and to the J3R452's captures (gp/test/fixtures).
 */
import {ecb} from '@noble/ciphers/aes';

import {
  CardError,
  type KeySet,
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
  equalConstantTime,
  toHex,
  xor,
} from './bytes';

const BLOCK = 16;

/** Amendment D's derivation constants: what each KDF output is for. */
const CARD_CRYPTOGRAM = 0x00;
const HOST_CRYPTOGRAM = 0x01;
const CARD_CHALLENGE = 0x02;
const S_ENC = 0x04;
const S_MAC = 0x06;

/** The bits of i this module reads (Amendment D, Table 5-1). */
const I_PSEUDO_RANDOM = 0x10;
const I_S16 = 0x01;

/** An AES-128 key as a fresh Uint8Array; any other length is a caller's fault. */
function aesKey(key: Bytes): Uint8Array {
  if (key.length !== BLOCK) {
    throw new RangeError(`an AES-128 key is 16 bytes, not ${key.length}`);
  }
  return Uint8Array.from(bytes(key));
}

/** AES-128 in ECB mode over whole blocks, without padding. */
export function aesEcb(key: Bytes, data: Bytes, decrypt = false): number[] {
  const d = bytes(data);
  if (d.length === 0 || d.length % BLOCK !== 0) {
    throw new RangeError(`AES-ECB takes whole 16-byte blocks, not ${d.length}`);
  }
  // a noble cipher object encrypts once, so each call gets its own
  const cipher = ecb(aesKey(key), {disablePadding: true});
  const input = Uint8Array.from(d);
  return Array.from(decrypt ? cipher.decrypt(input) : cipher.encrypt(input));
}

/** SP 800-38B's doubling in GF(2^128): shift left one bit, XOR 87 into the last byte when the top bit falls off. */
function double(block: number[]): number[] {
  const out = new Array<number>(BLOCK);
  for (let i = 0; i < BLOCK; i++) {
    out[i] = ((block[i] << 1) | (i + 1 < BLOCK ? block[i + 1] >> 7 : 0)) & 0xff;
  }
  if (block[0] & 0x80) {
    out[BLOCK - 1] ^= 0x87;
  }
  return out;
}

/**
 * AES-CMAC (NIST SP 800-38B) of `data`: all 16 bytes. A complete last block
 * is XORed with K1; a short or empty one is padded with 80 and zeros and
 * XORed with K2; then CBC under the key from a zero IV, the last block out.
 */
export function cmac(key: Bytes, data: Bytes): number[] {
  const k = bytes(key);
  const m = bytes(data);
  const k1 = double(aesEcb(k, new Array(BLOCK).fill(0)));
  const k2 = double(k1);
  const blocks = Math.max(1, Math.ceil(m.length / BLOCK));
  const tail = m.slice((blocks - 1) * BLOCK);
  const last =
    m.length > 0 && m.length % BLOCK === 0
      ? xor(tail, k1)
      : xor(
          concat(tail, [0x80], new Array(BLOCK - 1 - tail.length).fill(0)),
          k2,
        );
  let chain = new Array<number>(BLOCK).fill(0);
  for (let i = 0; i < blocks - 1; i++) {
    chain = aesEcb(k, xor(chain, m.slice(i * BLOCK, (i + 1) * BLOCK)));
  }
  return aesEcb(k, xor(chain, last));
}

/**
 * Amendment D's KDF: the first `lengthBits` / 8 bytes of CMAC(key, eleven
 * zeros | constant | 00 | L (two bytes, in bits) | 01 | context). AES-128
 * needs one block, so the counter is always 01: 64 bits for a challenge or a
 * cryptogram, 128 for a session key.
 */
export function scp03Kdf(
  key: Bytes,
  constant: number,
  lengthBits: 64 | 128,
  context: Bytes,
): number[] {
  if (lengthBits !== 64 && lengthBits !== 128) {
    throw new RangeError(
      `the KDF makes 64 or 128 bits here, not ${lengthBits}`,
    );
  }
  const label = concat(new Array(11).fill(0), [
    constant & 0xff,
    0x00,
    lengthBits >> 8,
    lengthBits & 0xff,
    0x01,
  ]);
  return cmac(key, concat(label, context)).slice(0, lengthBits / 8);
}

/** An AES key's check value: the first three bytes of its encryption of sixteen 01 bytes. */
export function aesKcv(key: Bytes): number[] {
  return aesEcb(key, new Array(BLOCK).fill(0x01)).slice(0, 3);
}

/** The pseudo-random card challenge (i=10): the static ENC key's KDF over the counter and the security domain's AID. */
export function scp03CardChallenge(
  staticEnc: Bytes,
  seq: Bytes,
  sdAid: Bytes,
): number[] {
  return scp03Kdf(staticEnc, CARD_CHALLENGE, 64, concat(seq, sdAid));
}

/** The card's cryptogram: S-MAC's KDF over host ‖ card challenge, constant 00. */
export function scp03CardCryptogram(
  sMac: Bytes,
  hostChallenge: Bytes,
  cardChallenge: Bytes,
): number[] {
  return scp03Kdf(
    sMac,
    CARD_CRYPTOGRAM,
    64,
    concat(hostChallenge, cardChallenge),
  );
}

/** The host's cryptogram: S-MAC's KDF over host ‖ card challenge, constant 01. */
export function scp03HostCryptogram(
  sMac: Bytes,
  hostChallenge: Bytes,
  cardChallenge: Bytes,
): number[] {
  return scp03Kdf(
    sMac,
    HOST_CRYPTOGRAM,
    64,
    concat(hostChallenge, cardChallenge),
  );
}

/**
 * The session keys of one SCP03 channel: S-ENC (constant 04, from the static
 * ENC key) and S-MAC (06, from the static MAC key), each over host ‖ card
 * challenge, and the static DEK, which SCP03 uses as it is for PUT KEY.
 */
export function scp03SessionKeys(
  keys: KeySet,
  hostChallenge: Bytes,
  cardChallenge: Bytes,
): SessionKeys {
  const context = concat(hostChallenge, cardChallenge);
  return {
    enc: scp03Kdf(keys.enc, S_ENC, 128, context),
    mac: scp03Kdf(keys.mac, S_MAC, 128, context),
    dek: bytes(keys.dek),
  };
}

/**
 * How long INITIALIZE UPDATE's reply under SCP03 is for `i`, as Amendment D's
 * Table 7-3 lays it out and GlobalPlatformPro v25.10.20's GPSession sizes it:
 * 29 (diversification data, key information, and S8's challenge and
 * cryptogram), 3 more for the counter a pseudo-random challenge (10) brings,
 * and 16 more in S16 mode (01). The one i this module takes, 10, gives 32.
 */
export function scp03ReplyLength(i: number): number {
  return 29 + (i & I_PSEUDO_RANDOM ? 3 : 0) + (i & I_S16 ? 16 : 0);
}

/** The 32 bytes INITIALIZE UPDATE answers with under SCP03 at i=10. */
export interface Scp03Reply {
  diversification: number[];
  kvn: number;
  /** 03. */
  scp: number;
  /** The i parameter: 10 is a pseudo-random card challenge with no R-MAC. */
  i: number;
  /** The sequence counter, 3 bytes; the card advances it at every INITIALIZE UPDATE. */
  seq: number[];
  cardChallenge: number[];
  cardCryptogram: number[];
}

/**
 * INITIALIZE UPDATE's reply under SCP03: diversification data (10), key
 * version, 03, i, card challenge (8), card cryptogram (8), counter (3). What
 * the card sent is the card's, so every refusal is a CardError: a status
 * other than 9000, another protocol, an i this module does not run, and a
 * length that is not the one i gives (scp03ReplyLength). i is read before the
 * length, so a card that opens a random challenge (bit 10 clear; its
 * cryptogram proves the MAC key, but nothing then proves the ENC key or the
 * security domain, and no card here opens it) or S16 (bit 01) is refused for
 * that, whatever length it answered with.
 */
export function parseScp03Reply(reply: Reply): Scp03Reply {
  if (reply.sw !== 0x9000) {
    throw new CardError(reply.sw, 0x50);
  }
  const d = reply.data;
  if (d.length < 13) {
    throw new CardError(
      reply.sw,
      0x50,
      `INITIALIZE UPDATE answered ${d.length} bytes, too few for SCP03's key information`,
    );
  }
  if (d[11] !== 0x03) {
    throw new CardError(
      reply.sw,
      0x50,
      `the card opens SCP${toHex([d[11]])}, not SCP03`,
    );
  }
  const i = d[12];
  if ((i & I_PSEUDO_RANDOM) === 0) {
    throw new CardError(
      reply.sw,
      0x50,
      `the card opens SCP03 with i=${toHex([i])}, a random card challenge, which this module does not take (only a pseudo-random one proves the ENC key and the security domain)`,
    );
  }
  if ((i & I_S16) !== 0) {
    throw new CardError(
      reply.sw,
      0x50,
      `the card opens SCP03 with i=${toHex([i])}: S16 is not supported`,
    );
  }
  const length = scp03ReplyLength(i);
  if (d.length !== length) {
    throw new CardError(
      reply.sw,
      0x50,
      `INITIALIZE UPDATE answered ${d.length} bytes for SCP03, not ${length}`,
    );
  }
  return {
    diversification: d.slice(0, 10),
    kvn: d[10],
    scp: d[11],
    i,
    cardChallenge: d.slice(13, 21),
    cardCryptogram: d.slice(21, 29),
    seq: d.slice(29, 32),
  };
}

/** What the offline check reads from one INITIALIZE UPDATE: what went out, where, and what came back. */
export interface Scp03Handshake {
  readonly hostChallenge: Bytes;
  /** The security domain the INITIALIZE UPDATE went to. */
  readonly sdAid: Bytes;
  readonly reply: {
    readonly seq: Bytes;
    readonly cardChallenge: Bytes;
    readonly cardCryptogram: Bytes;
  };
}

/**
 * The session keys when the card proves `keys`, else null: its challenge
 * must be the one the static ENC key makes for this counter and security
 * domain, and its cryptogram the one S-MAC makes. Offline: sends nothing.
 */
export function scp03KeysIfMatches(
  keys: KeySet,
  hs: Scp03Handshake,
): SessionKeys | null {
  const {seq, cardChallenge, cardCryptogram} = hs.reply;
  const session = scp03SessionKeys(keys, hs.hostChallenge, cardChallenge);
  const challengeOk = equalConstantTime(
    scp03CardChallenge(keys.enc, seq, hs.sdAid),
    cardChallenge,
  );
  const cryptogramOk = equalConstantTime(
    scp03CardCryptogram(session.mac, hs.hostChallenge, cardChallenge),
    cardCryptogram,
  );
  // both are computed whatever the first says, and both must hold
  return challengeOk && cryptogramOk ? session : null;
}

/**
 * An open SCP03 channel at C-MAC level. Each command's MAC is the first 8
 * bytes of CMAC(S-MAC, chaining value | CLA|04, INS, P1, P2, Lc + 8 | data),
 * and the chaining value is the previous command's full 16-byte CMAC, 16
 * zeros before EXTERNAL AUTHENTICATE. Le stays outside the MAC. Every
 * command goes through `wrap` or `send`, in order: one sent twice, or not at
 * all after being wrapped, breaks the chain. Replies come back as data and
 * SW; at level 01 the card adds no R-MAC.
 */
export class Scp03Session {
  readonly scp = 3;
  readonly keys: SessionKeys;
  private chain: number[] = new Array(BLOCK).fill(0);

  constructor(
    keys: SessionKeys,
    /** The version observed in the authenticated INITIALIZE UPDATE, if known. */
    readonly keyVersion?: number,
  ) {
    this.keys = {
      enc: bytes(keys.enc),
      mac: bytes(keys.mac),
      dek: bytes(keys.dek),
    };
  }

  /** The APDU with its C-MAC: CLA|04, Lc + 8, the MAC after the data, Le kept. */
  wrap(apdu: Bytes): number[] {
    const c = parseCommand(apdu);
    if (c.data.length > 255 - 8) {
      throw new RangeError(
        `${c.data.length} bytes of data leave no room for the C-MAC`,
      );
    }
    const header = [c.cla | 0x04, c.ins, c.p1, c.p2, c.data.length + 8];
    const full = cmac(this.keys.mac, concat(this.chain, header, c.data));
    this.chain = full;
    return concat(
      header,
      c.data,
      full.slice(0, 8),
      c.le === null ? [] : [c.le],
    );
  }

  /** Wraps and sends one command. */
  send(tx: Transceiver, apdu: Bytes): Promise<Reply> {
    return transmit(tx, this.wrap(apdu));
  }
}
