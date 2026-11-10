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

JDK 17, Android SDK with platform 36, and the Rust that builds the phone's offline core
(`host/pos-core`, docs/POS-OFFLINE.md § Android): rustup (the toolchain `host/rust-toolchain.toml`
pins), its Android targets, cargo-ndk 4.1.2 and an NDK r27 or newer.

```sh
rustup target add --toolchain 1.91.1 aarch64-linux-android x86_64-linux-android
cargo install cargo-ndk --locked --version 4.1.2
export ANDROID_NDK_HOME=$HOME/Library/Android/sdk/ndk/<r27 or newer>
cd android-relay
echo "sdk.dir=$HOME/Library/Android/sdk" > local.properties
./gradlew test assembleDebug
```

Every build runs `host/scripts/pos-core-mobile.sh bindings kotlin` first (Rust only: the unit tests
need no NDK), and packaging runs `pos-core-mobile.sh android` for arm64-v8a and x86_64; Gradle
skips both while nothing the core is built from has changed.
Both write under `host/target/pos-core-mobile/` and are never committed: the bindings and the
libraries must come from one tree. The first build compiles the core (a few minutes).

APK: `android-relay/app/build/outputs/apk/debug/app-debug.apk` (arm64-v8a and x86_64 only; check
the 16 KB alignment with `zipalign -c -P 16 -v 4 <apk>` from build-tools 35 or newer).

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

Over the LAN instead (no tunnels, docs/POS-LINK.md): run `host/scripts/till-service.sh pair` on the
laptop, then in the app's settings "Pair with till" and scan the QR code (or paste the
`fedimintcard-till:` text). The URL and token are stored encrypted under a Keystore key. While
paired, each charge and balance read opens a card link (WebSocket) to the till once the card is on
the phone, answers the till's APDUs from it, and sends the job with `X-Card-Link`. "Unpair" goes
back to the USB path above.

To try the UI without the Rust service: `MOCK_SCENARIO=success python3 tools/mock_till.py`
(also `change`, `undecided` which records the sale after 8 s, or any error code such as `insufficient`), then `adb reverse` as above.

Paired, the phone takes a 3.0 card itself when the till cannot be reached ("Paid offline") and
syncs it when the till answers again; Settings shows the offline kit, what waits on the phone and
any charge the till refused (docs/POS-OFFLINE.md § Android).

Keep the app in the foreground while a card is being charged. The power button, Recents and
pulling down notifications drop the NFC reader mode and with it the card. The till copes: the
charge ends as `no_card` (nothing moved) or `undecided`/outcome unknown, in which case the app
keeps the sale's `client_ref`, polls `/v1/sales` and shows the receipt as soon as the sale
appears. "Try again" re-sends the same `client_ref`, so the card is never charged twice.
