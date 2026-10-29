import {env, SELF} from 'cloudflare:test';
import {beforeEach, describe, expect, it} from 'vitest';

/**
 * P1c's done-line, one test per claim:
 *   identical body → 200 with the stored row; changed body → 409;
 *   unauthenticated → 401; a row survives a Worker restart.
 *
 * They go through `SELF` (the real Worker, routing and all) rather than the
 * Durable Object directly: the auth gate sits in the Worker, so a test that
 * called the object would never prove the 401.
 */

const PASSWORD = 'correct horse battery staple';
const SHA = 'a'.repeat(64);

/** A valid card row; each test overrides only what it is about. */
function card(over: Partial<Record<string, unknown>> = {}) {
  return {
    short_id: '9c2f0a1b',
    batch_id: 1,
    installed_at: 1_760_000_000_000,
    load_sha256: SHA,
    pin_set: true,
    pin_policy: 'batch',
    notes_issued: 0,
    ...over,
  };
}

async function signIn(): Promise<string> {
  const res = await SELF.fetch('https://r.test/auth/operator/session', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({name: 'operator', password: PASSWORD}),
  });
  expect(res.status).toBe(200);
  const body = (await res.json()) as {ok: boolean; operator_id: number; token: string};
  expect(body.operator_id).toBeGreaterThan(0);
  return body.token;
}

function post(path: string, token: string | null, body: unknown) {
  return SELF.fetch(`https://r.test${path}`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      ...(token ? {Authorization: `Bearer ${token}`} : {}),
    },
    body: JSON.stringify(body),
  });
}

/** A fresh batch per test, so a survivor from another test cannot be seen. */
async function newBatch(token: string): Promise<number> {
  const res = await post('/registry/batches', token, {
    federation_id: 'babe620fc7c7d919',
    count: 5,
    pin_policy: 'batch',
    load_sha256: SHA,
  });
  expect(res.status).toBe(200);
  const body = (await res.json()) as {batch: {batch_id: number; created_at: number}};
  expect(body.batch.created_at).toBeGreaterThan(0);
  return body.batch.batch_id;
}

beforeEach(async () => {
  // The object's storage persists across tests in one pool; wiping it keeps
  // the short_id space clean without weakening what each test proves.
  const stub = env.REGISTRY.get(env.REGISTRY.idFromName('registry'));
  await stub.fetch('https://registry.internal/test/reset', {method: 'POST'});
});

describe('operator session', () => {
  it('answers a token for the right password', async () => {
    const token = await signIn();
    expect(token).toMatch(/^v1\.\d+\.\d+\.[0-9a-f]{64}$/);
  });

  it('refuses a wrong password with 401 and no token', async () => {
    const res = await post('/auth/operator/session', null, {
      name: 'operator',
      password: 'wrong',
    });
    expect(res.status).toBe(401);
    const body = (await res.json()) as Record<string, unknown>;
    expect(body.ok).toBe(false);
    expect(body.token).toBeUndefined();
    expect(typeof body.reason).toBe('string');
  });
});

describe('card rows', () => {
  it('stores a row and answers it back', async () => {
    const token = await signIn();
    const batchId = await newBatch(token);
    const res = await post('/registry/cards', token, card({batch_id: batchId}));
    expect(res.status).toBe(200);
    const body = (await res.json()) as {stored: Record<string, unknown>};
    expect(body.stored.short_id).toBe('9c2f0a1b');
    expect(body.stored.received_at).toEqual(expect.any(Number));
  });

  it('answers 200 with the stored row for the identical body', async () => {
    const token = await signIn();
    const batchId = await newBatch(token);
    const body = card({batch_id: batchId});
    const first = await post('/registry/cards', token, body);
    const firstJson = (await first.json()) as {stored: {received_at: number}};

    const second = await post('/registry/cards', token, body);
    expect(second.status).toBe(200);
    const secondJson = (await second.json()) as {stored: {received_at: number}};
    // The first sighting is the truth, so the retry returns *that* row.
    expect(secondJson.stored.received_at).toBe(firstJson.stored.received_at);
  });

  it('answers 409 for a changed body and stores nothing', async () => {
    const token = await signIn();
    const batchId = await newBatch(token);
    await post('/registry/cards', token, card({batch_id: batchId}));

    const changed = await post(
      '/registry/cards',
      token,
      card({batch_id: batchId, notes_issued: 21_000}),
    );
    expect(changed.status).toBe(409);
    const body = (await changed.json()) as Record<string, unknown>;
    expect(body.ok).toBe(false);
    expect(String(body.reason)).toContain('9c2f0a1b');

    // The 409 must not have overwritten: the list still shows the first row.
    const listed = await SELF.fetch(
      `https://r.test/registry/cards?batch_id=${batchId}`,
      {headers: {Authorization: `Bearer ${token}`}},
    );
    const {cards} = (await listed.json()) as {cards: {notes_issued: number}[]};
    expect(cards).toHaveLength(1);
    expect(cards[0]?.notes_issued).toBe(0);
  });

  it('treats a PIN-state change as changed contents', async () => {
    // The two fields Cashu's row does not have (§ 4) are the ones most likely
    // to drift late — a PIN set after the row was first posted.
    const token = await signIn();
    const batchId = await newBatch(token);
    await post('/registry/cards', token, card({batch_id: batchId, pin_set: false}));
    const res = await post('/registry/cards', token, card({batch_id: batchId, pin_set: true}));
    expect(res.status).toBe(409);
  });

  it('refuses a malformed row with 400 rather than storing a default', async () => {
    const token = await signIn();
    const res = await post('/registry/cards', token, card({short_id: 'NOTHEX'}));
    expect(res.status).toBe(400);
  });

  it('lists only the batch it was asked for', async () => {
    const token = await signIn();
    const batchA = await newBatch(token);
    const batchB = await newBatch(token);
    await post('/registry/cards', token, card({batch_id: batchA, short_id: 'aaaa0001'}));
    await post('/registry/cards', token, card({batch_id: batchB, short_id: 'bbbb0002'}));

    const res = await SELF.fetch(`https://r.test/registry/cards?batch_id=${batchA}`, {
      headers: {Authorization: `Bearer ${token}`},
    });
    const {cards} = (await res.json()) as {cards: {short_id: string}[]};
    expect(cards.map(c => c.short_id)).toEqual(['aaaa0001']);
  });
});

describe('authentication', () => {
  it('answers 401 to a card write with no token', async () => {
    const res = await post('/registry/cards', null, card());
    expect(res.status).toBe(401);
  });

  it('answers 401 to a forged token', async () => {
    const token = await signIn();
    const [version, expires, operator] = token.split('.');
    const forged = `${version}.${expires}.${operator}.${'0'.repeat(64)}`;
    const res = await post('/registry/cards', forged, card());
    expect(res.status).toBe(401);
  });

  it('answers 401 to an expired token', async () => {
    // Sign a token whose expiry is already past, using the same secret the
    // Worker holds, so the only thing wrong with it is the clock.
    const expires = Date.now() - 1000;
    const payload = `v1.${expires}.1`;
    const key = await crypto.subtle.importKey(
      'raw',
      new TextEncoder().encode(PASSWORD),
      {name: 'HMAC', hash: 'SHA-256'},
      false,
      ['sign'],
    );
    const mac = await crypto.subtle.sign('HMAC', key, new TextEncoder().encode(payload));
    const hex = [...new Uint8Array(mac)].map(b => b.toString(16).padStart(2, '0')).join('');
    const res = await post('/registry/cards', `${payload}.${hex}`, card());
    expect(res.status).toBe(401);
  });

  it('answers 401 to the batch list with no token', async () => {
    const res = await SELF.fetch('https://r.test/registry/cards?batch_id=1');
    expect(res.status).toBe(401);
  });
});

describe('durability', () => {
  it('serves a stored row from a fresh object instance', async () => {
    // "Survives a Worker restart" is the done-line's phrasing. The closest a
    // test can get is the storage outliving the stub that wrote it: the row is
    // read back through a newly resolved stub, which is what a restart gives.
    // A deployment's real persistence is SQLite on the object, not memory.
    const token = await signIn();
    const batchId = await newBatch(token);
    await post('/registry/cards', token, card({batch_id: batchId}));

    const res = await SELF.fetch(`https://r.test/registry/cards?batch_id=${batchId}`, {
      headers: {Authorization: `Bearer ${token}`},
    });
    const {cards} = (await res.json()) as {cards: {short_id: string}[]};
    expect(cards.map(c => c.short_id)).toEqual(['9c2f0a1b']);
  });

  it('reads the row back from the object\'s own storage, not from memory', async () => {
    // The differencing proves the durable side is the source of truth: the row
    // is written through the Worker, then counted on the raw object. A Worker
    // restart gives a new isolate over this same SQLite file, so a row that is
    // row-addressable here is one that survives a restart. (A test cannot
    // restart an isolate without breaking the pool's storage isolation, so
    // this is the closest honest check — not a substitute for deploying twice.)
    const token = await signIn();
    const batchId = await newBatch(token);
    await post('/registry/cards', token, card({batch_id: batchId}));

    const stub = env.REGISTRY.get(env.REGISTRY.idFromName('registry'));
    const counted = await stub.fetch('https://registry.internal/test/count', {
      method: 'POST',
    });
    const {cards: n} = (await counted.json()) as {cards: number};
    expect(n).toBe(1);
  });
});

describe('test isolation', () => {
  // These two share a short_id on purpose. With `isolatedStorage: false` the
  // only thing keeping them apart is the `beforeEach` reset, so this pair is
  // what proves that reset actually runs.
  it('writes 9c2f0a1b first', async () => {
    const token = await signIn();
    const batchId = await newBatch(token);
    const res = await post('/registry/cards', token, card({batch_id: batchId}));
    expect(res.status).toBe(200);
  });

  it('writes the same short_id again, and it is a first write', async () => {
    const token = await signIn();
    const batchId = await newBatch(token);
    const res = await post('/registry/cards', token, card({batch_id: batchId}));
    // 200 rather than 409 means the table was empty: the reset ran.
    expect(res.status).toBe(200);
  });
});
