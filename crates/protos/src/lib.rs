// Copyright 2024 RustFS Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#[allow(unsafe_code)]
mod generated;

use proto_gen::node_service::node_service_client::NodeServiceClient;
use rustfs_common::{
    ConnPoolEntry, GLOBAL_CONN_MAP, GLOBAL_CONN_POOL_SIZE, GLOBAL_MTLS_IDENTITY, GLOBAL_ROOT_CERT, evict_connection,
};
use std::{error::Error, time::Duration};
use tonic::{
    Request, Status,
    service::interceptor::InterceptedService,
    transport::{Certificate, Channel, ClientTlsConfig, Endpoint},
};
use tracing::{debug, warn};

// Type alias for the complex client type
pub type NodeServiceClientType = NodeServiceClient<
    InterceptedService<Channel, Box<dyn Fn(Request<()>) -> Result<Request<()>, Status> + Send + Sync + 'static>>,
>;

pub use generated::*;

// Default 100 MB
pub const DEFAULT_GRPC_SERVER_MESSAGE_LEN: usize = 100 * 1024 * 1024;

/// Timeout for connection establishment - reduced for faster failure detection
const CONNECT_TIMEOUT_SECS: u64 = 3;

/// TCP keepalive interval - how often to probe the connection
const TCP_KEEPALIVE_SECS: u64 = 10;

/// HTTP/2 keepalive interval - application-layer heartbeat
const HTTP2_KEEPALIVE_INTERVAL_SECS: u64 = 5;

/// HTTP/2 keepalive timeout - how long to wait for PING ACK
const HTTP2_KEEPALIVE_TIMEOUT_SECS: u64 = 3;

/// Overall RPC timeout - maximum time for any single RPC operation
const RPC_TIMEOUT_SECS: u64 = 30;

/// Default HTTPS prefix for rustfs
/// This is the default HTTPS prefix for rustfs.
/// It is used to identify HTTPS URLs.
/// Default value: https://
const RUSTFS_HTTPS_PREFIX: &str = "https://";

/// Dial a single gRPC channel to `addr` with standard keepalive and TLS settings.
/// Does not insert into the global cache — use `get_or_create_pool_channel` for that.
/// Build a fully-configured `Endpoint` for `addr`, applying TCP/HTTP2 timeouts and TLS
/// settings from globals. The endpoint can then be connected eagerly (`connect().await`) or
/// lazily (`connect_lazy()`).
async fn build_endpoint(addr: &str) -> Result<Endpoint, Box<dyn Error>> {
    let mut connector = Endpoint::from_shared(addr.to_string())?
        // Fast connection timeout for dead peer detection
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        // TCP-level keepalive - OS will probe connection
        .tcp_keepalive(Some(Duration::from_secs(TCP_KEEPALIVE_SECS)))
        // HTTP/2 PING frames for application-layer health check
        .http2_keep_alive_interval(Duration::from_secs(HTTP2_KEEPALIVE_INTERVAL_SECS))
        // How long to wait for PING ACK before considering connection dead
        .keep_alive_timeout(Duration::from_secs(HTTP2_KEEPALIVE_TIMEOUT_SECS))
        // Send PINGs even when no active streams (critical for idle connections)
        .keep_alive_while_idle(true)
        // Overall timeout for any RPC - fail fast on unresponsive peers
        .timeout(Duration::from_secs(RPC_TIMEOUT_SECS));

    let root_cert = GLOBAL_ROOT_CERT.read().await;
    if addr.starts_with(RUSTFS_HTTPS_PREFIX) {
        if root_cert.is_none() {
            debug!("No custom root certificate configured; using system roots for TLS: {}", addr);
            // If no custom root cert is configured, try to use system roots.
            connector = connector.tls_config(ClientTlsConfig::new())?;
        }
        if let Some(cert_pem) = root_cert.as_ref() {
            let ca = Certificate::from_pem(cert_pem);
            // Derive the hostname from the HTTPS URL for TLS hostname verification.
            let domain = addr
                .trim_start_matches(RUSTFS_HTTPS_PREFIX)
                .split('/')
                .next()
                .unwrap_or("")
                .split(':')
                .next()
                .unwrap_or("");
            let tls = if !domain.is_empty() {
                let mut cfg = ClientTlsConfig::new().ca_certificate(ca).domain_name(domain);
                let mtls_identity = GLOBAL_MTLS_IDENTITY.read().await;
                if let Some(id) = mtls_identity.as_ref() {
                    let identity = tonic::transport::Identity::from_pem(id.cert_pem.clone(), id.key_pem.clone());
                    cfg = cfg.identity(identity);
                }
                cfg
            } else {
                // Fallback: configure TLS without explicit domain if parsing fails.
                ClientTlsConfig::new().ca_certificate(ca)
            };
            connector = connector.tls_config(tls)?;
            debug!("Configured TLS with custom root certificate for: {}", addr);
        } else {
            return Err(std::io::Error::other(
                "HTTPS requested but no trusted roots are configured. Provide tls/ca.crt (or enable system roots via RUSTFS_TRUST_SYSTEM_CA=true)."
            ).into());
        }
    }

    Ok(connector)
}

/// Return a channel from the per-peer pool, creating the pool on first use.
///
/// Pool size N is determined by `GLOBAL_CONN_POOL_SIZE` (computed from worker thread count,
/// overridable via `RUSTFS_RPC_CHANNEL_POOL_SIZE`). Channels are selected round-robin per
/// peer address, distributing stream load across N independent H2 connections.
///
/// On eviction (`evict_connection`) the entire pool for the address is dropped; the next call
/// rebuilds it. If two concurrent callers both find no pool entry, one set of N channels is
/// kept and the other is dropped (last writer wins — same as the prior single-channel behavior).
pub async fn get_or_create_pool_channel(addr: &str) -> Result<Channel, Box<dyn Error>> {
    // Fast path: pool already exists.
    {
        let map = GLOBAL_CONN_MAP.read().await;
        if let Some(entry) = map.get(addr) {
            debug!("Using pooled gRPC channel for: {}", addr);
            return Ok(entry.next_channel());
        }
    }

    // Slow path: build one endpoint (reads TLS config once), then create N lazy channels.
    // connect_lazy() returns immediately and each channel establishes its TCP/TLS connection
    // on first use — all N connections start concurrently when the first RPCs are dispatched,
    // avoiding the N×timeout latency of sequential eager dialing.
    let pool_size = *GLOBAL_CONN_POOL_SIZE;
    debug!("Creating gRPC channel pool (size={}) for: {}", pool_size, addr);

    let endpoint = build_endpoint(addr).await?;
    let channels: Vec<Channel> = (0..pool_size).map(|_| endpoint.connect_lazy()).collect();

    let mut map = GLOBAL_CONN_MAP.write().await;
    // If a concurrent caller inserted first, use their pool and drop ours.
    map.entry(addr.to_string()).or_insert_with(|| ConnPoolEntry::new(channels));
    Ok(map.get(addr).unwrap().next_channel())
}

/// Create a new gRPC channel to `addr`, caching it in the global connection pool.
///
/// Delegates to `get_or_create_pool_channel`. Kept for backward compatibility.
pub async fn create_new_channel(addr: &str) -> Result<Channel, Box<dyn Error>> {
    get_or_create_pool_channel(addr).await
}

/// Evict a connection from the cache after a failure.
/// This should be called when an RPC fails to ensure fresh connections are tried.
pub async fn evict_failed_connection(addr: &str) {
    warn!("Evicting failed gRPC connection pool: {}", addr);
    evict_connection(addr).await;
}
