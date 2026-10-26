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
gp --apdu 00A4040008F0464D4341524401                                      # 9000
gp --apdu B0100000                                                        # GEN_KEY slot 0: 32 bytes + 9000
gp --uninstall applet/target/fedimint-card-0.1.cap                        # cleanup: the package and both instances
```

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
