# Fedimint Card NFC for React Native

Use a foreground NFC tap to read a Fedimint Card on iOS or Android.

```sh
npm install fedimintcard fedimintcard-react-native react-native-nfc-manager@^3.17.2
```

For local builds, install both SDK tarballs in the same command. Add the native NFC configuration below and rebuild the app on a phone with NFC.

```ts
import { cardReaderAvailable, withFedimintCard } from 'fedimintcard-react-native';

export async function readCard(signal: AbortSignal) {
  if (!(await cardReaderAvailable())) throw new Error('NFC is unavailable');
  return withFedimintCard(
    reader => reader.readBalance(),
    { signal, prompt: 'Hold your Fedimint Card near the top of the phone.' },
  );
}
```

The result contains `notesMsat` and `pendingChangeMsat` as separate `bigint` values. See the [reader methods](../typescript/README.md) for state, slots and certificates.

Give the library exclusive access to the NFC manager during a tap. It opens one ISO-DEP session, runs the callback, and releases the session on return, cancellation or error. Abort the signal when leaving the scan screen. Await each command inside the callback. Android uses NFC-A and NFC-B reader modes and skips NDEF dispatch while scanning. For a custom foreground card operation, `withNfcTransport(async transceive => ..., options)` exposes that same scoped connection.

## Native configuration

**iOS**. Enable the Near Field Communication Tag Reading capability on the app's signing identifier and in Xcode. Include `TAG` in the `com.apple.developer.nfc.readersession.formats` entitlement. Set `NFCReaderUsageDescription` to a user-facing explanation and include `F0464D4341524401` in the Info.plist array `com.apple.developer.nfc.readersession.iso7816.select-identifiers`. Install the native pods and rebuild.

**Android**. Include `android.permission.NFC` in AndroidManifest.xml. If other app features can work without NFC, declare the `android.hardware.nfc` feature with `android:required="false"` and check availability before starting a tap. The NFC manager links through React Native autolinking.

**Expo**. Use a native development build with the NFC manager's config plugin:

```json
{
  "expo": {
    "plugins": [
      ["react-native-nfc-manager", {
        "nfcPermission": "Read your Fedimint Card when you tap.",
        "selectIdentifiers": ["F0464D4341524401"],
        "includeNdefEntitlement": false
      }]
    ]
  }
}
```

Changes to native permissions and entitlements require a fresh native build.

## Pairing a till

The reader and NFC transport work independently of a network. To use the optional `TillClient` in React Native, load a complete WHATWG URL implementation before importing the SDK (for example, install `react-native-url-polyfill` and import `react-native-url-polyfill/auto` at the app entry point). Supply `TillOptions.fetch` backed by an HTTPS client configured to reject redirects **before following** them. Check this behavior on both phone platforms. The default React Native fetch cannot enforce that guarantee for paired requests.
