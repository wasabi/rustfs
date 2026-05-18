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

use super::{metadata_sys::get_bucket_metadata_sys, versioning::VersioningApi};
use crate::disk::RUSTFS_META_BUCKET;
use crate::error::Result;
use s3s::dto::VersioningConfiguration;
use tracing::warn;

pub struct BucketVersioningSys {}

impl Default for BucketVersioningSys {
    fn default() -> Self {
        Self::new()
    }
}

impl BucketVersioningSys {
    pub fn new() -> Self {
        Self {}
    }
    pub async fn enabled(bucket: &str) -> bool {
        match Self::get(bucket).await {
            Ok(res) => res.enabled(),
            Err(err) => {
                warn!("{:?}", err);
                false
            }
        }
    }

    pub async fn prefix_enabled(bucket: &str, prefix: &str) -> bool {
        match Self::get(bucket).await {
            Ok(res) => res.prefix_enabled(prefix),
            Err(err) => {
                warn!("{:?}", err);
                false
            }
        }
    }

    pub async fn suspended(bucket: &str) -> bool {
        match Self::get(bucket).await {
            Ok(res) => res.suspended(),
            Err(err) => {
                warn!("{:?}", err);
                false
            }
        }
    }

    pub async fn prefix_suspended(bucket: &str, prefix: &str) -> bool {
        match Self::get(bucket).await {
            Ok(res) => res.prefix_suspended(prefix),
            Err(err) => {
                warn!("{:?}", err);
                false
            }
        }
    }

    pub async fn get(bucket: &str) -> Result<VersioningConfiguration> {
        if bucket == RUSTFS_META_BUCKET || bucket.starts_with(RUSTFS_META_BUCKET) {
            return Ok(VersioningConfiguration::default());
        }

        let bucket_meta_sys_lock = get_bucket_metadata_sys()?;
        // Read lock is correct: no global BucketMetadataSys write access is needed.
        // get_config may hydrate the cache on a miss, but that mutation is protected
        // by the inner metadata_map RwLock — concurrent hydration under a global
        // read lock is safe. All 21 other getter functions in metadata_sys.rs use
        // the same pattern. The three legitimate write-lock sites (set_bucket_metadata,
        // update, delete) are genuine mutations of the outer state.
        let bucket_meta_sys = bucket_meta_sys_lock.read().await;

        let (cfg, _) = bucket_meta_sys.get_versioning_config(bucket).await?;

        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bucket::metadata::BucketMetadata;
    use crate::bucket::metadata_sys::BucketMetadataSys;
    use s3s::dto::BucketVersioningStatus;
    use std::sync::Arc;

    /// Verifies that `get_versioning_config` returns correct results when called
    /// concurrently from 20 tasks. This test operates at the `BucketMetadataSys`
    /// level and does not exercise the global `RwLock` acquired by
    /// `BucketVersioningSys::get` — it cannot do so without a live `ECStore`.
    /// The lock-type change (write → read) is validated by the safety comment in
    /// `get` and the full pre-commit gate. This test guards against correctness
    /// regressions in the read path (e.g. a cache-miss that overwrites the result).
    #[tokio::test]
    async fn get_versioning_config_correct_under_concurrent_reads() {
        let sys = BucketMetadataSys::new_for_test();

        let mut bm = BucketMetadata::new("test-bucket");
        bm.versioning_config = Some(VersioningConfiguration {
            status: Some(BucketVersioningStatus::from_static(BucketVersioningStatus::ENABLED)),
            ..Default::default()
        });
        sys.set("test-bucket".to_string(), Arc::new(bm)).await;

        let sys = Arc::new(sys);
        let handles: Vec<_> = (0..20)
            .map(|_| {
                let s = Arc::clone(&sys);
                tokio::spawn(async move { s.get_versioning_config("test-bucket").await })
            })
            .collect();

        for h in handles {
            let (cfg, _) = h.await.unwrap().unwrap();
            assert!(cfg.enabled(), "expected versioning enabled");
        }

        // Prevent ECStore::drop from running on the uninitialised Arc allocation
        // created by new_for_test(). Arc::try_unwrap succeeds because all spawned
        // tasks have been joined above.
        let inner = Arc::try_unwrap(sys).expect("all handles dropped");
        std::mem::forget(inner);
    }
}
