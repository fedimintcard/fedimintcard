/**
 * SCP03 as GlobalPlatformPro v25.10.20 runs it on the J3R452 (i=10, level
 * 01): AES-128 against FIPS 197, CMAC against NIST SP 800-38B, both and the
 * KDF against node:crypto (gp/test/node-aes.ts) on random input, and the
 * handshake and C-MAC chain against the bytes of the owner's capture
 * scp03-07 (gp/test/fixtures), inlined here so they hold with no fixture.
 */
import assert from 'node:assert/strict';
import {randomBytes} from 'node:crypto';
import {describe, it} from 'node:test';

import {ISD_AID} from '../aids';
import {concat, fromHex, toHex} from '../bytes';
import {
  CardError,
  FACTORY_KEYS,
  SPIKE_KEYS,
  WrongKeys,
  externalAuthenticateCommand,
  openSession,
  parseInitializeUpdate,
  type KeySet,
  type Reply,
  type Transceiver,
} from '../scp02';
import {
  Scp03Session,
  aesEcb,
  aesKcv,
  cmac,
  parseScp03Reply,
  scp03CardChallenge,
  scp03CardCryptogram,
  scp03HostCryptogram,
  scp03Kdf,
  scp03KeysIfMatches,
  scp03ReplyLength,
  scp03SessionKeys,
} from '../scp03';
import {nodeAesEcb, nodeCmac, nodeScp03Kdf} from './node-aes';

function rnd(n: number): number[] {
  return Array.from(randomBytes(n));
}

/**
 * The first login of scp03-07 (`gp -d --key-ver 1 --list`), under the public
 * default 40..4F at key version 01: the host challenge, the card's 32-byte
 * reply, EXTERNAL AUTHENTICATE and the first GET STATUS, as on the wire.
 */
const T07 = {
  host: fromHex('2B A5 B2 1E 11 21 56 35'),
  reply: fromHex(
    '00005339367991214559' + // diversification: CPLC date, serial, batch
      '01' + // key version
      '03' + // SCP03
      '10' + // i: pseudo-random challenge, no R-MAC
      '86C8BD65FA1044EE' + // card challenge
      '176B3CB5B20C6617' + // card cryptogram
      '000001', // sequence counter
  ),
  externalAuthenticate: fromHex(
    '8482010010' + 'CDB1E7EE99DF534D' + '8E457A5C3CF8C7A2' + '00',
  ),
  getStatus: fromHex('84F280020A4F00' + 'D0C82BCA7D2B4A11' + '00'),
};

const ok = (data: number[]) => ({data, sw: 0x9000});

describe('SCP03: AES-128 and CMAC', () => {
  it('encrypts FIPS 197 C.1 and Appendix B, and decrypts them back', () => {
    for (const [key, plain, cipher] of [
      [
        '000102030405060708090A0B0C0D0E0F',
        '00112233445566778899AABBCCDDEEFF',
        '69C4E0D86A7B0430D8CDB78070B4C55A',
      ],
      [
        '2B7E151628AED2A6ABF7158809CF4F3C',
        '3243F6A8885A308D313198A2E0370734',
        '3925841D02DC09FBDC118597196A0B32',
      ],
    ]) {
      assert.equal(toHex(aesEcb(fromHex(key), fromHex(plain))), cipher);
      assert.equal(toHex(aesEcb(fromHex(key), fromHex(cipher), true)), plain);
    }
  });

  it('holds AES-ECB to node:crypto on random keys and data, and takes whole blocks of a 16-byte key only', () => {
    for (let i = 0; i < 50; i++) {
      const key = rnd(16);
      const data = rnd(16 * (1 + (i % 4)));
      assert.deepEqual(aesEcb(key, data), nodeAesEcb(key, data));
      assert.deepEqual(aesEcb(key, data, true), nodeAesEcb(key, data, true));
    }
    assert.throws(() => aesEcb(rnd(16), rnd(15)), RangeError);
    assert.throws(() => aesEcb(rnd(16), []), RangeError);
    assert.throws(() => aesEcb(rnd(24), rnd(16)), RangeError);
    assert.throws(() => aesEcb(rnd(8), rnd(16)), RangeError);
  });

  it("gives SP 800-38B's four AES-128 CMAC examples: 0, 16, 40 and 64 bytes", () => {
    const key = fromHex('2B7E151628AED2A6ABF7158809CF4F3C');
    const m = fromHex(
      '6BC1BEE22E409F96E93D7E117393172A' +
        'AE2D8A571E03AC9C9EB76FAC45AF8E51' +
        '30C81C46A35CE411E5FBC1191A0A52EF' +
        'F69F2445DF4F9B17AD2B417BE66C3710',
    );
    assert.equal(toHex(cmac(key, [])), 'BB1D6929E95937287FA37D129B756746');
    assert.equal(
      toHex(cmac(key, m.slice(0, 16))),
      '070A16B46B4D4144F79BDD9DD04A287C',
    );
    assert.equal(
      toHex(cmac(key, m.slice(0, 40))),
      'DFA66747DE9AE63030CA32611497C827',
    );
    assert.equal(toHex(cmac(key, m)), '51F0BEBF7E3B9D92FC49741779363CFE');
  });

  it('holds CMAC to node:crypto at every length from 0 to 80 bytes, and never writes to its input', () => {
    const key = rnd(16);
    for (let n = 0; n <= 80; n++) {
      const data = rnd(n);
      const copy = toHex(data);
      assert.deepEqual(cmac(key, data), nodeCmac(key, data), `${n} bytes`);
      assert.equal(toHex(data), copy);
    }
  });

  it("makes an AES key's check value from sixteen 01 bytes: 504A77 for the public default", () => {
    assert.equal(toHex(aesKcv(FACTORY_KEYS.enc)), '504A77');
    for (let i = 0; i < 20; i++) {
      const key = rnd(16);
      assert.deepEqual(
        aesKcv(key),
        nodeAesEcb(key, new Array(16).fill(1)).slice(0, 3),
      );
    }
  });
});

describe("SCP03: Amendment D's KDF", () => {
  it('holds the KDF to node:crypto for the five constants this module uses, at 64 and 128 bits', () => {
    for (let i = 0; i < 20; i++) {
      const key = rnd(16);
      const context = rnd(16);
      for (const constant of [0x00, 0x01, 0x02, 0x04, 0x06]) {
        for (const bits of [64, 128] as const) {
          assert.deepEqual(
            scp03Kdf(key, constant, bits, context),
            nodeScp03Kdf(key, constant, bits, context),
          );
        }
      }
    }
    const unsupported = 192 as number;
    assert.throws(
      () => scp03Kdf(rnd(16), 0x06, unsupported as 128, rnd(16)),
      RangeError,
    );
  });

  it("makes scp03-07's card challenge from the static ENC key, the counter and the ISD AID (constant 02)", () => {
    assert.equal(
      toHex(scp03CardChallenge(FACTORY_KEYS.enc, fromHex('000001'), ISD_AID)),
      '86C8BD65FA1044EE',
    );
    assert.equal(
      toHex(
        scp03Kdf(
          FACTORY_KEYS.enc,
          0x02,
          64,
          concat(fromHex('000001'), ISD_AID),
        ),
      ),
      '86C8BD65FA1044EE',
    );
  });

  it("makes scp03-07's card cryptogram (00) and host cryptogram (01) under S-MAC (06) over both challenges", () => {
    const card = fromHex('86C8BD65FA1044EE');
    const s = scp03SessionKeys(FACTORY_KEYS, T07.host, card);
    const context = concat(T07.host, card);
    assert.deepEqual(s.mac, nodeScp03Kdf(FACTORY_KEYS.mac, 0x06, 128, context));
    assert.equal(
      toHex(scp03CardCryptogram(s.mac, T07.host, card)),
      '176B3CB5B20C6617',
    );
    assert.equal(
      toHex(scp03HostCryptogram(s.mac, T07.host, card)),
      'CDB1E7EE99DF534D',
    );
  });

  it('derives S-ENC by constant 04 and keeps the static DEK, neither of which a level-01 capture can prove', () => {
    const host = rnd(8);
    const card = rnd(8);
    const keys = {enc: rnd(16), mac: rnd(16), dek: rnd(16)};
    const s = scp03SessionKeys(keys, host, card);
    assert.deepEqual(
      s.enc,
      nodeScp03Kdf(keys.enc, 0x04, 128, concat(host, card)),
    );
    assert.deepEqual(s.dek, keys.dek);
  });
});

describe("SCP03: INITIALIZE UPDATE's reply", () => {
  it("parses scp03-07's 32 bytes", () => {
    assert.deepEqual(parseScp03Reply(ok(T07.reply)), {
      diversification: fromHex('00005339367991214559'),
      kvn: 0x01,
      scp: 0x03,
      i: 0x10,
      cardChallenge: fromHex('86C8BD65FA1044EE'),
      cardCryptogram: fromHex('176B3CB5B20C6617'),
      seq: fromHex('000001'),
    });
  });

  it("sizes the reply from i as Amendment D's Table 7-3 and gp's GPSession do: 29, 3 more with the counter (10), 16 more in S16 (01)", () => {
    for (const [i, length] of [
      [0x00, 29], // a random challenge: no counter
      [0x10, 32], // scp03-07's
      [0x01, 45], // S16, random
      [0x11, 48], // S16 with the counter
      [0x30, 32], // R-MAC support adds nothing to the reply
      [0x70, 32], // nor does R-ENC
    ]) {
      assert.equal(scp03ReplyLength(i), length, `i=${toHex([i])}`);
    }
    assert.equal(scp03ReplyLength(T07.reply[12]), T07.reply.length);
  });

  it("refuses, as the card's fault, an error status, a length i does not give, another protocol, a random challenge and S16", () => {
    const with12 = (i: number) => {
      const r = T07.reply.slice();
      r[12] = i;
      return r;
    };
    const with11 = T07.reply.slice();
    with11[11] = 0x02;
    // the lengths a card that opens these answers with (scp03ReplyLength)
    const randomChallenge = concat(
      T07.reply.slice(0, 12),
      [0x00],
      T07.reply.slice(13, 29),
    );
    const s16 = (i: number, counter: boolean) =>
      concat(
        T07.reply.slice(0, 12),
        [i],
        rnd(16),
        rnd(16),
        counter ? T07.reply.slice(29) : [],
      );
    assert.equal(randomChallenge.length, 29);
    assert.equal(s16(0x11, true).length, 48);
    assert.equal(s16(0x01, false).length, 45);
    const refused: [Reply, RegExp][] = [
      [{data: [], sw: 0x6a88}, /6A88/],
      [ok(T07.reply.slice(0, 12)), /12 bytes, too few/],
      [ok(T07.reply.slice(0, 28)), /28 bytes for SCP03, not 32/],
      [ok(T07.reply.slice(0, 29)), /29 bytes for SCP03, not 32/],
      [ok(with11), /SCP02, not SCP03/],
      // i is read before the length: refused for its cause at the length it comes in
      [ok(randomChallenge), /i=00, a random card challenge/],
      [ok(s16(0x11, true)), /i=11: S16 is not supported/],
      [ok(s16(0x01, false)), /i=01, a random card challenge/],
      // and at a length that does not go with it
      [ok(with12(0x00)), /i=00, a random card challenge/],
      [ok(with12(0x20)), /i=20, a random card challenge/],
      [ok(with12(0x11)), /i=11: S16 is not supported/],
    ];
    for (const [reply, why] of refused) {
      assert.throws(
        () => parseScp03Reply(reply),
        (e: unknown) =>
          e instanceof CardError && e.ins === 0x50 && why.test(e.message),
        why.source,
      );
    }
    // R-MAC and R-ENC support do not matter at level 01
    for (const i of [0x30, 0x70]) {
      assert.equal(parseScp03Reply(ok(with12(i))).i, i);
    }
  });
});

describe('SCP03: the offline check of the card', () => {
  const hs = (overrides: {sdAid?: number[]; reply?: number[]} = {}) => {
    const reply = parseScp03Reply(ok(overrides.reply ?? T07.reply));
    return {
      hostChallenge: T07.host,
      sdAid: overrides.sdAid ?? ISD_AID,
      reply,
    };
  };

  it("proves the public default on scp03-07's handshake, and gives S-ENC, S-MAC and the static DEK", () => {
    const s = scp03KeysIfMatches(FACTORY_KEYS, hs());
    assert.ok(s);
    assert.deepEqual(
      s,
      scp03SessionKeys(FACTORY_KEYS, T07.host, fromHex('86C8BD65FA1044EE')),
    );
  });

  it('refuses another key set, another security domain, and a cryptogram that matches over a challenge that does not', () => {
    const other = {enc: rnd(16), mac: rnd(16), dek: rnd(16)};
    assert.equal(scp03KeysIfMatches(other, hs()), null);
    // the right MAC key with the wrong ENC key: the cryptogram holds, the challenge does not
    assert.equal(
      scp03KeysIfMatches({...FACTORY_KEYS, enc: rnd(16)}, hs()),
      null,
    );
    assert.equal(
      scp03KeysIfMatches(
        FACTORY_KEYS,
        hs({sdAid: fromHex('A000000003000000')}),
      ),
      null,
    );
    // a card that made its cryptogram over a challenge the counter and AID do not give
    const card = rnd(8);
    const s = scp03SessionKeys(FACTORY_KEYS, T07.host, card);
    const forged = concat(
      T07.reply.slice(0, 13),
      card,
      scp03CardCryptogram(s.mac, T07.host, card),
      T07.reply.slice(29),
    );
    assert.equal(scp03KeysIfMatches(FACTORY_KEYS, hs({reply: forged})), null);
  });
});

describe('SCP03: the C-MAC chain', () => {
  const t07Keys = () =>
    scp03SessionKeys(FACTORY_KEYS, T07.host, fromHex('86C8BD65FA1044EE'));

  it("wraps scp03-07's EXTERNAL AUTHENTICATE and the GET STATUS after it byte for byte: chained from 16 zeros, then from the full CMAC", () => {
    const session = new Scp03Session(t07Keys(), 0x01);
    assert.equal(session.scp, 3);
    assert.equal(session.keyVersion, 0x01);
    assert.equal(
      toHex(
        session.wrap(externalAuthenticateCommand(fromHex('CDB1E7EE99DF534D'))),
      ),
      toHex(T07.externalAuthenticate),
    );
    assert.equal(
      toHex(session.wrap(fromHex('80F28002024F0000'))),
      toHex(T07.getStatus),
    );
  });

  it("chains each command from the previous one's full 16-byte CMAC, over CLA|04 and Lc + 8, with Le outside it", () => {
    const keys = {enc: rnd(16), mac: rnd(16), dek: rnd(16)};
    const session = new Scp03Session(keys);
    const commands = [
      externalAuthenticateCommand(rnd(8)), // case 4: Le 00 after the MAC
      fromHex('80CAFF2100'), // case 2: Le kept after the MAC
      fromHex('80E40000094F07D2760000850101'), // case 3 stays case 3
      fromHex('80F24002024F0000'),
      fromHex('80E8000003010203' + '00'),
    ];
    let chain = new Array(16).fill(0);
    for (const c of commands) {
      const hasData = c.length > 5;
      const lc = hasData ? c[4] : 0;
      const data = hasData ? c.slice(5, 5 + lc) : [];
      const le =
        c.length === 5 ? [c[4]] : c.length > 5 + lc ? [c[c.length - 1]] : [];
      const header = [c[0] | 0x04, c[1], c[2], c[3], lc + 8];
      const full = nodeCmac(keys.mac, concat(chain, header, data));
      assert.equal(
        toHex(session.wrap(c)),
        toHex(concat(header, data, full.slice(0, 8), le)),
      );
      chain = full;
    }
  });

  it('refuses a command whose data leaves no room for the MAC, and a malformed one', () => {
    const session = new Scp03Session({
      enc: rnd(16),
      mac: rnd(16),
      dek: rnd(16),
    });
    assert.throws(
      () =>
        session.wrap(
          concat([0x80, 0xe8, 0x00, 0x00, 248], new Array(248).fill(0)),
        ),
      RangeError,
    );
    assert.doesNotThrow(() =>
      session.wrap(
        concat([0x80, 0xe8, 0x00, 0x00, 247], new Array(247).fill(0)),
      ),
    );
    assert.throws(
      () => session.wrap([0x80, 0xe8, 0x00, 0x00, 0x05, 1, 2]),
      RangeError,
    );
  });

  it('never writes to the command or the keys it is given', () => {
    const keys = {enc: rnd(16), mac: rnd(16), dek: rnd(16)};
    const before = [toHex(keys.enc), toHex(keys.mac), toHex(keys.dek)];
    const session = new Scp03Session(keys);
    const c = fromHex('80E40000094F07D2760000850101');
    const copy = toHex(c);
    session.wrap(c);
    session.wrap(c);
    assert.equal(toHex(c), copy);
    assert.deepEqual(
      [toHex(keys.enc), toHex(keys.mac), toHex(keys.dek)],
      before,
    );
  });
});

/**
 * A J3R452's card manager for one login, computed with node:crypto: it
 * answers INITIALIZE UPDATE for `keys` with the pseudo-random challenge for
 * `sdAid` and `seq`, EXTERNAL AUTHENTICATE with 9000 when its host cryptogram
 * and C-MAC are right, and every later command with `after`.
 */
function scriptedCard(
  options: {
    keys?: KeySet;
    sdAid?: number[];
    seq?: number[];
    challenge?: (made: number[]) => number[];
    initializeUpdate?: (reply: number[]) => number[];
    after?: number[];
  } = {},
) {
  const keys = options.keys ?? FACTORY_KEYS;
  const seq = options.seq ?? [0x00, 0x00, 0x2a];
  const sent: number[][] = [];
  let context: number[] = [];
  const tx: Transceiver = async apdu => {
    sent.push(apdu);
    if (apdu[1] === 0x50) {
      const host = apdu.slice(5, 13);
      const made = nodeScp03Kdf(
        keys.enc,
        0x02,
        64,
        concat(seq, options.sdAid ?? ISD_AID),
      );
      const card = options.challenge ? options.challenge(made) : made;
      context = concat(host, card);
      const sMac = nodeScp03Kdf(keys.mac, 0x06, 128, context);
      const reply = concat(
        new Array(10).fill(0x11),
        [0x01, 0x03, 0x10],
        card,
        nodeScp03Kdf(sMac, 0x00, 64, context),
        seq,
      );
      return concat(
        options.initializeUpdate ? options.initializeUpdate(reply) : reply,
        [0x90, 0x00],
      );
    }
    if (apdu[1] === 0x82) {
      const sMac = nodeScp03Kdf(keys.mac, 0x06, 128, context);
      const host = nodeScp03Kdf(sMac, 0x01, 64, context);
      const mac = nodeCmac(
        sMac,
        concat(new Array(16).fill(0), apdu.slice(0, 13)),
      ).slice(0, 8);
      const good =
        apdu.length === 22 &&
        toHex(apdu.slice(5, 13)) === toHex(host) &&
        toHex(apdu.slice(13, 21)) === toHex(mac);
      return good ? [0x90, 0x00] : [0x63, 0x00];
    }
    return options.after ?? [0x90, 0x00];
  };
  return {tx, sent};
}

describe('SCP03 through openSession: one way in, the protocol read from byte 11', () => {
  it('reads an SCP03 reply from byte 11 and logs in: INITIALIZE UPDATE, then EXTERNAL AUTHENTICATE 84 82 01 00 10 with the host cryptogram, its C-MAC and Le 00', async () => {
    const card = scriptedCard();
    const host = rnd(8);
    const session = await openSession(card.tx, FACTORY_KEYS, () => host);
    assert.ok(session instanceof Scp03Session);
    assert.equal(session.scp, 3);
    assert.equal(session.keyVersion, 0x01);
    assert.equal(card.sent.length, 2);
    assert.equal(toHex(card.sent[0]), '8050000008' + toHex(host) + '00');
    assert.deepEqual(card.sent[1].slice(0, 5), [0x84, 0x82, 0x01, 0x00, 0x10]);
    assert.equal(card.sent[1][21], 0x00, 'Le 00 after the MAC');
    assert.deepEqual(
      parseInitializeUpdate({data: T07.reply, sw: 0x9000}),
      parseScp03Reply(ok(T07.reply)),
    );
  });

  it('refuses 28 bytes with 03, 32 with 02, 29 bytes, an i without 10, S16 and an error status, each as a CardError after the one APDU', async () => {
    const replies: [string, (r: number[]) => number[], RegExp][] = [
      ['28 bytes with 03', r => r.slice(0, 28), /28 bytes for SCP03, not 32/],
      [
        '32 bytes with 02',
        r => concat(r.slice(0, 11), [0x02], r.slice(12)),
        /32 bytes for SCP02, not 28/,
      ],
      ['29 bytes', r => r.slice(0, 29), /29 bytes for SCP03, not 32/],
      [
        'i 00 in 29 bytes, as a random challenge comes: no counter',
        r => concat(r.slice(0, 12), [0x00], r.slice(13, 29)),
        /i=00, a random card challenge/,
      ],
      [
        'i 11 in 48 bytes, as S16 comes: 16-byte challenge and cryptogram, then the counter',
        r => concat(r.slice(0, 12), [0x11], rnd(16), rnd(16), r.slice(29)),
        /i=11: S16 is not supported/,
      ],
      [
        'i 00 in 32 bytes',
        r => concat(r.slice(0, 12), [0x00], r.slice(13)),
        /i=00, a random card challenge/,
      ],
      [
        'protocol 01',
        r => concat(r.slice(0, 11), [0x01], r.slice(12)),
        /SCP01, neither SCP02 nor SCP03/,
      ],
    ];
    for (const [name, change, why] of replies) {
      const card = scriptedCard({initializeUpdate: change});
      await assert.rejects(
        openSession(card.tx, FACTORY_KEYS, () => rnd(8)),
        (e: unknown) =>
          e instanceof CardError && e.ins === 0x50 && why.test(e.message),
        name,
      );
      assert.equal(card.sent.length, 1, `${name}: only INITIALIZE UPDATE`);
    }
    const sent: number[][] = [];
    await assert.rejects(
      openSession(
        async apdu => (sent.push(apdu), [0x69, 0x85]),
        FACTORY_KEYS,
        () => rnd(8),
      ),
      (e: unknown) => e instanceof CardError && e.sw === 0x6985,
    );
    assert.equal(sent.length, 1);
  });

  it('sends no EXTERNAL AUTHENTICATE for another key set, or for a security domain whose AID the challenge was not made over', async () => {
    const wrongKeys = scriptedCard({keys: SPIKE_KEYS});
    await assert.rejects(
      openSession(wrongKeys.tx, FACTORY_KEYS, () => rnd(8)),
      WrongKeys,
    );
    assert.deepEqual(
      wrongKeys.sent.map(a => a[1]),
      [0x50],
    );
    const otherDomain = fromHex('A000000003000000');
    const wrongAid = scriptedCard({sdAid: otherDomain});
    await assert.rejects(
      openSession(wrongAid.tx, FACTORY_KEYS, () => rnd(8)),
      WrongKeys,
    );
    assert.deepEqual(
      wrongAid.sent.map(a => a[1]),
      [0x50],
    );
    // the same card, told its security domain, opens
    const rightAid = scriptedCard({sdAid: otherDomain});
    const session = await openSession(rightAid.tx, FACTORY_KEYS, () => rnd(8), {
      sdAid: otherDomain,
    });
    assert.equal(session.scp, 3);
  });

  it('sends no EXTERNAL AUTHENTICATE when the cryptogram matches but the challenge is not the one the keys, counter and AID make', async () => {
    const card = scriptedCard({challenge: () => rnd(8)});
    await assert.rejects(
      openSession(card.tx, FACTORY_KEYS, () => rnd(8)),
      WrongKeys,
    );
    assert.deepEqual(
      card.sent.map(a => a[1]),
      [0x50],
    );
  });

  it('turns a refused EXTERNAL AUTHENTICATE into a CardError with its SW', async () => {
    const card = scriptedCard();
    const tx: Transceiver = async apdu =>
      apdu[1] === 0x82 ? [0x63, 0x00] : card.tx(apdu);
    await assert.rejects(
      openSession(tx, FACTORY_KEYS, () => rnd(8)),
      (e: unknown) =>
        e instanceof CardError && e.sw === 0x6300 && e.ins === 0x82,
    );
  });

  it('expects no R-MAC at level 01: a reply comes back as the card sent it, its last eight bytes neither stripped nor checked', async () => {
    const data = rnd(20);
    const card = scriptedCard({after: concat(data, [0x90, 0x00])});
    const session = await openSession(card.tx, FACTORY_KEYS, () => rnd(8));
    const reply = await session.send(card.tx, fromHex('80F24002024F0000'));
    assert.deepEqual(reply, {data, sw: 0x9000});
    const bare = await session.send(card.tx, fromHex('80CAFF2100'));
    assert.deepEqual(bare, {data, sw: 0x9000});
  });
});
