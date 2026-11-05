import React from 'react';
import TestRenderer, {act} from 'react-test-renderer';

import {ApiServiceError} from '../api';
import type {TillRow} from '../ops';
import {StoreProvider, type StoreApi} from '../store';
import {TillsScreen} from './TillsScreen';

const LIVE = 'https://registry.fedimintcard.org';

function fakeApi(over: Partial<StoreApi> = {}) {
  const calls: string[] = [];
  let tills: TillRow[] = [{till_id: 1, name: 'Front till', created_at: Date.now() - 120_000, revoked_at: null}];
  const api: StoreApi = {
    serviceUrl: LIVE,
    signIn: async () => ({operatorId: 1, token: 't'}),
    postBatch: async () => undefined,
    postCard: async () => undefined,
    retryPending: async () => [],
    listTills: async () => {
      calls.push('list');
      return tills;
    },
    revokeTill: async id => {
      calls.push(`revoke ${id}`);
      tills = tills.map(t => (t.till_id === id ? {...t, revoked_at: Date.now()} : t));
    },
    claimInfo: async code => {
      calls.push(`info ${code}`);
      return {name: 'Back till', created_at: Date.now() - 30_000, expires_at: Date.now() + 500_000, status: 'pending'};
    },
    approveClaim: async code => {
      calls.push(`approve ${code}`);
      tills = [...tills, {till_id: 2, name: 'Back till', created_at: Date.now(), revoked_at: null}];
    },
    ...over,
  };
  return {api, calls};
}

async function mount(api: StoreApi) {
  let r!: TestRenderer.ReactTestRenderer;
  await act(async () => {
    r = TestRenderer.create(
      <StoreProvider api={api}>
        <TillsScreen onBack={() => undefined} />
      </StoreProvider>,
    );
  });
  return r;
}

const byId = (r: TestRenderer.ReactTestRenderer, id: string) => r.root.findByProps({testID: id});
const has = (r: TestRenderer.ReactTestRenderer, id: string) => r.root.findAllByProps({testID: id}).length > 0;
const text = (r: TestRenderer.ReactTestRenderer) =>
  r.root.findAllByType('Text' as any).map(n => [].concat(n.props.children).join('')).join('\n');
const press = async (r: TestRenderer.ReactTestRenderer, id: string) => {
  await act(async () => {
    byId(r, id).props.onPress();
  });
};

test('lists tills and stops one only after the confirm', async () => {
  const {api, calls} = fakeApi();
  const r = await mount(api);
  expect(text(r)).toContain('Front till');
  expect(text(r)).toContain('Added 2 minutes ago');
  await press(r, 'till-1-stop');
  expect(text(r)).toContain('That till stops charging at once');
  expect(calls).not.toContain('revoke 1');
  await press(r, 'till-1-confirm');
  expect(calls).toContain('revoke 1');
  expect(text(r)).toContain('Front till has stopped');
  expect(text(r)).toMatch(/Stopped just now/);
  expect(has(r, 'till-1-stop')).toBe(false);
});

test('adds a till by typed code: look up, approve, done, list refreshed', async () => {
  const {api, calls} = fakeApi();
  const r = await mount(api);
  await press(r, 'tills-add-entry');
  await act(async () => {
    byId(r, 'tills-code').props.onChangeText('k7qm3xrp');
  });
  await press(r, 'tills-check');
  expect(calls).toContain('info K7QM-3XRP');
  expect(text(r)).toContain('Back till');
  expect(calls.some(c => c.startsWith('approve'))).toBe(false);
  await press(r, 'tills-approve');
  expect(calls).toContain('approve K7QM-3XRP');
  expect(text(r)).toContain('Back till is connected');
  const listsBefore = calls.filter(c => c === 'list').length;
  await press(r, 'tills-done-button');
  expect(calls.filter(c => c === 'list').length).toBe(listsBefore + 1);
  expect(has(r, 'till-2')).toBe(true);
});

test('refuses a claim for another registry before asking anything', async () => {
  const {api, calls} = fakeApi();
  const r = await mount(api);
  await press(r, 'tills-add-entry');
  await act(async () => {
    byId(r, 'tills-code').props.onChangeText(
      'fedimintcard-till-claim:https://fedimintcard-registry-staging.old-cell-30e7.workers.dev?c=K7QM-3XRP',
    );
  });
  await press(r, 'tills-check');
  expect(byId(r, 'tills-problem').props.children).toMatch(/asked the test registry.*signed in to the live registry/);
  expect(calls.some(c => c.startsWith('info'))).toBe(false);
});

test('an expired code is said plainly', async () => {
  const {api} = fakeApi({
    claimInfo: async () => {
      throw new ApiServiceError('no claim', 404);
    },
  });
  const r = await mount(api);
  await press(r, 'tills-add-entry');
  await act(async () => {
    byId(r, 'tills-code').props.onChangeText('K7QM-3XRP');
  });
  await press(r, 'tills-check');
  expect(byId(r, 'tills-problem').props.children).toMatch(/No till is waiting with that code/);
});
