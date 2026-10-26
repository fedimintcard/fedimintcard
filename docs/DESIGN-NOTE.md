# Design note: fedimint-card

Written 2026-10-26, after the spike ([SPIKE.md](SPIKE.md)), revised the same day after an
adversarial design review. Fedimint references are to `v0.13.0-beta.1`. The design covers both
the current mint module ("v1") and `mintv2` ("v2"); the spike tested v2 only.

## One paragraph

A fedimint-card is a JavaCard that holds the spend keys of Fedimint notes and signs spends
with them. The owner's phone does everything that needs BLS or network: it issues notes to
the card's keys, prepares the card's future change in advance, and later turns the card's
change outputs into notes. A till builds a spend transaction and streams it to the card; the
card parses it, computes the fees itself, and signs only if everything that leaves the card
is either the stated amount or change to keys it holds. No consensus change is needed; the
spike proved the federation side on a real J3R180. The design work is about change,
custody, denominations and the phone's role, and the hard limits are stated below rather
than hidden.

## What the spike fixed

| Fact | Consequence |
|---|---|
| A note is spent with one BIP340 signature from its nonce key over the 32-byte `tx_hash()`; v1 and v2 agree on this (`spend_key` is the nonce in both) | The card needs secp256k1 only; BLS stays on the phone |
| The card signs in about 850 ms; GEN_KEY 125 ms; GET_PUB 27 ms | A tap is one or two signatures; consolidation happens at home |
| `tx_hash` covers inputs, outputs and the 8-byte nonce, nothing else; the server re-encodes what it decoded | The card can hash the stream it parses and the till cannot submit anything else under that signature |
| v2: 87 bytes per input, 69 per output, 1-byte denomination; v1: variable (see the module table) | Parsing is cheap next to the signature |
| Fees are base plus ppm per input and per output in both modules; overpaying is accepted and the excess burned (core consensus 2.1) | The card must compute fees itself or a till can burn a note |
| Resubmitting an identical transaction is idempotent; a note's nonce enters a spent set | A till cannot double-charge with one signature; a transaction never expires |
| Issuance to an external key is a hand-built transaction; the client's builder cannot take an external signature | The phone needs a small library on top of `fedimint-client`, to upstream later |

## Parties and trust

| Party | Holds | Trusted for |
|---|---|---|
| Card | the master secret, note list, change pool, PIN, spend log, fee and limit config | never leaking keys; refusing spends that break its rules |
| Owner phone (Fedi, or a sidecar app) | owner seed (which also yields the card's master secret and the pool blinding keys), the federation client | preparing change honestly; it already holds the owner's money |
| Till | its own notes and keys | showing the right amount; the card checks everything else |
| Federation | consensus | accepting valid spends; rejecting double spends |

The till is untrusted. It can show a wrong amount on its screen, as any POS can, and it can
hold a signed transaction and submit it later. It cannot take more than the stated amount,
cannot burn the card's value beyond a dust allowance, cannot redirect change, and cannot spend
a note twice. The card is a bearer device bounded by PIN and cumulative limits.

## Custody: the phone can rebuild the card

Decision: the card's master secret `m` is derived by the phone from the owner seed,
`m = KDF(seed, "fedimint-card", card_id)`, and injected once at personalisation. NFC is plaintext and
there is no owner key yet at that moment, so the injection is encrypted: the card makes an
ephemeral EC key, the phone does ECDH against it and sends `m` under AES, and the card's
ephemeral public key is confirmed out of band (a contact reader in a trusted setting, or the
card's attestation if the chip offers one). After that `m` is held in a key object on the card
and is never read out. This is the hardware-wallet model: a lost card is swept by the phone, which knows
every card key, racing the thief within what the PIN and limits allow. The alternative,
card-only custody where `m` is born on the card and a lost card is lost cash, is simpler and
leaks nothing at personalisation; it is the right choice for low-value gift cards and can be
a personalisation option. In that mode the phone cannot cancel an unsubmitted transaction by
reissue (it has no key), so the card offers an owner-signed `NOTE_RESTORE` that brings a
logged input back for an owner-mode reissue only. The product default is the recoverable one.

Pool blinding keys are derived the same way, `bk_j = KDF(seed, "fedimint-card/pool", card_id,
i_j)`, so restoring a phone from its seed restores the pool and the ability to unblind.

## Keys: derive, do not store

The card derives per-note keys from `m` with the primitives the applet already has (SHA-256
and SchnorrHW's `reduceModN`, `cmp32`, `sub32`):

    d_i = tagged_sha256("fedimint-card/key", m || i || ctr),  ctr = 0, 1, ... until 0 < d_i < n
    P_i = d_i * G

`GET_PUB(i)` returns the x-only `P_i`. Indices are allocated by the card only (`ALLOC(k)`
returns the next `k` indices from a monotonic counter) and never reused; the counter is the
proof, and each live note or pool entry carries one state byte. The phone never chooses an index, which closes the nonce
collision that would make a change note unspendable (the federation's spent set is keyed by
nonce). The card stores `x(P_i)` with each note so a spend does not need a 125 ms point
multiplication to check that the input's nonce is its own. Each `SIGN` loads `d_i` into the
signing key object; if the card refuses the transient key type that is one EEPROM write per
signature, which is acceptable wear.

Owner commands are authenticated with native `Signature.ALG_ECDSA_SHA_256` over the owner's
public key, since the card has no point addition for BIP340 verification. Every owner command
carries a counter, so none can be replayed.

## Notes on the card

A spendable note is `(denomination or amount, nonce = P_i, BLS signature)`. The card stores
per note: index `i` (2 bytes), amount in msat (8 bytes), `x(P_i)` (32 bytes), and the BLS
signature (48 bytes), 90 bytes. The signature is for the till's benefit; the keys are what
matter. `NOTE_ADD` is owner-signed, and the card checks the index is one it allocated and has
not seen before. The list is capped by measured free memory (see EEPROM).

## Change: the pool

The card cannot verify a blinded output by itself: the blinded message is a G1 point. So the
phone prepares change in advance.

**Filling.** The phone calls `ALLOC(k)`, derives `P_i` for each new index with `GET_PUB`,
picks `bk_j` from its seed, and for each entry chooses the denomination `d_j` it will carry.
It computes `B_j = blind(msg(P_i), bk_j)`, where `msg` is `from_bytes_sha256(nonce)` in v2
and `from_bytes(nonce)` in v1, and writes `POOL_ADD(i, d_j, B_j)` owner-signed. The phone
builds a binary ladder of denominations, two of each power from the dust allowance (below) up
to the largest note it keeps on the card, so that any remainder above the allowance can be
expressed. A pool entry is 2 + 8 + 48 = 58 bytes.

**Spending.** The flow is a session:

    SIGN_BEGIN(amount_msat)                         opens a session; PIN or limits apply
    BALANCE                                         the note list, only inside a session
    POOL_TAKE(remainder_msat)                       one entry per set bit of the remainder, with a slot number each
    SIGN_DATA(chunk)...                             the consensus encoding of inputs, outputs, nonce
    SIGN_END(slot hints)                            returns one signature per card input, or refuses

A session that ends without `SIGN_END`, or whose `SIGN_END` refuses, retires every pool entry
it revealed: the card marks them dead and logs them so the phone replaces them. Revealing an
entry is therefore as final as spending it. This closes the skimmer who opens a free session,
reads the pool, and submits the blinded messages as dust outputs on the federation (v1 would
then refuse the holder's real change). The failure mode becomes "pool runs dry, card refuses",
which fails closed, and the phone checks pool liveness at sync anyway. Each opened session
also counts a fixed step against the cumulative limit, so a skimmer cannot drain the pool by
opening sessions in a loop. A no-PIN session does expose the note list to whoever opened it;
that is the privacy cost of no-PIN taps and is stated to the holder.

The till picks the card inputs (one, or several if no single note covers the amount), builds
its own outputs for the amount, and the change outputs from pool entries whose denominations
sum to the remainder. The card hashes the stream as it arrives and parses it. It checks:

1. every input is a mint input on the personalised module instance, its nonce is `02||x(P_i)`
   for a note the card holds, and no index appears twice; `k` inputs mean `k` signatures;
2. every output is a mint output on that instance; unknown variants, length mismatches,
   other instances, and trailing bytes after the 8-byte transaction nonce are refused;
3. an output whose blinded message equals an offered pool entry, and whose denomination equals
   that entry's `d_j`, is change; each entry matches at most once; the card sums `change`;
4. every other output is the till's; the card sums `paid`;
5. fees are computed on the card from the stored fee consensus: `fee = sum over inputs and
   outputs of (base + amount * ppm / 1e6)`; the card requires
   `input_total - change - paid - fee <= dust_allowance`, where the allowance is a
   personalisation parameter, by default the larger of 2^9 msat and 4 × base rounded up to a
   power of two (v1 calls this the minimum economical denomination; v2 has no such constant,
   so the card computes it). A fee-free federation would otherwise get an allowance of 1 msat
   and no pool denomination small enough to match, and could not spend at all;
6. `paid <= amount_msat`, and the cumulative spend counter plus `input_total - change` stays
   within the limits set by the owner.

Several signatures do not fit one short APDU response, so `SIGN_END` answers with `61xx`
chaining (or extended length where the reader allows it). If all hold, `SIGN_END` performs
one atomic JavaCard transaction: remove the input notes, mark
the matched pool entries consumed, append a log entry `(txid, paid, [(out_idx, i, d_j)] for
each change output, [i] for each input)`, advance the spend counter, and only then return the
signatures. A tear before the commit leaves the card unchanged and no signature released; a
tear after it is "signed, status unknown": the card has committed, but the phone cannot tell
whether the till received the signatures. The phone treats every such entry as a live cheque
and runs the cancel-by-reissue rule for it (one signature per input, plus fees, at the next
tap), which is the price of never re-adding a note that might be spent.

Check 5 is what stops a till from pocketing or burning the difference. The amount on the
till's screen is the trust point, as with a bank card. `amount_msat` is bounded by the limits;
there is no fee budget for the till to inflate.

**After the spend.** At the next sync the phone reads the log (`LOG_READ`, cleared only by an
owner-signed acknowledgement), fetches the signature shares for each `(txid, out_idx)`,
verifies them with the public key share for `d_j`, unblinds with `bk_j`, and writes the notes
back with `NOTE_ADD`. The phone keeps `bk_j` until it has seen the change note's nonce either
spent or in the card's list.

**A transaction the till never submits.** Fedimint transactions never expire, so a signed
transaction is a live cheque. The card has already removed the note and consumed the entries.
The phone must not re-add the note: it cancels the cheque by spending the same note itself in a
reissue transaction (it holds the key), which makes the till's copy a double spend. The race
is harmless to the holder: if the till's transaction lands first, the phone's reissue fails
with "already spent", `paid` was within the agreed amount, and the phone then claims the
change of the till's transaction from the log's `(out_idx, i, d_j)`; if the reissue lands
first, the merchant goes unpaid, which is the merchant's risk for delaying submission. The
phone never forgets the pool entries of a pending transaction until one of the two is final,
and `bk_j` is re-derivable from the seed. Entries revealed in the unsubmitted transaction are
retired afterwards because the till may still submit it later (and in v2 a second use would be
signed again and burned). The phone waits one sync cycle after reading the log before it
cancels, so a slow but honest till is not undercut.

**"Tap again."** A till that claims failure and asks for a second tap gets a second signature
on a different note and can submit both. The cumulative limit, not a per-transaction one,
bounds this: the card has no clock, so limits are counters that only an owner sync resets.

**Pool exhaustion and the offline limit.** Change is unspendable until the phone has unblinded
it. A card used without its phone spends down its note list and does not get change back; the
design accepts this and states it. The phone keeps several large notes on the card and refills
the pool at every sync. If the pool cannot express the remainder, the card refuses, and the
till may offer a transaction that burns at most the dust allowance.

**Why not let the card sign blind and trust the till.** It would make the till trusted, since
only the till sees the transaction. The parse costs little next to the signature.

**Why the till sees the pool at all.** `POOL_TAKE` is only available inside a `SIGN` session
that the PIN or the no-PIN limit has opened, and `BALANCE` likewise, so a skimmer cannot read
nonces or burn pool entries in v1 by submitting them as dust outputs. The phone also checks
pool liveness at each sync (`BLIND_NONCE_USED` in v1, the recovery endpoints in v2) and
retires anything already seen.

## Denominations, several inputs, consolidation

Powers of two mean a 7,999 sat balance is thirteen notes. The till picks the smallest set of
notes that covers the amount; several inputs cost one signature each (about 850 ms). Change
comes back as the binary decomposition of the remainder, matched against pool denominations.
After many spends the card holds many small notes; the phone consolidates at home with an
owner-mode `SIGN` session (no amount, no limits, all outputs must be change) that reissues
several card notes into a few large pool entries. Top-up from the phone's own balance does not
pass through `SIGN` and does not need the card present: the phone uses indices it allocated
earlier that are not pool entries, builds and submits the transaction itself, and writes the
notes with `NOTE_ADD` at the next tap. (`POOL_CONSUME` exists for the rare case where the
phone wants to use pool entries directly.)

## Both mint modules, precisely

| | v1 (`fedimint-mint`) | v2 (`fedimint-mintv2`) |
|---|---|---|
| Input | `V0 { amount: Amount (BigSize, 1/3/5/9 bytes), note: { nonce: 33, signature: 48 } }` | `V0 { note: { denomination: 1, nonce: 33, signature: 48 } }` |
| Output | `V0 { amount: Amount, blind_nonce: 48 }` | `V0 { denomination: 1, nonce: BlindedMessage 48, tweak: 16 }` |
| Amounts | tiers read from the federation config (the default generator gives about 57 powers of two) | `Denomination(n)` = 2^n msat, n from 0 to 41 |
| BLS message | `Message::from_bytes(nonce)` | `Message::from_bytes_sha256(nonce)` |
| Duplicate blinded message | refused at submission (in consensus only warned, so a malicious guardian could burn it) | signed again (burns the duplicate) |
| Fees | base + ppm per input and output | same |
| Spend key | the nonce | the nonce |

The card is personalised with the federation id, the mint module instance id and its kind,
and the fee consensus. The parser is a table per kind. Fee or tier changes in the federation
require re-personalisation (an owner command). Neither module rotates signing keys by epoch at
this pin; if that changes, notes would need reissuing and the phone would drive it. A
federation migrating from v1 to v2 means reissuing every card note; the phone can do that
with the consolidation flow. A card is bound to one federation; a second federation is a
second personalisation on a different card, since one key space across federations would
reuse nonces.

## Commands (v1 of the real applet)

| Command | Who | Does |
|---|---|---|
| `GET_INFO` | anyone | version, federation id, counts, PIN state, limits, free memory |
| `GET_PUB(i)` | owner session | x-only key for an allocated index |
| `ALLOC(k)` | owner session | allocate `k` fresh indices |
| `SIGN_BEGIN`, `BALANCE`, `POOL_TAKE`, `SIGN_DATA`, `SIGN_END` | spend session (PIN or no-PIN limit) | the spend, as above |
| `LOG_READ`, `LOG_ACK` | owner session | the spend log; cleared only on acknowledgement |
| `NOTE_ADD`, `POOL_ADD`, `POOL_CONSUME`, `NOTE_SWEEP`, `NOTE_RESTORE` | owner-signed | write notes and pool entries; mark entries used; drop notes the phone has swept; bring a logged input back for an owner-mode reissue |
| `PERSONALISE`, `SET_LIMITS`, `SET_PIN`, `UNBLOCK_PIN`, `WIPE` | owner-signed or PUK | as cashucard; `WIPE` refuses while notes remain unless the command says so twice |

The PIN is entered on the till, an untrusted device. The no-PIN limit is therefore the
skimmer's reach, and the PIN protects only what is above it; a holder who enters the PIN on a
hostile till exposes the cumulative limit, nothing more. This is cashucard's model and is
stated to the holder.

## EEPROM

The J3R180 left cashucard about 12 KB after install. This applet is a quarter of cashucard's
size, but its lists are larger. Budget at 90 bytes per note and 58 per pool entry: 64 notes
and 64 pool entries is 9.5 KB plus the log (about 130 bytes per spend; the card refuses
`SIGN_BEGIN` when the log is full, so the phone must sync before it fills). The real numbers
come from `JCSystem.getAvailableMemory` after install; the lists are sized from that measurement, and
`GET_INFO` reports it. Wear is not the constraint (about ten page writes per spend is decades
at ten spends a day); tearing is, so the atomic write set of `SIGN_END` is specified above and
must fit the commit buffer.

## Security summary

- **Keys** never leave the card after personalisation. The spike's review closed the scalar
  paths in GEN_KEY and the signer's scratch. `m` lives in a key object.
- **Nonces.** BIP340 with 32 bytes of card randomness per signature, kept on purpose against
  fault injection. `SchnorrHW.mulModN` is not constant time; a side-channel assessment is a
  milestone, not a footnote.
- **Lost card.** Swept by the phone (custody decision above), racing the thief within the limits.
- **Lost phone.** Restored from the seed: `m`, every `bk_j` and the owner key come back; the
  change still on the federation is found with the recovery-by-blinded-message endpoints.
- **Malicious till.** Covered by checks 1-6, the atomic commit, the cumulative limits, and the
  cancel-by-reissue rule for unsubmitted transactions.
- **Privacy.** `BALANCE` and pool entries are visible only inside a session, and a no-PIN
  session can be opened by any reader within the no-PIN limit; revealed pool entries are
  retired, but the note list is seen. A till learns the notes it spends, as any payee does.
- **Federation.** Sees nothing it did not already see for a phone spend.

## The phone library

A Rust crate on top of `fedimint-client` and `fedimint-core`, grown from `host/fed-tests/src/lib.rs`:

- issue notes to external keys (hand-built transaction, shares, unblind), for v1 and v2;
- the pool: allocate, derive `bk_j`, choose the denomination ladder, refill, retire;
- build spend transactions for a till (note selection, change decomposition, slot hints);
- build consolidation and top-up transactions; the cancel-by-reissue rule;
- sync: read the log, fetch shares, unblind, write notes, check pool liveness, sweep a lost card;
- the card protocol over any transport (bridge, PC/SC, phone NFC), from `host/card`.

It exposes a UniFFI surface for phone apps. The pieces that touch `fedimint-client` internals
(external-key outputs, submitting a pre-signed transaction) are the ones to upstream as a
client feature: "inputs signed elsewhere".

## Fedi first

As far as this repository can tell, a FediMod is a web app given a WebLN provider (and
possibly a Nostr signer) by the host, with no access to notes, NFC or the federation client.
Three ways to an app-level demo, in order of how much Fedi has to do:

1. **Sidecar app.** Our own mobile app over the Rust library joins the same federation with
   an invite code, is funded from Fedi (over Lightning through a gateway, or by ecash export if
   Fedi allows it), and does the card flows. No Fedi code changes. Demo in weeks.
2. **Fedi bridge hook.** If Fedi's app exposes "issue to external keys", "submit pre-signed
   transaction" and the sync flow from its client layer, plus NFC in the native shell, the
   card becomes a Fedi feature and the library slots in. This is the product path.
3. **FediMod with native NFC and a note-level API.** Furthest away.

Questions for Fedi, to settle before the demo: which mint module their federations run and
when v2 lands; what a mod can access today; whether ecash can be exported to a sidecar without
a Lightning hop; whether their client layer could host the library; and what demo they would
want to see first (top-up, tap at a till, change coming back).

Proposal: build 1 with Fedi watching, and agree on 2 as the goal. The till for the demo is a
laptop or an Android phone running the same library.

## Taking it to Fedimint

Go with a working demo, the spike, and concrete asks: a client API for inputs signed
elsewhere; a review of the change pool against each mint module (duplicate blinded messages
in v2, blinded-message reuse rules in v1); and their view on key rotation and tier changes.
Nothing needs a module or a consensus change. If a card-aware federation rule is ever wanted,
it can be a module later; the design does not depend on it.

## Milestones

| # | Milestone | Done when |
|---|---|---|
| 1 | Wire spec frozen: APDUs, parse tables for v1 and v2, the atomic write set, limits semantics | written, with differential tests of the Java parser against fedimint's decoder for both modules |
| 2 | Applet v1: derived keys, note list, pool, session `SIGN` with parse and on-card fees, owner commands (ECDSA), PIN, cumulative limits | jCardSim tests for every check, including a till that tries each cheat in this note; tearing tests; installed, timed and memory-measured on the J3R180 |
| 3 | Phone library: issue, pool, spend build, consolidate, top-up, sync, sweep, both modules | federation tests end to end for v1 and v2: fund card, spend at a till, sync change, spend the change; cancel an unsubmitted transaction; restore phone from seed and sync again |
| 4 | Till app (laptop and Android) | a card pays a till over NFC against a test federation; every cheat refused |
| 5 | Sidecar phone app for the Fedi demo | live demo: Fedi balance to card to till and change back |
| 6 | Review: adversarial review of applet and library; side-channel assessment of signing; skimmer reach under the no-PIN limit | findings closed or filed |
| 7 | Fedimint engagement: upstream the client pieces; post the design | PRs open; discussion posted |

Each milestone is a plan, build, review and verify loop with tests as the gate, as the spike
was run.

## Open questions

1. Which mint module do Fedi's federations run, and when do they move to v2?
2. Does the J3R180 accept `TYPE_EC_FP_PRIVATE_TRANSIENT_DESELECT`? Decide with a debug build.
3. Should the phone be able to release `bk_j` to a trusted till so change is spendable without
   a sync? It trades privacy for offline use; off by default.
4. The ppm fee term needs 64-bit multiply and divide on a 16-bit VM; cheap to implement, but
   confirm the cost on silicon.
5. Is 64 notes and 64 pool entries enough for daily use between syncs? Measure after install.
6. The session step against the cumulative limit: how large, so that honest retries are not
   penalised but a skimmer loop is?
