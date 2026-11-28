# Fedimint Card for Dart

Read public Card 4.x records using an ISO 7816 transport, verify the issuing registry’s certificate signature, or connect to a paired till. Install with `dart pub add fedimintcard` after registry publication. During development use a path dependency pointing to this directory. For NFC on iOS and Android use the companion `fedimintcard_flutter` adapter.

```dart
import 'package:fedimintcard/fedimintcard.dart';

Future<CardBalance> balance(Transceive exchange) async {
  final reader = await FedimintCardReader.select(exchange);
  try {
    return await reader.balance();
  } finally {
    reader.close();
  }
}
```

The transport receives a complete ISO 7816 command and returns bytes containing the payload and SW1/SW2. Select once per connection and await reads in sequence. Slot zero provides the card identity. Balances separate card-reported stored and owed amounts in millisatoshis. Use the paired till to confirm payment. For certificate issuance verification, pass the raw certificate, independently pinned uncompressed registry public key and observed slot-zero identity to `verifyCertificate`. Proving live possession also requires a card challenge.

### Paired till

Use `TillClient.fromPairing(pairingText)` for the HTTPS endpoint and token issued by the merchant’s till. The POS scope supports balance, charge, top-up quotes and loading. Pay-only scope supports a Lightning payment and status lookup. Top-up quotes and outgoing payments require the host wallet’s BOLT11 signature, amount, network and expiry verifier. Persist a unique operation reference before sending a charge, quote or payment. After an interrupted request, look up its original reference before attempting another payment.

Pass the active card link from the paired till’s APDU relay to operations that need a tap. The direct reader above reads the card locally. Securely store pairing tokens in the host app and keep PIN entry within the foreground transaction.

```dart
import 'package:fedimintcard/fedimintcard.dart';

Future<int> readLiveMsat(String pairingText, String activeCardLink) async {
  final till = TillClient.fromPairing(pairingText);
  final balance = await till.balance(activeCardLink);
  return balance.liveMsat;
}
```

On Android, apps connecting to the till need the Internet permission in the main application manifest. The NFC companion app also needs the NFC permission and the platform NFC configuration described by `fedimintcard_flutter`.
