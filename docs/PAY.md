# Paying an invoice with a card (for app developers)

Status: built 2026-11-12 (`POST /v1/pay` and its refund). The route reference, with every error
code, is `docs/TILL-API.md` § POST /v1/pay; this page is how an app uses it.

A Fedimint card's notes can only be redeemed by a till. An app that wants a card to pay one of its
Lightning invoices (a "tap a card to add funds" button, a checkout) asks a till to do it: the till
charges the card the invoice plus a fee reserve and its margin, then pays the invoice from its own
wallet with what it took. The app never handles eCash, and the card never leaves the holder's
hand.

## What the app needs

- **A till that pays invoices.** The operator runs `till serve --pay-enabled` and sets its limits
  (largest invoice, fee reserve cap, margin, daily cap: `TILL-API.md`).
- **A pay token.** The operator runs `card-sidecar till pair --scope pay` (or
  `host/scripts/till-service.sh pair --scope pay`) and the app scans the QR, or takes the text:
  `fedimintcard-till-pay:<till url>?t=<token>`. Send `Authorization: Bearer <token>` on every
  request. The token opens only the pay routes, `/v1/health` and the card link, so the app cannot
  charge cards for itself, read sales or load cards. Keep it in the platform's secret store.
- **The card in reach.** The phone the app runs on reads the card over NFC and lends it to the
  till through the card link (`docs/POS-LINK.md`: a WebSocket at `/v1/card/link` that forwards the
  till's APDUs). Without a link the till uses the card behind its own relay (`CARD_BRIDGE_URL`).
- **The holder's PIN** when the till is on a registry (`pin_required` otherwise). The app sends it
  in the body; the till never logs, stores or echoes it.

## The flow

1. The app makes its invoice (with an amount, on the till's network, with at least 2 minutes to
   live by default).
2. It shows the price: invoice + fee reserve + margin. The till's answers carry `price_msat`; the
   app can work it out from the till's settings, or show the invoice amount and "plus up to N
   sats in fees".
3. The holder taps the card. The app opens the card link, gets its id, and sends
   `POST /v1/pay` with `X-Card-Link: <id>` and
   `{"bolt11":"…","max_fee_msat":N,"client_ref":"<new UUID>","pin":"1234"}`. Use a read timeout of
   at least 120 s.
4. On `200` with `"state":"paid"` the invoice is paid: `preimage` is the proof, and the app's own
   invoice shows paid. Done.

Make a new `client_ref` per payment and keep it until the pay is settled: it is what makes a retry
safe. The same `client_ref` never charges the card twice or pays the invoice twice.

## When it does not answer 200

- **`202 undecided`**: the card paid (or may have) and the invoice is not settled yet. Do not tap
  again or make a new pay. Poll `GET /v1/pay/{pay_id}` (or repeat the same `POST`) until `state`
  is `paid` or `pay_failed`; the till keeps driving the pay by itself.
- **Before the card was touched** (`bad_invoice`, `wrong_network`, `invoice_expired`,
  `over_limit`, `daily_cap`, `invoice_in_use`, `pay_disabled`, `pin_required`): nothing moved. Fix
  the invoice or the input and try again with a new `client_ref`.
- **The card refused** (`wrong_pin`, `insufficient`, `no_card`, with `"state":"not_charged"`; or
  `card_link_gone`, before anything ran): nothing moved. Ask again (another PIN, the card held
  still, a new link) and resend the **same** `client_ref`.
- **`502 pay_failed`**: the card paid, the invoice certainly was not (no route, the gateway asked
  more than the fee reserve, the invoice expired while the till waited). The value stays in the
  till's wallet, owed to the holder. Offer a refund to the card (below), with the card that paid
  (`short_id` in the answer).

## Refunding a failed pay

`POST /v1/pay/{pay_id}/refund` with the card that paid behind `X-Card-Link` loads the price back
onto it, rounded down to whole notes, never up: the till never loads more than it holds for the
holder. It answers the pay with `"state":"refunded"`, `refund_msat` and `refund_txid`; less than
one smallest note may stay with the till (`refund_dust_msat`). A card with too few free slots
takes part of the refund: the answer is `200` with `"state":"pay_failed"`, `refund_msat` (on the
card so far) and `refund_owed_msat` (the rest), and the holder spends from the card and taps again
until the pay is `refunded`. Each load happens at most once: calling it again answers the same
without touching the card, and a refund cut short finishes on the next call. Another card is
refused (`wrong_card`), a full card is `card_full` (spend from it, then refund again), and a till
off its registry cannot refund (`ops_not_connected`).

## What the holder pays

The card is charged the invoice, the fee reserve (the app's `max_fee_msat`, capped by the till)
and the till's margin. The part of the fee reserve the gateway did not take (`fee_refund_msat`)
stays with the till in this version; it is not put back on the card.

## Not in this version

- Putting the unspent fee reserve back on the card.
- Paying offline: a pay needs the federation, so a till that cannot reach it refuses
  (`ops_unreachable` and the like) rather than taking the card offline.
