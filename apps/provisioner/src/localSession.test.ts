/**
 * The local-session dev flag (`src/localSession.ts`).
 *
 * Two things have to stay true for the flag to be safe to leave in the tree,
 * and these are them: it is off in the build that ships, and a session it
 * supplies can never be mistaken for a real one. The rest of the module is
 * exercised through `App`'s wiring, which is a render, not a unit.
 */
import {registryOrigin} from './api';
import {
  LOCAL_SESSION_ENABLED,
  LOCAL_SESSION_LABEL,
  localStoreApi,
} from './localSession';

describe('the local-session flag', () => {
  it('is off by default', () => {
    // A release build must not carry a path that signs an operator in without
    // a registry. Turning it on is a deliberate local edit.
    expect(LOCAL_SESSION_ENABLED).toBe(false);
  });

  it('labels the session as one that sends nothing', () => {
    // The label is what a person reads before trusting the screen. It must say
    // both that it is local and that no registry sees the work.
    expect(LOCAL_SESSION_LABEL).toMatch(/LOCAL SESSION/);
    expect(LOCAL_SESSION_LABEL).toMatch(/nothing is sent/i);
  });
});

describe('the flag as App uses it', () => {
  it('never names a registry host', () => {
    // The flag is not a way to fill REGISTRY_ORIGINS quietly: with no origin,
    // `App` has nothing live to build and can only fall back to the fake.
    expect(registryOrigin('test')).toBeNull();
    expect(registryOrigin('production')).toBeNull();
  });
});

describe('the local store api', () => {
  it('signs in without a service and names itself', async () => {
    const api = localStoreApi();
    await expect(api.signIn('operator', 'anything')).resolves.toEqual({
      operatorId: 0,
      token: 'local-session',
    });
    // A URL that is obviously not one, so nothing can be posted at a guess.
    expect(api.serviceUrl).toBe('local-session');
  });

  it.each(['postBatch', 'postCard', 'retryPending'] as const)(
    'refuses %s rather than pretending a row was stored',
    async method => {
      // A fake that resolved here would let a card be counted as registered
      // when nothing left the device — the one failure this flag must not cause.
      const api = localStoreApi();
      const call = api[method] as (...args: unknown[]) => Promise<unknown>;
      await expect(call.call(api, {}, {})).rejects.toThrow(
        /needs a live registry/,
      );
    },
  );
});
