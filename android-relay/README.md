# android-relay

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
