// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use crate::teardown_configuration::TeardownConfiguration;
use solstone_core_journal_io::JournalRoot;
use std::fs;

fn fixture() -> (tempfile::TempDir, Arc<TeardownLease>, Record) {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("config")).unwrap();
    let config = serde_json::json!({"backup":{"mode":"byo","enabled":true,
        "daily_key":"synthetic-daily-secret","recovery_key":"A".repeat(crate::RECOVERY_KEY_LENGTH),
        "destination":{"backend":"s3","repository":"s3:host/owner-bucket/owner-prefix",
        "credentials":{"access_key_id":"synthetic-access-secret","secret_access_key":"synthetic-storage-secret","region":"us-east-1"}}},
        "providers":{"token":"synthetic-provider-secret"}});
    fs::write(
        temp.path().join("config/journal.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
    let lease = TeardownLease::try_acquire(root.clone()).unwrap();
    let configuration = TeardownConfiguration::capture(&root).unwrap();
    let mut preparation = TeardownPreparation::create(lease.clone()).unwrap();
    let observed = preparation.observation().clone();
    preparation
        .write_marker(b"synthetic-schema-fixture")
        .unwrap();
    preparation.publish().unwrap();
    fs::remove_file(
        temp.path()
            .join(&observed.destination.directory_name)
            .join(MARKER),
    )
    .unwrap();
    preparation.published_directory().unwrap().sync().unwrap();
    let record = Record {
        version: 2,
        mode: Mode::Byo,
        local: configuration.local_observation().unwrap(),
        carrier: observed.destination.clone(),
        preparation: observed,
        preparation_ready: true,
        offload_was_recorded: true,
        // Schema/storage controls only. These synthetic native observations
        // cannot construct native admission; actual C/phone proofs are separate.
        native: Some(NativeCustodyObservation {
            destination: NativeDestination {
                endpoint: "https://host:443".into(),
                region: "us-east-1".into(),
                bucket: "owner-bucket".into(),
                prefix: "owner-prefix".into(),
            },
            storage: NativeStorageObservation {
                installation: "synthetic-install".into(),
                family: "synthetic-family".into(),
                directory: "synthetic-directory".into(),
            },
            record: NativeRecordObservation {
                version: 1,
                present: true,
                device: 1,
                inode: 2,
                id: "a".repeat(32),
                operation: 5,
                active: false,
                scope: String::new(),
                origin: String::new(),
                reconciled: false,
            },
        }),
        origin: "a".repeat(32),
        repository_id: Some("b".repeat(64)),
        snapshot_ids: vec!["c".repeat(64)],
        remote_settled: false,
        remote_facts: None,
        clear: None,
        binding_removed: false,
        native_retired: false,
        carrier_retired: false,
    };
    (temp, lease, record)
}

#[test]
fn record_contains_only_bounded_nonsecret_observations_and_refuses_legacy_secrets() {
    let (_temp, _lease, record) = fixture();
    let bytes = serde_json::to_vec(&record).unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    for secret in [
        "synthetic-daily-secret",
        "synthetic-access-secret",
        "synthetic-storage-secret",
        "synthetic-provider-secret",
        &"A".repeat(crate::RECOVERY_KEY_LENGTH),
    ] {
        assert!(!text.contains(secret));
    }
    assert_eq!(serde_json::from_slice::<Record>(&bytes).unwrap(), record);
    for field in [
        "daily_key",
        "recovery_key",
        "broker_token",
        "s3_secret",
        "encrypted_credentials",
        "passwords",
    ] {
        let mut value = serde_json::to_value(&record).unwrap();
        value[field] = serde_json::json!("reversible-secret-material");
        assert!(serde_json::from_value::<Record>(value).is_err(), "{field}");
    }
    let mut legacy = serde_json::to_value(&record).unwrap();
    legacy["version"] = serde_json::json!(1);
    assert!(
        serde_json::from_value::<Record>(legacy)
            .unwrap()
            .validate()
            .is_err()
    );
}

#[test]
fn actual_store_refuses_overwrite_replacement_cohort_change_and_absence_cleanup() {
    let (temp, lease, record) = fixture();
    assert!(Store::load(lease.clone()).unwrap().is_none());
    let mut store = Store::create(lease.clone(), record.clone()).unwrap();
    assert!(Store::create(lease.clone(), record.clone()).is_err());
    let mut next = record.clone();
    next.snapshot_ids.push("d".repeat(64));
    assert!(store.update(next).is_err());
    let mut next = record.clone();
    next.origin = "e".repeat(32);
    assert!(store.update(next).is_err());
    let mut next = record.clone();
    next.remote_settled = true;
    next.remote_facts = Some(super::super::RemoteFacts {
        version: 1,
        effect: super::super::RemoteEffect::SnapshotsPruned {
            admitted_snapshot_count: 1,
            remaining_snapshots: 0,
        },
        storage_retention: Some(super::super::StorageRetention::Keeps(
            super::super::StorageKeeps::Versions,
        )),
    });
    assert!(store.update(next.clone()).is_err());
    next.native.as_mut().unwrap().record.operation = 11;
    next.native.as_mut().unwrap().record.origin = next.origin.clone();
    next.native.as_mut().unwrap().record.scope = destructive_scope(
        &next.local.original_sha256,
        &next.native.as_ref().unwrap().destination,
        next.repository_id.as_deref().unwrap(),
        &next.origin,
        &next.snapshot_ids,
    )
    .unwrap();
    // A failed no-write opcode-11 completion is not a successful prune.
    assert!(store.update(next.clone()).is_err());
    next.native.as_mut().unwrap().record.reconciled = true;
    store.update(next.clone()).unwrap();
    drop(store);
    let store = Store::load(lease.clone()).unwrap().unwrap();
    assert_eq!(store.record(), &next);
    let path = temp.path().join("backup").join(TEARDOWN_RECOVERY_FILENAME);
    let bytes = fs::read(&path).unwrap();
    fs::rename(&path, temp.path().join("original-record")).unwrap();
    fs::write(&path, &bytes).unwrap();
    assert!(store.revalidate().is_err());
    assert!(store.remove().is_err());
    assert_eq!(fs::read(&path).unwrap(), bytes);
    fs::remove_file(&path).unwrap();
    assert!(Store::load(lease).unwrap().is_none());
    assert!(temp.path().join("original-record").is_file());
}

#[test]
fn preparation_transfer_process_helper() {
    let Some(journal) = std::env::var_os("SOLSTONE_TRANSFER_TEST_JOURNAL") else {
        return;
    };
    let cut = std::env::var("SOLSTONE_TRANSFER_TEST_CUT").unwrap();
    let root = Arc::new(JournalRoot::open(Path::new(&journal)).unwrap());
    let lease = TeardownLease::try_acquire(root.clone()).unwrap();
    let config = TeardownConfiguration::capture(&root).unwrap();
    let mut preparation = TeardownPreparation::create(lease.clone()).unwrap();
    let record = Record::preflight(
        Mode::Byo,
        config.local_observation().unwrap(),
        true,
        preparation.observation().clone(),
    )
    .unwrap();
    preparation
        .write_marker(&serde_json::to_vec(&record).unwrap())
        .unwrap();
    preparation.publish().unwrap();
    if cut == "marker_only" {
        std::process::exit(77);
    }
    let mut store = Store::create(lease, record).unwrap();
    if cut == "canonical" {
        std::process::exit(77);
    }
    if cut == "unlinked" {
        let _ = solstone_core_journal_io::run_with_claim_removal_barrier(
            solstone_core_journal_io::ClaimRemovalPrimitive::DirectorySync,
            1,
            || std::process::exit(77),
            || store.complete_preparation(&preparation),
        );
        panic!("post-unlink process cut was not reached");
    }
    store.complete_preparation(&preparation).unwrap();
    assert_eq!(cut, "ready");
    std::process::exit(77);
}

#[test]
fn new_process_transfers_actual_marker_and_holds_unconfirmed_unlink() {
    for cut in ["marker_only", "canonical", "unlinked", "ready"] {
        let (temp, lease, original) = fixture();
        drop(lease);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "teardown_recovery::retained::tests::preparation_transfer_process_helper",
                "--nocapture",
            ])
            .env("SOLSTONE_TRANSFER_TEST_JOURNAL", temp.path())
            .env("SOLSTONE_TRANSFER_TEST_CUT", cut)
            .output()
            .unwrap();
        assert_eq!(
            child.status.code(),
            Some(77),
            "{cut}: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        let lease = TeardownLease::try_acquire(root.clone()).unwrap();
        let mut store = Store::load(lease.clone()).unwrap();
        let found =
            Store::discover_preparation(lease.clone(), store.as_ref().map(Store::record)).unwrap();
        if cut == "unlinked" {
            let store = store.unwrap();
            assert!(found.is_none());
            assert!(!store.record().preparation_ready);
            assert!(store.confirm_carrier_ready().is_err());
            assert!(
                !temp
                    .path()
                    .join(&store.record().carrier.directory_name)
                    .join(MARKER)
                    .exists()
            );
            assert!(
                temp.path()
                    .join("backup")
                    .join(TEARDOWN_RECOVERY_FILENAME)
                    .is_file()
            );
        } else {
            if cut != "ready" {
                let mut found = found.unwrap();
                found.preparation.publish().unwrap();
                let mut canonical = store
                    .take()
                    .unwrap_or_else(|| Store::create(lease.clone(), found.record).unwrap());
                canonical.complete_preparation(&found.preparation).unwrap();
                store = Some(canonical);
            } else {
                assert!(found.is_none());
            }
            let store = store.unwrap();
            assert!(store.record().native.is_none());
            assert!(!store.record().remote_settled);
            store.confirm_carrier_ready().unwrap();
            solstone_core_journal_io::media::RootTeardownCarrier::reacquire(
                lease.clone(),
                &store.record().carrier,
            )
            .unwrap();
        }
        assert_eq!(
            TeardownConfiguration::capture(&root)
                .unwrap()
                .local_observation()
                .unwrap(),
            original.local
        );
    }
}

#[test]
fn discovery_refuses_conflicts_multiple_markers_and_applied_unlink_sync_failure() {
    for change in ["conflict", "multiple", "unlink_sync"] {
        let (temp, lease, original) = fixture();
        let mut preparation = TeardownPreparation::create(lease.clone()).unwrap();
        let record = Record::preflight(
            Mode::Byo,
            original.local.clone(),
            true,
            preparation.observation().clone(),
        )
        .unwrap();
        preparation
            .write_marker(&serde_json::to_vec(&record).unwrap())
            .unwrap();
        preparation.publish().unwrap();
        let mut store = Store::create(lease.clone(), record.clone()).unwrap();
        if change == "conflict" {
            let mut canonical = record.clone();
            canonical.offload_was_recorded = false;
            assert!(Store::discover_preparation(lease.clone(), Some(&canonical)).is_err());
        } else if change == "multiple" {
            let mut second = TeardownPreparation::create(lease.clone()).unwrap();
            let second_record = Record::preflight(
                Mode::Byo,
                original.local,
                true,
                second.observation().clone(),
            )
            .unwrap();
            second
                .write_marker(&serde_json::to_vec(&second_record).unwrap())
                .unwrap();
            second.publish().unwrap();
            assert!(Store::discover_preparation(lease.clone(), Some(&record)).is_err());
            assert!(second.marker().unwrap().is_some());
        } else {
            let (result, consumed) = solstone_core_journal_io::run_with_claim_removal_fault(
                solstone_core_journal_io::ClaimRemovalPrimitive::DirectorySync,
                1,
                5,
                || store.complete_preparation(&preparation),
            );
            assert!(result.is_err() && consumed);
            assert!(matches!(
                store.preparation_removal(),
                Some(Ok(ClaimRemovalOutcome::RemovedDurabilityUncertain))
            ));
            assert!(preparation.marker().unwrap().is_none());
            assert!(!store.record().preparation_ready);
            assert!(
                Store::load(lease)
                    .unwrap()
                    .unwrap()
                    .confirm_carrier_ready()
                    .is_err()
            );
        }
        assert!(
            temp.path()
                .join("backup")
                .join(TEARDOWN_RECOVERY_FILENAME)
                .is_file()
        );
        if change != "unlink_sync" {
            assert!(preparation.marker().unwrap().is_some());
        }
    }
}
