/**
 * The install against the fake card: the happy path in order, a pull at every
 * APDU (before the card acts and after) followed by a run that sweeps and
 * installs, both guesses at what a torn LOAD leaves, the opener rule, a mute
 * INSTALL, refusals, the guards, a tampered load file, and events that throw.
 *
 * This plan has no `guard`: the applet has no GET_INFO, so `provision` skips
 * `probe` and starts at `selectIsd`. That changes three things these tests pin
 * rather than assume: the first APDU is SELECT of the card manager, not of the
 * applet; `mayLeaveGuardedApplet` is false in every phase; and nothing on the
 * card side stops a sweep, so Replace is the only gate and these tests carry
 * it through a pull themselves.
 */
import assert from 'node:assert/strict';
import {randomBytes} from 'node:crypto';
import {readFileSync} from 'node:fs';
import {join} from 'node:path';
import {describe, it} from 'node:test';

import {CARD_AID, FEDIMINT_PACKAGE_AID, OPENER_AID} from '../aids';
import {sha256} from '@noble/hashes/sha256';
import {fromHex, toHex} from '../bytes';
import {
  PHASES,
  fedimintPlan,
  mayLeaveGuardedApplet,
  notEmpty,
  provision,
  rotateKeys,
  type LoadFileAsset,
  type Plan,
  type ProvisionOptions,
  type RunEvent,
} from '../provision';
import {
  FACTORY_KEYS,
  SPIKE_KEYS,
  parseCommand,
  selectAid,
  splitLoad,
  type Transceiver,
} from '../scp02';
import {fedimintLoadFile} from './assets';
import {FakeCard, CARD_MODULE, OPENER_MODULE, pullAt} from './fake-card';

const LOAD_FILE = fedimintLoadFile();
const PLAN = fedimintPlan(LOAD_FILE);
const OPENER = toHex(OPENER_AID);
const CARD = toHex(CARD_AID);
const PKG = toHex(FEDIMINT_PACKAGE_AID);
const APPLET_JAVA = join(
  __dirname,
  '../../../../applet/src/main/java/io/github/bobodread876/fedimintcard/applet/FedimintCardApplet.java',
);

function options(extra: Partial<ProvisionOptions> = {}): ProvisionOptions {
  let t = 0;
  return {random: n => randomBytes(n), now: () => (t += 7), ...extra};
}

/** The card as a clean install leaves it: the package, the opener, the applet, in order, empty. */
function assertInstalled(card: FakeCard) {
  assert.deepEqual(card.instanceAids(), [OPENER, CARD]);
  assert.deepEqual([...card.packages.keys()], [PKG]);
  assert.equal(card.partial.size, 0);
  assert.equal(card.failedAuths, 0);
}

/**
 * The block count, taken from the same splitLoad provision uses rather than by
 * dividing: a quotient is not an index, and `new Array(3.004)` throws.
 */
const BLOCKS = splitLoad(fromHex(LOAD_FILE.hex)).length;

/**
 * A load file of `size` bytes with a matching hash, for the runs that are about
 * the loop rather than the CAP. The plan's guard checks both before the first
 * APDU, so a test asset must be honest or nothing is sent.
 */
function assetOf(size: number): LoadFileAsset {
  // Shaped like a CAP load file (C4, BER length, Header, Applet), because the
  // fake card parses what it loads and answers 6A80 to anything else. Built
  // from lengths, never from counted offsets: a hand-counted header is how the
  // placeholder load file declared three bytes more than it carried.
  const tlv = (tag: number, body: number[]) => [
    tag,
    (body.length >> 8) & 0xff,
    body.length & 0xff,
    ...body,
  ];
  const pkg = FEDIMINT_PACKAGE_AID;
  const header = tlv(0x01, [
    0xde,
    0xca,
    0xbe,
    0xef, // magic
    0x01,
    0x02, // minor, major
    0x00,
    0x00,
    0x03, // flags, package minor, package major
    pkg.length,
    ...pkg,
  ]);
  // Every module the plan installs, in install order: the opener first, then
  // the card applet. A missing module makes INSTALL answer 6A88.
  const modules = [OPENER_AID, CARD_AID];
  const applet = tlv(0x03, [
    modules.length,
    OPENER_AID.length,
    ...OPENER_AID,
    0x00,
    0x00,
    CARD_AID.length,
    ...CARD_AID,
    0x00,
    0x00,
  ]);
  const body = size - 4;
  const bytes = new Uint8Array(size).fill(0x30);
  bytes.set([0xc4, 0x82, (body >> 8) & 0xff, body & 0xff], 0);
  bytes.set(header, 4);
  bytes.set(applet, 4 + header.length);
  return {
    name: 'test load file',
    size,
    // Hash the bytes this asset publishes, not a second copy of them: provision
    // re-hashes `hex` and refuses to send when the two disagree.
    sha256: toHex(sha256(bytes)),
    hex: toHex(bytes),
  };
}

/**
 * The module AID of an INSTALL [selectable]: skip the header (5), the package
 * LV, then read the module LV. Everything after that is covered by the C-MAC.
 */
function installModule(apdu: number[]): string {
  let at = 5;
  at += 1 + apdu[at]; // package LV
  const n = apdu[at];
  return toHex(apdu.slice(at + 1, at + 1 + n));
}

/** No probe: SELECT the card manager, then FF21, then the handshake. */
const INSTALL_ORDER = [
  0xa4, // SELECT the card manager (no probe: this plan has no guard)
  0xca, // FF21
  0x50, // INITIALIZE UPDATE
  0x82, // EXTERNAL AUTHENTICATE
  0xe4,
  0xe4,
  0xe4, // sweep: the applet, the opener, the package
  0xe6, // INSTALL [for load]
  ...new Array(BLOCKS).fill(0xe8), // LOAD
  0xe6,
  0xe6, // the opener, then the applet
  0xa4, // verify: SELECT the applet
  0x11,
  0x11,
  0x11,
  0x11,
  0x11,
  0x11,
  0x11,
  0x11, // GET_PUB slots 0..7
  0x11, // and slot 31
  0x11, // and slot 32, past the last
  0xa4,
  0xca, // SELECT the card manager, FF21 again
];

describe('provision: the Fedimint card', () => {
  it('installs on a clean card in the documented order, every card-manager command MACed', async () => {
    const card = new FakeCard();
    const r = await provision(card.transceive, PLAN, options());
    assert.equal(r.ok, true, r.reason);
    assert.equal(r.phase, 'done');
    assertInstalled(card);
    assert.deepEqual(card.insLog(), INSTALL_ORDER);
    const sweep = card.log
      .filter(a => a[1] === 0xe4)
      .map(a => toHex(a.slice(7, 7 + a[6])));
    // the applet first, then the opener, then the package that holds them
    assert.deepEqual(sweep, [CARD, OPENER, PKG]);
    assert.ok(
      card.log
        .filter(a => [0x82, 0xe4, 0xe6, 0xe8].includes(a[1]))
        .every(a => a[0] === 0x84),
    );
    // as gp sends them: Le 00 after the MAC on every one (GPSession.transmit)
    for (const a of card.log.filter(x => (x[0] & 0x04) !== 0)) {
      assert.equal(
        parseCommand(a).le,
        0x00,
        `INS ${toHex([a[1]])} ends in Le 00`,
      );
    }
    // probe (no guard) and installTag (no tag applet) are the two phases this
    // plan never reaches; every other phase runs once, in order
    assert.deepEqual(
      r.timings.map(t => t.phase),
      PHASES.filter(p => p !== 'probe' && p !== 'installTag'),
    );
    assert.equal(r.apdus.length, card.log.length);
    assert.deepEqual(r.blocks, {at: BLOCKS, of: BLOCKS});
    assert.ok(
      r.freeBefore &&
        r.freeAfter &&
        r.freeAfter.nonVolatile < r.freeBefore.nonVolatile,
    );
  });

  it('has no guard, so it sends no probe: a card applet already on the card is swept without asking', async () => {
    const card = new FakeCard();
    assert.equal((await provision(card.transceive, PLAN, options())).ok, true);
    card.reset();
    const before = card.log.length;
    // no Replace: there is no card-side gate on this applet, so the run goes ahead
    const again = await provision(card.transceive, PLAN, options());
    assert.equal(again.ok, true, again.reason);
    assert.equal(again.timings[0].phase, 'selectIsd');
    assert.equal(
      card.log.slice(before)[0][1],
      0xa4,
      'the first command is SELECT, not a probe',
    );
    assertInstalled(card);
  });

  it("verifies the applet's version as FedimintCardApplet.java has it, which the fake card answers", async () => {
    const java = readFileSync(APPLET_JAVA, 'utf8');
    const version = ['VERSION_MAJOR', 'VERSION_MINOR'].map(name => {
      // FedimintCardApplet.java declares these as `static final byte NAME = 0x..;`
      const m = new RegExp(
        `static final byte ${name}\\s*=\\s*(?:\\(byte\\)\\s*)?0x([0-9A-Fa-f]{2})`,
      ).exec(java);
      assert.ok(m, `${name} in FedimintCardApplet.java`);
      return parseInt(m[1], 16);
    });
    const select = PLAN.checks.find(
      c => toHex(c.apdu) === toHex(selectAid(CARD_AID)),
    );
    assert.deepEqual(select?.data, version, "verify's whole-AID SELECT");
    const card = new FakeCard();
    assert.equal((await provision(card.transceive, PLAN, options())).ok, true);
    assert.deepEqual(
      await card.transceive(selectAid(CARD_AID)),
      [...version, 0x90, 0x00],
      "the fake card's SELECT",
    );
  });

  it('records each APDU as phase, INS, Lc, SW, reply length and time, and never its bytes', async () => {
    const card = new FakeCard();
    const r = await provision(card.transceive, PLAN, options());
    for (const record of r.apdus) {
      assert.deepEqual(Object.keys(record).sort(), [
        'ins',
        'lc',
        'ms',
        'phase',
        'rlen',
        'sw',
      ]);
    }
    const text = JSON.stringify(r);
    assert.ok(!/[0-9A-Fa-f]{16}/.test(text), 'no run of hex in the result');
  });

  it('comes back from a pull at any APDU, before or after the card acts, with a run that sweeps and installs', async () => {
    const total = INSTALL_ORDER.length;
    for (const partialLoad of ['discard', 'keep'] as const) {
      for (const when of ['before', 'after'] as const) {
        for (let n = 0; n < total; n++) {
          const card = new FakeCard({partialLoad});
          const pull = pullAt(card, n, when);
          const first = await provision(pull.tx, PLAN, options());
          const where = `${partialLoad}/${when}/APDU ${n}`;
          assert.equal(first.ok, false, where);
          assert.equal(first.kind, 'pulled', where);
          assert.equal(
            pull.state.late,
            0,
            `${where}: nothing was sent after the pull`,
          );
          card.reset();
          const again = await provision(card.transceive, PLAN, options());
          assert.equal(
            again.ok,
            true,
            `${where}: ${again.kind} ${again.reason}`,
          );
          assertInstalled(card);
        }
      }
    }
  });

  it('never reports that a pull may leave a guarded applet: this plan guards nothing', async () => {
    for (const phase of [...PHASES, 'putKey']) {
      assert.equal(mayLeaveGuardedApplet(PLAN, phase), false, phase);
    }
  });

  it('keeps a torn LOAD out of the way in both variants: the sweep deletes a kept partial package', async () => {
    for (const partialLoad of ['discard', 'keep'] as const) {
      const card = new FakeCard({partialLoad});
      const loadAt = INSTALL_ORDER.indexOf(0xe8) + 1;
      const pull = pullAt(card, loadAt, 'after');
      const first = await provision(pull.tx, PLAN, options());
      assert.equal(first.kind, 'pulled');
      assert.equal(first.phase, 'load');
      assert.deepEqual(first.blocks, {at: 2, of: BLOCKS});
      card.reset();
      assert.equal(card.partial.size, partialLoad === 'keep' ? 1 : 0);
      const again = await provision(card.transceive, PLAN, options());
      assert.equal(again.ok, true, again.reason);
      // the kept partial blocks INSTALL [for load] until the sweep deletes it
      assert.equal(card.insLog().filter(i => i === 0xe4).length >= 3, true);
      assertInstalled(card);
    }
  });

  it('reports only LOAD blocks whose successful response reached the host', async () => {
    const card = new FakeCard();
    const events: RunEvent[] = [];
    const pull = pullAt(card, INSTALL_ORDER.indexOf(0xe8) + 1, 'after');
    const result = await provision(
      pull.tx,
      PLAN,
      options({onEvent: event => events.push(event)}),
    );
    assert.equal(result.kind, 'pulled');
    assert.deepEqual(result.blocks, {at: 2, of: BLOCKS});
    const written = events.filter(
      (event): event is Extract<RunEvent, {type: 'block'}> =>
        event.type === 'block',
    );
    assert.equal(written.at(-1)?.index, 1);
  });

  it('stops at a mute INSTALL of the card applet, and the next run recovers', async () => {
    const card = new FakeCard();
    let installs = 0;
    card.override = apdu =>
      apdu[1] === 0xe6 && apdu[2] === 0x0c && ++installs === 2
        ? 'mute'
        : undefined;
    const first = await provision(card.transceive, PLAN, options());
    assert.equal(first.kind, 'pulled');
    assert.equal(first.phase, 'installCard');
    card.override = undefined;
    card.reset();
    const again = await provision(card.transceive, PLAN, options());
    assert.equal(again.ok, true, again.reason);
    assertInstalled(card);
  });

  it('meets the opener rule: the card applet first goes mute or 6F00 and installs nothing', async () => {
    // the phases run in table order, so put the card applet's INSTALL in the opener's phase
    const [openerStep, cardStep] = PLAN.installs;
    const wrongOrder: Plan = {
      ...PLAN,
      installs: [
        {...cardStep, phase: 'installOpener'},
        {...openerStep, phase: 'installCard'},
      ],
    };
    for (const openerRule of ['mute', '6F00'] as const) {
      const card = new FakeCard({openerRule});
      const r = await provision(card.transceive, wrongOrder, options());
      assert.equal(r.ok, false);
      assert.equal(r.phase, 'installOpener');
      assert.equal(r.kind, openerRule === 'mute' ? 'pulled' : 'refused');
      assert.deepEqual(card.instanceAids(), []);
      card.reset();
      const again = await provision(card.transceive, PLAN, options());
      assert.equal(again.ok, true, again.reason);
      assertInstalled(card);
    }
  });

  it('stops at 6F00, 6985 or 6A80 from any card-manager command, and sends nothing more', async () => {
    const targets: [string, (a: number[]) => boolean][] = [
      ['DELETE', a => a[1] === 0xe4],
      ['INSTALL [for load]', a => a[1] === 0xe6 && a[2] === 0x02],
      ['the first LOAD', a => a[1] === 0xe8 && a[3] === 0],
      ['the last LOAD', a => a[1] === 0xe8 && a[2] === 0x80],
      // 84 E6 0C 00 Lc | 07 <package: 6..12> | 08 <module: 14..21> | 08 <instance: 23..30>
      // Offsets derived from the APDU, never counted by hand: MACed Lc is at 4.
      [
        'INSTALL the opener',
        a => a[1] === 0xe6 && a[2] === 0x0c && installModule(a) === OPENER,
      ],
      [
        'INSTALL the card applet',
        a => a[1] === 0xe6 && a[2] === 0x0c && installModule(a) === CARD,
      ],
    ];
    for (const sw of [0x6f00, 0x6985, 0x6a80]) {
      for (const [name, match] of targets) {
        const card = new FakeCard();
        let at = -1;
        card.override = (apdu, i) =>
          at < 0 && match(apdu) ? ((at = i), sw) : undefined;
        const r = await provision(card.transceive, PLAN, options());
        assert.ok(at >= 0, `${name} was sent`);
        assert.equal(r.kind, 'refused', name);
        assert.equal(r.sw, sw, name);
        assert.equal(card.log.length, at + 1, `${name}: nothing after it`);
      }
    }
  });

  it('sends nothing at all for a load file that is not the build', async () => {
    const flip = (hex: string, at: number) =>
      hex.slice(0, at) + (hex[at] === '0' ? '1' : '0') + hex.slice(at + 1);
    const tampered = [
      {...LOAD_FILE, hex: flip(LOAD_FILE.hex, 400)},
      {...LOAD_FILE, hex: LOAD_FILE.hex.slice(0, -2)},
      {
        ...LOAD_FILE,
        sha256: LOAD_FILE.sha256.replace(/^./, c => (c === '0' ? '1' : '0')),
      },
      {...LOAD_FILE, hex: 'zz' + LOAD_FILE.hex.slice(2)},
    ];
    for (const loadFile of tampered) {
      const card = new FakeCard();
      const r = await provision(
        card.transceive,
        {...PLAN, loadFile},
        options(),
      );
      assert.equal(r.kind, 'guarded');
      assert.equal(card.log.length, 0);
      assert.equal(r.apdus.length, 0);
    }
  });

  it('sends the same APDUs whether onEvent is absent, counts, throws or rejects', async () => {
    const counts: number[] = [];
    const events: RunEvent[] = [];
    const listeners: (ProvisionOptions['onEvent'] | undefined)[] = [
      undefined,
      e => {
        events.push(e);
      },
      () => {
        throw new Error('the screen is gone');
      },
      (() =>
        Promise.reject(
          new Error('async listener'),
        )) as unknown as ProvisionOptions['onEvent'],
    ];
    for (const onEvent of listeners) {
      const card = new FakeCard();
      const r = await provision(card.transceive, PLAN, options({onEvent}));
      assert.equal(r.ok, true);
      counts.push(card.log.length);
    }
    assert.deepEqual(counts, new Array(4).fill(INSTALL_ORDER.length));
    const phases = events
      .filter(e => e.type === 'phase')
      .map(e => (e as {phase: string}).phase);
    assert.deepEqual(
      phases,
      PHASES.filter(p => p !== 'probe' && p !== 'installTag'),
    );
    const blocks = events.filter(e => e.type === 'block') as {
      index: number;
      of: number;
    }[];
    assert.deepEqual(
      blocks.map(b => b.index),
      Array.from({length: BLOCKS}, (_, i) => i + 1),
    );
    assert.ok(blocks.every(b => b.of === BLOCKS));
    assert.equal(
      events.filter(e => e.type === 'apdu').length,
      INSTALL_ORDER.length,
    );
  });

  it('stops before EXTERNAL AUTHENTICATE on a card with other keys', async () => {
    const card = new FakeCard({
      keys: {
        enc: Array.from(SPIKE_KEYS.enc),
        mac: Array.from(SPIKE_KEYS.mac),
        dek: Array.from(SPIKE_KEYS.dek),
      },
    });
    const r = await provision(card.transceive, PLAN, options());
    assert.equal(r.kind, 'wrongKeys');
    assert.equal(r.phase, 'auth');
    assert.deepEqual(card.insLog(), [0xa4, 0xca, 0x50]);
    assert.equal(card.failedAuths, 0);
  });

  it('reports a refused EXTERNAL AUTHENTICATE as refused in auth (a card that MACs the other header)', async () => {
    const card = new FakeCard({macOverModifiedApdu: false});
    const r = await provision(card.transceive, PLAN, options());
    assert.equal(r.kind, 'refused');
    assert.equal(r.phase, 'auth');
    assert.equal(r.ins, 0x82);
    assert.equal(r.sw, 0x6300);
    assert.equal(card.log.length, 4);
    card.reset();
    const again = await provision(
      card.transceive,
      PLAN,
      options({macOverModifiedApdu: false}),
    );
    assert.equal(again.ok, true, again.reason);
  });

  it('loads in smaller blocks when the plan says so', async () => {
    const loadFile = assetOf(1000);
    const card = new FakeCard();
    const r = await provision(
      card.transceive,
      {...fedimintPlan(loadFile), blockSize: 128},
      options(),
    );
    assert.equal(r.ok, true, r.reason);
    assert.equal(card.insLog().filter(i => i === 0xe8).length, 8);
    assertInstalled(card);
  });

  it('verifies the install-tap slots and the last, so an applet that kept a key across a reinstall is seen', () => {
    const gets = PLAN.checks.filter(c => c.apdu[1] === 0x11);
    assert.deepEqual(
      gets.map(c => c.apdu[2]),
      [0, 1, 2, 3, 4, 5, 6, 7, 31, 32],
    );
    // an unused slot is this applet's success: 6985, which Check.sw names;
    // slot 32 is past the last of a 32-slot build, so 6B00
    const inRange = gets.filter(c => c.apdu[2] < 32);
    assert.ok(inRange.every(c => c.sw === 0x6985 && c.data === undefined));
    assert.deepEqual(
      gets.filter(c => c.apdu[2] === 32).map(c => c.sw),
      [0x6b00],
    );
  });

  it('verifies only after the sweep, so a key left by an earlier run is cleared before the check', async () => {
    // The sweep reinstalls the applet, which is why verify can require every
    // slot empty. This pins that order: the run that finds a keyed applet from
    // a previous attempt deletes and reinstalls it, and verify then sees 6985.
    const card = new FakeCard();
    assert.equal((await provision(card.transceive, PLAN, options())).ok, true);
    card.setCardState(s => {
      s.used[5] = true;
      s.pubs[5] = [0x04, ...new Array(64).fill(0x01)];
    });
    card.reset();
    const before = card.log.length;
    const r = await provision(card.transceive, PLAN, options());
    assert.equal(r.ok, true, r.reason);
    const ins = card.log.slice(before).map(a => a[1]);
    const lastDelete = ins.lastIndexOf(0xe4);
    const firstGetPub = ins.indexOf(0x11);
    assert.ok(lastDelete >= 0, 'the run swept');
    assert.ok(firstGetPub > lastDelete, 'verify came after the sweep');
    // and the reinstalled applet is empty
    const aid = card.instanceAids().find(a => a.endsWith('01'))!;
    const state = card.instances.get(aid)!.state;
    assert.ok(
      state.used.every(used => used === false),
      'every slot is empty after the install',
    );
  });

  it('fails verify when a slot answers a key: the check is not satisfied by any 9000', () => {
    // Check.sw names the expected refusal, so the plan cannot pass a keyed slot.
    const gets = PLAN.checks.filter(c => c.apdu[1] === 0x11 && c.apdu[2] < 32);
    assert.ok(
      gets.every(c => c.sw === 0x6985 && c.length === undefined),
      'a keyed slot answers 9000 with 65 bytes, which is not the expected 6985',
    );
  });

  it('fails verify when SELECT lands on the opener: 9000 with no data is not 02 01', async () => {
    const card = new FakeCard();
    const tx: Transceiver = async apdu => {
      // a card whose card applet was not instantiated: the whole-AID SELECT
      // reaches no instance, and a partial name reaches the opener
      if (apdu[1] === 0xa4 && toHex(apdu.slice(5, 5 + apdu[4])) === CARD) {
        return [0x90, 0x00];
      }
      return card.transceive(apdu);
    };
    const r = await provision(tx, PLAN, options());
    assert.equal(r.ok, false);
    assert.equal(r.phase, 'verify');
    assert.match(r.reason ?? '', /answered .*not 0201/);
  });
});

describe('provision: the fake card', () => {
  it('refuses to delete a package before its instances', async () => {
    const card = new FakeCard();
    assert.equal((await provision(card.transceive, PLAN, options())).ok, true);
    // inside a session, DELETE the package alone: the applet and the opener still point at it
    const r = await provision(
      card.transceive,
      {...PLAN, sweep: [FEDIMINT_PACKAGE_AID]},
      options(),
    );
    assert.equal(r.ok, false);
    assert.equal(r.sw, 0x6985);
    // and the applet is still there
    assert.deepEqual(card.instanceAids(), [OPENER, CARD]);
  });

  it('dispatches on the module, not on INS: the opener does not answer as the card applet', async () => {
    const card = new FakeCard();
    assert.equal((await provision(card.transceive, PLAN, options())).ok, true);
    // the opener is an instance of this package; its AID begins with the package's
    assert.deepEqual(
      await card.transceive(selectAid(OPENER_AID)),
      [0x90, 0x00],
    );
    assert.deepEqual(
      await card.transceive([0xb0, 0x11, 0x00, 0x00, 0x00]),
      [0x6d, 0x00],
      'the opener has no GET_PUB',
    );
    // and the card applet, whose AID also begins with the package's, still answers as itself
    assert.deepEqual(
      await card.transceive(selectAid(CARD_AID)),
      [0x02, 0x01, 0x90, 0x00],
    );
  });

  it('answers a selected instance from its module even after another is installed', async () => {
    const card = new FakeCard();
    assert.equal((await provision(card.transceive, PLAN, options())).ok, true);
    const modules = card
      .instanceAids()
      .map(aid => card.instances.get(aid)!.module);
    assert.deepEqual(modules, [OPENER_MODULE, CARD_MODULE]);
  });
});

describe('notEmpty: Cashu-shaped and unused by this plan', () => {
  it('is not wired into fedimintPlan, which has no GET_INFO to read', () => {
    assert.equal(PLAN.guard, undefined);
  });

  it('still reads Cashu-shaped 30 bytes, for the day an applet has a GET_INFO', () => {
    assert.equal(notEmpty(new Array(30).fill(0)), null);
    const pin = new Array(30).fill(0);
    pin[7] = 1;
    assert.equal(notEmpty(pin), 'it has a PIN');
    assert.match(notEmpty(new Array(12).fill(0)) ?? '', /not 30/);
  });
});

describe('fedimintPlan: shape', () => {
  it('installs the opener first, then the card applet, and sweeps them back newest first', () => {
    assert.deepEqual(
      PLAN.installs.map(s => s.phase),
      ['installOpener', 'installCard'],
    );
    assert.deepEqual(PLAN.sweep.map(toHex), [CARD, OPENER, PKG]);
    assert.equal(PLAN.removeAfterChecks, undefined);
    assert.equal(toHex(PLAN.packageAid), PKG);
  });
});

/**
 * Per-card card-manager keys (docs/OPS-PLAN.md O5): the install offered both
 * the card's own set and the factory set, and the rotation after it. The fake
 * card decrypts PUT KEY with node:crypto, so the keys it ends on are checked
 * against an implementation that shares no code with scp02.ts.
 */
describe('per-card manager keys', () => {
  const OWN = {
    enc: Array.from(SPIKE_KEYS.enc),
    mac: Array.from(SPIKE_KEYS.mac),
    dek: Array.from(SPIKE_KEYS.dek),
  };
  const FACTORY = {
    enc: Array.from(FACTORY_KEYS.enc),
    mac: Array.from(FACTORY_KEYS.mac),
    dek: Array.from(FACTORY_KEYS.dek),
  };
  const sameKeys = (card: FakeCard, keys: typeof OWN) =>
    toHex(card.keys.enc) === toHex(keys.enc) &&
    toHex(card.keys.mac) === toHex(keys.mac) &&
    toHex(card.keys.dek) === toHex(keys.dek);

  it('installs on whichever of the offered sets the card proves, with one INITIALIZE UPDATE', async () => {
    for (const [keys, index] of [
      [FACTORY, 1],
      [OWN, 0],
    ] as const) {
      const card = new FakeCard({keys: {...keys}});
      const r = await provision(
        card.transceive,
        PLAN,
        options({keys: [OWN, FACTORY_KEYS]}),
      );
      assert.equal(r.ok, true, r.reason);
      assert.equal(r.keySet, index);
      assert.equal(card.insLog().filter(i => i === 0x50).length, 1);
      assert.equal(card.failedAuths, 0);
      assertInstalled(card);
    }
  });

  it('stops wrongKeys on a card that proves neither set, having sent nothing after INITIALIZE UPDATE', async () => {
    const card = new FakeCard({
      keys: {
        enc: fromHex('55'.repeat(16)),
        mac: fromHex('55'.repeat(16)),
        dek: fromHex('55'.repeat(16)),
      },
    });
    const r = await provision(
      card.transceive,
      PLAN,
      options({keys: [OWN, FACTORY_KEYS]}),
    );
    assert.equal(r.ok, false);
    assert.equal(r.kind, 'wrongKeys');
    assert.equal(r.keySet, undefined);
    assert.equal(card.insLog().at(-1), 0x50);
    assert.equal(card.failedAuths, 0);
  });

  it('rotates factory keys to the card’s own in place (version kept), and the card proves them after', async () => {
    const card = new FakeCard();
    const r = await rotateKeys(card.transceive, FACTORY_KEYS, OWN, options());
    assert.equal(r.ok, true, r.reason);
    assert.equal(r.rotated, true);
    assert.equal(r.keyVersion, 0x01);
    assert.ok(sameKeys(card, OWN));
    assert.equal(card.kvn, 0x01);
    const put = card.log.find(a => a[1] === 0xd8)!;
    assert.deepEqual(put.slice(0, 4), [0x84, 0xd8, 0x01, 0x81]);
    assert.equal(card.failedAuths, 0);
  });

  it('finds the card already on its own keys after a PUT KEY whose reply was lost, without a failed login', async () => {
    const card = new FakeCard();
    // the first run: SELECT, INITIALIZE UPDATE, EXTERNAL AUTHENTICATE, then PUT KEY (index 3), acted on and lost
    const torn = pullAt(card, 3, 'after');
    const first = await rotateKeys(torn.tx, FACTORY_KEYS, OWN, options());
    assert.equal(first.ok, false);
    assert.equal(first.kind, 'pulled');
    assert.ok(
      sameKeys(card, OWN),
      'the card took the keys before the reply was lost',
    );
    card.reset();
    const again = await rotateKeys(
      card.transceive,
      FACTORY_KEYS,
      OWN,
      options(),
    );
    assert.equal(again.ok, true, again.reason);
    assert.equal(again.rotated, false);
    assert.equal(
      card.insLog().filter(i => i === 0xd8).length,
      1,
      'PUT KEY was not sent twice',
    );
    assert.equal(card.failedAuths, 0);
  });

  it('a pull before PUT KEY leaves the factory keys, which the next rotation still proves', async () => {
    const card = new FakeCard();
    const torn = pullAt(card, 3, 'before');
    assert.equal(
      (await rotateKeys(torn.tx, FACTORY_KEYS, OWN, options())).kind,
      'pulled',
    );
    assert.ok(sameKeys(card, FACTORY));
    card.reset();
    const again = await rotateKeys(
      card.transceive,
      FACTORY_KEYS,
      OWN,
      options(),
    );
    assert.equal(again.rotated, true);
    assert.ok(sameKeys(card, OWN));
  });

  it('a rotation against a card on neither set stops with no EXTERNAL AUTHENTICATE sent', async () => {
    const other = {
      enc: fromHex('66'.repeat(16)),
      mac: fromHex('66'.repeat(16)),
      dek: fromHex('66'.repeat(16)),
    };
    const card = new FakeCard({keys: other});
    const r = await rotateKeys(card.transceive, FACTORY_KEYS, OWN, options());
    assert.equal(r.kind, 'wrongKeys');
    assert.ok(!card.insLog().includes(0x82));
    assert.equal(card.failedAuths, 0);
  });

  it('the sweep deletes this card’s package and nothing else on the card', async () => {
    const card = new FakeCard({keys: {...OWN}});
    assert.equal(
      (
        await provision(
          card.transceive,
          PLAN,
          options({keys: [OWN, FACTORY_KEYS]}),
        )
      ).ok,
      true,
    );
    // a Cashu card's package and instance on the same chip
    const cashuPkg = 'F043415348554341';
    const cashuApp = 'F04341534855434101';
    card.packages.set(cashuPkg, {
      aid: cashuPkg,
      modules: [cashuApp],
      size: 4000,
      instancesMade: 1,
    });
    card.instances.set(cashuApp, {
      aid: cashuApp,
      module: cashuApp,
      pkg: cashuPkg,
      state: card.instances.get(CARD)!.state,
    });
    card.reset();
    assert.equal(
      (
        await provision(
          card.transceive,
          PLAN,
          options({keys: [OWN, FACTORY_KEYS]}),
        )
      ).ok,
      true,
    );
    assert.ok(card.packages.has(cashuPkg));
    assert.ok(card.instances.has(cashuApp));
    const deleted = card.log
      .filter(a => a[1] === 0xe4)
      .map(a => toHex(a.slice(7, 7 + a[6])));
    assert.ok(
      deleted.every(aid => [CARD, OPENER, PKG].includes(aid)),
      deleted.join(' '),
    );
  });
});
