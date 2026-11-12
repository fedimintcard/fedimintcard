# host/

Rust side of the spike: a `Card` transport and federation tests that spend a note held by the card.

| Crate | What |
|---|---|
| `card/` (`fedimint-card-host-card`) | `Card` trait (`gen_key`, `get_pub`, `sign`, and applet 3.0's `set_note`, `get_slot`, `get_state`, `set_change`, `set_limit`, `set_header`, `attest_init`, `set_cert`, `get_cert`, `attest`; `applet_version`); `CardClient<T: Transport>`; backends `SoftCard` (in-process; `SoftCard::new()` is 2.1, `SoftCard::v3()` follows applet 3.0), `BridgeTransport` (HTTP, `ureq`), `PcscTransport` (feature `pcsc`); `v3` (3.0 answers, header work, the registry's certificate) |
| `fed-tests/` (`fedimint-card-fed-tests`) | 4-guardian in-process federation (`fedimint-testing`), hand-built transactions, steps 3-6 as `cargo test` |
| `pos-core/` (`fedimint-card-pos-core`) | The offline charge the till and the POS phones share (`docs/POS-OFFLINE.md`): attestation, note checks, the planner, the durable queue, the offline kit and a phone's store; no fedimint client, no async runtime. Feature `ffi`: the apps' UniFFI interface; `scripts/pos-core-mobile.sh android\|ios\|bindings\|check` builds it for the phones |

## Pin

Fedimint **`v0.13.0-beta.1`**, commit `f9dccb6656cdad6760d6ed4bc33da585ca87a32a`, module `mintv2`.
`Cargo.lock` was seeded from fedimint's own lock file at that tag (git dependencies ignore it).
`rust-toolchain.toml` pins Rust 1.91.1: the dependency tree (`n0-*`, `netwatch`, `portmapper`) needs rustc 1.91, and 1.90 fails to resolve.

## Build and run

System packages: a C/C++ toolchain, `clang`/`libclang`, `cmake`, `protobuf-compiler`, `pkg-config`, OpenSSL headers on Linux.
On macOS: Xcode command line tools, `brew install cmake protobuf`.

```
cd host
cargo test --workspace -- --test-threads=2
```

Cold build (empty `target/`, registry and git cache warm; Apple silicon, 1.91.1): about 2m40s to compile everything, then about 25-35 s to run all tests (each federation test starts its own 4 guardians in a few seconds).

### Backends

Federation tests (`fed-tests/tests/*.rs`) and `card_from_env` pick the card from the environment, with no code change:

```
CARD_BACKEND=soft                                  # default, in-process keys
CARD_BACKEND=bridge CARD_BRIDGE_URL=http://127.0.0.1:9876 cargo test --workspace
CARD_BACKEND=pcsc [CARD_PCSC_READER=name] cargo test --workspace --features fedimint-card-host-card/pcsc
```

The CLI reads the same variable. A `card` command against a USB reader (`card load`, `card balance`)
needs a binary built with `cargo build -p fedimint-card-sidecar --features pcsc` and
`CARD_BACKEND=pcsc` set; without them it talks to an empty in-process SoftCard, which answers
`6985` ("reading the card's slot 0 key") however the real card is provisioned. `CARD_PIN` names the
PIN for commands that sign; it defaults to the test PIN `1234`, and a wrong guess costs a try.

Bridge wire format: `POST /apdu` `{"apdu":"<hex>"}` -> `{"response":"<hex data||SW>"}`; `POST /reset`; `GET /health`.
Verified against the Java bridge from spike/applet (`bridge.jar --port N`): the whole workspace passes with `CARD_BACKEND=bridge`, and `card_roundtrip` passed 20/20 loops. `bridge_roundtrip_when_configured` runs only if `CARD_BRIDGE_URL` is set. Tests use slots 0 and 1 and create keys on demand (`get_or_gen`), so they work against a persistent card.

## Lightning

`card-sidecar ln gateways` lists the federation's gateways and their fees; `ln receive <amount_msat>`
prints an invoice and waits until the eCash is in the wallet. `ln pay <bolt11> [--max-fee-msat N]`
pays an invoice (with an amount) from the wallet's balance and prints the outcome as JSON
(`status`: `paid` with the preimage and gateway fee, `pending` with the operation id after 90 s,
`refunded`, `failed`, or `refused` with nothing spent; exit 1 unless paid or pending). It goes
through the federation's `ln` module, else its lnv2 module; the till registers the lnv2 client only
on a federation without `ln`, so on one with both (Island Bitcoin Community) only `ln` is used.
With no `ln` gateway reachable, an invoice of the same federation is still paid, inside the
federation with no gateway fee; any other invoice is refused with nothing spent. Paying an invoice
again answers the earlier payment while it is paid, in flight or `failed` (an error the client
could not settle, which can leave the contract funded and the gateway able to claim it); only after
a refund does it fund a new attempt.

Inside the till the same payment is a library call (`sidecar/src/wallet/pay.rs`):
`Wallet::pay_invoice(bolt11, max_fee_msat, wait)` answers `paid` (preimage, gateway fee),
`failed` (certain: nothing was funded, or the refund is back in the wallet) or `unknown` (in
flight, or an outcome the client cannot settle; never to be refunded). `resolve_pay(operation_id)`
answers it again from the client's record, after a restart too, and `pay_status(bolt11)` finds
this wallet's payments of an invoice when the operation id was never recorded (`None`: never
funded).

## Till service

A card on a USB reader can stand in for one behind a phone: `card-sidecar relay` (a build with
`--features pcsc`; loopback only; `--listen`, default 127.0.0.1:9876, `--reader` or
`CARD_PCSC_READER`) serves the reader over the bridge protocol, and `till serve --relay
http://127.0.0.1:9876` then charges, checks and loads that card. It takes no wallet, so it runs
beside `till serve`.

`card-sidecar till serve [--listen 127.0.0.1:9877] [--relay http://127.0.0.1:9876]` runs the
point-of-sale HTTP API described in `docs/TILL-API.md` (charge, card balance, sales, status). The
relay URL also comes from `CARD_BRIDGE_URL`. It starts without a card; every charge or balance
call opens the card behind the phone relay afresh.

Wiring over USB (both forwards die when USB reconnects, so re-run them):

```
adb forward tcp:9876 tcp:9876   # laptop -> phone NFC relay
adb reverse tcp:9877 tcp:9877   # phone -> till service on the laptop
```

* `serve` owns the data directory. Do not run any other `card-sidecar` command (card, ecash,
  info) against the same `--data-dir` while it runs; they share `card_state.json` and the client
  database.
* One charge or balance call at a time; a second gets `409 busy`.
* `sales.json` in the data dir (mode 0600) is a convenience ledger for the app and for
  `client_ref` replays. The money truth is the federation and `card_state.json`.
* No authentication: keep it on loopback (a warning is printed otherwise).
* Applet 3.0 cards and offline charges (`docs/TILL-API.md`, `docs/OFFLINE-PROTOCOL.md` § 4):
  `--esplora-url` (`TILL_ESPLORA_URL`, default `https://mempool.space/api`, empty to turn off)
  is where block headers for the cards' clocks come from (every 10 minutes, kept in
  `block-header.json`); `--offline-cap-msat` (default 100,000,000) and `--offline-cap-count`
  (default 200) cap what the till holds unsettled from offline charges; `--spare-target`
  (`TILL_SPARE_TARGET`, default 12) and `--spare-keys-per-tap` (`TILL_SPARE_KEYS_PER_TAP`,
  default 2) set the spare reserve online taps keep a 3.0 card at, so it can take change offline;
  `--offline-fee-base-msat` (`TILL_OFFLINE_FEE_BASE_MSAT`, default 100) and
  `--offline-fee-headroom-ppm` (`TILL_OFFLINE_FEE_HEADROOM_PPM`, default 0) set the fee schedule an
  offline charge keeps a fee margin for, on a federation at core consensus 2.1 or later (0 and 0:
  none; `docs/TILL-API.md`, "Fee margin"). Charges taken offline
  wait in `offline_queue.json` (0600) and are sent every 30 s while the till is on ops; the
  registry's certificate key is kept as `cert_key` in `till-ops.json`. Back up both with the
  wallet: the queue is the merchant's money until it is sent. At an online tap of a 3.0 card the
  till first sends the registry's releases (§ 6) for charges the federation rejected or the
  registry refused as double spends, logging any the card does not take.

## Findings (step 5 transaction)

Spend of one 2^20 msat card note into four host notes (2^19, 2^18, 2^17, 2^16), fees on. The card signs the 32-byte `tx_hash()` only; it never sees these bytes.

Consensus encoding: **439 bytes**.

| Part | Bytes |
|---|---|
| inputs: count 1, then one `MintInput::V0` (instance id 1, length 1, variant 1, length 1, denomination 1, nonce 33, BLS signature 48) | 87 |
| outputs: count 1, then 4 x `MintOutput::V0` (instance id 1, length 1, variant 1, length 1, denomination 1, blinded message 48, tweak 16) = 69 each | 277 |
| transaction nonce | 8 |
| signatures: variant 0 (`NaiveMultisig`), length 65, count 1, 64-byte BIP340 signature | 67 |

`tx_hash()` = SHA-256 of the first 372 bytes (inputs, outputs, nonce). Signatures are not hashed. Hex of one real transaction (a fresh random nonce and blinding every run, so yours differs):

```
0101540052140285a818b90f0367e37a17da8b458654887977c96f5a1725815f98a871dd0ac9d98ab21b42db61f7bf86
5bc7f5d07e42910e046778d8af87218fd4ac288cb410b11a8838281930763379be769e85a0fd7b040143004113b1ebb9
c5ced22e9ec864640d154f7a7ee47e6fa536ef35bfe0e9f87f216c4d4014bc536a7217571e58a1a19827803868c5dbf9
805d0cd20b0735bdcdc3fa7f2d0143004112b9dad7f615feb8d9a53b1b4b3e6458c4cc1f85edfa104276da73f2c19f1a
ce43351b9c1219cbd2f5a3de989a3ab2ee756809726ef974d14d9a5dc32e928f2ca50143004111a8b85dfd279f44d788
502f1a6c827f61ad06319e04505eff17988aa3e8ef01be57f0b2ce8460ef5171dd759f407f5f3411546a41dbb4fbc324
ad30b17a2f6e060143004110b4d55c2103057c2f61b45edb8df36febfd2b80798bfdf9b7cf3ef79f7c39d5ed3e688d77
58b0265cdc6f442878cce20152ea5f536b56b4b957b015c268a9ad0060959aa893d21c1b004101f3f0f3171d5f14685a
8f6be6cf8f75b673aa98cc3f92c02488ef27b83e5ab3313da8cb14664e378483f161ec36100c3d9ba6b5782f0e2a06a2
7d016bf6de0727
```

txid (the 32 bytes handed to `SIGN`): `263c2e9d6cea2ee48f8e42e77a87f8d1f3342eb99fa567fa8e0687605492464a`

Extra bytes per additional output: 69. The input is fixed at 87 bytes whatever the outputs are. A card that wants to check its own change output would need to parse the first 87 bytes plus walk the outputs (each carries a 48-byte blinded message it cannot interpret).

## Notes

- The client's transaction builder signs with `Keypair`s it holds, so the glue builds `Transaction` by hand and submits via `IGlobalFederationApi::submit_transaction`.
- The nonce is the card's x-only key with **even** parity (`02` prefix); the same 33 bytes are hashed at issue and presented at spend.
- Funding for step 4 comes from the dummy module (it accepts any key and creates value), through a hand-built `[DummyInput] -> [MintOutput]` transaction; no client funding needed.
- Resubmitting an identical transaction is idempotent (`Ok(txid)`); the double-spend test uses a fresh transaction.
