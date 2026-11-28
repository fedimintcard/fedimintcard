# Fedimint Card for Flutter

Read a Card 4.x applet over an NFC session on an Android phone or iPhone. The card reports notes held and change owed in millisatoshis. A merchant confirms a payment through its paired till.

Install the package with `flutter pub add fedimintcard_flutter` after publication, or use a path dependency on this directory while testing. Flutter 3.47 or later, Java 17, Android API 24+ and iOS 13+ are required by the NFC transport. Follow the [Flutter NFC Kit platform setup](https://pub.dev/packages/flutter_nfc_kit#setup) for compatible Gradle versions.

Add `android.permission.NFC` to the Android manifest. In Xcode enable Near Field Communication Tag Reading, with the TAG session entitlement. In Info.plist add a user-visible `NFCReaderUsageDescription` and include `F0464D4341524401` in the string array `com.apple.developer.nfc.readersession.iso7816.select-identifiers`. Scan on a physical NFC-capable device.

```dart
import 'package:fedimintcard_flutter/fedimintcard_flutter.dart';

Future<BigInt> readStoredMsat() async {
  final balance = await withFedimintCard((reader) => reader.balance());
  return balance.storedMsat;
}
```

Call from a user gesture. The session closes after the callback or on error. Proceed with one awaited read at a time. A custom transport can import `package:fedimintcard/fedimintcard.dart`, pass a callback to `FedimintCardReader.select` and return response data with SW1 and SW2. The reader requires a new SELECT for each connection. Slot identifiers range from 0 to 31. Use `verifyCertificate(certificate, registryKey, cardIdentity)` with the independently pinned, uncompressed P-256 registry key and slot-zero identity. A certificate proves issuance. Confirming possession also requires a live challenge.

When testing before publication, point the consuming app at the local `fedimintcard_flutter` directory. Add a consumer-level `dependency_overrides` entry for `fedimintcard` that points to the core Dart directory. Published apps can resolve both packages from pub.dev.
