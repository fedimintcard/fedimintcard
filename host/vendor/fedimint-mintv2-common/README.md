# Fedimint mint v2 common types

The source is from [Fedimint v0.13.0](https://github.com/fedimint/fedimint/tree/264ebf22b698c158055bcdd80d5b586fdb48d1fc/modules/fedimint-mintv2-common), under the included MIT license.

The till also supports the deployed mint v2 static fee of 10 msat per input or output with zero proportional fee. The local change in `src/config.rs` accepts that exact schedule in JSON and consensus decoding. The original fee fields, wire encoding and calculation are preserved. The host workspace applies this crate to the SDK client, server fixtures and card tools through a Cargo patch.
