# Plan: a hosted ops service, and no laptop in the operator's loop

Status: **proposed** (2026-11-04), against `main` @ `ea7c12f`. Nothing here is built.
It changes § 4 of `PROVISIONER-PLAN.md` ("hosted registry, local wallet"). Where the two
disagree, this plan is the newer intent once the owner accepts it (§ 9).

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

The per-card PIN question in `DEMO-HANDOFF.md` stays open (§ 9, decision 3). Moving the
PIN to the phone does not answer it.

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

- Till tokens are stored and revocable. Operator tokens stay as they are.
- `passwordMatches` (`index.ts:264`) says it hashes both sides, but compares the hex of the
  raw bytes, so the length leaks. Hash with SHA-256 first.
- `batches.federation_id` takes the decoded federation id (`decodeInvite` in the app), not the
  whole invite. That is decision 4 in § 9, already flagged in `DEMO-HANDOFF.md`.

## 6. The till, and where value comes from

**Till.** The sidecar's till keeps its federation client, its `sales.json` and its output
secrets. Its card state comes from ops: a `StateStore` backend over HTTPS replaces the file,
keyed by short id, which the till reads from the card (`GET_PUB` slot 0). The charge logic in
`glue.rs` stays. Only where it loads and saves changes. The till can then charge any card the
operator provisioned.

**Value.** The provisioner stops loading value; install-only is already the default
(`7254d43`). cashucard does the same. The first load moves to something that already has a
funded federation client:

- **At a till (recommended).** A "load card" action on the till, reusing `issue`
  (`cards.rs`, `CardWallet::topup`) against ops state. The merchant takes cash or Lightning
  and loads the card from the till's own wallet, as with a gift card.
- **From a holder's wallet.** Later, a Fedimint wallet app that writes to ops. Not in scope.

This removes the top-up quick picks and the `Top-up` section that `ea7c12f` added to the
provisioner.

## 7. Phases

| Phase | Work | Done when |
|---|---|---|
| **O0** | Owner decisions in § 9 | answered here |
| **O1** | Phone key setup and PIN (§ 4) in cardkit and `LoopScreen`; registry rule for short id; `slot_pubkeys` on the card row; remove `issue`, `LocalClient` and the top-up UI | jest green with a fake card transceiver; **silicon**: one blank J3R180 finished on the Pixel and one on Max, with no Mac running, `GET_PUB` ×8 matching the registry row, `SIGN` refused before `VERIFY_PIN` (`6A94`) and accepted after |
| **O2** | Ops tables and routes (§ 5); till tokens; password hash fix; decoded federation id | vitest for each route, including 409 on a stale version and on a changed row; deployed to a staging Worker first |
| **O3** | Till on ops: an HTTPS `StateStore` in the sidecar, keyed by short id; recovery of an open intent | the sidecar's existing charge tests pass against a local `wrangler dev`; a charge killed between `SIGN` and outcome is recovered by a second till; two cards charged on one till |
| **O4** | Load at a till (§ 6) | a card provisioned on Max, loaded at a till, charged at another till |
| **O5** | Docs and store forms | `PROVISIONER-PLAN.md` § 4 rewritten; App Store privacy and Play data-safety forms name the card state ops holds; `android-relay` dropped from the operator setup |

O1 stands alone and removes the laptop from provisioning. O2 and O3 move the till.

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
  nightly encrypted R2 backup is the model.
- **iOS session length.** Install plus key setup must fit one reader session. O1 measures it.

## 9. Decisions for the owner

1. **Accept this shape?** Ops on Cloudflare holds card state; no wallet is hosted.
2. **Where first value comes from:** load at a till (recommended), or a holder wallet.
3. **Per-card PINs:** today the summary masks the generated PIN and keeps only the last one,
   so holders cannot learn it. Decide when a PIN may be shown, or drop per-card PINs.
4. **Federation id in the registry:** send the decoded id (recommended).
5. **Tenancy:** fedimintcard.org has one operator today. Serving several operators or
   federations needs operator accounts and per-operator data. Alternatively, each operator
   deploys the same Worker on their own Cloudflare account.
6. **Holder PIN at the till:** the till reads the PIN from `CARD_PIN` in its environment
   (`host/card/src/lib.rs`). A real till asks the holder. This is outside this plan, but it
   blocks a real charge.

## 10. Non-goals

- Moving notes onto the card (shape C). That is the long-term answer to § 8's first two risks.
- A holder wallet app.
- Hosting any operator's or merchant's wallet.
