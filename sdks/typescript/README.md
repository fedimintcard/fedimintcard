# Fedimint Card for TypeScript

Read a Fedimint Card 4.x applet from a connected NFC or USB reader. The package also exposes the paired HTTPS till client for merchant applications.

```sh
npm install fedimintcard
```

For a local package build, install the path to the packed `.tgz` in place of the registry name.

```ts
import { FedimintCardReader, type Transceive } from 'fedimintcard';

export async function readTap(transceive: Transceive) {
  const card = await FedimintCardReader.connect(transceive);
  const { notesMsat, pendingChangeMsat } = await card.readBalance();
  return { version: card.version, notesMsat, pendingChangeMsat };
}
```

Implement `transceive` with your reader's ISO 7816 transport. It takes one command as a `Uint8Array` and returns a promise for the complete response bytes, including the two-byte status word. Open one card connection per tap and await operations in order. React Native apps can use the [iOS and Android NFC transport](../react-native/README.md).

| Reader method | Result |
| --- | --- |
| `readState()` | State of all 32 slots, card clock, spending limit, PIN and certificate status |
| `readSlot(index)` | The slot's key, amount and fields for its current state |
| `readPublicKey(index = 0)` | The x-only public key, or `null` for an empty slot |
| `readCertificate()` | The card's registry certificate, or `null` if none has been installed |
| `readBalance()` | Sum of on-card notes and a separate sum of change owed, each in millisatoshis as a `bigint` |

The note total describes what the card currently stores. Confirm spendability with the federation through a till. Exclude pending change from the spendable amount. Reading card state does not require a PIN.

To check an issuer signature, call `verifiesCertificate(certificate, observedSlot0, trustedRegistryKey)` with the slot-zero key read on the same connection and a trusted registry's uncompressed P-256 key. The check binds the certificate to those keys. Use the till's attestation flow when a payment requires proof that a genuine card is present.

`CardStatusError.statusWord` carries the card's ISO status. `UnsupportedCardError` reports an unsupported applet version. `CardProtocolError` reports a malformed reply. Transport failures propagate to the caller.

`TillClient.fromPairing(pairingText, options)` accepts a merchant's scanned HTTPS till pairing. Payment operations require an active card link. Use an invoice signature verifier for flows that display or pay a Lightning invoice. Mobile apps should follow the [paired transport requirements](../react-native/README.md#pairing-a-till).
