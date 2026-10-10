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
