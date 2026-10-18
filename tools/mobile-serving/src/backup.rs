// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use serde_json::{Map, json};
use solstone_core_backup::archive_admission::ArchiveCancellation;
use solstone_core_backup_runtime::{
    hosted_runtime::{HttpError, HttpRequest, HttpResponse, HttpTransport},
    runner::{ToolOutput, ToolRequest, ToolRunner},
};
use solstone_core_backup_web::BackupWebDeps;
use solstone_core_callosum::{
    CallosumConnectionPhase, CallosumEnvelope, CallosumReceiveEvent, CallosumSocketConnection,
};
use solstone_core_journal_io::JournalRoot;
use solstone_core_maintenance::{CliRun, EmbeddedBackupRoutine};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
};

static NEXT_START: AtomicU64 = AtomicU64::new(1);

fn routine(envelope: &CallosumEnvelope) -> Option<EmbeddedBackupRoutine> {
    if envelope.tract != "supervisor" || envelope.event != "request" {
        return None;
    }
    let command = envelope.extra.get("cmd")?.as_array()?;
    if command.len() != 5
        || command[..4]
            != [
                json!("solstone"),
                json!("journal"),
                json!("maintenance"),
                json!("run"),
            ]
    {
        return None;
    }
    match command[4].as_str()? {
        "backup:run" => Some(EmbeddedBackupRoutine::Run),
        "backup:verify" => Some(EmbeddedBackupRoutine::Verify),
        "backup:prune" => Some(EmbeddedBackupRoutine::Prune),
        _ => None,
    }
}

/// One terminal disposition for an embedded backup maintenance run.
///
/// Bus submit, permit release, and task completion are not this disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackupDisposition {
    Success,
    Refusal,
    Failure,
}

/// Classify a finished backup command from its `CliRun`, root binding, and
/// any status the backup writer already recorded.
pub(crate) fn classify_backup_run(
    run: &CliRun,
    root_bound: bool,
    recorded_status: Option<&str>,
) -> BackupDisposition {
    if !root_bound {
        return BackupDisposition::Refusal;
    }
    if let Some(status) = recorded_status {
        return match status {
            "ok" | "skipped" | "cleared_superseded" => BackupDisposition::Success,
            "refused" => BackupDisposition::Refusal,
            _ => BackupDisposition::Failure,
        };
    }
    if run.exit_code == 0 {
        BackupDisposition::Success
    } else if run
        .stdout
        .split_whitespace()
        .any(|token| token == "reason=refused")
    {
        BackupDisposition::Refusal
    } else {
        BackupDisposition::Failure
    }
}

pub(crate) fn backup_terminal(
    root: &JournalRoot,
    routine: EmbeddedBackupRoutine,
    run: &CliRun,
) -> BackupDisposition {
    let root_bound = root.revalidate_canonical_binding().is_ok();
    let recorded = root_bound.then(|| recorded_status(root, routine)).flatten();
    classify_backup_run(run, root_bound, recorded.as_deref())
}

pub(crate) fn log_backup_terminal(disposition: BackupDisposition, exit_code: i32) {
    match disposition {
        BackupDisposition::Success => {}
        BackupDisposition::Refusal => {
            log::warn!("embedded backup maintenance refused");
        }
        BackupDisposition::Failure => {
            log::warn!("embedded backup maintenance failed (exit {exit_code})");
        }
    }
}

fn recorded_status(root: &JournalRoot, routine: EmbeddedBackupRoutine) -> Option<String> {
    let key = match routine {
        EmbeddedBackupRoutine::Run => "last_backup",
        EmbeddedBackupRoutine::Verify => "last_verification",
        EmbeddedBackupRoutine::Prune => "last_prune",
    };
    let config = solstone_core_backup::get_backup_config(root.canonical_path()).ok()?;
    let record = config.get(key)?.as_object()?;
    let status = record.get("status")?;
    if status.is_null() {
        return None;
    }
    let status = status.as_str()?;
    if status == "error"
        && record.get("error_reason").and_then(|value| value.as_str()) == Some("refused")
    {
        return Some("refused".to_owned());
    }
    Some(status.to_owned())
}

struct CancelledRunner<'a> {
    inner: &'a dyn ToolRunner,
    cancellation: &'a ArchiveCancellation,
}

impl ToolRunner for CancelledRunner<'_> {
    fn run(&self, request: &ToolRequest<'_>) -> std::io::Result<ToolOutput> {
        self.inner.run_cancellable(request, self.cancellation)
    }
    fn run_cancellable(
        &self,
        request: &ToolRequest<'_>,
        _: &ArchiveCancellation,
    ) -> std::io::Result<ToolOutput> {
        self.run(request)
    }
    fn cancellation(&self) -> Option<&ArchiveCancellation> {
        Some(self.cancellation)
    }

    #[cfg(target_os = "ios")]
    fn ios_journal_root(&self) -> Option<&Arc<JournalRoot>> {
        self.inner.ios_journal_root()
    }
}

struct CancelledHttp<'a> {
    inner: &'a dyn HttpTransport,
    cancellation: &'a ArchiveCancellation,
}

impl HttpTransport for CancelledHttp<'_> {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        self.inner.execute_cancellable(request, self.cancellation)
    }
    fn execute_cancellable(
        &self,
        request: &HttpRequest,
        _: &ArchiveCancellation,
    ) -> Result<HttpResponse, HttpError> {
        self.execute(request)
    }
}

pub(super) async fn run(
    root: Arc<JournalRoot>,
    deps: BackupWebDeps,
    stop: watch::Receiver<bool>,
    ready: oneshot::Sender<()>,
) -> Result<(), tokio::task::JoinError> {
    run_with_executor(root, deps, stop, ready, Arc::new(execute)).await
}

type Executor = Arc<
    dyn Fn(&JournalRoot, EmbeddedBackupRoutine, &BackupWebDeps, &ArchiveCancellation) + Send + Sync,
>;

fn execute(
    root: &JournalRoot,
    routine: EmbeddedBackupRoutine,
    deps: &BackupWebDeps,
    cancellation: &ArchiveCancellation,
) {
    let runner = CancelledRunner {
        inner: deps.runner.as_ref(),
        cancellation,
    };
    let http = CancelledHttp {
        inner: deps.http.as_ref(),
        cancellation,
    };
    let outcome = solstone_core_maintenance::run_embedded_backup(
        root.canonical_path(),
        routine,
        &runner,
        &http,
        &deps.executable,
    );
    let disposition = backup_terminal(root, routine, &outcome);
    log_backup_terminal(disposition, outcome.exit_code);
}

async fn run_with_executor(
    root: Arc<JournalRoot>,
    deps: BackupWebDeps,
    mut stop: watch::Receiver<bool>,
    ready: oneshot::Sender<()>,
    executor: Executor,
) -> Result<(), tokio::task::JoinError> {
    let mut connection = CallosumSocketConnection::new(
        root.canonical_path().join("health/callosum.sock"),
        Map::new(),
    );
    connection.start();
    let marker = NEXT_START.fetch_add(1, Ordering::Relaxed);
    let mut ready = Some(ready);
    let mut worker: Option<JoinHandle<()>> = None;
    loop {
        if *stop.borrow() {
            break;
        }
        tokio::select! {
            _ = stop.changed() => break,
            finished = async {
                match worker.as_mut() {
                    Some(worker) => Some(worker.await),
                    None => std::future::pending().await,
                }
            } => {
                if let Some(Err(error)) = finished {
                    deps.lifetime().request_stop();
                    connection.stop().await;
                    // Return the actual error and opaque payload to the original
                    // app owner. A completed permit is not successful retirement.
                    return Err(error);
                }
                worker = None;
            },
            event = connection.next_event() => {
                let Some(event) = event else { break };
                let envelope = match event {
                    CallosumReceiveEvent::Continuity { phase: CallosumConnectionPhase::Connected, .. } => {
                        // Reflection proves registration in the broadcast set before
                        // the app can accept an owner backup request.
                        connection.emit("supervisor", "status", Map::from_iter([
                            ("service".into(), json!("backup")),
                            ("embedded_backup_start".into(), json!(marker)),
                        ]));
                        continue;
                    },
                    CallosumReceiveEvent::Continuity { .. } => continue,
                    CallosumReceiveEvent::Envelope { envelope, .. } => envelope,
                };
                if envelope.tract == "supervisor" && envelope.event == "status"
                    && envelope.extra.get("embedded_backup_start") == Some(&json!(marker))
                {
                    if let Some(ready) = ready.take() { let _ = ready.send(()); }
                    continue;
                }
                if ready.is_some() { continue; }
                let Some(routine) = routine(&envelope) else { continue };
                if worker.is_some() {
                    log::warn!("embedded backup request refused while a maintenance worker is active");
                    continue;
                }
                let permit = match deps.lifetime().admit_maintenance() {
                    Ok(permit) => permit,
                    Err(_) => {
                        log::warn!("embedded backup request refused by operation admission");
                        continue;
                    },
                };
                let root = Arc::clone(&root);
                let deps = deps.clone();
                let executor = Arc::clone(&executor);
                worker = Some(tokio::task::spawn_blocking(move || {
                    // The actual blocking worker owns the permit, including when
                    // an async observer is cancelled or the reply is lost.
                    let _permit = permit;
                    if root.revalidate_canonical_binding().is_err() {
                        log::warn!("embedded backup refused a changed journal root");
                        return;
                    }
                    let cancellation = _permit.cancellation();
                    executor(&root, routine, &deps, cancellation);
                }));
            },
        }
    }
    deps.lifetime().request_stop();
    let result = match worker {
        Some(worker) => worker.await,
        None => Ok(()),
    };
    connection.stop().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    fn request(command: Value) -> CallosumEnvelope {
        serde_json::from_value(json!({"tract":"supervisor","event":"request","cmd":command}))
            .unwrap()
    }
    #[test]
    fn only_the_normal_backup_maintenance_requests_are_admitted() {
        let prefix = ["solstone", "journal", "maintenance", "run"];
        for (name, expected) in [
            ("backup:run", EmbeddedBackupRoutine::Run),
            ("backup:verify", EmbeddedBackupRoutine::Verify),
            ("backup:prune", EmbeddedBackupRoutine::Prune),
        ] {
            let mut command = prefix.to_vec();
            command.push(name);
            assert_eq!(routine(&request(json!(command))), Some(expected));
        }
        for command in [
            json!([
                "solstone",
                "journal",
                "maintenance",
                "run",
                "backup:run",
                "--extra"
            ]),
            json!([
                "solstone",
                "journal",
                "maintenance",
                "run",
                "backup:offload"
            ]),
            json!(["other", "journal", "maintenance", "run", "backup:run"]),
            json!(null),
        ] {
            assert_eq!(routine(&request(command)), None);
        }
        let mut other = request(json!([
            "solstone",
            "journal",
            "maintenance",
            "run",
            "backup:run"
        ]));
        other.event = "status".into();
        assert_eq!(routine(&other), None);
    }

    fn run(exit_code: i32, stdout: &str) -> CliRun {
        CliRun {
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_code,
        }
    }

    #[test]
    fn backup_disposition_follows_binding_and_recorded_status() {
        assert_eq!(
            classify_backup_run(&run(0, ""), true, None),
            BackupDisposition::Success
        );
        assert_eq!(
            classify_backup_run(&run(1, "reason=refused"), true, None),
            BackupDisposition::Refusal
        );
        assert_eq!(
            classify_backup_run(&run(1, "reason=missing"), true, None),
            BackupDisposition::Failure
        );
        assert_eq!(
            classify_backup_run(&run(0, ""), false, Some("ok")),
            BackupDisposition::Refusal
        );
        for status in ["ok", "skipped", "cleared_superseded"] {
            assert_eq!(
                classify_backup_run(&run(1, ""), true, Some(status)),
                BackupDisposition::Success
            );
        }
        assert_eq!(
            classify_backup_run(&run(0, ""), true, Some("refused")),
            BackupDisposition::Refusal
        );
        assert_eq!(
            classify_backup_run(&run(0, ""), true, Some("error")),
            BackupDisposition::Failure
        );
    }

    #[test]
    fn recorded_backup_error_is_a_failure_and_a_refused_reason_is_a_refusal() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("config")).unwrap();
        std::fs::write(root.path().join("config/journal.json"), "{}").unwrap();
        solstone_core_backup::record_backup_result(
            root.path(),
            "error",
            json!(null),
            json!(null),
            json!("tool"),
            None,
            None,
        )
        .unwrap();
        let journal = JournalRoot::open(root.path()).unwrap();
        assert_eq!(
            backup_terminal(&journal, EmbeddedBackupRoutine::Run, &run(1, "")),
            BackupDisposition::Failure
        );
        solstone_core_backup::record_backup_result(
            root.path(),
            "error",
            json!(null),
            json!(null),
            json!("refused"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            backup_terminal(&journal, EmbeddedBackupRoutine::Run, &run(0, "")),
            BackupDisposition::Refusal
        );
    }

    #[cfg(feature = "full-tests")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reflected_consumer_retains_real_maintenance_until_completion_and_returns_panic() {
        use std::{
            sync::{Mutex, atomic::AtomicBool},
            time::{Duration, Instant},
        };
        for (panic, stop_before_release) in [(false, true), (true, true), (true, false)] {
            struct Payload(Arc<AtomicBool>);
            impl Drop for Payload {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                    panic!("opaque worker panic payload must remain owned");
                }
            }
            let directory = tempfile::tempdir().unwrap();
            let root = Arc::new(JournalRoot::open(directory.path()).unwrap());
            let socket = root.canonical_path().join("health/callosum.sock");
            let bus = solstone_core_callosum::CallosumSocketServer::bind(socket.clone())
                .await
                .unwrap();
            let mut observer = CallosumSocketConnection::new(socket, Map::new());
            observer.start();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match observer.next_event().await {
                        Some(CallosumReceiveEvent::Continuity {
                            phase: CallosumConnectionPhase::Connected,
                            ..
                        }) => {
                            observer.emit(
                                "supervisor",
                                "status",
                                Map::from_iter([("backup_test_observer".into(), json!(true))]),
                            );
                        }
                        Some(CallosumReceiveEvent::Envelope { envelope, .. })
                            if envelope.extra.get("backup_test_observer") == Some(&json!(true)) =>
                        {
                            break;
                        }
                        _ => {}
                    }
                }
            })
            .await
            .unwrap();
            let deps = BackupWebDeps::new(root.canonical_path().to_path_buf());
            let lifetime = deps.lifetime();
            let (entered, entry) = oneshot::channel();
            let entered = Mutex::new(Some(entered));
            let (release, held) = std::sync::mpsc::channel();
            let held = Mutex::new(held);
            let dropped = Arc::new(AtomicBool::new(false));
            let payload_dropped = dropped.clone();
            let (stop, stopping) = watch::channel(false);
            let (ready, registered) = oneshot::channel();
            let mut consumer = tokio::spawn(run_with_executor(
                root.clone(),
                deps,
                stopping,
                ready,
                Arc::new(move |root, routine, _, cancellation| {
                    assert_eq!(routine, EmbeddedBackupRoutine::Run);
                    root.revalidate_canonical_binding().unwrap();
                    entered.lock().unwrap().take().unwrap().send(()).unwrap();
                    held.lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                    assert_eq!(cancellation.is_cancelled(), stop_before_release);
                    if panic {
                        std::panic::panic_any(Payload(payload_dropped.clone()));
                    }
                }),
            ));
            tokio::time::timeout(Duration::from_secs(5), registered)
                .await
                .unwrap()
                .unwrap();
            observer.emit(
                "supervisor",
                "request",
                Map::from_iter([(
                    "cmd".into(),
                    json!(["solstone", "journal", "maintenance", "run", "backup:run"]),
                )]),
            );
            tokio::time::timeout(Duration::from_secs(5), entry)
                .await
                .unwrap()
                .unwrap();
            if stop_before_release {
                stop.send(true).unwrap();
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(25), &mut consumer)
                    .await
                    .is_err()
            );
            assert_eq!(
                lifetime.drain(
                    &tokio::runtime::Handle::current(),
                    Instant::now() + Duration::from_millis(25)
                ),
                solstone_core_backup_web::BackupRouteDrain::Pending
            );
            if stop_before_release {
                assert!(lifetime.admit_maintenance().is_err());
            }
            release.send(()).unwrap();
            let result = tokio::time::timeout(Duration::from_secs(5), consumer)
                .await
                .unwrap()
                .unwrap();
            if panic {
                let failure = result.unwrap_err();
                assert!(failure.is_panic());
                let payload = failure.into_panic();
                assert!(Arc::ptr_eq(
                    &payload
                        .downcast_ref::<Payload>()
                        .expect("exact injected opaque payload")
                        .0,
                    &dropped
                ));
                assert!(!dropped.load(Ordering::Acquire));
                // Model the sticky original-owner failure custody; never discard
                // this opaque payload while reporting a shutdown result.
                std::mem::forget(payload);
            } else {
                result.unwrap();
            }
            assert!(lifetime.admit_maintenance().is_err());
            assert_eq!(
                lifetime.drain(
                    &tokio::runtime::Handle::current(),
                    Instant::now() + Duration::from_secs(1)
                ),
                solstone_core_backup_web::BackupRouteDrain::Drained
            );
            observer.stop().await;
            bus.stop().await;
        }
    }
}
