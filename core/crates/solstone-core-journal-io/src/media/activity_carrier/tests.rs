// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use crate::activity::{ActivityAdmissionError, TeardownLease};
use std::fs;

fn fixture() -> (tempfile::TempDir, Arc<ActivityLease>) {
    let temp = match std::env::var_os("SOLSTONE_MEDIA_TEST_ROOT") {
        Some(root) => tempfile::tempdir_in(root).unwrap(),
        None => tempfile::tempdir().unwrap(),
    };
    let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
    let activity = ActivityLease::try_acquire(root).unwrap();
    (temp, activity)
}

fn failure(
    result: Result<RootActivityCarrier, ActivityCarrierPreparationFailure>,
) -> ActivityCarrierPreparationFailure {
    result.err().expect("preparation must refuse")
}

#[test]
fn lost_activity_refuses_before_creating_a_reservation() {
    let (temp, activity) = fixture();
    fs::rename(
        temp.path().join("backup-activity.lock"),
        temp.path().join("old.lock"),
    )
    .unwrap();
    let failed = failure(RootActivityCarrier::prepare(activity));
    assert!(failed.diagnostic_path().is_none());
    assert_eq!(failed.receipt().stage_durability(), Durability::Unproven);
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[cfg(windows)]
#[test]
fn windows_unproven_directory_durability_holds_created_reservation() {
    let (_temp, activity) = fixture();
    let root = activity.journal_root().clone();
    let failed = failure(RootActivityCarrier::prepare(activity));
    assert_eq!(failed.error().kind(), io::ErrorKind::Unsupported);
    assert_eq!(failed.receipt().stage_durability(), Durability::Unproven);
    assert_eq!(failed.receipt().root_durability(), Durability::Unproven);
    assert_eq!(failed.receipt().confirmation(), Confirmation::Unverified);
    let path = failed.diagnostic_path().unwrap();
    assert!(path.is_dir());
    assert!(matches!(
        TeardownLease::try_acquire(root.clone()),
        Err(ActivityAdmissionError::Busy)
    ));
    drop(failed);
    assert!(path.is_dir());
    TeardownLease::try_acquire(root).unwrap();
}

#[cfg(unix)]
#[test]
fn preparation_and_owned_retirement_have_independent_facts_and_retain_lease() {
    let (_temp, activity) = fixture();
    let root = activity.journal_root().clone();
    let carrier = RootActivityCarrier::prepare(activity.clone()).unwrap();
    assert!(Arc::ptr_eq(carrier.activity(), &activity));
    assert!(Arc::ptr_eq(carrier.journal_root(), &root));
    assert_eq!(carrier.preparation().stage_durability(), Durability::Synced);
    assert_eq!(carrier.preparation().root_durability(), Durability::Synced);
    assert_eq!(
        carrier.preparation().confirmation(),
        Confirmation::Confirmed
    );
    let path = carrier.diagnostic_path();
    drop(activity);
    let outcome = carrier.retire_after_settlement();
    assert_eq!(
        outcome.receipt.effect(),
        ActivityCarrierRetirementEffect::Removed
    );
    assert_eq!(outcome.receipt.root_durability(), Durability::Synced);
    assert_eq!(outcome.receipt.confirmation(), Confirmation::Confirmed);
    assert!(outcome.receipt.failures().is_empty());
    assert!(!path.exists());
    assert!(matches!(
        TeardownLease::try_acquire(root.clone()),
        Err(ActivityAdmissionError::Busy)
    ));
    drop(outcome);
    TeardownLease::try_acquire(root).unwrap();
}

#[cfg(unix)]
#[test]
fn preparation_faults_preserve_reservation_and_each_completed_flush() {
    for (cut, stage, root) in [
        (
            MediaPrimitive::StageCreated,
            Durability::Unproven,
            Durability::Unproven,
        ),
        (
            MediaPrimitive::CarrierStageSync,
            Durability::Unproven,
            Durability::Unproven,
        ),
        (
            MediaPrimitive::CarrierRootSync,
            Durability::Synced,
            Durability::Unproven,
        ),
        (
            MediaPrimitive::CarrierConfirmation,
            Durability::Synced,
            Durability::Synced,
        ),
    ] {
        let (_temp, activity) = fixture();
        let root_handle = activity.journal_root().clone();
        let (result, consumed) = run_with_media_hook(
            cut,
            || Err(io::Error::other("cut")),
            || RootActivityCarrier::prepare(activity),
        );
        assert!(consumed);
        let failed = failure(result);
        assert_eq!(failed.receipt().stage_durability(), stage);
        assert_eq!(failed.receipt().root_durability(), root);
        assert_eq!(failed.receipt().confirmation(), Confirmation::Unverified);
        let path = failed.diagnostic_path().unwrap();
        assert!(path.is_dir());
        assert!(matches!(
            TeardownLease::try_acquire(root_handle.clone()),
            Err(ActivityAdmissionError::Busy)
        ));
        drop(failed); // Unknown cooperation: no cleanup method is called.
        assert!(path.is_dir());
        TeardownLease::try_acquire(root_handle).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn known_unlaunched_failure_can_retire_only_its_owned_empty_reservation() {
    let (_temp, activity) = fixture();
    let (result, consumed) = run_with_media_hook(
        MediaPrimitive::CarrierRootSync,
        || Err(io::Error::other("root flush")),
        || RootActivityCarrier::prepare(activity),
    );
    assert!(consumed);
    let failed = failure(result);
    let path = failed.diagnostic_path().unwrap();
    let outcome = failed.retire_after_settlement();
    assert_eq!(
        outcome.receipt.effect(),
        ActivityCarrierRetirementEffect::Removed
    );
    assert_eq!(outcome.receipt.confirmation(), Confirmation::Confirmed);
    assert!(!path.exists());
    // Preparation error and preparation durability remain independently retained.
    assert_eq!(outcome.owned.error().to_string(), "root flush");
    assert_eq!(
        outcome.owned.receipt().root_durability(),
        Durability::Unproven
    );
}

#[cfg(unix)]
#[test]
fn nonempty_final_confirmation_holds_failure_and_competing_content() {
    let (_temp, activity) = fixture();
    let root_path = activity.journal_root().canonical_path().to_owned();
    let (result, consumed) = run_with_media_hook(
        MediaPrimitive::CarrierConfirmation,
        move || {
            let stage = fs::read_dir(root_path)?
                .map(Result::unwrap)
                .find(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".tmp-offload-")
                })
                .unwrap();
            fs::write(stage.path().join("competitor"), b"keep")
        },
        || RootActivityCarrier::prepare(activity),
    );
    assert!(consumed);
    let failed = failure(result);
    let path = failed.diagnostic_path().unwrap();
    let outcome = failed.retire_after_settlement();
    assert_eq!(
        outcome.receipt.effect(),
        ActivityCarrierRetirementEffect::NotAttempted
    );
    assert_eq!(fs::read(path.join("competitor")).unwrap(), b"keep");
}

#[cfg(unix)]
#[test]
fn dropping_prepared_carrier_leaves_reservation_after_lease_release() {
    let (_temp, activity) = fixture();
    let root = activity.journal_root().clone();
    let carrier = RootActivityCarrier::prepare(activity).unwrap();
    let path = carrier.diagnostic_path();
    drop(carrier);
    assert!(path.is_dir());
    TeardownLease::try_acquire(root).unwrap();
}

#[cfg(unix)]
#[test]
fn root_rebind_before_parent_flush_preserves_reservation_and_replacement() {
    let (temp, activity) = fixture();
    let current = temp.path().to_owned();
    let moved = current.with_extension("old");
    let moved_hook = moved.clone();
    let (result, consumed) = run_with_media_hook(
        MediaPrimitive::CarrierRootSync,
        move || {
            fs::rename(&current, &moved_hook)?;
            fs::create_dir(&current)?;
            fs::write(current.join("competitor"), b"keep")
        },
        || RootActivityCarrier::prepare(activity),
    );
    assert!(consumed);
    let failed = failure(result);
    assert_eq!(failed.receipt().root_durability(), Durability::Unproven);
    let outcome = failed.retire_after_settlement();
    assert_eq!(
        outcome.receipt.effect(),
        ActivityCarrierRetirementEffect::NotAttempted
    );
    assert_eq!(fs::read(temp.path().join("competitor")).unwrap(), b"keep");
    assert!(fs::read_dir(&moved).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".tmp-offload-")
    }));
    drop(outcome);
    fs::remove_dir_all(moved).unwrap();
}

#[cfg(unix)]
#[test]
fn flush_failure_does_not_erase_removed_effect_or_absence_confirmation() {
    let (_temp, activity) = fixture();
    let carrier = RootActivityCarrier::prepare(activity).unwrap();
    let (outcome, consumed) = run_with_media_hook(
        MediaPrimitive::CarrierRetirementRootSync,
        || Err(io::Error::other("root flush")),
        || carrier.retire_after_settlement(),
    );
    assert!(consumed);
    assert_eq!(
        outcome.receipt.effect(),
        ActivityCarrierRetirementEffect::Removed
    );
    assert_eq!(outcome.receipt.root_durability(), Durability::Unproven);
    assert_eq!(outcome.receipt.confirmation(), Confirmation::Confirmed);
    assert_eq!(outcome.receipt.failures().len(), 1);
}

#[cfg(unix)]
#[test]
fn late_confirmation_failure_keeps_removal_and_sync_facts() {
    let (_temp, activity) = fixture();
    let carrier = RootActivityCarrier::prepare(activity).unwrap();
    let (outcome, consumed) = run_with_media_hook(
        MediaPrimitive::CarrierRetirementConfirmation,
        || Err(io::Error::other("confirmation")),
        || carrier.retire_after_settlement(),
    );
    assert!(consumed);
    assert_eq!(
        outcome.receipt.effect(),
        ActivityCarrierRetirementEffect::Removed
    );
    assert_eq!(outcome.receipt.root_durability(), Durability::Synced);
    assert_eq!(outcome.receipt.confirmation(), Confirmation::Unverified);
}

#[cfg(unix)]
#[test]
fn late_entry_or_stage_replacement_holds_preparation_and_failure_cleanup() {
    use std::os::unix::fs::PermissionsExt;
    for replace_entry in [true, false] {
        let (temp, activity) = fixture();
        let root_path = temp.path().to_owned();
        let (result, consumed) = run_with_media_hook(
            MediaPrimitive::CarrierConfirmation,
            move || {
                let path = if replace_entry {
                    root_path.join("backup-activity.lock")
                } else {
                    fs::read_dir(&root_path)?
                        .map(Result::unwrap)
                        .find(|entry| {
                            entry
                                .file_name()
                                .to_string_lossy()
                                .starts_with(".tmp-offload-")
                        })
                        .unwrap()
                        .path()
                };
                fs::rename(&path, path.with_extension("held"))?;
                if replace_entry {
                    fs::write(&path, b"keep")?;
                    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                } else {
                    fs::create_dir(path)
                }
            },
            || RootActivityCarrier::prepare(activity),
        );
        assert!(consumed);
        let failed = failure(result);
        assert_eq!(failed.receipt().root_durability(), Durability::Synced);
        assert_eq!(failed.receipt().confirmation(), Confirmation::Unverified);
        let path = failed.diagnostic_path().unwrap();
        let outcome = failed.retire_after_settlement();
        assert_eq!(
            outcome.receipt.effect(),
            ActivityCarrierRetirementEffect::NotAttempted
        );
        assert!(path.is_dir());
        if replace_entry {
            assert_eq!(
                fs::read(temp.path().join("backup-activity.lock")).unwrap(),
                b"keep"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn empty_replacement_before_final_check_is_preserved() {
    let (_temp, activity) = fixture();
    let carrier = RootActivityCarrier::prepare(activity).unwrap();
    let path = carrier.diagnostic_path();
    let path_hook = path.clone();
    let original = path.with_extension("held");
    let original_hook = original.clone();
    let (outcome, consumed) = run_with_media_hook(
        MediaPrimitive::StageRemove,
        move || {
            fs::rename(&path_hook, original_hook)?;
            fs::create_dir(path_hook)
        },
        || carrier.retire_after_settlement(),
    );
    assert!(consumed);
    assert_eq!(
        outcome.receipt.effect(),
        ActivityCarrierRetirementEffect::Attempted
    );
    assert_eq!(outcome.receipt.confirmation(), Confirmation::Unverified);
    assert!(path.is_dir());
    assert!(original.is_dir());
}

#[cfg(unix)]
#[test]
fn uncooperating_empty_swap_at_native_rmdir_cut_is_explicitly_unsupported() {
    let (_temp, activity) = fixture();
    let carrier = RootActivityCarrier::prepare(activity).unwrap();
    let path = carrier.diagnostic_path();
    let path_hook = path.clone();
    let original = path.with_extension("held");
    let original_hook = original.clone();
    let (outcome, consumed) = run_with_media_hook(
        MediaPrimitive::NativeDirectoryRemoveCut,
        move || {
            fs::rename(&path_hook, original_hook)?;
            fs::create_dir(path_hook)
        },
        || carrier.retire_after_settlement(),
    );
    assert!(consumed);
    // This deliberately violates the qualified-cooperation precondition. Native
    // rmdir removed the empty replacement; the original reservation survives.
    // The receipt cannot certify cooperation or unconditional exact retirement.
    assert_eq!(
        outcome.receipt.effect(),
        ActivityCarrierRetirementEffect::Removed
    );
    assert!(!path.exists());
    assert!(original.is_dir());
}
