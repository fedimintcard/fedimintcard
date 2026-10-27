# android-relay

Two things in one app: a point-of-sale UI ("Fedimint Card POS", below) and the NFC relay it is built on.

## Relay

Turns an NFC phone into the card reader for the fedimint-card demo. It runs the same HTTP API as
the laptop `bridge/`, but forwards APDUs to a real card over NFC (IsoDep) instead of a simulator.

- `GET /health` -> `{"ok":true}`
- `POST /reset` -> `{"ok":true}` (no-op on a real card)
- `POST /apdu` `{"apdu":"<hex>"}` -> `{"response":"<hex data||SW>"}`; `503 {"error":"no card"}` if no card is held to the phone

Port 9876, all interfaces. No auth: use it on a trusted network, or only through `adb forward`.
APDU data is never logged; only INS and SW.

## Build

JDK 17, Android SDK with platform 35.

```sh
cd android-relay
echo "sdk.dir=$HOME/Library/Android/sdk" > local.properties
./gradlew test assembleDebug
```

APK: `android-relay/app/build/outputs/apk/debug/app-debug.apk`

## Run (USB, normal path)

```sh
adb install -r app/build/outputs/apk/debug/app-debug.apk
adb forward tcp:9876 tcp:9876
```

Open "Card Relay" on the phone and keep it in the foreground (reader mode only works then).
Hold the card against the phone's NFC antenna until the screen says "card present". Then, from `host/`:

```sh
CARD_BACKEND=bridge CARD_BRIDGE_URL=http://127.0.0.1:9876 cargo run ...   # your usual sidecar command
curl http://127.0.0.1:9876/health
```

## Wi-Fi alternative

Put the phone and laptop on the same network; the app shows the phone's IP. Use

```sh
CARD_BACKEND=bridge CARD_BRIDGE_URL=http://<phone-ip>:9876 ...
```

## Point of sale

The app is also a till UI: enter an amount, hold the card to the phone, and the app asks the
laptop's till service (`card-sidecar till serve`, API in `docs/TILL-API.md`) to charge it. The
till drives the card back through this app's relay on port 9876. The old relay status screen is
still there: open settings (gear, or long-press the title) and choose "Card relay".

Both tunnels are needed, run from the laptop with the phone on USB:

```sh
adb forward tcp:9876 tcp:9876   # laptop -> phone relay (the till uses this)
adb reverse tcp:9877 tcp:9877   # phone -> laptop till service (the app uses this)
```

Both are dropped when USB reconnects (cable bump, adb restart): re-run them. The status dot in
the header turns red after two failed status polls. The till URL defaults to
`http://127.0.0.1:9877` and can be changed in settings. The app uses cleartext HTTP
(`usesCleartextTraffic`); keep the till on loopback or an `adb` tunnel, there is no auth.

To try the UI without the Rust service: `MOCK_SCENARIO=success python3 tools/mock_till.py`
(also `change`, `undecided` which records the sale after 8 s, or any error code such as `insufficient`), then `adb reverse` as above.

Keep the app in the foreground while a card is being charged. The power button, Recents and
pulling down notifications drop the NFC reader mode and with it the card. The till copes: the
charge ends as `no_card` (nothing moved) or `undecided`/outcome unknown, in which case the app
keeps the sale's `client_ref`, polls `/v1/sales` and shows the receipt as soon as the sale
appears. "Try again" re-sends the same `client_ref`, so the card is never charged twice.
