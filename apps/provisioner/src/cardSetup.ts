/**
 * Finishing a card on the phone, in the tap that installed it (docs/OPS-PLAN.md
 * § 4). The install leaves an applet with empty slots (8 before applet 2.1,
 * 32 from 2.1 on) and no PIN; this reads how many slots the card has, asks it
 * to generate its keys, reads them back, gives it the registry's certificate
 * and key, anchors its clock to a Bitcoin block header and sets the PIN, all
 * in one selection (applet 4.0, DESIGN § 4). It is the sidecar's `key_setup` (`host/sidecar/src/cards.rs`)
 * and `card_setup` (`glue.rs`) moved here, over the same NFC session, so a
 * card is finished with no laptop and no relay, on iOS as on Android.
 *
 * The APDUs are the host's (`host/card/src/lib.rs`): CLA B0, `GEN_KEY` 10 and
 * `GET_PUB` 11 with P1 = slot and Le 00. On applet 4.0 `SET_PIN` 41 and
 * `VERIFY_PIN` 40 are sealed (P1 = 01, docs/OFFLINE-PROTOCOL.md § 9,
 * `sealedPin.ts`): GET_NONCE first, the card's certificate and PIN key checked,
 * then one envelope. Only the reset of an applet before 4.0 still sends the
 * PIN's ASCII digits as data (`oldCardResetPinApdu`, the reset of an old card only). Existing keys are kept, as the sidecar keeps
 * them, except on a resume (`rekey`): a card an earlier tap left keyed with no
 * PIN gets every slot generated again with P2 = 01, which the applet
 * allows only while no PIN is set (FedimintCardApplet.genKey). Nothing here
 * sends P2 = 01 to a card with a PIN.
 *
 * The PIN is a parameter and nothing else: it goes to the card sealed in one
 * APDU (SET_PIN, whose selection counts as having the PIN, so no VERIFY_PIN
 * follows) and is not kept, logged or returned. No PIN is sealed to a key that
 * has not been checked under the registry's certificate key.
 */
import {CARD_AID, type Transceiver} from './cardkit';
import {checkHeader, HEADER_LEN} from './header';
import {CheckedPinKey, pinBlock, sealedPinApdu, TrustedCard} from './sealedPin';

/**
 * The slot counts an applet can have: 8 before applet 2.1, 32 from 2.1 on.
 * A card's own count is read from the card (`probeSlotCount`), never assumed,
 * so a card from either applet finishes here; the registry takes 8 or 32 keys.
 */
export const SLOT_COUNTS = [8, 32] as const;

/**
 * The install-tap seam: how many slots, from 0, `finishCard` keys in the tap.
 * Split keying: a slot's first GEN_KEY takes ~318 ms on a J3R180
 * (docs/CAP-BUILD.md), so all 32 would hold the tap for ~10 s. The tap keys
 * slots 0-7, which is every slot of an 8-slot card, and a 32-slot card is
 * registered with `slot_count: 32` and those 8 keys. Slots 8-31 stay empty
 * until a till generates them at a later tap (GEN_KEY P2 = 00, which the
 * applet allows on an empty slot with or without a PIN) and posts them to the
 * registry (`POST /cards/:id/slot-keys`).
 */
export const INSTALL_TAP_KEY_SLOTS = 8;

const CLA = 0xb0;
export const INS_GEN_KEY = 0x10;
export const INS_GET_PUB = 0x11;
export const INS_VERIFY_PIN = 0x40;
const INS_SET_PIN = 0x41;
/** Applet 3.0 (docs/OFFLINE-PROTOCOL.md § 1). */
export const INS_GET_STATE = 0x16;
export const INS_SET_LIMIT = 0x18;
export const INS_ATTEST_INIT = 0x30;
export const INS_SET_CERT = 0x31;
export const INS_GET_CERT = 0x32;
/** SET_REGISTRY_KEY: the registry's certificate key, 65 bytes; only before SET_PIN. */
export const INS_SET_REGISTRY_KEY = 0x34;
/** SET_HEADER: an 80-byte Bitcoin block header; answers the card's clock (4 bytes, big-endian). */
export const INS_SET_HEADER = 0x19;

/** The applet major this build sets up and reads (SELECT answers major, minor). */
export const APPLET_MAJOR = 4;
/** GET_STATE's length on applet 4.0. */
export const STATE_LEN = 65;
/** What the person is told about a card whose applet is older than 4.0. */
export const OLD_APPLET = 'This card has an older applet. Reset it and set it up again.';
/** What the person is told when there is no block header to anchor the card's clock to. */
export const NEEDS_INTERNET =
  'Setting up a card needs the internet: the latest Bitcoin block could not be fetched. Nothing was written to the card.';

/** The most a certificate can be (FedimintCardApplet.CERT_MAX). */
export const CERT_MAX = 160;
/** The default offline limit for a day, in sat; 0 is none (SET_LIMIT). */
// No limit by default (owner, 2026-11-07): a limit is for village batches that pay offline.
export const DEFAULT_DAILY_LIMIT_SAT = 0;

export const SW_OK = 0x9000;
/** GET_PUB: the slot is empty. SET_PIN: a PIN is already set. */
export const SW_CONDITIONS = 0x6985;
/** VERIFY_PIN or SET_PIN on a blocked card: applets before 4.0 only, read by the reset of an old card. */
export const SW_BLOCKED = 0x6983;
/** VERIFY_PIN on applet 4.0: a wait is owed before the next try; the same VERIFY_PIN is sent again. */
export const SW_KEEP_HOLDING = 0x6a98;
/** SET_HEADER: too little work, or weak; nothing written. */
export const SW_LITTLE_WORK = 0x6a93;
/** SET_PIN: this selection did not anchor the clock. */
export const SW_NOT_ANCHORED = 0x6a96;
/** VERIFY_PIN: no PIN was ever set on this applet. */
export const SW_PIN_NOT_SET = 0x6984;
/** VERIFY_PIN refused for its length, before any try is spent: a PIN is set and not blocked. */
export const SW_WRONG_LENGTH = 0x6700;
/** GEN_KEY, GET_PUB or SIGN for a slot past the card's last (FedimintCardApplet.slot). */
export const SW_WRONG_P1P2 = 0x6b00;

/** What the applet's PIN is, read without spending a try. `blocked` comes only from an applet before 4.0. */
export type PinStatus = 'set' | 'blocked' | 'notSet';

/** True when SELECT answered 9000 with exactly two bytes, major 4: the only applet this build sends a PIN command. */
export function isCurrentApplet(selected: {readonly body: readonly number[]; readonly sw: number | null}): boolean {
  return selected.sw === SW_OK && selected.body.length === 2 && selected.body[0] === APPLET_MAJOR;
}

/**
 * The anchor set-up needs, checked on the phone: the offline setup's block
 * header, passing `checkHeader` at the setup's clock. Checked before a tap
 * touches the card, and again before `finishCard`'s first APDU.
 */
export function checkedAnchor(
  offline: OfflineSetup | undefined,
): {readonly ok: true; readonly header: readonly number[]; readonly time: number} | {readonly ok: false; readonly reason: string} {
  const header = offline?.header ?? null;
  if (!offline || header === null) {
    return {ok: false, reason: NEEDS_INTERNET};
  }
  const checked = checkHeader(header, (offline.now ?? Date.now)());
  return checked.ok
    ? {ok: true, header, time: checked.time}
    : {ok: false, reason: `${NEEDS_INTERNET} (${checked.reason})`};
}

export function stateApdu(): number[] {
  return [CLA, INS_GET_STATE, 0x00, 0x00, 0x00];
}

/** Applet 4.0's GET_STATE, the parts this app reads. */
export interface CardState {
  /** The card's clock, seconds (bytes 32-35, big-endian). */
  readonly now: number;
  readonly certLength: number;
  readonly registryKeySet: boolean;
  /** Byte 59: 0 no PIN, 1 set. */
  readonly pinSet: boolean;
  /** Byte 60: wrong tries in a row (15 or more reads 15). */
  readonly wrongTries: number;
  /** Bytes 61-62: askings owed before the next try. */
  readonly owed: number;
  /** Byte 63: this selection anchored the clock. */
  readonly anchored: boolean;
}

/** GET_STATE's body read as applet 4.0's 65 bytes; null for any other length or an unknown PIN state. */
export function parseState(body: readonly number[]): CardState | null {
  if (body.length !== STATE_LEN || (body[59] !== 0 && body[59] !== 1)) {
    return null;
  }
  return {
    now: ((body[32] << 24) | (body[33] << 16) | (body[34] << 8) | body[35]) >>> 0,
    certLength: body[56],
    registryKeySet: body[58] === 1,
    pinSet: body[59] === 1,
    wrongTries: body[60],
    owed: (body[61] << 8) | body[62],
    anchored: body[63] === 1,
  };
}

/** About how long the wait owed lasts: an asking is about half a second (16 are about 8 s on a J3R180). */
export function waitSeconds(owed: number): number {
  return Math.ceil(owed / 2);
}

/** Applet 4.0: the most askings the card serves before it writes down what is left (FedimintCardApplet.PIN_STRETCH). */
export const PIN_STRETCH = 16;
/** The longest wait a PIN can owe (0x4000 askings) with each stretch's closing asking, and one more. */
const MOST_KEEP_HOLDING = 0x4000 + 0x4000 / PIN_STRETCH + 1;

/**
 * Applet 4.0's VERIFY_PIN, sealed, with its wait served in this selection:
 * GET_NONCE and the PIN key checked under `trust`, one envelope sealed, and
 * the same bytes sent again, with nothing in between, while the card answers
 * `6A98` (a nonce asked mid-wait would make the old envelope a wrong try). A
 * PIN key that does not check sends nothing: `refused` says why. `owed` is GET_STATE's count, read in this selection before the first
 * VERIFY_PIN. The card counts the askings of a stretch only within one
 * selection, so a stretch cut by lifting the card starts again on the next
 * tap; only finished stretches stay served. `onWait` gets about how many
 * seconds are left, before the first asking and after each one. Returns the
 * first answer that is not `6A98`, or `6A98` when the card asks past its own
 * count. The library's `verify_pin_held` does the same.
 */
export async function verifyPinHeld(
  tx: Transceiver,
  trust: TrustedCard,
  pin: string,
  owed: number,
  onWait: (seconds: number) => void = () => undefined,
): Promise<{body: number[]; sw: number | null; refused?: string}> {
  const asked = await CheckedPinKey.ask(apdu => exchange(tx, apdu), trust);
  if (!asked.ok) {
    return {body: [], sw: asked.sw, refused: asked.reason};
  }
  const verify = sealedPinApdu(INS_VERIFY_PIN, asked.key, pinBlock(pin));
  return serveWait(tx, verify, owed, onWait);
}

async function serveWait(
  tx: Transceiver,
  verify: readonly number[],
  owed: number,
  onWait: (seconds: number) => void,
): Promise<{body: number[]; sw: number | null}> {
  let left = owed;
  let asked = 0;
  if (left > 0) {
    onWait(waitSeconds(left));
  }
  for (let i = 0; i <= MOST_KEEP_HOLDING; i++) {
    const reply = await exchange(tx, verify);
    if (reply.sw !== SW_KEEP_HOLDING) {
      return reply;
    }
    // The card's waitOwed, mirrored: askings 1..stretch, then the one that writes what is left.
    const stretch = Math.min(left, PIN_STRETCH);
    if (asked < stretch) {
      asked += 1;
    } else if (left > stretch) {
      asked = 0;
      left -= stretch;
    } else {
      // The card would have tried the PIN at this asking.
      return reply;
    }
    onWait(waitSeconds(left - asked));
  }
  return {body: [], sw: SW_KEEP_HOLDING};
}

/**
 * The PIN's state of the selected applet, spending no try: GET_STATE's PIN
 * byte on applet 4.0 (whose VERIFY_PIN may answer a wait before it looks at
 * the length), the three-byte VERIFY_PIN probe on an older applet, which the
 * reset of an old card still needs.
 */
export async function readPinStatus(
  tx: Transceiver,
  selected: {readonly body: readonly number[]; readonly sw: number | null},
): Promise<{pin: PinStatus | null; sw: number | null; state?: CardState}> {
  if (isCurrentApplet(selected)) {
    const reply = await exchange(tx, stateApdu());
    const state = reply.sw === SW_OK ? parseState(reply.body) : null;
    return state === null
      ? {pin: null, sw: reply.sw}
      : {pin: state.pinSet ? 'set' : 'notSet', sw: reply.sw, state};
  }
  const status = await exchange(tx, pinStatusApdu());
  return {pin: pinStatusOf(status.sw), sw: status.sw};
}

/**
 * Applets before 4.0 only. VERIFY_PIN with three bytes, shorter than any PIN. The applet answers a
 * blocked card `69 83` and an unset PIN `69 84` before it reads the data, and
 * refuses the length (`67 00`) before it spends a try
 * (FedimintCardApplet.verifyPin), so this tells the three apart for free.
 */
export function pinStatusApdu(): number[] {
  return [0xb0, INS_VERIFY_PIN, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00];
}

/** The PIN's state from `pinStatusApdu`'s answer; null for an answer that is none of the three. */
export function pinStatusOf(sw: number | null): PinStatus | null {
  return sw === SW_WRONG_LENGTH
    ? 'set'
    : sw === SW_BLOCKED
      ? 'blocked'
      : sw === SW_PIN_NOT_SET
        ? 'notSet'
        : null;
}

/** Where a failed setup stopped, for the batch row. */
export type SetupStep = 'select' | 'keys' | 'attest' | 'anchor' | 'setPin' | 'verifyPin';

export type SetupResult =
  | {
      readonly ok: true;
      /**
       * The x-only public keys, slot 0 first: the first
       * `INSTALL_TAP_KEY_SLOTS` slots (all of an 8-slot card's, 0-7 of a
       * 32-slot card's).
       */
      readonly keys: readonly (readonly number[])[];
      /** How many slots the card has, as it answered `probeSlotCount`. */
      readonly slotCount: number;
      /** Slots whose key this run generated; the rest were already there. */
      readonly generated: readonly number[];
    }
  | {
      readonly ok: false;
      readonly step: SetupStep;
      readonly sw: number | null;
      readonly reason: string;
      /**
       * Set when the registry, not the card, is why the step stopped (it refused,
       * was unreachable, or answered something unusable): `reason` is then the
       * registry's own words and the card is only waiting.
       */
      readonly registry?: true;
      /** Set when the card's applet is older than 4.0: `reason` is `OLD_APPLET`, and its reset is offered. */
      readonly oldApplet?: true;
    };

export function selectApdu(): number[] {
  return [0x00, 0xa4, 0x04, 0x00, CARD_AID.length, ...CARD_AID, 0x00];
}

export function keyApdu(ins: number, slot: number): number[] {
  return [CLA, ins, slot, 0x00, 0x00];
}

/** SET_LIMIT: the day's limit in msat as 8 bytes, big-endian. Arithmetic, not BigInt. */
export function limitApdu(msat: number): number[] {
  if (!Number.isSafeInteger(msat) || msat < 0) {
    throw new Error('limitApdu: the limit must be a non-negative integer of msat');
  }
  const bytes: number[] = new Array(8).fill(0);
  let rest = msat;
  for (let i = 7; i >= 0 && rest > 0; i--) {
    bytes[i] = rest % 256;
    rest = Math.floor(rest / 256);
  }
  return [CLA, INS_SET_LIMIT, 0x00, 0x00, 8, ...bytes];
}

/**
 * The PIN in an older applet's form (P1 00), for one use only: the holder's
 * consent in the reset of a card older than 4.0 (`reset.ts`). First PIN, resume
 * and every other PIN in this app are sealed, and the till and the POS phones
 * send no PIN to a card older than 4.0. A 4.x card refuses P1 = 00 (6B00) and is never sent it.
 */
export function oldCardResetPinApdu(ins: number, pin: string): number[] {
  return [CLA, ins, 0x00, 0x00, pin.length, ...Array.from(pin, c => c.charCodeAt(0))];
}

function split(reply: readonly number[]): {body: number[]; sw: number | null} {
  if (reply.length < 2) {
    return {body: [], sw: null};
  }
  const sw = (reply[reply.length - 2] << 8) | reply[reply.length - 1];
  return {body: reply.slice(0, -2), sw};
}

function hex(sw: number | null): string {
  return sw === null ? 'no status word' : sw.toString(16).toUpperCase().padStart(4, '0');
}

/**
 * Sends one APDU and follows `61 xx` (GET RESPONSE) and `6C xx` (resend with
 * the right Le), as the host's `exchange` does. A `6C` on a GEN_KEY is never
 * resent here either: the first attempt may already have made the key.
 */
export async function exchange(
  tx: Transceiver,
  apdu: readonly number[],
): Promise<{body: number[]; sw: number | null}> {
  let {body, sw} = split(await tx([...apdu]));
  const data = [...body];
  for (let round = 0; round < 8 && sw !== null; round++) {
    if (sw >> 8 === 0x61) {
      ({body, sw} = split(await tx([0x00, 0xc0, 0x00, 0x00, sw & 0xff])));
      data.push(...body);
    } else if (sw >> 8 === 0x6c && apdu[1] !== INS_GEN_KEY) {
      const again = [...apdu];
      again[again.length - 1] = sw & 0xff;
      ({body, sw} = split(await tx(again)));
      data.push(...body);
    } else {
      break;
    }
  }
  return {body: data, sw};
}

/**
 * What is on the card before anything is written: no applet (a blank card, or
 * one this batch's install may sweep), the applet with no keys (installed, but
 * a key setup that did not finish), or the applet with keys (a finished card,
 * which may hold value).
 *
 * The install's sweep deletes the applet instance, and its keys with it; a
 * keyed card's notes would then be unspendable. So a keyed card is never
 * installed over (the loop stops it as `guarded`), and an unkeyed one skips the
 * install and is only keyed.
 */
export type AppletProbe = 'none' | 'unkeyed' | 'keyed';

export async function probeApplet(tx: Transceiver): Promise<AppletProbe> {
  const selected = await exchange(tx, selectApdu());
  if (selected.sw !== SW_OK) {
    return 'none';
  }
  const slot0 = await exchange(tx, keyApdu(INS_GET_PUB, 0));
  if (slot0.sw === SW_CONDITIONS) {
    return 'unkeyed';
  }
  // Anything else from a selected applet (a key, or an answer this build does
  // not know) is treated as a card that may hold value: never swept.
  return 'keyed';
}

/**
 * `probeApplet`, and for a keyed card slot 0's key and the PIN's state, read
 * in the same selection without spending a try. `slot0` and `pin` are null
 * when the card answered something this build does not know; such a card is
 * treated as one that may hold value.
 */
export type CardProbe =
  /** `old`: an applet before 4.0, which this build never installs over, keys or sets up. */
  | {readonly kind: 'none' | 'unkeyed' | 'old'}
  | {
      readonly kind: 'keyed';
      readonly slot0: readonly number[] | null;
      readonly pin: PinStatus | null;
    };

export async function probeCard(tx: Transceiver): Promise<CardProbe> {
  const selected = await exchange(tx, selectApdu());
  if (selected.sw !== SW_OK) {
    return {kind: 'none'};
  }
  if (!isCurrentApplet(selected)) {
    return {kind: 'old'};
  }
  const slot0 = await exchange(tx, keyApdu(INS_GET_PUB, 0));
  if (slot0.sw === SW_CONDITIONS) {
    return {kind: 'unkeyed'};
  }
  if (slot0.sw !== SW_OK || slot0.body.length !== 32) {
    return {kind: 'keyed', slot0: null, pin: null};
  }
  const status = await readPinStatus(tx, selected);
  return {kind: 'keyed', slot0: slot0.body, pin: status.pin};
}

/**
 * How many slots the selected applet has. The applet answers `6B 00` to a slot
 * past its last before it looks at anything else (FedimintCardApplet.slot), so
 * GET_PUB on slot `n` answers `6B 00` exactly when the card has `n` slots or
 * fewer; an empty slot answers `69 85` and a keyed one its key. Asked for each
 * count in `SLOT_COUNTS`, smallest first: one APDU on an 8-slot card, two on a
 * 32-slot one, and nothing written. Any other answer, or a card with a slot
 * past the largest count, is not a card this build knows.
 */
export async function probeSlotCount(
  tx: Transceiver,
): Promise<{ok: true; count: number} | {ok: false; sw: number | null; reason: string}> {
  for (const count of SLOT_COUNTS) {
    const reply = await exchange(tx, keyApdu(INS_GET_PUB, count));
    if (reply.sw === SW_WRONG_P1P2) {
      return {ok: true, count};
    }
    if (reply.sw !== SW_OK && reply.sw !== SW_CONDITIONS) {
      return {ok: false, sw: reply.sw, reason: `slot count: slot ${count} answered ${hex(reply.sw)}`};
    }
  }
  const most = SLOT_COUNTS[SLOT_COUNTS.length - 1];
  return {ok: false, sw: SW_OK, reason: `slot count: the card has a slot ${most}, more than this app knows`};
}

export interface OfflineSetup {
  /** The offline limit for a day, in msat; 0 for none. */
  readonly limitMsat: number;
  /**
   * Asks the registry for the certificate over the card's attestation key
   * (`POST /registry/cards/:short_id/attestation`): the 65-byte point and
   * slot 0's x-only key in, the certificate out.
   */
  readonly certify: (
    attestPub: readonly number[],
    slot0: readonly number[],
  ) => Promise<readonly number[]>;
  /**
   * The registry's certificate key (`GET /registry/cert-key`): 65 bytes,
   * uncompressed, starting 04. Fetched before the tap, so no network call
   * is made while the card is on the phone for it. A card that answers
   * GET_STATE with 58 bytes (an older 3.0 build) has no SET_REGISTRY_KEY
   * and is never sent it.
   */
  readonly registryKey: readonly number[];
  /**
   * The latest Bitcoin block header from the registry (`GET /v1/chain/tip`),
   * 80 bytes, fetched before the tap; null when it could not be fetched.
   * Checked here again (`checkHeader`) before any APDU: without a header
   * that passes, nothing is sent to the card.
   */
  readonly header: readonly number[] | null;
  /** The phone's clock in ms, for the header's time window; `Date.now` when absent. */
  readonly now?: () => number;
}

/**
 * Keys, then (applet 3.0) the offline limit and certificate, then the PIN. Each step is reported through `onStep` before its first
 * APDU, so the loop can name where the card is.
 */
export async function finishCard(
  tx: Transceiver,
  pin: string,
  onStep: (step: SetupStep) => void = () => undefined,
  options: {
    /**
     * Generate every slot again (GEN_KEY P2 = 01), whatever they hold:
     * the resume of a card an earlier tap keyed and did not PIN. The applet
     * refuses it once a PIN is set, so it can never replace a key that could
     * hold value.
     */
    readonly rekey?: boolean;
    /** Slots to key from 0, capped at the card's count; `INSTALL_TAP_KEY_SLOTS` when absent. */
    readonly keySlots?: number;
    /**
     * The offline steps, between the keys and SET_PIN: SET_LIMIT,
     * ATTEST_INIT, the registry's certificate, SET_CERT, SET_REGISTRY_KEY,
     * then the anchor (SET_HEADER and GET_STATE). Applet 4.0 takes no PIN
     * without them, so without them (or without a header) nothing is sent.
     */
    readonly offline?: OfflineSetup;
  } = {},
): Promise<SetupResult> {
  if (!/^[0-9]{4,8}$/.test(pin)) {
    // Not a card's answer: the caller passed a PIN the applet would refuse.
    throw new Error('finishCard: the PIN must be 4 to 8 digits');
  }

  // The anchor is checked before the first APDU: with no header that passes
  // the phone's own checks, nothing at all goes to the card. (`installTap`
  // checks it before the tap touches the card; this is the second check.)
  const offline = options.offline;
  const checked = checkedAnchor(offline);
  if (!offline || !checked.ok) {
    return {
      ok: false,
      step: 'anchor',
      sw: null,
      reason: checked.ok ? NEEDS_INTERNET : checked.reason,
      registry: true,
    };
  }
  const header = checked.header;

  // The install leaves the card manager selected; the applet answers only
  // once it is selected itself, and a PIN session lasts one selection.
  onStep('select');
  const selected = await exchange(tx, selectApdu());
  if (selected.sw !== SW_OK) {
    return {
      ok: false,
      step: 'select',
      sw: selected.sw,
      reason: `SELECT of the applet answered ${hex(selected.sw)}`,
    };
  }
  // Before any PIN command, and before anything is written: applet 4.0 only.
  if (!isCurrentApplet(selected)) {
    return {ok: false, step: 'select', sw: selected.sw, reason: OLD_APPLET, oldApplet: true};
  }

  onStep('keys');
  const counted = await probeSlotCount(tx);
  if (!counted.ok) {
    return {ok: false, step: 'keys', sw: counted.sw, reason: counted.reason};
  }
  // A certificate already on the card is over slot 0's key and cannot be
  // replaced, so a card that has one keeps the keys it has: re-keying would
  // orphan the certificate.
  let rekey = options.rekey === true;
  if (rekey) {
    const state = await exchange(tx, stateApdu());
    if (state.sw === SW_OK && state.body.length >= 58 && state.body[56] !== 0) {
      rekey = false;
    }
  }
  const keySlots = Math.min(counted.count, options.keySlots ?? INSTALL_TAP_KEY_SLOTS);
  const keys: number[][] = [];
  const generated: number[] = [];
  for (let slot = 0; slot < keySlots; slot++) {
    let reply = rekey
      ? await exchange(tx, [CLA, INS_GEN_KEY, slot, 0x01, 0x00])
      : await exchange(tx, keyApdu(INS_GET_PUB, slot));
    if (rekey) {
      if (reply.sw === SW_OK) {
        generated.push(slot);
      }
    } else if (reply.sw === SW_CONDITIONS) {
      reply = await exchange(tx, keyApdu(INS_GEN_KEY, slot));
      if (reply.sw === SW_OK) {
        generated.push(slot);
      }
    }
    if (reply.sw !== SW_OK || reply.body.length !== 32) {
      return {
        ok: false,
        step: 'keys',
        sw: reply.sw,
        reason: `slot ${slot}: ${
          reply.sw === SW_OK
            ? `a ${reply.body.length}-byte key, not 32`
            : `answered ${hex(reply.sw)}`
        }`,
      };
    }
    keys.push(reply.body);
  }

  onStep('attest');
  const failed = await offlineSteps(tx, offline, keys[0]);
  if (failed) {
    return failed;
  }

  // The anchor, in the selection that sends SET_PIN: the card takes a PIN only
  // after this selection anchored its clock (6A96 otherwise).
  onStep('anchor');
  const unanchored = await anchorStep(tx, header, checked.time);
  if (unanchored) {
    return unanchored;
  }

  // SET_PIN's selection counts as having the PIN, so no VERIFY_PIN follows.
  // It is sealed to the PIN key, checked first: the certificate just written,
  // under the registry's key this tap gave the card, over this card's slot 0;
  // then GET_NONCE's binding under that certificate's attestation key.
  onStep('setPin');
  const send = (apdu: readonly number[]) => exchange(tx, apdu);
  const trusted = await TrustedCard.check(send, offline.registryKey);
  if (!trusted.ok) {
    return {ok: false, step: 'setPin', sw: trusted.sw, reason: trusted.reason};
  }
  const asked = await CheckedPinKey.ask(send, trusted.trust);
  if (!asked.ok) {
    return {ok: false, step: 'setPin', sw: asked.sw, reason: asked.reason};
  }
  const set = await exchange(tx, sealedPinApdu(INS_SET_PIN, asked.key, pinBlock(pin)));
  if (set.sw !== SW_OK) {
    return {
      ok: false,
      step: 'setPin',
      sw: set.sw,
      reason:
        set.sw === SW_CONDITIONS
          ? 'the card already has a PIN'
          : set.sw === SW_NOT_ANCHORED
            ? 'the card did not keep the block header as its anchor'
            : set.sw === SW_WRONG_LENGTH
              ? 'the card did not open the sealed PIN; ask for a new nonce and set it again'
              : `SET_PIN answered ${hex(set.sw)}`,
    };
  }

  return {ok: true, keys, generated, slotCount: counted.count};
}

const anchorFailed = (sw: number | null, reason: string): SetupResult => ({
  ok: false,
  step: 'anchor',
  sw,
  reason,
});

/**
 * SET_HEADER with the checked header, which on a card with no PIN and no value
 * anchors the clock to its time; then GET_STATE must show that time and this
 * selection anchored. Returns the failure, or null when SET_PIN may follow.
 */
async function anchorStep(
  tx: Transceiver,
  header: readonly number[],
  time: number,
): Promise<SetupResult | null> {
  const sent = await exchange(tx, [CLA, INS_SET_HEADER, 0x00, 0x00, HEADER_LEN, ...header]);
  if (sent.sw !== SW_OK) {
    return anchorFailed(
      sent.sw,
      sent.sw === SW_LITTLE_WORK
        ? 'the card refused the block header for its work'
        : `SET_HEADER answered ${hex(sent.sw)}`,
    );
  }
  const read = await exchange(tx, stateApdu());
  const state = read.sw === SW_OK ? parseState(read.body) : null;
  if (state === null) {
    return anchorFailed(read.sw, `GET_STATE answered ${hex(read.sw)} with ${read.body.length} bytes, not ${STATE_LEN}`);
  }
  if (state.pinSet) {
    return anchorFailed(read.sw, 'the card already has a PIN');
  }
  if (!state.anchored || state.now !== time) {
    return anchorFailed(read.sw, 'the card did not take the block header as its anchor');
  }
  return null;
}

const attestFailed = (sw: number | null, reason: string, registry?: true): SetupResult => ({
  ok: false,
  step: 'attest',
  sw,
  reason,
  ...(registry ? {registry} : {}),
});

/**
 * SET_LIMIT, then the certificate, then the registry's key. Every step is safe to run again on a card
 * a lifted tap left part-way, so the next tap's resume sends them all again:
 * SET_LIMIT may be repeated before the PIN; ATTEST_INIT may be repeated until
 * SET_CERT, and is skipped once the card holds a certificate (GET_STATE byte
 * 56), which is then only checked to be over this card's slot 0 key.
 * SET_REGISTRY_KEY may be repeated before the PIN; GET_STATE byte 58 says it
 * is done, and a 58-byte GET_STATE (an older 3.0 build) skips it.
 * Returns the failure, or null when the card is ready for its PIN.
 */
async function offlineSteps(
  tx: Transceiver,
  offline: OfflineSetup,
  slot0: readonly number[],
): Promise<SetupResult | null> {
  const limit = await exchange(tx, limitApdu(offline.limitMsat));
  if (limit.sw !== SW_OK) {
    return attestFailed(limit.sw, `SET_LIMIT answered ${hex(limit.sw)}`);
  }
  const state = await exchange(tx, stateApdu());
  if (state.sw !== SW_OK || parseState(state.body) === null) {
    return attestFailed(state.sw, `GET_STATE answered ${hex(state.sw)} with ${state.body.length} bytes, not ${STATE_LEN}`);
  }
  const certified = await certificateStep(tx, offline, slot0, state.body[56] !== 0);
  if (certified) {
    return certified;
  }
  // Byte 58 is 1 once the key is set; the step is then done and not repeated.
  if (state.body[58] !== 1) {
    const key = offline.registryKey;
    if (key.length !== 65 || key[0] !== 0x04) {
      return attestFailed(null, `the registry's certificate key is ${key.length} bytes, not 65 starting 04`, true);
    }
    const set = await exchange(tx, [CLA, INS_SET_REGISTRY_KEY, 0x00, 0x00, 65, ...key]);
    if (set.sw !== SW_OK) {
      return attestFailed(set.sw, `SET_REGISTRY_KEY answered ${hex(set.sw)}`);
    }
  }
  return null;
}

/** The certificate: checked when the card holds one, otherwise ATTEST_INIT, the registry, SET_CERT. */
async function certificateStep(
  tx: Transceiver,
  offline: OfflineSetup,
  slot0: readonly number[],
  hasCert: boolean,
): Promise<SetupResult | null> {
  if (hasCert) {
    const cert = await exchange(tx, [CLA, INS_GET_CERT, 0x00, 0x00, 0x00]);
    const same =
      cert.sw === SW_OK &&
      cert.body.length >= 33 &&
      cert.body.slice(1, 33).every((b, i) => b === slot0[i]);
    return same
      ? null
      : attestFailed(
          cert.sw,
          cert.sw === SW_OK
            ? 'the card holds a certificate for another key; Settings → Reset a card takes it back to blank'
            : `GET_CERT answered ${hex(cert.sw)}`,
        );
  }
  const init = await exchange(tx, [CLA, INS_ATTEST_INIT, 0x00, 0x00, 0x00]);
  if (init.sw !== SW_OK || init.body.length !== 65 || init.body[0] !== 0x04) {
    return attestFailed(
      init.sw,
      init.sw === SW_OK
        ? `ATTEST_INIT answered a ${init.body.length}-byte key, not 65`
        : `ATTEST_INIT answered ${hex(init.sw)}`,
    );
  }
  let cert: readonly number[];
  try {
    cert = await offline.certify(init.body, slot0);
  } catch (e) {
    return attestFailed(null, e instanceof Error ? e.message : String(e), true);
  }
  if (cert.length < 1 || cert.length > CERT_MAX) {
    return attestFailed(null, `the registry's certificate is ${cert.length} bytes, not 1 to ${CERT_MAX}`, true);
  }
  const set = await exchange(tx, [CLA, INS_SET_CERT, 0x00, 0x00, cert.length, ...cert]);
  if (set.sw !== SW_OK) {
    return attestFailed(set.sw, `SET_CERT answered ${hex(set.sw)}`);
  }
  return null;
}

/**
 * A card whose PIN a lifted tap set and whose answer was lost: one VERIFY_PIN
 * with the PIN that tap sent (any wait the card owes served first, in this
 * selection, with `onWait` told the seconds left), and, only if the card takes
 * it, the keys read back. SET_PIN is sent only after every keyed slot answered
 * a key in the same selection, so a card with a PIN has its whole key set;
 * nothing is generated here. A wrong PIN spends that one try and is never retried.
 * The PIN is sealed, so the card's certificate is checked under `certKey`
 * first; without the key, no PIN is sent.
 */
export async function finishVerified(
  tx: Transceiver,
  pin: string,
  certKey: readonly number[] | null,
  onStep: (step: SetupStep) => void = () => undefined,
  onWait?: (seconds: number) => void,
): Promise<SetupResult> {
  onStep('select');
  const selected = await exchange(tx, selectApdu());
  if (selected.sw !== SW_OK) {
    return {
      ok: false,
      step: 'select',
      sw: selected.sw,
      reason: `SELECT of the applet answered ${hex(selected.sw)}`,
    };
  }
  if (!isCurrentApplet(selected)) {
    return {ok: false, step: 'select', sw: selected.sw, reason: OLD_APPLET, oldApplet: true};
  }
  onStep('verifyPin');
  const read = await exchange(tx, stateApdu());
  const state = read.sw === SW_OK ? parseState(read.body) : null;
  if (state === null) {
    return {
      ok: false,
      step: 'verifyPin',
      sw: read.sw,
      reason: `GET_STATE answered ${hex(read.sw)} with ${read.body.length} bytes, not ${STATE_LEN}`,
    };
  }
  const trusted = await TrustedCard.check(apdu => exchange(tx, apdu), certKey);
  if (!trusted.ok) {
    return {ok: false, step: 'verifyPin', sw: trusted.sw, reason: trusted.reason};
  }
  const verified = await verifyPinHeld(tx, trusted.trust, pin, state.owed, onWait);
  if (verified.sw !== SW_OK) {
    return {
      ok: false,
      step: 'verifyPin',
      sw: verified.sw,
      reason: verified.refused ??
        (verified.sw !== null && verified.sw >> 4 === 0x63c
          ? `the PIN this phone sent was not the card's; ${verified.sw & 0x0f} wrong tries in a row`
          : verified.sw === SW_KEEP_HOLDING
            ? 'the card asked for a longer wait than it owes'
            : `VERIFY_PIN answered ${hex(verified.sw)}`),
    };
  }
  onStep('keys');
  const counted = await probeSlotCount(tx);
  if (!counted.ok) {
    return {ok: false, step: 'keys', sw: counted.sw, reason: counted.reason};
  }
  const keys: number[][] = [];
  for (let slot = 0; slot < Math.min(counted.count, INSTALL_TAP_KEY_SLOTS); slot++) {
    const reply = await exchange(tx, keyApdu(INS_GET_PUB, slot));
    if (reply.sw !== SW_OK || reply.body.length !== 32) {
      return {
        ok: false,
        step: 'keys',
        sw: reply.sw,
        reason: `slot ${slot}: ${
          reply.sw === SW_OK
            ? `a ${reply.body.length}-byte key, not 32`
            : `answered ${hex(reply.sw)}`
        }`,
      };
    }
    keys.push(reply.body);
  }
  return {ok: true, keys, generated: [], slotCount: counted.count};
}
