/**
 * Reset a card in the provisioner (OPS-PLAN O5): Settings → Reset a card.
 * The card's applet is deleted and installed again, the card manager is left
 * on (or moved to) the card's own keys, and the card gets new slot keys and a
 * new PIN, so it comes back as a new card with a new short id. Ops links the
 * old record to the new one.
 *
 * Two taps. The first reads slot 0's key (the short id) and the PIN's state
 * without spending a try, and the phone asks ops whether the card may go: not
 * while ops shows a live or pending note, an open charge or an open load, and
 * never another operator's card. The second, after the holder typed the PIN:
 *
 *   slot 0 again (the same card) → ops' check again → VERIFY_PIN in the same
 *   selection → CPLC → the card's keys from ops → journal → DELETE and
 *   reinstall (cardkit's sweep: this card's two instances and its package,
 *   nothing else) → rotate if still on factory keys → GEN_KEY ×8, SET_PIN,
 *   VERIFY_PIN → journal → ops records the reset
 *
 * The holder's PIN is the consent: ops cannot see a load made around it. A
 * wrong PIN stops with the tries left and is never retried here. A blocked
 * card (or one whose PIN was never set) can be reset only when ops shows it
 * empty and the operator has confirmed a second warning.
 *
 * **The journal.** Written before the first DELETE, so a tap that breaks after
 * it can be finished: the card is then blank, or half installed, on its
 * factory keys or its own, and the next tap checks it is the same chip and
 * carries on without asking for the PIN again (the old PIN went with the
 * applet). It holds the operator, the old short id, the chip and, once the
 * card is finished, the new card's public keys. No PINs, no manager keys, no
 * notes: cashucard's rule for its reset journal.
 */
import {
  CardError,
  TransportError,
  type Random,
  type RunEvent,
  type Transceiver,
} from './cardkit';
import {
  exchange,
  INS_GET_PUB,
  INS_VERIFY_PIN,
  keyApdu,
  pinApdu,
  selectApdu,
  SW_BLOCKED,
  SW_CONDITIONS,
  SW_OK,
} from './cardSetup';
import {
  chipAndKeys,
  forget,
  installRotateFinish,
  NEITHER_SET,
} from './installTap';
import type {CardOps, ResetCardRow, ResetCheck, ResetRecorded} from './ops';
import {shortIdOf} from './store';

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

const hex = (keys: readonly number[]) =>
  keys.map(b => b.toString(16).padStart(2, '0')).join('');

/* ------------------------------------------------------------------------- */
/* The journal                                                               */
/* ------------------------------------------------------------------------- */

/** The one key. Bumping the suffix discards older shapes. */
export const RESET_JOURNAL_KEY = 'prov.reset.v1';

/** Where the journal lives: AsyncStorage on the phone, a map in the tests. */
export interface JournalStorage {
  getItem(key: string): Promise<string | null>;
  setItem(key: string, value: string): Promise<void>;
  removeItem(key: string): Promise<void>;
}

export interface ResetJournal {
  readonly schema: 1;
  /** `erasing`: the card may have been wiped. `recording`: finished, ops not told yet. */
  readonly stage: 'erasing' | 'recording';
  readonly operator_id: number;
  readonly old_short_id: string;
  readonly chip: string;
  readonly started_at: number;
  /** The finished card, public keys only; set with `recording`. */
  readonly card?: ResetCardRow;
}

const SHORT_ID = /^[0-9a-f]{8}$/;
const CHIP = /^cplc-v1:[0-9a-f]{24}$/;

function isCardRow(v: unknown): v is ResetCardRow {
  const r = v as ResetCardRow | undefined;
  return (
    typeof r === 'object' &&
    r !== null &&
    SHORT_ID.test(r.short_id) &&
    Number.isSafeInteger(r.installed_at) &&
    CHIP.test(r.chip) &&
    Array.isArray(r.slot_pubkeys) &&
    r.slot_pubkeys.length === 8 &&
    r.slot_pubkeys.every(k => typeof k === 'string' && /^[0-9a-f]{64}$/.test(k))
  );
}

/** The saved reset, or null for none (or one this build cannot read). */
export async function readJournal(
  storage: JournalStorage,
): Promise<ResetJournal | null> {
  const raw = await storage.getItem(RESET_JOURNAL_KEY);
  if (raw === null) {
    return null;
  }
  try {
    const j = JSON.parse(raw) as ResetJournal;
    const ok =
      j.schema === 1 &&
      (j.stage === 'erasing' || j.stage === 'recording') &&
      Number.isSafeInteger(j.operator_id) &&
      SHORT_ID.test(j.old_short_id) &&
      CHIP.test(j.chip) &&
      Number.isSafeInteger(j.started_at) &&
      (j.stage === 'erasing' || isCardRow(j.card));
    return ok ? j : null;
  } catch {
    return null;
  }
}

/** Only the fields named here are written: never a PIN, a key set or a reply. */
async function writeJournal(
  storage: JournalStorage,
  j: ResetJournal,
): Promise<void> {
  const out: ResetJournal = {
    schema: 1,
    stage: j.stage,
    operator_id: j.operator_id,
    old_short_id: j.old_short_id,
    chip: j.chip,
    started_at: j.started_at,
    ...(j.card
      ? {
          card: {
            short_id: j.card.short_id,
            installed_at: j.card.installed_at,
            slot_pubkeys: [...j.card.slot_pubkeys],
            chip: j.card.chip,
          },
        }
      : {}),
  };
  await storage.setItem(RESET_JOURNAL_KEY, JSON.stringify(out));
}

export function clearJournal(storage: JournalStorage): Promise<void> {
  return storage.removeItem(RESET_JOURNAL_KEY);
}

/* ------------------------------------------------------------------------- */
/* Tap 1: which card, and may it go                                          */
/* ------------------------------------------------------------------------- */

export type Identified =
  | {readonly ok: true; readonly shortId: string; readonly pin: PinStatus}
  | {
      readonly ok: false;
      /** `noApplet`: blank or wiped. `unkeyed`: installed, never keyed. */
      readonly why: 'noApplet' | 'unkeyed' | 'refused';
      readonly sw?: number | null;
    };

/** Slot 0's key and the PIN's state. Writes nothing and spends no PIN try. */
export async function identifyCard(tx: Transceiver): Promise<Identified> {
  const selected = await exchange(tx, selectApdu());
  if (selected.sw !== SW_OK) {
    return {ok: false, why: 'noApplet', sw: selected.sw};
  }
  const slot0 = await exchange(tx, keyApdu(INS_GET_PUB, 0));
  if (slot0.sw === SW_CONDITIONS) {
    return {ok: false, why: 'unkeyed'};
  }
  if (slot0.sw !== SW_OK || slot0.body.length !== 32) {
    return {ok: false, why: 'refused', sw: slot0.sw};
  }
  const status = await exchange(tx, pinStatusApdu());
  const pin: PinStatus | null =
    status.sw === SW_WRONG_LENGTH
      ? 'set'
      : status.sw === SW_BLOCKED
      ? 'blocked'
      : status.sw === SW_PIN_NOT_SET
      ? 'notSet'
      : null;
  if (pin === null) {
    return {ok: false, why: 'refused', sw: status.sw};
  }
  return {ok: true, shortId: shortIdOf(slot0.body), pin};
}

/* ------------------------------------------------------------------------- */
/* Tap 2: the reset                                                          */
/* ------------------------------------------------------------------------- */

export type ResetStop =
  /** Not the card the first tap read, or not the chip the journal names. */
  | {readonly kind: 'differentCard'; readonly reason: string}
  /** Ops refuses: value or work in flight, already reset, or not this operator's. */
  | {readonly kind: 'notResettable'; readonly reasons: readonly string[]}
  | {readonly kind: 'wrongPin'; readonly triesLeft: number}
  /** The PIN is blocked or was never set: the second warning, then a confirmed tap. */
  | {readonly kind: 'pinBlocked'; readonly pin: 'blocked' | 'notSet'}
  /** The card's PIN works, so the holder must type it. */
  | {readonly kind: 'pinRequired'}
  /** The journal could not be written, so nothing was deleted. */
  | {readonly kind: 'journal'; readonly reason: string}
  /** A reset another operator started on this phone is unfinished. */
  | {readonly kind: 'otherOperator'}
  /** The card is finished and only ops' record is left: `recordReset`, no tap. */
  | {readonly kind: 'recordPending'}
  /** The card's own steps: cardkit's stops, ops' keys, or the keys and PIN. */
  | {
      readonly kind:
        | 'pulled'
        | 'refused'
        | 'wrongKeys'
        | 'guarded'
        | 'noManagerKeys'
        | 'notFinished';
      readonly reason?: string;
      readonly sw?: number | null;
    };

export type ResetTapResult =
  | {readonly ok: true; readonly card: ResetCardRow}
  | {
      readonly ok: false;
      readonly stop: ResetStop;
      /** True once the journal was written: the card may be wiped, and the next tap finishes it. */
      readonly resumable: boolean;
    };

export type ResetPhase =
  | 'check'
  | 'consent'
  | 'chip'
  | 'install'
  | 'rotate'
  | 'keys'
  | 'pin';

export interface ResetTapOptions {
  readonly operatorId: number;
  /** The short id the first tap read; null when finishing a journalled reset. */
  readonly expected: string | null;
  /** The holder's PIN, typed for this tap only; null on the blocked path. */
  readonly holderPin: string | null;
  /** The operator confirmed the second warning for a blocked (or unset) PIN. */
  readonly blockedConfirmed: boolean;
  /** The new card's PIN; shown by the caller once the tap finishes. */
  readonly newPin: string;
  readonly ops: Pick<CardOps, 'managerKeys' | 'resetCheck'>;
  readonly journal: JournalStorage;
  readonly random: Random;
  readonly now: () => number;
  readonly onPhase?: (phase: ResetPhase) => void;
  readonly onEvent?: (event: RunEvent) => void;
}

const stop = (s: ResetStop, resumable = false): ResetTapResult => ({
  ok: false,
  stop: s,
  resumable,
});

/** What the card holds now: a keyed applet with its short id, an unkeyed one, or none. */
async function readCard(
  tx: Transceiver,
): Promise<{kind: 'keyed'; id: string} | {kind: 'unkeyed' | 'none'}> {
  const selected = await exchange(tx, selectApdu());
  if (selected.sw !== SW_OK) {
    return {kind: 'none'};
  }
  const slot0 = await exchange(tx, keyApdu(INS_GET_PUB, 0));
  if (slot0.sw === SW_OK && slot0.body.length === 32) {
    return {kind: 'keyed', id: shortIdOf(slot0.body)};
  }
  return {kind: slot0.sw === SW_CONDITIONS ? 'unkeyed' : 'none'};
}

/**
 * The holder's consent, in the selection that read slot 0. Never sends a
 * second VERIFY_PIN: a wrong PIN is the holder's to retry, on the next tap.
 */
async function consent(
  tx: Transceiver,
  options: ResetTapOptions,
): Promise<ResetStop | null> {
  if (options.holderPin !== null) {
    const verified = await exchange(
      tx,
      pinApdu(INS_VERIFY_PIN, options.holderPin),
    );
    if (verified.sw === SW_OK) {
      return null;
    }
    if (verified.sw !== null && verified.sw >> 4 === 0x63c) {
      return {kind: 'wrongPin', triesLeft: verified.sw & 0x0f};
    }
    if (verified.sw === SW_BLOCKED) {
      return {kind: 'pinBlocked', pin: 'blocked'};
    }
    if (verified.sw === SW_PIN_NOT_SET) {
      return {kind: 'pinBlocked', pin: 'notSet'};
    }
    return {kind: 'refused', sw: verified.sw, reason: 'VERIFY_PIN was refused'};
  }
  const status = await exchange(tx, pinStatusApdu());
  const pin =
    status.sw === SW_BLOCKED
      ? 'blocked'
      : status.sw === SW_PIN_NOT_SET
      ? 'notSet'
      : null;
  if (pin === null) {
    return status.sw === SW_WRONG_LENGTH
      ? {kind: 'pinRequired'}
      : {
          kind: 'refused',
          sw: status.sw,
          reason: 'the PIN state could not be read',
        };
  }
  return options.blockedConfirmed ? null : {kind: 'pinBlocked', pin};
}

/**
 * The reset tap. Throws only for a transport loss before the journal (the
 * card is then untouched, and the caller says so) and for bugs; every other
 * outcome is a result.
 */
export async function resetTap(
  tx: Transceiver,
  options: ResetTapOptions,
): Promise<ResetTapResult> {
  if (!/^[0-9]{4,8}$/.test(options.newPin)) {
    throw new Error('resetTap: the new PIN must be 4 to 8 digits');
  }
  const saved = await readJournal(options.journal);
  if (saved && saved.operator_id !== options.operatorId) {
    return stop({kind: 'otherOperator'});
  }
  if (saved?.stage === 'recording') {
    // The card is finished; only ops is left, and that needs no card. Never
    // an `ok`: the caller would show `newPin`, which this card does not have.
    return stop({kind: 'recordPending'}, true);
  }
  const card = await readCard(tx);
  // A journalled reset whose card still answers with the old short id never
  // reached DELETE: it starts over, consent and all.
  const resuming =
    saved !== null &&
    !(card.kind === 'keyed' && card.id === saved.old_short_id);
  const oldId = saved?.old_short_id ?? options.expected;
  if (oldId === null) {
    throw new Error('resetTap: no card to reset was read');
  }

  if (!resuming) {
    if (card.kind !== 'keyed') {
      return stop({
        kind: 'differentCard',
        reason:
          card.kind === 'none'
            ? 'this card has no Fedimint applet'
            : 'this card was never keyed',
      });
    }
    if (card.id !== oldId) {
      return stop({
        kind: 'differentCard',
        reason: `this is card ${card.id}, not ${oldId}`,
      });
    }
    options.onPhase?.('check');
    let check: ResetCheck;
    try {
      check = await options.ops.resetCheck(oldId);
    } catch (e) {
      return stop({
        kind: 'notResettable',
        reasons: [e instanceof Error ? e.message : String(e)],
      });
    }
    if (!check.resettable) {
      return stop({
        kind: 'notResettable',
        reasons: check.reasons.length
          ? check.reasons
          : ['ops will not reset this card'],
      });
    }
    options.onPhase?.('consent');
    // The applet is still selected from `readCard`: the PIN is proved for the card just read.
    const refused = await consent(tx, options);
    if (refused) {
      return stop(refused);
    }
  }

  options.onPhase?.('chip');
  const fetched = await chipAndKeys(tx, chip =>
    options.ops.managerKeys(chip, oldId),
  );
  if (!fetched.ok) {
    const s = fetched.stop;
    return s.ok
      ? stop({kind: 'refused'}, resuming)
      : stop({kind: s.kind, reason: s.reason, sw: s.sw}, resuming);
  }
  try {
    if (saved && fetched.chip !== saved.chip) {
      return stop(
        {
          kind: 'differentCard',
          reason: 'this is not the chip the unfinished reset was on',
        },
        true,
      );
    }
    if (!saved) {
      try {
        await writeJournal(options.journal, {
          schema: 1,
          stage: 'erasing',
          operator_id: options.operatorId,
          old_short_id: oldId,
          chip: fetched.chip,
          started_at: options.now(),
        });
      } catch (e) {
        return stop({
          kind: 'journal',
          reason: e instanceof Error ? e.message : String(e),
        });
      }
    }
    options.onPhase?.('install');
    // `none` always: the plan sweeps whatever of this card's applet is there
    // (a keyed applet, a torn install, or nothing) and installs it fresh.
    const done = await installRotateFinish(tx, 'none', fetched.keys, {
      pin: options.newPin,
      random: options.random,
      now: options.now,
      onEvent: options.onEvent,
      onPhase: () => options.onPhase?.('rotate'),
      onSetupStep: step =>
        options.onPhase?.(
          step === 'setPin' || step === 'verifyPin' ? 'pin' : 'keys',
        ),
    });
    if (!done.installed.ok) {
      const i = done.installed;
      return stop(
        {
          kind: i.kind,
          sw: i.sw,
          reason: i.kind === 'wrongKeys' ? NEITHER_SET : i.reason,
        },
        true,
      );
    }
    const setup = done.setup;
    if (!setup || !setup.ok) {
      return stop(
        {
          kind: 'notFinished',
          sw: setup?.sw ?? null,
          reason: setup?.reason ?? 'key setup did not run',
        },
        true,
      );
    }
    const row: ResetCardRow = {
      short_id: shortIdOf(setup.keys[0]),
      installed_at: options.now(),
      slot_pubkeys: setup.keys.map(k => hex([...k])),
      chip: fetched.chip,
    };
    try {
      await writeJournal(options.journal, {
        schema: 1,
        stage: 'recording',
        operator_id: options.operatorId,
        old_short_id: oldId,
        chip: fetched.chip,
        started_at: saved?.started_at ?? options.now(),
        card: row,
      });
    } catch {
      // The card is finished either way; the caller records it now, and a
      // failed record is retried from the card's row it holds.
    }
    return {ok: true, card: row};
  } catch (e) {
    if (e instanceof TransportError || e instanceof CardError) {
      return stop(
        {
          kind: e instanceof TransportError ? 'pulled' : 'refused',
          reason: e.message,
        },
        true,
      );
    }
    throw e;
  } finally {
    forget(fetched.keys);
  }
}

/**
 * Tells ops, after the tap: the new row, and the old one marked reset. Clears
 * the journal once ops has it; a failure keeps it, and the same body is sent
 * again next time (the route is idempotent).
 */
export async function recordReset(
  ops: Pick<CardOps, 'recordReset'>,
  journal: JournalStorage,
  oldShortId: string,
  card: ResetCardRow,
): Promise<ResetRecorded> {
  const recorded = await ops.recordReset(oldShortId, card);
  await clearJournal(journal);
  return recorded;
}
