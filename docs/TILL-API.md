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

**Fee margin.** An offline charge is signed at the federation's fees of that moment and sent
later. On a federation at core consensus 2.1 or later (read from its client config; logged at
startup), which takes a transaction that pays more than its outputs and fees and burns the
surplus, the till plans an offline charge with a fee margin: its inputs also pay the fees this
transaction would owe under a headroom fee schedule, less its fees now (floored at 0). The
headroom is `--offline-fee-base-msat` (`TILL_OFFLINE_FEE_BASE_MSAT`, default 100 msat per input
and per output) plus `--offline-fee-headroom-ppm` (`TILL_OFFLINE_FEE_HEADROOM_PPM`, default 0),
fedimint's default mint fees; 0 and 0 turn it off. A federation already charging the default fees
gives a margin of 0. The margin comes out of the change or out of the till's overpay (its
rounding surplus over the price); the till always gets at least the price. A margin that would need another change note, slot or input is not taken: the
charge is then exact, never refused for it. The margin is `fee_margin_msat` in the answer and in
the queue entry. Online charges always balance exactly. On an older federation (no surplus
allowed) offline charges balance exactly, and a fee change before the charge is sent gets it
rejected (`rejected: true`, its notes released, below).

A sender (every 30 s while on ops, one job at a time) reports each queued charge
(`POST /cards/:id/offline-charges`), writes it into the card's file as an open, signed charge, and
lets the ordinary recovery send it to the federation, collect the change and the till's notes, and
post `/outcome`; the sale then has `collection_pending: false`. A `409 double_spend` marks the
sale `double_spent: true`; a federation rejection after the registry took it marks it
`rejected: true` (an input spent where the registry did not see it, or a fee rise beyond the
charge's fee margin). Both are losses, kept and counted. The registry signs a release for what such a
charge left on the card (docs/OFFLINE-PROTOCOL.md § 6): for a double spend its change slots, for a
rejection its notes and change slots. The card's next online tap at any till sends the releases
that still fit (`RELEASE`) before the charge is planned, so a rejected charge's notes pay again
and its change slots take notes again; a card provisioned without the registry key keeps them as
they are. The next tap of the card writes its change (`SET_NOTE`). A queue survives restarts; an entry cut off between
the card signing and the save is finished at the card's next offline tap from the card's stored
signatures.

**Stranded slots.** A charge that reaches no till (a POS phone's offline sales, lost with the
phone before it synced) leaves its card SPENT slots whose notes the registry still calls live, and
CHANGE slots it calls free. The card signs a note once, and a CHANGE slot takes only the change
it is owed, so every charge, balance or issue tap of a 3.0 card on ops, after the releases and
before anything is planned, holds such slots back for that tap: a SPENT slot whose note is live
and is no input of an open charge, and a CHANGE slot whose key never carried a note, unless the
transaction the slot signed or waits on is one this till or the registry has (an open charge, a
live or pending note's transaction, a charge in this till's offline queue that it has not
abandoned). A held note is out of the plan and out of `live_msat`, and a held CHANGE slot is no
room for change. A top-up load (`POST /v1/topup/{topup_id}/load`) reads no slot state from the
card: it relies on the registry leaving held CHANGE slots out of its free slots. A slot the
registry already lists as stranded is held as listed; up to 8 others per tap are read
(`GET_SLOT`) for their transaction, and the rest, or one whose read fails, are held on doubt and
read at a later tap. As the tap ends the till reports the slots the registry does not list yet,
or lists as due
(`POST /cards/:id/stranded`, docs/OFFLINE-PROTOCOL.md § 7). A slot is due 37 days after the
registry first heard of it: a kit's 7 days plus its 30-day redeem window. For the due spent notes
the till asks the federation, 3 s for them all. A note it says is unspent goes as
`unspent: true`, and the registry releases the slots of that transaction, which the next tap
sends to the card (`RELEASE`): the holder has the notes back. A note it says is spent goes as
`spent: true`: the registry records it spent and never releases it, the till's copy has it spent
too, and its slot is renewed at the next charge with the PIN. That transaction landed, so its
change is real and its CHANGE slots are never released. No answer sends neither, and a later tap
asks again. A CHANGE slot whose transaction has no SPENT slot (a card pulled before it signed), or
has a release already, is released after the grace with no federation check. A report of a
transaction after its release is refused as a double spend: the merchant's loss, within the caps.
A registry older than stranded slots answers the report 404, which is logged; the slots are held
all the same.

**Spare slots for offline change.** Offline, change goes only to a KEYED slot whose key never
carried a note, and no till can key one without the registry; a freshly loaded card has only a
few (the F6 drill's card ran out after two charges with change). So every online tap of a 3.0
card on ops (a charge, a balance, a load), after the releases and the notes it writes and before
anything is planned, tops up a spare reserve: when the card has fewer than `--spare-target`
(`TILL_SPARE_TARGET`, default 12; 0 turns it off) fresh keyed slots, the till keys more, at most
`--spare-keys-per-tap` (`TILL_SPARE_KEYS_PER_TAP`, default 2) in one tap, since a key takes about
320 ms on the card. A tap that already sent releases or wrote notes keys fewer (about 100 ms per
release and 40 ms per note come out of that budget). SPENT slots whose notes are settled (spent
or dead in the registry, confirmed spent by the federation, no charge open on them) are renewed
first, queued to the registry as every renewal is (`PUT /cards/:id/slots/:n`); renewing takes the
holder's PIN, so only a charge renews. Then never-keyed slots (`unkeyed_slots`) are keyed and
recorded with `POST /cards/:id/slot-keys`. A failure is logged and the tap goes on; only a PIN the
card refuses ends it (`wrong_pin`, as the charge would when it signs, so the holder spends one try,
not two). 2.x cards are left as they are.

An offline charge whose price needs change the card has no room for is refused with
`offline_no_change_room` (409), and the answer adds the prices the card can pay with the slots it
has, in whole sats: `"suggested_msat":N|null` (the nearer of the two), `"suggest_below_msat"`
and `"suggest_above_msat"` (each `N` or null). The POS shows "This card can't take change while
offline. Try N sats" (or "Try another amount"), not "card needs a top up".

**A card taken away mid-tap.** A card pulled after it signed some of an offline charge's inputs
leaves the charge `signing`, and that tap is answered `undecided` (202): only the card's next tap
can say whether it paid. (A card pulled before it signed anything leaves nothing behind, and the
answer is `no_card`.) Tapped again with the same `client_ref`,
the till finishes that charge from the card's stored signatures and answers it, as a repeat; it
never signs a second transaction for the sale. Another card tapped with that `client_ref` while
the first card's charge is still signing is answered `undecided` (202) with nothing signed: only
the first card's next tap can say whether it paid. Once the till is online again, a tap with
that `client_ref` still goes the offline way, never the online charge, which would take the sale
a second time from the card's other notes: the first card finishes its charge (answered as the
sale, `offline: true`; the sender's next pass, every 30 s, reports it), and another card is
`undecided`. A charge left signing that can never be finished (an input slot of it has since
paid another transaction, or was re-keyed) is set aside at the card's next tap on the offline
path (`abandoned` in the queue, kept for the operator: the notes the card signed for it stay
spent on the card, and those it never signed pay again); nothing was charged for it, and the tap
goes on as a new charge, for the same `client_ref` too. It is never sent, so the slots the card
spent or opened for it are stranded slots (above): held, reported, and released after the grace.
Notes the till's checks refuse (no denomination of the federation's, or a federation signature
that does not verify) are skipped; when the rest do not pay, the charge is
`offline_note_invalid` (403), not `insufficient`.

**POS phones offline.** A paired POS phone with no network takes a 3.0 card itself, with the
same charge as the till's (`host/pos-core`, docs/POS-OFFLINE.md), and syncs what it took when it
next reaches the till: it fetches an offline kit while online (`GET /v1/offline/kit`, below) and
sends its charges with `POST /v1/offline/charges`, which puts each into this till's offline queue
as `queued`; the sender above reports and settles it as the till's own, and its sale is in
`sales.json` with `offline: true`. A kit lasts `--offline-kit-ttl-hours`
(`TILL_OFFLINE_KIT_TTL_HOURS`, default 168, a week) from the fetch that answered it; the charges
taken with it are taken for `--offline-kit-grace-days` (`TILL_OFFLINE_KIT_GRACE_DAYS`, default
30) after that, then the kit's unused entries are dropped with their secrets. A phone's charges
count toward this till's caps above; one is never refused for them (the money is already taken).

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

**The pay scope.** `till pair --scope pay` makes a second token, in `<data_dir>/till-pay-token`
(0600; the POS token is made too if there is none), and prints
`fedimintcard-till-pay:<url>?t=<token>`. Its bearer opens `POST /v1/pay`, `GET /v1/pay/{pay_id}`,
`POST /v1/pay/{pay_id}/refund`, `GET /v1/health` and `GET /v1/card/link`, and nothing else, from
every peer, loopback included: any other route (`GET /v1/pays`, every holder's pays, too) answers
`403 {"ok":false,"error":"forbidden","detail":"…"}`. It is for an app that has a card pay its
invoices (`docs/PAY.md`) without holding the till; paying still needs the card and, on ops, its
PIN. The token lives in a phone app, so it is held to its own card and its own pays: a
`POST /v1/pay` or `POST /v1/pay/{pay_id}/refund` with it and no `X-Card-Link` answers
`400 card_link_required` before anything runs (never the card on the till's own relay or reader),
and `GET /v1/pay/{pay_id}` with it answers only with `?client_ref=<the pay's client_ref>`
(`404 unknown_pay` otherwise, as for no pay at all). `--scope pay --new-token` replaces it. `till serve` reads it at start (a running till keeps
the token it started with).

`GET /v1/card/link` upgrades to a WebSocket over which the till sends the card's APDUs to the
phone holding it. A `POST /v1/charge`, `POST /v1/card/balance`, `POST /v1/topup/{id}/load`,
`POST /v1/pay` or `POST /v1/pay/{id}/refund` with `X-Card-Link: <id>` uses only that link as its
card and closes it after replying; without the header the till uses the
relay (`CARD_BRIDGE_URL`) as before. Frames, timeouts and the pairing flow are in
`docs/POS-LINK.md`. Errors specific to it:

| code | HTTP | meaning | app action |
|---|---|---|---|
| `card_link_gone` | 409 | the `X-Card-Link` id is unknown or the link has closed; nothing ran | open a new link (tap again) |
| `no_card` | 409 | the link closed, the phone sent `lost`, or an APDU went unanswered for 10 s mid-job | as for the relay: retry the same `client_ref` |

## GET /v1/health
`200 {"ok":true}`

## GET /v1/status
`200 {"federation_id":"<hex>","federation_name":"<string|null>","wallet_balance_msat":N,"card_relay_ok":bool,"busy":bool,"connected":bool,"ops_state":"<state>","ops_url":"<url|null>","claim_code":"<XXXX-XXXX|null>","claim_expires_at_ms":N|null,"registry_reachable":bool|null,"federation_reachable":bool,"offline":{"queued":N,"queued_msat":N,"double_spent":N,"refused":N,"rejected":N,"cap_msat":N,"cap_count":N,"fee_margin":bool,"fee_headroom_base_msat":N,"fee_headroom_ppm":N,"last_fee_margin_msat":N|null}|null}`

`registry_reachable` and `federation_reachable` are the probe of the offline section above (2 s
each, kept 10 s); `registry_reachable` is null without a registry. `offline` is null when the
till takes no offline charges (not `serve`). `queued` counts charges taken offline and not yet
settled, `queued_msat` what the till is owed for them (its outputs: the price plus any overpay),
`double_spent` the ones the registry found spent elsewhere, `rejected` the ones the federation
refused after the registry took them, `refused` reports the registry refused for another reason
(all kept for the operator). `fee_margin` is true when offline charges carry a fee margin (the
federation is at core consensus 2.1 or later and the headroom is not 0); `fee_headroom_base_msat`
and `fee_headroom_ppm` are the headroom; `last_fee_margin_msat` is the margin of the newest charge
in the queue (null when there is none).

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

An offline charge (above) adds `"offline": true` and `"fee_margin_msat":N` (the fee margin its
inputs pay, burned when it is sent; 0 for an exact charge); its `received_msat` is 0 and
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
* the earlier attempt is still unresolved, or still in flight (the service is busy with it), or
  its card was taken away while it signed an offline charge (that card's next tap finishes it):
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
| `undecided` | 202 | submitted, not yet confirmed; nothing further to do now. Offline, or online for a sale this till took offline: the card was taken away while it signed (its next tap finishes the sale), or an earlier attempt with this `client_ref` waits for that card and this one was not charged | show "pending", then poll /v1/sales; for a sale taken offline, ask for the first card again (its tap finishes the sale, online or not) |
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
| `offline_no_change_room` | 409 | offline: the price needs change and the card has no fresh keyed slot (or too few) for it; an online till keys more. The body adds `suggested_msat`, `suggest_below_msat` and `suggest_above_msat`, prices in whole sats the card pays exactly now (each may be null); nothing moved | show "This card can't take change while offline. Try N sats" (`suggested_msat`), else "Try another amount" |
| `offline_cap_reached` | 503 | offline: this till's unsettled offline total or count would pass its cap; nothing moved | decline, or retry once the till is online and has sent its queue |
| `offline_note_invalid` | 403 | offline: some of the card's notes do not check against the federation's keys (no denomination of its, or a signature that does not verify) and the rest do not pay the price; nothing was signed | decline: the card cannot be checked offline; it pays online |
| `offline_kit_expired` | 403 | a POS phone's offline charge only: its offline kit is past its expiry, by the phone's clock or the card's; nothing was asked of the card | take the card online, or fetch a new kit from the till |
| `offline_pool_exhausted` | 409 | a POS phone's offline charge only: the price needs a till note of a tier the phone's offline kit has run out of; the answer has the prices the kit still pays (`suggested_msat`, `below_msat`, `above_msat`, as `offline_no_change_room`); nothing moved | show "Try N sats", and fetch a new kit when online |
| `internal` | 500 | anything else | show detail |

## GET /v1/sales
`200 {"count":N,"total_price_msat":N,"total_received_msat":N,"sales":[{"id":N,"ts_unix_ms":N,"price_msat":N,"received_msat":N,"overpaid_msat":N,"fees_msat":N,"txid":"<hex>","inputs":[...],"change":[...],"client_ref":"<string|null>","offline":bool,"double_spent":bool,"rejected":bool,"collection_pending":bool}]}`

Oldest first. The sales file (`sales.json` in the data dir, mode 0600) is a convenience ledger;
the money truth is the federation and `card_state.json`.

## GET /v1/offline/kit

`GET /v1/offline/kit?phone=<32 hex>`: the offline kit of the POS phone whose store id
(`phone-id`, from the core's `PosCore.phone_id()`) is `phone`. Under the POS token like every
`/v1/*` route; the answer is `Cache-Control: no-store`.

`200 {"ok":true,"kit":{"version":1,"kit_id":"<32 hex>","phone_id":"<32 hex>","federation_id":"<hex>","mint_instance_id":N,"tbs_pks":{"<msat>":"<hex>",…},"fee_consensus":"<hex>","fee_headroom":{"base_msat":N,"ppm":N},"surplus_allowed":bool,"cert_key":"<130 hex>","cap_msat":N,"cap_count":N,"issued_at_ms":T,"expires_at_ms":T,"block_header":"<160 hex>"|absent,"pool":[{"amount_msat":N,"blinded":["<96 hex>",…]},…]}}`

The kit carries the federation's note keys per tier and its mint fees, the till's fee headroom,
the registry's certificate key, the phone's caps (the till's `--offline-cap-*`), its validity
(`--offline-kit-ttl-hours`), the newest block header, and a pool of the till's outputs: per tier
from the smallest note the planner gives the till (4 msat on fedimint's default fees) to the
largest at or below `cap_msat`, 40 blinded nonces each (fewer when the kit would pass 128 KB; the
till logs it). Their spend keys and blinding keys stay on the till, in `offline_pool.json` (0600),
written before the kit is answered. Asked again within an hour, while every tier has 4 entries
left, the phone gets the same kit (its entries still unused); else a new one, but never more than
3 kits that have not expired: at the cap, the newest again, its tiers topped up to 40 under the
same `kit_id` once one has fewer than 4 left. A kit given again is good for
`--offline-kit-ttl-hours` from that fetch, and its charges are taken for the grace after that
(both move at most once an hour). Each call drops the unused entries of kits past their grace,
with their secrets; the entries charges used stay, so a charge sent again is still `already`.
Pass the whole answer to the core's `charge`.

`400 bad_request` for a missing or malformed `phone`; `503 offline_not_ready` when the till has no
certificate key from the registry yet (it fetches one while online), or takes no offline charges.

## POST /v1/offline/charges

`{"phone_id":"<32 hex>","charges":[…]}`: what the phone took offline, at most 50 charges, the body
of the core's `drain` as it is (each charge in the offline queue's format, with its signed bytes
and its `pool` outputs). The body is at most 1 MiB (`413` past it).

`200 {"ok":true,"results":[{"txid":"<hex>","status":"queued"|"already"|"refused","code":"<code>","detail":"…","retry":bool}]}`, one per charge, in order.

Each charge is checked from its signed bytes alone, with no federation or registry call: the
txid; every input a note of this federation's mint whose federation signature verifies, signed
by the card over the txid, as declared; every output the declared change (its blinding key and
the card's key) or a pool entry this till issued to this phone, unused, within its kit's redeem
window, of the same amount. A charge that checks is put into the till's offline queue as
`queued`, with the till's secrets for its pool outputs, then its pool entries are marked used,
then its sale is recorded (a `client_ref` this ledger has under another transaction is dropped
from the sale, with a warning); a request's charges are one write of the queue and one of the
ledger. A txid the queue or the ledger already has is `already`: send it again freely. The sender
reports and settles it like the till's own charges.

| code | retry | meaning |
|---|---|---|
| `bad_charge` | false | the bytes do not decode, or are not the charge it declares |
| `bad_note` | false | an input note's federation signature does not verify, or its amount is no denomination |
| `bad_signature` | false | a card signature does not verify over the txid |
| `pool_entry_unknown` | false | a pool output this till did not issue to this phone, or one of a kit past its redeem window whose unused entries are dropped |
| `pool_entry_expired` | false | a pool output of a kit past its redeem window (its secrets are dropped at the next kit call) |
| `pool_entry_reused` | false | a pool output another charge already paid to (a phone store restored from a backup) |
| `internal` | true | the till could not write its queue: send it again later |

The phone keeps a refused charge, with its code, to show (`retry: false`), and sends a `retry`
one again. `400 bad_request` for a body that is not this shape, a malformed `phone_id` or more
than 50 charges; `503 offline_not_ready` when the till takes no offline charges.

## POST /v1/card/balance
Empty body. Reconciles the card's notes with the federation (shares the busy flag).

`200 {"ok":true,"live_msat":N,"stranded_msat":N,"stranded_until_ms":N|null,"notes":[{"slot":0,"msat":N,"status":"live"}],"free_slots":[...]}`
Same error table as `/v1/charge` where applicable.

`live_msat` is what the card can pay. A note an online tap holds back (on ops; "Stranded slots"
above) keeps its status, is marked `"stranded": true`, and counts in `stranded_msat` instead of
`live_msat`. `stranded_until_ms` is when the last of them may be released, by the registry's
clock, or `null` when there is no date to wait for: none is held, the registry has not listed one
yet (its grace starts with this tap's report), or every one is due (the notes come back within a
tap or two). A held CHANGE slot is not in `free_slots`. A till from before these fields sends
neither.

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

## POST /v1/pay, GET /v1/pay/{pay_id}, GET /v1/pays, POST /v1/pay/{pay_id}/refund

Paying a Lightning invoice with a card (`docs/PAY.md` for app developers): the till charges the
card the invoice, a fee reserve and its margin, through the charge path, then pays the invoice
from its own wallet with what it took. Off by default: `till serve --pay-enabled` (or
`TILL_PAY_ENABLED=1`). Settings, each also an env variable:

| flag | env | default | meaning |
|---|---|---|---|
| `--pay-enabled` | `TILL_PAY_ENABLED` | off | `POST /v1/pay` is refused (`503 pay_disabled`) unless set; records already there are still settled and refunded |
| `--pay-max-invoice-msat` | `TILL_PAY_MAX_INVOICE_MSAT` | 100,000,000 | largest invoice paid |
| `--pay-max-fee-msat` | `TILL_PAY_MAX_FEE_MSAT` | 1,000,000 | cap on the fee reserve: a larger `max_fee_msat`, or none, is this |
| `--pay-margin-msat` | `TILL_PAY_MARGIN_MSAT` | 0 | the till's margin on each pay, msat ... |
| `--pay-margin-bps` | `TILL_PAY_MARGIN_BPS` | 0 | ... plus basis points of the invoice |
| `--pay-daily-cap-msat` | `TILL_PAY_DAILY_CAP_MSAT` | 1,000,000,000 | most the till charges for pays in any 24 hours (pays not charged, failed or refunded do not count) |
| `--pay-min-expiry-secs` | `TILL_PAY_MIN_EXPIRY_SECS` | 120 | least time an invoice must have left |

The routes answer `503 lightning_unavailable` when the till's federation has no `ln` module the
wallet can pay with; `POST /v1/pay` answers it too when no gateway can pay the invoice now.

### POST /v1/pay

`{"bolt11":"<invoice>","max_fee_msat":N,"client_ref":"<1-124 bytes>","pin":"<optional, 4-8 digits>"}`,
with `X-Card-Link` (or, with the POS token only, the relay's card). `max_fee_msat` is optional (the till's cap); `pin` as for
`/v1/charge` (required on ops; never logged, stored or echoed). Use a read timeout of at least
120 s: the till charges the card, then follows the payment for up to 60 s.

1. The invoice is checked before the card is touched: it decodes (`lightning:` prefix allowed),
   has an amount, is for this till's network, has at least `--pay-min-expiry-secs` left, and is at
   most `--pay-max-invoice-msat`. Then the price (invoice + fee reserve + margin, at most
   2^40 msat as every charge), ops and the PIN, the daily cap, that no other pay
   holds the invoice and the wallet never paid it, and the gateway's quote: no gateway answering
   for the invoice is `503 lightning_unavailable`, a gateway fee above the fee reserve is
   `400 fee_reserve_too_low` (with `gateway_fee_msat`). Nothing is charged for any of these.
2. `charging` is saved, and the card is charged the price with `client_ref` `pay:<client_ref>`
   through the charge path, never offline. The card's short id is read at that tap and kept.
3. `charged`, then `paying` (saved before the payment starts), then `paid` with the preimage, or
   `pay_failed`. A payment refused with nothing funded only because the till's wallet does not
   hold the amount yet, or no gateway answers for a moment, is not a failure: the pay stays
   `charged` (`undecided` 202) and the sweeper pays it once it can, until the invoice expires.

`200 {"ok":true,"pay_id":N,"client_ref":"…","state":"paid","payment_hash":"<hex>","invoice_msat":N,"fee_reserve_msat":N,"margin_msat":N,"price_msat":N,"received_msat":N,"overpaid_msat":N,"sale_id":N,"txid":"<hex>","short_id":"<8 hex>","fee_paid_msat":N,"fee_refund_msat":N,"preimage":"<hex>","created_at_ms":T,"updated_at_ms":T,"invoice_expires_at_ms":T}`

`price_msat` is what the card was charged. `received_msat` (once `charged`) is what the card paid
the till: the price, plus `overpaid_msat` when the card's notes could not make exact change (at
most 2,048 msat, as for any charge). `fee_paid_msat` is the gateway's fee; `fee_refund_msat`, the
unspent part of the reserve, stays in the till's wallet (it is not put back on the card). The
preimage is the proof of payment: it is answered only here (and in a replay of this `POST`),
never by `GET`. No answer carries the invoice or the wallet's operation id; the PIN, the invoice
and preimages are never logged.

`client_ref` makes the whole pay idempotent:

* a finished pay replays its answer (`paid`: the same 200 with the preimage; `pay_failed`: the
  same 502), whatever the invoice's time left, the settings, ops or a PIN say now;
* a pay the card paid (or may have) that is not settled answers `undecided` 202 and the till
  drives it on: a sweeper every 30 s (and at start) follows every open pay. A charge left
  undecided is resolved without a card (accepted: the pay is `charged` and paid; never sent or
  rejected: `not_charged`), and a repeat of the `POST` does the same. Before it pays, it asks the
  wallet what it did with the invoice, so nothing is paid twice;
* a pay whose card was not charged (`not_charged`: a wrong PIN, an `insufficient` card, a card
  error) is tapped again with the same `client_ref`;
* the same `client_ref` with another invoice or another `max_fee_msat` is `conflict` 409.

`state` is `charging`, `not_charged`, `charged`, `paying`, `paid`, `pay_failed`, `refunding` or
`refunded`. Failures add `pay_id`, `state`, `price_msat`, `sale_id` and (once the card tapped)
`short_id` to the error body.

| error | HTTP | meaning | app action |
|---|---|---|---|
| `bad_request` | 400 | body, `client_ref` or `pin` malformed; nothing ran | fix input |
| `card_link_required` | 400 | a pay token sent no `X-Card-Link`; nothing ran | open the card link and send its id |
| `fee_reserve_too_low` | 400 | the gateway's quote (`gateway_fee_msat`) is above the fee reserve; nothing was charged | resend with `max_fee_msat` of at least `gateway_fee_msat` (when above the till's cap, decline) |
| `bad_invoice` | 400 | the invoice does not decode, or has no amount | ask for an invoice with an amount |
| `wrong_network` | 400 | the invoice is for another network than the till's federation | ask for an invoice on this network |
| `invoice_expired` | 400 | the invoice has expired or has less than `--pay-min-expiry-secs` left | ask for a new invoice |
| `over_limit` | 400 | the invoice is above `--pay-max-invoice-msat`, or the price above the largest charge | ask for a smaller invoice |
| `pin_required` | 400 | a till on ops was sent no `pin`; nothing ran | ask the holder for the PIN |
| `daily_cap` | 403 | this pay would take the till past `--pay-daily-cap-msat` in 24 hours; nothing was charged | decline, or try later |
| `invoice_in_use` | 409 | another pay holds this invoice, or the till's wallet already paid it; nothing was charged | do not pay it again |
| `conflict` | 409 | this `client_ref` was used for another invoice or fee reserve, or by a `/v1/charge` | use a new `client_ref` |
| `not_charged` | 409 | an earlier attempt with this `client_ref` was refused by the card | tap again with the same `client_ref` |
| `undecided` | 202 | the card paid (or may have) and the invoice is not settled yet; nothing is charged or paid again | poll `GET /v1/pay/{pay_id}`, or repeat the `POST` |
| `pay_failed` | 502 | certain the invoice was not paid; what the card paid (`received_msat`) stays in the till's wallet, owed to the holder (`state` `pay_failed`, `refunding` or `refunded`) | refund it to the card (below) |
| `lightning_error` | 502 | the wallet could not be asked about the invoice; nothing was charged | retry |
| `pay_disabled` | 503 | the till does not pay invoices (`--pay-enabled` is off) | none |
| `lightning_unavailable` | 503 | the till's federation has no usable `ln` module, or no gateway can pay the invoice now; nothing was charged | try again later |
| card and ops errors | | as `/v1/charge` (`wrong_pin`, `pin_blocked`, `insufficient`, `no_card`, `card_link_gone`, `busy`, `ops_*`, `card_limit`, …), with `state: "not_charged"` when the card was refused | as `/v1/charge` |
| `internal` | 500 | the pay could not be recorded, so the card was not charged; or anything else | show detail |

### GET /v1/pay/{pay_id}, GET /v1/pays

`GET /v1/pay/{pay_id}` (with the pay token: `GET /v1/pay/{pay_id}?client_ref=<the pay's>`, else
404) answers the record as above, without the preimage, and with the refund's
fields: `refund_owed_msat` for a `pay_failed` pay (what the till still holds for the holder),
`refund_msat` once a refund started (what is back on the card, or will be once the load in flight
lands), `refund_txid` once a load landed, and `refund_dust_msat` for a `refunded` pay;
`404 unknown_pay`.
`GET /v1/pays` (POS token only) answers `{"count":N,"total_price_msat":N,"total_invoice_msat":N,"pays":[…]}`, the
newest 200 oldest first, totals over `paid` pays. Records live in `<data_dir>/pays.json` (0600),
each state saved before the step it guards.

### POST /v1/pay/{pay_id}/refund

Empty body, with `X-Card-Link` (or, with the POS token only, the relay's card): loads a
`pay_failed` pay's value (`received_msat`: the price and any overpay) back onto the card that paid, as a holder load (the top-up's load, `docs/TOPUP.md`; recorded in ops, so on
ops only). The card's short id must be the one read at the pay's charge (the till reads it before
it charges the card, and keeps it on the charge's sale too). The till's wallet pays the load and
the mint fees.

A refund never loads more than the till holds for the holder: each tap loads what is still owed
rounded **down** to whole notes, at most 7 and at most the card's free and unkeyed slots. A card
with room for all of it is refunded in one tap. A card with too few free slots takes part: the
pay goes back to `pay_failed` with `refund_msat` (on the card so far) and `refund_owed_msat` (the
rest), and the app asks the holder to free slots (spend from the card) and taps again; each tap
loads more, never past what the card paid. Once less than the smallest note the federation issues is
still owed, the pay is `refunded`, and that remainder (`refund_dust_msat`, under one note) stays
with the till.

`200` with the record: `"state":"refunded"`, `"refund_msat":N`, `"refund_dust_msat":N` and
`"refund_txid":"<hex>"` (the last load) when done; `"state":"pay_failed"`, `"refund_msat":N` and
`"refund_owed_msat":N` after a partial load, to tap again. A refunded pay answers the same again
without touching a card.

Each refund load happens at most once: `refunding` is saved before the load, and every load is
tagged `pay-refund:<pay_id>` in `<data_dir>/holder_loads.json` and kept there once finished, with
whether the federation took it. What is on the card is the sum of the kept loads it took. A refund
cut short after its load was journalled stays `refunding`, and the next `POST` (with or without
the card) finishes that load instead of making another: kept as taken, it counts; kept as not
taken (the federation never saw it), it does not, and only what is still owed is loaded. A load
that left nothing puts the pay back to `pay_failed`. The pay's `POST /v1/pay` replay keeps
answering `pay_failed` (with `state` `refunded`).

| error | HTTP | meaning | app action |
|---|---|---|---|
| `unknown_pay` | 404 | no such pay | none |
| `not_refundable` | 409 | the pay is not `pay_failed` (paid, or not settled yet) | none, or wait for it to settle |
| `wrong_card` | 409 | the card presented is not the card that paid; nothing was loaded | ask for the card that paid (`short_id`) |
| `card_unknown` | 409 | the till did not record which card paid (only a pay recorded before the till kept the card on the charge's sale); the value stays in the till's wallet | none at the till |
| `card_full` | 409 | the card has no free slot for the refund's notes; the pay stays `pay_failed`, owed `refund_owed_msat` | spend from the card, then refund again |
| `no_refund_amount` | 422 | what is still owed makes no amount the federation's notes can make | none at the till |
| `ops_not_connected` | 503 | the till is not on a registry; a refund is a load recorded there | connect the till |
| `ops_unreachable` | 502 | the registry could not be reached; the pay stays `pay_failed` or, with its load sent, `refunding` | retry |
| card errors | | as `/v1/topup/{id}/load` (`no_card`, `card_link_gone`, `busy`, …) | as there |

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
