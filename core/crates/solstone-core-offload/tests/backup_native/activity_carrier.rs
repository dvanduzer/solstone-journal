// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#![cfg(all(test, feature = "full-tests"))]
#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use solstone_core_journal_io::JournalRoot;
use solstone_core_journal_io::activity::{ActivityLease, TeardownLease};
use solstone_core_journal_io::media::{
    ActivityCarrierRetirementEffect, Confirmation, Durability, RootActivityCarrier,
};
use solstone_core_offload::recovery::{RecoveryObligation, observe_obligations};
use solstone_core_system::process::{
    InspectResult, InstanceVerdict, ProcessInstanceSource, SystemProcessInstanceSource,
};

const TIMEOUT: Duration = Duration::from_secs(30);

struct Coordinator(Child);
impl Drop for Coordinator {
    fn drop(&mut self) {
        let _ = self.0.kill();
        self.0.wait().expect("reap actual coordinator");
    }
}

fn connect() -> UnixStream {
    let stream = UnixStream::connect(std::env::var_os("CARRIER_CONTROL").unwrap()).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    stream.set_write_timeout(Some(TIMEOUT)).unwrap();
    stream
}

#[test]
#[ignore = "only launched by the owning explicit carrier death control"]
fn writer_probe() {
    let mut stream = connect();
    let pid = std::process::id();
    let InspectResult::Present {
        ppid: Some(ppid), ..
    } = SystemProcessInstanceSource.inspect(pid)
    else {
        panic!("actual writer identity unavailable")
    };
    writeln!(stream, "WRITER {pid} {ppid}").unwrap();
    let mut byte = [0];
    stream.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [1]);
    writeln!(stream, "WRITER_SETTLED").unwrap();
}

#[test]
#[ignore = "only launched by the owning explicit carrier death control"]
fn coordinator_probe() {
    let root =
        Arc::new(JournalRoot::open(Path::new(&std::env::var_os("CARRIER_ROOT").unwrap())).unwrap());
    let activity = ActivityLease::try_acquire(root).unwrap();
    let carrier = RootActivityCarrier::prepare(activity).unwrap();
    let mut writer = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "activity_carrier::writer_probe",
            "--ignored",
            "--nocapture",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stream = connect();
    writeln!(stream, "COORDINATOR {} {}", std::process::id(), writer.id()).unwrap();
    let mut byte = [0];
    stream.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [1]);
    let waited = writer.wait().unwrap();
    assert!(waited.success(), "actual writer wait failed: {waited}");
    // This controlled fixture has one actual writer, joined above, and no other
    // namespace actor. It qualifies neither arbitrary runners nor HTTP workers.
    let outcome = carrier.retire_after_settlement();
    assert_eq!(
        outcome.receipt.effect(),
        ActivityCarrierRetirementEffect::Removed
    );
    assert_eq!(outcome.receipt.root_durability(), Durability::Synced);
    assert_eq!(outcome.receipt.confirmation(), Confirmation::Confirmed);
    assert!(outcome.receipt.failures().is_empty());
    writeln!(stream, "COORDINATOR_RETIRED child_wait={waited}").unwrap();
}

fn accept(listener: &UnixListener, coordinator: &mut Coordinator) -> BufReader<UnixStream> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // Darwin can inherit the listener's nonblocking mode.
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                stream.set_write_timeout(Some(TIMEOUT)).unwrap();
                return BufReader::new(stream);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "fixture handshake deadline");
                assert!(
                    coordinator.0.try_wait().unwrap().is_none(),
                    "coordinator exited before handshake"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("fixture handshake: {error}"),
        }
    }
}

fn line(stream: &mut BufReader<UnixStream>) -> String {
    let mut line = String::new();
    assert!(
        stream.read_line(&mut line).unwrap() > 0,
        "fixture closed before receipt"
    );
    line.trim().to_owned()
}

fn control(kill_coordinator: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let root_path = tmp.path().join("journal");
    fs::create_dir(&root_path).unwrap();
    let control = tmp.path().join("control.sock");
    let listener = UnixListener::bind(&control).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut coordinator = Coordinator(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "activity_carrier::coordinator_probe",
                "--ignored",
                "--nocapture",
            ])
            .env("CARRIER_ROOT", &root_path)
            .env("CARRIER_CONTROL", &control)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    // Verify the native observer on the known current process before the child.
    let InspectResult::Present { instance: own, .. } =
        SystemProcessInstanceSource.inspect(std::process::id())
    else {
        panic!("observer cannot see known current process")
    };
    assert_eq!(own.pid, std::process::id());
    let mut writer_stream = None;
    let mut coordinator_stream = None;
    let mut writer_pid = None;
    for _ in 0..2 {
        let mut stream = accept(&listener, &mut coordinator);
        let receipt = line(&mut stream);
        let parts: Vec<_> = receipt.split_whitespace().collect();
        match parts.as_slice() {
            ["WRITER", pid, ppid] => {
                assert_eq!(ppid.parse::<u32>().unwrap(), coordinator.0.id());
                writer_pid = Some(pid.parse::<u32>().unwrap());
                writer_stream = Some(stream);
            }
            ["COORDINATOR", pid, child] => {
                assert_eq!(pid.parse::<u32>().unwrap(), coordinator.0.id());
                coordinator_stream = Some((stream, child.parse::<u32>().unwrap()));
            }
            _ => panic!("unexpected handshake {receipt}"),
        }
    }
    let mut writer_stream = writer_stream.unwrap();
    let (mut coordinator_stream, expected_writer) = coordinator_stream.unwrap();
    let writer_pid = writer_pid.unwrap();
    assert_eq!(expected_writer, writer_pid);
    let InspectResult::Present {
        instance: writer,
        ppid: Some(ppid),
        ..
    } = SystemProcessInstanceSource.inspect(writer_pid)
    else {
        panic!("actual live writer cannot be observed")
    };
    assert_eq!(ppid, coordinator.0.id());
    println!(
        "CARRIER_LIVE_HANDSHAKE observer_pid={} coordinator_pid={} writer_ppid={} writer={writer:?}",
        own.pid,
        coordinator.0.id(),
        ppid,
    );
    assert!(matches!(
        SystemProcessInstanceSource.observe(&writer),
        InstanceVerdict::SameLive { .. }
    ));
    if kill_coordinator {
        coordinator.0.kill().unwrap();
        let waited = coordinator.0.wait().unwrap();
        assert!(!waited.success());
        println!("CARRIER_COORDINATOR_REAPED {waited}");
        assert!(matches!(
            SystemProcessInstanceSource.observe(&writer),
            InstanceVerdict::SameLive { .. }
        ));
        for _ in 0..2 {
            let fresh_root = Arc::new(JournalRoot::open(&root_path).unwrap());
            let teardown = TeardownLease::try_acquire(fresh_root.clone()).unwrap();
            let census = observe_obligations(&fresh_root).unwrap();
            assert!(matches!(
                census.entries(),
                [RecoveryObligation::Stage { .. }]
            ));
            teardown.revalidate().unwrap();
            assert!(matches!(
                SystemProcessInstanceSource.observe(&writer),
                InstanceVerdict::SameLive { .. }
            ));
        }
        println!("CARRIER_SURVIVING_WRITER_FRESH_CENSUS_HELD");
        writer_stream.get_mut().write_all(&[1]).unwrap();
        assert_eq!(line(&mut writer_stream), "WRITER_SETTLED");
        let deadline = Instant::now() + TIMEOUT;
        while matches!(
            SystemProcessInstanceSource.observe(&writer),
            InstanceVerdict::SameLive { .. }
        ) {
            assert!(Instant::now() < deadline, "orphan writer did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            SystemProcessInstanceSource.observe(&writer),
            InstanceVerdict::NotSameOrExited
        );
        // No wait handle or original carrier remains: even observed writer exit
        // does not grant adoption/cleanup. The reservation remains held.
        let fresh_root = Arc::new(JournalRoot::open(&root_path).unwrap());
        assert!(matches!(
            observe_obligations(&fresh_root).unwrap().entries(),
            [RecoveryObligation::Stage { .. }]
        ));
        println!("CARRIER_DISCOVERED_RESERVATION_STILL_HELD_AFTER_WRITER_EXIT");
    } else {
        writer_stream.get_mut().write_all(&[1]).unwrap();
        assert_eq!(line(&mut writer_stream), "WRITER_SETTLED");
        coordinator_stream.get_mut().write_all(&[1]).unwrap();
        assert!(line(&mut coordinator_stream).starts_with("COORDINATOR_RETIRED child_wait="));
        let waited = coordinator.0.wait().unwrap();
        assert!(waited.success(), "coordinator wait: {waited}");
        let fresh_root = Arc::new(JournalRoot::open(&root_path).unwrap());
        let teardown = TeardownLease::try_acquire(fresh_root.clone()).unwrap();
        assert!(
            observe_obligations(&fresh_root)
                .unwrap()
                .entries()
                .is_empty()
        );
        teardown.revalidate().unwrap();
        println!("CARRIER_SETTLED_CHILD_OWNED_RETIREMENT_CONFIRMED");
    }
}

#[test]
#[ignore = "explicit actual coordinator-death/settlement qualification; no WAN"]
fn coordinator_death_and_settlement() {
    control(true);
    control(false);
}
