// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use solstone_core_retention::archive_proof::{AcceptedProcessingObservation, ArchiveFileProof};

use super::*;

struct Fixture {
    temp: tempfile::TempDir,
    root: Arc<JournalRoot>,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        Self { temp, root }
    }

    fn proof(&self, id: &str, layout: SegmentLayout) -> OffloadProof {
        OffloadProof {
            journal_id: self.root.identity().observation_id(),
            manifest_id: format!("manifest-{id}"),
            target: Target {
                day: "20260101".into(),
                stream: if layout == SegmentLayout::Direct {
                    "_default".into()
                } else {
                    "camera".into()
                },
                dir: "prefix-010000_001_suffix".into(),
            },
            layout,
            repository_id: "a".repeat(64),
            snapshot_id: "b".repeat(64),
            files: ["audio.flac", "z.flac"]
                .into_iter()
                .map(|name| {
                    ArchiveFileProof {
                    witness_id: format!("witness-{id}-{name}"),
                    name: name.into(),
                    bytes: 7,
                    sha256: "c".repeat(64),
                    archive_path: format!("/stage/{name}"),
                    processing: AcceptedProcessingObservation {
                    record: (name == "z.flac").then(|| serde_json::json!({
                        "schema":"solstone.processing.v1", "handler":"transcribe", "state":"empty",
                        "input_size":7, "reason_code":"no_decodable_audio",
                        "attempted_at":"2026-01-01T00:00:00Z", "source":"backfill",
                        "extra":{"preserved":[null,true,"detail"]}
                    })),
                    has_analysis_row: name != "z.flac",
                    },
                }
                })
                .collect(),
        }
    }

    fn path(&self, day: &str) -> std::path::PathBuf {
        self.temp
            .path()
            .join("health/offload")
            .join(format!("{day}.jsonl"))
    }
}

#[cfg(windows)]
#[test]
fn windows_refuses_before_creating_any_namespace() {
    let fixture = Fixture::new();
    let proof = fixture.proof("one", SegmentLayout::Direct);
    assert_eq!(
        append_archive_witness(&fixture.root, &proof, 1).unwrap_err(),
        LedgerWriteRefusal::Unsupported
    );
    assert_eq!(
        append_restore_operation(
            &fixture.root,
            &proof.target,
            proof.layout,
            &RestoreOperation {
                operation_id: "op".into(),
                files: vec![]
            },
            1
        )
        .unwrap_err(),
        LedgerWriteRefusal::Unsupported
    );
    assert!(!fixture.temp.path().join("health").exists());
}

#[cfg(unix)]
mod native {
    use std::ffi::OsStr;
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, Write};
    use std::os::fd::AsFd;
    use std::os::unix::fs::{MetadataExt, symlink};
    use std::time::Duration;

    use solstone_core_journal_io::{FlatDirectory, locking::acquire_existing_parent_lock_bound};

    use super::super::unix::{self, Hooks, Point, SyncPoint};
    use super::*;
    use crate::ledger::append_offload_event;
    use solstone_core_retention::archive_catalog::{
        OffloadFile, RestoreDisposition, RestoreFileV2, codec::Event, read_complete_catalog,
    };

    fn archive_event(proof: &OffloadProof) -> Event {
        Event::ArchiveV2 {
            schema_version: 2,
            time: 1,
            day: proof.target.day.clone(),
            stream: proof.target.stream.clone(),
            segment: "010000_001".into(),
            segment_dir: proof.target.dir.clone(),
            layout: proof.layout,
            journal_id: proof.journal_id.clone(),
            manifest_id: proof.manifest_id.clone(),
            repository_id: proof.repository_id.clone(),
            snapshot_id: proof.snapshot_id.clone(),
            files: proof.files.clone(),
        }
    }

    fn operation(proof: &OffloadProof, id: &str) -> RestoreOperation {
        RestoreOperation {
            operation_id: id.into(),
            files: proof
                .files
                .iter()
                .enumerate()
                .map(|(index, f)| RestoreFileV2 {
                    witness_id: f.witness_id.clone(),
                    name: f.name.clone(),
                    disposition: if index == 0 {
                        RestoreDisposition::Published
                    } else {
                        RestoreDisposition::Conflict
                    },
                })
                .collect(),
        }
    }

    fn raw_event(f: &Fixture, day: &str, event: &Event) {
        fs::create_dir_all(f.path(day).parent().unwrap()).unwrap();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(f.path(day))
            .unwrap();
        let mut bytes = serde_json::to_vec(event).unwrap();
        bytes.push(b'\n');
        file.write_all(&bytes).unwrap();
    }

    fn confirmed(receipt: &LedgerWriteReceipt) {
        assert_eq!(
            receipt.confirmation(),
            LedgerConfirmation::Confirmed,
            "{receipt:?}"
        );
        assert_eq!(
            receipt.durability(),
            [LedgerSync::Confirmed; 4],
            "{receipt:?}"
        );
        assert!(receipt.failures().is_empty());
    }

    #[test]
    fn confirmation_resyncs_existing_archive_without_duplicate_bytes() {
        let fixture = Fixture::new();
        let proof = fixture.proof("confirm", SegmentLayout::Direct);
        confirmed(&append_archive_witness(&fixture.root, &proof, 1).unwrap());
        let before = fs::read(fixture.path(&proof.target.day)).unwrap();
        let receipt = confirm_archive_witness(&fixture.root, &proof, 2).unwrap();
        assert_eq!(receipt.effect(), LedgerWriteEffect::AlreadyRecorded);
        confirmed(&receipt);
        assert_eq!(fs::read(fixture.path(&proof.target.day)).unwrap(), before);
    }

    #[test]
    fn confirmation_never_appends_a_disappeared_or_changed_manifest() {
        for absent in [false, true] {
            let fixture = Fixture::new();
            let proof = fixture.proof("confirm", SegmentLayout::Named);
            confirmed(&append_archive_witness(&fixture.root, &proof, 1).unwrap());
            let path = fixture.path(&proof.target.day);
            if absent {
                fs::write(&path, b"").unwrap();
            } else {
                let mut changed = proof.clone();
                changed.files[0].archive_path = "/changed/audio.flac".into();
                fs::write(&path, {
                    let mut bytes = serde_json::to_vec(&archive_event(&changed)).unwrap();
                    bytes.push(b'\n');
                    bytes
                })
                .unwrap();
            }
            let before = fs::read(&path).unwrap();
            assert!(matches!(
                confirm_archive_witness(&fixture.root, &proof, 2),
                Err(LedgerWriteRefusal::Changed | LedgerWriteRefusal::Collision)
            ));
            assert_eq!(fs::read(path).unwrap(), before);
        }
    }

    enum WriteFault {
        None,
        Prefix,
        UnknownAfterWrite,
    }
    struct Faults<'a> {
        at: Box<dyn FnMut(Point) + 'a>,
        sync: Option<SyncPoint>,
        write: WriteFault,
    }
    impl Default for Faults<'_> {
        fn default() -> Self {
            Self {
                at: Box::new(|_| {}),
                sync: None,
                write: WriteFault::None,
            }
        }
    }
    impl Hooks for Faults<'_> {
        fn checkpoint(&mut self, point: Point) {
            (self.at)(point);
        }
        fn sync(&mut self, file: &impl AsFd, point: SyncPoint) -> io::Result<()> {
            if self.sync == Some(point) {
                Err(io::Error::other("injected sync uncertainty"))
            } else {
                nix::unistd::fsync(file).map_err(io::Error::from)
            }
        }
        fn write(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<usize> {
            match self.write {
                WriteFault::None => file.write(bytes),
                WriteFault::Prefix => file.write(&bytes[..19]),
                WriteFault::UnknownAfterWrite => {
                    file.write_all(bytes)?;
                    Err(io::Error::other("write result unknown"))
                }
            }
        }
    }

    #[test]
    fn both_layouts_roundtrip_all_bindings_and_retry_ignores_diagnostic_time() {
        for layout in [SegmentLayout::Direct, SegmentLayout::Named] {
            let f = Fixture::new();
            let proof = f.proof("one", layout);
            assert!(!f.temp.path().join("health").exists());
            let receipt = append_archive_witness(&f.root, &proof, 1).unwrap();
            confirmed(&receipt);
            assert!(matches!(
                receipt.effect(),
                LedgerWriteEffect::Appended { .. }
            ));
            let bytes = fs::read(f.path(&proof.target.day)).unwrap();
            let event: Event = serde_json::from_slice(bytes.strip_suffix(b"\n").unwrap()).unwrap();
            assert_eq!(
                serde_json::to_value(&event).unwrap(),
                serde_json::to_value(archive_event(&proof)).unwrap()
            );
            let catalog = read_complete_catalog(&f.root).unwrap();
            let witness = &catalog.days[0].segments[0].history[0].files[0];
            assert_eq!(
                witness.segment_dir.as_deref(),
                Some(proof.target.dir.as_str())
            );
            assert_eq!(witness.layout, Some(layout));
            assert_eq!(
                witness.processing.as_ref(),
                Some(&proof.files[0].processing)
            );
            let retry = append_archive_witness(&f.root, &proof, 999).unwrap();
            confirmed(&retry);
            assert_eq!(retry.effect(), LedgerWriteEffect::AlreadyRecorded);
            assert_eq!(fs::read(f.path(&proof.target.day)).unwrap(), bytes);
        }
    }

    #[test]
    fn changed_proof_or_source_root_refuses_without_append() {
        let f = Fixture::new();
        let proof = f.proof("one", SegmentLayout::Direct);
        confirmed(&append_archive_witness(&f.root, &proof, 1).unwrap());
        let before = fs::read(f.path(&proof.target.day)).unwrap();
        let mut twins = Vec::new();
        let mut p = proof.clone();
        p.target.dir = "other-010000_001".into();
        twins.push(p);
        let mut p = proof.clone();
        p.files[0].bytes += 1;
        twins.push(p);
        let mut p = proof.clone();
        p.files[0].archive_path = "/elsewhere/audio.flac".into();
        twins.push(p);
        let mut p = proof.clone();
        p.repository_id = "d".repeat(64);
        twins.push(p);
        let mut p = proof.clone();
        p.snapshot_id = "e".repeat(64);
        twins.push(p);
        let mut p = proof.clone();
        p.files[0].processing.record = Some(serde_json::json!({"unknown":true}));
        twins.push(p);
        let mut p = proof.clone();
        p.manifest_id = "different".into();
        twins.push(p);
        for p in twins {
            assert!(append_archive_witness(&f.root, &p, 2).is_err());
            assert_eq!(fs::read(f.path(&proof.target.day)).unwrap(), before);
        }
        let copy = Fixture::new();
        assert_eq!(
            append_archive_witness(&copy.root, &proof, 2).unwrap_err(),
            LedgerWriteRefusal::RootMismatch
        );
        assert!(!copy.temp.path().join("health").exists());
    }

    #[test]
    fn restore_requires_exact_historical_target_layout_and_ordered_operation() {
        let f = Fixture::new();
        let proof = f.proof("one", SegmentLayout::Direct);
        confirmed(&append_archive_witness(&f.root, &proof, 1).unwrap());
        let op = operation(&proof, "restore");
        confirmed(&append_restore_operation(&f.root, &proof.target, proof.layout, &op, 2).unwrap());
        let bytes = fs::read(f.path(&proof.target.day)).unwrap();
        let retry =
            append_restore_operation(&f.root, &proof.target, proof.layout, &op, 88).unwrap();
        confirmed(&retry);
        assert_eq!(retry.effect(), LedgerWriteEffect::AlreadyRecorded);
        assert_eq!(fs::read(f.path(&proof.target.day)).unwrap(), bytes);
        let catalog = read_complete_catalog(&f.root).unwrap();
        assert_eq!(
            catalog.days[0].segments[0].restores.as_slice(),
            std::slice::from_ref(&op)
        );
        assert_eq!(catalog.days[0].segments[0].active.len(), 1);
        let mut reversed = op.clone();
        reversed.files.reverse();
        assert_eq!(
            append_restore_operation(&f.root, &proof.target, proof.layout, &reversed, 3)
                .unwrap_err(),
            LedgerWriteRefusal::Collision
        );
        let mut wrong = proof.target.clone();
        wrong.dir = "other-010000_001".into();
        assert!(append_restore_operation(&f.root, &wrong, proof.layout, &op, 3).is_err());
        assert!(
            append_restore_operation(&f.root, &proof.target, SegmentLayout::Named, &op, 3).is_err()
        );
        let mut wrong_name = op.clone();
        wrong_name.files[0].name = "missing.flac".into();
        assert!(
            append_restore_operation(&f.root, &proof.target, proof.layout, &wrong_name, 3).is_err()
        );
        assert_eq!(fs::read(f.path(&proof.target.day)).unwrap(), bytes);
    }

    #[test]
    fn simultaneous_same_parsed_key_witnesses_cannot_be_mixed() {
        let f = Fixture::new();
        let one = f.proof("one", SegmentLayout::Direct);
        let mut two = f.proof("two", SegmentLayout::Direct);
        two.target.dir = "other-010000_001_suffix".into();
        confirmed(&append_archive_witness(&f.root, &one, 1).unwrap());
        confirmed(&append_archive_witness(&f.root, &two, 2).unwrap());
        let mut mixed = operation(&one, "mixed");
        mixed.files[1].witness_id = two.files[1].witness_id.clone();
        let before = fs::read(f.path(&one.target.day)).unwrap();
        assert!(append_restore_operation(&f.root, &one.target, one.layout, &mixed, 3).is_err());
        assert_eq!(fs::read(f.path(&one.target.day)).unwrap(), before);
        confirmed(
            &append_restore_operation(
                &f.root,
                &two.target,
                two.layout,
                &operation(&two, "two-op"),
                4,
            )
            .unwrap(),
        );
    }

    #[test]
    fn global_manifest_and_witness_collisions_refuse_even_an_exact_local_retry() {
        for duplicate_manifest in [false, true] {
            let f = Fixture::new();
            let one = f.proof("one", SegmentLayout::Direct);
            confirmed(&append_archive_witness(&f.root, &one, 1).unwrap());
            let mut two = f.proof("two", SegmentLayout::Direct);
            two.target.day = "20260102".into();
            if duplicate_manifest {
                two.manifest_id = one.manifest_id.clone();
            } else {
                two.files[0].witness_id = one.files[0].witness_id.clone();
            }
            raw_event(&f, &two.target.day, &archive_event(&two));
            assert!(read_complete_catalog(&f.root).is_ok());
            let before = fs::read(f.path(&one.target.day)).unwrap();
            assert_eq!(
                append_archive_witness(&f.root, &one, 3).unwrap_err(),
                LedgerWriteRefusal::Collision
            );
            assert_eq!(fs::read(f.path(&one.target.day)).unwrap(), before);
        }
    }

    #[test]
    fn global_operation_collision_refuses_even_an_exact_local_retry() {
        let f = Fixture::new();
        let one = f.proof("one", SegmentLayout::Direct);
        let mut two = f.proof("two", SegmentLayout::Direct);
        two.target.day = "20260102".into();
        for p in [&one, &two] {
            confirmed(&append_archive_witness(&f.root, p, 1).unwrap());
        }
        let one_op = operation(&one, "same-op");
        confirmed(&append_restore_operation(&f.root, &one.target, one.layout, &one_op, 2).unwrap());
        let two_op = operation(&two, "same-op");
        raw_event(
            &f,
            &two.target.day,
            &Event::RestoreV2 {
                schema_version: 2,
                time: 2,
                day: two.target.day.clone(),
                stream: two.target.stream.clone(),
                segment: "010000_001".into(),
                operation_id: two_op.operation_id,
                files: two_op.files,
            },
        );
        assert!(read_complete_catalog(&f.root).is_ok());
        assert_eq!(
            append_restore_operation(&f.root, &one.target, one.layout, &one_op, 3).unwrap_err(),
            LedgerWriteRefusal::Collision
        );
    }

    #[test]
    fn legacy_unbound_restore_and_degraded_unrelated_history_refuse() {
        let f = Fixture::new();
        let proof = f.proof("one", SegmentLayout::Direct);
        append_offload_event(
            f.temp.path(),
            &proof.target.day,
            &proof.target.stream,
            "010000_001",
            "legacy",
            &[OffloadFile {
                name: "audio.flac".into(),
                bytes: 7,
                sha256: "c".repeat(64),
            }],
            1,
        )
        .unwrap();
        let catalog = read_complete_catalog(&f.root).unwrap();
        let witness = &catalog.days[0].segments[0].active[0];
        let op = RestoreOperation {
            operation_id: "legacy-op".into(),
            files: vec![RestoreFileV2 {
                witness_id: witness.witness_id.clone(),
                name: witness.file.name.clone(),
                disposition: RestoreDisposition::AlreadyPresent,
            }],
        };
        assert!(append_restore_operation(&f.root, &proof.target, proof.layout, &op, 2).is_err());
        fs::write(f.path("20260102"), b"{torn").unwrap();
        let before = fs::read(f.path(&proof.target.day)).unwrap();
        assert!(matches!(
            append_archive_witness(&f.root, &proof, 2),
            Err(LedgerWriteRefusal::HistoryUnavailable(_))
        ));
        assert_eq!(fs::read(f.path(&proof.target.day)).unwrap(), before);
    }

    #[test]
    fn both_entries_refuse_real_lock_contention_then_succeed() {
        let f = Fixture::new();
        let proof = f.proof("one", SegmentLayout::Direct);
        confirmed(&append_archive_witness(&f.root, &proof, 1).unwrap());
        let parent = FlatDirectory::open(&f.root, std::path::Path::new("health/offload")).unwrap();
        let lock = acquire_existing_parent_lock_bound(
            &parent,
            OsStr::new("ledger.lock"),
            Duration::ZERO,
            Duration::from_millis(1),
        )
        .unwrap();
        let two = f.proof("two", SegmentLayout::Direct);
        let op = operation(&proof, "restore");
        assert_eq!(
            append_archive_witness(&f.root, &two, 2).unwrap_err(),
            LedgerWriteRefusal::Busy
        );
        assert_eq!(
            append_restore_operation(&f.root, &proof.target, proof.layout, &op, 2).unwrap_err(),
            LedgerWriteRefusal::Busy
        );
        drop(lock);
        confirmed(&append_archive_witness(&f.root, &two, 2).unwrap());
        confirmed(&append_restore_operation(&f.root, &proof.target, proof.layout, &op, 2).unwrap());
    }

    #[test]
    fn symlink_nonregular_and_hardlinked_ledgers_refuse_unchanged() {
        for kind in 0..3 {
            let f = Fixture::new();
            let proof = f.proof("one", SegmentLayout::Direct);
            fs::create_dir_all(f.path(&proof.target.day).parent().unwrap()).unwrap();
            let outside = f.temp.path().join("outside");
            fs::write(&outside, b"keep").unwrap();
            match kind {
                0 => symlink(&outside, f.path(&proof.target.day)).unwrap(),
                1 => fs::create_dir(f.path(&proof.target.day)).unwrap(),
                _ => fs::hard_link(&outside, f.path(&proof.target.day)).unwrap(),
            }
            assert!(append_archive_witness(&f.root, &proof, 1).is_err());
            assert_eq!(fs::read(outside).unwrap(), b"keep");
        }
    }

    #[test]
    fn retained_chain_and_leaf_replacements_refuse_before_append() {
        for component in ["health", "health/offload", "health/offload/20260101.jsonl"] {
            let f = Fixture::new();
            let proof = f.proof("one", SegmentLayout::Direct);
            confirmed(&append_archive_witness(&f.root, &proof, 1).unwrap());
            let old = f.temp.path().join(component);
            let saved = f.temp.path().join("saved");
            let original = fs::read(f.path(&proof.target.day)).unwrap();
            let mut hooks = Faults {
                at: Box::new(|point| {
                    if point == Point::BeforeWrite {
                        fs::rename(&old, &saved).unwrap();
                        if component.ends_with("jsonl") {
                            fs::write(&old, b"replacement").unwrap();
                        } else {
                            fs::create_dir(&old).unwrap();
                        }
                    }
                }),
                ..Default::default()
            };
            assert!(
                unix::append_archive(
                    &f.root,
                    &f.proof("two", SegmentLayout::Direct),
                    2,
                    &mut hooks
                )
                .is_err()
            );
            let saved_ledger = if component == "health" {
                saved.join("offload/20260101.jsonl")
            } else if component == "health/offload" {
                saved.join("20260101.jsonl")
            } else {
                saved.clone()
            };
            assert_eq!(fs::read(saved_ledger).unwrap(), original);
            if component.ends_with("jsonl") {
                assert_eq!(fs::read(&old).unwrap(), b"replacement");
            } else {
                assert_eq!(fs::read_dir(&old).unwrap().count(), 0);
            }
        }
    }

    #[test]
    fn copied_canonical_root_binding_refuses_without_touching_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("journal");
        fs::create_dir(&path).unwrap();
        let root = Arc::new(JournalRoot::open(&path).unwrap());
        let mut proof = Fixture::new().proof("one", SegmentLayout::Direct);
        proof.journal_id = root.identity().observation_id();
        fs::rename(&path, temp.path().join("old")).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(append_archive_witness(&root, &proof, 1).is_err());
        assert_eq!(fs::read_dir(path).unwrap().count(), 0);
        assert_eq!(fs::read_dir(temp.path().join("old")).unwrap().count(), 0);
    }

    #[test]
    fn each_sync_failure_retains_full_append_and_retry_resyncs_without_reappend() {
        for (index, point) in [
            SyncPoint::File,
            SyncPoint::Offload,
            SyncPoint::Health,
            SyncPoint::Root,
        ]
        .into_iter()
        .enumerate()
        {
            let f = Fixture::new();
            let proof = f.proof("one", SegmentLayout::Direct);
            let mut hooks = Faults {
                sync: Some(point),
                ..Default::default()
            };
            let receipt = unix::append_archive(&f.root, &proof, 1, &mut hooks).unwrap();
            assert!(matches!(
                receipt.effect(),
                LedgerWriteEffect::Appended { .. }
            ));
            assert_eq!(receipt.confirmation(), LedgerConfirmation::Confirmed);
            for (i, actual) in receipt.durability().into_iter().enumerate() {
                assert_eq!(
                    actual,
                    if i == index {
                        LedgerSync::Uncertain
                    } else {
                        LedgerSync::Confirmed
                    }
                );
            }
            let bytes = fs::read(f.path(&proof.target.day)).unwrap();
            let receipt = unix::append_archive(&f.root, &proof, 2, &mut hooks).unwrap();
            assert_eq!(receipt.effect(), LedgerWriteEffect::AlreadyRecorded);
            assert_eq!(receipt.durability()[index], LedgerSync::Uncertain);
            assert_eq!(fs::read(f.path(&proof.target.day)).unwrap(), bytes);
            confirmed(&append_archive_witness(&f.root, &proof, 3).unwrap());
            assert_eq!(fs::read(f.path(&proof.target.day)).unwrap(), bytes);
        }
    }

    #[test]
    fn partial_and_unknown_writes_retain_observed_effects_without_retry() {
        for fault in [WriteFault::Prefix, WriteFault::UnknownAfterWrite] {
            let f = Fixture::new();
            let proof = f.proof("one", SegmentLayout::Direct);
            let expected = match fault {
                WriteFault::Prefix => Some(19),
                _ => None,
            };
            let mut hooks = Faults {
                write: fault,
                ..Default::default()
            };
            let receipt = unix::append_archive(&f.root, &proof, 1, &mut hooks).unwrap();
            assert_eq!(
                receipt.effect(),
                LedgerWriteEffect::Attempted { written: expected }
            );
            assert_eq!(receipt.confirmation(), LedgerConfirmation::Uncertain);
            let bytes = fs::read(f.path(&proof.target.day)).unwrap();
            if expected.is_some() {
                assert_eq!(bytes.len(), 19);
                assert!(append_archive_witness(&f.root, &proof, 2).is_err());
            } else {
                assert!(!bytes.is_empty());
                let retry = append_archive_witness(&f.root, &proof, 2).unwrap();
                confirmed(&retry);
                assert_eq!(retry.effect(), LedgerWriteEffect::AlreadyRecorded);
            }
            assert_eq!(fs::read(f.path(&proof.target.day)).unwrap(), bytes);
            assert!(!f.temp.path().join("health/marks.json").exists());
        }
    }

    #[test]
    fn concurrent_legacy_append_is_preserved_and_confirmation_uncertain() {
        let f = Fixture::new();
        let proof = f.proof("one", SegmentLayout::Direct);
        let mut hooks = Faults {
            at: Box::new(|point| {
                if point == Point::AfterWrite {
                    append_offload_event(
                        f.temp.path(),
                        &proof.target.day,
                        &proof.target.stream,
                        "010000_001",
                        "legacy",
                        &[OffloadFile {
                            name: "later.flac".into(),
                            bytes: 9,
                            sha256: "d".repeat(64),
                        }],
                        2,
                    )
                    .unwrap();
                }
            }),
            ..Default::default()
        };
        let receipt = unix::append_archive(&f.root, &proof, 1, &mut hooks).unwrap();
        assert!(matches!(
            receipt.effect(),
            LedgerWriteEffect::Appended { .. }
        ));
        assert_eq!(receipt.confirmation(), LedgerConfirmation::Uncertain);
        let catalog = read_complete_catalog(&f.root).unwrap();
        assert_eq!(catalog.days[0].segments[0].history.len(), 2);
        let before = fs::read(f.path(&proof.target.day)).unwrap();
        let retry = append_archive_witness(&f.root, &proof, 3).unwrap();
        confirmed(&retry);
        assert_eq!(retry.effect(), LedgerWriteEffect::AlreadyRecorded);
        assert_eq!(fs::read(f.path(&proof.target.day)).unwrap(), before);
    }

    #[test]
    fn already_recorded_keeps_fact_when_leaf_changes_during_retry() {
        let f = Fixture::new();
        let proof = f.proof("one", SegmentLayout::Direct);
        confirmed(&append_archive_witness(&f.root, &proof, 1).unwrap());
        let old = f.temp.path().join("saved.jsonl");
        let mut hooks = Faults {
            at: Box::new(|point| {
                if point == Point::BeforeWrite {
                    fs::rename(f.path(&proof.target.day), &old).unwrap();
                    fs::write(f.path(&proof.target.day), b"replacement").unwrap();
                }
            }),
            ..Default::default()
        };
        let receipt = unix::append_archive(&f.root, &proof, 2, &mut hooks).unwrap();
        assert_eq!(receipt.effect(), LedgerWriteEffect::AlreadyRecorded);
        assert_eq!(receipt.confirmation(), LedgerConfirmation::Uncertain);
        assert!(!receipt.failures().is_empty());
        assert_eq!(fs::read(f.path(&proof.target.day)).unwrap(), b"replacement");
        assert_eq!(fs::metadata(&old).unwrap().nlink(), 1);
    }

    #[test]
    fn post_append_leaf_replacement_keeps_append_fact_and_replacement_bytes() {
        let f = Fixture::new();
        let proof = f.proof("one", SegmentLayout::Direct);
        let old = f.temp.path().join("saved.jsonl");
        let mut hooks = Faults {
            at: Box::new(|point| {
                if point == Point::AfterWrite {
                    fs::rename(f.path(&proof.target.day), &old).unwrap();
                    fs::write(f.path(&proof.target.day), b"replacement").unwrap();
                }
            }),
            ..Default::default()
        };
        let receipt = unix::append_archive(&f.root, &proof, 1, &mut hooks).unwrap();
        assert!(matches!(
            receipt.effect(),
            LedgerWriteEffect::Appended { .. }
        ));
        assert_eq!(receipt.confirmation(), LedgerConfirmation::Uncertain);
        assert_eq!(fs::read(f.path(&proof.target.day)).unwrap(), b"replacement");
        assert!(!fs::read(&old).unwrap().is_empty());
    }

    #[test]
    fn unrelated_semantic_drift_refuses_but_whitespace_drift_is_harmless() {
        for semantic in [false, true] {
            let f = Fixture::new();
            let proof = f.proof("one", SegmentLayout::Direct);
            fs::create_dir_all(f.path("20260102").parent().unwrap()).unwrap();
            fs::write(f.path("20260102"), b"\n").unwrap();
            let mut hooks = Faults {
                at: Box::new(|point| {
                    if point == Point::BeforeWrite {
                        if semantic {
                            let mut other = f.proof("other", SegmentLayout::Direct);
                            other.target.day = "20260102".into();
                            raw_event(&f, &other.target.day, &archive_event(&other));
                        } else {
                            OpenOptions::new()
                                .append(true)
                                .open(f.path("20260102"))
                                .unwrap()
                                .write_all(b"\n")
                                .unwrap();
                        }
                    }
                }),
                ..Default::default()
            };
            let result = unix::append_archive(&f.root, &proof, 1, &mut hooks);
            if semantic {
                assert_eq!(result.unwrap_err(), LedgerWriteRefusal::Changed);
                assert!(fs::read(f.path(&proof.target.day)).unwrap().is_empty());
            } else {
                confirmed(&result.unwrap());
            }
        }
    }

    #[test]
    fn restore_old_witness_preserves_newer_and_retains_durability_uncertainty() {
        let f = Fixture::new();
        let one = f.proof("one", SegmentLayout::Named);
        let two = f.proof("two", SegmentLayout::Named);
        for p in [&one, &two] {
            confirmed(&append_archive_witness(&f.root, p, 1).unwrap());
        }
        let op = operation(&one, "old-op");
        let mut hooks = Faults {
            sync: Some(SyncPoint::File),
            ..Default::default()
        };
        let receipt =
            unix::append_restore(&f.root, &one.target, one.layout, &op, 2, &mut hooks).unwrap();
        assert!(matches!(
            receipt.effect(),
            LedgerWriteEffect::Appended { .. }
        ));
        assert_eq!(receipt.confirmation(), LedgerConfirmation::Confirmed);
        assert_eq!(receipt.durability()[0], LedgerSync::Uncertain);
        let catalog = read_complete_catalog(&f.root).unwrap();
        assert_eq!(
            catalog.days[0].segments[0]
                .active
                .iter()
                .map(|w| &w.witness_id)
                .collect::<Vec<_>>(),
            two.files.iter().map(|w| &w.witness_id).collect::<Vec<_>>()
        );
        let before = fs::read(f.path(&one.target.day)).unwrap();
        let retry = append_restore_operation(&f.root, &one.target, one.layout, &op, 3).unwrap();
        confirmed(&retry);
        assert_eq!(retry.effect(), LedgerWriteEffect::AlreadyRecorded);
        assert_eq!(fs::read(f.path(&one.target.day)).unwrap(), before);
    }
}
