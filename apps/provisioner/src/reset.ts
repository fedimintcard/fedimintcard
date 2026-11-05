/**
 * Reset a card in the provisioner (OPS-PLAN O5): Settings → Reset a card.
 * The card ends where it left the chip maker: the Fedimint package deleted
 * (only ours: F0464D43415244, its applet and its opener), the card manager
 * back on the factory keys at version 01, and both proved by a full login
 * under those keys. Ops then marks the old record reset and releases the
 * chip, so any operator can install it again in a new batch (the install tap
 * gets the same per-card keys back from ops).
 *
 * Two taps. The first reads slot 0's key (the short id), the PIN's state
 * without spending a try, and the chip's CPLC id, and the phone asks ops
 * whether the card may go, naming both: not while ops shows a live or pending
 * note, an open charge or an open load, and never another operator's card.
 * The chip is always sent, not only when the short id is unknown: it costs
 * no extra round trip, and it is what tells ops a card that never reached a
 * holder (below) from one that did, even when ops has a stopped row under
 * the short id. The second, after the holder typed the PIN:
 *
 *   slot 0 again (the same card) → ops' check again → VERIFY_PIN in the same
 *   selection → CPLC (the chip the first tap read) → the card's keys from
 *   ops → cardkit's `factoryReset`:
 *   one INITIALIZE UPDATE checked against the card's own set and the factory
 *   set → login with the one it proved → GET STATUS → journal → DELETE ours
 *   → PUT KEY back to the factory set → a fresh factory login and GET STATUS
 *   (the proof) → the completion into the outbox → ops
 *
 * The holder's PIN is the consent: ops cannot see a load made around it. A
 * wrong PIN stops with the tries left and is never retried here. A blocked
 * card (or one whose PIN was never set) can be reset only when ops shows it
 * empty and the operator has confirmed a second warning.
 *
 * **No holder, no PIN.** An install that stopped after SET_PIN, with the
 * phone no longer holding the PIN it sent (`needsReset`), leaves a card with
 * a PIN nobody knows, under a short id ops may not have. Ops finds it by its
 * chip and answers `needs_holder_pin: false` only when the chip's escrow is
 * this operator's, no open row on the chip is a finished card, and ops holds
 * nothing on any of them: the card never reached a holder, so there is no
 * consent to ask for. The tap then skips VERIFY_PIN and the PIN's state
 * altogether; DELETE and PUT KEY need the card manager's keys, not the
 * applet's PIN.
 *
 * **Crash safety.** The journal is written just before the first DELETE or
 * PUT KEY, so a tap lifted after it is finished by tapping the same card
 * again: the next tap checks it is the same chip and lets `factoryReset` read
 * where the card is (own keys with ours present: carry on deleting; own keys
 * with ours gone: restore the keys; factory keys with ours gone: done, only
 * the record is left; factory keys with ours present: stop). It never sends
 * EXTERNAL AUTHENTICATE for a set the card did not prove. The holder is not
 * asked again: the PIN went with the applet. The journal holds the operator,
 * the old short id, the chip and when; no PINs, no keys.
 *
 * **The outbox.** Once the card is proved factory, its completion (the proof,
 * no secrets) is saved before the journal goes, and posted to ops from
 * there; a failed post is retried from the outbox, never from the card. The
 * card is finished either way.
 */
import {
  CardError,
  TransportError,
  factoryReset,
  type Random,
  type RunEvent,
  type Transceiver,
  readChipIdentity,
} from './cardkit';
import {
  exchange,
  INS_GET_PUB,
  INS_VERIFY_PIN,
  keyApdu,
  pinApdu,
  pinStatusApdu,
  selectApdu,
  SW_BLOCKED,
  SW_CONDITIONS,
  SW_OK,
  SW_PIN_NOT_SET,
  SW_WRONG_LENGTH,
  type PinStatus,
} from './cardSetup';
import {chipAndKeys, forget} from './installTap';
import type {CardOps, ResetCheck, ResetCompletion, ResetRecorded} from './ops';
import {shortIdOf} from './store';

export {
  pinStatusApdu,
  SW_PIN_NOT_SET,
  SW_WRONG_LENGTH,
  type PinStatus,
} from './cardSetup';

/* ------------------------------------------------------------------------- */
/* The journal and the outbox                                                */
/* ------------------------------------------------------------------------- */

/** The one key. Bumping the suffix discards older shapes (v1 was the reinstalling reset's). */
export const RESET_JOURNAL_KEY = 'prov.reset.v2';
/** Completions ops has not acknowledged yet. */
export const RESET_OUTBOX_KEY = 'prov.reset.outbox.v1';

/** Where the journal lives: AsyncStorage on the phone, a map in the tests. */
export interface JournalStorage {
  getItem(key: string): Promise<string | null>;
  setItem(key: string, value: string): Promise<void>;
  removeItem(key: string): Promise<void>;
}

/** A reset that may have written to the card and is not yet proved factory. */
export interface ResetJournal {
  readonly schema: 2;
  readonly operator_id: number;
  readonly old_short_id: string;
  readonly chip: string;
  readonly started_at: number;
  /** Ops has a row under the old short id; absent (older journals) reads true. */
  readonly known?: boolean;
}

/** A proved completion waiting for ops, and whose it is. */
export interface PendingCompletion extends ResetCompletion {
  readonly operator_id: number;
}

const SHORT_ID = /^[0-9a-f]{8}$/;
const CHIP = /^cplc-v1:[0-9a-f]{24}$/;

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
      j.schema === 2 &&
      Number.isSafeInteger(j.operator_id) &&
      SHORT_ID.test(j.old_short_id) &&
      CHIP.test(j.chip) &&
      Number.isSafeInteger(j.started_at) &&
      (j.known === undefined || typeof j.known === 'boolean');
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
    schema: 2,
    operator_id: j.operator_id,
    old_short_id: j.old_short_id,
    chip: j.chip,
    started_at: j.started_at,
    known: j.known !== false,
  };
  await storage.setItem(RESET_JOURNAL_KEY, JSON.stringify(out));
}

export function clearJournal(storage: JournalStorage): Promise<void> {
  return storage.removeItem(RESET_JOURNAL_KEY);
}

function isPending(v: unknown): v is PendingCompletion {
  const p = v as PendingCompletion | undefined;
  return (
    typeof p === 'object' &&
    p !== null &&
    Number.isSafeInteger(p.operator_id) &&
    SHORT_ID.test(p.short_id) &&
    CHIP.test(p.chip) &&
    Number.isInteger(p.gp_key_version) &&
    p.manager_keys === 'factory' &&
    p.package === 'absent' &&
    Number.isSafeInteger(p.verified_at)
  );
}

/** Every completion ops has not acknowledged, whoever's; entries this build cannot read are dropped. */
export async function readOutbox(
  storage: JournalStorage,
): Promise<PendingCompletion[]> {
  const raw = await storage.getItem(RESET_OUTBOX_KEY);
  if (raw === null) {
    return [];
  }
  try {
    const list = JSON.parse(raw) as unknown;
    return Array.isArray(list) ? list.filter(isPending) : [];
  } catch {
    return [];
  }
}

async function writeOutbox(
  storage: JournalStorage,
  list: readonly PendingCompletion[],
): Promise<void> {
  if (list.length === 0) {
    await storage.removeItem(RESET_OUTBOX_KEY);
    return;
  }
  const out = list.map(p => ({
    operator_id: p.operator_id,
    short_id: p.short_id,
    chip: p.chip,
    gp_key_version: p.gp_key_version,
    manager_keys: p.manager_keys,
    package: p.package,
    verified_at: p.verified_at,
  }));
  await storage.setItem(RESET_OUTBOX_KEY, JSON.stringify(out));
}

/** Adds (or replaces, by old short id) a proved completion. */
async function addToOutbox(
  storage: JournalStorage,
  p: PendingCompletion,
): Promise<void> {
  const list = (await readOutbox(storage)).filter(
    q => q.short_id !== p.short_id,
  );
  await writeOutbox(storage, [...list, p]);
}

export interface Flushed {
  readonly recorded: readonly ResetRecorded[];
  /** Ops' refusal or the network's, for the first entry that failed; the rest wait. */
  readonly failed: {readonly short_id: string; readonly reason: string} | null;
  /** This operator's completions still waiting. */
  readonly left: readonly PendingCompletion[];
}

/**
 * Posts this operator's waiting completions to ops, oldest first, and drops
 * each one ops acknowledges (the route is idempotent, so a repeat is safe).
 * Stops at the first failure; another operator's entries are left alone.
 */
export async function flushOutbox(
  ops: Pick<CardOps, 'completeReset'>,
  storage: JournalStorage,
  operatorId: number,
): Promise<Flushed> {
  const recorded: ResetRecorded[] = [];
  let failed: Flushed['failed'] = null;
  for (const p of await readOutbox(storage)) {
    if (p.operator_id !== operatorId) {
      continue;
    }
    try {
      recorded.push(
        await ops.completeReset({
          short_id: p.short_id,
          chip: p.chip,
          gp_key_version: p.gp_key_version,
          manager_keys: p.manager_keys,
          package: p.package,
          verified_at: p.verified_at,
        }),
      );
    } catch (e) {
      failed = {
        short_id: p.short_id,
        reason: e instanceof Error ? e.message : String(e),
      };
      break;
    }
    const rest = (await readOutbox(storage)).filter(
      q => q.short_id !== p.short_id,
    );
    await writeOutbox(storage, rest);
  }
  const left = (await readOutbox(storage)).filter(
    q => q.operator_id === operatorId,
  );
  return {recorded, failed, left};
}

/* ------------------------------------------------------------------------- */
/* Tap 1: which card, and may it go                                          */
/* ------------------------------------------------------------------------- */

export type Identified =
  | {
      readonly ok: true;
      readonly shortId: string;
      readonly pin: PinStatus;
      /** The chip's CPLC id, which ops finds a card by when it has no row for the short id. */
      readonly chip: string;
    }
  | {
      readonly ok: false;
      /** `noApplet`: blank or wiped. `unkeyed`: installed, never keyed. */
      readonly why: 'noApplet' | 'unkeyed' | 'refused';
      readonly sw?: number | null;
    };

/**
 * Slot 0's key, the PIN's state, then the chip's CPLC id (which selects the
 * card manager, so it comes last). Writes nothing and spends no PIN try.
 */
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
  let chip: string;
  try {
    chip = (await readChipIdentity(tx)).id;
  } catch (e) {
    if (e instanceof CardError) {
      return {ok: false, why: 'refused', sw: e.sw};
    }
    throw e;
  }
  return {ok: true, shortId: shortIdOf(slot0.body), pin, chip};
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
  /** The card's own steps: cardkit's stops, or ops' keys. */
  | {
      readonly kind:
        | 'pulled'
        | 'refused'
        | 'wrongKeys'
        | 'guarded'
        | 'noManagerKeys';
      readonly reason?: string;
      readonly sw?: number | null;
    };

export type ResetTapResult =
  /** The card is proved factory; the completion is in the outbox for ops. */
  | {readonly ok: true; readonly completion: ResetCompletion}
  | {
      readonly ok: false;
      readonly stop: ResetStop;
      /** True once the journal was written: the card may be part way, and the next tap finishes it. */
      readonly resumable: boolean;
    };

export type ResetPhase =
  | 'check'
  | 'consent'
  | 'chip'
  | 'erase'
  | 'restore'
  | 'prove';

export interface ResetTapOptions {
  readonly operatorId: number;
  /** The short id the first tap read; null when finishing a journalled reset. */
  readonly expected: string | null;
  /**
   * The chip the first tap read, sent with ops' check and matched against
   * the CPLC before anything is written; null or absent when finishing a
   * journalled reset (the journal names the chip) or from an older caller.
   */
  readonly expectedChip?: string | null;
  /** The holder's PIN, typed for this tap only; null on the blocked path. */
  readonly holderPin: string | null;
  /** The operator confirmed the second warning for a blocked (or unset) PIN. */
  readonly blockedConfirmed: boolean;
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

/** `beforeWrite` could not save the journal: nothing was written to the card. */
class JournalFailed extends Error {}

const CARD_PHASES: Record<string, ResetPhase> = {
  erase: 'erase',
  restore: 'restore',
  prove: 'prove',
};

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
 * The reset tap. Throws only for a transport loss before cardkit's run (the
 * card is then untouched, and the caller says so) and for bugs; every other
 * outcome is a result.
 */
export async function resetTap(
  tx: Transceiver,
  options: ResetTapOptions,
): Promise<ResetTapResult> {
  const saved = await readJournal(options.journal);
  if (saved && saved.operator_id !== options.operatorId) {
    return stop({kind: 'otherOperator'});
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
  const expectedChip = saved ? saved.chip : options.expectedChip ?? null;
  /** Whether ops has a row under the old id: only then is it named with the keys. */
  let known = saved?.known !== false;

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
      check =
        expectedChip === null
          ? await options.ops.resetCheck(oldId)
          : await options.ops.resetCheck(oldId, expectedChip);
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
    known = check.known;
    // A card that never reached a holder has no PIN to consent with (ops
    // says so only for the chip this tap must then prove, below).
    if (check.needs_holder_pin || expectedChip === null) {
      options.onPhase?.('consent');
      // The applet is still selected from `readCard`: the PIN is proved for the card just read.
      const refused = await consent(tx, options);
      if (refused) {
        return stop(refused);
      }
    }
  }

  options.onPhase?.('chip');
  // Ops links the chip to a short id only when it has the card's row.
  const fetched = await chipAndKeys(tx, async chip => {
    if (expectedChip !== null && chip !== expectedChip) {
      throw new Error('this is not the chip the card was read on');
    }
    return known
      ? options.ops.managerKeys(chip, oldId)
      : options.ops.managerKeys(chip);
  });
  if (!fetched.ok) {
    if (
      expectedChip !== null &&
      fetched.chip !== undefined &&
      fetched.chip !== expectedChip
    ) {
      // Refused before ops was asked for anything.
      return stop(
        {
          kind: 'differentCard',
          reason: saved
            ? 'this is not the chip the unfinished reset was on'
            : 'this is not the chip the card was read on',
        },
        saved !== null,
      );
    }
    const s = fetched.stop;
    return s.ok
      ? stop({kind: 'refused'}, resuming)
      : stop({kind: s.kind, reason: s.reason, sw: s.sw}, resuming);
  }
  let journaled = saved !== null;
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
    // The CPLC was matched in this tap: a factory card proved below is this chip.
    const done = await factoryReset(tx, fetched.keys, {
      random: options.random,
      now: options.now,
      onEvent: event => {
        if (event.type === 'phase' && CARD_PHASES[event.phase]) {
          options.onPhase?.(CARD_PHASES[event.phase]);
        }
        options.onEvent?.(event);
      },
      beforeWrite: async () => {
        if (journaled) {
          return;
        }
        try {
          await writeJournal(options.journal, {
            schema: 2,
            operator_id: options.operatorId,
            old_short_id: oldId,
            chip: fetched.chip,
            started_at: options.now(),
            known,
          });
        } catch (e) {
          throw new JournalFailed(e instanceof Error ? e.message : String(e));
        }
        journaled = true;
      },
    });
    if (!done.ok) {
      return stop(
        {kind: done.kind ?? 'refused', reason: done.reason, sw: done.sw},
        journaled,
      );
    }
    const completion: ResetCompletion = {
      short_id: oldId,
      chip: fetched.chip,
      gp_key_version: done.keyVersion!,
      manager_keys: 'factory',
      package: 'absent',
      verified_at: options.now(),
    };
    try {
      await addToOutbox(options.journal, {
        ...completion,
        operator_id: options.operatorId,
      });
      await clearJournal(options.journal);
    } catch {
      // The card is factory either way. A journal left behind is finished by
      // the next tap, which proves the card factory again and saves this.
    }
    return {ok: true, completion};
  } catch (e) {
    if (e instanceof JournalFailed) {
      return stop({kind: 'journal', reason: e.message});
    }
    if (e instanceof TransportError || e instanceof CardError) {
      return stop(
        {
          kind: e instanceof TransportError ? 'pulled' : 'refused',
          reason: e.message,
        },
        journaled,
      );
    }
    throw e;
  } finally {
    forget(fetched.keys);
  }
}
