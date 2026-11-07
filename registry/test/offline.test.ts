import {env, SELF} from 'cloudflare:test';
import {beforeEach, describe, expect, it} from 'vitest';

import {bls12_381} from '@noble/curves/bls12-381.js';

import {p256} from '@noble/curves/nist.js';

import {decrypt} from '../src/backup';
import {fromHex, toHex} from '../src/http';
import {derToRaw, rawToDer, releaseMessage} from '../src/offline';
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

  describe('before the card is registered (the install tap asks first)', () => {
    async function batchOf(token: string) {
      const res = await post('/registry/batches', token, {federation_id: FED, count: 5, pin_policy: 'batch', load_sha256: SHA});
      return ((await res.json()) as {batch: {batch_id: number}}).batch.batch_id;
    }
    const register = (token: string, batchId: number, shortId: string, slotKeys = slots(shortId)) =>
      post('/registry/cards', token, {
        short_id: shortId, batch_id: batchId, installed_at: 1_760_000_000_000, load_sha256: SHA,
        pin_set: true, pin_policy: 'batch', notes_issued: 0, slot_pubkeys: slotKeys,
      });

    it('certifies the slot 0 key the card names, and the card keeps that certificate once registered', async () => {
      const token = await signIn();
      const shortId = '1e1b01ef';
      const slot0 = slots(shortId)[0]!;
      const attest = await attestKey();
      const res = await post(`/registry/cards/${shortId}/attestation`, token, {attest_pub: attest, slot0});
      expect(res.status).toBe(200);
      const {cert} = (await res.json()) as {cert: string};
      expect(cert.slice(2, 66)).toBe(slot0);
      // the same request again before registering: the same bytes
      const again = (await (await post(`/registry/cards/${shortId}/attestation`, token, {attest_pub: attest, slot0})).json()) as {cert: string};
      expect(again.cert).toBe(cert);
      expect((await register(token, await batchOf(token), shortId)).status).toBe(200);
      // registered: the card's own row answers the same certificate, so it was adopted
      const stored = (await (await post(`/registry/cards/${shortId}/attestation`, token, {attest_pub: attest})).json()) as {cert: string};
      expect(stored.cert).toBe(cert);
    });

    it('is not adopted by a card registered with another slot 0', async () => {
      const token = await signIn();
      const shortId = '2e2b02ef';
      const attest = await attestKey();
      const {cert} = (await (await post(`/registry/cards/${shortId}/attestation`, token, {attest_pub: attest, slot0: slots(shortId)[0]})).json()) as {cert: string};
      const other = slots(shortId);
      other[0] = key(9, shortId);
      expect((await register(token, await batchOf(token), shortId, other)).status).toBe(200);
      const fresh = (await (await post(`/registry/cards/${shortId}/attestation`, token, {attest_pub: attest})).json()) as {cert: string};
      expect(fresh.cert).not.toBe(cert);
      expect(fresh.cert.slice(2, 66)).toBe(other[0]);
    });

    it('refuses a slot 0 that is not the short id’s, a missing one, and another operator’s set-up', async () => {
      const token = await signIn();
      const attest = await attestKey();
      expect((await post('/registry/cards/3e3b03ef/attestation', token, {attest_pub: attest, slot0: slots('ffffffff')[0]})).status).toBe(400);
      expect((await post('/registry/cards/3e3b03ef/attestation', token, {attest_pub: attest})).status).toBe(404);
      expect((await post('/registry/cards/3e3b03ef/attestation', token, {attest_pub: attest, slot0: slots('3e3b03ef')[0]})).status).toBe(200);
      await hook('/test/operator', {name: 'other', password: PASSWORD});
      const other = await signIn('other');
      expect((await post('/registry/cards/3e3b03ef/attestation', other, {attest_pub: attest, slot0: slots('3e3b03ef')[0]})).status).toBe(409);
    });

    it('refuses a registered card’s slot 0 that is not the key named', async () => {
      const token = await signIn();
      const shortId = await cardWithNote(token, '4e4b04ef');
      expect((await post(`/registry/cards/${shortId}/attestation`, token, {attest_pub: await attestKey(), slot0: key(9, shortId)})).status).toBe(409);
    });
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

describe('releases (docs/OFFLINE-PROTOCOL.md § 6)', () => {
  type Release = {slot: number; txid: string; sig: string};
  const releasesOf = async (tok: string, shortId: string) =>
    ((await (await get(`/cards/${shortId}/state`, tok)).json()) as {releases: Release[]}).releases;

  /** Each release verifies under the cert key over label ‖ slot 0 ‖ slot ‖ txid, with WebCrypto and noble. */
  async function expectSigned(shortId: string, rels: Release[]) {
    const certKey = ((await (await get('/registry/cert-key', null)).json()) as {key: string}).key;
    const pub = await crypto.subtle.importKey('raw', fromHex(certKey), {name: 'ECDSA', namedCurve: 'P-256'}, false, ['verify']);
    for (const r of rels) {
      const msg = releaseMessage(slots(shortId)[0]!, r.slot, r.txid);
      expect(msg.length).toBe(82);
      const der = fromHex(r.sig);
      expect(toHex(rawToDer(derToRaw(der)))).toBe(r.sig);
      expect(await crypto.subtle.verify({name: 'ECDSA', hash: 'SHA-256'}, pub, derToRaw(der), msg)).toBe(true);
      expect(p256.verify(der, msg, fromHex(certKey), {format: 'der', lowS: false})).toBe(true);
      // Not transferable to another slot.
      expect(await crypto.subtle.verify({name: 'ECDSA', hash: 'SHA-256'}, pub, derToRaw(der), releaseMessage(slots(shortId)[0]!, r.slot + 1, r.txid))).toBe(false);
    }
  }

  const online = (over: Record<string, unknown> = {}) => ({
    version: 0, txid: txid(9), client_ref: 'online-1', tx_hex: 'ab'.repeat(50), inputs: [nonce(1)],
    change: [{slot: 3, amount_msat: 24_000, nonce: nonce(6), blinding_key: 'e'.repeat(64), out_idx: 1}],
    ...over,
  });

  describe('DER', () => {
    const raw = (r: number[], s: number[]) => {
      const out = new Uint8Array(64);
      out.set(r, 32 - r.length);
      out.set(s, 64 - s.length);
      return out;
    };
    it('is minimal and positive: leading zeros stripped, 00 before a high bit', () => {
      // r = 1 (31 leading zero bytes), s = 0x80 (high bit after stripping)
      expect(toHex(rawToDer(raw([1], [0x80])))).toBe('3007020101020200' + '80');
      // r has its own high bit: 33 bytes; s = 0x7f..: 32 bytes, no pad
      const hi = new Uint8Array(32).fill(0xff);
      const lo = new Uint8Array(32).fill(0x7f);
      const der = rawToDer(raw([...hi], [...lo]));
      expect(toHex(der.subarray(0, 5))).toBe('3045022100');
      expect(der.length).toBe(71);
      // leading 00 followed by a high-bit byte: stripped to 31 bytes, then padded to 32
      const z = [0x00, 0x80, ...new Array(30).fill(1)];
      const d2 = rawToDer(raw(z, z));
      expect(toHex(d2.subarray(0, 4))).toBe('30440220');
      expect(d2[4]).toBe(0);
      expect(d2[5]).toBe(0x80);
      // r = 0 is one zero byte
      expect(toHex(rawToDer(raw([], [1])))).toBe('3006020100020101');
      for (const x of [raw([1], [0x80]), raw([...hi], [...lo]), raw(z, z), raw([], [1])]) {
        expect(toHex(derToRaw(rawToDer(x)))).toBe(toHex(x));
      }
    });

    it('refuses non-minimal, negative and malformed DER', () => {
      for (const bad of ['3008020200010202000180', '3006020180020101', '3006020101020101ff', '3106020101020101', '300602010102']) {
        expect(() => derToRaw(fromHex(bad))).toThrow();
      }
      expect(() => rawToDer(new Uint8Array(63))).toThrow();
      // and the minimal form of the same values parses
      expect(toHex(derToRaw(fromHex('3006020101020101')))).toBe('00'.repeat(31) + '01' + '00'.repeat(31) + '01');
    });

    it('carries a WebCrypto signature that verifies after the round trip, and in noble', async () => {
      const pair = (await crypto.subtle.generateKey({name: 'ECDSA', namedCurve: 'P-256'}, true, ['sign', 'verify'])) as CryptoKeyPair;
      const pub = new Uint8Array((await crypto.subtle.exportKey('raw', pair.publicKey)) as ArrayBuffer);
      for (let i = 0; i < 20; i += 1) {
        const msg = new Uint8Array([i, 1, 2, 3]);
        const sig = new Uint8Array(await crypto.subtle.sign({name: 'ECDSA', hash: 'SHA-256'}, pair.privateKey, msg));
        const der = rawToDer(sig);
        expect(await crypto.subtle.verify({name: 'ECDSA', hash: 'SHA-256'}, pair.publicKey, derToRaw(der), msg)).toBe(true);
        expect(p256.verify(der, msg, pub, {format: 'der', lowS: false})).toBe(true);
      }
    });
  });

  it('a rejected online charge: a release for its input slot and its change slot, listed in the state', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const shortId = await cardWithNote(token);
    expect((await post(`/cards/${shortId}/intent`, till.token, online())).status).toBe(200);
    const res = await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(9), accepted: false});
    expect(res.status).toBe(200);
    expect(Object.keys((await res.json()) as object).sort()).toEqual(['ok', 'replayed', 'version']);
    const rels = await releasesOf(till.token, shortId);
    expect(rels.map(r => [r.slot, r.txid])).toEqual([[0, txid(9)], [3, txid(9)]]);
    await expectSigned(shortId, rels);
    // The input stays live; a replay of the outcome adds nothing.
    expect((await state(till.token, shortId)).notes.find(n => n.nonce === nonce(1))?.status).toBe('live');
    expect((await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(9), accepted: false})).status).toBe(200);
    expect(await releasesOf(till.token, shortId)).toEqual(rels);
  });

  it('an accepted charge, and a load, leave no release', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const shortId = await cardWithNote(token);
    expect((await post(`/cards/${shortId}/intent`, till.token, online())).status).toBe(200);
    const sig = sign(nonce(6), 24_000);
    expect((await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(9), accepted: true, change: [{nonce: nonce(6), signature: sig}]})).status).toBe(200);
    const load = online({version: 2, txid: txid(10), inputs: [], tx_hex: undefined,
      change: [{slot: 4, amount_msat: 24_000, nonce: nonce(8), blinding_key: 'e'.repeat(64), out_idx: 0}]});
    expect((await post(`/cards/${shortId}/intent`, till.token, load)).status).toBe(200);
    expect((await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(10), accepted: false})).status).toBe(200);
    expect(await releasesOf(till.token, shortId)).toEqual([]);
  });

  it('a rejected offline charge: its inputs are live again, with releases for input and change', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const shortId = await cardWithNote(token);
    expect((await post(`/cards/${shortId}/offline-charges`, till.token, charge())).status).toBe(200);
    expect((await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(7), accepted: false})).status).toBe(200);
    const notes = (await state(till.token, shortId)).notes;
    expect(notes.find(n => n.nonce === nonce(1))?.status).toBe('live');
    expect(notes.find(n => n.nonce === nonce(2))).toBeUndefined(); // dead, and not on a slot's key
    const rels = await releasesOf(till.token, shortId);
    expect(rels.map(r => [r.slot, r.txid])).toEqual([[0, txid(7)], [1, txid(7)]]);
    await expectSigned(shortId, rels);
    // The note is live: a new charge may spend it.
    expect((await post(`/cards/${shortId}/intent`, till.token, online({version: 2}))).status).toBe(200);
  });

  it('an offline double spend: releases for the change slots it named only, once', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const shortId = await cardWithNote(token);
    expect((await post(`/cards/${shortId}/offline-charges`, till.token, charge())).status).toBe(200);
    const twice = charge({
      txid: txid(8), client_ref: 'offline-2',
      change: [
        {slot: 4, amount_msat: 24_000, nonce: nonce(12), blinding_key: 'e'.repeat(64), out_idx: 1},
        {slot: 5, amount_msat: 24_000, nonce: nonce(13), blinding_key: 'e'.repeat(64), out_idx: 2},
      ],
    });
    expect((await post(`/cards/${shortId}/offline-charges`, till.token, twice)).status).toBe(409);
    const rels = await releasesOf(till.token, shortId);
    expect(rels.map(r => [r.slot, r.txid])).toEqual([[4, txid(8)], [5, txid(8)]]);
    await expectSigned(shortId, rels);
    // The replay answers the stored conflicts and signs nothing new.
    expect((await post(`/cards/${shortId}/offline-charges`, till.token, twice)).status).toBe(409);
    expect(await releasesOf(till.token, shortId)).toEqual(rels);
    // The first charge's input stays pending: it was spent, by txid(7).
    expect((await state(till.token, shortId)).notes.find(n => n.nonce === nonce(1))?.status).toBe('pending');
  });

  it('goes when the slot is keyed anew, and all of them when the card is reset', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const shortId = await cardWithNote(token);
    expect((await post(`/cards/${shortId}/intent`, till.token, online())).status).toBe(200);
    expect((await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(9), accepted: false})).status).toBe(200);
    expect((await releasesOf(till.token, shortId)).map(r => r.slot)).toEqual([0, 3]);
    const renew = {version: 2, old_pubkey: slots(shortId)[3], pubkey: key(0x33)};
    expect((await call('PUT', `/cards/${shortId}/slots/3`, till.token, renew)).status).toBe(200);
    expect((await releasesOf(till.token, shortId)).map(r => r.slot)).toEqual([0]);
    const reset = {short_id: shortId, chip: 'cplc-v1:' + '4'.repeat(24), gp_key_version: 1, manager_keys: 'factory', package: 'absent', verified_at: 1_770_000_000_000};
    expect((await post(`/registry/cards/${shortId}/reset`, token, reset)).status).toBe(200);
    expect(await releasesOf(token, shortId)).toEqual([]);
  });

  it('without CERT_SIGNING_KEY the outcome and the refusal stand, with no release', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const shortId = await cardWithNote(token);
    await hook('/test/cert-key', {off: true});
    expect((await post(`/cards/${shortId}/intent`, till.token, online())).status).toBe(200);
    expect((await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(9), accepted: false})).status).toBe(200);
    expect((await state(till.token, shortId)).open_txs).toEqual([]);
    await hook('/test/note', {short_id: shortId, nonce: nonce(20), slot: 6, amount_msat: 1_024_000, signature: 'c'.repeat(96)});
    expect((await post(`/cards/${shortId}/offline-charges`, till.token, charge({txid: txid(11), inputs: [nonce(20)]}))).status).toBe(200);
    expect((await post(`/cards/${shortId}/offline-charges`, till.token, charge({txid: txid(12), client_ref: 'x', inputs: [nonce(20)]}))).status).toBe(409);
    expect(await releasesOf(till.token, shortId)).toEqual([]);
  });

  it('is in the backup', async () => {
    const token = await signIn();
    const till = await newTill(token);
    const shortId = await cardWithNote(token);
    await post(`/cards/${shortId}/intent`, till.token, online());
    await post(`/cards/${shortId}/outcome`, till.token, {txid: txid(9), accepted: false});
    const res = await stub().fetch('https://registry.internal/internal/backup', {method: 'POST'});
    const {key: objectKey} = (await res.json()) as {key: string};
    const snap = await decrypt(await (await env.BACKUPS!.get(objectKey))!.text(), BACKUP_IDENTITY);
    expect(snap.tables.releases!.map(r => [r.short_id, r.slot, r.txid])).toEqual([[shortId, 0, txid(9)], [shortId, 3, txid(9)]]);
  });
});
