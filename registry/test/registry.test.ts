import {env, runInDurableObject, SELF} from 'cloudflare:test';
import {beforeEach, describe, expect, it} from 'vitest';

import {applySetup, LOGIN_MAX_ATTEMPTS, parseSetup} from '../src/auth';
import {migrate, MIGRATIONS} from '../src/db';

/**
 * OPS-PLAN O2's done-line: a test for each route, including 409 on a stale
 * card version and on a changed row. They go through `SELF` (the real Worker,
 * routing and all) so the auth gate is part of what is proven; the setup
 * command is exercised on the object itself, as the constructor runs it.
 *
 * The operator `operator` is created by the bootstrap in vitest.config.ts.
 */

const PASSWORD = 'correct horse battery staple';
const SHA = 'a'.repeat(64);
const FED = 'b'.repeat(64);

/** A deterministic 32-byte key, hex, whose last 4 bytes are `tail`. */
function key(seed: number, tail = 'deadbeef'): string {
  return seed.toString(16).padStart(2, '0').repeat(28) + tail;
}

/** Eight slot keys for a card whose short id is `shortId`. */
function slots(shortId: string): string[] {
  return Array.from({length: 8}, (_, i) => key(i + 1, i === 0 ? shortId : `0000000${i}`));
}

const nonce = (n: number) => '02' + n.toString(16).padStart(2, '0').repeat(32);
const txid = (n: number) => n.toString(16).padStart(2, '0').repeat(32);

function card(over: Partial<Record<string, unknown>> = {}) {
  const shortId = (over.short_id as string | undefined) ?? '9c2f0a1b';
  return {
    short_id: shortId,
    batch_id: 1,
    installed_at: 1_760_000_000_000,
    load_sha256: SHA,
    pin_set: true,
    pin_policy: 'batch',
    notes_issued: 0,
    slot_pubkeys: slots(shortId),
    ...over,
  };
}

function call(method: string, path: string, token: string | null, body?: unknown) {
  return SELF.fetch(`https://r.test${path}`, {
    method,
    headers: {
      'Content-Type': 'application/json',
      ...(token ? {Authorization: `Bearer ${token}`} : {}),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}
const post = (path: string, token: string | null, body: unknown) => call('POST', path, token, body);
const get = (path: string, token: string | null) => call('GET', path, token);

async function signIn(name = 'operator', password = PASSWORD): Promise<string> {
  const res = await post('/auth/operator/session', null, {name, password});
  expect(res.status).toBe(200);
  const body = (await res.json()) as {operator_id: number; token: string};
  expect(body.operator_id).toBeGreaterThan(0);
  return body.token;
}

async function newBatch(token: string): Promise<number> {
  const res = await post('/registry/batches', token, {
    federation_id: FED,
    count: 5,
    pin_policy: 'batch',
    load_sha256: SHA,
  });
  expect(res.status).toBe(200);
  return ((await res.json()) as {batch: {batch_id: number}}).batch.batch_id;
}

async function newTill(token: string, name = 'front desk'): Promise<{till_id: number; token: string}> {
  const res = await post('/tills', token, {name});
  expect(res.status).toBe(200);
  return (await res.json()) as {till_id: number; token: string};
}

const stub = () => env.REGISTRY.get(env.REGISTRY.idFromName('registry'));
const hook = (path: string, body?: unknown) =>
  stub().fetch(`https://registry.internal${path}`, {
    method: 'POST',
    body: body === undefined ? undefined : JSON.stringify(body),
  });

/** A provisioned card with one live 1,024 sat note in slot 0. */
async function cardWithNote(token: string, shortId = '9c2f0a1b') {
  const batchId = await newBatch(token);
  expect((await post('/registry/cards', token, card({short_id: shortId, batch_id: batchId}))).status).toBe(200);
  await hook('/test/note', {
    short_id: shortId,
    nonce: nonce(1),
    slot: 0,
    amount_msat: 1_024_000,
    signature: 'c'.repeat(96),
  });
  return {batchId, shortId};
}

async function state(token: string, shortId: string) {
  const res = await get(`/cards/${shortId}/state`, token);
  expect(res.status).toBe(200);
  return (await res.json()) as {
    version: number;
    slots: {slot: number; pubkey: string}[];
    notes: {slot: number; nonce: string; status: string; amount_msat: number; signature: string | null}[];
    open_txs: {txid: string}[];
  };
}

/** A charge spending slot 0's note, with 24 sat change into slot 1. */
function intent(version: number, over: Partial<Record<string, unknown>> = {}) {
  return {
    version,
    txid: txid(7),
    client_ref: 'sale-1',
    tx_hex: 'ab'.repeat(100),
    inputs: [nonce(1)],
    change: [{slot: 1, amount_msat: 24_000, nonce: nonce(2), blinding_key: 'e'.repeat(64), out_idx: 1}],
    ...over,
  };
}

beforeEach(async () => {
  await hook('/test/reset');
});

describe('operator sessions', () => {
  it('answers an op_ token for the right name and password', async () => {
    expect(await signIn()).toMatch(/^op_[0-9a-f]{64}$/);
  });

  it('answers a wrong password and an unknown name alike, 401, no token', async () => {
    for (const body of [
      {name: 'operator', password: 'wrong'},
      {name: 'nobody', password: PASSWORD},
    ]) {
      const res = await post('/auth/operator/session', null, body);
      expect(res.status).toBe(401);
      expect(await res.json()).toEqual({ok: false, reason: 'the operator name or password is not correct'});
    }
  });

  it('refuses a body without both fields with 400', async () => {
    expect((await post('/auth/operator/session', null, {name: 'operator'})).status).toBe(400);
  });

  it('budgets sign-in: past the limit in a minute answers 429', async () => {
    for (let i = 0; i < LOGIN_MAX_ATTEMPTS; i += 1) {
      expect((await post('/auth/operator/session', null, {name: 'x', password: 'y'})).status).toBe(401);
    }
    const res = await post('/auth/operator/session', null, {name: 'operator', password: PASSWORD});
    expect(res.status).toBe(429);
  });

  it('answers 401 with no token, a forged one, and after sign-out', async () => {
    expect((await get('/registry/batches', null)).status).toBe(401);
    expect((await get('/registry/batches', `op_${'0'.repeat(64)}`)).status).toBe(401);
    const token = await signIn();
    expect((await get('/registry/batches', token)).status).toBe(200);
    expect((await post('/auth/operator/signout', token, {})).status).toBe(200);
    expect((await get('/registry/batches', token)).status).toBe(401);
  });

  it('expires a session after 12 hours', async () => {
    const token = await signIn();
    await runInDurableObject(stub(), async (_instance, state) => {
      state.storage.sql.exec('UPDATE sessions SET expires_at = ?', Date.now() - 1);
    });
    expect((await get('/registry/batches', token)).status).toBe(401);
  });
});

describe('the setup command', () => {
  it('refuses a malformed command without echoing it', () => {
    const raw = JSON.stringify({action: 'bootstrap', nonce: 'short', name: 'a', password: 'hunter2-secret'});
    expect(() => parseSetup(raw)).toThrow('OPERATOR_SETUP fields are invalid');
    try {
      parseSetup(raw);
    } catch (e) {
      expect(String(e)).not.toContain('hunter2');
    }
  });

  it('applies once per nonce, and a reused nonce for another command is refused', async () => {
    await runInDurableObject(stub(), async (_instance, state) => {
      const again = await applySetup(state.storage, env.OPERATOR_SETUP, Date.now());
      expect(again).toEqual({operator_id: 1, applied: false});
      const reused = JSON.stringify({
        action: 'recover',
        nonce: 'test-bootstrap-nonce-0001',
        name: 'operator',
        password: 'another long password',
      });
      await expect(applySetup(state.storage, reused, Date.now())).rejects.toThrow(/already used/);
    });
  });

  it('refuses a second bootstrap: recovery is explicit', async () => {
    await runInDurableObject(stub(), async (_instance, state) => {
      const second = JSON.stringify({
        action: 'bootstrap',
        nonce: 'second-bootstrap-0001',
        name: 'other',
        password: 'another long password',
      });
      await expect(applySetup(state.storage, second, Date.now())).rejects.toThrow(/use recover/);
    });
  });

  it('recover resets the password and revokes live sessions', async () => {
    const old = await signIn();
    await runInDurableObject(stub(), async (_instance, state) => {
      const recover = JSON.stringify({
        action: 'recover',
        nonce: `recover-${Date.now()}-0001`,
        name: 'operator',
        password: 'a brand new passphrase',
      });
      expect((await applySetup(state.storage, recover, Date.now()))?.applied).toBe(true);
    });
    expect((await get('/registry/batches', old)).status).toBe(401);
    expect((await post('/auth/operator/session', null, {name: 'operator', password: PASSWORD})).status).toBe(401);
    const fresh = await signIn('operator', 'a brand new passphrase');
    expect((await get('/registry/batches', fresh)).status).toBe(200);
    // Put the test password back for the tests after this one.
    await runInDurableObject(stub(), async (_instance, state) => {
      await applySetup(
        state.storage,
        JSON.stringify({action: 'recover', nonce: `restore-${Date.now()}-0001`, name: 'operator', password: PASSWORD}),
        Date.now(),
      );
    });
  });
});

describe('batches', () => {
  it('records the federation id and lists only this operator’s batches', async () => {
    const token = await signIn();
    const batchId = await newBatch(token);
    const res = await get('/registry/batches', token);
    const {batches} = (await res.json()) as {batches: {batch_id: number; federation_id: string}[]};
    expect(batches).toEqual([expect.objectContaining({batch_id: batchId, federation_id: FED})]);

    await hook('/test/operator', {name: 'second', password: 'another long password'});
    const other = await signIn('second', 'another long password');
    const theirs = (await (await get('/registry/batches', other)).json()) as {batches: unknown[]};
    expect(theirs.batches).toEqual([]);
  });

  it('refuses the invite text in place of the federation id', async () => {
    const token = await signIn();
    const res = await post('/registry/batches', token, {
      federation_id: 'fed11qgqyj3mfwfhksw309uekyvesvenrzdpjx93xvdf4xcex2d33xajx',
      count: 1,
      pin_policy: 'batch',
      load_sha256: SHA,
    });
    expect(res.status).toBe(400);
  });
});

describe('card rows', () => {
  it('stores a row with its slot keys and answers the identical body with the stored row', async () => {
    const token = await signIn();
    const batchId = await newBatch(token);
    const first = await post('/registry/cards', token, card({batch_id: batchId}));
    expect(first.status).toBe(200);
    const stored = ((await first.json()) as {stored: {received_at: number; slot_pubkeys: string[]}}).stored;
    expect(stored.slot_pubkeys).toHaveLength(8);
    const again = await post('/registry/cards', token, card({batch_id: batchId}));
    expect(again.status).toBe(200);
    expect(((await again.json()) as {stored: {received_at: number}}).stored.received_at).toBe(stored.received_at);
  });

  it('answers 409 for a changed body, including a changed key, and stores nothing', async () => {
    const token = await signIn();
    const batchId = await newBatch(token);
    await post('/registry/cards', token, card({batch_id: batchId}));
    const keys = slots('9c2f0a1b');
    keys[3] = key(99);
    for (const changed of [card({batch_id: batchId, pin_set: false}), card({batch_id: batchId, slot_pubkeys: keys})]) {
      expect((await post('/registry/cards', token, changed)).status).toBe(409);
    }
    const listed = (await (await get(`/registry/cards?batch_id=${batchId}`, token)).json()) as {
      cards: {pin_set: boolean; slot_pubkeys: string[]}[];
    };
    expect(listed.cards).toHaveLength(1);
    expect(listed.cards[0]!.pin_set).toBe(true);
    expect(listed.cards[0]!.slot_pubkeys[3]).toBe(slots('9c2f0a1b')[3]);
  });

  it('accepts a row without slot keys, as an older app queued it', async () => {
    const token = await signIn();
    const batchId = await newBatch(token);
    const {slot_pubkeys: _drop, ...older} = card({batch_id: batchId});
    expect((await post('/registry/cards', token, older)).status).toBe(200);
  });

  it('refuses keys whose slot 0 does not end in the short id', async () => {
    const token = await signIn();
    const batchId = await newBatch(token);
    const res = await post('/registry/cards', token, card({batch_id: batchId, slot_pubkeys: slots('11111111')}));
    expect(res.status).toBe(400);
  });

  it('refuses a malformed row with 400 rather than storing a default', async () => {
    const token = await signIn();
    const batchId = await newBatch(token);
    expect((await post('/registry/cards', token, card({batch_id: batchId, short_id: 'ABC'}))).status).toBe(400);
    expect((await post('/registry/cards', token, card({batch_id: batchId, slot_pubkeys: ['00']}))).status).toBe(400);
  });

  it('keeps operators apart: another operator’s batch is not found', async () => {
    const token = await signIn();
    const batchId = await newBatch(token);
    await post('/registry/cards', token, card({batch_id: batchId}));
    await hook('/test/operator', {name: 'second', password: 'another long password'});
    const other = await signIn('second', 'another long password');
    expect((await post('/registry/cards', other, card({batch_id: batchId, short_id: '22222222'}))).status).toBe(404);
    expect((await get(`/registry/cards?batch_id=${batchId}`, other)).status).toBe(404);
    expect((await get('/cards/9c2f0a1b/state', other)).status).toBe(404);
  });

  it('reads the row back from the object’s own storage', async () => {
    const token = await signIn();
    const batchId = await newBatch(token);
    await post('/registry/cards', token, card({batch_id: batchId}));
    const counted = await hook('/test/count');
    expect(((await counted.json()) as {cards: number}).cards).toBe(1);
  });
});

describe('tills', () => {
  it('issues a till_ token once, lists it, and revokes it', async () => {
    const token = await signIn();
    const till = await newTill(token);
    expect(till.token).toMatch(/^till_[0-9a-f]{64}$/);
    const {tills} = (await (await get('/tills', token)).json()) as {tills: {till_id: number; name: string; revoked_at: number | null}[]};
    expect(tills).toEqual([expect.objectContaining({till_id: till.till_id, name: 'front desk', revoked_at: null})]);
    expect(JSON.stringify(tills)).not.toContain(till.token);

    const {shortId} = await cardWithNote(token);
    expect((await get(`/cards/${shortId}/state`, till.token)).status).toBe(200);
    expect((await call('DELETE', `/tills/${till.till_id}`, token)).status).toBe(200);
    expect((await get(`/cards/${shortId}/state`, till.token)).status).toBe(401);
  });

  it('a till token cannot act as an operator', async () => {
    const token = await signIn();
    const till = await newTill(token);
    expect((await get('/registry/batches', till.token)).status).toBe(403);
    expect((await post('/tills', till.token, {name: 'x'})).status).toBe(403);
  });

  it('another operator cannot revoke a till, and their till cannot see this card', async () => {
    const token = await signIn();
    const mine = await newTill(token);
    const {shortId} = await cardWithNote(token);
    await hook('/test/operator', {name: 'second', password: 'another long password'});
    const other = await signIn('second', 'another long password');
    expect((await call('DELETE', `/tills/${mine.till_id}`, other)).status).toBe(404);
    const theirs = await newTill(other);
    expect((await get(`/cards/${shortId}/state`, theirs.token)).status).toBe(404);
  });
});

describe('card state', () => {
  it('starts at version 0 with the provisioned keys', async () => {
    const token = await signIn();
    const {shortId} = await cardWithNote(token);
    const s = await state(token, shortId);
    expect(s.version).toBe(0);
    expect(s.slots.map(x => x.pubkey)).toEqual(slots(shortId));
    expect(s.notes).toEqual([expect.objectContaining({slot: 0, status: 'live', amount_msat: 1_024_000})]);
  });

  it('only a till records a charge', async () => {
    const token = await signIn();
    const {shortId} = await cardWithNote(token);
    expect((await post(`/cards/${shortId}/intent`, token, intent(0))).status).toBe(403);
  });

  it('records an intent: change pending, version bumped, replay answers the same', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const {shortId} = await cardWithNote(token);
    const res = await post(`/cards/${shortId}/intent`, till.token, intent(0));
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({ok: true, version: 1, replayed: false});
    const s = await state(till.token, shortId);
    expect(s.open_txs.map(t => t.txid)).toEqual([txid(7)]);
    expect(s.notes.find(n => n.slot === 1)).toEqual(expect.objectContaining({status: 'pending', signature: null}));

    const replay = await post(`/cards/${shortId}/intent`, till.token, intent(0));
    expect(await replay.json()).toEqual({ok: true, version: 1, replayed: true});
  });

  it('answers 409 to a stale version', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const {shortId} = await cardWithNote(token);
    const res = await post(`/cards/${shortId}/intent`, till.token, intent(3));
    expect(res.status).toBe(409);
    expect(((await res.json()) as {reason: string}).reason).toMatch(/version 0, not 3/);
  });

  it('answers 409 to a different body under a recorded txid, and to a second open charge', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const {shortId} = await cardWithNote(token);
    await post(`/cards/${shortId}/intent`, till.token, intent(0));
    expect((await post(`/cards/${shortId}/intent`, till.token, intent(1, {client_ref: 'sale-2'}))).status).toBe(409);
    expect((await post(`/cards/${shortId}/intent`, till.token, intent(1, {txid: txid(8)}))).status).toBe(409);
  });

  it('answers 409 to an input that is not a live note, and to change into an occupied slot', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const {shortId} = await cardWithNote(token);
    expect((await post(`/cards/${shortId}/intent`, till.token, intent(0, {inputs: [nonce(9)]}))).status).toBe(409);
    const occupied = intent(0, {
      change: [{slot: 0, amount_msat: 1_000, nonce: nonce(3), blinding_key: 'e'.repeat(64), out_idx: 1}],
    });
    expect((await post(`/cards/${shortId}/intent`, till.token, occupied)).status).toBe(409);
  });

  it('accepted: inputs spent, change live with its signature; a repeat is a replay, a different answer 409', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const {shortId} = await cardWithNote(token);
    await post(`/cards/${shortId}/intent`, till.token, intent(0));
    expect((await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(7), accepted: true, change: []})).status).toBe(400);
    const outcome = {txid: txid(7), accepted: true, change: [{nonce: nonce(2), signature: 'f'.repeat(96)}]};
    const res = await post(`/cards/${shortId}/outcome`, till.token, outcome);
    expect(await res.json()).toEqual({ok: true, version: 2, replayed: false});
    const s = await state(till.token, shortId);
    expect(s.open_txs).toEqual([]);
    expect(s.notes).toEqual([expect.objectContaining({slot: 1, status: 'live', signature: 'f'.repeat(96), amount_msat: 24_000})]);
    expect(await (await post(`/cards/${shortId}/outcome`, till.token, outcome)).json()).toEqual({ok: true, version: 2, replayed: true});
    expect((await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(7), accepted: false})).status).toBe(409);
  });

  it('rejected: change dead, inputs still live', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const {shortId} = await cardWithNote(token);
    await post(`/cards/${shortId}/intent`, till.token, intent(0));
    expect((await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(7), accepted: false})).status).toBe(200);
    const s = await state(till.token, shortId);
    expect(s.notes).toEqual([expect.objectContaining({slot: 0, status: 'live'})]);
  });

  it('a second till finishes a charge the first one left open', async () => {
    const token = await signIn();
    const first = await newTill(token, 'first');
    const second = await newTill(token, 'second');
    const {shortId} = await cardWithNote(token);
    await post(`/cards/${shortId}/intent`, first.token, intent(0));
    const s = await state(second.token, shortId);
    expect(s.open_txs.map(t => t.txid)).toEqual([txid(7)]);
    const res = await post(`/cards/${shortId}/outcome`, second.token, {
      txid: txid(7),
      accepted: true,
      change: [{nonce: nonce(2), signature: 'f'.repeat(96)}],
    });
    expect(res.status).toBe(200);
  });

  it('takes the signed bytes once for an open charge; a repeat replays, other bytes are 409', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const {shortId} = await cardWithNote(token);
    await post(`/cards/${shortId}/intent`, till.token, intent(0));
    const signed = {txid: txid(7), tx_hex: 'cd'.repeat(120)};
    expect((await post(`/cards/${shortId}/submitted`, till.token, signed)).status).toBe(200);
    expect((await post(`/cards/${shortId}/submitted`, till.token, signed)).status).toBe(200);
    expect((await post(`/cards/${shortId}/submitted`, till.token, {...signed, tx_hex: 'ef'.repeat(120)})).status).toBe(409);
    const s = (await (await get(`/cards/${shortId}/state`, till.token)).json()) as {
      open_txs: {txid: string; signed_hex: string | null}[];
    };
    expect(s.open_txs).toEqual([expect.objectContaining({txid: txid(7), signed_hex: signed.tx_hex})]);
    expect((await post(`/cards/${shortId}/submitted`, token, signed)).status).toBe(403);
    expect((await post(`/cards/${shortId}/submitted`, till.token, {...signed, txid: txid(8)})).status).toBe(404);
  });

  it('refuses signed bytes for a charge another till already recorded rejected', async () => {
    const token = await signIn();
    const first = await newTill(token, 'first');
    const second = await newTill(token, 'second');
    const {shortId} = await cardWithNote(token);
    await post(`/cards/${shortId}/intent`, first.token, intent(0));
    const open = (await (await get(`/cards/${shortId}/state`, second.token)).json()) as {
      open_txs: {signed_hex: string | null}[];
    };
    expect(open.open_txs[0]!.signed_hex).toBeNull();
    await post(`/cards/${shortId}/outcome`, second.token, {txid: txid(7), accepted: false});
    const late = await post(`/cards/${shortId}/submitted`, first.token, {txid: txid(7), tx_hex: 'cd'.repeat(120)});
    expect(late.status).toBe(409);
  });

  it('renews a spent slot’s key, never one that backs a note, and only from the key on record', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const {shortId} = await cardWithNote(token);
    const old = slots(shortId)[0]!;
    const renew = (version: number, over: Record<string, unknown> = {}) =>
      call('PUT', `/cards/${shortId}/slots/0`, till.token, {version, old_pubkey: old, pubkey: key(50), ...over});
    expect((await renew(0)).status).toBe(409); // slot 0 backs the live note

    await post(`/cards/${shortId}/intent`, till.token, intent(0));
    await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(7), accepted: true, change: [{nonce: nonce(2), signature: 'f'.repeat(96)}]});
    expect((await renew(2, {old_pubkey: key(77)})).status).toBe(409);
    expect((await renew(1)).status).toBe(409); // stale version
    const ok = await renew(2);
    expect(await ok.json()).toEqual({ok: true, version: 3});
    expect((await state(till.token, shortId)).slots[0]!.pubkey).toBe(key(50));
    // The provisioning row still re-posts cleanly: it is compared as provisioned.
    const batchId = ((await (await get('/registry/batches', token)).json()) as {batches: {batch_id: number}[]}).batches[0]!.batch_id;
    expect((await post('/registry/cards', token, card({batch_id: batchId}))).status).toBe(200);
  });
});

describe('federation names', () => {
  it('a till reports the name; its operator reads it, another operator does not', async () => {
    const token = await signIn();
    const till = await newTill(token);
    expect((await post('/federations', till.token, {federation_id: FED, name: '  Island Bitcoin  '})).status).toBe(200);
    expect((await post('/federations', till.token, {federation_id: FED, name: 'Island Bitcoin Fed'})).status).toBe(200);
    const res = await get('/registry/federations', token);
    expect(res.status).toBe(200);
    expect(((await res.json()) as {federations: unknown[]}).federations).toEqual([
      expect.objectContaining({federation_id: FED, name: 'Island Bitcoin Fed'}),
    ]);
    await hook('/test/operator', {name: 'other', password: PASSWORD});
    const other = await signIn('other');
    expect(((await (await get('/registry/federations', other)).json()) as {federations: unknown[]}).federations).toEqual([]);
  });

  it('only a till reports a name, and only for a decoded id with a real name', async () => {
    const token = await signIn();
    const till = await newTill(token);
    expect((await post('/federations', token, {federation_id: FED, name: 'x'})).status).toBe(403);
    expect((await post('/federations', till.token, {federation_id: 'fed11abc', name: 'x'})).status).toBe(400);
    expect((await post('/federations', till.token, {federation_id: FED, name: '   '})).status).toBe(400);
    expect((await get('/registry/federations', till.token)).status).toBe(403);
  });
});

describe('migrations', () => {
  it('adopts the first deployment’s tables and rows, and gives its batches to operator 1', async () => {
    const legacy = env.REGISTRY.get(env.REGISTRY.idFromName('legacy-upgrade'));
    await runInDurableObject(legacy, async (_instance, state) => {
      const sql = state.storage.sql;
      // Back to exactly what the 2026-10-31 deployment had.
      for (const t of sql.exec("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '_cf_%'").toArray()) {
        sql.exec(`DROP TABLE IF EXISTS "${String(t.name)}"`);
      }
      sql.exec(MIGRATIONS[0]!);
      sql.exec(
        "INSERT INTO batches (federation_id, count, pin_policy, load_sha256, created_at) VALUES ('fed11old', 1, 'batch', ?, 1)",
        SHA,
      );
      sql.exec(
        "INSERT INTO cards (short_id, batch_id, installed_at, load_sha256, pin_set, pin_policy, notes_issued, received_at) VALUES ('0abcdef1', 1, 1, ?, 1, 'batch', 0, 2)",
        SHA,
      );
      migrate(state.storage);
      expect(sql.exec('SELECT version FROM schema_version').one()).toEqual({version: MIGRATIONS.length});
      expect(sql.exec('SELECT operator_id FROM batches').one()).toEqual({operator_id: 1});
      expect(sql.exec('SELECT short_id, slot_pubkeys FROM cards').one()).toEqual({short_id: '0abcdef1', slot_pubkeys: null});
      migrate(state.storage); // a second run is a no-op
      expect(sql.exec('SELECT COUNT(*) AS n FROM cards').one()).toEqual({n: 1});
    });
  });
});

describe('the Worker', () => {
  it('answers /health without a session', async () => {
    expect((await get('/health', null)).status).toBe(200);
  });

  it('never forwards a test hook', async () => {
    expect((await post('/test/reset', null, {})).status).toBe(404);
  });
});
