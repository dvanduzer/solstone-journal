// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use solstone_core_backup::teardown_recovery::retained::{
    Mode, NativeCustodyObservation, NativeDestination, NativeRecordObservation,
    NativeStorageObservation, Record, destructive_scope,
};
use solstone_core_backup::teardown_recovery::{RemoteEffect, RemoteFacts, StorageRetention};
use solstone_core_journal_config_write::BoundConfigOutcome;
use solstone_core_journal_io::JournalRoot;
use solstone_core_journal_io::activity::{ActivityLease, TeardownLease};
use std::{fs, sync::Arc};

struct Fixture {
    temp: tempfile::TempDir,
    root: Arc<JournalRoot>,
    store: Store,
    preparation: TeardownPreparation,
    carrier: RootTeardownCarrier,
    configuration: TeardownConfiguration,
    publication: BoundConfigPublication,
}
impl Fixture {
    fn check(&self) -> Result<(), ()> {
        confirm_local_custody(
            &self.store,
            &self.preparation,
            &self.carrier,
            &self.configuration,
            &self.publication,
        )
    }
}

// Real local publication, lease, preparation, canonical and carrier handles.
// Native observations here are synthetic schema inputs only: these tests cannot
// construct LockedDestination or prove the separate iOS native handoff.
fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("config")).unwrap();
    let config = serde_json::json!({"backup":{"mode":"byo","enabled":true,
        "daily_key":"synthetic-daily","recovery_key":"A".repeat(solstone_core_backup::RECOVERY_KEY_LENGTH),
        "destination":{"backend":"s3","repository":"s3:host/owner-bucket/owner-prefix",
        "credentials":{"access_key_id":"synthetic-access","secret_access_key":"synthetic-storage","region":"us-east-1"}}},
        "providers":{"token":"synthetic-provider"}});
    fs::write(
        temp.path().join("config/journal.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
    let lease = TeardownLease::try_acquire(root.clone()).unwrap();
    let configuration = TeardownConfiguration::capture(&root).unwrap();
    let mut preparation = TeardownPreparation::create(lease.clone()).unwrap();
    let initial = Record::preflight(
        Mode::Byo,
        configuration.local_observation().unwrap(),
        false,
        preparation.observation().clone(),
    )
    .unwrap();
    preparation
        .write_marker(&serde_json::to_vec(&initial).unwrap())
        .unwrap();
    preparation.publish().unwrap();
    let mut store = Store::create(lease.clone(), initial).unwrap();
    store.complete_preparation(&preparation).unwrap();
    let mut next = store.record().clone();
    let destination = NativeDestination {
        endpoint: "https://host:443".into(),
        region: "us-east-1".into(),
        bucket: "owner-bucket".into(),
        prefix: "owner-prefix".into(),
    };
    next.origin = "a".repeat(32);
    next.repository_id = Some("b".repeat(64));
    next.snapshot_ids = vec!["c".repeat(64)];
    let scope = destructive_scope(
        &next.local.original_sha256,
        &destination,
        next.repository_id.as_deref().unwrap(),
        &next.origin,
        &next.snapshot_ids,
    )
    .unwrap();
    next.native = Some(NativeCustodyObservation {
        destination,
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
            id: "d".repeat(32),
            operation: 11,
            active: false,
            scope,
            origin: next.origin.clone(),
            reconciled: true,
        },
    });
    next.remote_settled = true;
    next.remote_facts = Some(RemoteFacts {
        version: 1,
        effect: RemoteEffect::SnapshotsPruned {
            admitted_snapshot_count: 1,
            remaining_snapshots: 0,
        },
        storage_retention: Some(StorageRetention::Unknown),
    });
    store.update(next).unwrap();
    let BoundConfigOutcome::Published(publication) = configuration.clear_configuration().unwrap()
    else {
        panic!("real publication")
    };
    let mut next = store.record().clone();
    next.clear = Some(
        configuration
            .published_clear_observation(&publication)
            .unwrap(),
    );
    store.update(next).unwrap();
    let carrier = RootTeardownCarrier::reacquire(lease, &store.record().carrier).unwrap();
    Fixture {
        temp,
        root,
        store,
        preparation,
        carrier,
        configuration,
        publication,
    }
}

#[test]
fn durable_clear_local_custody_preserves_files_and_reopens_original_owners() {
    let f = fixture();
    assert!(f.check().is_ok());
    let config = fs::read(f.temp.path().join("config/journal.json")).unwrap();
    let canonical = fs::read(f.temp.path().join("backup/teardown-recovery.json")).unwrap();
    let saved = f.store.record().clone();
    assert!(ActivityLease::try_acquire(f.root.clone()).is_err());
    let Fixture {
        temp,
        root,
        store,
        preparation,
        carrier,
        configuration,
        publication,
    } = f;
    drop((store, preparation, carrier, configuration, publication));
    let lease = TeardownLease::try_acquire(root).unwrap();
    let recovered = Store::load(lease.clone()).unwrap().unwrap();
    assert_eq!(*recovered.record(), saved);
    recovered.confirm_carrier_ready().unwrap();
    let preparation = TeardownPreparation::reopen(lease.clone(), &saved.preparation).unwrap();
    let carrier = RootTeardownCarrier::reacquire(lease, &saved.carrier).unwrap();
    assert_eq!(*preparation.observation(), saved.preparation);
    assert_eq!(carrier.recovery_observation().unwrap(), saved.carrier);
    assert_eq!(
        fs::read(temp.path().join("config/journal.json")).unwrap(),
        config
    );
    assert_eq!(
        fs::read(temp.path().join("backup/teardown-recovery.json")).unwrap(),
        canonical
    );
}

#[test]
fn changed_canonical_config_carrier_preparation_and_publication_refuse_without_release() {
    for change in [
        "canonical",
        "config",
        "carrier",
        "marker",
        "preparation-parent",
        "publication-hash",
        "publication-flush",
    ] {
        let mut f = fixture();
        assert!(f.check().is_ok());
        match change {
            "canonical" => {
                fs::write(f.temp.path().join("backup/teardown-recovery.json"), b"{}").unwrap()
            }
            "config" => {
                let p = f.temp.path().join("config/journal.json");
                let mut b = fs::read(&p).unwrap();
                b.push(b' ');
                fs::write(p, b).unwrap();
            }
            "carrier" => fs::write(
                f.temp
                    .path()
                    .join(&f.store.record().carrier.directory_name)
                    .join("unrelated"),
                b"preserve",
            )
            .unwrap(),
            "marker" => fs::write(
                f.temp
                    .path()
                    .join(&f.store.record().carrier.directory_name)
                    .join(solstone_core_journal_io::media::teardown_preparation::MARKER),
                b"preserve",
            )
            .unwrap(),
            "preparation-parent" => {
                let p = f.temp.path().join("backup/teardown-preparations");
                fs::rename(&p, f.temp.path().join("backup/preserved-preparations")).unwrap();
                fs::create_dir(p).unwrap();
            }
            "publication-hash" => {
                f.publication.candidate_sha256 = "sha256:".to_owned() + &"e".repeat(64)
            }
            "publication-flush" => f.publication.directory_flush = DirectoryFlush::Failed,
            _ => unreachable!(),
        }
        assert!(f.check().is_err(), "{change}");
        assert!(
            ActivityLease::try_acquire(f.root.clone()).is_err(),
            "{change}"
        );
        assert!(
            f.temp
                .path()
                .join(&f.store.record().carrier.directory_name)
                .is_dir()
        );
    }
}
