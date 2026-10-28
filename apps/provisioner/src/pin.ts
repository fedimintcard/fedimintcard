/**
 * PIN entry, generation, and the wipe-on-leave rules
 * (docs/PROVISIONER-PLAN.md § 5, "PIN handling in the app").
 *
 * A PIN crosses three places and must survive none of them. This module owns
 * the first: the PIN as the operator types it, as the app generates it, and
 * the exact rules under which a copy of it may outlive the screen or the card
 * it was typed for. It never owns the second (the `SET_PIN` APDU, which
 * `session.ts` sends with the value it is handed) or the third (the till).
 *
 * The rules, verbatim from § 5, and where each one lives:
 *
 * - "held in component state only" — `usePinDraft` and `useRetainedPin`:
 *   `useState`/`useRef` inside a component, never a Context value that is read
 *   by unrelated screens, never module state, never the store.
 * - "cleared on leaving the screen" — `usePinDraft`'s unmount effect, plus the
 *   screen's own `clear()` on its leave path and `WipeOnBlur` for the field.
 * - "never in the store's persisted batch state" — the store records a policy
 *   name and nothing else; `BatchConfig` has no PIN field to put one in.
 * - "never in the registry body" — nothing in this module is exported to the
 *   network layer; the policy name is the whole of what leaves, and it is a
 *   `PinPolicy`, not a function of a PIN.
 * - "never in a log or a crash report" — the value never becomes a string
 *   except through `maskedPin`, and `toJSON`/`toString` on `PinDraft` redact
 *   it, so a PIN inside a logged object or a devtools dump prints as
 *   `pin(redacted)`.
 * - "A generated PIN is shown once on the summary and is not retrievable
 *   after the batch ends" — `usePinReveal`'s slot: `grant` at generation,
 *   `current` for the `SET_PIN` APDU, `reveal` once on the summary, and
 *   `wipeAll` from the store's `end()` and `signOut()`.
 *
 * There is no import of `@react-native-async-storage/async-storage` here, and
 * there must never be: the only storage this app writes is
 * `prov.settings.v1` (settings.tsx) and the session/outbox records in api.ts,
 * none of which takes a PIN.
 */
import React, {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react';
import {TextInput, type TextInputProps} from 'react-native';

/** Where the PIN enters: BatchSetup itself, or the loop's generated reveal. */
export type PinPolicy = 'batch' | 'perCard';

/**
 * The applet's bounds, transcribed from `FedimintCardApplet.java`
 * (`PIN_MIN = 4`, `PIN_MAX = 8`). The applet never interprets the bytes; it
 * only checks the length, and refuses anything outside 4..8 with `67 00`
 * without spending a retry. A UI that guessed 6 could both reject a valid
 * 4-digit PIN and send one the card refuses. Keep these in step with the
 * applet; `pin.test.ts` pins the range.
 */
export const PIN_MIN = 4;
export const PIN_MAX = 8;

/** The applet's three tries; a fourth wrong PIN answers `69 83` and blocks the card. */
export const PIN_TRIES = 3;

/**
 * The length this app generates. Inside the applet's range, and the applet
 * stores digits as bytes, so a generated PIN is digits only.
 */
export const GENERATED_PIN_LENGTH = 6;

export const PIN_POLICY_LABEL: Record<PinPolicy, string> = {
  batch: 'One PIN for the batch',
  perCard: 'A PIN per card, generated',
};

/** Digits only; anything else is dropped as it is typed or pasted. */
export function digitsOnly(value: string): string {
  return value.replace(/\D/g, '');
}

/**
 * The value the card would accept: digits, of a length the applet takes, and
 * not trivially guessable. Longer than `PIN_MAX` is truncated at entry, shorter
 * than `PIN_MIN` is refused before the card is touched.
 */
export function isValidPin(pin: string): boolean {
  if (pin.length < PIN_MIN || pin.length > PIN_MAX) {
    return false;
  }
  if (!/^[0-9]+$/.test(pin)) {
    return false;
  }
  return !isWeakPin(pin);
}

/** All one digit, or a straight run: the two a bench would guess first. */
export function isWeakPin(pin: string): boolean {
  if (pin.length === 0) {
    return false;
  }
  if (/^(.)\1*$/.test(pin)) {
    return true;
  }
  let ascending = true;
  let descending = true;
  for (let i = 1; i < pin.length; i++) {
    const d = pin.charCodeAt(i) - pin.charCodeAt(i - 1);
    if (d !== 1) {
      ascending = false;
    }
    if (d !== -1) {
      descending = false;
    }
  }
  return ascending || descending;
}

/** Why a typed PIN cannot be used, or null. The screen shows this, not a guess. */
export function pinProblem(pin: string): string | null {
  if (pin.length === 0) {
    return 'Enter a PIN.';
  }
  if (!/^[0-9]+$/.test(pin)) {
    return 'A PIN is digits only.';
  }
  if (pin.length < PIN_MIN) {
    return `A PIN is at least ${PIN_MIN} digits.`;
  }
  if (pin.length > PIN_MAX) {
    return `A PIN is at most ${PIN_MAX} digits.`;
  }
  if (isWeakPin(pin)) {
    return 'Avoid repeated digits and straight runs.';
  }
  return null;
}

/** Uniform random digits from an injected source; the card's PIN is never derived from a seed. */
function randomPin(random: () => number, length: number): string {
  let out = '';
  while (out.length < length) {
    const r = random();
    if (!Number.isFinite(r) || r < 0 || r >= 1) {
      continue;
    }
    out += Math.floor(r * 10).toString();
  }
  return out;
}

/**
 * A fresh PIN for one card. The default source is the platform CSPRNG; a test
 * injects its own. A regenerated PIN is retried, never weakened, so the result
 * always satisfies `isValidPin` for the lengths this app generates.
 */
export function generatePin(
  random: () => number = platformRandom,
  length: number = GENERATED_PIN_LENGTH,
): string {
  if (length < PIN_MIN || length > PIN_MAX) {
    throw new Error(`a generated PIN must be ${PIN_MIN}..${PIN_MAX} digits`);
  }
  for (let attempt = 0; attempt < 32; attempt++) {
    const pin = randomPin(random, length);
    if (!isWeakPin(pin)) {
      return pin;
    }
  }
  // 32 weak draws in a row is not a plausible CSPRNG; refusing beats a weak PIN.
  throw new Error('the random source produced no usable PIN');
}

/** Uniform in [0, 1) from the platform CSPRNG, without bias for a single digit. */
const platformRandom = (): number => {
  const g = globalThis as {crypto?: {getRandomValues?: (a: Uint32Array) => Uint32Array}};
  const c = g.crypto;
  if (c?.getRandomValues) {
    const b = new Uint32Array(1);
    c.getRandomValues(b);
    return b[0] / 0x1_0000_0000;
  }
  return Math.random();
};

/**
 * The only string form of a PIN that may be displayed or logged: the length,
 * not the digits. There is deliberately no `toDisplay` that shows the value —
 * the holder's PIN is not the operator's to see after the batch ends.
 */
export function maskedPin(pin: string): string {
  return pin.length === 0 ? '' : '•'.repeat(pin.length);
}

/** A value the card would take, without reference to the batch that will send it. */
export interface PinDraft {
  /** The PIN, or null. Never rendered, never serialised (see `toJSON`). */
  readonly value: string | null;
  set(next: string): void;
  clear(): void;
  readonly valid: boolean;
  readonly problem: string | null;
  /** Explicit wipe; the screen's leave path and the store's end-of-batch path call this. */
  wipe(): void;
  toJSON(): string;
  toString(): string;
}

/** The redacted form every accidental stringification of a draft goes through. */
const REDACTED = 'pin(policy)';

function makeDraft(
  value: string | null,
  onChange: (next: string | null) => void,
): PinDraft {
  const draft: PinDraft = {
    value,
    set(next: string): void {
      onChange(next === '' ? null : digitsOnly(next).slice(0, PIN_MAX));
    },
    clear(): void {
      onChange(null);
    },
    get valid(): boolean {
      return value !== null && isValidPin(value);
    },
    get problem(): string | null {
      return value === null ? null : pinProblem(value);
    },
    wipe(): void {
      onChange(null);
    },
    toJSON(): string {
      return REDACTED;
    },
    toString(): string {
      return REDACTED;
    },
  };
  return draft;
}

/**
 * A PIN held in component state only, cleared when the component goes away.
 *
 * `usePinDraft` is for the operator typing a batch PIN in BatchSetup; the
 * value lives in this component's state and in nothing else, and `clear()`
 * runs on unmount. Screens must call `clear()` themselves on their own leave
 * path too — a screen that is popped but kept mounted (a navigator that
 * detaches instead of unmounting) is a real case, and the cost of clearing
 * twice is nothing.
 */
export function usePinDraft(initial: string | null = null): PinDraft {
  const [value, setValue] = useState<string | null>(initial);

  // Leaving the screen, by any route, clears. An unmount is the last of them.
  useEffect(() => () => setValue(null), []);

  return useMemo(
    () => makeDraft(value, setValue),
    // `makeDraft` closes over the value it renders, so the identity changes with it.
    [value],
  );
}

/**
 * The batch PIN, held across two screens: BatchSetup types it, the loop re-sends
 * it for every card, and nothing else may see it.
 *
 * The holder is a provider's ref, not the store's state: `BatchConfig` has no
 * field for a value (§ 5), and a provider is the narrowest thing that can
 * outlive a screen without outliving the batch. `set` is called once, on START
 * BATCH; `value` is read by the loop immediately before the `SET_PIN` APDU and
 * by nothing else. `wipe` runs when the batch ends, when the operator signs out
 * and when the provider unmounts — three paths, all idempotent.
 *
 * A per-card policy stores nothing here at all: the loop generates that PIN and
 * stages it in `usePinReveal`'s slot for the one card it belongs to.
 */
export interface BatchPinContext {
  /** The batch PIN for the `SET_PIN` APDU, or null when there is none. */
  readonly value: string | null;
  set(pin: string): void;
  wipe(): void;
}

const BatchPinCtx = createContext<BatchPinContext | null>(null);

export function BatchPinProvider({children}: {children: React.ReactNode}) {
  const ref = useRef<string | null>(null);
  // A batch never outlives the provider; neither does its PIN.
  useEffect(
    () => () => {
      ref.current = null;
    },
    [],
  );
  // One object for the provider's life: the getter reads the ref, so the
  // context value never changes and no consumer re-renders when the PIN does.
  const value = useMemo<BatchPinContext>(
    () => ({
      get value(): string | null {
        return ref.current;
      },
      set(pin: string): void {
        ref.current = digitsOnly(pin).slice(0, PIN_MAX);
      },
      wipe(): void {
        ref.current = null;
      },
    }),
    [],
  );
  return React.createElement(BatchPinCtx.Provider, {value}, children);
}

/** The batch PIN's holder; throws outside `BatchPinProvider` rather than handing out null. */
export function useBatchPin(): BatchPinContext {
  const ctx = useContext(BatchPinCtx);
  if (!ctx) {
    throw new Error('useBatchPin outside BatchPinProvider');
  }
  return ctx;
}

/**
 * The batch PIN's holder where its absence is legitimate — the store mounts
 * above the provider, and still has to wipe on sign-out in a tree that has no
 * provider at all (a screen under test).
 */
export function useOptionalBatchPin(): BatchPinContext | null {
  return useContext(BatchPinCtx);
}

/**
 * Where a generated PIN may be shown. A per-card PIN is generated in the loop,
 * shown once on the summary, and not retrievable after the batch ends: one
 * card's PIN at a time, and the store's `end()` wipes it. The store never
 * receives the value; it exists only in this slot, keyed by the row it belongs to.
 */
export interface PinRevealContext {
  /** Store the generated PIN for this card's row. Replaces any other card's. */
  grant(id: string, pin: string): void;
  /** The one visually revealed PIN. BatchSummary calls this once, for the card just programmed. */
  reveal(): RevealedPin | null;
  /** The value for the `SET_PIN` APDU of the card being programmed now. */
  current(): RevealedPin | null;
  /** Drop it: a regenerate, a card that failed, or the batch ending. */
  wipe(): void;
}

/** The revealed PIN's row id, for a caller that needs to label which card it was. */
export interface RevealedPin {
  readonly id: string;
  readonly pin: string;
}

const RevealCtx = createContext<PinRevealContext | null>(null);

/**
 * The reveal slot's provider. Deliberately not part of `store.tsx`: the store
 * dispatches `wipeAllPins` at end of batch, but the value itself never enters
 * the store's state or its action payloads beyond the one call into here.
 */
export function PinRevealProvider({children}: {children: React.ReactNode}) {
  const ref = useRef<RevealedPin | null>(null);
  // A batch never outlives the provider; neither does a PIN.
  useEffect(
    () => () => {
      ref.current = null;
    },
    [],
  );
  // One object for the provider's life: every method reads the ref, so no
  // method changes identity and no consumer re-renders when the PIN does.
  const value = useMemo<PinRevealContext>(
    () => ({
      grant(id: string, pin: string): void {
        ref.current = {id, pin: digitsOnly(pin).slice(0, PIN_MAX)};
      },
      reveal: () => ref.current,
      current: () => ref.current,
      wipe(): void {
        ref.current = null;
      },
    }),
    [],
  );
  return React.createElement(RevealCtx.Provider, {value}, children);
}

/** Outside the provider the slot is inert, so a screen tree without one still renders. */
const NO_REVEAL: PinRevealContext = {
  grant(): void {},
  reveal: () => null,
  current: () => null,
  wipe(): void {},
};

export function usePinReveal(): PinRevealContext {
  return useContext(RevealCtx) ?? NO_REVEAL;
}

/**
 * A blur that clears the field. `TextInput`'s `onBlur` is the hardware
 * keyboard dismissal, not the screen leaving; the screen-leave path is
 * `usePinDraft`'s unmount and the screen's own `clear()`.
 */
export function WipeOnBlur({
  draft,
  children,
}: {
  draft: PinDraft;
  children: (onBlur: () => void) => React.ReactNode;
}) {
  const onBlur = useCallback(() => draft.clear(), [draft]);
  return React.createElement(React.Fragment, null, children(onBlur));
}

/**
 * The PIN field: digits only, capped at the applet's maximum, `secureTextEntry`,
 * autofill and clipboard suggestions off. The styling comes from the caller's
 * theme; this component only enforces the mechanics.
 */
export function PinField({
  draft,
  placeholder = 'PIN',
  ...rest
}: {
  draft: PinDraft;
  placeholder?: string;
} & Pick<
  TextInputProps,
  'style' | 'editable' | 'testID' | 'accessibilityLabel'
>) {
  return React.createElement(TextInput, {
    value: draft.value ?? '',
    onChangeText: (v: string) => draft.set(v),
    keyboardType: 'number-pad',
    secureTextEntry: true,
    textContentType: 'none',
    autoComplete: 'off',
    autoCorrect: false,
    autoCapitalize: 'none',
    importantForAutofill: 'no',
    maxLength: PIN_MAX,
    placeholder,
    ...rest,
  });
}

