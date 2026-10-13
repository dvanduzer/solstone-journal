// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use serde_json::json;
use solstone_core_journal_io::{
    ClaimRemovalPrimitive, LockOptions, run_with_claim_removal_barrier,
    run_with_claim_removal_fault,
};
use std::fs;

fn fixture() -> (
    tempfile::TempDir,
    Arc<JournalRoot>,
    TeardownConfiguration,
    BoundConfigPublication,
) {
    let temp = tempfile::tempdir().unwrap();
    let journal = temp.path().join("journal");
    fs::create_dir_all(journal.join("config")).unwrap();
    fs::write(
        journal.join("config/journal.json"),
        serde_json::to_vec(&json!({
            "backup":{"enabled":true,"mode":"operated"},"other":7
        }))
        .unwrap(),
    )
    .unwrap();
    crate::save_hosted_binding(
        &journal,
        &crate::HostedBinding {
            broker_endpoint: "https://example.invalid".into(),
            account_id: "account".into(),
            instance_id: "instance".into(),
            bucket: "bucket".into(),
            prefix: "prefix/".into(),
            broker_token: "secret".into(),
        },
    )
    .unwrap();
    let root = Arc::new(JournalRoot::open(&journal).unwrap());
    let captured = TeardownConfiguration::capture(&root).unwrap();
    let BoundConfigOutcome::Published(publication) = captured.clear_configuration().unwrap() else {
        panic!("must publish")
    };
    captured.revalidate_published_clear(&publication).unwrap();
    (temp, root, captured, publication)
}

#[test]
fn actual_removal_keeps_both_guards_until_explicit_release() {
    let (temp, root, captured, publication) = fixture();
    let journal = temp.path().join("journal");
    let mut held = captured.begin_binding_finalization(&publication).unwrap();
    let options = LockOptions {
        timeout: Duration::ZERO,
        ..LockOptions::default()
    };
    let blocked = solstone_core_journal_config_write::conditional_update_journal_config_at_root(
        &root,
        &publication.candidate_sha256,
        |_| panic!("lock held"),
        options,
    );
    assert!(matches!(
        blocked,
        Err(BoundConfigRefusal::Lock(ExistingParentLockError::Timeout(
            _
        )))
    ));
    struct TimeoutReset;
    impl Drop for TimeoutReset {
        fn drop(&mut self) {
            crate::hosted::BINDING_LOCK_TIMEOUT.with(|v| v.set(None));
        }
    }
    crate::hosted::BINDING_LOCK_TIMEOUT.with(|v| v.set(Some(Duration::ZERO)));
    let _reset = TimeoutReset;
    let binding = captured.binding().unwrap();
    assert!(crate::save_hosted_binding(&journal, binding).is_err());
    assert!(crate::delete_hosted_binding(&journal).is_err());
    let receipt = held.remove_binding().clone();
    assert!(receipt.attempted());
    assert!(receipt.complete());
    assert!(matches!(
        receipt.native(),
        Some(Ok(ClaimRemovalOutcome::Removed))
    ));
    held.revalidate_removed().unwrap();
    assert!(!journal.join("backup/hosted/binding.json").exists());
    assert!(crate::save_hosted_binding(&journal, binding).is_err());
    assert!(Arc::ptr_eq(&receipt, held.remove_binding()));
    let final_receipt = held.into_receipt().unwrap();
    assert!(Arc::ptr_eq(&receipt, &final_receipt));
    crate::save_hosted_binding(&journal, binding).unwrap();
    let current = solstone_core_journal_config::read_journal_config(&journal)
        .unwrap()
        .sha256
        .unwrap();
    solstone_core_journal_config_write::conditional_update_journal_config_at_root(
        &root,
        &current,
        |_| None,
        options,
    )
    .unwrap();
}

#[test]
fn config_drift_between_clear_and_lock_or_after_lock_refuses_binding_mutation() {
    for after_lock in [false, true] {
        let (temp, _, captured, publication) = fixture();
        let path = temp.path().join("journal/config/journal.json");
        let mut held =
            after_lock.then(|| captured.begin_binding_finalization(&publication).unwrap());
        fs::write(&path, b"{\"newer\":true}").unwrap();
        if let Some(held) = held.as_mut() {
            let receipt = held.remove_binding();
            assert!(!receipt.attempted());
            assert!(!receipt.complete());
        } else {
            assert!(captured.begin_binding_finalization(&publication).is_err());
        }
        assert!(
            temp.path()
                .join("journal/backup/hosted/binding.json")
                .exists()
        );
        assert_eq!(fs::read(path).unwrap(), b"{\"newer\":true}");
    }
}

#[test]
fn identical_binding_or_lock_replacement_is_not_adopted() {
    for replace_lock in [false, true] {
        let (temp, _, captured, publication) = fixture();
        let mut held = captured.begin_binding_finalization(&publication).unwrap();
        let parent = temp.path().join("journal/backup/hosted");
        let path = parent.join(if replace_lock { BINDING_LOCK } else { BINDING });
        let bytes = fs::read(&path).unwrap();
        fs::rename(&path, parent.join("old-entry")).unwrap();
        fs::write(&path, &bytes).unwrap();
        let receipt = held.remove_binding();
        assert!(!receipt.attempted());
        assert!(!receipt.complete());
        assert_eq!(fs::read(path).unwrap(), bytes);
        assert!(parent.join(BINDING).exists());
    }
}

#[test]
fn root_replacement_after_claim_keeps_actual_removal_and_confirmation_failure() {
    let (temp, _, captured, publication) = fixture();
    let mut held = captured.begin_binding_finalization(&publication).unwrap();
    let journal = temp.path().join("journal");
    let moved = temp.path().join("original");
    let bytes = fs::read(journal.join("backup/hosted/binding.json")).unwrap();
    let replacement = bytes.clone();
    let source = journal.clone();
    let old = moved.clone();
    let ((), fired) = run_with_claim_removal_barrier(
        ClaimRemovalPrimitive::BeforeClaim,
        1,
        move || {
            fs::rename(&source, &old).unwrap();
            fs::create_dir_all(source.join("backup/hosted")).unwrap();
            fs::write(source.join("backup/hosted/binding.json"), replacement).unwrap();
        },
        || {
            held.remove_binding();
        },
    );
    assert!(fired);
    let receipt = held.receipt().unwrap();
    assert!(receipt.attempted());
    assert!(matches!(
        receipt.native(),
        Some(Ok(ClaimRemovalOutcome::Removed))
    ));
    assert!(receipt.confirmation().is_err());
    assert!(!receipt.complete());
    assert!(held.revalidate_removed().is_err());
    assert!(!moved.join("backup/hosted/binding.json").exists());
    assert_eq!(
        fs::read(journal.join("backup/hosted/binding.json")).unwrap(),
        bytes
    );
}

#[test]
fn sync_failure_and_oversized_claim_remain_independent_holds() {
    let (temp, _, captured, publication) = fixture();
    let mut held = captured.begin_binding_finalization(&publication).unwrap();
    let ((), fired) = run_with_claim_removal_fault(
        ClaimRemovalPrimitive::DirectorySync,
        1,
        nix::libc::EIO,
        || {
            held.remove_binding();
        },
    );
    assert!(fired);
    let receipt = held.receipt().unwrap();
    assert!(matches!(
        receipt.native(),
        Some(Ok(ClaimRemovalOutcome::RemovedDurabilityUncertain))
    ));
    assert!(receipt.confirmation().is_ok());
    assert!(!receipt.complete());
    assert!(held.revalidate_removed().is_err());
    assert!(
        !temp
            .path()
            .join("journal/backup/hosted/binding.json")
            .exists()
    );
    let (temp, _, captured, publication) = fixture();
    let mut held = captured.begin_binding_finalization(&publication).unwrap();
    let claim = temp
        .path()
        .join("journal/backup/hosted")
        .join(held.claim.as_os_str());
    let changed = claim.clone();
    let ((), fired) = run_with_claim_removal_barrier(
        ClaimRemovalPrimitive::BeforeInspection,
        1,
        move || fs::write(changed, vec![b'x'; MAX_BINDING_BYTES + 1]).unwrap(),
        || {
            held.remove_binding();
        },
    );
    assert!(fired);
    let receipt = held.receipt().unwrap();
    assert!(matches!(
        receipt.native(),
        Some(Err(ClaimRemovalError::PostClaimInspection {
            source: FlatDirectoryError::SizeLimitExceeded { .. },
            ..
        }))
    ));
    assert!(!receipt.complete());
    assert_eq!(
        fs::metadata(claim).unwrap().len(),
        (MAX_BINDING_BYTES + 1) as u64
    );
}

#[test]
fn descriptive_publication_cannot_substitute_for_original_clear_candidate() {
    let (_temp, _root, captured, mut publication) = fixture();
    publication.candidate_sha256 = captured.sha256().to_owned();
    assert!(matches!(
        captured.begin_binding_finalization(&publication),
        Err(BindingFinalizationRefusal::Configuration(
            Refusal::ConfigurationChanged
        ))
    ));
}

#[test]
fn compact_config_cannot_bypass_pretty_clear_candidate_limit() {
    let raw = json!({
        "backup":{"enabled":true,"mode":"operated"},
        "other": vec![0u8; MAX_BOUND_CONFIG_BYTES / 3],
    });
    let raw = raw.as_object().unwrap();
    assert!(serde_json::to_vec(raw).unwrap().len() < MAX_BOUND_CONFIG_BYTES);
    let backup = crate::merge_backup_config(raw);
    let clear = crate::state::cleared_backup_configuration(raw, &backup).unwrap();
    assert!(serde_json::to_vec_pretty(&clear).unwrap().len() > MAX_BOUND_CONFIG_BYTES);
    assert!(super::super::expected_clear_fingerprint(raw, &backup).is_none());
}
