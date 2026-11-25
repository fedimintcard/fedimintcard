# Admin payout API

The merchant's payouts from the till's own wallet, on the admin listener
(`host/sidecar/src/payout.rs`, `host/sidecar/src/payout/`). The admin router wraps every route
below with the admin bearer (`Authorization: Bearer <admin password>`, `401 unauthorized`
without it), a 32 KiB body limit and `Cache-Control: no-store`. Nothing here is on the POS
listener, and no POS or pay-scope token reaches it.

Amounts are **integer sats** at this boundary (`*_sat`); the till keeps msat inside. JSON
bodies only. An error is `{"ok": false, "error": "<code>", "detail": "<text>"}` with the HTTP
status shown; an error about a payout that was recorded also carries it (`"payout": {...}`).

Until the federation is joined every route answers `503 setup_required`.

## Model

- **Destination**: one Lightning address (`name@domain`, LUD-16). It is resolved, shown to the
  merchant and **confirmed** before any payout may go to it. Confirming pins the callback host
  and the address's identity in the LNURL metadata; a later change of either stops Lightning
  payouts (`destination_changed`) until the merchant confirms again.
- **Mode** governs **Lightning payouts only**: `auto` (default; the worker pays out on its
  cadence, manual Lightning requests refused), `manual`, `both`, `off`. **eCash withdrawal is
  available in every mode**, `auto` and `off` included.
- **Settings**: `reserve_sat` (float the wallet always keeps), `threshold_sat` (least amount of an
  automatic payout), `max_fee_sat` and `max_fee_ppm` (gateway fee cap; the smaller binds),
  `interval_minutes` (15–10080).
- **Available** = wallet balance − obligations − open payouts − reserve − fees, never below 0.
  - Obligations are value in the wallet owed to holders, each with a provable bound on the
    federation fees of the spends that pay it out: top-ups `waiting`, `paid`, `failed`, `expired`;
    pays `charging`, `charged`, `paying`, `refunding` (all the card paid), `pay_failed` (what is
    still owed back), `not_charged`. There is no time window. A `failed` or `expired` top-up is
    released only when the wallet's own receive operation ended canceled; a `not_charged` pay only
    when no charge job carries its ref, no unresolved charge does, and no sale for its price
    records it (read after the balance). The files of a service not running now are read from
    disk. If any cannot be read or bounded, **every payout is refused**
    (`503 obligations_unknown`).
  - Fees held back. **Lightning**: the fee cap (the gateway's own quote must be within it), plus
    a provable bound on the federation's fees of the funding transaction. That bound is
    `base × 1041 + ⌈ppm × (2 × balance − contract) / 1e6⌉ + base`, plus the Lightning module's
    contract fee. 1041 is `Transaction::MAX_TX_SIZE / 48`, the most mint items a transaction can
    hold; the mint's schedule is read from its own fee function. The client's own `send_fee_quote`
    must be within it (`409 fee_unbounded` otherwise; `503 fee_unknown` without one). A schedule
    that is not a base plus a proportional part turns Lightning payouts off (`409 fee_unbounded`,
    with the reason; `funds.lightning_blocked` in the status). On the typical schedule (100 msat,
    no proportional part) this is about 104 sat. **eCash**: the larger of the mint's own quote
    and that bound for one mint transaction (no quote: `503 fee_unknown`).
- **One wallet admission guard**: the payout's final snapshot (store, obligations, balance) and
  its funding run with no other wallet debit and no new liability between them. Debits are top-up
  loads, pays, refunds, card issues, offline sends, note collection, and reclaim reissues; new
  liabilities are a pay's `charging` and a top-up's `waiting`. Every actor job holds it shared,
  once. A payout waits for it at most 15 s (`503 wallet_busy`, nothing funded).
- **One payout at a time**: under the admin payout lock; a new payout is refused
  (`409 payout_in_progress`) while another is open.
- **Halt**: when the wallet's own record of a paid payout shows more federation fees than were
  held back, new payouts stop (`409 halted`) until the merchant resumes (`"resume": true` in
  settings).

## Routes

### `GET /admin/payout`

```json
{
  "ok": true,
  "settings": {"mode": "auto", "reserve_sat": 10000, "threshold_sat": 50000,
               "max_fee_sat": 100, "max_fee_ppm": 5000, "interval_minutes": 1440},
  "destination": {"address": "shop@example.com", "callback_host": "example.com", "description": "Pay to shop",
                  "confirmed_at_ms": 1764000000000, "min_sendable_sat": 1, "max_sendable_sat": 1000000,
                  "problem": null},
  "pending_destination": null,
  "funds": {"balance_sat": 120000, "obligations_sat": 30000, "open_payouts_sat": 0,
            "reserve_sat": 10000, "fee_allowance_sat": 112, "available_sat": 79888,
            "obligations": {"topups_sat": 20000, "pays_sat": 9800, "fees_sat": 200},
            "fee_schedule": {"mint_base_msat": 100, "mint_ppm": 0, "max_tx_items": 1041},
            "lightning_blocked": null},
  "blocked": null,
  "next_auto_at_ms": 1764086400000,
  "open": null,
  "recent": [ <payout>, ... ]
}
```

`funds` is `null` and `blocked` is `{"error", "detail"}` when the wallet or the obligations
cannot be read; `blocked` also names a halt (`halted`) or a destination that changed
(`destination_changed`). `recent` lists the newest 50 payouts, newest first. History omits bearer notes. The `ecash_step_sat` field gives the smallest whole-sat eCash withdrawal increment.

### `PUT /admin/payout/settings`

Body: any subset of the settings fields, and `"resume": true` to clear a halt. Answers the
status. `400 bad_settings` for a value out of range (`threshold_sat` ≥ 1; `max_fee_ppm` ≤
100,000; `max_fee_sat` ≤ 1,000,000; `interval_minutes` 15–10080; `mode` one of the four).

### `POST /admin/payout/destination`

Body `{"address": "shop@example.com"}`. Resolves the address now (HTTPS on 443, public
addresses only, no redirects, pinned DNS, 64 KiB answer limit) and keeps it pending for 10
minutes: `{"ok": true, "pending_destination": {..., "confirm_token": "<64 hex>",
"expires_at_ms": ...}}`. Errors: `400 bad_address`, `400 lnurl_unsafe`, `400 lnurl_identity`
(the metadata does not name this address), `502 lnurl_unreachable`, `502 lnurl_refused`.

### `POST /admin/payout/destination/confirm`

Body `{"confirm_token": "<hex>"}`. `409 no_pending_destination`, `409 confirm_token_mismatch`,
`410 pending_expired`. Answers the status. Starts the cadence: the first automatic payout comes
one interval later.

### `DELETE /admin/payout/destination`

Removes the destination (and any pending one). Open payouts are still driven. Answers the
status.

### `POST /admin/payout/lightning`

Send the confirmed recipient and its `confirmed_at_ms` from the payout status the merchant reviewed.

```json
{"request_id":"merchant-1","amount_sat":25000,"expected_destination":"shop@example.com","expected_confirmed_at_ms":1790000000000}
```

Use a request ID of 1 to 64 letters, digits, underscores or hyphens. Omitting `amount_sat` pays the available amount. Manual withdrawals require mode `manual` or `both`. A changed recipient or confirmation returns `409 destination_changed` before the till requests an invoice.

`request_id` makes it idempotent: the same id answers the same payout and never funds a second
(`409 request_conflict` with a different amount or recipient). Replay the original request after an interrupted response, even if the active recipient has since changed.

Answers `{"ok": true, "payout": <payout>}`: `200` with an outcome, `202` while `funding` (poll
the record, or repeat the request). Refused before anything is recorded: `400 bad_amount`,
`409 insufficient_available` (`available_sat`), `400 below_min_sendable` /
`above_max_sendable`, `409 payout_in_progress`, `409 halted`, `409 mode_disallows`, `409
no_destination`, `409 destination_changed`, `503 obligations_unknown`, `503
wallet_unavailable`, `503 lightning_unavailable`. Failures of a recorded payout, nothing funded:
`400 lnurl_unsafe` / `lnurl_identity` (both also mark the destination changed), `502
lnurl_unreachable` / `lnurl_refused`, `400 outside_sendable`, `409 destination_changed`, `502
invoice_rejected` (amount, network, description hash, time left, or a payment hash used before),
`503 wallet_busy`, `503 lightning_unavailable` (no gateway quotes the invoice), `502
fee_too_high` (the gateway's quote is over the cap), `409 insufficient_available` (the final
snapshot under the guard), `502 payment_failed` (refused by the wallet, or refunded).

### `POST /admin/payout/ecash`

Body `{"request_id": "...", "amount_sat": 25000}`. Any mode. Exports eCash of this federation
(with its invite) worth exactly `amount_sat`; `400 bad_amount` (with `min_note_sat`, the smallest whole-sat step) when that is
not a multiple of the federation's smallest economical note. Same `request_id` rules. Answers
`{"ok": true, "payout": <payout>}` with `payout.notes` (the eCash text, to copy) and
`payout.qr_svg` (a QR of it made by the till, or `null` with `qr_error: "too_long"` when it does
not fit one code). `502 export_failed` (with the failed payout) only when the wallet's log proves
nothing was exported; when the wallet's answer is unclear the payout answers `202` in
`exporting` and the worker settles it. `503 fee_unknown`, `503 wallet_busy`, `409 halted`, `409
insufficient_available`, `409 payout_in_progress`, `503 obligations_unknown` as above.

The notes are kept in `payout.json` (0600), never logged, and wiped only once the wallet's own
reissue of them (a reclaim) is done.

### `GET /admin/payout/records/{id}`

`{"ok": true, "payout": <payout>}` (with `qr_svg` for eCash notes it shows); `404
unknown_payout`.

### `POST /admin/payout/records/{id}/reclaim`

eCash `exported` (or `reclaiming`, to follow it): reissue the notes into the till's wallet,
tagged, saved `reclaiming` first. Answers by the reissue operation's own state:

- `200` `reclaimed`: the value is back; the notes are wiped.
- `202` `reclaiming`: still running; the worker follows it, also after a restart (found by its
  tag).
- `409 reclaim_rejected`: the federation rejected the reissue. The payout is `exported` again
  with its notes kept: the recipient may have redeemed them, which the till does not assume.
- `409 reissued_elsewhere`: this wallet already reissued these notes outside the payout (the
  operator's own reclaim); `exported`, notes kept.
- `502 reclaim_failed`: the reissue did not start; `exported`, notes kept, try again.

### `POST /admin/payout/records/{id}/delivered`

eCash `exported` only: the merchant notes the notes were handed over (`delivered_at_ms`). The
notes are **kept** (this may be the only copy, and nothing here can tell whether they were
redeemed); a later reclaim is still possible.

## Payout object

```json
{"id": "po_1a2b3c4d5e6f7a8b", "kind": "lightning" | "ecash", "origin": "auto" | "manual",
 "request_id": "...", "state": "...", "amount_sat": 25000, "created_at_ms": 0, "updated_at_ms": 0,
 "address": "shop@example.com", "payment_hash": "<hex>", "fee_cap_sat": 100, "fee_paid_sat": 3,
 "federation_fee_sat": 0, "delivered_at_ms": null, "notes": "<eCash>", "qr_svg": "<svg…>",
 "detail": "..."}
```

Lightning states: `resolving` → `invoiced` → `funding` → `paid` | `failed`. eCash:
`exporting` → `exported` → (`reclaiming` →) `reclaimed`; `failed` when the wallet's log proves
nothing was exported (re-checked for 24 h, and `exported` if the spend turns up).

`failed` always means nothing left the wallet (or it came back). A payout whose outcome is not
certain stays `funding` / `exporting` / `reclaiming` and is resolved from the wallet's own
records (the invoice's payments, the tagged operations), also after a restart; it is never
repeated.

## Worker

`payout::sweeper(state)` runs for the life of the admin service. Every 60 s it takes the payout
lock and settles every open or reclaiming payout, then, in `auto` or `both` with a confirmed,
unchanged destination, no halt, once `next_auto_at_ms` has passed and `available_sat` ≥
`threshold_sat`, starts one automatic payout of everything available. A failed automatic payout
retries after 15 minutes (or the interval, if shorter).

## Elsewhere

`POST /v1/cards/issue` (POS listener) now refuses with `409 holder_funds` an issue that would
spend what the wallet holds for holders, and `503 obligations_unknown` when that cannot be read;
a wallet that holds nothing for anyone issues as before.
