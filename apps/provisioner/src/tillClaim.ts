/**
 * Settings → Tills, the parts that are not drawing (docs/TILL-CLAIM.md): the
 * claim QR's text, the code an operator types, what a refusal means in plain
 * words, and the Add-a-till steps as a reducer the screen renders.
 */
import {normalizeClaimCode, REGISTRY_ORIGINS} from './api';
import type {TillClaimInfo} from './ops';

export {normalizeClaimCode};

export const CLAIM_PREFIX = 'fedimintcard-till-claim:';

/** What a scanned or pasted claim says, checked against the registry the app is on. */
export type ClaimText =
  | {readonly ok: true; readonly code: string; readonly origin: string}
  | {readonly ok: false; readonly reason: 'otherRegistry'; readonly origin: string}
  | {readonly ok: false; readonly reason: 'badCode'};

/** An origin as compared: scheme and host lower case, no trailing slash. */
export function normalizeOrigin(origin: string): string {
  return origin.trim().replace(/\/+$/, '').toLowerCase();
}

/**
 * `fedimintcard-till-claim:<registry origin>?c=<code>`. Null when the text is
 * not a till claim at all (another QR code); otherwise the code, or why it
 * cannot be used here. `current` is the origin this app is signed in to.
 */
export function parseClaimText(text: string, current: string): ClaimText | null {
  const trimmed = text.trim();
  if (!trimmed.toLowerCase().startsWith(CLAIM_PREFIX)) {
    return null;
  }
  const rest = trimmed.slice(CLAIM_PREFIX.length);
  const q = rest.indexOf('?');
  const origin = normalizeOrigin(q < 0 ? rest : rest.slice(0, q));
  const query = q < 0 ? '' : rest.slice(q + 1);
  let raw: string | null = null;
  for (const pair of query.split('&')) {
    const eq = pair.indexOf('=');
    if (eq > 0 && pair.slice(0, eq) === 'c') {
      try {
        raw = decodeURIComponent(pair.slice(eq + 1));
      } catch {
        raw = null;
      }
    }
  }
  if (origin !== normalizeOrigin(current)) {
    return {ok: false, reason: 'otherRegistry', origin};
  }
  const code = raw === null ? null : normalizeClaimCode(raw);
  return code === null ? {ok: false, reason: 'badCode'} : {ok: true, code, origin};
}

/**
 * What the operator typed into the code field: a code with or without its
 * dash, or a whole claim text pasted from somewhere.
 */
export function readTypedClaim(input: string, current: string): ClaimText {
  const parsed = parseClaimText(input, current);
  if (parsed) {
    return parsed;
  }
  const code = normalizeClaimCode(input);
  return code === null
    ? {ok: false, reason: 'badCode'}
    : {ok: true, code, origin: normalizeOrigin(current)};
}

/** A registry by the name Settings uses for it, else its host. */
export function registryLabel(origin: string): string {
  const o = normalizeOrigin(origin);
  if (o === normalizeOrigin(REGISTRY_ORIGINS.production ?? '')) {
    return 'live registry';
  }
  if (o === normalizeOrigin(REGISTRY_ORIGINS.test ?? '')) {
    return 'test registry';
  }
  return o.replace(/^https?:\/\//, '');
}

/** Why a till call did not go through, as the screen words it. */
export type TillFailure =
  | 'offline'
  | 'unknownCode'
  | 'alreadyApproved'
  | 'signedOut'
  | 'noTill'
  | 'other';

/**
 * A rejection from `TillOps` as a failure kind. By the error's name and status,
 * as `ui.tsx`'s `signInReason` does, so this module needs no class identity.
 * `step` says which call it was: a 404 on a claim is an unknown or expired
 * code; on a revoke, a till that is not this operator's.
 */
export function tillFailure(e: unknown, step: 'claim' | 'revoke' | 'list'): TillFailure {
  if (!(e instanceof Error)) {
    return 'other';
  }
  if (e.name === 'ApiUnreachableError') {
    return 'offline';
  }
  const status = (e as {status?: unknown}).status;
  if (e.name === 'ApiServiceError') {
    if (status === 401) {
      return 'signedOut';
    }
    if (status === 404) {
      return step === 'claim' ? 'unknownCode' : step === 'revoke' ? 'noTill' : 'other';
    }
    if (status === 409 && step === 'claim') {
      return 'alreadyApproved';
    }
  }
  return 'other';
}

/* ------------------------------------------------------------------------- */
/* Add a till: the steps                                                       */
/* ------------------------------------------------------------------------- */

export type AddState =
  | {readonly step: 'enter'; readonly problem: AddProblem | null}
  | {readonly step: 'checking'; readonly code: string}
  | {readonly step: 'review'; readonly code: string; readonly claim: TillClaimInfo}
  | {readonly step: 'approving'; readonly code: string; readonly claim: TillClaimInfo}
  | {readonly step: 'done'; readonly name: string};

/** What stopped the last attempt, shown on the entry step. */
export type AddProblem =
  | {readonly kind: 'badCode'}
  | {readonly kind: 'otherRegistry'; readonly asked: string}
  | {readonly kind: TillFailure; readonly detail?: string};

export type AddEvent =
  | {readonly type: 'submit'; readonly claim: ClaimText}
  | {readonly type: 'found'; readonly claim: TillClaimInfo}
  | {readonly type: 'approve'}
  | {readonly type: 'approved'}
  | {readonly type: 'failed'; readonly failure: TillFailure; readonly detail?: string}
  | {readonly type: 'restart'};

export const ADD_START: AddState = {step: 'enter', problem: null};

/** The Add-a-till steps. Events that do not fit the current step are ignored. */
export function addReducer(state: AddState, event: AddEvent): AddState {
  switch (event.type) {
    case 'restart':
      return ADD_START;
    case 'submit':
      if (state.step !== 'enter') {
        return state;
      }
      if (event.claim.ok) {
        return {step: 'checking', code: event.claim.code};
      }
      return event.claim.reason === 'otherRegistry'
        ? {step: 'enter', problem: {kind: 'otherRegistry', asked: event.claim.origin}}
        : {step: 'enter', problem: {kind: 'badCode'}};
    case 'found':
      if (state.step !== 'checking') {
        return state;
      }
      // An approved claim is waiting for its till to collect the token: there
      // is nothing left for this operator to approve.
      return event.claim.status === 'approved'
        ? {step: 'enter', problem: {kind: 'alreadyApproved'}}
        : {step: 'review', code: state.code, claim: event.claim};
    case 'approve':
      return state.step === 'review' ? {...state, step: 'approving'} : state;
    case 'approved':
      return state.step === 'approving' ? {step: 'done', name: state.claim.name} : state;
    case 'failed':
      if (state.step !== 'checking' && state.step !== 'approving') {
        return state;
      }
      return {
        step: 'enter',
        problem: {kind: event.failure, ...(event.detail ? {detail: event.detail} : {})},
      };
  }
}

/* ------------------------------------------------------------------------- */
/* Times                                                                       */
/* ------------------------------------------------------------------------- */

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

/** "just now", "4 minutes ago", "2 hours ago", else the day: "5 Nov 2026". */
export function whenText(at: number, now: number): string {
  const minutes = Math.floor((now - at) / 60_000);
  if (minutes < 1) {
    return 'just now';
  }
  if (minutes < 60) {
    return `${minutes} ${minutes === 1 ? 'minute' : 'minutes'} ago`;
  }
  const hours = Math.floor(minutes / 60);
  if (hours < 24) {
    return `${hours} ${hours === 1 ? 'hour' : 'hours'} ago`;
  }
  const d = new Date(at);
  return `${d.getDate()} ${MONTHS[d.getMonth()]} ${d.getFullYear()}`;
}
