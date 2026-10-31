# Plan: the fedimint-card provisioner, to store parity

Status: **build started** (2026-10-27). Re-verified against `main` @ `2c78c07` on
2026-10-27. Build has started, with **P0, P1a, P1b and P1c as the parallel opening set**
(§ 6). Rewritten after 66 commits landed on `main` (`2c78c07`): the spike is done on
silicon, and the sidecar, the till and the Android POS app exist. An earlier version of
this plan assumed the applet was unproven and is superseded.

Decided by the operator 2026-10-27: **shape B** (install + issue), **React Native on the
ported cardkit** as cashucard does, **a hosted registry**, **a mandatory PIN**, and a
**public listing reached through TestFlight and the Play internal track first**, with a
**trimmed UI surface** (§ 5) as the one deliberate divergence from cashucard. App
identity is settled (§ 8.6).

This copy lives in `~/.opencode/plan/provisioner-plan.md`; the working copy the phases
build against is `docs/PROVISIONER-PLAN.md` in the repo.

The reference is the Cashu provisioner, `cashucard/cashucard` `apps/provisioner`, whose
brief is `docs/PROVISIONER-APP.md` there: a React Native (RN 0.77.1) app that programs
blank J3R180s into cards one tap at a time in batches, with six screens (sign-in,
batch setup, the loop, batch summary, settings, load test), a registry outbox, and a
demo mode.

Scope of "parity": the same screens and the same loop, plus the parts that make the
loop trustworthy on this card — because this card's applet is not the Cashu applet and
installing it is not the whole job. Parity is measured on the card path, not on the
screen count: § 5 names every screen this plan trims and why.

Nothing in this plan publishes to either store. TestFlight and the Play internal track are
the first targets; the public listing is gated on P7 sign-off and an explicit go-ahead
(see § 7).

---

## 1. What exists now

| Piece | Where | State |
|---|---|---|
| Applet: `GEN_KEY` / `GET_PUB` / `SIGN` / PIN, 8 slots; the Java tests of `SPIKE.md` | `applet/` | on silicon (J3R180, JCOP4) before P1b; load file 5,513 B after it; sign median 847 ms |
| Card transport: `Card` trait, soft card, HTTP bridge, PC/SC | `host/card/` | tested; `SoftCard` for CI |
| Federation tests: issue, spend, negatives | `host/fed-tests/` | 16 tests green |
| Card wallet CLI: `join`, `info`, `ecash`, `card setup/topup/balance/spend/cashout` | `host/sidecar/` | demo-rehearsed |
| Till service: `POST /v1/charge`, `/v1/card/balance`, `GET /v1/sales` | `host/sidecar/src/till.rs` | demo-rehearsed, `client_ref` replay-safe |
| Android app: NFC relay on :9876 + POS UI, Kotlin, no architecture deps | `android-relay/` | demo-rehearsed on a Pixel |
| Design | `docs/DESIGN-NOTE.md` | v1 of the real applet: derived keys, note list, change pool, owner commands, PIN — **not built** |

So the card *works*, and it works through a laptop and an Android phone. The gap this
plan closes is the operator's loop: getting the applet onto a card, giving the card
identity, and getting value onto it, from a phone, without a laptop.

## 2. The honest difference from Cashu

Cashu's provisioner programs a blank card and stops. On that card, programming *is*
provisioning: the applet arrives with keys and the mint association is made elsewhere.

This card's install is thinner and shorter (4.6 KB vs. a metered 82-block load, no
tag applet, no `GET_INFO` guard applet), but a programmed card is still not a usable
card. The applet has no `GET_INFO`, no PIN, no owner key and no note list: it holds
8 keys and signs. Everything else lives in `card_state.json` on the host. A card is
ready when the host knows its keys and the card holds notes.

That gives three shapes. **Decided: B**, with the operator's PIN requirement folded in.

| | A: install-only | **B: install + issue (decided)** | C: install + issue + owner |
|---|---|---|---|
| Installs the applet over NFC | yes | yes | yes |
| Runs `card setup` (GEN_KEY × empty slots), writes the card's key set + short id | — (read-only identity) | yes | yes |
| Tops the card up from the operator's wallet | — | yes, with a previewed plan | yes |
| Sets a PIN that the card enforces at spend | — | **yes, by operator decision** | yes |
| Owner key, limits, custody from `DESIGN-NOTE.md` | — | — (later) | yes |
| Depends on new applet commands | no | **yes: PIN only** (`VERIFY_PIN`, `SET_PIN`, a PIN-gated `SIGN`) | yes: all of `ALLOC`, `NOTE_ADD`, owner-signed commands |

Why B and not A: A ships an app that cannot produce a card a till can charge from. Why B
and not C: C needs the full applet (derived keys, note list, change pool, atomic write
sets, owner signatures). B needs one slice of it — a PIN — which is the smallest applet
change that makes the stored-value claim honest.

**The PIN is now on the critical path.** The applet today has exactly three commands
(`GEN_KEY`, `GET_PUB`, `SIGN`) and no PIN, no owner key, no note list; `grep -ri pin` over
`applet/src/main/java/` returns nothing. So "PIN mandatory" is not a screen change, it is
applet work + host work + a new load file, and it reorders the phases (§ 6, P1b). It does
*not* require the change pool or `NOTE_ADD`: the PIN can gate `SIGN` on the existing slot
model, which is the point — B stays reachable without waiting for C.

The applet side is small and is copied in shape, not in code, from cashucard: Java Card's
own `OwnerPIN` (verified in `CashuApplet.java:969`, `new OwnerPIN(PIN_MAX_TRIES, ...)`)
for storage and the retry counter, a persistent one-byte `pinState` used as the commit
point, and the card's own status words for the two terminal states — `69 83`
(`SW_PIN_BLOCKED`, `CashuApplet.java:461`) after too many wrong tries and `69 84`
(`SW_PIN_NOT_SET`, line 462) for a card installed and keyed but not PIN'd. Cashu's own
notes say why the state byte exists: a `CHANGE_PIN` torn after the PIN write but before
the state write must read as blocked, not as set. Only `SIGN` changes among the existing
commands; `SET_PIN` (which sets the state byte) and `VERIFY_PIN` are added, and a
not-yet-verified `SIGN` answers `6A 94`, which is the word cashucard already uses for
"a PIN is set and was not given".

`DESIGN-NOTE.md:253` states the model to copy: *the PIN is entered on the till, an
untrusted device; the no-PIN limit is the skimmer's reach and the PIN protects only what
is above it.* With PIN mandatory there is no no-PIN limit to hide behind, so the rule
becomes: **`SIGN` refuses without a successful `VERIFY_PIN` in the same session.** That is
stricter than cashucard, whose PIN arms a no-PIN allowance, and it is simpler to state on
a public listing.

## 3. What "parity" means here, item by item

Cashu's surface, and what it becomes on this card:

| Cashu (`apps/provisioner`) | fedimint-card | Notes |
|---|---|---|
| S1 sign-in against the ops service | sign-in against the **hosted registry** (§ 4) | same shape: token in memory; no `env` split |
| S2 batch setup: mint, PIN policy, limits, count | batch setup: **federation**, **count**, **PIN policy**, **top-up plan preview** | federation stays a runtime choice (Settings). PIN policy is mandatory: one PIN for the batch, or one generated per card, shown once. A PIN is never logged and never in the registry body. Limits are dropped: shape C, unenforceable by the card (§ 5) |
| S3 the loop: probe → ISD → auth → sweep → load → install → verify → done | probe → ISD → auth → sweep → load → install (opener, applet) → verify → **key setup** → **set PIN** → **issue** → done | three extra phases; see § 5 |
| S4 summary + registry rows + outbox | summary + registry rows (short id, batch, PIN state, notes issued) + outbox | outbox posts to the **hosted** service; identical body → 200, changed → 409 |
| S5 settings: key env, theme, about, demo | **`Settings`** (~120): federation, registry endpoint, top-up amounts, theme, load sha, warning, demo mode | no factory/issued key env: this applet has no deploy keys to rotate. Build-time constants are shown read-only (§ 5) |
| `tools/cardkit/gp/`: SCP02, DES, load splitting, plan, phases | **ported**, credited (MIT), AIDs and plan replaced | ~1,800 lines of TypeScript; the engine is the expensive part and is proven on silicon |
| `session.ts`: `withCard`, ISO-DEP, iOS alert line | **portable almost unchanged** | biggest risk is `3.17.2`'s iOS behaviour, which Cashu already patched |
| `store.tsx`, `api.ts`, `RegistryOutbox`, `demo/` | **ported**, pointed at the hosted service | state machine carried over; amount semantics differ |

Not carried over:

- Hard-coded release signing (`storeFile file('release.keystore')`,
  `storePassword 'android'`, `keyPassword 'android'`). Our release keystore is supplied
  by CI secrets and never committed; the plan treats a leaked key as a re-ship.
- A second mint: this app has one federation at a time, chosen at runtime in Settings.
- The `daily` / no-PIN limit fields: shape C, not enforceable by this applet.
- `LoadTestScreen` (§ 5): a laptop job, duplicated by `cardctl`.
- `TAG_CLASS_AID` / NDEF tag applet: the fedimint applet has no NFC tag class.
- `GET_INFO` guard: we have no `GET_INFO`; the guard becomes "can this card be keyed?"
  (see § 5, key setup) plus a read-only pre-check.

## 4. The backend: hosted registry, local wallet

Two services, split by what they touch:

**Hosted (the registry).** Cashu's `ops` is the shape: one Worker and one SQLite-backed
Durable Object, operator sessions, and `/registry/cards` as an idempotent upsert
(identical body → 200, changed body for the same short id → 409). We need the same, plus
two card fields Cashu does not have: whether a PIN is set, and which batch's PIN policy
applied. The hosted service never sees keys, ecash or PINs — only the fact that a card was
provisioned, its short id and its state.

**Still local (loopback).** The operator's *wallet* cannot be hosted: the issue step
needs `fedimint-client`, the federation API, and the mnemonic. That stays in the sidecar on
the operator's machine, which also drives the card through the phone's relay.

| Endpoint | Where | Status | Role |
|---|---|---|---|
| `GET /v1/health`, `GET /v1/status` | local | exists | federation name/id, relay reachability |
| `POST /v1/card/balance` | local | exists | reconcile |
| `POST /v1/charge`, `GET /v1/sales` | local | exists | merchant (till) |
| `POST /v1/cards/precheck` | local | **delivered (P0)**, silicon-read-only | read-only: what is on this card, is the ISD open, does the applet answer |
| `POST /v1/cards/keySetup` | local | **delivered (P0)**, silicon refusal only | `card setup`, then `SET_PIN`; returns keys + short id + PIN state |
| `POST /v1/cards/issue` | local | **delivered (P0)**, silicon refusal only | body: amounts; previewed plan + `client_ref`; returns notes/txid |
| `POST /auth/operator/session` | hosted | **new** | operator token |
| `POST /registry/cards`, `GET /registry/cards?batch_id=` | hosted | **new** | idempotent card rows |
| `POST /registry/batches` | hosted | **new** | batch record (federation id, PIN policy, count, load sha) |

The app talks to the hosted registry for everything about the batch, and to the local
sidecar for everything about the card. That split keeps the disclosure small: the money
never leaves the operator's machine, and the registry never holds anything that could be
spent.

`issue` reuses `CardWallet::topup` (`host/sidecar/src/glue.rs`) — it already plans
exact amounts, submits a hand-built transaction, and writes its notes before submitting. The
endpoint obeys the same rules: write state first, one job at a time, `client_ref` replay returns
the same result. It wraps `topup` as `topup_ref(amounts, client_ref)` rather than reimplementing
it; the ref is recorded in the *same* `save()` that writes the pending notes, before the
federation hears of the transaction, so a replay can never see one without the other. The
replay record lives in `CardState.issues` (a `Vec<IssueRec>`), not in `PendingTx` — `topup` never
creates a `PendingTx`; only `sign_and_submit` does.

The three endpoints live in `host/sidecar/src/cards.rs`, registered by `till.rs` `router()` and
admitted through the same single-job actor as a charge, so one card is touched at a time.
`precheck` writes nothing at all; `keySetup` never passes `--fresh` (re-keying destroys live
notes) and refuses a card holding keys this wallet did not produce; `issue` refuses any card the
wallet already holds a live or pending note on, whatever slot — not just the slots the new plan
would use, which is the case that actually doubles a copied card's value.

## 5. The app

React Native 0.77.1, TypeScript, Cashu's screens and loop ported rather than rewritten —
**with one deliberate divergence**: the screen surface is trimmed (see "Trimmed surface"
below), keeping what the operator genuinely varies at runtime.

The stack is the one cashucard ships: the cardkit engine is plain TypeScript with one
dependency (`@noble/hashes`), reached from the app through a single re-export module and
bundled by Metro via `watchFolders`; it runs unchanged under `node --test` on the desktop. The only phone-specific piece is the ISO-DEP transceiver from
`react-native-nfc-manager`, injected into the engine as a `Transceiver` function.

```
apps/provisioner/
  src/cardkit.ts        re-export of tools/cardkit + FEDIMINT_PLAN
  src/session.ts        withCard over react-native-nfc-manager
  src/api.ts            RegistryClient (hosted) + LocalClient (sidecar) + RegistryOutbox
  src/store.tsx         batch state machine, phases, stop kinds, PIN policy
  src/pin.ts            PIN entry, generation, and the wipe-on-leave rules below
  src/settings.tsx      runtime: federation, registry endpoint, top-up amounts; build constants
  src/demo/             fake card + scripted stops
  src/screens/          SignIn, BatchSetup, Loop, BatchSummary, Settings
  tools/cardkit/        ported gp/ (SCP02, DES, provision, aids) + fedimint AIDs
```

### Trimmed surface

Parity is about the card path, not the pixel count. Measured against Cashu's source
(`apps/provisioner/src/screens/`, 1,887 lines across six screens). Roughly 35% of each
screen is a style sheet; the numbers below separate that from logic, because only logic
can be cut.

| Cashu screen | Total | Logic | Decision |
|---|---|---|---|
| `BatchSetupScreen` | 458 | ~250 | **kept, cut to ~200** |
| `LoopScreen` | 489 | ~320 | **kept as is** — it is the card path |
| `BatchSummaryScreen` | 264 | ~180 | **kept, trimmed** — rows and the pending warning stay; share/export goes |
| `SignInScreen` | 213 | ~140 | **kept** — no `env` split |
| `SettingsScreen` | 251 | ~180 | **kept, ~120** — the operator's runtime surface |
| `LoadTestScreen` | 212 | ~150 | **not ported** — duplicates `cardctl` on the laptop |

**Two classes of setting, kept apart deliberately.**

*Runtime — the operator changes these without a rebuild* (`src/settings.tsx`, persisted,
shown in Settings): which federation, the registry endpoint (test vs. production), the
top-up amounts, theme. These are operational decisions, not constants; making them
build-time would mean a store review cycle to change a copy of the app's own config.

*Build-time — one correct value each:* applet and package AIDs, load sha256, cardkit
version, app identity. These are in `src/settings.tsx` as frozen defaults, never editable.

**BatchSetup, field by field.** A field earns its place if it is per-batch *and* the card
enforces it, *or* if the operator varies it between batches.

| Field | Cashu | Ours | Why |
|---|---|---|---|
| mint/federation picker (grid, custom URL, failure banner) | ~75 lines | **kept** | a second federation is expected; a rebuild to switch is a trap |
| `dailyUsd`, `noPinUsd` limits | ~40 lines | **dropped** | limits are shape C, which this applet does not have; a field the card cannot enforce is a promise the app cannot keep |
| card design chip | ~15 lines | **dropped** | no equivalent in this repo |
| count | 1 field | **kept** | the loop's denominator |
| PIN policy + batch PIN | radio + field | **kept, required** | the card enforces it (§ 8.5) |
| — | — | **top-up plan** | new: the amounts to issue, previewed by `plan.rs` before the batch starts |

Net across the six screens: five remain, ~400 lines of logic removed against ~1,230
ported. The card path — the loop, its stops, the demo, the outbox — is untouched.

**Kept deliberately, though it looks like ceremony:**

- **Demo mode as a full simulation** (`demo/fakeCard.ts`, 236 lines + `demo.test.ts`, 75).
  It computes real SCP02 cryptograms with cardkit's own DES, and its timings come from a
  measured hardware report (iPhone column: installCard 1280 ms, loadBlock 82 ms).
  `demo.test.ts` pins every demo state to the stop it must produce. This is the only way a
  reviewer without a J3R180 can exercise the install path, and it is what makes
  Guideline 2.1 survivable.
- **The registry outbox** (~55 lines in `api.ts` plus `api.test.ts`). Cheap, tested, and
  the frozen-body rule stops a retry becoming an unresolvable 409.

**This is the floor.** Everything cut is either a duplicate of a laptop tool or a field
the card cannot honour; everything kept is either the card path, a measured proof, or a
decision the operator makes between batches. The remaining bulk is the cardkit port
(~1,800 lines) and demo mode (~310), neither of which is UI. Collapsing the five screens
into two would save ~200 lines and discard Cashu's tested screen flow, with nothing to
verify the replacement against; it is a P2 decision point, to be taken with the loop in
the hand, not on paper.

### PIN handling in the app

A PIN crosses three places and must survive none of them:

- **Typed by the operator** in BatchSetup, held in component state only, cleared on
  leaving the screen, never in the store's persisted batch state, never in the registry
  body, never in a log or a crash report. A generated PIN is shown once on the summary
  and is not retrievable after the batch ends.
- **Sent to the card** as a `SET_PIN` APDU over the NFC session, and verified by
  `VERIFY_PIN` before the card is counted as provisioned.
- **Entered by the holder at a till**, which is the untrusted device the design note
  already reasons about; the app plays no part there, and the card's retry counter is
  what makes a hostile till cost tries.

A batch's PIN policy is therefore one of two: *one PIN for the batch* (operator types it
once, the app re-sends it per card) or *generated per card* (the app generates, sets, and
shows once). Both are recorded in the registry as a policy name only — never as a value.

### The fedimint plan

```
package F0 46 4D 43 41 52 44        (FMCARD, 7 bytes)
opener  F0 46 4D 43 41 52 44 4D     instantiated first
applet  F0 46 4D 43 41 52 44 01     what hosts SELECT
```

**The AIDs are frozen.** This is not an open decision. `applet/build.xml:59-62` already
fixes them as shown above, and the fixed values are correct. The CAP is
`applet/target/fedimint-card-0.1.cap`, package version `0.1`, Java package
`io.github.bobodread876.fedimintcard.applet`.

The AIDs live in `build.xml` **alone**: there are no AID constants in Java at all. Java
and Rust reference them in exactly four places, and each spells out the applet AID:

| Reference | What it is |
|---|---|
| `applet/src/test/java/io/github/bobodread876/fedimintcard/applet/CardTestSupport.java:18` | `AID_HEX` |
| `bridge/src/main/java/io/github/bobodread876/fedimintcard/bridge/SimCard.java:17` | `AID_HEX` |
| `bridge/src/test/java/io/github/bobodread876/fedimintcard/bridge/BridgeHttpTest.java:57` | SELECT APDU |
| `host/card/src/lib.rs:31` | `SELECT_APDU` bytes |

Any change to the AID is therefore a break of `docs/CAP-BUILD.md`'s verified install
sequence and of those four references at once. The plan inherits the values; it does not
choose them.

Phases, in the operator's words:

| Phase | Line | What happens |
|---|---|---|
| probe | `Checking the card` | SELECT applet → `9000` **with an empty body**; then `GET_PUB 0`: `6985` means an installed but unkeyed fedimint card, 32 bytes means a keyed one, `6D 00` means a foreign or unknown applet |
| selectIsd | `Opening the card manager` | SELECT ISD `A000000151000000` |
| auth | `Logging in` | SCP02 INITIALIZE UPDATE with factory keys; the host checks the card's cryptogram and stops `wrongKeys` if it does not match, sending nothing more |
| sweep | `Removing the old install` | DELETE applet, opener, package (never the ISD) |
| installForLoad | `Preparing` | INSTALL [for load] |
| load | `Writing n of N` | blocks of 247 bytes (5,513 B = 23 blocks, not 82) |
| installOpener / installCard | `Installing the card` | opener first, then the applet |
| verify | `Checking the install` | SELECT applet, `GET_PUB 0` → `6985` (empty slot, as it should be) |
| keySetup | `Setting up keys` | the applet is written but has no keys until the **card** generates them, and only a host can ask. Over the same relay, after the install session closes |
| setPin | `Setting the PIN` | `SET_PIN` with the batch's PIN (one for the batch, or a generated per-card one), then `VERIFY_PIN` to prove the card took it; a card whose PIN does not verify is stopped, not retried |
| issue | `Loading n of m notes` | local sidecar issues the batch's amounts to the card's keys |
| done | `Done · …9c2f` | short id = last 4 bytes of slot-0 x-only key, as Cashu's; a generated PIN shows here once |

`keySetup` matters: a freshly installed applet has 8 empty slots and is useless. The
cards must be programmed *and* keyed, and keying is 8 × ~125 ms GEN_KEYs. `setPin`
matters just as much under the PIN-mandatory decision: without it the card would be
unspendable by design.

### Stops

Cashu's four stops carry over with this card's meanings, and three are new:

| Stop | When |
|---|---|
| `pulled` | no reply mid-run; the next tap sweeps and continues |
| `refused` | card answered a status word the plan did not expect |
| `wrongKeys` | SEQ 17 or 0 mismatches between the factory key set and the card's (including a card personalised by another bench): detected on the host after INITIALIZE UPDATE, or by EXTERNAL AUTHENTICATE returning `69 82`. A failed login counts towards a permanent lock — blocks the batch until acknowledged |
| `guarded` | pre-check found a card with keys, or a key set already known to the local sidecar |
| **`differentCard`** (new) | the pubkey read after install is not the slot-0 key the batch was going to issue to |
| **`pinNotSet`** (new) | `SET_PIN` succeeded but `VERIFY_PIN` did not: the card is programmed, keyed and **unspendable**. Recorded as such in the registry so it cannot be issued to or handed out |
| **`issueFailed`** (new) | install and PIN succeeded, top-up failed: card is programmed and empty; the row records programmed-only, retry is a separate tap |

The "holds value" refusal in Cashu depends on `GET_INFO` telling it a balance. We have
no such command, so the equivalent check happens **server-side first**: the local sidecar
refuses to key or issue to a slot-0 key that is already known and holds live notes
(`card_state.json`), and the hosted registry refuses a row whose short id already exists
with a different body. That check is a lookup, not a card command, and it falls over to
the pre-check for unknown cards ("a key we have never seen may still be someone else's").

There is no `GET_INFO` **and no version bytes**, and this changes two phases. The applet
answers SELECT with `90 00` and **zero bytes** — no version, no FCI — and it answers
`INS 01` with `6D 00`. `FedimintCardApplet.java` does both: `selectingApplet()` returns
immediately, and the dispatch switch handles only `0x10`, `0x11`, `0x12`, with `6D00` as
its default. Two consequences:

- **Cardkit's Cashu `checks` cannot be copied.** Their probes are SELECT → `02 01`,
  GET_INFO, GET_PUBKEY. Ours must be: SELECT → `9000` with an empty body; `GET_PUB` slot 0
  → `6985` on a clean card, and 32 bytes after `GEN_KEY`. The sweep guard must likewise
  not depend on `GET_INFO`.
- **There is no card-side guard.** Cashu's `guard` / `mayLeaveGuardedApplet` reads
  GET_INFO; our applet has nothing to read. The destructive sweep is therefore gated only
  by the sidecar pre-check and the operator's Replace switch. See § 10.

Wording and layout follow Cashu's brief (`PROVISIONER-APP.md` there) with this repo's
theme; the card figure is drawn in `react-native-svg` like Cashu's.

Where this plan departs from cashucard's surface deliberately, § 5 lists every cut with
its line count. The departures are UI only: no stop, no verification step and no registry
contract is thinned.

## 6. Phases of work

Each phase is plan → build → review → verify on a device, as the spike was run. A phase
is done when its verification line is true, not when it compiles.

| # | Phase | Deliverable | Done when |
|---|---|---|---|
| P0 | Card-side and local service surface | sidecar `precheck`, `keySetup`, `issue` endpoints + tests | **delivered** — `host/sidecar/tests/cards.rs`, 7 tests, all four named cases plus body validation: precheck on blank / installed / foreign card; keySetup idempotent; issue `client_ref` replay; issue refuses a known holding card. **Proved against `SoftCard`, which models the applet's PIN machine** (`SET_PIN` once, three tries, `63 CX`, `6984`, `6983`, and a `SIGN` that needs a PIN on the card and this selection's `VERIFY_PIN`, else `6A94`); nothing here has run on a J3R180 — the PIN state machine on silicon is P1b's done-line, and the reader/relay path is P3's |
| P1a | cardkit port | `tools/cardkit/gp/` ported to `apps/provisioner/tools/`, AIDs and `FEDIMINT_PLAN` | engine's unit tests run unchanged; plan's `checks` include SELECT → `9000` with empty body and `GET_PUB 0` = `6985`; the fake card answers Fedimint framing, not Cashu's; NOTICE/credit added |
| P1b | **applet PIN** | `SET_PIN`, `VERIFY_PIN` and a PIN-gated `SIGN` in `FedimintCardApplet.java`; new CAP and load sha; host wrappers in `host/card`; a PIN section in `docs/DESIGN-NOTE.md` matching what is built | on a J3R180: `SIGN` before `VERIFY_PIN` → `6A 94`; after `VERIFY_PIN` → signature verifies under the slot key; three wrong PINs → `69 83` and no `SIGN`; `SET_PIN`/`VERIFY_PIN` over the APDU bridge; CAP under the applet-size bound and within the transient budget (`SPIKE.md`: ~610 B).<br><br>**Status note (P0, 2026-10-28):** the applet side of P1b is still open, and the "host wrappers in `host/card`" half of this line was not in the tree when P0 started — `Card` was keys-only (`gen_key`/`get_pub`/`sign`), with no way to send `SET_PIN`, `VERIFY_PIN` or `SELECT`. P0 added them as defaulted `Card` methods (`select_applet`, `get_data_ff21`, `set_pin`, `verify_pin`, backed by an `apdu()` hook) so the sidecar could be built and tested; the trait's shape is now whatever P1b builds against. `SoftCard` models the applet's PIN state machine so these paths are testable before silicon, which is the only place they have run.<br><br>**Test surface, measured before the work:** baseline is 44 green jCardSim tests (`mvn -B -f applet/pom.xml test`: 6 classes, 0 failures). Four things break or are unguarded, and P1b must handle each:<br>• `NoPrivateKeyLeakTest.onlyThreeCommandsExist` hard-asserts the INS set is exactly `0x10,0x11,0x12`; it **will fail** when PIN commands land and must be updated to include `0x40,0x41,0x42`.<br>• Every `CardTestSupport.sign(...)` call site — `SignTest`, `GenKeyGetPubTest`, `NoPrivateKeyLeakTest` — asserts `9000` and **will fail** once `SIGN` requires a PIN. `CardTestSupport` needs a `verifyPin(...)` helper.<br>• The `NoPrivateKeyLeakTest` 256-INS sweeps will send bogus PINs and can exhaust the retry counter; each sweep must either reinstall the card or use a PIN-exempt clause.<br>• There is **no** mechanical no-allocation test here (`noAllocationOutsideInstallTime` / `INSTALL_TIME_METHODS` do not exist); the no-`new`-outside-constructor rule is javadoc only, so PIN code is unguarded against allocation. P1b adds such a test or states the gap explicitly in `DESIGN-NOTE.md`.<br><br>There is no `OwnerPIN`, no `SW_PIN_*` constant, no session state and no transaction anywhere in the applet today. Three status words (`6983`, `6984`, `6A94`) are new and must be defined in the applet. |
| P1c | **hosted registry** | Worker + Durable Object, operator session, `/registry/batches`, `/registry/cards` idempotent upsert with the two new card fields (PIN state, PIN policy) | **delivered locally** — `registry/`; identical body → 200 with the stored row (same `received_at`); changed body → 409 and nothing overwritten; unauthenticated → 401; a row survives a Worker restart (verified across a real `wrangler dev` process restart); 16 vitest cases + curl against `wrangler dev`. **Deployed 2026-10-31** at `https://registry.fedimintcard.org` (custom domain, TLS valid), with `REGISTRY_ORIGINS` wired to it and the auth/batch wire verified live |
| P2 | app skeleton, demo mode | five screens (§ 5, trimmed), `settings.tsx`, store, demo fake card, scripted stops | demo mode walks every screen and every stop on both phones with no card; `demo.test.ts` green, so each demo state still maps to the stop it must produce; settings persist across a relaunch; no per-batch field outside federation, count, PIN policy and the plan preview |
| P3 | real install | probe → ISD → auth → sweep → load → install (opener, applet) → verify over NFC | 10 consecutive blank J3R180s installed from one phone; loop counter correct; a mid-run pull recovers on the next tap |
| P4 | key setup, PIN, issue | `keySetup` → `SET_PIN` → `VERIFY_PIN` → `issue` wired to P0 and P1b | a blank card taps once and comes out with 8 keys, a verifying PIN and the batch's notes; a `SIGN` from a till succeeds only after the PIN is entered, and the POS charges it for the demo amount |
| P5 | summary, registry, outbox | S4 + retry, against P1c | ending a batch with pending rows says so; killing the app mid-batch, relaunching, retrying the row works; the row's PIN state matches the card's |
| P6 | hardening | wrong-keys lockout, foreign card, torn load, a `pinNotSet` card, campaign of 50 cards | 50 cards: zero wrong-key attempts, every torn run recovered, no orphaned key set in the sidecar, no card leaves without a verifying PIN |
| P7 | store readiness (§ 7) | TestFlight + Play internal track first, then the public listing: demo mode as the reviewer path, privacy manifest naming the registry, data-safety forms, signing from CI secrets | TestFlight and internal track builds accepted; then App Review and Play production review pass on the same build |

**P1c status (2026-10-28): built and tested, not deployed.** `registry/` is a Worker
(`src/index.ts`) in front of one SQLite-backed Durable Object at `idFromName('registry')`.
Every done-line case is exercised twice — 16 `vitest` cases through the real Worker
(`SELF`, so the auth gate is covered) and `curl` against `wrangler dev`:
- identical body → 200 returning the **stored** row, `received_at` unchanged (a retry
  reads, it does not overwrite);
- changed body, same `short_id` → 409 with a `reason` naming the id, and the list still
  shows the first row; a `pin_set` change alone is a 409 too;
- unauthenticated, forged or expired token → 401 (the app signs out on it);
- a row survives a **process** restart: the row was re-read after killing and restarting
  `wrangler dev`, same `received_at`, out of `.wrangler/state`.
What that does **not** prove, and must not be read as proven:
- **No host exists.** `REGISTRY_ORIGINS` is still empty in `api.ts`, so no build can reach
  this service. Deploying and filling the two strings is the remaining wiring.
- **Never deployed.** A local `wrangler dev` is Miniflare; it says nothing about the
  production Durable Object's storage, a real `wrangler secret put`, or Cloudflare's
  clock. A deployed service also needs its own check that a restart keeps rows, which is
  a deploy, not a test.
- One operator account, no rate limit on `/auth/operator/session`, and no way to list or
  delete a card row. None is on the done-line; each matters before a public listing (§ 7).
`wrangler dev` reads the operator password from `.dev.vars`, **not** the shell environment
— with only `OPERATOR_PASSWORD=` exported the Worker sees it undefined and every route
answers 503 (correctly: the guard that refuses to run unauthenticated). `.dev.vars` and
`.wrangler/` are gitignored.

**P2 status (2026-10-28): built, and verified short of its own done-line.** The five
screens, settings, store, PIN handling, the demo fake card and the registry adapter exist
in `apps/provisioner/`, and `npx jest` is green (57 tests / 4 suites) with `tsc` clean.
`demo.test.ts` holds — each demo state maps to the stop it must produce — and the real
`<App />` mounts and renders SignIn. What the done-line also requires, and what has **not**
happened: walking every screen and every stop on both phones, and settings surviving a
relaunch on a device. Until a phone runs it, P2 is a Jest-verified tree, not a working app.
Two structural notes for whoever picks it up:
- **The registry was deployed on 2026-10-31.** `api.ts`'s `REGISTRY_ORIGINS` names
  `https://registry.fedimintcard.org` for both `test` and `production` (Worker
  `fedimintcard-registry`; `OPERATOR_PASSWORD` is a Worker secret). The register → token →
  `POST /registry/batches` path is verified against the live Worker, so sign-in no longer
  refuses for want of a host — see `docs/PROVISIONER-HANDOFF.md` for the evidence. That
  earlier cap on the Android device pass is therefore gone in principle, but nothing past
  SignIn has been reached on a phone either way: the 2026-10-28 pass stopped there, and no
  pass has run since the wiring.
- **The iOS build now runs on the iPhone 13 Pro Max (2026-10-28).** Two faults had kept it
  off the phone, both in the Xcode project rather than in code:
  - `CODE_SIGN_STYLE` was unset, so Xcode fell back to the account's **wildcard**
    `iOS Team Provisioning Profile: *`, and a wildcard App ID cannot carry an app-specific
    capability — the build failed on "doesn't include the NFC Tag Reading capability" and
    the matching `com.apple.developer.nfc.readersession.formats` entitlement. Signing in to
    Xcode did not change it; the setting did. Both configurations are now
    `CODE_SIGN_STYLE = Automatic`, which registers `org.fedimintcard.provisioner` with the
    entitlement and mints a real profile.
  - The "Bundle React Native code and images" phase ran `$SRCROOT/../scripts/bundle-ios.sh`,
    a file that has never existed in this repo, so `PhaseScriptExecution` failed on every
    build. Restored to RN 0.77's standard invocation
    (`with-environment.sh` + `react-native-xcode.sh`), which embeds `main.jsbundle`.
  With both fixed, `xcodebuild … -allowProvisioningUpdates build` → **BUILD SUCCEEDED**, and
  the app installs and launches on the device with its own 4.4 MB Hermes bundle. It stopped
  at the same place as Android: SignIn, because no registry origin was configured then.
  Nothing past it has been tapped on iOS either, and no pass has run since the registry was
  deployed.
- **`replace` gates nothing on this applet.** cardkit checks it only inside `probe`, and
  `FEDIMINT_PLAN` has no `guard`, so a card already holding the applet is swept with or
  without Replace. The sidecar precheck is therefore the only pre-write gate; the screen
  must not claim a refusal the engine never makes.

P0, P1a and P1c can run in parallel with different hands. **P1b is the long pole**: it is
applet code on an already-proven card, and P4, P6 and the public listing all wait on it.
P3 is the only phase that needs the phone and the pile of cards. Both P3 and P1b need
hardware; the authorisation for it was granted 2026-10-28 (§ 9), and the P1b silicon
checklist there is ready to run but has not been run.

## 7. Store readiness

The path is staged: **TestFlight and the Play internal testing track first**, then the
public listing on the same build once P7 passes. Everything below is required for the
public listing; the staging only decides *when* each item is exercised, not whether it
is done. A public listing changes two things from an internal-only ship: Guideline 2.1
becomes load-bearing (a reviewer cannot program a card), and both stores ask what data
leaves the device.

### Apple App Store

- **Guideline 2.1 (demo account / working app).** **Wired 2026-10-29.** `src/screens/DemoScreen.tsx`
  reaches all seven demo states through `runDemo` over the real engine, and it is reachable
  from SignIn *ahead of* the sign-in gate (a cold reviewer has no account) and from Settings.
  All seven states verified against the engine headlessly. **Not yet seen on a screen**: the
  test Pixel was locked behind a device PIN, so this is verified-in-code, not verified-in-UI.
  The original requirement:
  Ship a **demo mode that is a first-class reviewer path**: it must run the full install
  loop against a simulated card and show a complete batch summary, plus notes in
  App Review that name the mode and the exact steps. A build that only says "connect a
  J3R180" is rejected for incompleteness, which is the usual failure for
  hardware-dependent apps (the plan's earlier finding; unchanged). **Mandatory once the
  listing is public.**
- **Guideline 2.3.3**: the listing and screenshots must not claim the app stores or
  secures funds beyond what it does. The app is a programmer and loader; the listing says
  so, and the experimental-software warning in `README.md` is repeated in the app's
  about screen. With PIN mandatory the honest claim is narrow and testable: the card
  refuses to sign without the PIN set at provisioning, and the app says nothing about
  limits or custody, which this applet does not have.
- **Guideline 2.5.19 / NFC**: `NFCReaderUsageDescription` must describe reading a card
  to install software and load value, and the entitlement's
  `iso7816.select-identifiers` must list `F0464D4341524401` and the ISD
  `A000000151000000` (the applet *and* the card manager are selected).
  Entitlements come with the provisioning profile; a free/personal team cannot sign them.
- **App Transport Security**: the local sidecar API is `http://127.0.0.1:9877` (`till.rs`
  `DEFAULT_LISTEN`, reached on the phone through `adb reverse tcp:9877`). A localhost
  exception is required and is defensible; a LAN cleartext exception is not. Keep the
  sidecar on loopback (USB/tether), as the till already does.
- **Privacy**: the registry is hosted (§ 4), so the app does transmit provisioning
  records — card short ids, batch ids, PIN-set state — to a server the operator runs.
  That is **not** an empty disclosure on either store. The privacy manifest declares no
  tracking and no advertising, and declares the registry as a collected data type
  ("other diagnostic/other" class: operator provisioning records, not end-user data),
  linked to the operator's account, used for app functionality, not sold. The App Review
  notes say plainly what the server stores and what it never stores (no keys, no ecash, no
  PINs). Sync the manifest to the code with Cashu's `PrivacyInfo.xcprivacy` as the shape.
- **Threats to normal operation**: the iOS NFC session lasts as long as the sheet is up;
  a long top-up fan-out (several transactions) must happen **after** the install session
  closes (the app re-taps for issue), or the iOS session will time out mid-issue.
- TestFlight first (internal and then external testers, which itself needs a short beta
  review); the App Store listing follows P7 sign-off and explicit go-ahead.

### Google Play

- `minSdk 26`, `targetSdk 35` (Android 15) as in `android-relay`; Play requires a
  recent target by its annual deadline, so this is a rolling chore.
- **Release signing from CI secrets**, not a committed keystore. Nothing from
  `cashucard`'s `apps/provisioner/android/app/build.gradle` gets copied: its release key
  password is the literal `android`.
- `uses-feature android.hardware.nfc required="true"`: the app is useless without NFC, so
  declaring it required keeps the listing honest; Play will not offer it to non-NFC
  devices.
- `usesCleartextTraffic` is needed for loopback; Play asks for a justification for
  cleartext in the data-safety form — loopback and USB-tethered only is the answer.
- Play's **Data safety** form: the hosted registry means the app declares "data
  collected" — app identifiers and provisioning records sent to the operator's own
  server, for app functionality, not shared with third parties, encrypted in transit,
  deletable on request. Claiming "no collection" against a hosted registry is a
  misdeclaration Play removes apps for. **Content rating**: utility, no user-generated
  content.
- Internal testing track first, then closed, then the public production listing on
  explicit go-ahead.

### Both

- **Done (2026-10-29): version and build numbers from one source.** `package.json`'s
  `version` and `provisionerBuild` are the source; `apps/provisioner/scripts/sync-version.cjs`
  writes them into Gradle (`versionCode`/`versionName`), the Xcode project
  (`CURRENT_PROJECT_VERSION`/`MARKETING_VERSION`, both configurations) and `Info.plist`'s
  literal. `--check` gates every Android build (`preBuild`, so debug too) and a `--check`
  against a hand-edited Gradle file exits 1 (verified). Set to **1.0.0 (11)**: the
  marketing version the devices already carried, with the build number moved forward
  because a store will not accept a reused one. Bump `provisionerBuild` before every
  upload.
- **Open: `apps/provisioner/fastlane`** (`Appfile`, `Fastfile`, `Matchfile`), with the App
  Store Connect key and Play service account in CI secrets.
- **Done (2026-10-29): the two release-gate scripts the Gradle file called.** Both had
  never existed, so `assembleRelease` failed before producing anything; with them present
  a signed release builds (verified with a throwaway key, `apksigner` confirming the
  upload cert). `configure-service.cjs --check --production` refuses a fake local
  session, a non-loopback sidecar, or a non-https registry origin; `check-android-signing.cjs`
  names the missing key variable and refuses placeholder passwords.
- No card data, no keys, no ecash in logs, crash reports or screenshots.

**Two deviations found while doing the above, neither fixed here:**
- **`minSdkVersion = 24`, not the 26 this section states.** Play accepts 24; the plan's
  26 was chosen deliberately, so this is a decision to re-make or the plan to correct —
  not a change to make silently.
- **`targetSdkVersion = 36`**, ahead of the 35 written above (Play's floor is a rolling
  requirement; 36 satisfies it).

## 8. Decisions

Every item below is settled: 1–5 on 2026-10-27, 6 on 2026-10-28. A new question opens a
new numbered item here; it does not reopen one of these.

1. **Shape: B** — install + issue. A newly programmed card comes out keyed and funded
   from the operator wallet, so the laptop is not needed to produce a usable card. The
   rest of C (owner key, limits, note list, change pool) stays deferred to the applet
   milestones in `DESIGN-NOTE.md`; the PIN slice of C does not (§ 8.5).
2. **Stack: React Native with the ported cardkit**, as in § 5. This is what cashucard
   does and it carries the same guarantees: the SCP02/DES/install engine is plain
   TypeScript with a single dependency (`@noble/hashes`), bundled by Metro through
   `watchFolders`, run unmodified under `node --test` on the desktop, and injected into
   the app only as a `Transceiver` function. The phone's contribution is
   `react-native-nfc-manager`'s ISO-DEP transceiver and nothing else. The alternative
   (Kotlin on `android-relay`) was rejected: it has no iOS path and duplicates the
   engine.
3. **Audience: public listing, staged.** TestFlight and the Play internal testing track
   first; the public listing follows P7 sign-off. Because the listing is public, the
   Guideline 2.1 demo-mode requirement in § 7 is mandatory, not optional: it is the only
   way a reviewer can exercise the app.
4. **Registry: hosted.** Cashu's `ops` shape (§ 4): a Worker and a SQLite-backed Durable
   Object, operator sessions, `/registry/cards` as an idempotent upsert. The wallet and
   the issue step stay on the operator's machine, so the registry sees no keys, no ecash
   and no PINs. Consequence for the stores: the app transmits card short ids and batch
   records, so **both stores' disclosures are non-empty** — the privacy manifest and the
   Play data-safety form name the registry, its purpose and that it is operator data,
   not end-user data (§ 7).
5. **PIN: mandatory.** Every card this app provisions gets a PIN, and the card enforces
   it: `SIGN` refuses unless `VERIFY_PIN` succeeded in the same session. This is applet
   work, not a screen change — the applet has three commands and no PIN today — so it
   adds P1b below and moves ahead of any store build. It deliberately does *not* pull in
   the change pool or `NOTE_ADD`.

6. **App identity — decided 2026-10-28.**

   | Field | Value |
   |---|---|
   | Android `applicationId` and `namespace` | `org.fedimintcard.provisioner` |
   | iOS `PRODUCT_BUNDLE_IDENTIFIER` | `org.fedimintcard.provisioner` |
   | RN `name` (and `displayName`) | `FedimintProvisioner` |

   Reverse-domain root `org.fedimintcard` matches what already ships in this repo
   (`android-relay` is `org.fedimintcard.relay`), and `.provisioner` is a sibling, not a
   child, so the two apps can be installed side by side and neither can capture the
   other's intents. Cashu's upstream id is `com.islandbitcoin.provisioner` on both
   platforms; nothing here collides with it.

   The display name is deliberately the same string on both platforms — a store name
   that differs between iOS and Android splits reviews and support. Renaming it later is
   a store listing change, not a bundle-id change.

   These are build-time constants (§ 5): frozen in `src/settings.tsx` as defaults,
   never editable at runtime.

## 9. Hardware authorisation — granted 2026-10-28

P3–P6, and the applet phase P1b, need these devices and actions, each named before use:

- NXP J3R180 cards (blank, and the pile from the demo), tapped on a phone.
- One iPhone and one Pixel, each with the app installed.
- A laptop running the local sidecar for P0–P5.
- Optional: the ACS ACR122U on the laptop, as the known-good fallback path.

**The operator granted authorisation for these devices and actions on 2026-10-28.** That
removes the blocker; it does not make any result measured. Each phase still names the
devices, the APDUs and the stop conditions before it starts, and no phase's "done when"
line is satisfied until it has actually been run.

Grants do not extend beyond this list. Anything added — another reader, a different chip,
a phone the app is not installed on — is a new authorisation.

### P1b silicon checklist (authorised, not yet run)

Run in this order on a J3R180, because two of these steps spend PIN tries and one bricks
the card when tries run out:

1. Record the card's identity and the blank state before anything is loaded.
2. Load and instantiate the opener, then the applet (`gp --install`, `gp --create`),
   per `docs/CAP-BUILD.md`.
3. `SELECT` the applet → expect `02 00 90 00`.
4. `SIGN` slot 0 **before** any `VERIFY_PIN` → expect `69 84` (no PIN set). `checkPinReady`
   runs before the slot and message checks, so this must be `69 84` and *not* `69 85`
   (empty slot) — if it answers `69 85`, the gate is not ahead of the slot check and the
   card is leaking which slots hold a key. Record which arrived.
5. `SET_PIN "1234"` → `90 00`. A second `SET_PIN` → `69 85`.
6. `SIGN` now, still without `VERIFY_PIN` → `6A 94` (PIN required). This is the pair that
   proves the gate distinguishes "no PIN configured" from "PIN not entered".
7. `VERIFY_PIN "1234"` → `90 00`; `SIGN` → 64 bytes, and the signature must verify under
   the slot-0 pubkey (`BIP340`), not merely return `90 00`.
8. Reset the card, `SIGN` with no `VERIFY_PIN` → `6A 94`. This is the sticky-check: a
   session PIN must not survive a reset.
9. Wrong PIN twice → `63 C1` then `63 C0`. **Do not spend the third try on a card whose
   key you care about** — the third is `69 83` and the block is permanent.
10. Repeat 7–8 over the APDU bridge (`bridge/`), not only over `gp`, so the app path is
    covered too.
11. Record the measured signature time against `SPIKE.md`'s ~610 B transient budget and
    the applet-size bound; both numbers go into `docs/CAP-BUILD.md` next to the load-file
    size.

If any step diverges, stop and record the exact APDU and status word. A divergence is a
result worth having; a workaround that hides it is not.

## 10. Risks

| Risk | Mitigation |
|---|---|
| iOS NFC session shorter than install + issue | issue happens in a second tap, after the session closes |
| `react-native-nfc-manager` 3.17.2 iOS bugs | Cashu already patches them; the same patch is carried over |
| A wrong-key login bricks a bench card | the app blocks the batch after the first failure; factory keys are verified in a pre-check |
| **PIN work reopens a proven applet** | the applet is proven on silicon as three commands (`SPIKE.md`); P1b adds two and gates one, and its verification line is measured on a J3R180, not read from code. Existing 45 Java tests stay green before the new ones are added |
| **The sweep is destructive and has no card-side guard** | on this applet there is no `GET_INFO` and no version byte, so a card-side `guard` has nothing to read (Cashu's `mayLeaveGuardedApplet` has no counterpart here). The sweep is gated only by the sidecar pre-check and the operator's Replace switch, and neither sees a value-holding card the sidecar has never met. Mitigation: the pre-check runs on every card, and the Replace switch is an explicit per-card operator act, never the default |
| **A PIN-gated `SIGN` breaks the till and the demo** | `6A 94` is the word cashucard already uses for "PIN not given", so the POS and the demo treat it as the normal pre-PIN answer, not as an error; P4's done-when is a real POS charge |
| **A `pinNotSet` card reaches a holder unspendable** | the card is recorded as programmed-only and never counted as provisioned; P6's campaign requires zero such cards in 50 |
| **A hosted registry widens what leaves the device** | only short id, batch, PIN policy name and state leave; keys, ecash and PIN values never do (§ 4); both stores' disclosures say so (§ 7) |
| Change pool / denominations make "issue" a toy (8 slots) | the batch's issue plan is previewed (`plan.rs` refuses plans it cannot make exact); the app shows the plan, not just success |
| Porting cardkit drifts from Cashu's | the port keeps the engine's tests; a differential test runs the same APDUs against the Java bridge |
| Store review of a hardware app | demo mode is a full simulation, reviewed on both phones |
| `apps/provisioner` and `android-relay` diverge into two NFC stacks | one shared transport shim, chosen in § 8.2 |

## 11. Non-goals

- The rest of shape C from `DESIGN-NOTE.md`: owner key, limits, note list, change pool.
  The PIN is in scope (§ 8.5); nothing else of C is.
- Hosting the operator wallet or the issue step: they need `fedimint-client` and the
  mnemonic and stay on the operator's machine.
- A merchant onboarding app: the POS in `android-relay` already is the merchant side.
- Any consensus or module change.
- Publishing to either store without an explicit go-ahead.

## 12. Decisions taken at build start (2026-10-27)

1. **Dual-protocol scope is 1–3, not 4.** In scope: (1) two applets on one chip, (2) one
   provisioning app that targets either, (3) a shared PIN command set. Explicitly out of
   scope: (4) unifying value across Cashu and Fedimint. *Reason:* Cashu proofs and
   Fedimint notes are different blind-signature schemes from different issuers; the card
   cannot unify them, only a custody-bearing service can.

2. **The app lives in `fedimintcard` (option A), not in `cashu-card`.** cardkit gains a
   third `Plan` (`fedimintPlan`) rather than a fork; `provision()`, SCP02, DES and the
   recovering install are untouched. *Reason:* the engine is proven and shared; a fork
   would duplicate it and drift. Renaming to `eCashCardProvisioner` is deferred to a
   follow-on merge into `cashu-card`, after the dual app works, and requires the other
   counterparty's agreement.

3. **P1b adopts Cashu's PIN command bytes and status words** — `0x40 VERIFY_PIN`,
   `0x41 SET_PIN`, `0x42 CHANGE_PIN`; `6983` blocked, `6984` not set, `6A94` pin needed.
   *Reason:* it is what makes a shared reader prompt and a dual card possible without a
   protocol break later.

4. **The Cashu load file is vendored, not built at install time.** If a Cashu target is
   added to the app, `make-assets.py` pins the Cashu CAP's sha256 as an input from a repo
   this project does not control. *Reason:* vendoring freezes the promise ("this app
   installs this specific card software") at a known version rather than letting a
   routine rebuild silently move it. Deferred until a Cashu target actually ships.

5. **cardkit's fake card must be ported alongside the engine.** *Reason:* the engine's
   tests are written against `gp/test/fake-card.ts`, which answers Cashu's GET_INFO and
   `02 01` SELECT; a ported engine tested against a Cashu-shaped fake proves nothing about
   Fedimint's `GEN_KEY`/`GET_PUB`/`SIGN` framing. The fedimint fake card needs four touch
   points: module constants, `heavyModules`, SELECT's version branch (fedimint answers
   empty `9000`), and an applet command branch.
