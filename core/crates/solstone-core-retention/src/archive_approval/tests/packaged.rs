// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Explicit copied-package controls. The owning operator fixture supplies actual
//! process/transport/HTTP observations; these assertions alone do not certify
//! production cooperation, settlement or release authority.

#![cfg(all(test, feature = "full-tests"))]
#![cfg(unix)]

use std::cell::Cell;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use solstone_core_backup::archive_activity::PreparedArchiveOperation;
use solstone_core_backup::archive_admission::{
    ARCHIVE_BACKUP_TIMEOUT_SECONDS, AdmissionError, ArchiveBudget, ArchiveCancellation, Clock,
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
    let started = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())?;
    let clock = ProbeClock(Rc::new(Cell::new(started)));
    let expiry = started
        .checked_add(i64::try_from(ARCHIVE_BACKUP_TIMEOUT_SECONDS)?)
        .ok_or_else(|| invalid("fixture deadline overflow"))?;
    let cancellation = ArchiveCancellation::new();
    let operation = PreparedArchiveOperation::prepare(
        root.clone(),
        ArchiveBudget::new_cancellable(&clock, cancellation.clone())?,
        &clock,
    )
    .map_err(|error| invalid(&format!("actual original preparation: {error:?}")))?;
    let destination = operation
        .configuration()
        .destination()
        .ok_or_else(|| invalid("fixture destination missing"))?;
    let password = operation
        .configuration()
        .keys()?
        .ok_or_else(|| invalid("fixture keys missing"))?
        .daily_key;
    let read = ReadOperation::admit(ReadAdmission::bind(
        operation,
        destination,
        password,
        input.repository_id.clone(),
        input.expected_restic_path,
    ))
    .map_err(|error| invalid(&format!("actual read admission: {error:?}")))?;
    let admitted_carrier = Arc::clone(read.carrier());
    let carrier = if input.case == "root_arc" {
        let foreign_root = Arc::new(JournalRoot::open(&input.journal)?);
        let foreign_activity = ActivityLease::try_acquire(foreign_root)?;
        Arc::new(
            RootActivityCarrier::prepare(foreign_activity)
                .map_err(|error| invalid(&format!("foreign carrier: {error:?}")))?,
        )
    } else {
        Arc::clone(&admitted_carrier)
    };
    let activity = Arc::clone(carrier.activity());
    let carrier_path = carrier.diagnostic_path();
    let mut original_stages = vec![carrier_path.clone()];
    if input.case == "root_arc" {
        original_stages.push(admitted_carrier.diagnostic_path());
        original_stages.sort();
    }
    let files = selected_files(
        &input.marks,
        &root.identity().observation_id(),
        &input.repository_id,
    )
    .map_err(|reason| invalid(&format!("fixture marks invalid: {reason:?}")))?;
    assert_eq!(stages(&input.journal)?, original_stages);
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

// Additive parent controls: these consume only real actor return values. The
// original leaf evidence remains frozen; its setup above follows the original
// operation ABI. Production qualification is held.
struct ParentFixture {
    mark: Mark,
    directory: Arc<MediaDirectory>,
    raw: Vec<PathBuf>,
    sidecars: Vec<(PathBuf, Vec<u8>, Vec<u8>)>,
    ledger: PathBuf,
    register_bytes: Vec<u8>,
}

fn parent_fixture(input: &Input, root: &Arc<JournalRoot>) -> TestResult<ParentFixture> {
    assert_eq!(input.marks.len(), 2);
    parent_fixture_for(input, root, &input.marks)
}

fn parent_fixture_for(
    input: &Input,
    root: &Arc<JournalRoot>,
    marks: &[Mark],
) -> TestResult<ParentFixture> {
    use crate::archive_catalog::codec::Event;
    use crate::content::{ClosedHandlerSet, JournalMedia};
    use crate::layout::{day_health_rel, segment_rel};

    let mut configuration: serde_json::Value =
        serde_json::from_slice(&fs::read(input.journal.join("config/journal.json"))?)?;
    configuration
        .as_object_mut()
        .ok_or_else(|| invalid("fixture configuration must be an object"))?
        .insert(
            "retention".into(),
            serde_json::json!({"raw_media":"days","raw_media_days":30}),
        );
    fs::write(
        input.journal.join("config/journal.json"),
        serde_json::to_vec(&configuration)?,
    )?;
    let first = marks
        .first()
        .and_then(|mark| mark.offload.as_ref())
        .ok_or_else(|| invalid("actual first offer missing"))?;
    let mut proof = first.proof.clone();
    proof.files.clear();
    for mark in marks {
        let offered = mark
            .offload
            .as_ref()
            .ok_or_else(|| invalid("actual offered file missing"))?;
        assert_eq!(offered.proof.files.len(), 1);
        proof.files.extend(offered.proof.files.iter().cloned());
    }
    proof.files.sort_by(|a, b| a.name.cmp(&b.name));
    let relative = segment_rel(&proof.target.day, &proof.target.stream, &proof.target.dir);
    let segment = input.journal.join(&relative);
    fs::create_dir_all(&segment)?;
    fs::create_dir_all(input.journal.join(day_health_rel(&proof.target.day)))?;
    fs::create_dir_all(input.journal.join("health/offload"))?;
    let mut raw = Vec::new();
    let mut sidecars = Vec::new();
    for file in &proof.files {
        let destination = segment.join(&file.name);
        fs::write(&destination, fs::read(&file.archive_path)?)?;
        let stem = Path::new(&file.name)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| invalid("actual media stem missing"))?;
        let sidecar = segment.join(format!("{stem}.jsonl"));
        let record = serde_json::json!({"_solstone_processing": {
            "schema":"solstone.processing.v1", "state":"analyzed",
            "reason_code":"ok", "handler":"transcribe", "input_size":file.bytes,
            "attempted_at":"2026-10-09T12:00:00Z"
        }});
        let mut header = serde_json::to_vec(&record)?;
        header.push(b'\n');
        let mut complete = header.clone();
        complete.extend_from_slice(b"{\"start\":0,\"text\":\"synthetic transcript\"}\n");
        fs::write(&sidecar, &complete)?;
        raw.push(destination);
        sidecars.push((sidecar, complete, header));
    }
    let directory = MediaDirectory::open(root.clone(), Path::new(&relative))?;
    let scan =
        crate::scan::scan_segment_bound(&directory, &ClosedHandlerSet, &JournalMedia, 65_536)?;
    assert_eq!(scan.files().len(), proof.files.len());
    for file in &mut proof.files {
        let current = scan
            .files()
            .iter()
            .find(|current| current.name.as_str() == file.name)
            .ok_or_else(|| invalid("actual scanned media missing"))?;
        let observed = directory
            .observe_file(std::ffi::OsStr::new(&file.name))?
            .ok_or_else(|| invalid("actual raw observation missing"))?;
        assert_eq!(observed.digest.size, file.bytes);
        assert_eq!(observed.digest.sha256, file.sha256);
        file.processing = AcceptedProcessingObservation::capture(
            &ClosedHandlerSet,
            &proof.target,
            &current.name,
            current.size,
            &current.sidecar,
        )?;
        assert!(file.processing.record.is_some());
        assert!(file.processing.has_analysis_row);
    }
    let offer = OffloadOffer::with_nonce(proof, first.nonce.clone())?;
    let names = offer
        .proof
        .files
        .iter()
        .map(|file| file.name.clone())
        .collect::<Vec<_>>();
    let mark = Mark {
        id: MarkId::derive(RemovalClass::OffloadRawRelease, &offer.proof.target, &names),
        class: RemovalClass::OffloadRawRelease,
        target: offer.proof.target.clone(),
        marked_at: "2026-10-09T12:00:00Z".into(),
        proposal: Proposal {
            bytes: offer.proof.bytes()?,
            reason: "synthetic parent control".into(),
            names,
        },
        state: MarkState::Marked,
        offload: Some(offer),
    };
    let offered = mark
        .offload
        .as_ref()
        .ok_or_else(|| invalid("typed offer missing"))?;
    let event = Event::ArchiveV2 {
        schema_version: 2,
        time: 1,
        day: mark.target.day.clone(),
        stream: mark.target.stream.clone(),
        segment: solstone_core_format::segment::segment_key(&mark.target.dir)
            .ok_or_else(|| invalid("actual segment key missing"))?,
        segment_dir: mark.target.dir.clone(),
        layout: offered.proof.layout,
        journal_id: offered.proof.journal_id.clone(),
        manifest_id: offered.proof.manifest_id.clone(),
        repository_id: offered.proof.repository_id.clone(),
        snapshot_id: offered.proof.snapshot_id.clone(),
        files: offered.proof.files.clone(),
    };
    let ledger = input
        .journal
        .join("health/offload")
        .join(format!("{}.jsonl", mark.target.day));
    solstone_core_journal_io::append_jsonl(&ledger, &event)?;
    let catalog =
        crate::archive_catalog::read_complete_catalog(root).map_err(|error| invalid(&error))?;
    for file in &offered.proof.files {
        assert!(catalog.active_witness(&offered.proof, file).is_some());
    }
    let register_bytes = parent_save_register(input, &mark)?;
    Ok(ParentFixture {
        mark,
        directory,
        raw,
        sidecars,
        ledger,
        register_bytes,
    })
}

// Two independently processed groups make the valid first group's Hashed cut
// observable before the present second selected member's processing refusal.
fn parent_selected_processing_entry(input: &Input, root: Arc<JournalRoot>) -> TestResult {
    use crate::remove_marked::{ReviewedRemovalRefusal, remove_reviewed_approved_with_test_hook};
    use chrono::TimeZone;

    assert_eq!(input.marks.len(), 2);
    let mut groups = Vec::new();
    for mark in &input.marks {
        groups.push(parent_fixture_for(
            input,
            &root,
            std::slice::from_ref(mark),
        )?);
    }
    let selected = groups
        .iter()
        .map(|group| group.mark.clone())
        .collect::<Vec<_>>();
    let mut register = Register {
        version: 2,
        marks: BTreeMap::new(),
    };
    for mark in &selected {
        register.marks.insert(mark.id.clone(), mark.clone());
    }
    validate(&register)?;
    let original_register = serde_json::to_vec(&register)?;
    let register_path = input.journal.join("health/retention-marks.json");
    fs::write(&register_path, &original_register)?;
    let ids = selected
        .iter()
        .map(|mark| mark.id.clone())
        .collect::<Vec<_>>();
    let context = crate::review_context::read_review_context(&root, &ids)
        .map_err(|reason| invalid(&format!("actual rendered processing context: {reason:?}")))?;
    let checked = crate::review_context::check_review_context(root.clone(), &context)
        .map_err(|reason| invalid(&format!("actual checked processing context: {reason:?}")))?;
    let originals = groups
        .iter()
        .map(parent_raw_observations)
        .collect::<TestResult<Vec<_>>>()?;
    let started = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())?;
    let clock = ProbeClock(Rc::new(Cell::new(started)));
    let operation =
        PreparedArchiveOperation::prepare(root.clone(), ArchiveBudget::new(&clock)?, &clock)
            .map_err(|error| invalid(&format!("actual original preparation: {error:?}")))?;
    let destination = operation
        .configuration()
        .destination()
        .ok_or_else(|| invalid("actual destination missing"))?;
    let password = operation
        .configuration()
        .keys()?
        .ok_or_else(|| invalid("actual keys missing"))?
        .daily_key;
    let read = ReadOperation::admit(ReadAdmission::bind(
        operation,
        destination,
        password,
        input.repository_id.clone(),
        input.expected_restic_path.clone(),
    ))
    .map_err(|error| invalid(&format!("actual read admission: {error:?}")))?;
    let activity = Arc::clone(read.activity());
    let carrier = Arc::clone(read.carrier());
    let carrier_path = carrier.diagnostic_path();
    parent_observer_cut(input, b"admitted")?;
    let approval = create_archive_approval_batch(read, carrier, &selected);
    let batch = approval
        .as_ref()
        .map_err(|failure| invalid(&format!("actual processing batch: {:?}", failure.reason())))?;
    check_owner(&batch.owner, &root, &activity, &selected);
    check_complete(&batch.owner, 2)?;
    evidence(
        "parent_processing_actual_batch_before_transfer",
        &batch.owner,
        None,
    )?;
    let first = groups
        .first()
        .ok_or_else(|| invalid("actual first group missing"))?;
    let second = groups
        .get(1)
        .ok_or_else(|| invalid("actual second group missing"))?;
    let first_path = first
        .raw
        .first()
        .ok_or_else(|| invalid("actual first raw path missing"))?;
    let (second_sidecar, _, second_header) = second
        .sidecars
        .first()
        .ok_or_else(|| invalid("actual second sidecar missing"))?;
    fs::write(second_sidecar, second_header)?;
    let scan = crate::scan::scan_segment_bound(
        &second.directory,
        &crate::content::ClosedHandlerSet,
        &crate::content::JournalMedia,
        65_536,
    )?;
    let changed = scan
        .files()
        .first()
        .ok_or_else(|| invalid("second selected raw is not PRESENT"))?;
    let offered = second
        .mark
        .offload
        .as_ref()
        .and_then(|offer| offer.proof.files.first())
        .ok_or_else(|| invalid("original second processing missing"))?;
    assert_eq!(changed.name.as_str(), offered.name);
    assert_eq!(changed.sidecar.record, offered.processing.record);
    assert!(!changed.sidecar.has_analysis_row);
    assert!(offered.processing.has_analysis_row);
    let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 10)
        .ok_or_else(|| invalid("actual control date missing"))?;
    let now = chrono::Utc
        .with_ymd_and_hms(2026, 10, 10, 12, 0, 0)
        .single()
        .ok_or_else(|| invalid("actual control time missing"))?;
    let mut checkpoints = Vec::new();
    let result =
        remove_reviewed_approved_with_test_hook(checked, approval, today, now, |point, path| {
            checkpoints.push((format!("{point:?}"), path.to_path_buf()))
        });
    assert!(matches!(
        result.refusal(),
        Some(ReviewedRemovalRefusal::Changed)
    ));
    assert!(result.attempts().is_empty());
    assert!(result.stage_failures().is_empty());
    assert!(result.mutation().is_none());
    assert!(result.outcome().removed_paths().next().is_none());
    assert!(
        checkpoints
            .iter()
            .any(|(point, path)| point == "Hashed" && path == first_path)
    );
    assert!(checkpoints.iter().all(|(point, _)| point == "Hashed"));
    assert_eq!(fs::read(&register_path)?, original_register);
    for (ordinal, group) in groups.iter().enumerate() {
        let original = originals
            .get(ordinal)
            .ok_or_else(|| invalid("actual original group observations missing"))?;
        assert_eq!(
            parent_raw_observations(group)?
                .iter()
                .map(|value| (value.identity, &value.digest))
                .collect::<Vec<_>>(),
            original
                .iter()
                .map(|value| (value.identity, &value.digest))
                .collect::<Vec<_>>()
        );
        parent_check_bookkeeping(input, group, &[])?;
        let (path, complete, header) = group
            .sidecars
            .first()
            .ok_or_else(|| invalid("actual retained sidecar missing"))?;
        assert_eq!(
            fs::read(path)?,
            if ordinal == 1 { header } else { complete }.clone()
        );
        let offer = group
            .mark
            .offload
            .as_ref()
            .ok_or_else(|| invalid("actual retained offer missing"))?;
        let catalog = crate::archive_catalog::read_complete_catalog(&root)
            .map_err(|error| invalid(&error))?;
        for file in &offer.proof.files {
            assert!(catalog.active_witness(&offer.proof, file).is_some());
        }
    }
    assert_eq!(stages(&input.journal)?, vec![carrier_path.clone()]);
    println!(
        "ARCHIVE_APPROVAL_PARENT_EVIDENCE {}",
        serde_json::json!({"case":input.case,"outcome":result.outcome(),"refusal":format!("{:?}",result.refusal()),"attempts":[],"register":register,"checkpoints":checkpoints,"carrier":carrier_path,"present_selected_groups":selected.iter().map(|mark| &mark.target).collect::<Vec<_>>()})
    );
    std::io::stdout().flush()?;
    Ok(())
}

fn parent_save_register(input: &Input, mark: &Mark) -> TestResult<Vec<u8>> {
    let mut register = Register {
        version: if mark.offload.is_some() { 2 } else { 1 },
        marks: BTreeMap::new(),
    };
    register.marks.insert(mark.id.clone(), mark.clone());
    validate(&register)?;
    let bytes = serde_json::to_vec(&register)?;
    fs::write(input.journal.join("health/retention-marks.json"), &bytes)?;
    Ok(bytes)
}

fn parent_checked(
    root: &Arc<JournalRoot>,
    mark: &Mark,
) -> TestResult<crate::review_context::CheckedReviewContext> {
    let context = crate::review_context::read_review_context(root, std::slice::from_ref(&mark.id))
        .map_err(|reason| invalid(&format!("actual rendered context: {reason:?}")))?;
    crate::review_context::check_review_context(root.clone(), &context)
        .map_err(|reason| invalid(&format!("actual checked context: {reason:?}")).into())
}

fn parent_observer_cut(input: &Input, message: &[u8]) -> TestResult {
    let path = input
        .control
        .as_ref()
        .ok_or_else(|| invalid("owning observer missing"))?;
    let mut control = UnixStream::connect(path)?;
    control.set_read_timeout(Some(Duration::from_secs(30)))?;
    control.write_all(message)?;
    let mut acknowledged = [0_u8];
    control.read_exact(&mut acknowledged)?;
    assert_eq!(acknowledged, [b'a']);
    Ok(())
}

fn parent_raw_observations(
    fixture: &ParentFixture,
) -> TestResult<Vec<solstone_core_journal_io::media::MediaObservation>> {
    fixture
        .mark
        .proposal
        .names
        .iter()
        .map(|name| {
            fixture
                .directory
                .observe_file(std::ffi::OsStr::new(name))?
                .ok_or_else(|| invalid("actual original disappeared").into())
        })
        .collect()
}

fn parent_check_attempts(
    result: &crate::remove_marked::ReviewedArchiveRemoval<'_>,
    expected: usize,
) -> TestResult {
    use solstone_core_journal_io::media::{Confirmation, Durability, OriginalName, RemovalEffect};
    assert_eq!(result.attempts().len(), expected);
    assert!(result.stage_failures().is_empty());
    for attempt in result.attempts() {
        let native = attempt.native().receipt();
        assert_eq!(native.effect, RemovalEffect::Deleted);
        assert_eq!(native.original_name, OriginalName::Absent);
        assert_eq!(native.claim_confirmation, Confirmation::Confirmed);
        assert_eq!(native.cleanup, Cleanup::Complete);
        for durability in [
            native.file_durability,
            native.source_durability,
            native.claim_durability,
            native.stage_parent_durability,
        ] {
            assert_eq!(durability, Durability::Synced);
        }
        let append = attempt
            .append()
            .ok_or_else(|| invalid("actual event append missing"))?;
        assert_eq!(
            append.confirmation(),
            solstone_core_journal_io::append::BoundAppendConfirmation::Confirmed
        );
        assert_eq!(
            append.file_durability(),
            solstone_core_journal_io::append::BoundAppendDurability::Synced
        );
        assert_eq!(
            append.parent_durability(),
            solstone_core_journal_io::append::BoundAppendDurability::Synced
        );
        assert!(append.failures().is_empty());
        let marker = attempt
            .marker()
            .ok_or_else(|| invalid("actual marker receipt missing"))?
            .as_ref()
            .map_err(|error| invalid(&format!("actual marker error: {error:?}")))?;
        assert!(matches!(
            marker.publication(),
            solstone_core_journal_io::BoundAtomicOutcome::Published { .. }
        ));
        assert!(marker.binding_error().is_none());
        assert!(marker.lock_error().is_none());
        assert!(attempt.binding_error().is_none());
    }
    Ok(())
}

fn parent_check_bookkeeping(
    input: &Input,
    fixture: &ParentFixture,
    expected_names: &[String],
) -> TestResult {
    let segment = fixture.directory.diagnostic_path();
    let events = segment.join("events.jsonl");
    if expected_names.is_empty() {
        assert!(!events.exists());
    } else {
        let rows = fs::read_to_string(events)?
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()?;
        let names = rows
            .iter()
            .map(|row| {
                assert_eq!(row.get("tract").and_then(|v| v.as_str()), Some("retention"));
                assert_eq!(
                    row.get("event").and_then(|v| v.as_str()),
                    Some("original_deleted")
                );
                assert_eq!(
                    row.get("class").and_then(|v| v.as_str()),
                    Some("offload_raw_release")
                );
                row.get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| invalid("actual event name missing"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            names,
            expected_names
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        );
    }
    let marker = solstone_core_journal_io::health_marker_path(
        &input.journal,
        &fixture.mark.target.day,
        solstone_core_journal_io::HealthMarkerKind::Stream,
    );
    if expected_names.is_empty() {
        assert!(!marker.exists());
    } else {
        let marker: solstone_core_journal_io::health_marker::HealthMarker =
            serde_json::from_slice(&fs::read(marker)?)?;
        assert_eq!(marker.version, 1);
        assert_eq!(marker.generation, u64::try_from(expected_names.len())?);
    }
    Ok(())
}

#[test]
#[ignore = "requires the frozen typed parent overlay and owning copied-package fixture"]
fn packaged_archive_parent_entry() -> TestResult {
    use crate::mark_mutation::{BoundMutationOutcome, MutationStatus, PlannedMarkMutation};
    use crate::remove_marked::{
        ReviewedCheckpoint, ReviewedRemovalRefusal, remove_reviewed_approved,
        remove_reviewed_approved_with_test_hook,
    };
    use chrono::TimeZone;

    let input: Input = serde_json::from_slice(&fs::read(
        std::env::var_os("ARCHIVE_APPROVAL_PROBE_JSON")
            .ok_or_else(|| invalid("explicit parent probe input required"))?,
    )?)?;
    let root = Arc::new(JournalRoot::open(&input.journal)?);
    if input.case == "parent_selected_processing" {
        return parent_selected_processing_entry(&input, root);
    }
    let mut fixture = parent_fixture(&input, &root)?;
    let original = parent_raw_observations(&fixture)?;
    let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 10)
        .ok_or_else(|| invalid("fixture date missing"))?;
    let now = chrono::Utc
        .with_ymd_and_hms(2026, 10, 10, 12, 0, 0)
        .single()
        .ok_or_else(|| invalid("fixture time missing"))?;
    if input.case == "parent_legacy_young" {
        fixture.mark.offload = None;
        fixture.register_bytes = parent_save_register(&input, &fixture.mark)?;
        let checked = parent_checked(&root, &fixture.mark)?;
        parent_observer_cut(&input, b"ordinary")?;
        let result = crate::remove_marked::remove_reviewed(checked, today, now);
        assert!(result.refusal().is_none(), "{:?}", result.refusal());
        assert!(result.attempts().is_empty());
        assert!(result.stage_failures().is_empty());
        assert_eq!(
            parent_raw_observations(&fixture)?
                .iter()
                .map(|value| (value.identity, &value.digest))
                .collect::<Vec<_>>(),
            original
                .iter()
                .map(|value| (value.identity, &value.digest))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            fs::read(input.journal.join("health/retention-marks.json"))?,
            fixture.register_bytes
        );
        assert!(result.outcome().removed_paths().next().is_none());
        let retained = result
            .outcome()
            .targets
            .iter()
            .flat_map(|row| &row.not_removed)
            .map(|row| row.entry.clone())
            .collect::<Vec<_>>();
        let expected = fixture
            .mark
            .proposal
            .names
            .iter()
            .map(|name| {
                crate::layout::content_rel(
                    &fixture.mark.target.day,
                    &fixture.mark.target.stream,
                    &fixture.mark.target.dir,
                    name,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(retained, expected);
        assert!(result.carrier_preparation().is_some());
        parent_check_bookkeeping(&input, &fixture, &[])?;
        println!(
            "ARCHIVE_APPROVAL_PARENT_EVIDENCE {}",
            serde_json::json!({"case":input.case,"outcome":result.outcome(),"register":crate::review_context::load_at_root(&root).map_err(|error| invalid(&format!("register: {error:?}")))?,"attempts":0,"raw_present":fixture.raw,"no_archive_admission":true})
        );
        return Ok(());
    }
    let floor_refusal = matches!(
        input.case.as_str(),
        "parent_floor_null" | "parent_floor_overflow"
    );
    if floor_refusal {
        let path = input.journal.join("config/journal.json");
        let mut configuration: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        let backup = configuration
            .get_mut("backup")
            .and_then(serde_json::Value::as_object_mut)
            .ok_or_else(|| invalid("actual backup configuration missing"))?;
        backup.insert(
            "offload".into(),
            serde_json::json!({"floor_bytes": if input.case == "parent_floor_null" {
                serde_json::Value::Null
            } else {
                serde_json::json!(u64::MAX)
            }}),
        );
        fs::write(path, serde_json::to_vec(&configuration)?)?;
    }
    let checked = parent_checked(&root, &fixture.mark)?;
    let started = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())?;
    let clock = ProbeClock(Rc::new(Cell::new(started)));
    let operation =
        PreparedArchiveOperation::prepare(root.clone(), ArchiveBudget::new(&clock)?, &clock)
            .map_err(|error| invalid(&format!("actual original preparation: {error:?}")))?;
    let destination = operation
        .configuration()
        .destination()
        .ok_or_else(|| invalid("actual destination missing"))?;
    let password = operation
        .configuration()
        .keys()?
        .ok_or_else(|| invalid("actual keys missing"))?
        .daily_key;
    let read = ReadOperation::admit(ReadAdmission::bind(
        operation,
        destination,
        password,
        input.repository_id.clone(),
        input.expected_restic_path.clone(),
    ))
    .map_err(|error| invalid(&format!("actual read admission: {error:?}")))?;
    let activity = Arc::clone(read.activity());
    let carrier = Arc::clone(read.carrier());
    let carrier_path = carrier.diagnostic_path();
    parent_observer_cut(&input, b"admitted")?;
    if input.case == "parent_positive" {
        let maximum = fixture
            .mark
            .offload
            .as_ref()
            .and_then(|offer| offer.proof.files.iter().map(|file| file.bytes).max())
            .ok_or_else(|| invalid("actual selected maximum missing"))?;
        let metadata = u64::try_from(crate::review_context::MAX_REGISTER_BYTES)? + 1024 * 1024;
        let required = read.offload_required_bytes(maximum, metadata)?;
        assert_eq!(required, maximum + 7 + metadata);
        println!(
            "ARCHIVE_APPROVAL_HELPER_EVIDENCE {}",
            serde_json::json!({"case":input.case,"maximum":maximum,"metadata":metadata,"captured_floor":7,"required":required,"scope":"actual helper admission on original read; subsequent positive uses real private Batch parent path"})
        );
        std::io::stdout().flush()?;
    }
    if floor_refusal {
        let configuration = fs::read(input.journal.join("config/journal.json"))?;
        let result = crate::remove_marked::remove_reviewed_with_archive(checked, read, today, now);
        assert!(matches!(
            result.refusal(),
            Some(ReviewedRemovalRefusal::OffloadFloorRequired)
        ));
        assert!(Arc::ptr_eq(result.archive_journal_root(), &root));
        assert!(Arc::ptr_eq(result.archive_activity(), &activity));
        assert!(result.archive_expected_marks().is_none());
        assert_eq!(result.archive_reads().count(), 0);
        assert!(result.approval_cleanup().is_none());
        assert!(result.approval_readback_error().is_none());
        assert!(result.approval_native_error().is_none());
        assert!(result.attempts().is_empty());
        assert!(result.stage_failures().is_empty());
        assert!(result.mutation().is_none());
        assert!(result.outcome().removed_paths().next().is_none());
        assert_eq!(
            parent_raw_observations(&fixture)?
                .iter()
                .map(|value| (value.identity, &value.digest))
                .collect::<Vec<_>>(),
            original
                .iter()
                .map(|value| (value.identity, &value.digest))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            fs::read(input.journal.join("health/retention-marks.json"))?,
            fixture.register_bytes
        );
        assert_eq!(
            fs::read(input.journal.join("config/journal.json"))?,
            configuration
        );
        for (path, complete, _) in &fixture.sidecars {
            assert_eq!(fs::read(path)?, *complete);
        }
        parent_check_bookkeeping(&input, &fixture, &[])?;
        assert_eq!(stages(&input.journal)?, vec![carrier_path.clone()]);
        assert!(payloads(&input.journal)?.is_empty());
        println!(
            "ARCHIVE_APPROVAL_PARENT_EVIDENCE {}",
            serde_json::json!({"case":input.case,"refusal":format!("{:?}",result.refusal()),"attempts":0,"actor_reads":0,"pending_original_operation":true,"carrier":carrier_path,"outcome":result.outcome()})
        );
        std::io::stdout().flush()?;
        drop(result);
        assert_eq!(stages(&input.journal)?, vec![carrier_path]);
        assert!(payloads(&input.journal)?.is_empty());
        return Ok(());
    }
    let approval = if input.case == "parent_actual_failure" {
        let (approval, consumed) = run_with_media_hook(
            MediaPrimitive::StageUnlink,
            || Err(io::Error::from_raw_os_error(5)),
            || create_archive_approval_batch(read, carrier, std::slice::from_ref(&fixture.mark)),
        );
        assert!(consumed);
        approval
    } else {
        create_archive_approval_batch(read, carrier, std::slice::from_ref(&fixture.mark))
    };
    if input.case == "parent_actual_failure" {
        let failed = approval
            .as_ref()
            .err()
            .ok_or_else(|| invalid("actual actor failure missing"))?;
        assert!(matches!(
            failed.reason(),
            ArchiveApprovalError::Cleanup(Cleanup::Pending)
        ));
        check_owner(
            &failed.owner,
            &root,
            &activity,
            std::slice::from_ref(&fixture.mark),
        );
        assert_eq!(failed.owner.held.len(), 1);
        match failed.owner.held.first() {
            Some(HeldResource::Prepared(prepared)) => {
                check_prepared(prepared);
                assert_eq!(prepared.cleanup, Some(Cleanup::Pending));
            }
            _ => return Err(invalid("original Pending owner missing").into()),
        }
        evidence(
            "parent_actual_failure_before_transfer",
            &failed.owner,
            Some(failed.reason()),
        )?;
    } else {
        let batch = approval
            .as_ref()
            .map_err(|failure| invalid(&format!("actual batch: {:?}", failure.reason())))?;
        check_owner(
            &batch.owner,
            &root,
            &activity,
            std::slice::from_ref(&fixture.mark),
        );
        check_complete(&batch.owner, 2)?;
        evidence("parent_actual_batch_before_transfer", &batch.owner, None)?;
    }
    let held_stages = stages(&input.journal)?;
    let held_payloads = payloads(&input.journal)?;
    let mut checkpoints = Vec::new();
    let mut retired = false;
    let mut checkpoint_error = None;
    let result = if input.case == "parent_late_retirement" {
        remove_reviewed_approved_with_test_hook(checked, approval, today, now, |point, path| {
            checkpoints.push((format!("{point:?}"), path.to_path_buf()));
            if input.case == "parent_late_retirement"
                && point == ReviewedCheckpoint::AfterRaw
                && !retired
            {
                if Some(path) != fixture.raw.first().map(PathBuf::as_path) {
                    checkpoint_error = Some(invalid(
                        "AfterRaw did not identify the first selected original",
                    ));
                    return;
                }
                let Some(offer) = fixture.mark.offload.as_ref() else {
                    checkpoint_error = Some(invalid("actual offer missing"));
                    return;
                };
                let Some(second) = offer.proof.files.get(1) else {
                    checkpoint_error = Some(invalid("second actual witness missing"));
                    return;
                };
                let Some(segment) =
                    solstone_core_format::segment::segment_key(&fixture.mark.target.dir)
                else {
                    checkpoint_error = Some(invalid("actual segment key missing"));
                    return;
                };
                let event = crate::archive_catalog::codec::Event::RestoreV2 {
                    schema_version: 2,
                    time: 2,
                    day: fixture.mark.target.day.clone(),
                    stream: fixture.mark.target.stream.clone(),
                    segment,
                    operation_id: "synthetic-parent-late-retirement".into(),
                    files: vec![crate::archive_catalog::RestoreFileV2 {
                        witness_id: second.witness_id.clone(),
                        name: second.name.clone(),
                        disposition: crate::archive_catalog::RestoreDisposition::Published,
                    }],
                };
                match solstone_core_journal_io::append_jsonl(&fixture.ledger, &event) {
                    Ok(()) => retired = true,
                    Err(error) => checkpoint_error = Some(io::Error::other(error)),
                }
            }
        })
    } else {
        remove_reviewed_approved(checked, approval, today, now)
    };
    if let Some(error) = checkpoint_error {
        return Err(error.into());
    }
    assert_eq!(result.journal_root().identity(), root.identity());
    let register = crate::review_context::load_at_root(&root)
        .map_err(|error| invalid(&format!("actual final register: {error:?}")))?;
    let offered = fixture
        .mark
        .offload
        .as_ref()
        .ok_or_else(|| invalid("original typed offer missing"))?;
    let catalog =
        crate::archive_catalog::read_complete_catalog(&root).map_err(|error| invalid(&error))?;
    let removed_names = match input.case.as_str() {
        "parent_positive" => {
            assert!(result.refusal().is_none(), "{:?}", result.refusal());
            parent_check_attempts(&result, 2)?;
            assert!(register.marks.is_empty());
            for path in &fixture.raw {
                assert!(!path.exists());
            }
            for file in &offered.proof.files {
                assert!(catalog.active_witness(&offered.proof, file).is_some());
            }
            fixture.mark.proposal.names.clone()
        }
        "parent_late_retirement" => {
            assert!(retired);
            assert!(matches!(
                result.refusal(),
                Some(ReviewedRemovalRefusal::Changed)
            ));
            parent_check_attempts(&result, 1)?;
            let first = offered
                .proof
                .files
                .first()
                .ok_or_else(|| invalid("first witness missing"))?;
            let second = offered
                .proof
                .files
                .get(1)
                .ok_or_else(|| invalid("second witness missing"))?;
            assert!(catalog.active_witness(&offered.proof, first).is_some());
            assert!(catalog.active_witness(&offered.proof, second).is_none());
            assert!(
                !fixture
                    .raw
                    .first()
                    .ok_or_else(|| invalid("first raw missing"))?
                    .exists()
            );
            let current = fixture
                .directory
                .observe_file(std::ffi::OsStr::new(&second.name))?
                .ok_or_else(|| invalid("retired-witness raw disappeared"))?;
            let original = original
                .get(1)
                .ok_or_else(|| invalid("second original missing"))?;
            assert_eq!(current.identity, original.identity);
            assert_eq!(current.digest, original.digest);
            let plan =
                PlannedMarkMutation::remainder(&fixture.mark, std::slice::from_ref(&second.name))?;
            let replacement = plan
                .replacement()
                .ok_or_else(|| invalid("actual remainder missing"))?;
            assert_eq!(register.marks.len(), 1);
            assert_eq!(register.marks.get(&replacement.id), Some(replacement));
            assert!(!register.marks.contains_key(&fixture.mark.id));
            let remainder = replacement
                .offload
                .as_ref()
                .ok_or_else(|| invalid("typed remainder offer missing"))?;
            assert_eq!(remainder.nonce, offered.nonce);
            assert_ne!(remainder.revision, offered.revision);
            assert_eq!(replacement.proposal.bytes, second.bytes);
            vec![first.name.clone()]
        }
        "parent_actual_failure" => {
            assert!(matches!(
                result.refusal(),
                Some(ReviewedRemovalRefusal::ArchiveApproval)
            ));
            assert_eq!(result.approval_cleanup(), Some(Cleanup::Pending));
            assert!(result.attempts().is_empty());
            assert!(result.stage_failures().is_empty());
            assert!(result.mutation().is_none());
            assert_eq!(
                parent_raw_observations(&fixture)?
                    .iter()
                    .map(|value| (value.identity, &value.digest))
                    .collect::<Vec<_>>(),
                original
                    .iter()
                    .map(|value| (value.identity, &value.digest))
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                fs::read(input.journal.join("health/retention-marks.json"))?,
                fixture.register_bytes
            );
            assert_eq!(stages(&input.journal)?, held_stages);
            assert_eq!(payloads(&input.journal)?, held_payloads);
            Vec::new()
        }
        _ => return Err(invalid("unknown explicit parent case").into()),
    };
    if !removed_names.is_empty() {
        let mutation = match result.mutation() {
            Some(Ok(BoundMutationOutcome::Published(mutation))) => mutation,
            _ => return Err(invalid("actual final Published CAS missing").into()),
        };
        assert_eq!(mutation.status, MutationStatus::Applied);
        assert_eq!(mutation.register, register);
    }
    let expected_paths = removed_names
        .iter()
        .map(|name| {
            crate::layout::content_rel(
                &fixture.mark.target.day,
                &fixture.mark.target.stream,
                &fixture.mark.target.dir,
                name,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        result
            .outcome()
            .removed_paths()
            .map(|path| path.as_str())
            .collect::<Vec<_>>(),
        expected_paths
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
    assert!(
        result
            .outcome()
            .targets
            .iter()
            .all(|row| row.post_commit_failure.is_none())
    );
    parent_check_bookkeeping(&input, &fixture, &removed_names)?;
    for (path, complete, _) in &fixture.sidecars {
        assert_eq!(fs::read(path)?, *complete);
    }
    assert!(carrier_path.is_dir());
    println!(
        "ARCHIVE_APPROVAL_PARENT_EVIDENCE {}",
        serde_json::json!({"case":input.case,"outcome":result.outcome(),"refusal":result.refusal().map(|reason| format!("{reason:?}")),"cleanup":result.approval_cleanup(),"attempts":result.attempts().iter().map(|attempt| serde_json::json!({"relative":attempt.relative(),"native_debug":format!("{:?}", attempt.native().receipt())})).collect::<Vec<_>>(),"register":register,"checkpoints":checkpoints,"carrier":carrier_path,"catalog_debug":format!("{catalog:?}")})
    );
    std::io::stdout().flush()?;
    if input.case == "parent_actual_failure" {
        drop(result);
        assert_eq!(stages(&input.journal)?, held_stages);
        assert_eq!(payloads(&input.journal)?, held_payloads);
    }
    Ok(())
}
