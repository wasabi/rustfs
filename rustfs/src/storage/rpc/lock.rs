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
use tracing::{debug_span, Instrument};

impl NodeService {
    pub(super) async fn handle_refresh(
        &self,
        request: Request<GenerallyLockRequest>,
    ) -> Result<Response<GenerallyLockResponse>, Status> {
        let request = request.into_inner();
        let _args: LockRequest = match serde_json::from_str(&request.args) {
            Ok(args) => args,
            Err(err) => {
                return Ok(Response::new(GenerallyLockResponse {
                    success: false,
                    error_info: Some(format!("can not decode args, err: {err}")),
                    lock_info: None,
                }));
            }
        };

        Ok(Response::new(GenerallyLockResponse {
            success: true,
            error_info: None,
            lock_info: None,
        }))
    }

    pub(super) async fn handle_force_un_lock(
        &self,
        request: Request<GenerallyLockRequest>,
    ) -> Result<Response<GenerallyLockResponse>, Status> {
        let request = request.into_inner();
        let args: LockRequest = match serde_json::from_str(&request.args) {
            Ok(args) => args,
            Err(err) => {
                return Ok(Response::new(GenerallyLockResponse {
                    success: false,
                    error_info: Some(format!("can not decode args, err: {err}")),
                    lock_info: None,
                }));
            }
        };

        let lock_client = self.get_lock_client()?;
        match lock_client.release(&args.lock_id).await {
            Ok(_) => Ok(Response::new(GenerallyLockResponse {
                success: true,
                error_info: None,
                lock_info: None,
            })),
            Err(err) => Ok(Response::new(GenerallyLockResponse {
                success: false,
                error_info: Some(format!(
                    "can not force_unlock, resource: {0}, owner: {1}, err: {2}",
                    args.resource, args.owner, err
                )),
                lock_info: None,
            })),
        }
    }

    pub(super) async fn handle_un_lock(
        &self,
        request: Request<GenerallyLockRequest>,
    ) -> Result<Response<GenerallyLockResponse>, Status> {
        let request = request.into_inner();
        let args: LockRequest = match serde_json::from_str(&request.args) {
            Ok(args) => args,
            Err(err) => {
                return Ok(Response::new(GenerallyLockResponse {
                    success: false,
                    error_info: Some(format!("can not decode args, err: {err}")),
                    lock_info: None,
                }));
            }
        };

        let lock_client = self.get_lock_client()?;
        match lock_client.release(&args.lock_id).await {
            Ok(_) => Ok(Response::new(GenerallyLockResponse {
                success: true,
                error_info: None,
                lock_info: None,
            })),
            Err(err) => Ok(Response::new(GenerallyLockResponse {
                success: false,
                error_info: Some(format!(
                    "can not unlock, resource: {0}, owner: {1}, err: {2}",
                    args.resource, args.owner, err
                )),
                lock_info: None,
            })),
        }
    }

    pub(super) async fn handle_lock(
        &self,
        request: Request<GenerallyLockRequest>,
    ) -> Result<Response<GenerallyLockResponse>, Status> {
        let request = request.into_inner();
        // Parse the request to extract resource and owner
        let args: LockRequest = match serde_json::from_str(&request.args) {
            Ok(args) => args,
            Err(err) => {
                return Ok(Response::new(GenerallyLockResponse {
                    success: false,
                    error_info: Some(format!("can not decode args, err: {err}")),
                    lock_info: None,
                }));
            }
        };

        let trace_id = args
            .metadata
            .tags
            .get("trace_id")
            .map(String::as_str)
            .unwrap_or("");
        let lock_source = args
            .metadata
            .tags
            .get("lock_source")
            .map(String::as_str)
            .unwrap_or("");
        let lock_source_detail = args
            .metadata
            .tags
            .get("lock_source_detail")
            .map(String::as_str)
            .unwrap_or("");

        let lock_client = self.get_lock_client()?;
        match lock_client
            .acquire_lock(&args)
            .instrument(debug_span!(
                target: "rustfs_lock_rpc",
                "lock_rpc.handle_lock",
                resource = %args.resource,
                operation_id = args.metadata.operation_id.as_deref().unwrap_or(""),
                trace_id = trace_id,
                lock_source = lock_source,
                lock_source_detail = lock_source_detail,
            ))
            .await
        {
            Ok(result) => {
                // Serialize lock_info if available
                let lock_info_json = result.lock_info.as_ref().and_then(|info| serde_json::to_string(info).ok());
                Ok(Response::new(GenerallyLockResponse {
                    success: result.success,
                    error_info: None,
                    lock_info: lock_info_json,
                }))
            }
            Err(err) => Ok(Response::new(GenerallyLockResponse {
                success: false,
                error_info: Some(format!(
                    "can not lock, resource: {0}, owner: {1}, err: {2}",
                    args.resource, args.owner, err
                )),
                lock_info: None,
            })),
        }
    }
}
