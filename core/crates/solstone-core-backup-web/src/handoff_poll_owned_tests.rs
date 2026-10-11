// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use crate::operation::{self, Terminal};

fn begin_poll(deps: &crate::BackupWebDeps) -> u64 {
    let generation = operation::begin(
        &deps.operations,
        "restore_hosted",
        Some("portal".into()),
        Some("owned-nonce".into()),
        Some("key".into()),
    )
    .unwrap()
    .generation;
    crate::handoff_poll::spawn(deps.clone(), "owned-nonce".into(), generation);
    generation
}

fn full_terminal() -> Terminal {
    Terminal {
        phase: "degraded".into(),
        reason_code: Some("partial".into()),
        detail: Some("detail".into()),
        guidance: Some("guidance".into()),
        recording_failure: Some("recording".into()),
    }
}

#[tokio::test]
async fn late_poll_failures_preserve_callback_admission_and_complete_worker_terminal() {
    for response in [
        Err(HttpError::Unreachable),
        Err(HttpError::Other),
        Ok(HttpResponse {
            status: 410,
            headers: vec![],
            body: vec![],
        }),
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: b"{bad".to_vec(),
        }),
    ] {
        for worker_first in [false, true] {
            let root = crate::test_support::root("healthy");
            let hold = Hold::new();
            let http = Arc::new(
                HttpScript::default()
                    .with_poll_responses(vec![response.clone()])
                    .with_hold(hold.clone()),
            );
            let runner = Arc::new(ScriptRunner::with_outputs(vec![]));
            let deps = engine_deps(root.path().to_path_buf(), runner, http.clone(), None);
            held_fixture(
                deps.clone(),
                vec![hold.clone()],
                vec![http.clone()],
                vec![root],
                async {
                    let generation = begin_poll(&deps);
                    wait_started_or_terminal(&hold, &deps, generation)
                        .await
                        .unwrap();
                    let flag = http.poll_cancellations.lock().unwrap()[0].clone();
                    crate::handoff_poll::spawn(deps.clone(), "owned-nonce".into(), generation);
                    crate::handoff_poll::spawn(
                        deps.clone(),
                        "owned-nonce".into(),
                        generation.wrapping_sub(1),
                    );
                    assert_eq!(http.request_count(), 1);
                    assert!(!flag.is_cancelled());
                    let matched = operation::match_handoff(&deps.operations, "owned-nonce", None)
                        .unwrap_or_else(|_| panic!("callback nonce"));
                    assert_eq!(matched.generation, generation);
                    assert!(flag.is_cancelled());
                    if !worker_first {
                        hold.release().await;
                        wait_lease_released(&deps, &http).await;
                        assert!(operation::is_busy(&deps.operations));
                    }
                    operation::spawn_worker(deps.operations.clone(), generation, full_terminal)
                        .unwrap()
                        .join()
                        .unwrap();
                    if worker_first {
                        assert!(operation::is_busy(&deps.operations));
                        assert!(!operation::mark_cancelled(&deps.operations, generation));
                        hold.release().await;
                        wait_lease_released(&deps, &http).await;
                    }
                    let current = operation::current(&deps.operations).unwrap();
                    let expected = full_terminal();
                    assert_eq!(current.phase, expected.phase);
                    assert_eq!(current.reason_code, expected.reason_code);
                    assert_eq!(current.detail, expected.detail);
                    assert_eq!(current.guidance, expected.guidance);
                    assert_eq!(current.recording_failure, expected.recording_failure);
                    let next = operation::begin(&deps.operations, "restore", None, None, None)
                        .unwrap()
                        .generation;
                    operation::finish_handoff_poll(&deps.operations, generation);
                    operation::fail_handoff_poll(
                        &deps.operations,
                        generation,
                        "owned-nonce",
                        "expired",
                    );
                    assert_eq!(operation::generation_of(&deps.operations), Some(next));
                    assert!(operation::is_busy(&deps.operations));
                    assert!(operation::mark_cancelled(&deps.operations, next));
                },
            )
            .await;
        }
    }
}

#[tokio::test]
async fn stop_spawn_and_binding_failure_preserve_poll_and_prevent_unadmitted_effects() {
    for failure in ["cancel", "spawn", "binding"] {
        let root = crate::test_support::root("healthy");
        let journal_path = root.path().to_path_buf();
        let before = fs::read(journal_path.join("config/journal.json")).unwrap();
        let hold = Hold::new();
        let http = Arc::new(HttpScript::default().with_hold(hold.clone()));
        let runner = Arc::new(ScriptRunner::with_outputs(vec![]));
        let deps = engine_deps(journal_path.clone(), runner.clone(), http.clone(), None);
        held_fixture(
            deps.clone(),
            vec![hold.clone()],
            vec![http.clone()],
            vec![root],
            async {
                let generation = begin_poll(&deps);
                wait_started_or_terminal(&hold, &deps, generation)
                    .await
                    .unwrap();
                operation::match_handoff(&deps.operations, "owned-nonce", None)
                    .unwrap_or_else(|_| panic!("nonce"));
                match failure {
                    "cancel" => assert!(operation::mark_cancelled(&deps.operations, generation)),
                    "spawn" => operation::fail_next_worker_start(),
                    "binding" => fs::write(journal_path.join("backup"), b"occupied").unwrap(),
                    _ => unreachable!(),
                }
                crate::persist_and_consume_hosted(
                    &deps,
                    generation,
                    "restore_hosted".into(),
                    crate::test_support::hosted_binding(),
                    Some("key".into()),
                );
                let deadline = tokio::time::Instant::now() + FIXTURE_WAIT;
                while deps
                    .operations
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .worker_active
                {
                    fixture_tick(deadline).await.unwrap();
                }
                assert!(operation::is_busy(&deps.operations));
                assert_eq!(
                    operation::current(&deps.operations).unwrap().phase,
                    "cancelling"
                );
                assert!(solstone_core_backup::load_hosted_binding(&journal_path).is_none());
                assert_eq!(
                    fs::read(journal_path.join("config/journal.json")).unwrap(),
                    before
                );
                assert_eq!(http.request_count(), 1);
                assert!(runner.calls.lock().unwrap().is_empty());
                hold.release().await;
                let done = wait_terminal(&deps).await;
                assert_eq!(
                    done["operation"]["reason_code"],
                    if failure == "cancel" {
                        "cancelled"
                    } else {
                        "failed"
                    }
                );
                assert!(!deps.handoff_poll_lease.load(Ordering::Acquire));
            },
        )
        .await;
    }
}

#[tokio::test]
async fn settled_poll_cannot_finish_a_paused_successor_closure() {
    let root = crate::test_support::root("healthy");
    let poll_hold = Hold::new();
    let worker_hold = Hold::new();
    let http = Arc::new(HttpScript::default().with_hold(poll_hold.clone()));
    let deps = engine_deps(
        root.path().to_path_buf(),
        Arc::new(ScriptRunner::with_outputs(vec![])),
        http.clone(),
        None,
    );
    held_fixture(
        deps.clone(),
        vec![poll_hold.clone(), worker_hold.clone()],
        vec![http.clone()],
        vec![root],
        async {
            let generation = begin_poll(&deps);
            wait_started_or_terminal(&poll_hold, &deps, generation)
                .await
                .unwrap();
            operation::match_handoff(&deps.operations, "owned-nonce", None)
                .unwrap_or_else(|_| panic!("nonce"));
            let paused = worker_hold.clone();
            let worker = operation::spawn_worker(deps.operations.clone(), generation, move || {
                paused.arrive_and_wait().unwrap();
                full_terminal()
            })
            .unwrap();
            wait_started_or_terminal(&worker_hold, &deps, generation)
                .await
                .unwrap();
            poll_hold.release().await;
            wait_lease_released(&deps, &http).await;
            assert!(
                deps.operations
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .worker_active
            );
            assert!(operation::is_busy(&deps.operations));
            assert!(operation::begin(&deps.operations, "restore", None, None, None).is_err());
            worker_hold.release().await;
            worker.join().unwrap();
            assert_eq!(
                operation::current(&deps.operations).unwrap().detail,
                full_terminal().detail
            );
        },
    )
    .await;
}

struct UnsupportedHttp;
impl HttpTransport for UnsupportedHttp {
    fn execute(&self, _: &HttpRequest) -> Result<HttpResponse, HttpError> {
        panic!("controlled polling must never fall back")
    }
}

struct PanickedHttp;
impl HttpTransport for PanickedHttp {
    fn execute(&self, _: &HttpRequest) -> Result<HttpResponse, HttpError> {
        unreachable!()
    }
    fn execute_cancellable(
        &self,
        _: &HttpRequest,
        _: &solstone_core_backup::archive_admission::ArchiveCancellation,
    ) -> Result<HttpResponse, HttpError> {
        panic!("controlled poll fixture panic")
    }
}

#[tokio::test]
async fn failed_poll_start_panic_and_unsupported_transport_release_owned_generation() {
    for failure in ["spawn", "panic", "unsupported"] {
        let root = crate::test_support::root("healthy");
        let http = Arc::new(HttpScript::default());
        let mut deps = engine_deps(
            root.path().to_path_buf(),
            Arc::new(ScriptRunner::with_outputs(vec![])),
            http.clone(),
            None,
        );
        if failure == "panic" {
            deps.http = Arc::new(PanickedHttp);
        }
        if failure == "unsupported" {
            deps.http = Arc::new(UnsupportedHttp);
        }
        held_fixture(
            deps.clone(),
            vec![],
            vec![http.clone()],
            vec![root],
            async {
                if failure == "spawn" {
                    crate::handoff_poll::fail_next_poll_start();
                }
                let generation = begin_poll(&deps);
                let done = wait_terminal(&deps).await;
                assert_eq!(done["operation"]["reason_code"], "failed");
                assert_eq!(http.request_count(), 0);
                assert!(!deps.handoff_poll_lease.load(Ordering::Acquire));
                assert!(!operation::handoff_poll_active(&deps.operations));
                assert!(!operation::is_busy(&deps.operations));
                let next = operation::begin(&deps.operations, "restore", None, None, None)
                    .unwrap()
                    .generation;
                assert_ne!(next, generation);
                assert!(operation::mark_cancelled(&deps.operations, next));
            },
        )
        .await;
    }
}
