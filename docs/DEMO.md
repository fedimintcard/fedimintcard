# Demo script: Fedi ecash on a card

Audience: Fedi. Length: about eight minutes live, or a three minute recording.
Rehearsed end to end on 2026-10-27 against the Island Bitcoin Community federation
(4 guardians, core consensus 2.1, mint module `mint` v2.0, mint fee 0 msat).

## What the audience sees

1. Ecash leaves the Fedi app as a normal "send ecash" string.
2. The string is loaded into a small wallet (the sidecar) that puts notes on a tap card.
3. The card pays a till by tap. The till is an Android phone (or a USB reader on a laptop).
   The federation accepts the spend; change comes back to the card in the same transaction.
4. The card is cashed out to an ecash string that the Fedi app receives like any other.

No Fedi code changed. The Fedi app only ever sees ecash strings it already understands.

## Kit

- NXP J3R180 card with the 8-slot `fedimint-card` applet installed (`docs/CAP-BUILD.md`).
- Till: Pixel with the `android-relay` app (`android-relay/README.md`), on USB with
  `adb forward tcp:9876 tcp:9876`. Fallback till: ACS ACR122U on the laptop (`CARD_BACKEND=pcsc`).
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

### 2. Tap to pay, exact (about 15 s)

Hold the card to the phone (screen says "card present").

```sh
$CS --data-dir $D card spend --price 393216
# submitted, accepted by the federation: <txid>
# inputs [(2, 262144), (3, 131072)]; till received 393216 msat; change notes []
```

Say: the card signed the spend itself (about 0.8 s per signature on the chip). The till
holds the sats now. Seven seconds wall clock, most of it waiting for the federation.

### 3. Tap to pay, with change (about 20 s)

```sh
$CS --data-dir $D card spend --price 300000
# inputs [(1, 524288)]; till received 301056 msat (overpaid 1056 msat)
# change notes [(0, 131072), (2, 65536), (3, 16384), (4, 8192), (5, 2048)]
$CS --data-dir $D card balance
```

Say: one 512 sat note went in, five change notes came back onto the card in the same
federation transaction. The one sat overpay is the planner refusing to create dust;
on a federation with fees the same rule bounds the overpay to under 2 sats.

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

## Numbers from the rehearsal (2026-10-27)

| Operation | ACR122U on laptop | Pixel till (USB relay) |
|---|---|---|
| ecash import 2,000 sat | under 5 s | n/a (no card involved) |
| topup of 4 notes | under 10 s | 2 s for one note |
| spend, 2 inputs, no change | 7 s | |
| spend, 1 input, 5 change notes | 8 s | 6 s (1 input, 2 change notes) |
| spend, 7 inputs, no change | | 16 s (7 card signatures) |
| cashout of one slot to a string | under 10 s | |

Card-side signature: median 847 ms per input on the J3R180 (`docs/SPIKE.md`).
