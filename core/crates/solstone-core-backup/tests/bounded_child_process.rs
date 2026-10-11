// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use solstone_core_backup::bounded_child::{Request, run};
use solstone_core_journal_io::JournalRoot;
use solstone_core_journal_io::media::{MediaDirectory, MediaIdentity, MediaStage, digest_reader};

fn cloexec(fd: impl AsFd) -> bool {
    let flags = fcntl(fd.as_fd(), FcntlArg::F_GETFD).unwrap();
    FdFlag::from_bits_truncate(flags).contains(FdFlag::FD_CLOEXEC)
}

fn descriptor_path(fd: i32) -> String {
    #[cfg(target_os = "linux")]
    {
        format!("/proc/self/fd/{fd}")
    }
    #[cfg(not(target_os = "linux"))]
    {
        format!("/dev/fd/{fd}")
    }
}

// This exact-test subprocess is a deterministic descriptor probe, never an
// admitted archive executable or neutral completion factory.
#[test]
fn descriptor_probe() {
    let Ok(mode) = std::env::var("DESCRIPTOR_PROBE_MODE") else {
        return;
    };
    let target: i32 = std::env::var("TARGET_FD").unwrap().parse().unwrap();
    if mode == "unrelated" {
        let expected_dev: u64 = std::env::var("TARGET_DEV").unwrap().parse().unwrap();
        let expected_ino: u64 = std::env::var("TARGET_INO").unwrap().parse().unwrap();
        if let Ok(metadata) = fs::metadata(descriptor_path(target)) {
            assert_ne!(
                (metadata.dev(), metadata.ino()),
                (expected_dev, expected_ino)
            );
        }
        let secret: i32 = std::env::var("SECRET_FD").unwrap().parse().unwrap();
        if let Ok(metadata) = fs::metadata(descriptor_path(secret)) {
            let expected_dev: u64 = std::env::var("SECRET_DEV").unwrap().parse().unwrap();
            let expected_ino: u64 = std::env::var("SECRET_INO").unwrap().parse().unwrap();
            assert_ne!(
                (metadata.dev(), metadata.ino()),
                (expected_dev, expected_ino)
            );
        }
        return;
    }
    let secret_fd: i32 = std::env::var("SECRET_FD").unwrap().parse().unwrap();
    let ready_fd: i32 = std::env::var("READY_FD").unwrap().parse().unwrap();
    let control_fd: i32 = std::env::var("CONTROL_FD").unwrap().parse().unwrap();
    // SAFETY: only this probe owns these explicitly passed child descriptors.
    #[allow(unsafe_code)]
    let (mut target, mut secret, mut ready, mut control) = unsafe {
        (
            File::from_raw_fd(target),
            File::from_raw_fd(secret_fd),
            File::from_raw_fd(ready_fd),
            File::from_raw_fd(control_fd),
        )
    };
    let mut contents = String::new();
    secret.read_to_string(&mut contents).unwrap();
    assert_eq!(contents, "private test credential");
    ready.write_all(b"r").unwrap();
    let mut release = [0u8];
    control.read_exact(&mut release).unwrap();
    assert_eq!(release, [b'g']);
    target.write_all(b"intended owned bytes").unwrap();
}

#[test]
fn intended_child_receives_target_while_unrelated_exec_does_not() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("media")).unwrap();
    let root = JournalRoot::open(directory.path()).unwrap();
    let parent = MediaDirectory::open(root.into(), Path::new("media")).unwrap();
    let mut target = MediaStage::create(parent).unwrap().create_target().unwrap();
    let secret_path = directory.path().join("secret");
    fs::write(&secret_path, b"private test credential").unwrap();
    let secret = File::open(&secret_path).unwrap();
    let secret_metadata = secret.metadata().unwrap();
    let (mut ready_parent, ready_child) = UnixStream::pair().unwrap();
    let (mut control_parent, control_child) = UnixStream::pair().unwrap();
    ready_parent
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let identity = target.identity();
    let (device, inode) = match identity {
        MediaIdentity::Unix { device, inode } => (device, inode),
        _ => unreachable!(),
    };
    let transport = target.helper_transport().unwrap();
    let target_fd = transport.descriptor.as_raw_fd();
    let executable = std::env::current_exe().unwrap();
    let mut env = BTreeMap::from([
        ("DESCRIPTOR_PROBE_MODE".into(), "intended".into()),
        ("TARGET_FD".into(), target_fd.to_string().into()),
        ("SECRET_FD".into(), secret.as_raw_fd().to_string().into()),
        (
            "READY_FD".into(),
            ready_child.as_raw_fd().to_string().into(),
        ),
        (
            "CONTROL_FD".into(),
            control_child.as_raw_fd().to_string().into(),
        ),
    ]);
    let request = Request {
        program: executable.as_os_str().to_owned(),
        argv: vec![
            "--exact".into(),
            "descriptor_probe".into(),
            "--nocapture".into(),
        ],
        env: env.clone(),
        timeout: Some(Duration::from_secs(5)),
        pass_fds: vec![
            transport.descriptor,
            secret.as_fd(),
            ready_child.as_fd(),
            control_child.as_fd(),
        ],
    };
    assert!(cloexec(transport.descriptor));
    assert!(cloexec(&secret));
    std::thread::scope(|scope| {
        let intended = scope.spawn(|| run(&request).unwrap());
        let mut ready = [0u8];
        ready_parent.read_exact(&mut ready).unwrap();
        assert_eq!(ready, [b'r']);
        assert!(cloexec(transport.descriptor));
        assert!(cloexec(&secret));
        env.insert("DESCRIPTOR_PROBE_MODE".into(), "unrelated".into());
        env.insert("TARGET_DEV".into(), device.to_string().into());
        env.insert("TARGET_INO".into(), inode.to_string().into());
        env.insert(
            "SECRET_DEV".into(),
            secret_metadata.dev().to_string().into(),
        );
        env.insert(
            "SECRET_INO".into(),
            secret_metadata.ino().to_string().into(),
        );
        let unrelated = Command::new(&executable)
            .args(["--exact", "descriptor_probe", "--nocapture"])
            .env_clear()
            .envs(&env)
            .output()
            .unwrap();
        assert!(
            unrelated.status.success(),
            "{}",
            String::from_utf8_lossy(&unrelated.stderr)
        );
        control_parent.write_all(b"g").unwrap();
        assert_eq!(intended.join().unwrap().returncode, 0);
    });
    assert!(cloexec(transport.descriptor));
    assert!(cloexec(&secret));
    drop(transport);
    let digest = digest_reader(&mut std::io::Cursor::new(b"intended owned bytes")).unwrap();
    let mut prepared = target.prepare_after_successful_helper(&digest).unwrap();
    assert_eq!(prepared.digest(), &digest);
    let _ = prepared.cleanup();
}

#[test]
fn spawn_failure_preserves_all_parent_descriptor_flags() {
    let directory = tempfile::tempdir().unwrap();
    let first = File::create(directory.path().join("first")).unwrap();
    let second = File::create(directory.path().join("second")).unwrap();
    let result = run(&Request {
        program: directory.path().join("missing").into_os_string(),
        argv: vec![],
        env: BTreeMap::new(),
        timeout: Some(Duration::from_secs(1)),
        pass_fds: vec![first.as_fd(), second.as_fd()],
    });
    assert!(result.is_err());
    assert!(cloexec(first));
    assert!(cloexec(second));
}

#[test]
fn timeout_retains_partial_output_and_parent_descriptor_flags() {
    let directory = tempfile::tempdir().unwrap();
    let secret = File::create(directory.path().join("secret")).unwrap();
    let output = run(&Request {
        program: "/bin/sh".into(),
        argv: vec!["-c".into(), "printf partial; sleep 5".into()],
        env: BTreeMap::new(),
        timeout: Some(Duration::from_millis(100)),
        pass_fds: vec![secret.as_fd()],
    })
    .unwrap();
    assert_eq!(output.returncode, 124);
    assert_eq!(output.stdout, b"partial");
    assert!(cloexec(secret));
}

/// Explicit operator target: execute this compiled harness from an admitted
/// copied package with the real restic member and a synthetic direct-S3 fixture.
#[test]
#[ignore = "requires an admitted copied package and synthetic direct S3 fixture"]
fn packaged_readback_entry() {
    use solstone_core_backup::archive_admission::{ArchiveBudget, ArchiveConfiguration, Clock};
    use solstone_core_backup::archive_readback::{
        ExactArchiveFile, ReadAdmission, ReadOperation, ReadbackError, ReadbackFailure,
    };
    use solstone_core_journal_io::media::{Cleanup, PreparedMedia};
    fn refused(result: Result<PreparedMedia, ReadbackFailure>) -> ReadbackFailure {
        match result {
            Err(failure) => failure,
            Ok(_) => panic!("readback should refuse"),
        }
    }
    struct ProbeClock(std::cell::Cell<i64>);
    impl Clock for ProbeClock {
        fn now_unix(&self) -> i64 {
            self.0.get()
        }
        fn iso_week(&self) -> u8 {
            1
        }
    }
    let input_path =
        std::env::var_os("READBACK_PROBE_JSON").expect("explicit probe input required");
    let input: serde_json::Value = serde_json::from_slice(&fs::read(input_path).unwrap()).unwrap();
    let root = std::sync::Arc::new(
        JournalRoot::open(Path::new(input["journal"].as_str().unwrap())).unwrap(),
    );
    let configuration = ArchiveConfiguration::capture(&root).unwrap();
    let destination = configuration.destination().unwrap();
    let keys = configuration.keys().unwrap().unwrap();
    let member =
        solstone_core_installed_payload::resolve_current_member("lib/solstone-restic/restic")
            .unwrap();
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let clock = ProbeClock(std::cell::Cell::new(started));
    let refused_admission = |repository_id: String, expected_restic_path: std::path::PathBuf| {
        match ReadOperation::admit(ReadAdmission {
            root: root.clone(),
            configuration: ArchiveConfiguration::capture(&root).unwrap(),
            budget: ArchiveBudget::new(&clock).unwrap(),
            clock: &clock,
            destination: destination.clone(),
            password: keys.daily_key.clone(),
            repository_id,
            expected_restic_path,
        }) {
            Err(error) => error,
            Ok(_) => panic!("archive admission should refuse"),
        }
    };
    assert_eq!(
        refused_admission(
            input["repository_id"].as_str().unwrap().into(),
            member.path().with_file_name("other-restic")
        ),
        ReadbackError::ToolChanged
    );
    let mismatched_repository = if input["repository_id"].as_str().unwrap().starts_with('d') {
        "e".repeat(64)
    } else {
        "d".repeat(64)
    };
    assert_eq!(
        refused_admission(mismatched_repository, member.path().into()),
        ReadbackError::RepositoryIdentityChanged
    );
    let mut operation = ReadOperation::admit(ReadAdmission {
        root: root.clone(),
        configuration,
        budget: ArchiveBudget::new(&clock).unwrap(),
        clock: &clock,
        destination,
        password: keys.daily_key,
        repository_id: input["repository_id"].as_str().unwrap().into(),
        expected_restic_path: member.path().into(),
    })
    .unwrap();
    let parent = MediaDirectory::open(root.clone(), Path::new("media")).unwrap();
    let fresh = || {
        MediaStage::create(parent.clone())
            .unwrap()
            .create_target()
            .unwrap()
    };
    let mut file = ExactArchiveFile {
        repository_id: input["repository_id"].as_str().unwrap().into(),
        snapshot_id: input["snapshot_id"].as_str().unwrap().into(),
        name: input["name"].as_str().unwrap().into(),
        archive_path: input["archive_path"].as_str().unwrap().into(),
        bytes: input["bytes"].as_u64().unwrap(),
        sha256: input["sha256"].as_str().unwrap().into(),
    };
    let mut prepared = operation.read_exact(&file, fresh()).unwrap();
    assert_eq!(prepared.digest().size, file.bytes);
    assert_eq!(prepared.digest().sha256, file.sha256);
    assert_eq!(prepared.cleanup(), Cleanup::Complete);
    let correct_sha = file.sha256.clone();
    file.sha256 = if correct_sha.starts_with('0') {
        "1".repeat(64)
    } else {
        "0".repeat(64)
    };
    let mut wrong = refused(operation.read_exact(&file, fresh()));
    assert_eq!(wrong.reason(), ReadbackError::NativeFailure);
    assert_eq!(wrong.cleanup(), Cleanup::Complete);
    file.sha256 = correct_sha;
    let source_path = file.archive_path.clone();
    file.archive_path = input["directory_path"].as_str().unwrap().into();
    file.name = file.archive_path.rsplit('/').next().unwrap().into();
    let mut nonfile = refused(operation.read_exact(&file, fresh()));
    assert_eq!(nonfile.reason(), ReadbackError::SnapshotNodeUnavailable);
    assert_eq!(nonfile.cleanup(), Cleanup::Complete);
    file.archive_path = source_path;
    file.name = input["name"].as_str().unwrap().into();
    let mut issued = fresh();
    drop(issued.helper_transport().unwrap());
    let mut reused = refused(operation.read_exact(&file, issued));
    assert_eq!(reused.reason(), ReadbackError::NativeFailure);
    assert_eq!(reused.cleanup(), Cleanup::Refused);
    file.snapshot_id = "latest".into();
    let mut invalid = refused(operation.read_exact(&file, fresh()));
    assert_eq!(invalid.reason(), ReadbackError::InvalidSelection);
    assert_eq!(invalid.cleanup(), Cleanup::Complete);
    file.snapshot_id = input["snapshot_id"].as_str().unwrap().into();
    let correct_repository = file.repository_id.clone();
    file.repository_id = if correct_repository.starts_with('d') {
        "e".repeat(64)
    } else {
        "d".repeat(64)
    };
    let mut other_repository = refused(operation.read_exact(&file, fresh()));
    assert_eq!(
        other_repository.reason(),
        ReadbackError::RepositoryIdentityChanged
    );
    assert_eq!(other_repository.cleanup(), Cleanup::Complete);
    file.repository_id = correct_repository;
    let correct_snapshot = file.snapshot_id.clone();
    file.snapshot_id = "f".repeat(64);
    let mut missing = refused(operation.read_exact(&file, fresh()));
    assert_eq!(missing.reason(), ReadbackError::ToolFailed);
    assert_eq!(missing.cleanup(), Cleanup::Complete);
    file.snapshot_id = correct_snapshot;
    let config_path = Path::new(input["journal"].as_str().unwrap()).join("config/journal.json");
    let original_config = fs::read(&config_path).unwrap();
    let mut changed_config: serde_json::Value = serde_json::from_slice(&original_config).unwrap();
    changed_config["backup"]["enabled"] = false.into();
    fs::write(&config_path, serde_json::to_vec(&changed_config).unwrap()).unwrap();
    let mut configuration_changed = refused(operation.read_exact(&file, fresh()));
    assert_eq!(
        configuration_changed.reason(),
        ReadbackError::Admission(
            solstone_core_backup::archive_admission::AdmissionError::ConfigurationChanged
        )
    );
    assert_eq!(configuration_changed.cleanup(), Cleanup::Complete);
    fs::write(&config_path, original_config).unwrap();
    let original_member = fs::read(member.path()).unwrap();
    let mut changed_member = original_member.clone();
    *changed_member.last_mut().unwrap() ^= 1;
    fs::write(member.path(), changed_member).unwrap();
    let mut member_changed = refused(operation.read_exact(&file, fresh()));
    assert_eq!(member_changed.reason(), ReadbackError::ToolChanged);
    assert_eq!(member_changed.cleanup(), Cleanup::Complete);
    fs::write(member.path(), original_member).unwrap();
    clock.0.set(
        started + solstone_core_backup::archive_admission::ARCHIVE_BACKUP_TIMEOUT_SECONDS as i64
            - 60,
    );
    let mut near_deadline = operation.read_exact(&file, fresh()).unwrap();
    assert_eq!(near_deadline.cleanup(), Cleanup::Complete);
    clock.0.set(
        started + solstone_core_backup::archive_admission::ARCHIVE_BACKUP_TIMEOUT_SECONDS as i64,
    );
    let mut exhausted = refused(operation.read_exact(&file, fresh()));
    assert_eq!(
        exhausted.reason(),
        ReadbackError::Admission(
            solstone_core_backup::archive_admission::AdmissionError::DeadlineExceeded
        )
    );
    assert_eq!(exhausted.cleanup(), Cleanup::Complete);
    println!(
        "direct-S3 exact readback and digest/node/repository/snapshot/config/member/deadline refusals passed; fresh cleanup and issued-target retention passed; member={} bytes={} sha256={}",
        member.relative_path(),
        member.bytes(),
        member.sha256()
    );
}
