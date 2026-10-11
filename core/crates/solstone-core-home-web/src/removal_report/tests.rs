// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    reason = "isolated synthetic journals and public receipt forwarding controls"
)]

use super::*;
use solstone_core_journal_io::media::{
    Cleanup, Confirmation, Durability, OriginalName, RemovalEffect, RemovalFailure,
};
use solstone_core_retention::mark_mutation::{
    ConditionalResult, MutationStatus, RegisterUncertainty,
};
use solstone_core_retention::marks::Register;

#[test]
fn native_effects_keep_independent_uncertainty_and_failure_facts() {
    for effect in [
        RemovalEffect::NotClaimed,
        RemovalEffect::ClaimUncertain,
        RemovalEffect::Claimed,
        RemovalEffect::DeletionUncertain,
        RemovalEffect::DeletePending,
        RemovalEffect::Deleted,
    ] {
        let receipt = RemovalReceipt {
            effect,
            original_name: OriginalName::Occupied,
            claim_confirmation: Confirmation::Unverified,
            file_durability: Durability::Synced,
            source_durability: Durability::Unproven,
            claim_durability: Durability::Synced,
            stage_parent_durability: Durability::Unproven,
            cleanup: Cleanup::Pending,
            claimed_identity: None,
            restoration_attempted: true,
            failures: vec![RemovalFailure {
                phase: RemovalPhase::Completion,
                error: io::Error::new(io::ErrorKind::PermissionDenied, "private source path"),
            }],
        };
        let facts = removal_facts(&receipt);
        assert_eq!(facts["effect"], serde_json::to_value(effect).unwrap());
        assert_eq!(facts["original_name"], "occupied");
        assert_eq!(facts["claim_confirmation"], "unverified");
        assert_eq!(facts["file_durability"], "synced");
        assert_eq!(facts["source_durability"], "unproven");
        assert_eq!(facts["claim_durability"], "synced");
        assert_eq!(facts["stage_parent_durability"], "unproven");
        assert_eq!(facts["cleanup"], "pending");
        assert_eq!(facts["restoration_attempted"], true);
        assert_eq!(
            facts["failures"],
            json!([{
                "phase":"completion", "error":io::ErrorKind::PermissionDenied.to_string(),
            }])
        );
        assert!(!facts.to_string().contains("private source path"));
    }
}

#[test]
fn published_mutation_keeps_uncertainty_without_register_payload() {
    let result = Ok(BoundMutationOutcome::PublishedUncertain {
        candidate: ConditionalResult {
            status: MutationStatus::Applied,
            register: Register::empty(),
        },
        uncertainty: RegisterUncertainty {
            durability: Some(io::Error::other("private sync detail")),
            observation: Some(io::Error::new(io::ErrorKind::NotFound, "private root")),
            bindings: vec![ReviewRefusal::RootChanged],
            lock: Some(ExistingParentLockError::ParentChanged {
                parent: "/private/config".into(),
            }),
        },
    });
    let facts = mutation_facts(&result);
    assert_eq!(facts["publication"], "published_uncertain");
    assert_eq!(facts["status"], "applied");
    assert_eq!(facts["binding_errors"], json!(["root_changed"]));
    assert_eq!(facts["lock_error"]["kind"], "parent_changed");
    assert!(facts["durability_error"].is_string());
    assert!(facts["observation_error"].is_string());
    assert!(facts.get("register").is_none());
    assert!(!facts.to_string().contains("private"));
}

#[cfg(unix)]
#[test]
fn actual_owner_preserves_success_post_commit_failure_and_stale_refusal() {
    use chrono::{NaiveDate, TimeZone, Utc};
    use solstone_core_journal_io::JournalRoot;
    use solstone_core_retention::marks::{Mark, MarkId, MarkState, Proposal, RemovalClass};
    use solstone_core_retention::remove_marked::remove_reviewed;
    use solstone_core_retention::review_context::{check_review_context, read_review_context};
    use std::{fs, sync::Arc};

    for scenario in 0..3 {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("config")).unwrap();
        fs::create_dir(temp.path().join("health")).unwrap();
        fs::write(
            temp.path().join("config/journal.json"),
            br#"{"retention":{"raw_media":"days","raw_media_days":1}}"#,
        )
        .unwrap();
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        let target = solstone_core_retention::Target {
            day: "20260701".into(),
            stream: "_default".into(),
            dir: "prefix_070000_17_suffix".into(),
        };
        let relative =
            solstone_core_retention::layout::segment_rel(&target.day, &target.stream, &target.dir);
        let segment = temp.path().join(&relative);
        fs::create_dir_all(&segment).unwrap();
        fs::create_dir_all(temp.path().join("chronicle/20260701/health")).unwrap();
        fs::write(segment.join("audio.flac"), b"raw").unwrap();
        fs::write(segment.join("audio.jsonl"), concat!(
            "{\"_solstone_processing\":{\"schema\":\"solstone.processing.v1\",\"state\":\"analyzed\",\"reason_code\":\"ok\",\"handler\":\"transcribe\",\"input_size\":3,\"attempted_at\":\"2026-07-01T00:00:00Z\"}}\n",
            "{\"start\":0,\"text\":\"x\"}\n",
        )).unwrap();
        let names = vec!["audio.flac".to_owned()];
        let mark = Mark {
            id: MarkId::derive(RemovalClass::PolicyRawRelease, &target, &names),
            target,
            class: RemovalClass::PolicyRawRelease,
            marked_at: "private proposal value".into(),
            proposal: Proposal {
                names,
                bytes: 3,
                reason: "release".into(),
            },
            state: MarkState::Marked,
            offload: None,
        };
        let mut register = Register::empty();
        register.marks.insert(mark.id.clone(), mark.clone());
        let register_path = temp.path().join("health/retention-marks.json");
        fs::write(&register_path, serde_json::to_vec(&register).unwrap()).unwrap();
        let rendered = read_review_context(&root, std::slice::from_ref(&mark.id)).unwrap();
        let checked = check_review_context(Arc::clone(&root), &rendered).unwrap();
        if scenario == 1 {
            fs::create_dir(segment.join("events.jsonl")).unwrap();
        } else if scenario == 2 {
            register.marks.get_mut(&mark.id).unwrap().marked_at = "changed".into();
            fs::write(&register_path, serde_json::to_vec(&register).unwrap()).unwrap();
        }
        let owner = remove_reviewed(
            checked,
            NaiveDate::from_ymd_opt(2026, 8, 6).unwrap(),
            Utc.with_ymd_and_hms(2026, 8, 6, 0, 0, 0).single().unwrap(),
        );
        let view = ordinary(&owner);
        let facts = serde_json::to_value(&view).unwrap();
        assert_eq!(
            facts["outcome"],
            serde_json::to_value(owner.outcome()).unwrap()
        );
        assert_eq!(
            facts["attempts"].as_array().unwrap().len(),
            owner.attempts().len()
        );
        assert!(
            !facts
                .to_string()
                .contains(&temp.path().to_string_lossy().to_string())
        );
        assert!(!facts.to_string().contains("private proposal value"));
        assert!(facts.get("settled").is_none());
        if scenario == 2 {
            assert!(owner.refusal().is_some());
            assert!(segment.join("audio.flac").exists());
            assert!(facts["outcome"]["halted"].is_object());
            assert_eq!(facts["attempts"], json!([]));
            assert!(facts["refusal"].is_object());
            assert!(facts["mutation"].is_null());
        } else {
            assert_eq!(owner.outcome().removed_paths().count(), 1);
            assert!(!segment.join("audio.flac").exists());
            assert_eq!(
                facts["outcome"]["targets"][0]["removed"],
                json!([format!("{relative}/audio.flac")])
            );
            assert_eq!(facts["attempts"][0]["native"]["effect"], "deleted");
            assert_eq!(facts["carrier_preparation"]["confirmation"], "confirmed");
            assert!(facts["carrier_failure"].is_null());
            if scenario == 1 {
                assert!(facts["outcome"]["targets"][0]["post_commit_failure"].is_object());
                assert_eq!(facts["attempts"][0]["append"]["effect"], "not_attempted");
                assert_eq!(
                    facts["attempts"][0]["marker"]["publication"]["effect"],
                    "published"
                );
            } else {
                assert_eq!(facts["attempts"][0]["append"]["effect"], "appended");
                assert_eq!(
                    facts["attempts"][0]["marker"]["publication"]["effect"],
                    "published"
                );
                assert_eq!(facts["mutation"]["publication"], "published");
                assert!(facts["outcome"]["targets"][0]["post_commit_failure"].is_null());
            }
        }
    }
}
