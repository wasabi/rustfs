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

//! Test for HeadObject on deleted objects with versioning enabled
//!
//! This test reproduces the issue where getting a deleted object returns
//! 200 OK instead of 404 NoSuchKey when versioning is enabled.

#![cfg(test)]

use crate::common::{RustFSTestEnvironment, init_logging};
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::types::{BucketVersioningStatus, VersioningConfiguration};
use bytes::Bytes;
use serial_test::serial;
use tracing::info;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const ACCESS_KEY: &str = "rustfsadmin";
const SECRET_KEY: &str = "rustfsadmin";
const BUCKET: &str = "test-head-deleted-versioning-bucket";

async fn create_aws_s3_client(endpoint_url: &str) -> Result<Client, BoxError> {
    let region_provider = RegionProviderChain::default_provider().or_else(Region::new("us-east-1"));
    let shared_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(region_provider)
        .credentials_provider(Credentials::new(ACCESS_KEY, SECRET_KEY, None, None, "static"))
        .endpoint_url(endpoint_url)
        .load()
        .await;

    let client = Client::from_conf(
        aws_sdk_s3::Config::from(&shared_config)
            .to_builder()
            .force_path_style(true)
            .build(),
    );
    Ok(client)
}

/// Setup test bucket, creating it if it doesn't exist, and enable versioning
async fn setup_test_bucket(client: &Client) -> Result<(), BoxError> {
    match client.create_bucket().bucket(BUCKET).send().await {
        Ok(_) => {}
        Err(SdkError::ServiceError(e)) => {
            let e = e.into_err();
            let error_code = e.meta().code().unwrap_or("");
            if !error_code.eq("BucketAlreadyExists") && !error_code.eq("BucketAlreadyOwnedByYou") {
                return Err(e.into());
            }
        }
        Err(e) => {
            return Err(e.into());
        }
    }

    // Enable versioning
    client
        .put_bucket_versioning()
        .bucket(BUCKET)
        .versioning_configuration(
            VersioningConfiguration::builder()
                .status(BucketVersioningStatus::Enabled)
                .build(),
        )
        .send()
        .await?;

    Ok(())
}

/// Test that HeadObject on a deleted object returns NoSuchKey when versioning is enabled
#[tokio::test]
#[serial]
async fn test_head_deleted_object_versioning_returns_nosuchkey() -> Result<(), BoxError> {
    init_logging();
    info!("🧪 Starting test_head_deleted_object_versioning_returns_nosuchkey");

    let mut env = RustFSTestEnvironment::new().await?;
    env.start_rustfs_server(vec![]).await?;
    let client = create_aws_s3_client(&env.url).await?;
    setup_test_bucket(&client).await?;

    let key = "test-head-deleted-versioning.txt";
    let content = b"Test content for HeadObject with versioning";

    // Upload and verify
    client
        .put_object()
        .bucket(BUCKET)
        .key(key)
        .body(Bytes::from_static(content).into())
        .send()
        .await?;

    // Delete the object (creates a delete marker)
    client.delete_object().bucket(BUCKET).key(key).send().await?;

    // Try to head the deleted object (latest version is delete marker)
    let head_result = client.head_object().bucket(BUCKET).key(key).send().await;

    assert!(head_result.is_err(), "HeadObject on deleted object should return an error");

    match head_result.unwrap_err() {
        SdkError::ServiceError(service_err) => {
            let s3_err = service_err.into_err();
            assert!(
                s3_err.meta().code() == Some("NoSuchKey")
                    || s3_err.meta().code() == Some("NotFound")
                    || s3_err.meta().code() == Some("404"),
                "Error should be NoSuchKey or NotFound, got: {s3_err:?}"
            );
            info!("✅ HeadObject correctly returns NoSuchKey/NotFound");
        }
        other_err => {
            panic!("Expected ServiceError but got: {other_err:?}");
        }
    }

    Ok(())
}
