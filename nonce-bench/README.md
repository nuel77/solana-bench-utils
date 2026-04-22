# nonce-bench

Sends a bundle of duplicate-nonce transactions — one per tip account — to a QUIC endpoint. All transactions share the same durable nonce, so they are duplicates from the validator's perspective. Useful for testing how a relay or block engine ranks bundles by tip size when nonces collide.

Each transaction contains:
1. `AdvanceNonceAccount` (signed by nonce authority)
2. `SetComputeUnitPrice` (priority fee)
3. `Transfer` from payer → tip account

## Usage

```
nonce-bench [OPTIONS] --quic-addr <QUIC_ADDR> --nonce-account <NONCE_ACCOUNT>
```

## Options

| Flag | Default | Description |
|------|---------|-------------|
| `--quic-addr` | *(required)* | Target QUIC endpoint `IP:PORT` |
| `--nonce-account` | *(required)* | Durable nonce account pubkey |
| `--keypair` | `~/.config/solana/id.json` | Payer keypair (also used as nonce authority if `--nonce-authority` is omitted) |
| `--nonce-authority` | same as `--keypair` | Nonce authority keypair |
| `--tip-accounts` | 8 Jito mainnet accounts | Space-separated tip account pubkeys (one transaction sent per account) |
| `--tip-lamports` | `1000` | Lamports transferred as tip per transaction |
| `--priority-fee` | `100000` | Compute unit price in micro-lamports |
| `--iterations` | `0` | Times to repeat the bundle (`0` = run once) |
| `--iter-delay-ms` | `500` | Delay between iterations in milliseconds |
| `--rpc-url` | mainnet-beta | RPC URL used to fetch nonce account state |
| `--num-endpoints` | `2` | Parallel QUIC endpoints |

## Examples

**Send a single bundle using the default Jito tip accounts:**
```bash
./target/release/nonce-bench \
  --quic-addr 127.0.0.1:1024 \
  --nonce-account 9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin
```

**Custom tip accounts, 5000 lamport tips, 10 iterations:**
```bash
./target/release/nonce-bench \
  --quic-addr 145.40.93.84:1022 \
  --keypair ~/.config/solana/id.json \
  --nonce-account 9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin \
  --tip-accounts \
    96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5 \
    HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe \
  --tip-lamports 5000 \
  --priority-fee 500000 \
  --iterations 10 \
  --iter-delay-ms 1000
```

**Separate nonce authority keypair:**
```bash
./target/release/nonce-bench \
  --quic-addr 127.0.0.1:1024 \
  --keypair ~/.config/solana/payer.json \
  --nonce-authority ~/.config/solana/nonce-auth.json \
  --nonce-account 9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin \
  --rpc-url http://127.0.0.1:8899
```

## Prerequisites

You need a funded durable nonce account. Create one with the Solana CLI:

```bash
# Create nonce account
solana create-nonce-account nonce.json 0.01

# Check nonce value
solana nonce nonce-account-pubkey
```
