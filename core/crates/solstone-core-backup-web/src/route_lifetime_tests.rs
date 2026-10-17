// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#[cfg(not(windows))]
use std::collections::VecDeque;
#[cfg(not(windows))]
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(not(windows))]
use std::sync::Mutex;
#[cfg(not(windows))]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;
use solstone_core_backup_runtime::hosted_runtime::HttpError;
#[cfg(not(windows))]
use solstone_core_backup_runtime::rotate_recovery_key;
use solstone_core_backup_runtime::{HttpRequest, HttpResponse, HttpTransport};
#[cfg(not(windows))]
use solstone_core_backup_runtime::{ToolOutput, ToolRequest, ToolRunner};

use crate::BackupWebDeps;
use crate::operation;
#[cfg(not(windows))]
use crate::operation::Terminal;
use crate::route_lifetime::BackupRouteDrain;
#[cfg(not(windows))]
use crate::{lifetime_runner, services};

struct ReleaseGuard(Option<std::sync::mpsc::Sender<()>>);

// Use the actual platform worker registration; drain retains its native handle.
fn spawn_lifetime_test_worker(
    deps: &BackupWebDeps,
    generation: u64,
    work: impl FnOnce() -> operation::Terminal + Send + 'static,
) -> bool {
    #[cfg(not(windows))]
    {
        operation::spawn_worker(deps.operations.clone(), generation, work)
    }
    #[cfg(windows)]
    {
        crate::cleanup::spawn_worker(deps.clone(), generation, move |_| work());
        !deps.operations.lock().unwrap().threads.is_empty()
    }
}

#[test]
fn held_operation_mutex_cannot_extend_stop_or_drain_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let deps = BackupWebDeps::new(dir.path().to_path_buf());
    let lifetime = deps.lifetime();
    let (entered, ready) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let operations = Arc::clone(&deps.operations);
    let holder = std::thread::spawn(move || {
        let _guard = operations.lock().unwrap();
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut release_guard = ReleaseGuard(Some(release));
    let stop_start = Instant::now();
    lifetime.request_stop();
    assert!(stop_start.elapsed() < Duration::from_millis(200));
    let clone = lifetime.clone();
    let (result, results) = std::sync::mpsc::channel();
    let drain = std::thread::spawn(move || {
        let start = Instant::now();
        let outcome = clone.drain(
            crate::test_support::join_executor(),
            start + Duration::from_millis(25),
        );
        result.send((outcome, start.elapsed())).unwrap();
    });
    let bounded = results.recv_timeout(Duration::from_millis(250));
    release_guard.0.take().unwrap().send(()).unwrap();
    holder.join().unwrap();
    drain.join().unwrap();
    let (outcome, elapsed) =
        bounded.expect("drain must return while the actual mutex remains held");
    assert_eq!(outcome, BackupRouteDrain::Pending);
    assert!(elapsed < Duration::from_millis(250));
    assert_eq!(
        lifetime.drain(
            crate::test_support::join_executor(),
            Instant::now() + Duration::from_secs(5)
        ),
        BackupRouteDrain::Drained
    );
}

impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_refuses_later_begin_and_sync_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let deps = BackupWebDeps::new(dir.path().to_path_buf());
    let lifetime = deps.lifetime();

    let (entered_rx, release_tx) = crate::arm_generate_keys_pause();
    let mut guard = ReleaseGuard(Some(release_tx));

    let generate_task = tokio::spawn({
        let deps = deps.clone();
        async move { crate::generate_keys(deps).await }
    });

    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("entered generate_keys");
    assert_eq!(
        lifetime.drain(crate::test_support::join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );

    lifetime.request_stop();

    let second_gen = crate::generate_keys(deps.clone()).await;
    assert_eq!(second_gen.status(), axum::http::StatusCode::BAD_REQUEST);

    let Err(second_begin) = operation::begin(&deps.operations, "enable", None, None, None) else {
        panic!("expected begin to fail");
    };
    assert_eq!(second_begin.status(), axum::http::StatusCode::BAD_REQUEST);

    let _ = guard.0.take().unwrap().send(());
    let gen_res = generate_task.await.unwrap();
    assert_eq!(gen_res.status(), axum::http::StatusCode::OK);

    assert_eq!(
        lifetime.drain(
            crate::test_support::join_executor(),
            Instant::now() + Duration::from_secs(5)
        ),
        BackupRouteDrain::Drained
    );
}

#[cfg(not(windows))]
#[test]
fn paused_worker_drain_stays_pending_until_return() {
    let dir = tempfile::tempdir().unwrap();
    let deps = BackupWebDeps::new(dir.path().to_path_buf());
    let lifetime = deps.lifetime();

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut guard = ReleaseGuard(Some(release_tx));

    let began = operation::begin(&deps.operations, "enable", None, None, None).unwrap();
    assert!(operation::spawn_worker(
        deps.operations.clone(),
        began.generation,
        move || {
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
            Terminal::done()
        }
    ));

    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("worker entered");
    lifetime.request_stop();

    assert_eq!(
        lifetime.drain(crate::test_support::join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );
    assert!(deps.operations.lock().unwrap().cancellation.is_cancelled());

    let dir2 = tempfile::tempdir().unwrap();
    let deps2 = BackupWebDeps::new(dir2.path().to_path_buf());
    let began2 = operation::begin(&deps2.operations, "enable", None, None, None).unwrap();
    assert!(operation::spawn_worker(
        deps2.operations.clone(),
        began2.generation,
        Terminal::done
    ));
    let handle2 = deps2.operations.lock().unwrap().threads.pop().unwrap();
    handle2.join().unwrap();

    let _ = guard.0.take().unwrap().send(());
    assert_eq!(
        lifetime.drain(
            crate::test_support::join_executor(),
            Instant::now() + Duration::from_secs(5)
        ),
        BackupRouteDrain::Drained
    );

    assert!(deps.operations.lock().unwrap().cancellation.is_cancelled());
    assert!(!deps2.operations.lock().unwrap().cancellation.is_cancelled());
}

struct ApprovedPollHttp;

impl HttpTransport for ApprovedPollHttp {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        self.execute_cancellable(
            request,
            &solstone_core_backup::archive_admission::ArchiveCancellation::new(),
        )
    }

    fn execute_cancellable(
        &self,
        _request: &HttpRequest,
        _cancellation: &solstone_core_backup::archive_admission::ArchiveCancellation,
    ) -> Result<HttpResponse, HttpError> {
        let binding = crate::test_support::hosted_binding();
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: serde_json::to_vec(&json!({
                "status": "approved",
                "broker_endpoint": binding.broker_endpoint,
                "account_id": binding.account_id,
                "instance_id": binding.instance_id,
                "bucket": binding.bucket,
                "prefix": binding.prefix,
                "broker_token": binding.broker_token
            }))
            .unwrap(),
        })
    }
}

#[test]
fn paused_handoff_does_not_save_hosted_binding() {
    let dir = tempfile::tempdir().unwrap();
    let mut deps = BackupWebDeps::new(dir.path().to_path_buf());
    deps.http = Arc::new(ApprovedPollHttp);
    let lifetime = deps.lifetime();

    let (entered_rx, release_tx) = crate::handoff_poll::arm_apply_approved_pause();
    let mut guard = ReleaseGuard(Some(release_tx));

    let began = operation::begin(
        &deps.operations,
        "enable_hosted",
        Some("https://services.solstone.app".into()),
        Some("test-nonce".into()),
        None,
    )
    .unwrap();
    crate::handoff_poll::spawn(deps.clone(), "test-nonce".into(), began.generation);

    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("entered apply_approved pause");
    assert_eq!(
        lifetime.drain(crate::test_support::join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );
    assert!(solstone_core_backup::load_hosted_binding(dir.path()).is_none());

    lifetime.request_stop();
    let _ = guard.0.take().unwrap().send(());

    assert_eq!(
        lifetime.drain(
            crate::test_support::join_executor(),
            Instant::now() + Duration::from_secs(5)
        ),
        BackupRouteDrain::Drained
    );
    assert!(solstone_core_backup::load_hosted_binding(dir.path()).is_none());
    assert!(
        solstone_core_backup::get_backup_config(dir.path())
            .map(|c| c.get("enabled") != Some(&serde_json::Value::Bool(true)))
            .unwrap_or(true)
    );
}

#[cfg(not(windows))]
#[test]
fn start_stop_race_owns_or_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let deps = BackupWebDeps::new(dir.path().to_path_buf());
    let lifetime = deps.lifetime();
    let release = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicUsize::new(0));

    let mut starters = Vec::new();
    for _ in 0..8 {
        let deps = deps.clone();
        let release = release.clone();
        let entered = entered.clone();
        starters.push(std::thread::spawn(move || {
            if let Ok(began) = operation::begin(&deps.operations, "enable", None, None, None) {
                let _ =
                    operation::spawn_worker(deps.operations.clone(), began.generation, move || {
                        entered.fetch_add(1, Ordering::SeqCst);
                        while !release.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Terminal::done()
                    });
            }
        }));
    }

    std::thread::sleep(Duration::from_millis(10));
    lifetime.request_stop();

    for t in starters {
        t.join().unwrap();
    }

    release.store(true, Ordering::SeqCst);

    assert_eq!(
        lifetime.drain(
            crate::test_support::join_executor(),
            Instant::now() + Duration::from_secs(5)
        ),
        BackupRouteDrain::Drained
    );
    assert!(deps.operations.lock().unwrap().threads.is_empty());
}

#[cfg(not(windows))]
#[test]
fn two_clones_stay_pending() {
    let dir = tempfile::tempdir().unwrap();
    let deps = BackupWebDeps::new(dir.path().to_path_buf());
    let lifetime1 = deps.lifetime();
    let lifetime2 = lifetime1.clone();

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut guard = ReleaseGuard(Some(release_tx));

    let began = operation::begin(&deps.operations, "enable", None, None, None).unwrap();
    assert!(operation::spawn_worker(
        deps.operations.clone(),
        began.generation,
        move || {
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
            Terminal::done()
        }
    ));

    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("entered");

    lifetime1.request_stop();
    let l1 = lifetime1.clone();
    let t1 =
        std::thread::spawn(move || l1.drain(crate::test_support::join_executor(), Instant::now()));
    let l2 = lifetime2.clone();
    let t2 =
        std::thread::spawn(move || l2.drain(crate::test_support::join_executor(), Instant::now()));

    assert_eq!(t1.join().unwrap(), BackupRouteDrain::Pending);
    assert_eq!(t2.join().unwrap(), BackupRouteDrain::Pending);

    let _ = guard.0.take().unwrap().send(());

    assert_eq!(
        lifetime1.drain(
            crate::test_support::join_executor(),
            Instant::now() + Duration::from_secs(5)
        ),
        BackupRouteDrain::Drained
    );
}

#[test]
fn closed_drains_keep_actual_join_owned_while_worker_tls_is_still_running() {
    struct TlsGate {
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Drop for TlsGate {
        fn drop(&mut self) {
            self.entered.send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
    }
    thread_local! {
        static TLS_GATE: std::cell::RefCell<Option<TlsGate>> = const { std::cell::RefCell::new(None) };
    }
    let dir = tempfile::tempdir().unwrap();
    let deps = BackupWebDeps::new(dir.path().to_path_buf());
    let lifetime = deps.lifetime();
    let began = operation::begin(&deps.operations, "enable", None, None, None).unwrap();
    let (entered, ready) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let mut guard = ReleaseGuard(Some(release));
    assert!(spawn_lifetime_test_worker(
        &deps,
        began.generation,
        move || {
            TLS_GATE.with(|tls| {
                *tls.borrow_mut() = Some(TlsGate {
                    entered,
                    release: released,
                })
            });
            operation::Terminal::done()
        }
    ));
    ready
        .recv_timeout(Duration::from_secs(5))
        .expect("actual TLS destructor entered after worker main returned");
    lifetime.request_stop();
    // std is_finished reports the main-function boundary, not native TLS exit.
    assert!(deps.operations.lock().unwrap().threads[0].is_finished());
    let started = Instant::now();
    assert_eq!(
        lifetime.drain(
            crate::test_support::join_executor(),
            started + Duration::from_millis(25)
        ),
        BackupRouteDrain::Pending
    );
    assert!(started.elapsed() < Duration::from_millis(250));
    assert!(deps.operations.joining());
    assert!(deps.operations.lock().unwrap().threads.is_empty());
    assert_eq!(
        lifetime.clone().drain(
            crate::test_support::join_executor(),
            Instant::now() + Duration::from_millis(25)
        ),
        BackupRouteDrain::Pending
    );
    guard.0.take().unwrap().send(()).unwrap();
    assert_eq!(
        lifetime.drain(
            crate::test_support::join_executor(),
            Instant::now() + Duration::from_secs(5)
        ),
        BackupRouteDrain::Drained
    );
    assert!(!deps.operations.joining());
    assert!(deps.operations.lock().unwrap().join_batches.is_empty());
}

#[test]
fn queued_join_task_cancellation_retains_original_native_handles() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let (entered, ready) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let mut release_guard = ReleaseGuard(Some(release));
    let blocker = runtime.spawn_blocking(move || {
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let deps = BackupWebDeps::new(dir.path().to_path_buf());
    let lifetime = deps.lifetime();
    let began = operation::begin(&deps.operations, "enable", None, None, None).unwrap();
    assert!(spawn_lifetime_test_worker(
        &deps,
        began.generation,
        operation::Terminal::done
    ));
    lifetime.request_stop();
    assert_eq!(
        lifetime.drain(runtime.handle(), Instant::now() + Duration::from_millis(25)),
        BackupRouteDrain::Pending
    );
    {
        let state = deps.operations.lock().unwrap();
        let batch = state
            .join_batches
            .first()
            .expect("actual queued join batch");
        assert_eq!(batch.owners.lock().unwrap().len(), 1);
        batch.task.as_ref().unwrap().abort();
    }
    // Tokio observes queued cancellation when the occupied blocking pool next
    // reaches that job. Until then, the original handles must remain owned.
    assert_eq!(
        lifetime.drain(runtime.handle(), Instant::now()),
        BackupRouteDrain::Pending
    );
    release_guard.0.take().unwrap().send(()).unwrap();
    runtime.block_on(blocker).unwrap();
    assert_eq!(
        lifetime.drain(runtime.handle(), Instant::now() + Duration::from_secs(1)),
        BackupRouteDrain::Failure
    );
    assert_eq!(
        lifetime.clone().drain(runtime.handle(), Instant::now()),
        BackupRouteDrain::Failure
    );
    let owners = {
        let state = deps.operations.lock().unwrap();
        assert_eq!(state.failed_joins.len(), 1);
        state.failed_joins[0].owners.clone()
    };
    assert_eq!(
        owners.lock().unwrap().len(),
        1,
        "canceled queued work must not drop the native handle"
    );
    // Test-only cleanup consumes the retained actual worker. It grants no
    // production reset: this lifetime's failure remains irreversible.
    owners.lock().unwrap().pop().unwrap().join().unwrap();
    assert_eq!(
        lifetime.drain(runtime.handle(), Instant::now()),
        BackupRouteDrain::Failure
    );
}

#[cfg(not(windows))]
#[test]
fn drain_deadline_is_retryable() {
    let dir = tempfile::tempdir().unwrap();
    let deps = BackupWebDeps::new(dir.path().to_path_buf());
    let lifetime = deps.lifetime();

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut guard = ReleaseGuard(Some(release_tx));

    let began = operation::begin(&deps.operations, "enable", None, None, None).unwrap();
    assert!(operation::spawn_worker(
        deps.operations.clone(),
        began.generation,
        move || {
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
            Terminal::done()
        }
    ));

    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("entered");

    lifetime.request_stop();
    assert_eq!(
        lifetime.drain(crate::test_support::join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );
    assert_eq!(deps.operations.lock().unwrap().threads.len(), 1);

    let _ = guard.0.take().unwrap().send(());

    assert_eq!(
        lifetime.drain(
            crate::test_support::join_executor(),
            Instant::now() + Duration::from_secs(5)
        ),
        BackupRouteDrain::Drained
    );
    assert!(deps.operations.lock().unwrap().threads.is_empty());
}

#[cfg(not(windows))]
struct ScriptRunner {
    outputs: Mutex<VecDeque<ToolOutput>>,
}

#[cfg(not(windows))]
impl ToolRunner for ScriptRunner {
    fn run(&self, _request: &ToolRequest<'_>) -> std::io::Result<ToolOutput> {
        let mut guard = self.outputs.lock().unwrap();
        if let Some(output) = guard.pop_front() {
            Ok(output)
        } else {
            Ok(ToolOutput {
                returncode: 0,
                stdout: vec![],
                stderr: vec![],
            })
        }
    }

    fn run_cancellable(
        &self,
        request: &ToolRequest<'_>,
        _cancellation: &solstone_core_backup::archive_admission::ArchiveCancellation,
    ) -> std::io::Result<ToolOutput> {
        self.run(request)
    }
}

#[cfg(not(windows))]
#[test]
fn rotation_after_remove_key_holds_drain_until_local_publish() {
    let journal = tempfile::tempdir().unwrap();
    let destination = solstone_core_backup::Destination {
        repository: "repo".into(),
        backend: "s3".into(),
        credentials: serde_json::json!({"access_key_id":"access","secret_access_key":"secret"})
            .as_object()
            .unwrap()
            .clone(),
    };
    solstone_core_backup::set_destination(journal.path(), &destination).unwrap();
    solstone_core_backup::generate_and_store_keys(journal.path()).unwrap();
    let prior_keys = solstone_core_backup::get_keys(journal.path())
        .unwrap()
        .unwrap();

    let runner = Arc::new(ScriptRunner {
        outputs: Mutex::new(VecDeque::from(vec![
            ToolOutput {
                returncode: 0,
                stdout: b"[{\"id\":\"old\",\"current\":true}]".to_vec(),
                stderr: vec![],
            },
            ToolOutput {
                returncode: 0,
                stdout: vec![],
                stderr: vec![],
            },
            ToolOutput {
                returncode: 0,
                stdout: vec![],
                stderr: vec![],
            },
            ToolOutput {
                returncode: 0,
                stdout: vec![],
                stderr: vec![],
            },
        ])),
    });

    let mut deps = BackupWebDeps::new(journal.path().to_path_buf());
    deps.runner = runner;
    let lifetime = deps.lifetime();

    let (entered_rx, release_tx) = solstone_core_backup_runtime::rotation::arm_rotation_pause();
    let mut guard = ReleaseGuard(Some(release_tx));

    let began = operation::begin(&deps.operations, "rotate", None, None, None).unwrap();
    let worker_deps = deps.clone();
    assert!(operation::spawn_worker(
        deps.operations.clone(),
        began.generation,
        move || {
            let tools = solstone_core_backup_runtime::ResolvedTools {
                restic: (PathBuf::from("/fixture/bin/restic")).into(),
                rclone_path: None,
            };
            let runner = lifetime_runner(&worker_deps);
            let result = rotate_recovery_key(
                &worker_deps.journal_root,
                &services(&worker_deps, &runner, &tools),
            );
            if result.status == "ok" {
                Terminal::done()
            } else {
                Terminal::error("failed")
            }
        }
    ));

    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("entered rotation pause");

    let current_keys = solstone_core_backup::get_keys(journal.path())
        .unwrap()
        .unwrap();
    assert_ne!(current_keys.recovery_key, prior_keys.recovery_key);
    let cfg = solstone_core_backup::get_backup_config(journal.path()).unwrap();
    assert!(cfg.get("pending_key_revocation").is_some());

    lifetime.request_stop();
    assert_eq!(
        lifetime.drain(crate::test_support::join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );

    let _ = guard.0.take().unwrap().send(());

    assert_eq!(
        lifetime.drain(
            crate::test_support::join_executor(),
            Instant::now() + Duration::from_secs(5)
        ),
        BackupRouteDrain::Drained
    );

    let final_keys = solstone_core_backup::get_keys(journal.path())
        .unwrap()
        .unwrap();
    assert_eq!(final_keys.recovery_key, current_keys.recovery_key);
    assert_ne!(final_keys.recovery_key, prior_keys.recovery_key);
    let cfg_after = solstone_core_backup::get_backup_config(journal.path()).unwrap();
    assert!(cfg_after.get("pending_key_revocation").is_none());
}

#[cfg(not(windows))]
fn scripted_output(code: i32, stdout: &str) -> ToolOutput {
    ToolOutput {
        returncode: code,
        stdout: stdout.as_bytes().to_vec(),
        stderr: vec![],
    }
}

#[cfg(not(windows))]
struct BlockingRemoveRunner {
    outputs: Mutex<VecDeque<ToolOutput>>,
    entered: Mutex<std::sync::mpsc::Sender<()>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    remove_runs: AtomicUsize,
    interrupted: AtomicBool,
    cancellation_supplied: AtomicBool,
}

#[cfg(not(windows))]
impl BlockingRemoveRunner {
    fn take_output(&self) -> std::io::Result<ToolOutput> {
        Ok(self
            .outputs
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted output"))
    }
}

#[cfg(not(windows))]
impl ToolRunner for BlockingRemoveRunner {
    fn run(&self, _request: &ToolRequest<'_>) -> std::io::Result<ToolOutput> {
        self.take_output()
    }

    fn run_cancellable(
        &self,
        request: &ToolRequest<'_>,
        cancellation: &solstone_core_backup::archive_admission::ArchiveCancellation,
    ) -> std::io::Result<ToolOutput> {
        self.cancellation_supplied.store(true, Ordering::SeqCst);
        if cancellation.is_cancelled() {
            self.interrupted.store(true, Ordering::SeqCst);
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "backup tool cancelled",
            ));
        }
        if request.argv.iter().any(|arg| arg.as_os_str() == "remove") {
            self.remove_runs.fetch_add(1, Ordering::SeqCst);
            let _ = self.entered.lock().unwrap().send(());
            let _ = self.release.lock().unwrap().recv();
        }
        self.take_output()
    }
}

#[cfg(not(windows))]
fn rotation_journal() -> (tempfile::TempDir, String) {
    let journal = tempfile::tempdir().unwrap();
    let destination = solstone_core_backup::Destination {
        repository: "repo".into(),
        backend: "s3".into(),
        credentials: serde_json::json!({"access_key_id":"access","secret_access_key":"secret"})
            .as_object()
            .unwrap()
            .clone(),
    };
    solstone_core_backup::set_destination(journal.path(), &destination).unwrap();
    solstone_core_backup::generate_and_store_keys(journal.path()).unwrap();
    let prior = solstone_core_backup::get_keys(journal.path())
        .unwrap()
        .unwrap()
        .recovery_key;
    (journal, prior)
}

#[cfg(not(windows))]
fn store_rotation(
    deps: &BackupWebDeps,
    stored: &Mutex<Option<solstone_core_backup_runtime::RotationResult>>,
) {
    let tools = solstone_core_backup_runtime::ResolvedTools {
        restic: (PathBuf::from("/fixture/bin/restic")).into(),
        rclone_path: None,
    };
    let runner = lifetime_runner(deps);
    let result = rotate_recovery_key(&deps.journal_root, &services(deps, &runner, &tools));
    *stored.lock().unwrap() = Some(result);
}

#[cfg(not(windows))]
#[test]
fn rotation_remove_success_after_stop_publishes_on_worker() {
    let (journal, prior) = rotation_journal();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let runner = Arc::new(BlockingRemoveRunner {
        outputs: Mutex::new(VecDeque::from([
            scripted_output(0, "[{\"id\":\"old\",\"current\":true}]"),
            scripted_output(0, ""),
            scripted_output(0, ""),
            scripted_output(0, ""),
        ])),
        entered: Mutex::new(entered_tx),
        release: Mutex::new(release_rx),
        remove_runs: AtomicUsize::new(0),
        interrupted: AtomicBool::new(false),
        cancellation_supplied: AtomicBool::new(false),
    });
    let mut deps = BackupWebDeps::new(journal.path().to_path_buf());
    deps.runner = runner.clone();
    let lifetime = deps.lifetime();
    let stored = Arc::new(Mutex::new(None));
    let stored_worker = Arc::clone(&stored);
    let began = operation::begin(&deps.operations, "rotate", None, None, None).unwrap();
    let worker_deps = deps.clone();
    assert!(operation::spawn_worker(
        deps.operations.clone(),
        began.generation,
        move || {
            store_rotation(&worker_deps, &stored_worker);
            if stored_worker
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|result| result.status == "ok")
            {
                Terminal::done()
            } else {
                Terminal::error("failed")
            }
        }
    ));

    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("entered remove");
    let current = solstone_core_backup::get_keys(journal.path())
        .unwrap()
        .unwrap();
    assert_ne!(current.recovery_key, prior);
    let cfg = solstone_core_backup::get_backup_config(journal.path()).unwrap();
    assert!(cfg.get("pending_key_revocation").is_some());

    lifetime.request_stop();
    assert!(deps.operations.lock().unwrap().cancellation.is_cancelled());
    assert_eq!(
        lifetime.drain(crate::test_support::join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );

    let _ = release_tx.send(());
    assert_eq!(
        lifetime.drain(
            crate::test_support::join_executor(),
            Instant::now() + Duration::from_secs(5)
        ),
        BackupRouteDrain::Drained
    );
    let final_keys = solstone_core_backup::get_keys(journal.path())
        .unwrap()
        .unwrap();
    assert_eq!(final_keys.recovery_key, current.recovery_key);
    assert_ne!(final_keys.recovery_key, prior);
    let cfg_after = solstone_core_backup::get_backup_config(journal.path()).unwrap();
    assert!(cfg_after.get("pending_key_revocation").is_none());

    let result = stored.lock().unwrap().take().expect("rotation result");
    assert_eq!(result.status, "ok");
    assert!(result.recovery_key.is_some());
    assert_eq!(
        result.recovery_key.as_deref(),
        Some(final_keys.recovery_key.as_str())
    );
    assert!(runner.cancellation_supplied.load(Ordering::SeqCst));
}

#[cfg(not(windows))]
#[test]
fn rotation_remove_timeout_124_after_stop_retains_candidate_and_pending() {
    let (journal, prior) = rotation_journal();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let runner = Arc::new(BlockingRemoveRunner {
        outputs: Mutex::new(VecDeque::from([
            scripted_output(0, "[{\"id\":\"old\",\"current\":true}]"),
            scripted_output(0, ""),
            scripted_output(0, ""),
            scripted_output(124, ""),
        ])),
        entered: Mutex::new(entered_tx),
        release: Mutex::new(release_rx),
        remove_runs: AtomicUsize::new(0),
        interrupted: AtomicBool::new(false),
        cancellation_supplied: AtomicBool::new(false),
    });
    let mut deps = BackupWebDeps::new(journal.path().to_path_buf());
    deps.runner = runner.clone();
    let lifetime = deps.lifetime();
    let clone_life = lifetime.clone();
    let stored = Arc::new(Mutex::new(None));
    let stored_worker = Arc::clone(&stored);
    let began = operation::begin(&deps.operations, "rotate", None, None, None).unwrap();
    let worker_deps = deps.clone();
    assert!(operation::spawn_worker(
        deps.operations.clone(),
        began.generation,
        move || {
            store_rotation(&worker_deps, &stored_worker);
            if stored_worker
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|result| result.status == "ok")
            {
                Terminal::done()
            } else {
                Terminal::error("failed")
            }
        }
    ));

    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("entered remove");
    let current = solstone_core_backup::get_keys(journal.path())
        .unwrap()
        .unwrap();
    assert_ne!(current.recovery_key, prior);
    let cfg = solstone_core_backup::get_backup_config(journal.path()).unwrap();
    assert!(cfg.get("pending_key_revocation").is_some());

    lifetime.request_stop();
    assert!(deps.operations.lock().unwrap().cancellation.is_cancelled());
    // Drain stays Pending while the wait holds, including after request_stop and for a second clone
    assert_eq!(
        lifetime.drain(crate::test_support::join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );
    assert_eq!(
        clone_life.drain(crate::test_support::join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );
    // and a drain(Instant::now()) deadline is retryable
    assert_eq!(
        lifetime.drain(crate::test_support::join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );

    let _ = release_tx.send(());
    assert_eq!(
        lifetime.drain(
            crate::test_support::join_executor(),
            Instant::now() + Duration::from_secs(5)
        ),
        BackupRouteDrain::Drained
    );
    let final_keys = solstone_core_backup::get_keys(journal.path())
        .unwrap()
        .unwrap();
    // After release, 124 keeps the candidate and the pending field
    assert_eq!(final_keys.recovery_key, current.recovery_key);
    let cfg_after = solstone_core_backup::get_backup_config(journal.path()).unwrap();
    assert!(cfg_after.get("pending_key_revocation").is_some());

    let result = stored.lock().unwrap().take().expect("rotation result");
    assert_eq!(result.status, "error");
    assert_eq!(result.reason_code.as_deref(), Some("timeout"));
    assert!(runner.cancellation_supplied.load(Ordering::SeqCst));
}

#[cfg(not(windows))]
#[test]
fn rotation_cancels_before_remove_and_drops_candidate() {
    let (journal, prior) = rotation_journal();
    let (entered_tx, _entered_rx) = std::sync::mpsc::channel();
    let (_release_tx, release_rx) = std::sync::mpsc::channel();
    let runner = Arc::new(BlockingRemoveRunner {
        outputs: Mutex::new(VecDeque::from([
            scripted_output(0, "[{\"id\":\"old\",\"current\":true}]"),
            scripted_output(0, ""),
            scripted_output(0, ""),
            scripted_output(0, ""),
        ])),
        entered: Mutex::new(entered_tx),
        release: Mutex::new(release_rx),
        remove_runs: AtomicUsize::new(0),
        interrupted: AtomicBool::new(false),
        cancellation_supplied: AtomicBool::new(false),
    });
    let mut deps = BackupWebDeps::new(journal.path().to_path_buf());
    deps.runner = Arc::clone(&runner) as Arc<dyn ToolRunner + Send + Sync>;
    let lifetime = deps.lifetime();
    let began = operation::begin(&deps.operations, "rotate", None, None, None).unwrap();
    lifetime.request_stop();
    assert!(!operation::spawn_worker(
        deps.operations.clone(),
        began.generation,
        || panic!("closed lifetime must not start remove")
    ));

    let stored = Arc::new(Mutex::new(None));
    let stored_worker = Arc::clone(&stored);
    let worker_deps = deps.clone();
    let worker = std::thread::Builder::new()
        .spawn(move || store_rotation(&worker_deps, &stored_worker))
        .unwrap();
    worker.join().unwrap();

    assert!(runner.interrupted.load(Ordering::SeqCst));
    assert_eq!(runner.remove_runs.load(Ordering::SeqCst), 0);
    let keys = solstone_core_backup::get_keys(journal.path())
        .unwrap()
        .unwrap();
    assert_eq!(keys.recovery_key, prior);
    let result = stored.lock().unwrap().take().expect("rotation result");
    assert!(result.recovery_key.is_none());
    assert_eq!(
        lifetime.drain(crate::test_support::join_executor(), Instant::now()),
        BackupRouteDrain::Drained
    );
}

#[test]
fn archive_reservation_holds_drain_until_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let deps = BackupWebDeps::new(dir.path().to_path_buf());
    let lifetime = deps.lifetime();
    let root = Arc::new(solstone_core_journal_io::JournalRoot::open(dir.path()).unwrap());
    let begun = crate::archive_worker::begin_owned(
        &deps.operations,
        "offload_restore",
        root,
        deps.clock.clone(),
    )
    .unwrap();
    lifetime.request_stop();
    assert_eq!(
        lifetime.drain(crate::test_support::join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );
    assert_eq!(
        deps.lifetime()
            .drain(crate::test_support::join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );
    begun.cancel_before_preparation(&deps.operations);
    assert_eq!(
        lifetime.drain(crate::test_support::join_executor(), Instant::now()),
        BackupRouteDrain::Drained
    );
}

#[test]
fn maintenance_permit_survives_dropped_caller() {
    let race_dir = tempfile::tempdir().unwrap();
    let race_deps = BackupWebDeps::new(race_dir.path().to_path_buf());
    let left = race_deps.lifetime();
    let right = race_deps.lifetime();
    let stopper = std::thread::spawn(move || left.request_stop());
    let admitter = std::thread::spawn(move || right.admit_maintenance());
    stopper.join().unwrap();
    if let Ok(permit) = admitter.join().unwrap() {
        assert!(permit.cancellation().is_cancelled());
    }

    let dir = tempfile::tempdir().unwrap();
    let deps = BackupWebDeps::new(dir.path().to_path_buf());
    let lifetime = deps.lifetime();
    let permit = lifetime.admit_maintenance().expect("maintenance admitted");
    let token = permit.cancellation().clone();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker = std::thread::Builder::new()
        .spawn(move || {
            let _held = permit;
            let _ = release_rx.recv();
        })
        .unwrap();
    let slot = Arc::clone(&deps.operations);
    let life_a = deps.lifetime();
    let life_b = deps.lifetime();
    drop(deps);
    drop(lifetime);
    assert!(slot.lock().unwrap().writes_in_flight > 0);
    assert_eq!(
        life_a.drain(crate::test_support::join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );
    assert_eq!(
        life_b.drain(crate::test_support::join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );
    life_a.request_stop();
    assert!(token.is_cancelled());
    assert_eq!(
        life_a.drain(crate::test_support::join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );
    let _ = release_tx.send(());
    worker.join().unwrap();
    assert_eq!(
        life_a.drain(
            crate::test_support::join_executor(),
            Instant::now() + Duration::from_secs(5)
        ),
        BackupRouteDrain::Drained
    );
    assert!(token.is_cancelled());
    let deps2 = BackupWebDeps::new(dir.path().to_path_buf());
    let second = deps2.operations.lock().unwrap().cancellation.clone();
    assert!(!second.is_cancelled());
    assert!(operation::begin(&deps2.operations, "enable", None, None, None).is_ok());
    assert!(token.is_cancelled());
    assert!(!second.is_cancelled());
}

#[cfg(all(test, feature = "full-tests"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirm_key_paused_loses_to_rotation() {
    let journal = tempfile::tempdir().unwrap();
    let destination = solstone_core_backup::Destination {
        repository: "repo".into(),
        backend: "s3".into(),
        credentials: serde_json::json!({"access_key_id":"access","secret_access_key":"secret"})
            .as_object()
            .unwrap()
            .clone(),
    };
    solstone_core_backup::set_destination(journal.path(), &destination).unwrap();
    solstone_core_backup::generate_and_store_keys(journal.path()).unwrap();
    let initial_keys = solstone_core_backup::get_keys(journal.path())
        .unwrap()
        .unwrap();

    let deps = BackupWebDeps::new(journal.path().to_path_buf());
    let (entered_rx, release_tx) = crate::arm_confirm_key_pause(journal.path());

    let body = serde_json::json!({
        "recovery_key": initial_keys.recovery_key
    });
    let body_bytes = axum::body::Bytes::from(serde_json::to_vec(&body).unwrap());

    let confirm_deps = deps.clone();
    let handle = tokio::spawn(async move { crate::confirm_key(confirm_deps, body_bytes).await });

    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("entered confirm pause");

    let candidate = solstone_core_backup::generate_recovery_key().unwrap();
    solstone_core_journal_config_write::conditional_update_journal_config_at_root(
        &Arc::new(solstone_core_journal_io::JournalRoot::open(journal.path()).unwrap()),
        &solstone_core_journal_config::read_journal_config(journal.path())
            .unwrap()
            .sha256
            .unwrap(),
        |cfg| {
            let mut m = cfg.clone();
            m.get_mut("backup")
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert(
                    "recovery_key".into(),
                    serde_json::Value::String(candidate.clone()),
                );
            Some(m)
        },
        solstone_core_journal_io::LockOptions::default(),
    )
    .unwrap();

    let _ = release_tx.send(());
    let response = handle.await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body_json["reason_code"], "recovery_key_mismatch");
    assert_eq!(body_json["error"], "that didn't match your recovery key.");

    let cfg = solstone_core_backup::get_backup_config(journal.path()).unwrap();
    assert_eq!(cfg["confirmed_recovery_key"], false);
}

#[cfg(all(test, feature = "full-tests"))]
#[tokio::test]
async fn confirm_key_confirms_current_candidate_with_pending_intent() {
    let journal = tempfile::tempdir().unwrap();
    let destination = solstone_core_backup::Destination {
        repository: "repo".into(),
        backend: "s3".into(),
        credentials: serde_json::json!({"access_key_id":"access","secret_access_key":"secret"})
            .as_object()
            .unwrap()
            .clone(),
    };
    solstone_core_backup::set_destination(journal.path(), &destination).unwrap();
    solstone_core_backup::generate_and_store_keys(journal.path()).unwrap();

    let candidate = solstone_core_backup::generate_recovery_key().unwrap();
    let keys = solstone_core_backup::get_keys(journal.path())
        .unwrap()
        .unwrap();
    let daily_key = &keys.daily_key;
    let candidate_binding = solstone_core_backup::candidate_binding(daily_key, &candidate).unwrap();
    let credential_binding =
        solstone_core_backup::credential_binding(daily_key, &destination.credentials).unwrap();
    let pending = solstone_core_backup::PendingKeyRevocation {
        original_key_id: "old_id".into(),
        candidate_binding,
        execution: solstone_core_backup::PendingExecution {
            mode: "byo".into(),
            repository: destination.repository.clone(),
            backend: destination.backend.clone(),
            credential_binding,
        },
        setup_fingerprint: solstone_core_backup::backup_setup_fingerprint(
            &solstone_core_backup::get_backup_config(journal.path()).unwrap(),
            None,
        ),
    };
    let root = Arc::new(solstone_core_journal_io::JournalRoot::open(journal.path()).unwrap());
    let read = solstone_core_journal_config::read_journal_config(journal.path()).unwrap();
    solstone_core_backup::stage_pending_key_revocation_at_root(
        &root,
        &read.sha256.unwrap(),
        &candidate,
        &pending,
        None,
    )
    .unwrap();

    let deps = BackupWebDeps::new(journal.path().to_path_buf());
    let body = serde_json::json!({
        "recovery_key": candidate
    });
    let body_bytes = axum::body::Bytes::from(serde_json::to_vec(&body).unwrap());
    let response = crate::confirm_key(deps, body_bytes).await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let cfg = solstone_core_backup::get_backup_config(journal.path()).unwrap();
    assert_eq!(cfg["confirmed_recovery_key"], true);
    assert!(cfg.get("pending_key_revocation").is_some());
}

#[cfg(all(test, feature = "full-tests"))]
#[tokio::test]
async fn status_shows_pending_revocation_without_secrets_and_reveal_shows_candidate() {
    let journal = tempfile::tempdir().unwrap();
    let destination = solstone_core_backup::Destination {
        repository: "repo".into(),
        backend: "s3".into(),
        credentials: serde_json::json!({"access_key_id":"access","secret_access_key":"secret"})
            .as_object()
            .unwrap()
            .clone(),
    };
    solstone_core_backup::set_destination(journal.path(), &destination).unwrap();
    solstone_core_backup::generate_and_store_keys(journal.path()).unwrap();

    let candidate = solstone_core_backup::generate_recovery_key().unwrap();
    let keys = solstone_core_backup::get_keys(journal.path())
        .unwrap()
        .unwrap();
    let candidate_binding =
        solstone_core_backup::candidate_binding(&keys.daily_key, &candidate).unwrap();
    let credential_binding =
        solstone_core_backup::credential_binding(&keys.daily_key, &destination.credentials)
            .unwrap();
    let pending = solstone_core_backup::PendingKeyRevocation {
        original_key_id: "orig_id".into(),
        candidate_binding,
        execution: solstone_core_backup::PendingExecution {
            mode: "byo".into(),
            repository: destination.repository.clone(),
            backend: destination.backend.clone(),
            credential_binding,
        },
        setup_fingerprint: solstone_core_backup::backup_setup_fingerprint(
            &solstone_core_backup::get_backup_config(journal.path()).unwrap(),
            None,
        ),
    };
    let root = Arc::new(solstone_core_journal_io::JournalRoot::open(journal.path()).unwrap());
    let read = solstone_core_journal_config::read_journal_config(journal.path()).unwrap();
    solstone_core_backup::stage_pending_key_revocation_at_root(
        &root,
        &read.sha256.unwrap(),
        &candidate,
        &pending,
        None,
    )
    .unwrap();

    let deps = BackupWebDeps::new(journal.path().to_path_buf());
    let status_val = crate::status::status(&deps.journal_root, &deps.operations).unwrap();
    assert_eq!(status_val["recovery_key_revocation_pending"], true);
    assert_eq!(status_val["recovery_key_revocation_unresolved"], true);

    let status_json_str = serde_json::to_string(&status_val).unwrap();
    assert!(!status_json_str.contains(&candidate));

    let reveal_resp = crate::reveal_key(deps).await;
    assert_eq!(reveal_resp.status(), axum::http::StatusCode::OK);
    let reveal_body = axum::body::to_bytes(reveal_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let reveal_json: serde_json::Value = serde_json::from_slice(&reveal_body).unwrap();
    let expected_display = solstone_core_backup::format_recovery_key_display(&candidate).unwrap();
    assert_eq!(reveal_json["recovery_key_display"], expected_display);
}

#[tokio::test]
async fn pending_restore_public_routes_and_late_handoff_preserve_configuration_and_binding() {
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use solstone_core_backup_runtime::restore::RESTORE_REASON_RECOVERY_KEY_REVOCATION_PENDING;
    use tower::ServiceExt;
    struct NoRestoreIo(std::sync::atomic::AtomicUsize);
    impl solstone_core_backup_runtime::ToolRunner for NoRestoreIo {
        fn run(
            &self,
            _: &solstone_core_backup_runtime::ToolRequest<'_>,
        ) -> std::io::Result<solstone_core_backup_runtime::ToolOutput> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("pending destination launched a helper")
        }
    }
    impl solstone_core_backup_runtime::HttpTransport for NoRestoreIo {
        fn execute(
            &self,
            _: &solstone_core_backup_runtime::HttpRequest,
        ) -> Result<
            solstone_core_backup_runtime::HttpResponse,
            solstone_core_backup_runtime::hosted_runtime::HttpError,
        > {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("pending destination contacted a broker")
        }
    }
    for pending in [
        serde_json::json!(null),
        serde_json::json!({"future":true}),
        serde_json::json!({"original_key_id":"original","candidate_binding":"a".repeat(64),"execution":{"mode":"byo","repository":"repo","backend":"s3","credential_binding":"b".repeat(64)},"setup_fingerprint":"c".repeat(64)}),
    ] {
        for binding_case in ["absent", "corrupt", "bound"] {
            let root = crate::test_support::root("healthy");
            let binding_path = root.path().join("backup/hosted/binding.json");
            if binding_case == "corrupt" {
                std::fs::create_dir_all(binding_path.parent().unwrap()).unwrap();
                std::fs::write(&binding_path, b"original corrupt binding").unwrap();
            } else if binding_case == "bound" {
                solstone_core_backup::save_hosted_binding(
                    root.path(),
                    &crate::test_support::hosted_binding(),
                )
                .unwrap();
            }
            let original_binding = std::fs::read(&binding_path).ok();
            let path = root.path().join("config/journal.json");
            let mut config: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            config["backup"]["pending_key_revocation"] = pending.clone();
            let before = serde_json::to_vec_pretty(&config).unwrap();
            std::fs::write(&path, &before).unwrap();
            let mut deps = BackupWebDeps::production(
                root.path().to_path_buf(),
                crate::measurement::with_geometry(crate::measurement::DeviceGeometry {
                    free_bytes: None,
                    total_bytes: None,
                }),
            );
            let io = Arc::new(NoRestoreIo(std::sync::atomic::AtomicUsize::new(0)));
            deps.runner = io.clone();
            deps.http = io.clone();
            deps.executable = root.path().join("missing/bin/solstone");
            for route in [
                "restore",
                "restore-hosted",
                "restore-hosted/prepare",
                "restore-hosted/key",
                "restore-hosted/arm",
                "restore-hosted/activate",
            ] {
                let response = crate::routes_with_deps(deps.clone())
                    .oneshot(
                        Request::post(format!("/app/backup/{route}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    axum::http::StatusCode::CONFLICT,
                    "{route}"
                );
                let body: serde_json::Value =
                    serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap())
                        .unwrap();
                assert_eq!(
                    body["reason_code"], RESTORE_REASON_RECOVERY_KEY_REVOCATION_PENDING,
                    "{route}"
                );
                assert_eq!(std::fs::read(&path).unwrap(), before, "{route}");
                assert_eq!(
                    std::fs::read(&binding_path).ok(),
                    original_binding,
                    "{route}"
                );
                assert!(deps.restore_prepare.lock().unwrap().is_none());
            }
            let terminal = crate::bound_restore_hosted_inner(
                &deps,
                crate::test_support::hosted_binding(),
                crate::test_support::RECOVERY_KEY,
            );
            assert_eq!(
                terminal.reason_code.as_deref(),
                Some(RESTORE_REASON_RECOVERY_KEY_REVOCATION_PENDING)
            );
            let terminal = crate::publish_restore_error(&deps, "restic_unavailable");
            assert_eq!(
                terminal.reason_code.as_deref(),
                Some(RESTORE_REASON_RECOVERY_KEY_REVOCATION_PENDING)
            );
            let terminal = crate::consume_hosted(
                &deps,
                "restore_hosted".into(),
                crate::test_support::hosted_binding(),
                Some(crate::test_support::RECOVERY_KEY.into()),
            );
            assert_eq!(
                terminal.reason_code.as_deref(),
                Some(RESTORE_REASON_RECOVERY_KEY_REVOCATION_PENDING)
            );
            // An already-approved handoff may arrive after the destination becomes pending.
            let began = operation::begin(
                &deps.operations,
                "restore_hosted",
                None,
                None,
                Some(crate::test_support::RECOVERY_KEY.into()),
            )
            .unwrap();
            crate::persist_and_consume_hosted(
                &deps,
                began.generation,
                "restore_hosted".into(),
                crate::test_support::hosted_binding(),
                Some(crate::test_support::RECOVERY_KEY.into()),
            );
            let lifetime = deps.lifetime();
            assert_eq!(
                lifetime.drain(
                    crate::test_support::join_executor(),
                    Instant::now() + Duration::from_secs(5)
                ),
                BackupRouteDrain::Pending
            );
            assert_eq!(
                deps.operations
                    .lock()
                    .unwrap()
                    .current
                    .as_ref()
                    .unwrap()
                    .view
                    .reason_code
                    .as_deref(),
                Some(RESTORE_REASON_RECOVERY_KEY_REVOCATION_PENDING)
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert_eq!(std::fs::read(&binding_path).ok(), original_binding);
            assert_eq!(io.0.load(std::sync::atomic::Ordering::SeqCst), 0);
            lifetime.request_stop();
            assert_eq!(
                lifetime.drain(
                    crate::test_support::join_executor(),
                    Instant::now() + Duration::from_secs(5)
                ),
                BackupRouteDrain::Drained
            );
        }
    }
}
