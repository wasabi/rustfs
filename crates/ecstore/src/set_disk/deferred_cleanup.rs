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

//! Crash-safe journal for deferred tmp cleanup.
//!
//! When a single-part PUT exits early (quorum reached before all N disk rename legs finish),
//! the tmp prefix cannot be deleted immediately because straggler legs may still be writing.
//! Rather than blocking the client, we:
//!
//!  1. Write a [`DeferredCleanupEntry`] to every local disk at
//!     `<RUSTFS_META_TMP_BUCKET>/.deferred-cleanup/<uuid>` **before** returning to the client.
//!  2. Spawn a background task (`run_straggler_cleanup` in `write.rs`) that waits for all
//!     straggler renames to finish, then deletes the tmp prefix and removes the journal entry.
//!  3. On startup (and every [`GC_INTERVAL`]), [`replay_and_gc`] scans for orphaned entries
//!     left by a crash and replays the delete so no tmp dirs linger indefinitely.
//!
//! # Crash safety
//!
//! The journal is written to all available local disks before the early return.  If at least
//! one local disk survives a crash, the GC replay will find and delete the orphaned tmp dir.
//! Journal writes are best-effort: if **all** local disk writes fail the call site falls back
//! to synchronous cleanup (wait for all legs, then delete inline).

use super::*;
use crate::disk::{DiskAPI, DiskStore, RUSTFS_META_TMP_BUCKET};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

/// Sub-directory inside `RUSTFS_META_TMP_BUCKET` that holds deferred-cleanup journal entries.
pub(super) const DEFERRED_CLEANUP_SUBDIR: &str = ".deferred-cleanup";

/// One journal record: persisted before the client-visible PUT response is sent.
///
/// Serialized as JSON (small, human-inspectable, no extra codec deps).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct DeferredCleanupEntry {
    /// Unique ID — used as the filename under `DEFERRED_CLEANUP_SUBDIR`.
    pub id: Uuid,
    /// Path under `RUSTFS_META_TMP_BUCKET` that needs to be deleted (e.g. `<uuid>/<object>`).
    pub tmp_prefix: String,
}

/// How often the GC loop rescans for orphaned journal entries.
const GC_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Write journal entries to every local disk.  Returns `Ok(())` if **at least one** disk write
/// succeeded so the caller has crash-safe coverage.  Returns `Err` only when all local disks fail.
pub(super) async fn write_journal_entries(local_disks: &[DiskStore], entry: &DeferredCleanupEntry) -> disk::error::Result<()> {
    if local_disks.is_empty() {
        return Err(DiskError::DiskNotFound);
    }

    let path = format!("{}/{}", DEFERRED_CLEANUP_SUBDIR, entry.id);
    let data = match serde_json::to_vec(entry) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            tracing::error!(target: "rustfs_ecstore", error = %e, "deferred_cleanup: serialize journal entry failed");
            return Err(DiskError::Unexpected);
        }
    };

    let mut any_ok = false;
    for disk in local_disks {
        match disk.write_all(RUSTFS_META_TMP_BUCKET, &path, data.clone()).await {
            Ok(()) => {
                any_ok = true;
            }
            Err(e) => {
                tracing::warn!(
                    target: "rustfs_ecstore",
                    error = ?e,
                    journal_id = %entry.id,
                    disk = ?disk.endpoint(),
                    "deferred_cleanup: journal write failed on local disk"
                );
            }
        }
    }

    if any_ok { Ok(()) } else { Err(DiskError::DiskNotFound) }
}

/// Remove the journal entry from all local disks after cleanup is complete.  Errors are
/// logged and ignored; a leftover entry causes only redundant GC work, not data loss.
pub(super) async fn delete_journal_entries(local_disks: &[DiskStore], id: Uuid) {
    let path = format!("{}/{}", DEFERRED_CLEANUP_SUBDIR, id);
    for disk in local_disks {
        if let Err(e) = disk
            .delete(
                RUSTFS_META_TMP_BUCKET,
                &path,
                crate::disk::DeleteOptions {
                    recursive: false,
                    ..Default::default()
                },
            )
            .await
        {
            tracing::warn!(
                target: "rustfs_ecstore",
                error = ?e,
                journal_id = %id,
                "deferred_cleanup: journal delete failed on local disk (best-effort; GC will retry)"
            );
        }
    }
}

/// Read all valid journal entries from a single disk.  Invalid or unreadable entries are
/// skipped with a warning (they may have been written by a concurrent deleter).
async fn read_journal_entries_from_disk(disk: &DiskStore) -> Vec<DeferredCleanupEntry> {
    let names = match disk.list_dir("", RUSTFS_META_TMP_BUCKET, DEFERRED_CLEANUP_SUBDIR, -1).await {
        Ok(names) => names,
        Err(e) if e == DiskError::FileNotFound || e == DiskError::VolumeNotFound => {
            return Vec::new();
        }
        Err(e) => {
            tracing::warn!(
                target: "rustfs_ecstore",
                error = ?e,
                "deferred_cleanup: list_dir failed; skipping GC scan for this disk"
            );
            return Vec::new();
        }
    };

    let mut entries = Vec::new();
    for name in names {
        let path = format!("{}/{}", DEFERRED_CLEANUP_SUBDIR, name.trim_end_matches('/'));
        match disk.read_all(RUSTFS_META_TMP_BUCKET, &path).await {
            Ok(bytes) => match serde_json::from_slice::<DeferredCleanupEntry>(&bytes) {
                Ok(entry) => entries.push(entry),
                Err(e) => {
                    tracing::warn!(
                        target: "rustfs_ecstore",
                        error = %e,
                        path = %path,
                        "deferred_cleanup: corrupt journal entry; skipping"
                    );
                }
            },
            Err(e) => {
                tracing::warn!(
                    target: "rustfs_ecstore",
                    error = ?e,
                    path = %path,
                    "deferred_cleanup: read journal entry failed; skipping"
                );
            }
        }
    }
    entries
}

/// Scan all local disks for orphaned journal entries and replay the `delete_all` for each.
/// Deduplicates by journal ID so repeated GC passes are idempotent.
pub(super) async fn replay_and_gc(set: &SetDisks) {
    let local_disks: Vec<DiskStore> = {
        let disks = set.disks.read().await;
        disks
            .iter()
            .filter_map(|d| d.as_ref().filter(|d| d.is_local()).cloned())
            .collect()
    };

    if local_disks.is_empty() {
        return;
    }

    // Collect all entries, deduplicate by ID (multiple local disks may hold the same entry).
    let mut seen = std::collections::HashSet::new();
    let mut to_clean: Vec<DeferredCleanupEntry> = Vec::new();

    for disk in &local_disks {
        for entry in read_journal_entries_from_disk(disk).await {
            if seen.insert(entry.id) {
                to_clean.push(entry);
            }
        }
    }

    for entry in to_clean {
        tracing::info!(
            target: "rustfs_ecstore",
            journal_id = %entry.id,
            tmp_prefix = %entry.tmp_prefix,
            "deferred_cleanup: GC replaying orphaned tmp delete"
        );
        if let Err(e) = set.delete_all(RUSTFS_META_TMP_BUCKET, &entry.tmp_prefix).await {
            tracing::warn!(
                target: "rustfs_ecstore",
                error = ?e,
                tmp_prefix = %entry.tmp_prefix,
                "deferred_cleanup: GC delete_all failed; will retry next cycle"
            );
            // Leave journal entry in place so the next GC cycle retries.
        } else {
            delete_journal_entries(&local_disks, entry.id).await;
        }
    }
}

/// Background task: replay orphaned entries at startup, then repeat every [`GC_INTERVAL`].
/// Spawn once per `SetDisks` instance (in `SetDisks::new`).
pub(super) async fn run_deferred_cleanup_gc(set: SetDisks) {
    // Run immediately on startup to clear any entries left by a prior crash.
    replay_and_gc(&set).await;

    let mut interval = tokio::time::interval(GC_INTERVAL);
    interval.tick().await; // consume the immediate first tick
    loop {
        interval.tick().await;
        replay_and_gc(&set).await;
    }
}
