// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

pub const REASON_HOME_MISSING_MOBILE: &str = "home_missing_mobile";
pub const REASON_SERVICE_TOKEN_REJECTED: &str = "service_token_rejected";
pub const REASON_RELAY_TUNNEL_REJECTED: &str = "relay_tunnel_rejected";
pub const REASON_RELAY_TUNNEL_UNREACHABLE: &str = "relay_tunnel_unreachable";
pub const REASON_LOCAL_PRIVATE_LISTENER_UNREACHABLE: &str = "local_private_listener_unreachable";
pub const REASON_RELAY_ADMISSION_SATURATED: &str = "relay_admission_saturated";

pub const OFFLINE_TUNNEL_REASONS: [&str; 2] = [
    REASON_SERVICE_TOKEN_REJECTED,
    REASON_LOCAL_PRIVATE_LISTENER_UNREACHABLE,
];
pub const LINK_HEALTH_EVENT: &str = "health";
pub const LINK_HEALTH_FRESHNESS_MS: i64 = 90_000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceTokenHealth {
    pub event_at_ms: Option<i64>,
    pub reason: Option<String>,
    pub tunnel_error_at_ms: Option<i64>,
    pub tunnel_success_at_ms: Option<i64>,
    pub now_ms: i64,
}

pub fn service_token_health_is_refused(health: &ServiceTokenHealth) -> bool {
    let is_fresh = health
        .event_at_ms
        .is_some_and(|ts| health.now_ms - ts <= LINK_HEALTH_FRESHNESS_MS);
    if !is_fresh {
        return false;
    }
    let is_token_rejected = health.reason.as_deref() == Some(REASON_SERVICE_TOKEN_REJECTED);
    let error_at = health.tunnel_error_at_ms.unwrap_or(0);
    let success_at = health.tunnel_success_at_ms.unwrap_or(0);
    is_token_rejected && error_at >= success_at
}
