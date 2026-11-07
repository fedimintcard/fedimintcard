# Till service API (v1)

The till service is `card-sidecar till serve`. It owns the Fedimint client, the merchant wallet
and the card state, and drives the card through the phone's NFC relay (`POST /apdu` on port
9876). The point-of-sale app on the phone owns the UI and calls this API. Default bind
`127.0.0.1:9877`; the phone reaches it through `adb reverse tcp:9877 tcp:9877`, the laptop
reaches the phone through `adb forward tcp:9876 tcp:9876`.

All responses are JSON. Failures are `{"ok":false,"error":"<code>","detail":"<text>"}`.
Responses never contain ecash strings or secrets. Auth and POS phones on the LAN: see below and
`docs/POS-LINK.md`.
While `serve` runs it owns the data directory; do not run other `card-sidecar` commands.

### On ops

`till serve --ops-url <registry> --ops-token <till_…>` (or `OPS_URL`, `OPS_TILL_TOKEN`; both or
neither) puts the till on ops (`docs/OPS-PLAN.md` § 6). Each card job (charge, balance, issue)
reads the card's short id (`GET_PUB` slot 0), loads that card's slots, notes and open charge from
the registry, and works in `cards/<short_id>.json` in the data directory instead of
`card_state.json`. The charge is recorded in ops before the card signs (`intent`), its signed
bytes before the till sends them (`submitted`), and its outcome after; a charge another till left
open is finished first (resubmitted if ops has its signed bytes, recorded rejected if not). If ops
gives no answer for the signed bytes, the charge is `undecided` and not sent; the next job with the
card (or a repeat of the `client_ref`) posts them again and sends only once ops takes them. The
till's own outputs, `sales.json` and `cashouts.log` stay local. Slot 0 is never re-keyed on ops:
its key is the card's short id. At startup the till reports the federation's name to ops. The
token is never logged.

### Applet 3.0 cards and offline charges

A card with applet 3.0 (`SELECT` answers `03 00`; `docs/OFFLINE-PROTOCOL.md` § 1) signs each slot
once, ever, and only a slot that holds its note. The till keeps it so, online: before the card
signs, every input slot gets its note if it lacks one (`SET_NOTE` from the registry's amount and
signature), and the card's daily limit is checked (`card_limit` otherwise, before anything is
recorded); notes issued in a tap (change, a load, a top-up load) are written to the card in that
tap; and every attach writes any live note the card still lacks (a change finished elsewhere). The
till also keeps the newest Bitcoin block header (`--esplora-url`, default
`https://mempool.space/api`, `TILL_ESPLORA_URL`; every 10 minutes, kept in
`<data_dir>/block-header.json`; an empty value turns it off) and shows it to the card
(`SET_HEADER`) when the card's clock is older; this never blocks or fails a job. 2.x cards get
none of this.

**Offline.** On ops, a charge first probes the registry (`GET /registry/cert-key`) and the
federation, 2 s each, the answer kept 10 s. While both answer, charges taken offline are reported
first and the charge runs online as above. When either does not answer (or the registry goes quiet
at the attach), a 3.0 card is charged offline: its certificate is checked against the registry's
certificate key the till keeps (`cert_key` in `till-ops.json`, refreshed by every probe while
online), the card attests to a fresh challenge, each note's federation signature is checked
against the federation's keys from the client config, notes this till already took offline are
skipped, and the till's caps are checked (`--offline-cap-msat`, default 100,000,000 msat, and
`--offline-cap-count`, default 200, of charges taken offline and not yet settled; env
`TILL_OFFLINE_CAP_MSAT`, `TILL_OFFLINE_CAP_COUNT`). The PIN is verified, change goes to the
card's KEYED slots (`SET_CHANGE`, the opening kept on the card), the card signs, and the signed
transaction is saved in `<data_dir>/offline_queue.json` (0600, fsync) before the answer: `200`
with `"offline": true` and `collection_pending: true`. The sale is in `sales.json` with
`offline: true`.

A sender (every 30 s while on ops, one job at a time) reports each queued charge
(`POST /cards/:id/offline-charges`), writes it into the card's file as an open, signed charge, and
lets the ordinary recovery send it to the federation, collect the change and the till's notes, and
post `/outcome`; the sale then has `collection_pending: false`. A `409 double_spend` marks the
sale `double_spent: true`; a federation rejection after the registry took it marks it
`rejected: true` (an input spent where the registry did not see it, or a fee the transaction no
longer covers). Both are losses, kept and counted. The registry signs a release for what such a
charge left on the card (docs/OFFLINE-PROTOCOL.md § 6): for a double spend its change slots, for a
rejection its notes and change slots. The card's next online tap at any till sends the releases
that still fit (`RELEASE`) before the charge is planned, so a rejected charge's notes pay again
and its change slots take notes again; a card provisioned without the registry key keeps them as
they are. The next tap of the card writes its change (`SET_NOTE`). A queue survives restarts; an entry cut off between
the card signing and the save is finished at the card's next offline tap from the card's stored
signatures.

### Connecting by claim

A till can ask for its own token instead (`docs/TILL-CLAIM.md`). `card-sidecar till claim
--ops-url <registry> --name <name> [--text] [--replace]` starts a claim, prints the QR code the
provisioner app scans (`fedimintcard-till-claim:<registry origin>?c=<code>`; with `--text`, that
text and on a second line the code), writes the claim to `<data_dir>/till-claim.json` (0600) and
exits. A till that already has a token is refused without `--replace` (its token works until the
new claim is approved). `till serve` without `--ops-token` uses the token in
`<data_dir>/till-ops.json` (0600, with the URL of the registry that issued it); with neither it is
on ops but not connected whenever `--ops-url` is given or a claim file exists. It then polls the
claim every 3 s, and on approval writes `till-ops.json`, deletes the claim file, reports the
federation and is on ops at once, without a restart. Until then `/v1/status` says why, and
`POST /v1/charge`, `/v1/card/balance` and `/v1/cards/issue` answer `ops_not_connected`. A till
with no registry settings at all keeps its card state locally, as before ops.

### Auth and the card link (POS phones on the LAN)

`card-sidecar till pair [--url URL]` (or `host/scripts/till-service.sh pair`) makes the POS token
in `<data_dir>/till-pos-token` (0600) if there is none and prints the pairing QR code and text,
`fedimintcard-till:<url>?t=<token>`. `till serve --listen` (or `TILL_LISTEN`) on anything but
loopback is refused until that file exists. With a token, every `/v1/*` request not from loopback
needs `Authorization: Bearer <token>`; otherwise `401 {"ok":false,"error":"unauthorized"}`.
Loopback stays open (the CLI, the tests, a USB phone through `adb reverse`), except with
`till serve --require-pos-token` (or `TILL_REQUIRE_POS_TOKEN=1`): then every `/v1/*` request needs
the bearer whatever its peer, and the token is made at startup if missing. A till behind a proxy
(the StartOS package) runs that way, because the proxy's requests come from a local address.
`till pair --url` takes `http://` or `https://` (host and port only); `--text` prints only the
pairing text, `--new-token` replaces the token first.

`GET /v1/card/link` upgrades to a WebSocket over which the till sends the card's APDUs to the
phone holding it. A `POST /v1/charge` or `POST /v1/card/balance` with `X-Card-Link: <id>` uses
only that link as its card and closes it after replying; without the header the till uses the
relay (`CARD_BRIDGE_URL`) as before. Frames, timeouts and the pairing flow are in
`docs/POS-LINK.md`. Errors specific to it:

| code | HTTP | meaning | app action |
|---|---|---|---|
| `card_link_gone` | 409 | the `X-Card-Link` id is unknown or the link has closed; nothing ran | open a new link (tap again) |
| `no_card` | 409 | the link closed, the phone sent `lost`, or an APDU went unanswered for 10 s mid-job | as for the relay: retry the same `client_ref` |

## GET /v1/health
`200 {"ok":true}`

## GET /v1/status
`200 {"federation_id":"<hex>","federation_name":"<string|null>","wallet_balance_msat":N,"card_relay_ok":bool,"busy":bool,"connected":bool,"ops_state":"<state>","ops_url":"<url|null>","claim_code":"<XXXX-XXXX|null>","claim_expires_at_ms":N|null,"registry_reachable":bool|null,"federation_reachable":bool,"offline":{"queued":N,"queued_msat":N,"double_spent":N,"refused":N,"rejected":N,"cap_msat":N,"cap_count":N}|null}`

`registry_reachable` and `federation_reachable` are the probe of the offline section above (2 s
each, kept 10 s); `registry_reachable` is null without a registry. `offline` is null when the
till takes no offline charges (not `serve`). `queued` counts charges taken offline and not yet
settled, `queued_msat` what the till is owed for them (its outputs: the price plus any overpay),
`double_spent` the ones the registry found spent elsewhere, `rejected` the ones the federation
refused after the registry took them, `refused` reports the registry refused for another reason
(all kept for the operator).

`ops_state` is `connected`, `waiting_for_approval` (a claim is live: `claim_code` and its expiry),
`claim_expired`, `not_connected` (no token and no claim) or `local` (no registry configured).
`connected` is true only for `connected`. A connected till with a newer claim waiting
(`till claim --replace`) also shows that claim's code.

`card_relay_ok` means the phone relay answered `/health`; it does not mean a card is present.

## POST /v1/charge
Body: `{"price_msat":N,"client_ref":"<optional string, e.g. a UUID>","pin":"<optional, 4-8 digits>"}`.
`pin` is the holder's, typed on the till's screen; the card verifies it in the selection that
signs. It is never logged, stored, echoed or sent to ops. Without it the till uses `CARD_PIN`
(for the CLI and tests).
Send it only once the card is present on the phone. Blocks until the federation decides,
normally 5–20 s, worst case about 120 s: use a read timeout of at least 120 s.

`200 {"ok":true,"sale_id":N,"txid":"<hex>","price_msat":N,"received_msat":N,"overpaid_msat":N,"fees_msat":N,"inputs":[{"slot":0,"msat":N}],"change":[{"slot":1,"msat":N}],"collection_pending":bool,"warnings":[],"duration_ms":N}`

An offline charge (above) adds `"offline": true`; its `received_msat` is 0 and
`collection_pending` true until the sender settles it. Show "Paid offline".

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

With `pin`: a finished sale replays without a PIN, but every other repeat on a till that requires
one (unresolved, in flight, or a fresh attempt) needs `pin` again, since `pin_required` is
answered before the till looks at the earlier attempt. A `wrong_pin` or `pin_blocked` comes from
inside the spend, after any earlier attempt with that ref was settled, so it proves nothing moved:
the POS asks the holder again and resends the **same** `client_ref` with the new PIN.
`pin_required` and the ops codes (`card_changed`, `unknown_card`, `ops_*`) prove nothing about an
earlier attempt.

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
| `pin_required` | 400 | a till on ops was sent a charge without `pin`; the card was not touched and no try was spent | ask the holder for the PIN and send it |
| `wrong_pin` | 403 | the card refused the holder's PIN (`63 CX`); `detail` says how many tries are left; nothing moved | ask the holder again |
| `pin_blocked` | 423 | the card's PIN is blocked (`69 83`) | stop: the card cannot pay |
| `card_changed` | 409 | on ops: the card's state changed under this till (another till, a stale version) | retry the tap |
| `unknown_card` | 404 | on ops: the registry has no such card for this till's operator | card not provisioned for this operator |
| `ops_unreachable` | 502 | on ops: the registry could not be reached; nothing was sent | retry; check the network |
| `ops_refused` | 502 | on ops: the registry refused the till (bad or revoked token) | check the till token |
| `ops_not_connected` | 503 | the till belongs on ops but has no token yet (no claim, a claim waiting for approval, or an expired one); nothing ran | connect the till (Connect to registry), approve it in the provisioner app |
| `card_limit` | 402 | a 3.0 card's daily limit (`6A95`) does not take this charge; checked before anything is signed or recorded | decline; the card pays again once a later block header starts its next day |
| `offline_unsupported_card` | 409 | the registry or the federation is unreachable and the card is applet 2.x, which this till takes only online; nothing moved | decline, or retry once the till is online |
| `offline_not_ready` | 503 | offline, and the till has no certificate key from the registry yet (it fetches one while online) | retry once the till is online |
| `card_not_genuine` | 403 | offline: the card's certificate does not check against the registry's key, is for another card, or the card's attestation does not verify (a clone); nothing moved | decline |
| `offline_cap_reached` | 503 | offline: this till's unsettled offline total or count would pass its cap; nothing moved | decline, or retry once the till is online and has sent its queue |
| `internal` | 500 | anything else | show detail |

## GET /v1/sales
`200 {"count":N,"total_price_msat":N,"total_received_msat":N,"sales":[{"id":N,"ts_unix_ms":N,"price_msat":N,"received_msat":N,"overpaid_msat":N,"fees_msat":N,"txid":"<hex>","inputs":[...],"change":[...],"client_ref":"<string|null>","offline":bool,"double_spent":bool,"rejected":bool,"collection_pending":bool}]}`

Oldest first. The sales file (`sales.json` in the data dir, mode 0600) is a convenience ledger;
the money truth is the federation and `card_state.json`.

## POST /v1/card/balance
Empty body. Reconciles the card's notes with the federation (shares the busy flag).

`200 {"ok":true,"live_msat":N,"notes":[{"slot":0,"msat":N,"status":"live"}],"free_slots":[...]}`
Same error table as `/v1/charge` where applicable.

On ops the reply also has `short_id` and `unkeyed_slots`. A 32-slot card registered with keys
for slots 0–7 only has slots with no key yet. `free_slots` lists keyed slots only, and
`unkeyed_slots` are room as well: the till keys them at the tap that needs them (a load, or a
charge's change). Room for a top-up is `free_slots.length + unkeyed_slots.length`.

## POST /v1/topup, GET /v1/topup/{topup_id}, POST /v1/topup/{topup_id}/load, GET /v1/topups/waiting

Top-up by Lightning (`docs/TOPUP.md`): the till makes an invoice through the federation's gateway
(its `ln` module) and loads what it received onto the card as a holder load, recorded in ops.
On ops only (`503 ops_not_connected` otherwise); `503 lightning_unavailable` when the federation's
`ln` module is not usable.

`POST /v1/topup` `{"amount_msat":N,"client_ref":"<1-64 bytes, optional>","short_id":"<8 hex, optional>"}`
rounds `N` to the nearest card amount (at most 3 notes) and answers the top-up. With `short_id`
(from `/v1/card/balance`) the till reads that card's free slots from ops (keyed and unkeyed), rounds
to fit them, and refuses a card with none (`409 card_full`) before any invoice exists; the card is recorded as the
top-up's `for_card`.

`200 {"ok":true,"topup_id":"<16 hex>","amount_msat":N,"invoice":"lnbc…","expires_at_ms":T,"state":"waiting"}`

The invoice is for exactly `amount_msat` and expires in 10 minutes. The same `client_ref` answers
the same top-up (no second invoice); with another amount it is `409 client_ref_conflict`.
`400 bad_amount` for a missing, zero or too large (over 100,000 sats) amount; `502 invoice_failed`
when no gateway made an invoice.

`GET /v1/topup/{topup_id}` answers the same body with the current `state`: `waiting`, `paid`,
`loaded` (with `load_txid`), `expired`, or `failed` (with `detail`). `404 unknown_topup`.

`POST /v1/topup/{topup_id}/load` with `X-Card-Link` loads a `paid` top-up onto the card and
answers it with `"state":"loaded"` and `"load_txid"`. A `loaded` top-up answers again without a
card. `409 not_paid`, `410 expired`, `409 topup_failed`, `409 card_full` (too few free slots even
after keying unkeyed ones; the top-up stays `paid`), and the card and ops errors of `/v1/charge`.
Keying a split-keyed card's slots at this tap can also answer `502 ops_unreachable` or
`409 card_changed` when the registry does not take the keys. The top-up then stays `paid`, and
the next tap retries. Shares the job slot with charges.

`GET /v1/topups/waiting?short_id=<8 hex>` lists the `paid` top-ups not yet on a card that this card
may take: made for it (`for_card`) or for no card, oldest first. Without `short_id`, every `paid`
top-up. `400 bad_request` for a malformed `short_id`.

`200 {"ok":true,"topups":[{"topup_id":"<16 hex>","amount_msat":N,"for_card":"<8 hex>"|null,"created_at_ms":T}]}`

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
