#![cfg(test)]
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

use crate::common::{RustFSTestEnvironment, init_logging};
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Credentials, Region};
use bytes::Bytes;
use serial_test::serial;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const ACCESS_KEY: &str = "rustfsadmin";
const SECRET_KEY: &str = "rustfsadmin";
const BUCKET: &str = "test-basic-bucket";

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

async fn setup_test_bucket(client: &Client) -> Result<(), BoxError> {
    match client.create_bucket().bucket(BUCKET).send().await {
        Ok(_) => {}
        Err(e) => {
            let error_str = e.to_string();
            if !error_str.contains("BucketAlreadyOwnedByYou") && !error_str.contains("BucketAlreadyExists") {
                return Err(e.into());
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn test_bucket_lifecycle_configuration() -> Result<(), BoxError> {
    use aws_sdk_s3::types::{BucketLifecycleConfiguration, LifecycleExpiration, LifecycleRule, LifecycleRuleFilter};

    init_logging();
    let mut env = RustFSTestEnvironment::new().await?;
    env.start_rustfs_server(vec![]).await?;
    let client = create_aws_s3_client(&env.url).await?;
    setup_test_bucket(&client).await?;

    // Upload test object first
    let test_content = "Test object for lifecycle expiration";
    let lifecycle_object_key = "lifecycle-test-object.txt";
    client
        .put_object()
        .bucket(BUCKET)
        .key(lifecycle_object_key)
        .body(Bytes::from(test_content.as_bytes()).into())
        .send()
        .await?;

    // Verify object exists initially
    let resp = client.get_object().bucket(BUCKET).key(lifecycle_object_key).send().await?;
    assert!(resp.content_length().unwrap_or(0) > 0);

    // Configure lifecycle rule: expire after 1 day (server requires days > 0)
    let expiration = LifecycleExpiration::builder().days(1).build();
    let filter = LifecycleRuleFilter::builder().prefix(lifecycle_object_key).build();
    let rule = LifecycleRule::builder()
        .id("expire-test-object")
        .filter(filter)
        .expiration(expiration)
        .status(aws_sdk_s3::types::ExpirationStatus::Enabled)
        .build()?;
    let lifecycle = BucketLifecycleConfiguration::builder().rules(rule).build()?;

    client
        .put_bucket_lifecycle_configuration()
        .bucket(BUCKET)
        .lifecycle_configuration(lifecycle)
        .send()
        .await?;

    // Verify lifecycle configuration was set and returned
    let resp = client.get_bucket_lifecycle_configuration().bucket(BUCKET).send().await?;
    let rules = resp.rules();
    assert!(rules.iter().any(|r| r.id().unwrap_or("") == "expire-test-object"));

    // Object still exists (expiration is 1 day; we do not wait for scanner to delete)
    let get_result = client.get_object().bucket(BUCKET).key(lifecycle_object_key).send().await?;
    assert!(get_result.content_length().unwrap_or(0) > 0);

    println!("Lifecycle configuration test completed.");
    Ok(())
}
