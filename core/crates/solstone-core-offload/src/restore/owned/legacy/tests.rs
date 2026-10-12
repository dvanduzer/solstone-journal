// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use serde::Deserialize;
use solstone_core_backup::archive_activity::OwnedPreparedArchiveOperation;
use solstone_core_backup::archive_admission::{ArchiveBudget, Clock};
use solstone_core_backup_runtime::{
    DirectReadFinishState, UreqHttpTransport, admit_prepared_archive_owned_current_read_installed,
};
use solstone_core_journal_io::activity::{ActivityAdmissionError, ActivityLease};
use solstone_core_journal_io::media::{MediaPrimitive, run_with_media_hook};
use solstone_core_retention::archive_catalog::{OffloadFile, codec::Event};
use solstone_core_retention::{MarkId, MarkState, Proposal, Register};
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    mode: String,
    journal: PathBuf,
    restic_path: PathBuf,
    repository_id: String,
    snapshot_id: String,
    files: Vec<OffloadFile>,
}

struct OwnedClock(Instant);
impl Clock for OwnedClock {
    fn now_unix(&self) -> i64 {
        10_000 + self.0.elapsed().as_secs() as i64
    }
    fn iso_week(&self) -> u8 {
        1
    }
}

fn write_event(path: &Path, event: &Event, append: bool) {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .append(append)
        .truncate(!append)
        .open(path)
        .unwrap();
    serde_json::to_writer(&mut file, event).unwrap();
    file.write_all(b"\n").unwrap();
    file.sync_all().unwrap();
}

fn probes(review: &OwnedLegacyRestoreReviewOutcome) -> serde_json::Value {
    serde_json::Value::Array(
        review
            .probes()
            .map(|probe| {
                serde_json::json!({
                    "name":probe.name,"capacity":probe.capacity.and_then(|v|v.as_ref().ok()),
                    "digest":probe.digest.map(|v|serde_json::json!({"bytes":v.size,"sha256":v.sha256})),
                    "cleanup":probe.cleanup.map(|v|format!("{v:?}")),
                    "parent_sync":probe.parent_sync.map(Result::is_ok),
                })
            })
            .collect(),
    )
}

/// Hidden restore stages left beside the day's segments.
fn scratch_residue(day: &Path) -> usize {
    fs::read_dir(day)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".tmp-offload-")
        })
        .count()
}

#[test]
#[ignore = "requires a copied current package and synthetic whole-manifest legacy repository"]
fn packaged_legacy_restore_review_entry() {
    let input: Input = serde_json::from_slice(
        &fs::read(std::env::var_os("NATIVE_LEGACY_PROBE_JSON").unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(input.files.len(), 3);
    assert!(matches!(
        input.mode.as_str(),
        "complete"
            | "conflict"
            | "changed_nonselected"
            | "wrong_review"
            | "stale_config"
            | "stale_catalog"
            | "unknown_stage"
            | "mark_during_reread"
            | "capacity_query"
            | "capacity_short"
    ));
    let target = Target {
        day: "20261009".into(),
        stream: "_default".into(),
        dir: "120000_60_fixture".into(),
    };
    let segment = input
        .journal
        .join("chronicle")
        .join(&target.day)
        .join(&target.dir);
    fs::create_dir_all(&segment).unwrap();
    let ledger = crate::ledger::ledger_path_for_day(&input.journal, &target.day).unwrap();
    fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    let event = Event::Archive {
        time: 1,
        day: target.day.clone(),
        stream: target.stream.clone(),
        segment: solstone_core_format::segment::segment_key(&target.dir).unwrap(),
        snapshot_id: input.snapshot_id[..8].to_owned(),
        files: input.files.clone(),
    };
    write_event(&ledger, &event, false);
    let mark_path = input.journal.join("health/retention-marks.json");
    fs::create_dir_all(mark_path.parent().unwrap()).unwrap();
    let names: Vec<_> = input.files.iter().map(|file| file.name.clone()).collect();
    let mark = Mark {
        id: MarkId::derive(RemovalClass::OffloadRawRelease, &target, &names),
        class: RemovalClass::OffloadRawRelease,
        target: target.clone(),
        marked_at: "2026-10-10T00:00:00Z".into(),
        proposal: Proposal {
            bytes: input.files.iter().map(|file| file.bytes).sum(),
            reason: "synthetic original legacy mark".into(),
            names,
        },
        state: MarkState::Marked,
        offload: None,
    };
    let mut register = Register::empty();
    register.marks.insert(mark.id.clone(), mark);
    let original_mark = serde_json::to_vec(&register).unwrap();
    fs::write(&mark_path, &original_mark).unwrap();
    if input.mode == "conflict" {
        fs::write(
            segment.join(&input.files[0].name),
            b"existing different raw",
        )
        .unwrap();
    }
    let root = Arc::new(JournalRoot::open(&input.journal).unwrap());
    let clock: Arc<dyn Clock + Send + Sync> = Arc::new(OwnedClock(Instant::now()));
    let prepared = OwnedPreparedArchiveOperation::prepare(
        Arc::clone(&root),
        ArchiveBudget::new(clock.as_ref()).unwrap(),
        Arc::clone(&clock),
    )
    .unwrap_or_else(|failure| panic!("legacy preparation: {:?}", failure.reason()));
    let installed = admit_prepared_archive_owned_current_read_installed(
        prepared,
        &input.restic_path,
        env!("CARGO_PKG_VERSION"),
        &UreqHttpTransport,
    )
    .unwrap_or_else(|failure| panic!("legacy admission: {:?}", failure.error()));
    let (read, finish) = installed.into_parts();
    assert_eq!(read.repository_id(), input.repository_id);
    let carrier = Arc::downgrade(read.carrier());
    let selection = LegacyRestoreSelection {
        target: target.clone(),
        layout: SegmentLayout::Direct,
        manifest_id: format!("v1:{}:1", target.day),
        names: input.files[..2]
            .iter()
            .map(|file| file.name.clone())
            .collect(),
    };
    let review = if input.mode == "capacity_query" {
        let (review, fired) = run_with_media_hook(
            MediaPrimitive::CapacityQuery,
            || Err(io::Error::other("synthetic retained capacity failure")),
            || prepare_owned_legacy_restore(read, selection),
        );
        assert!(fired);
        review
    } else {
        prepare_owned_legacy_restore(read, selection)
    };
    let probe_facts = probes(&review);
    let result = if matches!(
        input.mode.as_str(),
        "changed_nonselected" | "capacity_query" | "capacity_short"
    ) {
        let reason = review.refusal().unwrap().reason();
        assert_eq!(
            reason,
            match input.mode.as_str() {
                "changed_nonselected" => "legacy_verification_failed",
                "capacity_query" => "legacy_capacity_unavailable",
                _ => "legacy_insufficient_free_space",
            }
        );
        assert!(review.candidate().is_none());
        // Each refusal's writer is known finished, so its scratch probe is removed.
        assert!(!review.retained_probe());
        assert!(
            review
                .probes()
                .last()
                .is_some_and(|probe| probe.cleanup == Some(Cleanup::Complete)
                    && probe.parent_sync.is_some_and(Result::is_ok))
        );
        assert_eq!(scratch_residue(segment.parent().unwrap()), 0);
        let held = review.into_owned_hold();
        let unknown = finish.into_unknown();
        let custody = std::thread::spawn(move || (held, unknown)).join().unwrap();
        assert_eq!(custody.0.refusal().unwrap().reason(), reason);
        assert_eq!(custody.1.state(), DirectReadFinishState::Unknown);
        assert!(Arc::ptr_eq(
            custody.0.carrier(),
            &carrier.upgrade().unwrap()
        ));
        assert!(matches!(
            ActivityLease::try_acquire(Arc::clone(&root)),
            Err(ActivityAdmissionError::Busy)
        ));
        assert!(
            read_complete_catalog(&root).unwrap().days[0].segments[0]
                .restore_bindings
                .is_empty()
        );
        assert_eq!(fs::read(&mark_path).unwrap(), original_mark);
        serde_json::json!({"state":"ReviewHeld","reason":reason,"closed_transfer":true})
    } else {
        assert!(
            review.refusal().is_none(),
            "{:?}",
            review.refusal().map(|v| v.reason())
        );
        let binding = review.candidate().unwrap().clone();
        assert_eq!(binding.files.len(), 3);
        assert_eq!(review.selected().len(), 2);
        assert_eq!(binding.repository_id, input.repository_id);
        assert_eq!(binding.snapshot_id, input.snapshot_id);
        assert!(
            binding
                .legacy_manifest
                .files
                .iter()
                .all(|w| w.repository_id.is_none()
                    && w.journal_id.is_none()
                    && w.processing.is_none()
                    && w.archive_path.is_none()
                    && w.layout.is_none()
                    && w.segment_dir.is_none())
        );
        assert!(!review.retained_probe());
        assert!(
            review.probes().all(|p| p.cleanup == Some(Cleanup::Complete)
                && p.parent_sync.is_some_and(Result::is_ok))
        );
        match input.mode.as_str() {
            "stale_config" => {
                let path = input.journal.join("config/journal.json");
                let mut config: serde_json::Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                config["backup"]["offload"]["floor_bytes"] = 1048577_u64.into();
                fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
            }
            "stale_catalog" => write_event(&ledger, &event, true),
            "unknown_stage" => fs::create_dir(
                segment
                    .parent()
                    .unwrap()
                    .join(".tmp-offload-unowned-review-cut"),
            )
            .unwrap(),
            _ => {}
        }
        let ledger_before_approval = fs::read(&ledger).unwrap();
        let revision = if input.mode == "wrong_review" {
            "0".repeat(64)
        } else {
            binding.revision.clone()
        };
        let outcome = if input.mode == "mark_during_reread" {
            let changed_path = mark_path.clone();
            let (outcome, fired) = run_with_media_hook(
                MediaPrimitive::FileSync,
                move || {
                    let mut changed: Register =
                        serde_json::from_slice(&fs::read(&changed_path)?).unwrap();
                    changed
                        .marks
                        .values_mut()
                        .next()
                        .unwrap()
                        .proposal
                        .reason
                        .push_str(" changed during reread");
                    fs::write(&changed_path, serde_json::to_vec(&changed).unwrap())
                },
                || review.approve(&binding.adoption_id, &revision, 2),
            );
            assert!(fired);
            outcome
        } else {
            review.approve(&binding.adoption_id, &revision, 2)
        };
        match outcome {
            Err(review) => {
                assert!(matches!(
                    input.mode.as_str(),
                    "wrong_review" | "stale_config" | "stale_catalog" | "unknown_stage"
                ));
                let reason = review.refusal().unwrap().reason();
                let held = review.into_owned_hold();
                let unknown = finish.into_unknown();
                let custody = std::thread::spawn(move || (held, unknown)).join().unwrap();
                assert_eq!(custody.0.refusal().unwrap().reason(), reason);
                assert!(matches!(
                    ActivityLease::try_acquire(Arc::clone(&root)),
                    Err(ActivityAdmissionError::Busy)
                ));
                assert_eq!(fs::read(&ledger).unwrap(), ledger_before_approval);
                if input.mode == "stale_catalog" {
                    assert!(read_complete_catalog(&root).is_err());
                }
                assert_eq!(fs::read(&mark_path).unwrap(), original_mark);
                serde_json::json!({"state":"ReviewHeld","reason":reason,"closed_transfer":true})
            }
            Ok(outcome) => {
                assert_eq!(outcome.binding(), &binding);
                let expected_files = if input.mode == "complete" {
                    2
                } else if input.mode == "conflict" {
                    1
                } else {
                    0
                };
                assert_eq!(outcome.report().files_restored, expected_files);
                let dispositions: Vec<_> = outcome
                    .files()
                    .map(|file| format!("{:?}", file.disposition))
                    .collect();
                let catalog = read_complete_catalog(&root).unwrap();
                let selected = &catalog.days[0].segments[0];
                assert_eq!(selected.restore_bindings, vec![binding.clone()]);
                if input.mode == "mark_during_reread" {
                    assert!(selected.legacy_restores.is_empty());
                    assert!(dispositions.iter().all(|d| d == "Unattempted"));
                    assert_ne!(fs::read(&mark_path).unwrap(), original_mark);
                    let held = outcome.into_owned_hold();
                    let unknown = finish.into_unknown();
                    let custody = std::thread::spawn(move || (held, unknown)).join().unwrap();
                    assert_eq!(custody.0.report().files_restored, 0);
                    assert!(matches!(
                        ActivityLease::try_acquire(Arc::clone(&root)),
                        Err(ActivityAdmissionError::Busy)
                    ));
                    serde_json::json!({"state":"ExecutionHeld","reason":"restore_unsettled","reviewed_mark_change_refused":true,"closed_transfer":true,"dispositions":dispositions})
                } else {
                    assert_eq!(selected.legacy_restores.len(), 2);
                    assert!(
                        selected
                            .legacy_restores
                            .iter()
                            .all(|event| event.adoption_id == binding.adoption_id)
                    );
                    assert!(outcome.files().all(|file| match file.evidence {
                        FileEvidence::Transaction(facts) =>
                            facts.completed
                                && facts.register.is_none()
                                && facts.retirement.is_some_and(|v| v.retired)
                                && facts.ledger.is_some_and(|v| v.confirmed),
                        _ => false,
                    }));
                    assert_eq!(fs::read(&mark_path).unwrap(), original_mark);
                    let bytes = outcome.report().bytes_restored;
                    let (settled, report) = outcome
                        .settle_direct()
                        .unwrap_or_else(|_| panic!("legacy Native settlement held"));
                    let completed = finish.finish_owned(settled);
                    assert_eq!(completed.state(), DirectReadFinishState::Completed);
                    drop(completed);
                    assert!(carrier.upgrade().is_none());
                    assert_eq!(report.report().bytes_restored, bytes);
                    let lease = ActivityLease::try_acquire(Arc::clone(&root)).unwrap();
                    drop(lease);
                    serde_json::json!({"state":"Completed","files":expected_files,"bytes":bytes,"dispositions":dispositions,"events":2,"mark_unchanged":true,"binding_files":3,"report_retained":true})
                }
            }
        }
    };
    assert!(!segment.join(&input.files[2].name).exists());
    println!(
        "NATIVE_LEGACY_RESULT {}",
        serde_json::json!({"mode":input.mode,"probes":probe_facts,"result":result})
    );
}

#[test]
#[ignore = "requires a copied current package and synthetic whole-manifest legacy repository"]
fn packaged_combined_restore_entry() {
    use sha2::{Digest, Sha256};
    let mut input: Input = serde_json::from_slice(
        &fs::read(std::env::var_os("NATIVE_LEGACY_PROBE_JSON").unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(input.files.len(), 3);
    assert!(matches!(
        input.mode.as_str(),
        "combined_complete" | "combined_conflict" | "combined_tombstone" | "combined_mismatch"
    ));
    let target = Target {
        day: "20261009".into(),
        stream: "_default".into(),
        dir: "120000_60_fixture".into(),
    };
    let segment = input
        .journal
        .join("chronicle")
        .join(&target.day)
        .join(&target.dir);
    fs::create_dir_all(&segment).unwrap();
    if input.mode == "combined_mismatch" {
        input.files[2].sha256 = "0".repeat(64);
    }
    let ledger = crate::ledger::ledger_path_for_day(&input.journal, &target.day).unwrap();
    fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    let event = Event::Archive {
        time: 1,
        day: target.day.clone(),
        stream: target.stream.clone(),
        segment: solstone_core_format::segment::segment_key(&target.dir).unwrap(),
        snapshot_id: input.snapshot_id[..8].to_owned(),
        files: input.files.clone(),
    };
    write_event(&ledger, &event, false);
    let mark_path = input.journal.join("health/retention-marks.json");
    let names: Vec<_> = input.files.iter().map(|file| file.name.clone()).collect();
    let mark = Mark {
        id: MarkId::derive(RemovalClass::OffloadRawRelease, &target, &names),
        class: RemovalClass::OffloadRawRelease,
        target: target.clone(),
        marked_at: "2026-10-10T00:00:00Z".into(),
        proposal: Proposal {
            bytes: input.files.iter().map(|file| file.bytes).sum(),
            reason: "synthetic original legacy mark".into(),
            names,
        },
        state: MarkState::Marked,
        offload: None,
    };
    let mut register = Register::empty();
    register.marks.insert(mark.id.clone(), mark);
    let original_mark = serde_json::to_vec(&register).unwrap();
    fs::write(&mark_path, &original_mark).unwrap();
    match input.mode.as_str() {
        "combined_conflict" => fs::write(
            segment.join(&input.files[0].name),
            b"existing different raw",
        )
        .unwrap(),
        "combined_tombstone" => fs::write(segment.join("tombstone.json"), b"{}").unwrap(),
        _ => {}
    }
    let listing = |path: &Path| {
        let mut entries: Vec<_> = fs::read_dir(path)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), fs::read(entry.path()).unwrap())
            })
            .collect();
        entries.sort();
        entries
    };
    let before = listing(&segment);
    let root = Arc::new(JournalRoot::open(&input.journal).unwrap());
    let clock: Arc<dyn Clock + Send + Sync> = Arc::new(OwnedClock(Instant::now()));
    let prepared = OwnedPreparedArchiveOperation::prepare(
        Arc::clone(&root),
        ArchiveBudget::new(clock.as_ref()).unwrap(),
        Arc::clone(&clock),
    )
    .unwrap_or_else(|failure| panic!("combined preparation: {:?}", failure.reason()));
    let (read, finish) = admit_prepared_archive_owned_current_read_installed(
        prepared,
        &input.restic_path,
        env!("CARGO_PKG_VERSION"),
        &UreqHttpTransport,
    )
    .unwrap_or_else(|failure| panic!("combined admission: {:?}", failure.error()))
    .into_parts();
    assert_eq!(read.repository_id(), input.repository_id);
    let outcome = crate::restore::owned::restore_owned_all(read, 2);
    let report = outcome.report().clone();
    let files: Vec<_> = outcome
        .files()
        .map(|file| {
            (
                file.witness.file.name.clone(),
                file.disposition,
                file.reason,
            )
        })
        .collect();
    let bindings = outcome.legacy_bindings().len();
    let catalog = read_complete_catalog(&root).unwrap();
    let recorded = catalog.days[0].segments[0].restore_bindings.len();
    match input.mode.as_str() {
        "combined_complete" | "combined_conflict" => {
            assert_eq!((bindings, recorded), (1, 1));
            for (index, (name, disposition, reason)) in files.iter().enumerate() {
                let local = fs::read(segment.join(name)).unwrap();
                if input.mode == "combined_conflict" && index == 0 {
                    assert_eq!(*disposition, RestoreDisposition::Conflict);
                    assert_eq!(*reason, Some("restore_conflict"));
                    assert_eq!(local, b"existing different raw");
                } else {
                    assert_eq!(*disposition, RestoreDisposition::Published);
                    assert_eq!(
                        format!("{:x}", Sha256::digest(&local)),
                        input.files[index].sha256
                    );
                }
            }
            let (settled, _) = outcome
                .settle_direct()
                .unwrap_or_else(|_| panic!("combined settlement held"));
            assert_eq!(
                finish.finish_owned(settled).state(),
                DirectReadFinishState::Completed
            );
            // Restore leaves the owner's release mark exactly as it was.
            assert_eq!(fs::read(&mark_path).unwrap(), original_mark);
        }
        _ => {
            // Nothing is published and no provenance is recorded for a refused manifest.
            assert_eq!((bindings, recorded), (0, 0));
            assert_eq!(report.files_restored, 0);
            let expected = if input.mode == "combined_tombstone" {
                "destination_admission_failed"
            } else {
                "legacy_verification_failed"
            };
            assert!(files.iter().all(|(_, disposition, reason)| {
                *disposition == RestoreDisposition::Unattempted && *reason == Some(expected)
            }));
            assert_eq!(listing(&segment), before);
            // A refused manifest leaves no scratch copy and does not hold the read.
            assert_eq!(scratch_residue(segment.parent().unwrap()), 0);
            assert!(
                outcome
                    .files()
                    .all(|file| !matches!(file.evidence, FileEvidence::LegacyReview(_)))
            );
            if input.mode == "combined_tombstone" {
                // Nothing was read, so the read settles normally.
                let (settled, _) = outcome
                    .settle_direct()
                    .unwrap_or_else(|_| panic!("refused manifest held the restore"));
                assert_eq!(
                    finish.finish_owned(settled).state(),
                    DirectReadFinishState::Completed
                );
            } else {
                // A completed read that didn't match never settles as success:
                // with its probe removed and its child reaped, the read is
                // released as refused and nothing of the operation remains.
                let held = match outcome.settle_direct() {
                    Ok(_) => panic!("a mismatched read settled as success"),
                    Err(held) => held,
                };
                let (read, released) = held
                    .release_refused()
                    .unwrap_or_else(|_| panic!("refused manifest held the restore"));
                assert_eq!(released.report().files_restored, 0);
                let runtime = finish.finish_refused(read);
                assert_ne!(runtime.state(), DirectReadFinishState::Completed);
                assert!(runtime.released());
            }
            assert!(ActivityLease::try_acquire(Arc::clone(&root)).is_ok());
            assert_eq!(fs::read(&mark_path).unwrap(), original_mark);
        }
    }
    println!(
        "NATIVE_COMBINED_RESULT {}",
        serde_json::json!({"mode":input.mode,"status":report.status,"reason":report.reason,
            "files_restored":report.files_restored,"bytes_restored":report.bytes_restored})
    );
}
