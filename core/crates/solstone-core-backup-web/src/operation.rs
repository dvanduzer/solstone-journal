// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#[cfg(not(windows))]
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
#[cfg(not(windows))]
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
use std::{cell::Cell, thread_local};

use axum::http::StatusCode;
use axum::response::Response;
use serde::Serialize;

use crate::{response, validation::RefusedReason};

pub const HANDOFF_TTL: Duration = Duration::from_secs(30 * 60);

const TERMINAL: &[&str] = &["done", "error", "degraded", "needs_subscription", "refused"];

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Operation {
    pub kind: String,
    pub phase: String,
    pub reason_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guidance: Option<String>,
    pub recording_failure: Option<String>,
    pub portal_url: Option<String>,
}

pub struct Slot {
    pub view: Operation,
    // Hosted wait capability; never serialized (the nonce only appears inside portal_url).
    pub nonce: Option<String>,
    // restore_hosted wait only; never an Operation field so it cannot leak into JSON.
    pub restore_key: Option<String>,
    pub started: Instant,
    pub generation: u64,
    pub(crate) worker_active: bool,
    #[cfg(windows)]
    pub(crate) cleanup: Option<PendingCleanup>,
}

#[cfg(windows)]
pub(crate) struct PendingCleanup {
    pub(crate) owners: Vec<solstone_core_system::process::BoundedHelperCleanup>,
    terminal: Terminal,
}

#[cfg(windows)]
fn observe_cleanup(current: &mut Slot) {
    use solstone_core_system::process::HelperCleanupStatus;
    let settled = current.cleanup.as_ref().is_some_and(|pending| {
        pending
            .owners
            .iter()
            .all(|owner| matches!(owner.observe(), HelperCleanupStatus::Quiescent))
    });
    if settled && let Some(pending) = current.cleanup.take() {
        apply_terminal(current, pending.terminal);
    }
}

fn apply_terminal(current: &mut Slot, terminal: Terminal) {
    current.view.phase = terminal.phase;
    current.view.reason_code = terminal.reason_code;
    current.view.detail = terminal.detail;
    current.view.guidance = terminal.guidance;
    current.view.recording_failure = terminal.recording_failure;
    current.view.portal_url = None;
    current.nonce = None;
    current.restore_key = None;
}

pub type SharedOperationSlot = Arc<Mutex<Option<Slot>>>;

#[cfg(test)]
thread_local! {
    static INSTANCE_ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

pub fn new_slot() -> SharedOperationSlot {
    Arc::new(Mutex::new(None))
}

pub fn is_terminal(phase: &str) -> bool {
    TERMINAL.contains(&phase)
}

pub fn running_phase(kind: &str) -> &'static str {
    match kind {
        "enable" | "enable_hosted" => "setting_up",
        "restore" | "restore_hosted" | "offload_restore" => "restoring",
        "rotate" => "rotating",
        "teardown" => "tearing_down",
        _ => "setting_up",
    }
}

fn hosted_wait_expired(slot: &Slot) -> bool {
    if slot.worker_active {
        return false;
    }
    #[cfg(windows)]
    if slot.cleanup.is_some() {
        return false;
    }
    !is_terminal(&slot.view.phase)
        && matches!(slot.view.kind.as_str(), "enable_hosted" | "restore_hosted")
        && slot.nonce.is_some()
        && slot.started.elapsed() > HANDOFF_TTL
}

fn expire_hosted_wait_in_place(slot: &mut Slot) {
    if !hosted_wait_expired(slot) {
        return;
    }
    slot.view.phase = "error".into();
    slot.view.reason_code = Some("expired".into());
    slot.view.detail = None;
    slot.view.guidance = None;
    slot.view.recording_failure = None;
    slot.view.portal_url = None;
    slot.nonce = None;
    slot.restore_key = None;
}

fn observe(slot: &mut Option<Slot>) -> Option<&Slot> {
    if let Some(current) = slot.as_mut() {
        #[cfg(windows)]
        observe_cleanup(current);
        expire_hosted_wait_in_place(current);
    }
    slot.as_ref()
}

pub fn is_busy(slot: &SharedOperationSlot) -> bool {
    #[cfg(windows)]
    {
        use solstone_core_system::process::{
            HelperAdmissionStatus, observe_bounded_helper_admission,
        };
        if !matches!(
            observe_bounded_helper_admission(),
            HelperAdmissionStatus::Ready
        ) {
            return true;
        }
        return observed_current(slot).map_or(true, |operation| {
            operation.is_some_and(|operation| !is_terminal(&operation.phase))
        });
    }
    #[cfg(not(windows))]
    observe(&mut slot.lock().expect("operation slot lock"))
        .is_some_and(|slot| !is_terminal(&slot.view.phase))
}

#[cfg(windows)]
pub(crate) fn observed_current(slot: &SharedOperationSlot) -> Result<Option<Operation>, ()> {
    let mut guard = slot.try_lock().map_err(|_| ())?;
    Ok(observe(&mut guard).map(|slot| slot.view.clone()))
}

#[cfg(windows)]
pub(crate) fn observed_generation(
    slot: &SharedOperationSlot,
    generation: Option<u64>,
) -> Result<Option<Operation>, ()> {
    let mut guard = slot.try_lock().map_err(|_| ())?;
    if guard.as_ref().map(|current| current.generation) != generation {
        return Err(());
    }
    Ok(observe(&mut guard).map(|current| current.view.clone()))
}

#[cfg(all(test, windows))]
pub(crate) fn worker_or_cleanup_active(slot: &SharedOperationSlot, generation: u64) -> bool {
    let Ok(guard) = slot.try_lock() else {
        return true;
    };
    guard.as_ref().is_some_and(|current| {
        current.generation == generation && (current.worker_active || current.cleanup.is_some())
    })
}

pub(crate) fn start_worker(slot: &SharedOperationSlot, generation: u64) -> bool {
    #[cfg(windows)]
    let Ok(mut guard) = slot.try_lock() else {
        return false;
    };
    #[cfg(not(windows))]
    let mut guard = slot.lock().expect("operation slot lock");
    let Some(current) = guard.as_mut() else {
        return false;
    };
    if current.generation != generation || is_terminal(&current.view.phase) || current.worker_active
    {
        return false;
    }
    #[cfg(windows)]
    if current.cleanup.is_some() {
        return false;
    }
    current.worker_active = true;
    true
}

#[cfg(windows)]
pub(crate) fn finish_worker(
    slot: &SharedOperationSlot,
    generation: u64,
    terminal: Terminal,
    owners: Vec<solstone_core_system::process::BoundedHelperCleanup>,
) {
    let Ok(mut guard) = slot.lock() else {
        return;
    };
    let Some(current) = guard.as_mut() else {
        return;
    };
    if current.generation != generation || !current.worker_active {
        return;
    }
    current.worker_active = false;
    if owners.iter().any(|owner| {
        matches!(
            owner.observe(),
            solstone_core_system::process::HelperCleanupStatus::Pending
        )
    }) {
        current.view.phase = "cleanup_pending".into();
        current.view.reason_code = Some("cleanup_pending".into());
        current.view.portal_url = None;
        current.nonce = None;
        current.restore_key = None;
        current.cleanup = Some(PendingCleanup { owners, terminal });
    } else {
        apply_terminal(current, terminal);
    }
}

#[cfg(not(windows))]
pub(crate) fn finish_worker(slot: &SharedOperationSlot, generation: u64, terminal: Terminal) {
    let mut guard = slot.lock().expect("operation slot lock");
    let Some(current) = guard.as_mut() else {
        return;
    };
    if current.generation != generation || !current.worker_active {
        return;
    }
    current.worker_active = false;
    apply_terminal(current, terminal);
}

#[cfg(any(not(windows), test))]
pub fn current(slot: &SharedOperationSlot) -> Option<Operation> {
    observe(&mut slot.lock().expect("operation slot lock")).map(|slot| slot.view.clone())
}

pub fn busy_response() -> Response {
    response::error(
        StatusCode::BAD_REQUEST,
        "that action isn't available in the current state.",
        "backup_busy",
        "",
    )
}

pub struct Begin {
    pub generation: u64,
}

pub fn begin(
    slot: &SharedOperationSlot,
    kind: &str,
    portal_url: Option<String>,
    nonce: Option<String>,
    restore_key: Option<String>,
) -> Result<Begin, Response> {
    #[cfg(windows)]
    {
        use solstone_core_system::process::{
            HelperAdmissionStatus, observe_bounded_helper_admission,
        };
        if !matches!(
            observe_bounded_helper_admission(),
            HelperAdmissionStatus::Ready
        ) {
            return Err(busy_response());
        }
    }
    #[cfg(windows)]
    let mut guard = slot.try_lock().map_err(|_| busy_response())?;
    #[cfg(not(windows))]
    let mut guard = slot.lock().expect("operation slot lock");
    if observe(&mut guard).is_some_and(|slot| !is_terminal(&slot.view.phase)) {
        return Err(busy_response());
    }
    let generation = guard
        .as_ref()
        .map(|slot| slot.generation.wrapping_add(1))
        .unwrap_or(1);
    let view = Operation {
        kind: kind.to_owned(),
        phase: running_phase(kind).to_owned(),
        reason_code: None,
        detail: None,
        guidance: None,
        recording_failure: None,
        portal_url,
    };
    *guard = Some(Slot {
        view,
        nonce,
        restore_key,
        started: Instant::now(),
        generation,
        worker_active: false,
        #[cfg(windows)]
        cleanup: None,
    });
    Ok(Begin { generation })
}

pub fn finish(
    slot: &SharedOperationSlot,
    generation: u64,
    phase: impl Into<String>,
    reason_code: Option<String>,
    recording_failure: Option<String>,
) {
    finish_terminal(
        slot,
        generation,
        Terminal {
            phase: phase.into(),
            reason_code,
            detail: None,
            guidance: None,
            recording_failure,
        },
    );
}

fn finish_terminal(slot: &SharedOperationSlot, generation: u64, terminal: Terminal) -> bool {
    let mut guard = slot.lock().expect("operation slot lock");
    let Some(current) = guard.as_mut() else {
        return false;
    };
    if current.generation != generation || is_terminal(&current.view.phase) {
        return false;
    }
    if current.worker_active {
        return false;
    }
    #[cfg(windows)]
    if current.cleanup.is_some() {
        return false;
    }
    apply_terminal(current, terminal);
    true
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Terminal {
    pub phase: String,
    pub reason_code: Option<String>,
    pub detail: Option<String>,
    pub guidance: Option<String>,
    pub recording_failure: Option<String>,
}

impl Terminal {
    pub fn done() -> Self {
        Self::phase("done", None)
    }

    pub fn error(reason_code: impl Into<String>) -> Self {
        Self {
            phase: "error".into(),
            reason_code: Some(reason_code.into()),
            detail: None,
            guidance: None,
            recording_failure: None,
        }
    }

    pub fn tool_error(error: solstone_core_backup_runtime::ClosedToolError) -> Self {
        Self {
            phase: "error".into(),
            reason_code: Some(error.to_string()),
            detail: Some(error.detail().to_owned()),
            guidance: Some(error.guidance().to_owned()),
            recording_failure: None,
        }
    }

    pub fn phase(phase: impl Into<String>, reason_code: Option<String>) -> Self {
        Self {
            phase: phase.into(),
            reason_code,
            detail: None,
            guidance: None,
            recording_failure: None,
        }
    }

    pub fn restore(
        phase: impl Into<String>,
        reason_code: Option<String>,
        recording_failure: Option<String>,
    ) -> Self {
        Self {
            phase: phase.into(),
            reason_code,
            detail: None,
            guidance: None,
            recording_failure,
        }
    }
}

#[cfg(not(windows))]
pub fn spawn_worker<F>(
    slot: SharedOperationSlot,
    generation: u64,
    work: F,
) -> Option<thread::JoinHandle<()>>
where
    F: FnOnce() -> Terminal + Send + 'static,
{
    if !start_worker(&slot, generation) {
        return None;
    }
    // ToolRunner/HttpTransport are blocking std process/HTTP calls. axum already
    // owns the tokio runtime; a worker thread must not hold the slot lock across
    // restic/broker, and resolve_operational_tools runs here so POST returns before
    // restic install.
    let failed_spawn = slot.clone();
    match spawn_thread(move || {
        let terminal = panic::catch_unwind(AssertUnwindSafe(work))
            .unwrap_or_else(|_| Terminal::error("failed"));
        finish_worker(&slot, generation, terminal);
    }) {
        Ok(handle) => Some(handle),
        Err(_) => {
            finish_worker(&failed_spawn, generation, Terminal::error("failed"));
            None
        }
    }
}

#[cfg(all(test, feature = "full-tests"))]
#[cfg(not(windows))]
thread_local! {
    static FAIL_WORKER_START: Cell<bool> = const { Cell::new(false) };
}

#[cfg(not(windows))]
fn spawn_thread(work: impl FnOnce() + Send + 'static) -> std::io::Result<thread::JoinHandle<()>> {
    #[cfg(all(test, feature = "full-tests"))]
    if FAIL_WORKER_START.with(|fail| fail.replace(false)) {
        return Err(std::io::Error::other(
            "injected backup worker creation failure",
        ));
    }
    thread::Builder::new().spawn(work)
}

#[allow(dead_code)]
pub fn mint_hex() -> Result<String, getrandom::Error> {
    #[cfg(test)]
    INSTANCE_ALLOCATIONS.with(|count| count.set(count.get().saturating_add(1)));
    mint_hex_from_csprng()
}

pub fn mint_capability() -> Result<String, getrandom::Error> {
    mint_hex_from_csprng()
}

fn mint_hex_from_csprng() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
pub fn reset_instance_allocations() {
    INSTANCE_ALLOCATIONS.with(|count| count.set(0));
}

#[cfg(test)]
pub fn instance_allocations() -> usize {
    INSTANCE_ALLOCATIONS.with(Cell::get)
}

pub fn portal_url(
    base: &str,
    nonce: &str,
    instance: &str,
    assertion: &str,
    ca_pubkey: &str,
) -> String {
    solstone_core_sol_link::home_reach::service_enable_portal_url(
        base, "backup", nonce, instance, assertion, ca_pubkey,
    )
}

pub fn restore_portal_url(base: &str, nonce: &str) -> String {
    format!(
        "{}/enable/backup?nonce={nonce}&intent=restore",
        base.trim_end_matches('/')
    )
}

pub struct HandoffMatch {
    pub kind: String,
    pub restore_key: Option<String>,
}

pub enum HandoffError {
    Invalid,
    Expired,
}

pub fn match_handoff(
    slot: &SharedOperationSlot,
    nonce: &str,
) -> Result<HandoffMatch, HandoffError> {
    let mut guard = slot.lock().expect("operation slot lock");
    let Some(current) = guard.as_mut() else {
        return Err(HandoffError::Invalid);
    };
    expire_hosted_wait_in_place(current);
    if is_terminal(&current.view.phase) {
        return if matches!(
            current.view.reason_code.as_deref(),
            Some("expired") | Some("restore_prepare_expired")
        ) {
            Err(HandoffError::Expired)
        } else {
            Err(HandoffError::Invalid)
        };
    }
    if !matches!(
        current.view.kind.as_str(),
        "enable_hosted" | "restore_hosted"
    ) || current.nonce.as_deref() != Some(nonce)
    {
        return Err(HandoffError::Invalid);
    }
    let _ = current.nonce.take();
    Ok(HandoffMatch {
        kind: current.view.kind.clone(),
        restore_key: current.restore_key.clone(),
    })
}

pub fn mark_needs_subscription(slot: &SharedOperationSlot, generation: u64) {
    finish(slot, generation, "needs_subscription", None, None);
}

pub fn mark_refused(slot: &SharedOperationSlot, generation: u64, reason: RefusedReason) {
    finish(
        slot,
        generation,
        "refused",
        Some(reason.code().into()),
        None,
    );
}

pub fn mark_expired(slot: &SharedOperationSlot, generation: u64) {
    finish(slot, generation, "error", Some("expired".into()), None);
}

pub fn mark_prepare_lease_expired(slot: &SharedOperationSlot, generation: u64) {
    finish(
        slot,
        generation,
        "error",
        Some("restore_prepare_expired".into()),
        None,
    );
}

pub fn mark_cancelled(slot: &SharedOperationSlot, generation: u64) -> bool {
    finish_terminal(slot, generation, Terminal::error("cancelled"))
}

pub fn nonce_for_generation(slot: &SharedOperationSlot, generation: u64) -> Option<String> {
    let guard = slot.lock().expect("operation slot lock");
    let current = guard.as_ref()?;
    (current.generation == generation
        && !is_terminal(&current.view.phase)
        && current.view.kind == "restore_hosted")
        .then(|| current.nonce.clone())
        .flatten()
}

pub fn generation_of(slot: &SharedOperationSlot) -> Option<u64> {
    slot.lock()
        .expect("operation slot lock")
        .as_ref()
        .map(|slot| slot.generation)
}

#[cfg(test)]
pub fn backdate_started(slot: &SharedOperationSlot, age: Duration) {
    let mut guard = slot.lock().expect("operation slot lock");
    if let Some(current) = guard.as_mut() {
        current.started = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
    }
}

#[cfg(test)]
mod tests {
    use super::terminal_tests::admitted_slot;
    use super::{
        HandoffError, mark_prepare_lease_expired, match_handoff, new_slot, portal_url,
        restore_portal_url,
    };

    fn parse_portal_url(url: &str) -> (&str, &str, &str, Vec<(&str, &str)>) {
        let (scheme, remainder) = url.split_once("://").expect("scheme separator");
        let authority_end = remainder.find('/').expect("authority/path separator");
        let (authority, path_and_query) = remainder.split_at(authority_end);
        let (path, query) = path_and_query
            .split_once('?')
            .expect("path/query separator");

        let mut query_pairs = query
            .split('&')
            .map(|pair| pair.split_once('=').expect("query key/value separator"))
            .collect::<Vec<_>>();
        query_pairs.sort_unstable();

        assert!(
            query_pairs.windows(2).all(|pair| pair[0].0 != pair[1].0),
            "duplicate query key"
        );

        (scheme, authority, path, query_pairs)
    }

    #[test]
    fn portal_url_uses_exact_backup_handoff_route_and_query() {
        let nonce = "alpha-nonce-1";
        let instance = "beta-instance-2";
        let assertion = "test-assertion";
        let ca_pubkey = "test-ca-pubkey";
        let url = portal_url(
            "http://portal.example.test:8123/",
            nonce,
            instance,
            assertion,
            ca_pubkey,
        );

        let (scheme, authority, path, query_pairs) = parse_portal_url(&url);

        assert_eq!(scheme, "http");
        assert_eq!(authority, "portal.example.test:8123");
        assert_eq!(path, "/enable/backup");
        assert_eq!(
            query_pairs,
            vec![
                ("assertion", assertion),
                ("ca_pubkey", ca_pubkey),
                ("instance", instance),
                ("nonce", nonce),
            ]
        );
    }

    #[test]
    fn restore_portal_url_uses_restore_intent_without_instance() {
        let nonce = "alpha-nonce-1";
        let url = restore_portal_url("http://portal.example.test:8123/", nonce);

        let (_, _, path, query_pairs) = parse_portal_url(&url);

        assert_eq!(path, "/enable/backup");
        assert_eq!(query_pairs, vec![("intent", "restore"), ("nonce", nonce)]);
    }

    #[test]
    fn match_handoff_classifies_prepare_lease_expiry_as_expired_not_invalid() {
        let slot = new_slot();
        *slot.lock().unwrap() = Some(admitted_slot(
            "restore_hosted",
            1,
            None,
            Some("nonce-1".into()),
            None,
        ));
        mark_prepare_lease_expired(&slot, 1);

        let result = match_handoff(&slot, "nonce-1");

        assert!(matches!(result, Err(HandoffError::Expired)));
    }
}

#[cfg(test)]
mod terminal_tests {
    use super::*;

    pub(super) fn running_slot() -> SharedOperationSlot {
        Arc::new(Mutex::new(Some(admitted_slot(
            "restore_hosted",
            2,
            Some("https://example.test/restore".into()),
            Some("test-nonce".into()),
            Some("test-key".into()),
        ))))
    }

    pub(super) fn admitted_slot(
        kind: &str,
        generation: u64,
        portal_url: Option<String>,
        nonce: Option<String>,
        restore_key: Option<String>,
    ) -> Slot {
        Slot {
            view: Operation {
                kind: kind.into(),
                phase: running_phase(kind).into(),
                reason_code: None,
                detail: None,
                guidance: None,
                recording_failure: None,
                portal_url,
            },
            nonce,
            restore_key,
            started: Instant::now(),
            generation,
            worker_active: false,
            #[cfg(windows)]
            cleanup: None,
        }
    }

    pub(super) fn failure() -> Terminal {
        Terminal {
            phase: "degraded".into(),
            reason_code: Some("tool_failed".into()),
            detail: Some("tool detail".into()),
            guidance: Some("next step".into()),
            recording_failure: Some("recording failed".into()),
        }
    }

    pub(super) fn assert_failure(slot: &SharedOperationSlot) {
        let guard = slot.lock().unwrap();
        let current = guard.as_ref().unwrap();
        assert_eq!(current.view.phase, "degraded");
        assert_eq!(current.view.reason_code.as_deref(), Some("tool_failed"));
        assert_eq!(current.view.detail.as_deref(), Some("tool detail"));
        assert_eq!(current.view.guidance.as_deref(), Some("next step"));
        assert_eq!(
            current.view.recording_failure.as_deref(),
            Some("recording failed")
        );
        assert!(current.view.portal_url.is_none());
        assert!(current.nonce.is_none());
        assert!(current.restore_key.is_none());
    }

    #[test]
    fn complete_terminal_preserves_payload_and_clears_secrets() {
        let slot = running_slot();
        finish_terminal(&slot, 2, failure());
        assert_failure(&slot);
    }

    #[test]
    fn complete_terminal_ignores_absent_stale_and_terminal_slots() {
        let absent = new_slot();
        finish_terminal(&absent, 2, failure());
        assert!(absent.lock().unwrap().is_none());

        let slot = running_slot();
        let original = slot.lock().unwrap().as_ref().unwrap().view.clone();
        finish_terminal(&slot, 1, failure());
        assert_eq!(slot.lock().unwrap().as_ref().unwrap().view, original);
        assert!(slot.lock().unwrap().as_ref().unwrap().restore_key.is_some());
        finish_terminal(&slot, 2, failure());
        finish_terminal(&slot, 2, Terminal::done());
        assert_failure(&slot);
    }

    #[cfg(windows)]
    #[test]
    fn complete_terminal_respects_worker_and_cleanup_ownership() {
        let slot = running_slot();
        let original = slot.lock().unwrap().as_ref().unwrap().view.clone();
        slot.lock().unwrap().as_mut().unwrap().worker_active = true;
        finish_terminal(&slot, 2, failure());
        assert_eq!(slot.lock().unwrap().as_ref().unwrap().view, original);
        {
            let mut guard = slot.lock().unwrap();
            let current = guard.as_mut().unwrap();
            current.worker_active = false;
            current.cleanup = Some(PendingCleanup {
                owners: Vec::new(),
                terminal: failure(),
            });
        }
        finish_terminal(&slot, 2, Terminal::done());
        let guard = slot.lock().unwrap();
        let current = guard.as_ref().unwrap();
        assert_eq!(current.view, original);
        assert!(current.cleanup.is_some());
        assert!(current.nonce.is_some() && current.restore_key.is_some());
    }

    #[test]
    fn active_worker_refuses_other_terminal_transitions_until_actual_settlement() {
        let slot = running_slot();
        let original = slot.lock().unwrap().as_ref().unwrap().view.clone();
        assert!(start_worker(&slot, 2));
        assert!(!start_worker(&slot, 2));
        assert!(!start_worker(&slot, 1));
        slot.lock().unwrap().as_mut().unwrap().started =
            Instant::now() - HANDOFF_TTL - Duration::from_secs(1);
        assert!(!mark_cancelled(&slot, 2));
        mark_expired(&slot, 2);
        finish_terminal(&slot, 2, Terminal::done());
        assert_eq!(current(&slot).unwrap(), original);
        assert!(is_busy(&slot));
        assert!(begin(&slot, "rotate", None, None, None).is_err());
        settle(&slot, 1, Terminal::done());
        assert!(slot.lock().unwrap().as_ref().unwrap().worker_active);
        settle(&slot, 2, failure());
        assert_failure(&slot);
        #[cfg(not(windows))]
        assert!(!is_busy(&slot));

        #[cfg(windows)]
        let next = {
            let mut guard = slot.lock().unwrap();
            let settled = guard.as_ref().unwrap();
            assert!(!settled.worker_active);
            assert!(settled.cleanup.is_none());
            let generation = settled.generation.wrapping_add(1);
            *guard = Some(admitted_slot("restore", generation, None, None, None));
            generation
        };
        #[cfg(not(windows))]
        let next = begin(&slot, "restore", None, None, None)
            .unwrap()
            .generation;
        assert!(start_worker(&slot, next));
        let current_view = current(&slot).unwrap();
        settle(&slot, 2, Terminal::error("stale"));
        assert_eq!(current(&slot).unwrap(), current_view);
        assert!(slot.lock().unwrap().as_ref().unwrap().worker_active);
        settle(&slot, next, Terminal::done());
    }

    fn settle(slot: &SharedOperationSlot, generation: u64, terminal: Terminal) {
        #[cfg(windows)]
        finish_worker(slot, generation, terminal, Vec::new());
        #[cfg(not(windows))]
        finish_worker(slot, generation, terminal);
    }

    #[test]
    fn cancellation_reports_only_an_applied_current_transition() {
        let absent = new_slot();
        assert!(!mark_cancelled(&absent, 2));
        assert!(!start_worker(&absent, 2));
        let slot = running_slot();
        assert!(!mark_cancelled(&slot, 1));
        assert!(mark_cancelled(&slot, 2));
        assert!(!mark_cancelled(&slot, 2));
        assert!(!start_worker(&slot, 2));
        let guard = slot.lock().unwrap();
        let current = guard.as_ref().unwrap();
        assert_eq!(current.view.reason_code.as_deref(), Some("cancelled"));
        assert!(current.nonce.is_none() && current.restore_key.is_none());
    }
}

#[cfg(all(test, feature = "full-tests"))]
#[cfg(not(windows))]
mod worker_tests {
    use super::terminal_tests::{assert_failure, failure, running_slot};
    use super::*;

    fn wait_for_worker(handle: thread::JoinHandle<()>) {
        let (completed, observed) = std::sync::mpsc::channel();
        thread::spawn(move || completed.send(handle.join()).unwrap());
        observed
            .recv_timeout(Duration::from_secs(5))
            .expect("worker finished")
            .expect("worker did not panic");
    }

    fn assert_no_worker_starts(slot: SharedOperationSlot, generation: u64) {
        let (entered, observed) = std::sync::mpsc::channel();
        assert!(
            spawn_worker(slot, generation, move || {
                entered.send(()).unwrap();
                Terminal::done()
            })
            .is_none()
        );
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(5)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        );
    }

    #[test]
    fn ordinary_worker_preserves_complete_terminal() {
        let slot = running_slot();
        wait_for_worker(spawn_worker(slot.clone(), 2, failure).unwrap());
        assert_failure(&slot);
    }

    #[test]
    fn ordinary_worker_preserves_panic_failure() {
        let slot = running_slot();
        wait_for_worker(spawn_worker(slot.clone(), 2, || panic!("test worker panic")).unwrap());
        let guard = slot.lock().unwrap();
        let current = guard.as_ref().unwrap();
        assert_eq!(current.view.phase, "error");
        assert_eq!(current.view.reason_code.as_deref(), Some("failed"));
        assert!(current.view.detail.is_none() && current.view.guidance.is_none());
        assert!(current.view.portal_url.is_none());
        assert!(current.nonce.is_none() && current.restore_key.is_none());
    }

    #[test]
    fn ordinary_worker_holds_the_slot_until_its_closure_settles() {
        let slot = running_slot();
        let (entered, observed) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let handle = spawn_worker(slot.clone(), 2, move || {
            entered.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(5)).unwrap();
            failure()
        })
        .unwrap();
        observed.recv_timeout(Duration::from_secs(5)).unwrap();
        slot.lock().unwrap().as_mut().unwrap().started =
            Instant::now() - HANDOFF_TTL - Duration::from_secs(1);
        assert!(!mark_cancelled(&slot, 2));
        mark_expired(&slot, 2);
        finish_terminal(&slot, 2, Terminal::done());
        assert!(is_busy(&slot));
        assert_eq!(current(&slot).unwrap().phase, "restoring");
        assert!(begin(&slot, "teardown", None, None, None).is_err());
        assert_no_worker_starts(slot.clone(), 2);
        release.send(()).unwrap();
        wait_for_worker(handle);
        assert_failure(&slot);
        assert!(!is_busy(&slot));
    }

    #[test]
    fn refused_worker_admission_starts_no_closure() {
        let absent = new_slot();
        assert_no_worker_starts(absent, 2);
        let slot = running_slot();
        assert_no_worker_starts(slot.clone(), 1);
        assert!(mark_cancelled(&slot, 2));
        assert_no_worker_starts(slot.clone(), 2);
        assert_eq!(
            current(&slot).unwrap().reason_code.as_deref(),
            Some("cancelled")
        );
    }

    #[test]
    fn failed_thread_creation_settles_only_its_admitted_generation() {
        let slot = running_slot();
        FAIL_WORKER_START.with(|fail| fail.set(true));
        assert_no_worker_starts(slot.clone(), 2);
        let current = current(&slot).unwrap();
        assert_eq!(current.phase, "error");
        assert_eq!(current.reason_code.as_deref(), Some("failed"));
        assert!(current.portal_url.is_none());
        assert!(!slot.lock().unwrap().as_ref().unwrap().worker_active);
        assert!(!is_busy(&slot));
        let next = begin(&slot, "rotate", None, None, None).unwrap().generation;
        wait_for_worker(spawn_worker(slot.clone(), next, Terminal::done).unwrap());
        assert_eq!(super::current(&slot).unwrap().phase, "done");
    }
}

#[cfg(all(test, windows))]
mod cleanup_state_tests {
    use super::terminal_tests::admitted_slot;
    use super::*;

    #[test]
    fn active_worker_keeps_ownership_across_cancel_and_hosted_expiry() {
        let slot = new_slot();
        let generation = 1;
        *slot.lock().unwrap() = Some(admitted_slot(
            "restore_hosted",
            generation,
            Some("portal".into()),
            Some("nonce".into()),
            Some("key".into()),
        ));
        assert!(start_worker(&slot, generation));
        slot.lock().unwrap().as_mut().unwrap().started =
            Instant::now() - HANDOFF_TTL - Duration::from_secs(1);
        mark_cancelled(&slot, generation);
        mark_expired(&slot, generation);
        assert_eq!(observed_current(&slot).unwrap().unwrap().phase, "restoring");
        assert!(worker_or_cleanup_active(&slot, generation));
        assert!(begin(&slot, "rotate", None, None, None).is_err());
        finish_worker(
            &slot,
            generation,
            Terminal::error("original_failure"),
            Vec::new(),
        );
        let current = observed_current(&slot).unwrap().unwrap();
        assert_eq!(current.phase, "error");
        assert_eq!(current.reason_code.as_deref(), Some("original_failure"));
        let guard = slot.lock().unwrap();
        let current = guard.as_ref().unwrap();
        assert!(!current.worker_active);
        assert!(current.cleanup.is_none());
        assert!(
            current.nonce.is_none()
                && current.restore_key.is_none()
                && current.view.portal_url.is_none()
        );
    }

    #[test]
    fn stale_worker_and_recovery_cannot_clear_a_new_generation() {
        let slot = new_slot();
        let first = 1;
        *slot.lock().unwrap() = Some(admitted_slot("rotate", first, None, None, None));
        assert!(start_worker(&slot, first));
        finish_worker(&slot, first, Terminal::done(), Vec::new());
        let second = {
            let mut guard = slot.lock().unwrap();
            let settled = guard.as_ref().unwrap();
            assert_eq!(settled.view.phase, "done");
            assert!(!settled.worker_active);
            assert!(settled.cleanup.is_none());
            let generation = settled.generation.wrapping_add(1);
            *guard = Some(admitted_slot("restore", generation, None, None, None));
            generation
        };
        assert!(start_worker(&slot, second));
        finish_worker(&slot, first, Terminal::error("stale"), Vec::new());
        assert!(observed_generation(&slot, Some(first)).is_err());
        assert!(worker_or_cleanup_active(&slot, second));
        assert_eq!(observed_current(&slot).unwrap().unwrap().phase, "restoring");
        finish_worker(&slot, second, Terminal::done(), Vec::new());
    }

    #[test]
    fn status_observation_refuses_slot_contention_without_waiting() {
        let slot = new_slot();
        let guard = slot.lock().unwrap();
        assert!(observed_current(&slot).is_err());
        assert!(observed_generation(&slot, None).is_err());
        drop(guard);
        assert!(observed_current(&slot).unwrap().is_none());
    }
}
