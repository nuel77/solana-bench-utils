# solana-utils

A collection of Rust tools for testing and benchmarking Solana transaction and shred submission.

## Workspace

| Crate | Description |
|-------|-------------|
| [`txn-blaster`](./txn-blaster) | Blast transactions to a Solana TPU via QUIC at a controlled rate |
| [`nonce-bench`](./nonce-bench) | Send duplicate-nonce bundles to test tip-based bundle ranking |
| [`udp-shred-blaster`](./udp-shred-blaster) | Blast real Merkle shreds at a UDP endpoint |
| `quic-client` | Shared QUIC client library (connection caching, retries, multi-endpoint) |

## Build

```bash
# Build all binaries
cargo build --release

# Build a specific binary
cargo build --release -p txn-blaster
cargo build --release -p nonce-bench
cargo build --release -p udp-shred-blaster
```

Binaries are placed in `target/release/`.
