# Top-up at the POS by Lightning

Card 4.0 supports customer-funded top-ups from an empty till.

The holder pays a Lightning invoice from any wallet on their own phone; the till receives eCash
through the federation's Lightning or Lightning v2 gateway and loads it onto the card with the holder load the Mac loader
uses (`holder.rs`, the registry's `/cards/:id/load` routes). No PIN: nothing leaves the card.

## Flow

1. The POS reads the card's balance. The balance screen has two buttons: **Charge card** (the
   amount keypad, then the charge) and **Top up card**.
2. **Top up card**: the holder or merchant types an amount in sats and taps **Review top-up**.
3. The till rounds the requested card credit to a value the card can hold (below). The POS shows **On your card**, **Top-up fee** and **Total to pay** before the customer chooses **Show payment QR** and pays the Lightning invoice.
4. The holder pays from their wallet. The POS polls; on `paid` it asks for the card.
5. Tap: the till loads the notes onto the card and records the load in ops. The POS shows
   "Loaded N sats".

## The amount: nearest card amount

A card holds one note per slot, each a power of two in msat, and has 8 slots (a 2.0 card) or 32
(a 2.1 card). A top-up is at most
**3 notes** (`MAX_TOPUP_NOTES`). The till takes the requested msat and picks whichever is nearer of
the largest value at or below it and the smallest value at or above it with at most 3 set bits (a
tie goes to the lower), each note a denomination the federation issues and at least its smallest
economical note. Examples: 100 sats → 100.352 sats; 1,000 sats → 1,048.576 sats (983.04 would
need 4 notes); 21 sats → 20.992 sats. The fixed top-up fee covers the cost of receiving the payment and issuing the notes. The invoice total is the card credit plus that fee. Any difference between the quoted fee and the final costs stays in the till. A sending wallet may quote a separate routing fee. The till protects this payment for its card load, including while a payout is running. Previously issued top-ups retain their original price.

## Room on the card

A top-up started from the balance screen names the card (`short_id`, from `/v1/card/balance`).
The till asks the registry for that card's free slots, which on a 32-slot card are slots 0 to 23
only (slots 24 to 31 take the change of offline sales, `OFFLINE-PROTOCOL.md` § 8), and rounds
to at most that many notes (still
at most 3); a card with no free slot is refused with 409 `card_full` before any invoice exists, so
nobody pays for a load that cannot land. (Without `short_id` the till assumes 3 and the load itself
answers `card_full`, keeping the top-up `paid`.)

## Cards older than 4.0

A till takes no card older than 4.0 for a top-up, as for a sale: its tap answers 409 `old_card`
(the old-card message in `detail`) before any command of the load reaches the card, and nothing
goes on it. The invoice is made before any card is read, so a top-up created from the keypad can be
paid before the card is tapped; it then stays `paid`, the eCash waits in the till's wallet,
`GET /v1/topups/waiting` lists it, and it loads at the tap of a 4.x card. A top-up started from the
balance screen never reaches an invoice for an older card: that card's balance tap is refused with
the same 409 `old_card`, so the screen has no `short_id` to start from.

A top-up that was paid and meets a card with no room at its tap stays `paid`: the eCash waits in
the till's wallet, `GET /v1/topups/waiting` lists it, and it loads at a later tap, once the holder
has spent from the card (`POST /v1/topup/{id}/load`). It is not refunded.

A 32-slot card may be registered with keys for slots 0–7 only (split keying). Its unkeyed slots
below 24 count as free here, because the load's tap keys the ones it needs (never one from 24 up): the shortfall plus
`ops::KEY_AHEAD` (3), each a `GEN_KEY` of about 318 ms, recorded in ops in one post before the
load. If the registry does not take the keys, the load answers 502 `ops_unreachable`, nothing is
sent and the top-up stays `paid`. The next tap records the keys the card already made.

Charges keep room too: when the cheapest charge plan would leave fewer than 3 free slots
(`plan::ROOM_AFTER_CHARGE`), the planner spends more of the card's notes so the change comes back
as fewer, larger notes. Unkeyed slots below 24 count as free here as well. A charge whose change needs
more keyed slots than the card has keys them at that tap, the same way a load does. Each extra input is one more card signature (about a second), and only on
a nearly full card.

## Till routes (POS token as for every `/v1` route)

| Route | Body → reply |
|---|---|
| `POST /v1/topup/fee-inclusive` | `{fee_quote_version: 1, amount_msat, client_ref?, short_id?}` → the top-up and its fixed quote |
| `GET /v1/topup/{topup_id}` | → the top-up |
| `POST /v1/topup/{topup_id}/load` | `{}` with `X-Card-Link` → the top-up, `state: "loaded"` |

Every reply is the top-up: `{ok, topup_id, amount_msat, invoice, expires_at_ms, state,
load_txid?, detail?}` (`load_txid` once loaded; `detail` when failed).

- A new quote includes `fee_quote_version: 1`, the rounded card credit in `amount_msat`, the fixed `topup_fee_msat`, and `invoice_amount_msat`, the total payable on the signed Lightning invoice.
- `fee_accounting` records the gateway deduction, federation claim cost and card-load cost in msat. Once reconciled, `retained_nominal_msat` is the fixed fee less these costs. `retained_spendable_msat` also accounts for any change in mint v1 notes too small to spend. Either amount can be unknown until the wallet can verify both operations. A paid top-up keeps its promised card credit while the wallet completes this accounting.
- `amount_msat` in the reply is the rounded card amount. Requests that round to nothing, or above
  `MAX_TOPUP_MSAT` (100,000 sats), answer 400 `bad_amount`.
- `client_ref` (1–64 chars) makes top-up creation idempotent: the same ref answers the same
  top-up; a different amount for a used ref answers 409 `client_ref_conflict`.
- `state`: `waiting` (invoice unpaid), `paid` (eCash in the till's wallet, not on a card yet),
  `loaded`, `expired` (unpaid at expiry; nothing to do), `failed` (`detail` says why; the eCash, if
  received, stays in the till's wallet).
- Invoices expire after 10 minutes.
- `load` on `waiting` answers 409 `not_paid`; on `expired` 410 `expired`; on `failed` 409
  `topup_failed` (with the `detail`); on `loaded` it answers the load again without touching the
  card. Card errors answer as `/v1/charge` does (`card_lost`, `card_link_gone`, …); a card older
  than 4.0 answers 409 `old_card` and the top-up stays `paid` (above); a card without
  enough free slots answers 409 `card_full` and the top-up stays `paid` (spend on the card, then tap
  again). A load is tagged with its top-up id in the loader's journal (`holder_loads.json`), and
  every load first finishes the loads an earlier run left: one cut short after it was sent is
  finished by the next tap (of this top-up or any other) and the top-up marked `loaded`, never sent
  twice.
- Other errors: 400 `bad_amount`, 400 `bad_request` (body, `client_ref`), 404 `unknown_topup`,
  409 `topup_quote_version` (update the POS), 503 `topup_quote_unavailable` (the till could not verify a funded quote), 503 `lightning_unavailable`
  (the till's federation has no usable `ln` module), and `busy` as for every card job.
- Without ops the till refuses top-ups (`ops_not_connected`): a holder load is recorded in ops.
- Records live in `<data_dir>/topups.json`; after a restart the till goes on watching unpaid
  invoices and finishes interrupted loads.

## Waiting top-ups

A top-up can be paid and not loaded: the card was lifted, the app closed, Cancel after paying, or
the card was full. The till keeps it `paid`, and the POS offers it again.

- Creating a top-up with `short_id` records that card as the top-up's card (`for_card`).
- `GET /v1/topups/waiting?short_id=<id>` → `{ok:true, topups:[{topup_id, amount_msat, for_card,
  created_at_ms}]}`: the `paid` top-ups whose `for_card` is that card or none, oldest first.
  Without `short_id`, every `paid` top-up.
- The POS asks after every balance read. With one or more, the balance screen shows **Load waiting
  top-up · N sats** (the oldest; after it loads the next is offered on the next read). A top-up
  made for another card is never offered. Tapping it goes straight to the tap screen and the
  ordinary `POST /v1/topup/{id}/load`.

## POS

- Balance screen: **Charge card** and **Top up card** side by side under the balance; either
  stops the screen's return countdown.
- Top-up keypad: title "Top up", the amount, **Review top-up**.
- The POS displays and checks the agreed card credit, fee and total before presenting the QR. Once paid, the next step is the card tap. No service restart is needed.
- Paid: the tap screen ("Hold the card to load N sats"), then the load over the card link, then the
  receipt "Loaded N sats".
- Expired: "The invoice expired" with **New invoice** and **Done**.

## Not in this step

- A cash top-up from the till's own balance (`POST /v1/topup` with `cash: true`): later, with an
  operator PIN or similar so a phone cannot drain the till.
