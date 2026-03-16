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

use aws_sdk_s3::primitives::ByteStream;
use rustfs_common::data_usage::DataUsageInfo;
use serial_test::serial;
use tokio::time::{Duration, sleep};

use crate::common::{RustFSTestEnvironment, TEST_BUCKET, awscurl_get, init_logging};

/// Number of objects to create; enough to assert "full count" (no truncation) without
/// making the test so long that it risks hitting timeouts or process-group signals.
const DATA_USAGE_TEST_OBJECT_COUNT: u32 = 200;

/// Regression test for data usage accuracy (issue #1012).
/// Launches rustfs, writes N objects, then asserts admin data usage reports the full count.
/// The admin API reads from backend storage updated by the data scanner; we run the server
/// with RUSTFS_SCANNER_SPEED=fastest so the first scan cycle completes sooner.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn data_usage_reports_all_objects() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    init_logging();

    let mut env = RustFSTestEnvironment::new().await?;
    env.start_rustfs_server_with_env(vec![], &[("RUSTFS_SCANNER_SPEED", "fastest")])
        .await?;

    let client = env.create_s3_client();

    // Create bucket and upload objects
    client.create_bucket().bucket(TEST_BUCKET).send().await?;

    for i in 0..DATA_USAGE_TEST_OBJECT_COUNT {
        let key = format!("obj-{i:04}");
        client
            .put_object()
            .bucket(TEST_BUCKET)
            .key(key)
            .body(ByteStream::from_static(b"hello-world"))
            .send()
            .await?;
    }

    // Query admin data usage API; counts are updated by the data scanner (writes to backend).
    // Poll until we see the expected count or timeout.
    let url = format!("{}/rustfs/admin/v3/datausageinfo", env.url);
    const POLL_INTERVAL: Duration = Duration::from_secs(1);
    const POLL_DEADLINE: Duration = Duration::from_secs(90);
    let deadline = std::time::Instant::now() + POLL_DEADLINE;
    let usage: DataUsageInfo = loop {
        let resp = awscurl_get(&url, &env.access_key, &env.secret_key).await?;
        let u: DataUsageInfo = serde_json::from_str(&resp)?;
        let bucket_count = u.buckets_usage.get(TEST_BUCKET).map(|b| b.objects_count).unwrap_or(0);
        if u.objects_total_count >= DATA_USAGE_TEST_OBJECT_COUNT as u64 && bucket_count >= DATA_USAGE_TEST_OBJECT_COUNT as u64 {
            break u;
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "data usage count did not reach {} within {:?}: total={}, bucket={}",
                DATA_USAGE_TEST_OBJECT_COUNT, POLL_DEADLINE, u.objects_total_count, bucket_count
            )
            .into());
        }
        sleep(POLL_INTERVAL).await;
    };

    // Assert total object count and per-bucket count are not truncated
    let bucket_usage = usage
        .buckets_usage
        .get(TEST_BUCKET)
        .cloned()
        .expect("bucket usage should exist");

    assert!(
        usage.objects_total_count >= DATA_USAGE_TEST_OBJECT_COUNT as u64,
        "total object count should be at least {}, got {}",
        DATA_USAGE_TEST_OBJECT_COUNT,
        usage.objects_total_count
    );
    assert!(
        bucket_usage.objects_count >= DATA_USAGE_TEST_OBJECT_COUNT as u64,
        "bucket object count should be at least {}, got {}",
        DATA_USAGE_TEST_OBJECT_COUNT,
        bucket_usage.objects_count
    );

    env.stop_server();
    Ok(())
}
