/**
 * One card's install tap, from the probe to the PIN (docs/OPS-PLAN.md § 4 and
 * O5), over the NFC session the loop opened. It is the loop's card path, kept
 * apart from the screen so the order can be tested against a card that keeps
 * its keys across taps.
 *
 *   probe → CPLC → keys from ops → install (factory or own keys) → rotate →
 *   GEN_KEY ×8, SET_PIN, VERIFY_PIN
 *
 * **Card-manager keys.** Ops answers the card's own ENC/MAC/DEK set, escrowed
 * against the chip's CPLC before the answer leaves ops. The install logs in
 * with whichever of the factory set and the card's own set the card proves,
 * from one INITIALIZE UPDATE checked offline against both; after it, the
 * rotation moves a factory card to its own set with PUT KEY and logs in again
 * to prove it took. Neither step ever sends EXTERNAL AUTHENTICATE for a set
 * the card has not proved, so a card on neither stops `wrongKeys` with no
 * failed login counted (a card manager locks for good after too many).
 *
 * **Crash safety.** Nothing is remembered between taps; every tap reads the
 * card again. A tap that broke before PUT KEY leaves the factory keys, which
 * the next tap's install or rotation still proves. One that broke after it
 * leaves the card on its own keys, which ops answers again byte for byte, so
 * the next tap proves those and goes on to the keys and the PIN. A card that
 * already holds slot keys is never installed over (`probeApplet`).
 *
 * The key set lives in this call only. It is zeroed when the tap ends and is
 * never logged, stored or returned.
 */
import {
  CardError,
  FACTORY_KEYS,
  FEDIMINT_PLAN,
  TransportError,
  fromHex,
  provision,
  readChipIdentity,
  rotateKeys,
  type KeySet,
  type Random,
  type RunEvent,
  type StopKind,
  type Transceiver,
} from './cardkit';
import {
  finishCard,
  probeApplet,
  type SetupResult,
  type SetupStep,
} from './cardSetup';
import type {ManagerKeysReply} from './ops';

/** Where a tap stops before its keys and PIN: cardkit's stops, and ops' missing answer. */
export type InstallStop = StopKind | 'noManagerKeys';

export type InstallOutcome =
  | {readonly ok: true}
  | {
      readonly ok: false;
      readonly kind: InstallStop;
      /** Where it stopped, in cardkit's phase names, `rotate` or `managerKeys`. */
      readonly phase: string;
      readonly sw?: number;
      readonly reason?: string;
    };

export interface TapResult {
  readonly installed: InstallOutcome;
  /** The key and PIN setup; null when the tap stopped before it. */
  readonly setup: SetupResult | null;
  /** The chip's CPLC id, once read. */
  readonly chip?: string;
  /** Whether this tap moved the card off its factory keys. */
  readonly rotated?: boolean;
}

export interface TapOptions {
  /** The PIN to set; passed to `finishCard` and nowhere else. */
  readonly pin: string;
  /** Ops' answer for the chip: the card's own key set. */
  readonly managerKeys: (chip: string) => Promise<ManagerKeysReply>;
  readonly random: Random;
  readonly now: () => number;
  /** cardkit's install events, as `provision` emits them. */
  readonly onEvent?: (event: RunEvent) => void;
  /** This tap's own steps: `managerKeys` before the install, `rotate` after it. */
  readonly onPhase?: (phase: 'managerKeys' | 'rotate') => void;
  readonly onSetupStep?: (step: SetupStep) => void;
}

/** The reply's hex as a key set; throws for anything but three 16-byte keys. */
export function keySetOf(reply: ManagerKeysReply): KeySet {
  const keys = {
    enc: fromHex(reply.enc),
    mac: fromHex(reply.mac),
    dek: fromHex(reply.dek),
  };
  if (![keys.enc, keys.mac, keys.dek].every(k => k.length === 16)) {
    throw new Error('ops answered a key that is not 16 bytes');
  }
  return keys;
}

/** Overwrites a key set this tap held, so it does not outlive the tap in memory. */
export function forget(keys: KeySet | null): void {
  if (!keys) {
    return;
  }
  for (const key of [keys.enc, keys.mac, keys.dek]) {
    if (Array.isArray(key)) {
      key.fill(0);
    }
  }
}

/** What a stop says when the card proved neither set. */
export const NEITHER_SET =
  'the card proved neither the factory keys nor its own keys from ops; nothing was sent after INITIALIZE UPDATE';

function stopped(
  phase: string,
  r: {kind?: StopKind; sw?: number; reason?: string},
): InstallOutcome {
  const kind = r.kind ?? 'refused';
  return {
    ok: false,
    kind,
    phase,
    sw: r.sw,
    reason: kind === 'wrongKeys' ? NEITHER_SET : r.reason,
  };
}

/**
 * Reads the chip and asks ops for its keys. Nothing is written on the card;
 * a failure is a stop with the card as it was.
 */
export async function chipAndKeys(
  tx: Transceiver,
  managerKeys: (chip: string) => Promise<ManagerKeysReply>,
): Promise<
  | {ok: true; chip: string; keys: KeySet}
  | {ok: false; chip?: string; stop: InstallOutcome}
> {
  let chip: string;
  try {
    chip = (await readChipIdentity(tx)).id;
  } catch (e) {
    if (e instanceof TransportError) {
      return {
        ok: false,
        stop: {
          ok: false,
          kind: 'pulled',
          phase: 'managerKeys',
          reason: e.message,
        },
      };
    }
    if (e instanceof CardError) {
      return {
        ok: false,
        stop: {
          ok: false,
          kind: 'refused',
          phase: 'managerKeys',
          sw: e.sw,
          reason: e.message,
        },
      };
    }
    throw e;
  }
  try {
    const reply = await managerKeys(chip);
    if (reply.chip !== chip) {
      throw new Error('ops answered keys for another chip');
    }
    return {ok: true, chip, keys: keySetOf(reply)};
  } catch (e) {
    return {
      ok: false,
      chip,
      stop: {
        ok: false,
        kind: 'noManagerKeys',
        phase: 'managerKeys',
        reason: e instanceof Error ? e.message : String(e),
      },
    };
  }
}

/**
 * Installs the applet if the card has none, rotates the card manager to the
 * card's own keys if it is still on the factory set, then keys and PINs the
 * card. `before` is what `probeApplet` found; a keyed card never gets here.
 */
export async function installRotateFinish(
  tx: Transceiver,
  before: 'none' | 'unkeyed',
  own: KeySet,
  options: Omit<TapOptions, 'managerKeys'>,
): Promise<Omit<TapResult, 'chip'>> {
  // Which set the card proved at the install: 0 its own, 1 the factory's.
  let proved: number | undefined;
  if (before === 'none') {
    const installed = await provision(tx, FEDIMINT_PLAN, {
      keys: [own, FACTORY_KEYS],
      random: options.random,
      now: options.now,
      onEvent: options.onEvent,
    });
    if (!installed.ok) {
      return {installed: stopped(installed.phase, installed), setup: null};
    }
    proved = installed.keySet;
  }
  let rotated = false;
  // A card the install found on its own keys needs no second login to say so.
  if (proved !== 0) {
    options.onPhase?.('rotate');
    const rotation = await rotateKeys(tx, FACTORY_KEYS, own, {
      random: options.random,
      now: options.now,
    });
    if (!rotation.ok) {
      return {installed: stopped('rotate', rotation), setup: null};
    }
    rotated = rotation.rotated === true;
  }
  const setup = await finishCard(tx, options.pin, options.onSetupStep);
  return {installed: {ok: true}, setup, rotated};
}

/** The loop's whole tap. Throws only for what is no card's outcome (a bug, or the session's own cancel). */
export async function installTap(
  tx: Transceiver,
  options: TapOptions,
): Promise<TapResult> {
  // Before anything is written: a card that already has keys is never
  // installed over, since the sweep would delete them with any value they
  // hold; one with the applet but no keys skips the install.
  const before = await probeApplet(tx);
  if (before === 'keyed') {
    return {
      installed: {
        ok: false,
        kind: 'guarded',
        phase: 'probe',
        reason: 'the card already has keys; this app never installs over them',
      },
      setup: null,
    };
  }
  options.onPhase?.('managerKeys');
  const fetched = await chipAndKeys(tx, options.managerKeys);
  if (!fetched.ok) {
    return {installed: fetched.stop, setup: null, chip: fetched.chip};
  }
  try {
    const rest = await installRotateFinish(tx, before, fetched.keys, options);
    return {...rest, chip: fetched.chip};
  } finally {
    forget(fetched.keys);
  }
}
