# txn-blaster

Blasts transactions to a Solana TPU QUIC endpoint at a controlled rate. Each transaction is a self-transfer (payer → payer, 1 lamport) signed with your keypair. Measures throughput and average send latency.

## Usage

```
txn-blaster [OPTIONS] --quic-addr <QUIC_ADDR>
```

## Options

| Flag | Default | Description |
|------|---------|-------------|
| `--quic-addr` | *(required)* | TPU QUIC endpoint `IP:PORT` |
| `--keypair` | `~/.config/solana/id.json` | Payer keypair JSON file |
| `--rpc-url` | mainnet-beta | RPC URL for fetching the latest blockhash |
| `--tps` | `100` | Target transactions per second |
| `--duration` | `30` | Run duration in seconds |
| `--num-endpoints` | `4` | Parallel QUIC endpoints (increases throughput) |
| `--max-attempts` | `1` | Max send attempts per transaction |

## Examples

**Blast at 200 TPS for 60 seconds against a local validator:**
```bash
./target/release/txn-blaster \
  --quic-addr 127.0.0.1:1024 \
  --keypair ~/.config/solana/id.json \
  --rpc-url http://127.0.0.1:8899 \
  --tps 200 \
  --duration 60
```

**High-throughput run against mainnet with 8 endpoints:**
```bash
./target/release/txn-blaster \
  --quic-addr 145.40.93.84:1022 \
  --tps 1000 \
  --duration 30 \
  --num-endpoints 8
```

**With retries enabled:**
```bash
./target/release/txn-blaster \
  --quic-addr 127.0.0.1:1024 \
  --tps 50 \
  --duration 10 \
  --max-attempts 3
```

## Output

At completion the tool prints a summary:

```
sent=6000 success=5987 failed=13 avg_latency=4ms
```

Blockhash is refreshed automatically every 25 seconds in the background.
