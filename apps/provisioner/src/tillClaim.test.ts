import {ApiServiceError, ApiUnreachableError, REGISTRY_ORIGINS} from './api';
import {
  ADD_START,
  addReducer,
  normalizeClaimCode,
  parseClaimText,
  readTypedClaim,
  registryLabel,
  tillFailure,
  whenText,
  type AddState,
} from './tillClaim';

const LIVE = REGISTRY_ORIGINS.production!;
const TEST = REGISTRY_ORIGINS.test!;

describe('claim codes', () => {
  it('upper-cases and accepts the code with or without its dash', () => {
    expect(normalizeClaimCode('K7QM-3XRP')).toBe('K7QM-3XRP');
    expect(normalizeClaimCode('k7qm3xrp')).toBe('K7QM-3XRP');
    expect(normalizeClaimCode(' k7qm 3xrp ')).toBe('K7QM-3XRP');
    expect(normalizeClaimCode('k7qm-3xrp')).toBe('K7QM-3XRP');
  });

  it('refuses a code of the wrong length or outside the alphabet', () => {
    expect(normalizeClaimCode('K7QM-3XR')).toBeNull();
    expect(normalizeClaimCode('K7QM-3XRPP')).toBeNull();
    // no 0, O, 1, I or L in a code
    for (const bad of ['K7QM-3XR0', 'K7QM-3XRO', 'K7QM-3XR1', 'K7QM-3XRI', 'K7QM-3XRL']) {
      expect(normalizeClaimCode(bad)).toBeNull();
    }
    expect(normalizeClaimCode('K7QM_3XRP')).toBeNull();
    expect(normalizeClaimCode('')).toBeNull();
  });
});

describe('claim QR text', () => {
  it('reads the spec example on the live registry', () => {
    expect(
      parseClaimText('fedimintcard-till-claim:https://registry.fedimintcard.org?c=K7QM-3XRP', LIVE),
    ).toEqual({ok: true, code: 'K7QM-3XRP', origin: 'https://registry.fedimintcard.org'});
  });

  it('matches the origin without regard to case or a trailing slash', () => {
    const r = parseClaimText('fedimintcard-till-claim:HTTPS://Registry.Fedimintcard.org/?c=k7qm3xrp', LIVE);
    expect(r).toEqual({ok: true, code: 'K7QM-3XRP', origin: 'https://registry.fedimintcard.org'});
  });

  it('refuses a claim for another registry, naming the one the till asked', () => {
    const r = parseClaimText(`fedimintcard-till-claim:${LIVE}?c=K7QM-3XRP`, TEST);
    expect(r).toEqual({ok: false, reason: 'otherRegistry', origin: LIVE});
    expect(registryLabel(LIVE)).toBe('live registry');
    expect(registryLabel(TEST)).toBe('test registry');
    expect(registryLabel('https://ops.example.com/')).toBe('ops.example.com');
  });

  it('says a claim with no usable code is a bad code', () => {
    expect(parseClaimText(`fedimintcard-till-claim:${LIVE}`, LIVE)).toEqual({ok: false, reason: 'badCode'});
    expect(parseClaimText(`fedimintcard-till-claim:${LIVE}?c=NOPE`, LIVE)).toEqual({ok: false, reason: 'badCode'});
    expect(parseClaimText(`fedimintcard-till-claim:${LIVE}?x=1&c=%E0%A4%A`, LIVE)).toEqual({ok: false, reason: 'badCode'});
  });

  it('reads the code among other parameters', () => {
    expect(parseClaimText(`fedimintcard-till-claim:${LIVE}?v=1&c=K7QM-3XRP`, LIVE)).toMatchObject({ok: true, code: 'K7QM-3XRP'});
  });

  it('is not a claim at all when the prefix is missing', () => {
    expect(parseClaimText('fed11qgqyj3mfwfhksw309', LIVE)).toBeNull();
    expect(parseClaimText('https://registry.fedimintcard.org?c=K7QM-3XRP', LIVE)).toBeNull();
  });

  it('takes a typed code or a pasted claim in the code field', () => {
    expect(readTypedClaim('k7qm3xrp', LIVE)).toEqual({ok: true, code: 'K7QM-3XRP', origin: LIVE});
    expect(readTypedClaim(`fedimintcard-till-claim:${TEST}?c=K7QM-3XRP`, LIVE)).toMatchObject({reason: 'otherRegistry'});
    expect(readTypedClaim('hello', LIVE)).toEqual({ok: false, reason: 'badCode'});
  });
});

describe('failures', () => {
  it('maps the registry refusals to plain kinds', () => {
    expect(tillFailure(new ApiUnreachableError(), 'claim')).toBe('offline');
    expect(tillFailure(new ApiServiceError('no claim', 404), 'claim')).toBe('unknownCode');
    expect(tillFailure(new ApiServiceError('already approved', 409), 'claim')).toBe('alreadyApproved');
    expect(tillFailure(new ApiServiceError('unauthorized', 401), 'list')).toBe('signedOut');
    expect(tillFailure(new ApiServiceError('no such till for this operator', 404), 'revoke')).toBe('noTill');
    expect(tillFailure(new ApiServiceError('boom', 500), 'claim')).toBe('other');
    expect(tillFailure('nope', 'claim')).toBe('other');
  });
});

describe('add a till: the steps', () => {
  const claim = {name: 'Front till', created_at: 1, expires_at: 2, status: 'pending' as const};
  const run = (events: Parameters<typeof addReducer>[1][]): AddState =>
    events.reduce(addReducer, ADD_START);

  it('goes enter → checking → review → approving → done', () => {
    let s = run([{type: 'submit', claim: {ok: true, code: 'K7QM-3XRP', origin: LIVE}}]);
    expect(s).toEqual({step: 'checking', code: 'K7QM-3XRP'});
    s = addReducer(s, {type: 'found', claim});
    expect(s).toEqual({step: 'review', code: 'K7QM-3XRP', claim});
    s = addReducer(s, {type: 'approve'});
    expect(s.step).toBe('approving');
    s = addReducer(s, {type: 'approved'});
    expect(s).toEqual({step: 'done', name: 'Front till'});
  });

  it('stays on the entry step with the reason for a bad code or another registry', () => {
    expect(run([{type: 'submit', claim: {ok: false, reason: 'badCode'}}])).toEqual({
      step: 'enter',
      problem: {kind: 'badCode'},
    });
    expect(run([{type: 'submit', claim: {ok: false, reason: 'otherRegistry', origin: TEST}}])).toEqual({
      step: 'enter',
      problem: {kind: 'otherRegistry', asked: TEST},
    });
  });

  it('treats an approved claim as nothing left to approve', () => {
    const s = run([
      {type: 'submit', claim: {ok: true, code: 'K7QM-3XRP', origin: LIVE}},
      {type: 'found', claim: {...claim, status: 'approved'}},
    ]);
    expect(s).toEqual({step: 'enter', problem: {kind: 'alreadyApproved'}});
  });

  it('returns to the entry step on a failed lookup or approval', () => {
    const s = run([
      {type: 'submit', claim: {ok: true, code: 'K7QM-3XRP', origin: LIVE}},
      {type: 'found', claim},
      {type: 'approve'},
      {type: 'failed', failure: 'unknownCode'},
    ]);
    expect(s).toEqual({step: 'enter', problem: {kind: 'unknownCode'}});
  });

  it('ignores events that do not fit the step', () => {
    expect(addReducer(ADD_START, {type: 'approve'})).toBe(ADD_START);
    expect(addReducer(ADD_START, {type: 'approved'})).toBe(ADD_START);
    expect(addReducer(ADD_START, {type: 'failed', failure: 'offline'})).toBe(ADD_START);
    const review: AddState = {step: 'review', code: 'K7QM-3XRP', claim};
    expect(addReducer(review, {type: 'submit', claim: {ok: false, reason: 'badCode'}})).toBe(review);
  });
});

describe('times', () => {
  const now = Date.UTC(2026, 10, 5, 12, 0, 0);
  it('words recent times relatively and older ones as a day', () => {
    expect(whenText(now - 10_000, now)).toBe('just now');
    expect(whenText(now - 60_000, now)).toBe('1 minute ago');
    expect(whenText(now - 4 * 60_000, now)).toBe('4 minutes ago');
    expect(whenText(now - 2 * 3_600_000, now)).toBe('2 hours ago');
    expect(whenText(now - 3 * 86_400_000, now)).toMatch(/^\d{1,2} Nov 2026$/);
  });
});
