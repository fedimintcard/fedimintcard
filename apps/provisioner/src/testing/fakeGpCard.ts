/**
 * A J3R180 for the install and reset tests: a GlobalPlatform card manager that
 * keeps its keys across taps, and the Fedimint applet as it answers on the
 * wire. With `scp: 3` it is a J3R452's card manager instead. Test-only;
 * nothing in the app imports it.
 *
 * The card side of SCP02 is written out here from the card's static keys
 * (session keys, both cryptograms, the C-MAC chain with its encrypted ICV,
 * PUT KEY's decryption and check values), over cardkit's DES primitives only,
 * which cardkit's own tests hold to node:crypto. It never calls cardkit's host
 * side (`deriveSessionKeys`, `cardCryptogram`, `Session.wrap`, `putKeys`), so a
 * bug there fails here instead of agreeing with itself.
 *
 * The card side of SCP03 (level 01, i=10) is written out the same way, over
 * cardkit's `aesEcb` and `cmac` only, never its `scp03Kdf`, `Scp03Session` or
 * `putKeys`: the KDF, the 32-byte INITIALIZE UPDATE reply with the
 * pseudo-random challenge for the ISD's AID, a 3-byte counter that each
 * INITIALIZE UPDATE moves up by one, the host cryptogram, the C-MAC chained
 * through the full CMAC, and PUT KEY of AES keys under the static DEK with
 * their check values. PUT KEY replaces the set in place, or with P1 00 on a
 * card at version FF (`kvn: 0xff`, a J3R452 as it ships) puts the new set at
 * its version and drops the FF set, as the test card did at first contact.
 * GET DATA E0, plain, lists the three keys of the set the card holds.
 *
 * What it keeps that the demo's card does not: the card manager's keys and
 * their version (PUT KEY changes them), failed EXTERNAL AUTHENTICATEs (counted;
 * a card manager locks at its limit), CPLC, other packages (a Cashu applet on
 * the same chip) and GET STATUS over them, and the applet's slots and PIN with
 * its tries.
 *
 * `pull` loses the card at a chosen APDU, before the card acts on it or after
 * it acted and the reply was lost; every APDU after that throws until
 * `reset()` (the next tap).
 */
import {
  CARD_AID,
  FACTORY_KEYS,
  FEDIMINT_PACKAGE_AID,
  ISD_AID,
  OPENER_AID,
  aesEcb,
  cmac,
  desEcb,
  fromHex,
  retailMac,
  tdesCbc,
  tdesCbcMac,
  tdesEcb,
  toHex,
  type KeySet,
} from '../cardkit';

const ok = (data: number[] = []) => [...data, 0x90, 0x00];
const sw = (word: number) => [word >> 8, word & 0xff];
const h = (b: ArrayLike<number>) => toHex(Array.from(b));

/** The CPLC of the owner's J3R180 (IC serial 09336320), as GET DATA 9F7F answers it. */
export const J3R180_CPLC = fromHex(
  '9F7F2A4790D321470000000000304109336320554700000000000000000432A939333336330000000000000000',
);
export const J3R180_CHIP = 'cplc-v1:4790d3213041093363205547';

/**
 * A J3R452's CPLC, made up for these tests: NXP's fabricator 4790 and the
 * P71D600's IC type D600, with a fabrication date, serial and batch of its own
 * (not the test card's).
 */
export const J3R452_CPLC = fromHex(
  '9F7F2A4790D6004700000000005100452452450452000000000000000000000000000000000000000000000000',
);
export const J3R452_CHIP = 'cplc-v1:4790d6005100452452450452';

/** Amendment D's KDF, written out over cardkit's CMAC: eleven zeros, the constant, 00, L in bits, 01, the context. */
function kdf03(key: number[], constant: number, bits: 64 | 128, context: number[]): number[] {
  const label = [...new Array(11).fill(0), constant, 0x00, bits >> 8, bits & 0xff, 0x01];
  return cmac(key, [...label, ...context]).slice(0, bits / 8);
}

/** A Cashu card's package and applet: on the chip, never this app's to touch. */
export const CASHU_PACKAGE = 'F043415348554341';
export const CASHU_APPLET = 'F04341534855434101';

const PKG = h(FEDIMINT_PACKAGE_AID);
const CARD = h(CARD_AID);
const OPENER = h(OPENER_AID);
const ISD = h(ISD_AID);

export interface AppletState {
  slots: (number[] | null)[];
  pin: string | null;
  tries: number;
  blocked: boolean;
  verified: boolean;
  /** Applet 3.0 (32 slots): the day's limit in msat, 8 bytes BE; absent until SET_LIMIT. */
  limit?: number[];
  /** The attestation public point (65 bytes) once ATTEST_INIT made it. */
  attest?: number[] | null;
  /** The certificate, once SET_CERT took it. */
  cert?: number[] | null;
  /** The registry's certificate key (65 bytes) once SET_REGISTRY_KEY took it. */
  registryKey?: number[] | null;
}

interface Instance {
  pkg: string;
  module: string;
  applet?: AppletState;
}

export type When = 'before' | 'after';

export class FakeGpCard {
  keys: {enc: number[]; mac: number[]; dek: number[]};
  kvn: number;
  /** SCP02's counter, moved by each EXTERNAL AUTHENTICATE. */
  seq = 0x0021;
  /** SCP03's 3-byte counter, moved up by one at each INITIALIZE UPDATE. */
  counter03 = 0;
  /** The secure channel the card manager opens: 2 (the J3R180) or 3 (the J3R452). */
  readonly scp: 2 | 3;
  /** PUT KEY commands the card took (answered 9000). */
  putKeysTaken = 0;
  failedAuths = 0;
  readonly failedAuthLimit: number;
  locked = false;
  /** Every APDU received, in order. */
  readonly log: number[][] = [];
  readonly packages = new Set<string>();
  readonly instances = new Map<string, Instance>();
  /** Keys generated so far: each GEN_KEY makes a new one, so a reinstalled card has a new short id. */
  private generation = 0;
  private selected: string = ISD;
  private handshake: {
    host: number[];
    card: number[];
    s: {enc: number[]; mac: number[]; dek: number[]};
  } | null = null;
  /** `lastMac`: SCP02's last C-MAC, or SCP03's last full CMAC (the chaining value). */
  private session: {mac: number[]; dek: number[]; lastMac: number[]} | null =
    null;
  private loading: {pkg: string; next: number} | null = null;
  private pulledAt: {
    test: (apdu: number[], i: number) => boolean;
    when: When;
  } | null = null;
  private gone = false;

  readonly cplc: number[];
  /** How many slots an applet installed on this card has: 8 before applet 2.1, 32 from it. */
  readonly appletSlots: number;

  constructor(
    options: {
      keys?: KeySet;
      failedAuthLimit?: number;
      cplc?: number[];
      appletSlots?: number;
      /** 2 (the J3R180, by default) or 3 (the J3R452). */
      scp?: 2 | 3;
      /** The key set version: 01 by default; FF is a J3R452 as it ships. */
      kvn?: number;
    } = {},
  ) {
    this.appletSlots = options.appletSlots ?? 8;
    this.scp = options.scp ?? 2;
    this.kvn = options.kvn ?? 0x01;
    const k = options.keys ?? FACTORY_KEYS;
    this.cplc = options.cplc ?? J3R180_CPLC;
    this.keys = {
      enc: Array.from(k.enc),
      mac: Array.from(k.mac),
      dek: Array.from(k.dek),
    };
    this.failedAuthLimit = options.failedAuthLimit ?? 10;
  }

  /** A Cashu applet on the same chip. */
  withCashu(): this {
    this.packages.add(CASHU_PACKAGE);
    this.instances.set(CASHU_APPLET, {
      pkg: CASHU_PACKAGE,
      module: CASHU_APPLET,
    });
    return this;
  }

  /**
   * The Fedimint applet as a lifted install tap leaves it: the card manager
   * on `keys` (the card's own, after the rotation), the first `keyed` slots
   * generated, and the PIN `pin` (null: not set).
   */
  withLiftedApplet(keyed: number, pin: string | null = null): this {
    this.packages.add(PKG);
    this.instances.set(OPENER, {pkg: PKG, module: OPENER});
    const applet: AppletState = {
      slots: new Array(this.appletSlots).fill(null),
      pin,
      tries: 3,
      blocked: false,
      verified: false,
    };
    for (let slot = 0; slot < keyed; slot++) {
      applet.slots[slot] = this.newKey(slot);
    }
    this.instances.set(CARD, {pkg: PKG, module: CARD, applet});
    return this;
  }

  /**
   * The Fedimint applet as a finished card has it: a PIN, and the slots the
   * install tap keys (0-7: every slot of an 8-slot card, the first 8 of a
   * 32-slot one), or `keyed` of them.
   */
  withFinishedApplet(pin = '2468', keyed = Math.min(this.appletSlots, 8)): this {
    this.packages.add(PKG);
    this.instances.set(OPENER, {pkg: PKG, module: OPENER});
    const applet: AppletState = {
      slots: [],
      pin,
      tries: 3,
      blocked: false,
      verified: false,
    };
    for (let slot = 0; slot < this.appletSlots; slot++) {
      applet.slots.push(slot < keyed ? this.newKey(slot) : null);
    }
    this.instances.set(CARD, {pkg: PKG, module: CARD, applet});
    return this;
  }

  get applet(): AppletState | undefined {
    return this.instances.get(CARD)?.applet;
  }

  /** The keys as a KeySet, to compare. */
  sameKeys(keys: KeySet): boolean {
    return (
      h(keys.enc) === h(this.keys.enc) &&
      h(keys.mac) === h(this.keys.mac) &&
      h(keys.dek) === h(this.keys.dek)
    );
  }

  /** INS of every APDU received. */
  ins(): number[] {
    return this.log.map(a => a[1]);
  }

  /** The card leaves the field at the first APDU `test` matches. */
  pull(test: (apdu: number[], i: number) => boolean, when: When): void {
    this.pulledAt = {test, when};
  }

  /** The next tap: power off and on. */
  reset(): void {
    this.gone = false;
    this.pulledAt = null;
    this.selected = ISD;
    this.handshake = null;
    this.session = null;
    this.loading = null;
    if (this.applet) {
      this.applet.verified = false;
    }
  }

  readonly transceive = async (apdu: number[]): Promise<number[]> => {
    const i = this.log.length;
    this.log.push([...apdu]);
    if (this.gone) {
      throw new Error('Tag was lost');
    }
    const p = this.pulledAt;
    if (p && p.test(apdu, i)) {
      this.gone = true;
      if (p.when === 'after') {
        this.process(apdu);
      }
      throw new Error('Tag was lost');
    }
    return this.process(apdu);
  };

  private newKey(slot: number): number[] {
    this.generation++;
    return Array.from({length: 32}, (_, i) =>
      i < 28
        ? (slot * 17 + i) & 0xff
        : [0xc0, slot, this.generation, 0x5d][i - 28],
    );
  }

  private process(apdu: number[]): number[] {
    const [cla, ins] = apdu;
    if ((cla & 0xfc) === 0x00 && ins === 0xa4) {
      return this.select(apdu);
    }
    if (this.selected === ISD) {
      return this.isd(apdu);
    }
    const inst = this.instances.get(this.selected);
    if (inst?.applet) {
      return this.cardApplet(inst.applet, apdu);
    }
    return sw(0x6d00);
  }

  private select(apdu: number[]): number[] {
    const aid = h(apdu.slice(5, 5 + apdu[4]));
    if (this.applet) {
      this.applet.verified = false; // CLEAR_ON_DESELECT
    }
    this.session = null;
    this.handshake = null;
    if (aid === ISD) {
      this.selected = ISD;
      return ok();
    }
    if (!this.instances.has(aid)) {
      return sw(0x6a82);
    }
    this.selected = aid;
    if (aid !== CARD) {
      return ok();
    }
    // The applet's version: 3.0 has 32 slots, 2.0 had 8.
    const slots = this.instances.get(aid)?.applet?.slots.length ?? 8;
    return ok(slots === 32 ? [0x03, 0x00] : [0x02, 0x00]);
  }

  private derive(seq: number[]) {
    const c = (k: number[], constant: number[]) =>
      tdesCbc(k, [...constant, ...seq, ...new Array(12).fill(0)]);
    return {
      enc: c(this.keys.enc, [0x01, 0x82]),
      mac: c(this.keys.mac, [0x01, 0x01]),
      dek: c(this.keys.dek, [0x01, 0x81]),
    };
  }

  private isd(apdu: number[]): number[] {
    const [cla, ins, p1, p2] = apdu;
    if (cla === 0x80 && ins === 0xca && p1 === 0x9f && p2 === 0x7f) {
      return ok(this.cplc);
    }
    if (cla === 0x80 && ins === 0xca && p1 === 0xff && p2 === 0x21) {
      return ok([0xff, 0x21, 0x06, 0x82, 0x04, 0x00, 0x02, 0x80, 0x50]);
    }
    if (cla === 0x80 && ins === 0xca && p1 === 0x00 && p2 === 0xe0) {
      // the key information template: keys 01 to 03 of the set the card holds
      const type = this.scp === 3 ? 0x88 : 0x80;
      return ok([0xe0, 0x12, ...[1, 2, 3].flatMap(id => [0xc0, 0x04, id, this.kvn, type, 0x10])]);
    }
    if (cla === 0x80 && ins === 0x50) {
      if (this.locked) {
        return sw(0x6983);
      }
      if (this.scp === 3) {
        return this.initializeUpdate03(apdu.slice(5, 13));
      }
      const seq = [this.seq >> 8, this.seq & 0xff];
      const host = apdu.slice(5, 13);
      const card = [
        0x5e,
        0x11,
        0xc0,
        0xde,
        (this.log.length >> 8) & 0xff,
        this.log.length & 0xff,
      ];
      const s = this.derive(seq);
      this.handshake = {host, card, s};
      return ok([
        ...new Array(10).fill(0x11),
        this.kvn,
        0x02,
        ...seq,
        ...card,
        ...tdesCbcMac(s.enc, [...host, ...seq, ...card]),
      ]);
    }
    if (cla === 0x84 && ins === 0x82) {
      const hs = this.handshake;
      this.handshake = null;
      if (!hs) {
        return sw(0x6985);
      }
      if (this.scp === 3) {
        return this.externalAuthenticate03(hs, apdu);
      }
      const seq = [this.seq >> 8, this.seq & 0xff];
      const cryptogram = apdu.slice(5, 13);
      const mac = apdu.slice(13, 21);
      const expected = tdesCbcMac(hs.s.enc, [...seq, ...hs.card, ...hs.host]);
      const expectedMac = retailMac(hs.s.mac, [
        0x84,
        0x82,
        p1,
        p2,
        0x10,
        ...cryptogram,
      ]);
      if (h(cryptogram) !== h(expected) || h(mac) !== h(expectedMac)) {
        this.failedAuths++;
        this.locked = this.failedAuths >= this.failedAuthLimit;
        return sw(0x6300);
      }
      this.session = {mac: hs.s.mac, dek: hs.s.dek, lastMac: mac};
      this.seq++;
      return ok();
    }
    if ((cla & 0x04) !== 0) {
      return this.wrapped(apdu);
    }
    return sw(0x6d00);
  }

  /** A command under the C-MAC: the MAC checked over the header as sent, the ICV encrypted. */
  private wrapped(apdu: number[]): number[] {
    const s = this.session;
    if (!s) {
      return sw(0x6982);
    }
    const [cla, ins, p1, p2, lc] = apdu;
    const data = apdu.slice(5, 5 + lc - 8);
    const mac = apdu.slice(5 + lc - 8, 5 + lc);
    if (this.scp === 3) {
      // the full CMAC over the chaining value and the header as sent
      const full = cmac(s.mac, [...s.lastMac, cla, ins, p1, p2, lc, ...data]);
      if (h(full.slice(0, 8)) !== h(mac)) {
        this.session = null;
        return sw(0x6982);
      }
      s.lastMac = full;
    } else {
      const icv = desEcb(s.mac.slice(0, 8), s.lastMac);
      if (h(retailMac(s.mac, [cla, ins, p1, p2, lc, ...data], icv)) !== h(mac)) {
        this.session = null;
        return sw(0x6982);
      }
      s.lastMac = mac;
    }
    switch (ins) {
      case 0xe4:
        return this.delete(data);
      case 0xe6:
        return p1 === 0x02 ? this.installForLoad(data) : this.install(data);
      case 0xe8:
        return this.load(p1, p2);
      case 0xd8:
        return this.putKey(p1, p2, data);
      case 0xf2:
        return this.getStatus(p1, p2, data);
      default:
        return sw(0x6d00);
    }
  }

  private delete(data: number[]): number[] {
    const aid = h(data.slice(2, 2 + data[1]));
    if (aid === ISD) {
      return sw(0x6985);
    }
    if (this.instances.delete(aid)) {
      return ok([0x00]);
    }
    if (this.packages.has(aid)) {
      if ([...this.instances.values()].some(i => i.pkg === aid)) {
        return sw(0x6985);
      }
      this.packages.delete(aid);
      return ok([0x00]);
    }
    return sw(0x6a88);
  }

  /**
   * GET STATUS as gp --list sends it: P1 40 (applications) or 20 (load
   * files), P2 02 (TLV), every entry (4F 00). One E3 per object: its AID, its
   * life cycle, and for an instance its load file; 6A88 when there are none.
   */
  private getStatus(p1: number, p2: number, data: number[]): number[] {
    if ((p1 !== 0x40 && p1 !== 0x20) || p2 !== 0x02 || h(data) !== '4F00') {
      return sw(0x6a86);
    }
    const tlv = (tag: number[], value: number[]) => [
      ...tag,
      value.length,
      ...value,
    ];
    const entry = (aid: string, pkg?: string) =>
      tlv(
        [0xe3],
        [
          ...tlv([0x4f], fromHex(aid)),
          ...tlv([0x9f, 0x70], [pkg ? 0x07 : 0x01]),
          ...(pkg ? tlv([0xc4], fromHex(pkg)) : []),
          ...tlv([0xcc], fromHex(ISD)),
        ],
      );
    const entries =
      p1 === 0x40
        ? [...this.instances.entries()].flatMap(([aid, i]) => entry(aid, i.pkg))
        : [...this.packages].flatMap(aid => entry(aid));
    return entries.length ? ok(entries) : sw(0x6a88);
  }

  private installForLoad(data: number[]): number[] {
    const pkg = h(data.slice(1, 1 + data[0]));
    if (this.packages.has(pkg)) {
      return sw(0x6985);
    }
    this.loading = {pkg, next: 0};
    return ok([0x00]);
  }

  private load(p1: number, p2: number): number[] {
    if (!this.loading || p2 !== this.loading.next) {
      return sw(0x6985);
    }
    this.loading.next++;
    if (p1 === 0x80) {
      this.packages.add(this.loading.pkg);
      this.loading = null;
    }
    return ok([0x00]);
  }

  private install(data: number[]): number[] {
    let at = 0;
    const field = () => {
      const n = data[at];
      const v = h(data.slice(at + 1, at + 1 + n));
      at += 1 + n;
      return v;
    };
    const pkg = field();
    const module = field();
    const instance = field();
    if (!this.packages.has(pkg) || this.instances.has(instance)) {
      return sw(0x6985);
    }
    this.instances.set(instance, {
      pkg,
      module,
      ...(module === CARD
        ? {
            applet: {
              // What INSTALL puts on the card is the load file the app carries: applet 3.0.
              slots: new Array(32).fill(null),
              pin: null,
              tries: 3,
              blocked: false,
              verified: false,
            },
          }
        : {}),
    });
    return ok([0x00]);
  }

  private putKey(p1: number, p2: number, data: number[]): number[] {
    const s = this.session!;
    if (this.scp === 3) {
      return this.putKey03(s.dek, p1, p2, data);
    }
    if (p1 !== this.kvn || p2 !== 0x81 || data.length !== 1 + 3 * 22) {
      return sw(0x6a80);
    }
    const keys: number[][] = [];
    for (let i = 0; i < 3; i++) {
      const at = 1 + i * 22;
      const key = tdesEcb(s.dek, data.slice(at + 2, at + 18), true);
      if (
        h(tdesEcb(key, new Array(8).fill(0)).slice(0, 3)) !==
        h(data.slice(at + 19, at + 22))
      ) {
        return sw(0x6a80);
      }
      keys.push(key);
    }
    this.keys = {enc: keys[0], mac: keys[1], dek: keys[2]};
    this.kvn = data[0];
    this.putKeysTaken++;
    return ok([this.kvn]);
  }

  /** SCP03's INITIALIZE UPDATE: the counter up by one, the challenge for it and the ISD's AID, then the cryptogram. */
  private initializeUpdate03(host: number[]): number[] {
    this.counter03 = (this.counter03 + 1) & 0xffffff;
    const seq = [(this.counter03 >> 16) & 0xff, (this.counter03 >> 8) & 0xff, this.counter03 & 0xff];
    const card = kdf03(this.keys.enc, 0x02, 64, [...seq, ...fromHex(ISD)]);
    const context = [...host, ...card];
    // S-ENC (04) is not used at level 01; SCP03's PUT KEY takes the static DEK as it is
    const s = {
      enc: kdf03(this.keys.enc, 0x04, 128, context),
      mac: kdf03(this.keys.mac, 0x06, 128, context),
      dek: [...this.keys.dek],
    };
    this.handshake = {host, card, s};
    return ok([
      ...new Array(10).fill(0x11),
      this.kvn,
      0x03,
      0x10,
      ...card,
      ...kdf03(s.mac, 0x00, 64, context),
      ...seq,
    ]);
  }

  /** SCP03's EXTERNAL AUTHENTICATE: the host cryptogram, and its C-MAC chained from sixteen zeros. */
  private externalAuthenticate03(
    hs: {host: number[]; card: number[]; s: {mac: number[]; dek: number[]}},
    apdu: number[],
  ): number[] {
    const [, , p1, p2] = apdu;
    const cryptogram = apdu.slice(5, 13);
    const mac = apdu.slice(13, 21);
    const expected = kdf03(hs.s.mac, 0x01, 64, [...hs.host, ...hs.card]);
    const full = cmac(hs.s.mac, [...new Array(16).fill(0), 0x84, 0x82, p1, p2, 0x10, ...cryptogram]);
    if (h(cryptogram) !== h(expected) || h(mac) !== h(full.slice(0, 8))) {
      this.failedAuths++;
      this.locked = this.failedAuths >= this.failedAuthLimit;
      return sw(0x6300);
    }
    this.session = {mac: hs.s.mac, dek: hs.s.dek, lastMac: full};
    return ok();
  }

  /**
   * SCP03's PUT KEY: three 88 11 10 <the key under the static DEK, AES-ECB> 03
   * <its AES check value> entries. In place (P1 the current version), or with
   * P1 00 on a card at FF to a version from 01 to 7F: first contact, after
   * which the FF set is gone.
   */
  private putKey03(dek: number[], p1: number, p2: number, data: number[]): number[] {
    const firstContact = p1 === 0x00 && this.kvn === 0xff && data[0] >= 0x01 && data[0] <= 0x7f;
    if ((p1 !== this.kvn && !firstContact) || p2 !== 0x81 || data.length !== 1 + 3 * 23) {
      return sw(0x6a80);
    }
    const keys: number[][] = [];
    for (let i = 0; i < 3; i++) {
      const at = 1 + i * 23;
      if (data[at] !== 0x88 || data[at + 1] !== 0x11 || data[at + 2] !== 0x10 || data[at + 19] !== 0x03) {
        return sw(0x6a80);
      }
      const key = aesEcb(dek, data.slice(at + 3, at + 19), true);
      if (h(aesEcb(key, new Array(16).fill(0x01)).slice(0, 3)) !== h(data.slice(at + 20, at + 23))) {
        return sw(0x6a80);
      }
      keys.push(key);
    }
    this.keys = {enc: keys[0], mac: keys[1], dek: keys[2]};
    this.kvn = data[0];
    this.putKeysTaken++;
    return ok([this.kvn]);
  }

  /** FedimintCardApplet's slot and PIN commands, as the applet answers them. */
  private cardApplet(a: AppletState, apdu: number[]): number[] {
    const [cla, ins, p1] = apdu;
    if (cla !== 0xb0) {
      return sw(0x6e00);
    }
    const lc = apdu.length > 5 ? apdu[4] : 0;
    const data = apdu.slice(5, 5 + lc);
    // A slot past the last answers 6B00 before anything else (FedimintCardApplet.slot).
    if ((ins === 0x10 || ins === 0x11) && p1 >= a.slots.length) {
      return sw(0x6b00);
    }
    switch (ins) {
      case 0x11:
        return a.slots[p1] ? ok(a.slots[p1]!) : sw(0x6985);
      case 0x10: {
        // P2 = 01 overwrites a used slot, which once a PIN is set takes a
        // verified PIN (FedimintCardApplet.genKey); an empty slot is made either way.
        const p2 = apdu[3];
        if (p2 !== 0x00 && p2 !== 0x01) {
          return sw(0x6b00);
        }
        if (a.slots[p1]) {
          if (p2 !== 0x01) {
            return sw(0x6985);
          }
          if (a.blocked) {
            return sw(0x6983);
          }
          if (a.pin !== null && !a.verified) {
            return sw(0x6a94);
          }
          a.slots[p1] = null;
        }
        a.slots[p1] = this.newKey(p1);
        return ok(a.slots[p1]!);
      }
      case 0x41:
        if (a.blocked) {
          return sw(0x6983);
        }
        if (a.pin !== null) {
          return sw(0x6985);
        }
        a.pin = String.fromCharCode(...data);
        return ok();
      case 0x40: {
        if (a.blocked) {
          return sw(0x6983);
        }
        if (a.pin === null) {
          return sw(0x6984);
        }
        if (lc < 4 || lc > 8) {
          return sw(0x6700);
        }
        if (String.fromCharCode(...data) === a.pin) {
          a.tries = 3;
          a.verified = true;
          return ok();
        }
        a.tries--;
        if (a.tries === 0) {
          a.blocked = true;
          return sw(0x6983);
        }
        return sw(0x63c0 | a.tries);
      }
      default:
        return a.slots.length === 32 ? this.offlineApplet(a, ins, data) : sw(0x6d00);
    }
  }

  private attestations = 0;

  /**
   * GET_STATE's length: 59 from the applet that has SET_REGISTRY_KEY, 58 before
   * it (an older 3.0 build, which answers INS 34 with `6D 00`).
   */
  stateLength: 58 | 59 = 59;

  /** Applet 3.0's GET_STATE, SET_LIMIT, ATTEST_INIT, SET_CERT and GET_CERT (FedimintCardApplet). */
  private offlineApplet(a: AppletState, ins: number, data: number[]): number[] {
    const beforePinAndCert = a.pin === null && !a.cert;
    switch (ins) {
      case 0x16: {
        const out: number[] = a.slots.map(s => (s ? 1 : 0));
        out.push(0, 0, 0, 0, 0, 0, 0, 0, ...(a.limit ?? new Array(8).fill(0)));
        out.push(...new Array(8).fill(0), a.cert ? a.cert.length : 0, a.attest ? 1 : 0);
        if (this.stateLength === 59) {
          out.push(a.registryKey ? 1 : 0);
        }
        return ok(out);
      }
      case 0x18:
        if (data.length !== 8) {
          return sw(0x6700);
        }
        if (a.pin !== null) {
          return sw(0x6985);
        }
        a.limit = [...data];
        return ok();
      case 0x30:
        if (data.length !== 0) {
          return sw(0x6700);
        }
        if (!beforePinAndCert) {
          return sw(0x6985);
        }
        this.attestations++;
        a.attest = [0x04, ...new Array(64).fill(0).map((_, i) => (this.attestations * 7 + i) & 0xff)];
        return ok(a.attest);
      case 0x31:
        if (data.length < 1 || data.length > 160) {
          return sw(0x6700);
        }
        if (!beforePinAndCert || !a.attest) {
          return sw(0x6985);
        }
        a.cert = [...data];
        return ok();
      case 0x32:
        return a.cert ? ok(a.cert) : sw(0x6a83);
      case 0x34:
        if (this.stateLength !== 59) {
          return sw(0x6d00);
        }
        if (data.length !== 65 || data[0] !== 0x04) {
          return sw(0x6700);
        }
        if (a.pin !== null) {
          return sw(0x6985);
        }
        a.registryKey = [...data];
        return ok();
      default:
        return sw(0x6d00);
    }
  }
}
