# Offline payments: the contract between card, registry, till and apps

Status: 2026-11-07, for `OFFLINE-PLAN.md` F3–F5. Applet 3.0 is built (`26d784a`); its commands are
in `FedimintCardApplet.java`'s class comment and are the source of truth for § 1.

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
  SET_LIMIT (a batch setting, default 10,000 sat), ATTEST_INIT, `POST …/attestation`, SET_CERT.
- **POS**: a charge answered with `offline: true` shows "Paid offline"; the till's status shows
  what is waiting to sync.
