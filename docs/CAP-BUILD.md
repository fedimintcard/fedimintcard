# Building and installing the CAP (stub for spike step 7)

Not done yet: there is no `applet/build.xml` and no `Opener` applet. This page records the recipe
so step 7 can start from it. Everything here is taken from cashucard/javacard
(`docs/HARDWARE_DEPLOYMENT.md` and its CI) and is unverified for this repo.

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
| Applet (SELECT) | `F0464D4341524401` |

## Install (gp commands)

The J3R180 needed a do-nothing `Opener` applet instantiated first in cashucard's case
(file lists `Opener` first, then `gp --create` for the real applet). Same shape here:

```sh
gp --install target/fedimint-card-applet.cap --applet <OPENER_AID>
gp --create F0464D4341524401 --package F0464D43415244 --applet F0464D4341524401   # fix the package AID to the one in build.xml
gp --list
gp --apdu 00A4040008F0464D4341524401
gp --apdu B0100000        # GEN_KEY slot 0
gp --delete F0464D4341524401   # cleanup, then the opener and the package
```

Open items for step 7: write `build.xml` and `Opener.java`, pick the package AID, confirm the
`ALG_EC_SVDP_DH_PLAIN_XY` output framing on silicon (SchnorrHW fails closed with `6F00` if it
is not 65 bytes `04||X||Y`).
