/**
 * One NFC session (docs/PROVISIONER-PLAN.md § 5: "`withCard`, ISO-DEP, iOS
 * alert line"). The card is `jest.setup.js`'s mock of
 * react-native-nfc-manager 3.17.2: its `NfcError` classes are the library's
 * own, so a thrown error is the one a phone would give this code.
 *
 * What matters here is that the engine is handed a working transceiver and
 * that the session always ends — a leaked session means the next tap says
 * "a run is already going" until the app is killed.
 */
import {Platform} from 'react-native';
import NfcManager, {NfcError} from 'react-native-nfc-manager';

import {
  ANDROID_TIMEOUT_MS,
  cancelCard,
  createCardSession,
  nfcErrorName,
  phaseMessage,
  selectAppletApdu,
  setCardMessage,
  statusWord,
  withCard,
} from './session';
import {CARD_AID} from './cardkit';

const mock = NfcManager as unknown as {
  start: jest.Mock;
  requestTechnology: jest.Mock;
  cancelTechnologyRequest: jest.Mock;
  setAlertMessageIOS: jest.Mock;
  setTimeout: jest.Mock;
  isoDepHandler: {transceive: jest.Mock};
};

beforeEach(() => {
  jest.clearAllMocks();
  mock.requestTechnology.mockResolvedValue('IsoDep');
});

test('the transceiver sends the APDU it is given and returns the reply bytes', async () => {
  mock.isoDepHandler.transceive
    .mockResolvedValueOnce([0x6a, 0x82]) // the SELECT of the applet
    .mockResolvedValueOnce([0x90, 0x00]);
  const {value, meta} = await withCard('hold', tx => tx([0x00, 0x01]));
  expect(value).toEqual([0x90, 0x00]);
  expect(mock.isoDepHandler.transceive).toHaveBeenNthCalledWith(1, selectAppletApdu());
  expect(mock.isoDepHandler.transceive).toHaveBeenNthCalledWith(2, [0x00, 0x01]);
  expect(meta.selectSw).toBe(0x6a82);
  expect(mock.cancelTechnologyRequest).toHaveBeenCalledTimes(1);
});

test('the run starts from a SELECT of the card applet, before anything else', async () => {
  mock.isoDepHandler.transceive.mockResolvedValue([0x90, 0x00]);
  await withCard('hold', async () => undefined);
  expect(selectAppletApdu()).toEqual([
    0x00, 0xa4, 0x04, 0x00, CARD_AID.length, ...CARD_AID, 0x00,
  ]);
  // The first bytes on the wire are the SELECT, and the select answers 9000
  // when the applet is installed.
  expect(mock.isoDepHandler.transceive.mock.calls[0][0]).toEqual(selectAppletApdu());
});

test('an iOS error loses its message, so it is rethrown under the library class name', async () => {
  mock.isoDepHandler.transceive
    .mockResolvedValueOnce([0x90, 0x00])
    .mockRejectedValueOnce(new NfcError.TagConnectionLost());
  const error: Error = await withCard('hold', tx => tx([0x00])).then(
    () => {
      throw new Error('the run should have failed');
    },
    (e: unknown) => e as Error,
  );
  expect(error).toBeInstanceOf(Error);
  expect(error.message).toBe('TagConnectionLost');
});

test('an Android error keeps its own message', async () => {
  mock.isoDepHandler.transceive
    .mockResolvedValueOnce([0x90, 0x00])
    .mockRejectedValueOnce(new Error('Tag was lost'));
  const error: Error = await withCard('hold', tx => tx([0x00])).then(
    () => {
      throw new Error('the run should have failed');
    },
    (e: unknown) => e as Error,
  );
  expect(error.message).toBe('Tag was lost');
});

test('the session is torn down even when the run throws', async () => {
  mock.isoDepHandler.transceive.mockResolvedValue([0x90, 0x00]);
  await expect(
    withCard('hold', async () => {
      throw new Error('card refused');
    }),
  ).rejects.toThrow('card refused');
  expect(mock.cancelTechnologyRequest).toHaveBeenCalled();
});

test('two runs at once are refused, and the first still ends', async () => {
  mock.isoDepHandler.transceive.mockResolvedValue([0x90, 0x00]);
  let resolve!: () => void;
  const gate = new Promise<void>(r => {
    resolve = r;
  });
  const first = withCard('hold', async () => {
    await gate;
  });
  await expect(withCard('hold', async () => undefined)).rejects.toThrow(
    'already going',
  );
  resolve();
  await first;
  // And the next one works: the session was released.
  await expect(withCard('hold', async () => 'ok')).resolves.toMatchObject({
    value: 'ok',
  });
});

test('cancelling this caller ends its session; another caller cannot cancel it', async () => {
  mock.isoDepHandler.transceive.mockResolvedValue([0x90, 0x00]);
  const mine = createCardSession();
  const theirs = createCardSession();
  let resolve!: () => void;
  const gate = new Promise<void>(r => {
    resolve = r;
  });
  const run = mine.run('hold', async () => {
    await gate;
  });
  const outcome = run.catch((e: Error) => e);
  await Promise.resolve();
  await theirs.cancel(); // not the owner: nothing happens
  expect(mock.cancelTechnologyRequest).not.toHaveBeenCalled();
  await mine.cancel();
  resolve();
  expect(await outcome).toBeInstanceOf(Error);
  await cancelCard();
});

test('a SELECT with no status word is a broken card, named as such', async () => {
  mock.isoDepHandler.transceive.mockResolvedValue([]);
  await expect(withCard('hold', async () => 1)).rejects.toThrow(
    'no status word',
  );
  expect(mock.cancelTechnologyRequest).toHaveBeenCalled();
});

test('the session reports the platform it ran on, and iOS is asked for a sheet line', async () => {
  mock.isoDepHandler.transceive.mockResolvedValue([0x90, 0x00]);
  const {meta} = await withCard('hold the card', async () => undefined);
  expect(meta.platform).toBe(Platform.OS);
  expect(meta.os).toBe(String(Platform.Version));
  // The mock preset runs as iOS, where the sheet needs its own message; on
  // Android the manager has no such call.
  if (Platform.OS === 'ios') {
    expect(mock.setAlertMessageIOS).toHaveBeenCalledWith('hold the card');
  } else {
    expect(ANDROID_TIMEOUT_MS).toBe(15000);
    expect(mock.setTimeout).toHaveBeenCalledWith(ANDROID_TIMEOUT_MS);
  }
});

test('the iOS sheet line and its no-op on Android are fire and forget', () => {
  expect(() => setCardMessage('Hold the card')).not.toThrow();
  expect(phaseMessage('load')).toContain('keep the card still');
  expect(phaseMessage('selectIsd')).toContain('Hold the card');
});

test('the library name table covers its classes and nothing else', () => {
  expect(nfcErrorName(new NfcError.UserCancel())).toBe('UserCancel');
  expect(nfcErrorName(new NfcError.TagConnectionLost())).toBe(
    'TagConnectionLost',
  );
  expect(nfcErrorName(new NfcError.NfcErrorBase())).toBeUndefined();
  expect(nfcErrorName(new Error('nope'))).toBeUndefined();
});

test('a reply status word is read from the last two bytes', () => {
  expect(statusWord([0x02, 0x00, 0x90, 0x00])).toBe(0x9000);
  expect(statusWord([0x6a, 0x82])).toBe(0x6a82);
  expect(statusWord([0x90])).toBeNull();
});
