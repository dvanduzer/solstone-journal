// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#![cfg(unix)]

use std::cell::{Cell, RefCell};
use std::fs;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use solstone_core_backup_runtime::engine::{
    backup_path_resolution_attempts, reset_backup_path_resolution_attempts,
};
use solstone_core_backup_runtime::hosted_runtime::HttpError;
use solstone_core_backup_runtime::{
    BackupServices, Clock, HttpRequest, HttpResponse, HttpTransport, JournalMaintenance,
    JournalMaintenanceError, SystemToolRunner, ToolOutput, ToolRequest, ToolRunner,
    backup_journal_resolved_hook_armed, install_backup_journal_resolved_hook,
    reset_backup_journal_resolved_hook, run_backup, run_restic,
};

const OUTER_DEADLINE: Duration = Duration::from_secs(2);

fn run_bounded<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        let _ = sender.send(result);
    });
    receiver
        .recv_timeout(OUTER_DEADLINE)
        .expect("test exceeded outer deadline")
        .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
}

fn write_fixture(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn wait_until_dead(pid: i32) {
    let started = Instant::now();
    loop {
        match kill(Pid::from_raw(pid), None) {
            Ok(()) => {
                assert!(
                    started.elapsed() < Duration::from_secs(1),
                    "descendant {pid} still live"
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(Errno::ESRCH) => return,
            Err(err) => panic!("kill({pid}, 0) failed: {err}"),
        }
    }
}

fn read_pid(path: &Path) -> i32 {
    fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .expect("descendant pid")
}

#[test]
fn real_fixture_process_observes_whitelisted_environment() {
    run_bounded(|| {
        let directory = tempfile::tempdir().unwrap();
        let fixture = directory.path().join("lib/solstone-restic/restic");
        fs::create_dir_all(fixture.parent().unwrap()).unwrap();
        write_fixture(
            &fixture,
            "#!/bin/sh\nprintf '%s' \"$RESTIC_REPOSITORY:$RESTIC_PASSWORD:$LEAK\"\n",
        );
        let result = run_restic(
            &SystemToolRunner,
            &[],
            "repo",
            "password",
            &fixture,
            None,
            false,
            None,
            Some(Duration::from_secs(1)),
            &[],
        )
        .unwrap();
        assert_eq!(result.stdout, "repo:[redacted]:");
    });
}

#[test]
fn controlled_runner_probe() {
    if std::env::var("CONTROLLED_RUNNER_PROBE").as_deref() != Ok("active") {
        return;
    }
    use std::io::Read;
    use std::os::fd::FromRawFd;
    let descriptor = |name| std::env::var(name).unwrap().parse::<i32>().unwrap();
    // SAFETY: this child exclusively owns its explicitly inherited descriptors.
    #[allow(unsafe_code)]
    let (mut ready, mut control) = unsafe {
        (
            fs::File::from_raw_fd(descriptor("READY_FD")),
            fs::File::from_raw_fd(descriptor("CONTROL_FD")),
        )
    };
    ready.write_all(&std::process::id().to_le_bytes()).unwrap();
    let mut release = [0];
    control.read_exact(&mut release).unwrap();
    println!("controlled runner finished");
}

#[test]
fn controlled_system_runner_cancels_and_reaps_its_actual_root() {
    use solstone_core_backup::archive_admission::ArchiveCancellation;
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    for cancelled in [false, true] {
        let (mut ready_parent, ready_child) = UnixStream::pair().unwrap();
        ready_parent
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (mut control_parent, control_child) = UnixStream::pair().unwrap();
        let request = ToolRequest {
            program: std::env::current_exe().unwrap().into_os_string(),
            argv: vec![
                "--exact".into(),
                "controlled_runner_probe".into(),
                "--nocapture".into(),
            ],
            env: std::collections::BTreeMap::from([
                ("CONTROLLED_RUNNER_PROBE".into(), "active".into()),
                (
                    "READY_FD".into(),
                    ready_child.as_raw_fd().to_string().into(),
                ),
                (
                    "CONTROL_FD".into(),
                    control_child.as_raw_fd().to_string().into(),
                ),
            ]),
            timeout: Some(Duration::from_secs(5)),
            pass_fds: vec![ready_child.as_fd(), control_child.as_fd()],
            stdin: None,
        };
        let cancellation = ArchiveCancellation::new();
        thread::scope(|scope| {
            let worker = scope.spawn(|| SystemToolRunner.run_cancellable(&request, &cancellation));
            let mut pid = [0; 4];
            ready_parent.read_exact(&mut pid).unwrap();
            let pid = Pid::from_raw(u32::from_le_bytes(pid) as i32);
            if cancelled {
                cancellation.cancel();
            } else {
                control_parent.write_all(b"r").unwrap();
            }
            let result = worker.join().unwrap();
            if cancelled {
                assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Interrupted);
            } else {
                let output = result.unwrap();
                assert_eq!(output.returncode, 0);
                assert!(
                    String::from_utf8(output.stdout)
                        .unwrap()
                        .contains("controlled runner finished")
                );
            }
            assert_eq!(
                nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)),
                Err(Errno::ECHILD)
            );
        });
    }
}

#[test]
fn timeout_scrubs_partial_output_and_passes_live_key_fd() {
    run_bounded(|| {
        #[cfg(target_os = "macos")]
        let (timeout, return_ceiling) = (Duration::from_millis(600), Duration::from_millis(900));
        #[cfg(not(target_os = "macos"))]
        let (timeout, return_ceiling) = (Duration::from_millis(200), Duration::from_millis(400));
        let directory = tempfile::tempdir().unwrap();
        let fixture = directory.path().join("lib/solstone-restic/restic");
        fs::create_dir_all(fixture.parent().unwrap()).unwrap();
        let pidfile = directory.path().join("sleep.pid");
        write_fixture(
            &fixture,
            "#!/bin/sh\nsleep 1 &\necho $! > \"$2\"\ncat /dev/fd/$1\nprintf ' PASSWORD' >&2\nwait\n",
        );
        let (reader, writer) = nix::unistd::pipe().unwrap();
        let mut writer = std::fs::File::from(writer);
        writer.write_all(b"PIPE_KEY").unwrap();
        drop(writer);
        let fd = reader.as_raw_fd();
        let started = Instant::now();
        let result = run_restic(
            &SystemToolRunner,
            &[fd.to_string(), pidfile.to_string_lossy().into_owned()],
            "repo",
            "PASSWORD",
            &fixture,
            None,
            true,
            None,
            Some(timeout),
            &[reader.as_fd()],
        )
        .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(result.returncode, 124);
        assert!(result.stdout.contains("PIPE_KEY"));
        assert_eq!(result.stderr, " [redacted]");
        assert_eq!(result.json, None);
        assert!(
            elapsed < return_ceiling,
            "timeout returned in {elapsed:?}, expected < {return_ceiling:?}"
        );
        wait_until_dead(read_pid(&pidfile));
    });
}

#[test]
fn natural_exit_terminates_orphaned_descendant() {
    run_bounded(|| {
        let directory = tempfile::tempdir().unwrap();
        let fixture = directory.path().join("lib/solstone-restic/restic");
        fs::create_dir_all(fixture.parent().unwrap()).unwrap();
        let pidfile = directory.path().join("sleep.pid");
        write_fixture(&fixture, "#!/bin/sh\nsleep 5 &\necho $! > \"$1\"\n");
        let started = Instant::now();
        let result = run_restic(
            &SystemToolRunner,
            &[pidfile.to_string_lossy().into_owned()],
            "repo",
            "PASSWORD",
            &fixture,
            None,
            false,
            None,
            None,
            &[],
        )
        .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(result.returncode, 0);
        assert!(
            elapsed < Duration::from_millis(400),
            "natural exit returned in {elapsed:?}, expected < 400ms"
        );
        wait_until_dead(read_pid(&pidfile));
    });
}

struct PanicHttp;
impl HttpTransport for PanicHttp {
    fn execute(&self, _: &HttpRequest) -> Result<HttpResponse, HttpError> {
        panic!("HTTP must not be reached")
    }
}

struct PanicRunner;
impl ToolRunner for PanicRunner {
    fn run(&self, _: &ToolRequest<'_>) -> std::io::Result<ToolOutput> {
        panic!("runner must not be reached")
    }
}

struct RecordingHttp {
    urls: RefCell<Vec<String>>,
}
impl HttpTransport for RecordingHttp {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        self.urls.borrow_mut().push(request.url.clone());
        Err(HttpError::Unreachable)
    }
}

struct FixedClock;
impl Clock for FixedClock {
    fn now_unix(&self) -> i64 {
        50
    }
    fn iso_week(&self) -> u8 {
        7
    }
}

struct Maintenance;
impl JournalMaintenance for Maintenance {
    fn rebuild_body_history(&self, _: &Path) -> Result<(), JournalMaintenanceError> {
        Ok(())
    }
    fn full_scan(&self, _: &Path) -> Result<(), JournalMaintenanceError> {
        Ok(())
    }
}

#[test]
#[ignore = "requires an admitted copied package and synthetic direct S3 fixture"]
fn packaged_owned_archive_entry() {
    use solstone_core_backup::archive_admission::{
        AdmissionError, ArchiveBudget, ArchiveCancellation,
    };
    use solstone_core_backup::archive_readback::ReadbackError;
    use solstone_core_backup_runtime::admit_archive_with_budget;
    use solstone_core_journal_io::{
        JournalRoot,
        media::{Cleanup, MediaDirectory, MediaStage, digest_reader},
    };
    use std::sync::Arc;

    struct ProbeClock(Cell<i64>);
    impl Clock for ProbeClock {
        fn now_unix(&self) -> i64 {
            self.0.get()
        }
        fn iso_week(&self) -> u8 {
            1
        }
    }
    let input_path =
        std::env::var_os("OWNED_ARCHIVE_PROBE_JSON").expect("explicit fixture required");
    let input: serde_json::Value = serde_json::from_slice(&fs::read(input_path).unwrap()).unwrap();
    let root = Arc::new(JournalRoot::open(Path::new(input["journal"].as_str().unwrap())).unwrap());
    let member =
        solstone_core_installed_payload::resolve_current_member("lib/solstone-restic/restic")
            .unwrap();
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let clock = ProbeClock(Cell::new(started));
    let cancellation = ArchiveCancellation::new();
    let http = RecordingHttp {
        urls: RefCell::new(Vec::new()),
    };
    let services = BackupServices {
        runner: &SystemToolRunner,
        http: &http,
        clock: &clock,
        restic_path: Some(member.path()),
        rclone_path: None,
        version: "2.0.40",
        journal_maintenance: &Maintenance,
    };
    let budget = ArchiveBudget::new_cancellable(&clock, cancellation.clone()).unwrap();
    let mut archive = admit_archive_with_budget(root.clone(), &services, budget).unwrap();
    assert_eq!(
        archive.repository_id(),
        input["repository_id"].as_str().unwrap()
    );
    let parent = MediaDirectory::open(root.clone(), Path::new("media")).unwrap();
    let fresh = || {
        MediaStage::create(parent.clone())
            .unwrap()
            .create_target()
            .unwrap()
    };
    let original_files = input["files"].as_array().unwrap();
    let copy = |index: usize| {
        let file = &original_files[index];
        let name = std::ffi::OsStr::new(file["name"].as_str().unwrap());
        let bytes = fs::read(root.canonical_path().join("media").join(name)).unwrap();
        let digest = digest_reader(&mut bytes.as_slice()).unwrap();
        assert_eq!(digest.sha256, file["sha256"].as_str().unwrap());
        assert_eq!(digest.size, file["bytes"].as_u64().unwrap());
        MediaStage::create(parent.clone())
            .unwrap()
            .create_named_target(name)
            .unwrap()
            .copy_from(&parent, name, &digest)
            .unwrap()
    };
    assert_eq!(original_files.len(), 2);
    let mut prepared_sources = vec![copy(0), copy(1)];
    let mut duplicate_one = copy(0);
    let mut duplicate_two = copy(0);
    let mut wrong_content = {
        let name = std::ffi::OsStr::new(original_files[0]["name"].as_str().unwrap());
        let path = root.canonical_path().join("media").join(name);
        let original = fs::read(&path).unwrap();
        let mut wrong = original.clone();
        wrong[0] ^= 1;
        fs::write(&path, &wrong).unwrap();
        let digest = digest_reader(&mut wrong.as_slice()).unwrap();
        let prepared = MediaStage::create(parent.clone())
            .unwrap()
            .create_named_target(name)
            .unwrap()
            .copy_from(&parent, name, &digest)
            .unwrap();
        fs::write(&path, original).unwrap();
        prepared
    };
    let sources = prepared_sources
        .iter_mut()
        .map(|p| p.archive_source().unwrap())
        .collect::<Vec<_>>();
    let duplicates = [
        duplicate_one.archive_source().unwrap(),
        duplicate_two.archive_source().unwrap(),
    ];
    let wrong_source = wrong_content.archive_source().unwrap();
    let initial_fd_flags = sources
        .iter()
        .map(|source| {
            source
                .descriptor()
                .map(|fd| nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFD).unwrap())
        })
        .collect::<Vec<_>>();
    for flags in initial_fd_flags.iter().flatten() {
        assert_ne!(flags & nix::fcntl::FdFlag::FD_CLOEXEC.bits(), 0);
    }
    let snapshot = archive.archive_owned(&sources).unwrap();
    let duplicate_snapshot = archive.archive_owned(&duplicates).unwrap();
    let mut reader = archive.into_read_operation().unwrap();
    let mut results = Vec::new();
    for source in &sources {
        let (exact, mut prepared) = reader
            .read_owned_snapshot(&snapshot.snapshot_id, source, fresh())
            .unwrap();
        assert_eq!(prepared.digest(), source.digest());
        assert_eq!(exact.name, source.name().to_str().unwrap());
        assert_eq!(exact.sha256, source.digest().sha256);
        assert_eq!(prepared.cleanup(), Cleanup::Complete);
        results.push(
            serde_json::json!({"name":exact.name, "archive_path":exact.archive_path,
            "bytes":exact.bytes, "sha256":exact.sha256}),
        );
    }
    let mut duplicate = reader
        .read_owned_snapshot(&duplicate_snapshot.snapshot_id, &sources[0], fresh())
        .err()
        .unwrap();
    assert_eq!(duplicate.reason(), ReadbackError::SnapshotNodeUnavailable);
    assert_eq!(duplicate.cleanup(), Cleanup::Complete);
    let mut wrong = reader
        .read_owned_snapshot(&snapshot.snapshot_id, &wrong_source, fresh())
        .err()
        .unwrap();
    assert_eq!(wrong.reason(), ReadbackError::NativeFailure);
    assert_eq!(wrong.cleanup(), Cleanup::Complete);
    let mut issued_target = fresh();
    drop(issued_target.helper_transport().unwrap());
    let mut issued = reader
        .read_owned_snapshot(&snapshot.snapshot_id, &sources[0], issued_target)
        .err()
        .unwrap();
    assert_eq!(issued.reason(), ReadbackError::NativeFailure);
    assert_eq!(issued.cleanup(), Cleanup::Refused);
    let mut invalid = reader
        .read_owned_snapshot("latest", &sources[0], fresh())
        .err()
        .unwrap();
    assert_eq!(invalid.reason(), ReadbackError::InvalidSelection);
    assert_eq!(invalid.cleanup(), Cleanup::Complete);
    let mut changed_bytes = fs::read(wrong_source.path()).unwrap();
    changed_bytes[0] ^= 2;
    fs::write(wrong_source.path(), changed_bytes).unwrap();
    let mut changed = reader
        .read_owned_snapshot(&snapshot.snapshot_id, &wrong_source, fresh())
        .err()
        .unwrap();
    assert_eq!(changed.reason(), ReadbackError::NativeFailure);
    assert_eq!(changed.cleanup(), Cleanup::Complete);
    clock.0.set(
        started + solstone_core_backup::archive_admission::ARCHIVE_BACKUP_TIMEOUT_SECONDS as i64,
    );
    let mut exhausted = reader
        .read_owned_snapshot(&snapshot.snapshot_id, &sources[0], fresh())
        .err()
        .unwrap();
    assert_eq!(
        exhausted.reason(),
        ReadbackError::Admission(AdmissionError::DeadlineExceeded)
    );
    assert_eq!(exhausted.cleanup(), Cleanup::Complete);
    cancellation.cancel();
    let mut cancelled = reader
        .read_owned_snapshot(&snapshot.snapshot_id, &sources[0], fresh())
        .err()
        .unwrap();
    assert_eq!(
        cancelled.reason(),
        ReadbackError::Admission(AdmissionError::Cancelled)
    );
    assert_eq!(cancelled.cleanup(), Cleanup::Complete);
    for (source, before) in sources.iter().zip(&initial_fd_flags) {
        source.revalidate().unwrap();
        assert_eq!(
            source
                .descriptor()
                .map(|fd| nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFD).unwrap()),
            *before
        );
    }
    assert!(http.urls.borrow().is_empty());
    println!(
        "OWNED_ARCHIVE_RESULT {}",
        serde_json::json!({"snapshot_id":snapshot.snapshot_id,
        "duplicate_snapshot_id":duplicate_snapshot.snapshot_id, "repository_id":snapshot.repository_id,
        "files":results, "original_budget_exhausted":true, "original_stop_observed":true,
        "duplicate_and_wrong_bytes_refused":true, "issued_target_cleanup":"refused"})
    );
    drop(sources);
    drop(duplicates);
    drop(wrong_source);
    for source in &mut prepared_sources {
        assert_eq!(source.cleanup(), Cleanup::Complete);
    }
    assert_eq!(duplicate_one.cleanup(), Cleanup::Complete);
    assert_eq!(duplicate_two.cleanup(), Cleanup::Complete);
    // Deliberately mutated owned input remains subject to Native cleanup refusal.
    let _ = wrong_content.cleanup();
}

fn configure_byo(journal: &Path) {
    let destination = solstone_core_backup::Destination {
        repository: "s3:repo".into(),
        backend: "s3".into(),
        credentials: serde_json::json!({
            "access_key_id": "access",
            "secret_access_key": "secret"
        })
        .as_object()
        .expect("credentials object")
        .clone(),
    };
    solstone_core_backup::set_destination(journal, &destination).expect("destination writes");
    solstone_core_backup::generate_and_store_keys(journal).expect("keys write");
    solstone_core_backup::set_enabled(journal, true).expect("backup enables");
}

fn backup_services<'a>(
    runner: &'a dyn ToolRunner,
    http: &'a dyn HttpTransport,
    clock: &'a dyn Clock,
    maintenance: &'a Maintenance,
) -> BackupServices<'a> {
    BackupServices {
        runner,
        http,
        clock,
        restic_path: Some(Path::new("/fixture/bin/restic")),
        rclone_path: Some(Path::new("/fixture/bin/rclone")),
        version: "test",
        journal_maintenance: maintenance,
    }
}

#[test]
fn external_hook_fires_between_canonical_resolution_and_config_read() {
    reset_backup_journal_resolved_hook();
    let source = tempfile::tempdir().expect("source journal creates");
    configure_byo(source.path());
    let replacement = tempfile::tempdir().expect("replacement journal creates");
    configure_byo(replacement.path());
    let sandbox = tempfile::tempdir().expect("sandbox creates");
    let alias = sandbox.path().join("journal");
    symlink(source.path(), &alias).expect("alias creates");

    let source_path = source.path().to_path_buf();
    let replacement_path = replacement.path().to_path_buf();
    let hook_alias = alias.clone();
    let firings = Rc::new(Cell::new(0));
    let hook_firings = Rc::clone(&firings);
    install_backup_journal_resolved_hook(move || {
        hook_firings.set(hook_firings.get() + 1);
        solstone_core_backup::set_mode(&source_path, "operated").expect("source mode changes");
        solstone_core_backup::save_hosted_binding(
            &source_path,
            &solstone_core_backup::HostedBinding {
                broker_endpoint: "https://source-broker.example".into(),
                account_id: "account".into(),
                instance_id: "instance".into(),
                bucket: "bucket".into(),
                prefix: "prefix".into(),
                broker_token: "token".into(),
            },
        )
        .expect("source binding writes");
        fs::remove_file(&hook_alias).expect("old alias removes");
        symlink(&replacement_path, &hook_alias).expect("alias retargets");
    });

    let runner = PanicRunner;
    let http = RecordingHttp {
        urls: RefCell::new(Vec::new()),
    };
    let clock = FixedClock;
    let maintenance = Maintenance;
    let result = run_backup(
        &alias,
        &backup_services(&runner, &http, &clock, &maintenance),
    );

    assert_eq!(firings.get(), 1);
    assert!(!backup_journal_resolved_hook_armed());
    assert_eq!(result.status, "error");
    assert_eq!(result.error_reason.as_deref(), Some("broker_unreachable"));
    assert_eq!(http.urls.borrow().len(), 1);
    assert!(http.urls.borrow()[0].starts_with("https://source-broker.example/"));
}

#[test]
fn second_hook_install_fails_without_replacing_the_first() {
    reset_backup_journal_resolved_hook();
    let firings = Rc::new(Cell::new(0));
    let hook_firings = Rc::clone(&firings);
    install_backup_journal_resolved_hook(move || hook_firings.set(hook_firings.get() + 1));
    let second = std::panic::catch_unwind(|| install_backup_journal_resolved_hook(|| {}));
    assert!(second.is_err());
    assert!(backup_journal_resolved_hook_armed());

    let journal = tempfile::tempdir().expect("journal creates");
    let runner = PanicRunner;
    let http = PanicHttp;
    let clock = FixedClock;
    let maintenance = Maintenance;
    let _ = run_backup(
        journal.path(),
        &backup_services(&runner, &http, &clock, &maintenance),
    );
    assert_eq!(firings.get(), 1);
    assert!(!backup_journal_resolved_hook_armed());
}

#[test]
fn failed_resolution_clears_hook_instead_of_leaking_to_the_next_run() {
    reset_backup_journal_resolved_hook();
    let firings = Rc::new(Cell::new(0));
    let hook_firings = Rc::clone(&firings);
    install_backup_journal_resolved_hook(move || hook_firings.set(hook_firings.get() + 1));

    let sandbox = tempfile::tempdir().expect("sandbox creates");
    let runner = PanicRunner;
    let http = PanicHttp;
    let clock = FixedClock;
    let maintenance = Maintenance;
    let services = backup_services(&runner, &http, &clock, &maintenance);
    let first = run_backup(&sandbox.path().join("missing"), &services);
    assert_eq!(
        first.error_reason.as_deref(),
        Some("journal_path_unresolved")
    );
    assert!(!backup_journal_resolved_hook_armed());

    let _ = run_backup(sandbox.path(), &services);
    assert_eq!(firings.get(), 0);
}

/// Binds a real Unix socket, so this lives in the `test-hooks` integration harness
/// rather than `engine.rs`'s inline unit tests, which the routine `make ci` unit
/// harness must stay free of hard-boundary (network) resources.
#[test]
fn non_directory_journal_roots_are_rejected_before_runtime_dependencies() {
    let sandbox = tempfile::tempdir().expect("test sandbox creates");
    let regular = sandbox.path().join("regular");
    fs::write(&regular, b"not a journal").expect("regular file writes");
    let fifo = sandbox.path().join("fifo");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo starts")
            .success(),
        "mkfifo succeeds"
    );
    let socket = sandbox.path().join("socket");
    let _listener = UnixListener::bind(&socket).expect("socket binds");

    let runner = PanicRunner;
    let http = PanicHttp;
    let clock = FixedClock;
    let maintenance = Maintenance;
    let services = BackupServices {
        runner: &runner,
        http: &http,
        clock: &clock,
        restic_path: Some(Path::new("/fixture/bin/restic")),
        rclone_path: None,
        version: "test",
        journal_maintenance: &maintenance,
    };

    for journal in [&regular, &fifo, &socket] {
        reset_backup_path_resolution_attempts();
        let result = run_backup(journal, &services);
        assert_eq!(result.status, "error");
        assert_eq!(
            result.error_reason.as_deref(),
            Some("journal_path_unresolved")
        );
        assert_eq!(backup_path_resolution_attempts(), 1);
    }
    assert!(
        !sandbox.path().join("config").exists(),
        "a non-directory journal root must not create sibling config artifacts"
    );
}
