/**
 * `GET /v1/chain/tip`: the latest Bitcoin block header, for the Setup app's first clock anchor
 * (DESIGN § 4, "Header source: the registry"). The phone asks the registry, never a third party,
 * and keeps its own checks on what comes back (80 bytes, work, time).
 *
 * Two public Esplora servers are asked for the tip hash (`/blocks/tip/hash`) and then the header
 * (`/block/:hash/header`). When both answer they must name the same hash; a difference is taken as
 * a race with a new block and asked once more, and a second difference answers 503. When one
 * fails, the other's answer is served; when both fail, 503. A header is used only if its double
 * SHA-256 is the hash it was asked for. An answer over 1 KB or slower than 5 s is a source that is
 * down. A good answer is kept 60 s; a failure is not kept.
 */

import {sha256} from '@noble/hashes/sha2.js';

import {fromHex, isHex, toHex} from './http';

/** The two sources, in the order their headers are tried. */
export const SOURCES: readonly {readonly name: string; readonly base: string}[] = [
  {name: 'mempool.space', base: 'https://mempool.space/api'},
  {name: 'blockstream.info', base: 'https://blockstream.info/api'},
];

export const CHAIN_TIP_TTL_MS = 60_000;
const TIMEOUT_MS = 5_000;

/** A GET that resolves to the body text of a 200 answer and throws on anything else. */
export type TipFetch = (url: string) => Promise<string>;

export type TipAnswer =
  | {ok: true; hash: string; header: string; sources: string[]}
  | {ok: false; status: 503; reason: string};

export interface TipCache {
  value: {at: number; tip: Extract<TipAnswer, {ok: true}>} | null;
  inflight: Promise<TipAnswer> | null;
}

export function newTipCache(): TipCache {
  return {value: null, inflight: null};
}

/** The most a source's answer may carry: a tip hash is 64 characters, a header 160. */
export const MAX_BODY_BYTES = 1024;

/**
 * `fetchFn` as a `TipFetch` that bounds what it reads: the whole request, body included, within
 * `timeoutMs`, and at most `maxBytes` of body. An answer that is not 200, larger, or slower
 * throws, which the lookup takes as that source being down; its body is cancelled unread.
 */
export function boundedFetch(fetchFn: typeof fetch, timeoutMs = TIMEOUT_MS, maxBytes = MAX_BODY_BYTES): TipFetch {
  return async url => {
    const abort = new AbortController();
    let reader: ReadableStreamDefaultReader<Uint8Array> | null = null;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const deadline = new Promise<never>((_, reject) => {
      timer = setTimeout(() => {
        abort.abort();
        reader?.cancel().catch(() => {});
        reject(new Error(`${url} took longer than ${timeoutMs} ms`));
      }, timeoutMs);
    });
    const read = async (): Promise<string> => {
      const res = await fetchFn(url, {signal: abort.signal});
      if (res.status !== 200) {
        await res.body?.cancel().catch(() => {});
        throw new Error(`${url} answered ${res.status}`);
      }
      const declared = Number(res.headers.get('content-length') ?? '');
      if (Number.isFinite(declared) && declared > maxBytes) {
        await res.body?.cancel().catch(() => {});
        throw new Error(`${url} answered more than ${maxBytes} bytes`);
      }
      if (!res.body) return '';
      reader = res.body.getReader();
      const chunks: Uint8Array[] = [];
      let total = 0;
      for (;;) {
        const {done, value} = await reader.read();
        if (done) break;
        total += value.byteLength;
        if (total > maxBytes) {
          await reader.cancel().catch(() => {});
          throw new Error(`${url} answered more than ${maxBytes} bytes`);
        }
        chunks.push(value);
      }
      const bytes = new Uint8Array(total);
      let at = 0;
      for (const c of chunks) {
        bytes.set(c, at);
        at += c.byteLength;
      }
      return new TextDecoder().decode(bytes).trim();
    };
    try {
      return await Promise.race([read(), deadline]);
    } finally {
      if (timer !== null) clearTimeout(timer);
    }
  };
}

/** The Worker's fetch, bounded in size and time, as a `TipFetch`. */
export const webFetch: TipFetch = boundedFetch((url, init) => fetch(url, init));

async function tipHash(get: TipFetch, base: string): Promise<string> {
  const text = (await get(`${base}/blocks/tip/hash`)).trim();
  if (!isHex(text, 32)) throw new Error('not a block hash');
  return text;
}

/** The header of `hash` from one source, only if it hashes to `hash`. */
async function headerOf(get: TipFetch, base: string, hash: string): Promise<string> {
  const text = (await get(`${base}/block/${hash}/header`)).trim();
  if (!isHex(text, 80)) throw new Error('not an 80-byte header');
  if (toHex(sha256(sha256(fromHex(text))).reverse()) !== hash) throw new Error('header does not hash to the tip');
  return text;
}

/** One round: both hashes, then a header from the sources that named the served hash. */
async function round(get: TipFetch): Promise<TipAnswer | 'disagree'> {
  const hashes = await Promise.all(SOURCES.map(s => tipHash(get, s.base).catch(() => null)));
  const named = hashes.filter((h): h is string => h !== null);
  if (named.length === 0) {
    return {ok: false, status: 503, reason: 'neither header source answered'};
  }
  if (named.length === 2 && named[0] !== named[1]) {
    return 'disagree';
  }
  const hash = named[0]!;
  const agreeing = SOURCES.filter((_, i) => hashes[i] === hash);
  for (const s of agreeing) {
    try {
      const header = await headerOf(get, s.base, hash);
      return {ok: true, hash, header, sources: agreeing.map(a => a.name)};
    } catch {
      // the next source that named the same hash
    }
  }
  return {ok: false, status: 503, reason: 'no header source served the header of its tip'};
}

async function lookUp(get: TipFetch): Promise<TipAnswer> {
  const first = await round(get);
  if (first !== 'disagree') return first;
  const second = await round(get);
  if (second !== 'disagree') return second;
  return {ok: false, status: 503, reason: 'the two header sources disagree on the tip'};
}

/** The tip, from `cache` while it is under 60 s old; callers arriving during a lookup share it. */
export async function chainTip(cache: TipCache, get: TipFetch, now: number): Promise<TipAnswer> {
  if (cache.value && now - cache.value.at < CHAIN_TIP_TTL_MS) {
    return cache.value.tip;
  }
  if (!cache.inflight) {
    cache.inflight = lookUp(get).finally(() => {
      cache.inflight = null;
    });
  }
  const answer = await cache.inflight;
  if (answer.ok && (!cache.value || cache.value.tip !== answer)) {
    cache.value = {at: now, tip: answer};
  }
  return answer;
}
