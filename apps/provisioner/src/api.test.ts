/**
 * The api surface and the outbox (docs/PROVISIONER-PLAN.md § 4, § 5). The pin
 * the plan names: the outbox freezes the row body at first write and re-posts
 * it byte-identical — a partial field change between retries must not become
 * a 409 the bench cannot resolve. Transport failures are ApiUnreachableError;
 * the service's own words are surfaced verbatim.
 *
 * The paths asserted here are the contract with the two services: the hosted
 * ones are § 4's table, the sidecar's are `host/sidecar/src/till.rs`'s
 * `router()`. A test that changes a string here is changing an endpoint.
 */
import {
  ApiCancelledError,
  ApiServiceError,
  ApiUnreachableError,
  LocalClient,
  RegistryClient,
  RegistryOutbox,
  type CardRegistryRow,
} from './api';

const row = (over: Partial<CardRegistryRow> = {}): CardRegistryRow => ({
  short_id: '9c2f',
  batch_id: 1,
  installed_at: 1_700_000_000,
  load_sha256: 'fe7d8ac3',
  pin_set: true,
  pin_policy: 'batch',
  notes_issued: 2000,
  ...over,
});

interface ScriptStep {
  status: number;
  json: unknown;
}

/** A fetcher that answers from a script, recording every URL and body it is handed. */
function scriptedFetcher(script: Array<(url: string, body: unknown) => ScriptStep>) {
  const state = {
    urls: [] as string[],
    bodies: [] as unknown[],
    headers: [] as unknown[],
    calls: 0,
  };
  const fetcher = async (url: string, init?: RequestInit) => {
    const step = script[Math.min(state.calls, script.length - 1)];
    state.calls++;
    const body = init?.body ? JSON.parse(String(init.body)) : undefined;
    state.urls.push(url);
    state.bodies.push(body);
    state.headers.push(init?.headers);
    const answer = step(url, body);
    return {
      ok: answer.status >= 200 && answer.status < 300,
      status: answer.status,
      json: async () => answer.json,
    } as Response;
  };
  // A getter, not a spread: `calls` changes after this returns, and a copied
  // number would freeze at 0 and make a request-count assertion vacuous.
  return {
    fetcher,
    get calls() {
      return state.calls;
    },
    get urls() {
      return state.urls;
    },
    get bodies() {
      return state.bodies;
    },
    get headers() {
      return state.headers;
    },
  };
}

/* ---------------------------------------------------------------- hosted */

test('sign-in posts to /auth/operator/session and keeps the token in memory', async () => {
  const {fetcher, urls, bodies} = scriptedFetcher([
    () => ({status: 200, json: {operator_id: 7, token: 't0k3n'}}),
  ]);
  const client = new RegistryClient({baseUrl: 'https://registry.test', fetcher});
  const session = await client.operatorSession('bench-lead', 'pw');
  expect(session).toEqual({operator_id: 7, token: 't0k3n'});
  expect(urls).toEqual(['https://registry.test/auth/operator/session']);
  expect(bodies[0]).toEqual({name: 'bench-lead', password: 'pw'});
});

test('a session without a stable operator id is refused', async () => {
  const {fetcher} = scriptedFetcher([() => ({status: 200, json: {token: 't'}})]);
  const client = new RegistryClient({baseUrl: 'https://registry.test', fetcher});
  await expect(client.operatorSession('n', 'p')).rejects.toThrow('malformed');
});

test('a failed sign-in surfaces the service reason, verbatim', async () => {
  const {fetcher} = scriptedFetcher([
    () => ({status: 401, json: {reason: 'bad-credentials'}}),
  ]);
  const client = new RegistryClient({baseUrl: 'https://registry.test', fetcher});
  const error = await client.operatorSession('n', 'p').catch(e => e);
  expect(error).toBeInstanceOf(ApiServiceError);
  expect(error.reason).toBe('bad-credentials');
  expect(error.operation).toBe('sign-in');
});

test('a network failure is ApiUnreachableError, not a generic throw', async () => {
  const fetcher = async (): Promise<Response> => {
    throw new TypeError('Network request failed');
  };
  const client = new RegistryClient({baseUrl: 'https://registry.test', fetcher});
  await expect(client.operatorSession('n', 'p')).rejects.toBeInstanceOf(
    ApiUnreachableError,
  );
});

test('the timeout fires: a hung fetcher rejects in ApiUnreachableError and is aborted', async () => {
  let signal: AbortSignal | null = null;
  const client = new RegistryClient({
    baseUrl: 'https://registry.test',
    timeoutMs: 20,
    fetcher: (_u, init) => {
      signal = init?.signal ?? null;
      return new Promise<Response>(() => undefined);
    },
  });
  await expect(client.operatorSession('n', 'p')).rejects.toBeInstanceOf(
    ApiUnreachableError,
  );
  expect(signal!.aborted).toBe(true);
});

test('a caller abort rejects with ApiCancelledError even if the transport ignores it', async () => {
  const controller = new AbortController();
  const client = new RegistryClient({
    baseUrl: 'https://registry.test',
    fetcher: () => new Promise<Response>(() => undefined),
  });
  const pending = client.operatorSession('n', 'p', controller.signal);
  const outcome = pending.catch(e => e);
  await Promise.resolve();
  controller.abort();
  expect(await outcome).toBeInstanceOf(ApiCancelledError);
});

test('batches and cards use §4 routes and the bearer token', async () => {
  const {fetcher, urls, bodies, headers} = scriptedFetcher([
    () => ({status: 200, json: {batch: {batch_id: 42}}}),
    () => ({status: 200, json: {stored: {...row(), received_at: 1}}}),
    () => ({status: 200, json: {cards: [row()]}}),
  ]);
  const client = new RegistryClient({baseUrl: 'https://registry.test', fetcher});
  const request = {
    federation_id: 'fed1',
    count: 3,
    pin_policy: 'batch' as const,
    load_sha256: 'ab'.repeat(32),
  };
  await client.createBatch('tok', request);
  await client.postCard('tok', row());
  await client.listCards('tok', 42);
  expect(urls).toEqual([
    'https://registry.test/registry/batches',
    'https://registry.test/registry/cards',
    'https://registry.test/registry/cards?batch_id=42',
  ]);
  expect(bodies[0]).toEqual(request);
  expect(bodies[1]).toEqual(row());
  expect(headers[0]).toEqual(expect.objectContaining({Authorization: 'Bearer tok'}));
});

test('the card row never carries a PIN', async () => {
  const {fetcher, bodies} = scriptedFetcher([() => ({status: 200, json: {}})]);
  const client = new RegistryClient({baseUrl: 'https://registry.test', fetcher});
  await client.postCard('tok', row({pin_set: true, pin_policy: 'perCard'}));
  expect(Object.keys(bodies[0] as object).sort()).toEqual([
    'batch_id',
    'installed_at',
    'load_sha256',
    'notes_issued',
    'pin_policy',
    'pin_set',
    'short_id',
  ]);
});

/* ---------------------------------------------------------------- sidecar */

test('the sidecar routes are the ones till.rs serves', async () => {
  const {fetcher, urls, bodies} = scriptedFetcher([
    () => ({status: 200, json: {ok: true}}),
    () => ({status: 200, json: {federation_id: 'fed1', card_relay_ok: true}}),
    () => ({status: 200, json: {ok: true, live_msat: 5, notes: [], free_slots: [0]}}),
    () => ({
      status: 200,
      json: {applet_present: true, isd_open: true, keyed_slots: [], safe_to_replace: true},
    }),
    () => ({status: 200, json: {keys: [], short_id: 'abcd', pin_set: true}}),
    () => ({
      status: 200,
      json: {client_ref: 'c1', txid: 'tx', notes: [{slot: 0, msat: 1000}], live_msat: 1000},
    }),
  ]);
  const client = new LocalClient({baseUrl: 'http://127.0.0.1:9878', fetcher});
  expect(await client.health()).toBe(true);
  expect((await client.status()).card_relay_ok).toBe(true);
  await client.balance();
  await client.precheck(false);
  await client.keySetup({pin: '123456', replace: false});
  await client.issue({amounts_msat: [1000], client_ref: 'c1'});
  expect(urls).toEqual([
    'http://127.0.0.1:9878/v1/health',
    'http://127.0.0.1:9878/v1/status',
    'http://127.0.0.1:9878/v1/card/balance',
    'http://127.0.0.1:9878/v1/cards/precheck',
    'http://127.0.0.1:9878/v1/cards/keySetup',
    'http://127.0.0.1:9878/v1/cards/issue',
  ]);
  expect(bodies[3]).toEqual({replace: false});
  expect(bodies[4]).toEqual({pin: '123456', replace: false});
  expect(bodies[5]).toEqual({amounts_msat: [1000], client_ref: 'c1'});
});

test('the sidecar keeps its own code on a failure', async () => {
  const {fetcher} = scriptedFetcher([
    () => ({status: 409, json: {ok: false, code: 'no_card', message: 'no card connected'}}),
  ]);
  const client = new LocalClient({baseUrl: 'http://127.0.0.1:9878', fetcher});
  const error = await client.balance().catch(e => e);
  expect(error).toMatchObject({
    name: 'ApiServiceError',
    status: 409,
    code: 'no_card',
    reason: 'no card connected',
    operation: 'balance',
  });
});

test('an undecided issue is the sidecar 202, code undecided', async () => {
  const {fetcher} = scriptedFetcher([
    () => ({status: 202, json: {ok: false, code: 'undecided', message: 'still resolving'}}),
  ]);
  const client = new LocalClient({baseUrl: 'http://127.0.0.1:9878', fetcher});
  await expect(
    client.issue({amounts_msat: [1000], client_ref: 'c1'}),
  ).rejects.toMatchObject({status: 202, code: 'undecided'});
});

test('a missing relay is ApiUnreachableError, not a card failure', async () => {
  const {fetcher} = scriptedFetcher([
    () => ({status: 502, json: {ok: false, code: 'relay_unreachable', message: 'no'}}),
  ]);
  const client = new LocalClient({baseUrl: 'http://127.0.0.1:9878', fetcher});
  await expect(client.balance()).rejects.toBeInstanceOf(ApiUnreachableError);
});

/* ---------------------------------------------------------------- outbox */

test('the outbox freezes the row body at first write and re-posts it byte-identical', async () => {
  const {fetcher, bodies} = scriptedFetcher([
    () => ({status: 503, json: {reason: 'upstream-unavailable'}}),
    () => ({status: 200, json: {stored: {}}}),
  ]);
  const client = new RegistryClient({baseUrl: 'https://registry.test', fetcher});
  const outbox = new RegistryOutbox(client);
  // The caller's own object moves on after the freeze (a PIN set late, a note
  // count settled); only the frozen copy may reach the wire.
  const live = row();
  outbox.add(live);
  Object.assign(live, {pin_set: false, notes_issued: 999});

  const first = await outbox.retryAll('tok');
  expect(first).toEqual({posted: 0, stillPending: 1});
  const second = await outbox.retryAll('tok');
  expect(second).toEqual({posted: 1, stillPending: 0});
  expect(bodies[1]).toEqual(bodies[0]);
  expect(bodies[0]).toEqual(row());
  expect(live).toEqual(row({pin_set: false, notes_issued: 999}));
});

test('answered rows flip to false and are never re-posted', async () => {
  let calls = 0;
  const client = new RegistryClient({
    baseUrl: 'https://registry.test',
    fetcher: async () => {
      calls++;
      return {ok: true, status: 200, json: async () => ({stored: {}})} as Response;
    },
  });
  const outbox = new RegistryOutbox(client);
  outbox.add(row({short_id: 'aaaa'}));
  outbox.add(row({short_id: 'bbbb'}));
  await outbox.retryAll('tok');
  expect(calls).toBe(2);
  await outbox.retryAll('tok');
  expect(calls).toBe(2);
  expect(outbox.pendingRows).toHaveLength(0);
  expect(outbox.all.every(entry => !entry.pending)).toBe(true);
});

test('a 409 keeps the row pending and visible, never dropped', async () => {
  const probe = scriptedFetcher([
    () => ({status: 409, json: {reason: 'short-id-exists'}}),
  ]);
  const client = new RegistryClient({
    baseUrl: 'https://registry.test',
    fetcher: probe.fetcher,
  });
  const outbox = new RegistryOutbox(client);
  outbox.add(row());
  expect(await outbox.retryAll('tok')).toEqual({posted: 0, stillPending: 1});
  expect(probe.calls).toBe(1);
  expect(outbox.pendingRows).toHaveLength(1);
});

test('a 401 aborts the pass; the caller signs out and no row posts behind it', async () => {
  const probe = scriptedFetcher([
    () => ({status: 401, json: {reason: 'no-session'}}),
    () => ({status: 200, json: {}}),
  ]);
  const client = new RegistryClient({
    baseUrl: 'https://registry.test',
    fetcher: probe.fetcher,
  });
  const outbox = new RegistryOutbox(client);
  outbox.add(row({short_id: 'aaaa'}));
  outbox.add(row({short_id: 'bbbb'}));
  await expect(outbox.retryAll('tok')).rejects.toMatchObject({status: 401});
  expect(probe.calls).toBe(1);
  expect(outbox.pendingRows).toHaveLength(2);
});
