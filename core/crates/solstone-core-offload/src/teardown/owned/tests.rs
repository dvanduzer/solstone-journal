// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;
use solstone_core_backup::{HostedBinding, save_hosted_binding};
use solstone_core_journal_io::activity::{ActivityAdmissionError, ActivityLease};
use solstone_core_journal_io::activity_obligations::observe_obligations;
use solstone_core_journal_io::media::{ActivityCarrierRetirementEffect, Confirmation, Durability};

struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn now_unix(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
    fn iso_week(&self) -> u8 {
        1
    }
}

fn request(peer: &mut TcpStream) -> (String, Vec<u8>) {
    // macOS hands an accepted socket the listener's non-blocking mode; Linux doesn't.
    peer.set_nonblocking(false).unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    peer.set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        peer.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() < 65536);
    }
    let head = String::from_utf8(bytes).unwrap();
    let length = head
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    assert!(length < 4 * 1024 * 1024);
    let mut body = vec![0; length];
    peer.read_exact(&mut body).unwrap();
    (head, body)
}

fn respond(peer: &mut TcpStream, body: &[u8]) {
    write!(
        peer,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    peer.write_all(body).unwrap();
}

fn closed(peer: &mut TcpStream) {
    peer.set_nonblocking(false).unwrap();
    let mut extra = Vec::new();
    match peer.read_to_end(&mut extra) {
        Ok(_) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ) => {}
        Err(error) => panic!("actual request socket did not close: {error}"),
    }
    assert!(extra.is_empty());
}

#[test]
fn production_reaches_root_admission_and_closed_custody_is_send() {
    let cancellation = ArchiveCancellation::new();
    let result = run_owned_teardown(
        Path::new("/no-journal-open"),
        Arc::new(TestClock(AtomicI64::new(1_700_000_000))),
        cancellation.clone(),
        "fixture",
    );
    assert!(matches!(
        result.refusal(),
        Some(TeardownRunRefusal::Root(_))
    ));
    assert_eq!(result.phase(), TeardownRunPhase::Admission);
    assert!(result.journal_root().is_none() && result.teardown_lease().is_none());
    assert!(result.carrier().is_none());
    assert!(!result.remote().unwrap().broker_attempted());
    assert!(
        !result.attempts().clear()
            && !result.attempts().binding()
            && !result.attempts().retirement()
    );
    assert!(result.refused_before_effects());
    assert!(result.cancellation().is_some());
    fn send_static<T: Send + 'static>() {}
    send_static::<OwnedTeardownOutcome>();
    send_static::<SettledTeardownReport>();
}

/// An operated journal whose hosted binding points at a local listener.
struct OperatedJournal {
    temp: tempfile::TempDir,
    listener: Option<TcpListener>,
}

#[cfg(all(test, feature = "full-tests"))]
impl OperatedJournal {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("config")).unwrap();
        fs::write(
            temp.path().join("config/journal.json"),
            br#"{"backup":{"enabled":true,"mode":"operated"},"other":7}"#,
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let binding = HostedBinding {
            broker_endpoint: format!("http://{}", listener.local_addr().unwrap()),
            account_id: "account".into(),
            instance_id: "instance".into(),
            bucket: "bucket".into(),
            prefix: "owner/".into(),
            broker_token: "fixture-token".into(),
        };
        save_hosted_binding(temp.path(), &binding).unwrap();
        Self {
            temp,
            listener: Some(listener),
        }
    }

    /// Record one bound archived file and return its local path.
    fn archive(&self, bound: bool) -> std::path::PathBuf {
        let directory = if bound {
            "010000_001_full"
        } else {
            "010000_001"
        };
        let segment = self.temp.path().join("chronicle/20260101").join(directory);
        fs::create_dir_all(&segment).unwrap();
        if bound {
            let event = json!({
                "event_kind":"offload_archive_v2", "schema_version":2, "time":2,
                "day":"20260101", "stream":"_default", "segment":"010000_001",
                "segment_dir":directory, "layout":"direct",
                "journal_id":"native-root-v1:unix:0000000000000001:0000000000000002",
                "repository_id":"c".repeat(64), "snapshot_id":"b".repeat(64),
                "manifest_id":"manifest", "files":[{
                    "witness_id":"witness", "name":"clip.webm", "bytes":3,
                    "sha256":"a".repeat(64), "archive_path":"/stage/clip.webm",
                    "processing":{"record":null,"has_analysis_row":true}
                }]
            });
            let ledger = crate::ledger::ledger_path_for_day(self.temp.path(), "20260101").unwrap();
            fs::create_dir_all(ledger.parent().unwrap()).unwrap();
            fs::write(ledger, format!("{event}\n")).unwrap();
        } else {
            crate::ledger::append_offload_event(
                self.temp.path(),
                "20260101",
                "_default",
                directory,
                "legacy-snapshot",
                &[solstone_core_retention::archive_catalog::OffloadFile {
                    name: "clip.webm".into(),
                    bytes: 3,
                    sha256: "a".repeat(64),
                }],
                1,
            )
            .unwrap();
        }
        segment.join("clip.webm")
    }

    fn run(&self) -> OwnedTeardownOutcome {
        run_owned_teardown(
            self.temp.path(),
            Arc::new(TestClock(AtomicI64::new(1_700_000_000))),
            ArchiveCancellation::new(),
            "fixture",
        )
    }

    fn effects(&self) -> [Vec<u8>; 2] {
        [
            fs::read(self.temp.path().join("config/journal.json")).unwrap(),
            fs::read(self.temp.path().join("backup/hosted/binding.json")).unwrap(),
        ]
    }

    fn no_request(&self) {
        assert!(matches!(
            self.listener.as_ref().unwrap().accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }
}

#[test]
fn backup_only_media_refuses_before_any_remote_or_config_effect() {
    use solstone_core_backup_runtime::TeardownReaderRefusal;
    use solstone_core_backup_runtime::teardown_admission::{
        TeardownAdmissionRefusal, TeardownCarrierRefusal,
    };
    for case in ["absent", "unbound", "resized"] {
        let journal = OperatedJournal::new();
        let local = journal.archive(case != "unbound");
        match case {
            "unbound" => fs::write(&local, b"abc").unwrap(),
            "resized" => fs::write(&local, b"different local bytes").unwrap(),
            _ => {}
        }
        let before = journal.effects();
        let local_before = fs::read(&local).ok();
        let result = journal.run();
        assert_eq!(result.phase(), TeardownRunPhase::Preparation, "{case}");
        assert!(
            matches!(
                result.refusal(),
                Some(TeardownRunRefusal::CarrierPreparation)
            ),
            "{case}"
        );
        assert_eq!(
            result.preparation_failure().unwrap().refusal(),
            TeardownCarrierRefusal::Admission(TeardownAdmissionRefusal::Reader(
                TeardownReaderRefusal::BackupOnlyHeld
            )),
            "{case}"
        );
        assert!(result.refused_before_effects(), "{case}");
        assert!(!result.remote().unwrap().broker_attempted(), "{case}");
        journal.no_request();
        assert_eq!(journal.effects(), before, "{case}");
        assert_eq!(fs::read(&local).ok(), local_before, "{case}");
        let root = result.journal_root().unwrap().clone();
        drop(result);
        assert!(
            observe_obligations(&root).unwrap().entries().is_empty(),
            "{case}"
        );
        drop(ActivityLease::try_acquire(root).unwrap());
    }
}

#[test]
fn fully_local_catalog_passes_coverage_to_the_remote_phase() {
    let mut journal = OperatedJournal::new();
    let local = journal.archive(true);
    fs::write(&local, b"abc").unwrap();
    let before = journal.effects();
    // Close the bound port so the remote phase ends locally, without any server.
    let closed = journal.listener.take().unwrap().local_addr().unwrap();
    let result = journal.run();
    assert!(TcpStream::connect(closed).is_err());
    assert_eq!(result.phase(), TeardownRunPhase::Remote);
    assert!(matches!(
        result.refusal(),
        Some(TeardownRunRefusal::Remote(_))
    ));
    assert!(!result.refused_before_effects());
    assert!(result.carrier().is_some());
    assert!(!result.attempts().clear());
    assert_eq!(journal.effects(), before);
    assert_eq!(fs::read(local).unwrap(), b"abc");
}

#[test]
fn owner_configured_destination_is_refused_after_admission_without_effects() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("config")).unwrap();
    let config = br#"{"backup":{"enabled":true,"destination":{"repository":"s3:bucket/prefix","backend":"s3"}}}"#;
    fs::write(temp.path().join("config/journal.json"), config).unwrap();
    let result = run_owned_teardown(
        temp.path(),
        Arc::new(TestClock(AtomicI64::new(1_700_000_000))),
        ArchiveCancellation::new(),
        "fixture",
    );
    assert_eq!(result.phase(), TeardownRunPhase::Admission);
    assert!(matches!(
        result.refusal(),
        Some(TeardownRunRefusal::UnsupportedDestination)
    ));
    assert!(result.refused_before_effects());
    assert_eq!(
        fs::read(temp.path().join("config/journal.json")).unwrap(),
        config
    );
}

#[test]
fn operated_destination_without_a_binding_is_not_called_unsupported() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("config")).unwrap();
    let config = br#"{"backup":{"enabled":true,"mode":"operated"}}"#;
    fs::write(temp.path().join("config/journal.json"), config).unwrap();
    let result = run_owned_teardown(
        temp.path(),
        Arc::new(TestClock(AtomicI64::new(1_700_000_000))),
        ArchiveCancellation::new(),
        "fixture",
    );
    assert!(!matches!(
        result.refusal(),
        Some(TeardownRunRefusal::UnsupportedDestination)
    ));
    assert!(result.refused_before_effects());
    assert_eq!(
        fs::read(temp.path().join("config/journal.json")).unwrap(),
        config
    );
}

const BOUNDARY_CASES: [&str; 11] = [
    "complete",
    "late_catalog",
    "late_binding",
    "cancel_body",
    "oversize_body",
    "panic_remote",
    "panic_transfer",
    "panic_postretire",
    "expiry",
    "backward",
    "cancel_post",
];

#[test]
#[ignore = "requires the copied installed package with the actual owned DNS member"]
#[cfg(all(test, feature = "full-tests"))]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn packaged_owned_teardown_boundaries() {
    let case = std::env::var("SOLSTONE_OWNED_TEARDOWN_CASE").unwrap();
    owned_teardown_boundaries(&case);
}

/// Windows resolves in process, so every case runs here without a package.
#[test]
#[cfg(all(test, feature = "full-tests"))]
#[cfg(windows)]
fn owned_teardown_boundaries_on_windows() {
    for case in BOUNDARY_CASES {
        owned_teardown_boundaries(case);
    }
}

/// Android's permanent in-process resolver uses the same owning coordinator.
#[test]
#[cfg(target_os = "android")]
fn owned_teardown_boundaries_on_android() {
    for case in BOUNDARY_CASES {
        owned_teardown_boundaries(case);
    }
}

#[cfg(all(test, feature = "full-tests"))]
fn owned_teardown_boundaries(case: &str) {
    let case = case.to_owned();
    assert!(BOUNDARY_CASES.contains(&case.as_str()));
    #[cfg(all(unix, not(target_os = "android")))]
    let parent = fs::canonicalize("/var/tmp").unwrap();
    #[cfg(any(windows, target_os = "android"))]
    let parent = fs::canonicalize(std::env::temp_dir()).unwrap();
    let temp = tempfile::Builder::new()
        .prefix("owned-teardown-journal-")
        // macOS links /var to /private/var; the journal root binds its real path.
        .tempdir_in(parent)
        .unwrap();
    fs::create_dir(temp.path().join("config")).unwrap();
    fs::write(
        temp.path().join("config/journal.json"),
        br#"{"backup":{"enabled":true,"mode":"operated"},"other":7}"#,
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let binding = HostedBinding {
        broker_endpoint: endpoint.clone(),
        account_id: "account".into(),
        instance_id: "instance".into(),
        bucket: "bucket".into(),
        prefix: "owner/&/".into(),
        broker_token: "fixture-token".into(),
    };
    save_hosted_binding(temp.path(), &binding).unwrap();
    let catalog = crate::ledger::ledger_path_for_day(temp.path(), "20260101").unwrap();
    fs::create_dir_all(catalog.parent().unwrap()).unwrap();
    let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
    super::super::read_teardown_coverage(
        &root,
        &solstone_core_backup::teardown_configuration::TeardownConfiguration::capture(&root)
            .unwrap(),
        None,
    )
    .unwrap();
    let clock = Arc::new(TestClock(AtomicI64::new(1_700_000_000)));
    let cancellation = ArchiveCancellation::new();
    let server_clock = clock.clone();
    let server_cancel = cancellation.clone();
    let journal = temp.path().to_owned();
    let server_case = case.clone();
    let expected = if ["late_binding", "expiry", "backward", "cancel_post"].contains(&case.as_str())
    {
        3
    } else if ["late_catalog", "cancel_body", "oversize_body"].contains(&case.as_str()) {
        4
    } else {
        7
    };
    let server = std::thread::spawn(move || {
        for step in 0..expected {
            let end = Instant::now() + Duration::from_secs(15);
            let (mut peer, _) = loop {
                match listener.accept() {
                    Ok(peer) => break peer,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < end, "missing actual request step{step}");
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            let (head, body) = request(&mut peer);
            let expected_line = match step {
                0 => "POST /backup/credentials HTTP/1.1",
                1 | 5 => "GET /bucket?list-type=2&prefix=owner%2F%26%2F HTTP/1.1",
                2 => "POST /bucket?delete= HTTP/1.1",
                3 | 6 => "GET /bucket?prefix=owner%2F%26%2F&uploads= HTTP/1.1",
                4 => "DELETE /bucket/owner/%26/part?uploadId=up%26id HTTP/1.1",
                _ => unreachable!(),
            };
            assert_eq!(head.lines().next(), Some(expected_line));
            if step == 0 {
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                    json!({"scope":"maintenance"})
                );
                respond(&mut peer,&serde_json::to_vec(&json!({
                    "access_key_id":"fixture-key","secret_access_key":"fixture-secret","session_token":"fixture-session",
                    "endpoint":endpoint,"expires_at":"2023-11-14T22:23:20Z"
                })).unwrap());
            } else {
                assert!(head.lines().any(|line| {
                    line.to_ascii_lowercase()
                        .starts_with("authorization: aws4-hmac-sha256 ")
                }));
                match step {
                    1=>respond(&mut peer,b"<ListBucketResult><Prefix>owner/&amp;/</Prefix><Contents><Key>owner/&amp;/one&amp;two</Key></Contents><IsTruncated>false</IsTruncated></ListBucketResult>"),
                    2=> {
                        assert_eq!(body,b"<Delete><Object><Key>owner/&amp;/one&amp;two</Key></Object></Delete>");
                        match server_case.as_str() {
                            "late_catalog"=>fs::write(&catalog,b"{torn\n").unwrap(),
                            "late_binding"=>{let mut changed=binding.clone();changed.bucket="different".into();save_hosted_binding(&journal,&changed).unwrap();},
                            "expiry"=>server_clock.0.store(1_700_000_600,Ordering::SeqCst),
                            "backward"=>server_clock.0.store(1_699_999_999,Ordering::SeqCst),
                            "cancel_post"=>{
                                server_cancel.cancel();
                                // Cancellation must close the in-flight request without
                                // waiting for a response. Windows may reset this socket.
                                closed(&mut peer);
                                continue;
                            },
                            _=>{},
                        }
                        respond(&mut peer,b"<DeleteResult><Deleted><Key>owner/&amp;/one&amp;two</Key></Deleted></DeleteResult>");
                    },
                    3 if server_case=="cancel_body"=>{
                        peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\n\r\nx").unwrap();
                        server_cancel.cancel();
                    },
                    3 if server_case=="oversize_body"=>{
                        let length=4*1024*1024+1;
                        write!(peer,"HTTP/1.1 200 OK\r\nContent-Length: {length}\r\n\r\n").unwrap();
                        let _=peer.write_all(&vec![b'x';length]);
                    },
                    3=>respond(&mut peer,b"<ListMultipartUploadsResult><Prefix>owner/&amp;/</Prefix><Upload><Key>owner/&amp;/part</Key><UploadId>up&amp;id</UploadId></Upload><IsTruncated>false</IsTruncated></ListMultipartUploadsResult>"),
                    4=>{assert!(body.is_empty());respond(&mut peer,b"");},
                    5=>respond(&mut peer,b"<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>"),
                    6=>respond(&mut peer,b"<ListMultipartUploadsResult><IsTruncated>false</IsTruncated></ListMultipartUploadsResult>"),
                    _=>unreachable!(),
                }
            }
            if step == 3 && ["cancel_body", "oversize_body"].contains(&server_case.as_str()) {
                closed(&mut peer);
            }
        }
        assert!(
            matches!(listener.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock)
        );
        expected
    });
    let cut = match case.as_str() {
        "panic_remote" => Some(TeardownRunPhase::Remote),
        "panic_transfer" => Some(TeardownRunPhase::CarrierTransfer),
        "panic_postretire" => Some(TeardownRunPhase::FinalChecks),
        _ => None,
    };
    PANIC_AT.with(|slot| slot.set(cut));
    // This installed fixture drives the coordinator directly with the actual owned
    // DNS member. Its writers use the actual protocols.
    let result = execute(temp.path(), Outside::new(clock, cancellation), "fixture");
    assert_eq!(server.join().unwrap(), expected);
    let remote = result.remote().unwrap();
    assert_eq!(remote.delete_attempts(), 1);
    if case == "cancel_post" {
        assert_eq!(remote.delete_confirmations(), 0);
        assert_eq!(remote.abort_attempts(), 0);
    }
    if ["late_catalog", "cancel_body", "oversize_body"].contains(&case.as_str()) {
        assert_eq!(remote.delete_confirmations(), 1);
        assert_eq!(remote.abort_attempts(), 0);
    }
    if case == "complete" {
        assert!(matches!(result, OwnedTeardownOutcome::Finished(_)));
        assert_eq!(remote.delete_confirmations(), 1);
        assert_eq!(remote.abort_attempts(), 1);
        assert_eq!(remote.abort_confirmations(), 1);
        assert!(remote.objects_empty() && remote.uploads_empty());
        assert!(result.binding().unwrap().complete());
        let retirement = result.retirement().unwrap();
        assert_eq!(
            retirement.effect(),
            ActivityCarrierRetirementEffect::Removed
        );
        // Windows has no directory flush; there the removal is confirmed unflushed.
        assert_eq!(
            retirement.root_durability(),
            if cfg!(windows) {
                Durability::Unproven
            } else {
                Durability::Synced
            }
        );
        assert_eq!(retirement.confirmation(), Confirmation::Confirmed);
        assert!(retirement.failures().is_empty());
        assert!(!temp.path().join("backup/hosted/binding.json").exists());
        assert!(observe_obligations(&root).unwrap().entries().is_empty());
        assert!(
            fs::read_to_string(temp.path().join("config/journal.json"))
                .unwrap()
                .contains("\"other\": 7")
        );
        drop(ActivityLease::try_acquire(root.clone()).unwrap());
    } else {
        assert!(!matches!(result, OwnedTeardownOutcome::Finished(_)));
        assert!(matches!(
            ActivityLease::try_acquire(root.clone()),
            Err(ActivityAdmissionError::Busy)
        ));
        if cut.is_some() {
            assert!(matches!(result, OwnedTeardownOutcome::Unknown(_)));
        }
        if case == "panic_postretire" {
            assert_eq!(
                result.retirement().unwrap().effect(),
                ActivityCarrierRetirementEffect::Removed
            );
            assert!(result.binding().unwrap().complete());
            assert!(observe_obligations(&root).unwrap().entries().is_empty());
        } else {
            assert!(result.retirement().is_none());
            assert!(!observe_obligations(&root).unwrap().entries().is_empty());
        }
        if case == "panic_transfer" {
            assert!(result.publication().is_some() && result.binding().unwrap().complete());
            assert!(
                result.carrier().is_none(),
                "no owner reconstruction after consuming transfer"
            );
            assert!(result.attempts().retirement());
        }
        if !["panic_transfer", "panic_postretire"].contains(&case.as_str()) {
            assert!(!result.attempts().clear());
            assert!(temp.path().join("backup/hosted/binding.json").is_file());
        }
    }
    std::thread::spawn(move || drop(result)).join().unwrap();
    drop(ActivityLease::try_acquire(root).unwrap());
    println!("owned_teardown_{case}_ok requests={expected}");
}

#[cfg(target_os = "android")]
#[path = "android_tests.rs"]
mod android_tests;
