# Demo script: Fedi ecash on a card

Audience: Fedi. Length: about eight minutes live, or a three minute recording.
Rehearsed end to end on 2026-10-27 against the Island Bitcoin Community federation
(4 guardians, core consensus 2.1, mint module `mint` v2.0, mint fee 0 msat).

## How this maps to a product

The Fedi app and the sidecar never talk to each other. They share the federation, and value
moves between them as ordinary ecash strings. The sidecar is a second Fedimint client that does
what the Fedi app cannot yet: issue notes to keys that live on a card, and have the card sign
when those notes are spent. It is the reference implementation, not the product.

Where the logic goes next, in order of preference:

1. Inside the Fedi app: the card module joins the app's own client; "Load card" and "Tap to pay"
   replace the copy and paste. This is the ask.
2. As a Fedi mod, once the app exposes NFC transceive and note issuance to mods (the bridge hook
   in `docs/DESIGN-NOTE.md`). Smaller ask, same result for the user.
3. On the merchant side: the reference point of sale in this repo (the Android app plus the till
   service) is the shape of a merchant product. Fedi has no point of sale today.

The copy and paste in step 1 of the live run is the seam where integration happens. Everything
below it, the card, the federation, the till, is finished.

## What the audience sees

1. Ecash leaves the Fedi app as a normal "send ecash" string.
2. The string is loaded into a small wallet (the sidecar) that puts notes on a tap card.
3. The card pays a till by tap. The till is an Android phone (or a USB reader on a laptop).
   The federation accepts the spend; change comes back to the card in the same transaction.
4. The card is cashed out to an ecash string that the Fedi app receives like any other.

No Fedi code changed. The Fedi app only ever sees ecash strings it already understands.

## Kit

- NXP J3R180 card with the 8-slot `fedimint-card` applet installed (`docs/CAP-BUILD.md`).
- Till: Pixel with the "Fedimint Card POS" app (`android-relay/README.md`) on USB, plus the till
  service on the laptop (`card-sidecar till serve`, API in `docs/TILL-API.md`). Two adb tunnels,
  re-run both after any USB reconnect:
  `adb forward tcp:9876 tcp:9876` (laptop -> phone NFC relay) and
  `adb reverse tcp:9877 tcp:9877` (phone -> till service).
  Fallback till: ACS ACR122U on the laptop (`CARD_BACKEND=pcsc`) with the CLI `card spend`.
- Laptop with the sidecar built: `cd host && cargo build -p fedimint-card-sidecar --features pcsc`.
  Binary: `host/target/debug/card-sidecar`.
- A Fedi app joined to the federation, holding a few thousand sats.
- Wallet data directory: `~/.local/share/fedimint-card-demo/wallet` (mnemonic, client.db,
  card_state.json). Never put it under /tmp.

Set once per terminal:

```sh
export CS=host/target/debug/card-sidecar
export D=~/.local/share/fedimint-card-demo/wallet
export CARD_BACKEND=bridge CARD_BRIDGE_URL=http://127.0.0.1:9876   # phone till
# or: export CARD_BACKEND=pcsc                                        # laptop reader till
```

## Before the demo (not shown)

```sh
$CS --data-dir $D join <invite code>        # first time only
$CS --data-dir $D info                      # federation name, versions, fee, balances
$CS --data-dir $D card setup                # generates keys in the 8 slots
```

`info` must show the mint module as kind `mint` and a core consensus of 2.1 or later.
Make sure the Fedi app and the sidecar are on the same federation id.

## Live run

### 1. Fedi app to card (about 90 s)

In Fedi: Send, ecash (offline), 2,000 sats, copy the string.

```sh
$CS --data-dir $D ecash import "<string from Fedi>"
# reissued 2000000 msat (2000.000 sat) (fees 0 msat)
$CS --data-dir $D card topup 1048576 524288 262144 131072
# slot 0 +1048576 msat ... slot 3 +131072 msat
$CS --data-dir $D card balance
```

Say: four notes now live on the card, each bound to a key that never leaves the chip.
The federation issued them to the card's keys, not to the laptop.

### 2. Start the till (not shown, 10 s)

The till service owns the wallet directory while it runs: stop it before any `card ...` CLI
command and start it again after.

```sh
$CS --data-dir $D till serve        # prints "listening on http://127.0.0.1:9877"
```

On the Pixel open Fedimint Card POS. The header shows the federation name, a green dot (till and
relay reachable) and a "Card: none" chip.

### 3. Tap to pay at the point of sale (about 20 s each)

Cashier keys `300`, presses Charge, customer holds the card to the phone. The screen goes
"Hold card to phone" -> "Card present" -> "Signing… 7s" -> green receipt: 300 sats, change
returned to card (5 notes), txid. Press New sale.

Repeat with `100`: receipt again, two of the change notes consumed, one new change note.

Then key `900` with 486 sats left on the card: "Card balance too low", nothing moved. Back.

Say: the phone is the till, the Rust service behind it is the merchant's back office and holds the
merchant wallet. Each sale carries a reference, so a dropped connection or a retry can never
charge the card twice; the service and the app were both reviewed for exactly that. The Sales
screen shows the ledger. Show it.

Fallback if the phone misbehaves: the same sale from the laptop reader,
`CARD_BACKEND=pcsc $CS --data-dir $D card spend --price 300000` (stop `till serve` first).

### 4. Card back to Fedi app (about 60 s)

```sh
$CS --data-dir $D card cashout 0 --qr
# exported 131072 msat ... then a QR and the string
```

In Fedi: Receive, ecash, scan or paste. Balance goes up.

Say: this is the recovery path and the "give the card back" path. It is plain ecash with the
federation invite attached, so a new phone can join and receive in one step.

## If something goes wrong

- `503 no card`: lift and re-seat the card on the phone; the relay app must be in the foreground.
- `SCARD_E_NOT_TRANSACTED` on the ACR122U: unplug and replug the reader.
- A spend that was submitted but not reported accepted: run `card balance`. It reconciles
  every live note with the federation and finishes or abandons the pending transaction.
- Slots marked `to renew`: the next `topup` regenerates them; no funds are at risk.
- Switch till: change `CARD_BACKEND` and rerun the same command. The wallet state is the same.
- POS shows a red dot or "Till service unreachable": re-run `adb reverse tcp:9877 tcp:9877`.
- POS shows "Phone relay unreachable": re-run `adb forward tcp:9876 tcp:9876`.
- POS shows "Payment pending" or "Outcome unknown": do nothing rash. The app polls the ledger and
  turns it into a receipt when the federation decides; "Try again" re-sends the same reference and
  cannot double charge. "Abandon" is the only way to drop it, and it asks first.
- Power button, Recents or a notification during Signing drops the card; the till answers no_card
  or undecided and the app handles both.

## Numbers from the rehearsal (2026-10-27)

| Operation | ACR122U on laptop | Pixel till (USB relay) |
|---|---|---|
| ecash import 2,000 sat | under 5 s | n/a (no card involved) |
| topup of 4 notes | under 10 s | 2 s for one note |
| spend, 2 inputs, no change | 7 s | |
| spend, 1 input, 5 change notes | 8 s | 6 s (1 input, 2 change notes); 8 s from the POS app |
| spend, 7 inputs, no change | | 16 s (7 card signatures) |
| POS: insufficient balance refused | | under 3 s |
| cashout of one slot to a string | under 10 s | |

Card-side signature: median 847 ms per input on the J3R180 (`docs/SPIKE.md`).
