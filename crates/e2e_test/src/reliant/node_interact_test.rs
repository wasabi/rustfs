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
use futures::future::join_all;
use rmp_serde::{Deserializer, Serializer};
use rustfs_ecstore::disk::{VolumeInfo, WalkDirOptions};
use rustfs_ecstore::rpc::{TonicInterceptor, gen_tonic_signature_interceptor, node_service_time_out_client};
use rustfs_filemeta::{MetaCacheEntry, MetacacheReader, MetacacheWriter};
use rustfs_protos::proto_gen::node_service::WalkDirRequest;
use rustfs_protos::{
    models::{PingBody, PingBodyBuilder},
    proto_gen::node_service::{
        ListVolumesRequest, LocalStorageInfoRequest, MakeVolumeRequest, PingRequest, PingResponse, ReadAllRequest,
    },
};
use serde::{Deserialize, Serialize};
use std::io::Cursor;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
use tokio::spawn;
use tonic::Request;
use tonic::codegen::tokio_stream::StreamExt;

#[tokio::test]
async fn ping() -> Result<(), BoxError> {
    init_logging();
    let mut env = RustFSTestEnvironment::new().await?;
    env.start_rustfs_server(vec![]).await?;

    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let payload = fbb.create_vector(b"hello world");

    let mut builder = PingBodyBuilder::new(&mut fbb);
    builder.add_payload(payload);
    let root = builder.finish();
    fbb.finish(root, None);

    let finished_data = fbb.finished_data();

    let decoded_payload = flatbuffers::root::<PingBody>(finished_data);
    assert!(decoded_payload.is_ok());

    // Create client
    let mut client = node_service_time_out_client(&env.url, TonicInterceptor::Signature(gen_tonic_signature_interceptor()))
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    // Construct PingRequest
    let request = Request::new(PingRequest {
        version: 1,
        body: bytes::Bytes::copy_from_slice(finished_data),
    });

    // Send request and get response
    let response: PingResponse = client.ping(request).await?.into_inner();

    // Print response
    let ping_response_body = flatbuffers::root::<PingBody>(&response.body);
    if let Err(e) = ping_response_body {
        eprintln!("{e}");
    } else {
        println!("ping_resp:body(flatbuffer): {ping_response_body:?}");
    }

    Ok(())
}

#[tokio::test]
async fn make_volume() -> Result<(), BoxError> {
    init_logging();
    let mut env = RustFSTestEnvironment::new().await?;
    env.start_rustfs_server(vec![]).await?;
    let mut client = node_service_time_out_client(&env.url, TonicInterceptor::Signature(gen_tonic_signature_interceptor()))
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let request = Request::new(MakeVolumeRequest {
        disk: "data".to_string(),
        volume: "dandan".to_string(),
    });

    let response = client.make_volume(request).await?.into_inner();
    if response.success {
        println!("success");
    } else {
        println!("failed: {:?}", response.error);
    }
    Ok(())
}

#[tokio::test]
async fn list_volumes() -> Result<(), BoxError> {
    init_logging();
    let mut env = RustFSTestEnvironment::new().await?;
    env.start_rustfs_server(vec![]).await?;
    let mut client = node_service_time_out_client(&env.url, TonicInterceptor::Signature(gen_tonic_signature_interceptor()))
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let request = Request::new(ListVolumesRequest {
        disk: "data".to_string(),
    });

    let response = client.list_volumes(request).await?.into_inner();
    let volume_infos: Vec<VolumeInfo> = response
        .volume_infos
        .into_iter()
        .filter_map(|json_str| serde_json::from_str::<VolumeInfo>(&json_str).ok())
        .collect();

    println!("{volume_infos:?}");
    Ok(())
}

#[tokio::test]
async fn walk_dir() -> Result<(), BoxError> {
    init_logging();
    let mut env = RustFSTestEnvironment::new().await?;
    env.start_rustfs_server(vec![]).await?;
    // Create bucket so the volume exists for walk_dir
    env.create_test_bucket("dandan").await?;
    println!("walk_dir");
    let opts = WalkDirOptions {
        bucket: "dandan".to_owned(),
        base_dir: "".to_owned(),
        recursive: true,
        ..Default::default()
    };
    let (rd, mut wr) = tokio::io::duplex(1024);
    let mut buf = Vec::new();
    opts.serialize(&mut Serializer::new(&mut buf))?;
    let mut client = node_service_time_out_client(&env.url, TonicInterceptor::Signature(gen_tonic_signature_interceptor()))
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    // Use the server's data directory (same path we started RustFS with).
    let disk_path = env.temp_dir.clone();
    let request = Request::new(WalkDirRequest {
        disk: disk_path,
        walk_dir_options: buf.into(),
    });
    let mut response = client.walk_dir(request).await?.into_inner();

    let job1 = spawn(async move {
        let mut out = MetacacheWriter::new(&mut wr);
        loop {
            match response.next().await {
                Some(Ok(resp)) => {
                    if !resp.success {
                        println!("{}", resp.error_info.unwrap_or("".to_string()));
                        let _ = out.close().await;
                        break;
                    }
                    if let Ok(entry) = serde_json::from_str::<MetaCacheEntry>(&resp.meta_cache_entry) {
                        let _ = out.write_obj(&entry).await;
                    }
                }
                None => {
                    let _ = out.close().await;
                    break;
                }
                _ => {
                    let _ = out.close().await;
                    break;
                }
            }
        }
    });
    let job2 = spawn(async move {
        let mut reader = MetacacheReader::new(rd);
        while let Ok(Some(entry)) = reader.peek().await {
            println!("{entry:?}");
        }
    });

    join_all(vec![job1, job2]).await;
    Ok(())
}

#[tokio::test]
async fn read_all() -> Result<(), BoxError> {
    init_logging();
    let mut env = RustFSTestEnvironment::new().await?;
    env.start_rustfs_server(vec![]).await?;
    let mut client = node_service_time_out_client(&env.url, TonicInterceptor::Signature(gen_tonic_signature_interceptor()))
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let request = Request::new(ReadAllRequest {
        disk: "data".to_string(),
        volume: "ff".to_string(),
        path: "format.json".to_string(),
    });

    let response = client.read_all(request).await?.into_inner();
    let volume_infos = response.data;

    println!("{}", response.success);
    println!("{volume_infos:?}");
    Ok(())
}

#[tokio::test]
async fn storage_info() -> Result<(), BoxError> {
    init_logging();
    let mut env = RustFSTestEnvironment::new().await?;
    env.start_rustfs_server(vec![]).await?;
    let mut client = node_service_time_out_client(&env.url, TonicInterceptor::Signature(gen_tonic_signature_interceptor()))
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let request = Request::new(LocalStorageInfoRequest { metrics: true });

    let response = client.local_storage_info(request).await?.into_inner();
    if !response.success {
        println!("{:?}", response.error_info);
        return Ok(());
    }
    let info = response.storage_info;

    let mut buf = Deserializer::new(Cursor::new(info));
    let storage_info: rustfs_madmin::StorageInfo = Deserialize::deserialize(&mut buf).unwrap();
    println!("{storage_info:?}");
    Ok(())
}
