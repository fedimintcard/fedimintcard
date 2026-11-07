import {env, SELF} from 'cloudflare:test';
import {beforeEach, describe, expect, it} from 'vitest';

import {bls12_381} from '@noble/curves/bls12-381.js';

import {decrypt} from '../src/backup';
import {fromHex, toHex} from '../src/http';
import {messageOf} from '../src/tbs';

/**
 * F4 (docs/OFFLINE-PROTOCOL.md § 2-3): the certificate, the offline-charge
 * report and the report log. Helpers mirror registry.test.ts.
 */

const PASSWORD = 'correct horse battery staple';
const SHA = 'a'.repeat(64);
const FED = 'b'.repeat(64);
const BACKUP_IDENTITY = 'AGE-SECRET-KEY-1AHS3RTK9NEEFMWRWJY6RQDF9HZZ9RQTD3UW0CNCX8YHQFSLN08LSECDQ2E';
const SHORT = '9c2f0a1b';

const key = (seed: number, tail = 'deadbeef') => seed.toString(16).padStart(2, '0').repeat(28) + tail;
const slots = (shortId: string) =>
  Array.from({length: 8}, (_, i) => key(i + 1, i === 0 ? shortId : i.toString(16).padStart(8, '0')));
const nonce = (n: number) => '02' + n.toString(16).padStart(2, '0').repeat(32);
const txid = (n: number) => n.toString(16).padStart(2, '0').repeat(32);

const TIERS = [1_000, 24_000, 1_024_000, 2_048_000, 4_096_000, 8_192_000];
const tierSecret = (amount: number) => BigInt(amount) * 0x1f2e3d4c5b6an + 7n;
const TBS_PKS = Object.fromEntries(
  TIERS.map(a => [String(a), toHex(bls12_381.G2.Point.BASE.multiply(tierSecret(a)).toBytes())]),
);
const sign = (n: string, amount: number) =>
  toHex(messageOf(fromHex(n)).multiply(tierSecret(amount)).toBytes());

function call(method: string, path: string, token: string | null, body?: unknown) {
  return SELF.fetch(`https://r.test${path}`, {
    method,
    headers: {'Content-Type': 'application/json', ...(token ? {Authorization: `Bearer ${token}`} : {})},
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}
const post = (path: string, token: string | null, body: unknown) => call('POST', path, token, body);
const get = (path: string, token: string | null) => call('GET', path, token);

const stub = () => env.REGISTRY.get(env.REGISTRY.idFromName('registry'));
const hook = (path: string, body?: unknown) =>
  stub().fetch(`https://registry.internal${path}`, {
    method: 'POST',
    body: body === undefined ? undefined : JSON.stringify(body),
  });

async function signIn(name = 'operator'): Promise<string> {
  const res = await post('/auth/operator/session', null, {name, password: PASSWORD});
  expect(res.status).toBe(200);
  return ((await res.json()) as {token: string}).token;
}

async function newTill(token: string, name = 'front desk') {
  const res = await post('/tills', token, {name});
  const till = (await res.json()) as {till_id: number; token: string};
  expect((await post('/federations', till.token, {federation_id: FED, tbs_pks: TBS_PKS})).status).toBe(200);
  return till;
}

/** A provisioned card with one live 1,024 sat note in slot 0 (nonce 1). */
async function cardWithNote(token: string, shortId = SHORT) {
  const res = await post('/registry/batches', token, {federation_id: FED, count: 5, pin_policy: 'batch', load_sha256: SHA});
  const batchId = ((await res.json()) as {batch: {batch_id: number}}).batch.batch_id;
  const card = {
    short_id: shortId, batch_id: batchId, installed_at: 1_760_000_000_000, load_sha256: SHA,
    pin_set: true, pin_policy: 'batch', notes_issued: 0, slot_pubkeys: slots(shortId),
  };
  expect((await post('/registry/cards', token, card)).status).toBe(200);
  await hook('/test/note', {short_id: shortId, nonce: nonce(1), slot: 0, amount_msat: 1_024_000, signature: 'c'.repeat(96)});
  return shortId;
}

async function state(token: string, shortId: string) {
  const res = await get(`/cards/${shortId}/state`, token);
  return (await res.json()) as {
    version: number;
    notes: {slot: number; nonce: string; status: string; txid: string | null}[];
    open_txs: {txid: string; signed_hex: string | null}[];
  };
}

/** A P-256 key pair; the card's attestation key as a 65-byte uncompressed point. */
async function attestKey() {
  const pair = (await crypto.subtle.generateKey({name: 'ECDSA', namedCurve: 'P-256'}, true, ['sign', 'verify'])) as CryptoKeyPair;
  return toHex(new Uint8Array((await crypto.subtle.exportKey('raw', pair.publicKey)) as ArrayBuffer));
}

function charge(over: Record<string, unknown> = {}) {
  return {
    txid: txid(7),
    client_ref: 'offline-1',
    tx_hex: 'ab'.repeat(100),
    inputs: [nonce(1)],
    change: [{slot: 1, amount_msat: 24_000, nonce: nonce(2), blinding_key: 'e'.repeat(64), out_idx: 1}],
    signed_hex: 'cd'.repeat(120),
    signed_at: 1_760_000_123_000,
    ...over,
  };
}

beforeEach(async () => {
  await hook('/test/reset');
});

describe('GET /registry/cert-key', () => {
  it('answers the public point, 65 bytes uncompressed, with no session', async () => {
    const res = await get('/registry/cert-key', null);
    expect(res.status).toBe(200);
    const body = (await res.json()) as {ok: boolean; key: string};
    expect(body.ok).toBe(true);
    expect(body.key).toMatch(/^04[0-9a-f]{128}$/);
    // The same point as the test key's own public half (vitest.config.ts).
    const priv = await crypto.subtle.importKey(
      'pkcs8',
      Uint8Array.from(atob(env.CERT_SIGNING_KEY!), c => c.charCodeAt(0)),
      {name: 'ECDSA', namedCurve: 'P-256'}, true, ['sign'],
    );
    const jwk = (await crypto.subtle.exportKey('jwk', priv)) as JsonWebKey;
    const b64 = (t: string) => Uint8Array.from(atob(t.replace(/-/g, '+').replace(/_/g, '/')), c => c.charCodeAt(0));
    expect(body.key).toBe(toHex(new Uint8Array([4, ...b64(jwk.x!), ...b64(jwk.y!)])));
  });
});

describe('POST /registry/cards/:id/attestation', () => {
  it('answers a 130-byte certificate whose signature verifies under the cert key', async () => {
    const token = await signIn();
    const shortId = await cardWithNote(token);
    const attest = await attestKey();
    const res = await post(`/registry/cards/${shortId}/attestation`, token, {attest_pub: attest});
    expect(res.status).toBe(200);
    const {cert} = (await res.json()) as {cert: string};
    const bytes = fromHex(cert);
    expect(bytes.length).toBe(130);
    expect(bytes[0]).toBe(1);
    expect(toHex(bytes.subarray(1, 33))).toBe(slots(shortId)[0]);
    const compressed = bytes.subarray(33, 66);
    const yOdd = parseInt(attest.slice(-2), 16) % 2 === 1;
    expect(compressed[0]).toBe(yOdd ? 3 : 2);
    expect(toHex(compressed.subarray(1))).toBe(attest.slice(2, 66));
    const certKey = ((await (await get('/registry/cert-key', null)).json()) as {key: string}).key;
    const pub = await crypto.subtle.importKey('raw', fromHex(certKey), {name: 'ECDSA', namedCurve: 'P-256'}, false, ['verify']);
    const signed = new Uint8Array([...new TextEncoder().encode('fedimintcard/cert1'), ...bytes.subarray(1, 66)]);
    expect(await crypto.subtle.verify({name: 'ECDSA', hash: 'SHA-256'}, pub, bytes.subarray(66), signed)).toBe(true);
    // The tampered message does not verify.
    signed[20] = signed[20]! ^ 1;
    expect(await crypto.subtle.verify({name: 'ECDSA', hash: 'SHA-256'}, pub, bytes.subarray(66), signed)).toBe(false);
  });

  it('covers both parities of y in the compressed key', async () => {
    const token = await signIn();
    const shortId = await cardWithNote(token);
    const seen = new Set<number>();
    for (let i = 0; i < 40 && seen.size < 2; i += 1) {
      const attest = await attestKey();
      // No notes would block a replacement; clear them so keys can be swapped freely.
      await hook('/test/reset-notes');
      const res = await post(`/registry/cards/${shortId}/attestation`, token, {attest_pub: attest});
      expect(res.status).toBe(200);
      const cert = fromHex(((await res.json()) as {cert: string}).cert);
      expect(cert[33]).toBe(parseInt(attest.slice(-2), 16) % 2 === 1 ? 3 : 2);
      seen.add(cert[33]!);
    }
    expect([...seen].sort()).toEqual([2, 3]);
  });

  it('answers the same key with the same certificate bytes', async () => {
    const token = await signIn();
    const shortId = await cardWithNote(token);
    const attest = await attestKey();
    const first = (await (await post(`/registry/cards/${shortId}/attestation`, token, {attest_pub: attest})).json()) as {cert: string};
    const again = (await (await post(`/registry/cards/${shortId}/attestation`, token, {attest_pub: attest})).json()) as {cert: string};
    expect(again.cert).toBe(first.cert);
  });

  it('refuses a different key while the card holds live notes, allows it when it holds none', async () => {
    const token = await signIn();
    const shortId = await cardWithNote(token);
    const a = await attestKey();
    const b = await attestKey();
    const first = (await (await post(`/registry/cards/${shortId}/attestation`, token, {attest_pub: a})).json()) as {cert: string};
    const refused = await post(`/registry/cards/${shortId}/attestation`, token, {attest_pub: b});
    expect(refused.status).toBe(409);
    // Still the first certificate.
    expect(((await (await post(`/registry/cards/${shortId}/attestation`, token, {attest_pub: a})).json()) as {cert: string}).cert).toBe(first.cert);
    await hook('/test/reset-notes');
    const replaced = await post(`/registry/cards/${shortId}/attestation`, token, {attest_pub: b});
    expect(replaced.status).toBe(200);
    expect(((await replaced.json()) as {cert: string}).cert).not.toBe(first.cert);
  });

  it('refuses a pending note like a live one', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const shortId = await cardWithNote(token);
    await post(`/registry/cards/${shortId}/attestation`, token, {attest_pub: await attestKey()});
    expect((await post(`/cards/${shortId}/offline-charges`, till.token, charge())).status).toBe(200);
    // Both the input and the change are pending now; neither is live, and the key is still held.
    expect((await post(`/registry/cards/${shortId}/attestation`, token, {attest_pub: await attestKey()})).status).toBe(409);
  });

  it('rejects a point that is not on P-256, and malformed keys, with 400', async () => {
    const token = await signIn();
    const shortId = await cardWithNote(token);
    for (const bad of ['04' + '11'.repeat(64), '04' + '00'.repeat(64), 'zz', '03' + '22'.repeat(32), undefined]) {
      const res = await post(`/registry/cards/${shortId}/attestation`, token, {attest_pub: bad});
      expect(res.status).toBe(400);
    }
  });

  it('is 401 with no session, 403 for a till, 404 for another operator’s card', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const shortId = await cardWithNote(token);
    const attest = await attestKey();
    expect((await post(`/registry/cards/${shortId}/attestation`, null, {attest_pub: attest})).status).toBe(401);
    expect((await post(`/registry/cards/${shortId}/attestation`, till.token, {attest_pub: attest})).status).toBe(403);
    await hook('/test/operator', {name: 'other', password: PASSWORD});
    const other = await signIn('other');
    expect((await post(`/registry/cards/${shortId}/attestation`, other, {attest_pub: attest})).status).toBe(404);
    expect((await post('/registry/cards/ffffffff/attestation', token, {attest_pub: attest})).status).toBe(404);
  });
});

describe('POST /cards/:id/offline-charges', () => {
  it('records an open signed charge: inputs pending, change pending, version bumped', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const shortId = await cardWithNote(token);
    const before = await state(till.token, shortId);
    const res = await post(`/cards/${shortId}/offline-charges`, till.token, charge());
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({ok: true, version: before.version + 1});
    const after = await state(till.token, shortId);
    expect(after.version).toBe(before.version + 1);
    expect(after.notes.map(n => [n.nonce, n.status, n.txid])).toEqual([
      [nonce(1), 'pending', txid(7)],
      [nonce(2), 'pending', txid(7)],
    ]);
    expect(after.open_txs).toEqual([expect.objectContaining({txid: txid(7), signed_hex: 'cd'.repeat(120)})]);
  });

  it('needs no version, and records after the card’s version moved on', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const shortId = await cardWithNote(token);
    // Move the version: an online charge on another note, recorded then rejected.
    await hook('/test/note', {short_id: shortId, nonce: nonce(5), slot: 2, amount_msat: 2_048_000, signature: 'c'.repeat(96)});
    const online = {
      version: 0, txid: txid(9), client_ref: 'online-1', tx_hex: 'ab'.repeat(50), inputs: [nonce(5)],
      change: [{slot: 3, amount_msat: 24_000, nonce: nonce(6), blinding_key: 'e'.repeat(64), out_idx: 1}],
    };
    expect((await post(`/cards/${shortId}/intent`, till.token, online)).status).toBe(200);
    expect((await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(9), accepted: false})).status).toBe(200);
    expect((await state(till.token, shortId)).version).toBe(2);
    const res = await post(`/cards/${shortId}/offline-charges`, till.token, charge({version: 0}));
    expect(res.status).toBe(200);
  });

  it('is not blocked by another till’s open charge', async () => {
    const token = await signIn();
    const first = await newTill(token, 'first');
    const second = await newTill(token, 'second');
    const shortId = await cardWithNote(token);
    await hook('/test/note', {short_id: shortId, nonce: nonce(5), slot: 2, amount_msat: 2_048_000, signature: 'c'.repeat(96)});
    const intent = {
      version: 0, txid: txid(9), client_ref: 'online-1', tx_hex: 'ab'.repeat(50), inputs: [nonce(5)],
      change: [{slot: 3, amount_msat: 24_000, nonce: nonce(6), blinding_key: 'e'.repeat(64), out_idx: 1}],
    };
    expect((await post(`/cards/${shortId}/intent`, first.token, intent)).status).toBe(200);
    expect((await state(second.token, shortId)).open_txs.length).toBe(1);
    expect((await post(`/cards/${shortId}/offline-charges`, second.token, charge())).status).toBe(200);
    expect((await state(second.token, shortId)).open_txs.length).toBe(2);
  });

  it('answers a double spend 409 with the conflicts, and keeps a report', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const shortId = await cardWithNote(token);
    expect((await post(`/cards/${shortId}/offline-charges`, till.token, charge())).status).toBe(200);
    // The same note again under another txid, plus a nonce the registry never saw.
    const second = charge({txid: txid(8), client_ref: 'offline-2', inputs: [nonce(1), nonce(77)],
      change: [{slot: 3, amount_msat: 1_000, nonce: nonce(9), blinding_key: 'e'.repeat(64), out_idx: 1}]});
    const res = await post(`/cards/${shortId}/offline-charges`, till.token, second);
    expect(res.status).toBe(409);
    expect(await res.json()).toEqual({
      ok: false,
      reason: 'double_spend',
      conflicts: [
        {nonce: nonce(1), status: 'pending', txid: txid(7)},
        {nonce: nonce(77), status: 'missing', txid: null},
      ],
    });
    // Nothing but the report was recorded.
    const s = await state(till.token, shortId);
    expect(s.open_txs.map(t => t.txid)).toEqual([txid(7)]);
    expect(s.notes.find(n => n.nonce === nonce(9))).toBeUndefined();
    const reports = ((await (await get('/registry/offline-reports', token)).json()) as {reports: {txid: string; result: string}[]}).reports;
    expect(reports.map(r => [r.txid, r.result]).sort()).toEqual([[txid(7), 'recorded'], [txid(8), 'double_spend']]);
  });

  it('is idempotent: a replay of either outcome answers as before', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const shortId = await cardWithNote(token);
    const ok1 = await (await post(`/cards/${shortId}/offline-charges`, till.token, charge())).json();
    const ok2 = await post(`/cards/${shortId}/offline-charges`, till.token, charge());
    expect(ok2.status).toBe(200);
    expect(await ok2.json()).toEqual(ok1);
    const bad = charge({txid: txid(8), client_ref: 'offline-2', change: []});
    const d1 = await post(`/cards/${shortId}/offline-charges`, till.token, bad);
    expect(d1.status).toBe(409);
    const d1b = await d1.json();
    const d2 = await post(`/cards/${shortId}/offline-charges`, till.token, bad);
    expect(d2.status).toBe(409);
    expect(await d2.json()).toEqual(d1b);
    // One report row per txid, not per attempt.
    const reports = ((await (await get('/registry/offline-reports', token)).json()) as {reports: unknown[]}).reports;
    expect(reports.length).toBe(2);
  });

  it('is then resolved by /outcome: accepted spends the inputs and makes the change live', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const shortId = await cardWithNote(token);
    expect((await post(`/cards/${shortId}/offline-charges`, till.token, charge())).status).toBe(200);
    const out = await post(`/cards/${shortId}/outcome`, till.token, {
      txid: txid(7), accepted: true, change: [{nonce: nonce(2), signature: sign(nonce(2), 24_000)}],
    });
    expect(out.status).toBe(200);
    const s = await state(till.token, shortId);
    expect(s.open_txs).toEqual([]);
    const byNonce = Object.fromEntries(s.notes.map(n => [n.nonce, n.status]));
    expect(byNonce[nonce(2)]).toBe('live');
    expect(byNonce[nonce(1)]).toBeUndefined(); // spent and no longer on a keyed slot's current note list
    // The input is spent, so reporting it again is a double spend.
    const again = await post(`/cards/${shortId}/offline-charges`, till.token, charge({txid: txid(8), change: []}));
    expect(again.status).toBe(409);
    expect(((await again.json()) as {conflicts: {status: string}[]}).conflicts[0]!.status).toBe('spent');
  });

  it('rejects malformed bodies and a till-less caller', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const shortId = await cardWithNote(token);
    for (const bad of [
      charge({signed_hex: undefined}), charge({signed_hex: 'ABCD'}), charge({signed_hex: ''}), charge({signed_hex: 'abc'}),
      charge({signed_at: 0}), charge({signed_at: 1.5}), charge({signed_at: undefined}),
      charge({inputs: [], change: [{slot: 1, amount_msat: 1_000, nonce: nonce(3), blinding_key: 'e'.repeat(64), out_idx: 0}]}),
    ]) {
      expect((await post(`/cards/${shortId}/offline-charges`, till.token, bad)).status).toBe(400);
    }
    expect((await post(`/cards/${shortId}/offline-charges`, null, charge())).status).toBe(401);
    expect((await post(`/cards/${shortId}/offline-charges`, token, charge())).status).toBe(403);
  });
});

describe('GET /registry/offline-reports', () => {
  it('lists newest first, filters by short_id, and is operator-only and scoped', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const a = await cardWithNote(token, SHORT);
    const b = await cardWithNote(token, '11aa22bb');
    await post(`/cards/${a}/offline-charges`, till.token, charge());
    await post(`/cards/${a}/offline-charges`, till.token, charge({txid: txid(8), change: []}));
    await post(`/cards/${b}/offline-charges`, till.token, charge({txid: txid(9)}));
    type R = {reports: {txid: string; short_id: string; result: string; till_id: number; detail: {conflicts?: unknown[]}}[]};
    const all = (await (await get('/registry/offline-reports', token)).json()) as R;
    expect(all.reports.length).toBe(3);
    const times = all.reports.map(r => r.txid);
    expect(times[0]).toBe(txid(9)); // newest
    const only = (await (await get(`/registry/offline-reports?short_id=${a}`, token)).json()) as R;
    expect(only.reports.map(r => [r.txid, r.result])).toEqual([[txid(8), 'double_spend'], [txid(7), 'recorded']]);
    expect(only.reports[0]!.till_id).toBe(till.till_id);
    expect(only.reports[0]!.detail.conflicts).toEqual([{nonce: nonce(1), status: 'pending', txid: txid(7)}]);
    expect((await get('/registry/offline-reports?short_id=zz', token)).status).toBe(400);
    expect((await get('/registry/offline-reports', till.token)).status).toBe(403);
    expect((await get('/registry/offline-reports', null)).status).toBe(401);
    await hook('/test/operator', {name: 'other', password: PASSWORD});
    const other = await signIn('other');
    expect(((await (await get('/registry/offline-reports', other)).json()) as R).reports).toEqual([]);
  });
});

describe('backup', () => {
  it('carries offline_reports and the attestation columns', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const shortId = await cardWithNote(token);
    const attest = await attestKey();
    await post(`/registry/cards/${shortId}/attestation`, token, {attest_pub: attest});
    await post(`/cards/${shortId}/offline-charges`, till.token, charge());
    const res = await stub().fetch('https://registry.internal/internal/backup', {method: 'POST'});
    const {key: objectKey} = (await res.json()) as {key: string};
    const snap = await decrypt(await (await env.BACKUPS!.get(objectKey))!.text(), BACKUP_IDENTITY);
    expect(snap.tables.offline_reports).toEqual([expect.objectContaining({txid: txid(7), short_id: shortId, result: 'recorded'})]);
    expect(snap.tables.cards![0]).toEqual(expect.objectContaining({attest_pub: attest, attest_cert: expect.stringMatching(/^[0-9a-f]{260}$/)}));
  });
});
