// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

const OPERATION: &str = "0123456789abcdef0123456789abcdef";

struct Fixture {
    temp: tempfile::TempDir,
    root: Arc<JournalRoot>,
    binding: ArchiveRecoveryBinding,
}

impl Fixture {
    fn new() -> Self {
        let temp = match std::env::var_os("SOLSTONE_MEDIA_TEST_ROOT") {
            Some(root) => tempfile::tempdir_in(root).unwrap(),
            None => tempfile::tempdir().unwrap(),
        };
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        let lease = ActivityLease::try_acquire(Arc::clone(&root)).unwrap();
        let carrier = crate::media::RootActivityCarrier::prepare(Arc::clone(&lease)).unwrap();
        let raw = activity_obligations::observe_obligations(&root).unwrap();
        let NativeObligation::Stage { entry, .. } = &raw.entries()[0] else {
            panic!()
        };
        let device = fs::metadata(temp.path()).unwrap().dev();
        // Structural data for native continuity controls. No boot has occurred.
        let origin = crate::archive_origin::ArchiveOriginEvidence::from_json(&serde_json::to_vec(&serde_json::json!({
            "format_version":1,"boot_id":"11111111-1111-1111-1111-111111111111",
            "machine_app_id_sha256":"a".repeat(64),"root_identity":root.identity().observation_id(),
            "canonical_chain":[{"name":[],"identity":root.identity().observation_id()}],
            "filesystem":"ext4","storage_topology":"direct-pcie-nvme",
            "filesystem_device":{"major":((device >> 8) & 0xfff) | ((device >> 32) & 0xffff_f000),
                "minor":(device & 0xff) | ((device >> 12) & 0xffff_ff00)},
            "namespace_device":{"major":259,"minor":0},"partition":1
        })).unwrap())
        .unwrap();
        let binding = ArchiveRecoveryBinding::observed(
            origin,
            entry.name.to_str().unwrap().into(),
            entry.identity,
            lease.recovery_lock_observation().unwrap(),
            &"a".repeat(64),
        )
        .unwrap();
        drop(carrier);
        drop(lease);
        fs::create_dir_all(temp.path().join("health/offload/restore-intents")).unwrap();
        fs::write(
            temp.path()
                .join("health/offload/restore-intents")
                .join(format!("{OPERATION}.json")),
            b"original bounded owner bytes",
        )
        .unwrap();
        Self {
            temp,
            root,
            binding,
        }
    }

    fn resources(&self) -> Resources {
        let mut resources = Resources::new(Arc::clone(&self.root), OPERATION, self.binding.clone());
        resources.activity = Some(
            ActivityLease::try_acquire_existing(Arc::clone(&self.root))
                .ok()
                .unwrap(),
        );
        resources
    }

    fn record_path(&self) -> std::path::PathBuf {
        self.temp
            .path()
            .join("health/offload/restore-intents")
            .join(format!("{OPERATION}.json"))
    }

    // Current physical host, synthetic historical UUID. This structural fixture
    // never qualifies actual reboot or grants intent semantics.
    fn structural_current_owner(&self) -> RecoveredArchiveReservation {
        let mut resources = self.resources();
        let origin = crate::archive_origin::observe_archive_origin(Arc::clone(&self.root)).unwrap();
        let mut old = serde_json::to_value(origin.evidence()).unwrap();
        old["boot_id"] = if origin.evidence().boot_id() == "11111111-1111-1111-1111-111111111111" {
            "22222222-2222-2222-2222-222222222222"
        } else {
            "11111111-1111-1111-1111-111111111111"
        }
        .into();
        resources.binding.0.origin = crate::archive_origin::ArchiveOriginEvidence::from_json(
            &serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        resources.origin = Some(origin);
        resources.open_native().unwrap();
        resources.finish_admission_with(|_| {}).unwrap();
        RecoveredArchiveReservation { resources }
    }
}

#[test]
fn actual_existing_native_owners_and_named_bytes_survive_same_byte_record_replacement() {
    let f = Fixture::new();
    let mut resources = f.resources();
    resources.open_native().unwrap();
    assert_eq!(resources.initial.as_ref().unwrap().entries().len(), 2);
    let original = resources.record.as_ref().unwrap().clone();
    fs::rename(f.record_path(), f.record_path().with_extension("old")).unwrap();
    fs::write(f.record_path(), &original.bytes).unwrap();
    let replaced = resources.read_record().unwrap();
    assert_eq!(replaced.bytes, original.bytes);
    assert_ne!(replaced, original);
    assert!(
        resources
            .require_population(&activity_obligations::observe_obligations(&f.root).unwrap())
            .is_err()
    );
    resources.reread = Some(replaced);
    let failure = ArchiveRecoveryAdmissionFailure {
        resources,
        error: Resources::native("actual named replacement"),
    };
    assert_eq!(failure.record(), Some(&original));
    assert_ne!(failure.record(), failure.reread());
    let failure = std::thread::spawn(move || failure).join().unwrap();
    assert!(matches!(
        ActivityLease::try_acquire(Arc::clone(&f.root)),
        Err(ActivityAdmissionError::Busy)
    ));
    drop(failure);
    ActivityLease::try_acquire_existing(Arc::clone(&f.root))
        .ok()
        .unwrap();
}

#[test]
fn existing_only_native_refusals_preserve_every_prior_handle_without_creating_paths() {
    for cut in 0..9 {
        let f = Fixture::new();
        let reservation = f.temp.path().join(f.binding.reservation_name());
        let mut resources = f.resources();
        match cut {
            0 => fs::remove_dir(&reservation).unwrap(),
            1 => fs::write(reservation.join("unknown"), b"held").unwrap(),
            2 => {
                fs::rename(&reservation, f.temp.path().join("old-reservation")).unwrap();
                fs::create_dir(&reservation).unwrap();
            }
            3 => {
                fs::remove_dir(&reservation).unwrap();
                symlink(f.temp.path().join("health"), &reservation).unwrap();
            }
            4 => fs::remove_file(f.record_path()).unwrap(),
            5 => {
                fs::remove_file(f.record_path()).unwrap();
                symlink(f.temp.path().join("backup-activity.lock"), f.record_path()).unwrap();
            }
            6 => fs::File::create(f.record_path())
                .unwrap()
                .set_len((MAX_INTENT_BYTES + 1) as u64)
                .unwrap(),
            7 => fs::remove_dir_all(f.temp.path().join("health")).unwrap(),
            _ => fs::hard_link(
                f.temp.path().join("backup-activity.lock"),
                f.temp.path().join("lock-alias"),
            )
            .unwrap(),
        }
        assert!(resources.open_native().is_err(), "cut {cut}");
        assert!(resources.activity.is_some());
        if cut == 0 {
            assert!(!reservation.exists());
        }
        if cut == 4 {
            assert!(!f.record_path().exists());
        }
        if cut == 7 {
            assert!(!f.temp.path().join("health").exists());
        }
        assert!(matches!(
            ActivityLease::try_acquire(Arc::clone(&f.root)),
            Err(ActivityAdmissionError::Busy)
        ));
    }
}

#[test]
fn final_sole_walk_retains_new_stage_and_rejects_initial_population() {
    let f = Fixture::new();
    let mut resources = f.resources();
    resources.open_native().unwrap();
    fs::create_dir(f.temp.path().join(".tmp-offload-late")).unwrap();
    resources.census(true).unwrap();
    assert_eq!(resources.initial.as_ref().unwrap().entries().len(), 2);
    assert_eq!(resources.final_census.as_ref().unwrap().entries().len(), 3);
    assert!(
        resources
            .require_population(resources.final_census.as_ref().unwrap())
            .is_err()
    );
}

#[test]
fn failed_late_intent_read_keeps_initial_bytes_listed_entry_raw_errno_and_lease() {
    let f = Fixture::new();
    let mut resources = f.resources();
    resources.open_native().unwrap();
    let (result, consumed) = crate::media::run_with_media_hook(
        crate::media::MediaPrimitive::BeforePrefixRead,
        || Err(io::Error::from_raw_os_error(13)),
        || resources.census(true),
    );
    assert!(consumed);
    let error = result.unwrap_err();
    let failure = ArchiveRecoveryAdmissionFailure { resources, error };
    assert_eq!(
        failure.record().unwrap().bytes,
        b"original bounded owner bytes"
    );
    assert_eq!(failure.initial_census().unwrap().entries().len(), 2);
    let incomplete = failure.incomplete().unwrap();
    assert_eq!(incomplete.entries().len(), 1);
    assert_eq!(
        incomplete.encountered().unwrap().0,
        Path::new("health/offload/restore-intents").join(format!("{OPERATION}.json"))
    );
    assert_eq!(
        incomplete.error().source.as_ref().unwrap().raw_os_error(),
        Some(13)
    );
    let failure = std::thread::spawn(move || failure).join().unwrap();
    assert!(matches!(
        ActivityLease::try_acquire(Arc::clone(&f.root)),
        Err(ActivityAdmissionError::Busy)
    ));
    drop(failure);
    ActivityLease::try_acquire_existing(Arc::clone(&f.root))
        .ok()
        .unwrap();
}

#[test]
fn invalid_selector_and_absent_existing_lock_retains_root_without_creating_a_lock() {
    let f = Fixture::new();
    fs::remove_file(f.temp.path().join("backup-activity.lock")).unwrap();
    for operation in ["../bad", OPERATION] {
        let failure =
            RecoveredArchiveReservation::admit(Arc::clone(&f.root), operation, f.binding.clone())
                .err()
                .unwrap();
        assert!(Arc::ptr_eq(failure.root(), &f.root));
        assert!(failure.activity().is_none());
        assert!(!f.temp.path().join("backup-activity.lock").exists());
    }
}

#[test]
fn recovered_and_failed_physical_owners_are_send_static_without_tool_or_clock() {
    fn require<T: Send + 'static>() {}
    require::<RecoveredArchiveReservation>();
    require::<ArchiveRecoveryAdmissionFailure>();
    require::<IncompleteRecoveryObservation>();
}

#[test]
#[ignore = "actual ext4/local NVMe host; same-boot refusal only, no reboot credit"]
#[cfg(all(test, feature = "full-tests"))]
#[cfg(target_os = "linux")]
fn public_same_boot_refuses_with_live_helper_and_retains_fresh_guard() {
    let f = Fixture::new();
    let lease = ActivityLease::try_acquire(Arc::clone(&f.root)).unwrap();
    let origin = crate::archive_origin::observe_archive_origin(Arc::clone(&f.root)).unwrap();
    let binding = ArchiveRecoveryBinding::observed(
        origin.evidence().clone(),
        f.binding.reservation_name().into(),
        f.binding.reservation_identity(),
        lease.recovery_lock_observation().unwrap(),
        f.binding.repository_id(),
    )
    .unwrap();
    let mut helper = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    drop(lease);
    let result = RecoveredArchiveReservation::admit(Arc::clone(&f.root), OPERATION, binding);
    let live = helper.try_wait().unwrap().is_none();
    helper.kill().unwrap();
    helper.wait().unwrap();
    assert!(live);
    let failure = result.err().unwrap();
    assert!(matches!(failure.error(), RecoveryAdmissionError::SameBoot));
    assert!(failure.activity().is_some());
    assert!(matches!(
        ActivityLease::try_acquire(Arc::clone(&f.root)),
        Err(ActivityAdmissionError::Busy)
    ));
}

#[test]
#[ignore = "actual current origin plus synthetic old UUID; structural relation only, not reboot qualification"]
fn structural_old_uuid_dispatch_keeps_intent_unresolved_and_final_cut_observes_new_stage() {
    let f = Fixture::new();
    let mut resources = f.resources();
    let origin = crate::archive_origin::observe_archive_origin(Arc::clone(&f.root)).unwrap();
    let mut old = serde_json::to_value(origin.evidence()).unwrap();
    old["boot_id"] = if origin.evidence().boot_id() == "11111111-1111-1111-1111-111111111111" {
        "22222222-2222-2222-2222-222222222222"
    } else {
        "11111111-1111-1111-1111-111111111111"
    }
    .into();
    resources.binding.0.origin =
        crate::archive_origin::ArchiveOriginEvidence::from_json(&serde_json::to_vec(&old).unwrap())
            .unwrap();
    resources.origin = Some(origin);
    resources.open_native().unwrap();
    resources.finish_admission_with(|_| {}).unwrap();
    let owner = RecoveredArchiveReservation { resources };
    let checked = owner.revalidate_current_population().unwrap();
    assert_eq!(checked.entries(), owner.census().entries());
    assert_eq!(owner.read_current_record().unwrap(), *owner.record());
    for extra in [
        ".tmp-offload-unknown",
        "health/offload/restore-intents/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json",
    ] {
        let path = f.temp.path().join(extra);
        if extra.starts_with('.') {
            fs::create_dir(&path).unwrap();
        } else {
            fs::write(&path, b"other intent").unwrap();
        }
        let failure = owner.revalidate_current_population().unwrap_err();
        assert_eq!(failure.first_read(), Some(owner.record()));
        assert_eq!(failure.census().unwrap().entries().len(), 3);
        assert!(failure.final_read().is_none());
        if extra.starts_with('.') {
            fs::remove_dir(&path).unwrap();
        } else {
            fs::remove_file(&path).unwrap();
        }
        assert!(owner.revalidate_current_population().is_ok());
    }
    let (result, consumed) = crate::media::run_with_media_hook(
        crate::media::MediaPrimitive::BeforePrefixRead,
        || Err(io::Error::from_raw_os_error(13)),
        || owner.revalidate_current_population(),
    );
    assert!(consumed);
    let failure = result.unwrap_err();
    assert!(
        matches!(failure.error(), RecoveryAdmissionError::Native(error) if error.raw_os_error() == Some(13))
    );
    assert!(failure.first_read().is_none());
    assert_eq!(owner.record().bytes, b"original bounded owner bytes");
    let census = activity_obligations::observe_with_carrier(&f.root, owner.observation()).unwrap();
    assert_eq!(census.entries().len(), 2);
    assert!(matches!(
        census.unresolved_entries().next(),
        Some(NativeObligation::Intent { .. })
    ));
    assert_eq!(census.unresolved_entries().count(), 1);
    drop(census);
    let reopened = Arc::new(JournalRoot::open(f.temp.path()).unwrap());
    assert!(activity_obligations::observe_with_carrier(&reopened, owner.observation()).is_err());
    let mut resources = owner.resources;
    assert!(
        resources
            .finish_admission_with(|_| {
                fs::create_dir(f.temp.path().join(".tmp-offload-final-cut")).unwrap();
            })
            .is_err()
    );
    assert_eq!(resources.final_census.as_ref().unwrap().entries().len(), 3);
    fs::remove_dir(f.temp.path().join(".tmp-offload-final-cut")).unwrap();
    let owner = RecoveredArchiveReservation { resources };
    assert!(owner.revalidate_current_population().is_ok());
    let replacement = owner
        .current_population_with(
            || {},
            || {
                fs::rename(f.record_path(), f.temp.path().join("saved-original-record")).unwrap();
                fs::write(f.record_path(), b"original bounded owner bytes").unwrap();
            },
        )
        .unwrap_err();
    assert_eq!(replacement.first_read(), Some(owner.record()));
    assert_eq!(replacement.census().unwrap().entries().len(), 2);
    assert_eq!(
        replacement.final_read().unwrap().bytes,
        owner.record().bytes
    );
    assert_ne!(replacement.final_read(), Some(owner.record()));
    let late = Fixture::new();
    let late_owner = late.structural_current_owner();
    assert!(late_owner.revalidate_current_population().is_ok());
    let failure = late_owner
        .current_population_with(
            || {
                fs::set_permissions(late.record_path(), fs::Permissions::from_mode(0o000)).unwrap();
            },
            || {},
        )
        .unwrap_err();
    assert_eq!(failure.first_read(), Some(late_owner.record()));
    let partial = failure.incomplete().unwrap();
    assert_eq!(partial.entries().len(), 1);
    assert!(partial.encountered().is_some());
    assert_eq!(
        partial.error().source.as_ref().unwrap().raw_os_error(),
        Some(13)
    );
    assert!(failure.census().is_none());
    assert!(matches!(
        ActivityLease::try_acquire(Arc::clone(&late.root)),
        Err(ActivityAdmissionError::Busy)
    ));
    assert_eq!(owner.record().bytes, b"original bounded owner bytes");
    assert_eq!(
        fs::metadata(f.temp.path().join("backup-activity.lock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}
