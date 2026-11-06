/**
 * One card's install tap, from the probe to the PIN (docs/OPS-PLAN.md § 4 and
 * O5), over the NFC session the loop opened. It is the loop's card path, kept
 * apart from the screen so the order can be tested against a card that keeps
 * its keys across taps.
 *
 *   probe → CPLC → keys from ops → install (factory or own keys) → rotate →
 *   GET_PUB past the last slot (the card's count, 8 or 32) → GEN_KEY on slots
 *   0-7 (every slot of an 8-slot card; a till keys a 32-slot card's 8-31
 *   later, split keying) → SET_PIN, VERIFY_PIN
 *
 * A card the probe finds keyed is never installed over. If it is ours with
 * no PIN (a tap lifted during GEN_KEY, SET_PIN or VERIFY_PIN), it is resumed:
 *
 *   probe (slot 0, PIN state) → CPLC → keys from ops → ops' resume check →
 *   one INITIALIZE UPDATE proving the card's own keys → GEN_KEY P2=01 on 0-7,
 *   SET_PIN, VERIFY_PIN
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
 * **Crash safety.** Every tap reads the card again; the one thing the phone
 * remembers between taps is, in memory only, the PIN it sent to a chip whose
 * tap was lifted after SET_PIN went out (`attemptedPin`). A tap that broke
 * before PUT KEY leaves the factory keys, which the next tap's install or
 * rotation still proves. One that broke after it, before the first GEN_KEY,
 * leaves the card on its own keys with empty slots, which ops answers again
 * byte for byte, so the next tap proves those and goes on to the keys and the
 * PIN. One that broke during GEN_KEY, SET_PIN or VERIFY_PIN leaves some or
 * all slots keyed:
 *
 * - **No PIN on the card** (read without spending a try): the card cannot
 *   have been spent from or loaded onto, since ops takes no load for a card
 *   it has no PIN-set row for. The next tap asks ops that it holds nothing
 *   under the short id the card answers and records no other open card on the
 *   chip, proves the card's own keys from one INITIALIZE UPDATE (no EXTERNAL
 *   AUTHENTICATE unless proved), then generates every slot again (the
 *   applet allows P2=01 only while no PIN is set), sets a fresh PIN and
 *   verifies it. The card gets a new short id; the row it is posted with
 *   names the old one (`resumedFrom`), and ops closes that in the same write.
 *   A tap lifted during the resume leaves the same state, so the next one
 *   does the same again.
 * - **A PIN on the card** (SET_PIN took, its answer was lost): SET_PIN goes
 *   out only after every slot answered a key, so the key set is whole.
 *   With the PIN this phone sent to this chip still in memory, the next tap
 *   makes the same checks, sends one VERIFY_PIN and, only if it is taken,
 *   reads the keys back: no key is generated, the short id is the one
 *   the card answers. Without that PIN, or if the card refuses it, the tap
 *   stops `needsReset` (no try spent without it, one at most with it): the
 *   card goes through Settings → Reset a card, which asks no PIN for a card
 *   that never reached a holder (ops finds it by its chip, `reset.ts`).
 *
 * A blocked PIN, a card whose answers this build does not know, and every
 * card ops shows anything for are never written to (`guarded`).
 *
 * The key set lives in this call only. It is zeroed when the tap ends and is
 * never logged, stored or returned.
 */
import {
  CardError,
  FACTORY_KEYS,
  confirmManagerKeys,
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
  finishVerified,
  probeCard,
  type CardProbe,
  type SetupResult,
  type SetupStep,
} from './cardSetup';
import type {ManagerKeysReply, ResumeCheck} from './ops';
import {shortIdOf, type CardPhase} from './store';

/**
 * Where a tap stops before its keys and PIN: cardkit's stops, ops' missing
 * answer, and a card with a PIN this phone cannot prove (`needsReset`).
 */
export type InstallStop = StopKind | 'noManagerKeys' | 'needsReset';

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
  /**
   * A resumed card: the short id it answered before this tap generated its
   * slots again. The finished row names it, so ops closes it.
   */
  readonly resumedFrom?: string;
  /**
   * The stop leaves the card for its own next tap to finish (nothing written,
   * or a resume that can run again): the loop keeps the batch's slot for it.
   */
  readonly resumable?: boolean;
  /**
   * The card was finished with the PIN an earlier, lifted tap sent it
   * (`attemptedPin`), not this tap's: that is the one to hand over.
   */
  readonly finishedWithAttemptedPin?: boolean;
}

/** How a tap the loop saw ended: a result, or the card lost mid-tap. */
export type SeenTap =
  | {readonly lost: false; readonly result: TapResult}
  | {
      readonly lost: true;
      /** The loop's phase when the card went: `keySetup` and `setPin` are the key and PIN writes. */
      readonly phase: CardPhase;
      /** The tap was resuming a keyed card (`onResume` fired). */
      readonly resuming: boolean;
    };

/**
 * Whether a tap that did not finish its card keeps the batch's place for the
 * same card's next tap. True for a card lost while its keys or PIN were being
 * written or while it was being resumed, for a key setup the card refused
 * before any PIN went out, and for a resume stop that wrote nothing; the next
 * tap of that card resumes it. False for everything the next tap cannot
 * finish (a stop the loop records, and the batch counts as tried).
 */
export function holdsPlace(seen: SeenTap): boolean {
  if (seen.lost) {
    return seen.resuming || seen.phase === 'keySetup' || seen.phase === 'setPin';
  }
  const {installed, setup} = seen.result;
  if (!installed.ok) {
    return seen.result.resumable === true;
  }
  return setup !== null && !setup.ok && (setup.step === 'select' || setup.step === 'keys');
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
  /** The chip, as soon as it is read: the loop keys the PIN it sends by it. */
  readonly onChip?: (chip: string) => void;
  /**
   * Ops' answer for a keyed card with no PIN; absent (a build with no
   * registry), such a card is guarded as before.
   */
  readonly resumeCheck?: (chip: string, shortId: string) => Promise<ResumeCheck>;
  /**
   * The PIN this phone sent to `chip` in a tap that lost the card after
   * SET_PIN went out, held in memory by the loop; null when it holds none.
   */
  readonly attemptedPin?: (chip: string) => string | null;
  /** The card is keyed and is being resumed: a loss from here keeps the batch's place. */
  readonly onResume?: (oldShortId: string) => void;
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

/** Where reading the chip and asking ops for its keys can stop; nothing is written by then. */
export type ChipStop = Extract<InstallOutcome, {ok: false}> & {
  readonly kind: 'pulled' | 'refused' | 'noManagerKeys';
};

/**
 * Reads the chip and asks ops for its keys. Nothing is written on the card;
 * a failure is a stop with the card as it was.
 */
export async function chipAndKeys(
  tx: Transceiver,
  managerKeys: (chip: string) => Promise<ManagerKeysReply>,
): Promise<
  | {ok: true; chip: string; keys: KeySet}
  | {ok: false; chip?: string; stop: ChipStop}
> {
  const read = await readChip(tx);
  if (!read.ok) {
    return read;
  }
  return keysFor(read.chip, managerKeys);
}

/** The chip's CPLC id; nothing is written. */
async function readChip(
  tx: Transceiver,
): Promise<{ok: true; chip: string} | {ok: false; stop: ChipStop}> {
  try {
    return {ok: true, chip: (await readChipIdentity(tx)).id};
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
}

/** Ops' key set for the chip, checked; a failure is a stop with nothing written. */
async function keysFor(
  chip: string,
  managerKeys: (chip: string) => Promise<ManagerKeysReply>,
): Promise<
  | {ok: true; chip: string; keys: KeySet}
  | {ok: false; chip: string; stop: ChipStop}
> {
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

/** What a stop says when a resumed card did not prove its own keys. */
export const NOT_OWN =
  'the card did not prove the keys ops holds for it; nothing was sent after INITIALIZE UPDATE';

/** What a stop says for a keyed card this app does not resume. */
export const KEYED = 'the card already has keys; this app never installs over them';

/**
 * A card the probe found keyed. Resumed only when it is ours, on its own keys,
 * with no PIN (or a PIN this phone sent it and the card takes), and ops holds
 * nothing for it; every other keyed card is `guarded`, with nothing written.
 *
 * The order keeps a finished card untouched: the chip is read (no write), and
 * ops is asked about the short id before its manager keys are, so a card ops
 * knows as finished never gets an escrow request from this tap.
 */
async function resumeTap(
  tx: Transceiver,
  probe: Extract<CardProbe, {kind: 'keyed'}>,
  options: TapOptions,
): Promise<TapResult> {
  const guard = (reason: string, chip?: string): TapResult => ({
    installed: {ok: false, kind: 'guarded', phase: 'probe', reason},
    setup: null,
    chip,
  });
  if (probe.slot0 === null || probe.pin === null) {
    return guard(KEYED);
  }
  if (probe.pin === 'blocked') {
    return guard('the card’s PIN is blocked; this app never installs over it');
  }
  const resumeCheck = options.resumeCheck;
  if (!resumeCheck) {
    return guard(KEYED);
  }
  const oldId = shortIdOf(probe.slot0);
  const pinSet = probe.pin === 'set';
  // A card with no PIN is ours to finish; one with a PIN is only when ops has
  // no row for it, which the check below decides. A loss from here holds.
  if (!pinSet) {
    options.onResume?.(oldId);
  }

  options.onPhase?.('managerKeys');
  const read = await readChip(tx);
  if (!read.ok) {
    return {installed: read.stop, setup: null, resumable: !pinSet};
  }
  const chip = read.chip;
  options.onChip?.(chip);
  const sentPin = pinSet ? options.attemptedPin?.(chip) ?? null : null;

  let check: ResumeCheck;
  try {
    check = await resumeCheck(chip, oldId);
  } catch (e) {
    if (pinSet && sentPin === null) {
      return guard(KEYED, chip);
    }
    return {
      installed: {
        ok: false,
        kind: 'noManagerKeys',
        phase: 'managerKeys',
        reason: `ops could not say whether card ${oldId} may be finished: ${
          e instanceof Error ? e.message : String(e)
        }`,
      },
      setup: null,
      chip,
      resumable: true,
    };
  }
  const answered = check.short_id === oldId && check.chip === chip;
  if (pinSet && (!answered || check.known)) {
    // A card ops knows (a finished one, most likely): never touched.
    return guard(KEYED, chip);
  }
  if (!answered || !check.resumable) {
    return guard(
      `ops holds something for card ${oldId}: ${
        check.reasons.join('; ') || 'its answer named another card'
      }`,
      chip,
    );
  }
  if (pinSet && sentPin === null) {
    // Ours and unknown to ops, but its PIN went out in a tap whose PIN this
    // phone no longer holds: nothing is tried against it.
    return {
      installed: {
        ok: false,
        kind: 'needsReset',
        phase: 'probe',
        reason: `card ${oldId} has a PIN this phone does not hold, so no PIN was tried; Settings → Reset a card takes it back to blank`,
      },
      setup: null,
      chip,
    };
  }
  if (pinSet) {
    options.onResume?.(oldId);
  }

  const fetched = await keysFor(chip, options.managerKeys);
  if (!fetched.ok) {
    // Nothing was written: the same card's next tap asks again.
    return {installed: fetched.stop, setup: null, chip, resumable: true};
  }
  try {
    // The card's own keys, proved from INITIALIZE UPDATE before any login.
    options.onPhase?.('rotate');
    const proved = await confirmManagerKeys(tx, fetched.keys, {
      random: options.random,
      now: options.now,
    });
    if (!proved.ok) {
      const kind = proved.kind ?? 'refused';
      return {
        installed: {
          ok: false,
          kind,
          phase: 'rotate',
          sw: proved.sw,
          reason: kind === 'wrongKeys' ? NOT_OWN : proved.reason,
        },
        setup: null,
        chip,
        resumable: kind === 'pulled',
      };
    }

    if (sentPin === null) {
      const setup = await finishCard(tx, options.pin, options.onSetupStep, {
        rekey: true,
      });
      return {
        installed: {ok: true},
        setup,
        chip,
        rotated: false,
        resumedFrom: oldId,
      };
    }
    const setup = await finishVerified(tx, sentPin, options.onSetupStep);
    if (!setup.ok && setup.step === 'verifyPin') {
      return {
        installed: {
          ok: false,
          kind: 'needsReset',
          phase: 'verifyPin',
          sw: setup.sw ?? undefined,
          reason: `${setup.reason}; Settings → Reset a card takes it back to blank`,
        },
        setup: null,
        chip,
      };
    }
    return {
      installed: {ok: true},
      setup,
      chip,
      rotated: false,
      finishedWithAttemptedPin: true,
    };
  } finally {
    forget(fetched.keys);
  }
}

/**
 * Installs the applet if the card has none, rotates the card manager to the
 * card's own keys if it is still on the factory set, then keys and PINs the
 * card. `before` is what `probeCard` found; a keyed card never gets here.
 */
async function installRotateFinish(
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
  // hold; one with the applet but no keys skips the install, and one with
  // keys and no PIN is resumed (`resumeTap`).
  const before = await probeCard(tx);
  if (before.kind === 'keyed') {
    return resumeTap(tx, before, options);
  }
  options.onPhase?.('managerKeys');
  const fetched = await chipAndKeys(tx, options.managerKeys);
  if (fetched.chip !== undefined) {
    options.onChip?.(fetched.chip);
  }
  if (!fetched.ok) {
    return {installed: fetched.stop, setup: null, chip: fetched.chip};
  }
  try {
    const rest = await installRotateFinish(tx, before.kind, fetched.keys, options);
    return {...rest, chip: fetched.chip};
  } finally {
    forget(fetched.keys);
  }
}
