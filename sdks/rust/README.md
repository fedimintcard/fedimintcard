# Fedimint Card reader for Rust

Select a Card 4.x applet and read its public identity, slot states, stored notes, owed change and raw certificate. Amounts are in millisatoshis. The card reports its balance; a merchant confirms settlement through its paired till.

Use `cargo add fedimintcard` after registry publication, or depend on this directory by path during development. The transport sends ISO 7816 command bytes and returns response data with the two status bytes appended. Open one new session for each tap.

```rust
use fedimintcard::{Balance, Reader, ReaderError};
use std::io;

fn read_balance(
    transceive: impl FnMut(&[u8]) -> io::Result<Vec<u8>>,
) -> Result<Balance, ReaderError> {
    Reader::select(transceive)?.balance()
}
```

The PC/SC example uses a connected desktop reader. Run `cargo run --features pcsc --example pcsc` from this directory. Linux requires the PC/SC development package and service. Every public read is available without a PIN. Use `verify_certificate(&certificate, &registry_key, &card_identity)` with the independently pinned, uncompressed registry public key and slot-zero identity. A certificate proves issuance. Confirming possession also requires a live challenge. A disconnected transport ends the reader session.
