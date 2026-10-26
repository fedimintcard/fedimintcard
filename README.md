# fedimint-card

An experiment in NFC cards for [Fedimint](https://github.com/fedimint/fedimint) ecash.

The card holds the spend keys of Fedimint notes and signs spends with them; the phone or
till does the rest. Because Fedimint notes are spent with a Schnorr signature from each
note's own key, this needs no change to the Fedimint protocol and works with existing
federations.

Status: spike in progress (steps 1-2, Java side done). The first spike is described in [docs/SPIKE-PLAN.md](docs/SPIKE-PLAN.md).

This is experimental software. Do not put real funds on a card.

## Layout

```
applet/   JavaCard applet (GEN_KEY, GET_PUB, SIGN) and jCardSim tests
bridge/   HTTP bridge that serves the simulated applet on 127.0.0.1
docs/     spike plan, CAP build notes
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

CLA `B0`. Applet AID `F0464D4341524401`. Four key slots (P1 = 0..3), P2 = 0.

| Command | INS | Data | Response |
|---|---|---|---|
| GEN_KEY | `10` | none | 32-byte x-only pubkey; `6985` if the slot is used |
| GET_PUB | `11` | none | 32-byte x-only pubkey; `6985` if empty |
| SIGN | `12` | 32-byte message | 64-byte BIP340 signature; `6985` if empty |

The applet is meant for contactless (T=1 style) use. GEN_KEY and GET_PUB call
`setIncomingAndReceive` on commands with no data, which may stall on T=0 contact transports;
step 7 must re-check this on the real card.

Status words: `9000` ok, `6700` wrong length, `6B00` bad P1/P2, `6985` slot state, `6D00`
unknown INS, `6E00` wrong CLA, `6F00` crypto failure.

## License

MIT
