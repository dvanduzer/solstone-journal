// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#[cfg(unix)]
mod unix {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use serde_json::json;
    use solstone_core_backup_runtime::{
        TeardownAdmission, TeardownAdmissionRefusal, admit_teardown,
    };
    use solstone_core_retention::archive_catalog::{OffloadFile, RestoreDisposition};

    use super::super::*;
    use crate::ledger::{append_offload_event, ledger_path_for_day};
    use crate::recovery::{IntentObservation, RecoveryObligation, observe_obligations};
    use solstone_core_journal_io::activity_obligations::RecoveryObservationFailure;
    use solstone_core_retention::restore_intent::{ConfirmedContent, Obligation};

    struct Fixture {
        temp: tempfile::TempDir,
        root: Arc<JournalRoot>,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_backup(json!({"mode":"byo"}))
        }

        fn with_backup(backup: serde_json::Value) -> Self {
            let temp = tempfile::tempdir().unwrap();
            fs::create_dir(temp.path().join("config")).unwrap();
            fs::write(
                temp.path().join("config/journal.json"),
                json!({"backup":backup,"private":"fixture-secret"}).to_string(),
            )
            .unwrap();
            let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
            Self { temp, root }
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.temp.path().join(relative)
        }

        fn admit(&self) -> TeardownAdmission {
            admit_teardown(self.root.clone()).unwrap()
        }

        fn snapshot(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
            fn walk(root: &Path, relative: &Path, state: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
                for entry in fs::read_dir(root.join(relative)).unwrap() {
                    let entry = entry.unwrap();
                    let path = relative.join(entry.file_name());
                    if entry.file_type().unwrap().is_dir() {
                        state.insert(path.clone(), None);
                        walk(root, &path, state);
                    } else {
                        state.insert(path, Some(fs::read(entry.path()).unwrap()));
                    }
                }
            }
            let mut state = BTreeMap::new();
            walk(self.temp.path(), Path::new(""), &mut state);
            state
        }

        fn archive(&self, bound: bool) -> PathBuf {
            let directory = if bound {
                "010000_001_full"
            } else {
                "010000_001"
            };
            let segment = self.path("chronicle/20260101").join(directory);
            fs::create_dir_all(&segment).unwrap();
            if bound {
                let event = json!({
                    "event_kind":"offload_archive_v2", "schema_version":2, "time":2,
                    "day":"20260101", "stream":"_default", "segment":"010000_001",
                    "segment_dir":directory, "layout":"direct",
                    "journal_id":"native-root-v1:unix:0000000000000001:0000000000000002",
                    "repository_id":"c".repeat(64), "snapshot_id":"b".repeat(64),
                    "manifest_id":"manifest", "files":[{
                        "witness_id":"witness", "name":"clip.webm", "bytes":3,
                        "sha256":"a".repeat(64), "archive_path":"/stage/clip.webm",
                        "processing":{"record":null,"has_analysis_row":true}
                    }]
                });
                let ledger = ledger_path_for_day(self.temp.path(), "20260101").unwrap();
                fs::create_dir_all(ledger.parent().unwrap()).unwrap();
                fs::write(ledger, format!("{event}\n")).unwrap();
            } else {
                append_offload_event(
                    self.temp.path(),
                    "20260101",
                    "_default",
                    directory,
                    "legacy-snapshot",
                    &[OffloadFile {
                        name: "clip.webm".into(),
                        bytes: 3,
                        sha256: "a".repeat(64),
                    }],
                    1,
                )
                .unwrap();
            }
            let catalog = read_complete_catalog(&self.root).unwrap();
            let witnesses = &catalog.days[0].segments[0].active;
            assert_eq!(witnesses.len(), 1);
            assert_eq!(witnesses[0].repository_id.is_some(), bound);
            assert_eq!(witnesses[0].file.bytes, 3);
            segment.join("clip.webm")
        }
    }

    fn check(admission: &TeardownAdmission) -> Result<(), TeardownAdmissionRefusal> {
        admission.check(&mut read_teardown_coverage)
    }

    #[test]
    fn live_own_carrier_preserves_fully_local_archive_before_and_after_clear() {
        let f = Fixture::new();
        let media = f.archive(true);
        fs::write(&media, b"abc").unwrap();
        let prepared = f
            .admit()
            .prepare_carrier(&mut read_teardown_coverage)
            .unwrap_or_else(|failure| panic!("{failure:?}"));
        assert_eq!(
            solstone_core_journal_io::activity_obligations::observe_obligations(&f.root)
                .unwrap()
                .entries()
                .len(),
            1
        );
        assert_eq!(prepared.check(&mut read_teardown_coverage), Ok(()));
        let cleared = prepared
            .clear_configuration(&mut read_teardown_coverage)
            .unwrap_or_else(|failure| panic!("{failure:?}"));
        assert_eq!(cleared.check(&mut read_teardown_coverage), Ok(()));
        assert_eq!(fs::read(media).unwrap(), b"abc");
        drop(cleared);
        assert_eq!(
            check(&f.admit()),
            Err(TeardownAdmissionRefusal::Reader(
                TeardownReaderRefusal::RecoveryHeld
            ))
        );
    }

    #[test]
    fn live_carrier_checks_retain_every_other_raw_hold_and_catalog_uncertainty() {
        let f = Fixture::new();
        fs::write(f.archive(true), b"abc").unwrap();
        let prepared = f
            .admit()
            .prepare_carrier(&mut read_teardown_coverage)
            .unwrap_or_else(|failure| panic!("{failure:?}"));
        for relative in [
            ".tmp-offload-unrelated",
            "chronicle/20260101/.tmp-offload-unrelated",
        ] {
            let other = f.path(relative);
            fs::create_dir(&other).unwrap();
            assert_eq!(
                prepared.check(&mut read_teardown_coverage),
                Err(TeardownAdmissionRefusal::Reader(
                    TeardownReaderRefusal::RecoveryHeld
                ))
            );
            fs::remove_dir(other).unwrap();
        }
        let cleared = prepared
            .clear_configuration(&mut read_teardown_coverage)
            .unwrap_or_else(|failure| panic!("{failure:?}"));
        let intent_directory = f.path("health/offload/restore-intents");
        fs::create_dir_all(&intent_directory).unwrap();
        let intent = intent_directory.join(
            solstone_core_journal_io::activity_obligations::intent_filename(&"a".repeat(32))
                .unwrap(),
        );
        fs::write(&intent, b"not a known intent").unwrap();
        assert_eq!(
            cleared.check(&mut read_teardown_coverage),
            Err(TeardownAdmissionRefusal::Reader(
                TeardownReaderRefusal::RecoveryHeld
            ))
        );
        fs::remove_file(intent).unwrap();
        let ledger = ledger_path_for_day(f.temp.path(), "20260101").unwrap();
        fs::write(ledger, b"{broken\n").unwrap();
        assert_eq!(
            cleared.check(&mut read_teardown_coverage),
            Err(TeardownAdmissionRefusal::Reader(
                TeardownReaderRefusal::CatalogUnavailable
            ))
        );
    }

    #[test]
    fn empty_known_coverage_is_fresh_and_read_only() {
        let f = Fixture::new();
        let admission = f.admit();
        let before = f.snapshot();
        assert_eq!(check(&admission), Ok(()));
        assert_eq!(check(&admission), Ok(()));
        assert_eq!(f.snapshot(), before);
    }

    fn backup_only() -> Result<(), TeardownAdmissionRefusal> {
        Err(TeardownAdmissionRefusal::Reader(
            TeardownReaderRefusal::BackupOnlyHeld,
        ))
    }

    #[test]
    fn absent_unbound_or_resized_local_media_holds_backup_only_copies() {
        for bound in [false, true] {
            let f = Fixture::new();
            let local = f.archive(bound);
            assert!(!local.exists());
            let admission = f.admit();
            let before = f.snapshot();
            assert_eq!(check(&admission), backup_only());
            assert_eq!(f.snapshot(), before);
            // Only a bound witness can be matched to a local file at all.
            fs::write(&local, b"abc").unwrap();
            let before = f.snapshot();
            let expected = if bound { Ok(()) } else { backup_only() };
            assert_eq!(check(&admission), expected);
            assert_eq!(f.snapshot(), before);
            fs::write(&local, b"different local bytes").unwrap();
            let before = f.snapshot();
            assert_eq!(check(&admission), backup_only());
            assert_eq!(f.snapshot(), before);
        }
    }

    #[test]
    fn unknown_local_presence_never_counts_as_covered() {
        let f = Fixture::new();
        let local = f.archive(true);
        fs::write(&local, b"abc").unwrap();
        let catalog = read_complete_catalog(&f.root).unwrap();
        let segment = &catalog.days[0].segments[0];
        let mut observed = CatalogMediaObservation {
            day: segment.day.clone(),
            stream: segment.stream.clone(),
            segment: segment.segment.clone(),
            witness: segment.active[0].clone(),
            presence: CatalogLocalPresence::Present { local_bytes: 3 },
        };
        let mut days = BTreeMap::new();
        assert!(matches!(
            coverage(&f.root, &mut days, &observed),
            Coverage::Covered
        ));
        // An incomplete local scan reports Unknown even when the same-size file is seen.
        observed.presence = CatalogLocalPresence::Unknown {
            known_local_bytes: Some(3),
        };
        assert!(matches!(
            coverage(&f.root, &mut days, &observed),
            Coverage::Restorable
        ));
    }

    #[test]
    fn owner_deleted_segment_releases_only_its_recorded_absence() {
        let f = Fixture::new();
        let local = f.archive(true);
        let segment = local.parent().unwrap().to_path_buf();
        let admission = f.admit();
        assert_eq!(check(&admission), backup_only());
        // An occupied marker is not a recorded owner deletion.
        std::os::unix::fs::symlink("elsewhere", segment.join("tombstone.json")).unwrap();
        assert_eq!(check(&admission), backup_only());
        fs::remove_file(segment.join("tombstone.json")).unwrap();
        fs::write(segment.join("tombstone.json"), b"{}").unwrap();
        let before = f.snapshot();
        assert_eq!(check(&admission), Ok(()));
        assert_eq!(f.snapshot(), before);

        // A legacy witness resolves by stream and segment key, as legacy restore does.
        let f = Fixture::new();
        let segment = f.archive(false).parent().unwrap().to_path_buf();
        let admission = f.admit();
        assert_eq!(check(&admission), backup_only());
        std::os::unix::fs::symlink("elsewhere", segment.join("tombstone.json")).unwrap();
        assert_eq!(check(&admission), backup_only());
        fs::remove_file(segment.join("tombstone.json")).unwrap();
        fs::write(segment.join("tombstone.json"), b"{}").unwrap();
        let before = f.snapshot();
        assert_eq!(check(&admission), Ok(()));
        assert_eq!(f.snapshot(), before);
    }

    fn unplaced() -> Result<(), TeardownAdmissionRefusal> {
        Err(TeardownAdmissionRefusal::Reader(
            TeardownReaderRefusal::BackupOnlyUnplaced,
        ))
    }

    #[test]
    fn backup_only_media_without_one_local_recording_is_named_unplaced() {
        // Legacy: the segment directory is gone, the day is gone, or two share its key.
        for case in ["segment", "day", "ambiguous"] {
            let f = Fixture::new();
            let segment = f.archive(false).parent().unwrap().to_path_buf();
            match case {
                "segment" => fs::remove_dir(&segment).unwrap(),
                "day" => fs::remove_dir_all(segment.parent().unwrap()).unwrap(),
                _ => fs::create_dir(segment.with_file_name("010000_001_full")).unwrap(),
            }
            let admission = f.admit();
            let before = f.snapshot();
            assert_eq!(check(&admission), unplaced(), "{case}");
            assert_eq!(f.snapshot(), before, "{case}");
        }
        // Bound: its exact segment directory is gone.
        let f = Fixture::new();
        fs::remove_dir(f.archive(true).parent().unwrap()).unwrap();
        assert_eq!(check(&f.admit()), unplaced());
        // Media that can still be restored is named first.
        let restorable = f.path("chronicle/20260101/010000_001");
        fs::create_dir(&restorable).unwrap();
        append_offload_event(
            f.temp.path(),
            "20260101",
            "_default",
            "010000_001",
            "legacy-snapshot",
            &[OffloadFile {
                name: "other.webm".into(),
                bytes: 3,
                sha256: "a".repeat(64),
            }],
            3,
        )
        .unwrap();
        assert_eq!(check(&f.admit()), backup_only());
    }

    #[test]
    fn missing_ledger_after_offload_was_recorded_refuses_as_unavailable() {
        let unavailable = Err(TeardownAdmissionRefusal::Reader(
            TeardownReaderRefusal::CatalogUnavailable,
        ));
        for backup in [
            json!({"mode":"byo","offload":{"enabled":true}}),
            json!({"mode":"byo","offload":{"enabled":false},
                "last_offload":{"time":"2026-01-01T00:00:00Z","files_marked":0}}),
            json!({"mode":"byo","last_offload":{"time":null,"files_marked":2}}),
        ] {
            let f = Fixture::with_backup(backup.clone());
            let admission = f.admit();
            let before = f.snapshot();
            assert_eq!(check(&admission), unavailable, "{backup}");
            assert_eq!(f.snapshot(), before, "{backup}");
            drop(admission);
            // A present, complete ledger with every file still local is enough.
            fs::write(f.archive(true), b"abc").unwrap();
            assert_eq!(check(&f.admit()), Ok(()), "{backup}");
        }
        // Offload that was never on and never ran needs no ledger.
        let f = Fixture::with_backup(json!({"mode":"byo","offload":{"enabled":false},
            "last_offload":{"time":null,"last_ok_time":null,"files_marked":0}}));
        assert_eq!(check(&f.admit()), Ok(()));
    }

    #[test]
    fn damaged_catalog_refuses_independently_and_does_not_reuse_earlier_success() {
        for damaged in [
            b"not json\n".as_slice(),
            br#"{"event_kind":"future","schema_version":999}"#.as_slice(),
        ] {
            let f = Fixture::new();
            fs::write(f.archive(true), b"abc").unwrap();
            let admission = f.admit();
            check(&admission).unwrap();
            let ledger = ledger_path_for_day(f.temp.path(), "20260101").unwrap();
            fs::write(&ledger, damaged).unwrap();
            assert!(read_complete_catalog(&f.root).is_err());
            assert!(observe_obligations(&f.root).unwrap().entries().is_empty());
            let before = f.snapshot();
            assert_eq!(
                check(&admission),
                Err(TeardownAdmissionRefusal::Reader(
                    TeardownReaderRefusal::CatalogUnavailable
                ))
            );
            assert_eq!(f.snapshot(), before);
        }
    }

    #[test]
    fn unknown_stages_and_malformed_intents_hold_with_clean_catalog() {
        for relative in [
            ".tmp-offload-unknown",
            "chronicle/20260101/.tmp-offload-unknown",
            "health/offload/restore-intents/unknown.json",
        ] {
            let f = Fixture::new();
            let carrier = f.path(relative);
            fs::create_dir_all(carrier.parent().unwrap()).unwrap();
            if relative.ends_with(".json") {
                fs::write(&carrier, b"torn intent").unwrap();
            } else {
                fs::create_dir(&carrier).unwrap();
                fs::write(carrier.join("sole.webm"), b"only raw").unwrap();
            }
            read_complete_catalog(&f.root).unwrap();
            assert!(!observe_obligations(&f.root).unwrap().entries().is_empty());
            let admission = f.admit();
            let before = f.snapshot();
            assert_eq!(
                check(&admission),
                Err(TeardownAdmissionRefusal::Reader(
                    TeardownReaderRefusal::RecoveryHeld
                ))
            );
            assert_eq!(f.snapshot(), before);
        }
    }

    #[test]
    fn recorded_pending_and_serialized_complete_copied_intents_all_hold() {
        for complete in [false, true] {
            let f = Fixture::new();
            let mut intent = crate::test_restore_intent::fixture(false);
            assert_ne!(intent.destination.root, f.root.identity().observation_id());
            if complete {
                intent.event.files[0].disposition = RestoreDisposition::AlreadyPresent;
                intent.confirmed = Some(ConfirmedContent {
                    identity: intent.stage.leaf,
                    digest: intent.expected_digest(),
                });
                intent.marker = Obligation::Complete;
                intent.event_recorded = Obligation::Complete;
                intent.cleanup = Obligation::Complete;
            }
            let bytes = intent.encode().unwrap();
            let directory = f.path("health/offload/restore-intents");
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join(format!("{}.json", intent.event.operation_id)),
                bytes,
            )
            .unwrap();
            read_complete_catalog(&f.root).unwrap();
            let observed = observe_obligations(&f.root).unwrap();
            assert!(matches!(observed.entries(), [RecoveryObligation::Intent {
                observation: IntentObservation::Recorded { operation_id }, ..
            }] if operation_id == &intent.event.operation_id));
            let admission = f.admit();
            let before = f.snapshot();
            assert_eq!(
                check(&admission),
                Err(TeardownAdmissionRefusal::Reader(
                    TeardownReaderRefusal::RecoveryHeld
                ))
            );
            assert_eq!(f.snapshot(), before);
        }
    }

    #[test]
    fn invalid_recovery_frontier_refuses_independently_of_clean_catalog() {
        let f = Fixture::new();
        fs::write(f.path("chronicle"), b"invalid frontier").unwrap();
        read_complete_catalog(&f.root).unwrap();
        assert_eq!(
            observe_obligations(&f.root).err().unwrap().failure,
            RecoveryObservationFailure::NotDirectory
        );
        let admission = f.admit();
        let before = f.snapshot();
        assert_eq!(
            check(&admission),
            Err(TeardownAdmissionRefusal::Reader(
                TeardownReaderRefusal::RecoveryUnavailable
            ))
        );
        assert_eq!(f.snapshot(), before);
    }

    #[test]
    fn later_noncooperating_stage_refuses_instead_of_using_empty_census() {
        let f = Fixture::new();
        let admission = f.admit();
        check(&admission).unwrap();
        let stage = f.path(".tmp-offload-introduced");
        fs::create_dir(&stage).unwrap();
        fs::write(stage.join("sole.webm"), b"only raw").unwrap();
        let before = f.snapshot();
        assert_eq!(
            check(&admission),
            Err(TeardownAdmissionRefusal::Reader(
                TeardownReaderRefusal::RecoveryHeld
            ))
        );
        assert_eq!(f.snapshot(), before);
    }
}
