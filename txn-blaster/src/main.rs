use anyhow::Result;
use clap::Parser;
use quic_client::{config::QuicSettings, quic_client::QuicClient};
use solana_commitment_config::CommitmentConfig;
use solana_hash::Hash;
use solana_keypair::read_keypair_file;
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::{message::Message, transaction::Transaction};
use solana_signer::Signer;
use solana_system_interface::instruction as system_instruction;
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[clap(name = "txn-blaster", about = "Blast transactions to a Solana TPU QUIC endpoint")]
struct Args {
    /// Target QUIC endpoint (IP:PORT)
    #[clap(long)]
    quic_addr: SocketAddr,

    /// Path to payer keypair JSON file
    #[clap(long, default_value = "~/.config/solana/id.json")]
    keypair: String,

    /// RPC URL for fetching blockhash
    #[clap(long, default_value = "https://api.mainnet-beta.solana.com")]
    rpc_url: String,

    /// Target transactions per second
    #[clap(long, default_value_t = 100)]
    tps: u32,

    /// Duration to run in seconds
    #[clap(long, default_value_t = 30)]
    duration: u64,

    /// Number of parallel QUIC endpoints (improves throughput)
    #[clap(long, default_value_t = 4)]
    num_endpoints: usize,

    /// Max send attempts per transaction
    #[clap(long, default_value_t = 1)]
    max_attempts: usize,
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        format!("{}/{}", home, rest)
    } else {
        path.to_string()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let keypair_path = expand_tilde(&args.keypair);
    let keypair = Arc::new(
        read_keypair_file(&keypair_path)
            .map_err(|e| anyhow::anyhow!("Failed to read keypair from {keypair_path}: {e}"))?,
    );

    let mut settings = QuicSettings::default();
    settings.set_num_endpoint(args.num_endpoints);
    settings.max_send_attempts = args.max_attempts;
    settings.send_timeout_secs = 2;

    let peer_addr = Arc::new(RwLock::new(args.quic_addr));
    let client = Arc::new(QuicClient::new(peer_addr, settings)?);

    // Fetch initial blockhash synchronously before entering the async loop.
    let rpc_url = args.rpc_url.clone();
    let initial_bh = tokio::task::spawn_blocking({
        let url = rpc_url.clone();
        move || {
            RpcClient::new_with_commitment(url, CommitmentConfig::confirmed())
                .get_latest_blockhash()
        }
    })
    .await??;

    let blockhash = Arc::new(RwLock::new(initial_bh));

    // Background task: refresh blockhash every 25 seconds.
    {
        let bh = blockhash.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(25)).await;
                let url = rpc_url.clone();
                match tokio::task::spawn_blocking(move || {
                    RpcClient::new_with_commitment(url, CommitmentConfig::confirmed())
                        .get_latest_blockhash()
                })
                .await
                {
                    Ok(Ok(new_bh)) => *bh.write().await = new_bh,
                    Ok(Err(e)) => warn!("blockhash refresh failed: {e}"),
                    Err(e) => warn!("blockhash task panicked: {e}"),
                }
            }
        });
    }

    let sent = Arc::new(AtomicU64::new(0));
    let success = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let total_latency_ms = Arc::new(AtomicU64::new(0));

    let interval_us = 1_000_000u64 / args.tps as u64;
    let mut ticker = tokio::time::interval(Duration::from_micros(interval_us));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let deadline = Instant::now() + Duration::from_secs(args.duration);

    info!(
        tps = args.tps,
        duration_secs = args.duration,
        target = %args.quic_addr,
        payer = %keypair.pubkey(),
        "txn-blaster starting"
    );

    while Instant::now() < deadline {
        ticker.tick().await;

        let bh: Hash = *blockhash.read().await;
        let kp = keypair.clone();
        let cli = client.clone();
        let sent_c = sent.clone();
        let succ_c = success.clone();
        let fail_c = failed.clone();
        let lat_c = total_latency_ms.clone();

        // Each send is spawned so the ticker keeps ticking at the target rate.
        tokio::spawn(async move {
            let ix = system_instruction::transfer(&kp.pubkey(), &kp.pubkey(), 1);
            let msg = Message::new(&[ix], Some(&kp.pubkey()));
            let tx = Transaction::new(&[kp.as_ref()], msg, bh);
            let wire = match bincode::serialize(&tx) {
                Ok(b) => b,
                Err(e) => {
                    warn!("serialize failed: {e}");
                    return;
                }
            };

            sent_c.fetch_add(1, Ordering::Relaxed);
            match cli.send_transaction(&wire).await {
                Ok(r) => {
                    succ_c.fetch_add(1, Ordering::Relaxed);
                    lat_c.fetch_add(r.latency_ms, Ordering::Relaxed);
                }
                Err(e) => {
                    fail_c.fetch_add(1, Ordering::Relaxed);
                    warn!("send error: {e}");
                }
            }
        });
    }

    // Give in-flight sends a moment to finish.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let s = sent.load(Ordering::Relaxed);
    let ok = success.load(Ordering::Relaxed);
    let err = failed.load(Ordering::Relaxed);
    let avg_lat = if ok > 0 {
        total_latency_ms.load(Ordering::Relaxed) / ok
    } else {
        0
    };

    info!(
        sent = s,
        success = ok,
        failed = err,
        avg_latency_ms = avg_lat,
        actual_tps = s as f64 / args.duration as f64,
        "txn-blaster finished"
    );

    Ok(())
}
