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

/// Environment variable to override the per-peer gRPC channel pool size.
///
/// When unset, the pool size is computed from the Tokio worker thread count:
/// one connection per four worker threads, clamped to `[1, 16]`.
/// Set to `1` to disable pooling (equivalent to the pre-pool behavior).
pub const ENV_RPC_CHANNEL_POOL_SIZE: &str = "RUSTFS_RPC_CHANNEL_POOL_SIZE";

/// Maximum number of gRPC channels per peer regardless of thread count or override.
pub const RPC_MAX_POOL_SIZE: usize = 16;
