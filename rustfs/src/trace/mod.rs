// Copyright 2024 RustFS Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Optional S3 request-handling and PUT-phase tracing (see `RUSTFS_TRACE_S3_HANDLING_ENABLED`).

mod s3;
pub use s3::{put_phase_span, with_s3_handling_span};
