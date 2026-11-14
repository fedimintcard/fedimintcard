# Offline payments: the contract between card, registry, till and apps

Status: 2026-11-07, for `OFFLINE-PLAN.md` F3–F5. Applet 3.0 is built (`26d784a`); its commands are
in `FedimintCardApplet.java`'s class comment and are the source of truth for § 1. § 7 (stranded
slots, ENG-867) added 2026-11-14.

## 1. Applet 3.0 (summary)

SELECT answers `03 00`. A host treats 3.0 as 32 slots with the rules below; 2.0 and 2.1 cards keep
the old behaviour (any keyed slot signs, nothing else exists).

| INS | Command | Data | Answer / rule |
|---|---|---|---|
| 12 | SIGN P1=slot | hash (32) | Only a NOTE slot (counted against the day's limit, then SPENT), or a SPENT slot for the same hash. 6985 otherwise, 6A95 over the limit |
| 14 | SET_NOTE P1=slot | amount msat (8, BE) ‖ federation signature (48) | KEYED → NOTE; CHANGE → NOTE with the same amount (6A80 if not). No PIN |
| 15 | GET_SLOT P1=slot | – | state (1) ‖ x-only key (32) ‖ amount+signature (56) ‖ spent hash (32) ‖ opening (66) = 187 |
| 16 | GET_STATE | – | 32 states ‖ now (4) ‖ window (4) ‖ limit (8) ‖ spent in window (8) ‖ cert len (1) ‖ attest key made (1) = 58 |
| 17 | SET_CHANGE P1=slot | amount (8) ‖ txid (32) ‖ out_idx (2, BE) ‖ blinding key (32) | KEYED → CHANGE |
| 18 | SET_LIMIT | limit msat (8) | only before SET_PIN; 0 = none |
| 19 | SET_HEADER | Bitcoin header (80) | → now (4, BE). 6A93 without the work |
| 30 | ATTEST_INIT | – | → P-256 public key, 65 bytes uncompressed. Before SET_PIN and SET_CERT only |
| 31 | SET_CERT | certificate (≤ 160) | once, before SET_PIN |
| 32 | GET_CERT | – | the certificate |
| 33 | ATTEST | challenge (32) | → ECDSA-SHA256, DER, over `"fedimintcard/at1"` (16 ASCII) ‖ challenge ‖ slot 0's x-only key |

States: EMPTY 0, KEYED 1, NOTE 2, CHANGE 3, SPENT 4. GEN_KEY P2=1 re-keys only KEYED or SPENT.

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
  a change nonce that already carried a note (409). A replay of a double spend answers the stored
  conflicts. `GET /registry/offline-reports` lists at most 200, newest first, only for the
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
cut off before the card's answer is finished from the card's stored signatures at its next offline
tap. Change goes only to KEYED slots the till's copy of the card does not know to hold a note.
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
  GET_STATE is 59 bytes: byte 58 is 1 once the registry key is set.
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
  registry lists open, a note's txid, its own offline queue), is not held, and a slot it could not
  read is held anyway. A slot the registry lists in `stranded` is held as listed. A held slot is
  out of the planner and out of the payable balance: `live_msat` counts payable notes only, with
  `stranded_msat` and `stranded_until_ms` beside it. After the job the till posts its holds when
  one is not listed or is listed `due`. For a due spent slot it first asks the federation, under
  one short timeout, whether the note's nonce is spent: unspent sends `unspent: true`, spent sends
  `spent: true`, no answer sends no flag and is asked again at the next tap. Reporting never fails
  the tap; releases arrive in the card's state and are sent at its next online tap (§ 6).
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
  for a spent slot, null for a change slot; `release_after` is `since` plus the grace, and `due` is
  true once the registry's clock reaches it. A row is open until the registry knows its
  transaction or releases its slot; renewing or keying its slot (`PUT /slots/:n`,
  `POST /slot-keys`) or resetting the card deletes it. While a change slot is held, `/intent`
  refuses change into it, `/load` refuses it and `GET /slots` leaves it out: a note there would
  take the key the late change needs.
- **Grace: 37 days** from the registry's first sight (`STRANDED_GRACE_MS`). A phone's kit lasts 7
  days and the till redeems its sales for 30 days after (`KitConfig`, `host/sidecar/src/pool.rs`),
  so a sale its phone can still sync is reported within the grace. Until then a late report of
  the transaction records as any other (§ 3), and its rows drop out on their own.
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
  under it is never released by the rules above and stays held.
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
outcomes. A sale another till took offline and has not reported yet is held until that till's
report makes it known. A till whose kit window is longer than the grace can hold a phone's sale
the registry has already refused (the merchant's loss, within the caps).
