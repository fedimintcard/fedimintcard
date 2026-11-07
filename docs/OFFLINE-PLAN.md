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
- **Spare slots offline.** A fresh card ran out of keyed spare slots after two charges with change.
  Online tills should key more ahead; an offline till may re-key the slots its own queued charges
  spent. *Built (online half):* every online tap keeps a 3.0 card at 6 fresh keyed slots, at most 2
  keys a tap, settled SPENT slots renewed before new slots are keyed (`docs/TILL-API.md`, "Spare
  slots for offline change"). Re-keying offline is still open.
- **Messages.** The POS says "card needs a top up" for "no slot for change"; the Setup app says
  "the card left the phone" for a registry refusal. *Built (POS half):* the till refuses with
  `offline_no_change_room` and a price the card can pay; both POS apps show "This card can't take
  change while offline. Try N sats". The Setup app's message is still open.
- **The daily limit counts online charges too, by gross input** (as cashucard's): with notes up
  to 16,777 sat, one charge can use most of a 20,000 sat day. Owner decision pending.
- **A note pending under another till's unresolved online charge** makes an offline report a
  `double_spend` even if that charge later fails.
- **Production**: registry v11 needs its own `CERT_SIGNING_KEY` (set by the owner), then the
  registry, the zion till package and the apps.
