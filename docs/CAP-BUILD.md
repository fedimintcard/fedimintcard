# Building and installing the CAP (spike step 7)

`applet/build.xml` builds the CAP with ant-javacard; `Opener.java` is the do-nothing applet the
J3R180 wants instantiated first. The recipe is from cashucard/javacard
(`docs/HARDWARE_DEPLOYMENT.md` and its CI).

## Build

```sh
git clone --filter=blob:none --sparse https://github.com/martinpaljak/oracle_javacard_sdks ~/.javacard/sdks
git -C ~/.javacard/sdks sparse-checkout set jc305u4_kit
git -C ~/.javacard/sdks checkout 6a75ec0d6913db236d354f154df7dbc9573d976d
brew install ant
cd applet && ant cap -Djc.sdk=$HOME/.javacard/sdks/jc305u4_kit     # -> applet/target/fedimint-card-0.1.cap
```

ant-javacard is downloaded into `applet/target/` and checked against a pinned SHA-256
(`14f5e25c...0589`, the same jar cashucard pins). The converter's off-card verifier runs as part
of the build. First build on 2026-10-26: load file 4,573 bytes (stored components, without the
1,028-byte Descriptor), 221 past a multiple of 256. With 8 slots and GEN_KEY overwrite (P2 = 1):
load file 4,643 bytes (without the Descriptor), 35 past a multiple of 256. That is a quarter of cashucard's size, so
the size window cashucard measured on the J3R180 may not apply; `stripcap.py` and
`refcheck.py` were not needed.

With the PIN commands of P1b the applet has grown: a build on 2026-10-27 gives a load file of
**5,513 bytes**, sha256 `4411d43e…79962`, 23 LOAD commands at 247 bytes. That is the figure
`tools/cardkit/make-assets.py` pins; the 4,643 above is the earlier build and is kept as the
record of what was measured then. Each PIN command adds a method and its branch code, and
`VERIFY_PIN` carries the attempt counter and the session flag.

## Pins

| Item | Value |
|---|---|
| JavaCard SDK | `jc305u4_kit` from `github.com/martinpaljak/oracle_javacard_sdks`, commit `6a75ec0d6913db236d354f154df7dbc9573d976d` |
| ant-javacard | `v26.05.15` (needs `ant`) |
| Build JDK | 17 |
| GlobalPlatformPro | `gp.jar` from `github.com/martinpaljak/GlobalPlatformPro` releases |

ant-javacard does not download SDKs. Check the kit out and pass `-Djc.sdk=/path/to/jc305u4_kit`.

## AIDs

| | AID |
|---|---|
| Package | `F0 46 4D 43 41 52 44` (7 bytes) |
| Opener (instantiated first) | `F0464D434152444D` |
| Applet (SELECT) | `F0464D4341524401` |

## Install (gp commands)

The J3R180 needed a do-nothing `Opener` applet instantiated first in cashucard's case
(file lists `Opener` first, then `gp --create` for the real applet). Same shape here:

```sh
gp --list                                                                 # what is on the card
gp --install applet/target/fedimint-card-0.1.cap --applet F0464D434152444D # load, instantiate the opener
gp --create F0464D4341524401 --package F0464D43415244 --applet F0464D4341524401
gp --list                                                                 # expect APP F0464D434152444D and F0464D4341524401
gp --apdu 00A4040008F0464D4341524401                                      # 02009000: version 2.0
gp --apdu B0100000                                                        # GEN_KEY slot 0: 32 bytes + 9000
gp --apdu B04100000431323334                                              # SET_PIN "1234" -> 9000
gp --apdu B04000000431323334                                              # VERIFY_PIN "1234" -> 9000, opens the session
gp --apdu B012000020<32-byte message in hex>                              # SIGN slot 0: 64 bytes + 9000
gp --uninstall applet/target/fedimint-card-0.1.cap                        # cleanup: the package and both instances
```

The three PIN lines are new in P1b. Hardware was authorised on 2026-10-28 and the ordered
checklist is in `PROVISIONER-PLAN.md` § 9 — **it has not been run**, so the only evidence for
these commands is still jCardSim (`applet/src/test/.../PinTest.java`). `SIGN` before
`VERIFY_PIN` is `6984` on a card with no PIN set and `6A94` once one is, so the order above
matters: the SCRIPT checks the PIN gate before the slot, and a `6985` where `6984` belongs
means that ordering broke. A `VERIFY_PIN` is not sticky across a
card reset or a SELECT of another AID. Three wrong PINs block the card for good (`6983`), so
check the PIN before spending tries on a card you care about.

Then the Rust tests over PC/SC, from `host/`:

```sh
cargo test --locked --workspace --features fedimint-card-host-card/pcsc -- --test-threads=1
```
with `CARD_BACKEND=pcsc` (and `CARD_PCSC_READER=ACR122U` if more than one reader is present).

Verified on an NXP J3R180 (JCOP4) through an ACS ACR122U on 2026-10-26: the install sequence
above works with the default GlobalPlatform keys; the applet signs (median 847 ms per BIP340
signature, GEN_KEY 125 ms, GET_PUB 27 ms); and all 18 Rust tests pass with `CARD_BACKEND=pcsc`.
A card personalised by the cashucard provisioner has non-default keys and must be reset in that
app first. Do not retry `gp` with the wrong keys: each failure counts toward the card lock.
Still unknown from outside: whether the nonce key is transient or the persistent fallback.

## Slot count on the J3R180 (2026-10-26)

A 16-slot build (32 EC key objects built in the constructor) loads but the INSTALL [for install and
make selectable] fails with `6F00` even with the Opener instantiated first; the constructor is too
heavy. 8 slots install. The ACR122U also drops the connection (`SCARD_E_NOT_TRANSACTED`) during a
default-block-size load about half the time and then needs a power cycle; `--bs 64` loads reliably.
Root cause and fix: writing the curve parameters into every slot's key objects in the constructor is
what exceeds the install budget; with the parameters written at GEN_KEY instead, 8 slots install
(load file 4,643 bytes). The 16-slot build was not retried after this fix.

## 32 slots on silicon (2026-11-05)

Applet 2.1 (`020c0d0`: 32 slots, keys built at each slot's first GEN_KEY, STATE_KEY) on a
blank NXP J3R180 (serial 09336120) through the ACR122U, `--bs 64`. CAP 34,824 bytes.

| | Persistent free | Transient COD | Transient COR |
|---|---|---|---|
| Blank card | 163,744 | 3,990 | 3,984 |
| Opener + applet installed | 155,816 | 2,980 | 3,984 |
| All 32 slots keyed | 131,880 | 2,980 | 3,984 |

Free memory is JCOP's `GET DATA DF25` on the ISD (`80CA00FE02DF2500`). The install, with the
AES-256 state secret built in the constructor, passed first time. Each slot's key pair costs about
748 bytes; 32 slots take 23,936.

| Command | Time on the ACR122U |
|---|---|
| GEN_KEY, first time for a slot (builds the key objects) | 299–337 ms, ~318 ms typical; 32 slots ≈ 10.2 s |
| GEN_KEY overwrite (P2=01) | 231 ms |
| GET_PUB | 27 ms |
| SIGN (slot 31) | 847 ms; the BIP-340 signature verifies off-card |
| STATE_KEY | 42 ms; the same 32 bytes across selections |

Slot 32 answers `6B00`. STATE_KEY answers `6984` before a PIN is set and `6A94` before this
selection's VERIFY_PIN, as SIGN does. GEN_KEY on a keyed slot without P2=01 answers `6985`.

### The install tap on phones (split keying, 2026-11-05)

The provisioner from `spike/32-slots` (cardkit `f8b80be`, `INSTALL_TAP_KEY_SLOTS = 8`) against the
staging registry, one card per batch, on the same J3R180:

| Phone | Card | Tap, card detected to last APDU | Result |
|---|---|---|---|
| Pixel 8 Pro | blank | 6.0 s (Android NFC log) | programmed, `…9FAC`, one tap; **applet 2.0, see below** |
| iPhone 13 Pro Max | the Pixel's card, applet swept first | 5.35 s, 75 APDUs (nfcd log) | programmed, `…C038`, one tap |

Both include the load (25 LOAD blocks), both installs, keys for slots 0–7, SET_PIN and the
post-install checks. Keying all 32 would add about 7.6 s (24 × 318 ms), which is why the other 24
are keyed by the till at the taps that need them (`4c19cff`).

Correction (2026-11-06): the Pixel's build carried a stale copy of cardkit (yarn copies `file:`
dependencies at install), so `…9FAC` got the 8-slot applet: SELECT answers `02 00`, slot 8 answers
`6B00`, and the registry has `slot_count: 8`. Its 6.0 s is the 2.0 load file (23 LOAD blocks), not
2.1. The Max row is a 2.1 install. `apps/provisioner/src/cardkitCopy.test.ts` now fails on a stale
copy.

## End to end on staging (2026-11-06)

A 32-slot card (`1e1b01ef`) set up by the Pixel (fresh cardkit) against the staging registry in the
Island Bitcoin Community federation; a laptop till (`till serve`, claimed on staging); the Android
POS paired to it over `adb reverse`.

**Load.** `card load 50000000` from the till's wallet through ops, card on the ACR122U: 12 notes
into slots 0–11, 7.2 s. The card arrived with slots 0–7 keyed; the load keyed 8–11 with the till's
token (split keying, `e7ac900`).

**Ten charges** at the POS, PIN each time. None refused; federation fees 0.

| Price (sat) | Inputs | Change notes | Overpay (msat) |
|---|---|---|---|
| 21 | 2 | 1 | 1,528 |
| 100 | 2 | 1 | 352 |
| 333 | 1 | 5 | 824 |
| 1,000 | 3 | 1 | 1,472 |
| 2,500 | 3 | 4 | 608 |
| 5,000 | 3 | 4 | 1,216 |
| 7,777 | 3 | 5 | 1,304 |
| 12,000 | 2 | 4 | 1,280 |
| 50 | 3 | 0 | 1,200 |
| 4,200 | 2 | 3 | 448 |

Change went back to slots up to 20; at the end the card held 16 notes. No charge reached the
3-input cap. Neither the till nor the POS logs the card's signing time; taps were 15–24 s apart
including typing the price and PIN, and SIGN alone is 847 ms (above).

**Drain.** Six charges chosen with the planner's own objective (zero change, ≤ 3 inputs, overpay
≤ `MAX_TILL_OVERPAY`) emptied the card exactly: 12,583, 2,361, 1,572, 262, 131 and 98 sat, every
one with no change. The till's wallet ended where it started (51,499 sat), so the whole run moved
50,000 sat onto the card and back with nothing lost. The registry then let the Setup app reset the
card; it refuses while any note is live, however small.

Two findings from the run, both fixed: the stale cardkit copy above, and a Setup app whose saved
federation was a placeholder (`de0fed…c0ffe`), which the till refuses to load (`card … belongs to
federation …`). The Setup app has no federation preset; check its settings before a batch.

## In production (2026-11-06)

Registry v9 (Worker version `185ded41`) on registry.fedimintcard.org, after a restore drill of that
night's backup; the StartOS till `0.1.0:1` (upstream `e7ac900`); Setup and POS apps from `e13773e`
on both phones. Card `a25e66d7` was set up on Live with the 32-slot applet, topped up by Lightning
through the zion till and charged.
