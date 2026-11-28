import { sha256 } from '@noble/hashes/sha256';
import { bytesToHex, hexToBytes } from '@noble/hashes/utils';

type Json = Record<string, unknown>;
type Scope = 'pos' | 'pay';
type Fetch = (url: string, init: RequestInit) => Promise<Response>;

/** Supply the wallet's signature-validating BOLT11 decoder, including its network policy. */
export interface VerifiedInvoice {
  amountMsat: number;
  paymentHash: string;
  expiresAtMs: number;
}
export interface TillOptions {
  /** Must reject redirects before following them. Required on React Native. */
  fetch?: Fetch;
  verifyInvoice?: (bolt11: string) => VerifiedInvoice | Promise<VerifiedInvoice>;
}
export class TillError extends Error {
  constructor(readonly code: string, readonly httpStatus?: number, readonly payId?: number,
              readonly payState?: string) {
    super(code);
    this.name = 'TillError';
  }
}

function invalid(): never { throw new TillError('invalid_till_response'); }
function record(value: unknown): Json {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return invalid();
  return value as Json;
}
function uint(value: unknown): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) return invalid();
  return value;
}
function bool(value: unknown): boolean {
  if (typeof value !== 'boolean') return invalid();
  return value;
}
function hex(value: unknown, length: number): value is string {
  return typeof value === 'string' && value.length === length && /^[0-9a-f]+$/.test(value);
}
function input(condition: boolean, message: string): void {
  if (!condition) throw new TypeError(message);
}
function ref(value: string, limit: number): string {
  input(typeof value === 'string' && /^[A-Za-z0-9._:-]+$/.test(value) && value.length <= limit,
        'Use a persistent ASCII reference for this operation');
  return value;
}
function pin(value: string): string {
  input(typeof value === 'string' && /^[0-9]{4,8}$/.test(value), 'Use a 4 to 8 digit PIN');
  return value;
}
function link(value: string): string {
  input(hex(value, 16), 'An active card link is required');
  return value;
}
function topupId(value: string): string {
  input(hex(value, 16), 'Invalid top-up ID');
  return value;
}
function amount(value: number, min: number, max: number): number {
  input(Number.isSafeInteger(value) && value >= min && value <= max, 'Amount is outside the supported range');
  return value;
}

export interface SaleReceipt {
  saleId: number; txid: string; priceMsat: number; receivedMsat: number;
  collectionPending: boolean; offline: boolean; doubleSpent: boolean;
  /** An offline sales ledger on a 4.0 till can omit the outcome of a rejected transaction. */
  rejected: boolean | undefined;
}
function receipt(row: Json, id: unknown, fromLedger = false): SaleReceipt {
  if (!hex(row.txid, 64)) return invalid();
  const offline = row.offline === undefined ? false : bool(row.offline);
  const rejected = row.rejected === undefined ? (fromLedger && offline ? undefined : false) : bool(row.rejected);
  return { saleId: uint(id), txid: row.txid, priceMsat: uint(row.price_msat),
    receivedMsat: uint(row.received_msat), collectionPending: bool(row.collection_pending),
    offline, rejected,
    doubleSpent: row.double_spent === undefined ? false : bool(row.double_spent) };
}

export type TopUp = { id: string; creditMsat: number } & (
  { state: 'waiting'; feeMsat: number; totalMsat: number; expiresAtMs: number; invoice: string } |
  { state: 'settlement_pending' | 'paid' | 'failed' | 'expired' } |
  { state: 'loaded'; txid: string }
);

const payStates = ['charging','not_charged','charged','paying','paid','pay_failed','refunding','refunded'] as const;
type PayState = typeof payStates[number];
function payState(value: unknown): PayState {
  const match = payStates.find(state => state === value);
  return match ?? invalid();
}

/** Paired HTTPS API. Supply an NFC link from the foreground session for every card operation. */
export class TillClient {
  readonly #origin: string;
  readonly #scope: Scope;
  readonly #token: string;
  readonly #fetch: Fetch;
  readonly #verify?: TillOptions['verifyInvoice'];

  private constructor(origin: string, scope: Scope, token: string, options: TillOptions) {
    this.#origin = origin; this.#scope = scope; this.#token = token;
    if (!options.fetch && globalThis.navigator?.product === 'ReactNative') {
      throw new TillError('redirect_safe_fetch_required');
    }
    this.#fetch = options.fetch ?? globalThis.fetch.bind(globalThis);
    this.#verify = options.verifyInvoice;
  }
  get origin(): string { return this.#origin; }
  get scope(): Scope { return this.#scope; }

  static fromPairing(text: string, options: TillOptions = {}): TillClient {
    const match = /^fedimintcard-till(-pay)?:(https:\/\/[^/\\?#@\s]+)\/?\?t=([a-zA-Z0-9]{1,256})$/.exec(text.trim());
    if (!match) throw new TypeError('Invalid HTTPS till pairing');
    let origin: string;
    try { origin = new URL(match[2]).origin; } catch {
      throw new TypeError('Pairing requires a valid origin and a complete WHATWG URL implementation');
    }
    if (typeof origin !== 'string' || !origin.startsWith('https://')) throw new TypeError('Invalid till origin');
    return new TillClient(origin, match[1] ? 'pay' : 'pos', match[3], options);
  }

  #require(scope: Scope): void {
    if (this.#scope !== scope) throw new TillError('wrong_pairing_scope');
  }
  async #request(path: string, method: 'GET' | 'POST', body?: Json, cardLink?: string): Promise<Json> {
    const headers: Record<string,string> = { Authorization: 'Bearer ' + this.#token, Accept: 'application/json' };
    if (cardLink !== undefined) headers['X-Card-Link'] = link(cardLink);
    if (body) headers['Content-Type'] = 'application/json';
    const controller = new AbortController();
    const deadline = setTimeout(() => controller.abort(), method === 'GET' ? 15000 : 150000);
    try {
      const response = await this.#fetch(this.#origin + path, {
        method, headers, redirect: 'error', credentials: 'omit', cache: 'no-store',
        body: body ? JSON.stringify(body) : undefined, signal: controller.signal,
      });
      if (response.redirected) throw new TillError('unexpected_redirect');
      let data: Json;
      try { data = record(await response.json()); } catch { return invalid(); }
      if (response.status !== 200 || data.ok === false) {
        let code = typeof data.error === 'string' && /^[a-z][a-z0-9_]{0,63}$/.test(data.error) ? data.error : 'unexpected_till_status';
        if (code === 'ops_refused' && data.detail === 'ops refused (409): the card belongs to a different federation or mint') {
          code = 'wrong_federation';
        }
        if (code === 'internal' && response.status === 500 && method === 'POST' &&
            /^\/v1\/topup\/[0-9a-f]{16}\/load$/.test(path) && typeof data.detail === 'string') {
          const mismatch = /^card [0-9a-f]{8} belongs to federation ([0-9a-f]{64}), and this wallet is in ([0-9a-f]{64})$/.exec(data.detail);
          if (data.detail === 'the registry mint version differs from this wallet' || (mismatch && mismatch[1] !== mismatch[2])) {
            code = 'wrong_federation';
          }
        }
        const knownState = payStates.find(state => state === data.state);
        const payId = typeof data.pay_id === 'number' && Number.isSafeInteger(data.pay_id) && data.pay_id > 0 ? data.pay_id : undefined;
        throw new TillError(code, response.status, payId, knownState);
      }
      if (data.ok !== true && path !== '/v1/sales') return invalid();
      return data;
    } catch (error) {
      if (error instanceof TillError) throw error;
      // An interrupted POST may have reached the till. Never replay it automatically.
      throw new TillError('till_request_interrupted');
    } finally { clearTimeout(deadline); }
  }
  async #invoice(invoice: string): Promise<VerifiedInvoice> {
    if (!this.#verify) throw new TillError('invoice_verifier_required');
    try {
      const v = await this.#verify(invoice);
      if (!hex(v.paymentHash, 64) || !uint(v.amountMsat) || !uint(v.expiresAtMs)) return invalid();
      return v;
    } catch { throw new TillError('invalid_signed_invoice'); }
  }
  async balance(cardLink: string) {
    this.#require('pos');
    const r = await this.#request('/v1/card/balance', 'POST', undefined, link(cardLink));
    if (r.short_id != null && !hex(r.short_id, 8)) return invalid();
    return { liveMsat: uint(r.live_msat), strandedMsat: uint(r.stranded_msat ?? 0), shortId: r.short_id ?? undefined };
  }
  async charge(args: { priceMsat: number; clientRef: string; pin: string; cardLink: string }): Promise<SaleReceipt> {
    this.#require('pos');
    const body = { price_msat: amount(args.priceMsat, 1, 2 ** 40), client_ref: ref(args.clientRef, 128), pin: pin(args.pin) };
    const r = await this.#request('/v1/charge', 'POST', body, link(args.cardLink));
    const sale = receipt(r, r.sale_id);
    if (sale.priceMsat !== args.priceMsat) return invalid();
    return sale;
  }
  /** A missing ledger entry does not establish that an interrupted charge failed. */
  async findSale(clientRef: string): Promise<SaleReceipt | undefined> {
    this.#require('pos');
    ref(clientRef, 128);
    const r = await this.#request('/v1/sales', 'GET');
    if (!Array.isArray(r.sales)) return invalid();
    const matches = r.sales.map(record).filter(row => row.client_ref === clientRef);
    if (matches.length > 1) return invalid();
    return matches.length ? receipt(matches[0], matches[0].id, true) : undefined;
  }
  async #topUp(r: Json, expectedId?: string): Promise<TopUp> {
    if (!hex(r.topup_id, 16) || (expectedId !== undefined && expectedId !== r.topup_id)) return invalid();
    const base = { id: r.topup_id, creditMsat: uint(r.amount_msat) };
    if (!base.creditMsat) return invalid();
    switch (r.state) {
      case 'loaded':
        if (!hex(r.load_txid, 64)) return invalid();
        return { ...base, state: 'loaded', txid: r.load_txid };
      case 'paid': case 'failed': case 'expired': return { ...base, state: r.state };
      case 'waiting': {
        const expiresAtMs = uint(r.expires_at_ms);
        if (!expiresAtMs) return invalid();
        if (expiresAtMs <= Date.now()) return { ...base, state: 'settlement_pending' };
        if (r.fee_quote_version !== 1 || typeof r.invoice !== 'string' || !r.invoice) return invalid();
        const totalMsat = uint(r.invoice_amount_msat), feeMsat = uint(r.topup_fee_msat);
        const signed = await this.#invoice(r.invoice);
        if (totalMsat - base.creditMsat !== feeMsat || signed.amountMsat !== totalMsat || signed.expiresAtMs < expiresAtMs) return invalid();
        return { ...base, state: 'waiting', invoice: r.invoice, expiresAtMs, feeMsat, totalMsat };
      }
      default: return invalid();
    }
  }
  async quoteTopUp(args: { amountMsat: number; clientRef: string; shortId: string }): Promise<TopUp> {
    this.#require('pos');
    if (!this.#verify) throw new TillError('invoice_verifier_required');
    input(hex(args.shortId, 8), 'Read the card ID before requesting a top-up');
    const body = { fee_quote_version: 1, amount_msat: amount(args.amountMsat, 1, 100_000_000),
      client_ref: ref(args.clientRef, 64), short_id: args.shortId };
    return this.#topUp(await this.#request('/v1/topup/fee-inclusive', 'POST', body));
  }
  async topUp(id: string): Promise<TopUp> {
    this.#require('pos');
    return this.#topUp(await this.#request('/v1/topup/' + topupId(id), 'GET'), id);
  }
  async loadTopUp(id: string, cardLink: string): Promise<TopUp> {
    this.#require('pos');
    return this.#topUp(await this.#request('/v1/topup/' + topupId(id) + '/load', 'POST', undefined, link(cardLink)), id);
  }
  /** Use pay-only pairing for a wallet. The till may charge a margin in addition to the fee reserve. */
  async payInvoice(args: { invoice: string; maxFeeMsat: number; clientRef: string; pin: string; cardLink: string }) {
    this.#require('pay');
    const body = { bolt11: args.invoice, max_fee_msat: amount(args.maxFeeMsat, 0, 2 ** 40),
      client_ref: ref(args.clientRef, 124), pin: pin(args.pin) };
    const signed = await this.#invoice(args.invoice);
    const r = await this.#request('/v1/pay', 'POST', body, link(args.cardLink));
    if (r.state !== 'paid' || r.client_ref !== args.clientRef || r.payment_hash !== signed.paymentHash ||
        r.invoice_msat !== signed.amountMsat || !hex(r.preimage, 64) ||
        bytesToHex(sha256(hexToBytes(r.preimage))) !== signed.paymentHash) return invalid();
    return { payId: uint(r.pay_id), priceMsat: uint(r.price_msat), receivedMsat: uint(r.received_msat), preimage: r.preimage };
  }
  async payStatus(id: number, clientRef: string) {
    this.#require('pay');
    amount(id, 1, Number.MAX_SAFE_INTEGER);
    ref(clientRef, 124);
    const r = await this.#request('/v1/pay/' + id + '?client_ref=' + encodeURIComponent(clientRef), 'GET');
    if (r.pay_id !== id || r.client_ref !== clientRef || !hex(r.payment_hash, 64)) return invalid();
    return { id, state: payState(r.state), paymentHash: r.payment_hash, priceMsat: uint(r.price_msat),
      refundOwedMsat: uint(r.refund_owed_msat ?? 0) };
  }
}
