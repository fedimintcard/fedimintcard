const { test } = require('node:test');
const assert = require('node:assert/strict');

// Exercise the published JS entry point with stand-ins only at its native module boundary.
function loadNative(platform) {
  const calls = [];
  const native = {
    isSupported: async () => true,
    isEnabled: async () => true,
    start: async () => {calls.push('start')},
    registerTagEvent: async opts => {calls.push(['register', opts])},
    unregisterTagEvent: async () => {calls.push('unregister')},
    requestTechnology: async (tech, opts) => {calls.push(['tech', tech, opts])},
    setTimeout: async t => {calls.push(['timeout', t])},
    cancelTechnologyRequest: async () => {calls.push('cancel')},
    isoDepHandler: {transceive: async cmd => {calls.push(['send', cmd]); return [4,0,0x90,0]}},
  };
  const deps = new Map([
    [require.resolve('react-native'), {Platform:{OS:platform}}],
    [require.resolve('react-native-nfc-manager'), {
      __esModule:true, default:native, NfcTech:{IsoDep:'IsoDep'},
      NfcAdapter:{FLAG_READER_NFC_A:1,FLAG_READER_SKIP_NDEF_CHECK:128},
    }]
  ]);
  for (const [path, exports] of deps) require.cache[path] = {id:path,filename:path,loaded:true,exports};
  delete require.cache[require.resolve('..')];
  return {sdk:require('..'), calls, native};
}
for (const platform of ['ios', 'android']) {
  test(platform + ' selects the applet and closes the native session', async () => {
    const {sdk,calls,native} = loadNative(platform);
    assert.equal(await sdk.cardReaderAvailable(), true);
    calls.length = 0;
    assert.deepEqual(await sdk.withFedimintCard(async r => r.version,{prompt:'Hold card'}), {major:4,minor:0});
    const android = platform === 'android';
    const cmd = [...Buffer.from('00a4040008f0464d434152440100','hex')];
    assert.deepEqual(calls, [
      'start', ...(android ? [['register',{isReaderModeEnabled:true,readerModeFlags:129}]] : []),
      ['tech','IsoDep',{alertMessage:'Hold card'}], ...(android ? [['timeout',5000]] : []),
      ['send',cmd], ...(android ? ['unregister'] : []), 'cancel',
    ]);
    native.isoDepHandler.transceive = async () => [-1,0];
    await assert.rejects(sdk.withFedimintCard(async r => r.version), /Invalid NFC response/);
    assert.equal(calls.at(-1),'cancel');
  });
}
