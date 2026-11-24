# Offline payments: the contract between card, registry, till and apps

Status: 2026-11-07, for `OFFLINE-PLAN.md` F3–F5. Applet 4.0 is built (branch `card-4.0`); its commands are
in `FedimintCardApplet.java`'s class comment and are the source of truth for § 1. § 7 (stranded
slots, ENG-867) added 2026-11-14.

## 1. Applet 4.0 (summary)

SELECT answers `04 00`. A host reads the version first and sends no PIN command to a card whose
major version is below 4. Slots: 32.

| INS | Command | Data | Answer / rule |
|---|---|---|---|
| 12 | SIGN P1=slot | hash (32) | Only a NOTE slot (counted against the day's limit, then SPENT), or a SPENT slot for the same hash. 6985 otherwise, 6A95 over the limit, 6A94 without a PIN session. 6F00 when the card's own check of the signature fails: the slot stays NOTE |
| 14 | SET_NOTE P1=slot | amount msat (8, BE) ‖ federation signature (48) ‖ registry signature (DER, 8..72) | Lc 64..128, else 6700. KEYED → NOTE; CHANGE → NOTE with the same amount (6A80 if not). 6985 with no registry key or in another state; 6982 for a signature that does not verify, with nothing written. No PIN |
| 15 | GET_SLOT P1=slot | – | state (1) ‖ x-only key (32) ‖ amount+signature (56) ‖ spent hash (32) ‖ opening (66) = 187 |
| 16 | GET_STATE | – | 32 states ‖ now (4) ‖ window (4) ‖ limit (8) ‖ spent in window (8) ‖ cert len (1) ‖ attest key made (1) ‖ registry key set (1, byte 58) ‖ PIN state (1, byte 59) ‖ wrong PINs in a row (1, byte 60) ‖ askings owed (2, bytes 61..62) ‖ anchored in this selection (1, byte 63) ‖ signer bits (1, byte 64: bit 0 chip RSA path, bit 1 nonce key in RAM) = 65 |
| 17 | SET_CHANGE P1=slot | amount (8) ‖ txid (32) ‖ out_idx (2, BE) ‖ blinding key (32) | KEYED → CHANGE. Once a PIN is set, only in a PIN session (6A94 otherwise) |
| 18 | SET_LIMIT | limit msat (8) | only before SET_PIN; 0 = none |
| 19 | SET_HEADER | Bitcoin header (80) | → now (4, BE). 6A93 for a header without enough work. The first header of a set-up selection, before any PIN and while no slot holds value, is the anchor: the card's clock takes its time |
| 30 | ATTEST_INIT | – | → P-256 public key, 65 bytes uncompressed. Before SET_PIN and SET_CERT only |
| 31 | SET_CERT | certificate (≤ 160) | once, before SET_PIN |
| 32 | GET_CERT | – | the certificate |
| 33 | ATTEST | challenge (32) | → ECDSA-SHA256, DER, over `"fedimintcard/at1"` (16 ASCII) ‖ challenge ‖ slot 0's x-only key |
| 40 | VERIFY_PIN | PIN (4..8) | 9000 opens the PIN session; 63CX a wrong PIN, X tries left; 6A98 while a wait is owed: send the same command again (see the wait below); 6984 no PIN set. The card never answers 6983 |
| 41 | SET_PIN | PIN (4..8) | once. 6A96 until the anchor is set in this selection; 6985 if a PIN is set, there is no registry key, or a slot holds value. Opens the PIN session |
| 42 | CHANGE_PIN | P1 = old PIN length (4..8; 0 = equal halves) ‖ old PIN ‖ new PIN (new 4..8, Lc ≤ 16) | the old PIN is checked as VERIFY_PIN checks it, wrong tries counted with their wait; the session ends. 6700 for a new PIN outside 4..8, 6B00 for a bad P1 or P2 |

States: EMPTY 0, KEYED 1, NOTE 2, CHANGE 3, SPENT 4. GEN_KEY P2=1 re-keys only KEYED or SPENT, in a
PIN session once a PIN is set.

**Signed notes.** The registry signs each note it issues: ECDSA P-256, SHA-256, by `CERT_SIGNING_KEY`
(the key SET_REGISTRY_KEY holds), over `"fedimintcard/note1"` (18 ASCII) ‖ the x-only key of the
slot (32) ‖ the note's 56 bytes (amount ‖ federation signature). The card checks it before it writes
the note. A host gets the signature as `notes[].reg_sig` (DER hex) from the registry; a note with no
`reg_sig` yet waits for a later tap, and the sync with the registry runs before the card write.

**Order of set-up, in one selection.** SET_REGISTRY_KEY, SET_HEADER (a header from the registry's
`GET /v1/chain/tip`; it answers its time), GET_STATE byte 63 = 1, then SET_PIN. Without the anchor
SET_PIN is 6A96; a header without enough work is 6A93.

**The wait.** The card counts a wrong PIN before it compares. After the n-th wrong PIN in a row the
next VERIFY_PIN answers 6A98 for 2^(n-1) askings (at most 16,384, from the 15th), each asking doing
fixed work on the card (about 600 ms on a J3R180), and the asking after them is the try. A host sends
the same VERIFY_PIN again on 6A98. The owed askings survive a lifted card and a power cycle; a right
PIN clears them. The card never blocks.

## 2. The certificate

130 bytes: `01` ‖ slot 0's x-only key (32) ‖ attestation key, SEC1 compressed (33) ‖ the registry's
signature, raw r ‖ s (64). The signature is ECDSA P-256 with SHA-256 over
`"fedimintcard/cert1"` (18 ASCII) ‖ slot 0's x-only key ‖ the compressed attestation key.

A till verifies a card offline by: GET_CERT; check the registry signature with the pinned
certificate key (§ 3); check the certificate's slot-0 key equals GET_PUB 0 (and so the short id);
ATTEST with a fresh random challenge; check that signature with the certificate's attestation key.

## 3. Registry (F4)

- `GET /registry/cert-key` — public: `{ok, key}`, the certificate key's public point, 65 bytes
  uncompressed hex. A till fetches it when it connects and keeps it in `till-ops.json`
  (`cert_key`); offline it uses the kept copy.
- `POST /registry/cards/:short_id/attestation` — operator session: `{attest_pub, slot0}` (65 bytes
  hex, uncompressed; slot 0's x-only key, 32 bytes hex) → `{ok, cert}` (130 bytes hex, § 2). The
  install tap asks before the Setup app has registered the card, so for an unregistered short id
  the registry certifies the `slot0` named (whose last 8 hex digits must be the short id) and holds
  the certificate until `POST /registry/cards` stores a card with that slot 0, then moves it onto
  the card. For a registered card `slot0`, if sent, must match the one on record.
  Stores `attest_pub` on the card; the same key again answers the same certificate; a different
  key is 409 while the card has live or pending notes.
- `POST /cards/:short_id/offline-charges` — till token: an offline charge reported after the
  fact. Body: the intent's fields (`txid`, `client_ref`, `tx_hex`, `inputs`, `change`, as
  `/intent`) plus `signed_hex` and `signed_at` (ms). Unlike `/intent` there is no version check
  and an open charge from another till does not block it: the card signed offline, so the only
  question is whether its inputs are still live. All live → recorded as an open charge with its
  signed bytes (as `/intent` then `/submitted`), inputs pending, change pending, version bumped:
  `200 {ok, version}`. Any input not live → `409 {ok:false, reason:'double_spend', conflicts:
  [{nonce, status, txid}]}`, nothing recorded but the report. The same txid again answers as
  before. The till then sends the bytes to the federation and posts `/outcome` as for any charge.
- `GET /registry/offline-reports?short_id=` — operator: every report, with its result.
- Secret `CERT_SIGNING_KEY`: the P-256 private key, PKCS#8 DER base64. Migration: `cards.attest_pub`,
  table `offline_reports (txid, short_id, till_id, result, detail, created_at)`.
- As built (registry/src/offline.ts, migration v10): `cards.attest_cert` also stores the issued
  certificate, since ECDSA signatures are randomized and the same key must answer the same bytes;
  `offline_reports` is keyed `(txid, till_id)`, and `detail` is JSON (`client_ref`, `signed_at`,
  `inputs`, and `conflicts` for a double spend). Both are in the nightly backup. Without
  `CERT_SIGNING_KEY`, `/registry/cert-key` and the attestation route answer 503. An offline charge
  does not repeat `/intent`'s change-slot checks (the card chose its slots offline); it refuses only
  a change nonce that already carried a note. That answers as a double spend (409, conflicts with
  status `change_used`) and releases the charge's change slots, so the card can write the note that
  slot waits for; the registry's notes are unchanged, and the inputs the card spent come back through
  a stranding, since such a refusal does not count as the registry having seen the transaction. A
  replay of a double spend answers the stored conflicts. `GET /registry/offline-reports` lists at most 200, newest first, only for the
  operator's own cards.

## 4. Till (F3)

1. **Card layer.** Commands of § 1 on the `Card` trait and in `SoftCard` (which follows 3.0 exactly).
2. **Online with a 3.0 card.** Before SIGN, every input slot must hold its note: GET_SLOT, and a
   KEYED or CHANGE slot gets SET_NOTE from the registry's note (amount, signature). After change or
   a load is issued in a tap, SET_NOTE it in the same tap; otherwise the next tap's attach does it.
3. **Clock.** The till keeps the newest block header (an Esplora URL, default
   `https://mempool.space/api`, every 10 minutes, cached in the data dir) and sends SET_HEADER when
   the card's `now` is older.
4. **Offline charge.** When the registry or the federation does not answer within 2 s: a 3.0 card
   only; attestation (§ 2) against the kept certificate key; each note verified (federation BLS
   signature over the note's nonce `02 ‖ x-only key`, against the denomination's key from the
   client config); no note in the till's own spent list; the till's caps (unsettled total and
   count). Plan from the card's own slots (GET_SLOT), change to KEYED slots with SET_CHANGE before
   SIGN. Queue the signed transaction durably, add its nonces to the spent list, record the sale as
   `offline`, answer the POS.
5. **Sender.** While online, every queued charge goes to `offline-charges`, then to the federation,
   then `/outcome`. A 409 marks the sale `double_spent`, a loss, kept for the operator.
6. **Status.** `/v1/status` adds `registry_reachable`, `federation_reachable` and
   `offline: {queued, queued_msat, cap_msat, cap_count}`.

The charge answer gains `"offline": true` for a charge taken offline (HTTP 200, as a paid charge).

As built (host/sidecar `offline.rs`, `clock.rs`; docs/TILL-API.md): offline charges need a till on
ops (the certificate key and the report route are the registry's). `offline` in `/v1/status` also
counts `double_spent` and `refused` (reports the registry refused for another reason). The queue is
`<data_dir>/offline_queue.json`; it is written before `SET_CHANGE`, with every secret, so an entry
cut off before the card's answer is finished from the card's stored signatures at its next tap,
offline or a `POST /v1/charge` (§ 7). Change goes only to KEYED slots from 24 to 31 whose current
key has never carried a note, by the till's copy of the card (§ 8).
Online, a 3.0 card's `SIGN` counts against its daily limit too, so the till checks the limit
(`GET_STATE`) before an intent and answers `card_limit` instead of leaving a half-signed charge.

## 5. Apps (F5)

- **Setup app**: installs 3.0 (cardkit's load file regenerated); after the keys and before the PIN:
  SET_LIMIT (a batch setting, default 0 = none; set for village batches), ATTEST_INIT, `POST …/attestation`, SET_CERT.
- **POS**: a charge answered with `offline: true` shows "Paid offline"; the till's status shows
  what is waiting to sync.

## 6. Releases (follow-up 1 of `OFFLINE-PLAN.md` § 10)

A charge the federation refuses, or an offline report the registry refuses as a double spend,
leaves the card holding state for a transaction that will never be accepted: its inputs SPENT
(the card signs one transaction per note, so their value is stranded) and its change slots
CHANGE. The registry, which learns of the refusal, signs a release per slot; the card checks it
and undoes exactly that.

- **Card.** `SET_REGISTRY_KEY` (INS 34, 65 bytes, the registry's certificate key from
  `GET /registry/cert-key`, uncompressed; only before SET_PIN). `RELEASE` (INS 35, P1 = slot,
  data = txid (32) ‖ DER ECDSA-SHA256 signature): a SPENT slot whose spent hash is txid → NOTE;
  a CHANGE slot whose opening names txid → KEYED; answers the new state (1). 6982 for a bad
  signature, 6985 for a slot the release does not fit or a card with no registry key.
  GET_STATE is 65 bytes (§ 1): byte 58 is 1 once the registry key is set.
- **Signed message.** ECDSA P-256, SHA-256, by `CERT_SIGNING_KEY`, over `"fedimintcard/rel1"`
  (17 ASCII) ‖ slot 0's x-only key (32) ‖ slot (1) ‖ txid (32). The card wants DER; WebCrypto
  gives r ‖ s, so the registry converts.
- **Registry.** When a charge's outcome is `rejected` (online or offline), a release for every
  input's slot and every change slot of that transaction; when an offline report is refused as a
  double spend, a release for every change slot it named (its inputs were spent elsewhere and
  stay spent). Kept in a `releases` table and listed in the card's state (`GET /cards/:id/state`,
  `releases: [{slot, txid, sig}]`, sig as DER hex) until the slot is keyed anew or the card reset.
- **Till.** At an online attach of a 3.0 card, each listed release whose slot still fits
  (GET_SLOT: SPENT with that spent hash, or CHANGE with that txid) is sent as RELEASE. A
  released input is a live note again (the registry restored it when it recorded the
  rejection); a released change slot is a free keyed slot.
- **Setup app.** SET_REGISTRY_KEY in the install tap before SET_PIN, with the key fetched from
  `GET /registry/cert-key`; GET_STATE byte 58 tells a resumed tap whether it is done.

As built (registry migration v12, `offline.ts` `recordReleases`; till `ops.rs` `card_releases`):
a rejected load (no inputs) gets no release, since a load never moves a slot's state. Releases are
signed after the transaction that recorded the refusal and written in one that checks it still
stands and that slot 0 is unchanged; without `CERT_SIGNING_KEY` none is written and the outcome
or refusal stands. Releases are in the nightly backup. A rejected offline charge's inputs, pending
under its txid since the report, are live again (before this they stayed pending). A replayed
outcome or double-spend report signs nothing new. The till's offline planner no longer puts change
in a KEYED slot whose key carried a note of any status: a rejected online charge's change slot
stays KEYED under its dead note's key until renewed, and the registry refuses that key as change.

## 7. Stranded slots: held, then released (ENG-867)

A sale a card signed offline on a POS phone that never synced it (its data cleared, the phone lost)
leaves the card SPENT on the sale's input slots and CHANGE on its change slots, for a transaction
the registry never saw. The registry still lists those notes live and those change keys unused, so
the card's balance counts value the card cannot pay, and a till plans sales the card refuses
(`POS-OFFLINE.md` risk 6). The till holds such slots out of its plans and reports them; the
registry records them and, once the phone can no longer report the sale, signs § 6 releases. A
release gives the value back to the holder; the merchant keeps the loss of the sale, which the
phone's caps bound.

- **What the till reports, and when.** At an online tap of a 3.0 card, a till on ops reads
  GET_STATE and holds, for the job, each slot it cannot account for: a SPENT slot whose note is
  live and is not the input of a charge it knows, and a CHANGE slot whose current key carried no
  note. GET_SLOT gives the slot's transaction (its spent hash, or its opening's txid); a slot whose
  key is not the registry's, or whose transaction the till knows (a charge it holds or the
  registry lists open, a live or pending note's txid, its own offline queue), is not held, and a
  slot it could not read is held anyway. A charge the queue holds `signing` is settled before this
  check at a `POST /v1/charge` tap, which carries the PIN: finished, dropped or set aside as at an
  offline tap (`TILL-API.md`). A balance, top-up or issue tap sends no PIN, and a pay's charge is
  taken online only; each leaves it, so at such a tap it still explains its slots. A slot the
  registry lists in `stranded` is held as listed. A held slot is out of the planner and out of the
  payable balance: `live_msat` counts payable notes only, with `stranded_msat` and
  `stranded_until_ms` beside it. After the job the till posts its holds when one is not listed or
  is listed `due`. For a due spent slot it first asks the federation, under one short timeout,
  whether the note's nonce is spent: unspent sends `unspent: true`, spent sends `spent: true`, no
  answer sends no flag and is asked again at the next tap. Reporting never fails the tap; releases
  arrive in the card's state and are sent at its next online tap (§ 6).
- **Route.** `POST /cards/:short_id/stranded`, till token (403 for an operator):
  `{slots: [{slot, kind, txid, unspent?, spent?}]}`, 1 to 32 slots, each named once; `kind` is
  `spent` or `change`, `txid` 32 bytes hex, the flags booleans (null reads as absent), meaningful on
  a spent slot only, and never both `true` (400). It answers `200 {ok, held, released, spent,
  ignored}`, slot lists: the report's slots held after it, the slots it released (any of the
  card's), the report's slots marked spent, and the report's slots not taken.
- **What the registry records.** A row `(short_id, slot, txid, kind, first_seen, till_id)` per
  slot, `first_seen` by the registry's clock; a repeat changes nothing. A slot is not taken when
  the registry knows its transaction (a charge on record, or an offline report for the card other
  than the gate's refusal below), when the slot is released for it already or held for another
  transaction, or when the slot has no key; a spent slot needs a live note under its current key,
  and a change slot a key that never carried a note. The note stays live (the status CHECK is
  unchanged, so a till that knows nothing of holds still reads the state) and no version is
  bumped. `GET /cards/:id/state` lists the open rows as
  `stranded: [{slot, kind, txid, amount_msat, since, release_after, due}]`: the live note's amount
  for a spent slot, null for a change slot; `release_after` is `since` plus the grace, and `due`
  is true once the registry's clock reaches it, but never on a change slot of a transaction found
  landed (below): nothing releases it, so a till has nothing to post for it. A row is open until
  the registry knows its transaction or releases its slot; renewing or keying its slot
  (`PUT /slots/:n`, `POST /slot-keys`) or resetting the card deletes it. While a change slot is
  held, `/intent` refuses change into it, `/load` refuses it and `GET /slots` leaves it out: a
  note there would take the key the late change needs.
- **Grace: 37 days** from the registry's first sight (`STRANDED_GRACE_MS`). A phone's kit lasts 7
  days and the till redeems its sales for 30 days after (`KitConfig`, `host/sidecar/src/pool.rs`),
  so a sale its phone can still sync is reported within the grace. Until then a late report of
  the transaction records as any other (§ 3), and its rows drop out on their own. The same bound
  applies to a till's own offline queue.
- **Release rules.** After recording, each report releases the card's open rows past the grace,
  per transaction, through § 6's signing, written only if the transaction is still unseen, has
  not landed, and every slot is still held when the signing ends:
  - a spent slot only on its own `unspent: true` in that report;
  - a change slot when a spent slot of its transaction is `unspent: true` in that report: one
    unspent input proves the transaction never landed;
  - a change slot whose transaction has no spent row, with no flag: no slot signed it (the card
    was pulled between SET_CHANGE and SIGN, `POS-OFFLINE.md` risk 5), so nothing can land and the
    federation is not asked;
  - a change slot whose transaction is released already: a spent slot of it was, at an earlier
    report, and the gate below refuses it for good.

  The released note pays again and the released change slot takes change again.
- **Spent at the federation.** `spent: true` on a spent slot the registry takes marks its note
  spent under the transaction's txid and deletes the row, at once, signing no release: the slot
  renews at the next PIN charge like any spent slot, and the till marks its own note spent. The
  transaction landed, so its change is real: a change slot of a transaction with a note spent
  under it is never released by the rules above and stays held, and is never `due`. A spent slot
  of the transaction still held is due after its grace like any other: a till asks the federation
  again, and the `spent: true` it gets ends that row the same way. A spent note's txid can name
  such a landed transaction, so a till does not count it as known.
- **The money gate.** `POST /offline-charges` refuses a transaction the card has a release for,
  after its replay checks: `409 double_spend` with every input as `{nonce, status: 'released',
  txid}`, kept as a `double_spend` report, nothing new signed; a repeat answers the same. A
  release is the registry's word that the transaction will never be accepted, and the card may
  already pay with the released note. At that point a release can only be a stranding's (a charge
  on record and a refused report are answered before it, and neither row is ever deleted), so the
  gate reads `releases` alone and holds after a restore, which brings back `releases` but not
  `strandings`. The gate's refusal is not the registry seeing the transaction: its other held
  slots go on to their own release. Any other refused report still ends the transaction's holds,
  and the live inputs of such a double spend are a separate ticket.
- **Old tills, old registries.** A till that does not know holds never reports and plans from
  the notes the registry lists live, so the card refuses those sales as before; `/intent` refuses
  it a change slot a newer till got held (409). A registry before v13 answers `/stranded` 404,
  which a till only logs: it still holds, so its sales succeed, and nothing is released until the
  registry is v13. The registry deploys first (`ROLLOUT-OFFLINE.md` step 8).

As built (registry migration v13, `offline.ts` `recordStranded`, `cards.ts` `openStrandings`):
`strandings` is not in the nightly backup, so a restore restarts the grace of what was held and
the tills report it again. Without `CERT_SIGNING_KEY` the rows are recorded and nothing is signed;
they stay held and `due` until a key is set. The registry trusts the till's flags as it trusts
outcomes. A sale another till took offline and has not reported yet is held until that till
reports it, but only for the grace: a till that stays off ops for longer than 37 days can have
those sales refused by the gate when it reconnects, the merchant's loss, as a wiped phone's would
be. A till whose kit window is longer than the grace can hold a phone's sale the registry has
already refused (the merchant's loss, within the caps).

## 8. Slots for loads and slots for change (Fedimint Card 4.0)

A 32-slot card's slots have two uses. Every card with applet 4.x is one: the applet's slot count
is fixed at 32 (`SLOTS`), so every such card has the eight change slots. Slots 0 to 23 take loads (a top-up, a holder's load, a
refund) and the change of an online charge. Slots 24 to 31 take only the change of an offline
charge, a phone's or the till's own.

- **Registry.** `GET /cards/:id/slots` lists free and unkeyed slots below 24 only;
  `GET /cards/:id/state` still lists slots 24 to 31, for the till to key. A holder's `/load` that
  names a slot from 24 up is 409, before anything is recorded. An `/intent` with inputs (a
  charge) whose change names a slot from 24 up, or a nonce under the current key of such a slot,
  is 409, after its replay check and before anything is recorded. An intent with no inputs there
  (a till's load) is recorded as before and logs `ALERT load_in_change_slot card=<id> till=<id>
  txid=<txid> slots=<list>` once; a holder's load finished late onto such a slot logs the same
  line, with `holder` for the till. An offline report's change in slots 24 to 31 is recorded as
  before (§ 3); a report whose change nonce already carried a note answers its 409 and logs
  `ALERT change_used card=<id> till=<id> txid=<txid> slots=<list>`.
- **`GET /registry/change-slot-check`** (operator session, 401 without): for the operator's own
  cards, every live or pending note on a slot from 24 up or under such a slot's key, with its txid
  and status, and every open or expired holder load that names such a slot, with its status. The
  change of offline sales recorded there is listed too; its txid tells it apart.
- **Till.** Loads and online change take slots 0 to 23: the room a top-up, a load or a refund
  counts is those slots, keyed or not, and a load keys only those. The spare reserve
  (`TILL-API.md`, "Spare slots for offline change") renews and keys slots 24 to 31 first, and while fewer
  than 2 of them are keyed and free it keys up to 2 whatever the tap's budget: a card's first
  online tap keys 24 and 25. The till's own offline charge puts change only in KEYED slots from 24
  to 31 whose current key carries no note in its copy of the card, so a slot that held a spent
  note takes change again only once it is re-keyed; with none it answers 409
  `offline_no_change_room`, with the prices the card pays exactly.
- **Phone.** The POS core's offline charge plans change only into KEYED slots from 24 to 31. A
  card with none is refused before it signs: 409 `offline_no_change_room`, with the prices the
  card pays exactly. A price that needs no change is taken as before. The card's next online tap
  at a till keys its change slots.
- **A dead change slot.** A slot from 24 to 31 whose key carried notes that are now all dead is
  not released at an attach, nor after a wrong PIN. Once the card takes the holder's PIN, the till
  sends the registry's release for the slot when one is listed and re-keys the slot at once, in
  the same tap, queuing the new key for the registry as every renewal is. A tap cut off between the two leaves the
  slot KEYED under its old key; the next tap that carries the PIN re-keys it (when such a slot
  waits, the till checks the PIN at attach). A tap without the PIN leaves the slot as it is.
- **Change written in the same tap.** A tap that settles an offline sale the till has reported
  (a balance tap, or a charge tap before it plans) sends the sale's outcome to the registry during
  the tap and writes the change notes onto the card before the tap ends. A charge tap can spend
  the change of the sale it settles.
- **The inputs of a refused sale.** When the registry refuses an offline sale (409
  `double_spend`, or another refusal), the card still holds the sale's inputs SPENT, and the sale's
  signed bytes are never sent to the federation. At the card's next online tap the till holds
  those slots out of its plans and reports them as stranded (§ 7). What comes back depends on the
  refusal:
  - a refusal that leaves the transaction unseen at the registry (a change nonce that already
    carried a note, `change_used`; the money gate's refusal; a 400 or another refusal, of which it
    keeps no report): the registry takes a stranding for each slot, and its release after the
    grace gives the value back;
  - a double spend, an input not live at the registry: the report counts as the registry having
    seen the transaction, and its change slots are released at once (§ 6). An input of that sale
    the registry still lists live is not given a stranding and is not released: its slot stays
    SPENT on the card, and the till holds it and reports it again at each online tap (§ 7, the
    live inputs of such a double spend).

  A charge the card's other notes pay goes ahead around them. A charge they do not pay, but
  would with the held notes, is refused 402 `notes_held`, its detail naming the held amount;
  nothing is signed.
