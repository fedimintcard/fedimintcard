# Build with Fedimint Card

Use a reader to inspect a Card 4.x during one tap. Use the paired till when a merchant needs an accepted sale, a Lightning top-up, or the balance the federation recognizes. The till must use a compatible Card 4.0 registry and the same federation and mint as the card.

| Developing for | Package | First operation |
| --- | --- | --- |
| TypeScript | [fedimintcard](typescript) | Supply an ISO 7816 transport and read a card. The same package includes the paired till client. |
| React Native for iPhone or Android | [fedimintcard-react-native](react-native) with the TypeScript reader | Open an NFC session from a button and read the card while the phone holds it. |
| Rust | [fedimintcard](rust) | Give the reader a transport or use the USB PC/SC example. |
| Dart | [fedimintcard](dart) | Read a card through an injected transport and use the paired till client in a standalone Dart project. |
| Flutter on iPhone or Android | [fedimintcard_flutter](flutter) with the Dart reader | Open a foreground NFC session and read a card. |

Each package directory contains its install instructions, platform requirements and a first read. These examples target SDK version 0.1.0 and the Card 4.x wire protocol.

## First tap in a React Native app

Configure NFC as described in the [React Native setup](react-native/README.md), then call this function from a button. A connected card supplies the version, the notes it currently holds and its pending change. Amounts are whole millisatoshis; 1,000 millisatoshis make one satoshi. Keep the amounts as integers.

```ts
import { withFedimintCard } from 'fedimintcard-react-native';

export async function readCard() {
  return withFedimintCard(async card => {
    const { notesMsat, pendingChangeMsat } = await card.readBalance();
    return {
      applet: card.version,
      notesMsat: notesMsat.toString(),
      pendingChangeMsat: pendingChangeMsat.toString(),
    };
  });
}
```

A card read is a snapshot. Display pending change separately. The issuer signature on a certificate can bind its keys to a registry. To establish that the tap comes from the certified chip, a payment flow also needs a fresh challenge. Use the established till flow for settlement. Treat card identifiers, certificates and raw NFC data as private.

## Build a paired checkout

Scan the QR supplied by a till with an HTTPS address. Keep the pairing secret in the platform's protected storage. Select a transport that enforces TLS and refuses redirects before forwarding credentials. The TypeScript client uses the Fetch API’s redirect-error mode. For React Native, supply a fetch adapter that refuses redirects and load a complete WHATWG URL implementation as described in the [mobile setup](react-native/README.md).

For a card operation the host app holds the tapped card, opens an authenticated card link to the **same till** and passes the ID from its ready message. End the link and the NFC session after the operation. The SDK provides the HTTP requests; for the foreground session use the [shipped iPhone card link](../ios-pos/FedimintCardPOS/CardLink.swift) and [card-link protocol](../docs/POS-LINK.md). An ISO 7816 response or a card's short ID cannot be substituted for a link ID.

- **Balance.** Call `till.balance(cardLink)` for the till's spendable amount and any held amount.
- **Charge.** Save an order reference before calling `till.charge` with the price in millisatoshis and the holder's PIN. Read `collectionPending`, `rejected` and `doubleSpent` before treating a charge as collected. If the response is interrupted, reconcile the original reference with `findSale`. An absent receipt leaves the outcome open. The released offline sales ledger can omit its rejection flag. An unknown flag is insufficient to mark an offline sale as settled. Keep the original reference and the same card for any retry.
- **Top up.** Read the card ID and request a fee-inclusive quote. Supply the wallet's signature-validating BOLT11 decoder with its expected network, amount, hash and expiry. Present the card credit, the fixed fee, the total and the deadline for the holder to review before showing the invoice. Keep the top-up ID. Poll the same top-up after paying. Once paid, load it on the original card. A closed payment window with unresolved settlement calls for polling the existing ID. The till keeps already paid credit available for the original card after a federation mismatch.
- **Pay a wallet's invoice.** Use a pay-enabled till's **pay-only** pairing. The [wallet example](examples/pay-with-wallet.ts) binds a tap to the saved wallet request and verifies the returned Lightning preimage. The till can charge its configured margin and reserve, and card denominations may lead to overpayment. Show the operator's terms, invoice and fee reserve before taking the PIN.

The till's own status and refund handling apply to any operation left pending or owed. New references can create new payments. Keep PINs, the pairing QR, preimages, and NFC frames out of analytics and logs. [Till API and recovery](../docs/TILL-API.md) · [Wallet payment flow](../docs/PAY.md)

The first SDKs cover direct card reads and paired online operations. Use the shipping POS for phone-offline acceptance. Installation, personalization and reset belong to the [issuer tools](../tools/cardkit).
