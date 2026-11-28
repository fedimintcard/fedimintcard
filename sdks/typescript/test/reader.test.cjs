const test = require('node:test');
const assert = require('node:assert/strict');
const {
  FedimintCardReader, CardStatusError, UnsupportedCardError, CardProtocolError, verifiesCertificate,
} = require('..');
const vectors = require('./reader-vectors.json');

const bytes = hex => Uint8Array.from(Buffer.from(hex, 'hex'));
const hex = data => Buffer.from(data).toString('hex');
function script(pairs) {
  return async command => {
    assert.ok(command instanceof Uint8Array);
    const [expected, response] = pairs.shift() ?? [];
    assert.equal(hex(command), expected);
    return bytes(response);
  };
}
async function withReader(pairs, run) {
  const steps = [[vectors.select.command, vectors.select.response], ...pairs];
  const reader = await FedimintCardReader.connect(script(steps));
  const result = await run(reader);
  assert.equal(steps.length, 0);
  return result;
}
function slot(state, amount) {
  const body = Buffer.alloc(187);
  body[0] = state;
  if (state) body.set(bytes(vectors.identity.xOnly), 1);
  body.writeBigUInt64BE(amount, 33);
  body.fill(0x21, 41, 89);
  body.fill(0x42, 89, 121);
  body.fill(0x53, 121, 153);
  body.writeUInt16BE(258, 153);
  body.fill(0x67, 155, 187);
  return hex(body) + '9000';
}

test('SELECT accepts supported 4.x and preserves legacy/missing and truncated response distinctions', async () => {
  for (const item of [vectors.select, vectors.futureMinor]) {
    const reader = await FedimintCardReader.connect(script([[vectors.select.command, item.response]]));
    assert.deepEqual(reader.version, { major: item.major, minor: item.minor });
  }
  for (const item of [vectors.legacy, vectors.malformedSelect]) {
    await assert.rejects(FedimintCardReader.connect(script([[vectors.select.command, item.response]])), UnsupportedCardError);
  }
  await assert.rejects(FedimintCardReader.connect(async () => bytes(vectors.absent.response)),
    error => error instanceof CardStatusError && error.statusWord === 0x6a82);
  await assert.rejects(FedimintCardReader.connect(async () => bytes(vectors.malformedResponse)), CardProtocolError);
});

test('GET_STATE and GET_PUB follow the shared vectors with exact 64-bit amounts', () => withReader(
  [[vectors.state.command, vectors.state.response], [vectors.identity.command, vectors.identity.response]],
  async reader => {
    const s = await reader.readState();
    assert.equal(s.slots.length, 32);
    const names = ['empty', 'keyed', 'note', 'change', 'spent'];
    assert.deepEqual(s.slots, vectors.state.slots.map(i => names[i]));
    assert.equal(s.headerTime, vectors.state.now);
    assert.equal(s.windowStart, vectors.state.window);
    assert.equal(s.dailyLimitMsat, BigInt(vectors.state.limitMsat));
    assert.equal(s.spentThisWindowMsat, BigInt(vectors.state.spentMsat));
    assert.equal(s.wrongPinAttempts, vectors.state.wrongTries);
    assert.equal(s.pinWaitRemaining, vectors.state.owed);
    assert.deepEqual(await reader.readPublicKey(), bytes(vectors.identity.xOnly));
  }
));

test('ISO continuations correct the current command length and only return the joined body', async () => {
  const steps = [
    [vectors.select.command, '6c02'], ['00a4040008f0464d434152440102', '046101'],
    ['00c0000001', '6c01'], ['00c0000001', '009000'],
  ];
  assert.equal((await FedimintCardReader.connect(script(steps))).version.major, 4);
  assert.equal(steps.length, 0);
  await assert.rejects(FedimintCardReader.connect(async () => bytes('6100')), CardProtocolError);
  await assert.rejects(FedimintCardReader.connect(async () => bytes('0102039000')), CardProtocolError);
});

test('only slot-scoped unavailable status maps to null; malformed frames reject', () => withReader(
  [['b0111f0000', '6985'], ['b032000000', '6a83'], ['b011000000', '6982'],
   ['b016000000', '009000'], ['b015010000', slot(5, 1n)], ['b011000000', 'ff'.repeat(32) + '9000']],
  async reader => {
    assert.equal(await reader.readPublicKey(31), null);
    assert.equal(await reader.readCertificate(), null);
    await assert.rejects(reader.readPublicKey(), e => e.statusWord === 0x6982);
    await assert.rejects(reader.readState(), CardProtocolError);
    await assert.rejects(reader.readSlot(1), CardProtocolError);
    await assert.rejects(reader.readPublicKey(), CardProtocolError);
    for (const invalid of [-1, 32, 255, 1.5]) await assert.rejects(reader.readSlot(invalid), RangeError);
  }
));

test('local inventory sums 64-bit notes separately from pending change, skipping retained spends', () => withReader(
  [[vectors.state.command, vectors.state.response], ['b015010000', slot(2, 0xffffffffffffffffn)],
   ['b015030000', slot(3, 1001n)], ['b015020000', slot(4, 99n)], ['b015030000', slot(3, 1001n)]],
  async reader => {
    assert.deepEqual(await reader.readBalance(), { notesMsat: 0xffffffffffffffffn, pendingChangeMsat: 1001n });
    const spent = await reader.readSlot(2);
    assert.equal(hex(spent.spentHash), '42'.repeat(32));
    assert.equal(spent.mintSignature, null);
    const change = await reader.readSlot(3);
    assert.equal(change.changeOpening.outputIndex, 258);
    assert.deepEqual(Object.keys(change.changeOpening).sort(), ['outputIndex', 'transactionId']);
  }
));

test('a changed slot interrupts inventory', () => withReader(
  [[vectors.state.command, vectors.state.response], ['b015010000', slot(4, 1n)]],
  reader => assert.rejects(reader.readBalance(), CardProtocolError)
));

test('GET_CERT checks the signed key binding using an independently pinned registry point', async () => {
  const fixture = require('./certificate-vector.json');
  const id = bytes(fixture.observedSlot0);
  const registry = bytes(fixture.registryPublicKey);
  await withReader([[fixture.command, fixture.response], [fixture.command, '02' + fixture.response.slice(2)]], async r => {
    const cert = await r.readCertificate();
    assert.equal(verifiesCertificate(cert, id, registry), true);
    assert.equal(verifiesCertificate(cert, id, bytes(fixture.otherRegistryPublicKey)), false);
    assert.equal(verifiesCertificate(cert, new Uint8Array(32), registry), false);
    cert.signature[0] ^= 1;
    assert.equal(verifiesCertificate(cert, id, registry), false);
    await assert.rejects(r.readCertificate(), CardProtocolError);
  });
});
