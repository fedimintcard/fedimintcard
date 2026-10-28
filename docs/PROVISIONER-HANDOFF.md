# Handoff: cardkit (P1a) and applet PIN (P1b)

Written 2026-10-27. Repo `fedimintcard/fedimintcard`, local `main` at `2c78c07`, **whose
working tree has uncommitted work**. Read § 1 before you run anything.

## 1. State of the tree

Nothing below is committed. `main` at `2c78c07` does not contain any of it.

```
 M .github/workflows/ci.yml
 M NOTICE
 M README.md
 M applet/src/main/java/.../FedimintCardApplet.java        P1b
 M applet/src/main/java/.../Opener.java                    P1b
 M applet/src/test/java/.../{CardTestSupport,GenKeyGetPubTest,NoPrivateKeyLeakTest,
                             ProtocolErrorsTest,SignTest}.java   P1b
 M bridge/src/test/java/.../BridgeHttpTest.java            P1b
 M docs/CAP-BUILD.md                                       P1b + today's size
 M docs/DESIGN-NOTE.md                                     P1b
?? applet/src/test/java/.../{NoAllocationTest,PinTest}.java  P1b
?? docs/PROVISIONER-PLAN.md                                  the 569-line plan
?? tools/                                                    P1a, all of cardkit
```

**There is no push token in this session**, so none of it is on GitHub. The first thing
to do is commit it in two commits — P1b (applet, bridge, DESIGN-NOTE, CAP-BUILD's PIN
section) and P1a (`tools/`, plan, CI, README, NOTICE) — or the work exists on one disk.

## 2. What is done and verified

Every number here was produced by running the command in this session.

| Thing | Evidence |
|---|---|
| P1b applet PIN | `mvn -B -ntp -pl applet,bridge test` → **85 tests, 0 failures, 0 errors** (79 applet + 6 bridge). Covers `SIGN` refusing with `6A94` before `VERIFY_PIN` and `6984` when no PIN is set, three wrong PINs → `6983`, and no private-key leak through the PIN commands. |
| P1a engine | `cd tools/cardkit && yarn typecheck && yarn test` → **62 tests, 0 failures**, 12 suites: DES (12), SCP02 (24), provision (26). |
| Port fidelity | `gp/bytes.ts`, `gp/des.ts`, `gp/test/node-des.ts` byte-identical to cashucard `main`; `gp/scp02.ts` differs by 1 comment line; `gp/test/des.test.ts` by 1 import line. Recorded in `tools/cardkit/NOTICE`. |
| The tests bite | Mutating the fake card to answer every slot as keyed and dropping `CARD_AID` from the whitelist → 22 failures; restored → 0. Mutating the C-MAC ICV to skip its single-DES pass → 1 failure; widening the DELETE whitelist → 1 failure; restored → 0. |
| Real CAP bytes | `ant cap` → `applet/target/fedimint-card-0.1.cap`, converter verifier passes. Load file **5,513 bytes**, sha256 `4411d43e…79962`, 23 LOAD commands. `make-assets.py --check` → exit 0. |
| Suite on real bytes | With the fixture committed, the install suite runs the real CAP and prints no placeholder warning (still 62/62). |
| CI | `cardkit` job added: `yarn install --frozen-lockfile`, `yarn typecheck`, `yarn test`. Verified locally with those exact commands; the YAML has three jobs and no duplicate keys. |

`docs/CAP-BUILD.md` said 4,643 bytes. That was the pre-PIN build; the PIN commands grew
the applet by 870 bytes. The doc now records both, and says which is current.

## 3. What is NOT verified

Be explicit about this with anyone downstream.

- **No card, phone or reader was touched.** P1b's "done when" line in the plan says *on a
  J3R180*. That has not happened. The PIN commands are proven under jCardSim only, and
  the install is proven against a software model of the card manager. Neither is
  evidence about silicon. Hardware authorisation is still not granted (plan § 9).
- **CI has not run on GitHub.** The job is written and its commands pass locally, but no
  run has happened.
- **Rust is unverified.** `cargo test --workspace` does not build on this machine
  (`aws-lc-sys-0.41.0` panics). Only CI can check `host/`.
- **`format:check` fails** on 6 files (prettier). It is a package script but not in CI.
  I left it out of CI because the repo's Rust job does not gate on `rustfmt` either; if
  you want a formatting gate, run `npx prettier --write .` in `tools/cardkit` and add the
  step.

## 4. Exact commands

```sh
# applet and bridge (needs JDK 17 and maven)
cd applet && ant cap -Djc.sdk=$HOME/.javacard/sdks/jc305u4_kit    # -> target/fedimint-card-0.1.cap
cd .. && mvn -B -ntp -pl applet,bridge test                        # 85 tests (79 applet + 6 bridge)

# cardkit
cd tools/cardkit && yarn install --frozen-lockfile
yarn typecheck
yarn test                                                          # 62 tests
cd ../.. && python3 tools/cardkit/make-assets.py --check           # exit 0; needs the CAP
```

To regenerate the load file after any applet change: build the CAP, run
`python3 tools/cardkit/make-assets.py` (it reports the new size and sha), then move the two
pins at the top of that script and commit them with the asset.

## 5. Decisions a reader must not silently reverse

- **No `guard` on `fedimintPlan`.** The applet has no GET_INFO, so there is nothing to ask
  a card whether it holds a key. `provision()` starts at `selectIsd` and sweeps whatever
  the package name matches. The protection against wiping a used card is the sidecar's
  precheck plus the operator's Replace switch — **host-side, not card-side**. If the app
  ever skips either, a used card dies silently.
- **`provision` always sweeps, installs for load, loads and verifies.** Only `probe` (needs
  a guard) and the listed `installs` can be skipped. A verify-only plan is not
  expressible, which is why the dirty-slot test asserts the sweep-then-verify order rather
  than a standalone verify.
- **AIDs are frozen** from `applet/build.xml`: package `F0464D43415244`, opener
  `…4D`, applet `…01`. The opener is instantiated first because this chip refuses the
  applet as its package's first instance.
- **PIN bytes follow Cashu**: `0x40 VERIFY_PIN`, `0x41 SET_PIN`, `0x42 CHANGE_PIN`; `6983`
  blocked, `6984` not set, `6A94` pin needed. `VERIFY_PIN` is not sticky across a reset.
- **Only the engine was ported.** Upstream's `identity`, `inspect`, `registry`, `reset`,
  `configuration`, `holderCommands` and the tag/hello applets are absent by design.

## 6. Bugs found and fixed this session

Worth reading if you touch the same code, because the failure modes repeat.

1. **`make-assets.py` wrote the fixture to `<repo>/gp/test/fixtures/`**, not
   `tools/cardkit/gp/test/fixtures/`. The `ASSET` constant lacked the `tools/cardkit/`
   prefix while `ROOT` was the repo root, so the write succeeded and no test ever saw it.
   Fixed by making `ASSET` absolute against the script's own directory; the stray
   directory was deleted.
2. **BER length in a load file is the component count, not the file size.** Four bytes of
   `C4 82 LL LL` sit in front of the count. Declaring the file size makes the card answer
   `6A80` to the first LOAD.
3. **MACed APDUs shift every offset by 5.** In `INSTALL [selectable]`, byte 13 is the
   module length and bytes 14–21 are the module AID. Hand-counting this gave wrong offsets
   four separate times. The test now derives the module from the APDU structure
   (`installModule()`), never from a literal.
4. **`tsx` can serve a stale transform.** An edit can appear not to have taken effect.
   Verify in a fresh `node --import tsx -e` process before concluding a fix failed.

## 7. Next moves, in order

1. Commit P1b and P1a (§ 1). Nothing else is safe until this happens.
2. Decide **app identity** — bundle id, Play application id, display name (plan § 8.6).
   It is baked into the first TestFlight build.
3. Get hardware authorisation (plan § 9), then run P1b's silicon checklist on a J3R180:
   `SIGN` before `VERIFY_PIN` → `6A94`; after → signature verifies under the slot key;
   three wrong PINs → `6983`; `SET_PIN`/`VERIFY_PIN` over the APDU bridge.
4. P0 (sidecar endpoints) and P1c (hosted registry) can run in parallel with different
   hands; P3 needs the phone and the cards.

`tools/cardkit/README.md` explains the engine; `docs/PROVISIONER-PLAN.md` is authoritative
for scope and the phase table.
