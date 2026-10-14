// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use axum::{
    body::Body,
    http::{Response, header},
};

// Shared Convey shell lives in solstone-core-convey-shell/assets/static/shell.html.
const SHELL: &[u8] = include_bytes!("../../solstone-core-convey-shell/assets/static/shell.html");
const WORKSPACE: &[u8] = include_bytes!("../assets/workspace.html");
const HOME_JS: &[u8] = include_bytes!("../assets/home.js");
const WEEK_JS: &[u8] = include_bytes!("../assets/week.js");
const REMOVALS_JS: &[u8] = include_bytes!("../assets/removals.js");

pub async fn shell() -> Response<Body> {
    asset(SHELL, "text/html; charset=utf-8")
}

pub async fn workspace() -> Response<Body> {
    asset(WORKSPACE, "text/html; charset=utf-8")
}

pub async fn home_js() -> Response<Body> {
    asset(HOME_JS, "text/javascript; charset=utf-8")
}

pub async fn week_js() -> Response<Body> {
    asset(WEEK_JS, "text/javascript; charset=utf-8")
}

pub async fn removals_js() -> Response<Body> {
    asset(REMOVALS_JS, "text/javascript; charset=utf-8")
}

fn asset(bytes: &'static [u8], content_type: &'static str) -> Response<Body> {
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(bytes))
        .expect("embedded home asset response")
}
