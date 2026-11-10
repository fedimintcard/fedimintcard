/**
 * The owner's GlobalPlatformPro captures of the J3R452 test card under SCP03
 * (gp/test/fixtures/README.md), replayed through this module: the handshake
 * must prove the keys, and every command gp sent under the MAC must be the
 * one this module sends. Each test skips, saying `pending fixtures`, while
 * its capture is missing, and is required once it is here.
 *
 * Ported from cashucard's gp/test/transcripts.test.ts at 0229dc8: its SCP03
 * section, as it is there, and its two readings of the key information
 * template. Cashucard's H0 captures of the J3R180 are not in this
 * repository; scp02-pins.test.ts pins what this module sends that chip. One
 * case is this repository's own: scp03-07's GET STATUS answers, read by
 * reset.ts as the reset reads them, list the seller's applets and none of
 * this card's.
 */
import assert from 'node:assert/strict';
import {existsSync, readFileSync} from 'node:fs';
import {createHash} from 'node:crypto';
import {join} from 'node:path';
import {describe, it} from 'node:test';

import {CARD_AID, FEDIMINT_PACKAGE_AID, ISD_AID, OPENER_AID} from '../aids';
import {concat, fromHex, toHex} from '../bytes';
import {getStatusCommand, parseStatusEntries, readOurs} from '../reset';
import {
  FACTORY_KEYS,
  installAndSelectable,
  installForLoad,
  parseCommand,
  parseKeyInformation,
  parseReply,
  splitLoad,
} from '../scp02';
import {SELLER_APPLETS} from './fake-card';
import {nodeScp03Kdf} from './node-aes';
import {parseGpLog, replay, type Exchange, type Replay} from './transcript';

const FIXTURES = join(__dirname, 'fixtures');

function capture(name: string): {skip: string | false; log: () => Exchange[]} {
  const path = join(FIXTURES, name);
  const present = existsSync(path);
  return {
    skip: present
      ? false
      : `pending fixtures: ${name} (tools/cardkit/gp/test/fixtures/README.md)`,
    log: () => parseGpLog(readFileSync(path, 'utf8')),
  };
}

function only(
  commands: number[][],
  match: (c: number[]) => boolean,
  what: string,
): number[] {
  const found = commands.filter(match);
  assert.equal(found.length, 1, `one ${what} in the capture`);
  return found[0];
}

/**
 * The load in a replayed gp --install: INSTALL [for load] is this module's,
 * the LOAD blocks gp sent carry the load file `sha256` names, and that file,
 * cut by splitLoad, is gp's blocks byte for byte (gp's block size is the
 * card's).
 */
function assertGpLoad(r: Replay, packageAid: number[], sha256: string): void {
  const forLoad = only(
    r.commands,
    x => x[1] === 0xe6 && x[2] === 0x02,
    'INSTALL [for load]',
  );
  assert.equal(toHex(forLoad), toHex(installForLoad(packageAid)));
  const loads = r.commands.filter(x => x[1] === 0xe8);
  const theirs = loads.flatMap(x => x.slice(5, 5 + x[4]));
  assert.equal(
    createHash('sha256').update(Buffer.from(theirs)).digest('hex'),
    sha256,
    'the load file in the LOAD blocks gp sent',
  );
  const ours = splitLoad(theirs);
  assert.equal(
    loads.length,
    ours.length,
    "the block count (gp's block size is the card's)",
  );
  loads.forEach((x, i) => assert.equal(toHex(x), toHex(ours[i]), `LOAD ${i}`));
}

describe('the key information template, as gp -i reads it', () => {
  it('reads a key information template as gp -i does: key ID, set version, and each type and length', () => {
    // key set 01 of three DES3 keys of 16 bytes, IDs 1 to 3, as the owner's gp -i -v shows it
    const kit = fromHex('E012C00401018010C00402018010C00403018010');
    assert.deepEqual(parseKeyInformation(kit), [
      {id: 1, kvn: 1, components: [{type: 0x80, length: 16}]},
      {id: 2, kvn: 1, components: [{type: 0x80, length: 16}]},
      {id: 3, kvn: 1, components: [{type: 0x80, length: 16}]},
    ]);
    assert.throws(() => parseKeyInformation(fromHex('6F00')), /not a key/);
    assert.throws(
      () => parseKeyInformation(fromHex('E006C004010180')),
      /runs past/,
    );
    assert.throws(
      () => parseKeyInformation(fromHex('E00AC0080101FF8800100000')),
      /extended/,
    );
    // a length form other than one byte or 81 and one is refused, not read as a short length
    assert.throws(
      () => parseKeyInformation(fromHex(`E082${'00'.repeat(130)}`)),
      /length form/,
    );
    assert.throws(() => parseKeyInformation(fromHex('E080')), /length form/);
    assert.deepEqual(parseKeyInformation(fromHex('E08100')), []);
  });

  it("reads the J3R452 test card's key information before and after first contact: AES keys 01 to 03 at FF, then at 01 (D182)", () => {
    // the owner's gp runs of 2026-11-08 (not in the repository): GET DATA E0 with no secure channel
    const aes = (kvn: number) =>
      [1, 2, 3].map(id => ({
        id,
        kvn,
        components: [{type: 0x88, length: 16}],
      }));
    assert.deepEqual(
      parseKeyInformation(fromHex('E012C00401FF8810C00402FF8810C00403FF8810')),
      aes(0xff),
    );
    assert.deepEqual(
      parseKeyInformation(fromHex('E012C00401018810C00402018810C00403018810')),
      aes(0x01),
    );
  });
});

/**
 * The J3R452's probe package (this repository's tools/j3r180-probe), which
 * scp03-09 loads and installs and scp03-10 deletes, and the sha256 of the
 * load file its LOAD blocks carry. Only a new capture moves it.
 */
const PROBE_PACKAGE_AID = fromHex('F0464D50524F42');
const PROBE_AID = fromHex('F0464D50524F4201');
const PROBE_LOAD_FILE_SHA256 =
  '32a12d40b6c65392fc4345165d8f7d02300fd715b313113193a505ef96f21fb5';

describe('SCP03: the J3R452 test card through gp v25.10.20, under the public default 40..4F', () => {
  // gp --list's GET STATUS: the ISD, the applications, then the load files
  // with their modules and the load files alone, each in pages (P2 02, then 03)
  const GET_STATUS = [
    '80F28002',
    '80F24002',
    '80F21002',
    '80F21003',
    '80F21003',
    '80F22002',
    '80F22003',
  ];
  // the four logins, in the order they were made, INITIALIZE UPDATE's P1 (07
  // names key version 01 with `--key-ver 1`, the rest let the card choose),
  // and what gp sent under the MAC after the first seven GET STATUS
  const logins: [string, number, string[]][] = [
    ['scp03-07-list-default-keyver1.txt', 0x01, []],
    ['scp03-08-list-default-auto.txt', 0x00, []],
    [
      'scp03-09-install-probe-default-key.txt',
      0x00,
      [
        '80E60200',
        ...[0, 1, 2, 3, 4, 5].map(i => '80E800' + toHex([i])),
        '80E88006',
        ...GET_STATUS,
        '80E60C00',
      ],
    ],
    ['scp03-10-delete-probe-default-key.txt', 0x00, ['80E40080']],
  ];
  const captures = logins.map(([name]) => capture(name));
  const anyMissing = captures.find(c => c.skip)?.skip ?? false;

  for (const [index, [name]] of logins.entries()) {
    const c = captures[index];
    it(
      `every session key, both cryptograms and every C-MAC of ${name} are this module's`,
      {skip: c.skip},
      () => {
        const log = c.log();
        const r = replay(log, FACTORY_KEYS);
        const {hostChallenge, reply} = r.handshake;
        assert.equal(reply.scp, 0x03);
        // the session keys, by node:crypto's KDF, which shares no code with scp03.ts
        const context = concat(hostChallenge, reply.cardChallenge);
        const sMac = nodeScp03Kdf(FACTORY_KEYS.mac, 0x06, 128, context);
        assert.deepEqual(r.session.keys, {
          enc: nodeScp03Kdf(FACTORY_KEYS.enc, 0x04, 128, context),
          mac: sMac,
          dek: Array.from(FACTORY_KEYS.dek),
        });
        // the card's challenge and cryptogram, which the replay has already held to the keys
        assert.deepEqual(
          reply.cardChallenge,
          nodeScp03Kdf(FACTORY_KEYS.enc, 0x02, 64, concat(reply.seq, ISD_AID)),
        );
        assert.deepEqual(
          reply.cardCryptogram,
          nodeScp03Kdf(sMac, 0x00, 64, context),
        );
        // the host cryptogram gp sent in EXTERNAL AUTHENTICATE
        const ea = log.find(e => e.command[1] === 0x82)!;
        assert.deepEqual(ea.command.slice(5, 13), r.hostCryptogram);
        assert.deepEqual(
          r.hostCryptogram,
          nodeScp03Kdf(sMac, 0x01, 64, context),
        );
        // and every command gp sent under the MAC was re-wrapped to its bytes
        const wrapped = log.filter(e => e.command[0] === 0x84);
        assert.equal(r.commands.length, wrapped.length - 1);
      },
    );
  }

  it(
    "the four logins hold 49 commands under the C-MAC, every one of them this module's",
    {skip: anyMissing},
    () => {
      const total = captures
        .map(c => replay(c.log(), FACTORY_KEYS))
        .reduce((n, r) => n + 1 + r.commands.length, 0);
      assert.equal(total, 49, 'four EXTERNAL AUTHENTICATE and 45 more');
    },
  );

  const install = captures[2];
  it(
    "scp03-09's INSTALL [for load], LOAD blocks and INSTALL [for install and make selectable] are this module's",
    {skip: install.skip},
    () => {
      const r = replay(install.log(), FACTORY_KEYS);
      assertGpLoad(r, PROBE_PACKAGE_AID, PROBE_LOAD_FILE_SHA256);
      const inst = only(
        r.commands,
        x => x[1] === 0xe6 && x[2] === 0x0c,
        'INSTALL [for install and make selectable]',
      );
      assert.equal(
        toHex(inst),
        toHex(installAndSelectable(PROBE_PACKAGE_AID, PROBE_AID, PROBE_AID)),
      );
    },
  );

  it(
    'the shape of each login: SELECT of the ISD, a 32-byte INITIALIZE UPDATE reply (version 01, SCP03, i=10, the counter one up each time), EXTERNAL AUTHENTICATE answered with no data, seven GET STATUS, then the command gp was asked for',
    {skip: anyMissing},
    () => {
      let counter = -1;
      for (const [index, [name, p1, then]] of logins.entries()) {
        const log = captures[index].log();
        const [select, iu, ea, ...rest] = log;
        // the card's default selection is the ISD, under the AID the challenge is made over
        assert.equal(toHex(select.command), '00A4040000', name);
        assert.ok(
          toHex(select.reply!).includes('84' + '08' + toHex(ISD_AID)),
          `${name}: the FCI names the ISD`,
        );
        assert.deepEqual(
          iu.command.slice(0, 5),
          [0x80, 0x50, p1, 0x00, 0x08],
          name,
        );
        const reply = parseReply(iu.reply!);
        assert.equal(reply.sw, 0x9000);
        assert.equal(reply.data.length, 32, name);
        assert.deepEqual(reply.data.slice(10, 13), [0x01, 0x03, 0x10], name);
        const seq =
          (reply.data[29] << 16) | (reply.data[30] << 8) | reply.data[31];
        assert.ok(
          counter < 0 || seq === counter + 1,
          `${name}: counter ${seq}`,
        );
        counter = seq;
        assert.deepEqual(
          ea.command.slice(0, 5),
          [0x84, 0x82, 0x01, 0x00, 0x10],
          name,
        );
        assert.equal(ea.command.length, 22, `${name}: Le 00 after the MAC`);
        assert.deepEqual(
          ea.reply,
          [0x90, 0x00],
          `${name}: no R-MAC at level 01`,
        );
        // then GET STATUS, and what gp was asked for, each CLA 84 with the MAC and Le 00
        assert.deepEqual(
          rest.map(e => toHex(e.command.slice(0, 4)).replace(/^84/, '80')),
          [...GET_STATUS, ...then],
          name,
        );
        for (const e of rest) {
          const c = parseCommand(e.command);
          assert.equal(c.cla, 0x84, name);
          assert.equal(c.le, 0x00, name);
          assert.ok(c.data.length >= 8, name);
        }
        assert.ok(
          rest
            .slice(0, 7)
            .every(
              e => toHex(parseCommand(e.command).data.slice(0, 2)) === '4F00',
            ),
          `${name}: each GET STATUS asks for 4F 00`,
        );
      }
    },
  );
});

describe("SCP03: scp03-07's GET STATUS, read as the reset reads it", () => {
  const c = capture('scp03-07-list-default-keyver1.txt');
  it(
    "lists the seller's four applets and twelve load files, none of them this card's, in the reset's own GET STATUS commands",
    {skip: c.skip},
    async () => {
      const r = replay(c.log(), FACTORY_KEYS);
      // the reset's GET STATUS is gp's, byte for byte: applications, then load files over two pages
      const answer = new Map<string, number[]>();
      r.commands.forEach((command, i) => {
        answer.set(toHex(command), r.replies[i]!);
      });
      const asked = [
        getStatusCommand('applications'),
        getStatusCommand('loadFiles'),
        getStatusCommand('loadFiles', true),
      ];
      for (const command of asked) {
        assert.ok(answer.has(toHex(command)), `gp sent ${toHex(command)}`);
      }
      // the first load file page ends 6310: more to come
      assert.equal(parseReply(answer.get(toHex(asked[1]))!).sw, 0x6310);
      const apps = parseStatusEntries(
        parseReply(answer.get(toHex(asked[0]))!).data,
      );
      assert.deepEqual(
        apps,
        SELLER_APPLETS.map(a => ({aid: a.instance, loadFile: a.pkg})),
        "the seller's instances, each from its own package, as the fake card's sellerApplets has them",
      );
      const files = parseStatusEntries(
        concat(
          parseReply(answer.get(toHex(asked[1]))!).data,
          parseReply(answer.get(toHex(asked[2]))!).data,
        ),
      );
      assert.equal(files.length, 12);
      assert.ok(files.every(f => f.loadFile === null));
      for (const pkg of SELLER_APPLETS.map(a => a.pkg)) {
        assert.ok(
          files.some(f => f.aid === pkg),
          `${pkg} among the load files`,
        );
      }
      const mine = [CARD_AID, OPENER_AID, FEDIMINT_PACKAGE_AID].map(toHex);
      for (const entry of [...apps, ...files]) {
        assert.ok(!mine.includes(entry.aid), `${entry.aid} is not this card's`);
        assert.notEqual(entry.loadFile, toHex(FEDIMINT_PACKAGE_AID));
      }
      // and readOurs, fed those answers, finds nothing of this card's to delete
      const found = await readOurs(async command => {
        const reply = answer.get(toHex(command));
        assert.ok(reply, `an answer to ${toHex(command)}`);
        return parseReply(reply!);
      });
      assert.deepEqual(found, {card: false, opener: false, package: false});
    },
  );
});
