// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Retained reconciliation controls over disposable journals.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "disposable retained journals exercise actual reconciliation and refusal"
)]

use super::tests::{capture, fixture, operation, proof, seed, selection, time};
use super::*;
use solstone_core_journal_io::{LockOptions, paths::SegmentLayout};
use solstone_core_segment::output_attempt::OutputClaims;
use std::fs;
use std::time::Duration;

fn reconciliation_mark(
    operation: &PreparedArchiveOperation<'_>,
    selected: &OffloadOfferSelection,
) -> Mark {
    let captured = capture(operation, std::slice::from_ref(selected)).unwrap();
    candidate_mark(&captured.groups()[0], proof(&captured), time().1).unwrap()
}

fn reconciliation_register(root: &JournalRoot, marks: &[Mark]) {
    let mut register = crate::marks::Register::empty();
    register.version = if marks.iter().any(|mark| mark.offload.is_some()) {
        2
    } else {
        1
    };
    for mark in marks {
        register.marks.insert(mark.id.clone(), mark.clone());
    }
    crate::marks::validate(&register).unwrap();
    fs::create_dir_all(root.canonical_path().join("health")).unwrap();
    fs::write(
        root.canonical_path().join("health/retention-marks.json"),
        serde_json::to_vec(&register).unwrap(),
    )
    .unwrap();
}

fn reconciliation_keep(root: &JournalRoot) {
    fs::write(
        root.canonical_path().join("config/journal.json"),
        br#"{"transcribe":{"preserve_all":true},"retention":{"raw_media":"keep"}}"#,
    )
    .unwrap();
}

fn reconcile(operation: &PreparedArchiveOperation<'_>) -> OffloadReconciliation {
    let (today, now) = time();
    reconcile_offload_empty(operation, today, now)
        .unwrap_or_else(|_| panic!("reconciliation failed"))
}

#[test]
fn reconciliation_shrinks_typed_kept_empty_only_and_preserves_pending_consent() {
    use crate::mark_mutation::PlannedMarkMutation;
    for (layout, unproven_sibling) in [
        (SegmentLayout::Direct, false),
        (SegmentLayout::Named, false),
        (SegmentLayout::Direct, true),
    ] {
        let (_journal, root) = fixture();
        let operation = operation(&root);
        let selected = selection(layout, &["empty.flac", "ordinary.flac"]);
        seed(&root, &selected, "empty.flac", "empty");
        seed(&root, &selected, "ordinary.flac", "analyzed");
        let mark = reconciliation_mark(&operation, &selected);
        if unproven_sibling {
            seed(&root, &selected, "ordinary.flac", "pending");
        }
        reconciliation_register(&root, std::slice::from_ref(&mark));
        let directory = root.canonical_path().join(selection_path(&selected));
        let before = [
            "empty.flac",
            "empty.jsonl",
            "ordinary.flac",
            "ordinary.jsonl",
        ]
        .map(|name| fs::read(directory.join(name)).unwrap());
        reconciliation_keep(&root);

        let result = reconcile(&operation);
        let expected = PlannedMarkMutation::remainder(&mark, &["ordinary.flac".into()]).unwrap();
        let remainder = result.register().marks.values().next().unwrap();
        assert_eq!(Some(remainder), expected.replacement());
        assert_eq!(result.register().marks.len(), 1);
        assert_eq!(
            remainder.offload.as_ref().unwrap().nonce,
            mark.offload.as_ref().unwrap().nonce
        );
        assert_ne!(
            remainder.offload.as_ref().unwrap().revision,
            mark.offload.as_ref().unwrap().revision
        );
        assert!(
            matches!(result.groups()[0].disposition(), OffloadReconciliationDisposition::Publication { outcome, observation: None }
                if matches!(outcome.as_ref(), crate::mark_mutation::BoundMutationOutcome::Published(_)))
        );
        for (name, bytes) in [
            "empty.flac",
            "empty.jsonl",
            "ordinary.flac",
            "ordinary.jsonl",
        ]
        .iter()
        .zip(before)
        {
            assert_eq!(fs::read(directory.join(name)).unwrap(), bytes);
        }
        assert!(!root.canonical_path().join("health/offload").exists());
        let bytes = fs::read(root.canonical_path().join("health/retention-marks.json")).unwrap();
        let second = reconcile(&operation);
        assert_eq!(second.register(), result.register());
        assert!(matches!(
            second.groups()[0].disposition(),
            OffloadReconciliationDisposition::Unchanged
        ));
        assert_eq!(
            fs::read(root.canonical_path().join("health/retention-marks.json")).unwrap(),
            bytes
        );
        let register = result.register().clone();
        let expected = result.groups()[0].expected().to_vec();
        let root_references = Arc::strong_count(&root);
        let report = result.into_report();
        fn require_send_static<T: Send + 'static>(_: &T) {}
        require_send_static(&report);
        assert_eq!(Arc::strong_count(&root), root_references - 1);
        assert_eq!(report.root_observation(), root.identity().observation_id());
        assert_eq!(report.register(), &register);
        assert_eq!(report.groups()[0].expected(), expected);
        assert_eq!(report.groups()[0].selection(), Some(&selected));
        assert!(
            matches!(report.groups()[0].disposition(), OffloadReconciliationDisposition::Publication { outcome, observation: None }
                if matches!(outcome.as_ref(), crate::mark_mutation::BoundMutationOutcome::Published(_)))
        );
    }
}

#[test]
fn reconciliation_resolves_all_kept_but_preserves_other_classes_and_failures() {
    let (_journal, root) = fixture();
    let operation = operation(&root);
    let empty = selection(SegmentLayout::Direct, &["empty.flac"]);
    seed(&root, &empty, "empty.flac", "empty");
    let mark = reconciliation_mark(&operation, &empty);
    let mut unrelated = mark.clone();
    unrelated.offload = None;
    unrelated.class = RemovalClass::PolicyRawRelease;
    unrelated.id = MarkId::derive(
        unrelated.class,
        &unrelated.target,
        &unrelated.proposal.names,
    );
    let mut failed = mark.clone();
    failed.target.dir = "130000_60".into();
    failed.offload.as_mut().unwrap().proof.target = failed.target.clone();
    let offer = failed.offload.take().unwrap();
    failed.offload =
        Some(crate::archive_proof::OffloadOffer::with_nonce(offer.proof, offer.nonce).unwrap());
    failed.id = MarkId::derive(failed.class, &failed.target, &failed.proposal.names);
    failed.state = crate::marks::MarkState::Failed(crate::marks::Failure {
        at: "later".into(),
        reason: "held".into(),
        staged: None,
    });
    reconciliation_register(&root, &[mark, unrelated.clone(), failed.clone()]);
    reconciliation_keep(&root);
    let result = reconcile(&operation);
    assert_eq!(result.register().marks.len(), 2);
    assert_eq!(result.register().marks.get(&unrelated.id), Some(&unrelated));
    assert_eq!(result.register().marks.get(&failed.id), Some(&failed));
}

#[test]
fn reconciliation_kept_and_eligible_empty_use_same_canonical_handler_evidence() {
    for kept in [false, true] {
        let (_journal, root) = fixture();
        let operation = operation(&root);
        let selected = selection(SegmentLayout::Direct, &["empty.flac"]);
        seed(&root, &selected, "empty.flac", "empty");
        let mark = reconciliation_mark(&operation, &selected);
        reconciliation_register(&root, std::slice::from_ref(&mark));
        if kept {
            reconciliation_keep(&root);
        }
        let before = fs::read(root.canonical_path().join("health/retention-marks.json")).unwrap();

        let policy = crate::remove_marked::ReviewedPolicy::capture(&root).unwrap();
        let directory =
            MediaDirectory::open(Arc::clone(&root), Path::new(&selection_path(&selected))).unwrap();
        let inspection = crate::grouped::inspect_grouped(
            &scan(&directory).unwrap(),
            &selected.target,
            selected.layout,
            policy.policy(),
            time().0,
            time().1,
            crate::grouped::DecisionPurpose::OffloadProposal,
        );
        assert_eq!(
            matches!(
                inspection.empty().verdict(),
                crate::grouped::GroupVerdict::Kept(_)
            ),
            kept
        );
        if !kept {
            assert!(matches!(
                inspection.empty().verdict(),
                crate::grouped::GroupVerdict::Eligible
            ));
        }
        // Release the actual config guard before the separate owning entry.
        let _policy_observation = policy.observation().0.clone();
        std::mem::drop(policy);
        let result = reconcile(&operation);
        if kept {
            assert!(result.register().marks.is_empty());
        } else {
            assert_eq!(result.register().marks.get(&mark.id), Some(&mark));
            assert!(matches!(
                result.groups()[0].disposition(),
                OffloadReconciliationDisposition::Unchanged
            ));
            assert_eq!(
                fs::read(root.canonical_path().join("health/retention-marks.json")).unwrap(),
                before
            );
        }
    }
}

#[test]
fn reconciliation_legacy_mixed_withdraws_whole_but_ambiguous_key_stays() {
    for ambiguous in [false, true] {
        let (_journal, root) = fixture();
        let operation = operation(&root);
        let selected = selection(SegmentLayout::Direct, &["empty.flac", "ordinary.flac"]);
        seed(&root, &selected, "empty.flac", "empty");
        seed(&root, &selected, "ordinary.flac", "analyzed");
        let mut legacy = reconciliation_mark(&operation, &selected);
        legacy.offload = None;
        legacy.target.dir = "120000_60".into();
        legacy.id = MarkId::derive(legacy.class, &legacy.target, &legacy.proposal.names);
        reconciliation_register(&root, std::slice::from_ref(&legacy));
        if ambiguous {
            let mut other = selected.clone();
            other.target.dir = "120000_60_other".into();
            seed(&root, &other, "empty.flac", "empty");
        }
        reconciliation_keep(&root);
        let result = reconcile(&operation);
        assert_eq!(result.register().version, 1);
        if ambiguous {
            assert_eq!(result.register().marks.get(&legacy.id), Some(&legacy));
            assert!(matches!(
                result.groups()[0].disposition(),
                OffloadReconciliationDisposition::Unbound
            ));
        } else {
            assert!(result.register().marks.is_empty());
        }
    }
}

#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "fault cuts replace disposable fixture files"
)]
fn reconciliation_unknown_or_changed_evidence_and_exact_layouts_are_preserved() {
    for drift in ["missing", "malformed", "pending", "changed", "nonplain"] {
        let (_journal, root) = fixture();
        let operation = operation(&root);
        let selected = selection(SegmentLayout::Named, &["empty.flac"]);
        seed(&root, &selected, "empty.flac", "empty");
        let mark = reconciliation_mark(&operation, &selected);
        reconciliation_register(&root, std::slice::from_ref(&mark));
        let path = root.canonical_path().join(selection_path(&selected));
        match drift {
            "missing" => {
                fs::remove_file(path.join("empty.flac")).unwrap();
            }
            "malformed" => fs::write(path.join("empty.jsonl"), b"\xff").unwrap(),
            "pending" => seed(&root, &selected, "empty.flac", "pending"),
            "changed" => fs::write(
                path.join("empty.jsonl"),
                fs::read_to_string(path.join("empty.jsonl"))
                    .unwrap()
                    .replace("no_decodable_audio", "no_speech"),
            )
            .unwrap(),
            "nonplain" => {
                fs::remove_file(path.join("empty.flac")).unwrap();
                fs::create_dir(path.join("empty.flac")).unwrap();
            }
            _ => panic!("unknown fixture"),
        }
        reconciliation_keep(&root);
        let before = fs::read(root.canonical_path().join("health/retention-marks.json")).unwrap();
        let result = reconcile(&operation);
        assert_eq!(
            result.register().marks.get(&mark.id),
            Some(&mark),
            "{drift}"
        );
        assert_eq!(
            fs::read(root.canonical_path().join("health/retention-marks.json")).unwrap(),
            before
        );
    }
    let (_journal, root) = fixture();
    let operation = operation(&root);
    let mut direct = selection(SegmentLayout::Direct, &["empty.flac"]);
    let mut named = direct.clone();
    named.layout = SegmentLayout::Named;
    direct.target.dir = "120000_60_a".into();
    named.target.dir = direct.target.dir.clone();
    let mut same_key = direct.clone();
    same_key.target.dir = "120000_60_b".into();
    let mut marks = Vec::new();
    for selected in [&direct, &named, &same_key] {
        seed(&root, selected, "empty.flac", "empty");
        marks.push(reconciliation_mark(&operation, selected));
    }
    // Mark IDs do not encode layout. Use disjoint names at identical typed coordinates.
    let mut named = named.clone();
    named.names = vec!["named.flac".into()];
    seed(&root, &named, "named.flac", "empty");
    marks[1] = reconciliation_mark(&operation, &named);
    reconciliation_register(&root, &marks);
    reconciliation_keep(&root);
    let result = reconcile(&operation);
    assert_eq!(result.groups().len(), 3);
    assert!(result.register().marks.is_empty());
}

#[test]
fn reconciliation_busy_segment_does_not_block_unrelated_group() {
    let (_journal, root) = fixture();
    let operation = operation(&root);
    let busy = selection(SegmentLayout::Direct, &["empty.flac"]);
    let mut ready = busy.clone();
    ready.target.dir = "130000_60".into();
    let mut marks = Vec::new();
    for selected in [&busy, &ready] {
        seed(&root, selected, "empty.flac", "empty");
        marks.push(reconciliation_mark(&operation, selected));
    }
    reconciliation_register(&root, &marks);
    reconciliation_keep(&root);

    let _claims = OutputClaims::discover(
        &root.canonical_path().join(selection_path(&busy)),
        LockOptions {
            timeout: Duration::ZERO,
            ..LockOptions::default()
        },
    )
    .unwrap();
    let result = reconcile(&operation);
    assert_eq!(result.register().marks.len(), 1);
    assert_eq!(result.register().marks.get(&marks[0].id), Some(&marks[0]));
    assert!(
        result
            .groups()
            .iter()
            .any(|group| matches!(group.disposition(), OffloadReconciliationDisposition::Busy))
    );
}

#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "root substitution and teardown affect only disposable fixtures"
)]
fn reconciliation_final_cut_preserves_newer_marks_and_refuses_changed_inputs() {
    for drift in [
        "nonce", "state", "config", "raw", "sidecar", "root", "legacy",
    ] {
        let (journal, root) = fixture();
        let operation = operation(&root);
        let selected = selection(SegmentLayout::Direct, &["empty.flac"]);
        seed(&root, &selected, "empty.flac", "empty");
        let mut mark = reconciliation_mark(&operation, &selected);
        if drift == "legacy" {
            mark.offload = None;
            mark.target.dir = "120000_60".into();
            mark.id = MarkId::derive(mark.class, &mark.target, &mark.proposal.names);
        }
        reconciliation_register(&root, std::slice::from_ref(&mark));
        reconciliation_keep(&root);

        let root_hook = Arc::clone(&root);
        let mark_hook = mark.clone();
        let selected_hook = selected.clone();
        let (today, now) = time();
        let result = reconciliation::unix::reconcile_with_test_hook(
            &operation,
            today,
            now,
            Box::new(move || match drift {
                "nonce" | "state" => {
                    let mut newer = mark_hook.clone();
                    if drift == "nonce" {
                        let offer = newer.offload.take().unwrap();
                        newer.offload =
                            Some(crate::archive_proof::OffloadOffer::new(offer.proof).unwrap());
                    } else {
                        newer.state = crate::marks::MarkState::Failed(crate::marks::Failure {
                            at: "later".into(),
                            reason: "newer".into(),
                            staged: None,
                        });
                    }
                    reconciliation_register(&root_hook, &[newer]);
                }
                "config" => fs::write(
                    root_hook.canonical_path().join("config/journal.json"),
                    b"{}",
                )
                .unwrap(),
                "raw" => fs::write(
                    root_hook
                        .canonical_path()
                        .join(selection_path(&selected_hook))
                        .join("empty.flac"),
                    b"changed",
                )
                .unwrap(),
                "sidecar" => fs::write(
                    root_hook
                        .canonical_path()
                        .join(selection_path(&selected_hook))
                        .join("empty.jsonl"),
                    b"{}\n",
                )
                .unwrap(),
                "root" => {
                    let old = root_hook.canonical_path().with_extension("moved");
                    fs::rename(root_hook.canonical_path(), &old).unwrap();
                    fs::create_dir(root_hook.canonical_path()).unwrap();
                }
                "legacy" => {
                    let mut other = selected_hook.clone();
                    other.target.dir = "120000_60_new".into();
                    seed(&root_hook, &other, "empty.flac", "empty");
                }
                _ => panic!("unknown fixture"),
            }),
        );
        if drift == "root" {
            assert!(result.is_err());
            assert!(root.canonical_path().is_dir());
            // The moved disposable fixture remains owned by this test.
            let moved = root.canonical_path().with_extension("moved");
            fs::remove_dir_all(&moved).unwrap();
        } else {
            let register = crate::review_context::load_at_root(&root).unwrap();
            assert_eq!(register.marks.len(), 1, "{drift}");
            if drift == "nonce" || drift == "state" {
                assert_ne!(register.marks.get(&mark.id), Some(&mark));
            } else {
                assert_eq!(register.marks.get(&mark.id), Some(&mark));
            }
        }
        let _journal = journal;
    }
}

#[test]
fn reconciliation_uncertain_second_write_retains_first_and_stops_before_third() {
    use solstone_core_journal_io::atomic::{
        BoundPublicationPrimitive, run_with_bound_publication_fault,
    };
    let (_journal, root) = fixture();
    let operation = operation(&root);
    let mut marks = Vec::new();
    for dir in ["120000_60", "130000_60", "140000_60"] {
        let mut selected = selection(SegmentLayout::Direct, &["empty.flac"]);
        selected.target.dir = dir.into();
        seed(&root, &selected, "empty.flac", "empty");
        marks.push(reconciliation_mark(&operation, &selected));
    }
    reconciliation_register(&root, &marks);
    reconciliation_keep(&root);

    let (result, consumed) =
        run_with_bound_publication_fault(BoundPublicationPrimitive::ParentSync, 2, 5, || {
            reconcile_offload_empty(&operation, time().0, time().1)
        });
    assert!(consumed);
    let failure = result.err().unwrap();
    assert!(matches!(
        failure.reason(),
        OffloadReconciliationStop::Publication
    ));
    assert_eq!(failure.groups().len(), 2);
    assert!(
        matches!(failure.groups()[0].disposition(), OffloadReconciliationDisposition::Publication { outcome, .. }
            if matches!(outcome.as_ref(), crate::mark_mutation::BoundMutationOutcome::Published(_)))
    );
    assert!(
        matches!(failure.groups()[1].disposition(), OffloadReconciliationDisposition::Publication { outcome, .. }
            if matches!(outcome.as_ref(), crate::mark_mutation::BoundMutationOutcome::PublishedUncertain { .. }))
    );
    assert_eq!(failure.last_confirmed_register().unwrap().marks.len(), 2);
    let observed = crate::review_context::load_at_root(&root).unwrap();
    assert_eq!(observed.marks.len(), 1);
    assert_eq!(observed.marks.get(&marks[2].id), Some(&marks[2]));
}
