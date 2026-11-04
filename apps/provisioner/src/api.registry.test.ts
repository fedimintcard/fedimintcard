import {RegistryClient, RegistryStoreApi} from './api';

const post = (body: unknown, status = 200) =>
  ({ok: status < 400, status, json: async () => body, text: async () => ''} as unknown as Response);

test('the adapter signs in, opens a batch, and posts a row at the registry shapes', async () => {
  const seen: any[] = [];
  const origin = 'https://registry.test';
  const fetcher = async (url: string, init?: RequestInit) => {
    const path = String(url).replace(origin, '');
    const body = init?.body ? JSON.parse(String(init.body)) : undefined;
    seen.push({path, body, auth: (init?.headers as any)?.Authorization ?? (init?.headers as any)?.authorization});
    if (path === '/auth/operator/session') return post({operator_id: 7, token: 'tok-1'});
    // the service wraps both answers: {batch: {...}} and {stored: {...}}
    if (path === '/registry/batches') return post({batch: {batch_id: 42, federation_id: 'f', count: 1, pin_policy: 'batch', load_sha256: 'x', created_at: 0}});
    if (path === '/registry/cards') return post({stored: {short_id: 'C1', received_at: 1}});
    throw new Error('unexpected ' + path);
  };
  const api = new RegistryStoreApi(new RegistryClient({baseUrl: origin, fetcher}), origin);
  expect(api.serviceUrl).toBe(origin);
  const s = await api.signIn('dread', 'pw');
  expect(s).toEqual({operatorId: 7, token: 'tok-1'});
  // The invite scanned from a guardian UI on 2026-11-04 (federation.test.ts).
  const invite =
    'fed11qgqyj3mfwfhksw309uekyvesvenrzdpjx93xvdf4xcex2d33xajxycmzxf3nzdn9x9jxyctpxy6k2vnpv56nwvtpvsmxgepnx93kvvnzvscnjefn8qurxc3eqqqjp8wgs64nt3zuzc4j073a5mlr2cm4kp6t2jtv06d05kcu3elavy5g5h4kwl';
  await api.postBatch({federation: invite, count: 1, pinPolicy: 'batch'});
  await api.postCard({
    id: 'C1', at: 1000, phase: 'done', ok: true, pinPolicy: 'batch',
    pinState: 'set', notesIssued: [1048, 524], pending: false,
  } as any);
  // The registry records the federation's id, never the invite text.
  expect(seen[1].body.federation_id).toBe(
    '9dc886ab35c45c162b27fa3da6fe356375b074b5496c7e9afa5b1c8e7fd61288',
  );
  expect(seen[1].body.batch_id).toBeUndefined();
  expect(seen[2].body.batch_id).toBe(42);
  expect(seen[2].body.notes_issued).toBe(1572);
  expect(seen[2].body.pin_set).toBe(true);
});

test('a batch or a card before sign-in is refused, not sent', async () => {
  const api = new RegistryStoreApi(new RegistryClient({baseUrl: 'https://r.test', fetcher: async () => { throw new Error('sent'); }}), 'https://r.test');
  await expect(api.postBatch({federation: 'f', count: 1, pinPolicy: 'batch'}))
    .rejects.toThrow(/before an operator signed in/);
});
