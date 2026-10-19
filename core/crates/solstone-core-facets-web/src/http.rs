// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use axum::{http::StatusCode, response::IntoResponse};
use solstone_core_convey_http::envelope::error_envelope;

pub fn error(
    reason_code: &str,
    message: &str,
    detail: String,
    status: StatusCode,
) -> axum::response::Response {
    error_envelope(reason_code, message, detail, status).into_response()
}
