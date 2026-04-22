use crate::config::QuicSettings;
use anyhow::{Context, Result};
use dashmap::DashMap;
use quinn::{
    ClientConfig, Connection as QuinnConnection, Endpoint, IdleTimeout, TransportConfig,
    crypto::rustls::QuicClientConfig,
};
use solana_keypair::Keypair;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// ALPN protocol identifier for Solana TPU.
const ALPN_TPU_PROTOCOL_ID: &[u8] = b"solana-tpu";

/// Generates proper QUIC server name (SNI) from socket address.
fn socket_addr_to_quic_server_name(addr: &SocketAddr) -> String {
    format!("{}.{}.sol", addr.ip(), addr.port())
}

#[derive(Debug, Clone)]
pub struct SendResult {
    pub latency_ms: u64,
    pub attempts: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("send timed out after {elapsed_ms}ms over {attempts} attempt(s)")]
    Timeout { elapsed_ms: u64, attempts: usize },

    #[error("all {attempts} attempt(s) failed in {latency_ms}ms: {last_error}")]
    AllAttemptsFailed {
        attempts: usize,
        latency_ms: u64,
        last_error: String,
    },
}

#[derive(Default)]
struct CachedConnection {
    conn: Option<QuinnConnection>,
}

pub struct QuicClient {
    /// Multiple QUIC endpoints to distribute load across.
    endpoints: Vec<Endpoint>,
    /// Cached connections by address.
    connections: Arc<DashMap<String, CachedConnection>>,
    /// Round-robin counter for endpoint selection.
    next_endpoint: Arc<AtomicUsize>,
    /// Shared validator address that can be updated dynamically.
    peer_addr: Arc<RwLock<SocketAddr>>,
    /// QUIC configuration settings.
    quic_settings: QuicSettings,
}

impl QuicClient {
    pub fn new(
        peer_addr: Arc<RwLock<SocketAddr>>,
        quic_settings: QuicSettings,
    ) -> Result<Self> {
        let client_certificate =
            solana_tls_utils::QuicClientCertificate::new(Some(&Keypair::new()));

        let mut crypto = solana_tls_utils::tls_client_config_builder()
            .with_client_auth_cert(
                vec![client_certificate.certificate.clone()],
                client_certificate.key.clone_key(),
            )
            .expect("Failed to set QUIC client certificates");

        // Enable 0-RTT for faster reconnection
        crypto.enable_early_data = true;
        crypto.alpn_protocols = vec![ALPN_TPU_PROTOCOL_ID.to_vec()];

        // Configure transport settings
        let transport_config = {
            let mut config = TransportConfig::default();
            let timeout = IdleTimeout::try_from(quic_settings.max_timeout()).unwrap();
            config.max_idle_timeout(Some(timeout));
            config.keep_alive_interval(Some(quic_settings.keep_alive()));
            config.send_fairness(false);
            config
        };

        let client_config =
            ClientConfig::new(Arc::new(QuicClientConfig::try_from(crypto).unwrap()));
        let client_config = {
            let mut cfg = client_config;
            cfg.transport_config(Arc::new(transport_config));
            cfg
        };

        let mut endpoints = Vec::with_capacity(quic_settings.num_endpoints);
        for i in 0..quic_settings.num_endpoints {
            let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)
                .context(format!("Failed to create QUIC endpoint {}", i))?;
            endpoint.set_default_client_config(client_config.clone());
            endpoints.push(endpoint);
        }

        info!("Created QUIC client");

        Ok(Self {
            endpoints,
            connections: Arc::new(DashMap::new()),
            next_endpoint: Arc::new(AtomicUsize::new(0)),
            peer_addr,
            quic_settings,
        })
    }

    /// Selects the next endpoint using round-robin distribution.
    fn select_endpoint(&self) -> &Endpoint {
        let idx = self.next_endpoint.fetch_add(1, Ordering::Relaxed) % self.endpoints.len();
        &self.endpoints[idx]
    }

    /// Called when validator address changes - clears all cached connections.
    pub async fn on_address_changed(&self, new_addr: SocketAddr) {
        info!(
            "QUIC client: Validator address changed to {}, clearing connection cache",
            new_addr
        );
        self.close_all();
    }

    pub async fn send_transaction(&self, transaction_data: &[u8]) -> Result<SendResult, SendError> {
        let send_timeout = self.quic_settings.send_timeout();
        let max_attempts = self.quic_settings.max_send_attempts;

        tokio::time::timeout(
            send_timeout,
            self.send_transaction_with_retry(transaction_data),
        )
        .await
        .unwrap_or(Err(SendError::Timeout {
            elapsed_ms: send_timeout.as_millis() as u64,
            attempts: max_attempts,
        }))
    }

    /// Internal method with retry logic.
    async fn send_transaction_with_retry(
        &self,
        transaction_data: &[u8],
    ) -> Result<SendResult, SendError> {
        let start = Instant::now();
        let max_attempts = self.quic_settings.max_send_attempts;
        let retry_delay = self.quic_settings.retry_delay();
        let mut last_error = String::new();

        for attempt in 0..max_attempts {
            match self.send_transaction_once(transaction_data).await {
                Ok(_) => {
                    return Ok(SendResult {
                        latency_ms: start.elapsed().as_millis() as u64,
                        attempts: attempt + 1,
                    });
                }
                Err(e) => {
                    last_error = e.to_string();
                    if attempt < max_attempts - 1 {
                        warn!(
                            "Transaction send attempt {} failed: {:?}. Retrying...",
                            attempt + 1,
                            e
                        );
                        tokio::time::sleep(retry_delay).await;
                    }
                }
            }
        }

        Err(SendError::AllAttemptsFailed {
            attempts: max_attempts,
            latency_ms: start.elapsed().as_millis() as u64,
            last_error,
        })
    }

    /// Sends transaction data (single attempt).
    async fn send_transaction_once(&self, transaction_data: &[u8]) -> Result<()> {
        let conn = self
            .get_or_create_connection()
            .await
            .context("Failed to get connection to validator endpoint")?;

        // Open unidirectional stream for transaction
        let mut send_stream = conn
            .open_uni()
            .await
            .context("Failed to open unidirectional stream")?;

        // Write transaction data
        send_stream
            .write_all(transaction_data)
            .await
            .context("Failed to write transaction data")?;

        // Finish the stream
        send_stream.finish().context("Failed to finish stream")?;

        info!(
            "connection: {:?} rtt: {:?}",
            self.peer_addr,
            conn.stats().path.rtt
        );

        Ok(())
    }

    /// Gets an existing connection or creates a new one.
    async fn get_or_create_connection(&self) -> Result<QuinnConnection> {
        let validator_addr = *self.peer_addr.read().await;
        let address = validator_addr.to_string();

        // Check for existing active connection
        if let Some(cached) = self.connections.get(&address) {
            if let Some(ref conn) = cached.conn {
                if conn.close_reason().is_none() {
                    return Ok(conn.clone());
                }
            }
        }

        // Mark as connecting
        self.connections
            .insert(address.clone(), CachedConnection::default());

        // Select endpoint using round-robin for load distribution
        let endpoint = self.select_endpoint();

        // Generate proper SNI
        let server_name = socket_addr_to_quic_server_name(&validator_addr);

        // Try 0-RTT connection first for lower latency
        let connection = match endpoint.connect(validator_addr, &server_name)?.into_0rtt() {
            Ok((conn, rtt_accepted)) => {
                let _ = rtt_accepted.await;
                conn
            }
            Err(connecting) => {
                match connecting.await {
                    Ok(conn) => conn,
                    Err(e) => {
                        // Failed to connect - remove from cache
                        self.connections.remove(&address);
                        return Err(e.into());
                    }
                }
            }
        };
        // Cache the connection
        self.connections.insert(
            address,
            CachedConnection {
                conn: Some(connection.clone()),
            },
        );

        Ok(connection)
    }

    /// Returns the number of active connections.
    pub fn connection_count(&self) -> usize {
        self.connections
            .iter()
            .filter(|entry| {
                entry
                    .value()
                    .conn
                    .as_ref()
                    .map(|c| c.close_reason().is_none())
                    .unwrap_or(false)
            })
            .count()
    }

    /// Closes all connections.
    pub fn close_all(&self) {
        for entry in self.connections.iter() {
            if let Some(ref conn) = entry.value().conn {
                conn.close(0u32.into(), b"shutdown");
            }
        }
        self.connections.clear();
    }
}

impl Drop for QuicClient {
    fn drop(&mut self) {
        self.close_all();
    }
}
