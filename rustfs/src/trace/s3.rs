// Copyright 2024 RustFS Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

use rustfs_config::observability::ENV_TRACE_S3_HANDLING_ENABLED;
use rustfs_utils::get_env_bool;
use std::sync::OnceLock;
use tracing::Instrument;

pub fn trace_s3_handling_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| get_env_bool(ENV_TRACE_S3_HANDLING_ENABLED, false))
}

/// Wraps an S3 API handler future with a child span of the current HTTP span (`s3.handling`, `s3.operation`).
pub async fn with_s3_handling_span<Fut, T, E>(operation: &'static str, fut: Fut) -> Result<T, E>
where
    Fut: std::future::Future<Output = Result<T, E>>,
{
    if !trace_s3_handling_enabled() {
        return fut.await;
    }
    let span = tracing::info_span!(
        target: "rustfs_s3_trace",
        parent: tracing::Span::current(),
        "s3.handling",
        s3.operation = operation,
    );
    fut.instrument(span).await
}

/// Phase span under `put_object` / ecstore PUT path (`put_object.phase`).
pub fn put_phase_span(phase: &'static str, bucket: &str, object: &str) -> tracing::Span {
    if !trace_s3_handling_enabled() {
        return tracing::Span::none();
    }
    tracing::info_span!(
        target: "rustfs_put_trace",
        parent: tracing::Span::current(),
        "put_object.phase",
        phase = phase,
        bucket = %bucket,
        object = %object,
    )
}
