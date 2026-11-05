import React from 'react';
import TestRenderer, {act} from 'react-test-renderer';

import {StoreProvider, useStore, type StoreApi, type StoreShape} from './store';

function fakeApi(origin: string): StoreApi {
  return {
    serviceUrl: origin,
    signIn: async () => ({operatorId: 1, token: `tok-${origin}`}),
    postBatch: async () => undefined,
    postCard: async () => undefined,
    retryPending: async () => [],
  };
}

describe('switching registry', () => {
  it('signs out, because the session belongs to the old registry', async () => {
    let seen: StoreShape | null = null;
    const Probe = () => {
      seen = useStore();
      return null;
    };
    const live = fakeApi('https://live');
    let r!: TestRenderer.ReactTestRenderer;
    await act(async () => {
      r = TestRenderer.create(
        <StoreProvider api={live}>
          <Probe />
        </StoreProvider>,
      );
    });
    await act(async () => {
      await seen!.signIn('dread', 'pw');
    });
    expect(seen!.signedIn).toBe(true);

    // A re-render with the same client keeps the session.
    await act(async () => {
      r.update(
        <StoreProvider api={live}>
          <Probe />
        </StoreProvider>,
      );
    });
    expect(seen!.signedIn).toBe(true);

    await act(async () => {
      r.update(
        <StoreProvider api={fakeApi('https://staging')}>
          <Probe />
        </StoreProvider>,
      );
    });
    expect(seen!.signedIn).toBe(false);
  });
});
