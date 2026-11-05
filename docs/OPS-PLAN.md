# Plan: a hosted ops service, and no laptop in the operator's loop

Status: **accepted** (2026-11-04), against `main` @ `ea7c12f`. **O1 is done; O2 is built and tested locally, not yet deployed** (§ 7); the rest is not built.
The owner's decisions are in § 9. This plan replaces § 4 of `PROVISIONER-PLAN.md` ("hosted
registry, local wallet"); where the two disagree, this plan wins.

## 1. Why

Today a card cannot be finished without the Mac. The provisioner installs the applet over
the phone's NFC, then calls the sidecar at `http://127.0.0.1:9877` for key setup, the PIN
and the top-up (`LoopScreen.tsx`, `LOCAL_SIDECAR_ORIGIN` in `settings.tsx:134`). The sidecar
reaches the card through a second app on the phone, `android-relay`, over
`adb forward tcp:9876`. So a production batch needs a laptop, a USB cable, Android, and two
apps. On iOS there is no relay, so a card cannot be finished at all. Max shows "Sidecar not
reachable from this phone" for this reason.

cashucard has no laptop in its loop. Its Cloudflare ops service holds operator sessions, the
registry and the batches, and **no money**. Its provisioner talks only to ops and to the
card. Its cards hold their own value, and loading value is a wallet flow outside the
provisioner. This plan gets fedimintcard to the same shape, as far as today's applet allows.

## 2. What the sidecar holds today, and where each piece goes

| Piece | Today | Spendable without the card? | Goes to |
|---|---|---|---|
| Key setup: `GEN_KEY` for empty slots, `GET_PUB` | sidecar over the relay (`cards.rs` `key_setup`, `glue.rs` `card_setup`) | n/a | **the phone, in the install tap** (§ 4) |
| `SET_PIN` | sidecar over the relay (`cards.rs` `set_pin`) | n/a | **the phone, same tap** |
| Slot pubkeys, short id | `card_state.json` `slots[]` | no | **ops** |
| Live notes: slot, amount, nonce, BLS signature | `card_state.json` `notes[]` | **no**: spending needs a BIP340 `SIGN` from the slot key, which never leaves the card, behind the card's PIN (`DESIGN-NOTE.md:23,89`) | **ops** |
| Blinding keys of pending change notes | `notes[].blinding_key` | no: they unblind a signature, and the note still needs the card to spend | **ops** (so any till can finish an interrupted charge) |
| Pending transactions, client_refs, tombstones | `pending_txs[]`, `tombstones[]` | no (a signed tx resubmitted is idempotent) | **ops** |
| Secret keys of the till's own outputs | `host_pending[].secret_key` | **yes**, while in flight | **stays at the till**, never ops |
| Operator wallet (mnemonic, `client.db`) | sidecar data dir | yes | **nowhere hosted**. Top-up leaves the provisioner (§ 6) |
| Sales log | `sales.json` | no | stays at the till |

Two facts the fact-finding turned up, which the plan has to fix anyway:

- **One card per sidecar.** `card_state.json` is "everything the host must remember about
  *the* card" (`state.rs:1`), one file per data dir. A till today can charge exactly one card.
  Ops makes state per card, which a real deployment needs regardless.
- **Two short-id rules.** The sidecar hashes the keys with Rust's `DefaultHasher`
  (`cards.rs:274`), which is not stable across Rust versions. The registry documents "the last
  4 bytes of the slot-0 x-only key" (`registry/src/index.ts:102`). This plan keeps the
  registry's rule and drops the hash.

## 3. Target shape

```
 provisioner phone ──NFC──▶ card   (install, GEN_KEY ×8, GET_PUB ×8, SET_PIN, one tap)
        │
        └──HTTPS──▶ ops (Cloudflare Worker + Durable Object, registry.fedimintcard.org)
                         ▲   batches, cards, slot keys, notes, pending txs
 till ──NFC──▶ card      │
   │   └──HTTPS──────────┘   read the card's notes, write the outcome of a charge
   └──▶ federation           (the till's own fedimint client: submit, await, reissue)
```

Ops never holds a seed, a PIN, a slot secret or a till's output secret. It holds what a card
owns, in a form that only the card can spend.

## 4. The provisioner: finish the card in one tap

After cardkit's `provision()` installs the applet, the same NFC session continues (the
transceiver is already open in `withCard`, `session.ts`):

1. `SELECT` the applet (`F0 46 4D 43 41 52 44 01`).
2. For slots 0 to 7: `GET_PUB` (INS `11`). On `6985`, `GEN_KEY` (INS `10`, P2 `00`). No PIN
   is needed for an empty slot. Keep existing keys, as the sidecar does.
3. `SET_PIN` (INS `41`, 4 to 8 digits). `6985` means a PIN is already set, which is a stop
   ("card already has a PIN"), not a pass.
4. `VERIFY_PIN` (INS `40`) once, to prove the PIN took.
5. Short id = last 4 bytes of the slot-0 key, lower-case hex.
6. The registry row gains the eight slot pubkeys (§ 5).

Timing: `GEN_KEY` is 125 ms on a J3R180 (`SPIKE.md:37`), so eight keys add about 1 s to the
tap. That fits inside iOS's reader session. P-O1 measures the whole tap on both phones.

The loop's phases become `probe → selectIsd → auth → sweep → load → install → verify →
keySetup → setPin → done`. The `issue` phase and `LocalClient` leave the app. The phone needs
no sidecar, so Max and any iPhone become provisioning phones.

**Showing a per-card PIN, as cashucard does** (§ 9, decision 3). The PIN the phone generated
is shown on the loop screen with the finished card, and stays there until the next card's tap
starts (cashucard `LoopScreen.tsx:407-412`, `PROVISIONER-APP.md:127-129`). The operator writes
it down or hands it over with the card. It is never on the summary, never sent to ops, never
exported. This replaces today's behaviour, where the summary masks the PIN and keeps only the
last card's (`BatchSummaryScreen.tsx:58`, `pin.ts`'s reveal slot). Generation stays as it is
here: `generatePin` uses the platform's secure random and refuses weak PINs (`pin.ts:156`),
which is stronger than cashucard's `Math.random()` 4-digit PIN. A batch PIN is typed by the
operator and, unlike cashucard's `pin_batch` column, is not stored in ops.

## 5. Ops: the registry Worker grows card state

Extend the deployed `registry/` Worker rather than adding a second service. It already has
operator sign-in, one SQLite Durable Object and an idempotent `/registry/cards`.

**New tables**

- `card_slots(short_id, slot, pubkey)`, written once at provisioning.
- `notes(short_id, slot, amount_msat, nonce, signature, blinding_key, status, txid, out_idx)`,
  with `status` live, pending, spent or dead, as in `state.rs`.
- `card_txs(short_id, txid, client_ref, tx_hex, inputs, change, status, till_id, ts)`.
- `card_versions(short_id, version)` for compare-and-swap.
- `tills(till_id, name, token_hash, revoked_at)`.

**New routes**

| Route | Who | Does |
|---|---|---|
| `POST /registry/cards` (extended) | operator | adds `slot_pubkeys[8]`; identical body 200, changed 409, as now |
| `GET /cards/:short_id/state` | till | slots, live and pending notes, open txs, `version` |
| `POST /cards/:short_id/intent` | till | records a charge before the card signs: tx hash, inputs, change outputs with blinding keys, `client_ref`; requires the `version` it read, else 409 |
| `POST /cards/:short_id/outcome` | till | accepted or rejected, the change notes' signatures; marks inputs spent; bumps `version` |
| `POST /tills`, `DELETE /tills/:id` | operator | issues and revokes a till token |

**Write order.** This is the sidecar's existing write-ahead order (`glue.rs`
`sign_and_submit`), moved to ops. The intent is in ops before the card signs. Any till that
next sees the card finds an open intent, asks the federation for that txid's outcome, and
records it. A till that dies mid-charge leaves nothing unrecoverable. The till's own output
secrets stay in its local store, as now.

**Concurrency.** A card is in one till's field at a time. The version check stops two tills
from both writing an outcome against the same notes. The federation's spent-nonce set stops
a double spend whatever ops says.

**Auth fixes that ride along**

- Till tokens are stored and revocable.
- Operator accounts and sessions follow cashucard (§ 9, decision 5). That retires
  `passwordMatches` (`index.ts:264`), which says it hashes both sides but compares the hex of
  the raw bytes, so the password's length leaks.
- `batches.federation_id` takes the decoded federation id (`decodeInvite` in the app), not the
  whole invite (§ 9, decision 4).

## 6. The till, and where value comes from

**Till.** The sidecar's till keeps its federation client, its `sales.json` and its output
secrets. Its card state comes from ops: a `StateStore` backend over HTTPS replaces the file,
keyed by short id, which the till reads from the card (`GET_PUB` slot 0). The charge logic in
`glue.rs` stays. Only where it loads and saves changes. The till can then charge any card the
operator provisioned.

**The holder's PIN at the till** (§ 9, decision 6). `/v1/charge` takes the PIN in its body
from the holder, typed on the till's screen, and the till sends `VERIFY_PIN` in the selection
that signs, as it already does (`703780e`). `CARD_PIN` in the environment
(`host/card/src/lib.rs`) is removed outside tests. The PIN is never logged, stored or sent to
ops. Wrong PINs report the tries left (`63 CX`); a blocked card (`69 83`) is a stop.

**Value comes from the holder's wallet** (§ 9, decision 2), as cashucard leaves loading to
holder apps. The provisioner stops loading value; install-only is already the default
(`7254d43`). A Fedimint wallet loads a card the way the sidecar's `issue` does today
(`cards.rs`, `CardWallet::topup`): it spends its own notes into outputs whose nonces are the
card's free slot keys, holds the blinding keys until the federation signs, and records the
new notes against the card. In this shape that record goes to ops:

| Route | Who | Does |
|---|---|---|
| `GET /cards/:short_id/slots` | holder wallet | the free slot keys to load into, and `version` |
| `POST /cards/:short_id/load` | holder wallet | the pending notes with their blinding keys, before submitting; version-checked |
| `POST /cards/:short_id/load/outcome` | holder wallet | the federation's signatures; the notes become live |

**Open design item: how ops trusts a load.** A wrong record can't steal anything, because
only the card can spend. But a bogus load could make a card show money it doesn't have, or
overwrite real notes. Two options, decided in O4:

- Ops verifies each new note's BLS signature against the federation's public keys before
  marking it live. This needs the federation's keys in the Worker, but no holder account.
  **Recommended**, because it is the offline-friendly one: a signed note proves itself, so a
  wallet can queue a load and upload it whenever it has a connection, and a till can check a
  card's notes without asking ops.
- The holder proves they hold the card, which needs the card in hand while ops is online. The applet's `SIGN` signs any 32 bytes with a spend
  key, so a challenge must never be something a till could submit as a transaction. That
  needs a domain-separated signing command in the applet. This is a larger change.

The wallet app itself is not in this plan (§ 10). This plan delivers the ops routes, a
reference loader in the sidecar's CLI that uses them, and the removal of the provisioner's
top-up: the quick picks and `Top-up` section from `ea7c12f`, `issue`, and the Settings value.

## 7. Phases

| Phase | Work | Done when |
|---|---|---|
| **O0** | Owner decisions in § 9 | **done** 2026-11-04 |
| **O1** | Phone key setup and PIN (§ 4) in `cardSetup.ts` and `LoopScreen`; a keyed card is never installed over (`probeApplet`); per-card PIN shown on the loop until the next tap, removed from the summary; registry rule for short id; decoded federation id; remove `issue`, `LocalClient`, the sidecar check and the top-up UI. `slot_pubkeys` on the card row moved to O2, which deploys the registry | jest green with a fake card transceiver; **silicon**: one blank J3R180 finished on the Pixel and one on Max, with no Mac running, `GET_PUB` ×8 matching the registry row, `SIGN` refused before `VERIFY_PIN` (`6A94`) and accepted after. **Pixel done 2026-11-04**: one tap finished a blank J3R180 (short id `2fe32d75`); on an ACR122U, 8 keys read back, `SIGN` → `6A94` before `VERIFY_PIN`, verifying signature after. **Max done 2026-11-04**: a J3R180 (IC serial `09336320`, wiped of its Fedimint package first) finished in one tap on iOS 26.5.2 (short id `15989f91`), same reader checks pass. **Keyed-card refusal on silicon**: the Pixel's finished card tapped on Max stopped as `guarded` at 0 %, and its 8 keys read back unchanged |
| **O2** | Ops tables and routes (§ 5); operators as cashucard has them (§ 9, decision 5); till tokens; decoded federation id | vitest for each route, including 409 on a stale version and on a changed row; deployed to a staging Worker first. **Built 2026-11-04**: `registry/src/{auth,cards,db,http,index}.ts`, 35 vitest cases through the real Worker (sessions, budget, setup command, scoping, tills, intent/outcome/renew, the v1 → v2 upgrade). PBKDF2 at 100,000 iterations for sign-in time. **Staging 2026-11-04**: `fedimintcard-registry-staging` deployed with the owner's `OPERATOR_SETUP`; `registry/scripts/smoke-staging.sh` passes all 16 checks there (sign-in 0.26 s). **Production 2026-11-04**: deployed (version `9e96ab3d`) after the owner set its `OPERATOR_SETUP`; existing rows upgraded to operator 1 |
| **O3** | Till on ops: an HTTPS `StateStore` in the sidecar, keyed by short id; recovery of an open intent; holder PIN in `/v1/charge`, `CARD_PIN` out of the charge path | the sidecar's existing charge tests pass against a local `wrangler dev`; a charge killed between `SIGN` and outcome is recovered by a second till; two cards charged on one till. **Built 2026-11-04; registry deployed to staging and production (`b3f8924e`) the same day; the Mac till runs on ops against production and its reported name ("Island Bitcoin Community") shows on the Pixel**: `host/sidecar/src/ops.rs` (per-card file `cards/<short_id>.json`, loaded from ops on every card job; writes queued in `ops_outbox` until ops has them), `serve --ops-url/--ops-token`, `pin` in `/v1/charge`. `tests/ops_till.rs` passes against a local `wrangler dev` (end to end, second-till recovery, two cards). Registry: an intent with no inputs is a load (`tx_hex` optional), the state reports spent notes on unrenewed keys, change under a used key is a 409, slot 0 is never renewed |
| **O4** | Holder loading (§ 6): the load routes, the trust choice, a reference loader in the sidecar CLI | a card provisioned on Max, loaded by the reference loader from a separate wallet, charged at a till with the holder typing the PIN; a load with a forged signature is refused **Built 2026-11-04 (`60463dc`); deployed to staging and production (`3de64977`) the same day, after the Workers Paid upgrade; the Mac till restarted on it and reported the mint keys**: ops verifies every note it makes live against the federation's mint keys, which tills report with the name (`registry/src/tbs.ts`, checked against vectors from the Rust code); holder routes `GET /cards/:id/slots`, `POST /cards/:id/load` (30-minute expiry, 12 writes an hour per card), `POST /cards/:id/load/outcome` (forged signature → 422); `card load` in the sidecar CLI is the reference loader. Passes against `wrangler dev`. **Done on silicon, production, 2026-11-04**: J3R180 `046eeae1` provisioned on Max (per-card PIN), loaded 131,072 msat by `card load` from a separate wallet (signature verified by ops), charged 65,536 msat at the Mac till through the Pixel relay with the holder typing the PIN; change 65,536 msat in slot 1; ops at version 5 with slots 2–7 free. Deploy order: upgraded till started first (it reports the keys), then the registry |
| **O5** | Docs and store forms | `PROVISIONER-PLAN.md` § 4 rewritten; App Store privacy and Play data-safety forms name the card state ops holds; `android-relay` dropped from the operator setup |

O1 stands alone and removes the laptop from provisioning. O2 and O3 move the till.

**After O4 (owner, 2026-11-04).** In order: a full working end-to-end run with the finishing work
(O5, a signed release build, a restore check); then fedimintcard.org back up, describing what
works; then a plan for flash-mobile as the holder's wallet. flash-mobile has NFC and Cashu but no
Fedimint client, so that plan starts with a Fedimint client inside a React Native app.

### O5: the finishing checklist (owner, 2026-11-04)

Done means every box is ticked and the acceptance run at the end passes on release builds.

**Card security and lifecycle**
- [ ] **Per-card card-manager keys, as cashucard does (D29).** Cards today keep the chip maker's
  default GlobalPlatform keys (`404142…4F`), so anyone with a reader can delete the applet. Ops
  derives each card's ENC/MAC/DEK set with HKDF-SHA256 from a master held as a Worker secret, keyed
  by the chip's identity (CPLC fabricator, IC type, serial: stable across resets), and escrows it in
  the same transaction that issues it, so a lost answer re-issues the same bytes. The phone fetches
  the set during the install tap and rotates the card's keys (PUT KEY) after installing, then
  forgets it. The backup then carries the escrow, still age-encrypted. Threat model written down:
  whoever holds the escrow can manage every card in it. **Built 2026-11-04, not deployed, silicon
  pending**: `registry/src/managerKeys.ts` (`POST /registry/manager-keys {chip, short_id?}`,
  operator token only; HKDF-SHA256 over `CARD_MANAGER_MASTER`, salt `fedimintcard/card-manager/v1`,
  info `gp-enc`|`gp-mac`|`gp-dek` + the chip id; the first operator to ask owns the chip, another
  gets 409; no master → 503), escrow table `manager_keys` in migration 6 and in the backup. Chip id
  = cardkit's `readChipIdentity` (`tools/cardkit/gp/identity.ts`, cashucard's CPLC id: fabricator,
  IC type, fabrication date, serial, batch). The install tap is `apps/provisioner/src/installTap.ts`:
  probe → CPLC → keys from ops → install logged in with factory or own keys (one INITIALIZE UPDATE
  checked offline against both) → PUT KEY in place (version 01 kept) and a fresh login under the
  new set → GEN_KEY ×8, SET_PIN, VERIFY_PIN. The card row carries `chip`. Deploy needs
  `wrangler secret put CARD_MANAGER_MASTER` on staging and production first.
  A tap lifted during GEN_KEY ×8, SET_PIN or VERIFY_PIN (found on silicon 2026-11-04: the card left
  on its own keys, keyed, with no PIN, and refused by every later tap) keeps the batch's place and
  is finished by tapping the same card again, in this batch or a later one: the probe reads slot 0
  and the PIN's state without a try; with no PIN, ops' `POST /registry/resume-check {chip,
  short_id}` must show nothing under that id and no other open card on the chip, one INITIALIZE
  UPDATE must prove the card's own keys, and then all eight slots are generated again (GEN_KEY
  P2 `01`, which the applet allows only while no PIN is set), a fresh PIN set and verified. The
  new row names the old short id as `supersedes`; ops closes that row (`cards.replaced_by`) in the
  same transaction, refusing if it holds value, has its PIN set or is another operator's or chip's,
  and the old id answers 410 like a reset id. A PIN that was set but whose answer was lost is
  verified once with the PIN the phone still holds in memory for that chip, and the keys are kept;
  without it the tap stops `needsReset` (no try spent) for Settings → Reset a card. Ops takes no
  load or till charge for a row whose PIN is not set (409).
- [ ] **Reset in the provisioner.** Settings → Reset a card: read slot 0, ask ops for the card
  (another operator's or an unknown card is refused); refuse while ops shows a live or pending
  note, an open charge or an open load; the holder's PIN is required as consent, because ops cannot
  see a load made around it; a blocked PIN needs ops to show the card empty plus a second operator
  confirmation. Then take the card back to factory, as cashucard's owned-card reset does: delete
  our package (only `F0464D43415244`, its applet and opener), PUT KEY the factory set back in place
  at version 01 (what a factory J3R180 reports), and prove both with a full login under the factory
  keys and a GET STATUS that lists none of ours. No reinstall, no new PIN, no new card row: the card
  is blank, and goes through a new batch to be used again (the install tap gets the same per-card
  keys back from ops). **Built 2026-11-04, not deployed, silicon pending**:
  `apps/provisioner/src/reset.ts` and `screens/ResetScreen.tsx` over cardkit's `factoryReset`
  (`tools/cardkit/gp/reset.ts`), two taps (read slot 0 and the PIN's state without spending a try;
  then slot 0 again, ops' check again, `VERIFY_PIN`, CPLC, keys, one INITIALIZE UPDATE checked
  against the card's own set and the factory set, GET STATUS, journal, DELETE, PUT KEY, the factory
  login and GET STATUS). A tap lifted anywhere is finished by tapping again: the card's state is
  read from that one INITIALIZE UPDATE and GET STATUS, never by an EXTERNAL AUTHENTICATE with keys
  the card did not prove (own keys and ours present: carry on deleting; own keys and ours gone:
  restore the keys; factory keys and ours gone: only the record is left; factory keys with ours
  still there: stop). The journal (AsyncStorage `prov.reset.v2`, no PINs or keys) is written just
  before the first DELETE or PUT KEY; the proved completion goes to an outbox
  (`prov.reset.outbox.v1`) and is retried until ops has it, so ops being unreachable never holds the
  operator up. Ops: `GET /registry/cards/:id/reset-check`; `POST /registry/cards/:id/reset` takes
  the completion `{short_id, chip, gp_key_version, manager_keys: 'factory', package: 'absent',
  verified_at}` from the card's owner only and, in one transaction, marks the old row reset, deletes
  the chip's `manager_keys` escrow row (the chip is released: the next operator to ask owns it and
  gets the same derived bytes) and stores a `card_resets` row (migration 7); a repeat answers the
  stored record and changes nothing. After a reset the old short id answers 410 to holder routes and
  till writes.
- [ ] **One tap on iPhone.** Measure install + key rotation + GEN_KEY ×8 + PIN on Max against the
  ~60 s reader session; split into two taps only if it does not fit.
- [ ] **Cards from before O2** (for example `15989f91`, no slot keys on record): mark them retired.

**Till**
- [ ] **PIN entry in the POS app** (`android-relay`, "Fedimint Card POS"): the holder types the
  PIN on the till's screen and the app sends it in `/v1/charge`; wrong-PIN tries left and a blocked
  card shown plainly. Today the PIN only reaches the till by hand.
- [ ] `duration_ms` is 0 in a charge reply.
- [ ] The till runs as a service (launchd) that restarts itself, with its token from the file, not
  a `nohup` line.
- [ ] Operator setup without `adb` tunnels where possible; drop `android-relay` from the operator
  path once the POS needs nothing from it (O5 row).

**Ops**
- [ ] **Restore.** A tested way to load a snapshot into a fresh registry object, drilled on staging
  from a real backup; operators recover with an `OPERATOR_SETUP` recover, tills get new tokens.
- [ ] **Backup monitoring.** Something outside Cloudflare reads `/registry/backup-status` daily and
  says so when it is 503.
- [ ] **Holder-route exposure** decided: `/slots` tells anyone whether a short id exists.

**Apps and release**
- [ ] **Signed release builds.** Android needs the upload key on this Mac; iOS a Release build that
  embeds its bundle (Max's Debug build needs Metro on the LAN).
- [ ] **Store forms.** App Store privacy and Play data safety name the card state ops holds.
- [ ] **Docs.** `PROVISIONER-PLAN.md` § 4 rewritten; `TILL-API.md` with the load routes; an operator
  runbook: till tokens, backup key custody, restore, reset.

**Acceptance run (release builds, production)**
- [ ] A blank card provisioned on Max with its own manager keys, loaded from a separate wallet,
  charged at the till with the PIN typed in the POS app, reset in the provisioner, provisioned again
  on the Pixel, and `gpp --list` with factory keys refused on it whenever it is provisioned (and
  answering, with nothing of ours listed, between the reset and the new install).

### O3 design (2026-11-04)

- **The intent carries the unsigned transaction.** A fedimint txid hashes inputs, outputs and the
  nonce, not the signatures (`fedimint-core` `Transaction::tx_hash_from_parts`), so the txid is
  fixed before the card signs. O2 asked for the signed bytes at intent time, which no till can
  have; the route now takes the unsigned encoding.
- **One more write: `submitted`.** After the card signs and before the till sends, the till posts
  the signed bytes (`POST /cards/{id}/submitted`). This is the sidecar's `submitted` flag moved to
  ops. A charge left open with no signed bytes never left its till, so the next till records it
  rejected. One with signed bytes may be in the federation, so the next till resubmits those bytes
  (idempotent) and records whatever the federation answers. A till never re-signs another till's
  charge: the holder at the second till did not agree to it.
- **What stays on the till.** The till's own outputs (`host_pending`: the merchant's note secrets)
  and its sales ledger stay in its data directory. Only the till that made a charge can collect its
  own output, as today. The card side (slots, notes, open charge) comes from ops on every tap and
  goes back through intent, submitted, outcome and slot renewals.
- **Holder PIN.** `/v1/charge` takes `pin`; `CARD_PIN` stays for the CLI and tests only.
- **Federation name.** A till reads `federation_name` from the federation's config and posts it
  (`POST /federations`), scoped to its operator; the provisioner shows it
  (`GET /registry/federations`) and falls back to "Federation 9dc8 86ab". The phone cannot ask the
  guardians itself: the invite's only guardian address is an iroh node.

## 8. Risks

- **Tills depend on ops.** If ops is down, no card can be charged. A cashucard till needs only
  its mint. This goes away only when notes live on the card (shape C, `PROVISIONER-PLAN.md`
  § 2).
- **Ops sees balances.** It holds every card's notes, so fedimintcard.org knows each card's
  balance and spend history by short id. It cannot spend them. This must appear in the privacy
  forms (O5).
- **One Durable Object.** Fine for a pilot. At scale, one object per card or per operator.
- **Ops is the only copy of card state.** Losing it strands every card's value: the card
  keeps its keys but no one knows its notes. Ops needs backups before O3 ships; cashucard's
  nightly encrypted R2 backup is the model. **Built 2026-11-04** (`registry/src/backup.ts`): a
  nightly snapshot at 03:17 UTC on the object's own alarm (the account's five free cron
  triggers are taken), age-encrypted to a recipient whose identity is kept off Cloudflare, in R2
  (`fedimintcard-registry-backups`, `-staging`). No password or token hashes, no sessions.
  `GET /registry/backup-status` is 503 before a first success or after 26 hours. Drilled on
  staging: `registry/scripts/backup-drill.mjs` fetched and decrypted one. Not built: a restore
  route; a restore today means a new object loaded from the snapshot by hand.
- **The card-manager key escrow** (O5). Ops holds every rotated card's ENC/MAC/DEK set, and the
  nightly backup carries it (still age-encrypted to the off-Cloudflare identity). Whoever holds
  the escrow, or a backup and its identity, or `CARD_MANAGER_MASTER`, can manage every card in it:
  delete the applet (and the card's keys with it) and install another. They cannot spend from a
  card: a note needs the card's slot key, which never leaves it, behind the holder's PIN. The
  master is a Worker secret with a copy kept offline; losing both master and escrow leaves every
  rotated card unmanageable, never unspendable.
- **iOS session length.** Install plus key setup must fit one reader session. O1 measures it.

## 9. Decisions (owner, 2026-11-04)

1. **Shape: accepted.** Ops on Cloudflare holds card state; no wallet is hosted.
2. **First value comes from the holder's wallet** (§ 6), not from the provisioner or a till.
3. **Per-card PINs as cashucard shows them** (§ 4): on the loop screen with the finished
   card until the next tap, nowhere else.
4. **The registry stores the decoded federation id**, not the invite text.
5. **Operators as cashucard has them**, in one shared deployment
   (cashucard `ops/src/db.ts:22-37`, `operator-setup.ts`, `pwhash.ts`, `auth.ts`):
   - an `operators` table with a unique name and a password hash (PBKDF2-SHA256 on Workers);
   - operators created only by a setup command injected at deploy (`bootstrap`, `recover`,
     with a nonce so a command applies once), never over HTTP;
   - 12-hour sessions stored as a hash, sign-out, revocation of an operator's sessions, and a
     login attempt budget;
   - the batch records the operator who created it.

   This replaces the single `OPERATOR_PASSWORD` secret and the stateless HMAC token, and with
   it the length leak in `passwordMatches` (`index.ts:264`). One addition beyond cashucard,
   because this ops holds balances and cashucard's does not: an operator's batch list and
   card lookups are limited to their own batches. cashucard records `operator_id` but does
   not filter on it (its `OPS-THREAT-MODEL.md:36` lists this as not implemented).
6. **The holder types the PIN at the till** (§ 6).

## 10. Non-goals

- Moving notes onto the card (shape C). That is the long-term answer to § 8's first two risks.
- A holder wallet app. This plan gives it the ops routes and a reference loader.
- Hosting any operator's or merchant's wallet.
