# The POS phone as an offline till

A POS phone with no network takes a 3.0 card payment, shows "Paid offline" and what waits to sync,
and the money reaches the till's wallet when the phone next reaches the till. A genuine card
cannot pay twice with the same note. A cloned card, or a lost or rooted phone, costs at most the
phone's caps, never the merchant's proceeds already queued.

Online, the hosted till does the charge exactly as before (`docs/TILL-API.md`); nothing in the
online path changes but for a sale the till's own offline queue holds `signing` (below). This
page is how the offline path is built (ENG-841; slice 1 is ENG-842: the shared core and the
till's routes; the apps are ENG-843, iOS, and ENG-844, Android).

## Architecture

**One Rust core, used by the till and both apps.** `host/pos-core` (`fedimint-card-pos-core`) is
the till's offline charge, cut out of the sidecar: the card driver (`host/card`, the 3.0
commands), the attestation check (`GET_CERT`, `ATTEST`), the note check (`Note::verify` under the
federation's key for the tier), the planner (`plan.rs`: inputs, change, fee headroom, now with
the tiers the till's outputs may use), `SET_CHANGE` and `SIGN` per slot, and the durable queue
with the till's write order: `signing` saved, with every secret, before the card is told
anything; `queued`, with the signed bytes, before the caller is answered. It depends on
fedimint-core, fedimint-mint-common, tbs and the card crate only: no fedimint client, no API
client, no rocksdb, no registry client (`host/scripts/pos-core-mobile.sh check` proves it in CI).
It is synchronous and runs no async runtime. The till's `offline::charge` runs it in one
`on_card`, with the till's own outputs; a phone runs it with an offline kit in place of the
wallet. It builds for aarch64-apple-ios, aarch64-apple-ios-sim, x86_64-apple-ios,
aarch64-linux-android and x86_64-linux-android (`pos-core-mobile.sh`, `.github/workflows/mobile.yml`),
with Swift and Kotlin bindings through UniFFI (feature `ffi`).

**The till stays the settler.** The phone never talks to the federation or the registry. Two till
routes for paired phones (`docs/TILL-API.md`): `GET /v1/offline/kit` and
`POST /v1/offline/charges`. The till puts each charge a phone sends into its own offline queue as
`queued`; its sender reports it to the registry, writes it into the card's file, lets the
ordinary recovery send it to the federation and collect its notes, and posts the outcome, as for
the till's own offline charges. Idempotent by txid. The registry is unchanged.

**The offline kit.** Fetched while online and on every sync, and good for a week (by default)
from the fetch that answered it, a kit the till gives again included: the
federation's id and mint module instance, its note keys per tier, its mint fees and the fee
headroom, the registry's certificate key the till pins, the phone's caps (count and msat, the
till's `--offline-cap-*`), the validity, the newest block header, and a **pool of till outputs per
tier**: blinded nonces whose spend keys and blinding keys stay on the till. The phone holds
nothing that can spend the merchant's proceeds: what it queues is public data and the card's
signatures. The pool covers every tier from the smallest note the planner gives the till (4 msat
on fedimint's default fees, 1 msat otherwise; with fees the till's side takes what they leave,
and a pool from the economical denomination up could pay no price on a card with no room for
change) to the largest at or below the phone's value cap, 40 entries a tier, fewer when the kit
would pass 128 KB. A phone holds at most 3 live kits; at that cap the till gives the newest
again, its tiers topped up once one runs low. The till never reuses an entry, and drops the
unused entries of a kit past its grace, secrets and all.

**When the phone goes local.** Only before anything reached the till's charge route: the app's
`linkFailed` at card-link open. A timeout after `POST /v1/charge` was sent stays "outcome unknown"
and never falls back (the double-charge guard). Order: try the till with the quick timeout; fall
back when the link cannot open and the kit is valid and under its caps.

**A sale the local charge answered `no_card` is retried locally only** (the core's `charge`, the
same `client_ref`), never on the till's `POST /v1/charge`, while `PosCore.signing_refs()` lists
its `client_ref`. The card may have signed part of a transaction that only its next local tap on
this phone finishes; an online charge would take the sale a second time from the card's other
notes, or spend the note that transaction still needs. The apps (ENG-843, ENG-844) check
`signing_refs()` before they try the till, "Try again" included: the order above (the till
first) is not for such a sale. A `charge` that throws is retried locally too, with the same
`client_ref`. The till keeps the same rule for its own offline queue: a tap whose `client_ref`
it holds `signing` goes the offline way even while the registry and the federation answer: the
card that began the charge finishes it (answered as the sale, `offline: true`; the sender's
next pass, every 30 s, reports it), and another card is `undecided` with nothing signed.

**Caps and the attacker.** The kit's caps are enforced by the core; an expired kit stops offline
acceptance; the card's own daily limit applies as with any till. Double spends across phones and
tills are found by the registry at report, as today, and counted.

**What does not change.** Applet 3.0, the online charge path (but for a sale the till's own
offline queue holds `signing`, taken the offline way as above), the registry API and the Setup
app.

## One charge

`charge(card, queue, env, price, client_ref)` runs in this order, so a charge that cannot succeed
spends no PIN try:

1. the price (1 msat to 2^40), the `client_ref` (1 to 128 ASCII characters) and the PIN's shape
   (4 to 8 digits): `bad_request`, no APDU;
2. a charge already in the queue under this `client_ref` is that charge (`replayed`), or
   `conflict` for another price;
3. a phone's kit expiry (against the latest of the phone's clock, the kit's issue and the latest
   time the store has seen), then the count cap: no APDU;
4. a 3.0 card (`offline_unsupported_card`), its certificate and attestation (`card_not_genuine`),
   its clock (`SET_HEADER` when older; the kit's expiry again, by the card's clock), the PIN;
5. charges this card left `signing` are finished from its stored signatures: a repeat of one is
   that charge (`replayed`), and another card tapped for one is `undecided`. One that can never be
   finished (an input slot since spent by another transaction, or re-keyed) is set aside as
   `abandoned`, nothing charged for it and its pool entries never taken again (its inputs the
   card never signed leave the spent list and pay again), and the tap goes on as a new charge,
   for the same `client_ref` too;
6. the card's notes (skipping those taken offline here already, and those that do not check),
   the plan, the value cap, the card's day (`card_limit`), then `signing`, `SET_CHANGE`, `SIGN` and
   `queued`.

Step 5 fixes a double charge the till had: a card pulled after it signed some inputs, tapped
again for the same sale, used to have its first transaction finished and a second one signed.

## The phone's store

One directory in the app's protected storage: `offline_queue.json` (the till's format, 0600,
fsync, rename), `phone-id` (16 random bytes, hex, made once) and `store.lock` (an advisory lock: a
second open of the store is refused). The store must not be restored from a backup: an old queue
would take pool entries again (the till refuses them, `pool_entry_reused`, and the sale is
lost). Both apps keep the directory out of the platform's backups.

## The apps' interface

UniFFI, feature `ffi`: Swift module `FedimintCardPosCore`, Kotlin package
`org.fedimintcard.poscore`.

- `ApduTransport.transmit(apdu) -> data || SW`, implemented by the app over its NFC session
  (`NFCISO7816Tag`, `IsoDep`). An error is the card leaving the field (`no_card`). The core
  selects the applet before its first command.
- `PosCore.open(dir)`, `phone_id()`, `charge(transport, kit_json, price_msat, client_ref, pin)`
  returning `Paid { txid, price_msat, till_msat, overpay_msat, fees_msat, fee_margin_msat,
  inputs, change, replayed }` or `Refused { code, detail, suggested_msat, below_msat, above_msat,
  tries_left }`, `stats()`, `signing_refs()`, `drain(max)`, `mark_synced(answer_json)`,
  `dismiss(txid)`.
- Every method but `phone_id()` throws `PosCoreError` (`Store`, `Answer`, or `Internal` for a
  failure the core did not expect, a panic caught); a refusal is `Refused`, not an error. A
  `charge` that throws has an unknown outcome, as `no_card`: retry it locally with the same
  `client_ref`.

**Threading.** Every call blocks until it is done, and `charge` waits on the card (each SIGN is
about a second on a J3R180). `transmit` is called on the thread that called `charge`, and waits
for the tag's answer there: call `charge` from a worker thread or queue of the app's own, never on
the NFC delegate's queue (iOS) or the NFC callback's thread (Android), where the tag's completion
handler runs and would wait behind it forever. One `PosCore` per store (a second open of the
store fails). Its charges run one at a time: a `charge` called while another runs waits for it,
so the two never plan from the same pool entries; the other calls take the queue's lock only
briefly.

**Sync.** On reconnect, on app foreground and after every online charge: `drain(50)`, post its
`body` to `POST /v1/offline/charges` while it lists charges, pass the answer to `mark_synced`, and
fetch the kit again. `stats().waiting` plus the till's queue is "N payments waiting to sync".
Charges the till refused are listed in `drain().refused` with their code until `dismiss`. The
phone's clock at a sync only decides which synced charges are old enough to leave the store; the
store's latest-seen time (which an expired kit is judged by) moves only with a charge, and never
past its kit's expiry.

## Refusals

The core answers the till API's codes (`docs/TILL-API.md`); the till's `classify` and the
phone's `Refused.code` come from one place (`pos_core::refusal`). A code the app does not know
is shown as its generic card error.

| code | when | the app shows |
|---|---|---|
| `bad_request` | the price, `client_ref` or PIN's shape; nothing sent to the card | fix the input |
| `conflict` | this `client_ref` was a sale of another price | use a new `client_ref` |
| `undecided` | this `client_ref`'s first card was taken away while it signed | ask for that card again |
| `offline_not_ready` | no kit yet, or one this phone cannot use | "Till service unreachable": no kit yet |
| `offline_kit_expired` | the kit is past its expiry (phone's clock or card's) | kit expired: reach the till |
| `offline_cap_reached` | the phone's caps on what waits to sync | caps reached: reach the till |
| `offline_unsupported_card` | an applet 2.x card | this card pays online only |
| `card_not_genuine` | not a card the registry certified (a clone) | decline |
| `offline_note_invalid` | notes that do not check, and the rest do not pay | a card the kit cannot verify |
| `offline_no_change_room` | needs change the card has no fresh slot for | "Try N sats" (`suggested_msat`) |
| `offline_pool_exhausted` | the kit has run out of a tier the price needs | "Try N sats", then a new kit |
| `wrong_pin` | the card refused the PIN (`tries_left`) | ask again |
| `pin_blocked` | the PIN is blocked | the card cannot pay |
| `card_limit` | the card's daily limit | decline |
| `insufficient`, `no_exact_plan` | the card's notes do not make the price | decline |
| `no_card` | the card left the field | tap again, same `client_ref` |
| `card_error`, `internal` | anything else | show `detail` |

## Risks

1. A phone gives change to a KEYED slot whose key the till knows carried a note before (a dead
   change key); the registry refuses that change at report (409) and the sale is refused, its notes
   spent and the merchant unpaid. The till filters such slots from its copy of the card; a phone
   has none. Fixed at the source by ENG-847 (online taps renew dead KEYED slots first).
2. A store restored from a backup reuses pool entries: excluded from backups by the apps.
3. Pool secrets live only in the till's `offline_pool.json` and queue: a lost data directory loses
   the proceeds of charges not yet synced, and a sync after the grace finds the secrets wiped.
4. The kit travels under the shared POS token, perhaps over plain http on a LAN: a swapped
   certificate key or note keys let the phone accept clones; the till re-checks every charge, so
   the loss stays within the phone's caps. Per-phone pairing tokens are ENG-848.
5. A card pulled between `SET_CHANGE` and `SIGN` that never returns strands its CHANGE slots, as on
   a till.
6. A lost phone loses its unsynced queue, as a lost till does; the caps bound it.
