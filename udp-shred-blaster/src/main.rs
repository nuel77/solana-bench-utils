//! udp-shred-blaster — blast real Agave Merkle shreds (data + coding) at a UDP endpoint.
//!
//! Shreds are produced by Agave's own `Shredder::entries_to_merkle_shreds_for_tests`,
//! which is identical to the production path the leader uses.  Each call returns both
//! data shreds and Reed-Solomon coding shreds, all properly signed and ready for the
//! receiver to reconstruct lost packets.
//!
//! Slot numbers increment every batch.  The chained Merkle root is threaded through
//! batches so the chain is intact.
//!
//! Optional RTT mode (--recv-port):
//!   After sending each shred the blaster records a send timestamp keyed by
//!   (slot, shred_index).  When the remote server mirrors a packet back to
//!   --recv-port the blaster parses slot+index out of the shred header with
//!   `solana_ledger::shred::layout` and looks up the matching timestamp.

use anyhow::{Context, Result};
use clap::Parser;
use rand::Rng;
use solana_entry::entry::Entry;
use solana_hash::Hash;
use solana_keypair::{Keypair, Signer as _};
use solana_ledger::shred::{self, ProcessShredsStats, ReedSolomonCache, Shred, Shredder};
use solana_pubkey::Pubkey;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{net::UdpSocket, sync::Mutex};
use tracing::{debug, info, warn};

const PACKET_DATA_SIZE: usize = 1232;

// ── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug, Clone)]
#[clap(
    name = "udp-shred-blaster",
    about = "Blast real Agave Merkle shreds (data + FEC coding) to a UDP endpoint"
)]
struct Args {
    /// Destination address (IP:PORT) to send shreds to
    #[clap(long)]
    target: SocketAddr,

    /// Target packets per second (data + coding shreds combined)
    #[clap(long, default_value_t = 5_000)]
    pps: u64,

    /// Starting slot number
    #[clap(long, default_value_t = 300_000_000)]
    start_slot: u64,

    /// Entries to pack per slot (controls how many shreds are produced per slot)
    #[clap(long, default_value_t = 8)]
    entries_per_slot: usize,

    /// Transactions per entry
    #[clap(long, default_value_t = 4)]
    txns_per_entry: usize,

    /// How long to run (seconds)
    #[clap(long, default_value_t = 30)]
    duration: u64,

    /// Shred version field passed to Shredder (cosmetic; use 0 for generic testing)
    #[clap(long, default_value_t = 0)]
    shred_version: u16,

    /// Local port to listen on for echoed shreds (enables RTT measurement).
    /// The remote server must send received shreds back to sender:<recv-port>.
    #[clap(long)]
    recv_port: Option<u16>,
}

// ── Entry / transaction helpers ──────────────────────────────────────────────

fn make_entries(rng: &mut impl Rng, num_entries: usize, txns_per_entry: usize) -> Vec<Entry> {
    (0..num_entries)
        .map(|_| {
            let from = Keypair::new();
            let txns = (0..txns_per_entry)
                .map(|_| {
                    let to = Pubkey::from(rng.r#gen::<[u8; 32]>());
                    // Hash::default() as recent_blockhash — txns don't need to land on-chain.
                    solana_system_transaction::transfer(&from, &to, 1, Hash::default())
                })
                .collect();
            Entry::new(&Hash::default(), 1, txns)
        })
        .collect()
}

// ── Shred generation (CPU-bound; called via spawn_blocking) ──────────────────

struct SlotShreds {
    data: Vec<Shred>,
    coding: Vec<Shred>,
    /// Merkle root extracted from the last data shred for chaining the next batch.
    last_merkle_root: Hash,
}

fn generate_slot_shreds(
    slot: u64,
    parent_slot: u64,
    shred_version: u16,
    entries_per_slot: usize,
    txns_per_entry: usize,
    chained_merkle_root: Hash,
    keypair: &Keypair,
    reed_solomon_cache: &ReedSolomonCache,
) -> Result<SlotShreds> {
    let mut rng = rand::thread_rng();
    let entries = make_entries(&mut rng, entries_per_slot, txns_per_entry);

    let shredder = Shredder::new(
        slot,
        parent_slot,
        0, /* reference_tick */
        shred_version,
    )
    .map_err(|e| anyhow::anyhow!("create Shredder: {e:?}"))?;

    let (data, coding) = shredder.entries_to_merkle_shreds_for_tests(
        keypair,
        &entries,
        true, // is_last_in_slot
        chained_merkle_root,
        0, // next_shred_index
        0, // next_code_index
        reed_solomon_cache,
        &mut ProcessShredsStats::default(),
    );

    // Pull the Merkle root out of the last data shred so the next slot can chain off it.
    let last_merkle_root = data
        .last()
        .and_then(|s| shred::layout::get_merkle_root(s.payload()))
        .unwrap_or_else(Hash::new_unique);

    info!(
        slot,
        data = data.len(),
        coding = coding.len(),
        "slot shreds generated"
    );

    Ok(SlotShreds {
        data,
        coding,
        last_merkle_root,
    })
}

// ── RTT receiver ────────────────────────────────────────────────────────────

type RttMap = Arc<Mutex<HashMap<(u64, u32), Instant>>>;

async fn recv_task(
    socket: Arc<UdpSocket>,
    rtt_map: RttMap,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut buf = vec![0u8; PACKET_DATA_SIZE];
    let mut echoes: u64 = 0;
    let mut total_rtt_us: u64 = 0;

    loop {
        tokio::select! {
            _ = stop.changed() => { if *stop.borrow() { break; } }
            result = socket.recv_from(&mut buf) => {
                match result {
                    Err(e) => warn!("recv error: {e}"),
                    Ok((n, _src)) => {
                        let payload = &buf[..n];
                        // Parse slot + index using Agave's own shred layout helpers.
                        if let (Some(slot), Some(index)) = (
                            shred::layout::get_slot(payload),
                            shred::layout::get_index(payload),
                        ) {
                            let now = Instant::now();
                            let mut map = rtt_map.lock().await;
                            if let Some(sent_at) = map.remove(&(slot, index)) {
                                let rtt = now.duration_since(sent_at).as_micros() as u64;
                                total_rtt_us += rtt;
                                echoes += 1;
                                debug!(slot, index, rtt_us = rtt, "echo received");
                                if echoes % 1_000 == 0 {
                                    info!(
                                        echoes,
                                        avg_rtt_us = total_rtt_us / echoes,
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

    if echoes > 0 {
        info!(echoes, avg_rtt_us = total_rtt_us / echoes, "RTT summary");
    }
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // Ephemeral leader keypair — every shred batch is signed by this key.
    let keypair = Arc::new(Keypair::new());
    // Thread-safe Reed-Solomon instance cache shared across spawned tasks.
    let reed_solomon_cache = Arc::new(ReedSolomonCache::default());

    let send_sock = Arc::new(
        UdpSocket::bind("0.0.0.0:0")
            .await
            .context("bind send socket")?,
    );
    send_sock.connect(args.target).await?;

    info!(
        target = %args.target,
        pps = args.pps,
        duration_secs = args.duration,
        start_slot = args.start_slot,
        entries_per_slot = args.entries_per_slot,
        txns_per_entry = args.txns_per_entry,
        leader = %keypair.pubkey(),
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

    // ── Rate-limited blast loop ──────────────────────────────────────────────
    let interval_us = if args.pps > 0 {
        1_000_000 / args.pps
    } else {
        0
    };
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
    let mut chained_merkle_root = Hash::default();
    let mut total_sent: u64 = 0;
    let mut total_failed: u64 = 0;

    'outer: loop {
        if Instant::now() >= deadline {
            break;
        }

        // Generate shreds on a blocking thread — Shredder uses rayon internally.
        let kp = keypair.clone();
        let rsc = reed_solomon_cache.clone();
        let cfg = args.clone();
        let parent_slot = slot.saturating_sub(1);
        let cmr = chained_merkle_root;

        let slot_shreds = tokio::task::spawn_blocking(move || {
            generate_slot_shreds(
                slot,
                parent_slot,
                cfg.shred_version,
                cfg.entries_per_slot,
                cfg.txns_per_entry,
                cmr,
                &kp,
                &rsc,
            )
        })
        .await
        .context("shred generation task panicked")?
        .context("generate_slot_shreds")?;

        chained_merkle_root = slot_shreds.last_merkle_root;

        // Send data shreds first (so receiver can start deshredding immediately),
        // then coding shreds (for loss recovery).
        let all_shreds: Vec<Shred> = slot_shreds
            .data
            .into_iter()
            .chain(slot_shreds.coding)
            .collect();

        for shred in &all_shreds {
            if Instant::now() >= deadline {
                break 'outer;
            }

            if let Some(t) = ticker.as_mut() {
                t.tick().await;
            }

            let payload: &[u8] = &*shred.payload();

            // Record send timestamp keyed by (slot, index) for RTT tracking.
            if args.recv_port.is_some() {
                if let Some(index) = shred::layout::get_index(payload) {
                    rtt_map.lock().await.insert((slot, index), Instant::now());
                }
            }

            match send_sock.send(payload).await {
                Ok(_) => {
                    total_sent += 1;
                    if total_sent % 10_000 == 0 {
                        info!(
                            sent = total_sent,
                            failed = total_failed,
                            current_slot = slot,
                            "progress"
                        );
                    }
                }
                Err(e) => {
                    total_failed += 1;
                    warn!("send failed: {e}");
                }
            }
        }

        slot += 1;
    }

    // Signal RTT task to stop and flush final stats.
    let _ = stop_tx.send(true);
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
