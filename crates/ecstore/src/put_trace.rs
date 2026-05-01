// Copyright 2024 RustFS Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Gated spans for ecstore PUT (`RUSTFS_TRACE_S3_HANDLING_ENABLED`).

use rustfs_config::observability::ENV_TRACE_S3_HANDLING_ENABLED;
use rustfs_utils::get_env_bool;
use std::sync::OnceLock;

pub(crate) fn enabled() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| get_env_bool(ENV_TRACE_S3_HANDLING_ENABLED, false))
}

/// Single-phase span for `set_disk::put_object` substeps (`put.phase`).
pub(crate) fn span_put_phase(phase: &'static str, bucket: &str, object: &str) -> tracing::Span {
    if !enabled() {
        return tracing::Span::none();
    }
    tracing::info_span!(
        target: "rustfs_put_trace",
        parent: tracing::Span::current(),
        "put_object.trace",
        put.phase = phase,
        bucket = %bucket,
        object = %object,
    )
}

/// Like [`span_put_phase`] but includes lock correlation `trace_id`.
pub(crate) fn span_put_phase_trace(phase: &'static str, bucket: &str, object: &str, trace_id: &str) -> tracing::Span {
    if !enabled() {
        return tracing::Span::none();
    }
    tracing::info_span!(
        target: "rustfs_put_trace",
        parent: tracing::Span::current(),
        "put_object.trace",
        put.phase = phase,
        bucket = %bucket,
        object = %object,
        trace_id = %trace_id,
    )
}
