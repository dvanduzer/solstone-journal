// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;

#[cfg(unix)]
mod unix {
    use std::fs;

    use solstone_core_journal_io::activity::ActivityLease;

    use super::*;

    const CONFIG: &[u8] = br#"{"backup":{"enabled":true,"mode":"byo"},"secret":"original-secret"}"#;
    const CHANGED: &[u8] = br#"{"backup":{"enabled":true,"mode":"byo"},"secret":"changed-secret"}"#;

    fn fixture() -> (tempfile::TempDir, Arc<JournalRoot>) {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("config")).unwrap();
        fs::write(temp.path().join("config/journal.json"), CONFIG).unwrap();
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        (temp, root)
    }

    #[test]
    fn admission_and_each_fresh_reader_keep_original_root_view_and_exclusion() {
        let (temp, root) = fixture();
        let admitted = admit_teardown(root.clone()).unwrap();
        let original_view = admitted.configuration();
        let mut calls = 0;
        let mut reader = |supplied: &Arc<JournalRoot>,
                          view: &TeardownConfiguration,
                          carrier: Option<RootCarrierObservation<'_>>| {
            assert!(carrier.is_none());
            calls += 1;
            assert!(Arc::ptr_eq(supplied, &root));
            assert!(std::ptr::eq(view, original_view));
            assert!(matches!(
                ActivityLease::try_acquire(root.clone()),
                Err(ActivityAdmissionError::Busy)
            ));
            assert!(matches!(
                TeardownLease::try_acquire(root.clone()),
                Err(ActivityAdmissionError::Busy)
            ));
            Ok(())
        };
        admitted.check(&mut reader).unwrap();
        admitted.check(&mut reader).unwrap();
        assert_eq!(calls, 2);
        drop(admitted);
        let activity = ActivityLease::try_acquire(root.clone()).unwrap();
        assert!(temp.path().join("backup-activity.lock").is_file());
        drop(activity);
        TeardownLease::try_acquire(root).unwrap();
        assert_eq!(
            fs::read(temp.path().join("config/journal.json")).unwrap(),
            CONFIG
        );
    }

    #[test]
    fn busy_admission_and_configuration_refusal_release_only_their_owned_lease() {
        let (temp, root) = fixture();
        let activity = ActivityLease::try_acquire(root.clone()).unwrap();
        assert!(matches!(
            admit_teardown(root.clone()),
            Err(TeardownAdmissionRefusal::ActivityBusy)
        ));
        drop(activity);
        fs::write(temp.path().join("config/journal.json"), b"{").unwrap();
        assert!(matches!(
            admit_teardown(root.clone()),
            Err(TeardownAdmissionRefusal::Configuration(_))
        ));
        let next = ActivityLease::try_acquire(root).unwrap();
        next.revalidate().unwrap();
        assert_eq!(
            fs::read(temp.path().join("config/journal.json")).unwrap(),
            b"{"
        );
    }

    #[test]
    fn unchanged_admission_preserves_each_reader_refusal() {
        let (_temp, root) = fixture();
        let admitted = admit_teardown(root).unwrap();
        for refusal in [
            TeardownReaderRefusal::CatalogUnavailable,
            TeardownReaderRefusal::RecoveryUnavailable,
            TeardownReaderRefusal::RecoveryHeld,
        ] {
            let mut calls = 0;
            assert_eq!(
                admitted.check(&mut |_, _, _| {
                    calls += 1;
                    Err(refusal)
                }),
                Err(TeardownAdmissionRefusal::Reader(refusal))
            );
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn precheck_drift_prevents_reader_and_preserves_changed_configuration() {
        let (temp, root) = fixture();
        let admitted = admit_teardown(root).unwrap();
        fs::write(temp.path().join("config/journal.json"), CHANGED).unwrap();
        let mut calls = 0;
        assert_eq!(
            admitted.check(&mut |_, _, _| {
                calls += 1;
                Ok(())
            }),
            Err(TeardownAdmissionRefusal::Configuration(
                TeardownConfigurationRefusal::ConfigurationChanged
            ))
        );
        assert_eq!(calls, 0);
        assert_eq!(
            fs::read(temp.path().join("config/journal.json")).unwrap(),
            CHANGED
        );
    }

    #[test]
    fn postcheck_drift_takes_precedence_for_accepting_and_refusing_readers() {
        for reader_result in [Ok(()), Err(TeardownReaderRefusal::RecoveryHeld)] {
            let (temp, root) = fixture();
            let admitted = admit_teardown(root).unwrap();
            let original_sha = admitted.configuration().sha256().to_owned();
            let mut calls = 0;
            let result = admitted.check(&mut |_, original, _| {
                calls += 1;
                fs::write(temp.path().join("config/journal.json"), CHANGED).unwrap();
                assert_eq!(original.sha256(), original_sha);
                reader_result
            });
            assert_eq!(
                result,
                Err(TeardownAdmissionRefusal::Configuration(
                    TeardownConfigurationRefusal::ConfigurationChanged
                ))
            );
            assert_eq!(calls, 1);
            assert!(!format!("{:?}", result.err().unwrap()).contains("secret"));
            assert_eq!(
                fs::read(temp.path().join("config/journal.json")).unwrap(),
                CHANGED
            );
        }
    }

    #[test]
    fn root_replacement_and_activity_entry_drift_refuse_before_reader() {
        let (temp, root) = fixture();
        let admitted = admit_teardown(root).unwrap();
        let entry = temp.path().join("backup-activity.lock");
        fs::rename(&entry, temp.path().join("original.lock")).unwrap();
        fs::write(&entry, b"replacement").unwrap();
        let mut calls = 0;
        assert_eq!(
            admitted.check(&mut |_, _, _| {
                calls += 1;
                Ok(())
            }),
            Err(TeardownAdmissionRefusal::ActivityUnavailable)
        );
        assert_eq!(calls, 0);
        assert_eq!(fs::read(&entry).unwrap(), b"replacement");

        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("journal");
        let replacement = parent.path().join("replacement");
        for root in [&path, &replacement] {
            fs::create_dir_all(root.join("config")).unwrap();
            fs::write(root.join("config/journal.json"), CONFIG).unwrap();
        }
        let root = Arc::new(JournalRoot::open(&path).unwrap());
        let admitted = admit_teardown(root).unwrap();
        fs::rename(&path, parent.path().join("original")).unwrap();
        fs::rename(&replacement, &path).unwrap();
        assert_eq!(
            admitted.check(&mut |_, _, _| {
                calls += 1;
                Ok(())
            }),
            Err(TeardownAdmissionRefusal::ActivityUnavailable)
        );
        assert_eq!(calls, 0);
        assert_eq!(fs::read(path.join("config/journal.json")).unwrap(), CONFIG);
        assert!(!path.join("backup-activity.lock").exists());
    }
}

#[cfg(not(unix))]
#[test]
fn unsupported_refuses_before_lock_or_state_read() {
    let temp = tempfile::tempdir().unwrap();
    let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
    assert!(matches!(
        admit_teardown(root),
        Err(TeardownAdmissionRefusal::Unsupported)
    ));
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}
