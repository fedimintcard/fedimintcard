# fedimint-card

An experiment in NFC cards for [Fedimint](https://github.com/fedimint/fedimint) ecash.

The card holds the spend keys of Fedimint notes and signs spends with them; the phone or
till does the rest. Because Fedimint notes are spent with a Schnorr signature from each
note's own key, this needs no change to the Fedimint protocol and works with existing
federations.

Status: running in production for the Island Bitcoin Community federation, with 32-slot cards
(applet 2.1) since 2026-11-06: a registry, a till on StartOS, a Setup app for operators and a POS
app for merchants. Results on silicon and end to end: [docs/CAP-BUILD.md](docs/CAP-BUILD.md).

This is experimental software. Put on a card only what you can afford to lose.

## Layout

```
applet/         JavaCard applet: 32 slots, GEN_KEY, GET_PUB, SIGN, PIN, STATE_KEY; jCardSim tests
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

## Docs

| Read | For |
|---|---|
| [DESIGN-NOTE.md](docs/DESIGN-NOTE.md) | the design: how a card holds Fedimint notes, and what it trusts |
| [SPIKE.md](docs/SPIKE.md), [SPIKE-PLAN.md](docs/SPIKE-PLAN.md) | the first spike: one card, one federation |
| [CAP-BUILD.md](docs/CAP-BUILD.md) | building and installing the applet; 32 slots on silicon; the end-to-end run and the production rollout |
| [OPS-PLAN.md](docs/OPS-PLAN.md) | the registry ("ops") and how tills, holders and operators use it |
| [PROVISIONER-PLAN.md](docs/PROVISIONER-PLAN.md), [PROVISIONER-HANDOFF.md](docs/PROVISIONER-HANDOFF.md) | the Setup app and cardkit |
| [TILL-API.md](docs/TILL-API.md), [TILL-CLAIM.md](docs/TILL-CLAIM.md), [POS-LINK.md](docs/POS-LINK.md) | the till's HTTP API, connecting a till to the registry, pairing a POS |
| [TOPUP.md](docs/TOPUP.md) | Lightning top-up at the POS |
| [OFFLINE-PAYMENTS.md](docs/OFFLINE-PAYMENTS.md) | research: card payments with no internet for days |
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

## APDUs

CLA `B0`. Applet AID `F0464D4341524401`. Key slots: 8 on a 2.0 card, 32 on a 2.1 card (P1 = 0..7 or 0..31). P2 = 0 on every command, except GEN_KEY which also takes P2 = 1.

| Command | INS | Data | Response |
|---|---|---|---|
| GEN_KEY | `10` | none | 32-byte x-only pubkey; P2 = 0: `6985` if the slot is used; P2 = 1 (overwrite): wipes the slot and generates a new key, used or not |
| GET_PUB | `11` | none | 32-byte x-only pubkey; `6985` if empty |
| SIGN | `12` | 32-byte message | 64-byte BIP340 signature; `6985` if empty; needs a `VERIFY_PIN` in the same session |
| VERIFY_PIN | `40` | 4..8-byte PIN | `9000` and this session may SIGN; `63CX` wrong, X tries left; `6983` blocked; `6984` no PIN set |
| SET_PIN | `41` | 4..8-byte PIN | `9000` the first time; `6985` if a PIN is already set; does not open a session |
| CHANGE_PIN | `42` | old PIN then new PIN, 8..16 bytes, equal lengths | `9000`; needs a `VERIFY_PIN` first and ends the session; a length change needs `SET_PIN` on a re-keyed card |

`SELECT` answers `02009000`: the applet's version 2.0 in two data bytes, then the status word.

The applet is meant for contactless (T=1 style) use. GEN_KEY and GET_PUB call
`setIncomingAndReceive` on commands with no data, which may stall on T=0 contact transports;
step 7 must re-check this on the real card.

Status words: `9000` ok, `6700` wrong length, `6A94` PIN required, `6983` PIN blocked,
`6984` no PIN set, `6B00` bad P1/P2, `6985` state refused, `6D00`
unknown INS, `6E00` wrong CLA, `6F00` crypto failure.

## License

MIT
