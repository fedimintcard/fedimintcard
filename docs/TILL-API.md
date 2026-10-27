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

A repeated `client_ref` returns the earlier sale again (200) without charging the card.

| error | HTTP | meaning | app action |
|---|---|---|---|
| `bad_request` | 400 | missing or zero price | fix input |
| `busy` | 409 | another charge or balance call is in flight | wait, retry |
| `no_card` | 409 | the relay has no card in the field, or the card did not answer | re-seat card, retry |
| `relay_unreachable` | 502 | the phone relay could not be reached from the laptop | check USB / adb forward |
| `card_error` | 502 | the card returned an error status | retry; if it persists, card fault |
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
