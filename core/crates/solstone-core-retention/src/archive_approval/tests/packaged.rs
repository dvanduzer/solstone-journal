// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Explicit copied-package controls. The owning operator fixture supplies actual
//! process/transport/HTTP observations; these assertions alone do not certify
//! production cooperation, settlement or release authority.

use std::cell::Cell;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use solstone_core_backup::archive_admission::{
    ARCHIVE_BACKUP_TIMEOUT_SECONDS, AdmissionError, ArchiveBudget, ArchiveCancellation,
    ArchiveConfiguration, Clock,
};
use solstone_core_backup::archive_readback::ReadAdmission;
use solstone_core_journal_io::media::{MediaPrimitive, run_with_media_hook};

use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    case: String,
    journal: PathBuf,
    repository_id: String,
    expected_restic_path: PathBuf,
    marks: Vec<Mark>,
    control: Option<PathBuf>,
}

struct ProbeClock(Rc<Cell<i64>>);

impl Clock for ProbeClock {
    fn now_unix(&self) -> i64 {
        self.0.get()
    }

    fn iso_week(&self) -> u8 {
        1
    }
}

fn stages(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".tmp-offload-")
        {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

fn payloads(root: &Path) -> io::Result<Vec<(PathBuf, MediaIdentity, u64)>> {
    let mut found = Vec::new();
    for stage in stages(root)? {
        for entry in fs::read_dir(stage)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            found.push((
                entry.path(),
                MediaIdentity::Unix {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                },
                metadata.len(),
            ));
        }
    }
    found.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(found)
}

fn batch<'a>(
    result: Result<ArchiveApprovalBatch<'a>, ArchiveApprovalFailure<'a>>,
) -> TestResult<ArchiveApprovalBatch<'a>> {
    match result {
        Ok(batch) => Ok(batch),
        Err(failure) => {
            Err(invalid(&format!("expected exact readback: {:?}", failure.reason())).into())
        }
    }
}

fn failure<'a>(
    result: Result<ArchiveApprovalBatch<'a>, ArchiveApprovalFailure<'a>>,
) -> TestResult<ArchiveApprovalFailure<'a>> {
    match result {
        Err(failure) => Ok(failure),
        Ok(_) => Err(invalid("expected owning refusal").into()),
    }
}

fn check_owner(
    owner: &ApprovalOwner<'_>,
    root: &Arc<JournalRoot>,
    activity: &Arc<ActivityLease>,
    marks: &[Mark],
) {
    assert!(Arc::ptr_eq(owner.read.journal_root(), root));
    assert!(Arc::ptr_eq(owner.carrier.activity(), activity));
    assert_eq!(owner.expected, marks);
}

fn check_prepared(prepared: &PreparedRead) {
    assert_eq!(prepared.target_identity, prepared.media.identity());
    assert_eq!(prepared.file.bytes, prepared.media.digest().size);
    assert_eq!(prepared.file.sha256, prepared.media.digest().sha256);
}

fn check_complete(owner: &ApprovalOwner<'_>, count: usize) -> TestResult {
    assert_eq!(owner.held.len(), count);
    for resource in &owner.held {
        match resource {
            HeldResource::Prepared(prepared) => {
                assert_eq!(prepared.cleanup, Some(Cleanup::Complete));
                check_prepared(prepared);
            }
            _ => {
                return Err(invalid("successful batch must retain original preparations").into());
            }
        }
    }
    Ok(())
}

fn evidence(
    phase: &str,
    owner: &ApprovalOwner<'_>,
    reason: Option<&ArchiveApprovalError>,
) -> TestResult {
    let held: Vec<_> = owner
        .held
        .iter()
        .map(|resource| match resource {
            HeldResource::Prepared(prepared) => serde_json::json!({
                "kind": "prepared", "repository": prepared.file.repository_id,
                "snapshot": prepared.file.snapshot_id, "path": prepared.file.archive_path,
                "name": prepared.file.name, "identity": prepared.target_identity,
                "cleanup": prepared.cleanup, "digest": prepared.media.digest(),
            }),
            HeldResource::Readback(failed) => serde_json::json!({
                "kind": "readback_failure", "repository": failed.file.repository_id,
                "snapshot": failed.file.snapshot_id, "path": failed.file.archive_path,
                "name": failed.file.name, "identity": failed.target_identity,
                "reason": format!("{:?}", failed.failure.reason()),
            }),
            HeldResource::Stage(stage) => {
                serde_json::json!({"kind": "stage", "present_owner": stage.is_some()})
            }
            HeldResource::Target(target) => {
                serde_json::json!({"kind": "target", "identity": target.identity()})
            }
        })
        .collect();
    println!(
        "ARCHIVE_APPROVAL_EVIDENCE {}",
        serde_json::json!({
            "phase": phase, "reason": reason.map(|reason| format!("{reason:?}")),
            "held": held, "stages": stages(owner.carrier.journal_root().canonical_path())?,
        })
    );
    std::io::stdout().flush()?;
    Ok(())
}

fn check_drop(failed: ArchiveApprovalFailure<'_>, root: &Path) -> TestResult {
    let before_stages = stages(root)?;
    let before_payloads = payloads(root)?;
    drop(failed);
    assert_eq!(stages(root)?, before_stages);
    assert_eq!(payloads(root)?, before_payloads);
    Ok(())
}

fn check_failed_read(owner: &ApprovalOwner<'_>, file: &ExactArchiveFile) -> TestResult {
    let failed = owner
        .held
        .iter()
        .find_map(|resource| match resource {
            HeldResource::Readback(failed) => Some(failed),
            _ => None,
        })
        .ok_or_else(|| invalid("actual owning readback failure missing"))?;
    assert_eq!(failed.file.name, file.name);
    assert_eq!(failed.file.archive_path, file.archive_path);
    assert_eq!(failed.file.snapshot_id, file.snapshot_id);
    assert!(
        payloads(owner.carrier.journal_root().canonical_path())?
            .iter()
            .any(|(_, identity, _)| *identity == failed.target_identity)
    );
    Ok(())
}

#[test]
#[ignore = "requires an admitted copied package and owning synthetic process/HTTP fixture"]
fn packaged_archive_approval_entry() -> TestResult {
    let input: Input = serde_json::from_slice(&fs::read(
        std::env::var_os("ARCHIVE_APPROVAL_PROBE_JSON")
            .ok_or_else(|| invalid("explicit probe input required"))?,
    )?)?;
    let root = Arc::new(JournalRoot::open(&input.journal)?);
    let activity_root = if input.case == "root_arc" {
        Arc::new(JournalRoot::open(&input.journal)?)
    } else {
        root.clone()
    };
    let activity = ActivityLease::try_acquire(activity_root)?;
    let carrier = match RootActivityCarrier::prepare(activity.clone()) {
        Ok(carrier) => carrier,
        Err(failure) => {
            return Err(invalid(&format!("carrier preparation failed: {failure:?}")).into());
        }
    };
    let carrier_path = carrier.diagnostic_path();
    let started = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())?;
    let clock = ProbeClock(Rc::new(Cell::new(started)));
    let expiry = started
        .checked_add(i64::try_from(ARCHIVE_BACKUP_TIMEOUT_SECONDS)?)
        .ok_or_else(|| invalid("fixture deadline overflow"))?;
    let cancellation = ArchiveCancellation::new();
    let configuration = ArchiveConfiguration::capture(&root)?;
    let destination = configuration
        .destination()
        .ok_or_else(|| invalid("fixture destination missing"))?;
    let password = configuration
        .keys()?
        .ok_or_else(|| invalid("fixture keys missing"))?
        .daily_key;
    let read = ReadOperation::admit(ReadAdmission {
        root: root.clone(),
        configuration,
        budget: ArchiveBudget::new_cancellable(&clock, cancellation.clone())?,
        clock: &clock,
        destination,
        password,
        repository_id: input.repository_id.clone(),
        expected_restic_path: input.expected_restic_path,
    })?;
    let files = selected_files(
        &input.marks,
        &root.identity().observation_id(),
        &input.repository_id,
    )
    .map_err(|reason| invalid(&format!("fixture marks invalid: {reason:?}")))?;
    assert_eq!(stages(&input.journal)?, vec![carrier_path.clone()]);
    // An owning observer acknowledges this actual post-admission cut so its
    // process/HTTP deltas exclude the real admission query without guessing.
    if let Some(path) = &input.control {
        let mut control = UnixStream::connect(path)?;
        control.set_read_timeout(Some(Duration::from_secs(30)))?;
        control.write_all(b"admitted")?;
        let mut acknowledged = [0_u8];
        control.read_exact(&mut acknowledged)?;
        assert_eq!(acknowledged, [b'a']);
    }

    if input.case == "positive" || input.case == "calibrated_digest" {
        assert!(files.len() >= 2);
        let observed_root = input.journal.clone();
        let observed_carrier = carrier_path.clone();
        let (result, consumed) = run_with_media_hook(
            MediaPrimitive::StageRemove,
            move || {
                let allocated = stages(&observed_root)?;
                assert_eq!(allocated.len(), 2);
                assert!(allocated.contains(&observed_carrier));
                assert!(payloads(&observed_root)?.is_empty());
                Ok(())
            },
            || create_archive_approval_batch(read, carrier, &input.marks),
        );
        assert!(consumed);
        let positive = batch(result)?;
        check_owner(&positive.owner, &root, &activity, &input.marks);
        assert!(Arc::ptr_eq(positive.journal_root(), &root));
        assert!(Arc::ptr_eq(positive.activity(), &activity));
        assert_eq!(positive.expected_marks(), input.marks);
        positive
            .revalidate_admission()
            .map_err(|reason| invalid(&format!("positive recheck: {reason:?}")))?;
        check_complete(&positive.owner, files.len())?;
        assert_eq!(stages(&input.journal)?, vec![carrier_path.clone()]);
        evidence("positive", &positive.owner, None)?;
        if input.case == "positive" {
            drop(positive);
            assert_eq!(stages(&input.journal)?, vec![carrier_path]);
            return Ok(());
        }
        assert_eq!(files.len(), 3);
        let mut changed = input.marks.clone();
        let mut ordinal = 0_usize;
        for mark in &mut changed {
            let old = mark
                .offload
                .take()
                .ok_or_else(|| invalid("offer missing"))?;
            let mut proof = old.proof;
            for file in &mut proof.files {
                if ordinal == 1 {
                    file.sha256 = if file.sha256 == "d".repeat(64) {
                        "e".repeat(64)
                    } else {
                        "d".repeat(64)
                    };
                }
                ordinal = ordinal
                    .checked_add(1)
                    .ok_or_else(|| invalid("ordinal overflow"))?;
            }
            mark.offload = Some(OffloadOffer::with_nonce(proof, old.nonce)?);
        }
        let ApprovalOwner {
            read,
            carrier,
            expected,
            held,
        } = *positive.owner;
        let positive_history = (expected, held);
        let failed = failure(create_archive_approval_batch(read, carrier, &changed))?;
        assert!(matches!(
            failed.reason(),
            ArchiveApprovalError::Readback(ReadbackError::NativeFailure)
        ));
        check_owner(&failed.owner, &root, &activity, &changed);
        assert_eq!(failed.owner.held.len(), 2);
        let first = failed
            .owner
            .held
            .first()
            .ok_or_else(|| invalid("first owner missing"))?;
        assert!(
            matches!(first, HeldResource::Prepared(prepared) if prepared.cleanup == Some(Cleanup::Complete))
        );
        let second = files
            .get(1)
            .ok_or_else(|| invalid("second tuple missing"))?;
        check_failed_read(&failed.owner, second)?;
        assert_eq!(positive_history.0, input.marks);
        assert_eq!(positive_history.1.len(), 3);
        evidence(
            "changed_second_digest",
            &failed.owner,
            Some(failed.reason()),
        )?;
        return check_drop(failed, &input.journal);
    }

    if input.case == "launched_cancel" {
        assert!(files.len() >= 2);
        let mut control = UnixStream::connect(
            input
                .control
                .as_ref()
                .ok_or_else(|| invalid("control socket missing"))?,
        )?;
        control.set_read_timeout(Some(Duration::from_secs(30)))?;
        control.write_all(b"dump")?;
        let mut armed = [0_u8];
        control.read_exact(&mut armed)?;
        assert_eq!(armed, [b'a']);
        let signal = cancellation.clone();
        let (result, mut control) = std::thread::scope(|scope| -> TestResult<_> {
            let cancel = scope.spawn(move || -> io::Result<UnixStream> {
                let mut reached = [0_u8; 5];
                control.read_exact(&mut reached)?;
                assert_eq!(reached.first(), Some(&b'c'));
                signal.cancel();
                control.write_all(b"d")?;
                Ok(control)
            });
            let result = create_archive_approval_batch(read, carrier, &input.marks);
            let control = cancel
                .join()
                .map_err(|_| invalid("cancellation observer failed"))??;
            Ok((result, control))
        })?;
        let failed = failure(result)?;
        assert!(matches!(
            failed.reason(),
            ArchiveApprovalError::Readback(ReadbackError::Admission(AdmissionError::Cancelled))
        ));
        check_owner(&failed.owner, &root, &activity, &input.marks);
        assert_eq!(failed.owner.held.len(), 1);
        check_failed_read(
            &failed.owner,
            files
                .first()
                .ok_or_else(|| invalid("first tuple missing"))?,
        )?;
        assert_eq!(stages(&input.journal)?.len(), 2);
        evidence(
            "launched_cancel_response_blocked",
            &failed.owner,
            Some(failed.reason()),
        )?;
        control.write_all(b"r")?;
        control.read_exact(&mut armed)?;
        assert_eq!(armed, [b'r']);
        return check_drop(failed, &input.journal);
    }

    let mut expected = input.marks.clone();
    if input.case == "invalid_later" {
        assert!(expected.len() >= 2);
        expected
            .get_mut(1)
            .ok_or_else(|| invalid("later row missing"))?
            .proposal
            .bytes = 0;
    }
    if input.case == "pre_cancel" {
        cancellation.cancel();
    }
    if input.case == "pre_expire" {
        clock.0.set(expiry);
    }

    let (result, consumed) = match input.case.as_str() {
        "stage_fail" => run_with_media_hook(
            MediaPrimitive::StageCreated,
            || Err(io::Error::from_raw_os_error(28)),
            || create_archive_approval_batch(read, carrier, &expected),
        ),
        "target_fail" => {
            let observed_root = input.journal.clone();
            let observed_carrier = carrier_path.clone();
            run_with_media_hook(
                MediaPrimitive::StageCreated,
                move || {
                    let allocated = stages(&observed_root)?;
                    let stage = allocated
                        .iter()
                        .find(|stage| **stage != observed_carrier)
                        .ok_or_else(|| invalid("actual reservation missing"))?;
                    fs::write(stage.join("payload"), b"native payload collision")
                },
                || create_archive_approval_batch(read, carrier, &expected),
            )
        }
        "cleanup_pending" => {
            assert!(files.len() >= 2);
            run_with_media_hook(
                MediaPrimitive::StageUnlink,
                || Err(io::Error::from_raw_os_error(5)),
                || create_archive_approval_batch(read, carrier, &expected),
            )
        }
        "cleanup_refused" => {
            assert!(files.len() >= 2);
            let observed_root = input.journal.clone();
            run_with_media_hook(
                MediaPrimitive::StageRemove,
                move || {
                    assert!(payloads(&observed_root)?.is_empty());
                    assert_eq!(stages(&observed_root)?.len(), 2);
                    Err(io::Error::from(io::ErrorKind::InvalidData))
                },
                || create_archive_approval_batch(read, carrier, &expected),
            )
        }
        "cleanup_expire_first" | "final_expire" | "final_cancel" | "final_unchanged" => {
            if input.case == "cleanup_expire_first" {
                assert!(files.len() >= 2);
            } else {
                assert_eq!(files.len(), 1);
            }
            let observed_clock = clock.0.clone();
            let observed_cancellation = cancellation.clone();
            let selected_case = input.case.clone();
            run_with_media_hook(
                MediaPrimitive::StageRemove,
                move || {
                    if selected_case == "final_cancel" {
                        observed_cancellation.cancel();
                    }
                    if selected_case != "final_cancel" && selected_case != "final_unchanged" {
                        observed_clock.set(expiry);
                    }
                    Ok(())
                },
                || create_archive_approval_batch(read, carrier, &expected),
            )
        }
        "root_arc" | "invalid_later" | "pre_cancel" | "pre_expire" | "batch_cancel"
        | "batch_expire" => (
            create_archive_approval_batch(read, carrier, &expected),
            false,
        ),
        _ => return Err(invalid("unknown explicit probe case").into()),
    };

    if input.case == "final_unchanged"
        || input.case == "batch_cancel"
        || input.case == "batch_expire"
    {
        let positive = batch(result)?;
        check_complete(&positive.owner, files.len())?;
        check_owner(&positive.owner, &root, &activity, &expected);
        if input.case == "batch_cancel" {
            cancellation.cancel();
        }
        if input.case == "batch_expire" {
            clock.0.set(expiry);
        }
        if input.case == "final_unchanged" {
            assert!(consumed);
            positive
                .revalidate_admission()
                .map_err(|reason| invalid(&format!("positive recheck: {reason:?}")))?;
        } else {
            let reason = positive
                .revalidate_admission()
                .err()
                .ok_or_else(|| invalid("original recheck must refuse"))?;
            let expected_error = if input.case == "batch_cancel" {
                AdmissionError::Cancelled
            } else {
                AdmissionError::DeadlineExceeded
            };
            assert!(matches!(
                reason,
                ArchiveApprovalError::Readback(ReadbackError::Admission(actual))
                    if actual == expected_error
            ));
        }
        evidence(&input.case, &positive.owner, None)?;
        assert_eq!(stages(&input.journal)?, vec![carrier_path]);
        return Ok(());
    }

    let failed = failure(result)?;
    check_owner(&failed.owner, &root, &activity, &expected);
    assert!(Arc::ptr_eq(failed.activity(), &activity));
    assert_eq!(failed.expected_marks(), expected);
    match input.case.as_str() {
        "stage_fail" | "target_fail" => {
            assert!(consumed);
            let error = match failed.reason() {
                ArchiveApprovalError::Native(error) => error,
                _ => return Err(invalid("native creation error missing").into()),
            };
            if input.case == "stage_fail" {
                assert_eq!(error.raw_os_error(), Some(28));
            } else {
                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            }
            assert_eq!(failed.owner.held.len(), 1);
            assert!(
                matches!(failed.owner.held.first(), Some(HeldResource::Stage(Some(stage))) if stage.diagnostic_path().is_dir())
            );
            assert_eq!(stages(&input.journal)?.len(), 2);
            if input.case == "target_fail" {
                let allocated = stages(&input.journal)?;
                let stage = allocated
                    .iter()
                    .find(|stage| **stage != carrier_path)
                    .ok_or_else(|| invalid("collision stage missing"))?;
                assert_eq!(
                    fs::read(stage.join("payload"))?,
                    b"native payload collision"
                );
            }
        }
        "cleanup_pending" | "cleanup_refused" => {
            assert!(consumed);
            let expected_cleanup = if input.case == "cleanup_pending" {
                Cleanup::Pending
            } else {
                Cleanup::Refused
            };
            assert!(
                matches!(failed.reason(), ArchiveApprovalError::Cleanup(actual) if *actual == expected_cleanup)
            );
            assert_eq!(failed.owner.held.len(), 1);
            assert!(
                matches!(failed.owner.held.first(), Some(HeldResource::Prepared(prepared)) if prepared.cleanup == Some(expected_cleanup) && prepared.target_identity == prepared.media.identity())
            );
            match failed.owner.held.first() {
                Some(HeldResource::Prepared(prepared)) => check_prepared(prepared),
                _ => return Err(invalid("uncertain original preparation missing").into()),
            }
            assert_eq!(stages(&input.journal)?.len(), 2);
            if input.case == "cleanup_pending" {
                assert_eq!(payloads(&input.journal)?.len(), 1);
            } else {
                assert!(payloads(&input.journal)?.is_empty());
            }
        }
        "cleanup_expire_first" | "final_expire" | "final_cancel" => {
            assert!(consumed);
            let expected_error = if input.case == "final_cancel" {
                AdmissionError::Cancelled
            } else {
                AdmissionError::DeadlineExceeded
            };
            assert!(
                matches!(failed.reason(), ArchiveApprovalError::Readback(ReadbackError::Admission(actual)) if *actual == expected_error)
            );
            check_complete(&failed.owner, 1)?;
            assert_eq!(stages(&input.journal)?, vec![carrier_path]);
        }
        "root_arc" => {
            assert!(matches!(failed.reason(), ArchiveApprovalError::RootChanged));
            assert!(!Arc::ptr_eq(failed.journal_root(), &root));
            assert!(failed.owner.held.is_empty());
        }
        "invalid_later" => {
            assert!(matches!(
                failed.reason(),
                ArchiveApprovalError::InvalidSelection
            ));
            assert!(failed.owner.held.is_empty());
        }
        "pre_cancel" => {
            assert!(matches!(
                failed.reason(),
                ArchiveApprovalError::Readback(ReadbackError::Admission(AdmissionError::Cancelled))
            ));
            assert!(failed.owner.held.is_empty());
        }
        "pre_expire" => {
            assert!(matches!(
                failed.reason(),
                ArchiveApprovalError::Readback(ReadbackError::Admission(
                    AdmissionError::DeadlineExceeded
                ))
            ));
            assert!(failed.owner.held.is_empty());
        }
        _ => return Err(invalid("unexpected refusal case").into()),
    }
    evidence(&input.case, &failed.owner, Some(failed.reason()))?;
    check_drop(failed, &input.journal)
}
