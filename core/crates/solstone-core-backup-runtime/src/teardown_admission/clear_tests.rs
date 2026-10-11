// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};
    use std::path::Path;

    use serde_json::{Value, json};
    use solstone_core_journal_io::activity::ActivityLease;
    use solstone_core_journal_io::activity_obligations::{
        observe_obligations, observe_with_carrier,
    };
    use solstone_core_journal_io::media::{
        Confirmation, Durability, MediaPrimitive, run_with_media_hook,
    };
    use solstone_core_journal_io::{
        BoundPublicationPrimitive, is_publication_candidate_name,
        run_with_bound_publication_faults, run_with_bound_publication_faults_and_barrier,
    };

    use super::super::*;

    fn fixture() -> (tempfile::TempDir, Arc<JournalRoot>) {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("config")).unwrap();
        fs::write(temp.path().join("config/journal.json"), serde_json::to_vec(&json!({
            "backup":{"enabled":true,"mode":"operated","daily_key":"daily-secret",
                "recovery_key":"A".repeat(solstone_core_backup::RECOVERY_KEY_LENGTH),
                "destination":{"backend":"s3","repository":"s3:bucket/repo",
                    "credentials":{"access_key_id":"access-secret","secret_access_key":"storage-secret"}}},
            "providers":{"credential":"provider-secret"}
        })).unwrap()).unwrap();
        fs::create_dir_all(temp.path().join("backup/hosted")).unwrap();
        fs::write(
            temp.path().join("backup/hosted/binding.json"),
            serde_json::to_vec(&json!({
                "broker_endpoint":"https://example.invalid","account_id":"account",
                "instance_id":"instance","bucket":"bucket","prefix":"prefix/",
                "broker_token":"broker-secret"
            }))
            .unwrap(),
        )
        .unwrap();
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        (temp, root)
    }

    fn accept(
        _: &Arc<JournalRoot>,
        _: &TeardownConfiguration,
        _: Option<RootCarrierObservation<'_>>,
    ) -> Result<(), TeardownReaderRefusal> {
        Ok(())
    }

    fn prepared(root: Arc<JournalRoot>) -> CarrierTeardownAdmission {
        admit_teardown(root)
            .unwrap()
            .prepare_carrier(&mut accept)
            .unwrap_or_else(|failure| panic!("{failure:?}"))
    }

    fn with_hook<T>(
        point: ClearCheckpoint,
        hook: impl FnOnce() + 'static,
        op: impl FnOnce() -> T,
    ) -> T {
        CLEAR_HOOK.with(|slot| assert!(slot.replace(Some((point, Box::new(hook)))).is_none()));
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                CLEAR_HOOK.with(|slot| {
                    slot.borrow_mut().take();
                });
            }
        }
        let _guard = Guard;
        let result = op();
        CLEAR_HOOK.with(|slot| assert!(slot.borrow().is_none(), "actual requested cut must run"));
        result
    }

    fn assert_busy(root: &Arc<JournalRoot>) {
        assert!(matches!(
            ActivityLease::try_acquire(root.clone()),
            Err(ActivityAdmissionError::Busy)
        ));
        assert!(matches!(
            TeardownLease::try_acquire(root.clone()),
            Err(ActivityAdmissionError::Busy)
        ));
    }

    fn drop_failure(failure: TeardownClearFailure, root: &Arc<JournalRoot>) {
        assert!(Arc::ptr_eq(failure.journal_root(), root));
        assert_busy(root);
        let diagnostic = format!("{failure:?}");
        for secret in [
            "daily-secret",
            "provider-secret",
            "broker-secret",
            "access-secret",
            "storage-secret",
        ] {
            assert!(!diagnostic.contains(secret));
        }
        drop(failure);
        let activity = ActivityLease::try_acquire(root.clone()).unwrap();
        drop(activity);
        drop(TeardownLease::try_acquire(root.clone()).unwrap());
        assert!(root.canonical_path().join("backup-activity.lock").is_file());
    }

    #[test]
    fn carrier_preparation_keeps_original_role_and_supplies_only_live_observations() {
        let (_temp, root) = fixture();
        let admission = admit_teardown(root.clone()).unwrap();
        let original_lease = admission.lease.clone();
        let original_sha = admission.configuration().sha256().to_owned();
        let mut calls = 0;
        let prepared = admission
            .prepare_carrier(&mut |supplied, view, carrier| {
                assert!(Arc::ptr_eq(supplied, &root));
                assert_eq!(view.sha256(), original_sha);
                assert_busy(supplied);
                if let Some(carrier) = carrier {
                    assert_eq!(calls, 1);
                    let evidence = observe_with_carrier(supplied, carrier).unwrap();
                    assert_eq!(evidence.entries().len(), 1);
                    assert_eq!(evidence.unresolved_entries().count(), 0);
                } else {
                    assert_eq!(calls, 0);
                    assert!(observe_obligations(supplied).unwrap().entries().is_empty());
                }
                calls += 1;
                Ok(())
            })
            .unwrap_or_else(|failure| panic!("{failure:?}"));
        assert_eq!(calls, 2);
        assert!(Arc::ptr_eq(prepared.carrier.teardown(), &original_lease));
        let reservation = prepared.carrier.diagnostic_path();
        drop(original_lease);
        assert_busy(&root);
        drop(prepared);
        assert!(reservation.is_dir());
        assert_eq!(observe_obligations(&root).unwrap().entries().len(), 1);
        drop(ActivityLease::try_acquire(root).unwrap());
    }

    #[test]
    fn preparation_reader_failures_retain_exact_reservation_and_exclusion() {
        for after_preparation in [false, true] {
            let (temp, root) = fixture();
            let before = fs::read(temp.path().join("config/journal.json")).unwrap();
            let failure = admit_teardown(root.clone())
                .unwrap()
                .prepare_carrier(&mut |_, _, carrier| {
                    if carrier.is_some() == after_preparation {
                        Err(TeardownReaderRefusal::RecoveryHeld)
                    } else {
                        Ok(())
                    }
                })
                .err()
                .unwrap();
            assert_eq!(
                failure.refusal(),
                TeardownCarrierRefusal::Admission(TeardownAdmissionRefusal::Reader(
                    TeardownReaderRefusal::RecoveryHeld
                ))
            );
            assert!(Arc::ptr_eq(failure.journal_root(), &root));
            assert_eq!(failure.preparation().is_some(), after_preparation);
            assert!(failure.preparation_error().is_none());
            let reservation = failure.reservation_path();
            assert_eq!(reservation.is_some(), after_preparation);
            assert_eq!(
                fs::read(temp.path().join("config/journal.json")).unwrap(),
                before
            );
            assert!(!temp.path().join("config/journal.json.lock").exists());
            assert_busy(&root);
            assert!(!format!("{failure:?}").contains("secret"));
            drop(failure);
            if let Some(reservation) = reservation {
                assert!(reservation.is_dir());
            }
            assert_eq!(
                observe_obligations(&root).unwrap().entries().len(),
                usize::from(after_preparation)
            );
            drop(ActivityLease::try_acquire(root).unwrap());
        }
    }

    #[test]
    fn consuming_partial_preparation_keeps_actual_native_failure_and_reservation() {
        let (temp, root) = fixture();
        let before = fs::read(temp.path().join("config/journal.json")).unwrap();
        let admission = admit_teardown(root.clone()).unwrap();
        let (result, consumed) = run_with_media_hook(
            MediaPrimitive::CarrierRootSync,
            || Err(std::io::Error::other("actual root sync failure")),
            || admission.prepare_carrier(&mut accept),
        );
        assert!(consumed);
        let failure = result.err().unwrap();
        assert_eq!(failure.refusal(), TeardownCarrierRefusal::Preparation);
        assert_eq!(
            failure.preparation_error().unwrap().kind(),
            std::io::ErrorKind::Other
        );
        let receipt = failure.preparation().unwrap();
        assert_eq!(receipt.stage_durability(), Durability::Synced);
        assert_eq!(receipt.root_durability(), Durability::Unproven);
        assert_eq!(receipt.confirmation(), Confirmation::Unverified);
        let Some(RetainedCarrier::Failed(native)) = failure.carrier.as_deref() else {
            panic!("actual lower failure must remain owned")
        };
        assert!(Arc::ptr_eq(native.teardown(), &failure.admission.lease));
        assert!(Arc::ptr_eq(native.journal_root(), &root));
        let reservation = failure.reservation_path().unwrap();
        assert!(reservation.is_dir());
        assert_busy(&root);
        assert_eq!(
            fs::read(temp.path().join("config/journal.json")).unwrap(),
            before
        );
        drop(failure);
        assert!(reservation.is_dir());
        assert_eq!(observe_obligations(&root).unwrap().entries().len(), 1);
        drop(TeardownLease::try_acquire(root).unwrap());
    }

    #[test]
    fn carrier_drift_after_publication_refuses_phase_and_retains_effect_facts() {
        for change in 0..3 {
            let (temp, root) = fixture();
            let admission = prepared(root.clone());
            let reservation = admission.carrier.diagnostic_path();
            let moved = temp.path().join(".tmp-offload-moved");
            let changed = reservation.clone();
            let retained = moved.clone();
            let failure = with_hook(
                ClearCheckpoint::AfterClear,
                move || match change {
                    0 => fs::write(changed.join("sole-raw"), b"keep").unwrap(),
                    1 => {
                        fs::rename(&changed, retained).unwrap();
                        fs::create_dir(changed).unwrap();
                    }
                    _ => fs::remove_dir(changed).unwrap(),
                },
                || admission.clear_configuration(&mut accept),
            )
            .err()
            .unwrap();
            assert!(failure.attempted());
            assert!(failure.publication().is_some());
            assert!(matches!(
                failure.refusal(),
                TeardownClearRefusal::Admission(TeardownAdmissionRefusal::CarrierUnavailable)
            ));
            assert!(Arc::ptr_eq(
                failure.admission.carrier.teardown(),
                &failure.admission.admission.lease
            ));
            assert_eq!(failure.admission.carrier.diagnostic_path(), reservation);
            let current: Value =
                serde_json::from_slice(&fs::read(temp.path().join("config/journal.json")).unwrap())
                    .unwrap();
            assert_eq!(
                current["backup"],
                Value::Object(solstone_core_backup::backup_defaults())
            );
            drop_failure(failure, &root);
            match change {
                0 => assert_eq!(fs::read(reservation.join("sole-raw")).unwrap(), b"keep"),
                1 => assert!(moved.is_dir() && reservation.is_dir()),
                _ => assert!(!reservation.exists()),
            }
        }
    }

    #[test]
    fn owned_clear_keeps_original_view_and_exclusion_for_each_fresh_callback() {
        let (temp, root) = fixture();
        let admission = prepared(root.clone());
        let original_sha = admission.configuration().sha256().to_owned();
        let backup = admission.configuration().backup().clone();
        let keys = admission.configuration().keys().cloned();
        let destination = admission.configuration().destination().cloned();
        let binding = admission.configuration().binding().cloned();
        let phase = admission
            .clear_configuration(&mut accept)
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert!(Arc::ptr_eq(phase.journal_root(), &root));
        assert_eq!(phase.configuration().sha256(), original_sha);
        assert_eq!(phase.configuration().backup(), &backup);
        assert_eq!(phase.configuration().keys(), keys.as_ref());
        assert_eq!(phase.configuration().destination(), destination.as_ref());
        assert_eq!(phase.configuration().binding(), binding.as_ref());
        let current: Value =
            serde_json::from_slice(&fs::read(temp.path().join("config/journal.json")).unwrap())
                .unwrap();
        assert_eq!(
            current["backup"],
            Value::Object(solstone_core_backup::backup_defaults())
        );
        assert_eq!(current["providers"]["credential"], "provider-secret");
        let view = phase.configuration();
        let mut calls = 0;
        let mut reader = |supplied: &Arc<JournalRoot>,
                          config: &TeardownConfiguration,
                          carrier: Option<RootCarrierObservation<'_>>| {
            assert!(carrier.is_some());
            calls += 1;
            assert!(Arc::ptr_eq(supplied, &root));
            assert!(std::ptr::eq(config, view));
            assert_busy(supplied);
            Ok(())
        };
        phase.check(&mut reader).unwrap();
        phase.check(&mut reader).unwrap();
        assert_eq!(calls, 2);
        for refusal in [
            TeardownReaderRefusal::CatalogUnavailable,
            TeardownReaderRefusal::RecoveryUnavailable,
            TeardownReaderRefusal::RecoveryHeld,
        ] {
            assert_eq!(
                phase.check(&mut |_, _, _| Err(refusal)),
                Err(TeardownAdmissionRefusal::Reader(refusal))
            );
        }
        assert!(
            phase.configuration().revalidate().is_err(),
            "old SHA must not be adopted after clear"
        );
        drop(phase);
        drop(ActivityLease::try_acquire(root).unwrap());
        assert!(temp.path().join("backup-activity.lock").exists());
    }

    #[test]
    fn reader_or_precheck_refusal_starts_no_clear_and_retains_failure_owner() {
        for drift in [false, true] {
            let (temp, root) = fixture();
            let admission = prepared(root.clone());
            let path = temp.path().join("config/journal.json");
            if drift {
                fs::write(&path, b"{}").unwrap();
            }
            let before = fs::read(&path).unwrap();
            let mut calls = 0;
            let failure = admission
                .clear_configuration(&mut |_, _, _| {
                    calls += 1;
                    Err(TeardownReaderRefusal::RecoveryHeld)
                })
                .err()
                .unwrap();
            assert!(!failure.attempted());
            assert!(failure.publication().is_none());
            assert_eq!(calls, usize::from(!drift));
            assert_eq!(fs::read(&path).unwrap(), before);
            assert!(!temp.path().join("config/journal.json.lock").exists());
            drop_failure(failure, &root);
        }
    }

    #[test]
    fn stale_missing_invalid_or_replaced_parent_at_clear_cut_preserves_competing_state() {
        for kind in 0..4 {
            let (temp, root) = fixture();
            let admission = prepared(root.clone());
            let path = temp.path().join("config/journal.json");
            let original = fs::read(&path).unwrap();
            let directory = temp.path().join("config");
            let before = original.clone();
            let target = path.clone();
            let failure = with_hook(
                ClearCheckpoint::BeforeClear,
                move || match kind {
                    0 => fs::write(&target, b"{}").unwrap(),
                    1 => fs::remove_file(&target).unwrap(),
                    2 => fs::write(&target, b"{").unwrap(),
                    _ => {
                        fs::rename(&directory, directory.with_file_name("old-config")).unwrap();
                        fs::create_dir(&directory).unwrap();
                        fs::write(&target, before).unwrap();
                    }
                },
                || admission.clear_configuration(&mut accept),
            )
            .err()
            .unwrap();
            assert!(failure.attempted());
            assert!(matches!(failure.refusal(), TeardownClearRefusal::Clear(_)));
            assert!(failure.publication().is_none());
            assert_eq!(
                fs::read(&path).ok(),
                match kind {
                    0 => Some(b"{}".to_vec()),
                    1 => None,
                    2 => Some(b"{".to_vec()),
                    _ => Some(original),
                }
            );
            assert!(!temp.path().join("config/journal.json.lock").exists());
            drop_failure(failure, &root);
        }
    }

    #[test]
    fn independent_and_combined_publication_uncertainties_refuse_and_keep_exclusion() {
        for (durability, observation, confirmation) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
            (true, true, false),
            (true, true, true),
        ] {
            let (temp, root) = fixture();
            let admission = prepared(root.clone());
            let mut faults = Vec::new();
            if durability {
                faults.push((BoundPublicationPrimitive::ParentSync, 1, 5));
            }
            let path = temp.path().to_path_buf();
            let (result, consumed, fired) = run_with_bound_publication_faults_and_barrier(
                faults.clone(),
                BoundPublicationPrimitive::Rename,
                1,
                move || {
                    let stage = fs::read_dir(path.join("config"))
                        .unwrap()
                        .map(|entry| entry.unwrap())
                        .find(|entry| is_publication_candidate_name(&entry.file_name()))
                        .unwrap()
                        .path();
                    if confirmation && observation {
                        fs::write(stage, b"{}").unwrap();
                    } else if confirmation {
                        change_bytes_preserving_flat_stamp(&stage);
                    } else if observation {
                        fs::File::open(stage)
                            .unwrap()
                            .set_times(
                                fs::FileTimes::new()
                                    .set_modified(std::time::SystemTime::UNIX_EPOCH),
                            )
                            .unwrap();
                    }
                },
                || admission.clear_configuration(&mut accept),
            );
            assert_eq!(consumed, faults.len());
            assert!(fired);
            let failure = result.err().unwrap();
            assert!(failure.attempted());
            assert!(matches!(
                failure.refusal(),
                TeardownClearRefusal::PublicationUncertain
            ));
            let publication = failure.publication().unwrap();
            assert_eq!(publication.durability_uncertainty.is_some(), durability);
            assert_eq!(publication.observation_uncertainty.is_some(), observation);
            assert_eq!(publication.confirmation_refusal.is_some(), confirmation);
            if confirmation && observation {
                assert_eq!(
                    fs::read(temp.path().join("config/journal.json")).unwrap(),
                    b"{}"
                );
            }
            drop_failure(failure, &root);
        }
    }

    #[test]
    fn lower_write_failure_preserves_native_orphan_and_cleanup_facts_with_owned_lease() {
        let (temp, root) = fixture();
        let admission = prepared(root.clone());
        let before = fs::read(temp.path().join("config/journal.json")).unwrap();
        let directory = temp.path().join("config");
        let (result, consumed, fired) = run_with_bound_publication_faults_and_barrier(
            [(BoundPublicationPrimitive::Rename, 1, 5)],
            BoundPublicationPrimitive::FileSync,
            1,
            move || {
                let stage = fs::read_dir(directory)
                    .unwrap()
                    .map(|e| e.unwrap())
                    .find(|e| is_publication_candidate_name(&e.file_name()))
                    .unwrap()
                    .path();
                fs::remove_file(&stage).unwrap();
                fs::create_dir(stage).unwrap();
            },
            || admission.clear_configuration(&mut accept),
        );
        assert_eq!(consumed, 1);
        assert!(fired);
        let failure = result.err().unwrap();
        assert!(failure.attempted());
        assert!(failure.publication().is_none());
        let TeardownClearRefusal::Clear(refusal) = failure.refusal() else {
            panic!("lower write refusal")
        };
        let BoundConfigRefusal::Write(error) = refusal.as_ref() else {
            panic!("native write refusal")
        };
        assert!(error.cleanup_error.is_some());
        let orphan = temp
            .path()
            .join("config")
            .join(error.orphan_stage.as_ref().unwrap());
        assert!(orphan.is_dir());
        assert_eq!(
            fs::read(temp.path().join("config/journal.json")).unwrap(),
            before
        );
        drop_failure(failure, &root);
        assert!(orphan.is_dir());
    }

    fn drift(path: &Path, kind: usize) {
        let config = path.join("config/journal.json");
        match kind {
            0 => change_bytes_preserving_flat_stamp(&config),
            1 => {
                let bytes = fs::read(&config).unwrap();
                fs::rename(&config, config.with_extension("old")).unwrap();
                fs::write(&config, bytes).unwrap();
            }
            2 => {
                fs::rename(path.join("config"), path.join("old-config")).unwrap();
                fs::create_dir(path.join("config")).unwrap();
                fs::copy(path.join("old-config/journal.json"), &config).unwrap();
            }
            3 => {
                fs::write(path.join("backup/hosted/binding.json"), b"{}").unwrap();
            }
            4 => {
                let binding = path.join("backup/hosted/binding.json");
                let bytes = fs::read(&binding).unwrap();
                fs::rename(&binding, binding.with_extension("old")).unwrap();
                fs::write(binding, bytes).unwrap();
            }
            5 => {
                fs::remove_file(&config).unwrap();
                fs::create_dir(&config).unwrap();
            }
            6 => {
                let target = path.join("other.json");
                fs::write(&target, b"{}").unwrap();
                fs::remove_file(&config).unwrap();
                symlink(target, &config).unwrap();
            }
            _ => unreachable!(),
        }
    }

    fn change_bytes_preserving_flat_stamp(config: &Path) {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(config)
            .unwrap();
        let modified = file.metadata().unwrap().modified().unwrap();
        let before = fs::metadata(config).unwrap();
        let mut bytes = fs::read(config).unwrap();
        let at = bytes
            .windows(b"provider-secret".len())
            .position(|w| w == b"provider-secret")
            .unwrap();
        bytes[at] = b'P';
        fs::write(config, bytes).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let after = fs::metadata(config).unwrap();
        assert_eq!(
            (
                before.ino(),
                before.len(),
                before.mtime(),
                before.mtime_nsec()
            ),
            (after.ino(), after.len(), after.mtime(), after.mtime_nsec())
        );
    }

    #[test]
    fn post_clear_and_later_drift_never_adopt_candidate_bytes_identity_parent_or_binding() {
        for kind in 0..7 {
            let (temp, root) = fixture();
            let admission = prepared(root.clone());
            let path = temp.path().to_path_buf();
            let failure = with_hook(
                ClearCheckpoint::AfterClear,
                move || drift(&path, kind),
                || admission.clear_configuration(&mut accept),
            )
            .err()
            .unwrap();
            assert!(failure.attempted());
            let publication = failure.publication().unwrap();
            assert!(publication.durability_uncertainty.is_none());
            assert!(publication.observation_uncertainty.is_none());
            assert!(publication.confirmation_refusal.is_none());
            assert!(matches!(
                failure.refusal(),
                TeardownClearRefusal::Admission(TeardownAdmissionRefusal::Configuration(_))
            ));
            drop_failure(failure, &root);

            let (temp, root) = fixture();
            let phase = prepared(root.clone())
                .clear_configuration(&mut accept)
                .unwrap_or_else(|e| panic!("{e:?}"));
            drift(temp.path(), kind);
            assert!(
                phase
                    .check(&mut |_, _, _| panic!("drift refuses before callback"))
                    .is_err()
            );
            assert_busy(&root);
        }
    }

    #[test]
    fn callback_drift_wins_over_accepting_or_refusing_reader() {
        for refusal in [None, Some(TeardownReaderRefusal::RecoveryHeld)] {
            let (temp, root) = fixture();
            let phase = prepared(root)
                .clear_configuration(&mut accept)
                .unwrap_or_else(|e| panic!("{e:?}"));
            assert_eq!(
                phase.check(&mut |_, _, _| {
                    fs::write(temp.path().join("config/journal.json"), b"{}").unwrap();
                    refusal.map_or(Ok(()), Err)
                }),
                Err(TeardownAdmissionRefusal::Configuration(
                    TeardownConfigurationRefusal::ConfigurationChanged
                ))
            );
        }
    }

    #[test]
    fn root_rebinding_refuses_phase_separately_from_old_root_exclusion() {
        for later in [false, true] {
            let parent = tempfile::tempdir().unwrap();
            let (temp, _) = fixture();
            let path = parent.path().join("journal");
            fs::rename(temp.path(), &path).unwrap();
            let root = Arc::new(JournalRoot::open(&path).unwrap());
            let admission = prepared(root);
            let swap = path.clone();
            let replace = move || {
                fs::rename(&swap, swap.with_file_name("old")).unwrap();
                fs::create_dir(&swap).unwrap();
            };
            if later {
                let phase = admission
                    .clear_configuration(&mut accept)
                    .unwrap_or_else(|e| panic!("{e:?}"));
                replace();
                assert!(
                    phase
                        .check(&mut |_, _, _| panic!("changed root refuses before callback"))
                        .is_err()
                );
                assert!(phase.journal_root().revalidate_canonical_binding().is_err());
            } else {
                let failure = with_hook(ClearCheckpoint::AfterClear, replace, || {
                    admission.clear_configuration(&mut accept)
                })
                .err()
                .unwrap();
                assert!(failure.publication().is_some());
                assert!(matches!(
                    failure.refusal(),
                    TeardownClearRefusal::Admission(_)
                ));
                assert!(
                    failure
                        .journal_root()
                        .revalidate_canonical_binding()
                        .is_err()
                );
            }
            let replacement = Arc::new(JournalRoot::open(&path).unwrap());
            drop(ActivityLease::try_acquire(replacement).unwrap());
        }
    }

    #[test]
    fn unchanged_classifier_only_returns_refusal_and_never_a_phase() {
        assert!(matches!(
            outcome_refusal(&BoundConfigOutcome::Unchanged),
            Some(TeardownClearRefusal::Unchanged)
        ));
        let (temp, root) = fixture();
        let admission = prepared(root.clone());
        let (result, consumed) =
            run_with_bound_publication_faults([(BoundPublicationPrimitive::Rename, 1, 5)], || {
                admission.clear_configuration(&mut accept)
            });
        assert_eq!(consumed, 1);
        let failure = result.err().unwrap();
        assert!(failure.publication().is_none());
        drop_failure(failure, &root);
        assert!(temp.path().join("config/journal.json").is_file());
    }
}
