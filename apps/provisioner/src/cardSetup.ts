/**
 * Finishing a card on the phone, in the tap that installed it (docs/OPS-PLAN.md
 * § 4). The install leaves an applet with eight empty slots and no PIN; this
 * asks the card to generate its keys, reads them back, sets the PIN and
 * proves it took. It is the sidecar's `key_setup` (`host/sidecar/src/cards.rs`)
 * and `card_setup` (`glue.rs`) moved here, over the same NFC session, so a
 * card is finished with no laptop and no relay, on iOS as on Android.
 *
 * The APDUs are the host's (`host/card/src/lib.rs`): CLA B0, `GEN_KEY` 10 and
 * `GET_PUB` 11 with P1 = slot and Le 00, `SET_PIN` 41 and `VERIFY_PIN` 40 with
 * the PIN's ASCII digits as data. Existing keys are kept, as the sidecar keeps
 * them, except on a resume (`rekey`): a card an earlier tap left keyed with no
 * PIN gets all eight slots generated again with P2 = 01, which the applet
 * allows only while no PIN is set (FedimintCardApplet.genKey). Nothing here
 * sends P2 = 01 to a card with a PIN.
 *
 * The PIN is a parameter and nothing else: it goes to the card in two APDUs and
 * is not kept, logged or returned.
 */
import {CARD_AID, type Transceiver} from './cardkit';

export const SLOT_COUNT = 8;

const CLA = 0xb0;
export const INS_GEN_KEY = 0x10;
export const INS_GET_PUB = 0x11;
export const INS_VERIFY_PIN = 0x40;
const INS_SET_PIN = 0x41;

export const SW_OK = 0x9000;
/** GET_PUB: the slot is empty. SET_PIN: a PIN is already set. */
export const SW_CONDITIONS = 0x6985;
/** VERIFY_PIN or SET_PIN on a blocked card. */
export const SW_BLOCKED = 0x6983;
/** VERIFY_PIN: no PIN was ever set on this applet. */
export const SW_PIN_NOT_SET = 0x6984;
/** VERIFY_PIN refused for its length, before any try is spent: a PIN is set and not blocked. */
export const SW_WRONG_LENGTH = 0x6700;

/** What the applet's PIN is, read without spending a try. */
export type PinStatus = 'set' | 'blocked' | 'notSet';

/**
 * VERIFY_PIN with three bytes, shorter than any PIN. The applet answers a
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
export type SetupStep = 'select' | 'keys' | 'setPin' | 'verifyPin';

export type SetupResult =
  | {
      readonly ok: true;
      /** The eight x-only public keys, slot 0 first. */
      readonly keys: readonly (readonly number[])[];
      /** Slots whose key this run generated; the rest were already there. */
      readonly generated: readonly number[];
    }
  | {
      readonly ok: false;
      readonly step: SetupStep;
      readonly sw: number | null;
      readonly reason: string;
    };

export function selectApdu(): number[] {
  return [0x00, 0xa4, 0x04, 0x00, CARD_AID.length, ...CARD_AID, 0x00];
}

export function keyApdu(ins: number, slot: number): number[] {
  return [CLA, ins, slot, 0x00, 0x00];
}

export function pinApdu(ins: number, pin: string): number[] {
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
  | {readonly kind: 'none' | 'unkeyed'}
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
  const slot0 = await exchange(tx, keyApdu(INS_GET_PUB, 0));
  if (slot0.sw === SW_CONDITIONS) {
    return {kind: 'unkeyed'};
  }
  if (slot0.sw !== SW_OK || slot0.body.length !== 32) {
    return {kind: 'keyed', slot0: null, pin: null};
  }
  const status = await exchange(tx, pinStatusApdu());
  return {kind: 'keyed', slot0: slot0.body, pin: pinStatusOf(status.sw)};
}

/**
 * Keys, then the PIN. Each step is reported through `onStep` before its first
 * APDU, so the loop can name where the card is.
 */
export async function finishCard(
  tx: Transceiver,
  pin: string,
  onStep: (step: SetupStep) => void = () => undefined,
  options: {
    /**
     * Generate all eight slots again (GEN_KEY P2 = 01), whatever they hold:
     * the resume of a card an earlier tap keyed and did not PIN. The applet
     * refuses it once a PIN is set, so it can never replace a key that could
     * hold value.
     */
    readonly rekey?: boolean;
  } = {},
): Promise<SetupResult> {
  if (!/^[0-9]{4,8}$/.test(pin)) {
    // Not a card's answer: the caller passed a PIN the applet would refuse.
    throw new Error('finishCard: the PIN must be 4 to 8 digits');
  }

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

  onStep('keys');
  const keys: number[][] = [];
  const generated: number[] = [];
  for (let slot = 0; slot < SLOT_COUNT; slot++) {
    let reply = options.rekey
      ? await exchange(tx, [CLA, INS_GEN_KEY, slot, 0x01, 0x00])
      : await exchange(tx, keyApdu(INS_GET_PUB, slot));
    if (options.rekey) {
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

  onStep('setPin');
  const set = await exchange(tx, pinApdu(INS_SET_PIN, pin));
  if (set.sw !== SW_OK) {
    return {
      ok: false,
      step: 'setPin',
      sw: set.sw,
      reason:
        set.sw === SW_CONDITIONS
          ? 'the card already has a PIN'
          : set.sw === SW_BLOCKED
            ? 'the card is blocked'
            : `SET_PIN answered ${hex(set.sw)}`,
    };
  }

  onStep('verifyPin');
  const verified = await exchange(tx, pinApdu(INS_VERIFY_PIN, pin));
  if (verified.sw !== SW_OK) {
    return {
      ok: false,
      step: 'verifyPin',
      sw: verified.sw,
      reason: `VERIFY_PIN answered ${hex(verified.sw)}`,
    };
  }

  return {ok: true, keys, generated};
}

/**
 * A card whose PIN a lifted tap set and whose answer was lost: one VERIFY_PIN
 * with the PIN that tap sent, and, only if the card takes it, the eight keys
 * read back. SET_PIN is sent only after all eight slots answered a key in the
 * same selection, so a card with a PIN has its whole key set; nothing is
 * generated here. A wrong PIN spends that one try and is never retried.
 */
export async function finishVerified(
  tx: Transceiver,
  pin: string,
  onStep: (step: SetupStep) => void = () => undefined,
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
  onStep('verifyPin');
  const verified = await exchange(tx, pinApdu(INS_VERIFY_PIN, pin));
  if (verified.sw !== SW_OK) {
    return {
      ok: false,
      step: 'verifyPin',
      sw: verified.sw,
      reason:
        verified.sw !== null && verified.sw >> 4 === 0x63c
          ? `the PIN this phone sent was not the card's; ${verified.sw & 0x0f} tries left`
          : `VERIFY_PIN answered ${hex(verified.sw)}`,
    };
  }
  onStep('keys');
  const keys: number[][] = [];
  for (let slot = 0; slot < SLOT_COUNT; slot++) {
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
  return {ok: true, keys, generated: []};
}
