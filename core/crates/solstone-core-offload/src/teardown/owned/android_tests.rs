// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#![cfg(all(test, feature = "full-tests"))]
#![cfg(target_os = "android")]

//! Each singleton fixture runs as one exact test in a fresh Android process.

use super::*;
use solstone_core_backup_runtime::test_support::{
    android_dns_lookup_before_start, android_dns_lookup_idle,
};
use std::sync::{Condvar, Mutex, OnceLock, atomic::AtomicUsize};

#[derive(Default)]
struct HeldLookup {
    state: Mutex<(bool, bool)>,
    wake: Condvar,
    calls: AtomicUsize,
}
static LOOKUP: OnceLock<HeldLookup> = OnceLock::new();

fn held_lookup(
    _: &str,
    port: u16,
    _: ureq::config::IpFamily,
) -> std::io::Result<Vec<std::net::SocketAddr>> {
    let held = LOOKUP.get().unwrap();
    held.calls.fetch_add(1, Ordering::SeqCst);
    let mut state = held.state.lock().unwrap();
    state.0 = true;
    held.wake.notify_all();
    while !state.1 {
        state = held.wake.wait(state).unwrap();
    }
    Ok(vec![std::net::SocketAddr::from(([127, 0, 0, 1], port))])
}

struct Release(ArchiveCancellation);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.cancel();
        let held = LOOKUP.get().unwrap();
        held.state.lock().unwrap().1 = true;
        held.wake.notify_all();
    }
}

fn assert_original_custody(
    result: &OwnedTeardownOutcome,
    journal: &OperatedJournal,
    before: &[Vec<u8>; 2],
) {
    assert_eq!(result.phase(), TeardownRunPhase::Remote);
    assert!(matches!(
        result.refusal(),
        Some(TeardownRunRefusal::Remote(_))
    ));
    assert!(!result.attempts().clear());
    assert!(!result.attempts().binding());
    assert!(!result.attempts().retirement());
    assert!(result.carrier().is_some() && result.teardown_lease().is_some());
    assert_eq!(&journal.effects(), before);
    let root = result.journal_root().unwrap().clone();
    assert!(!observe_obligations(&root).unwrap().entries().is_empty());
    assert!(matches!(
        ActivityLease::try_acquire(root),
        Err(ActivityAdmissionError::Busy)
    ));
}

fn held_dns_refusal(cancel: bool) {
    LOOKUP.set(HeldLookup::default()).ok().unwrap();
    android_dns_lookup_before_start(held_lookup).unwrap();
    let journal = OperatedJournal::new();
    let mut binding = solstone_core_backup::load_hosted_binding(journal.temp.path()).unwrap();
    let port = journal
        .listener
        .as_ref()
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    binding.broker_endpoint = format!("http://held.invalid:{port}");
    save_hosted_binding(journal.temp.path(), &binding).unwrap();
    let before = journal.effects();
    let cancellation = ArchiveCancellation::new();
    let started = Instant::now();
    std::thread::scope(|scope| {
        // Drop before the scope's automatic join on assertion failure.
        let release = Release(cancellation.clone());
        let attempt = scope.spawn(|| {
            run_owned_teardown(
                journal.temp.path(),
                Arc::new(TestClock(AtomicI64::new(1_700_000_000))),
                cancellation.clone(),
                "fixture",
            )
        });
        let held = LOOKUP.get().unwrap();
        let entered = held
            .wake
            .wait_timeout_while(
                held.state.lock().unwrap(),
                Duration::from_secs(5),
                |state| !state.0,
            )
            .unwrap();
        assert!(entered.0.0, "production resolver never entered");
        drop(entered);
        let cancelled_at = Instant::now();
        if cancel {
            cancellation.cancel();
        }
        let result = attempt.join().unwrap();
        if cancel {
            assert!(cancelled_at.elapsed() < Duration::from_secs(1));
        } else {
            let budget = Duration::from_secs(
                solstone_core_backup_runtime::hosted_runtime::BROKER_TIMEOUT_SECONDS,
            );
            assert!(started.elapsed() >= budget);
            assert!(started.elapsed() < budget + Duration::from_secs(1));
        }
        assert_original_custody(&result, &journal, &before);
        journal.no_request();
        // A distinct coordinator cannot queue a replacement while DNS is held.
        let other = OperatedJournal::new();
        let mut other_binding =
            solstone_core_backup::load_hosted_binding(other.temp.path()).unwrap();
        let other_port = other
            .listener
            .as_ref()
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        other_binding.broker_endpoint = format!("http://held.invalid:{other_port}");
        save_hosted_binding(other.temp.path(), &other_binding).unwrap();
        let other_before = other.effects();
        let replacement = other.run();
        assert_original_custody(&replacement, &other, &other_before);
        other.no_request();
        assert_eq!(held.calls.load(Ordering::SeqCst), 1);
        drop(release); // allow the original late result, after both refusals
        // Observe the actual worker's post-lookup busy release. This read cannot
        // reset or replace the owner or release any of the coordinator guards.
        let released_by = Instant::now() + Duration::from_secs(5);
        while !android_dns_lookup_idle() {
            assert!(Instant::now() < released_by);
            std::thread::yield_now();
        }
        journal.no_request();
        other.no_request();
        assert_eq!(journal.effects(), before);
        assert_eq!(held.calls.load(Ordering::SeqCst), 1);
        let root = result.journal_root().unwrap().clone();
        let carrier_path = result.carrier().unwrap().diagnostic_path();
        use std::os::unix::fs::MetadataExt;
        let original = fs::metadata(&carrier_path).unwrap();
        drop(result);
        drop(replacement);
        // Drop closes owners only. It never certifies settlement or unlinks the
        // original reservation; a later admitted recovery must handle it.
        assert!(!observe_obligations(&root).unwrap().entries().is_empty());
        let retained = fs::metadata(carrier_path).unwrap();
        assert_eq!(
            (retained.dev(), retained.ino()),
            (original.dev(), original.ino())
        );
        drop(ActivityLease::try_acquire(root).unwrap());
    });
    println!(
        "ANDROID_OWNED_DNS_{}_NO_EFFECTS",
        if cancel { "CANCEL" } else { "DEADLINE" }
    );
}

#[test]
fn cancelled_dns_preserves_original_coordinator_and_discards_late_answer() {
    held_dns_refusal(true);
}

#[test]
fn deadline_dns_preserves_original_coordinator_and_discards_late_answer() {
    held_dns_refusal(false);
}

fn broker_response(
    status: u16,
    body: &'static [u8],
) -> (OperatedJournal, OwnedTeardownOutcome, [Vec<u8>; 2]) {
    let mut journal = OperatedJournal::new();
    let listener = journal.listener.take().unwrap();
    let before = journal.effects();
    let server = std::thread::spawn(move || {
        let end = Instant::now() + Duration::from_secs(5);
        let mut peer = loop {
            match listener.accept() {
                Ok((peer, _)) => break peer,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < end);
                    std::thread::yield_now();
                }
                Err(error) => panic!("{error}"),
            }
        };
        assert_eq!(
            request(&mut peer).0.lines().next(),
            Some("POST /backup/credentials HTTP/1.1")
        );
        write!(
            peer,
            "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        peer.write_all(body).unwrap();
        listener
    });
    let result = journal.run();
    journal.listener = Some(server.join().unwrap());
    (journal, result, before)
}

#[test]
fn broker_refusal_and_missing_credentials_preserve_original_coordinator() {
    for (status, body) in [
        (401, b"{}".as_slice()),
        (402, b"{}".as_slice()),
        (200, b"{}".as_slice()),
        (503, b"{}".as_slice()),
    ] {
        let (journal, result, before) = broker_response(status, body);
        assert_original_custody(&result, &journal, &before);
        assert!(result.remote().unwrap().broker_attempted());
        assert_eq!(result.remote().unwrap().delete_attempts(), 0);
        journal.no_request();
    }
}

#[test]
fn binding_superseded_finishes_authorized_local_cleanup() {
    let (journal, result, _) = broker_response(401, br#"{"error":"binding_superseded"}"#);
    eprintln!(
        "superseded phase={:?} remote={:?} attempts={:?} publication={:?} clear={:?} binding={:?} retirement={:?}",
        result.phase(),
        result.remote(),
        result.attempts(),
        result.publication(),
        result.clear_failure(),
        result.binding_failure().map(|failure| failure.refusal()),
        result.retirement()
    );
    if let Some(binding) = result.binding() {
        eprintln!(
            "superseded native={:?} confirmation={:?}",
            binding.native(),
            binding.confirmation()
        );
    }
    assert!(matches!(result, OwnedTeardownOutcome::Finished(_)));
    assert!(
        result.attempts().clear() && result.attempts().binding() && result.attempts().retirement()
    );
    assert_eq!(
        result.remote().unwrap().phase(),
        solstone_core_backup_runtime::teardown::owned::RemoteTeardownPhase::BindingSuperseded
    );
    assert_eq!(result.remote().unwrap().delete_attempts(), 0);
    assert!(result.binding().unwrap().complete());
    assert!(
        !journal
            .temp
            .path()
            .join("backup/hosted/binding.json")
            .exists()
    );
    assert!(
        fs::read_to_string(journal.temp.path().join("config/journal.json"))
            .unwrap()
            .contains("\"other\": 7")
    );
    journal.no_request();
}

static RETENTION_LOOKUPS: AtomicUsize = AtomicUsize::new(0);
fn retention_lookup(
    host: &str,
    port: u16,
    _: ureq::config::IpFamily,
) -> std::io::Result<Vec<std::net::SocketAddr>> {
    assert_eq!(host, "retention.invalid");
    RETENTION_LOOKUPS.fetch_add(1, Ordering::SeqCst);
    Ok(vec![std::net::SocketAddr::from(([127, 0, 0, 1], port))])
}

struct ResticResponses(Arc<Mutex<Vec<Vec<String>>>>);
impl solstone_core_backup_runtime::runner::ToolRunner for ResticResponses {
    fn run(
        &self,
        request: &solstone_core_backup_runtime::runner::ToolRequest<'_>,
    ) -> std::io::Result<solstone_core_backup_runtime::runner::ToolOutput> {
        let argv: Vec<String> = request
            .argv
            .iter()
            .map(|value| value.to_str().unwrap().to_owned())
            .collect();
        self.0.lock().unwrap().push(argv.clone());
        let commands: Vec<&str> = argv
            .iter()
            .map(String::as_str)
            .filter(|arg| *arg != "--no-cache")
            .collect();
        let stdout = match commands.as_slice() {
            ["snapshots", "--json"] if self.0.lock().unwrap().len() == 1 => {
                br#"[{"id":"ab12"}]"#.to_vec()
            }
            ["snapshots", "--json"] => b"[]".to_vec(),
            ["forget", "ab12", "--prune"] => Vec::new(),
            _ => panic!("unexpected restic request: {argv:?}"),
        };
        Ok(solstone_core_backup_runtime::runner::ToolOutput {
            returncode: 0,
            stdout,
            stderr: Vec::new(),
        })
    }
}

fn retention_coordinator(case: &'static str) {
    use solstone_core_backup_runtime::teardown::owned::StorageRetention;
    use solstone_core_backup_runtime::test_support::{
        install_packaged_android_test_tools, with_teardown_restic_fixture,
    };
    // The native directory comes from the signed probe APK; the copied assets
    // are digest-bound in its receipt and admitted through the real startup API.
    let native = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned();
    let golden_assets =
        std::path::PathBuf::from(std::env::var_os("SOLSTONE_ANDROID_TEST_SERVICE_ASSETS").unwrap());
    let private_assets = tempfile::tempdir().unwrap();
    let assets = private_assets.path();
    fs::create_dir(assets.join("resources")).unwrap();
    for file in ["manifest.json", "mobile-native-build.json"] {
        fs::copy(golden_assets.join(file), assets.join(file)).unwrap();
    }
    for entry in fs::read_dir(golden_assets.join("resources")).unwrap() {
        let entry = entry.unwrap();
        assert!(entry.file_type().unwrap().is_file());
        fs::copy(
            entry.path(),
            assets.join("resources").join(entry.file_name()),
        )
        .unwrap();
    }
    install_packaged_android_test_tools(&native, assets);
    android_dns_lookup_before_start(retention_lookup).unwrap();
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("config")).unwrap();
    fs::write(temp.path().join("config/journal.json"), b"{\"other\":7}").unwrap();
    fs::write(temp.path().join("local-data"), b"owner bytes").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let destination = solstone_core_backup::Destination {
        repository: format!(
            "s3:http://retention.invalid:{}/bucket/prefix",
            listener.local_addr().unwrap().port()
        ),
        backend: "s3".into(),
        credentials: json!({"access_key_id":"fixture-key","secret_access_key":"fixture-secret"})
            .as_object()
            .unwrap()
            .clone(),
    };
    solstone_core_backup::set_destination(temp.path(), &destination).unwrap();
    solstone_core_backup::generate_and_store_keys(temp.path()).unwrap();
    solstone_core_backup::set_enabled(temp.path(), true).unwrap();
    let before = fs::read(temp.path().join("config/journal.json")).unwrap();
    let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
    let clock = Arc::new(TestClock(AtomicI64::new(1_700_000_000)));
    let cancellation = ArchiveCancellation::new();
    let server_clock = clock.clone();
    let server_cancel = cancellation.clone();
    let resources = assets.join("resources");
    let server = std::thread::spawn(move || {
        let mut cancelled_at = None;
        let mut stall_duration = None;
        let expected = if case == "positive" || case == "changed_closure" {
            2
        } else {
            1
        };
        for step in 0..expected {
            let end = Instant::now() + Duration::from_secs(5);
            let mut peer = loop {
                match listener.accept() {
                    Ok((peer, _)) => break peer,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < end);
                        std::thread::yield_now();
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            let head = request(&mut peer).0;
            assert_eq!(
                head.lines().next(),
                Some(if step == 0 {
                    "GET /bucket?object-lock= HTTP/1.1"
                } else {
                    "GET /bucket?versioning= HTTP/1.1"
                })
            );
            assert!(
                head.to_ascii_lowercase()
                    .contains("authorization: aws4-hmac-sha256 ")
            );
            match case {
                "cancel_stall" | "request_timeout" => {
                    let stalled_at = Instant::now();
                    peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\n\r\nx")
                        .unwrap();
                    if case == "cancel_stall" {
                        cancelled_at = Some(Instant::now());
                        server_cancel.cancel();
                    }
                    peer.set_read_timeout(Some(Duration::from_secs(35)))
                        .unwrap();
                    closed(&mut peer);
                    stall_duration = Some(stalled_at.elapsed());
                }
                "oversize" => {
                    let length = 4 * 1024 * 1024 + 1;
                    write!(peer, "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\n\r\n").unwrap();
                    let _ = peer.write_all(&vec![b'x'; length]);
                    closed(&mut peer);
                }
                "original_expired" => {
                    server_clock.0.store(1_700_007_201, Ordering::SeqCst);
                    respond(&mut peer, b"<ObjectLockConfiguration/>");
                }
                _ => {
                    if step == 1 && case == "changed_closure" {
                        fs::write(
                            resources.join("ca-bundle.pem"),
                            b"changed after admitted inventory",
                        )
                        .unwrap();
                    }
                    respond(
                        &mut peer,
                        if step == 0 {
                            b"<ObjectLockConfiguration/>"
                        } else {
                            b"<VersioningConfiguration/>"
                        },
                    );
                }
            }
        }
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
        (expected, cancelled_at, stall_duration)
    });
    let calls = Arc::new(Mutex::new(Vec::new()));
    let result = with_teardown_restic_fixture(ResticResponses(calls.clone()), || {
        run_owned_teardown(temp.path(), clock, cancellation, "fixture")
    });
    let returned_at = Instant::now();
    let (requests, cancelled_at, stall_duration) = server.join().unwrap();
    assert_eq!(RETENTION_LOOKUPS.load(Ordering::SeqCst), requests);
    let unknown = matches!(
        case,
        "oversize" | "cancel_stall" | "request_timeout" | "original_expired"
    );
    assert_eq!(
        result.remote().unwrap().storage_retention(),
        Some(if unknown {
            StorageRetention::Unknown
        } else {
            StorageRetention::Removes
        })
    );
    let refused = matches!(
        case,
        "cancel_stall" | "original_expired" | "changed_closure"
    );
    let commands = calls.lock().unwrap();
    assert_eq!(
        result.remote().unwrap().delete_attempts(),
        if matches!(case, "cancel_stall" | "original_expired") {
            0
        } else {
            1
        }
    );
    assert_eq!(
        result.remote().unwrap().delete_confirmations(),
        if refused { 0 } else { 1 }
    );
    if refused {
        assert_eq!(commands.len(), 1, "forget/prune must not reach the runner");
        assert_eq!(result.phase(), TeardownRunPhase::Remote);
        assert!(
            !result.attempts().clear()
                && !result.attempts().binding()
                && !result.attempts().retirement()
        );
        assert!(result.carrier().is_some() && result.teardown_lease().is_some());
        assert_eq!(
            fs::read(temp.path().join("config/journal.json")).unwrap(),
            before
        );
        assert!(!observe_obligations(&root).unwrap().entries().is_empty());
        assert!(matches!(
            ActivityLease::try_acquire(root.clone()),
            Err(ActivityAdmissionError::Busy)
        ));
    } else {
        assert_eq!(commands.len(), 3);
        assert!(matches!(result, OwnedTeardownOutcome::Finished(_)));
        assert!(result.attempts().clear() && result.attempts().retirement());
        assert!(!result.attempts().binding());
        assert_eq!(
            result.retirement().unwrap().root_durability(),
            Durability::Synced
        );
        assert!(observe_obligations(&root).unwrap().entries().is_empty());
        drop(ActivityLease::try_acquire(root.clone()).unwrap());
    }
    if case == "request_timeout" {
        assert!(
            stall_duration.unwrap() >= Duration::from_secs(29)
                && stall_duration.unwrap() < Duration::from_secs(31)
        );
    }
    if case == "cancel_stall" {
        assert!(returned_at.duration_since(cancelled_at.unwrap()) < Duration::from_secs(1));
    }
    assert_eq!(
        fs::read(temp.path().join("local-data")).unwrap(),
        b"owner bytes"
    );
    drop(result);
    drop(ActivityLease::try_acquire(root).unwrap());
    println!(
        "ANDROID_RETENTION_{case}_OK http={requests} restic={}",
        commands.len()
    );
}

#[test]
fn retention_positive_finishes_real_coordinator() {
    retention_coordinator("positive");
}
#[test]
fn retention_oversize_is_unknown_and_preserves_authorized_cleanup() {
    retention_coordinator("oversize");
}
#[test]
fn retention_stall_cancellation_refuses_before_forget_and_local_effects() {
    retention_coordinator("cancel_stall");
}
#[test]
fn retention_stall_request_deadline_closes_socket_and_reports_unknown() {
    retention_coordinator("request_timeout");
}
#[test]
fn retention_original_budget_expiry_refuses_before_forget_and_local_effects() {
    retention_coordinator("original_expired");
}
#[test]
fn changed_admitted_closure_refuses_before_forget_and_local_effects() {
    retention_coordinator("changed_closure");
}
