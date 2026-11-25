import {SELF} from 'cloudflare:test';
import {afterEach, describe, expect, it, vi} from 'vitest';

import {sha256} from '@noble/hashes/sha2.js';

import {boundedFetch, CHAIN_TIP_TTL_MS, chainTip, MAX_BODY_BYTES, newTipCache, SOURCES, type TipFetch} from '../src/chain';
import {toHex} from '../src/http';

/**
 * `GET /v1/chain/tip` (DESIGN § 4, "Header source: the registry"). Every test answers the two
 * sources with fakes: nothing here reaches mempool.space or blockstream.info.
 */

/** An 80-byte header whose only varying field is the nonce, and its block hash (display order). */
function block(n: number): {hash: string; header: string} {
  const h = new Uint8Array(80);
  h[0] = 0x04;
  new DataView(h.buffer).setUint32(76, n, true);
  const hash = toHex(sha256(sha256(h)).reverse());
  return {hash, header: toHex(h)};
}

const A = SOURCES[0]!.base;
const B = SOURCES[1]!.base;

/** A fake fetch over a table: base -> {tip, headers}; a missing base or `down` throws; counts calls. */
function fake(table: Record<string, {tips: string[]; headers?: Record<string, string>} | 'down'>) {
  const calls: string[] = [];
  const served: Record<string, number> = {};
  const fetch: TipFetch = async url => {
    calls.push(url);
    for (const [base, src] of Object.entries(table)) {
      if (!url.startsWith(base)) continue;
      if (src === 'down') throw new Error('connection refused');
      if (url === `${base}/blocks/tip/hash`) {
        const i = served[base] ?? 0;
        served[base] = i + 1;
        return src.tips[Math.min(i, src.tips.length - 1)]!;
      }
      const m = /\/block\/([0-9a-f]{64})\/header$/.exec(url);
      const header = m ? src.headers?.[m[1]!] : undefined;
      if (header === undefined) throw new Error('404');
      return header;
    }
    throw new Error(`unexpected ${url}`);
  };
  return {fetch, calls};
}

function both(b: {hash: string; header: string}) {
  return {tips: [b.hash], headers: {[b.hash]: b.header}};
}

describe('chainTip', () => {
  const t0 = 1_700_000_000_000;

  it('serves the tip when both sources agree on the hash', async () => {
    const b = block(1);
    const {fetch} = fake({[A]: both(b), [B]: both(b)});
    const r = await chainTip(newTipCache(), fetch, t0);
    expect(r).toEqual({ok: true, hash: b.hash, header: b.header, sources: ['mempool.space', 'blockstream.info']});
  });

  it('serves the other source when one is down, either one', async () => {
    const b = block(2);
    for (const down of [A, B]) {
      const up = down === A ? B : A;
      const {fetch} = fake({[down]: 'down', [up]: both(b)});
      const r = await chainTip(newTipCache(), fetch, t0);
      expect(r).toEqual({
        ok: true,
        hash: b.hash,
        header: b.header,
        sources: [up === A ? 'mempool.space' : 'blockstream.info'],
      });
    }
  });

  it('answers 503 when both are down', async () => {
    const {fetch} = fake({[A]: 'down', [B]: 'down'});
    const r = await chainTip(newTipCache(), fetch, t0);
    expect(r).toEqual({ok: false, status: 503, reason: expect.stringMatching(/neither/)});
  });

  it('retries a race once and serves the hash both then agree on', async () => {
    const old = block(3);
    const tip = block(4);
    const {fetch, calls} = fake({
      [A]: {tips: [tip.hash, tip.hash], headers: {[tip.hash]: tip.header}},
      [B]: {tips: [old.hash, tip.hash], headers: {[tip.hash]: tip.header}},
    });
    const r = await chainTip(newTipCache(), fetch, t0);
    expect(r).toEqual({ok: true, hash: tip.hash, header: tip.header, sources: ['mempool.space', 'blockstream.info']});
    expect(calls.filter(c => c.endsWith('/blocks/tip/hash'))).toHaveLength(4);
  });

  it('answers 503 when the sources still disagree after the retry', async () => {
    const x = block(5);
    const y = block(6);
    const {fetch, calls} = fake({[A]: both(x), [B]: both(y)});
    const r = await chainTip(newTipCache(), fetch, t0);
    expect(r).toEqual({ok: false, status: 503, reason: expect.stringMatching(/disagree/)});
    expect(calls.filter(c => c.endsWith('/blocks/tip/hash'))).toHaveLength(4);
  });

  it('treats a source whose header does not hash to its tip as down', async () => {
    const b = block(7);
    const wrong = block(8);
    // Alone, the bad source serves nothing.
    const alone = fake({[A]: {tips: [b.hash], headers: {[b.hash]: wrong.header}}, [B]: 'down'});
    expect(await chainTip(newTipCache(), alone.fetch, t0)).toMatchObject({ok: false, status: 503});
    // Beside a good one that agrees on the hash, the good one's header is served.
    const pair = fake({[A]: {tips: [b.hash], headers: {[b.hash]: wrong.header}}, [B]: both(b)});
    expect(await chainTip(newTipCache(), pair.fetch, t0)).toMatchObject({ok: true, hash: b.hash, header: b.header});
    // A malformed tip hash is a source down.
    const junk = fake({[A]: {tips: ['not a hash']}, [B]: both(b)});
    expect(await chainTip(newTipCache(), junk.fetch, t0)).toEqual({
      ok: true,
      hash: b.hash,
      header: b.header,
      sources: ['blockstream.info'],
    });
  });

  it('caches an answer for 60 s and asks again after', async () => {
    const first = block(9);
    const next = block(10);
    const table = {[A]: both(first), [B]: both(first)};
    const {fetch, calls} = fake(table);
    const cache = newTipCache();
    expect(await chainTip(cache, fetch, t0)).toMatchObject({hash: first.hash});
    const asked = calls.length;
    table[A] = both(next);
    table[B] = both(next);
    expect(await chainTip(cache, fetch, t0 + CHAIN_TIP_TTL_MS - 1)).toMatchObject({hash: first.hash});
    expect(calls.length).toBe(asked);
    expect(await chainTip(cache, fetch, t0 + CHAIN_TIP_TTL_MS)).toMatchObject({hash: next.hash});
    expect(calls.length).toBeGreaterThan(asked);
  });

  it('does not cache a failure', async () => {
    const b = block(11);
    const table: Parameters<typeof fake>[0] = {[A]: 'down', [B]: 'down'};
    const {fetch} = fake(table);
    const cache = newTipCache();
    expect(await chainTip(cache, fetch, t0)).toMatchObject({ok: false});
    table[A] = both(b);
    expect(await chainTip(cache, fetch, t0 + 1)).toMatchObject({ok: true, hash: b.hash});
  });

  it('shares one lookup among callers that arrive while it runs', async () => {
    const b = block(12);
    const {fetch, calls} = fake({[A]: both(b), [B]: both(b)});
    const cache = newTipCache();
    const [x, y] = await Promise.all([chainTip(cache, fetch, t0), chainTip(cache, fetch, t0)]);
    expect(x).toEqual(y);
    expect(calls.filter(c => c.endsWith('/blocks/tip/hash'))).toHaveLength(2);
  });
});

describe('GET /v1/chain/tip', () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('answers the tip with no token, from the two sources, and only to GET', async () => {
    const b = block(13);
    const urls: string[] = [];
    vi.spyOn(globalThis, 'fetch').mockImplementation(async input => {
      const url = String(input instanceof Request ? input.url : input);
      urls.push(url);
      return new Response(url.endsWith('/blocks/tip/hash') ? b.hash : b.header, {status: 200});
    });
    const res = await SELF.fetch('https://r.test/v1/chain/tip');
    expect(res.status).toBe(200);
    expect(res.headers.get('cache-control')).toBe('public, max-age=60');
    expect(await res.json()).toEqual({ok: true, hash: b.hash, header: b.header, sources: ['mempool.space', 'blockstream.info']});
    expect(urls.every(u => u.startsWith(A) || u.startsWith(B))).toBe(true);
    expect((await SELF.fetch('https://r.test/v1/chain/tip', {method: 'POST'})).status).toBe(405);
  });
});

describe('reading a source: bounded in size and time', () => {
  const enc = new TextEncoder();
  /** A body served in `chunks`, then left open when `hang`; records whether it was cancelled. */
  function body(chunks: string[], hang = false) {
    const seen = {cancelled: false};
    let i = 0;
    const stream = new ReadableStream<Uint8Array>({
      pull(c) {
        if (i < chunks.length) {
          c.enqueue(enc.encode(chunks[i++]!));
        } else if (!hang) {
          c.close();
        }
        return hang && i >= chunks.length ? new Promise<void>(() => {}) : undefined;
      },
      cancel() {
        seen.cancelled = true;
      },
    });
    return {stream, seen};
  }
  const answering = (res: () => Response) => (async () => res()) as unknown as typeof fetch;
  const tip = block(7);

  it('reads a small 200 answer, trimmed', async () => {
    const {stream} = body([tip.hash.slice(0, 20), tip.hash.slice(20) + '\n']);
    expect(await boundedFetch(answering(() => new Response(stream)))('https://x/blocks/tip/hash')).toBe(tip.hash);
  });

  it('refuses a body that declares more than the bound, and cancels it unread', async () => {
    const {stream, seen} = body(['a'.repeat(10)]);
    const res = () => new Response(stream, {headers: {'content-length': String(MAX_BODY_BYTES + 1)}});
    await expect(boundedFetch(answering(res))('https://x/a')).rejects.toThrow(/more than/);
    expect(seen.cancelled).toBe(true);
  });

  it('stops reading a body that streams past the bound without declaring it, and cancels it', async () => {
    const {stream, seen} = body(Array.from({length: 64}, () => 'b'.repeat(100)));
    await expect(boundedFetch(answering(() => new Response(stream)))('https://x/a')).rejects.toThrow(/more than/);
    expect(seen.cancelled).toBe(true);
  });

  it('takes a body of exactly the bound', async () => {
    const {stream} = body(['c'.repeat(MAX_BODY_BYTES)]);
    expect(await boundedFetch(answering(() => new Response(stream)))('https://x/a')).toHaveLength(MAX_BODY_BYTES);
  });

  it('cancels the body of an answer that is not 200', async () => {
    const {stream, seen} = body(['not found']);
    await expect(boundedFetch(answering(() => new Response(stream, {status: 404})))('https://x/a')).rejects.toThrow(/404/);
    expect(seen.cancelled).toBe(true);
  });

  it('gives up on a source that sends no answer in time, and aborts the request', async () => {
    let aborted = false;
    const silent = ((_url: string, init?: RequestInit) =>
      new Promise<Response>(() => {
        init?.signal?.addEventListener('abort', () => {
          aborted = true;
        });
      })) as unknown as typeof fetch;
    await expect(boundedFetch(silent, 30)('https://x/a')).rejects.toThrow(/longer than/);
    expect(aborted).toBe(true);
  });

  it('gives up on a body that trickles past the time bound, and cancels it', async () => {
    const {stream, seen} = body(['d'], true);
    await expect(boundedFetch(answering(() => new Response(stream)), 30)('https://x/a')).rejects.toThrow(/longer than/);
    expect(seen.cancelled).toBe(true);
  });

  it('takes an oversized or slow source as down, and serves the other', async () => {
    const big = 'f'.repeat(MAX_BODY_BYTES * 4);
    const plain = (text: string) => new Response(text);
    for (const misbehave of ['big', 'slow'] as const) {
      const fetchFn = (async (url: string) => {
        if (url.startsWith(A)) {
          if (misbehave === 'big') return plain(big);
          return new Response(body([], true).stream);
        }
        if (url === `${B}/blocks/tip/hash`) return plain(tip.hash);
        if (url === `${B}/block/${tip.hash}/header`) return plain(tip.header);
        return new Response('', {status: 404});
      }) as unknown as typeof fetch;
      expect(await chainTip(newTipCache(), boundedFetch(fetchFn, 30), Date.now())).toEqual({
        ok: true,
        hash: tip.hash,
        header: tip.header,
        sources: [SOURCES[1]!.name],
      });
    }
  });
});
