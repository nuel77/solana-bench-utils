//! udp-shred-blaster — blast valid Solana shreds at a UDP endpoint.
//!
//! Shreds are built using the Agave legacy-data format (variant byte 0x80).
//! Each shred is properly signed with an ephemeral leader keypair.  Slot
//! numbers increment with every batch so the receiver sees monotonically
//! growing slot IDs.
//!
//! Optional RTT mode (--recv-port):
//!   Sends shreds to --target and listens on --recv-port.  When the remote
//!   server mirrors a packet back, the blaster matches it by (slot, index)
//!   and prints the round-trip time.

use anyhow::{Context, Result};
use clap::Parser;
use rand::Rng;
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_sdk::{message::Message, transaction::Transaction};
use solana_signer::Signer as _;
use solana_system_interface::instruction as system_instruction;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{net::UdpSocket, sync::Mutex};
use tracing::{debug, info, warn};

// ── Agave legacy-data shred layout ─────────────────────────────────────────
//
// Byte range  Field
// [0  .. 64)  Ed25519 signature (signs bytes [64..SHRED_SIZE])
// [64]        ShredVariant  (0x80 = legacy data)
// [65 .. 73)  slot          (u64 LE)
// [73 .. 77)  index         (u32 LE)
// [77 .. 79)  version       (u16 LE)
// [79 .. 83)  fec_set_index (u32 LE)  — first index in this FEC set
// [83 .. 85)  parent_offset (u16 LE)  — slot - parent_slot
// [85]        flags         (u8)
// [86 .. 88)  data_size     (u16 LE)  — total bytes used incl. headers
// [88 ..1228) payload       (up to 1140 bytes of serialised txns)
//
// References: agave/ledger/src/shred/{shred_data.rs,mod.rs}
// ---------------------------------------------------------------------------

const SHRED_SIZE: usize = 1228;
const SIG_SIZE: usize = 64;
const DATA_HEADER_END: usize = 88; // first byte of payload
const DATA_CAPACITY: usize = SHRED_SIZE - DATA_HEADER_END; // 1140

const VARIANT_LEGACY_DATA: u8 = 0x80;

// ShredFlags for data shreds (bitfield).
const FLAG_DATA_COMPLETE: u8 = 0b0000_0010;
const FLAG_LAST_IN_SLOT: u8 = 0b0000_0001;

fn write_u16_le(buf: &mut [u8], offset: usize, v: u16) {
    buf[offset..offset + 2].copy_from_slice(&v.to_le_bytes());
}
fn write_u32_le(buf: &mut [u8], offset: usize, v: u32) {
    buf[offset..offset + 4].copy_from_slice(&v.to_le_bytes());
}
fn write_u64_le(buf: &mut [u8], offset: usize, v: u64) {
    buf[offset..offset + 8].copy_from_slice(&v.to_le_bytes());
}

/// Build a single legacy data shred and return the 1228-byte payload.
///
/// `data` must be ≤ DATA_CAPACITY bytes; excess is silently truncated.
fn build_data_shred(
    keypair: &Keypair,
    slot: u64,
    parent_slot: u64,
    version: u16,
    index: u32,
    fec_set_index: u32,
    data: &[u8],
    is_last: bool,
) -> Vec<u8> {
    let mut shred = vec![0u8; SHRED_SIZE];

    // Common header (bytes 64–83).
    shred[SIG_SIZE] = VARIANT_LEGACY_DATA;
    write_u64_le(&mut shred, 65, slot);
    write_u32_le(&mut shred, 73, index);
    write_u16_le(&mut shred, 77, version);
    write_u32_le(&mut shred, 79, fec_set_index);

    // Data shred header (bytes 83–88).
    let parent_offset = slot.saturating_sub(parent_slot).min(u16::MAX as u64) as u16;
    write_u16_le(&mut shred, 83, parent_offset);

    let flags = if is_last {
        FLAG_DATA_COMPLETE | FLAG_LAST_IN_SLOT
    } else {
        0
    };
    shred[85] = flags;

    let data_len = data.len().min(DATA_CAPACITY);
    // data_size field = absolute end offset of used data (including headers).
    let data_size = (DATA_HEADER_END + data_len) as u16;
    write_u16_le(&mut shred, 86, data_size);

    // Payload.
    shred[DATA_HEADER_END..DATA_HEADER_END + data_len].copy_from_slice(&data[..data_len]);

    // Sign everything after the signature field.
    let sig = keypair.sign_message(&shred[SIG_SIZE..]);
    shred[..SIG_SIZE].copy_from_slice(sig.as_ref());

    shred
}

/// Parse (slot, index) out of a received shred (or any 1228-byte buffer).
fn parse_slot_index(buf: &[u8]) -> Option<(u64, u32)> {
    if buf.len() < 78 {
        return None;
    }
    let slot = u64::from_le_bytes(buf[65..73].try_into().ok()?);
    let index = u32::from_le_bytes(buf[73..77].try_into().ok()?);
    Some((slot, index))
}

// ── Transaction helpers ─────────────────────────────────────────────────────

/// Create a random system-transfer transaction.  The transactions don't need
/// to be valid on-chain; they just need to be parseable shred payload.
fn random_transfer_tx(rng: &mut impl Rng, payer: &Keypair, blockhash: Hash) -> Transaction {
    let to = Pubkey::new_from_array(rng.r#gen());
    let lamports = rng.gen_range(1..=1_000_000u64);
    let ix = system_instruction::transfer(&payer.pubkey(), &to, lamports);
    let msg = Message::new(&[ix], Some(&payer.pubkey()));
    Transaction::new(&[payer], msg, blockhash)
}

/// Serialise several transactions into a flat byte buffer for packing into shreds.
fn pack_transactions(txns: &[Transaction]) -> Vec<u8> {
    let mut buf = Vec::new();
    for tx in txns {
        if let Ok(bytes) = bincode::serialize(tx) {
            // 2-byte length prefix so a decoder can split them.
            let len = bytes.len() as u16;
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(&bytes);
        }
    }
    buf
}

// ── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[clap(
    name = "udp-shred-blaster",
    about = "Blast valid Solana shreds to a UDP endpoint at high frequency"
)]
struct Args {
    /// Destination address (IP:PORT) to send shreds to
    #[clap(long)]
    target: SocketAddr,

    /// Target packets per second
    #[clap(long, default_value_t = 10_000)]
    pps: u64,

    /// Starting slot number (increments every --shreds-per-slot shreds)
    #[clap(long, default_value_t = 300_000_000)]
    start_slot: u64,

    /// Number of data shreds per slot
    #[clap(long, default_value_t = 32)]
    shreds_per_slot: u32,

    /// Number of random transactions packed into each shred's payload
    #[clap(long, default_value_t = 4)]
    txns_per_shred: usize,

    /// How long to run (seconds)
    #[clap(long, default_value_t = 30)]
    duration: u64,

    /// Local port to listen on for echoed shreds (enables RTT mode).
    /// The remote server must echo packets back to the sender's recv-port.
    #[clap(long)]
    recv_port: Option<u16>,

    /// Shred format version field (cosmetic; validators don't reject on version)
    #[clap(long, default_value_t = 1)]
    shred_version: u16,
}

// ── RTT tracking ────────────────────────────────────────────────────────────

type RttMap = Arc<Mutex<HashMap<(u64, u32), Instant>>>;

async fn recv_task(socket: Arc<UdpSocket>, rtt_map: RttMap, mut stop: tokio::sync::watch::Receiver<bool>) {
    let mut buf = vec![0u8; SHRED_SIZE + 64];
    let mut count = 0u64;
    let mut total_rtt_us = 0u64;

    loop {
        tokio::select! {
            _ = stop.changed() => {
                if *stop.borrow() { break; }
            }
            result = socket.recv_from(&mut buf) => {
                match result {
                    Err(e) => { warn!("recv error: {e}"); }
                    Ok((n, _src)) => {
                        if let Some((slot, index)) = parse_slot_index(&buf[..n]) {
                            let key = (slot, index);
                            let now = Instant::now();
                            let mut map = rtt_map.lock().await;
                            if let Some(sent_at) = map.remove(&key) {
                                let rtt = now.duration_since(sent_at).as_micros() as u64;
                                total_rtt_us += rtt;
                                count += 1;
                                debug!(slot, index, rtt_us = rtt, "echo received");
                                if count % 1000 == 0 {
                                    info!(
                                        echoes = count,
                                        avg_rtt_us = total_rtt_us / count,
                                        "RTT stats"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if count > 0 {
        info!(
            echoes = count,
            avg_rtt_us = total_rtt_us / count,
            "RTT summary"
        );
    }
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // Ephemeral leader keypair — shreds are signed by this key.
    let leader = Keypair::new();
    // Payer for transactions packed inside shreds.
    let payer = Keypair::new();
    let mut rng = rand::thread_rng();

    // Bind the send socket on an ephemeral port.
    let send_bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let send_sock = Arc::new(
        UdpSocket::bind(send_bind)
            .await
            .context("bind send socket")?,
    );
    send_sock.connect(args.target).await?;

    info!(
        target = %args.target,
        pps = args.pps,
        duration_secs = args.duration,
        start_slot = args.start_slot,
        shreds_per_slot = args.shreds_per_slot,
        txns_per_shred = args.txns_per_shred,
        leader = %leader.pubkey(),
        "udp-shred-blaster starting"
    );

    // ── Optional RTT recv task ───────────────────────────────────────────────
    let rtt_map: RttMap = Arc::new(Mutex::new(HashMap::new()));
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);

    if let Some(recv_port) = args.recv_port {
        let recv_bind: SocketAddr = format!("0.0.0.0:{recv_port}").parse().unwrap();
        let recv_sock = Arc::new(
            UdpSocket::bind(recv_bind)
                .await
                .with_context(|| format!("bind recv socket on port {recv_port}"))?,
        );
        info!(recv_port, "RTT mode enabled — listening for echoed shreds");
        let rtt = rtt_map.clone();
        let stop = stop_rx.clone();
        tokio::spawn(async move { recv_task(recv_sock, rtt, stop).await });
    }

    // ── Blast loop ───────────────────────────────────────────────────────────
    let interval_us = if args.pps > 0 { 1_000_000 / args.pps } else { 0 };
    let mut ticker = if interval_us > 0 {
        Some(tokio::time::interval(Duration::from_micros(interval_us)))
    } else {
        None
    };
    if let Some(t) = ticker.as_mut() {
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    }

    let deadline = Instant::now() + Duration::from_secs(args.duration);
    let mut slot = args.start_slot;
    let mut total_sent: u64 = 0;
    let mut total_failed: u64 = 0;

    // Fixed blockhash for all transactions (they don't need to land on-chain).
    let fake_blockhash = Hash::new_unique();

    'outer: loop {
        let parent_slot = slot.saturating_sub(1);
        let fec_set_index = 0u32; // one FEC set per slot for simplicity

        for shred_index in 0..args.shreds_per_slot {
            if Instant::now() >= deadline {
                break 'outer;
            }

            if let Some(t) = ticker.as_mut() {
                t.tick().await;
            }

            // Build payload from random transactions.
            let txns: Vec<Transaction> = (0..args.txns_per_shred)
                .map(|_| random_transfer_tx(&mut rng, &payer, fake_blockhash))
                .collect();
            let payload = pack_transactions(&txns);

            let is_last = shred_index + 1 == args.shreds_per_slot;
            let shred = build_data_shred(
                &leader,
                slot,
                parent_slot,
                args.shred_version,
                shred_index,
                fec_set_index,
                &payload,
                is_last,
            );

            // Record send time before the syscall (RTT mode).
            if args.recv_port.is_some() {
                rtt_map
                    .lock()
                    .await
                    .insert((slot, shred_index), Instant::now());
            }

            match send_sock.send(&shred).await {
                Ok(_) => total_sent += 1,
                Err(e) => {
                    total_failed += 1;
                    warn!("send failed: {e}");
                }
            }

            if total_sent % 10_000 == 0 && total_sent > 0 {
                info!(
                    sent = total_sent,
                    failed = total_failed,
                    current_slot = slot,
                    "progress"
                );
            }
        }

        slot += 1;
    }

    // Signal the RTT recv task to stop.
    let _ = stop_tx.send(true);
    // Give it a moment to flush stats.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let elapsed = args.duration as f64;
    info!(
        total_sent,
        total_failed,
        actual_pps = total_sent as f64 / elapsed,
        slots_blasted = slot - args.start_slot,
        "udp-shred-blaster finished"
    );

    Ok(())
}
