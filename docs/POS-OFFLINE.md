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

**A sale the local charge answered `undecided` (its card was taken away while it signed) is
retried locally only** (the core's `charge`, the same `client_ref`), never on the till's
`POST /v1/charge`, while `PosCore.signing_refs()` lists its `client_ref`. The card may have signed
part of a transaction that only its next local tap on
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
  (`NFCISO7816Tag`, `IsoDep`). An error is the card leaving the field (`no_card`, or `undecided`
  once the card had signed an input of this sale). The core
  selects the applet before its first command.
- `PosCore.open(dir)`, `phone_id()`, `charge(transport, kit_json, price_msat, client_ref, pin)`
  returning `Paid { txid, price_msat, till_msat, overpay_msat, fees_msat, fee_margin_msat,
  inputs, change, replayed }` or `Refused { code, detail, suggested_msat, below_msat, above_msat,
  tries_left }`, `stats()`, `signing_refs()`, `drain(max)`, `mark_synced(answer_json)`,
  `dismiss(txid)`.
- Every method but `phone_id()` throws `PosCoreError` (`Store`, `Answer`, or `Internal` for a
  failure the core did not expect, a panic caught); a refusal is `Refused`, not an error. A
  `charge` that throws has an unknown outcome, as `undecided`: retry it locally with the same
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

## The iOS app

`ios-pos/` (ENG-843).

**Build.** `ios-pos/scripts/build-poscore.sh` runs `host/scripts/pos-core-mobile.sh ios` and
`bindings swift` in one run, joins the two simulator libraries (`lipo`), and writes the package
`ios-pos/Packages/FedimintCardPosCore/`: `FedimintCardPosCoreFFI.xcframework` (the device library
and the simulator one, each with the FFI header and its module map) and the generated
`Sources/FedimintCardPosCore/FedimintCardPosCore.swift`. Both are gitignored; `Package.swift` is
in git. Run the script before `xcodegen generate` and the first build, and again after any change
under `host/pos-core` or `host/card`: a library and bindings from different builds stop the app
(UniFFI's checksum `fatalError`) at its first call into the core, online too. With no core
built, resolving the package stops the build and names the script. The package is its own Swift
module, so the generated names (`TransportError`, `PhoneStats`) stay apart from the app's; only
`OfflinePhone.swift` imports it, and the rest of the app and the tests see the core through the
`OfflineCore` protocol. No CI job builds the app.

**The store.** `Application Support/PosCore`, in the complete-protection file class and excluded
from backups (`isExcludedFromBackup`). Both are read back at every launch before the store opens;
offline stays off, and Settings says why, when either does not hold. The simulator has no data
protection and reads back no class; a device must read back complete. The store opens once per
process, off the main thread, and a failure is tried again at the next foreground. The kit is
`kit.json` in the same directory (written atomically, complete protection), with the till it came
from: pairing another till drops it; forgetting the till keeps it, for a held sale and for pairing
that till again. Every call into the core runs on one serial queue of the app's, never on the main
queue, where the NFC tag answers; an APDU the card does not answer in 15 s is lost.

**When the phone takes the card.** At `linkFailed` of a charge only, in the same NFC session, when
the till is not serving (no connection, the link's quick timeout, a handshake that fails, a 5xx),
no earlier attempt of the sale was sent to the till's charge route at all, and the kit is there,
unexpired and under its caps. A till on ops refuses from its card checks (`no_card`, `wrong_pin`)
before it looks at an earlier attempt with that `client_ref`, so a refusal proves nothing about
the attempt before it. Once a `POST /v1/charge` was sent for the sale, whatever it answered (a
refusal, a 500, a 5xx the app cannot read, a code it does not know, no answer at all), only the
till's own `client_ref` guard stops a second charge, so a retry that cannot reach the till shows
the unreachable screen and takes nothing. A 401 or 403 never falls back: the pairing is dead. An
unresolved sale whose retry cannot reach the till stays on the unreachable screen with the lost
reply's copy, and "Outcome unknown" is as it was. Otherwise the unreachable screen says which: no
kit yet, the kit expired, or the caps reached. Balance reads and top-ups never fall back. The
till's own answers with the codes of its offline path (`offline_cap_reached`, `offline_not_ready`,
`offline_note_invalid`) read as before, in the till's words: the phone's copy for them is about
the phone's kit and queue.

**Local only.** A sale in `signing_refs()`, or whose last local charge answered `undecided` or
threw, is retried with `PosCore.charge` only, with the same `client_ref`, whatever the till's
health: "Try again" opens an NFC session with no card link, and the till never sees the sale
("Payment not finished": hold the same card to the phone, it won't be charged twice). The caps do
not hold such a retry back, and it fetches a new kit first when its kit has expired and the till
answers. The sale stays held whatever its retry answers: the core refuses a retap at its card
checks (`no_card`, `card_error`, `wrong_pin`, `pin_blocked`, `offline_unsupported_card`,
`card_not_genuine`) before it reaches the held charge, and `signing_refs()` still lists it. So
there is no Back, Cancel on a wrong PIN goes back to the held sale, and Try again stays on the
phone. Abandon only leaves the screen, and its alert says the card's next tap on this phone still
finishes the sale (the owner's decision, 2026-11-10; ENG-854 is the follow-up that names the
finished sale). After a restart the home screen says how many sales wait for their cards
(`stats().signing`).

**Kit and sync.** Off the status poll, while the till answers. The kit is fetched when none is
kept, when the kept one is 10 minutes old, after a sync that synced a charge, and after a local
`offline_kit_expired`, `offline_pool_exhausted` or `offline_cap_reached`; a fetch that failed
waits a minute before the next. A kit is kept only when it is `ok:true`, for this phone, and of
the till's federation. The sync runs on reconnect, on foreground and after every charge, until a
drain finds nothing waiting: up to 20 rounds of `drain(50)`, its body to
`POST /v1/offline/charges`, the 200 answer to `mark_synced`. It stops on an HTTP error, a round
the till took none of, or charges the till does not know, then waits a minute. A till answers
`pool_entry_unknown`, `bad_note` or `bad_charge` for a charge taken with another till's kit (pool
entries it never issued, notes of another federation). The till a charge was taken for never does
within the charge's redeem window: the core checked the notes against that till's kit, and the
pool entries stay that till's. So the phone does not take those answers as final: the charge
keeps waiting, Settings says "N payments wait for the till they were taken for", and it syncs
once the phone is paired with that till again. Any other refusal is as the till says: final, or
sent again when it says `retry`. Both talk to the till the poll reached, whatever is paired by
the time they ask. After a sync that synced, the till's status is read again, so the home
screen's count (the till's queue plus the phone's) moves once.

**Settings.** The kit's age, its expiry and the caps; the payments waiting to sync, held for their
cards and synced, or why offline is off; why the last sync stopped short ("Sync stopped: the till
did not answer"), until a sync finds nothing waiting; and each charge the till refused, with its
code and Dismiss behind "Delete this record?". Pairing another till, or forgetting this one, with
payments on the phone asks "Unpair anyway?" first; they stay on the phone, and sync only to the
till they were taken for.

**Limits.** 50 or more charges for another till at the head of the queue hold back the charges
taken since with the new till's kit. A charge its own till no longer knows (synced after its
redeem window) waits for good instead of being listed refused.

**The owner's pass on the Max.**

1. Paired online, Settings shows the kit and its caps.
2. Airplane mode: a sale says "Paid offline · N sats", and the home screen counts it waiting.
3. Back online: the count moves from the phone to the till in one step, and the till lists the
   sale.
4. The card pulled mid-SIGN: "Payment not finished". Wi-Fi on, Try again with the same card
   finishes it on the phone; the till sees no `POST /v1/charge` for it.
5. The clock set 8 days ahead, airplane mode: "The offline kit has expired", and nothing is asked
   of the card.
6. The phone locked mid-tap: the sale is held or not taken, never taken twice.
7. The file class of the core's files (`offline_queue.json`, `phone-id`, `store.lock`) and of
   `kit.json`: complete.
8. With a second till at hand, pair it while payments wait (they keep waiting, "N payments wait
   for the till they were taken for"), then pair back (they sync).

## Refusals

The core answers the till API's codes (`docs/TILL-API.md`); the till's `classify` and the
phone's `Refused.code` come from one place (`pos_core::refusal`). A code the app does not know
is shown as its generic card error.

| code | when | the app shows |
|---|---|---|
| `bad_request` | the price, `client_ref` or PIN's shape; nothing sent to the card | fix the input |
| `conflict` | this `client_ref` was a sale of another price | use a new `client_ref` |
| `undecided` | the card was taken away while it signed (its next tap here finishes the sale), or this `client_ref` waits for the card that was | tap that card again, same `client_ref` |
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
| `no_card` | the card left the field before it signed anything | tap again, same `client_ref` |
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
