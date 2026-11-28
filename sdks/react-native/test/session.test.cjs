const test = require('node:test');
const assert = require('node:assert/strict');
const { makeSession } = require('../dist/session');
function deferred() {
  let resolve, reject;
  const promise = new Promise((ok, fail) => { resolve = ok; reject = fail; });
  return { promise, resolve, reject };
}
function fixture(overrides = {}) {
  const calls = [];
  const ops = {
    start: async () => { calls.push('start'); },
    open: async p => { calls.push(p); },
    configure: async () => { calls.push('configure'); },
    exchange: async request => { calls.push(request); return Uint8Array.of(0x90, 0); },
    close: async () => { calls.push('close'); },
    ...overrides,
  };
  return { calls, session: makeSession(ops) };
}
test('selected session configures, exchanges and releases exactly once; retained transport refuses reuse', async () => {
  const {session, calls} = fixture();
  let later;
  const cmd = Uint8Array.of(0);
  const result = await session(async send => { later = send; return send(cmd); }, { prompt: 'Tap' });
  assert.deepEqual(result, Uint8Array.of(0x90, 0));
  assert.deepEqual(calls, ['start', 'Tap', 'configure', cmd, 'close']);
  await assert.rejects(later(cmd), /ended/);
});
test('overlapping scans and commands reject; later scans can open', async () => {
  const reading = deferred(), entered = deferred();
  const {session} = fixture({ exchange: () => reading.promise });
  const pending = session(async send => {
    const first = send(Uint8Array.of(1));
    await assert.rejects(send(Uint8Array.of(2)), /in progress/);
    entered.resolve();
    return first;
  });
  await entered.promise;
  await assert.rejects(session(async () => {}), /already open/);
  reading.resolve(Uint8Array.of(0x90, 0));
  await pending;
  assert.equal(await session(async () => 42), 42);
});
test('cancel while waiting for tag closes and allows a later scan', async () => {
  const controller = new AbortController(), opened = deferred(), waiting = deferred();
  let closes = 0;
  const {session} = fixture({
    open: async () => { opened.resolve(); if (!closes) await waiting.promise; },
    close: async () => { closes++; waiting.reject(new Error('native cancelled')); },
  });
  const pending = session(async () => assert.fail(), {signal: controller.signal});
  await opened.promise;
  controller.abort();
  await assert.rejects(pending, /cancelled/);
  assert.equal(closes, 1);
  assert.equal(await session(async () => 123), 123);
  assert.equal(closes, 2);
});
test('closed screens release already acquired reader mode even if aborted during startup', async () => {
  const controller = new AbortController(), ready = deferred();
  const {session, calls} = fixture({start: () => ready.promise});
  const pending = session(async () => assert.fail(), {signal: controller.signal});
  controller.abort();
  ready.resolve();
  await assert.rejects(pending, /cancelled/);
  assert.deepEqual(calls, ['close']);
  await assert.rejects(session(async () => assert.fail(), {signal: controller.signal}), /cancelled/);
  assert.deepEqual(calls, ['close']);
});
test('native errors from waiting for a tag, configuration and the consumer release the scan', async () => {
  for (const phase of ['open', 'configure', 'callback']) {
    const fail = async () => { throw new Error(phase); };
    const {session, calls} = fixture(phase === 'callback' ? {} : {[phase]:fail});
    await assert.rejects(session(fail), new RegExp(phase));
    assert.equal(calls.at(-1), 'close');
  }
});
