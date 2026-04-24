//! nonce-bench — sends the same durable-nonce transaction to multiple tip accounts
//! via a single QUIC endpoint.  All transactions share the same nonce (same
//! recent_blockhash) but differ only in the tip recipient, which lets you test
//! how a validator/relay ranks duplicate-nonce bundles by tip size.
//!
//! Transaction layout (per tip account):
//!   1. SystemInstruction::AdvanceNonceAccount  (nonce_authority signs)
//!   2. ComputeBudgetInstruction::SetComputeUnitPrice  (priority fee)
//!   3. SystemInstruction::Transfer  payer → tip_account  (tip lamports)

use anyhow::{Context, Result};
use clap::Parser;
use quic_client::{config::QuicSettings, quic_client::QuicClient};
use solana_commitment_config::CommitmentConfig;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_hash::Hash;
use solana_keypair::{Keypair, read_keypair_file};
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::{
    instruction::Instruction, message::Message, signature::Signature, transaction::Transaction,
};
use solana_signer::Signer;
use solana_system_interface::instruction as system_instruction;

// ── Nonce account deserialization ────────────────────────────────────────────
// We define the structs locally to avoid pinning to a specific nonce crate API.
// These match the bincode layout of the on-chain nonce account verbatim.

#[derive(serde::Deserialize)]
struct NonceFeeCalculator {
    #[allow(dead_code)]
    lamports_per_signature: u64,
}

#[derive(serde::Deserialize)]
struct NonceDurableNonce(Hash);

#[derive(serde::Deserialize)]
struct NonceData {
    #[allow(dead_code)]
    authority: Pubkey,
    durable_nonce: NonceDurableNonce,
    #[allow(dead_code)]
    fee_calculator: NonceFeeCalculator,
}

#[derive(serde::Deserialize)]
enum NonceState {
    Uninitialized,
    Initialized(NonceData),
}

// Agave serialises nonce accounts wrapped in a Versions enum (bincode variant index).
#[derive(serde::Deserialize)]
enum NonceVersions {
    Legacy(Box<NonceState>),
    Current(Box<NonceState>),
}
use std::{
    net::SocketAddr,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use futures_util::future::join_all;
use tokio::sync::RwLock;
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[clap(
    name = "nonce-bench",
    about = "Send duplicate-nonce transactions with varying tip accounts to a QUIC endpoint"
)]
struct Args {
    /// Target QUIC endpoint (IP:PORT)
    #[clap(long)]
    quic_addr: SocketAddr,

    /// Path to payer keypair (also used as nonce authority if --nonce-authority not given)
    #[clap(long, default_value = "~/.config/solana/id.json")]
    keypair: String,

    /// Path to nonce authority keypair (defaults to --keypair)
    #[clap(long)]
    nonce_authority: Option<String>,

    /// Durable nonce account pubkey
    #[clap(long)]
    nonce_account: String,

    /// Tip accounts (space-separated pubkeys). One transaction is sent per account.
    /// Defaults to the 8 Jito mainnet tip accounts.
    #[clap(long, num_args = 1.., value_delimiter = ' ')]
    tip_accounts: Option<Vec<String>>,

    /// Lamports to transfer as tip per transaction
    #[clap(long, default_value_t = 1_000)]
    tip_lamports: u64,

    /// Compute unit price (micro-lamports) for priority fee
    #[clap(long, default_value_t = 100_000)]
    priority_fee: u64,

    /// Number of times to repeat the full bundle (0 = run once)
    #[clap(long, default_value_t = 0)]
    iterations: u32,

    /// Delay between iterations in milliseconds
    #[clap(long, default_value_t = 500)]
    iter_delay_ms: u64,

    /// RPC URL to fetch nonce account state
    #[clap(long, default_value = "https://api.mainnet-beta.solana.com")]
    rpc_url: String,

    /// Number of parallel QUIC endpoints
    #[clap(long, default_value_t = 2)]
    num_endpoints: usize,
}

// Jito mainnet tip accounts (used when --tip-accounts is not specified).
const JITO_TIP_ACCOUNTS: &[&str] = &[
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1sPn9cEoRhI",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tFAcEJOKYPQj4hQAUWWBYE8f6vUz5tDN",
];

/// Deserialise a nonce account and return the durable nonce hash used as
/// `recent_blockhash` in durable-nonce transactions.
fn nonce_blockhash(account_data: &[u8]) -> Result<Hash> {
    let versions: NonceVersions =
        bincode::deserialize(account_data).context("deserialize nonce account")?;
    let state = match versions {
        NonceVersions::Legacy(s) | NonceVersions::Current(s) => *s,
    };
    match state {
        NonceState::Initialized(data) => Ok(data.durable_nonce.0),
        NonceState::Uninitialized => anyhow::bail!("nonce account is uninitialized"),
    }
}

/// Build a single nonce-tip transaction.
fn build_nonce_tip_tx(
    payer: &Keypair,
    nonce_authority: &Keypair,
    nonce_account: &Pubkey,
    tip_account: &Pubkey,
    tip_lamports: u64,
    priority_fee: u64,
    nonce_hash: Hash,
) -> Result<(Vec<u8>, Signature)> {
    let advance_ix =
        system_instruction::advance_nonce_account(nonce_account, &nonce_authority.pubkey());
    let priority_ix = ComputeBudgetInstruction::set_compute_unit_price(priority_fee);
    // Random memo makes every call produce a distinct transaction even with identical inputs.
    let memo_ix = Instruction {
        program_id: "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"
            .parse()
            .unwrap(),
        accounts: vec![],
        data: rand::random::<u64>().to_string().into_bytes(),
    };
    let tip_ix = system_instruction::transfer(&payer.pubkey(), tip_account, tip_lamports);

    // The payer and nonce_authority may be the same keypair or different.
    // We collect unique signers (dedup by pubkey).
    let mut signers: Vec<&dyn solana_signer::Signer> = vec![payer];
    if nonce_authority.pubkey() != payer.pubkey() {
        signers.push(nonce_authority);
    }

    let msg = Message::new(
        &[advance_ix, priority_ix, memo_ix, tip_ix],
        Some(&payer.pubkey()),
    );
    let tx = Transaction::new(&signers, msg, nonce_hash);
    let sig = tx.signatures[0];
    let wire = bincode::serialize(&tx).context("serialize tx")?;
    Ok((wire, sig))
}

async fn run_iteration(
    client: &Arc<QuicClient>,
    payer: &Arc<Keypair>,
    nonce_authority: &Arc<Keypair>,
    nonce_account: &Pubkey,
    tip_accounts: &[Pubkey],
    tip_lamports: u64,
    priority_fee: u64,
    nonce_hash: Hash,
) {
    let mut handles = Vec::with_capacity(tip_accounts.len());

    for tip in tip_accounts {
        let (wire, sig) = match build_nonce_tip_tx(
            payer,
            nonce_authority,
            nonce_account,
            tip,
            tip_lamports,
            priority_fee,
            nonce_hash,
        ) {
            Ok(b) => b,
            Err(e) => {
                warn!(tip = %tip, "build tx failed: {e}");
                continue;
            }
        };

        let cli = client.clone();
        let tip_key = *tip;
        handles.push(tokio::spawn(async move {
            match cli.send_transaction(&wire).await {
                Ok(r) => info!(signature = %sig, tip = %tip_key, nonce = %nonce_hash, latency_ms = r.latency_ms, "sent"),
                Err(e) => warn!(signature = %sig, tip = %tip_key, "send failed: {e}"),
            }
        }));
    }

    join_all(handles).await;
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let payer = Arc::new(
        read_keypair_file(&args.keypair)
            .map_err(|e| anyhow::anyhow!("read keypair {e}"))?,
    );

    let nonce_auth_path = args
        .nonce_authority
        .as_deref()
        .unwrap_or(&args.keypair)
        .to_string();
    let nonce_authority = Arc::new(
        read_keypair_file(&nonce_auth_path)
            .map_err(|e| anyhow::anyhow!("read nonce authority {nonce_auth_path}: {e}"))?,
    );

    let nonce_account =
        Pubkey::from_str(&args.nonce_account).context("invalid nonce account pubkey")?;

    let tip_accounts: Vec<Pubkey> = match &args.tip_accounts {
        Some(list) => list
            .iter()
            .map(|s| Pubkey::from_str(s).context("invalid tip account pubkey"))
            .collect::<Result<_>>()?,
        None => JITO_TIP_ACCOUNTS
            .iter()
            .map(|s| Pubkey::from_str(s).unwrap())
            .collect(),
    };

    let mut settings = QuicSettings::default();
    settings.set_num_endpoint(args.num_endpoints);
    settings.max_send_attempts = 1;
    settings.send_timeout_secs = 3;

    let peer_addr = Arc::new(RwLock::new(args.quic_addr));
    let client = Arc::new(QuicClient::new(peer_addr, settings)?);

    // Fetch nonce state once per iteration (nonce advances after each on-chain tx,
    // but for the bench we intentionally reuse the same hash).
    let rpc_url = args.rpc_url.clone();
    let nonce_account_key = nonce_account;

    let iterations = args.iterations.max(1);
    let iter_delay = Duration::from_millis(args.iter_delay_ms);

    for i in 0..iterations {
        let url = rpc_url.clone();
        let nonce_data = tokio::task::spawn_blocking(move || {
            RpcClient::new_with_commitment(url, CommitmentConfig::finalized())
                .get_account_data(&nonce_account_key)
        })
        .await??;

        let nonce_hash = nonce_blockhash(&nonce_data)?;

        info!(
            iteration = i + 1,
            nonce_hash = %nonce_hash,
            tip_count = tip_accounts.len(),
            "sending nonce bundle"
        );

        let t0 = Instant::now();
        run_iteration(
            &client,
            &payer,
            &nonce_authority,
            &nonce_account,
            &tip_accounts,
            args.tip_lamports,
            args.priority_fee,
            nonce_hash,
        )
        .await;

        info!(
            iteration = i + 1,
            elapsed_ms = t0.elapsed().as_millis(),
            "iteration done"
        );

        if i + 1 < iterations {
            tokio::time::sleep(iter_delay).await;
        }
    }

    Ok(())
}
