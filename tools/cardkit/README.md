# @fedimintcard/cardkit

GlobalPlatform over an injected transport: SCP02, the card-manager commands, and
an install that recovers from a pull at any APDU. This is the engine the phone
provisioner runs to put the Fedimint card applet onto a blank J3R180.

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
python3 tools/cardkit/make-assets.py                                # writes gp/test/fixtures/load-file.ts
python3 tools/cardkit/make-assets.py --check                        # fails on any drift
```

`make-assets.py` pins the load file's size and sha256, not the CAP's: the
converter stamps the CAP manifest with the build time, so the CAP hash moves on
every correct rebuild and a pin on it would fail. The pins are filled in from
the build of 2026-10-27: 5,513 bytes, 23 blocks of 247. Run `--check` after any
applet change; `--check` refuses while a pin is `None` rather than passing on a
guess. CI does not run it, because a hosted runner has no JavaCard SDK to build
the CAP from.

Without a built CAP the install suites run on a placeholder load file. It is not
a CAP, it prints a warning, and it proves nothing about the real bytes — the
descriptions that read bytes off the card (the fake card, SCP02, DES) are
unaffected. With the fixture committed, the install suites use the real bytes;
the placeholder is only for a machine with neither the CAP nor the fixture.

Without a built CAP the install suites run on a placeholder load file. It is not
a CAP, it prints a warning, and it proves nothing about the real bytes — the
descriptions that read bytes off the card (the fake card, SCP02, DES) are
unaffected.

## Tests

```sh
yarn install --frozen-lockfile
yarn typecheck
yarn test
```

The suite runs the engine against `gp/test/fake-card.ts`, a J3R180 card manager
in software whose card side is computed with `node:crypto`, sharing no code with
the host side under test. It models what the chip is known or suspected to do
when things go wrong: the opener rule (the first heavy INSTALL goes mute or
answers `6F00` and installs nothing), a LOAD torn by the card leaving the field
(discarded, or kept as a partial package in both the variants observed), a
package with instances refusing DELETE, and EXTERNAL AUTHENTICATE failures
locking the card manager. The provision suite pulls the card at every APDU,
before and after the card acts, and requires the next run to recover.

The fake card dispatches on the **module** an instance was made from, not on INS,
because SELECT is global: the opener is a real instance of the same package and
must not answer as the card applet would. SELECT checks a whole AID before any
prefix match for the same reason — every AID of this package begins with the
package's own bytes.

These are tests against a model of the card. They are not evidence that a real
card behaves this way; only a card in a reader is.

## Licence

MIT. The engine is ported from cashucard's `tools/cardkit`; see `NOTICE`.
