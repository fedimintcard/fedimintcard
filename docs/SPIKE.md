# Spike results: a JavaCard that holds Fedimint spend keys

Run 2026-10-26 against the plan in [SPIKE-PLAN.md](SPIKE-PLAN.md). Fedimint pinned to
`v0.13.0-beta.1` (commit `f9dccb6656cdad6760d6ed4bc33da585ca87a32a`), module `mintv2`.

## Answer

**Yes.** A JavaCard applet holding a secp256k1 key signed a Fedimint `mintv2` spend, a
4-guardian test federation accepted it, and the key never left the card. No consensus
change was needed; the federation code is stock. The spend used the current `mintv2`
module directly, so the fallback to the old mint module was not needed. It was shown first
with the applet in jCardSim behind the HTTP bridge (`CARD_BACKEND=bridge`), then on a real
NXP J3R180 over PC/SC (`CARD_BACKEND=pcsc`), with the same tests.

What was shown, in `cargo test`:

| Step | Result |
|---|---|
| 1 applet | `GEN_KEY`, `GET_PUB`, `SIGN` on jCardSim; 45 Java tests green, BIP340 vectors 0-14 pass, no command returns a private scalar |
| 2 transport | Rust `Card` trait with `SoftCard`, HTTP bridge and PC/SC backends; round trip verified with `secp256k1` |
| 3 federation | 4 guardians, fake bitcoin, no devimint or nix; starts in seconds inside a test |
| 4 issue | A note whose nonce is the card's x-only key (even parity) was issued by a hand-built `[DummyInput] -> [MintOutput]` transaction; shares fetched from `signature_shares`, combined and unblinded; `verify_note` passes |
| 5 spend | A hand-built reissue transaction (card note in, four host notes out) signed by the card over the raw 32-byte `tx_hash()` was accepted; the host notes were redeemed through the stock client, balance exact with fees off |
| 6 negatives | Signature from another key: `InvalidSignature`. Missing or extra signature: `InvalidWitnessLength`. Output or nonce changed after signing: `InvalidSignature`. Second spend of the note in a fresh transaction: `Input(...)` "already spent". Controls: bad BLS signature, unbalanced, wrong denomination all rejected with the expected error |
| 7 real card | CAP built with ant-javacard (load file 4,573 bytes), installed on an NXP J3R180 (JCOP4, ATR `3B8A800150564A434F503453494471`) through an ACR122U with GlobalPlatformPro, Opener first then the card applet. Steps 2, 5 and 6 rerun over PC/SC: 18 of 18 tests pass. Recipe in [CAP-BUILD.md](CAP-BUILD.md) |
| 8 this note | Done |

## Timings

| What | Measured |
|---|---|
| Java tests (applet + bridge) | 45 tests, about 4 s |
| Rust cold compile (registry warm, Apple silicon) | about 2m40s |
| Rust test run, all 16 tests, `--test-threads=2` | 25-35 s; each federation test starts its own 4 guardians in a few seconds |
| Same run over the Java bridge | same; 13 `SIGN` commands served per run |
| Same run on the J3R180 over PC/SC, `--test-threads=1` | spend tests 36 s (15 s with the soft card); whole workspace 3m29s including the ci-profile build |
| J3R180 `GEN_KEY` | 125 ms |
| J3R180 `GET_PUB` | 27 ms |
| J3R180 `SIGN` (BIP340, `host/card/examples/bench.rs`, 10 runs, all verified) | median 847 ms, min 841 ms, max 854 ms |

## The transaction the card signs

One 2^20 msat card note into four host notes (2^19, 2^18, 2^17, 2^16), fees on, consensus
encoding **439 bytes**. The card only ever sees the 32-byte `tx_hash()`, which is SHA-256 of
the first 372 bytes (inputs, outputs, nonce); signatures are not hashed.

| Part | Bytes |
|---|---|
| inputs: count, then one `MintInput::V0` (instance id, length, variant, length, denomination, nonce 33, BLS signature 48) | 87 |
| outputs: count, then 4 x `MintOutput::V0` (…, denomination, blinded message 48, tweak 16), 69 each | 277 |
| transaction nonce | 8 |
| signatures: `NaiveMultisig`, one 64-byte BIP340 signature | 67 |

A full hex dump and the byte-level breakdown are in `host/README.md`. Each extra output costs
69 bytes; the input is fixed at 87. For a card to check its own change output before signing
it would have to parse the first 87 bytes and walk the outputs, each of which carries a
48-byte blinded message the card cannot interpret on its own. Denominations are powers of
two in msat (`Denomination(n)` = 2^n msat); the server accepts n in 0..42, the stock client
uses 9..42. Fees: 100 msat base per input and per output in the test federation, 0 ppm.
Overpaying is allowed (core consensus 2.1), the excess is burned.

## Facts about Fedimint that the design note depends on

- `InputMeta.pub_key` is the note's nonce as a full 33-byte `PublicKey`; the server verifies
  the BIP340 signature against its x-only form. Parity does not matter for verification, but
  the same 33 bytes must be blinded at issue and presented at spend. We use even parity.
- The client's `TransactionBuilder` signs with `Keypair`s it holds and has no pre-signed
  path, so the host builds `fedimint_core::transaction::Transaction` by hand and submits it
  through `IGlobalFederationApi::submit_transaction`. Shares are fetched with
  `request_with_strategy_retry` and `FilterMapThreshold` (3 of 4), as the stock client does.
- Resubmitting an identical transaction is idempotent (`Ok(txid)`); a double spend must be a
  fresh transaction to be rejected.
- `NoteIssuanceRequest` has public fields, so host-owned outputs can be built without the
  client's root secret; card-owned outputs are blinded directly from `nonce_message(nonce)`.
- The dummy module creates value for any key, which makes step 4 a one-transaction test.
- Noise to expect in test logs: when a test federation is dropped, a guardian thread can panic
  at `fedimint-server/src/consensus/mod.rs:511` (`api_handler.stop().expect(...)`), and a
  long-polling `submit_transaction` handler can report "API handler panicked" at the same
  moment. It only happens at teardown with tests running in parallel and never fails a test.
  It is a fixtures race in fedimint, not a protocol problem.

## Applet facts

- `SchnorrHW.java` from cashucard/javacard (MIT) does the BIP340 signing with
  `ALG_EC_SVDP_DH_PLAIN_XY`; the PACE_GM key agreement, `agree` and `addToG` were removed.
- Review fixes before silicon: private scalar cleared on every GEN_KEY exit path; the nonce
  key is transient-deselect where the card allows it, otherwise overwritten with a fixed value
  after each signature; `sc` and `work` zeroed after `sign`; `getS` outputs of 1..33 bytes
  normalised by one shared `alignScalar`; compressed `getW` handled and tested.
- GEN_KEY never overwrites a slot. There is no delete command. Four slots (state at the spike; superseded on this branch by 16 slots and GEN_KEY P2=1 overwrite).
- Transient memory is about 610 B (the J3R180 refused 1,030 B for cashucard).
- Settled on silicon (J3R180, 2026-10-26): the DH output is the 65-byte `04||X||Y` form
  SchnorrHW expects (signatures verify; a 64-byte form would have failed closed with `6F00`);
  the heavy constructor installs when the `Opener` is instantiated first, as cashucard found;
  the card's GlobalPlatform keys must be the defaults (a card personalised by the cashucard
  provisioner refuses `gp` until it is reset); case-2 commands work over the ACR122U
  (contactless, so T=0 was not exercised). Not observable from outside: whether the nonce key
  was built as `TYPE_EC_FP_PRIVATE_TRANSIENT_DESELECT` or fell back to a persistent key; both
  paths behave the same at the APDU level.
- Signing takes about 850 ms on the J3R180. That is one BIP340 signature per note spent, so a
  spend of n notes costs about n seconds of tap time unless the applet batches or the design
  keeps one note per spend.

## Questions for the design note

1. **Change pool.** Who prepares the blinded change outputs for a card spend, and how does the
   card check that one of the 69-byte outputs is its own change before it signs? Options:
   the owner's phone pre-blinds change nonces for card slots and hands the card a commitment;
   or the card signs blindly and the owner's phone is trusted to assemble.
2. **On-card parsing.** Is 87 + 69n bytes small enough to stream into the card over APDUs and
   hash on-card, so the card verifies what it signs rather than a 32-byte hash it is handed?
   SHA-256 on the card is cheap next to the 850 ms signature, so the cost is APDU round trips.
3. **Slots and denominations.** Four slots at one note each is a toy. Powers-of-two
   denominations mean a balance needs several notes; how many slots, and does the card
   derive per-note keys from one seed (BIP32-style) so slot count stops mattering?
4. **Owner rules.** PIN, PUK, slot reset and reissue are out of scope here but gate any real
   product; which of cashucard's `CLEAR_PIN`, `SET_PUK`, `UNBLOCK_PIN` carry over?
5. **Till flow.** A till needs the federation's config, the note, the card signature, and a
   place to put the change; what does a merchant device run, and what does it need to trust?
6. **Federation module.** Nothing above needed one. Would a module that enforces card-only
   rules (e.g. change must return to the same key) be worth a consensus change?
7. **Non-constant-time signing.** `SchnorrHW.mulModN` is schoolbook; the BIP340 aux-random
   nonce is kept on purpose (fault injection), but side channels are unassessed.

## Repository state at the end of the spike

| Branch | Contents | Status |
|---|---|---|
| `spike/applet` | applet, jCardSim tests, bridge, CI java job, `CAP-BUILD.md` | three review rounds, approved |
| `spike/host` | Rust workspace, federation tests, CI rust job, `host/README.md` | two review rounds, approved; stacked on `spike/applet` |
| `spike/docs` | this note, README status | stacked on `spike/host` |
| `spike/real-card` | `applet/build.xml`, `Opener.java`, `host/card/examples/bench.rs`, step 7 results | stacked on `spike/docs` |

CI has not run: the branches are local by choice for now. Commits are authored as Dread but
unsigned because the session could not use the GPG key.
