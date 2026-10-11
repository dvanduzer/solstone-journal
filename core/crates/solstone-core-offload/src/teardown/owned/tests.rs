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
fn production_hold_is_prelaunch_and_closed_custody_is_send() {
    struct UntouchedClock;
    impl Clock for UntouchedClock {
        fn now_unix(&self) -> i64 {
            panic!("production hold must not prepare a budget")
        }
        fn iso_week(&self) -> u8 {
            panic!("production hold must not read time")
        }
    }
    let cancellation = ArchiveCancellation::new();
    let result = run_owned_teardown(
        Path::new("/no-journal-open"),
        Arc::new(UntouchedClock),
        cancellation.clone(),
        "fixture",
    );
    assert!(matches!(
        result.refusal(),
        Some(TeardownRunRefusal::NamespaceCooperationHeld)
    ));
    assert_eq!(result.phase(), TeardownRunPhase::Prelaunch);
    assert!(result.remote().is_none() && result.journal_root().is_none());
    assert!(result.teardown_lease().is_none() && result.carrier().is_none());
    assert!(
        !result.attempts().clear()
            && !result.attempts().binding()
            && !result.attempts().retirement()
    );
    assert!(result.cancellation().is_some());
    fn send_static<T: Send + 'static>() {}
    send_static::<OwnedTeardownOutcome>();
    send_static::<SettledTeardownReport>();
}

#[test]
#[ignore = "requires the copied installed package with the actual owned DNS member"]
#[cfg(all(test, feature = "full-tests"))]
fn packaged_owned_teardown_boundaries() {
    let case = std::env::var("SOLSTONE_OWNED_TEARDOWN_CASE").unwrap();
    assert!(
        [
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
            "cancel_post"
        ]
        .contains(&case.as_str())
    );
    let temp = tempfile::Builder::new()
        .prefix("owned-teardown-journal-")
        .tempdir_in("/var/tmp")
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
                            "cancel_post"=>server_cancel.cancel(),
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
    // Only this private installed fixture executes the same coordinator while
    // production cooperation remains held. Its writers use the actual protocols.
    let result = execute(temp.path(), Outside::new(clock, cancellation), "fixture");
    assert_eq!(server.join().unwrap(), expected);
    let remote = result.remote().unwrap();
    assert_eq!(remote.delete_attempts(), 1);
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
        assert_eq!(retirement.root_durability(), Durability::Synced);
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
