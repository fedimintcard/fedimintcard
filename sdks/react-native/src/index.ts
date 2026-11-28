import { Platform } from 'react-native';
import NfcManager, { NfcAdapter, NfcTech } from 'react-native-nfc-manager';
import { FedimintCardReader } from 'fedimintcard';
import { makeSession, type NfcSessionOptions } from './session.js';

export type { NfcSessionOptions } from './session.js';

export async function cardReaderAvailable(): Promise<boolean> {
  if (Platform.OS !== 'android' && Platform.OS !== 'ios') return false;
  if (!(await NfcManager.isSupported())) return false;
  await NfcManager.start();
  return NfcManager.isEnabled();
}

/** The transport is valid only inside the callback. Await every command inside it. */
export const withNfcTransport = makeSession({
  async start() {
    if (Platform.OS !== 'android' && Platform.OS !== 'ios') throw new Error('NFC requires Android or iOS');
    await NfcManager.start();
    if (Platform.OS === 'android') {
      // Own foreground dispatch before requestTechnology starts waiting for a tap.
      await NfcManager.registerTagEvent({
        isReaderModeEnabled: true,
        readerModeFlags: NfcAdapter.FLAG_READER_NFC_A | NfcAdapter.FLAG_READER_SKIP_NDEF_CHECK,
      });
    }
  },
  async open(prompt) {
    await NfcManager.requestTechnology(NfcTech.IsoDep, { alertMessage: prompt });
  },
  async configure() {
    if (Platform.OS === 'android') await NfcManager.setTimeout(5000);
  },
  async exchange(command) {
    // The manager's IsoDep handler joins iOS body/SW1/SW2 and uses Android transceive.
    const reply = await NfcManager.isoDepHandler.transceive(Array.from(command));
    if (!Array.isArray(reply) || !reply.every((b) => Number.isInteger(b) && b >= 0 && b <= 255)) {
      throw new Error('Invalid NFC response');
    }
    return Uint8Array.from(reply);
  },
  async close() {
    try {
      // End discovery first so a late Android request cannot begin waiting for a tag.
      if (Platform.OS === 'android') await NfcManager.unregisterTagEvent();
    } finally {
      await NfcManager.cancelTechnologyRequest();
    }
  },
});

export function withFedimintCard<T>(
  run: (reader: FedimintCardReader) => Promise<T>,
  options?: NfcSessionOptions,
): Promise<T> {
  return withNfcTransport(async send => run(await FedimintCardReader.connect(send)), options);
}
