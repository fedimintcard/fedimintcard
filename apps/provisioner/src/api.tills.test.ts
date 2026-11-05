import {ApiServiceError, ApiUnreachableError, RegistryClient, RegistryStoreApi} from './api';

const reply = (body: unknown, status = 200) =>
  ({ok: status < 400, status, json: async () => body} as unknown as Response);

const origin = 'https://registry.test';

function harness(route: (method: string, path: string, body: any) => Response | Promise<Response>) {
  const seen: {method: string; path: string; body: any; auth?: string}[] = [];
  const fetcher = async (url: string, init?: RequestInit) => {
    const path = String(url).replace(origin, '');
    const method = init?.method ?? 'GET';
    const body = init?.body ? JSON.parse(String(init.body)) : undefined;
    seen.push({method, path, body, auth: (init?.headers as any)?.Authorization});
    if (path === '/auth/operator/session') {return reply({operator_id: 3, token: 'op_tok'});}
    return route(method, path, body);
  };
  const api = new RegistryStoreApi(new RegistryClient({baseUrl: origin, fetcher}), origin);
  return {api, seen};
}

test('lists the tills at the registry shape, revoked ones included', async () => {
  const {api, seen} = harness(() =>
    reply({
      ok: true,
      tills: [
        {till_id: 1, name: 'Front', created_at: 100, revoked_at: null},
        {till_id: 2, name: 'Back', created_at: 200, revoked_at: 300},
        {till_id: 'x', name: 'malformed'},
      ],
    }),
  );
  await api.signIn('op', 'pw');
  expect(await api.listTills()).toEqual([
    {till_id: 1, name: 'Front', created_at: 100, revoked_at: null},
    {till_id: 2, name: 'Back', created_at: 200, revoked_at: 300},
  ]);
  expect(seen[1]).toMatchObject({method: 'GET', path: '/tills', auth: 'Bearer op_tok'});
});

test('a till list that is not a list is refused', async () => {
  const {api} = harness(() => reply({ok: true}));
  await api.signIn('op', 'pw');
  await expect(api.listTills()).rejects.toBeInstanceOf(ApiServiceError);
});

test('revokes with DELETE /tills/:id, and a 404 is a typed refusal', async () => {
  const {api, seen} = harness((_m, path) =>
    path === '/tills/7' ? reply({ok: true}) : reply({ok: false, reason: 'no such till for this operator'}, 404),
  );
  await api.signIn('op', 'pw');
  await api.revokeTill(7);
  expect(seen[1]).toMatchObject({method: 'DELETE', path: '/tills/7', auth: 'Bearer op_tok'});
  const e = await api.revokeTill(8).catch(x => x);
  expect(e).toBeInstanceOf(ApiServiceError);
  expect(e.status).toBe(404);
  expect(e.reason).toBe('no such till for this operator');
  expect(e.operation).toBe('revoke-till');
});

test('reads a claim by its normalised code', async () => {
  const {api, seen} = harness(() =>
    reply({ok: true, name: 'Front till', created_at: 10, expires_at: 600_010, status: 'pending'}),
  );
  await api.signIn('op', 'pw');
  expect(await api.claimInfo('k7qm3xrp')).toEqual({
    name: 'Front till',
    created_at: 10,
    expires_at: 600_010,
    status: 'pending',
  });
  expect(seen[1]).toMatchObject({method: 'GET', path: '/till-claims/K7QM-3XRP', auth: 'Bearer op_tok'});
});

test('a malformed claim, an unknown code and an unreachable registry are told apart', async () => {
  let next: () => Response | Promise<Response> = () => reply({name: 'x'});
  const {api} = harness(() => next());
  await api.signIn('op', 'pw');
  await expect(api.claimInfo('K7QM-3XRP')).rejects.toMatchObject({name: 'ApiServiceError', operation: 'claim-info'});
  next = () => reply({ok: false, reason: 'no claim'}, 404);
  await expect(api.claimInfo('K7QM-3XRP')).rejects.toMatchObject({status: 404, reason: 'no claim'});
  next = () => Promise.reject(new TypeError('Network request failed'));
  await expect(api.claimInfo('K7QM-3XRP')).rejects.toBeInstanceOf(ApiUnreachableError);
});

test('approves with POST .../approve and an empty body; a 409 is a typed refusal', async () => {
  let status = 200;
  const {api, seen} = harness(() =>
    status === 200 ? reply({ok: true}) : reply({ok: false, reason: 'already approved'}, status),
  );
  await api.signIn('op', 'pw');
  await api.approveClaim('K7QM-3XRP');
  expect(seen[1]).toMatchObject({
    method: 'POST',
    path: '/till-claims/K7QM-3XRP/approve',
    body: {},
    auth: 'Bearer op_tok',
  });
  status = 409;
  await expect(api.approveClaim('K7QM3XRP')).rejects.toMatchObject({status: 409, operation: 'approve-claim'});
});

test('a bad code or a call before sign-in is never sent', async () => {
  const {api, seen} = harness(() => reply({ok: true}));
  await expect(api.listTills()).rejects.toThrow(/before an operator signed in/);
  await api.signIn('op', 'pw');
  await expect(api.claimInfo('NOPE')).rejects.toThrow(/not a till claim code/);
  await expect(api.approveClaim('K7QM-3XR0')).rejects.toThrow(/not a till claim code/);
  expect(seen.map(s => s.path)).toEqual(['/auth/operator/session']);
});
