# Top-up at the POS by Lightning

Status: agreed 2026-11-05 (owner: "yes", amount rule "nearest card amount"), being built. O6 step 3
in `OPS-PLAN.md`.

The holder pays a Lightning invoice from any wallet on their own phone; the till receives eCash
through the federation's gateway and loads it onto the card with the holder load the Mac loader
uses (`holder.rs`, the registry's `/cards/:id/load` routes). No PIN: nothing leaves the card.

## Flow

1. The POS reads the card's balance. The balance screen has two buttons: **Charge card** (the
   amount keypad, then the charge) and **Top up card**.
2. **Top up card**: the holder or merchant types an amount in sats and taps **Create invoice**.
3. The till rounds it to the nearest card amount (below), makes an invoice for exactly that, and the
   POS shows it as a QR code with the amount.
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
need 4 notes); 21 sats → 20.992 sats. The invoice is for exactly that amount, so the holder pays exactly what lands on the card.
The gateway's receive fee, if any, is the till's cost (zero on the gateways seen 2026-11-05).

## Room on the card

A top-up started from the balance screen names the card (`short_id`, from `/v1/card/balance`).
The till asks the registry for that card's free slots and rounds to at most that many notes (still
at most 3); a card with no free slot is refused with 409 `card_full` before any invoice exists, so
nobody pays for a load that cannot land. (Without `short_id` the till assumes 3 and the load itself
answers `card_full`, keeping the top-up `paid`.)

A 32-slot card may be registered with keys for slots 0–7 only (split keying). Its unkeyed slots
count as free here, because the load's tap keys the ones it needs: the shortfall plus
`ops::KEY_AHEAD` (3), each a `GEN_KEY` of about 318 ms, recorded in ops in one post before the
load. If the registry does not take the keys, the load answers 502 `ops_unreachable`, nothing is
sent and the top-up stays `paid`. The next tap records the keys the card already made.

Charges keep room too: when the cheapest charge plan would leave fewer than 3 free slots
(`plan::ROOM_AFTER_CHARGE`), the planner spends more of the card's notes so the change comes back
as fewer, larger notes. Unkeyed slots count as free here as well. A charge whose change needs
more keyed slots than the card has keys them at that tap, the same way a load does. Each extra input is one more card signature (about a second), and only on
a nearly full card.

## Till routes (POS token as for every `/v1` route)

| Route | Body → reply |
|---|---|
| `POST /v1/topup` | `{amount_msat, client_ref?, short_id?}` → the top-up |
| `GET /v1/topup/{topup_id}` | → the top-up |
| `POST /v1/topup/{topup_id}/load` | `{}` with `X-Card-Link` → the top-up, `state: "loaded"` |

Every reply is the top-up: `{ok, topup_id, amount_msat, invoice, expires_at_ms, state,
load_txid?, detail?}` (`load_txid` once loaded; `detail` when failed).

- `amount_msat` in the reply is the rounded amount. Requests that round to nothing, or above
  `MAX_TOPUP_MSAT` (100,000 sats), answer 400 `bad_amount`.
- `client_ref` (1–64 chars) makes `POST /v1/topup` idempotent: the same ref answers the same
  top-up; a different amount for a used ref answers 409 `client_ref_conflict`.
- `state`: `waiting` (invoice unpaid), `paid` (eCash in the till's wallet, not on a card yet),
  `loaded`, `expired` (unpaid at expiry; nothing to do), `failed` (`detail` says why; the eCash, if
  received, stays in the till's wallet).
- Invoices expire after 10 minutes.
- `load` on `waiting` answers 409 `not_paid`; on `expired` 410 `expired`; on `failed` 409
  `topup_failed` (with the `detail`); on `loaded` it answers the load again without touching the
  card. Card errors answer as `/v1/charge` does (`card_lost`, `card_link_gone`, …); a card without
  enough free slots answers 409 `card_full` and the top-up stays `paid` (spend on the card, then tap
  again). A load is tagged with its top-up id in the loader's journal (`holder_loads.json`), and
  every load first finishes the loads an earlier run left: one cut short after it was sent is
  finished by the next tap (of this top-up or any other) and the top-up marked `loaded`, never sent
  twice.
- Other errors: 400 `bad_amount`, 400 `bad_request` (body, `client_ref`), 404 `unknown_topup`,
  502 `invoice_failed` (no gateway answered, or it made no invoice), 503 `lightning_unavailable`
  (the till's federation has no usable `ln` module), and `busy` as for every card job.
- Without ops the till refuses top-ups (`ops_not_connected`): a holder load is recorded in ops.
- Records live in `<data_dir>/topups.json`; after a restart the till goes on watching unpaid
  invoices and finishes interrupted loads.

## Waiting top-ups

A top-up can be paid and not loaded: the card was lifted, the app closed, Cancel after paying, or
the card was full. The till keeps it `paid`, and the POS offers it again.

- `POST /v1/topup` with `short_id` records that card as the top-up's card (`for_card`).
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
- Top-up keypad: title "Top up", the amount, **Create invoice**.
- Invoice screen: "Top up N sats" (the rounded amount), the QR (`lightning:` + invoice, upper
  case), "Scan with a Lightning wallet", the time left, and Cancel. Polls `GET` every 1.5 s.
- Paid: the tap screen ("Hold the card to load N sats"), then the load over the card link, then the
  receipt "Loaded N sats".
- Expired: "The invoice expired" with **New invoice** and **Done**.

## Not in this step

- A cash top-up from the till's own balance (`POST /v1/topup` with `cash: true`): later, with an
  operator PIN or similar so a phone cannot drain the till.
- lnv2: the federation's lnv2 module (v1.0) is older than the client in fedimint v0.13 speaks; the
  till uses the federation's `ln` module.
