# @fedimintcard/cardkit

GlobalPlatform over an injected transport: SCP02 and SCP03 behind one way in,
the card-manager commands, and an install that recovers from a pull at any APDU.
This is the engine the phone provisioner runs to put the Fedimint card applet
onto a blank J3R180 or a J3R452; on a J3R452 the install's rotation is its first
contact, which moves the card manager from FF to the version a J3R180 starts at
(below).

The core imports no NFC, no HTTP and no platform module. Callers pass a
`Transceiver` — one APDU in, data and status bytes out — so the phone and the
Node test suite run the same code.

```ts
import {fedimintPlan, provision, type Transceiver} from '@fedimintcard/cardkit';

const result = await provision(tx as Transceiver, fedimintPlan(loadFile), {
  random: n => crypto.getRandomValues(new Uint8Array(n)),
  now: () => Date.now(),
});
```

`provision` never throws for anything the card does; it returns a result whose
`kind` says how the run ended (`done`, `pulled`, `refused`, `wrongKeys`,
`guarded`) and whose `blocks` says how far a LOAD got. That result is what the
screen shows and what the next tap acts on.

## The card

The AIDs come from `applet/build.xml` and nowhere else:

| Object        | AID                | Bytes                     |
| ------------- | ------------------ | ------------------------- |
| Package       | `F0` then `FMCARD` | `F0 46 4D 43 41 52 44`    |
| Opener applet | instantiated first | `F0 46 4D 43 41 52 44 4D` |
| Card applet   | what hosts select  | `F0 46 4D 43 41 52 44 01` |

The J3R180 will not take the card's applet as its package's first instance, so
the plan installs the opener first and the applet second. The sweep deletes them
in the reverse order — applet, opener, package — because a package with
instances refuses DELETE (`6985`).

`fedimintPlan` sets **no `guard`**: the applet has no GET_INFO command, so there
is nothing on the card to ask whether it holds a key or a PIN. A run therefore
starts at `selectIsd`, not at `probe`, and sweeps whatever the package's name
matches. The gate against destroying a used card is the host's — the sidecar's
precheck and the operator's Replace switch — not the card's. `notEmpty` is kept
for the day an applet grows a GET_INFO; nothing calls it today.

The plan's checks expect `6985` from `GET_PUB` on every slot: an applet that has
just been installed holds no key. `Check.sw` exists so an expected refusal can be
a check's success.

## Building the load file

The load file is what gp sends after `INSTALL [for load]` — the CAP's components
in load order, without the Descriptor. It is generated from a real CAP, never
hand-written:

```sh
(cd applet && ant cap -Djc.sdk=$HOME/.javacard/sdks/jc305u4_kit)   # docs/CAP-BUILD.md
python3 tools/cardkit/make-assets.py                                # writes gp/load-file.ts
python3 tools/cardkit/make-assets.py --check                        # fails on any drift
```

`make-assets.py` pins the load file's size and sha256, not the CAP's: the
converter stamps the CAP manifest with the build time, so the CAP hash moves on
every correct rebuild and a pin on it would fail. The pins are the build's of
2026-11-07 (d43c40b): 9,890 bytes, 41 blocks of 247. Run `--check` after any
applet change; `--check` refuses while a pin is `None` rather than passing on a
guess. CI does not run it, because a hosted runner has no JavaCard SDK to build
the CAP from.

Without a built CAP the install suites run on a placeholder load file. It is not
a CAP, it prints a warning, and it proves nothing about the real bytes — the
descriptions that read bytes off the card (the fake card, SCP02, DES) are
unaffected. With the fixture committed, the install suites use the real bytes;
the placeholder is only for a machine with neither the CAP nor the fixture.

## SCP03, and the J3R452's first contact

INITIALIZE UPDATE's reply names the protocol in byte 11, so `openSession` (and
`provision`, `rotateKeys`, `confirmManagerKeys` and `factoryReset`, which all
go through the same hub in `gp/scp02.ts`) opens SCP02 on a J3R180 or SCP03 on a
J3R452 without being told which. SCP03 runs at level 01 (C-MAC only) with
AES-128 keys, and takes only a pseudo-random card challenge (i=10), which
proves the ENC key and the security domain before anything is sent after
INITIALIZE UPDATE. The SCP03 engine (`gp/apdu.ts`, `gp/scp03.ts`) is
cashucard's (its D181), and AES is `@noble/ciphers` 1.3.0.

A J3R452 ships with its card manager at key set version FF. `rotateKeys` makes
its first contact (cashucard's D182): on an SCP03 card whose INITIALIZE UPDATE
answers FF it sends PUT KEY with P1 `00` and new version 01, and the card drops
the FF set. The reopen reads the key information with GET DATA `E0`, which
needs no key, and requires keys 01 to 03 at version 01, AES-128, and nothing at
FF; then it proves the new set at 01, and the result says `firstContact`. From
there the card is at the version a J3R180 starts at, and the install, the
rotation and the reset treat both chips alike. The Setup app's install tap
(`apps/provisioner/src/installTap.ts`) rotates with `rotateKeys` already, so on
a J3R452 its rotation is first contact with no change to the app.

A run that finds the new set already at 01 on an SCP03 card sends no PUT KEY,
but after its login it selects the card manager again and makes the same read,
stopping `guarded` on the same answers. Such a run is the rerun after a pull
between the PUT KEY and the read, or the retap of a card whose reopen the read
stopped, which the Setup app sends straight to the rotation because its applet
is installed with no keys: without the read either run would report the card
on the new set, and the tap would go on to key it while a kept FF set still
opened its card manager. A rotation in place at 01 reads no key information.

`rotateKeys` now stops `guarded` before EXTERNAL AUTHENTICATE in four cases
where it used to go on: the new set already proved at FF, a caller's `putKey`
at first contact, FF under SCP02 (never measured), and P1 `00` on any card not
at first contact (a `putKey: {p1: 0}` at 01, or the default on a card at
version 00). At version 01 it sends a J3R180 the same bytes as before
(`gp/test/scp02-pins.test.ts`).
`factoryReset` refuses every version but 01 before EXTERNAL AUTHENTICATE, as
before, and names FF in its own refusal: only the install tap's first contact
takes a card off FF.

First contact, in-place PUT KEY under SCP03 and the reset of a J3R452 have run
against the fake card and the four captures in `gp/test/fixtures` only, never
on a card in a reader.

Migration from 0.1.0:

- `putKeys(keys, sessionDek, options)` is now `putKeys(keys, session, options)`:
  pass the open session where `session.keys.dek` went. Its protocol picks the
  key type and check value (`80` and the DES3 KCV, or `88` and the AES KCV).
- `openSession` and `externalAuthenticate` return `SecureSession`, which is
  `Session` (SCP02) or `Scp03Session` (SCP03). Both wrap and send alike; narrow
  on `session.scp` (2 or 3) where code names `Session`.
- `Handshake` carries `sdAid`, the security domain INITIALIZE UPDATE went to,
  so a handshake built by hand needs it. `initializeUpdate` takes it as its
  fourth argument and `openSession` as its `sdAid` option, the ISD by default.
  `provision`, `rotateKeys` and `factoryReset` select the ISD and nothing else.
- New: `FACTORY_KVN` (01), `SHIPPED_KVN` (FF), `getKeyInformation()` (GET DATA
  `00E0`) and `parseKeyInformation`, `RotateResult.firstContact`, the SCP03
  primitives (`aesEcb`, `cmac`, `scp03Kdf`, `aesKcv`, `Scp03Session` and the
  rest of `gp/scp03.ts`), and a runtime dependency on `@noble/ciphers` 1.3.0.
  `FACTORY_KEY_VERSION` is `FACTORY_KVN`.

### Drift from cashucard

The shared files are aligned with cashucard's `tools/cardkit` at 0229dc8 (see
`NOTICE` for which are verbatim). What this copy keeps of its own:

- `provision`'s multi-set key candidates (`keys: [own, factory]`, one
  INITIALIZE UPDATE checked against each) and the exported `Guard`, `Run` and
  `expectOk`, which `gp/reset.ts` builds on;
- `factoryReset` (`gp/reset.ts`), this card's reset, in place of cashucard's
  owned-card reset, with its 01-only guard: cashucard's reset restores the
  public default in place at whatever version it observes and refuses only 00
  and FF (D182), where this one refuses every version but 01, because nothing
  moves a Fedimint card off 01 and the registry records versions 1 to 127 only;
- `gp/identity.ts` and `gp/load-file.ts`, and the fake card in `gp/test` (with
  the Fedimint applet behind it) rather than cashucard's `testing/` entry point;
- no `sdAid` run option: the hub takes the security domain, and every run here
  selects the ISD;
- the key information read in a `rotateKeys` run that finds the new set
  already at 01 under SCP03, until cashucard's copy takes the same change:
  0229dc8 reads it only in first contact's reopen, so after a pull past the
  PUT KEY (the residual D182 names) or on a retap it reports a card that kept
  its FF set on the new set.

Not ported, because this card does not need them: the registry, inspect,
reset-preflight, settled-cleanup, configuration and holderCommands modules,
the `/cleanup` entry point, the hello applet, and the install's handling of the
seller's NDEF tag (`readCashuRegistry`, `replaceSellerTag`, `readForeignTag`,
`sellerTagRemoved`). That tag sits at `D2760000850101`, the AID Cashu's own tag
takes; `gp/aids.ts` names only the ISD and this card's three AIDs, which all
begin `F0464D43415244`, so the sweep never meets the seller's tag.

## Tests

```sh
yarn install --frozen-lockfile
yarn typecheck
yarn test
```

The suite runs the engine against `gp/test/fake-card.ts`, a J3R180 card manager
in software whose card side is computed with `node:crypto`, sharing no code with
the host side under test; with `scp: 3` it is a J3R452's (SCP03, version FF as
it ships with `kvn: 0xff`, the seller's applets with `sellerApplets`). It models
what the chip is known or suspected to do when things go wrong: the opener rule
(the first heavy INSTALL goes mute or answers `6F00` and installs nothing), a
LOAD torn by the card leaving the field (discarded, or kept as a partial package
in both the variants observed), a package with instances refusing DELETE, and
EXTERNAL AUTHENTICATE failures locking the card manager. The provision suite pulls the card at every APDU,
before and after the card acts, and requires the next run to recover, under
both protocols; so do the rotation (first contact included) and the reset.

Two kinds of test hold the fake to something outside it. `scp02-pins.test.ts`
pins sha256 of every APDU a J3R180 receives in an install, a rotation and a
reset, as at 4301b0a, before SCP03 was added. `transcripts.test.ts` replays the
owner's four `gp` captures of the J3R452 under SCP03 (`gp/test/fixtures`): every
session key, cryptogram and C-MAC recomputed and compared to the wire. SCP03's
AES and CMAC are also held to FIPS 197 and NIST SP 800-38B in `scp03.test.ts`.

The fake card dispatches on the **module** an instance was made from, not on INS,
because SELECT is global: the opener is a real instance of the same package and
must not answer as the card applet would. SELECT checks a whole AID before any
prefix match for the same reason — every AID of this package begins with the
package's own bytes.

These are tests against a model of the card. They are not evidence that a real
card behaves this way; only a card in a reader is.

## Licence

MIT. The engine is ported from cashucard's `tools/cardkit`; see `NOTICE`.
