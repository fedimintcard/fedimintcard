# Offline payments: research and recommendation

Status: research, 2026-11-07. Nothing here is built. Question from the owner: for a remote village
with no internet for days, with some trust in the community, a low-to-moderate tolerance for
double-spends and a low expected risk of chip cloning or tampering, which design should card
payments use offline: the card refusing to double-spend (option 1) or value locked to a
co-signer at top-up (option 2)? Caps with store-and-forward (option 3) apply either way.

Sources: Fedimint `v0.13.0-beta.1` (`f9dccb665`, cargo checkout) for protocol facts; this repo's
applet and till for card and till facts; public sources for prior art (linked; secondary sources
are marked).

## Recommendation

**Option 1, layered on option 3.** The card signs each note's spend once; the till verifies the
note and the card offline, queues the signed transaction and submits it at the next sync; caps
bound what a broken card or a lost till can cost. Option 2 is sound and Fedimint supports it
without protocol changes, but it ties a note to one co-signer chosen at top-up, which in a
village means one coordinator for everyone, and it costs the most card work for a threat this
model rates low. Keep it in reserve for a village that already runs a single local hub.

## What the till can do offline today

Nothing that moves money. Every charge, balance and top-up needs the registry
(`ops_load` refuses a stale copy, `host/sidecar/src/ops.rs:452`) and the federation (the charge is
a federation reissue, not an accepted note). A federation outage mid-charge leaves the card
locked by an open charge until the federation answers (`503 recovering`). What still works with
no internet: the sales list, status, retrying a charge that already finished (`till.rs:1368`),
and card precheck and key setup. There is no store-and-forward for payments.

## Fedimint facts both options rest on

- A spend is one BIP-340 signature per input over `tx_hash`, the SHA-256 of the transaction's
  inputs, outputs and nonce (`fedimint-core/src/transaction.rs:70-123`). The card's signature
  therefore commits to the till's outputs: once signed, the payment can only go to that till.
  This is already the P2PK-like lock; what it does not stop is the card signing a *different*
  transaction over the same notes.
- A signed transaction has no expiry, epoch or session bound; it stays valid until an input is
  spent. It is checked against the federation's state at submission: a fee or config change in
  between can underfund it (`UnbalancedTransaction`). Overpay at signing (a fee margin that the
  federation burns) to survive that.
- Transactions are atomic: one already-spent input fails the whole transaction and none of its
  outputs are issued (`fedimint-server/src/consensus/transaction.rs:16-120`). Keep one charge per
  transaction, as the till does now.
- A till can verify a note offline: the federation's blind signature is a BLS12-381 pairing check
  against the denomination's aggregate public key from the client config (`crypto/tbs`). Per note
  it needs the amount, the note's public key (33 bytes) and the signature (48 bytes). It cannot
  learn offline whether the note is unspent; only the federation's spent-nonce set knows.
- Fedimint has no spending conditions (no NUT-11 equivalent): no P2PK, scripts or time locks in
  any mint module. Its own offline send (`ECash` bundles) is bearer and first-come-first-served.
- The till uses mint v1 (`fedimint_mint_client`); mint v2 authorises spends the same way and adds
  nothing for offline use.

## Option 1: the card signs each note once

**Card.** Per slot, store the hash the card signed; sign again only that same hash (so recovery
can re-ask), refuse any other until the slot holds a new note. Order of operations for tearing
(card pulled mid-tap): compute the signature in RAM, then record hash and counters in one
`JCSystem` transaction, then answer. A tear before the commit records nothing; a tear after it
leaves a recorded hash that the same transaction can be re-signed for. No state signs two
different transactions; no note is stranded. The applet's comment that 3.0.5 has no transaction
primitive (`FedimintCardApplet.java:399`) is wrong: `beginTransaction` exists since JavaCard 2.1;
measure `getMaxCommitCapacity()` on the J3R180 first. GEN_KEY overwrite is not atomic today
(`used[s] = false` … `used[s] = true`, `FedimintCardApplet.java:~511-539`) and should be.

**Offline verification.** Store each note's amount, public key and federation signature on the
card (89 bytes a slot with the signed hash; about 3 KB for 32 slots, against 131,880 bytes free
with every slot keyed, `CAP-BUILD.md`). The till checks the BLS signature and the card's BIP-340
signature offline.

**Genuine card.** An attestation key generated on the card at install, its public key signed by
the operator at provisioning and kept on the card with the certificate; `ATTEST(challenge)` lets
an offline till check it holds a genuine card, and the card can attest each slot key so a note
key is bound to a genuine card. A phone emulating a card fails; a relay to a real card does not
(bound with a short challenge window). JCOP 4 offers no NXP platform attestation to applets that
we know of; this is operator attestation.

**Caps on the card.** Persistent counters updated in the same transaction as the hash: offline
amount and count since the last sync. A `SYNC` command, HMAC-authenticated with a per-card key
derived from an operator master (the applet already has HMAC-SHA256 for `STATE_KEY`), resets them.
The card cannot check BLS signatures (no pairing), so a stored amount is a claim the till
verifies; the cap only bounds exposure.

**Cost.** Three to five new APDUs, about 3–6 KB, about 0.9 s signing plus 0.1–0.2 s attestation
per tap, low tearing risk. Precedent: every offline CBDC design reviewed (ECB digital euro, e-CNY
hardware wallets, RBI's offline framework) relies on a secure element plus caps and forced sync,
and cashucard already ships option 1 on the same chip (below).

**Weakness.** The card is the whole guarantee: extract one card's keys and its notes can be
spent twice, offline, up to the caps. Extraction is a lab attack (EUCLEAK 2024 on Infineon, NXP
A700x / CVE-2021-3011), unlikely in this threat model, but our software Schnorr is a softer
target than native code: `SchnorrHW` copies the private key into RAM (`SchnorrHW.java:361`), does
`e·d` in bytecode, and branches on secret-dependent parities (`:389`, `:437`). Harden it
(branch-free code, key shares re-randomised per signature, blinding) before relying on option 1,
and give every batch its own keys so one broken card model can be revoked at sync.

## Cashucard already does option 1 on a J3R180

Checked in [cashucard/cashucard](https://github.com/cashucard/cashucard) at `b7e803c`
(`applet/src/main/java/me/flashapp/cashu/CashuApplet.java`, protocol 2.2). Its card signs a
payment once and burns the pieces it spends before the signature leaves the card; its docs say an
offline terminal keeps the payment for later settlement and carries double-spend and mint-solvency
risk until then (`docs/VISION.md`, "Offline acceptance and settlement"). Its October hardware
report on a J3R180 records offline payments and settlement on reconnection. What to take from it:

- **Burn, then answer; keep the answer.** `SPEND_ALL_SIGN` commits the burn, the day's count and
  a log entry together, then sends the signature. If the answer is lost, `SPEND_ALL_AGAIN` returns
  the stored last signature instead of signing again. That is simpler than re-signing the same
  hash, and gives nothing away: the signature is good only for the outputs it was made for.
- **The chip's transaction is small.** On the J3R180 a commit held about a dozen places; thirty-two
  were refused. The card writes the list of places first, commits one `burnPending` byte with the
  signature and counters, then marks the places spent outside the transaction, finishing at the
  next command if the card was pulled (`finishBurn`). The same pattern fits a sign-once Fedimint
  applet; our three-input limit is within a dozen either way.
- **Who chooses the message.** Cashu's `SPEND_PROOF` has the card rebuild its own message rather
  than sign 32 bytes from the reader, because a reader-supplied hash let slot A be burned for slot
  B's signature. Fedimint cannot do that: the card must sign `tx_hash`, which includes the till's
  outputs. Burning per note on signing, keyed to the note's own slot, closes the same hole. Cashu's
  `SIG_ALL` also covers every piece with one signature; Fedimint needs one per input.
- **A clock that only moves forward, for the daily limit.** The card's notion of today comes from
  the newest Bitcoin block header it has been shown (`SET_HEADER`): a terminal holding the PIN can
  spend one day's limit and can only start another day with a real block mined a day later. That
  resets an offline cap with no operator key and no network, a better fit for a village than an
  HMAC `SYNC`; it needs a header from somewhere at least now and then.
- **Genuine card and owner.** `AUTH` proves the card is genuine over a tagged hash nothing else can
  produce; an owner key (P-256 ECDSA, native on the chip) authorises changing the PIN, the limit
  or the owner. A refund key and a date on each piece let whoever loaded a lost card take its
  value back.
- **A settlement record on the card.** Since 2.2 the card keeps the last payment's
  assisted-settlement packet (`GET_OUTBOX`), so a payment can be settled even if the terminal lost
  it.

## Option 2: notes co-signed at top-up

**Protocol.** Feasible without change: the mint sees one public key per note and one BIP-340
signature per input, so a MuSig2 aggregate of the card key and a co-signer key is
indistinguishable from a normal note key, and the mint blind-signs it without learning it.
Key aggregation needs only public keys.

**The catch.** A note's key is fixed when it is minted, so the co-signer must be chosen at
top-up. Locking to one merchant makes the card useless at the next shop; in practice the
co-signer is a village coordinator every till reaches (a local registry on the village network),
whose ledger refuses a second spend. Notes are not recoverable by a stock Fedimint client, a lost
co-signer key strands every note, and no deployment of this model at scale was found; the nearest
relatives (Sarafu community vouchers, mobile-money agents) are trust-and-ledger systems, not
cryptographic locks.

**Card.** MuSig2 needs two rounds with secret nonces held in transient RAM between APDUs; nonce
reuse across two messages leaks the key, so one session at a time, zeroed on use. About 1.0–1.3 s
per tap, more secret-dependent arithmetic than today, medium tearing risk. It still wants
attestation and caps.

**When it wins.** When a card breach is likely (the co-signer's share never sits on the card), or
when the village already runs one hub that every till reaches. A cheaper middle step needs no
cryptographic lock at all: run the registry itself on the village network, so tills there share
one spent-note ledger without internet. That stops double-spends between connected tills, not at
a till outside that network.

## Option 3: caps and store-and-forward (always)

- **Store and forward.** The till keeps each signed transaction durably (the pending-transaction
  machinery in `glue.rs` already stores signed bytes for recovery) and a background sender submits
  them at the first connection. Overpay at signing to survive fee changes. Back the queue up: a
  lost till before sync is lost money.
- **Caps** (what real systems chose): RBI offline at INR 500 a payment and INR 2,000 on an
  instrument at any time; e-CNY offline up to RMB 500 and 10 payments (press sources); EMV
  consecutive-offline counters force a card online. For a village: a per-card offline total of a
  few days' typical spend, a per-payment limit, a count limit, and a per-till exposure limit,
  with a forced sync measured in days, not weeks.
- **Small denominations.** Notes are powers of two already; a broken card yields little each.
- **Gossip.** Tills that can see each other locally can share spent-note lists, shrinking the
  double-spend window even without a full local registry.

## Limits no option removes

- **Value only flows one way offline.** A card pays a till; nothing refills a card or lets a
  merchant pay a supplier, because new notes need the federation. The offline period is bounded
  by what cards hold when the network goes; plan the top-up before it does.
- **The merchant carries double-spend losses** up to the caps until the sync, in every option.
- **A sync is required** for merchants to be paid at all; the village needs a reliable path out
  (a trip to town, a satellite link) on a known schedule.

## Next steps, if pursued

1. Measure on the J3R180: transaction commit capacity, native ECDSA on secp256k1 for the
   attestation key, the split of the 847 ms SIGN.
2. Harden `SchnorrHW` against side channels; make GEN_KEY overwrite atomic.
3. Applet: sign-once per slot with cashucard's burn-then-answer pattern and a stored last
   signature, note data on card, `ATTEST`, and a daily limit on a block-header clock.
4. Till: offline verification, a durable queue with a background sender, caps, the fee margin.
5. A plan document like `OPS-PLAN.md` with the owner's decisions on the caps.
