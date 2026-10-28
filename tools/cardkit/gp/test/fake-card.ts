/**
 * A J3R180 card manager in software, for the provisioner's tests: the card
 * side of SCP02 computed with node:crypto (gp/test/node-des.ts), sharing no
 * code with the host side under test, and the GlobalPlatform commands an
 * install uses, with what the chip is known or suspected to do when things go
 * wrong:
 *
 * - the opener rule: the INSTALL that is a package's first instantiation and
 *   runs a heavy constructor goes mute (the reader gives up) or answers 6F00,
 *   and installs nothing;
 * - a LOAD torn by the card leaving the field either vanishes with the power
 *   (`partialLoad: 'discard'`) or stays as a partial package that blocks a new
 *   INSTALL [for load] until it is deleted (`'keep'`); which the chip does is
 *   not known, so both are run;
 * - a package with instances refuses DELETE (6985): the order is the
 *   instances first;
 * - EXTERNAL AUTHENTICATE failures are counted, and lock the card manager at
 *   `failedAuthLimit`, as the chip does for good.
 *
 * The applet under this card is FedimintCardApplet, as it answers on the wire:
 * SELECT sends the two version bytes `02 00` and no FCI; GET_PUB (B0 11) on a
 * slot with no key answers 6985; SIGN (B0 12) is refused before the PIN. The
 * fake dispatches on the **module** the selected instance was made from, not
 * on INS, because SELECT is global: the opener is a real instance of this
 * package and must not answer as the card applet would.
 *
 * `reset()` is the power going: whatever was selected, the session, a pending
 * handshake (counted in `abandonedHandshakes`) and a pending load go with it.
 */
import {nodeCbc, nodeEcb, nodeFullMac, nodeRetailMac} from './node-des';

type Bytes = number[];

const hex = (b: ArrayLike<number>) =>
  Buffer.from(Array.from(b)).toString('hex').toUpperCase();
const fromHex = (s: string): Bytes => Array.from<number>(Buffer.from(s, 'hex'));
const sw = (v: number, data: Bytes = []) => [...data, v >> 8, v & 0xff];

export const ISD = 'A000000151000000';
/** applet/build.xml: package F0 "FMCARD", applets package + 01 (card) and + 4D (opener). */
export const CARD_MODULE = 'F0464D4341524401';
export const OPENER_MODULE = 'F0464D434152444D';
const K40 = fromHex('404142434445464748494A4B4C4D4E4F');

/** SLOTS in FedimintCardApplet. */
export const SLOTS = 8;

export interface StaticKeys {
  enc: Bytes;
  mac: Bytes;
  dek: Bytes;
}

/** What the card applet holds. All zero is a mint-fresh applet. */
export interface CardAppletState {
  /** Per slot: false until GEN_KEY. */
  used: boolean[];
  /** Per slot: the 65-byte SEC1 public key once keyed. */
  pubs: Bytes[];
  pinSet: boolean;
}

export interface FakeCardOptions {
  keys?: StaticKeys;
  kvn?: number;
  seq?: number;
  /** How the card checks a C-MAC: over the modified header (gp's way) or the original. */
  macOverModifiedApdu?: boolean;
  openerRule?: 'mute' | '6F00';
  partialLoad?: 'discard' | 'keep';
  heavyModules?: string[];
  failedAuthLimit?: number;
  freeNonVolatile?: number;
}

interface Package {
  aid: string;
  modules: string[];
  size: number;
  instancesMade: number;
}

interface Instance {
  aid: string;
  module: string;
  pkg: string;
  state: CardAppletState;
}

interface PendingLoad {
  aid: string;
  data: Bytes;
  next: number;
}

interface Session {
  mac: Bytes;
  dek: Bytes;
  lastMac: Bytes;
}

/** A card answers `sw` to the command, doing nothing; 'mute' answers nothing until reset. */
export type Override = (
  apdu: Bytes,
  index: number,
) => number | 'mute' | undefined;

function freshState(): CardAppletState {
  return {
    used: new Array(SLOTS).fill(false),
    pubs: new Array(SLOTS).fill([]),
    pinSet: false,
  };
}

export class FakeCard {
  readonly log: Bytes[] = [];
  failedAuths = 0;
  abandonedHandshakes = 0;
  lockedOut = false;
  mute = false;
  keys: StaticKeys;
  kvn: number;
  seq: number;
  free: number;
  override: Override | undefined;
  readonly packages = new Map<string, Package>();
  /** In the order they were made. */
  readonly instances = new Map<string, Instance>();
  /** Partial packages a torn LOAD left, in the 'keep' variant, with their bytes. */
  readonly partial = new Map<string, number>();
  private pending: PendingLoad | null = null;
  private selected: string = ISD;
  private session: Session | null = null;
  private handshake: {
    host: Bytes;
    card: Bytes;
    enc: Bytes;
    mac: Bytes;
    dek: Bytes;
  } | null = null;
  private challengeCounter = 0;
  private readonly opts: Required<
    Omit<FakeCardOptions, 'keys' | 'kvn' | 'seq' | 'freeNonVolatile'>
  >;

  constructor(options: FakeCardOptions = {}) {
    this.keys = options.keys ?? {enc: K40, mac: K40, dek: K40};
    this.kvn = options.kvn ?? 0x01;
    this.seq = options.seq ?? 0x0009;
    this.free = options.freeNonVolatile ?? 164176;
    this.opts = {
      macOverModifiedApdu: options.macOverModifiedApdu ?? true,
      openerRule: options.openerRule ?? 'mute',
      partialLoad: options.partialLoad ?? 'discard',
      heavyModules: options.heavyModules ?? [CARD_MODULE],
      failedAuthLimit: options.failedAuthLimit ?? 10,
    };
  }

  /** The card in the field: one APDU in, data and SW out, or a rejection when it does not answer. */
  readonly transceive = async (apdu: number[]): Promise<number[]> => {
    const index = this.log.length;
    this.log.push(Array.from(apdu));
    if (this.mute) {
      throw new Error('the card does not answer');
    }
    const o = this.override?.(apdu, index);
    if (o === 'mute') {
      this.mute = true;
      throw new Error('the card does not answer');
    }
    if (typeof o === 'number') {
      return sw(o);
    }
    return this.process(Array.from(apdu));
  };

  /** The power goes: the card leaves the field, or the reader resets it. */
  reset(): void {
    this.mute = false;
    this.selected = ISD;
    this.session = null;
    if (this.handshake) {
      this.abandonedHandshakes++;
      this.handshake = null;
    }
    if (this.pending) {
      if (this.opts.partialLoad === 'keep') {
        this.partial.set(this.pending.aid, this.pending.data.length);
      } else {
        this.free += this.pending.data.length;
      }
      this.pending = null;
    }
  }

  /** The card applet's state, for a keyed or PINned card. */
  setCardState(change: (state: CardAppletState) => void): void {
    const card = this.instances.get(CARD_MODULE);
    if (!card) {
      throw new Error('no card applet');
    }
    change(card.state);
  }

  /** The instance AIDs, in the order they were made. */
  instanceAids(): string[] {
    return [...this.instances.keys()];
  }

  /** INS bytes of everything received, for asserting what was sent. */
  insLog(): number[] {
    return this.log.map(a => a[1]);
  }

  private process(apdu: Bytes): Bytes {
    if ((apdu[0] & 0xfc) === 0x00 && apdu[1] === 0xa4) {
      return this.select(apdu);
    }
    if (this.selected === ISD) {
      return this.isd(apdu);
    }
    const inst = this.instances.get(this.selected);
    return inst ? this.applet(inst, apdu) : sw(0x6a82);
  }

  private endSessions(): void {
    this.session = null;
    if (this.handshake) {
      this.abandonedHandshakes++;
      this.handshake = null;
    }
  }

  private select(apdu: Bytes): Bytes {
    if (apdu[2] !== 0x04) {
      return sw(0x6a86);
    }
    const lc = apdu.length > 5 ? apdu[4] : 0;
    const aid = hex(apdu.slice(5, 5 + lc));
    let target: string | undefined;
    if (lc === 0 || aid === ISD) {
      target = ISD;
    } else if (this.instances.has(aid)) {
      // a whole AID wins, always: every AID of this package begins with the
      // package's own bytes, so a prefix rule that ran first would send every
      // SELECT of an applet to the opener
      target = aid;
    } else {
      // a partial name reaches the first instance made whose AID begins with it
      target = [...this.instances.keys()].find(a => a.startsWith(aid));
    }
    if (!target) {
      return sw(0x6a82);
    }
    this.endSessions();
    this.selected = target;
    if (target === ISD) {
      return sw(0x9000, fromHex('6F108408A000000151000000A5049F6501FF'));
    }
    const inst = this.instances.get(target)!;
    if (inst.module === CARD_MODULE) {
      // FedimintCardApplet: selectingApplet() sends VERSION_MAJOR, VERSION_MINOR and nothing else
      return sw(0x9000, [0x02, 0x00]);
    }
    // the opener: an Applet with no process(), so the toolkit answers 9000 with no data
    return sw(0x9000);
  }

  private applet(inst: Instance, apdu: Bytes): Bytes {
    if (inst.module !== CARD_MODULE) {
      return sw(0x6d00);
    }
    const [cla, ins, p1] = apdu;
    if (cla !== 0xb0) {
      return sw(0x6e00); // SW_CLA_NOT_SUPPORTED
    }
    switch (ins) {
      case 0x10:
        return this.genKey(inst.state, p1);
      case 0x11:
        return this.getPub(inst.state, p1);
      case 0x12:
        return this.sign(inst.state, p1);
      default:
        return sw(0x6d00);
    }
  }

  private genKey(state: CardAppletState, slot: number): Bytes {
    if (slot >= SLOTS) {
      return sw(0x6a86);
    }
    // FedimintCardApplet refuses a slot that already holds a key
    if (state.used[slot]) {
      return sw(0x6985);
    }
    // The real applet derives from a card key pair; the fake only needs the
    // shape: an uncompressed SEC1 point, 65 bytes, distinct per slot.
    const pub = [
      0x04,
      ...new Array(32).fill(0x5a ^ (slot + 1)),
      ...new Array(32).fill(0xa5),
    ];
    state.used[slot] = true;
    state.pubs[slot] = pub;
    return sw(0x9000, pub);
  }

  private getPub(state: CardAppletState, slot: number): Bytes {
    if (slot >= SLOTS || !state.used[slot]) {
      // ISO7816.SW_CONDITIONS_NOT_SATISFIED: the slot has no key
      return sw(0x6985);
    }
    return sw(0x9000, state.pubs[slot]);
  }

  private sign(state: CardAppletState, slot: number): Bytes {
    // The PIN gate comes first: without it, a reader learns nothing about the slots.
    if (!state.pinSet) {
      return sw(0x6a94); // "pin needed", per the applet's PIN gate
    }
    if (slot >= SLOTS || !state.used[slot]) {
      return sw(0x6985);
    }
    return sw(0x9000, new Array(64).fill(0x11));
  }

  private deriveSession(seq: number) {
    const s = [seq >> 8, seq & 0xff];
    const d = (key: Bytes, c: number[]) =>
      nodeCbc(key, [...c, ...s, ...new Array(12).fill(0)]);
    return {
      enc: d(this.keys.enc, [0x01, 0x82]),
      mac: d(this.keys.mac, [0x01, 0x01]),
      dek: d(this.keys.dek, [0x01, 0x81]),
    };
  }

  private isd(apdu: Bytes): Bytes {
    const [cla, ins, p1] = apdu;
    if (ins === 0x50 && cla === 0x80) {
      if (this.lockedOut) {
        return sw(0x6983);
      }
      if (p1 !== 0 && p1 !== this.kvn) {
        return sw(0x6a88);
      }
      if (apdu[4] !== 8 || apdu.length < 13) {
        return sw(0x6700);
      }
      this.endSessions();
      const host = apdu.slice(5, 13);
      this.challengeCounter++;
      const card = [
        0xc0,
        0xff,
        0xee,
        0x00,
        (this.challengeCounter >> 8) & 0xff,
        this.challengeCounter & 0xff,
      ];
      const keys = this.deriveSession(this.seq);
      const seq = [this.seq >> 8, this.seq & 0xff];
      this.handshake = {host, card, ...keys};
      const cryptogram = nodeFullMac(keys.enc, [...host, ...seq, ...card]);
      return sw(0x9000, [
        ...fromHex('0000936320C4A1B2C3D4'),
        this.kvn,
        0x02,
        ...seq,
        ...card,
        ...cryptogram,
      ]);
    }
    if (ins === 0x82 && cla === 0x84) {
      const hs = this.handshake;
      this.handshake = null;
      if (!hs) {
        return sw(0x6985);
      }
      const seq = [this.seq >> 8, this.seq & 0xff];
      const expectedHost = nodeFullMac(hs.enc, [
        ...seq,
        ...hs.card,
        ...hs.host,
      ]);
      const header = this.opts.macOverModifiedApdu
        ? [0x84, 0x82, p1, apdu[3], 0x10]
        : [0x84, 0x82, p1, apdu[3], 0x08];
      const host = apdu.slice(5, 13);
      const mac = apdu.slice(13, 21);
      const expectedMac = nodeRetailMac(hs.mac, [...header, ...host]);
      if (
        apdu[4] !== 0x10 ||
        hex(host) !== hex(expectedHost) ||
        hex(mac) !== hex(expectedMac) ||
        p1 !== 0x01
      ) {
        this.failedAuths++;
        if (this.failedAuths >= this.opts.failedAuthLimit) {
          this.lockedOut = true;
        }
        return sw(0x6300);
      }
      this.session = {mac: hs.mac, dek: hs.dek, lastMac: mac};
      this.seq++;
      return sw(0x9000);
    }
    if ((cla & 0x04) !== 0) {
      const s = this.session;
      if (!s) {
        return sw(0x6982);
      }
      const lc = apdu[4];
      if (apdu.length < 5 + lc || lc < 8) {
        this.session = null;
        return sw(0x6700);
      }
      const data = apdu.slice(5, 5 + lc - 8);
      const mac = apdu.slice(5 + lc - 8, 5 + lc);
      const le = apdu.slice(5 + lc);
      const original = cla & ~0x04;
      const header = this.opts.macOverModifiedApdu
        ? [cla, ins, p1, apdu[3], lc]
        : [original, ins, p1, apdu[3], lc - 8];
      const icv = nodeEcb(s.mac.slice(0, 8), s.lastMac);
      if (hex(nodeRetailMac(s.mac, [...header, ...data], icv)) !== hex(mac)) {
        this.session = null;
        return sw(0x6982);
      }
      s.lastMac = mac;
      return this.isdCommand(ins, p1, apdu[3], data, le);
    }
    if (this.session) {
      // a command without its MAC inside a secure channel ends the channel
      this.session = null;
      return sw(0x6982);
    }
    if (cla === 0x80 && ins === 0xca && p1 === 0xff && apdu[3] === 0x21) {
      return this.freeMemory();
    }
    if (cla === 0x80 && ins === 0xca && p1 === 0x9f && apdu[3] === 0x7f) {
      return sw(
        0x9000,
        fromHex(
          '9F7F2A4790D32147000000000030410933632055470000000000000000048C2337323937360000000000000000',
        ),
      );
    }
    if (ins === 0xe4 || ins === 0xe6 || ins === 0xe8 || ins === 0xd8) {
      return sw(0x6982);
    }
    return sw(0x6d00);
  }

  private freeMemory(): Bytes {
    const n = this.instances.size;
    const f = this.free;
    return sw(0x9000, [
      0xff,
      0x21,
      0x10,
      0x81,
      0x02,
      n >> 8,
      n & 0xff,
      0x82,
      0x04,
      (f >>> 24) & 0xff,
      (f >>> 16) & 0xff,
      (f >>> 8) & 0xff,
      f & 0xff,
      0x83,
      0x04,
      0x00,
      0x00,
      0x12,
      0xc0,
    ]);
  }

  private isdCommand(
    ins: number,
    p1: number,
    p2: number,
    data: Bytes,
    _le: Bytes,
  ): Bytes {
    if (ins === 0xca && p1 === 0xff && p2 === 0x21) {
      return this.freeMemory();
    }
    if (ins === 0xe4) {
      return this.delete(data);
    }
    if (
      ins === 0xf2 &&
      (p1 === 0x40 || p1 === 0x20) &&
      p2 === 2 &&
      hex(data) === '4F00'
    ) {
      const tlv = (tag: Bytes, value: Bytes) => [
        ...tag,
        value.length,
        ...value,
      ];
      const aid = (tag: number, value: string) => tlv([tag], fromHex(value));
      const entry = (id: string, pkg?: string) => {
        const fields = [
          ...aid(0x4f, id),
          ...tlv([0x9f, 0x70], [pkg ? 7 : 1]),
          ...(pkg ? [...tlv([0xc5], [0, 0, 0]), ...aid(0xc4, pkg)] : []),
          ...aid(0xcc, ISD),
        ];
        return tlv([0xe3], fields);
      };
      const entries =
        p1 === 0x40
          ? [...this.instances.values()].flatMap(i => entry(i.aid, i.pkg))
          : [...this.packages.keys(), ...this.partial.keys()].flatMap(id =>
              entry(id),
            );
      return sw(entries.length ? 0x9000 : 0x6a88, entries);
    }
    if (ins === 0xe6 && p1 === 0x02) {
      return this.installForLoad(data);
    }
    if (ins === 0xe8) {
      return this.load(p1, p2, data);
    }
    if (ins === 0xe6 && p1 === 0x0c) {
      return this.install(data);
    }
    if (ins === 0xd8) {
      return this.putKey(p1, p2, data);
    }
    return sw(0x6d00);
  }

  private delete(data: Bytes): Bytes {
    if (data[0] !== 0x4f || data[1] + 2 !== data.length) {
      return sw(0x6a80);
    }
    const aid = hex(data.slice(2));
    if (aid === ISD) {
      return sw(0x6985);
    }
    const inst = this.instances.get(aid);
    if (inst) {
      this.instances.delete(aid);
      this.free += 2048;
      return sw(0x9000, [0x00]);
    }
    const pkg = this.packages.get(aid);
    if (pkg) {
      if ([...this.instances.values()].some(i => i.pkg === aid)) {
        return sw(0x6985);
      }
      this.packages.delete(aid);
      this.free += pkg.size;
      return sw(0x9000, [0x00]);
    }
    if (this.partial.has(aid)) {
      this.free += this.partial.get(aid)!;
      this.partial.delete(aid);
      return sw(0x9000, [0x00]);
    }
    return sw(0x6a88);
  }

  private lv(data: Bytes, at: number): [Bytes, number] {
    const n = data[at];
    if (n === undefined || at + 1 + n > data.length) {
      throw new RangeError('bad LV');
    }
    return [data.slice(at + 1, at + 1 + n), at + 1 + n];
  }

  private installForLoad(data: Bytes): Bytes {
    let pkg: Bytes, sd: Bytes, at: number;
    try {
      [pkg, at] = this.lv(data, 0);
      [sd, at] = this.lv(data, at);
      [, at] = this.lv(data, at);
      [, at] = this.lv(data, at);
      [, at] = this.lv(data, at);
    } catch {
      return sw(0x6a80);
    }
    if (at !== data.length || (sd.length > 0 && hex(sd) !== ISD)) {
      return sw(0x6a80);
    }
    const aid = hex(pkg);
    if (this.packages.has(aid) || this.partial.has(aid)) {
      return sw(0x6985);
    }
    if (this.pending) {
      this.free += this.pending.data.length;
    }
    this.pending = {aid, data: [], next: 0};
    return sw(0x9000, [0x00]);
  }

  private load(p1: number, p2: number, data: Bytes): Bytes {
    const p = this.pending;
    if (!p) {
      return sw(0x6985);
    }
    if (p2 !== (p.next & 0xff)) {
      return sw(0x6a86);
    }
    p.data.push(...data);
    p.next++;
    this.free -= data.length;
    if ((p1 & 0x80) === 0) {
      return sw(0x9000);
    }
    this.pending = null;
    const parsed = parseLoadFile(p.data);
    if (!parsed || parsed.packageAid !== p.aid) {
      this.free += p.data.length;
      return sw(0x6a80);
    }
    this.packages.set(p.aid, {
      aid: p.aid,
      modules: parsed.modules,
      size: p.data.length,
      instancesMade: 0,
    });
    return sw(0x9000, [0x00]);
  }

  private install(data: Bytes): Bytes {
    let pkg: Bytes, mod: Bytes, inst: Bytes, at: number;
    try {
      [pkg, at] = this.lv(data, 0);
      [mod, at] = this.lv(data, at);
      [inst, at] = this.lv(data, at);
      [, at] = this.lv(data, at);
      [, at] = this.lv(data, at);
      [, at] = this.lv(data, at);
    } catch {
      return sw(0x6a80);
    }
    if (at !== data.length) {
      return sw(0x6a80);
    }
    const p = this.packages.get(hex(pkg));
    const module = hex(mod);
    const aid = hex(inst);
    if (!p || !p.modules.includes(module)) {
      return sw(0x6a88);
    }
    if (this.instances.has(aid) || aid === ISD) {
      return sw(0x6985);
    }
    if (this.opts.heavyModules.includes(module) && p.instancesMade === 0) {
      if (this.opts.openerRule === 'mute') {
        this.mute = true;
        throw new Error('the card does not answer');
      }
      return sw(0x6f00);
    }
    p.instancesMade++;
    this.instances.set(aid, {
      aid,
      module,
      pkg: p.aid,
      state: freshState(),
    });
    this.free -= 2048;
    return sw(0x9000, [0x00]);
  }

  private putKey(p1: number, p2: number, data: Bytes): Bytes {
    const s = this.session!;
    if (p1 !== this.kvn || p2 !== 0x81 || data.length !== 1 + 3 * 22) {
      return sw(0x6a80);
    }
    const keys: Bytes[] = [];
    const kcvs: Bytes[] = [];
    for (let i = 0; i < 3; i++) {
      const at = 1 + i * 22;
      if (
        data[at] !== 0x80 ||
        data[at + 1] !== 0x10 ||
        data[at + 18] !== 0x03
      ) {
        return sw(0x6a80);
      }
      const key = nodeEcb(s.dek, data.slice(at + 2, at + 18), true);
      const kcv = data.slice(at + 19, at + 22);
      if (hex(nodeEcb(key, new Array(8).fill(0)).slice(0, 3)) !== hex(kcv)) {
        return sw(0x6a80);
      }
      keys.push(key);
      kcvs.push(kcv);
    }
    this.keys = {enc: keys[0], mac: keys[1], dek: keys[2]};
    this.kvn = data[0];
    return sw(0x9000, [data[0], ...kcvs[0], ...kcvs[1], ...kcvs[2]]);
  }
}

/** The package AID (Header component) and applet AIDs (Applet component) of a load file. */
export function parseLoadFile(
  file: Bytes,
): {packageAid: string; modules: string[]} | null {
  if (file[0] !== 0xc4) {
    return null;
  }
  let at = 1;
  let len = file[at++];
  if (len === 0x81) {
    len = file[at++];
  } else if (len === 0x82) {
    len = (file[at] << 8) | file[at + 1];
    at += 2;
  }
  if (at + len !== file.length) {
    return null;
  }
  let packageAid = '';
  const modules: string[] = [];
  while (at < file.length) {
    const tag = file[at];
    const size = (file[at + 1] << 8) | file[at + 2];
    const body = file.slice(at + 3, at + 3 + size);
    if (tag === 0x01) {
      // magic (4), minor, major, flags, package minor, major, AID length, AID
      const n = body[9];
      packageAid = hex(body.slice(10, 10 + n));
    } else if (tag === 0x03) {
      let p = 1;
      for (let i = 0; i < body[0]; i++) {
        const n = body[p];
        modules.push(hex(body.slice(p + 1, p + 1 + n)));
        p += 1 + n + 2;
      }
    }
    at += 3 + size;
  }
  return packageAid ? {packageAid, modules} : null;
}

/**
 * A transceiver that loses the card at APDU `n`: `before` it reaches the
 * card, or `after` the card has acted on it and the reply is lost. Every call
 * after the pull is counted in `late` and fails.
 */
export function pullAt(card: FakeCard, n: number, when: 'before' | 'after') {
  const state = {calls: 0, late: 0, pulled: false};
  const tx = async (apdu: number[]): Promise<number[]> => {
    const i = state.calls++;
    if (state.pulled) {
      state.late++;
      throw new Error('the card is gone');
    }
    if (i === n) {
      state.pulled = true;
      if (when === 'after') {
        await card.transceive(apdu).catch(() => undefined);
      }
      throw new Error('Tag was lost');
    }
    return card.transceive(apdu);
  };
  return {tx, state};
}
