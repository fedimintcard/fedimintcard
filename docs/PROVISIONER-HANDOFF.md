# Handoff: cardkit (P1a) and applet PIN (P1b)

Written 2026-10-27; updated 2026-10-28 after the work was committed and merged. Repo
`fedimintcard/fedimintcard`, `main` at `3b274a3` — **merged** (PR #14), CI green,
tree clean.

## 1. State of the tree

Committed 2026-10-27 and 2026-10-28 on `feat/cardkit-pin`, then merged to `main` as
`3b274a3` on 2026-10-28. The CI-fix commit matters most: it is what makes any of the
rest checkable.

| Commit | Contents |
|---|---|
| `0923ba0` | P1b — applet PIN commands, `Opener`, applet tests, bridge test, DESIGN-NOTE, CAP-BUILD PIN section |
| `aa9823c` | P1a — `tools/cardkit`, `NOTICE`, README, CI job, `PROVISIONER-PLAN.md`, this handoff, `.gitignore` |
| `bd25581` | CI fix — see § 6.5; the workflow had never parsed |
| `7209966`, `a1acf88` | `setup-java@v6`/`setup-node@v7`, Prettier and a `format:check` gate |
| `2771a61`, `897c4ad` | This document corrected to what was actually verified |

`main` is at `ed9487b` as of 2026-10-28 (confirmed by
`gh api repos/fedimintcard/fedimintcard/commits/main`). It carries, on top of the merge:

| Commit | Contents |
|---|---|
| `c68db6a` | cardkit becomes an installable package: `main`/`types`/`exports`, `reactNative: index.ts`, `tsconfig.build.json`, CI's `yarn build`, and the load file moved out of `gp/test/fixtures/` into `gp/` |
| `ed9487b` | `apps/provisioner/` — the P2 app (five screens, settings, store, PIN handling, demo fake card, `RegistryStoreApi`) and the bug fixes in § 6.7–6.16 |

**Pushing needs the `islandbitcoin` identity.** `bobodread876` is an org member with
`pull` only on this repo, so `git push` fails 403 however the URL is formed; the
`islandbitcoin` token is org admin with `push` and `admin`. `gh auth switch --user
islandbitcoin` (or `GH_TOKEN=$(gh auth token --user islandbitcoin) git push ...`) is the
way in. The commits stay authored as `Dread`.

`node_modules/` was untracked and unignored — `git add tools/` would have committed 578
dependency files. `.gitignore` now covers it.

## 2. What is done and verified

Every number here was produced by running the command in this session.

| Thing | Evidence |
|---|---|
| P1b applet PIN | `mvn -B -ntp -pl applet,bridge test` → **85 tests, 0 failures, 0 errors** (79 applet + 6 bridge). Covers `SIGN` refusing with `6A94` before `VERIFY_PIN` and `6984` when no PIN is set, three wrong PINs → `6983`, and no private-key leak through the PIN commands. |
| P1a engine | `cd tools/cardkit && yarn typecheck && yarn test` → **62 tests, 0 failures**, 12 suites: DES (12), SCP02 (24), provision (26). |
| Port fidelity | `gp/bytes.ts`, `gp/des.ts`, `gp/test/node-des.ts` byte-identical to cashucard `main`; `gp/scp02.ts` differs by 1 comment line; `gp/test/des.test.ts` by 1 import line. Recorded in `tools/cardkit/NOTICE`. |
| The tests bite | Mutating the fake card to answer every slot as keyed and dropping `CARD_AID` from the whitelist → 22 failures; restored → 0. Mutating the C-MAC ICV to skip its single-DES pass → 1 failure; widening the DELETE whitelist → 1 failure; restored → 0. Re-confirmed 2026-10-28: dropping `CARD_AID` from `sweep` → **8 failures**, restored → 62/62. |
| Real CAP bytes | `ant cap` → `applet/target/fedimint-card-0.1.cap`, converter verifier passes. Load file **5,513 bytes**, sha256 `4411d43e…79962`, 23 LOAD commands. `make-assets.py --check` → exit 0. |
| Suite on real bytes | With the fixture committed, the install suite runs the real CAP and prints no placeholder warning (still 62/62). |
| CI | Runs on GitHub, **green in full** on `a1acf88`: `cardkit` (install, typecheck, `format:check`, test), `java (11)`, `java (17)`, `rust` (20 tests). See § 1 and § 6.5 — before the parse fix no job had ever been queued. |

| P2 app (UI) | `cd apps/provisioner && npx tsc --noEmit` → clean (also clean under `--noUnusedLocals --noUnusedParameters`). `npx jest` → **57 tests, 0 failures, 4 suites** (`api`, `api.registry`, `session`, `demo`). `npx eslint .` → 0 errors, 24 warnings (bitwise/number formatting, inline styles). |
| P2 renders | Mounted the real `<App />` under `react-test-renderer`: it renders SignIn and prints `4411d43e · 5513 bytes` — the load sha traced from `@fedimintcard/cardkit` through `settings.tsx` into the screen. The tree mounts with the registry adapter in it. |
| P2 card semantics | The demo suite drives a fake card through the real cardkit engine. Verified there: sweep deletes applet then opener then package; reinstall over `hasOld`/`oldVersion` reaches `done`; `foreign` is refused at `installForLoad` (`6985`) with **no LOAD sent**; `wrongKeys` stops at `auth` with no `EXTERNAL AUTHENTICATE`; SCP02 reply is 28+SW with KVN/SCP at 10/11. |
| P2 registry adapter | `api.registry.test.ts` drives `RegistryStoreApi` against a scripted fetcher: bearer token on every call, `batch_id` from the `{batch:{...}}` envelope threaded into each row, notes summed (1048+524 → 1572), real load sha sent; a call before sign-in is refused rather than posted. |

| P0 sidecar endpoints | `host/sidecar/tests/cards.rs` → **7 tests, 0 failures** (blank/installed/foreign precheck, keySetup idempotent, keySetup refusing a foreign card, issue replay, issue refusing a holding card, body/PIN validation). Full workspace: `cargo test --locked --workspace -- --test-threads=1` → **91 passed, 0 failed** (was 84; +7). Everything runs against `SoftCard`, not silicon. |

`docs/CAP-BUILD.md` said 4,643 bytes. That was the pre-PIN build; the PIN commands grew
the applet by 870 bytes. The doc now records both, and says which is current.

## 3. What is NOT verified

Be explicit about this with anyone downstream.

- **A J3R180 was read on 2026-10-28, but nothing was written to it.** The card in the
  reader carries the **keys-only applet**: `SELECT F0464D4341524401` → `9000`,
  `SIGN` slot 0 → `9000` with 64 bytes and no PIN given, `VERIFY_PIN` under the applet's
  CLA (`B0`) → `6D00` (INS unsupported). All 8 slots hold keys that match no wallet on
  this machine (`0E92BC8B…`, `C338D4D6…`, …), so it is the **foreign** case: useful for
  proving P0's refusals, unusable for proving its writes. The PIN build is in the tree —
  a rebuilt CAP is byte-identical to the committed load file in all nine loaded
  components (Header, Directory, Applet, Import, ConstantPool, Class, Method,
  StaticField, RefLocation) — so the card is simply older than the applet. Loading the
  PIN CAP means plan § 9, not a casual reload.
- **P1b's PIN commands are still proven under jCardSim only.** No `SET_PIN` or
  `VERIFY_PIN` has ever reached silicon; the card that has the PIN applet has not been
  loaded. Plan § 9's ordered checklist is not run.
- **CI passed in full on 2026-10-28** — `cardkit`, `java (11)`, `java (17)` and `rust` on
  commit `a1acf88` ([run 40618880405](https://github.com/fedimintcard/fedimintcard/actions/runs/40618880405)). Before that, no run had ever executed: the
  workflow did not parse (§ 6.5), so all 14 earlier runs were 0-second failures with no
  jobs. A cancelled `rust` in between was the concurrency group doing its job, not a
  failure — read the step conclusion before reading a run's conclusion.
- **Rust builds locally as of 2026-10-28**, once `cmake` and `pkg-config` are installed
  (`brew install cmake pkg-config`); without them `aws-lc-sys-0.41.0` panics with "Required
  build dependency is missing", which is a missing prerequisite, not a code fault. With
  them, `cargo test --locked --workspace -- --test-threads=1` runs **92 tests** here, all
  passing (was 84 before P0: 2 issue + 7 spend + 2 card unit + 12 roundtrip + 19 sidecar
  unit + 22 wallet + 20 till; P0 added 7 in `sidecar/tests/cards.rs`, and the blank-card
  work added 1 more in `sidecar/tests/cards.rs`). CI's `rust` job is still the gate for a
  clean checkout.
- **The iOS project had never built, and nothing said so in code.** `CODE_SIGN_STYLE` was
  unset (→ wildcard profile, which cannot carry NFC Tag Reading: the build failed on the
  entitlement) and the bundle phase ran `scripts/bundle-ios.sh`, a file this repo has never
  had. Both are fixed in the project file; § 6 bug 22. Signed in to Xcode did **not** fix it —
  signing in gives identities, not an App ID with the capability.
- **The app has now run on two devices, to SignIn and no further.** On 2026-10-28 a Pixel 8
  Pro (Android, RN 0.77.1, Fabric) ran `org.fedimintcard.provisioner`: Metro served the
  bundle, the app mounted (`Running "FedimintProvisioner"`, JS log) and rendered SignIn with
  the real build stamp (`4411d43e · 5513 bytes`). The iPhone 13 Pro Max (iOS 26.5.2) runs it
  too — installed from `/tmp/fcdd/Build/Products/Debug-iphoneos/FedimintProvisioner.app`,
  1.0.0 (10), launched with `devicectl device process launch`, process alive and the SignIn
  screen confirmed on the device, and inside it: the SignIn fields accept text, and pressing
  **SIGN IN raises the designed refusal** rather than anything unexpected — `store.tsx`'s
  `signIn` throws `no registry client configured for this build` whenever no `api` is
  supplied, and `api.ts`'s `registryOrigin()` returns null for both endpoints, so the app
  never attempts a request. That path is exercised on silicon now, not only in Jest. The iPhone build embeds its own 4.4 MB Hermes
  `main.jsbundle`, so it runs without Metro; on a device the iOS app never connects to the
  packager, which is expected, not a fault. (No iOS screenshot was possible:
  `idevicescreenshot` fails with "Could not start screenshotr service: Invalid service" on
  26.5, and `devicectl` has no screenshot subcommand — the screen was confirmed by the
  operator, not captured here.) Everything past SignIn is unreachable on
  device: `signedIn` comes only from a hosted operator session, and `REGISTRY_ORIGINS` is
  empty (§ below), so the loop, batch setup, summary, Settings and the NFC path have still
  never run on hardware. The iOS device pass has not started. Fonts, safe areas, the iOS
  sheet, Android's 15 s `setTimeout` clamp and `transceive` remain unexercised.
- **The provisioner would not build until `react-native-svg` was pinned to 15.11.1.**
  15.15.5 (what `package.json` carried) uses `yoga::StyleSizeLength`, which RN 0.77.1 does
  not have, so `app:buildCMakeDebug` failed in `RNSVGLayoutableShadowNode.cpp` and no APK
  was produced. 15.11.1 is the last release that compiles against 0.77's Yoga; the fix
  belongs with RN 0.78, not with the svg pin.
- **The registry has no origin.** `REGISTRY_ORIGINS` is `{test: '', production: ''}` on
  purpose — P1c builds the registry and this repo names no host. The app therefore cannot
  sign in today, and says so ("no registry client configured for this build") rather than
  posting credentials at a guessed server. Filling those two strings is the whole wiring.
- **The three P0 routes have crossed a reader, read-only — first through a laptop PC/SC
  relay, then through the phone's own NFC reader.** `till serve` with
  `CARD_BACKEND=bridge` pointed at a PC/SC relay on `:9876` (the contract the phone relay
  speaks: `{"apdu":"<hex>"}` → `{"response":"<hex data||SW>"}`) answered against the
  J3R180 above: `precheck` → 200 `kind:"foreign"`, `foreign_slots:[0..7]`,
  `keyed_slots:[0..7]`, `free_slots:[]`, `applet_present:true`, `isd_open:false`,
  `live_msat:0`, `safe_to_replace:false`, nothing written; `keySetup` → 502 `card_error`
  naming slot 0 (`0e92bc8bacb14f66`); `issue` → 422 `no_exact_plan` (was 500 `internal`
  before the fix in § 6). **No write has crossed a reader**: no `SET_PIN`, no `GEN_KEY`,
  no `topup` on silicon. `keySetup` idempotency and the issue replay path remain proved
  against `SoftCard` only.
- **The phone relay path works end to end.** `android-relay` installed on the Pixel
  (package `org.fedimintcard.relay`; the previous install was signed with another key and
  had to be uninstalled first), `adb forward tcp:9876` + `adb reverse tcp:9877`, app in the
  foreground with the card on the antenna: `/health` → `{"ok":true}`, and `SELECT` of the
  applet over NFC → `9000`. `precheck` through it reproduced the laptop reader's answer
  byte for byte on the same card.
- **A blank J3R180 was found and correctly described.** ISD `9000`, package and applet
  `6A82`, every `B0 xx` → `6E00`: nothing loaded. `precheck` now answers
  `kind:"blank"`, `applet_present:false`, `isd_open:true`, `safe_to_replace:true`,
  `slots:8`, 200 — after the fixes in § 6, and only after them: before, the route 502'd,
  because `open_relay_card` selected the applet up front and `precheck`'s slot walk read
  `6E00` as a transport failure.
- **Two NFC owners, and only one can be foreground.** `apps/provisioner` reads
  cards itself through `react-native-nfc-manager` (`src/session.ts`); the sidecar reaches a card
  only through `android-relay` on `:9876`, and that relay services APDUs **only while it is the
  foreground activity** (its README says so; on 2026-10-28 the provisioner being foregrounded
  made `/health` hang until the relay was raised). So the phases alternate owners: install /
  sweep / load are the app's own session, then keySetup / setPin / issue are the sidecar's, over
  the relay, **after** the app's session closes (plan § 5's phase table says exactly this at
  `keySetup`). Nothing in the tree enforces the handoff yet — it is a UI responsibility that has
  never run end to end. The relay also cannot be driven while the app is in front, so any test
  that leaves the app foregrounded will see the relay time out rather than refuse.
- **A blank card cannot be keyed without a load, by design.** Against that card `keySetup`
  answers 502 `card_error: slot 0: card returned status word 6E00`. `card_setup` generates
  into slots the applet owns, and with no applet there are none. The install (cardkit
  SCP02 load) is the app's `install` phase, not a sidecar route — `apps/provisioner/src/api.ts`
  says so at the top. So the ordering that matters is precheck → install → keySetup →
  issue, and a blank card is the correct starting point, not a fault.

- ~~`format:check` fails on 6 files~~ — fixed 2026-10-28 and now **gated in CI**
  (`yarn format:check`). One file is deliberately exempt: `gp/test/des.test.ts` keeps
  upstream's import layout so it stays diffable against cashucard, and is listed in
  `.prettierignore` (the reason is also in `NOTICE`).

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

### P0 — the card endpoints (2026-10-28)

- **`issue` wraps `topup`, never reimplements it.** `topup(amounts)` became a thin caller of
  `topup_ref(amounts, Option<&str>)`; every body rule (min denom, `require_tiers`, free slots,
  balance ≥ sum + 2×fee, persist-before-submit) is unchanged. Reimplementing the body would
  have meant two copies of the money rules.
- **The ref is saved in the same `save()` as the pending notes**, before submit; on a build
  failure both are removed together. A replay therefore cannot see a record over notes that
  were never written, or notes with no record. The ref's `txid` is filled after
  `range.txid()`.
- **The replay record is `CardState.issues: Vec<IssueRec>`, not `PendingTx`.** `topup` never
  creates a `PendingTx` — only `sign_and_submit` (the till's spend path) does — so the till's
  ref mechanism was not reusable. `STATE_VERSION` stayed 1; the field is `serde(default)` and
  skipped when empty, so older state files load.
- **`precheck` writes nothing** and is safe on every tap. It is the app's only pre-write
  gate, because `replace` gates nothing in cardkit (`probe` checks it; `FEDIMINT_PLAN` has no
  `guard`). It walks slots with `get_pub`, classifies `6985`/`6A82`/PIN words, marks any slot
  whose key this state file never produced as **foreign**, probes `GET DATA FF21` and
  `SELECT`, and reads live notes from state — never from the federation.
- **`applet_present` is SELECT → `9000`**, not a PIN-word inference. `kind` is derived
  separately (`AppletState` from the PIN status words) because a card can be selectable while
  the PIN probes say nothing useful — that is exactly the blank-card case.
- **`keySetup` never passes `--fresh`.** Re-keying destroys live notes. A card holding keys
  this wallet did not produce is refused with `SpendFault::WrongCard` (→ `502 card_error`)
  instead. `generated` is the before/after diff, so a repeat reports `[]`. `SET_PIN` on a
  pinned card answers `6985` and is reported `pin: "set"`, never re-set — that is what makes
  a re-run safe. `pin_set` is true only when `VERIFY_PIN` accepted the PIN.
- **`issue` refuses a card holding *any* live or pending note, whatever slot.** The first
  version checked only the slots the new plan would use; its test caught the hole: a card with
  a note in slot 0 and seven free slots took a second batch in slots 1–6 and was worth both.
  Slots are recycled, so "free" says nothing about whether the card was already issued to.
  This is the guard the app cannot provide — the card looks blank to the applet, and a copied
  card looks exactly like an unused one.
- **PIN bounds are 4..8 bytes** (`FedimintCardApplet.java:63-64`) and are enforced in
  `pin_apdu` *before* the card sees the APDU, so a malformed PIN cannot spend a try.
  `GENERATED_PIN_LENGTH = 6` is only the generator's default.
- **`host/card` had no PIN or SELECT wrappers** when P0 started, despite P1b's done-line
  naming "host wrappers in `host/card`" — that line was written before the work and was
  wrong about the tree. `Card` was keys-only. P0 added defaulted methods (`apdu`,
  `select_applet`, `get_data_ff21`, `set_pin`, `verify_pin`) with an `apdu()` hook that
  `CardClient` overrides with `exchange` (so `61xx`/`6Cxx` chaining is reused) and
  `SoftCard` implements as a model of the applet's dispatch. Backends that cannot send raw
  APDUs get a loud error, not a silent `Ok`.
- **`SoftCard` now models the applet's PIN machine**: installed vs blank (`blank()` answers
  `6A82` to SELECT), `SET_PIN` once then `6985`, three tries, `9000` on match, `63 CX` with
  tries left, `6983` at zero, `6984` before any PIN, `6D00` for anything else. Before this it
  had no PIN model at all, so any PIN test would have passed vacuously.

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
5. **The CI workflow never parsed, so CI never ran.** `concurrency` was a flow mapping
   holding an expression:

   ```yaml
   concurrency: { group: ci-${{ github.ref }}, cancel-in-progress: true }
   ```

   `{ ... }` containing `${{ }}` is not valid YAML, and GitHub rejects the whole file. The
   symptom is easy to misread: the run appears instantly as `failure` in **0s with zero
   jobs and no logs**, which looks like an infrastructure problem rather than a syntax
   one. Every push since the file was added failed this way, `main` included. Fixed in
   `bd25581` by writing it as a block mapping; `yaml.safe_load` is the cheap check, and
   CI then ran the three jobs for the first time.
6. **Surefire reports outlive the tests that made them.** `target/surefire-reports` held
   `TmpProbeTest` and `ZzProbeTest` from deleted scratch files, so a run reported 81 applet
   tests where the tree has 79 — and the handoff carried "87 tests" as a result. A green
   count that does not match `grep -c @Test` means stale output; `mvn clean test` is the
   only count worth quoting. Real figure: **85 (79 applet + 6 bridge)**.

7. **`apps/provisioner` cardkit was a source tree, not a package.** No `main`, no
   `exports`, so Node had no entry and Jest could not load the TypeScript at all. Fixed by
   giving `tools/cardkit/package.json` a real shape (`main: dist/index.js`,
   `reactNative: index.ts`, an `exports` map, `prepack: yarn build`) plus
   `tsconfig.build.json`, and adding `yarn build` to CI's `cardkit` job. Metro consumes
   `index.ts`; Jest and Node consume `dist/`. A `react-native` export condition that
   points at source is the reason the build step is not optional.
8. **The load file lived under `gp/test/fixtures/`.** The shipped app would have imported
   from a test tree. Moved to `tools/cardkit/gp/load-file.ts`, exported from the package
   index and re-hashed by `make-assets.py --check`; a vendored second copy is exactly the
   stale asset the sha check exists to catch.
9. **The demo's fake card could not delete anything.** `DELETE` carries its AID as an
   `4F <len> <aid>` TLV (`gp/scp02.ts deleteAid`), and the fake matched the raw data, so
   every DELETE answered `6A88`, the sweep never ran, and a reinstall always stopped at
   `installForLoad`. It now parses the TLV.
10. **The fake tracked "instances" as one boolean.** Deleting the card applet left the
    opener reported as present (and vice versa), so the package could never be removed.
    Now per-AID `hasCard`/`hasOpener`.
11. **The fake's free-memory TLV declared the wrong length** (`FF 21 0A` for five bytes),
    so `parseFreeMemory` returned null and `freeBefore`/`freeAfter` were always undefined.
    Correct length is `06`.
12. **A `foreign` card was programmed end to end** (`ok: true`, 23 LOADs) because the fake
    answered `9000` to every INSTALL. `INSTALL [for load]` now refuses `6985` and
    `and-make-selectable` `6A80` for that state, and no LOAD block is ever sent.
13. **`apps/provisioner` had no way to sign in.** `StoreProvider` takes an `api`, `App.tsx`
    passed none, and `api.ts` had no registry origin despite a comment promising it did.
    Added `REGISTRY_ORIGINS`/`registryOrigin` (both empty on purpose until P1c) and
    `RegistryStoreApi`, which adapts `RegistryClient` to the store's `StoreApi` and holds
    the bearer token in memory only. With no origin the store refuses with "no registry
    client configured for this build", which `signInReason` shows verbatim.
14. **The per-call transport timeout could be raised by the operation's constant.**
    `call.timeoutMs ?? this.timeoutMs` let `SIGN_IN_TIMEOUT_MS = 30_000` override a
    caller's narrower budget; now `Math.min(...)`, so a 20 ms cap fires in 20 ms.
15. **A 2xx that says it failed was treated as success.** The sidecar answers `202` with
    `{ok:false, code:'undecided'}`; the transport resolved it as a successful issue with an
    empty txid, which would hand an operator a card with unrecorded value. Now
    `if (!res.ok || json?.ok === false)`.
16. **`setCardMessage` was exported and never called**, so iOS's NFC sheet never carried
    the session's line. Now called after `requestTechnology`.
17. **PIN APDUs were sent with ISO CLA `00`.** `FedimintCardApplet.process` checks CLA
    against `B0` *before* reading the INS, so it answers `6E00` (CLA not supported) — the
    same word a card with no PIN applet gives. On the J3R180 this hid the applet's
    identity: the ISO-CLA probe could not tell "keys-only build" from "PIN build, wrong
    CLA". Fixed: `pin_apdu` sends `CLA` (`B0`); `SoftCard` now refuses any CLA but `B0`
    for PIN INS and `00` for ISO, so the model cannot accept what the chip rejects.
    **How it was found:** the hardware reported `6E00`; the applet's source says why.
    `SELECT` (`00 A4`) and `GET DATA` (`00 CA`) stay ISO CLA.
18. **A full card reported `500 internal`.** `topup_ref` refused too few free slots with a
    bare `anyhow!` string, and `classify()` maps typed errors only, so a plan refusal
    reached the app as a server fault. Now `PlanError::NoFreeSlots { need, free }` →
    422 `no_exact_plan`. Caught by calling `issue` against the real (full) card; the
    `SoftCard` fixtures were never full, so no test had reached it.
19. **A blank card was a 502 instead of a described card.** Two separate faults, both found
    by presenting a blank J3R180 to the phone relay:
    - `open_relay_card` selected the applet before handing the card to any job, so `SELECT`
      → `6A82` aborted with "card relay at …: card returned status word 6A82" **before**
      `precheck` ran. Every route, including the one whose job is to describe a card with no
      applet, refused the only card a batch starts from. Now it probes `GET DATA FF21`,
      which any card answers: a status word means a card is there, a transport error means
      one is not, and the job decides the rest.
    - `precheck`'s slot walk took `6E00` for a transport failure. Now `6E00`/`6D00` mean
      "no dispatcher, so every slot is empty", and `select_applet` gates the walk.
    `SoftCard` modelled the chip too generously here: its trait methods bypassed `apdu()`,
    so a not-installed card answered `6985` per slot where the silicon answers `6E00`.
    They now call `require_installed()` first. `precheck_on_a_blank_card_says_blank_and_safe`
    had passed only because of that leniency.
20. **`key_setup` invented a foreign key on a blank card.** Its `before` probe treated any
    status word but `6985` as "a key is present", so `6E00` became slot-0 key `!6e00` and
    the card was refused as foreign (`slot 0 holds a key this wallet did not produce
    (!6e00)`). Now `6E00`/`6D00` mean an empty slot, matching `precheck`. `keySetup` still
    refuses a genuinely blank card: `card_setup` reads the slot as empty and calls `GEN_KEY`,
    which the card answers `6E00` — "slot 0: card returned status word 6E00". That refusal
    is correct; keys cannot be generated into an applet that is not loaded, and install
    (cardkit) comes first.
21. **`isd_open` could report a refusing ISD as open.** It called `get_data_ff21()` twice
    and accepted `6D00`, which a card gives when it does not implement the command — not
    when the ISD is open. Now one probe, `6A82`/`6A88` only.


22. **The iOS target could not build or sign.** Two project-file faults, neither visible from
    `tsc` or Jest:
    - `CODE_SIGN_STYLE` was absent from both configurations, so `xcodebuild` used the account's
      wildcard `iOS Team Provisioning Profile: *`. A wildcard App ID cannot carry a
      capability, so the NFC entitlement check failed before anything compiled. Fixed:
      `CODE_SIGN_STYLE = Automatic` (team `H7UAM79QQP`), which makes Xcode register
      `org.fedimintcard.provisioner` with the entitlement and mint a matching profile.
    - The "Bundle React Native code and images" phase invoked `$SRCROOT/../scripts/bundle-ios.sh`,
      which does not exist and never did, so `PhaseScriptExecution` failed on every build.
      Restored to RN 0.77's own invocation. It now embeds `main.jsbundle`, so the debug app
      runs without Metro.
    **How it was found:** only by building for a device. Both are invisible to the JS gates,
    and the earlier "needs a portal change" reading was wrong — no portal change was needed.
## 6a. Dependabot alerts (open, 2026-10-28)

14 open alerts, all Rust, all transitive under `host/Cargo.lock`. Fixed here by a lockfile
bump alone — no `Cargo.toml` change:

| Crate | Was | Now | Reaches |
|---|---|---|---|
| `rustls` | 0.23.45 | 0.23.46 | `bitreq` → `esplora-client` → `fedimint-bitcoind` → sidecar |
| `rand` | 0.8.5 | 0.8.8 | `bip39` → `bdk_wallet` → `fedimint-ldk-node` → sidecar |

**Not fixable here:** `hickory-proto` 0.25.2 and `hickory-resolver` 0.25.2, six alerts
between them. `iroh 0.35.0` requires `hickory-resolver = "^0.25.1"` and
`hickory-proto = "^0.25"`; the fixes are 0.26.x. The pin comes from
`fedimint-connectors v0.13.0-beta.1`, so it lifts when Fedimint moves `iroh`. Do not try
to force it — `cargo update --precise` refuses, correctly. Recheck on the next Fedimint
bump; until then these are accepted risk, and the exposure is iroh's DNS resolution.

`lru` 0.16.3 and `rustls-webpki` 0.103.15 are already at or past their fixed versions;
their alerts fire on *other* locked copies, and the older `lru` 0.12/0.13 and
`rustls-webpki` 0.101/0.102 copies are pinned by crates that have not moved.

## 7. Next moves, in order

1. ~~Commit P1b and P1a (§ 1).~~ ~~Merge the branch.~~ **Merged 2026-10-28** as
   `3b274a3` (PR #14), with all four CI jobs green on the merge. `main` now has cardkit,
   the PIN applet and a working CI.
2. Decide **app identity** — bundle id, Play application id, display name (plan § 8.6).
   It is baked into the first TestFlight build. Constraints found on 2026-10-28:
   `android-relay` already ships `org.fedimintcard.relay` as both namespace and
   applicationId. **Decided 2026-10-28** (plan § 8.6): `org.fedimintcard.provisioner` for
   both platforms, `FedimintProvisioner` as the RN name and display name. Nothing is
   scaffolded yet — `apps/provisioner/` is P2, and these values go into `app.json` and both
   platform projects when it is.
3. ~~Get hardware authorisation.~~ **Granted 2026-10-28** (plan § 9). Next: run the P1b
   silicon checklist there, in its printed order — the third wrong PIN is permanent, so
   steps 9 and 7 do not get swapped.
4. ~~P0 (sidecar endpoints).~~ **Done 2026-10-28**, uncommitted at the time of writing.
   Next: commit and push it, then the device pass below. P1c (hosted registry) can run in
   parallel with a different hand; P3 needs the phone and the cards.
5. **Run the app against the sidecar on a real reader.** The sidecar has now been driven
   against silicon (§ 3), read-only. Still blocked for the app: **no phone attached**
   (`adb devices` is empty) and the **relay APK not built**
   (`android-relay/app/build/outputs/apk/debug/` does not exist). The other two gaps are
   closed and the recipe is repeatable:

   - **Federation without a server.** `fedimint-testing`'s `Fixtures` builds a 4-guardian
     federation in-process; `Fixtures::new_primary(MintClientInit, MintInit)
     .with_module(DummyClientInit, DummyInit).new_fed_builder(1).disable_mint_fees()
     .build()` then `invite_code()`. It dies with the process, so run it as a background
     service, not under a command deadline.
   - **Funds without a faucet.** The `dummy` module mints. The sidecar's `Wallet::join`
     registers **only** `MintClientInit`, so a wallet joined through it has no dummy module
     and `get_first_module::<DummyClientModule>()` fails with *No module of kind dummy*. A
     client that also registers `DummyClientInit` must do the `preview(..).join(..)` into a
     **fresh** `client.db` (re-joining an initialised DB fails *The client database is
     already initialized*). The tests sidestep this with `fed.new_client()`, which
     registers every module the federation has.
   - **Reader without a phone.** `till serve`'s card routes go through the *bridge*
     (`POST /apdu`, `{"apdu":"<hex>"}` → `{"response":"<hex data||SW>"}`), not the CLI's
     `CARD_BACKEND` — `pcsc` selects the card for the one-shot commands only. A ~60-line
     PC/SC relay over `Transport::transmit` on `:9876` plus
     `CARD_BACKEND=bridge CARD_BRIDGE_URL=http://127.0.0.1:9876` puts real silicon behind
     the HTTP routes. Keep such examples behind `#[cfg(feature = "pcsc")]` or the default
     `cargo test --workspace` build breaks on them.

   Then:
   - `android-relay/` up (`adb forward tcp:9876 tcp:9876`, per its README — do not modify it);
   - `card-sidecar till serve` on the laptop, data dir the one the batch signs with;
   - `cd apps/provisioner && yarn start`, debug build on the phone.
   Walk precheck → install (cardkit) → keySetup → issue, on a **blank** J3R180 first, then a
   re-tap of the same card for the keySetup-idempotent and issue-replay paths. Record the card
   state, reader and federation actually used here in § 3, and do not mark P2's or P3's
   done-lines met beyond what was walked.

`tools/cardkit/README.md` explains the engine; `docs/PROVISIONER-PLAN.md` is authoritative
for scope and the phase table.
