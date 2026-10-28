# fedimint-card

An experiment in NFC cards for [Fedimint](https://github.com/fedimint/fedimint) ecash.

The card holds the spend keys of Fedimint notes and signs spends with them; the phone or
till does the rest. Because Fedimint notes are spent with a Schnorr signature from each
note's own key, this needs no change to the Fedimint protocol and works with existing
federations.

Status: spike done, including on a real J3R180. Plan: [docs/SPIKE-PLAN.md](docs/SPIKE-PLAN.md). Results: [docs/SPIKE.md](docs/SPIKE.md). Design: [docs/DESIGN-NOTE.md](docs/DESIGN-NOTE.md).

This is experimental software. Do not put real funds on a card.

## Layout

```
applet/   JavaCard applet (GEN_KEY, GET_PUB, SIGN, PIN) and jCardSim tests
bridge/   HTTP bridge that serves the simulated applet on 127.0.0.1
host/     Rust workspace: card transport and federation tests (see host/README.md)
docs/     spike plan, results, CAP build notes, design note
```

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

CLA `B0`. Applet AID `F0464D4341524401`. Eight key slots (P1 = 0..7). P2 = 0 on every command, except GEN_KEY which also takes P2 = 1.

| Command | INS | Data | Response |
|---|---|---|---|
| GEN_KEY | `10` | none | 32-byte x-only pubkey; P2 = 0: `6985` if the slot is used; P2 = 1 (overwrite): wipes the slot and generates a new key, used or not |
| GET_PUB | `11` | none | 32-byte x-only pubkey; `6985` if empty |
| SIGN | `12` | 32-byte message | 64-byte BIP340 signature; `6985` if empty; needs a `VERIFY_PIN` in the same session |
| VERIFY_PIN | `40` | 4..8-byte PIN | `9000` and this session may SIGN; `63CX` wrong, X tries left; `6983` blocked; `6984` no PIN set |
| SET_PIN | `41` | 4..8-byte PIN | `9000` the first time; `6985` if a PIN is already set; does not open a session |
| CHANGE_PIN | `42` | old PIN then new PIN, 8..16 bytes | `9000`; needs a `VERIFY_PIN` first and ends the session |

`SELECT` answers `02009000`: the applet's version 2.0 in two data bytes, then the status word.

The applet is meant for contactless (T=1 style) use. GEN_KEY and GET_PUB call
`setIncomingAndReceive` on commands with no data, which may stall on T=0 contact transports;
step 7 must re-check this on the real card.

Status words: `9000` ok, `6700` wrong length, `6A94` PIN required, `6983` PIN blocked,
`6984` no PIN set, `6B00` bad P1/P2, `6985` state refused, `6D00`
unknown INS, `6E00` wrong CLA, `6F00` crypto failure.

## License

MIT
