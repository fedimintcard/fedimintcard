import {env, runDurableObjectAlarm, runInDurableObject, SELF} from 'cloudflare:test';
import {beforeEach, describe, expect, it} from 'vitest';

import {bls12_381} from '@noble/curves/bls12-381.js';

import {applySetup, LOGIN_MAX_ATTEMPTS, parseSetup} from '../src/auth';
import {decrypt, nextBackupAt} from '../src/backup';
import {migrate, MIGRATIONS} from '../src/db';
import {fromHex, sha256Hex, toHex} from '../src/http';
import {deriveManagerKeys, issueManagerKeys, parseMaster} from '../src/managerKeys';
import {messageOf} from '../src/tbs';

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

/** Slot keys (8 by default, or 32) for a card whose short id is `shortId`. */
function slots(shortId: string, count = 8): string[] {
  return Array.from({length: count}, (_, i) => key(i + 1, i === 0 ? shortId : i.toString(16).padStart(8, '0')));
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

/**
 * A test mint: one secret key per note tier, the tier keys a till reports (`tbs_pks`), and the
 * unblinded signature the federation's blind flow ends in (`message * sk`), so outcomes carry
 * signatures that verify. `tbs.test.ts` holds the port itself to the Rust code's vectors.
 */
const TIERS = [1_000, 24_000, 1_024_000, 2_048_000, 4_096_000, 8_192_000];
const tierSecret = (amount: number) => BigInt(amount) * 0x1f2e3d4c5b6an + 7n;
const TBS_PKS = Object.fromEntries(
  TIERS.map(a => [String(a), toHex(bls12_381.G2.Point.BASE.multiply(tierSecret(a)).toBytes())]),
);
function sign(nonceHex: string, amount: number): string {
  return toHex(messageOf(fromHex(nonceHex)).multiply(tierSecret(amount)).toBytes());
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

/** A till, which reports the test mint's keys for FED as a till does at startup. */
async function newTill(token: string, name = 'front desk'): Promise<{till_id: number; token: string}> {
  const res = await post('/tills', token, {name});
  expect(res.status).toBe(200);
  const till = (await res.json()) as {till_id: number; token: string};
  expect((await post('/federations', till.token, {federation_id: FED, tbs_pks: TBS_PKS})).status).toBe(200);
  return till;
}

/** The test-only identity for vitest.config.ts's BACKUP_RECIPIENT. */
const BACKUP_IDENTITY = 'AGE-SECRET-KEY-1AHS3RTK9NEEFMWRWJY6RQDF9HZZ9RQTD3UW0CNCX8YHQFSLN08LSECDQ2E';

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

  it('add makes another operator, with its own empty account, and refuses a name in use', async () => {
    await runInDurableObject(stub(), async (_instance, state) => {
      const add = JSON.stringify({action: 'add', nonce: 'add-reviewer-0000001', name: 'reviewer', password: 'a reviewer passphrase'});
      expect((await applySetup(state.storage, add, Date.now()))?.applied).toBe(true);
      // the same command again: already applied, nothing new
      expect((await applySetup(state.storage, add, Date.now()))?.applied).toBe(false);
      const taken = JSON.stringify({action: 'add', nonce: 'add-operator-000002', name: 'operator', password: 'yet another passphrase'});
      await expect(applySetup(state.storage, taken, Date.now())).rejects.toThrow(/new operator name/);
    });
    const reviewer = await signIn('reviewer', 'a reviewer passphrase');
    const res = await get('/registry/batches', reviewer);
    expect(res.status).toBe(200);
    expect(((await res.json()) as {batches: unknown[]}).batches).toEqual([]);
    // the first operator is untouched
    expect((await get('/registry/batches', await signIn())).status).toBe(200);
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

describe('till claims (docs/TILL-CLAIM.md)', () => {
  let ipSeq = 0;
  /** Each test's claims come from its own address, so the per-IP budget is the rate-limit test's alone. */
  const ip = () => `198.51.100.${(ipSeq += 1)}`;
  function start(name: unknown = 'StartOS till', from = ip()) {
    return SELF.fetch('https://r.test/till-claims', {
      method: 'POST',
      headers: {'Content-Type': 'application/json', 'CF-Connecting-IP': from},
      body: JSON.stringify({name}),
    });
  }
  type Started = {claim_id: string; code: string; poll_secret: string; expires_at: number};
  async function started(name = 'StartOS till'): Promise<Started> {
    const res = await start(name);
    expect(res.status).toBe(200);
    return (await res.json()) as Started;
  }
  const pollOf = (c: Started, secret = c.poll_secret) => post(`/till-claims/${c.claim_id}/poll`, null, {poll_secret: secret});

  it('runs the whole flow: start, read, approve, poll pending then the token once, and the claim is gone', async () => {
    const c = await started('Front counter');
    expect(c.claim_id).toMatch(/^[0-9a-f]{32}$/);
    expect(c.poll_secret).toMatch(/^[0-9a-f]{64}$/);
    expect(c.code).toMatch(/^[2-9A-HJKMNP-Z]{4}-[2-9A-HJKMNP-Z]{4}$/);
    expect(c.expires_at - Date.now()).toBeGreaterThan(9 * 60 * 1000);

    expect(await (await pollOf(c)).json()).toEqual({ok: true, status: 'pending'});
    const token = await signIn();
    const read = await get(`/till-claims/${c.code}`, token);
    expect(read.status).toBe(200);
    expect(await read.json()).toEqual({ok: true, name: 'Front counter', created_at: expect.any(Number), expires_at: c.expires_at, status: 'pending'});
    expect((await post(`/till-claims/${c.code}/approve`, token, {})).status).toBe(200);
    expect(((await (await get(`/till-claims/${c.code}`, token)).json()) as {status: string}).status).toBe('approved');

    const res = await pollOf(c);
    expect(res.status).toBe(200);
    const done = (await res.json()) as {status: string; till_id: number; token: string};
    expect(done).toEqual(expect.objectContaining({status: 'approved', till_id: expect.any(Number)}));
    expect(done.token).toMatch(/^till_[0-9a-f]{64}$/);
    // Once: the claim is gone for the till and for the operator.
    expect((await pollOf(c)).status).toBe(404);
    expect((await get(`/till-claims/${c.code}`, token)).status).toBe(404);

    // The token is a till's like any other, listed under the approving operator.
    const {tills} = (await (await get('/tills', token)).json()) as {tills: {till_id: number; name: string}[]};
    expect(tills).toEqual([expect.objectContaining({till_id: done.till_id, name: 'Front counter'})]);
    expect((await post('/federations', done.token, {federation_id: FED, tbs_pks: TBS_PKS})).status).toBe(200);
    const {shortId} = await cardWithNote(token);
    expect((await get(`/cards/${shortId}/state`, done.token)).status).toBe(200);
  });

  it('takes the code in any case, with or without the dash', async () => {
    const token = await signIn();
    const c = await started();
    const bare = c.code.replace('-', '');
    expect((await get(`/till-claims/${bare.toLowerCase()}`, token)).status).toBe(200);
    expect((await get(`/till-claims/${c.code.toLowerCase()}`, token)).status).toBe(200);
    expect((await post(`/till-claims/${bare}/approve`, token, {})).status).toBe(200);
    expect(((await (await pollOf(c)).json()) as {status: string}).status).toBe('approved');
  });

  it('answers a wrong poll secret, an unknown claim and a malformed one 404, alike', async () => {
    const c = await started();
    for (const res of [
      await pollOf(c, 'f'.repeat(64)),
      await post(`/till-claims/${'0'.repeat(32)}/poll`, null, {poll_secret: c.poll_secret}),
      await pollOf(c, 'nope'),
    ]) {
      expect(res.status).toBe(404);
      expect(await res.json()).toEqual({ok: false, reason: 'no claim'});
    }
    const token = await signIn();
    expect((await get('/till-claims/AAAA-AAAA', token)).status).toBe(404);
    expect((await get('/till-claims/0OIL-1234', token)).status).toBe(404);
    expect((await post('/till-claims/AAAA-AAAA/approve', token, {})).status).toBe(404);
    // The right secret still works after the wrong ones.
    expect((await pollOf(c)).status).toBe(200);
  });

  it('answers an expired claim 404 everywhere, and sweeps it', async () => {
    const token = await signIn();
    const c = await started();
    await runInDurableObject(stub(), async (_i, state) => {
      state.storage.sql.exec('UPDATE till_claims SET expires_at = ? WHERE claim_id = ?', Date.now() - 1, c.claim_id);
    });
    expect((await get(`/till-claims/${c.code}`, token)).status).toBe(404);
    expect((await post(`/till-claims/${c.code}/approve`, token, {})).status).toBe(404);
    expect((await pollOf(c)).status).toBe(404);
    await runInDurableObject(stub(), async (_i, state) => {
      expect(state.storage.sql.exec('SELECT COUNT(*) AS n FROM till_claims').one()).toEqual({n: 0});
    });
  });

  it('keeps an approved claim for a till that polls after the pending window, then lets it go', async () => {
    const token = await signIn();
    const late = await started('Asleep at approval');
    const never = await started('Never comes back');
    for (const c of [late, never]) expect((await post(`/till-claims/${c.code}/approve`, token, {})).status).toBe(200);
    const sevenDays = 7 * 24 * 60 * 60 * 1000;
    await runInDurableObject(stub(), async (_i, state) => {
      const row = state.storage.sql.exec('SELECT expires_at, approved_at FROM till_claims WHERE claim_id = ?', late.claim_id).one();
      expect(Number(row.expires_at) - Number(row.approved_at)).toBe(sevenDays);
      // Past both claims' first 10 minutes, and past the second one's approved window.
      state.storage.sql.exec('UPDATE till_claims SET created_at = created_at - ?', 60 * 60 * 1000);
      state.storage.sql.exec('UPDATE till_claims SET expires_at = ? WHERE claim_id = ?', Date.now() - 1, never.claim_id);
    });
    const res = await pollOf(late);
    expect(res.status).toBe(200);
    expect(((await res.json()) as {status: string}).status).toBe('approved');
    expect((await pollOf(never)).status).toBe(404);
  });

  it('answers a second approve 409; another operator neither sees nor approves an approved claim', async () => {
    const token = await signIn();
    const c = await started();
    await hook('/test/operator', {name: 'second', password: 'another long password'});
    const other = await signIn('second', 'another long password');
    expect((await post(`/till-claims/${c.code}/approve`, token, {})).status).toBe(200);
    const again = await post(`/till-claims/${c.code}/approve`, token, {});
    expect(again.status).toBe(409);
    expect((await get(`/till-claims/${c.code}`, other)).status).toBe(404);
    expect((await post(`/till-claims/${c.code}/approve`, other, {})).status).toBe(404);
    // The till lands under the operator who approved it.
    const done = (await (await pollOf(c)).json()) as {till_id: number};
    const theirs = (await (await get('/tills', other)).json()) as {tills: unknown[]};
    expect(theirs.tills).toEqual([]);
    const mine = (await (await get('/tills', token)).json()) as {tills: {till_id: number}[]};
    expect(mine.tills.map(t => t.till_id)).toEqual([done.till_id]);
  });

  it('is for operators only: no token is 401, a till token is 403', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const c = await started();
    expect((await get(`/till-claims/${c.code}`, null)).status).toBe(401);
    expect((await post(`/till-claims/${c.code}/approve`, till.token, {})).status).toBe(403);
    expect((await get(`/till-claims/${c.code}`, till.token)).status).toBe(403);
  });

  it('refuses a name that is empty, too long or carries control characters', async () => {
    for (const name of ['', '   ', 'x'.repeat(65), 'bad\nname', 7, null]) {
      expect((await start(name)).status).toBe(400);
    }
    expect((await start('Café till ☕')).status).toBe(200);
  });

  it('rate limits starts per IP (429) while another address still gets through', async () => {
    const from = '203.0.113.9';
    for (let i = 0; i < 10; i += 1) expect((await start('t', from)).status).toBe(200);
    const res = await start('t', from);
    expect(res.status).toBe(429);
    expect(((await res.json()) as {ok: boolean}).ok).toBe(false);
    expect((await start('t', '203.0.113.10')).status).toBe(200);
  });

  it('caps pending claims across the registry (429); approved and expired ones stop counting', async () => {
    await runInDurableObject(stub(), async (_i, state) => {
      const sql = state.storage.sql;
      for (let i = 0; i < 200; i += 1) {
        sql.exec(
          `INSERT INTO till_claims (claim_id, code, name, poll_secret_hash, created_at, expires_at) VALUES (?, ?, 'x', ?, ?, ?)`,
          i.toString(16).padStart(32, '0'), `T${i.toString().padStart(7, '0')}`, 'a'.repeat(64), Date.now(), Date.now() + 600_000,
        );
      }
    });
    expect((await start()).status).toBe(429);
    // An approved claim waits for its till for days; it does not hold a pending place.
    await runInDurableObject(stub(), async (_i, state) => {
      state.storage.sql.exec('UPDATE till_claims SET approved_by = 1, approved_at = ? WHERE claim_id = ?', Date.now(), '0'.repeat(32));
    });
    expect((await start()).status).toBe(200);
    expect((await start()).status).toBe(429);
    await runInDurableObject(stub(), async (_i, state) => {
      state.storage.sql.exec('UPDATE till_claims SET expires_at = ? WHERE claim_id < ?', Date.now() - 1, '0'.repeat(30) + '10');
    });
    expect((await start()).status).toBe(200);
  });

  it('keeps claims and their secrets out of the backup', async () => {
    const token = await signIn();
    const pending = await started('pending one');
    const approved = await started('approved one');
    expect((await post(`/till-claims/${approved.code}/approve`, token, {})).status).toBe(200);
    const res = await stub().fetch('https://registry.internal/internal/backup', {method: 'POST'});
    expect(res.status).toBe(200);
    const {key} = (await res.json()) as {key: string};
    const snap = await decrypt(await (await env.BACKUPS!.get(key))!.text(), BACKUP_IDENTITY);
    expect(Object.keys(snap.tables)).not.toContain('till_claims');
    expect(Object.keys(snap.tables)).not.toContain('till_claim_budget');
    const text = JSON.stringify(snap);
    for (const c of [pending, approved]) {
      expect(text).not.toContain(c.claim_id);
      expect(text).not.toContain(c.poll_secret);
      expect(text).not.toContain(c.code);
      expect(text).not.toContain(await sha256Hex(c.poll_secret));
    }
    expect(text).not.toContain('poll_secret_hash');
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
    const outcome = {txid: txid(7), accepted: true, change: [{nonce: nonce(2), signature: sign(nonce(2), 24_000)}]};
    const res = await post(`/cards/${shortId}/outcome`, till.token, outcome);
    expect(await res.json()).toEqual({ok: true, version: 2, replayed: false});
    const s = await state(till.token, shortId);
    expect(s.open_txs).toEqual([]);
    expect(s.notes).toEqual([expect.objectContaining({slot: 1, status: 'live', signature: sign(nonce(2), 24_000), amount_msat: 24_000})]);
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
      change: [{nonce: nonce(2), signature: sign(nonce(2), 24_000)}],
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
    await hook('/test/note', {short_id: shortId, nonce: nonce(5), slot: 2, amount_msat: 2_048_000, signature: 'c'.repeat(96)});
    const old = slots(shortId)[2]!;
    const renew = (version: number, over: Record<string, unknown> = {}) =>
      call('PUT', `/cards/${shortId}/slots/2`, till.token, {version, old_pubkey: old, pubkey: key(50), ...over});
    expect((await renew(0)).status).toBe(409); // slot 2 backs the live note

    const spend = intent(0, {
      inputs: [nonce(5)],
      change: [{slot: 3, amount_msat: 24_000, nonce: nonce(6), blinding_key: 'e'.repeat(64), out_idx: 1}],
    });
    await post(`/cards/${shortId}/intent`, till.token, spend);
    await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(7), accepted: true, change: [{nonce: nonce(6), signature: sign(nonce(6), 24_000)}]});
    expect((await renew(2, {old_pubkey: key(77)})).status).toBe(409);
    expect((await renew(1)).status).toBe(409); // stale version
    const ok = await renew(2);
    expect(await ok.json()).toEqual({ok: true, version: 3});
    expect((await state(till.token, shortId)).slots[2]!.pubkey).toBe(key(50));
    // The provisioning row still re-posts cleanly: it is compared as provisioned.
    const batchId = ((await (await get('/registry/batches', token)).json()) as {batches: {batch_id: number}[]}).batches[0]!.batch_id;
    expect((await post('/registry/cards', token, card({batch_id: batchId}))).status).toBe(200);
  });
});

describe('O3: loads, burned keys and slot 0', () => {
  /** Slot `n`'s key as a note nonce (even parity), the way the sidecar writes it. */
  const keyNonce = (shortId: string, n: number) => '02' + slots(shortId)[n]!;

  it('takes a load (no inputs, no tx_hex) from a till and finishes it like a charge', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const batchId = await newBatch(token);
    const shortId = '0a0b0c0d';
    await post('/registry/cards', token, card({short_id: shortId, batch_id: batchId}));
    const load = {
      version: 0,
      txid: txid(9),
      client_ref: 'issue-1',
      inputs: [],
      change: [{slot: 1, amount_msat: 4_096_000, nonce: keyNonce(shortId, 1), blinding_key: 'e'.repeat(64), out_idx: 0}],
    };
    const res = await post(`/cards/${shortId}/intent`, till.token, load);
    expect(await res.json()).toEqual({ok: true, version: 1, replayed: false});
    expect(await (await post(`/cards/${shortId}/intent`, till.token, load)).json()).toEqual({ok: true, version: 1, replayed: true});
    const sig = {txid: txid(9), accepted: true, change: [{nonce: keyNonce(shortId, 1), signature: sign(keyNonce(shortId, 1), 4_096_000)}]};
    expect((await post(`/cards/${shortId}/outcome`, till.token, sig)).status).toBe(200);
    const s = await state(till.token, shortId);
    expect(s.notes).toEqual([expect.objectContaining({slot: 1, status: 'live', amount_msat: 4_096_000})]);
    expect(s.open_txs).toEqual([]);
  });

  it('refuses a load with no notes, and a charge without tx_hex', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const {shortId} = await cardWithNote(token);
    expect((await post(`/cards/${shortId}/intent`, till.token, intent(0, {inputs: [], change: []}))).status).toBe(400);
    expect((await post(`/cards/${shortId}/intent`, till.token, intent(0, {tx_hex: undefined}))).status).toBe(400);
  });

  it('reports a spent note while its key is unrenewed, and refuses change under that key', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const batchId = await newBatch(token);
    const shortId = '0a0b0c0e';
    await post('/registry/cards', token, card({short_id: shortId, batch_id: batchId}));
    await hook('/test/note', {short_id: shortId, nonce: keyNonce(shortId, 2), slot: 2, amount_msat: 2_048_000, signature: 'c'.repeat(96)});
    const spend = intent(0, {
      inputs: [keyNonce(shortId, 2)],
      change: [{slot: 3, amount_msat: 24_000, nonce: keyNonce(shortId, 3), blinding_key: 'e'.repeat(64), out_idx: 1}],
    });
    await post(`/cards/${shortId}/intent`, till.token, spend);
    await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(7), accepted: true, change: [{nonce: keyNonce(shortId, 3), signature: sign(keyNonce(shortId, 3), 24_000)}]});
    const s = await state(till.token, shortId);
    expect(s.notes.map(n => [n.slot, n.status])).toEqual([[2, 'spent'], [3, 'live']]);
    // Change back into slot 2 under its burned key is refused, not a 500.
    const reuse = intent(2, {
      txid: txid(8),
      inputs: [keyNonce(shortId, 3)],
      change: [{slot: 2, amount_msat: 1_000, nonce: keyNonce(shortId, 2), blinding_key: 'e'.repeat(64), out_idx: 1}],
    });
    const res = await post(`/cards/${shortId}/intent`, till.token, reuse);
    expect(res.status).toBe(409);
    expect(((await res.json()) as {reason: string}).reason).toMatch(/renew slot 2/);
    // Once renewed, the old key's history is no longer reported.
    const renewed = await call('PUT', `/cards/${shortId}/slots/2`, till.token, {version: 2, old_pubkey: slots(shortId)[2], pubkey: key(60)});
    expect(renewed.status).toBe(200);
    expect((await state(till.token, shortId)).notes.map(n => n.slot)).toEqual([3]);
  });

  it('never renews slot 0: its key is the short id', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const batchId = await newBatch(token);
    const shortId = '0a0b0c0f';
    await post('/registry/cards', token, card({short_id: shortId, batch_id: batchId}));
    const res = await call('PUT', `/cards/${shortId}/slots/0`, till.token, {version: 0, old_pubkey: slots(shortId)[0], pubkey: key(61)});
    expect(res.status).toBe(409);
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

describe('O4: mint keys, and every live note verified', () => {
  it('takes a federation’s tier keys once; the same set replays, another set is 409', async () => {
    const token = await signIn();
    const till = await newTill(token); // posts TBS_PKS for FED
    expect((await post('/federations', till.token, {federation_id: FED, tbs_pks: TBS_PKS})).status).toBe(200);
    const other = {...TBS_PKS, '1000': TBS_PKS['24000']};
    expect((await post('/federations', till.token, {federation_id: FED, tbs_pks: other})).status).toBe(409);
    // Keys without a name are fine; neither is not; a key that is not a G2 point is refused.
    const fed2 = 'c'.repeat(64);
    expect((await post('/federations', till.token, {federation_id: fed2})).status).toBe(400);
    expect((await post('/federations', till.token, {federation_id: fed2, tbs_pks: {'1000': 'ab'.repeat(96)}})).status).toBe(400);
    expect((await post('/federations', till.token, {federation_id: fed2, tbs_pks: {'0': TBS_PKS['1000']}})).status).toBe(400);
    expect((await post('/federations', token, {federation_id: fed2, tbs_pks: TBS_PKS})).status).toBe(403);
    // Keys alone make no name row.
    expect(((await (await get('/registry/federations', token)).json()) as {federations: unknown[]}).federations).toEqual([]);
  });

  it('refuses a till’s outcome whose change signature the federation did not make, and changes nothing', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const {shortId} = await cardWithNote(token);
    await post(`/cards/${shortId}/intent`, till.token, intent(0));
    for (const signature of [
      'c'.repeat(96), // not a point
      sign(nonce(3), 24_000), // a real signature, on another nonce
      sign(nonce(2), 1_000), // the right nonce, another tier's key
    ]) {
      const res = await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(7), accepted: true, change: [{nonce: nonce(2), signature}]});
      expect(res.status).toBe(422);
    }
    const s = await state(till.token, shortId);
    expect(s.open_txs.map(t => t.txid)).toEqual([txid(7)]);
    expect(s.notes.map(n => [n.slot, n.status])).toEqual([[0, 'live'], [1, 'pending']]);
    expect((await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(7), accepted: true, change: [{nonce: nonce(2), signature: sign(nonce(2), 24_000)}]})).status).toBe(200);
  });

  it('refuses an accepted outcome while no till has reported the federation’s keys', async () => {
    const token = await signIn();
    const res = await post('/tills', token, {name: 'keyless'});
    const till = (await res.json()) as {token: string};
    const {shortId} = await cardWithNote(token);
    await post(`/cards/${shortId}/intent`, till.token, intent(0));
    const out = await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(7), accepted: true, change: [{nonce: nonce(2), signature: sign(nonce(2), 24_000)}]});
    expect(out.status).toBe(422);
    expect(((await out.json()) as {reason: string}).reason).toMatch(/mint keys/);
    // A rejection needs no keys.
    expect((await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(7), accepted: false})).status).toBe(200);
  });
});

describe('O4: holder loading', () => {
  const keyNonce = (shortId: string, n: number) => '02' + slots(shortId)[n]!;
  const BK = 'e'.repeat(64);

  /** A provisioned card with no notes, and a till of its operator that reported the mint keys. */
  async function freshCard(shortId = '1a2b3c4d') {
    const token = await signIn();
    const till = await newTill(token);
    const batchId = await newBatch(token);
    expect((await post('/registry/cards', token, card({short_id: shortId, batch_id: batchId}))).status).toBe(200);
    return {token, till, shortId};
  }

  function load(shortId: string, version: number, notes: [slot: number, amount: number][], t = txid(20)) {
    return {
      version,
      txid: t,
      notes: notes.map(([slot, amount], i) => ({slot, amount_msat: amount, nonce: keyNonce(shortId, slot), blinding_key: BK, out_idx: i})),
    };
  }
  function signed(shortId: string, notes: [slot: number, amount: number][], t = txid(20)) {
    return {txid: t, notes: notes.map(([slot, amount]) => ({nonce: keyNonce(shortId, slot), signature: sign(keyNonce(shortId, slot), amount)}))};
  }
  const slotsOf = async (shortId: string) => {
    const res = await get(`/cards/${shortId}/slots`, null);
    expect(res.status).toBe(200);
    return (await res.json()) as {version: number; federation_id: string; free: {slot: number; pubkey: string}[]};
  };

  it('answers the free slot keys without a token, and nothing about balances', async () => {
    const {shortId} = await freshCard();
    await hook('/test/note', {short_id: shortId, nonce: keyNonce(shortId, 2), slot: 2, amount_msat: 2_048_000, signature: 'c'.repeat(96)});
    const res = await get(`/cards/${shortId}/slots`, null);
    expect(res.status).toBe(200);
    const body = (await res.json()) as Record<string, unknown>;
    expect(Object.keys(body).sort()).toEqual(['federation_id', 'free', 'ok', 'slot_count', 'unkeyed_slots', 'version']);
    expect(body.slot_count).toBe(8);
    expect(body.unkeyed_slots).toEqual([]);
    expect(body.federation_id).toBe(FED);
    expect((body.free as {slot: number}[]).map(f => f.slot)).toEqual([0, 1, 3, 4, 5, 6, 7]);
    expect(JSON.stringify(body)).not.toMatch(/amount|signature|notes|2048000/);
  });

  it('answers 404 for an unknown card, and 400 for a malformed short id', async () => {
    await signIn();
    expect((await get('/cards/99999999/slots', null)).status).toBe(404);
    expect((await post('/cards/99999999/load', null, load('99999999', 0, [[1, 1_000]]))).status).toBe(404);
    expect((await post('/cards/99999999/load/outcome', null, signed('99999999', [[1, 1_000]]))).status).toBe(404);
    expect((await get('/cards/XYZ/slots', null)).status).toBe(400);
  });

  it('loads from a holder’s wallet: pending, then live once every signature verifies', async () => {
    const {till, shortId} = await freshCard();
    const notes: [number, number][] = [[1, 4_096_000], [3, 1_024_000]];
    const res = await post(`/cards/${shortId}/load`, null, load(shortId, 0, notes));
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual(expect.objectContaining({ok: true, version: 1, replayed: false}));
    expect(await (await post(`/cards/${shortId}/load`, null, load(shortId, 0, notes))).json()).toEqual(expect.objectContaining({version: 1, replayed: true}));
    expect((await slotsOf(shortId)).free.map(f => f.slot)).toEqual([0, 2, 4, 5, 6, 7]);

    // A till sees the slots held, but neither the txid nor the blinding key, so it never tries
    // to collect a holder's notes; and it cannot put change there.
    const pending = await state(till.token, shortId);
    expect(pending.notes).toEqual([
      expect.objectContaining({slot: 1, status: 'pending', txid: null, blinding_key: null}),
      expect.objectContaining({slot: 3, status: 'pending', txid: null, blinding_key: null}),
    ]);

    const done = await post(`/cards/${shortId}/load/outcome`, null, signed(shortId, notes));
    expect(await done.json()).toEqual({ok: true, version: 2, replayed: false});
    const s = await state(till.token, shortId);
    expect(s.notes.map(n => [n.slot, n.status, n.amount_msat])).toEqual([[1, 'live', 4_096_000], [3, 'live', 1_024_000]]);
    expect(s.notes[0]!.signature).toBe(sign(keyNonce(shortId, 1), 4_096_000));
    expect(await (await post(`/cards/${shortId}/load/outcome`, null, signed(shortId, notes))).json()).toEqual({ok: true, version: 2, replayed: true});
    expect((await post(`/cards/${shortId}/load/outcome`, null, {txid: txid(20), accepted: false})).status).toBe(409);
    // The loaded notes are spendable at a till like any other.
    const charge = intent(2, {inputs: [keyNonce(shortId, 1)], change: [{slot: 4, amount_msat: 24_000, nonce: keyNonce(shortId, 4), blinding_key: BK, out_idx: 1}]});
    expect((await post(`/cards/${shortId}/intent`, till.token, charge)).status).toBe(200);
  });

  it('refuses a forged signature (422) and changes nothing; the real ones still finish it', async () => {
    const {till, shortId} = await freshCard();
    const notes: [number, number][] = [[1, 4_096_000], [2, 1_000]];
    await post(`/cards/${shortId}/load`, null, load(shortId, 0, notes));
    const good = signed(shortId, notes);
    const forgeries = [
      // A flipped bit in one signature.
      {...good, notes: [good.notes[0]!, {...good.notes[1]!, signature: good.notes[1]!.signature.slice(0, 94) + 'ff'}]},
      // Each note's signature made under another tier's key: claims 4,096 sat for a 1-sat note.
      {...good, notes: [{...good.notes[0]!, signature: sign(keyNonce(shortId, 1), 1_000)}, good.notes[1]!]},
      // Real signatures, swapped between the two notes.
      {...good, notes: [{...good.notes[0]!, signature: good.notes[1]!.signature}, {...good.notes[1]!, signature: good.notes[0]!.signature}]},
    ];
    for (const forged of forgeries) {
      const res = await post(`/cards/${shortId}/load/outcome`, null, forged);
      expect(res.status).toBe(422);
    }
    const s = await state(till.token, shortId);
    expect(s.version).toBe(1);
    expect(s.notes.map(n => n.status)).toEqual(['pending', 'pending']);
    expect((await post(`/cards/${shortId}/load/outcome`, null, good)).status).toBe(200);
  });

  it('a load the federation refused frees its slots, keys unburned', async () => {
    const {till, shortId} = await freshCard();
    await post(`/cards/${shortId}/load`, null, load(shortId, 0, [[1, 1_000]]));
    expect(await (await post(`/cards/${shortId}/load/outcome`, null, {txid: txid(20), accepted: false})).json()).toEqual({ok: true, version: 2, replayed: false});
    expect((await state(till.token, shortId)).notes).toEqual([]);
    expect((await slotsOf(shortId)).free.map(f => f.slot)).toContain(1);
    // The same key takes the next load.
    expect((await post(`/cards/${shortId}/load`, null, load(shortId, 2, [[1, 1_000]], txid(21)))).status).toBe(200);
  });

  it('checks the version, the slots, the keys and the tiers', async () => {
    const {shortId} = await freshCard();
    await hook('/test/note', {short_id: shortId, nonce: keyNonce(shortId, 2), slot: 2, amount_msat: 2_048_000, signature: 'c'.repeat(96)});
    expect((await post(`/cards/${shortId}/load`, null, load(shortId, 5, [[1, 1_000]]))).status).toBe(409); // stale
    expect((await post(`/cards/${shortId}/load`, null, load(shortId, 0, [[2, 1_000]]))).status).toBe(409); // held
    const wrongKey = load(shortId, 0, [[1, 1_000]]);
    wrongKey.notes[0]!.nonce = keyNonce(shortId, 3);
    expect((await post(`/cards/${shortId}/load`, null, wrongKey)).status).toBe(409); // not slot 1's key
    expect((await post(`/cards/${shortId}/load`, null, load(shortId, 0, [[1, 3_000]]))).status).toBe(422); // no such tier
    expect((await post(`/cards/${shortId}/load`, null, load(shortId, 0, [[1, 1_000], [1, 1_000]]))).status).toBe(400);
    expect((await post(`/cards/${shortId}/load`, null, {...load(shortId, 0, []), notes: []})).status).toBe(400);
    expect((await slotsOf(shortId)).version).toBe(0);
  });

  it('allows one open load per card', async () => {
    const {shortId} = await freshCard();
    expect((await post(`/cards/${shortId}/load`, null, load(shortId, 0, [[1, 1_000]]))).status).toBe(200);
    const second = await post(`/cards/${shortId}/load`, null, load(shortId, 1, [[3, 1_000]], txid(21)));
    expect(second.status).toBe(409);
    expect(((await second.json()) as {reason: string}).reason).toMatch(/in flight/);
  });

  it('expires an open load: its slots are free again and another load can take them', async () => {
    const {till, shortId} = await freshCard();
    await post(`/cards/${shortId}/load`, null, load(shortId, 0, [[1, 1_000], [3, 24_000]]));
    await runInDurableObject(stub(), async (_i, st) => {
      st.storage.sql.exec('UPDATE holder_loads SET expires_at = ?', Date.now() - 1);
    });
    expect((await slotsOf(shortId)).free.map(f => f.slot)).toEqual([0, 1, 2, 3, 4, 5, 6, 7]);
    expect((await state(till.token, shortId)).notes).toEqual([]);
    expect((await post(`/cards/${shortId}/load`, null, load(shortId, 1, [[1, 1_000]], txid(21)))).status).toBe(200);
    // The expired load's wallet comes back late: slot 1 was used since, so it cannot finish.
    expect((await post(`/cards/${shortId}/load/outcome`, null, signed(shortId, [[1, 1_000], [3, 24_000]]))).status).toBe(409);
  });

  it('finishes an expired load late while its slots are untouched', async () => {
    const {till, shortId} = await freshCard();
    await post(`/cards/${shortId}/load`, null, load(shortId, 0, [[5, 8_192_000]]));
    await runInDurableObject(stub(), async (_i, st) => {
      st.storage.sql.exec('UPDATE holder_loads SET expires_at = ?', Date.now() - 1);
    });
    expect((await slotsOf(shortId)).free.map(f => f.slot)).toContain(5);
    expect((await post(`/cards/${shortId}/load/outcome`, null, signed(shortId, [[5, 8_192_000]]))).status).toBe(200);
    expect((await state(till.token, shortId)).notes).toEqual([expect.objectContaining({slot: 5, status: 'live', amount_msat: 8_192_000})]);
  });

  it('budgets holder writes per card: past the limit in an hour answers 429', async () => {
    const {shortId} = await freshCard();
    for (let i = 0; i < 12; i += 1) {
      expect((await post(`/cards/${shortId}/load`, null, load(shortId, 99, [[1, 1_000]], txid(30 + i)))).status).toBe(409);
    }
    expect((await post(`/cards/${shortId}/load`, null, load(shortId, 0, [[1, 1_000]]))).status).toBe(429);
    expect((await get(`/cards/${shortId}/slots`, null)).status).toBe(200); // reading is not budgeted
  });
});

describe('backups', () => {
  const IDENTITY = BACKUP_IDENTITY;

  it('status is 503 before the first backup', async () => {
    const token = await signIn();
    const res = await get('/registry/backup-status', token);
    expect(res.status).toBe(503);
    expect(await res.json()).toEqual(expect.objectContaining({configured: true, last_success_at: null, stale: true}));
  });

  it('writes an encrypted snapshot to R2 that decrypts to every card row and no secret', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const {shortId} = await cardWithNote(token);
    await post('/federations', till.token, {federation_id: FED, name: 'Island Bitcoin'});
    const res = await stub().fetch('https://registry.internal/internal/backup', {method: 'POST'});
    expect(res.status).toBe(200);
    const {key} = (await res.json()) as {key: string};
    const object = await env.BACKUPS!.get(key);
    const armored = await object!.text();
    expect(armored).toContain('BEGIN AGE ENCRYPTED FILE');
    expect(armored).not.toContain(shortId);
    const snap = await decrypt(armored, IDENTITY);
    expect(snap.schema_version).toBe(MIGRATIONS.length);
    expect(snap.tables.cards!.map(c => c.short_id)).toEqual([shortId]);
    expect(snap.tables.notes).toEqual([expect.objectContaining({short_id: shortId, status: 'live'})]);
    expect(snap.tables.federations).toEqual([expect.objectContaining({name: 'Island Bitcoin'})]);
    const text = JSON.stringify(snap);
    expect(text).not.toContain('password_hash');
    expect(text).not.toContain('token_hash');
    expect(text).not.toContain(till.token);
    const status = await get('/registry/backup-status', token);
    expect(status.status).toBe(200);
    expect(await status.json()).toEqual(expect.objectContaining({last_key: key, stale: false, last_error: null}));
  });

  it('the object arms its own nightly alarm, and the alarm backs up and re-arms', async () => {
    const token = await signIn();
    expect(await runDurableObjectAlarm(stub())).toBe(true);
    const status = (await (await get('/registry/backup-status', token)).json()) as {last_success_at: number; stale: boolean};
    expect(status.stale).toBe(false);
    await runInDurableObject(stub(), async (_i, state) => {
      expect(await state.storage.getAlarm()).toBe(nextBackupAt(status.last_success_at));
    });
  });

  it('schedules for the next 03:17 UTC', () => {
    expect(new Date(nextBackupAt(Date.UTC(2026, 10, 4, 1, 0))).toISOString()).toBe('2026-11-04T03:17:00.000Z');
    expect(new Date(nextBackupAt(Date.UTC(2026, 10, 4, 3, 17))).toISOString()).toBe('2026-11-05T03:17:00.000Z');
  });

  it('the Worker never forwards /internal/', async () => {
    expect((await SELF.fetch('https://r.test/internal/backup', {method: 'POST'})).status).toBe(404);
  });

  it('a till cannot read backup status', async () => {
    const token = await signIn();
    const till = await newTill(token);
    expect((await get('/registry/backup-status', till.token)).status).toBe(403);
  });
});

describe('O5: per-card manager keys', () => {
  const CHIP = 'cplc-v1:4790d3213041093363205547';
  const OTHER_CHIP = 'cplc-v1:4790d3213180072976207515';
  /** vitest.config.ts's test master. */
  const MASTER = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';
  type Keys = {chip: string; key_version: number; enc: string; mac: string; dek: string};
  const keysOf = async (res: Response) => {
    expect(res.status).toBe(200);
    return (await res.json()) as Keys & {ok: boolean};
  };

  it('derives with HKDF-SHA256 exactly as WebCrypto does: per chip, per master, 16 bytes each', async () => {
    const master = parseMaster(MASTER)!;
    const a = deriveManagerKeys(master, CHIP);
    expect(deriveManagerKeys(master, CHIP)).toEqual(a);
    const ikm = await crypto.subtle.importKey('raw', master, 'HKDF', false, ['deriveBits']);
    for (const [label, value] of [['gp-enc', a.enc], ['gp-mac', a.mac], ['gp-dek', a.dek]] as const) {
      const bits = await crypto.subtle.deriveBits(
        {
          name: 'HKDF',
          hash: 'SHA-256',
          salt: new TextEncoder().encode('fedimintcard/card-manager/v1'),
          info: new TextEncoder().encode(label + CHIP),
        },
        ikm,
        128,
      );
      expect(value).toBe(toHex(new Uint8Array(bits)));
    }
    expect(new Set([a.enc, a.mac, a.dek]).size).toBe(3);
    const otherChip = deriveManagerKeys(master, OTHER_CHIP);
    const otherMaster = deriveManagerKeys(parseMaster('ff'.repeat(32))!, CHIP);
    for (const other of [otherChip, otherMaster]) {
      expect(other.enc).not.toBe(a.enc);
      expect(other.mac).not.toBe(a.mac);
      expect(other.dek).not.toBe(a.dek);
    }
    expect(parseMaster(undefined)).toBeNull();
    expect(parseMaster('00'.repeat(31))).toBeNull();
    expect(parseMaster('zz'.repeat(32))).toBeNull();
  });

  it('escrows on the first ask and answers the identical set on every repeat', async () => {
    const token = await signIn();
    const first = await keysOf(await post('/registry/manager-keys', token, {chip: CHIP}));
    expect(first).toEqual({ok: true, ...deriveManagerKeys(parseMaster(MASTER)!, CHIP)});
    expect(first.key_version).toBe(1);
    expect(await keysOf(await post('/registry/manager-keys', token, {chip: CHIP}))).toEqual(first);
    await runInDurableObject(stub(), async (_i, state) => {
      const rows = state.storage.sql.exec('SELECT * FROM manager_keys').toArray();
      expect(rows).toEqual([expect.objectContaining({chip: CHIP, operator_id: 1, key_version: 1, enc: first.enc, short_id: null})]);
      // A changed master does not change what was issued: the escrow answers, not a fresh derivation.
      const again = issueManagerKeys(state.storage, 1, 'ff'.repeat(32), {chip: CHIP}, Date.now(), () => null);
      expect(again.enc).toBe(first.enc);
    });
  });

  it('the first operator owns the chip: another gets 409 and no keys; a till gets 403', async () => {
    const token = await signIn();
    await keysOf(await post('/registry/manager-keys', token, {chip: CHIP}));
    await hook('/test/operator', {name: 'other', password: 'another long password'});
    const other = await signIn('other', 'another long password');
    const res = await post('/registry/manager-keys', other, {chip: CHIP});
    expect(res.status).toBe(409);
    const body = await res.text();
    expect(body).toContain('another operator');
    expect(body).not.toMatch(/[0-9a-f]{32}/);
    // Their own chip is theirs.
    expect((await post('/registry/manager-keys', other, {chip: OTHER_CHIP})).status).toBe(200);
    const till = await newTill(token);
    expect((await post('/registry/manager-keys', till.token, {chip: CHIP})).status).toBe(403);
    expect((await post('/registry/manager-keys', null, {chip: CHIP})).status).toBe(401);
  });

  it('refuses a malformed chip, and a short id that is not this operator’s card', async () => {
    const token = await signIn();
    for (const chip of [undefined, '4790d3213041093363205547', 'cplc-v1:XYZ', 'cplc-v1:' + 'a'.repeat(23)]) {
      expect((await post('/registry/manager-keys', token, {chip})).status).toBe(400);
    }
    expect((await post('/registry/manager-keys', token, {chip: CHIP, short_id: 'deadbeef'})).status).toBe(404);
    expect((await post('/registry/manager-keys', token, {chip: CHIP, short_id: 'XY'})).status).toBe(400);
  });

  it('answers 503 without a master, and nothing else stops working', async () => {
    const token = await signIn();
    await runInDurableObject(stub(), async instance => {
      const live = instance as unknown as {env: Record<string, unknown>};
      const saved = live.env;
      live.env = {...saved, CARD_MANAGER_MASTER: undefined};
      try {
        const call = (method: string, path: string, body?: unknown) =>
          instance.fetch(
            new Request(`https://registry.internal${path}`, {
              method,
              headers: {Authorization: `Bearer ${token}`, 'Content-Type': 'application/json'},
              body: body === undefined ? undefined : JSON.stringify(body),
            }),
          );
        const res = await call('POST', '/registry/manager-keys', {chip: CHIP});
        expect(res.status).toBe(503);
        expect(await res.text()).toContain('CARD_MANAGER_MASTER');
        expect((await call('POST', '/registry/batches', {federation_id: FED, count: 1, pin_policy: 'batch', load_sha256: SHA})).status).toBe(200);
        expect((await call('GET', '/registry/federations')).status).toBe(200);
        live.env = {...saved, CARD_MANAGER_MASTER: 'not hex'};
        expect((await call('POST', '/registry/manager-keys', {chip: CHIP})).status).toBe(503);
      } finally {
        live.env = saved;
      }
    });
    await runInDurableObject(stub(), async (_i, state) => {
      expect(state.storage.sql.exec('SELECT COUNT(*) AS n FROM manager_keys').one()).toEqual({n: 0});
    });
  });

  it('a card row that names its chip points the escrow at its short id', async () => {
    const token = await signIn();
    await keysOf(await post('/registry/manager-keys', token, {chip: CHIP}));
    const batchId = await newBatch(token);
    const row = card({short_id: '5e5e5e5e', batch_id: batchId, chip: CHIP});
    expect((await post('/registry/cards', token, row)).status).toBe(200);
    // the identical body replays; the same row naming another chip is a different row
    expect((await post('/registry/cards', token, row)).status).toBe(200);
    expect((await post('/registry/cards', token, {...row, chip: OTHER_CHIP})).status).toBe(409);
    expect((await post('/registry/cards', token, {...row, short_id: '6e6e6e6e', slot_pubkeys: slots('6e6e6e6e'), chip: 'cplc-v1:zz'})).status).toBe(400);
    await runInDurableObject(stub(), async (_i, state) => {
      expect(state.storage.sql.exec('SELECT short_id FROM manager_keys WHERE chip = ?', CHIP).one()).toEqual({short_id: '5e5e5e5e'});
    });
  });

  it('the nightly backup carries the escrow, encrypted', async () => {
    const token = await signIn();
    const keys = await keysOf(await post('/registry/manager-keys', token, {chip: CHIP}));
    const res = await stub().fetch('https://registry.internal/internal/backup', {method: 'POST'});
    const {key} = (await res.json()) as {key: string};
    const armored = await (await env.BACKUPS!.get(key))!.text();
    expect(armored).not.toContain(keys.enc);
    const snap = await decrypt(armored, 'AGE-SECRET-KEY-1AHS3RTK9NEEFMWRWJY6RQDF9HZZ9RQTD3UW0CNCX8YHQFSLN08LSECDQ2E');
    expect(snap.tables.card_resets).toEqual([]);
    expect(snap.tables.manager_keys).toEqual([
      expect.objectContaining({chip: CHIP, operator_id: 1, key_version: 1, enc: keys.enc, mac: keys.mac, dek: keys.dek}),
    ]);
  });
});

describe('O5: reset in the provisioner', () => {
  const CHIP = 'cplc-v1:4790d3213041093363205547';
  const OLD = '0a0b0c0d';

  async function provisioned(token: string, shortId = OLD) {
    const batchId = await newBatch(token);
    expect((await post('/registry/cards', token, card({short_id: shortId, batch_id: batchId, chip: CHIP}))).status).toBe(200);
    return batchId;
  }
  const check = async (token: string, shortId = OLD) => {
    const res = await get(`/registry/cards/${shortId}/reset-check`, token);
    expect(res.status).toBe(200);
    return (await res.json()) as {
      resettable: boolean;
      empty: boolean;
      reasons: string[];
      live_notes: number;
      pending_notes: number;
      open_charges: number;
      open_loads: number;
      chip: string | null;
    };
  };
  /** The completion the phone posts once the card is proved factory. */
  const done = (over: Partial<Record<string, unknown>> = {}) => ({
    short_id: OLD,
    chip: CHIP,
    gp_key_version: 1,
    manager_keys: 'factory',
    package: 'absent',
    verified_at: 1_770_000_000_000,
    ...over,
  });
  const other = async () => {
    await hook('/test/operator', {name: 'other', password: 'another long password'});
    return signIn('other', 'another long password');
  };

  it('an empty card may be reset; the check names its chip', async () => {
    const token = await signIn();
    await provisioned(token);
    expect(await check(token)).toEqual(
      expect.objectContaining({resettable: true, empty: true, reasons: [], chip: CHIP, live_notes: 0, open_loads: 0}),
    );
  });

  it('refuses while ops shows a live note, a pending note or an open charge, and says which', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const {shortId} = await cardWithNote(token, OLD);
    let c = await check(token, shortId);
    expect(c).toEqual(expect.objectContaining({resettable: false, empty: false, live_notes: 1}));
    expect(c.reasons).toEqual(['1 live note, 1024000 msat']);
    expect((await post(`/cards/${shortId}/intent`, till.token, intent(0))).status).toBe(200);
    c = await check(token, shortId);
    expect(c).toEqual(expect.objectContaining({resettable: false, live_notes: 1, pending_notes: 1, open_charges: 1}));
    expect(c.reasons).toEqual(['1 live note, 1024000 msat', '1 pending note, 24000 msat', '1 open charge at a till']);
  });

  it('refuses while a holder’s load is open, and not once it has expired', async () => {
    const token = await signIn();
    await newTill(token);
    await provisioned(token);
    const load = {version: 0, txid: txid(30), notes: [{slot: 1, amount_msat: 1_000, nonce: '02' + slots(OLD)[1]!, blinding_key: 'e'.repeat(64), out_idx: 0}]};
    expect((await post(`/cards/${OLD}/load`, null, load)).status).toBe(200);
    expect(await check(token)).toEqual(expect.objectContaining({resettable: false, open_loads: 1, pending_notes: 1}));
    await runInDurableObject(stub(), async (_i, state) => {
      state.storage.sql.exec('UPDATE holder_loads SET expires_at = 0');
    });
    expect(await check(token)).toEqual(expect.objectContaining({resettable: true, open_loads: 0, pending_notes: 0}));
  });

  it('another operator’s card and an unknown card answer 404; a till cannot ask', async () => {
    const token = await signIn();
    await provisioned(token);
    const them = await other();
    expect((await get(`/registry/cards/${OLD}/reset-check`, them)).status).toBe(404);
    expect((await post(`/registry/cards/${OLD}/reset`, them, done())).status).toBe(404);
    expect((await get('/registry/cards/99999999/reset-check', token)).status).toBe(404);
    expect((await get('/registry/cards/XYZ/reset-check', token)).status).toBe(400);
    const till = await newTill(token);
    expect((await get(`/registry/cards/${OLD}/reset-check`, till.token)).status).toBe(403);
  });

  it('completes the reset: the old row reset, the chip released, the proof stored; a repeat changes nothing', async () => {
    const token = await signIn();
    const batchId = await provisioned(token);
    const keys = (await (await post('/registry/manager-keys', token, {chip: CHIP, short_id: OLD})).json()) as {enc: string};
    // Before the reset the chip is this operator's: another gets 409.
    const them = await other();
    expect((await post('/registry/manager-keys', them, {chip: CHIP})).status).toBe(409);

    const res = await post(`/registry/cards/${OLD}/reset`, token, done());
    expect(res.status).toBe(200);
    const body = (await res.json()) as {reset: Record<string, unknown>; stranded: {live_notes: number}; released: boolean};
    expect(body.reset).toEqual(
      expect.objectContaining({short_id: OLD, chip: CHIP, operator_id: 1, gp_key_version: 1, manager_keys: 'factory', package: 'absent', verified_at: 1_770_000_000_000}),
    );
    expect(body.stranded.live_notes).toBe(0);
    expect(body.released).toBe(true);
    await runInDurableObject(stub(), async (_i, state) => {
      const sql = state.storage.sql;
      expect(sql.exec('SELECT COUNT(*) AS n FROM manager_keys WHERE chip = ?', CHIP).one()).toEqual({n: 0});
      expect(sql.exec('SELECT short_id, chip, operator_id, gp_key_version, manager_keys, package FROM card_resets').toArray()).toEqual([
        {short_id: OLD, chip: CHIP, operator_id: 1, gp_key_version: 1, manager_keys: 'factory', package: 'absent'},
      ]);
      expect(sql.exec('SELECT COUNT(*) AS n FROM cards').one()).toEqual({n: 1});
    });

    // The repeat (an outbox retry) answers the stored record and changes nothing.
    const replay = await post(`/registry/cards/${OLD}/reset`, token, done({verified_at: 1_770_000_000_500}));
    expect(replay.status).toBe(200);
    const again = (await replay.json()) as {reset: {verified_at: number; completed_at: number}; released: boolean};
    expect(again.reset).toEqual(body.reset);
    expect(again.released).toBe(false);
    expect((await post(`/registry/cards/${OLD}/reset`, token, done({chip: 'cplc-v1:' + '1'.repeat(24)}))).status).toBe(409);

    const old = await check(token);
    expect(old).toEqual(expect.objectContaining({resettable: false}));
    expect(old.reasons).toEqual(['already reset: the card is blank, on its factory keys']);
    const listed = (await (await get(`/registry/cards?batch_id=${batchId}`, token)).json()) as {cards: {short_id: string; reset_at: number | null}[]};
    expect(listed.cards.map(c => [c.short_id, c.reset_at !== null])).toEqual([[OLD, true]]);

    // The chip is released: another operator now gets its keys (the same bytes) and owns it.
    const theirs = (await (await post('/registry/manager-keys', them, {chip: CHIP})).json()) as {enc: string};
    expect(theirs.enc).toBe(keys.enc);
    expect((await post('/registry/manager-keys', token, {chip: CHIP})).status).toBe(409);
    // A late repeat from the first phone still changes nothing: their escrow stays.
    expect((await post(`/registry/cards/${OLD}/reset`, token, done())).status).toBe(200);
    await runInDurableObject(stub(), async (_i, state) => {
      const owner = state.storage.sql.exec('SELECT operator_id FROM manager_keys WHERE chip = ?', CHIP).one() as {operator_id: number};
      expect(owner.operator_id).not.toBe(1);
    });
  });

  it('refuses a completion that proves less than factory, or names another card or chip', async () => {
    const token = await signIn();
    await provisioned(token);
    for (const bad of [
      done({manager_keys: 'own'}),
      done({package: 'present'}),
      done({gp_key_version: 0}),
      done({gp_key_version: 'x'}),
      done({verified_at: undefined}),
      done({chip: 'cplc-v1:XYZ'}),
      done({short_id: '99999999'}),
    ]) {
      expect((await post(`/registry/cards/${OLD}/reset`, token, bad)).status).toBe(400);
    }
    expect((await post(`/registry/cards/${OLD}/reset`, token, done({chip: 'cplc-v1:' + '1'.repeat(24)}))).status).toBe(409);
    expect((await check(token)).resettable).toBe(true);
  });

  it('only the owner completes: another operator’s chip escrow, or a chip that carries another card, is 409', async () => {
    const token = await signIn();
    // a card row from before O5 names no chip, and the chip's escrow is someone else's
    const batchId = await newBatch(token);
    expect((await post('/registry/cards', token, card({short_id: OLD, batch_id: batchId}))).status).toBe(200);
    const them = await other();
    expect((await post('/registry/manager-keys', them, {chip: CHIP})).status).toBe(200);
    const res = await post(`/registry/cards/${OLD}/reset`, token, done());
    expect(res.status).toBe(409);
    expect(await res.text()).toContain('another operator');
    expect((await check(token)).resettable).toBe(true);
    await runInDurableObject(stub(), async (_i, state) => {
      expect(state.storage.sql.exec('SELECT COUNT(*) AS n FROM manager_keys').one()).toEqual({n: 1});
      expect(state.storage.sql.exec('SELECT COUNT(*) AS n FROM card_resets').one()).toEqual({n: 0});
    });
  });

  it('a chip whose escrow points at another live card is not released by this one', async () => {
    const token = await signIn();
    await provisioned(token);
    const batchId = await newBatch(token);
    expect((await post('/registry/cards', token, card({short_id: '5e5e5e5e', batch_id: batchId, chip: CHIP}))).status).toBe(200);
    expect((await post('/registry/manager-keys', token, {chip: CHIP, short_id: '5e5e5e5e'})).status).toBe(200);
    expect((await post(`/registry/cards/${OLD}/reset`, token, done())).status).toBe(409);
  });

  it('after a reset the old short id takes no load and no charge (410); its state still reads', async () => {
    const token = await signIn();
    const till = await newTill(token);
    await provisioned(token);
    expect((await post(`/registry/cards/${OLD}/reset`, token, done())).status).toBe(200);
    expect((await get(`/cards/${OLD}/slots`, null)).status).toBe(410);
    expect((await post(`/cards/${OLD}/load`, null, {version: 0, txid: txid(31), notes: []})).status).toBe(410);
    expect((await post(`/cards/${OLD}/intent`, till.token, intent(0))).status).toBe(410);
    expect((await get(`/cards/${OLD}/state`, till.token)).status).toBe(200);
  });
});

describe('O5: reset by chip, a card that never reached a holder', () => {
  const CHIP = 'cplc-v1:4790d3213041093363205547';
  const OLD = '0a0b0c0d';
  const NEW = '1e1f2021';
  const other = async () => {
    await hook('/test/operator', {name: 'other', password: 'another long password'});
    return signIn('other', 'another long password');
  };
  type Check = {known: boolean; needs_holder_pin: boolean; resettable: boolean; reasons: string[]; chip: string | null; live_notes: number};
  const check = async (token: string, shortId = NEW, chip: string | null = CHIP) => {
    const res = await get(`/registry/cards/${shortId}/reset-check${chip === null ? '' : `?chip=${chip}`}`, token);
    return {status: res.status, body: res.status === 200 ? ((await res.json()) as Check) : null};
  };
  const done = (over: Partial<Record<string, unknown>> = {}) => ({
    short_id: NEW,
    chip: CHIP,
    gp_key_version: 1,
    manager_keys: 'factory',
    package: 'absent',
    verified_at: 1_770_000_000_000,
    ...over,
  });
  /** The stopped install's row: the card posted with its PIN not set. */
  async function stoppedRow(token: string, shortId = OLD) {
    const batchId = await newBatch(token);
    expect((await post('/registry/cards', token, card({short_id: shortId, batch_id: batchId, chip: CHIP, pin_set: false}))).status).toBe(200);
  }
  const escrow = async (token: string) => expect((await post('/registry/manager-keys', token, {chip: CHIP})).status).toBe(200);

  it('needsReset: PIN set, short id unknown to ops, chip escrowed by the caller, nothing held: reset by chip without the PIN', async () => {
    const token = await signIn();
    await stoppedRow(token); // linked to the chip by the post
    await escrow(token);
    const c = await check(token);
    expect(c.status).toBe(200);
    expect(c.body).toEqual(
      expect.objectContaining({known: false, needs_holder_pin: false, resettable: true, reasons: [], chip: CHIP, live_notes: 0}),
    );
    // Without the chip the short id is unknown, as before.
    expect((await check(token, NEW, null)).status).toBe(404);

    const res = await post(`/registry/cards/${NEW}/reset`, token, done());
    expect(res.status).toBe(200);
    const body = (await res.json()) as {reset: Record<string, unknown>; released: boolean};
    expect(body.reset).toEqual(expect.objectContaining({short_id: NEW, chip: CHIP, operator_id: 1}));
    expect(body.released).toBe(true);
    await runInDurableObject(stub(), async (_i, state) => {
      const sql = state.storage.sql;
      expect(sql.exec('SELECT COUNT(*) AS n FROM manager_keys').one()).toEqual({n: 0});
      expect(sql.exec('SELECT short_id, chip FROM card_resets').toArray()).toEqual([{short_id: NEW, chip: CHIP}]);
      // The stopped row on the chip is closed too; no row is made for the unknown id.
      expect((sql.exec('SELECT reset_at FROM cards WHERE short_id = ?', OLD).one() as {reset_at: unknown}).reset_at).not.toBeNull();
      expect(sql.exec('SELECT COUNT(*) AS n FROM cards').one()).toEqual({n: 1});
    });
    expect((await get(`/cards/${OLD}/slots`, null)).status).toBe(410);

    // Idempotent: the repeat answers the stored record and releases nothing.
    const replay = await post(`/registry/cards/${NEW}/reset`, token, done({verified_at: 1_770_000_000_900}));
    expect(replay.status).toBe(200);
    const again = (await replay.json()) as {reset: Record<string, unknown>; released: boolean};
    expect(again.reset).toEqual(body.reset);
    expect(again.released).toBe(false);
    // The chip is anyone's again; a late repeat leaves the new owner's escrow alone.
    const them = await other();
    await escrow(them);
    expect((await post(`/registry/cards/${NEW}/reset`, token, done())).status).toBe(200);
    expect((await post(`/registry/cards/${NEW}/reset`, them, done())).status).toBe(404);
    await runInDurableObject(stub(), async (_i, state) => {
      expect(state.storage.sql.exec('SELECT operator_id FROM manager_keys WHERE chip = ?', CHIP).one()).not.toEqual({operator_id: 1});
    });
  });

  it('a stopped row the card still answers, or no row at all, needs no PIN either', async () => {
    const token = await signIn();
    await escrow(token);
    expect((await check(token)).body).toEqual(expect.objectContaining({known: false, needs_holder_pin: false, resettable: true}));
    await stoppedRow(token);
    expect((await check(token, OLD)).body).toEqual(expect.objectContaining({known: true, needs_holder_pin: false, resettable: true}));
    expect((await post(`/registry/cards/${OLD}/reset`, token, done({short_id: OLD}))).status).toBe(200);
  });

  it('another operator’s chip answers as if unknown', async () => {
    const token = await signIn();
    const them = await other();
    await stoppedRow(them);
    await escrow(them);
    expect((await check(token)).status).toBe(404);
    expect((await check(token, OLD)).status).toBe(404);
    expect((await post(`/registry/cards/${NEW}/reset`, token, done())).status).toBe(404);
    // A chip nobody escrowed is unknown too.
    expect((await check(token, NEW, 'cplc-v1:' + '1'.repeat(24))).status).toBe(404);
    expect((await get(`/registry/cards/${NEW}/reset-check?chip=nope`, token)).status).toBe(400);
    await runInDurableObject(stub(), async (_i, state) => {
      expect(state.storage.sql.exec('SELECT COUNT(*) AS n FROM manager_keys').one()).toEqual({n: 1});
      expect(state.storage.sql.exec('SELECT COUNT(*) AS n FROM card_resets').one()).toEqual({n: 0});
    });
  });

  it('a chip with a finished open card still needs the holder’s PIN, and is not closed by another id', async () => {
    const token = await signIn();
    const batchId = await newBatch(token);
    expect((await post('/registry/cards', token, card({short_id: OLD, batch_id: batchId, chip: CHIP}))).status).toBe(200);
    await escrow(token);
    expect((await check(token, OLD)).body).toEqual(expect.objectContaining({known: true, needs_holder_pin: true, resettable: true}));
    const stray = await check(token);
    expect(stray.body).toEqual(expect.objectContaining({known: false, needs_holder_pin: true, resettable: false}));
    expect(stray.body!.reasons).toEqual([`ops records card ${OLD} on this chip`]);
    expect((await post(`/registry/cards/${NEW}/reset`, token, done())).status).toBe(409);
    expect((await check(token, OLD)).body!.resettable).toBe(true);
  });

  it('a chip with value on an open row is refused', async () => {
    const token = await signIn();
    await stoppedRow(token);
    await escrow(token);
    await hook('/test/note', {short_id: OLD, nonce: nonce(7), slot: 0, amount_msat: 2_000, signature: 'c'.repeat(96)});
    const c = await check(token);
    expect(c.body).toEqual(expect.objectContaining({needs_holder_pin: true, resettable: false, live_notes: 1}));
    expect(c.body!.reasons).toEqual([`card ${OLD}: 1 live note, 2000 msat`]);
    expect((await check(token, OLD)).body).toEqual(expect.objectContaining({needs_holder_pin: true, resettable: false}));
  });
});

describe('O5: finishing a card lifted after its rotation', () => {
  const CHIP = 'cplc-v1:4790d3213041093363205547';
  const OLD = '0a0b0c0d';
  const NEW = '1e1f2021';

  const other = async () => {
    await hook('/test/operator', {name: 'other', password: 'another long password'});
    return signIn('other', 'another long password');
  };
  const resumeCheck = async (token: string, shortId = OLD, chip = CHIP) => {
    const res = await post('/registry/resume-check', token, {chip, short_id: shortId});
    expect(res.status).toBe(200);
    return (await res.json()) as {known: boolean; resumable: boolean; reasons: string[]; live_notes: number};
  };
  /** A row an older build could have posted: the card stopped with its PIN not set. */
  async function stoppedRow(token: string, shortId = OLD) {
    const batchId = await newBatch(token);
    expect((await post('/registry/cards', token, card({short_id: shortId, batch_id: batchId, chip: CHIP, pin_set: false}))).status).toBe(200);
    return batchId;
  }
  const finished = (batchId: number, over: Partial<Record<string, unknown>> = {}) =>
    card({short_id: NEW, batch_id: batchId, chip: CHIP, supersedes: OLD, ...over});
  const replacedBy = async (shortId: string) => {
    let out: unknown;
    await runInDurableObject(stub(), async (_i, state) => {
      out = (state.storage.sql.exec('SELECT replaced_by FROM cards WHERE short_id = ?', shortId).one() as {replaced_by: unknown}).replaced_by;
    });
    return out;
  };

  it('a card ops has no row for may be finished; the post stores the new card and nothing for the old id', async () => {
    const token = await signIn();
    expect(await resumeCheck(token)).toEqual(expect.objectContaining({known: false, resumable: true, reasons: []}));
    const batchId = await newBatch(token);
    expect((await post('/registry/cards', token, finished(batchId))).status).toBe(200);
    expect((await get(`/cards/${OLD}/slots`, null)).status).toBe(404);
    expect((await get(`/cards/${NEW}/slots`, null)).status).toBe(200);
  });

  it('closes the stopped row in the same transaction: the old id answers 410, and a repeat changes nothing', async () => {
    const token = await signIn();
    const till = await newTill(token);
    await stoppedRow(token);
    expect(await resumeCheck(token)).toEqual(expect.objectContaining({known: true, resumable: true, reasons: []}));
    const batchId = await newBatch(token);
    const res = await post('/registry/cards', token, finished(batchId));
    expect(res.status).toBe(200);
    expect(await replacedBy(OLD)).toBe(NEW);
    expect((await get(`/cards/${OLD}/slots`, null)).status).toBe(410);
    expect((await post(`/cards/${OLD}/load`, null, {version: 0, txid: txid(41), notes: []})).status).toBe(410);
    expect((await post(`/cards/${OLD}/intent`, till.token, intent(0))).status).toBe(410);
    // The new card is an ordinary one.
    expect((await get(`/cards/${NEW}/slots`, null)).status).toBe(200);
    // An outbox retry: the same row, the old one already closed by it.
    expect((await post('/registry/cards', token, finished(batchId))).status).toBe(200);
    expect(await replacedBy(OLD)).toBe(NEW);
    // The old id is closed for good: a check on it, or another card naming it, is refused.
    const again = await resumeCheck(token);
    expect(again.resumable).toBe(false);
    expect((await post('/registry/cards', token, card({short_id: '2a2b2c2d', batch_id: batchId, chip: CHIP, supersedes: OLD}))).status).toBe(409);
    // A reset check on the old id says why it is closed.
    const reset = (await (await get(`/registry/cards/${OLD}/reset-check`, token)).json()) as {resettable: boolean; reasons: string[]};
    expect(reset.resettable).toBe(false);
    expect(reset.reasons[0]).toContain(NEW);
  });

  it('refuses while the old row holds value, and stores nothing', async () => {
    const token = await signIn();
    await stoppedRow(token);
    await hook('/test/note', {short_id: OLD, nonce: nonce(7), slot: 0, amount_msat: 1_024_000, signature: 'c'.repeat(96)});
    const c = await resumeCheck(token);
    expect(c).toEqual(expect.objectContaining({resumable: false, live_notes: 1}));
    expect(c.reasons).toEqual(['1 live note, 1024000 msat']);
    const batchId = await newBatch(token);
    expect((await post('/registry/cards', token, finished(batchId))).status).toBe(409);
    expect((await get(`/cards/${NEW}/slots`, null)).status).toBe(404);
    expect(await replacedBy(OLD)).toBeNull();
  });

  it('refuses another operator’s row, a row with its PIN set, or one on another chip', async () => {
    const token = await signIn();
    const them = await other();
    await stoppedRow(them);
    const c = await resumeCheck(token);
    expect(c.resumable).toBe(false);
    expect(c.reasons[0]).toContain('another operator');
    const batchId = await newBatch(token);
    expect((await post('/registry/cards', token, finished(batchId))).status).toBe(409);
    expect(await replacedBy(OLD)).toBeNull();

    // This operator's row, but finished (PIN set): never closed by a resume.
    const pinned = '3a3b3c3d';
    expect((await post('/registry/cards', token, card({short_id: pinned, batch_id: batchId, chip: 'cplc-v1:' + '2'.repeat(24)}))).status).toBe(200);
    expect((await resumeCheck(token, pinned, 'cplc-v1:' + '2'.repeat(24))).reasons).toEqual([`ops records card ${pinned} with its PIN set`]);
    expect((await post('/registry/cards', token, card({short_id: NEW, batch_id: batchId, chip: 'cplc-v1:' + '2'.repeat(24), supersedes: pinned}))).status).toBe(409);

    // A stopped row of this operator's, on another chip than the one being finished.
    await stoppedRow(token, '4a4b4c4d');
    expect((await resumeCheck(token, '4a4b4c4d', 'cplc-v1:' + '3'.repeat(24))).reasons).toContain('ops recorded card 4a4b4c4d on another chip');
  });

  it('refuses a malformed supersedes, one naming the card itself, or a finished row without its chip or PIN', async () => {
    const token = await signIn();
    const batchId = await newBatch(token);
    expect((await post('/registry/cards', token, finished(batchId, {supersedes: 'XYZ'}))).status).toBe(400);
    expect((await post('/registry/cards', token, finished(batchId, {supersedes: NEW}))).status).toBe(400);
    expect((await post('/registry/cards', token, finished(batchId, {chip: undefined}))).status).toBe(400);
    expect((await post('/registry/cards', token, finished(batchId, {pin_set: false}))).status).toBe(400);
    expect((await post('/registry/resume-check', token, {chip: 'nope', short_id: OLD})).status).toBe(400);
    const till = await newTill(token);
    expect((await post('/registry/resume-check', till.token, {chip: CHIP, short_id: OLD})).status).toBe(403);
  });

  it('a row whose PIN is not set takes no load and no charge', async () => {
    const token = await signIn();
    const till = await newTill(token);
    await stoppedRow(token);
    expect((await get(`/cards/${OLD}/slots`, null)).status).toBe(409);
    const load = {version: 0, txid: txid(42), notes: [{slot: 1, amount_msat: 1_000, nonce: '02' + slots(OLD)[1]!, blinding_key: 'e'.repeat(64), out_idx: 0}]};
    const res = await post(`/cards/${OLD}/load`, null, load);
    expect(res.status).toBe(409);
    expect(await res.text()).toContain('no PIN set');
    expect((await post(`/cards/${OLD}/intent`, till.token, intent(0))).status).toBe(409);
    await runInDurableObject(stub(), async (_i, state) => {
      expect(state.storage.sql.exec('SELECT COUNT(*) AS n FROM holder_loads').one()).toEqual({n: 0});
      expect(state.storage.sql.exec('SELECT COUNT(*) AS n FROM notes').one()).toEqual({n: 0});
    });
  });
});

describe('32-slot cards', () => {
  const BK = 'e'.repeat(64);
  const keyNonce32 = (shortId: string, n: number) => '02' + slots(shortId, 32)[n]!;

  async function card32(shortId = '3a3b3c3d') {
    const token = await signIn();
    const till = await newTill(token);
    const batchId = await newBatch(token);
    const res = await post('/registry/cards', token, card({short_id: shortId, batch_id: batchId, slot_pubkeys: slots(shortId, 32)}));
    expect(res.status).toBe(200);
    return {token, till, batchId, shortId, stored: ((await res.json()) as {stored: {slot_count: number; slot_pubkeys: string[]}}).stored};
  }

  it('registers 32 keys and records 32 slots; an 8-key row records 8, a keyless row 8', async () => {
    const {token, till, batchId, shortId, stored} = await card32();
    expect(stored.slot_count).toBe(32);
    expect(stored.slot_pubkeys).toHaveLength(32);
    const s = await state(till.token, shortId);
    expect(s.slots.map(x => x.slot)).toEqual(Array.from({length: 32}, (_, i) => i));
    expect((s as unknown as {slot_count: number}).slot_count).toBe(32);

    const eight = await post('/registry/cards', token, card({short_id: '8a8b8c8d', batch_id: batchId}));
    expect(((await eight.json()) as {stored: {slot_count: number}}).stored.slot_count).toBe(8);
    const {slot_pubkeys: _drop, ...older} = card({short_id: '0a0a0a0a', batch_id: batchId});
    const keyless = await post('/registry/cards', token, older);
    expect(((await keyless.json()) as {stored: {slot_count: number}}).stored.slot_count).toBe(8);
    // The identical 32-key body replays; the same keys cut to 8 are a different card.
    expect((await post('/registry/cards', token, card({short_id: shortId, batch_id: batchId, slot_pubkeys: slots(shortId, 32)}))).status).toBe(200);
    expect((await post('/registry/cards', token, card({short_id: shortId, batch_id: batchId, slot_pubkeys: slots(shortId, 32).slice(0, 8)}))).status).toBe(409);
  });

  it('refuses any key count but 8 or 32', async () => {
    const token = await signIn();
    const batchId = await newBatch(token);
    for (const n of [1, 7, 9, 16, 31, 33]) {
      const res = await post('/registry/cards', token, card({short_id: '5a5b5c5d', batch_id: batchId, slot_pubkeys: slots('5a5b5c5d', n)}));
      expect(res.status).toBe(400);
    }
  });

  it('bounds a charge’s change by the card’s own count: slot 31 on a 32-slot card, slot 8 nowhere on an 8-slot one', async () => {
    const {till, shortId} = await card32();
    await hook('/test/note', {short_id: shortId, nonce: nonce(1), slot: 0, amount_msat: 1_024_000, signature: 'c'.repeat(96)});
    const bad = await post(`/cards/${shortId}/intent`, till.token, intent(0, {change: [{slot: 32, amount_msat: 24_000, nonce: nonce(2), blinding_key: BK, out_idx: 1}]}));
    expect(bad.status).toBe(400);
    expect(await bad.text()).toContain('slot 0-31');
    const ok = await post(`/cards/${shortId}/intent`, till.token, intent(0, {change: [{slot: 31, amount_msat: 24_000, nonce: nonce(2), blinding_key: BK, out_idx: 1}]}));
    expect(ok.status).toBe(200);
    expect((await state(till.token, shortId)).notes.find(n => n.slot === 31)?.status).toBe('pending');

    const token = await signIn();
    const {shortId: eight} = await cardWithNote(token, '8a8b8c8e');
    const res = await post(`/cards/${eight}/intent`, till.token, intent(0, {txid: txid(8), change: [{slot: 8, amount_msat: 24_000, nonce: nonce(3), blinding_key: BK, out_idx: 1}]}));
    expect(res.status).toBe(400);
    expect(await res.text()).toContain('slot 0-7');
  });

  it('renews a two-digit slot on a 32-slot card; slot 8 of an 8-slot card is out of range', async () => {
    const {till, batchId, token, shortId} = await card32();
    const old = slots(shortId, 32)[20]!;
    const ok = await call('PUT', `/cards/${shortId}/slots/20`, till.token, {version: 0, old_pubkey: old, pubkey: key(90)});
    expect(await ok.json()).toEqual({ok: true, version: 1});
    expect((await state(till.token, shortId)).slots[20]!.pubkey).toBe(key(90));
    expect((await call('PUT', `/cards/${shortId}/slots/32`, till.token, {version: 1, old_pubkey: old, pubkey: key(91)})).status).toBe(400);

    await post('/registry/cards', token, card({short_id: '8a8b8c8f', batch_id: batchId}));
    const res = await call('PUT', '/cards/8a8b8c8f/slots/8', till.token, {version: 0, old_pubkey: key(9), pubkey: key(92)});
    expect(res.status).toBe(400);
    expect(await res.text()).toContain('0 to 7');
  });

  it('lists all 32 free slots and takes a 12-note holder load, which an 8-slot card could not hold', async () => {
    const {till, shortId} = await card32();
    const free = (await (await get(`/cards/${shortId}/slots`, null)).json()) as {free: {slot: number}[]};
    expect(free.free.map(f => f.slot)).toEqual(Array.from({length: 32}, (_, i) => i));
    const picked = Array.from({length: 12}, (_, i) => 20 + i); // slots 20-31
    const body = {
      version: 0,
      txid: txid(30),
      notes: picked.map((slot, i) => ({slot, amount_msat: 1_000, nonce: keyNonce32(shortId, slot), blinding_key: BK, out_idx: i})),
    };
    expect((await post(`/cards/${shortId}/load`, null, body)).status).toBe(200);
    const outcome = {txid: txid(30), notes: picked.map(slot => ({nonce: keyNonce32(shortId, slot), signature: sign(keyNonce32(shortId, slot), 1_000)}))};
    expect((await post(`/cards/${shortId}/load/outcome`, null, outcome)).status).toBe(200);
    expect((await state(till.token, shortId)).notes.filter(n => n.status === 'live').map(n => n.slot)).toEqual(picked);

    // Out of range is refused before the card's keys are read: slot 32 here, slot 8 and a ninth note on 8 slots.
    const over = {version: 2, txid: txid(31), notes: [{slot: 32, amount_msat: 1_000, nonce: nonce(4), blinding_key: BK, out_idx: 0}]};
    expect((await post(`/cards/${shortId}/load`, null, over)).status).toBe(400);
    const token = await signIn();
    const batchId = await newBatch(token);
    await post('/registry/cards', token, card({short_id: '8a8b8c90', batch_id: batchId}));
    const slot8 = {version: 0, txid: txid(32), notes: [{slot: 8, amount_msat: 1_000, nonce: nonce(5), blinding_key: BK, out_idx: 0}]};
    expect((await post('/cards/8a8b8c90/load', null, slot8)).status).toBe(400);
    const nine = {
      version: 0,
      txid: txid(33),
      notes: Array.from({length: 9}, (_, i) => ({slot: i, amount_msat: 1_000, nonce: nonce(10 + i), blinding_key: BK, out_idx: i})),
    };
    const res = await post('/cards/8a8b8c90/load', null, nine);
    expect(res.status).toBe(400);
    expect(await res.text()).toContain('1 to 8 notes');
  });
});

describe('split keying: a 32-slot card registered with keys for slots 0-7', () => {
  const BK = 'e'.repeat(64);
  const SHORT = '4a4b4c4d';
  const all = slots(SHORT, 32);
  const keyNonce = (n: number) => '02' + all[n]!;
  type SlotsBody = {version: number; slot_count: number; free: {slot: number}[]; unkeyed_slots: number[]};
  type KeyAnswer = {ok: boolean; version: number; added: number[]; replayed: boolean; unkeyed_slots: number[]};

  async function split(over: Partial<Record<string, unknown>> = {}) {
    const token = await signIn();
    const till = await newTill(token);
    const batchId = await newBatch(token);
    const body = card({short_id: SHORT, batch_id: batchId, slot_count: 32, slot_pubkeys: all.slice(0, 8), ...over});
    const res = await post('/registry/cards', token, body);
    expect(res.status).toBe(200);
    return {token, till, batchId, body, stored: ((await res.json()) as {stored: {slot_count: number; slot_pubkeys: string[]}}).stored};
  }
  const range = (a: number, b: number) => Array.from({length: b - a}, (_, i) => a + i);

  it('registers slot_count 32 with 8 keys; state and /slots show 8-31 unkeyed and not free', async () => {
    const {till, token, batchId, body, stored} = await split();
    expect(stored.slot_count).toBe(32);
    expect(stored.slot_pubkeys).toHaveLength(8);
    const s = (await state(till.token, SHORT)) as unknown as {slot_count: number; slots: {slot: number}[]; unkeyed_slots: number[]};
    expect(s.slot_count).toBe(32);
    expect(s.slots.map(x => x.slot)).toEqual(range(0, 8));
    expect(s.unkeyed_slots).toEqual(range(8, 32));
    const free = (await (await get(`/cards/${SHORT}/slots`, null)).json()) as SlotsBody;
    expect(free.slot_count).toBe(32);
    expect(free.free.map(f => f.slot)).toEqual(range(0, 8));
    expect(free.unkeyed_slots).toEqual(range(8, 32));
    // The outbox's re-post replays; the same keys as an 8-slot card are a different card.
    expect((await post('/registry/cards', token, body)).status).toBe(200);
    expect((await post('/registry/cards', token, {...body, slot_count: 8})).status).toBe(409);
    expect((await post('/registry/cards', token, card({short_id: SHORT, batch_id: batchId, slot_pubkeys: all.slice(0, 8)}))).status).toBe(409);
  });

  it('takes slot_count with a prefix of 8 to 32 keys; refuses fewer than 8, more than the count, and a bad count', async () => {
    const token = await signIn();
    const batchId = await newBatch(token);
    const id = '6a6b6c6d';
    const k = slots(id, 32);
    const row = (over: Record<string, unknown>) => post('/registry/cards', token, card({short_id: id, batch_id: batchId, ...over}));
    for (const [count, n] of [[32, 7], [32, 33], [8, 7], [8, 9], [8, 32], [16, 16], ['32', 8]] as const) {
      const res = await row({slot_count: count, slot_pubkeys: slots(id, n)});
      expect(res.status, `${count}/${n}`).toBe(400);
    }
    // Explicit 8 with 8 keys is what an old client's 8 keys record.
    expect(((await (await row({slot_count: 8, slot_pubkeys: k.slice(0, 8)})).json()) as {stored: {slot_count: number}}).stored.slot_count).toBe(8);
    const id2 = '7a7b7c7d';
    const res = await post('/registry/cards', token, card({short_id: id2, batch_id: batchId, slot_count: 32, slot_pubkeys: slots(id2, 12)}));
    expect(((await res.json()) as {stored: {slot_count: number}}).stored.slot_count).toBe(32);
    const s = (await state(token, id2)) as unknown as {unkeyed_slots: number[]};
    expect(s.unkeyed_slots).toEqual(range(12, 32));
    // A key named twice is refused.
    const dup = slots('8a7b7c7d', 8);
    dup[3] = dup[2]!;
    expect((await post('/registry/cards', token, card({short_id: '8a7b7c7d', batch_id: batchId, slot_pubkeys: dup}))).status).toBe(400);
  });

  it('a till keys one slot; the same key again replays; another key is 409; version bumps once', async () => {
    const {till} = await split();
    const r1 = await post(`/cards/${SHORT}/slots/8/key`, till.token, {pubkey: all[8]});
    expect(r1.status).toBe(200);
    expect(await r1.json()).toEqual({ok: true, version: 1, added: [8], replayed: false, unkeyed_slots: range(9, 32)});
    const again = await post(`/cards/${SHORT}/slots/8/key`, till.token, {pubkey: all[8]});
    expect(await again.json()).toEqual({ok: true, version: 1, added: [], replayed: true, unkeyed_slots: range(9, 32)});
    const other = await post(`/cards/${SHORT}/slots/8/key`, till.token, {pubkey: key(99)});
    expect(other.status).toBe(409);
    expect(await other.text()).toContain('renewal');
    // A provisioned slot is keyed already: also 409.
    expect((await post(`/cards/${SHORT}/slots/3/key`, till.token, {pubkey: key(98)})).status).toBe(409);
    const free = (await (await get(`/cards/${SHORT}/slots`, null)).json()) as SlotsBody;
    expect(free.version).toBe(1);
    expect(free.free.map(f => f.slot)).toEqual(range(0, 9));
  });

  it('keys several slots in one call, all or nothing', async () => {
    const {till} = await split();
    const batch = range(8, 12).map(slot => ({slot, pubkey: all[slot]}));
    const ok = (await (await post(`/cards/${SHORT}/slot-keys`, till.token, {keys: batch})).json()) as KeyAnswer;
    expect(ok).toEqual({ok: true, version: 1, added: [8, 9, 10, 11], replayed: false, unkeyed_slots: range(12, 32)});
    // One bad entry (slot 8 with another key) refuses the whole call: 12 stays unkeyed.
    const mixed = await post(`/cards/${SHORT}/slot-keys`, till.token, {keys: [{slot: 12, pubkey: all[12]}, {slot: 8, pubkey: key(97)}]});
    expect(mixed.status).toBe(409);
    expect(((await state(till.token, SHORT)) as unknown as {unkeyed_slots: number[]}).unkeyed_slots).toEqual(range(12, 32));
    // A replayed prefix with one new slot adds just the new one.
    const more = (await (await post(`/cards/${SHORT}/slot-keys`, till.token, {keys: [{slot: 11, pubkey: all[11]}, {slot: 12, pubkey: all[12]}]})).json()) as KeyAnswer;
    expect(more).toMatchObject({version: 2, added: [12], replayed: false});
    // The rest, 13-31, in one call.
    const rest = (await (await post(`/cards/${SHORT}/slot-keys`, till.token, {keys: range(13, 32).map(slot => ({slot, pubkey: all[slot]}))})).json()) as KeyAnswer;
    expect(rest).toMatchObject({version: 3, unkeyed_slots: []});
    expect((await state(till.token, SHORT)).slots.map(s => s.pubkey)).toEqual(all);
  });

  it('validates: slot in range, pubkey format, no duplicates, no key already on the card', async () => {
    const {till} = await split();
    expect((await post(`/cards/${SHORT}/slots/32/key`, till.token, {pubkey: all[31]})).status).toBe(400);
    expect((await post(`/cards/${SHORT}/slots/9/key`, till.token, {pubkey: 'AB'.repeat(32)})).status).toBe(400);
    expect((await post(`/cards/${SHORT}/slots/9/key`, till.token, {pubkey: '02' + all[9]})).status).toBe(400);
    expect((await post(`/cards/${SHORT}/slots/9/key`, till.token, {})).status).toBe(400);
    expect((await post(`/cards/${SHORT}/slot-keys`, till.token, {keys: []})).status).toBe(400);
    expect((await post(`/cards/${SHORT}/slot-keys`, till.token, {keys: [{slot: 9, pubkey: all[9]}, {slot: 9, pubkey: all[10]}]})).status).toBe(400);
    expect((await post(`/cards/${SHORT}/slot-keys`, till.token, {keys: [{slot: 9, pubkey: all[9]}, {slot: 10, pubkey: all[9]}]})).status).toBe(400);
    const taken = await post(`/cards/${SHORT}/slots/9/key`, till.token, {pubkey: all[2]});
    expect(taken.status).toBe(409);
    expect(await taken.text()).toContain('another slot');
    // Nothing above changed the card.
    expect((await state(till.token, SHORT)).version).toBe(0);
  });

  it('an 8-slot card has nothing to key: slot 8 is out of range, slot 0-7 are keyed', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const batchId = await newBatch(token);
    await post('/registry/cards', token, card({short_id: '9a9b9c9d', batch_id: batchId}));
    const s = (await state(till.token, '9a9b9c9d')) as unknown as {slot_count: number; unkeyed_slots: number[]};
    expect(s).toMatchObject({slot_count: 8, unkeyed_slots: []});
    const res = await post('/cards/9a9b9c9d/slots/8/key', till.token, {pubkey: key(96)});
    expect(res.status).toBe(400);
    expect(await res.text()).toContain('0 to 7');
    expect((await post('/cards/9a9b9c9d/slots/7/key', till.token, {pubkey: key(96)})).status).toBe(409);
  });

  it('auth: a till or an operator of the card; no token 401, another operator’s till 404', async () => {
    const {token} = await split();
    expect((await post(`/cards/${SHORT}/slots/8/key`, null, {pubkey: all[8]})).status).toBe(401);
    await hook('/test/operator', {name: 'other', password: PASSWORD});
    const otherOp = await signIn('other');
    const otherTill = await newTill(otherOp, 'elsewhere');
    expect((await post(`/cards/${SHORT}/slots/8/key`, otherTill.token, {pubkey: all[8]})).status).toBe(404);
    expect((await post(`/cards/${SHORT}/slots/8/key`, otherOp, {pubkey: all[8]})).status).toBe(404);
    const byOperator = (await (await post(`/cards/${SHORT}/slots/8/key`, token, {pubkey: all[8]})).json()) as KeyAnswer;
    expect(byOperator).toMatchObject({version: 1, added: [8]});
    // An operator still cannot charge or renew.
    expect((await call('PUT', `/cards/${SHORT}/slots/8`, token, {version: 1, old_pubkey: all[8], pubkey: key(95)})).status).toBe(403);
  });

  it('refuses a card with no PIN (409), as every till write does', async () => {
    const {till} = await split({pin_set: false});
    expect((await post(`/cards/${SHORT}/slots/8/key`, till.token, {pubkey: all[8]})).status).toBe(409);
  });

  it('no load or change into an unkeyed slot until it is keyed; then both work', async () => {
    const {till} = await split();
    const holderLoad = (slot: number, version: number, t = txid(40)) => ({
      version,
      txid: t,
      notes: [{slot, amount_msat: 1_000, nonce: keyNonce(slot), blinding_key: BK, out_idx: 0}],
    });
    const refused = await post(`/cards/${SHORT}/load`, null, holderLoad(20, 0));
    expect(refused.status).toBe(409);
    expect(await refused.text()).toContain('no key in slot 20');
    await hook('/test/note', {short_id: SHORT, nonce: keyNonce(0), slot: 0, amount_msat: 1_024_000, signature: 'c'.repeat(96)});
    const change = (version: number, t = txid(41)) =>
      intent(version, {txid: t, inputs: [keyNonce(0)], change: [{slot: 21, amount_msat: 24_000, nonce: keyNonce(21), blinding_key: BK, out_idx: 1}]});
    const noChange = await post(`/cards/${SHORT}/intent`, till.token, change(0));
    expect(noChange.status).toBe(409);
    expect(await noChange.text()).toContain('no key in slot 21');

    const keyed = (await (await post(`/cards/${SHORT}/slot-keys`, till.token, {keys: [{slot: 20, pubkey: all[20]}, {slot: 21, pubkey: all[21]}]})).json()) as KeyAnswer;
    expect(keyed.version).toBe(1);
    expect((await post(`/cards/${SHORT}/intent`, till.token, change(1))).status).toBe(200);
    expect((await post(`/cards/${SHORT}/load`, null, holderLoad(20, 2))).status).toBe(200);
    // Renewing a never-keyed slot is not the way to key it.
    const renew = await call('PUT', `/cards/${SHORT}/slots/30`, till.token, {version: 3, old_pubkey: all[30], pubkey: key(94)});
    expect(renew.status).toBe(404);
    expect(await renew.text()).toContain('/slots/30/key');
  });

  it('refuses a key a note was once recorded under', async () => {
    const {till} = await split();
    await hook('/test/note', {short_id: SHORT, nonce: keyNonce(9), slot: 9, amount_msat: 1_000, signature: 'c'.repeat(96)});
    expect((await post(`/cards/${SHORT}/slots/9/key`, till.token, {pubkey: all[9]})).status).toBe(409);
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

  it('v9 gives existing cards 8 slots and rebuilds card_slots for 0-31 without losing a row', async () => {
    const v8 = env.REGISTRY.get(env.REGISTRY.idFromName('v8-upgrade'));
    await runInDurableObject(v8, async (_instance, state) => {
      const sql = state.storage.sql;
      for (const t of sql.exec("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '_cf_%'").toArray()) {
        sql.exec(`DROP TABLE IF EXISTS "${String(t.name)}"`);
      }
      // Exactly the schema v8 had, with two 8-slot cards on it, one renewed.
      for (const m of MIGRATIONS.slice(0, 8)) sql.exec(m);
      sql.exec('CREATE TABLE schema_version (version INTEGER NOT NULL)');
      sql.exec('INSERT INTO schema_version (version) VALUES (8)');
      sql.exec("INSERT INTO batches (federation_id, count, pin_policy, load_sha256, created_at) VALUES (?, 2, 'batch', ?, 1)", FED, SHA);
      const rows: [string, number, string][] = [];
      for (const id of ['11aa22bb', '33cc44dd']) {
        const keys = slots(id);
        sql.exec(
          "INSERT INTO cards (short_id, batch_id, installed_at, load_sha256, pin_set, pin_policy, notes_issued, received_at, slot_pubkeys) VALUES (?, 1, 1, ?, 1, 'batch', 0, 2, ?)",
          id, SHA, JSON.stringify(keys),
        );
        keys.forEach((k, slot) => rows.push([id, slot, slot === 5 && id === '33cc44dd' ? key(70) : k]));
      }
      for (const [id, slot, k] of rows) sql.exec('INSERT INTO card_slots (short_id, slot, pubkey) VALUES (?, ?, ?)', id, slot, k);
      expect(() => sql.exec("INSERT INTO card_slots (short_id, slot, pubkey) VALUES ('11aa22bb', 8, 'x')")).toThrow(/CHECK/);

      migrate(state.storage);
      expect(sql.exec('SELECT version FROM schema_version').one()).toEqual({version: MIGRATIONS.length});
      expect(sql.exec('SELECT short_id, slot_count FROM cards ORDER BY short_id').toArray()).toEqual([
        {short_id: '11aa22bb', slot_count: 8},
        {short_id: '33cc44dd', slot_count: 8},
      ]);
      const after = sql.exec('SELECT short_id, slot, pubkey FROM card_slots ORDER BY short_id, slot').toArray();
      expect(after.map(r => [r.short_id, r.slot, r.pubkey])).toEqual(rows);
      // The rebuilt table takes slots to 31, refuses 32, and keeps its key.
      sql.exec("INSERT INTO card_slots (short_id, slot, pubkey) VALUES ('11aa22bb', 31, 'x')");
      expect(() => sql.exec("INSERT INTO card_slots (short_id, slot, pubkey) VALUES ('11aa22bb', 32, 'x')")).toThrow(/CHECK/);
      expect(() => sql.exec("INSERT INTO card_slots (short_id, slot, pubkey) VALUES ('11aa22bb', 0, 'y')")).toThrow(/UNIQUE|PRIMARY/);
      expect(() => sql.exec("UPDATE cards SET slot_count = 16 WHERE short_id = '11aa22bb'")).toThrow(/CHECK/);
      expect(sql.exec("SELECT name FROM sqlite_master WHERE name = 'card_slots_v9'").toArray()).toEqual([]);
      migrate(state.storage); // a second run is a no-op
      expect(sql.exec('SELECT COUNT(*) AS n FROM card_slots').one()).toEqual({n: 17});
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
