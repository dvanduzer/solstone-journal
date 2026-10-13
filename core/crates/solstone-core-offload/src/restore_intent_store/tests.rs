// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#[cfg(unix)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod unix_tests {
    use super::super::*;
    use std::fs;
    use std::os::fd::AsFd;
    use std::os::unix::fs::{MetadataExt, symlink};
    use std::path::PathBuf;

    use solstone_core_journal_io::JournalRoot;
    use solstone_core_journal_io::atomic::{
        BoundPublicationPrimitive, run_with_bound_publication_barrier,
        run_with_bound_publication_fault,
    };

    use super::super::unix::{Hooks, NativePublication, Point};
    use crate::recovery::{RecoveryObligation, observe_obligations};
    use crate::test_restore_intent::fixture;
    use solstone_core_journal_io::activity_obligations::MAX_INTENT_BYTES;
    use solstone_core_retention::restore_intent::Obligation;

    struct Fixture {
        temp: tempfile::TempDir,
        activity: Arc<ActivityLease>,
        intent: RestoreIntent,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
            let activity = ActivityLease::try_acquire(root).unwrap();
            let mut intent = fixture(true);
            intent.destination.root = activity.journal_root().identity().observation_id();
            Self {
                temp,
                activity,
                intent,
            }
        }
        fn store(&self) -> RestoreIntentStore {
            RestoreIntentStore::create(Arc::clone(&self.activity))
                .unwrap_or_else(|failure| panic!("{:?}", failure.problem))
        }
        fn path(&self) -> PathBuf {
            self.temp
                .path()
                .join("health/offload/restore-intents")
                .join(format!("{}.json", self.intent.event.operation_id))
        }
        fn write(&self) -> StoreWrite {
            let result = self.store().create_intent(self.intent.clone());
            assert!(
                result.receipt.fully_confirmed(),
                "{:?}",
                result.receipt.failures
            );
            result
        }
    }

    #[test]
    fn durable_create_and_observed_update_preserve_whole_mark_and_remain_held_after_drop() {
        let f = Fixture::new();
        let result = f.write();
        assert_eq!(result.owner.preparation.durability, [Durability::Synced; 4]);
        assert!(result.owner.preparation.confirmed);
        let observed = result
            .owner
            .read(&f.intent.event.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(observed.intent, f.intent);
        assert_eq!(
            observed.intent.retirement.expected(),
            f.intent.retirement.expected()
        );
        let mut next = observed.intent.clone();
        next.cleanup = Obligation::Complete;
        let result = result.owner.update(&observed, next.clone());
        assert!(result.receipt.fully_confirmed());
        assert_eq!(fs::read(f.path()).unwrap(), next.encode().unwrap());
        drop(result);
        let census = observe_obligations(f.activity.journal_root()).unwrap();
        assert_eq!(census.entries().len(), 1); // persistent sibling lock is not an intent
        assert!(matches!(
            census.entries()[0],
            RecoveryObligation::Intent { .. }
        ));
        assert!(!f.temp.path().join("chronicle").exists());
        assert!(!f.temp.path().join("health/offload/20261009.jsonl").exists());
    }

    #[test]
    fn same_activity_clones_are_busy_and_prior_admission_token_never_updates() {
        let f = Fixture::new();
        let result = f.write();
        let observed = result
            .owner
            .read(&f.intent.event.operation_id)
            .unwrap()
            .unwrap();
        let failure = match RestoreIntentStore::create(Arc::clone(&f.activity)) {
            Err(failure) => failure,
            Ok(_) => panic!("overlapping store acquired"),
        };
        assert_eq!(failure.problem, StoreProblem::Busy);
        assert!(Arc::ptr_eq(&failure.owner.activity, &f.activity));
        drop(failure);
        drop(result);
        let original = fs::read(f.path()).unwrap();
        let inode = fs::metadata(f.path()).unwrap().ino();
        let result = f.store().update(&observed, f.intent.clone());
        assert!(result.receipt.native.is_none());
        assert_eq!(result.receipt.failures, vec![StoreProblem::Changed]);
        assert_eq!(fs::read(f.path()).unwrap(), original);
        assert_eq!(fs::metadata(f.path()).unwrap().ino(), inode);
        let fresh = result
            .owner
            .read(&f.intent.event.operation_id)
            .unwrap()
            .unwrap();
        let result = result.owner.update(&fresh, f.intent.clone());
        assert!(result.receipt.fully_confirmed());
    }

    #[test]
    fn create_collisions_never_overwrite_file_symlink_or_directory() {
        for kind in 0..3 {
            let f = Fixture::new();
            let store = f.store();
            let target = f.temp.path().join("untouched");
            fs::write(&target, b"existing").unwrap();
            match kind {
                0 => fs::write(f.path(), b"existing").unwrap(),
                1 => symlink(&target, f.path()).unwrap(),
                _ => fs::create_dir(f.path()).unwrap(),
            }
            let before = fs::symlink_metadata(f.path()).unwrap();
            let result = store.create_intent(f.intent.clone());
            assert!(matches!(
                result.receipt.native,
                Some(NativePublication::Create(Err(_)))
            ));
            assert!(!result.receipt.fully_confirmed());
            assert_eq!(fs::symlink_metadata(f.path()).unwrap().ino(), before.ino());
            assert_eq!(fs::read(target).unwrap(), b"existing");
            if kind == 0 {
                assert_eq!(fs::read(f.path()).unwrap(), b"existing");
            }
        }
    }

    #[test]
    fn missing_is_known_but_torn_unknown_oversize_and_name_mismatch_are_holds() {
        let f = Fixture::new();
        let store = f.store();
        assert!(store.read(&f.intent.event.operation_id).unwrap().is_none());
        assert!(store.read("../escape").is_err());
        let mut wrong_name = f.intent.clone();
        wrong_name.event.operation_id = "a".repeat(32);
        let mut unknown = serde_json::to_value(&f.intent).unwrap();
        unknown["extra"] = true.into();
        for bytes in [
            b"{".to_vec(),
            serde_json::to_vec(&unknown).unwrap(),
            wrong_name.encode().unwrap(),
            vec![b' '; MAX_INTENT_BYTES + 1],
        ] {
            fs::write(f.path(), bytes).unwrap();
            assert!(store.read(&f.intent.event.operation_id).is_err());
        }
    }

    struct ChangeBeforeWrite {
        path: PathBuf,
        action: u8,
    }
    impl Hooks for ChangeBeforeWrite {
        fn checkpoint(&mut self, point: Point) {
            if point != Point::BeforeWrite {
                return;
            }
            match self.action {
                0 => fs::write(&self.path, b"new accepted bytes").unwrap(),
                1 => {
                    fs::remove_file(&self.path).unwrap();
                    fs::write(&self.path, b"replacement").unwrap();
                }
                _ => fs::remove_file(&self.path).unwrap(),
            }
        }
    }

    #[test]
    fn accepted_record_late_append_replacement_or_loss_refuses_before_native_update() {
        for action in 0..3 {
            let f = Fixture::new();
            let result = f.write();
            let token = result
                .owner
                .read(&f.intent.event.operation_id)
                .unwrap()
                .unwrap();
            let result = result.owner.write_bound(
                f.intent.clone(),
                Some(&token),
                &mut ChangeBeforeWrite {
                    path: f.path(),
                    action,
                },
            );
            assert!(result.receipt.native.is_none());
            assert!(!result.receipt.failures.is_empty());
            if action == 0 {
                assert_eq!(fs::read(f.path()).unwrap(), b"new accepted bytes");
            }
            if action == 1 {
                assert_eq!(fs::read(f.path()).unwrap(), b"replacement");
            }
            if action == 2 {
                assert!(!f.path().exists());
            }
        }
    }

    #[test]
    fn valid_changed_binding_and_completed_obligation_reset_refuse_without_effect() {
        let f = Fixture::new();
        let result = f.write();
        let token = result
            .owner
            .read(&f.intent.event.operation_id)
            .unwrap()
            .unwrap();
        let original = fs::read(f.path()).unwrap();
        let mut next = f.intent.clone();
        next.stage.boot_id = Some("different-boot".into());
        next.validate().unwrap();
        let result = result.owner.update(&token, next);
        assert!(result.receipt.native.is_none());
        assert_eq!(fs::read(f.path()).unwrap(), original);
        let token = result
            .owner
            .read(&f.intent.event.operation_id)
            .unwrap()
            .unwrap();
        let mut next = token.intent.clone();
        next.cleanup = Obligation::Complete;
        let result = result.owner.update(&token, next);
        assert!(result.receipt.fully_confirmed());
        let token = result
            .owner
            .read(&f.intent.event.operation_id)
            .unwrap()
            .unwrap();
        let result = result.owner.update(&token, f.intent.clone());
        assert!(result.receipt.native.is_none());
        assert_eq!(
            RestoreIntent::decode(Ok(&fs::read(f.path()).unwrap()))
                .unwrap()
                .cleanup,
            Obligation::Complete
        );
    }

    #[test]
    fn persistent_lock_loss_or_replacement_refuses_under_original_owner() {
        for replace in [false, true] {
            let f = Fixture::new();
            let result = f.write();
            let token = result
                .owner
                .read(&f.intent.event.operation_id)
                .unwrap()
                .unwrap();
            let original = fs::read(f.path()).unwrap();
            let lock = f.temp.path().join("health/offload/restore-intents.lock");
            fs::rename(&lock, lock.with_extension("saved")).unwrap();
            if replace {
                fs::write(&lock, b"new sidecar").unwrap();
            }
            let result = result.owner.update(&token, f.intent.clone());
            assert!(result.receipt.native.is_none());
            assert!(!result.receipt.failures.is_empty());
            assert_eq!(fs::read(f.path()).unwrap(), original);
            if replace {
                assert_eq!(fs::read(lock).unwrap(), b"new sidecar");
            }
        }
    }

    #[test]
    fn native_prepublication_and_postpublication_failures_keep_independent_facts() {
        for primitive in [
            BoundPublicationPrimitive::Write,
            BoundPublicationPrimitive::FileSync,
            BoundPublicationPrimitive::ParentSync,
            BoundPublicationPrimitive::PostPublishObserve,
            BoundPublicationPrimitive::StageUnlink,
        ] {
            let f = Fixture::new();
            let store = f.store();
            let (result, fired) =
                run_with_bound_publication_fault(primitive, 1, nix::libc::EIO, || {
                    store.create_intent(f.intent.clone())
                });
            assert!(fired);
            assert!(!result.receipt.fully_confirmed());
            assert!(result.receipt.native.is_some());
            let published = matches!(
                primitive,
                BoundPublicationPrimitive::ParentSync
                    | BoundPublicationPrimitive::PostPublishObserve
                    | BoundPublicationPrimitive::StageUnlink
            );
            assert_eq!(f.path().exists(), published);
            if published {
                assert_eq!(fs::read(f.path()).unwrap(), f.intent.encode().unwrap());
            }
            assert_eq!(result.receipt.durability, [Some(Durability::Synced); 4]);
        }
    }

    #[test]
    fn failed_native_stage_cleanup_is_not_lost_on_prepublication_error() {
        let f = Fixture::new();
        let store = f.store();
        let parent = f.path().parent().unwrap().to_owned();
        let (result, fired) = run_with_bound_publication_barrier(
            BoundPublicationPrimitive::FileSync,
            1,
            move || {
                let stage = fs::read_dir(parent)
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                fs::rename(&stage, stage.with_extension("retained")).unwrap();
                fs::create_dir(stage).unwrap();
            },
            || store.create_intent(f.intent.clone()),
        );
        assert!(fired);
        let Some(NativePublication::Create(Err(error))) = result.receipt.native else {
            panic!("missing native error");
        };
        assert!(error.orphan_stage.is_some());
        assert!(error.cleanup_error.is_some());
        assert!(!f.path().exists());
        assert!(
            !observe_obligations(f.activity.journal_root())
                .unwrap()
                .entries()
                .is_empty()
        );
    }

    struct LateRoot {
        root: PathBuf,
        changed: bool,
    }
    impl Hooks for LateRoot {
        fn checkpoint(&mut self, point: Point) {
            if point == Point::AfterWrite {
                fs::rename(&self.root, self.root.with_extension("held")).unwrap();
                fs::create_dir(&self.root).unwrap();
                fs::write(self.root.join("replacement"), b"keep").unwrap();
                self.changed = true;
            }
        }
        fn sync(&mut self, directory: &impl AsFd, index: usize) -> std::io::Result<()> {
            if self.changed && index == 3 {
                return Err(std::io::Error::other("root sync fault"));
            }
            nix::unistd::fsync(directory).map_err(std::io::Error::from)
        }
    }

    #[test]
    fn publication_survives_late_root_loss_with_independent_sync_and_confirmation_failures() {
        let f = Fixture::new();
        let store = f.store();
        let result = store.write_bound(
            f.intent.clone(),
            None,
            &mut LateRoot {
                root: f.temp.path().to_owned(),
                changed: false,
            },
        );
        assert!(matches!(
            result.receipt.native,
            Some(NativePublication::Create(Ok(_)))
        ));
        assert!(result.receipt.confirmation.is_none());
        assert_eq!(result.receipt.durability[3], Some(Durability::Unproven));
        assert!(result.receipt.failures.len() >= 3);
        assert_eq!(
            fs::read(f.temp.path().join("replacement")).unwrap(),
            b"keep"
        );
        let saved = f.temp.path().with_extension("held");
        assert_eq!(
            fs::read(
                saved
                    .join("health/offload/restore-intents")
                    .join(f.path().file_name().unwrap())
            )
            .unwrap(),
            f.intent.encode().unwrap()
        );
        // Test owns both trees; production store never removes either.
        fs::remove_dir_all(saved).unwrap();
    }

    #[test]
    fn failed_preparation_retains_attempted_parent_creation_and_original_lease() {
        let f = Fixture::new();
        fs::create_dir(f.temp.path().join("health")).unwrap();
        fs::write(f.temp.path().join("health/offload"), b"blocker").unwrap();
        let failure = match RestoreIntentStore::create(Arc::clone(&f.activity)) {
            Ok(_) => panic!("unexpected preparation"),
            Err(failure) => failure,
        };
        assert!(Arc::ptr_eq(&failure.owner.activity, &f.activity));
        assert_eq!(
            failure.owner.preparation.attempted,
            ["health", "health/offload"]
        );
        assert_eq!(
            fs::read(f.temp.path().join("health/offload")).unwrap(),
            b"blocker"
        );
        assert!(!f.temp.path().join("chronicle").exists());
    }

    #[test]
    fn update_native_parent_failure_cannot_be_erased_by_later_successful_sync() {
        let f = Fixture::new();
        let result = f.write();
        let token = result
            .owner
            .read(&f.intent.event.operation_id)
            .unwrap()
            .unwrap();
        let mut next = f.intent.clone();
        next.cleanup = Obligation::Complete;
        let (result, fired) = run_with_bound_publication_fault(
            BoundPublicationPrimitive::ParentSync,
            1,
            nix::libc::EIO,
            || result.owner.update(&token, next),
        );
        assert!(fired);
        assert!(matches!(result.receipt.native, Some(NativePublication::Update(Ok(solstone_core_journal_io::atomic::BoundAtomicOutcome::PublishedDurabilityUncertain { .. })))));
        assert_eq!(result.receipt.durability, [Some(Durability::Synced); 4]);
        assert!(result.receipt.confirmation.is_some());
        assert!(!result.receipt.fully_confirmed());
    }

    #[test]
    fn invalid_creation_retains_owner_and_never_issues_native_publication() {
        let f = Fixture::new();
        let mut invalid = f.intent.clone();
        invalid.event.operation_id = "../escape".into();
        let result = f.store().create_intent(invalid);
        assert!(result.receipt.native.is_none());
        assert!(Arc::ptr_eq(&result.owner.activity, &f.activity));
        assert_eq!(fs::read_dir(f.path().parent().unwrap()).unwrap().count(), 0);
    }

    #[test]
    fn copied_pending_record_can_be_observed_but_cannot_be_updated_on_new_root() {
        let source = Fixture::new();
        let bytes = source.intent.encode().unwrap();
        let destination = Fixture::new();
        let store = destination.store();
        fs::write(destination.path(), &bytes).unwrap();
        let token = store
            .read(&source.intent.event.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(token.intent, source.intent);
        let result = store.update(&token, source.intent.clone());
        assert!(result.receipt.native.is_none());
        assert_eq!(fs::read(destination.path()).unwrap(), bytes);
        assert!(
            !observe_obligations(destination.activity.journal_root())
                .unwrap()
                .entries()
                .is_empty()
        );
    }

    #[test]
    fn accepted_intervening_valid_revision_is_preserved_under_actual_store_lock() {
        let f = Fixture::new();
        let result = f.write();
        let token = result
            .owner
            .read(&f.intent.event.operation_id)
            .unwrap()
            .unwrap();
        let mut accepted = f.intent.clone();
        accepted.cleanup = Obligation::Complete;
        let bytes = accepted.encode().unwrap();
        fs::write(f.path(), &bytes).unwrap();
        let result = result.owner.update(&token, f.intent.clone());
        assert!(result.receipt.native.is_none());
        assert_eq!(result.receipt.failures, vec![StoreProblem::Changed]);
        assert_eq!(fs::read(f.path()).unwrap(), bytes);
    }

    struct RootSyncFailure;
    impl Hooks for RootSyncFailure {
        fn sync(&mut self, directory: &impl AsFd, index: usize) -> std::io::Result<()> {
            if index == 3 {
                return Err(std::io::Error::other("actual root sync cut"));
            }
            nix::unistd::fsync(directory).map_err(std::io::Error::from)
        }
    }

    #[test]
    fn preparation_sync_failure_keeps_each_independent_fact_and_created_parent() {
        let f = Fixture::new();
        let mut owner = RestoreIntentStore::unprepared(Arc::clone(&f.activity));
        assert!(owner.prepare(&mut RootSyncFailure).is_err());
        assert_eq!(
            owner.preparation.durability,
            [
                Durability::Synced,
                Durability::Synced,
                Durability::Synced,
                Durability::Unproven
            ]
        );
        assert_eq!(owner.preparation.failures.len(), 1);
        assert!(owner.preparation.confirmed);
        assert_eq!(owner.chain.len(), 3);
        assert!(Arc::ptr_eq(&owner.activity, &f.activity));
        assert!(f.path().parent().unwrap().is_dir());
        assert!(!f.path().exists());
    }
}

#[cfg(windows)]
#[test]
fn windows_store_refuses_before_any_parent_or_store_lock_creation() {
    use super::*;
    use solstone_core_journal_io::JournalRoot;
    use solstone_core_journal_io::activity::ActivityLease;
    use std::sync::Arc;
    let temp = tempfile::tempdir().unwrap();
    let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
    let activity = ActivityLease::try_acquire(root).unwrap();
    let failure = match RestoreIntentStore::create(Arc::clone(&activity)) {
        Err(failure) => failure,
        Ok(_) => panic!("unsupported store admitted"),
    };
    assert_eq!(failure.problem, StoreProblem::Unsupported);
    assert!(failure.owner.preparation.attempted.is_empty());
    assert!(!temp.path().join("health").exists());
    assert!(Arc::ptr_eq(&activity, &failure.owner.activity));
}
