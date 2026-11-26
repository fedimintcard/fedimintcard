# fedimint-card

An experiment in NFC cards for [Fedimint](https://github.com/fedimint/fedimint) ecash.

The card holds the spend keys for its Fedimint notes. When the holder pays, the card signs the spend and the till settles it with the federation.

## Card 4.0 merchant till

The till holds the merchant's wallet. Use Till admin to join a federation, pair a POS phone, review sales and pay out the proceeds. POS can also load a customer's card from one Lightning payment, even when the till starts with an empty wallet. The customer reviews the card credit, fixed top-up fee and total before choosing **Show payment QR**. [Top-up guide](docs/TOPUP.md) · [Sales overview](docs/images/till/overview.png) · [Payouts](docs/images/till/payouts.png)

**Compatibility.** Use a Card 4.x card and a Card 4.0 registry that supports your federation's Bitcoin mint (v1 or v2). In Fedimint Card Setup, choose the federation the till joined and sign in to the registry that will approve its claim. Keep a compatible till for older cards. Spend or withdraw their balance and settle outstanding holds before resetting them. [Registry pairing](docs/TILL-CLAIM.md)

### Install the till and POS

| Platform | Release 0.1.0 |
|---|---|
| Docker | Use the [tagged Compose file](https://github.com/fedimintcard/fedimintcard/blob/till-v0.1.0/compose.yaml) for Linux amd64 or arm64. [Installation and backup](docs/TILL-DOCKER.md) |
| StartOS | Back up an existing Card 4.0 till. Sideload revision `0.1.0:12` for [x86_64](https://github.com/fedimintcard/fedimintcard/releases/download/till-v0.1.0/fedimintcard-till-0.1.0-12-x86_64.s9pk) or [aarch64](https://github.com/fedimintcard/fedimintcard/releases/download/till-v0.1.0/fedimintcard-till-0.1.0-12-aarch64.s9pk). [Package checksums](https://github.com/fedimintcard/fedimintcard/releases/download/till-v0.1.0/SHA256SUMS) · [Setup and pairing](https://github.com/bobodread876/fedimintcard-till-startos/blob/7ddd06d51ab1297c90edb48e94b685b29d16d2b0/instructions.md) |
| Umbrel | Follow the [official store submission](https://github.com/getumbrel/umbrel-apps/pull/6423) for availability. |
| Android POS | For a fresh install or an update signed by the same distributor, download [Fedimint Card POS 1.1.0 (8)](https://github.com/fedimintcard/fedimintcard/releases/download/till-v0.1.0/FedimintCard-POS-1.1.0-8.apk). The APK supports arm64-v8a and x86_64. For an existing Play installation, keep its data and update through Play when a compatible version is available. [APK checksum](https://github.com/fedimintcard/fedimintcard/releases/download/till-v0.1.0/FedimintCard-POS-1.1.0-8.apk.sha256) |
| Setup app | The operator uses a Card 4.0 Setup build to provision cards and approve tills. Check the build with your distributor. This release includes the [Setup source and operator guide](apps/provisioner/README.md). |
| iOS POS | The merchant needs a build with Card 4.0 and fee-inclusive top-ups. Ask your distributor to confirm its build. The tagged release includes the [iOS source](https://github.com/fedimintcard/fedimintcard/tree/till-v0.1.0/ios-pos). |

Fedimint Card is experimental software. Put on a card only what you can afford to lose. [Till release notes](https://github.com/fedimintcard/fedimintcard/releases/tag/till-v0.1.0)

## Layout

```
applet/         Card 4.0 JavaCard applet, 32 key slots, sealed PIN and jCardSim tests
bridge/         HTTP bridge that serves the simulated applet on 127.0.0.1
host/           Rust workspace: card transport, the till (card-sidecar) and federation tests (host/README.md)
registry/       Cloudflare Worker: cards, notes, tills and operators; nightly encrypted backup
apps/provisioner/  Fedimint Card Setup, the operator's app (React Native, Android and iOS)
tools/cardkit/  the GlobalPlatform engine the Setup app installs the applet with
android-relay/  Fedimint Card POS for Android
ios-pos/        Fedimint Card POS for iOS
scripts/        release builds
docs/           plans, protocols and results (below)
```

See the release guide for the GitHub APK, TestFlight and Google Play builds. [Mobile releases](docs/RELEASING.md)

## Docs

| Read | For |
|---|---|
| [DESIGN-NOTE.md](docs/DESIGN-NOTE.md) | the design: how a card holds Fedimint notes, and what it trusts |
| [SPIKE.md](docs/SPIKE.md), [SPIKE-PLAN.md](docs/SPIKE-PLAN.md) | the first spike: one card, one federation |
| [CAP-BUILD.md](docs/CAP-BUILD.md) | building and installing the applet; 32 slots on silicon; the historical end-to-end run and rollout |
| [HARDWARE_TEST_REPORT_2026-11-19.j3r452.md](docs/HARDWARE_TEST_REPORT_2026-11-19.j3r452.md) | hardware test report: the Setup app and a staging till on a J3R452 and a J3R180, 2026-11-15 to 2026-11-19 |
| [OPS-PLAN.md](docs/OPS-PLAN.md) | the registry ("ops") and how tills, holders and operators use it |
| [PROVISIONER-PLAN.md](docs/PROVISIONER-PLAN.md), [PROVISIONER-HANDOFF.md](docs/PROVISIONER-HANDOFF.md) | the Setup app and cardkit |
| [TILL-API.md](docs/TILL-API.md), [TILL-CLAIM.md](docs/TILL-CLAIM.md), [POS-LINK.md](docs/POS-LINK.md) | the till's HTTP API, connecting a till to the registry, pairing a POS |
| [TOPUP.md](docs/TOPUP.md) | Lightning top-up at the POS |
| [OFFLINE-PAYMENTS.md](docs/OFFLINE-PAYMENTS.md) | research: card payments with no internet for days |
| [OFFLINE-PLAN.md](docs/OFFLINE-PLAN.md) | plan: offline card payments, as built and drilled |
| [OFFLINE-PROTOCOL.md](docs/OFFLINE-PROTOCOL.md) | the contract between card, registry, till and apps |
| [ROLLOUT-OFFLINE.md](docs/ROLLOUT-OFFLINE.md) | dated rollout checklist and deployment history |
| [DEMO.md](docs/DEMO.md) | the first demo script (2026-10-27, laptop till) |

## Build

Needs JDK 11+ and Maven.

```sh
mvn -B test                                   # applet and bridge tests
mvn -B package -DskipTests                    # builds bridge/target/bridge.jar
java -jar bridge/target/bridge.jar --port 9876
```

The bridge listens on 127.0.0.1 only. `POST /apdu` takes `{"apdu":"<hex>"}` and returns
`{"response":"<hex data||SW>"}`; `POST /reset` gives a fresh card; `GET /health`. Keys are seeded
from `SecureRandom` per card; `--fixed-keys` turns that off (jCardSim then makes the same first
key every time).

## Card protocol

The Card 4.0 contactless applet uses AID `F0464D4341524401`, command class `B0`, and 32 key slots. A successful `SELECT` returns version bytes `04 00` followed by status `90 00`. The host verifies the card and seals PIN commands to its authenticated key using a fresh nonce. See the protocol for the commands, status words and PIN handling. [Card protocol](docs/OFFLINE-PROTOCOL.md) · [Sealed PIN](docs/OFFLINE-PROTOCOL.md#9-the-sealed-pin-fedimint-card-40) · [Applet command reference](applet/src/main/java/io/github/bobodread876/fedimintcard/applet/FedimintCardApplet.java)

## License

MIT
