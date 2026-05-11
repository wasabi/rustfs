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

#![allow(non_upper_case_globals)] // FIXME

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::{
    LazyLock,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::RwLock;
use tonic::transport::Channel;

// Env var names — must match the constants defined in rustfs-config.
const ENV_RPC_CHANNEL_POOL_SIZE: &str = "RUSTFS_RPC_CHANNEL_POOL_SIZE";
const ENV_RUNTIME_WORKER_THREADS: &str = "RUSTFS_RUNTIME_WORKER_THREADS";

/// One gRPC connection is created per this many Tokio worker threads.
/// At the default of ~16 workers this yields N=4, cutting per-connection stream
/// counts from ~100 (N=1) to ~25 (N=4), the empirically measured sweet spot.
const RPC_POOL_THREADS_PER_CONN: usize = 4;

/// Safety cap on per-peer channel pool size to prevent FD runaway on high-core hosts.
/// Mirrors RPC_MAX_POOL_SIZE in rustfs-config.
pub const RPC_MAX_POOL_SIZE: usize = 16;

/// A pool of gRPC channels to a single peer address.
///
/// Channels are selected in round-robin order to spread concurrent gRPC stream load
/// across multiple H2 connections, keeping per-connection stream counts manageable.
/// `Channel` is Arc-backed so cloning is cheap.
pub struct ConnPoolEntry {
    channels: Vec<Channel>,
    next: AtomicUsize,
}

impl ConnPoolEntry {
    pub fn new(channels: Vec<Channel>) -> Self {
        assert!(!channels.is_empty(), "channel pool must not be empty");
        Self {
            channels,
            next: AtomicUsize::new(0),
        }
    }

    /// Return the next channel using per-entry round-robin selection.
    pub fn next_channel(&self) -> Channel {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.channels.len();
        self.channels[idx].clone()
    }

    pub fn len(&self) -> usize {
        self.channels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }
}

/// Compute the per-peer gRPC channel pool size.
///
/// If `override_val` is `Some`, it is used directly (clamped to `[1, RPC_MAX_POOL_SIZE]`).
/// Otherwise the size is derived from `worker_threads`: one connection per
/// `RPC_POOL_THREADS_PER_CONN` workers, clamped to `[1, RPC_MAX_POOL_SIZE]`.
pub fn compute_pool_size(worker_threads: usize, override_val: Option<usize>) -> usize {
    if let Some(n) = override_val {
        return n.clamp(1, RPC_MAX_POOL_SIZE);
    }
    (worker_threads / RPC_POOL_THREADS_PER_CONN).clamp(1, RPC_MAX_POOL_SIZE)
}

/// Number of gRPC channels to maintain per peer address. Computed once at first use.
///
/// Override with `RUSTFS_RPC_CHANNEL_POOL_SIZE`. When unset, derived from
/// `RUSTFS_RUNTIME_WORKER_THREADS` (or physical CPU count): one connection per
/// four worker threads, clamped to `[1, 16]`. Pool size `1` is equivalent to no pool.
pub static GLOBAL_CONN_POOL_SIZE: LazyLock<usize> = LazyLock::new(|| {
    let override_val = std::env::var(ENV_RPC_CHANNEL_POOL_SIZE)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0);

    let worker_threads = std::env::var(ENV_RUNTIME_WORKER_THREADS)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4));

    compute_pool_size(worker_threads, override_val)
});

pub static GLOBAL_LOCAL_NODE_NAME: LazyLock<RwLock<String>> = LazyLock::new(|| RwLock::new("".to_string()));
pub static GLOBAL_RUSTFS_HOST: LazyLock<RwLock<String>> = LazyLock::new(|| RwLock::new("".to_string()));
pub static GLOBAL_RUSTFS_PORT: LazyLock<RwLock<String>> = LazyLock::new(|| RwLock::new("9000".to_string()));
pub static GLOBAL_RUSTFS_ADDR: LazyLock<RwLock<String>> = LazyLock::new(|| RwLock::new("".to_string()));
pub static GLOBAL_CONN_MAP: LazyLock<RwLock<HashMap<String, ConnPoolEntry>>> = LazyLock::new(|| RwLock::new(HashMap::new()));
pub static GLOBAL_ROOT_CERT: LazyLock<RwLock<Option<Vec<u8>>>> = LazyLock::new(|| RwLock::new(None));
pub static GLOBAL_MTLS_IDENTITY: LazyLock<RwLock<Option<MtlsIdentityPem>>> = LazyLock::new(|| RwLock::new(None));
/// Global initialization time of the RustFS node.
pub static GLOBAL_INIT_TIME: LazyLock<RwLock<Option<DateTime<Utc>>>> = LazyLock::new(|| RwLock::new(None));

/// Set the global local node name.
///
/// # Arguments
/// * `name` - A string slice representing the local node name.
pub async fn set_global_local_node_name(name: &str) {
    *GLOBAL_LOCAL_NODE_NAME.write().await = name.to_string();
}

/// Get the global local node name.
///
/// # Returns
/// * `String` - The local node name.
pub async fn get_global_local_node_name() -> String {
    GLOBAL_LOCAL_NODE_NAME.read().await.clone()
}

/// Set the global RustFS initialization time to the current UTC time.
pub async fn set_global_init_time_now() {
    let now = Utc::now();
    *GLOBAL_INIT_TIME.write().await = Some(now);
}

/// Get the global RustFS initialization time.
///
/// # Returns
/// * `Option<DateTime<Utc>>` - The initialization time if set.
pub async fn get_global_init_time() -> Option<DateTime<Utc>> {
    *GLOBAL_INIT_TIME.read().await
}

/// Set the global RustFS address used for gRPC connections.
///
/// # Arguments
/// * `addr` - A string slice representing the RustFS address (e.g., "https://node1:9000").
pub async fn set_global_addr(addr: &str) {
    *GLOBAL_RUSTFS_ADDR.write().await = addr.to_string();
}

/// Set the global root CA certificate for outbound gRPC clients.
/// This certificate is used to validate server TLS certificates.
/// When set to None, clients use the system default root CAs.
///
/// # Arguments
/// * `cert` - A vector of bytes representing the PEM-encoded root CA certificate.
pub async fn set_global_root_cert(cert: Vec<u8>) {
    *GLOBAL_ROOT_CERT.write().await = Some(cert);
}

/// Set the global mTLS identity (cert+key PEM) for outbound gRPC clients.
/// When set, clients will present this identity to servers requesting/requiring mTLS.
/// When None, clients proceed with standard server-authenticated TLS.
///
/// # Arguments
/// * `identity` - An optional MtlsIdentityPem struct containing the cert and key PEM.
pub async fn set_global_mtls_identity(identity: Option<MtlsIdentityPem>) {
    *GLOBAL_MTLS_IDENTITY.write().await = identity;
}

/// Evict a stale/dead connection pool from the global cache.
/// This is critical for cluster recovery when a node dies unexpectedly (e.g., power-off).
/// All channels in the pool for the given address are dropped; the next request will
/// establish a fresh pool.
///
/// # Arguments
/// * `addr` - The address of the connection pool to evict.
pub async fn evict_connection(addr: &str) {
    let removed = GLOBAL_CONN_MAP.write().await.remove(addr);
    if removed.is_some() {
        tracing::warn!("Evicted stale connection pool from cache: {}", addr);
    }
}

/// Check if a connection pool exists in the cache for the given address.
///
/// # Arguments
/// * `addr` - The address to check.
///
/// # Returns
/// * `bool` - True if a cached connection pool exists, false otherwise.
pub async fn has_cached_connection(addr: &str) -> bool {
    GLOBAL_CONN_MAP.read().await.contains_key(addr)
}

/// Clear all cached connection pools. Useful for full cluster reset/recovery.
pub async fn clear_all_connections() {
    let mut map = GLOBAL_CONN_MAP.write().await;
    let count = map.len();
    map.clear();
    if count > 0 {
        tracing::warn!("Cleared {} cached connection pools from global map", count);
    }
}

/// Optional client identity (cert+key PEM) for outbound mTLS.
///
/// When present, gRPC clients will present this identity to servers requesting/requiring mTLS.
/// When absent, clients proceed with standard server-authenticated TLS.
#[derive(Clone, Debug)]
pub struct MtlsIdentityPem {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::transport::Endpoint;

    fn lazy_channel() -> Channel {
        Endpoint::from_static("http://127.0.0.1:9999").connect_lazy()
    }

    #[tokio::test]
    async fn pool_entry_len_matches_input() {
        for n in [1usize, 2, 4, 8] {
            let entry = ConnPoolEntry::new((0..n).map(|_| lazy_channel()).collect());
            assert_eq!(entry.len(), n);
        }
    }

    #[tokio::test]
    async fn pool_entry_round_robin_uniform() {
        let n = 4usize;
        let entry = ConnPoolEntry::new((0..n).map(|_| lazy_channel()).collect());
        // Counter starts at 0 and advances by 1 with each call.
        // After n*3 calls the counter is n*3 and idx wraps back to 0.
        for expected_counter in 0..(n * 3) {
            assert_eq!(entry.next.load(Ordering::Relaxed), expected_counter);
            entry.next_channel();
        }
        assert_eq!(entry.next.load(Ordering::Relaxed) % n, 0);
    }

    #[tokio::test]
    async fn pool_entry_n1_always_zero_idx() {
        let entry = ConnPoolEntry::new(vec![lazy_channel()]);
        for _ in 0..100 {
            entry.next_channel();
        }
        // Counter advanced to 100 without panic; with a single-element pool idx = counter % 1
        // is always 0, so every call returns the one channel. Verify counter progression.
        assert_eq!(entry.next.load(Ordering::Relaxed), 100);
    }

    #[tokio::test]
    async fn per_address_index_independence() {
        let n = 4usize;
        let a = ConnPoolEntry::new((0..n).map(|_| lazy_channel()).collect());
        let b = ConnPoolEntry::new((0..n).map(|_| lazy_channel()).collect());
        // Advance A by 3 steps; B must remain at 0.
        a.next_channel();
        a.next_channel();
        a.next_channel();
        assert_eq!(a.next.load(Ordering::Relaxed), 3);
        assert_eq!(b.next.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn compute_pool_size_uses_override() {
        assert_eq!(compute_pool_size(16, Some(7)), 7);
        assert_eq!(compute_pool_size(16, Some(0)), 1); // 0 clamped to 1
        assert_eq!(compute_pool_size(16, Some(100)), RPC_MAX_POOL_SIZE); // clamped to max
    }

    #[test]
    fn compute_pool_size_derived_from_workers() {
        assert_eq!(compute_pool_size(16, None), 4); // 16/4 = 4
        assert_eq!(compute_pool_size(32, None), 8); // 32/4 = 8
        assert_eq!(compute_pool_size(1, None), 1); // min clamp: 1/4 = 0 → 1
        assert_eq!(compute_pool_size(0, None), 1); // min clamp: 0/4 = 0 → 1
        assert_eq!(compute_pool_size(64, None), 16); // max clamp: 64/4 = 16
        assert_eq!(compute_pool_size(128, None), RPC_MAX_POOL_SIZE); // capped
    }
}
