# Plan: offline card payments

Status: **accepted** (2026-11-07, owner decisions in § 8), against `main` @ `9fcb606`. The research and the choice
of design are in `OFFLINE-PAYMENTS.md`: the card signs each note once (option 1), with caps and
store-and-forward (option 3). The owner chose to stay as close to cashucard as possible and to
work F0 to F6 through without stopping.

## 1. Goal

A till with no internet for days keeps taking card payments, and settles them when it is next
online. A genuine card cannot pay twice with the same note; a broken or cloned card, or a lost
till, costs at most the caps. Online behaviour does not change.

## 2. What changes where

| Part | Today | After |
|---|---|---|
| Applet | `SIGN` signs any hash, any number of times | each slot signs one transaction hash, ever; note data, attestation, an offline limit with its own clock |
| Till | every charge needs the registry and the federation | offline charges verified locally, queued, sent later; caps per till |
| Registry | knows every charge before it happens | also accepts charges reported after the fact, and flags double-spends |
| POS | "Card service unreachable" | "Paid offline" with what is owed to sync |
| Setup app | installs applet 2.1 | installs 2.2 and certifies the card's attestation key |

## 3. The card (applet 2.2)

Patterns from cashucard's applet, which does this on the same J3R180 (`OFFLINE-PAYMENTS.md`).

1. **Sign once per slot.** `SIGN(slot, hash)`: a slot that has signed answers only for the same
   hash. Compute the signature in RAM, then commit in one `JCSystem` transaction the slot's hash,
   its spent flag and the offline counters, then answer. One slot per APDU keeps the commit small
   (cashucard measured about a dozen places per commit on this chip). A lost answer is recovered by
   asking for the same hash again. `GEN_KEY` overwrite clears the hash and re-keys in one
   transaction (it is not atomic today, `FedimintCardApplet.java:~511-539`).
2. **Note data on the card.** `SET_NOTE(slot, amount, federation signature)` stores 56 bytes beside
   the key at load, top-up and change; `GET_NOTES` returns them a page at a time. The card cannot
   check a BLS signature (no pairing); the till does.
3. **Attestation.** A P-256 key made on the card at install (native ECDSA, not our software
   Schnorr). At provisioning the Setup app reads its public key and the registry signs a
   certificate over (chip id, attestation key, the slot-0 key), stored on the card.
   `ATTEST(challenge)` answers a signature and the certificate. Tills pin the registry's
   certificate key.
4. **Offline limit with a forward-only clock.** Persistent: the day, the amount and count signed
   since the last online settlement, the limits. The day moves only with a valid Bitcoin block
   header (`SET_HEADER`, as cashucard). Without new headers the day does not move, so a long
   outage allows one day's limit in all, which is the cap we want offline. An online till resets
   the counters with an HMAC-authenticated `SETTLED` from the registry.
5. **Hardened signing** before any of this is relied on: no secret-dependent branches in
   `SchnorrHW`, the key never whole in RAM (re-randomised shares), blinding of `e·d`.

## 4. The till

1. **Know when it is offline.** Probe the registry and the federation (short timeouts) and report
   both in `/v1/status`. Today a charge waits 15 s for the registry before failing.
2. **Offline charge.** Verify the card (`ATTEST` against the pinned key), its notes (BLS against
   the federation's keys from the client config), that none of the notes is in the till's own
   spent list, and the till's caps. Plan with no change where the overpay allows (`plan.rs`,
   `MAX_TILL_OVERPAY`), plus a fee margin, since fees can change before the sync. The card signs;
   the till stores the signed transaction durably, adds its notes to its spent list, records the
   sale as `offline`, and answers.
3. **Change offline, as cashucard does it.** A change note cannot be issued without the federation,
   so the card keeps the change's *opening* until it can be finished. Cashucard's card makes its own
   change output and keeps the nonce and blinding factor (`SPEND_ALL_CHANGE`, `GET_CHANGE`); any
   phone that later has the mint's signature finishes the piece and loads it, and only the card's
   key can spend it. Here the card cannot blind (hashing to BLS12-381 G1 is out of its reach), so
   the till blinds and the card stores the opening beside the change slot's key: the amount and
   the blinding key (`SET_CHANGE`, `GET_CHANGE`). Any online till, at a later tap, fetches the
   federation's signature shares for that transaction, finishes the note with the opening and
   writes it with `SET_NOTE`. The offline limit counts what leaves the card: the inputs less the
   change.
4. **Sender.** A background loop submits queued transactions when the federation answers, then
   reports them to the registry. One spent input fails a whole transaction; the sale is marked
   `double_spent` and the loss counted.
5. **Caps per till.** Total unsettled offline value, and count; past either the till refuses
   offline charges.
6. **Backup.** The queue is the merchant's money until it is sent: keep it beside the wallet and in
   the StartOS backup.

## 5. The registry

- `POST /cards/:id/offline` from a till: the signed transaction and its inputs, after the fact.
  The registry marks the notes spent, or answers which notes another till already reported, so a
  double-spend is visible to the operator even before the federation refuses it.
- `SETTLED` for the card: once a card's offline spends are all reported, an online till may reset
  its counters (a MAC under a per-card key derived from a registry secret, like `STATE_KEY`).
- Certificate signing at provisioning, with its key in a Worker secret.

## 6. POS and Setup apps

- POS: "Paid offline" as its own result; the till's offline status and how much is waiting to sync.
- Setup app: the 2.2 load file; read the attestation key, get the certificate, write it; the
  offline limit as a batch setting.

## 7. Phases

| Phase | Work | Done when |
|---|---|---|
| **F0** | Measure on the J3R180: commit capacity, native P-256 ECDSA time, the split of SIGN's 847 ms | numbers in `CAP-BUILD.md` |
| **F1** | Harden `SchnorrHW`; atomic GEN_KEY overwrite | jCardSim tests, and SIGN timing on silicon |
| **F2** | Applet 2.2: sign once, note data, attestation, limit and clock | jCardSim tests incl. tearing at every APDU; one card on silicon |
| **F3** | Till: status probes, fail fast, offline verify, queue, sender, caps | federation tests with the federation stopped and restarted |
| **F4** | Registry: after-the-fact reports, double-spend flags, `SETTLED`, certificates | vitest |
| **F5** | Setup and POS apps | on the Pixel and Max |
| **F6** | A village drill: a till offline for a day, two cards, a deliberate double-spend, then a sync | the double-spend is caught and its loss is within the caps |

F0 and F1 are worth doing even if the rest waits: they harden what is in production now.

## 8. Decisions (owner, 2026-11-07)

1. **Clock: Bitcoin block headers**, as cashucard (`SET_HEADER`).
2. **Change offline: as cashucard** (§ 4.3): allowed; the card keeps the opening; the limit counts
   inputs less change.
3. **Reuse: port from cashucard with credit** in `NOTICE` (MIT, as `SchnorrHW` and cardkit already
   are).
4. **Order: F0 to F6 straight through**, stopping only when stuck.
5. **The POS phone as an offline till** (owner, 2026-11-09, final): a phone with no network of its
   own takes a 3.0 card offline ("Paid offline") and the till settles it later; online, the hosted
   till charges exactly as before. One Rust core (`host/pos-core`) for the till and both apps; the
   phone holds an offline kit whose pool of till outputs keeps their secrets on the till; two till
   routes take kits out and charges in. Slices ENG-842 to ENG-845; `docs/POS-OFFLINE.md`. Drilled
   on production on 2026-11-13 (§ 10, third e2e run).

Still open, defaults used until the owner says otherwise: the limits (a per-card daily offline
limit set per batch in the Setup app, default none (owner, 2026-11-07: set one for village batches); a till's unsettled limit, default
100,000 sat); applet 2.2 needs re-provisioning, so cards in the field stay online-only until they
are reset and set up again.

## 9. Non-goals

- Paying a merchant with eCash the till received offline; topping up a card offline. Both need
  the federation.
- Co-signed notes (option 2). Reconsider if card extraction becomes a real threat.
- Tills gossiping spent notes over a local network; a later improvement.

## 10. As built and drilled (2026-11-07)

F0–F5 are on `main` (applet 3.0 `332a45f`, registry `3cee827`/`f65e7c0`, till `00c7bf4`/`ede3c2c`,
apps `bb62673`ff.). F6 ran on staging with a real J3R180 (`908901dc`), the Pixel and a laptop till:

1. Setup app on the Test registry: applet 3.0, keys 0–7, a 20,000 sat daily limit, attestation
   key, a 130-byte certificate from the registry, then the PIN. (First found: the app sent the
   keys in upper-case hex and the registry refused them; fixed in `4ec5b25`.)
2. `card load 30000000` from the staging till: 10 notes, each written to the card (SET_NOTE).
3. Online charge 50 sat: paid; change to three spare slots.
4. Till restarted with the registry unreachable: 30 sat → "Paid offline" (change to a CHANGE
   slot); 10 sat refused (no spare slot for change, no exact plan); 16 sat → "Paid offline"
   (one note, no change).
5. Till back online: the sender reported both charges, the federation took them, the queue went
   to 0 with no double spend or rejection, the till's wallet rose by the 47.488 sat it was owed.
6. A balance tap online finished the change: the CHANGE slot became a NOTE with its signature.
   The card's clock had moved to the tip header; its day's count read 121.2 sat (gross inputs).

A double spend cannot be staged with a genuine card: it refuses a second transaction per note.
Double spends from a copied card are covered by `host/sidecar/tests/offline.rs` and the registry
tests.

### Follow-ups found

- **Release for rejected charges.** A charge the federation rejects (e.g. a fee change) leaves
  its inputs SPENT on the card and its change slots CHANGE for good: the inputs' value is
  stranded. Proposed: the registry signs a release (card, slot, txid) with its certificate key;
  the card checks it (native P-256, about 75 ms) and returns the slot to NOTE or KEYED.
- **Fee margin** on offline charges, so a fee change before the sync does not reject them.
  *Built:* on a federation at core consensus 2.1 or later (the till reads it from the client
  config, logs it at startup and shows it as `offline.fee_margin` in `/v1/status`), an offline
  charge's inputs also pay a surplus the federation burns: the fees the transaction would owe
  under a headroom schedule (default fedimint's default mint fees, 100 msat per input and output,
  0 ppm; `--offline-fee-base-msat`, `--offline-fee-headroom-ppm`) less its fees now, floored at 0
  (`host/sidecar/src/plan.rs`, `solve_with_margin`). The change or the till's overpay pays it; the
  till always gets at least the price. (First built with the change alone paying when there was
  change; in the second e2e run every margin was dropped, since a power-of-two change note cannot
  be trimmed by a few hundred msat without more notes.) It covers a rise of the federation's fees up to the headroom, e.g.
  a zero-fee federation turning on the default base fee. It does not cover a rise beyond the
  headroom; a charge where neither the change nor the overpay can pay the margin without another
  change note, slot or input (it goes exact rather than being refused, `fee_margin_msat: 0`); or
  a federation older than 2.1, which refuses any surplus: there a fee change before the sync
  still rejects the charge and the card's notes are released (`OFFLINE-PROTOCOL.md` § 6). Online
  charges stay exact. In fedimint v0.13 a mint's fees are fixed in the federation's consensus
  config, so the in-process test federation cannot change them mid-test: the tests show a charge
  with a margin accepted on a zero-fee federation and the margin covering the default fees, not
  a live fee change.
- **Spare slots offline.** A fresh card ran out of keyed spare slots after two charges with change.
  Online tills should key more ahead; an offline till may re-key the slots its own queued charges
  spent. *Built (online half):* every online tap keeps a 3.0 card at 12 fresh keyed slots (by
  default), at most 2 keys a tap. Settled SPENT slots are renewed before never-keyed slots are
  keyed, but renewing takes the holder's PIN, so only a charge renews: a balance tap keys
  never-keyed slots only (`docs/TILL-API.md`, "Spare slots for offline change"). Re-keying
  offline is still open.
- **Messages.** The POS says "card needs a top up" for "no slot for change"; the Setup app says
  "the card left the phone" for a registry refusal. *Built (POS half):* the till refuses with
  `offline_no_change_room` and a price the card can pay; both POS apps show "This card can't take
  change while offline. Try N sats". The Setup app's message is still open.
- **The daily limit counts online charges too, by gross input** (as cashucard's): with notes up
  to 16,777 sat, one charge can use most of a 20,000 sat day. Owner decision pending.
- **A note pending under another till's unresolved online charge** makes an offline report a
  `double_spend` even if that charge later fails.
- **Production**: registry v11 needs its own `CERT_SIGNING_KEY` (set by the owner), then the
  registry, the zion till package and the apps. *Done:* `ROLLOUT-OFFLINE.md`, which reached step 6
  on 2026-11-07.

### Second e2e run (2026-11-07, staging)

Card `4f5e6da3` (the wiped chip `…63205547`), set up by the Setup app with no daily limit, the
registry key and a certificate; 30,000 sat loaded (10 notes). Online: a 50 sat charge and five
balance taps brought the spare reserve to 12 fresh slots. Offline (registry unreachable): 30, 10
and 7 sat, all "Paid offline" (10 sat had been refused in the first drill). Back online: the queue
of 3 settled with no double spend or rejection, the till's wallet rose by exactly the 49.152 sat it
was owed, and a balance tap finished both change slots. Found: every fee margin was 0 (fixed, as
above).

### Third e2e run (2026-11-13, prod on zion)

The POS phones as offline tills (§ 8, decision 5), on production: zion's till `0.1.0:4` (built
from `a630a22`; caps of 200 payments and 100,000 sat), the owner's card `a5c816c9` (1,933,312 msat
live), the Max (iOS POS 0.1.0 (3)) and the Pixel (Android POS 0.3.0 (2), then 0.4.0 (3) from the
PR #26 branch). The staging rig was ready but both staging cards were empty, so the owner moved the
drills to zion. Sales of 16 sat, or 8 where 16 was refused:

- (a) One sale per phone in airplane mode. Back online, each synced, the Max within seconds and the
  Pixel in about a minute, and the registry recorded both, the Max's under its receipt's txid.
- (b) Both phones offline on one card: two sales, two txids, no double spend; the till's balance
  rose by the 24,576 msat the card's live total fell by.
- (c) Refused offline with nothing signed: 16 sat with too few fresh slots for its change ("Try 8
  sats"), 100,001 sat over the value cap, and 8 sat with the phone's clock 8 days ahead (kit
  expired). The kit renewed online.
- (d) Three offline sales (48 sat) on the Pixel, then its data cleared: the till and the registry
  never saw them. The merchant lost the 48 sat. The three notes they spent on the card (65,536 msat)
  stay live on the registry: the card's balance counts them, and an online sale that picks them
  fails. The caps bound only the merchant's loss: a lost till or phone also strands, on the
  holders' cards, the inputs of its unsynced sales (`POS-OFFLINE.md`, risk 6); the "at most the
  caps" of § 1 is the merchant's bound.

The count cap (200) and a double spend were not staged. Found: no Android phone could open its
offline store (fixed, PR #26); ENG-864, offline runs of 5 to 10 sales on one card (fresh keyed
slots, change waiting for an online tap, the note mix); ENG-865, the copy for a price over the cap;
ENG-866, the card's id in the apps, and the Pixel's slower sync; ENG-867, notes stranded by a wiped
phone, which online taps now hold back until the registry releases them, 37 days on (`TILL-API.md`,
"Stranded slots"). Details: `POS-OFFLINE.md` § Drill log.
