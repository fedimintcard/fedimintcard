# Till service API (v1)

The till service is `card-sidecar till serve`. It owns the Fedimint client, the merchant wallet
and the card state, and drives the card through the phone's NFC relay (`POST /apdu` on port
9876). The point-of-sale app on the phone owns the UI and calls this API. Default bind
`127.0.0.1:9877`; the phone reaches it through `adb reverse tcp:9877 tcp:9877`, the laptop
reaches the phone through `adb forward tcp:9876 tcp:9876`.

All responses are JSON. Failures are `{"ok":false,"error":"<code>","detail":"<text>"}`.
No auth: keep it on loopback. Responses never contain ecash strings or secrets.
While `serve` runs it owns the data directory; do not run other `card-sidecar` commands.

## GET /v1/health
`200 {"ok":true}`

## GET /v1/status
`200 {"federation_id":"<hex>","federation_name":"<string|null>","wallet_balance_msat":N,"card_relay_ok":bool,"busy":bool}`

`card_relay_ok` means the phone relay answered `/health`; it does not mean a card is present.

## POST /v1/charge
Body: `{"price_msat":N,"client_ref":"<optional string, e.g. a UUID>"}`.
Send it only once the card is present on the phone. Blocks until the federation decides,
normally 5–20 s, worst case about 120 s: use a read timeout of at least 120 s.

`200 {"ok":true,"sale_id":N,"txid":"<hex>","price_msat":N,"received_msat":N,"overpaid_msat":N,"fees_msat":N,"inputs":[{"slot":0,"msat":N}],"change":[{"slot":1,"msat":N}],"collection_pending":bool,"warnings":[],"duration_ms":N}`

`received_msat` is what the wallet has collected so far; when `collection_pending` is true the
federation has accepted the payment but the wallet has not yet swept it. Show "paid" either way.

`client_ref` is stored with the transaction record in `card_state.json`, in the same save that
creates it and before anything is signed or submitted. A repeated `client_ref` therefore never
starts a second spend while the first may still count:

* the earlier attempt was accepted: the same success body is returned again (200, same `sale_id`
  and `txid`), without touching the card. The sale is in `sales.json` as soon as the federation has
  accepted it (`collection_pending` true until the till notes are in the wallet), so an app can
  also poll `/v1/sales` for its `client_ref`;
* the earlier attempt is still unresolved, or still in flight (the service is busy with it):
  `undecided` 202, never `busy`, never a second spend. The service resolves it on the next job
  and records the sale;
* the earlier attempt is proven to have left nothing behind (`insufficient`, `no_card`,
  `declined`, a crash before submission, ...): the repeat is a fresh attempt with the same ref;
* the same `client_ref` with a different `price_msat` is `conflict` 409.

| error | HTTP | meaning | app action |
|---|---|---|---|
| `bad_request` | 400 | missing or zero price | fix input |
| `conflict` | 409 | this `client_ref` was used for a sale of a different price | use a new `client_ref` |
| `busy` | 409 | another charge or balance call is in flight | wait, retry |
| `no_card` | 409 | the relay has no card in the field, or the card did not answer | re-seat card, retry |
| `relay_unreachable` | 502 | the phone relay could not be reached from the laptop | check USB / adb forward |
| `card_error` | 502 | the card returned an error status, or a different card than the wallet's was presented ("different card presented") | retry; if it persists, card fault |
| `insufficient` | 402 | the card's notes do not cover the price | decline the sale |
| `no_exact_plan` | 422 | notes cannot make the price without too many change notes | decline; card needs a top up |
| `declined` | 402 | the federation rejected the spend; nothing moved | retry once, then decline |
| `undecided` | 202 | submitted, not yet confirmed; nothing further to do now | show "pending", then poll /v1/sales |
| `recovering` | 503 | an earlier transaction is still being resolved | retry after a few seconds |
| `internal` | 500 | anything else | show detail |

## GET /v1/sales
`200 {"count":N,"total_price_msat":N,"total_received_msat":N,"sales":[{"id":N,"ts_unix_ms":N,"price_msat":N,"received_msat":N,"overpaid_msat":N,"fees_msat":N,"txid":"<hex>","inputs":[...],"change":[...],"client_ref":"<string|null>"}]}`

Oldest first. The sales file (`sales.json` in the data dir, mode 0600) is a convenience ledger;
the money truth is the federation and `card_state.json`.

## POST /v1/card/balance
Empty body. Reconciles the card's notes with the federation (shares the busy flag).

`200 {"ok":true,"live_msat":N,"notes":[{"slot":0,"msat":N,"status":"live"}],"free_slots":[...]}`
Same error table as `/v1/charge` where applicable.

## POST /v1/cards/precheck
Empty body (or `{"batch_id":"<string>"}`, ignored). **Read-only**: nothing is written to
`card_state.json`, so the app may call it on every tap. Answers what is on the card, in the order
the provisioning loop needs it — must the sweep ask first, is the applet ours, can the ISD take an
INSTALL.

`200 {"ok":true,"applet_present":B,"isd_open":B,"keyed_slots":[...],"live_msat":N,"safe_to_replace":B,"kind":"blank|installed|foreign","applet":"unknown|not_set|set|blocked","empty_slots":[...],"foreign_slots":[...],"free_slots":[...],"live_notes":[{"slot":S,"msat":N}],"slots":8}`

* `applet_present` — `SELECT` of our AID answered `9000`; `false` on `6A82`.
* `isd_open` — `GET DATA FF21` got an answer. A status word counts; only a transport error does not.
* `safe_to_replace` — `true` only for `kind:"blank"`. Any key on the card means the app must ask
  before it sweeps (`keyed_slots` is what would be destroyed).
* `kind:"foreign"` — a slot holds a key this wallet never produced. `keySetup` and `issue` both
  refuse it; the sweep cannot see it (the applet is not selected before `DELETE`, so `6A88` looks
  the same as on a blank card) and it is caught at `INSTALL [for load]` (`6985`).
* `live_msat` is in satoshi, `live_notes[].msat` in millisatoshis, as everywhere else here.

## POST /v1/cards/keySetup
`{"pin":"<4-8 digits>"}` — `pin` is optional; without it the card is keyed and left unpinned.
Runs `card setup` (never `--fresh`: re-keying destroys live notes) then `SET_PIN`, then
`VERIFY_PIN`. Shares the single job slot with charges.

`200 {"ok":true,"keys":[{"slot":0,"pubkey":"<hex>"}],"short_id":"<8 hex>","pin_set":B,"pin":"set|set_now|not_set|blocked","generated":[...]}`

Idempotent: keys already present are kept, an existing PIN answers `6985` and is reported `"set"`
(not reset), and `generated` is empty on a repeat. `short_id` is 8 hex chars hashed over the whole
key set; it is stable for a given card. `pin_set` is `true` only when `VERIFY_PIN` accepted the PIN
on this call — a `set_now` that failed to verify reports `pin_set:false`, which the registry
records as `pinNotSet` and refuses to hand out.
Errors: `400 bad_request` for a PIN outside 4-8 digits (refused before the card sees it, so a
malformed PIN cannot spend a try); `502 card_error` with `detail` naming the slot on a foreign card.

## POST /v1/cards/issue
`{"amounts_msat":[N,...],"client_ref":"<1-128 bytes>"}` — each amount becomes one note per set bit,
as `card topup` does. Shares the single job slot with charges; the reply waits for the federation
to accept the transaction.

`200 {"ok":true,"client_ref":"...","txid":"<hex>","notes":[{"slot":0,"msat":N}],"live_msat":N,"replayed":B,"plan":{...}}`

`client_ref` is the idempotency key. The record is written to `card_state.json` in the same save
that writes the pending notes, before the federation hears of it; a repeat therefore returns the
same `txid` and `notes` without touching the card or the wallet. A repeat with a *different*
`amounts_msat` is `409 ref_mismatch`.

`409 card_holds_notes` when the wallet has any live or pending note on this card, whatever slot it
sits in — issuing again would double the card's value. This is the guard the app cannot provide:
the card looks blank to the applet, and a copied card looks exactly like an unused one.

`202 undecided` when this `client_ref` is still in flight — a concurrent tap of the same batch
item answers "ask again" rather than `busy`, and nothing was issued twice.
Other errors are `/v1/charge`'s table plus `402 insufficient` and `422 no_exact_plan` from the
planner.
