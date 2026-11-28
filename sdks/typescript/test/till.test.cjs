const test = require('node:test');
const assert = require('node:assert/strict');
const { createHash } = require('node:crypto');
const { TillClient, TillError } = require('../dist');

const token = 'a'.repeat(64), link = '1'.repeat(16), txid = '2'.repeat(64), id = '3'.repeat(16);
const preimage = '7'.repeat(64);
const hash = createHash('sha256').update(Buffer.from(preimage, 'hex')).digest('hex');
const future = Date.now() + 600_000;
const pos = 'fedimintcard-till:https://till.example/?t=' + token;
const pay = pos.replace('till:', 'till-pay:');
const invoice = 'signed-invoice-fixture';
const verifyInvoice = text => {
  assert.equal(text, invoice);
  return { amountMsat: 2105000, paymentHash: hash, expiresAtMs: future };
};
function harness(pairing, responses, verify = verifyInvoice) {
  const requests = [];
  const client = TillClient.fromPairing(pairing, { verifyInvoice: verify, fetch: async (url, options) => {
    requests.push({ url, ...options });
    const reply = responses.shift();
    if (reply instanceof Error) throw reply;
    assert.ok(reply, 'unplanned request');
    return { status: reply.status ?? 200, redirected: reply.redirected ?? false, json: async () => reply.json };
  }});
  return { client, requests };
}
const sale = { sale_id: 1, ok: true, price_msat: 5000, received_msat: 5000, collection_pending: false, txid };
const saleArgs = { priceMsat: 5000, clientRef: 'order-7', pin: '4321', cardLink: link };
const quote = { ok: true, topup_id: id, fee_quote_version: 1, amount_msat: 2100224, topup_fee_msat: 4776,
  invoice_amount_msat: 2105000, invoice, expires_at_ms: future - 5000, state: 'waiting' };
const topUpArgs = { amountMsat: 2100000, clientRef: 'deposit-9', shortId:'a01234bc' };
const payArgs = { invoice, maxFeeMsat: 3000, clientRef: 'order-8', pin: '4321', cardLink: link };
const paid = { ok:true, pay_id: 7, price_msat: 2108000, received_msat: 2108000, state: 'paid',
  invoice_msat: 2105000, client_ref: payArgs.clientRef, preimage, payment_hash: hash };
const errorCode = code => error => error instanceof TillError && error.code === code;

test('pairing accepts the two released scopes and rejects ambiguous origins; secrets stay out of errors', () => {
  assert.equal(TillClient.fromPairing(pos).scope, 'pos');
  assert.equal(TillClient.fromPairing(pay).origin, 'https://till.example');
  assert.equal(JSON.stringify(TillClient.fromPairing(pay)), '{}');
  for (const text of [pos.replace('https:', 'http:'), pos.replace('/?', '/private?'), pos + '&t=x', pos + '#frag',
    pos.replace('till.example', 'till.example@elsewhere'), pos.replace('till.example', 'till.example\\elsewhere')]) {
    assert.throws(() => TillClient.fromPairing(text), error => error instanceof TypeError && !error.message.includes(token));
  }
});
test('requires a redirect-safe adapter in React Native before a bearer can be sent', () => {
  const previous = Object.getOwnPropertyDescriptor(globalThis, 'navigator');
  Object.defineProperty(globalThis, 'navigator', { value: {product:'ReactNative'}, configurable:true });
  try {
    assert.throws(() => TillClient.fromPairing(pos), errorCode('redirect_safe_fetch_required'));
    assert.equal(harness(pos, []).client.scope, 'pos');
  } finally {
    if (previous) Object.defineProperty(globalThis, 'navigator', previous);
    else delete globalThis.navigator;
  }
});
test('maps the earlier released registry refusal to a federation mismatch', async () => {
  const {client} = harness(pos, [{status:409,json:{ok:false,error:'ops_refused',detail:'ops refused (409): the card belongs to a different federation or mint'}}]);
  await assert.rejects(client.charge(saleArgs),errorCode('wrong_federation'));
});
test('charge binds the sale to the foreground card link and handles a live balance including held funds', async () => {
  const {client, requests} = harness(pos, [{json: {ok:true, live_msat: 0, stranded_msat: 60, short_id: 'a01234bc'}}, {json:sale}]);
  assert.deepEqual(await client.balance(link), {liveMsat:0, strandedMsat:60, shortId:'a01234bc'});
  assert.equal((await client.charge(saleArgs)).receivedMsat, 5000);
  assert.equal(requests[1].url, 'https://till.example/v1/charge');
  assert.equal(requests[1].headers.Authorization, 'Bearer ' + token);
  assert.equal(requests[1].headers['X-Card-Link'], link);
  assert.equal(requests[1].redirect, 'error');
  assert.deepEqual(JSON.parse(requests[1].body), { price_msat:5000, client_ref:'order-7', pin:'4321' });
});
test('interrupted charge sends once; exact-reference lookup can find an offline or rejected sale', async () => {
  const {client, requests} = harness(pos, [new Error('socket failure'), {json: {sales: [
    {...sale, id:2, client_ref:'order-7', offline:true, rejected:true, received_msat:0},
  ]}}, {json: {sales:[]}}]);
  await assert.rejects(client.charge(saleArgs), errorCode('till_request_interrupted'));
  const entry = await client.findSale('order-7');
  assert.equal(entry.rejected, true);
  assert.equal(entry.receivedMsat, 0);
  assert.equal(await client.findSale('order-7'), undefined);
  assert.deepEqual(requests.map(r => r.method), ['POST','GET','GET']);
});
test('refusals, pending responses, malformed receipts and an absent link cannot be mistaken for a payment', async () => {
  const {client, requests} = harness(pos, [
    {status:409,json:{ok:false,error:'wrong_federation'}}, {status:202,json:{ok:false,error:'undecided'}},
    {json:{...sale, txid:'oops'}}, {json:{...sale, price_msat: 6000}}]);
  await assert.rejects(client.charge({...saleArgs,cardLink:''}),TypeError);
  await assert.rejects(client.charge(saleArgs),errorCode('wrong_federation'));
  await assert.rejects(client.charge(saleArgs),errorCode('undecided'));
  await assert.rejects(client.charge(saleArgs),errorCode('invalid_till_response'));
  await assert.rejects(client.charge(saleArgs),errorCode('invalid_till_response'));
  assert.equal(requests.length,4);
});
test('signed fee-inclusive terms are checked before exposing an invoice and no legacy route is used', async () => {
  const {client,requests} = harness(pos, [{json:quote}, {json:{...quote, invoice_amount_msat:2105001,topup_fee_msat:4777}},
    {json:{...quote, topup_fee_msat:4775}}, {json:{...quote, expires_at_ms:future+5000}},
    {json:{...quote,fee_quote_version:0}}, {status:404,json:{ok:false,error:'not_found'}}]);
  assert.deepEqual(await client.quoteTopUp(topUpArgs), {id,creditMsat:2100224,state:'waiting',feeMsat:4776,
    totalMsat:2105000,expiresAtMs:future-5000, invoice});
  for (let i=0;i<4;i++) await assert.rejects(client.quoteTopUp(topUpArgs),errorCode('invalid_till_response'));
  await assert.rejects(client.quoteTopUp(topUpArgs),errorCode('not_found'));
  assert.ok(requests.every(r=>r.url.endsWith('/v1/topup/fee-inclusive')));
  assert.deepEqual(JSON.parse(requests[0].body),{fee_quote_version:1,amount_msat:2100000,short_id:'a01234bc',client_ref:'deposit-9'});
});
test('expired payment window waits for settlement; an existing paid top-up resumes and loads under its original ID', async () => {
  const {client,requests} = harness(pos, [{json:{...quote, invoice:'',expires_at_ms:Date.now()-1}},
    {json:{...quote, state:'paid'}}, {json:{...quote,state:'loaded',load_txid:txid}}, {json:{...quote,topup_id:link}}]);
  assert.deepEqual(await client.topUp(id),{ state:'settlement_pending', id,creditMsat:2100224 });
  assert.equal((await client.topUp(id)).state,'paid');
  assert.deepEqual(await client.loadTopUp(id,link),{id,creditMsat:2100224,state:'loaded',txid});
  assert.equal(requests[2].headers['X-Card-Link'], link);
  await assert.rejects(client.topUp(id),errorCode('invalid_till_response'));
});
test('wallet uses least-privilege pairing, verifies payment proof and can recover by pay ID and original reference', async () => {
  const {client,requests} = harness(pay, [{json:paid}, {json:{...paid,preimage:txid}},
    {status:202,json:{ok:false,error:'undecided',pay_id:7,state:'paying'}}, {json:{...paid,preimage:undefined}}]);
  await assert.rejects(client.charge(saleArgs),errorCode('wrong_pairing_scope'));
  assert.equal((await client.payInvoice(payArgs)).preimage,preimage);
  await assert.rejects(client.payInvoice(payArgs),errorCode('invalid_till_response'));
  await assert.rejects(client.payInvoice(payArgs),error=>error.code==='undecided' && error.payId===7 && error.payState==='paying');
  assert.equal((await client.payStatus(7, 'order-8')).state, 'paid');
  assert.ok(requests[3].url.endsWith('/v1/pay/7?client_ref=order-8'));
});
test('wallet fails closed without signed-invoice validation or valid local inputs', async () => {
  const {client,requests} = harness(pos, [], null);
  await assert.rejects(client.quoteTopUp(topUpArgs),errorCode('invoice_verifier_required'));
  await assert.rejects(client.payInvoice(payArgs),errorCode('wrong_pairing_scope'));
  await assert.rejects(client.charge({...saleArgs,priceMsat:1.5}),TypeError);
  await assert.rejects(client.charge({...saleArgs,pin:'123'}),TypeError);
  assert.equal(requests.length,0);
});
