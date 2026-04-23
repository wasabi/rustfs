// Copyright 2024 RustFS Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Priority 3b: sampled holder / wait-queue diagnostics (`rustfs_lock_holder` target).
//!
//! Environment (optional):
//! - `RUSTFS_LOCK_HOLDER_TRACE_SAMPLE` — emit `wait_blocked` / `notify_enter` one in N times (default **32**).
//! - `RUSTFS_LOCK_HOLDER_MIN_HOLD_LOG_MS` — log `exclusive_released` / `shared_released` when hold duration ≥ this (default **20** ms).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use crate::fast_lock::types::{LockMode, ObjectKey, ObjectLockRequest};

/// One shared lock holder for contention snapshots (at most a few per object).
#[derive(Debug, Clone)]
pub struct SharedOwnerBrief {
    pub owner_fp: u64,
    pub held_ms: u128,
    pub prefix: String,
    /// Same correlation as the corresponding read [`ObjectLockRequest`], if provided at acquire.
    pub trace_id: Option<Arc<str>>,
    pub operation_id: Option<Arc<str>>,
}

/// Snapshot for 3b diagnostics (wait queue + current exclusive holder age).
#[derive(Debug, Clone)]
pub struct WaiterContentionSnapshot {
    pub lock_mode: Option<LockMode>,
    pub exclusive_holder_fp: Option<u64>,
    pub exclusive_holder_held_ms: Option<u128>,
    /// Current shared lock holders (up to 4); empty if none.
    pub shared_readers: Vec<SharedOwnerBrief>,
    pub readers: u8,
    pub readers_waiting: u16,
    pub writers_waiting: u16,
}

fn format_shared_readers_compact(readers: &[SharedOwnerBrief]) -> String {
    const MAX_LEN: usize = 512;
    let mut out = String::new();
    for (i, s) in readers.iter().enumerate() {
        if i > 0 {
            out.push('|');
        }
        let op = s
            .operation_id
            .as_deref()
            .or(s.trace_id.as_deref())
            .unwrap_or("-");
        let piece = format!("fp={} op={} held_ms={} prefix={}", s.owner_fp, op, s.held_ms, s.prefix);
        if out.len() + piece.len() > MAX_LEN {
            out.push_str("...(trunc)");
            break;
        }
        out.push_str(&piece);
    }
    out
}

fn format_shared_id_csv(readers: &[SharedOwnerBrief], pick: impl Fn(&SharedOwnerBrief) -> &Option<Arc<str>>) -> String {
    let mut out = String::new();
    for (i, s) in readers.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match pick(s) {
            Some(a) => out.push_str(a.as_ref()),
            None => out.push('-'),
        }
    }
    out
}

const HOLDER_TARGET: &str = "rustfs_lock_holder";

static WAIT_SEQ: AtomicU64 = AtomicU64::new(0);

fn trace_sample_mod() -> u64 {
    static M: OnceLock<u64> = OnceLock::new();
    *M.get_or_init(|| {
        std::env::var("RUSTFS_LOCK_HOLDER_TRACE_SAMPLE")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(32)
    })
}

fn min_hold_log_ms() -> u128 {
    static M: OnceLock<u128> = OnceLock::new();
    *M.get_or_init(|| {
        std::env::var("RUSTFS_LOCK_HOLDER_MIN_HOLD_LOG_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(20) as u128
    })
}

#[inline]
pub fn owner_fingerprint(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[inline]
fn sample_take() -> bool {
    if !tracing::enabled!(target: HOLDER_TARGET, tracing::Level::DEBUG) {
        return false;
    }
    let m = trace_sample_mod();
    WAIT_SEQ.fetch_add(1, Ordering::Relaxed) % m == 0
}

/// First line of contention in slow path (who is ahead, queue depths).
pub fn emit_wait_blocked(request: &ObjectLockRequest, snap: &WaiterContentionSnapshot, retry_count: u32) {
    if !sample_take() {
        return;
    }
    let trace = request.trace_id.as_deref().unwrap_or("");
    let op = request.operation_id.as_deref().unwrap_or("");
    let lock_source = request.lock_source.as_deref().unwrap_or("");
    let lock_source_detail = request.lock_source_detail.as_deref().unwrap_or("");
    tracing::debug!(
        target: HOLDER_TARGET,
        event = "wait_blocked",
        resource = %request.key,
        trace_id = trace,
        operation_id = op,
        lock_source = lock_source,
        lock_source_detail = lock_source_detail,
        mode = ?request.mode,
        retry_count,
        waiter_owner_fp = owner_fingerprint(request.owner.as_ref()),
        lock_mode = ?snap.lock_mode,
        exclusive_holder_fp = ?snap.exclusive_holder_fp,
        exclusive_holder_held_ms = ?snap.exclusive_holder_held_ms,
        readers = snap.readers,
        readers_waiting = snap.readers_waiting,
        writers_waiting = snap.writers_waiting,
        shared_reader_count = snap.shared_readers.len(),
        shared_readers = %format_shared_readers_compact(&snap.shared_readers),
        shared_reader_operation_ids = %format_shared_id_csv(&snap.shared_readers, |s| &s.operation_id),
        shared_reader_trace_ids = %format_shared_id_csv(&snap.shared_readers, |s| &s.trace_id),
        "lock holder snapshot while waiting"
    );
}

/// About to park on notify (after backoff ladder); lower default sample via same counter.
pub fn emit_notify_enter(request: &ObjectLockRequest, snap: &WaiterContentionSnapshot, retry_count: u32) {
    if !sample_take() {
        return;
    }
    let trace = request.trace_id.as_deref().unwrap_or("");
    let op = request.operation_id.as_deref().unwrap_or("");
    let lock_source = request.lock_source.as_deref().unwrap_or("");
    let lock_source_detail = request.lock_source_detail.as_deref().unwrap_or("");
    tracing::debug!(
        target: HOLDER_TARGET,
        event = "notify_enter",
        resource = %request.key,
        trace_id = trace,
        operation_id = op,
        lock_source = lock_source,
        lock_source_detail = lock_source_detail,
        mode = ?request.mode,
        retry_count,
        waiter_owner_fp = owner_fingerprint(request.owner.as_ref()),
        exclusive_holder_fp = ?snap.exclusive_holder_fp,
        exclusive_holder_held_ms = ?snap.exclusive_holder_held_ms,
        readers = snap.readers,
        readers_waiting = snap.readers_waiting,
        writers_waiting = snap.writers_waiting,
        shared_reader_count = snap.shared_readers.len(),
        shared_readers = %format_shared_readers_compact(&snap.shared_readers),
        shared_reader_operation_ids = %format_shared_id_csv(&snap.shared_readers, |s| &s.operation_id),
        shared_reader_trace_ids = %format_shared_id_csv(&snap.shared_readers, |s| &s.trace_id),
        "entering notify wait for lock"
    );
}

/// Exclusive lock released — logs when hold duration exceeds threshold (not sampled).
pub fn emit_exclusive_released(key: &ObjectKey, holder: &str, held: Duration) {
    if !tracing::enabled!(target: HOLDER_TARGET, tracing::Level::DEBUG) {
        return;
    }
    let ms = held.as_millis();
    if ms < min_hold_log_ms() {
        return;
    }
    let holder_prefix: String = holder.chars().take(120).collect();
    tracing::debug!(
        target: HOLDER_TARGET,
        event = "exclusive_released",
        resource = %key,
        holder_owner_fp = owner_fingerprint(holder),
        holder_prefix = %holder_prefix,
        hold_ms = ms,
        "exclusive lock released after hold"
    );
}

/// Shared lock released (one refcount / owner row dropped); same hold threshold as exclusive.
pub fn emit_shared_released(key: &ObjectKey, holder: &str, held: Duration) {
    if !tracing::enabled!(target: HOLDER_TARGET, tracing::Level::DEBUG) {
        return;
    }
    let ms = held.as_millis();
    if ms < min_hold_log_ms() {
        return;
    }
    let holder_prefix: String = holder.chars().take(120).collect();
    tracing::debug!(
        target: HOLDER_TARGET,
        event = "shared_released",
        resource = %key,
        holder_owner_fp = owner_fingerprint(holder),
        holder_prefix = %holder_prefix,
        hold_ms = ms,
        "shared lock released after hold"
    );
}
