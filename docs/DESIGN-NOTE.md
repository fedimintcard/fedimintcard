# Design note: fedimint-card

Written 2026-10-26, after the spike ([SPIKE.md](SPIKE.md)). Fedimint references are to
`v0.13.0-beta.1`; the design works for both the current mint module and `mintv2`, which share
the note model that matters here.

## One paragraph

A fedimint-card is a JavaCard that holds the spend keys of Fedimint notes and signs spends
with them. The owner's phone does everything that needs BLS or network: it issues notes to
the card's keys, prepares the card's future change in advance, and later turns the card's
change outputs into notes. A till builds a spend transaction, streams it to the card, and the
card signs only if the change comes back to keys it holds and the value leaving the card is
the amount shown. No consensus change is needed; the spike proved the federation side on a
real J3R180. The design work is all about change, denominations and the phone's role.

## What the spike fixed

| Fact | Consequence |
|---|---|
| A note is spent with one BIP340 signature from its nonce key over the 32-byte `tx_hash()` | The card needs secp256k1 only; BLS stays on the phone |
| The card signs in about 850 ms; GEN_KEY 125 ms | One note per spend is the target; consolidation happens at home, not at the till |
| A spend transaction is 87 bytes per input plus 69 per output plus 8, and nothing else is hashed | The card can parse what it signs; SHA-256 on-card is cheap next to the signature |
| Denominations are powers of two in msat; fees are a base per input and per output; overpaying is allowed | Change is a binary decomposition, up to about 20 outputs; exact change is not required |
| Issuance to an external key is a hand-built transaction; the client's builder cannot take an external signature | The phone needs a small library on top of `fedimint-client`, which can later be upstreamed |
| The card's GlobalPlatform keys and the `Opener` install sequence are known | Personalisation is a solved operational step |

## Parties and trust

| Party | Holds | Trusted for |
|---|---|---|
| Card | spend keys, change pool, PIN, spend log | never leaking keys; refusing spends that break its rules |
| Owner phone (Fedi, or a sidecar app) | owner key, blinding keys for the change pool, the federation client | preparing change honestly; it already holds the owner's money |
| Till | its own notes and keys | showing the right amount; the card checks everything else |
| Federation | consensus | accepting valid spends; rejecting double spends |

The till is untrusted. It can show a wrong amount on its screen, as any POS can, but it cannot
take more than the card agrees to, cannot redirect the card's change, and cannot spend a note
twice. The card is a bearer device limited by PIN and spend limits, as cashucard is.

## Keys: derive, do not store

The spike's four slots become an index space. The card holds one 32-byte master secret `m`
made at personalisation and never exported, and derives

    d_i = H("fedimint-card/key" || m || i) mod n,   P_i = d_i * G,   i = 0, 1, 2, ...

`GET_PUB(i)` returns the x-only `P_i`; `SIGN` takes an index. Indices are never reused: the
card keeps a `next_index` counter and a bitmap of indices that have signed. HMAC-SHA256 and a
scalar reduction are cheap on the card. This removes the slot limit and the overwrite question,
and lets the phone enumerate the card's keys from `GET_PUB` alone.

The owner key (secp256k1, held by the phone) is set at personalisation. Anything that changes
the card's rules is a command signed by the owner key, as in cashucard: set limits, add to the
change pool, reset the PIN, wipe.

## Notes on the card

A note the card can spend is `(denomination, nonce = P_i, BLS signature)`. The card does not
need the BLS signature to sign a spend; the till needs it to build the input. So the card
stores, per note, `(i, denomination, sig48)` = 51 bytes, and a `BALANCE` command lists them.
Two hundred notes is 10 KB of EEPROM, within what the J3R180 gave cashucard. A note enters the
card's list when the phone writes it (`NOTE_ADD`, owner-signed) after issuance or after
unblinding change. A note leaves when `SIGN` consumes it. The list is a cache for the till's
convenience; the keys are what matter, and the phone can rebuild the list from the federation
if the card loses it.

## Change: the pool

The card cannot verify a blinded output by itself: the blinded message is a G1 point, and the
card has no BLS12-381. So the phone prepares change in advance.

**Filling the pool.** For each future change note `j`, the phone picks the card key index
`i_j` (the next unused ones), derives the nonce `P_{i_j}` from `GET_PUB`, picks a blinding key
`bk_j` from its own seed, and computes `B_j = blind(nonce_message(P_{i_j}), bk_j)` (48 bytes).
It writes `POOL_ADD(i_j, B_j)` to the card, owner-signed. The phone keeps `(i_j, bk_j)` so it
can unblind later. The card stores `(i_j, B_j)`: 50 bytes each; a pool of 200 is 10 KB.

**Spending.** The till asks the card for a note that covers the amount (`BALANCE`), and for
enough pool entries to express the change (`POOL_TAKE(k)` returns the next `k` entries without
consuming them). The till builds the transaction: input = the note; outputs = the till's own
blinded outputs for the amount, then the change outputs using the pool entries, each with the
denomination the till chose. It streams the transaction to the card:

    SIGN_BEGIN(index, amount_msat, fee_budget_msat)
    SIGN_DATA(chunk)...        the consensus encoding of inputs, outputs, nonce (no signatures)
    SIGN_END()                 returns the 64-byte signature, or refuses

The card hashes the stream as it arrives and parses it at the same time. It checks:

1. exactly one input, and it is its own note at `index`;
2. every output is a `MintOutput::V0` for the federation's mint instance;
3. an output whose 48-byte blinded message equals one of the offered pool entries is change;
   the card sums those denominations as `change`;
4. every other output is the till's; the card sums those as `paid`;
5. `paid <= amount_msat` and `denomination(input) - change - paid <= fee_budget_msat`;
6. PIN state and spend limits allow `paid`.

Only then does it sign `SHA-256(stream)`. It marks the matched pool entries consumed, records
`(txid, index, consumed pool entries, paid)` in the spend log, and removes the note. Check 5 is
what stops a till from pocketing the difference: value that is neither change to the card nor
within the stated amount plus fees cannot exist. The amount on the till's screen is the trust
point, exactly as with a bank card.

**After the spend.** The phone, when it next sees the card, reads the spend log, fetches the
signature shares for `(txid, out_idx)` of each consumed pool entry, unblinds with `bk_j`, and
writes the resulting notes back with `NOTE_ADD`. The card is spendable again for that value
once this has happened. Until then the change exists on the federation and is safe; it is just
not yet in the card's list. This is the same shape as cashucard's owed-change, with the
federation rather than a mint holding the value.

**Pool exhaustion.** If the pool cannot express the change, the card refuses, and the till can
offer a spend that overpays by less than the fee budget, or the holder pays with a note that
needs less change. The phone tops the pool up every time it syncs; 200 entries covers dozens of
spends.

**Why not let the card sign blind and trust the phone.** It would make the till trusted, since
only the till sees the transaction. The parse costs little: SHA-256 and a walk over 87 + 69n
bytes, streamed in 250-byte APDUs.

## Denominations and consolidation

Powers of two mean a 7,999 sat balance is thirteen notes. The card holds notes of many sizes;
the till picks the smallest note that covers the amount, and change comes back as the binary
decomposition of the remainder. After many spends the card holds many small notes. The phone
consolidates at home: it builds a reissue transaction with several card inputs and a few large
change outputs from the pool, and the card signs each input (850 ms each, fine at home). The
same flow tops the card up from the phone's own balance: the phone's notes in, pool outputs out.

A payment never needs more than one card input if the card holds a note at least as large as
the amount plus fees. The phone keeps a few large notes on the card for that reason.

## Both mint modules

The current mint module and `mintv2` have the same structure for this design: a note is a
nonce key plus a BLS signature, the input's public key is the nonce, outputs are blinded
messages with an amount. Encodings differ (tiers vs denominations, field order), so the card's
parser is a table per module, selected by the mint instance id in the transaction. Which one a
federation runs is read from its config; Fedi's federations today run the current mint module,
so the first product build targets that one and keeps `mintv2` in the tests.

## Commands (v1 of the real applet)

| Command | Who | Does |
|---|---|---|
| `GET_INFO` | anyone | version, federation id, counts, PIN state, limits |
| `GET_PUB(i)` | anyone | x-only key for index `i` |
| `BALANCE` | anyone (or PIN) | the note list `(i, denomination, sig48)` |
| `POOL_TAKE(k)` | anyone | the next `k` pool entries, not consumed |
| `SIGN_BEGIN / SIGN_DATA / SIGN_END` | PIN or limit-gated | the spend, as above |
| `LOG_READ` | owner | the spend log since the last read |
| `NOTE_ADD`, `POOL_ADD` | owner-signed | write notes and pool entries |
| `SET_OWNER`, `SET_LIMITS`, `SET_PIN`, `UNBLOCK_PIN`, `WIPE` | owner-signed or PUK | as cashucard |

Everything owner-signed carries a counter so a command cannot be replayed. The card refuses
`WIPE` while it holds notes unless the owner's command says so twice.

## Security

- **Keys.** Never leave the card; the spike's review already closed the scalar-handling paths
  in GEN_KEY and the signer's scratch. The derived-key scheme adds HMAC over `m`; `m` is written
  once and is never read.
- **Nonces.** BIP340 with 32 bytes of card randomness per signature (`aux_rand`), kept on purpose
  against fault injection. `SchnorrHW.mulModN` is not constant time; a side-channel assessment
  is needed before cards hold real money.
- **Lost card.** The phone holds `(i, bk)` for pool entries and knows every card key; it can
  sweep the card's unspent notes only if it has their BLS signatures, which it does (it issued
  or unblinded them). So a lost card is recoverable by the phone until the thief spends, which
  the PIN and limits bound. A card without a paired phone is a plain bearer instrument.
- **Compromised phone.** It already holds the owner's money. It cannot extract card keys, so
  the card's balance is only at risk through the owner-signed commands the phone can issue.
- **Malicious till.** Covered by the parse. It can refuse to serve, show a wrong amount, or
  waste pool entries by asking for `POOL_TAKE` and not spending; the card does not consume on
  take.
- **Federation.** Sees nothing it did not already see for a phone spend.

## The phone library

A Rust crate on top of `fedimint-client` and `fedimint-core`, grown from `host/fed-tests/src/lib.rs`:

- issue notes to external keys (hand-built transaction, shares, unblind), for both mint modules;
- build spend transactions for a till (note selection, change decomposition, pool use);
- build consolidation and top-up transactions for the phone;
- the pool: generate, store `(i, bk)`, refill, unblind after a spend log;
- the card protocol over any transport (bridge, PC/SC, phone NFC), from `host/card`.

It exposes a C-ABI or UniFFI surface so a phone app can call it. The pieces that touch
`fedimint-client` internals (external-key outputs, submitting a pre-signed transaction) are
the ones to upstream later as a client feature: "inputs signed elsewhere".

## Fedi first: what a demo needs

A FediMod is a web app that sees a WebLN provider and nothing else: no notes, no NFC, no
federation client. So a mod alone cannot drive a card. Three ways to an app-level demo, in
order of how much Fedi has to do:

1. **Sidecar app.** Our own mobile app (Flutter or React Native over the Rust library) joins the
   same federation with an invite code, is funded from Fedi over Lightning (the existing
   Flash-style bridge), and does the card flows. Fedi is the wallet people already have; the
   card app is a companion. No Fedi code changes. Demo in weeks.
2. **Fedi bridge hook.** Fedi's app uses a Rust bridge around `fedimint-client`. If Fedi exposes
   "issue to external keys", "submit pre-signed transaction" and the spend log sync as bridge
   calls, plus NFC in the native shell, the card becomes a Fedi feature and the library above
   slots in as a bridge module. This is the product path.
3. **FediMod with native NFC.** If Fedi adds an NFC capability to mods and a note-level API,
   the card UI could be a mod. Furthest away.

Proposal to Fedi: do 1 now as the demo, with Fedi's team watching, and agree on 2 as the goal.
The till for the demo is a laptop or an Android phone running the same library.

## Taking it to Fedimint

Go with a working demo, the spike, and two concrete asks: a client API for inputs signed
elsewhere, and a review of the change-pool design against the mint module's roadmap. Nothing
needs a module or a consensus change, which is the strongest part of the pitch. If a
card-aware federation rule is ever wanted (for example change must return to the same key), it
can be a module later; the design does not depend on it.

## Milestones

| # | Milestone | Done when |
|---|---|---|
| 1 | Applet v1: derived keys, note list, pool, streaming `SIGN` with parse, owner-signed commands, PIN | jCardSim tests for every rule above, including a till that tries each cheat; installed and timed on the J3R180 |
| 2 | Phone library: issue, pool, spend build, consolidate, sync, both mint modules | federation tests like the spike's, driven end to end: fund card, spend at a till, sync change, spend the change |
| 3 | Till: a small app (laptop and Android) that charges a card | a card pays a till over NFC against a test federation; refusals for every cheat |
| 4 | Sidecar phone app for the Fedi demo: join federation, fund from Fedi, top up card, sync | a live demo with Fedi: Fedi balance to card to till and change back |
| 5 | Review: adversarial review of applet and library, side-channel assessment of signing, limits | findings closed or filed |
| 6 | Fedimint engagement: upstream the client pieces, discussion of the design | PRs open; design discussion posted |

Each milestone is a plan, build, review and verify loop with tests as the gate, as the spike
was run.

## Open questions

1. Which mint module do Fedi's federations run today, and when do they move to `mintv2`?
2. Does the J3R180 accept `TYPE_EC_FP_PRIVATE_TRANSIENT_DESELECT`? Unobservable from outside;
   decide by instrumenting a debug build.
3. Fee budget: who sets it, and how does the till learn the federation's fee consensus? Read
   from config at the till; the card only needs the budget the till states.
4. Pool entry size could drop to a 32-byte hash of `B_j` if EEPROM gets tight; the card would
   then hash each output's blinded message to compare.
5. Should the card hold the BLS signatures at all, or should the till fetch them from the phone?
   Holding them makes the card self-sufficient at the till, which is the point of a card.
