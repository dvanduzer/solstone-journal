// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    reason = "synthetic root replacement exercises refusal without invoking a production removal door"
)]

use solstone_core_journal_io::{JournalRoot, errors::FlatDirectoryError};
use solstone_core_retention::archive_proof::{
    AcceptedProcessingObservation, ArchiveFileProof, OffloadOffer, OffloadProof,
};
#[cfg(unix)]
use solstone_core_retention::mark_mutation::{BoundMutationOutcome, MutationStatus};
use solstone_core_retention::mark_mutation::{
    BoundMutationRefusal, PlannedMarkMutation, conditional_mutate_at_root, decline_reviewed,
};
use solstone_core_retention::marks::{Failure, Proposal, RemovalClass};
use solstone_core_retention::marks::{Mark, MarkId, MarkState, Register};
use solstone_core_retention::receipt::Target;
use solstone_core_retention::review_context::*;
use std::sync::Arc;

fn root(path: &std::path::Path) -> Arc<JournalRoot> {
    Arc::new(JournalRoot::open(&std::fs::canonicalize(path).unwrap()).unwrap())
}

fn mark(root: &JournalRoot, name: &str, bound: bool) -> Mark {
    let class = if bound {
        RemovalClass::OffloadRawRelease
    } else {
        RemovalClass::PolicyRawRelease
    };
    let target = Target {
        day: "20261009".into(),
        stream: "camera".into(),
        dir: "120000_60_suffix".into(),
    };
    let proposal = Proposal {
        bytes: 7,
        reason: "archive".into(),
        names: vec![name.into()],
    };
    let offload = bound.then(|| {
        OffloadOffer::new(OffloadProof {
            journal_id: root.identity().observation_id(),
            manifest_id: "manifest".into(),
            target: target.clone(),
            layout: solstone_core_journal_io::paths::SegmentLayout::Named,
            repository_id: "a".repeat(64),
            snapshot_id: "b".repeat(64),
            files: vec![ArchiveFileProof {
                witness_id: "witness".into(),
                name: name.into(),
                bytes: 7,
                sha256: "c".repeat(64),
                archive_path: format!("/stage/{name}"),
                processing: AcceptedProcessingObservation {
                    record: None,
                    has_analysis_row: true,
                },
            }],
        })
        .unwrap()
    });
    Mark {
        id: MarkId::derive(class, &target, &proposal.names),
        class,
        target,
        marked_at: "2026-10-10T00:00:00Z".into(),
        proposal,
        state: MarkState::Marked,
        offload,
    }
}

fn save(root: &JournalRoot, marks: &[Mark]) {
    let mut register = Register::empty();
    if marks.iter().any(|mark| mark.offload.is_some()) {
        register.version = 2;
    }
    for mark in marks {
        register.marks.insert(mark.id.clone(), mark.clone());
    }
    let health = root.canonical_path().join("health");
    std::fs::create_dir_all(&health).unwrap();
    std::fs::write(
        health.join("retention-marks.json"),
        serde_json::to_vec(&register).unwrap(),
    )
    .unwrap();
}

#[test]
fn unchanged_restart_roundtrip_order_and_deserialized_duplicates() {
    let journal = tempfile::tempdir().unwrap();
    let first = root(journal.path());
    let marks = vec![mark(&first, "a.flac", false), mark(&first, "b.flac", false)];
    save(&first, &marks);
    let ids = vec![marks[1].id.clone(), marks[0].id.clone()];
    let context = read_review_context(&first, &ids).unwrap();
    let context: ReviewContext =
        serde_json::from_slice(&serde_json::to_vec(&context).unwrap()).unwrap();
    drop(first);
    let restarted = root(journal.path());
    let checked = check_review_context(Arc::clone(&restarted), &context).unwrap();
    assert_eq!(checked.marks()[0].id, ids[0]);
    assert_eq!(checked.marks()[1].id, ids[1]);
    assert_eq!(checked.root().identity(), restarted.identity());
    let mut repeated = serde_json::to_value(&context).unwrap();
    let duplicate = repeated["marks"][0].clone();
    repeated["marks"].as_array_mut().unwrap().push(duplicate);
    let repeated: ReviewContext = serde_json::from_value(repeated).unwrap();
    assert!(matches!(
        check_review_context(restarted, &repeated),
        Err(ReviewRefusal::Duplicate)
    ));
}

#[test]
fn rendered_selection_keeps_full_rows_and_excludes_unselected_failures() {
    let journal = tempfile::tempdir().unwrap();
    let root = root(journal.path());
    let first = mark(&root, "a.flac", false);
    let second = mark(&root, "b.flac", true);
    let mut failed = mark(&root, "failed.flac", false);
    failed.state = MarkState::Failed(Failure {
        at: "now".into(),
        reason: "failed".into(),
        staged: None,
    });
    save(&root, &[first.clone(), second.clone(), failed.clone()]);
    let rendered = read_review_context(
        &root,
        &[first.id.clone(), failed.id.clone(), second.id.clone()],
    )
    .unwrap();
    let selected = rendered
        .select(&[second.id.clone(), first.id.clone()])
        .unwrap();
    assert_eq!(selected.root_observation(), rendered.root_observation());
    assert_eq!(selected.marks(), &[second.clone(), first.clone()]);
    assert!(check_review_context(Arc::clone(&root), &selected).is_ok());
    assert!(matches!(rendered.select(&[]), Err(ReviewRefusal::Empty)));
    assert!(matches!(
        rendered.select(&[first.id.clone(), first.id.clone()]),
        Err(ReviewRefusal::Duplicate)
    ));
    let missing = mark(&root, "missing.flac", false);
    assert!(matches!(
        rendered.select(&[missing.id]),
        Err(ReviewRefusal::Missing { .. })
    ));
    let selected_failed = rendered.select(std::slice::from_ref(&failed.id)).unwrap();
    assert!(matches!(
        check_review_context(Arc::clone(&root), &selected_failed),
        Err(ReviewRefusal::Failed { .. })
    ));
    let mut changed = second.clone();
    changed.marked_at = "2026-10-10T00:00:01Z".into();
    save(&root, &[first, changed, failed]);
    // Selection uses rendered bytes, even after a newer row was written.
    let selected = rendered.select(std::slice::from_ref(&second.id)).unwrap();
    assert_eq!(selected.marks(), &[second]);
    assert!(matches!(
        check_review_context(root, &selected),
        Err(ReviewRefusal::Stale { .. })
    ));
    let mut ambiguous = serde_json::to_value(&rendered).unwrap();
    let repeated = ambiguous["marks"][0].clone();
    ambiguous["marks"].as_array_mut().unwrap().push(repeated);
    let ambiguous: ReviewContext = serde_json::from_value(ambiguous).unwrap();
    assert!(matches!(
        ambiguous.select(std::slice::from_ref(&rendered.marks()[0].id)),
        Err(ReviewRefusal::Duplicate)
    ));
}

#[cfg(unix)]
#[test]
fn reviewed_decline_checks_all_rows_before_publication() {
    for typed in [false, true] {
        let journal = tempfile::tempdir().unwrap();
        let root = root(journal.path());
        let first = mark(&root, "a.flac", typed);
        let second = mark(&root, "b.flac", false);
        let unrelated = mark(&root, "unrelated.flac", typed);
        let mut failed = mark(&root, "failed.flac", false);
        failed.state = MarkState::Failed(Failure {
            at: "now".into(),
            reason: "failed".into(),
            staged: None,
        });
        save(
            &root,
            &[
                first.clone(),
                second.clone(),
                failed.clone(),
                unrelated.clone(),
            ],
        );
        let ids = [first.id.clone(), second.id.clone()];
        let rendered = read_review_context(&root, &ids).unwrap();
        let checked = check_review_context(Arc::clone(&root), &rendered).unwrap();
        let mut changed = second.clone();
        changed.marked_at = "2026-10-10T00:00:01Z".into();
        save(
            &root,
            &[first.clone(), changed, failed.clone(), unrelated.clone()],
        );
        let path = journal.path().join("health/retention-marks.json");
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            decline_reviewed(checked),
            Err(BoundMutationRefusal::Review(ReviewRefusal::Stale { .. }))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        save(&root, &[first, second, failed.clone(), unrelated.clone()]);
        let checked = check_review_context(Arc::clone(&root), &rendered).unwrap();
        let BoundMutationOutcome::Published(result) = decline_reviewed(checked).unwrap() else {
            panic!("decline was not confirmed")
        };
        assert_eq!(result.status, MutationStatus::Applied);
        assert_eq!(result.register.version, if typed { 2 } else { 1 });
        assert_eq!(result.register.marks.len(), 2);
        assert_eq!(result.register.marks.get(&failed.id), Some(&failed));
        assert_eq!(result.register.marks.get(&unrelated.id), Some(&unrelated));
        assert_eq!(load_at_root(&root).unwrap(), result.register);
    }
}

#[cfg(unix)]
#[test]
fn checked_decline_retains_preflight_health_and_root() {
    for typed in [false, true] {
        for replace_root in [false, true] {
            let parent = tempfile::tempdir().unwrap();
            let path = parent.path().join("journal");
            std::fs::create_dir(&path).unwrap();
            let root = root(&path);
            let mark = mark(&root, "a.flac", typed);
            save(&root, std::slice::from_ref(&mark));
            let rendered = read_review_context(&root, std::slice::from_ref(&mark.id)).unwrap();
            let checked = check_review_context(Arc::clone(&root), &rendered).unwrap();
            let bytes = std::fs::read(path.join("health/retention-marks.json")).unwrap();
            let old_register = if replace_root {
                std::fs::rename(&path, parent.path().join("old-root")).unwrap();
                std::fs::create_dir_all(path.join("health")).unwrap();
                parent.path().join("old-root/health/retention-marks.json")
            } else {
                std::fs::rename(path.join("health"), path.join("old-health")).unwrap();
                std::fs::create_dir(path.join("health")).unwrap();
                path.join("old-health/retention-marks.json")
            };
            std::fs::write(path.join("health/retention-marks.json"), &bytes).unwrap();
            assert!(decline_reviewed(checked).is_err());
            assert_eq!(std::fs::read(old_register).unwrap(), bytes);
            assert_eq!(
                std::fs::read(path.join("health/retention-marks.json")).unwrap(),
                bytes
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn retained_cas_preserves_newer_full_rows_and_exact_idempotence() {
    let journal = tempfile::tempdir().unwrap();
    let root = root(journal.path());
    let expected = mark(&root, "a.flac", false);
    let mut current = expected.clone();
    current.marked_at = "2026-10-10T00:00:01Z".into();
    save(&root, std::slice::from_ref(&current));
    let path = journal.path().join("health/retention-marks.json");
    let before = std::fs::read(&path).unwrap();
    let plan = PlannedMarkMutation::resolve(&expected).unwrap();
    let BoundMutationOutcome::Unchanged(result) = conditional_mutate_at_root(&root, &plan).unwrap()
    else {
        panic!("stale row was changed")
    };
    assert_eq!(result.status, MutationStatus::Stale);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let plan = PlannedMarkMutation::failure(
        &current,
        Failure {
            at: "now".into(),
            reason: "failed".into(),
            staged: None,
        },
    )
    .unwrap();
    let BoundMutationOutcome::Published(result) = conditional_mutate_at_root(&root, &plan).unwrap()
    else {
        panic!("exact row was not changed")
    };
    assert_eq!(result.status, MutationStatus::Applied);
    let BoundMutationOutcome::Unchanged(result) = conditional_mutate_at_root(&root, &plan).unwrap()
    else {
        panic!("exact replacement was rewritten")
    };
    assert_eq!(result.status, MutationStatus::AlreadyApplied);
    std::fs::remove_file(&path).unwrap();
    assert!(conditional_mutate_at_root(&root, &plan).is_err());
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn generic_retained_typed_mutation_remains_held_without_rewriting() {
    let journal = tempfile::tempdir().unwrap();
    let root = root(journal.path());
    let mark = mark(&root, "a.flac", true);
    save(&root, std::slice::from_ref(&mark));
    let path = journal.path().join("health/retention-marks.json");
    let before = std::fs::read(&path).unwrap();
    let plan = PlannedMarkMutation::resolve(&mark).unwrap();
    assert!(matches!(
        conditional_mutate_at_root(&root, &plan),
        Err(BoundMutationRefusal::Review(ReviewRefusal::Register(
            solstone_core_retention::marks::StoreError::WriterHeld
        )))
    ));
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn reviewed_typed_decline_preserves_restart_and_refuses_reoffered_nonce() {
    let journal = tempfile::tempdir().unwrap();
    let first = root(journal.path());
    let offered = mark(&first, "audio.flac", true);
    save(&first, std::slice::from_ref(&offered));
    let rendered = read_review_context(&first, std::slice::from_ref(&offered.id)).unwrap();
    drop(first);
    let restarted = root(journal.path());
    let checked = check_review_context(Arc::clone(&restarted), &rendered).unwrap();
    let old_checked = check_review_context(Arc::clone(&restarted), &rendered).unwrap();
    assert!(matches!(
        decline_reviewed(checked).unwrap(),
        BoundMutationOutcome::Published(_)
    ));
    assert!(load_at_root(&restarted).unwrap().marks.is_empty());
    assert!(matches!(
        check_review_context(Arc::clone(&restarted), &rendered),
        Err(ReviewRefusal::Missing { .. })
    ));

    let mut reoffered = offered.clone();
    reoffered.offload =
        Some(OffloadOffer::new(offered.offload.as_ref().unwrap().proof.clone()).unwrap());
    assert_ne!(
        reoffered.offload.as_ref().unwrap().nonce,
        offered.offload.as_ref().unwrap().nonce
    );
    assert_ne!(
        reoffered.offload.as_ref().unwrap().revision,
        offered.offload.as_ref().unwrap().revision
    );
    save(&restarted, std::slice::from_ref(&reoffered));
    let path = journal.path().join("health/retention-marks.json");
    let bytes = std::fs::read(&path).unwrap();
    assert!(matches!(
        check_review_context(Arc::clone(&restarted), &rendered),
        Err(ReviewRefusal::Stale { .. })
    ));
    assert!(matches!(
        decline_reviewed(old_checked),
        Err(BoundMutationRefusal::Review(ReviewRefusal::Stale { .. }))
    ));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let fresh = read_review_context(&restarted, std::slice::from_ref(&reoffered.id)).unwrap();
    let checked = check_review_context(Arc::clone(&restarted), &fresh).unwrap();
    assert!(matches!(
        decline_reviewed(checked).unwrap(),
        BoundMutationOutcome::Published(_)
    ));
    assert!(load_at_root(&restarted).unwrap().marks.is_empty());
}

#[cfg(windows)]
#[test]
fn checked_windows_register_writes_refuse_until_native_qualification() {
    let journal = tempfile::tempdir().unwrap();
    let root = root(journal.path());
    let mark = mark(&root, "a.flac", false);
    save(&root, std::slice::from_ref(&mark));
    let rendered = read_review_context(&root, std::slice::from_ref(&mark.id)).unwrap();
    let checked = check_review_context(Arc::clone(&root), &rendered).unwrap();
    let plan = PlannedMarkMutation::resolve(&mark).unwrap();
    let path = journal.path().join("health/retention-marks.json");
    let before = std::fs::read(&path).unwrap();
    assert!(matches!(
        decline_reviewed(checked),
        Err(BoundMutationRefusal::Unsupported)
    ));
    assert!(matches!(
        conditional_mutate_at_root(&root, &plan),
        Err(BoundMutationRefusal::Unsupported)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!path.with_file_name("retention-marks.json.lock").exists());
}

#[cfg(unix)]
#[test]
fn bound_and_legacy_register_writers_use_the_same_sidecar() {
    use solstone_core_journal_io::{
        FlatDirectory, LockOptions, acquire_existing_parent_lock_bound, hold_lock,
    };
    use std::ffi::OsStr;
    use std::time::Duration;
    let journal = tempfile::tempdir().unwrap();
    let root = root(journal.path());
    save(&root, &[mark(&root, "a.flac", false)]);
    let health = FlatDirectory::open(&root, std::path::Path::new("health")).unwrap();
    let bound = acquire_existing_parent_lock_bound(
        &health,
        OsStr::new("retention-marks.json.lock"),
        Duration::ZERO,
        Duration::ZERO,
    )
    .unwrap();
    let options = LockOptions {
        timeout: Duration::ZERO,
        ..LockOptions::default()
    };
    let path = journal.path().join("health/retention-marks.json");
    assert!(hold_lock(&path, options).is_err());
    drop(bound);
    let _legacy = hold_lock(&path, options).unwrap();
    assert!(
        acquire_existing_parent_lock_bound(
            &health,
            OsStr::new("retention-marks.json.lock"),
            Duration::ZERO,
            Duration::ZERO
        )
        .is_err()
    );
}

#[cfg(unix)]
#[test]
fn bound_publication_keeps_effect_and_uncertainty_distinct() {
    use solstone_core_journal_io::{
        BoundPublicationPrimitive, run_with_bound_publication_barrier,
        run_with_bound_publication_fault,
    };
    for primitive in [
        BoundPublicationPrimitive::Rename,
        BoundPublicationPrimitive::ParentSync,
    ] {
        let journal = tempfile::tempdir().unwrap();
        let root = root(journal.path());
        let mark = mark(&root, "a.flac", false);
        save(&root, std::slice::from_ref(&mark));
        let path = journal.path().join("health/retention-marks.json");
        let before = std::fs::read(&path).unwrap();
        let plan = PlannedMarkMutation::resolve(&mark).unwrap();
        let (outcome, consumed) = run_with_bound_publication_fault(primitive, 1, 5, || {
            conditional_mutate_at_root(&root, &plan)
        });
        assert!(consumed);
        if primitive == BoundPublicationPrimitive::Rename {
            assert!(matches!(outcome, Err(BoundMutationRefusal::Write(_))));
            assert_eq!(std::fs::read(&path).unwrap(), before);
        } else {
            let BoundMutationOutcome::PublishedUncertain {
                candidate,
                uncertainty,
            } = outcome.unwrap()
            else {
                panic!("published effect was lost")
            };
            assert!(candidate.register.marks.is_empty());
            assert!(uncertainty.durability.is_some());
            assert!(load_at_root(&root).unwrap().marks.is_empty());
        }
    }
    let journal = tempfile::tempdir().unwrap();
    let root = root(journal.path());
    let mark = mark(&root, "a.flac", false);
    save(&root, std::slice::from_ref(&mark));
    let plan = PlannedMarkMutation::resolve(&mark).unwrap();
    let path = journal.path().to_path_buf();
    let (outcome, consumed) = run_with_bound_publication_barrier(
        BoundPublicationPrimitive::ParentSync,
        1,
        move || {
            std::fs::rename(path.join("health"), path.join("old-health")).unwrap();
            std::fs::create_dir(path.join("health")).unwrap();
        },
        || conditional_mutate_at_root(&root, &plan),
    );
    assert!(consumed);
    let BoundMutationOutcome::PublishedUncertain {
        candidate,
        uncertainty,
    } = outcome.unwrap()
    else {
        panic!("retired-parent publication was claimed current")
    };
    assert!(candidate.register.marks.is_empty());
    assert!(!uncertainty.bindings.is_empty());
    assert!(!journal.path().join("health/retention-marks.json").exists());
    assert!(
        serde_json::from_slice::<Register>(
            &std::fs::read(journal.path().join("old-health/retention-marks.json")).unwrap()
        )
        .unwrap()
        .marks
        .is_empty()
    );
}

#[test]
fn copied_context_and_copied_typed_offer_refuse_new_root() {
    for bound in [false, true] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let original = root(source.path());
        let mark = mark(&original, "audio.flac", bound);
        save(&original, std::slice::from_ref(&mark));
        let context = read_review_context(&original, std::slice::from_ref(&mark.id)).unwrap();
        let copied = root(destination.path());
        save(&copied, std::slice::from_ref(&mark));
        let path = destination.path().join("health/retention-marks.json");
        let bytes = std::fs::read(&path).unwrap();
        assert!(matches!(
            check_review_context(Arc::clone(&copied), &context),
            Err(ReviewRefusal::RootChanged)
        ));
        if bound {
            assert!(matches!(
                read_review_context(&copied, std::slice::from_ref(&mark.id)),
                Err(ReviewRefusal::SourceRootChanged { .. })
            ));
        } else {
            let fresh = read_review_context(&copied, std::slice::from_ref(&mark.id)).unwrap();
            assert!(check_review_context(copied, &fresh).is_ok());
        }
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn whole_selection_refuses_drift_failed_missing_and_empty() {
    let journal = tempfile::tempdir().unwrap();
    let root = root(journal.path());
    assert!(read_review_context(&root, &[]).unwrap().marks().is_empty());
    assert!(matches!(
        check_review_context(Arc::clone(&root), &read_review_context(&root, &[]).unwrap()),
        Err(ReviewRefusal::Empty)
    ));
    let original = mark(&root, "audio.flac", false);
    for change in 0..3 {
        save(&root, std::slice::from_ref(&original));
        let context = read_review_context(&root, std::slice::from_ref(&original.id)).unwrap();
        let mut changed = original.clone();
        match change {
            0 => changed.proposal.bytes = 999,
            1 => changed.marked_at = "2026-10-10T00:00:01Z".into(),
            _ => {
                changed.state = MarkState::Failed(Failure {
                    at: "now".into(),
                    reason: "failed".into(),
                    staged: None,
                })
            }
        }
        save(&root, std::slice::from_ref(&changed));
        assert!(matches!(
            check_review_context(Arc::clone(&root), &context),
            Err(ReviewRefusal::Stale { .. })
        ));
        save(&root, &[]);
        assert!(matches!(
            check_review_context(Arc::clone(&root), &context),
            Err(ReviewRefusal::Missing { .. })
        ));
    }
    let mut failed = original.clone();
    failed.state = MarkState::Failed(Failure {
        at: "now".into(),
        reason: "failed".into(),
        staged: None,
    });
    let valid = mark(&root, "extra.flac", false);
    save(&root, &[valid.clone(), failed.clone()]);
    let mixed = read_review_context(&root, &[valid.id.clone(), failed.id.clone()]).unwrap();
    assert!(matches!(
        check_review_context(Arc::clone(&root), &mixed),
        Err(ReviewRefusal::Failed { .. })
    ));
    let mut owner = original.clone();
    owner.class = RemovalClass::OwnerRawRelease;
    owner.id = MarkId::derive(owner.class, &owner.target, &owner.proposal.names);
    save(&root, std::slice::from_ref(&owner));
    let context = read_review_context(&root, std::slice::from_ref(&owner.id)).unwrap();
    assert!(matches!(
        check_review_context(root, &context),
        Err(ReviewRefusal::NotRequired { .. })
    ));
}

#[test]
fn corrupt_unknown_and_oversized_registers_never_become_empty() {
    let journal = tempfile::tempdir().unwrap();
    let root = root(journal.path());
    let health = root.canonical_path().join("health");
    std::fs::create_dir(&health).unwrap();
    let path = health.join("retention-marks.json");
    for bytes in [
        b"{broken".as_slice(),
        b"{\"version\":999,\"marks\":{}}".as_slice(),
    ] {
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(
            read_review_context(&root, &[]),
            Err(ReviewRefusal::Register(_))
        ));
    }
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(33 * 1024 * 1024).unwrap();
    assert!(matches!(
        read_review_context(&root, &[]),
        Err(ReviewRefusal::Read(
            FlatDirectoryError::SizeLimitExceeded { .. }
        ))
    ));
}

#[cfg(unix)]
#[test]
fn retained_root_refuses_replaced_canonical_binding_and_symlink_health() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("journal");
    std::fs::create_dir(&path).unwrap();
    let retained = root(&path);
    let mark = mark(&retained, "audio.flac", false);
    save(&retained, std::slice::from_ref(&mark));
    let context = read_review_context(&retained, std::slice::from_ref(&mark.id)).unwrap();
    std::fs::rename(&path, parent.path().join("moved")).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(matches!(
        check_review_context(retained, &context),
        Err(ReviewRefusal::Root(_))
    ));
    let admitted = root(&path);
    std::os::unix::fs::symlink(parent.path().join("moved/health"), path.join("health")).unwrap();
    assert!(matches!(
        read_review_context(&admitted, &[]),
        Err(ReviewRefusal::Read(_))
    ));
}
