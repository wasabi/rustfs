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

use super::*;

use crate::disk::{DiskStore, RenameDataResp};
use crate::set_disk::deferred_cleanup::delete_journal_entries;
use futures::stream::{FuturesUnordered, StreamExt};
use rustfs_common::heal_channel::HealChannelPriority;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::task::JoinSet;

/// Sampled observability for Phase G PoC [`SetDisks::rename_data_with_barrier`] (lab).
static RENAME_BARRIER_POC_EARLY_OK: AtomicU64 = AtomicU64::new(0);
static RENAME_BARRIER_POC_FULL_OK: AtomicU64 = AtomicU64::new(0);
static RENAME_BARRIER_POC_ERR: AtomicU64 = AtomicU64::new(0);
static RENAME_BARRIER_POC_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Emit [`rustfs_put_trace`] **debug** with cumulative totals every N completions (grep `rename_data_barrier_poc_totals`).
const RENAME_BARRIER_POC_LOG_EVERY: u64 = 4096;

#[derive(Clone, Copy)]
enum RenameBarrierPocOutcome {
    EarlyOk,
    FullOk,
    ErrAfterDrain,
}

fn rename_data_barrier_poc_record(outcome: RenameBarrierPocOutcome) {
    match outcome {
        RenameBarrierPocOutcome::EarlyOk => {
            RENAME_BARRIER_POC_EARLY_OK.fetch_add(1, Ordering::Relaxed);
        }
        RenameBarrierPocOutcome::FullOk => {
            RENAME_BARRIER_POC_FULL_OK.fetch_add(1, Ordering::Relaxed);
        }
        RenameBarrierPocOutcome::ErrAfterDrain => {
            RENAME_BARRIER_POC_ERR.fetch_add(1, Ordering::Relaxed);
        }
    }
    let total = RENAME_BARRIER_POC_TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
    if total.is_multiple_of(RENAME_BARRIER_POC_LOG_EVERY) {
        let early = RENAME_BARRIER_POC_EARLY_OK.load(Ordering::Relaxed);
        let full = RENAME_BARRIER_POC_FULL_OK.load(Ordering::Relaxed);
        let err = RENAME_BARRIER_POC_ERR.load(Ordering::Relaxed);
        tracing::debug!(
            target: "rustfs_put_trace",
            rename_data_barrier_poc_totals = true,
            rename_data_barrier_poc_early_ok_total = early,
            rename_data_barrier_poc_full_ok_total = full,
            rename_data_barrier_poc_err_after_drain_total = err,
            rename_data_barrier_poc_all_total = total,
            "rename_data_with_barrier PoC cumulative counts (early_ok=quorum exit w/ pending legs; full_ok=all legs joined before return)"
        );
    }
}

/// Context needed for background cleanup after a quorum-early-exit PUT completes.
/// Installed on [`TmpRenameBarrier`] before returning to the client; consumed by [`Drop`].
pub(super) struct CleanupCtx {
    /// The `SetDisks` this PUT was served by — needed to call `delete_all`.
    pub store: SetDisks,
    /// Local disks on this node — used to delete the per-disk journal entry.
    pub local_disks: Vec<DiskStore>,
    /// Path under `RUSTFS_META_TMP_BUCKET` to delete after all straggler renames finish.
    pub tmp_prefix: String,
    /// Journal entry ID written before the client got a response.  Deleted after cleanup.
    pub journal_id: uuid::Uuid,
    /// `"pool_{pool_idx}_set_{set_idx}"` string used to call `send_heal_disk` on failures.
    pub set_disk_id: String,
}

/// After quorum-level success from [`SetDisks::rename_data_with_barrier`], per-disk rename
/// tasks may still be running.  For the overwrite path callers must call
/// [`TmpRenameBarrier::wait_all`] before `delete_all(tmp_dir)`.  For new-object PUTs, install
/// a [`CleanupCtx`] with [`TmpRenameBarrier::install_cleanup_ctx`]; the barrier then schedules
/// deferred cleanup automatically when it is dropped.
pub(super) struct TmpRenameBarrier {
    #[allow(clippy::type_complexity)]
    join_set: JoinSet<std::result::Result<(usize, std::result::Result<RenameDataResp, DiskError>), DiskError>>,
    /// Present only on the deferred-cleanup path; `None` for overwrite and full-drain paths.
    ctx: Option<CleanupCtx>,
}

impl TmpRenameBarrier {
    /// Install a cleanup context so that [`Drop`] will schedule background tmp cleanup.
    /// Must be called at most once per barrier.
    pub(super) fn install_cleanup_ctx(&mut self, ctx: CleanupCtx) {
        debug_assert!(self.ctx.is_none(), "cleanup ctx already installed");
        self.ctx = Some(ctx);
    }

    /// Synchronously drain all in-flight straggler renames, logging and returning the first
    /// failure.  Used on the **overwrite** path where the old version must not be deleted
    /// until every leg has committed.  Clears `ctx` so `Drop` is a no-op.
    pub(super) async fn wait_all(mut self) -> disk::error::Result<()> {
        self.ctx = None; // prevent Drop from spawning redundant cleanup
        let mut first_err: Option<DiskError> = None;
        let mut error_count: u64 = 0;
        while let Some(join_res) = self.join_set.join_next().await {
            let disk_result: std::result::Result<RenameDataResp, DiskError> = match join_res {
                Ok(Ok((_idx, r))) => r,
                Ok(Err(e)) => Err(e),
                Err(_) => Err(DiskError::Unexpected),
            };
            if let Err(e) = disk_result {
                error_count += 1;
                tracing::warn!(
                    target: "rustfs_ecstore",
                    error = ?e,
                    straggler_error_count = error_count,
                    "TmpRenameBarrier: straggler rename leg failed; disk should be healed"
                );
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for TmpRenameBarrier {
    fn drop(&mut self) {
        if self.join_set.is_empty() {
            return; // nothing pending — FullOk or already drained via wait_all
        }
        match self.ctx.take() {
            Some(ctx) => {
                // Replace join_set with a new empty one so we can move the original into
                // the spawned task without leaving a partially-drained JoinSet behind.
                let join_set = std::mem::replace(&mut self.join_set, JoinSet::new());
                tokio::spawn(run_straggler_cleanup(join_set, ctx));
            }
            None => {
                // Barrier has pending tasks but no cleanup context — caller bug.  Log loudly;
                // the GC cycle will clean up the tmp dir via the journal entry.
                tracing::error!(
                    target: "rustfs_ecstore",
                    pending = self.join_set.len(),
                    "TmpRenameBarrier dropped with pending rename tasks and no cleanup ctx;                      tmp dir will be cleaned up by the next deferred-cleanup GC cycle"
                );
            }
        }
    }
}

/// Background task spawned by `TmpRenameBarrier::drop` on the deferred-cleanup path.
///
/// Drains all straggler rename tasks; triggers `send_heal_disk` for failed disks; then
/// deletes the tmp prefix and removes the journal entry written before the PUT returned.
#[allow(clippy::type_complexity)]
async fn run_straggler_cleanup(
    mut join_set: JoinSet<std::result::Result<(usize, std::result::Result<RenameDataResp, DiskError>), DiskError>>,
    ctx: CleanupCtx,
) {
    let mut heal_triggered = false;
    while let Some(join_res) = join_set.join_next().await {
        let disk_result: std::result::Result<RenameDataResp, DiskError> = match join_res {
            Ok(Ok((_idx, r))) => r,
            Ok(Err(e)) => Err(e),
            Err(_) => Err(DiskError::Unexpected),
        };
        if let Err(e) = disk_result {
            tracing::warn!(
                target: "rustfs_ecstore",
                error = ?e,
                set_disk_id = %ctx.set_disk_id,
                "deferred_cleanup: straggler rename leg failed; triggering heal"
            );
            if !heal_triggered {
                heal_triggered = true;
                let id = ctx.set_disk_id.clone();
                tokio::spawn(async move {
                    let _ = rustfs_common::heal_channel::send_heal_disk(id, Some(HealChannelPriority::Normal)).await;
                });
            }
        }
    }

    // All straggler legs done. Delete the tmp prefix.
    if let Err(e) = ctx.store.delete_all(RUSTFS_META_TMP_BUCKET, &ctx.tmp_prefix).await {
        tracing::warn!(
            target: "rustfs_ecstore",
            error = ?e,
            tmp_prefix = %ctx.tmp_prefix,
            "deferred_cleanup: delete_all failed; GC cycle will retry via journal"
        );
        // Leave the journal entry in place so the GC loop retries.
        return;
    }

    // Cleanup succeeded — remove the journal entry so GC has nothing to replay.
    delete_journal_entries(&ctx.local_disks, ctx.journal_id).await;
}

/// Among disks that have completed rename successfully (`slot_errs[i] == Some(None)`), returns the
/// agreed `old_data_dir` key only when a unique bucket reaches `write_quorum` successes.
fn quorum_old_data_dir_among_successes(
    data_dirs: &[Option<Uuid>],
    slot_errs: &[Option<Option<DiskError>>],
    write_quorum: usize,
) -> Option<Option<Uuid>> {
    let mut counts = std::collections::HashMap::<Option<Uuid>, usize>::new();
    for i in 0..slot_errs.len() {
        if slot_errs[i] != Some(None) {
            continue;
        }
        *counts.entry(data_dirs[i]).or_insert(0) += 1;
    }
    if counts.is_empty() {
        return None;
    }
    let max_c = counts.values().copied().max().unwrap_or(0);
    if max_c < write_quorum {
        return None;
    }
    let winners: Vec<Option<Uuid>> = counts.into_iter().filter(|(_, c)| *c == max_c).map(|(k, _)| k).collect();
    if winners.len() != 1 {
        return None;
    }
    Some(winners[0])
}

/// Whether quorum rename may proceed before all disk legs complete (PoC gate — conservative).
///
/// Requires strictly more completed successes than incomplete legs (`successes > pending`) so we do
/// not exit when successes exactly equal `write_quorum` with all remaining disks still in flight
/// (ambiguous `reduce_common_data_dir` ties with trailing `None` slots — see metadata tests).
fn rename_data_early_ready(
    slot_errs: &[Option<Option<DiskError>>],
    data_dirs: &[Option<Uuid>],
    write_quorum: usize,
    n: usize,
) -> bool {
    let completed = slot_errs.iter().filter(|s| s.is_some()).count();
    let pending = n.saturating_sub(completed);
    let successes = slot_errs.iter().filter(|s| *s == &Some(None)).count();
    successes >= write_quorum
        && successes > pending
        && quorum_old_data_dir_among_successes(data_dirs, slot_errs, write_quorum).is_some()
}

/// Build a provisional err vector for [`SetDisks::eval_disks`] before all legs finish: pending slots
/// are treated as offline (`DiskNotFound`).
fn errs_for_partial_eval(slot_errs: &[Option<Option<DiskError>>]) -> disk::error::Result<Vec<Option<DiskError>>> {
    slot_errs
        .iter()
        .map(|s| match s {
            None => Ok(Some(DiskError::DiskNotFound)),
            Some(None) => Ok(None),
            Some(Some(e)) => Ok(Some(e.clone())),
        })
        .collect()
}

impl SetDisks {
    /// Records one disk leg's `rename_data` result into the aggregate buffers. Completion order
    /// does not matter as long as each index is recorded exactly once.
    fn rename_data_record_disk_outcome(
        idx: usize,
        disk_result: std::result::Result<RenameDataResp, DiskError>,
        data_dirs: &mut [Option<Uuid>],
        disk_versions: &mut [Option<Vec<u8>>],
        slot_errs: &mut [Option<Option<DiskError>>],
    ) {
        match disk_result {
            Ok(res) => {
                data_dirs[idx] = res.old_data_dir;
                disk_versions[idx].clone_from(&res.sign);
                slot_errs[idx] = Some(None);
            }
            Err(e) => {
                slot_errs[idx] = Some(Some(e));
            }
        }
    }

    pub(super) fn default_read_quorum(&self) -> usize {
        self.set_drive_count - self.default_parity_count
    }

    pub(super) fn default_write_quorum(&self) -> usize {
        let mut data_count = self.set_drive_count - self.default_parity_count;
        if data_count == self.default_parity_count {
            data_count += 1
        }

        data_count
    }

    #[tracing::instrument(level = "debug", skip(disks, file_infos))]
    #[allow(clippy::type_complexity)]
    pub(super) async fn rename_data(
        disks: &[Option<DiskStore>],
        src_bucket: &str,
        src_object: &str,
        file_infos: &[FileInfo],
        dst_bucket: &str,
        dst_object: &str,
        write_quorum: usize,
    ) -> disk::error::Result<(Vec<Option<DiskStore>>, Option<Vec<u8>>, Option<Uuid>)> {
        let n = disks.len();

        let mut errs = Vec::with_capacity(n);

        let src_bucket = Arc::new(src_bucket.to_string());
        let src_object = Arc::new(src_object.to_string());
        let dst_bucket = Arc::new(dst_bucket.to_string());
        let dst_object = Arc::new(dst_object.to_string());

        // Drive per-disk renames concurrently; completions may arrive in any order.
        //
        // We must still await every spawned leg before returning: `put_object` calls
        // `delete_all(RUSTFS_META_TMP_BUCKET, &tmp_dir)` as soon as this future resolves. If any
        // disk were still moving data out of `tmp_dir`, that delete would race the straggler.
        // Quorum-only early return would require splitting this API (e.g. defer tmp cleanup until
        // an explicit "all renames finished" barrier) or a wider protocol change.
        let mut rename_tasks = FuturesUnordered::new();

        for (i, (disk, file_info)) in disks.iter().zip(file_infos.iter()).enumerate() {
            let mut file_info = file_info.clone();
            let disk = disk.clone();
            let src_bucket = src_bucket.clone();
            let src_object = src_object.clone();
            let dst_object = dst_object.clone();
            let dst_bucket = dst_bucket.clone();

            let handle = tokio::spawn(async move {
                if file_info.erasure.index == 0 {
                    file_info.erasure.index = i + 1;
                }

                if !file_info.is_valid() {
                    return Err(DiskError::FileCorrupt);
                }

                if let Some(disk) = disk {
                    disk.rename_data(&src_bucket, &src_object, file_info, &dst_bucket, &dst_object)
                        .await
                } else {
                    Err(DiskError::DiskNotFound)
                }
            });

            rename_tasks.push(async move {
                let disk_result = handle.await.map_err(|_| DiskError::Unexpected)?;
                Ok::<_, DiskError>((i, disk_result))
            });
        }

        let mut disk_versions = vec![None; n];
        let mut data_dirs = vec![None; n];
        let mut slot_errs: Vec<Option<Option<DiskError>>> = vec![None; n];

        for _ in 0..n {
            let (idx, disk_result) = rename_tasks.next().await.ok_or(DiskError::Unexpected)??;

            Self::rename_data_record_disk_outcome(idx, disk_result, &mut data_dirs, &mut disk_versions, &mut slot_errs);
        }

        for slot in slot_errs {
            errs.push(slot.ok_or(DiskError::Unexpected)?);
        }

        let mut futures = Vec::with_capacity(disks.len());
        if let Some(ret_err) = reduce_write_quorum_errs(&errs, OBJECT_OP_IGNORED_ERRS, write_quorum) {
            // TODO: add concurrency
            for (i, err) in errs.iter().enumerate() {
                if err.is_some() {
                    continue;
                }

                if let Some(disk) = disks[i].as_ref() {
                    let fi = file_infos[i].clone();
                    let old_data_dir = data_dirs[i];
                    let disk = disk.clone();
                    let src_bucket = src_bucket.clone();
                    let src_object = src_object.clone();
                    futures.push(tokio::spawn(async move {
                        let _ = disk
                            .delete_version(
                                &src_bucket,
                                &src_object,
                                fi,
                                false,
                                DeleteOptions {
                                    undo_write: true,
                                    old_data_dir,
                                    ..Default::default()
                                },
                            )
                            .await
                            .map_err(|e| {
                                debug!("rename_data delete_version err {:?}", e);
                                e
                            });
                    }));
                }
            }

            let _ = join_all(futures).await;
            return Err(ret_err);
        }

        let versions = None;
        // TODO: reduceCommonVersions

        let data_dir = Self::reduce_common_data_dir(&data_dirs, write_quorum);

        // // TODO: reduce_common_data_dir
        // if let Some(old_dir) = rename_ress
        //     .iter()
        //     .filter_map(|v| if v.is_some() { v.as_ref().unwrap().old_data_dir } else { None })
        //     .map(|v| v.to_string())
        //     .next()
        // {
        //     let cm_errs = self.commit_rename_data_dir(&shuffle_disks, &bucket, &object, &old_dir).await;
        //     warn!("put_object commit_rename_data_dir:{:?}", &cm_errs);
        // }

        // self.delete_all(RUSTFS_META_TMP_BUCKET, &tmp_dir).await?;

        Ok((Self::eval_disks(disks, &errs), versions, data_dir))
    }

    /// Like [`SetDisks::rename_data`], but may return as soon as write quorum (and agreed
    /// `old_data_dir` among successes) is known. Remaining per-disk work stays in
    /// [`TmpRenameBarrier`]; callers must [`TmpRenameBarrier::wait_all`] before `delete_all(tmp_dir)`.
    #[tracing::instrument(level = "debug", skip(disks, file_infos))]
    #[allow(clippy::type_complexity)]
    pub(super) async fn rename_data_with_barrier(
        disks: &[Option<DiskStore>],
        src_bucket: &str,
        src_object: &str,
        file_infos: &[FileInfo],
        dst_bucket: &str,
        dst_object: &str,
        write_quorum: usize,
    ) -> disk::error::Result<(Vec<Option<DiskStore>>, Option<Vec<u8>>, Option<Uuid>, TmpRenameBarrier)> {
        let n = disks.len();

        let src_bucket = Arc::new(src_bucket.to_string());
        let src_object = Arc::new(src_object.to_string());
        let dst_bucket = Arc::new(dst_bucket.to_string());
        let dst_object = Arc::new(dst_object.to_string());

        let mut join_set = JoinSet::new();

        for (i, (disk, file_info)) in disks.iter().zip(file_infos.iter()).enumerate() {
            let mut file_info = file_info.clone();
            let disk = disk.clone();
            let src_bucket = src_bucket.clone();
            let src_object = src_object.clone();
            let dst_object = dst_object.clone();
            let dst_bucket = dst_bucket.clone();

            join_set.spawn(async move {
                let handle = tokio::spawn(async move {
                    if file_info.erasure.index == 0 {
                        file_info.erasure.index = i + 1;
                    }

                    if !file_info.is_valid() {
                        return Err(DiskError::FileCorrupt);
                    }

                    if let Some(disk) = disk {
                        disk.rename_data(&src_bucket, &src_object, file_info, &dst_bucket, &dst_object)
                            .await
                    } else {
                        Err(DiskError::DiskNotFound)
                    }
                });

                let disk_result = handle.await.map_err(|_| DiskError::Unexpected)?;
                Ok::<_, DiskError>((i, disk_result))
            });
        }

        let mut disk_versions = vec![None; n];
        let mut data_dirs = vec![None; n];
        let mut slot_errs: Vec<Option<Option<DiskError>>> = vec![None; n];

        while let Some(join_res) = join_set.join_next().await {
            let (idx, disk_result) = join_res.map_err(|_| DiskError::Unexpected)??;

            Self::rename_data_record_disk_outcome(idx, disk_result, &mut data_dirs, &mut disk_versions, &mut slot_errs);

            if rename_data_early_ready(&slot_errs, &data_dirs, write_quorum, n) {
                let errs = errs_for_partial_eval(&slot_errs)?;
                let versions = None;
                let data_dir = match quorum_old_data_dir_among_successes(&data_dirs, &slot_errs, write_quorum) {
                    Some(d) => d,
                    None => Self::reduce_common_data_dir(&data_dirs, write_quorum),
                };

                rename_data_barrier_poc_record(RenameBarrierPocOutcome::EarlyOk);
                return Ok((
                    Self::eval_disks(disks, &errs),
                    versions,
                    data_dir,
                    TmpRenameBarrier { join_set, ctx: None },
                ));
            }
        }

        let mut errs = Vec::with_capacity(n);
        for slot in slot_errs {
            errs.push(slot.ok_or(DiskError::Unexpected)?);
        }

        let mut futures = Vec::with_capacity(disks.len());
        if let Some(ret_err) = reduce_write_quorum_errs(&errs, OBJECT_OP_IGNORED_ERRS, write_quorum) {
            for (i, err) in errs.iter().enumerate() {
                if err.is_some() {
                    continue;
                }

                if let Some(disk) = disks[i].as_ref() {
                    let fi = file_infos[i].clone();
                    let old_data_dir = data_dirs[i];
                    let disk = disk.clone();
                    let src_bucket = src_bucket.clone();
                    let src_object = src_object.clone();
                    futures.push(tokio::spawn(async move {
                        let _ = disk
                            .delete_version(
                                &src_bucket,
                                &src_object,
                                fi,
                                false,
                                DeleteOptions {
                                    undo_write: true,
                                    old_data_dir,
                                    ..Default::default()
                                },
                            )
                            .await
                            .map_err(|e| {
                                debug!("rename_data delete_version err {:?}", e);
                                e
                            });
                    }));
                }
            }

            let _ = join_all(futures).await;
            rename_data_barrier_poc_record(RenameBarrierPocOutcome::ErrAfterDrain);
            return Err(ret_err);
        }

        let versions = None;
        let data_dir = Self::reduce_common_data_dir(&data_dirs, write_quorum);

        rename_data_barrier_poc_record(RenameBarrierPocOutcome::FullOk);
        Ok((
            Self::eval_disks(disks, &errs),
            versions,
            data_dir,
            TmpRenameBarrier {
                join_set: JoinSet::new(),
                ctx: None,
            },
        ))
    }

    #[allow(dead_code)]
    #[tracing::instrument(level = "debug", skip(self, disks))]
    pub(super) async fn commit_rename_data_dir(
        &self,
        disks: &[Option<DiskStore>],
        bucket: &str,
        object: &str,
        data_dir: &str,
        write_quorum: usize,
    ) -> disk::error::Result<()> {
        let file_path = Arc::new(format!("{object}/{data_dir}"));
        let bucket = Arc::new(bucket.to_string());
        let futures = disks.iter().map(|disk| {
            let file_path = file_path.clone();
            let bucket = bucket.clone();
            let disk = disk.clone();
            tokio::spawn(async move {
                if let Some(disk) = disk {
                    (disk
                        .delete(
                            &bucket,
                            &file_path,
                            DeleteOptions {
                                recursive: true,
                                ..Default::default()
                            },
                        )
                        .await)
                        .err()
                } else {
                    Some(DiskError::DiskNotFound)
                }
            })
        });
        let errs: Vec<Option<DiskError>> = join_all(futures)
            .await
            .into_iter()
            .map(|e| e.unwrap_or(Some(DiskError::Unexpected)))
            .collect();

        if let Some(err) = reduce_write_quorum_errs(&errs, OBJECT_OP_IGNORED_ERRS, write_quorum) {
            return Err(err);
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub(super) async fn cleanup_multipart_path(&self, paths: &[String]) {
        let disks = self.get_disks_internal().await;

        let mut errs = Vec::with_capacity(disks.len());

        // Use improved simple batch processor instead of join_all for better performance
        let processor = get_global_processors().write_processor();

        let tasks: Vec<_> = disks
            .iter()
            .map(|disk| {
                let disk = disk.clone();
                let paths = paths.to_vec();

                async move {
                    if let Some(disk) = disk {
                        disk.delete_paths(RUSTFS_META_MULTIPART_BUCKET, &paths).await
                    } else {
                        Err(DiskError::DiskNotFound)
                    }
                }
            })
            .collect();

        let results = processor.execute_batch(tasks).await;
        for result in results {
            match result {
                Ok(_) => {
                    errs.push(None);
                }
                Err(e) => {
                    errs.push(Some(e));
                }
            }
        }

        if errs.iter().any(|e| e.is_some()) {
            warn!("cleanup_multipart_path errs {:?}", &errs);
        }
    }

    #[tracing::instrument(skip(disks, meta))]
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn rename_part(
        &self,
        disks: &[Option<DiskStore>],
        src_bucket: &str,
        src_object: &str,
        dst_bucket: &str,
        dst_object: &str,
        meta: Bytes,
        write_quorum: usize,
    ) -> disk::error::Result<Vec<Option<DiskStore>>> {
        let src_bucket = Arc::new(src_bucket.to_string());
        let src_object = Arc::new(src_object.to_string());
        let dst_bucket = Arc::new(dst_bucket.to_string());
        let dst_object = Arc::new(dst_object.to_string());

        let mut errs = Vec::with_capacity(disks.len());

        let futures = disks.iter().map(|disk| {
            let disk = disk.clone();
            let meta = meta.clone();
            let src_bucket = src_bucket.clone();
            let src_object = src_object.clone();
            let dst_bucket = dst_bucket.clone();
            let dst_object = dst_object.clone();
            tokio::spawn(async move {
                if let Some(disk) = disk {
                    disk.rename_part(&src_bucket, &src_object, &dst_bucket, &dst_object, meta)
                        .await
                } else {
                    Err(DiskError::DiskNotFound)
                }
            })
        });

        let results = join_all(futures).await;
        for result in results {
            match result? {
                Ok(_) => {
                    errs.push(None);
                }
                Err(e) => {
                    errs.push(Some(e));
                }
            }
        }

        if let Some(err) = reduce_write_quorum_errs(&errs, OBJECT_OP_IGNORED_ERRS, write_quorum) {
            warn!("rename_part errs {:?}", &errs);
            self.cleanup_multipart_path(&[dst_object.to_string(), format!("{dst_object}.meta")])
                .await;
            return Err(err);
        }

        let disks = Self::eval_disks(disks, &errs);
        Ok(disks)
    }

    pub(super) fn eval_disks(disks: &[Option<DiskStore>], errs: &[Option<DiskError>]) -> Vec<Option<DiskStore>> {
        if disks.len() != errs.len() {
            return Vec::new();
        }

        let mut online_disks = vec![None; disks.len()];

        for (i, err_op) in errs.iter().enumerate() {
            if err_op.is_none() {
                online_disks[i].clone_from(&disks[i]);
            }
        }

        online_disks
    }

    #[tracing::instrument(skip(disks, files))]
    pub(super) async fn write_unique_file_info(
        disks: &[Option<DiskStore>],
        org_bucket: &str,
        bucket: &str,
        prefix: &str,
        files: &[FileInfo],
        write_quorum: usize,
    ) -> disk::error::Result<()> {
        let mut futures = Vec::with_capacity(disks.len());
        let mut errs = Vec::with_capacity(disks.len());

        for (i, disk) in disks.iter().enumerate() {
            let mut file_info = files[i].clone();
            file_info.erasure.index = i + 1;
            futures.push(async move {
                if let Some(disk) = disk {
                    disk.write_metadata(org_bucket, bucket, prefix, file_info).await
                } else {
                    Err(DiskError::DiskNotFound)
                }
            });
        }

        let results = join_all(futures).await;
        for result in results {
            match result {
                Ok(_) => {
                    errs.push(None);
                }
                Err(e) => {
                    errs.push(Some(e));
                }
            }
        }

        if let Some(err) = reduce_write_quorum_errs(&errs, OBJECT_OP_IGNORED_ERRS, write_quorum) {
            // TODO: add concurrency
            for (i, err) in errs.iter().enumerate() {
                if err.is_some() {
                    continue;
                }

                if let Some(disk) = disks[i].as_ref() {
                    let _ = disk
                        .delete(
                            bucket,
                            &path_join_buf(&[prefix, STORAGE_FORMAT_FILE]),
                            DeleteOptions {
                                recursive: true,
                                ..Default::default()
                            },
                        )
                        .await
                        .map_err(|e| {
                            warn!("write meta revert err {:?}", e);
                            e
                        });
                }
            }

            return Err(err);
        }
        Ok(())
    }

    pub(super) async fn update_object_meta(
        &self,
        bucket: &str,
        object: &str,
        fi: FileInfo,
        disks: &[Option<DiskStore>],
    ) -> disk::error::Result<()> {
        self.update_object_meta_with_opts(bucket, object, fi, disks, &UpdateMetadataOpts::default())
            .await
    }

    pub(super) async fn update_object_meta_with_opts(
        &self,
        bucket: &str,
        object: &str,
        fi: FileInfo,
        disks: &[Option<DiskStore>],
        opts: &UpdateMetadataOpts,
    ) -> disk::error::Result<()> {
        if fi.metadata.is_empty() && !opts.replace_user_metadata {
            return Ok(());
        }

        let mut futures = Vec::with_capacity(disks.len());

        let mut errs = Vec::with_capacity(disks.len());

        for disk in disks.iter() {
            let fi = fi.clone();
            futures.push(async move {
                if let Some(disk) = disk {
                    disk.update_metadata(bucket, object, fi, opts).await
                } else {
                    Err(DiskError::DiskNotFound)
                }
            })
        }

        let results = join_all(futures).await;
        for result in results {
            match result {
                Ok(_) => {
                    errs.push(None);
                }
                Err(e) => {
                    errs.push(Some(e));
                }
            }
        }

        if let Some(err) = reduce_write_quorum_errs(&errs, OBJECT_OP_IGNORED_ERRS, fi.write_quorum(self.default_write_quorum())) {
            return Err(err);
        }

        Ok(())
    }

    pub(super) async fn delete_if_dangling(
        &self,
        bucket: &str,
        object: &str,
        meta_arr: &[FileInfo],
        errs: &[Option<DiskError>],
        data_errs_by_part: &HashMap<usize, Vec<usize>>,
        opts: ObjectOptions,
    ) -> disk::error::Result<FileInfo> {
        let (m, can_heal) = is_object_dangling(meta_arr, errs, data_errs_by_part);

        if !can_heal {
            return Err(DiskError::ErasureReadQuorum);
        }

        let mut tags: HashMap<String, String> = HashMap::new();
        tags.insert("set".to_string(), self.set_index.to_string());
        tags.insert("pool".to_string(), self.pool_index.to_string());
        tags.insert("merrs".to_string(), join_errs(errs));
        tags.insert("derrs".to_string(), format!("{data_errs_by_part:?}"));
        if m.is_valid() {
            tags.insert("sz".to_string(), m.size.to_string());
            tags.insert(
                "mt".to_string(),
                m.mod_time
                    .as_ref()
                    .map_or(String::new(), |mod_time| mod_time.unix_timestamp().to_string()),
            );
            tags.insert("d:p".to_string(), format!("{}:{}", m.erasure.data_blocks, m.erasure.parity_blocks));
        } else {
            tags.insert("invalid".to_string(), "1".to_string());
            tags.insert(
                "d:p".to_string(),
                format!("{}:{}", self.set_drive_count - self.default_parity_count, self.default_parity_count),
            );
        }
        let mut offline = 0;
        for (i, err) in errs.iter().enumerate() {
            let mut found = false;
            if let Some(err) = err
                && err == &DiskError::DiskNotFound
            {
                found = true;
            }
            for p in data_errs_by_part {
                if let Some(v) = p.1.get(i)
                    && *v == CHECK_PART_DISK_NOT_FOUND
                {
                    found = true;
                    break;
                }
            }

            if found {
                offline += 1;
            }
        }

        if offline > 0 {
            tags.insert("offline".to_string(), offline.to_string());
        }

        let mut fi = FileInfo::default();
        if let Some(ref version_id) = opts.version_id {
            fi.version_id = rustfs_filemeta::S3VersionId::parse_api_version_id(version_id).ok().flatten();
        }

        fi.set_tier_free_version_id(&Uuid::new_v4().to_string());

        let disks = self.get_disks_internal().await;

        let mut futures = Vec::with_capacity(disks.len());
        for disk_op in disks.iter() {
            let bucket = bucket.to_string();
            let object = object.to_string();
            let fi = fi.clone();
            futures.push(async move {
                if let Some(disk) = disk_op {
                    disk.delete_version(&bucket, &object, fi, false, DeleteOptions::default())
                        .await
                } else {
                    Err(DiskError::DiskNotFound)
                }
            });
        }

        let results = join_all(futures).await;
        for (index, result) in results.into_iter().enumerate() {
            let key = format!("ddisk-{index}");
            match result {
                Ok(_) => {
                    tags.insert(key, "<nil>".to_string());
                }
                Err(e) => {
                    tags.insert(key, e.to_string());
                }
            }
        }

        // TODO: audit

        Ok(m)
    }

    pub(super) async fn delete_prefix(&self, bucket: &str, prefix: &str) -> disk::error::Result<()> {
        let disks = self.get_disks_internal().await;
        let write_quorum = disks.len() / 2 + 1;

        let mut futures = Vec::with_capacity(disks.len());

        for disk_op in disks.iter() {
            let bucket = bucket.to_string();
            let prefix = prefix.to_string();
            futures.push(async move {
                if let Some(disk) = disk_op {
                    disk.delete(
                        &bucket,
                        &prefix,
                        DeleteOptions {
                            recursive: true,
                            immediate: true,
                            ..Default::default()
                        },
                    )
                    .await
                } else {
                    Ok(())
                }
            });
        }

        let errs = join_all(futures).await.into_iter().map(|v| v.err()).collect::<Vec<_>>();

        if let Some(err) = reduce_write_quorum_errs(&errs, OBJECT_OP_IGNORED_ERRS, write_quorum) {
            return Err(err);
        }

        Ok(())
    }

    pub(super) async fn check_write_precondition(
        &self,
        bucket: &str,
        object: &str,
        opts: &ObjectOptions,
    ) -> Option<StorageError> {
        let mut opts = opts.clone();

        let http_preconditions = opts.http_preconditions?;
        opts.http_preconditions = None;

        // Never claim a lock here, to avoid deadlock
        // - If no_lock is false, we must have obtained the lock out side of this function
        // - If no_lock is true, we should not obtain locks
        opts.no_lock = true;
        let oi = self.get_object_info(bucket, object, &opts).await;

        match oi {
            Ok(oi) => {
                // If top level is a delete marker proceed to upload.
                if oi.delete_marker {
                    return None;
                }
                let if_none_match = http_preconditions.if_none_match_value().map(str::to_owned);
                let if_match = http_preconditions.if_match_value().map(str::to_owned);
                if should_prevent_write(&oi, if_none_match, if_match) {
                    return Some(StorageError::PreconditionFailed);
                }
            }

            Err(StorageError::VersionNotFound(_, _, _))
            | Err(StorageError::ObjectNotFound(_, _))
            | Err(StorageError::ErasureReadQuorum) => {
                // When the object is not found,
                // - if If-Match is set, we should return 404 NotFound
                // - if If-None-Match is set, we should be able to proceed with the request
                if http_preconditions.if_match_value().is_some() {
                    return Some(StorageError::ObjectNotFound(bucket.to_string(), object.to_string()));
                }
            }

            Err(e) => {
                return Some(e);
            }
        }

        None
    }
}

#[cfg(test)]
mod rename_data_completion_tests {
    use super::*;
    use crate::disk::RenameDataResp;
    use crate::disk::error_reduce::{OBJECT_OP_IGNORED_ERRS, reduce_write_quorum_errs};
    use rand::seq::SliceRandom;

    type DiskRenameResult = std::result::Result<RenameDataResp, DiskError>;

    #[allow(clippy::type_complexity)]
    fn merge_events_in_application_order(
        events: &[DiskRenameResult],
        application_perm: &[usize],
    ) -> (Vec<Option<DiskError>>, Vec<Option<Uuid>>, Vec<Option<Vec<u8>>>) {
        let n = events.len();
        let mut data_dirs = vec![None; n];
        let mut disk_versions = vec![None; n];
        let mut slot_errs: Vec<Option<Option<DiskError>>> = vec![None; n];

        for &disk_idx in application_perm {
            SetDisks::rename_data_record_disk_outcome(
                disk_idx,
                events[disk_idx].clone(),
                &mut data_dirs,
                &mut disk_versions,
                &mut slot_errs,
            );
        }

        let errs = slot_errs
            .into_iter()
            .map(|s| s.expect("each disk slot must be recorded exactly once"))
            .collect::<Vec<_>>();

        (errs, data_dirs, disk_versions)
    }

    fn next_permutation(p: &mut [usize]) -> bool {
        let n = p.len();
        if n < 2 {
            return false;
        }
        let mut i = n - 1;
        while i > 0 && p[i - 1] >= p[i] {
            i -= 1;
        }
        if i == 0 {
            return false;
        }
        let mut j = n - 1;
        while p[j] <= p[i - 1] {
            j -= 1;
        }
        p.swap(i - 1, j);
        p[i..].reverse();
        true
    }

    fn for_each_permutation(n: usize, mut f: impl FnMut(Vec<usize>)) {
        let mut p: Vec<usize> = (0..n).collect();
        loop {
            f(p.clone());
            if !next_permutation(&mut p) {
                break;
            }
        }
    }

    fn quorum_summary(
        errs: &[Option<DiskError>],
        data_dirs: &[Option<Uuid>],
        write_quorum: usize,
    ) -> (Option<DiskError>, Option<Uuid>) {
        let qerr = reduce_write_quorum_errs(errs, OBJECT_OP_IGNORED_ERRS, write_quorum);
        let data_dir = SetDisks::reduce_common_data_dir(&data_dirs.to_vec(), write_quorum);
        (qerr, data_dir)
    }

    #[test]
    fn rename_data_merge_order_independent_small_n() {
        let u = Uuid::nil();
        let cases: Vec<Vec<DiskRenameResult>> = vec![
            vec![
                Ok(RenameDataResp {
                    old_data_dir: Some(u),
                    sign: None,
                }),
                Err(DiskError::DiskNotFound),
                Ok(RenameDataResp {
                    old_data_dir: Some(u),
                    sign: Some(vec![1]),
                }),
                Err(DiskError::FileCorrupt),
            ],
            vec![
                Ok(RenameDataResp::default()),
                Ok(RenameDataResp::default()),
                Err(DiskError::FaultyDisk),
                Err(DiskError::FaultyDisk),
                Ok(RenameDataResp {
                    old_data_dir: Some(u),
                    sign: None,
                }),
            ],
        ];

        for events in cases {
            let n = events.len();
            let expected = merge_events_in_application_order(&events, &(0..n).collect::<Vec<_>>());

            for_each_permutation(n, |perm| {
                let got = merge_events_in_application_order(&events, &perm);
                assert_eq!(got.0, expected.0, "errs mismatch for perm={perm:?}");
                assert_eq!(got.1, expected.1, "data_dirs mismatch for perm={perm:?}");
                assert_eq!(got.2, expected.2, "disk_versions mismatch for perm={perm:?}");
            });
        }
    }

    #[test]
    fn rename_data_quorum_summary_order_independent() {
        let u = Uuid::nil();
        // Five successes agree on `old_data_dir`, three hard errors leave `None` slots — no tie at
        // `write_quorum` vs `reduce_common_data_dir`'s HashMap iteration (see `metadata.rs`).
        let events: Vec<DiskRenameResult> = vec![
            Ok(RenameDataResp {
                old_data_dir: Some(u),
                sign: None,
            }),
            Ok(RenameDataResp {
                old_data_dir: Some(u),
                sign: None,
            }),
            Err(DiskError::DiskNotFound),
            Ok(RenameDataResp {
                old_data_dir: Some(u),
                sign: None,
            }),
            Err(DiskError::FileCorrupt),
            Ok(RenameDataResp {
                old_data_dir: Some(u),
                sign: None,
            }),
            Ok(RenameDataResp {
                old_data_dir: Some(u),
                sign: None,
            }),
            Err(DiskError::DiskNotFound),
            Err(DiskError::DiskNotFound),
        ];
        let write_quorum = 4usize;
        let n = events.len();

        let baseline = merge_events_in_application_order(&events, &(0..n).collect::<Vec<_>>());
        let expected_summary = quorum_summary(&baseline.0, &baseline.1, write_quorum);

        let mut rng = rand::rng();
        for _ in 0..300 {
            let mut perm: Vec<usize> = (0..n).collect();
            perm.shuffle(&mut rng);
            let got = merge_events_in_application_order(&events, &perm);
            assert_eq!(quorum_summary(&got.0, &got.1, write_quorum), expected_summary, "perm={perm:?}");
        }
    }
}
