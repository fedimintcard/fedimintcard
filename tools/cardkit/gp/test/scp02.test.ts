/**
 * SCP02 as GlobalPlatformPro v25.10.20 runs it on the J3R180 (i=55): session
 * keys, the two cryptograms and the C-MAC chain, each checked against
 * node:crypto; the handshake against a scripted card; and the command builders
 * byte for byte as gp puts them on the wire, where GPSession.transmit sends
 * every command it has wrapped with Le 00.
 *
 * Adapted from cashucard's tools/cardkit gp/test/scp02.test.ts: the AIDs and
 * the byte-for-byte literals are this card's, and the whitelist test names
 * three AIDs rather than six. See ../../NOTICE.
 */
import assert from 'node:assert/strict';
import {randomBytes} from 'node:crypto';
import {describe, it} from 'node:test';

import {
  CARD_AID,
  FEDIMINT_PACKAGE_AID,
  ISD_AID,
  OPENER_AID,
} from '../aids';
import {concat, fromHex, toHex} from '../bytes';
import {
  CardError,
  FACTORY_KEYS,
  SPIKE_KEYS,
  Session,
  TransportError,
  WrongKeys,
  cardCryptogram,
  deleteAid,
  deriveSessionKeys,
  externalAuthenticateCommand,
  getFreeMemory,
  hostCryptogram,
  installAndSelectable,
  installForLoad,
  kcv,
  openSession,
  parseCommand,
  parseFreeMemory,
  putKeys,
  selectAid,
  splitLoad,
  transmit,
  type KeySet,
  type Transceiver,
} from '../scp02';
import {
  nodeCbc,
  nodeEcb,
  nodeFullMac,
  nodeRetailMac,
} from './node-des';

function rnd(n: number): number[] {
  return Array.from(randomBytes(n));
}

function nodeSessionKey(
  key: ArrayLike<number>,
  constant: number[],
  seq: number[],
): number[] {
  return nodeCbc(key, concat(constant, seq, new Array(12).fill(0)));
}

/** A card that answers one INITIALIZE UPDATE for `keys`, then EXTERNAL AUTHENTICATE with 9000. */
function scriptedCard(
  keys: KeySet,
  seq = [0x00, 0x09],
  cardChallenge = rnd(6),
) {
  const sent: number[][] = [];
  const tx: Transceiver = async apdu => {
    sent.push(apdu);
    if (apdu[1] === 0x50) {
      const host = apdu.slice(5, 13);
      const sEnc = nodeSessionKey(keys.enc, [0x01, 0x82], seq);
      const cryptogram = nodeFullMac(sEnc, concat(host, seq, cardChallenge));
      return concat(
        new Array(10).fill(0x11),
        [0x01, 0x02],
        seq,
        cardChallenge,
        cryptogram,
        [0x90, 0x00],
      );
    }
    return [0x90, 0x00];
  };
  return {tx, sent, seq, cardChallenge};
}

describe('SCP02: keys and cryptograms', () => {
  it('derives S-ENC (0182), S-MAC (0101) and S-DEK (0181) by 3DES-CBC over constant, counter and twelve zeros', () => {
    for (let i = 0; i < 50; i++) {
      const keys = {enc: rnd(16), mac: rnd(16), dek: rnd(16)};
      const seq = rnd(2);
      const s = deriveSessionKeys(keys, seq);
      assert.deepEqual(s.enc, nodeSessionKey(keys.enc, [0x01, 0x82], seq));
      assert.deepEqual(s.mac, nodeSessionKey(keys.mac, [0x01, 0x01], seq));
      assert.deepEqual(s.dek, nodeSessionKey(keys.dek, [0x01, 0x81], seq));
    }
  });

  it('computes the card cryptogram over host, counter, card and the host cryptogram over counter, card, host', () => {
    for (let i = 0; i < 50; i++) {
      const sEnc = rnd(16);
      const host = rnd(8);
      const seq = rnd(2);
      const card = rnd(6);
      assert.deepEqual(
        cardCryptogram(sEnc, host, seq, card),
        nodeFullMac(sEnc, concat(host, seq, card)),
      );
      assert.deepEqual(
        hostCryptogram(sEnc, seq, card, host),
        nodeFullMac(sEnc, concat(seq, card, host)),
      );
    }
  });

  it('holds the factory key set and the spike set the plan names', () => {
    const k40 = '404142434445464748494A4B4C4D4E4F';
    assert.deepEqual(
      [
        toHex(FACTORY_KEYS.enc),
        toHex(FACTORY_KEYS.mac),
        toHex(FACTORY_KEYS.dek),
      ],
      [k40, k40, k40],
    );
    assert.deepEqual(
      [toHex(SPIKE_KEYS.enc), toHex(SPIKE_KEYS.mac), toHex(SPIKE_KEYS.dek)],
      [
        '101112131415161718191A1B1C1D1E1F',
        '202122232425262728292A2B2C2D2E2F',
        '303132333435363738393A3B3C3D3E3F',
      ],
    );
  });
});

describe('SCP02: the handshake', () => {
  it('sends INITIALIZE UPDATE 80 50 00 00 08 <challenge> 00, then EXTERNAL AUTHENTICATE 84 82 01 00 10 with the host cryptogram, its C-MAC and Le 00', async () => {
    const card = scriptedCard(FACTORY_KEYS);
    const host = rnd(8);
    const session = await openSession(card.tx, FACTORY_KEYS, () => host);
    assert.equal(card.sent.length, 2);
    assert.equal(toHex(card.sent[0]), '8050000008' + toHex(host) + '00');
    const s = deriveSessionKeys(FACTORY_KEYS, card.seq);
    const hc = nodeFullMac(s.enc, concat(card.seq, card.cardChallenge, host));
    // GlobalPlatformPro MACs the modified header: CLA 84 and Lc + 8 (D27; the H0 transcript confirms it)
    const mac = nodeRetailMac(s.mac, concat(fromHex('8482010010'), hc));
    // and sends it, as every command it wraps, with Le 00, which the MAC does not cover
    assert.equal(
      toHex(card.sent[1]),
      '8482010010' + toHex(hc) + toHex(mac) + '00',
    );
    assert.ok(session instanceof Session);
  });

  it('checks the card cryptogram before EXTERNAL AUTHENTICATE: wrong keys throw WrongKeys and send nothing more', async () => {
    const card = scriptedCard(SPIKE_KEYS);
    await assert.rejects(
      openSession(card.tx, FACTORY_KEYS, () => rnd(8)),
      WrongKeys,
    );
    assert.equal(card.sent.length, 1, 'only INITIALIZE UPDATE went out');
    assert.equal(card.sent[0][1], 0x50);
  });

  it('refuses a card that answers INITIALIZE UPDATE with an error or a short reply, sending nothing more', async () => {
    for (const reply of [
      [0x69, 0x83],
      [0x6a, 0x88],
      concat(new Array(12).fill(0), [0x90, 0x00]),
    ]) {
      const sent: number[][] = [];
      const tx: Transceiver = async apdu => {
        sent.push(apdu);
        return reply;
      };
      await assert.rejects(
        openSession(tx, FACTORY_KEYS, () => rnd(8)),
        CardError,
      );
      assert.equal(sent.length, 1);
    }
  });

  it('refuses a card that is not SCP02', async () => {
    const tx: Transceiver = async () =>
      concat(
        new Array(10).fill(0),
        [0x01, 0x03],
        new Array(16).fill(0),
        [0x90, 0x00],
      );
    await assert.rejects(
      openSession(tx, FACTORY_KEYS, () => rnd(8)),
      /SCP03|not SCP02/,
    );
  });

  it('turns a refused EXTERNAL AUTHENTICATE into a CardError with its SW', async () => {
    const card = scriptedCard(FACTORY_KEYS);
    const tx: Transceiver = async apdu =>
      apdu[1] === 0x82 ? [0x63, 0x00] : card.tx(apdu);
    await assert.rejects(
      openSession(tx, FACTORY_KEYS, () => rnd(8)),
      (e: unknown) =>
        e instanceof CardError && e.sw === 0x6300 && e.ins === 0x82,
    );
  });

  it('wants eight random bytes for the challenge', async () => {
    const card = scriptedCard(FACTORY_KEYS);
    await assert.rejects(
      openSession(card.tx, FACTORY_KEYS, () => rnd(7)),
      RangeError,
    );
    assert.equal(card.sent.length, 0);
  });
});

describe('SCP02: the C-MAC chain', () => {
  it('MACs each command over CLA|04 and Lc+8 with the previous C-MAC, single-DES encrypted under S-MAC[0..8], as the ICV', () => {
    const sMac = rnd(16);
    const session = new Session({enc: rnd(16), mac: sMac, dek: rnd(16)});
    const commands = [
      externalAuthenticateCommand(rnd(8)), // the zero ICV; Le 00 after the MAC
      deleteAid(CARD_AID), // case 4, as gp sends it: Le 00 after the MAC
      fromHex('80CAFF2100'), // case 2: Le kept after the MAC
      fromHex('80F24000024F00' + '00'),
      fromHex('80E8000003010203' + '00'),
      fromHex('80E40000094F07D2760000850101'), // case 3 stays case 3
    ];
    let icv = new Array(8).fill(0);
    let first = true;
    for (const c of commands) {
      const wrapped = session.wrap(c);
      const hasData = c.length > 5;
      const lc = hasData ? c[4] : 0;
      const data = hasData ? c.slice(5, 5 + lc) : [];
      const le =
        c.length === 5 ? [c[4]] : c.length > 5 + lc ? [c[c.length - 1]] : [];
      if (!first) {
        icv = nodeEcb(sMac.slice(0, 8), icv);
      }
      const mac = nodeRetailMac(
        sMac,
        concat([c[0] | 0x04, c[1], c[2], c[3], lc + 8], data),
        icv,
      );
      assert.equal(
        toHex(wrapped),
        toHex(concat([c[0] | 0x04, c[1], c[2], c[3], lc + 8], data, mac, le)),
      );
      icv = mac;
      first = false;
    }
  });

  it('MACs the unmodified header and Lc when macOverModifiedApdu is false, and still sends CLA|04 and Lc+8', () => {
    const sMac = rnd(16);
    const session = new Session(
      {enc: rnd(16), mac: sMac, dek: rnd(16)},
      {macOverModifiedApdu: false},
    );
    const c = fromHex('80E40000094F07D2760000850101');
    const mac = nodeRetailMac(sMac, c);
    assert.equal(
      toHex(session.wrap(c)),
      '84E40000114F07D2760000850101' + toHex(mac),
    );
  });

  it('defaults to the modified header, which GlobalPlatformPro v25.10.20 MACs for every SCP02 i', () => {
    assert.equal(
      new Session({enc: rnd(16), mac: rnd(16), dek: rnd(16)})
        .macOverModifiedApdu,
      true,
    );
  });

  it('never writes to the command or the keys it is given', () => {
    const keys = {enc: rnd(16), mac: rnd(16), dek: rnd(16)};
    const before = [toHex(keys.enc), toHex(keys.mac), toHex(keys.dek)];
    const session = new Session(keys);
    const c = installForLoad(FEDIMINT_PACKAGE_AID);
    const copy = toHex(c);
    session.wrap(c);
    session.wrap(c);
    assert.equal(toHex(c), copy);
    assert.deepEqual(
      [toHex(keys.enc), toHex(keys.mac), toHex(keys.dek)],
      before,
    );
  });

  it('refuses a command whose data leaves no room for the MAC', () => {
    const session = new Session({enc: rnd(16), mac: rnd(16), dek: rnd(16)});
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
});

describe('SCP02: replies', () => {
  it('splits data and SW, and throws TransportError for a reply under two bytes or a transceiver that throws', async () => {
    assert.deepEqual(await transmit(async () => [1, 2, 0x90, 0x00], [0]), {
      data: [1, 2],
      sw: 0x9000,
    });
    await assert.rejects(
      transmit(async () => [0x90], [0]),
      TransportError,
    );
    await assert.rejects(
      transmit(async () => [], [0]),
      TransportError,
    );
    await assert.rejects(
      transmit(async () => {
        throw new Error('TagLost');
      }, [0]),
      TransportError,
    );
  });
});

// gp's GPSession.transmit rebuilds every command it has wrapped as
// new CommandAPDU(cla, ins, p1, p2, data, 256), so each goes out with Le 00;
// SELECT, INITIALIZE UPDATE and GET DATA go unwrapped, as they were built.
describe('GP commands, byte for byte as GlobalPlatformPro v25.10.20 sends them', () => {
  it('SELECT by name, with Le', () => {
    assert.equal(toHex(selectAid(ISD_AID)), '00A4040008A00000015100000000');
    assert.equal(
      toHex(selectAid(CARD_AID)),
      '00A4040008F0464D434152440100',
    );
  });

  it('GET DATA FF21, and its parse', () => {
    assert.equal(toHex(getFreeMemory()), '80CAFF2100');
    assert.deepEqual(
      parseFreeMemory(fromHex('FF2110810200058204000280508304000012C0')),
      {
        applications: 5,
        nonVolatile: 0x00028050,
        volatile: 0x000012c0,
      },
    );
    assert.equal(parseFreeMemory(fromHex('6A88')), null);
  });

  it('EXTERNAL AUTHENTICATE 84 82 01 00 08: C-MAC only, the host cryptogram, Le 00', () => {
    assert.equal(
      toHex(externalAuthenticateCommand(fromHex('0102030405060708'))),
      '8482010008' + '0102030405060708' + '00',
    );
    assert.throws(
      () => externalAuthenticateCommand(new Array(7).fill(0)),
      RangeError,
    );
  });

  it('DELETE 80 E4 00 00 with 4F and the AID, and Le 00', () => {
    assert.equal(
      toHex(deleteAid(CARD_AID)),
      '80E400000A4F08F0464D434152440100',
    );
    assert.equal(
      toHex(deleteAid(FEDIMINT_PACKAGE_AID)),
      '80E40000094F07F0464D4341524400',
    );
  });

  it('DELETE names the whitelist and nothing else: two instances and the package, never the ISD', () => {
    for (const aid of [CARD_AID, OPENER_AID, FEDIMINT_PACKAGE_AID]) {
      assert.doesNotThrow(() => deleteAid(aid));
    }
    assert.throws(() => deleteAid(ISD_AID), /card manager/);
    // An AID this card does not install is refused, even one shaped like ours:
    // the whitelist is a list, not a prefix test.
    assert.throws(() => deleteAid(fromHex('D276000085010201')), /whitelist/);
    assert.throws(() => deleteAid(fromHex('A0000001510000')), /whitelist/);
    assert.throws(() => deleteAid(fromHex('F0464D4341524402')), /whitelist/);
  });

  it('INSTALL [for load] 80 E6 02 00: package, the ISD, no hash, no parameters, an empty token, Le 00', () => {
    assert.equal(
      toHex(installForLoad(FEDIMINT_PACKAGE_AID)),
      '80E602001407F0464D4341524408A00000015100000000000000',
    );
  });

  it('INSTALL [for install and make selectable] 80 E6 0C 00: package, module, instance, privileges 00, C9 00, an empty token, Le 00', () => {
    assert.equal(
      toHex(installAndSelectable(FEDIMINT_PACKAGE_AID, OPENER_AID, OPENER_AID)),
      '80E60C002007F0464D4341524408F0464D434152444D08F0464D434152444D010002C9000000',
    );
    // the card applet: the same command with the applet as both module and instance
    assert.equal(
      toHex(installAndSelectable(FEDIMINT_PACKAGE_AID, CARD_AID, CARD_AID)),
      '80E60C002007F0464D4341524408F0464D434152440108F0464D4341524401010002C9000000',
    );
  });

  it('PUT KEY 80 D8 <KVN> 81: the new KVN, then each key 80 10 <3DES-ECB under S-DEK> 03 <KCV>, and Le 00', () => {
    const sDek = rnd(16);
    const apdu = putKeys(SPIKE_KEYS, sDek, {p1: 0x01, newKvn: 0x01});
    const expected = concat(
      [0x80, 0xd8, 0x01, 0x81, 67, 0x01],
      ...[SPIKE_KEYS.enc, SPIKE_KEYS.mac, SPIKE_KEYS.dek].map(k =>
        concat(
          [0x80, 0x10],
          nodeEcb(sDek, k),
          [0x03],
          nodeEcb(k, new Array(8).fill(0)).slice(0, 3),
        ),
      ),
      [0x00],
    );
    assert.equal(toHex(apdu), toHex(expected));
    assert.equal(
      toHex(kcv(FACTORY_KEYS.enc)),
      toHex(nodeEcb(FACTORY_KEYS.enc, new Array(8).fill(0)).slice(0, 3)),
    );
  });

  it('ends every command that goes under the C-MAC in Le 00, as gp sends them all', () => {
    const wrapped: [string, number[]][] = [
      ['EXTERNAL AUTHENTICATE', externalAuthenticateCommand(rnd(8))],
      ...[CARD_AID, OPENER_AID, FEDIMINT_PACKAGE_AID].map(
        aid => ['DELETE ' + toHex(aid), deleteAid(aid)] as [string, number[]],
      ),
      ['INSTALL [for load]', installForLoad(FEDIMINT_PACKAGE_AID)],
      [
        'INSTALL [for install and make selectable]',
        installAndSelectable(FEDIMINT_PACKAGE_AID, CARD_AID, CARD_AID),
      ],
      ['PUT KEY', putKeys(SPIKE_KEYS, rnd(16), {p1: 0x01, newKvn: 0x01})],
      ...splitLoad(rnd(600)).map(
        (b, i) => ['LOAD ' + i, b] as [string, number[]],
      ),
    ];
    for (const [name, apdu] of wrapped) {
      const c = parseCommand(apdu);
      assert.ok(c.data.length > 0, `${name} carries data`);
      assert.equal(c.le, 0x00, `${name} ends in Le 00`);
    }
  });
});
