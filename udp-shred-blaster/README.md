# udp-shred-blaster

Blasts real Agave Merkle shreds at a UDP endpoint. Shreds are generated using the same `Shredder::entries_to_merkle_shreds_for_tests` path as a production leader, with proper Merkle root chaining across slots. Data shreds are sent before coding shreds. Slot numbers increment with each batch.

Optionally measures round-trip time (RTT) if the remote host echoes received shreds back to a local listener port.

## Usage

```
udp-shred-blaster [OPTIONS] --target <TARGET>
```

## Options

| Flag | Default | Description |
|------|---------|-------------|
| `--target` | *(required)* | Destination `IP:PORT` to send shreds to |
| `--pps` | `5000` | Target packets per second (data + coding combined) |
| `--start-slot` | `300000000` | Starting slot number |
| `--entries-per-slot` | `8` | Entries packed per slot (controls shred count per slot) |
| `--txns-per-entry` | `4` | Transactions per entry |
| `--duration` | `30` | Run duration in seconds |
| `--shred-version` | `0` | Shred version field (use `0` for generic testing) |
| `--recv-port` | *(disabled)* | Local port to listen on for echoed shreds; enables RTT measurement |

## Examples

**Basic blast at 5000 pps for 30 seconds:**
```bash
./target/release/udp-shred-blaster \
  --target 192.168.1.10:8002
```

**High-rate blast at 50 000 pps for 60 seconds:**
```bash
./target/release/udp-shred-blaster \
  --target 192.168.1.10:8002 \
  --pps 50000 \
  --duration 60
```

**Denser slots — more shreds per slot:**
```bash
./target/release/udp-shred-blaster \
  --target 192.168.1.10:8002 \
  --pps 10000 \
  --entries-per-slot 32 \
  --txns-per-entry 16 \
  --start-slot 400000000
```

**With RTT measurement (server must echo shreds back to sender on port 9000):**
```bash
./target/release/udp-shred-blaster \
  --target 192.168.1.10:8002 \
  --pps 5000 \
  --duration 30 \
  --recv-port 9000
```

**Specific shred version for a known cluster:**
```bash
./target/release/udp-shred-blaster \
  --target 192.168.1.10:8002 \
  --shred-version 50093 \
  --pps 5000
```

## Output

Progress is logged every 10 000 packets. At completion:

```
total_sent=150000 failed=0 actual_pps=5000 slots_processed=1875
```

With RTT mode enabled, echo reception and final RTT statistics are also printed.
