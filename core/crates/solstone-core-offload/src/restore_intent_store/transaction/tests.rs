// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use super::*;
use sha2::{Digest, Sha256};
use solstone_core_journal_io::activity::ActivityLease;
use solstone_core_journal_io::media::{
    MediaDigest, MediaDirectory, MediaPrimitive, run_with_media_hook,
};
use solstone_core_journal_io::paths::{PathOrDay, iter_segments};
use solstone_core_retention::archive_proof::{ArchiveFileProof, OffloadOffer, OffloadProof};
use std::cell::{Cell, RefCell};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Point {
    AfterInitialMark,
    AfterPublication,
}

type Hook = (Point, Box<dyn FnOnce()>);
thread_local! { static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) }; }

pub(super) fn checkpoint(point: Point) {
    let hook = HOOK.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot
            .as_ref()
            .is_some_and(|(expected, _)| *expected == point)
        {
            slot.take().map(|(_, callback)| callback)
        } else {
            None
        }
    });
    if let Some(hook) = hook {
        hook();
    }
}

fn with_hook<T>(point: Point, hook: impl FnOnce() + 'static, operation: impl FnOnce() -> T) -> T {
    HOOK.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some((point, Box::new(hook)));
    });
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            HOOK.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
    let _clear = Clear;
    let result = operation();
    HOOK.with(|slot| assert!(slot.borrow().is_none(), "checkpoint did not fire"));
    result
}

fn options() -> LockOptions {
    LockOptions {
        timeout: Duration::ZERO,
        poll_interval: Duration::ZERO,
        mode: Some(0o600),
    }
}

struct Fixture {
    temp: tempfile::TempDir,
    root: Arc<JournalRoot>,
    locator: crate::restore_intent::RestoreLocator,
    witness: ArchiveWitness,
    mark: Option<Mark>,
}

impl Fixture {
    fn new(with_mark: bool) -> Self {
        let template = crate::restore_intent::tests::fixture(with_mark);
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join(template.locator.relative_segment())).unwrap();
        fs::create_dir(temp.path().join("source")).unwrap();
        fs::write(temp.path().join("source/a.flac"), b"abc").unwrap();
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        let mut witness = template.witness.clone();
        witness.file.sha256 = format!("{:x}", Sha256::digest(b"abc"));
        witness.journal_id = Some(root.identity().observation_id());
        let file = ArchiveFileProof {
            witness_id: witness.witness_id.clone(),
            name: witness.file.name.clone(),
            bytes: witness.file.bytes,
            sha256: witness.file.sha256.clone(),
            archive_path: witness.archive_path.clone().unwrap(),
            processing: witness.processing.clone().unwrap(),
        };
        let mut proof = OffloadProof {
            journal_id: root.identity().observation_id(),
            manifest_id: witness.manifest_id.clone(),
            target: template.locator.target.clone(),
            layout: template.locator.layout,
            repository_id: witness.repository_id.clone().unwrap(),
            snapshot_id: witness.snapshot_id.clone(),
            files: vec![file.clone()],
        };
        let mark = template.retirement.expected().cloned().map(|mut mark| {
            let mut offered = mark.offload.as_ref().unwrap().proof.clone();
            offered.journal_id = proof.journal_id.clone();
            offered.files[0] = file;
            mark.offload = Some(OffloadOffer::new(offered.clone()).unwrap());
            proof = offered;
            mark
        });
        let written = crate::ledger::writer::append_archive_witness(&root, &proof, 1).unwrap();
        assert_eq!(written.confirmation(), LedgerConfirmation::Confirmed);
        if let Some(mark) = &mark {
            let mut register = Register::empty();
            register.version = 2;
            register.marks.insert(mark.id.clone(), mark.clone());
            fs::write(
                temp.path().join("health/retention-marks.json"),
                serde_json::to_vec(&register).unwrap(),
            )
            .unwrap();
        }
        Self {
            temp,
            root,
            locator: template.locator,
            witness,
            mark,
        }
    }

    fn raw(&self) -> PathBuf {
        self.temp
            .path()
            .join(self.locator.relative_segment())
            .join("a.flac")
    }
    fn ledger(&self) -> PathBuf {
        self.temp.path().join("health/offload/20261009.jsonl")
    }
    fn marker(&self) -> PathBuf {
        self.temp.path().join("health/20261009/stream.updated")
    }

    fn staged<'a>(&self, allowed: &'a Cell<bool>) -> StagedRestore<'a, 'static> {
        let activity = ActivityLease::try_acquire(Arc::clone(&self.root)).unwrap();
        let carrier = RootActivityCarrier::prepare(Arc::clone(&activity))
            .unwrap_or_else(|error| panic!("carrier {:?}", error.error()));
        let segment = iter_segments(self.root.canonical_path(), PathOrDay::Day("20261009"))
            .unwrap()
            .remove(0);
        let destination = PreparedRestore::admit(Arc::clone(&self.root), "20261009", &segment)
            .unwrap_or_else(|error| panic!("admit {:?}", error.reason));
        let stage = MediaStage::create(destination.stage_parent(), activity).unwrap();
        let observation = stage.observation().unwrap();
        let source =
            MediaDirectory::open(Arc::clone(&self.root), std::path::Path::new("source")).unwrap();
        let expected = MediaDigest {
            size: self.witness.file.bytes,
            sha256: self.witness.file.sha256.clone(),
        };
        let media = stage
            .create_target()
            .unwrap()
            .copy_from(&source, OsStr::new("a.flac"), &expected)
            .unwrap();
        StagedRestore {
            stage: StageEvidence {
                parent: observation.parent_identity,
                directory: observation.directory_identity,
                leaf: media.identity(),
                stage_name: observation.directory_name,
                leaf_name: "payload".into(),
                boot_id: None,
            },
            witness: self.witness.clone(),
            owner: Owner {
                carrier,
                destination,
                admission: Admission::LocalTest(allowed),
                media: Some(media),
            },
        }
    }
}

#[test]
fn actual_publish_records_intent_marker_and_exact_event_without_recount_on_replay() {
    let f = Fixture::new(false);
    let allowed = Cell::new(true);
    let result = f.staged(&allowed).commit(2, options());
    assert!(result.hold.is_none(), "{:?}", result.hold);
    assert_eq!(result.effects, AttemptEffects { files: 1, bytes: 3 });
    assert_eq!(fs::read(f.raw()).unwrap(), b"abc");
    let record = result.record.as_ref().unwrap();
    assert_eq!(record.intent.marker, Obligation::Complete);
    assert_eq!(record.intent.event_recorded, Obligation::Complete);
    assert_eq!(record.intent.cleanup, Obligation::Complete);
    assert!(record.writes.iter().all(StoreWriteReceipt::fully_confirmed));
    assert!(f.marker().exists());
    let id = record.intent.event.operation_id.clone();
    let before = fs::read(f.ledger()).unwrap();
    let owner = result.owner;
    drop(result.record);
    let replayed = replay_local(owner, &id, 3, options());
    assert!(replayed.hold.is_none(), "{:?}", replayed.hold);
    assert_eq!(replayed.effects, AttemptEffects::default());
    assert_eq!(fs::read(f.ledger()).unwrap(), before);
}

#[test]
fn replay_claim_contention_retains_admitted_store_and_exact_record() {
    let f = Fixture::new(false);
    let allowed = Cell::new(true);
    let result = f.staged(&allowed).commit(2, options());
    assert!(result.hold.is_none());
    let intent = result.record.as_ref().unwrap().intent.clone();
    let owner = result.owner;
    drop(result.record);
    let _claims = OutputClaims::acquire([f.raw().with_extension("jsonl")], options()).unwrap();
    let replayed = replay_local(owner, &intent.event.operation_id, 3, options());
    assert!(matches!(
        replayed.hold,
        Some(Hold::Repair(RepairHold::ClaimsUnavailable))
    ));
    let record = replayed.record.as_ref().unwrap();
    assert!(record.store.is_some());
    assert_eq!(record.persisted.as_ref(), Some(&intent));
    assert_eq!(replayed.effects, AttemptEffects::default());
    assert!(replayed.ledger.is_none());
    assert!(replayed.register.is_none());
}

#[test]
fn identical_is_zero_bytes_while_different_and_nonfile_are_conflicts() {
    for kind in 0..3 {
        let f = Fixture::new(false);
        let allowed = Cell::new(true);
        if kind == 0 {
            fs::write(f.raw(), b"abc").unwrap();
        }
        if kind == 1 {
            fs::write(f.raw(), b"old").unwrap();
        }
        if kind == 2 {
            fs::create_dir(f.raw()).unwrap();
        }
        let result = f.staged(&allowed).commit(2, options());
        let record = result
            .record
            .as_ref()
            .unwrap_or_else(|| panic!("kind {kind}: {:?}", result.hold));
        assert_eq!(result.effects, AttemptEffects::default());
        assert_eq!(
            record.intent.event.files[0].disposition,
            if kind == 0 {
                RestoreDisposition::AlreadyPresent
            } else {
                RestoreDisposition::Conflict
            }
        );
        if kind == 1 {
            assert_eq!(fs::read(f.raw()).unwrap(), b"old");
        }
        if kind == 2 {
            assert!(f.raw().is_dir());
        }
        assert_eq!(record.intent.event_recorded, Obligation::Complete);
    }
}

#[test]
fn torn_initial_write_refuses_before_native_publication() {
    let f = Fixture::new(false);
    let allowed = Cell::new(true);
    let staged = f.staged(&allowed);
    use solstone_core_journal_io::atomic::{
        BoundPublicationPrimitive, run_with_bound_publication_fault,
    };
    let (result, fired) = run_with_bound_publication_fault(
        BoundPublicationPrimitive::FileSync,
        1,
        nix::libc::EIO,
        || staged.commit(2, options()),
    );
    assert!(fired);
    assert!(result.hold.is_some());
    assert!(result.publication.is_none());
    assert!(!f.raw().exists());
    assert_eq!(result.effects, AttemptEffects::default());
    assert!(
        result
            .record
            .as_ref()
            .is_some_and(|record| !record.writes[0].fully_confirmed())
    );
}

#[test]
fn actual_publication_survives_torn_outcome_persistence_without_fabricating_success() {
    let f = Fixture::new(false);
    let allowed = Cell::new(true);
    let staged = f.staged(&allowed);
    let intent_dir = f.temp.path().join("health/offload/restore-intents");
    let result = with_hook(
        Point::AfterPublication,
        move || {
            let path = fs::read_dir(intent_dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.extension().is_some_and(|ext| ext == "json"))
                .unwrap();
            fs::write(path, b"{").unwrap();
        },
        || staged.commit(2, options()),
    );
    assert!(result.hold.is_some());
    assert_eq!(result.effects, AttemptEffects { files: 1, bytes: 3 });
    assert_eq!(
        result.publication.as_ref().unwrap().effect,
        PublicationEffect::Published
    );
    assert_eq!(fs::read(f.raw()).unwrap(), b"abc");
    assert_eq!(
        result
            .record
            .as_ref()
            .unwrap()
            .intent
            .outcome
            .as_ref()
            .unwrap()
            .effect,
        PublicationEffect::Published
    );
    assert!(!f.marker().exists());
}

#[test]
fn cancellation_after_publication_still_persists_actual_effect_then_halts_marker_event_cas() {
    let f = Fixture::new(false);
    let allowed = std::rc::Rc::new(Cell::new(true));
    let staged = f.staged(&allowed);
    let stop = std::rc::Rc::clone(&allowed);
    let result = with_hook(
        Point::AfterPublication,
        move || stop.set(false),
        || staged.commit(2, options()),
    );
    assert!(matches!(result.hold, Some(Hold::Admission(_))));
    assert_eq!(result.effects, AttemptEffects { files: 1, bytes: 3 });
    let record = result.record.as_ref().unwrap();
    assert_eq!(
        record
            .persisted
            .as_ref()
            .unwrap()
            .outcome
            .as_ref()
            .unwrap()
            .effect,
        PublicationEffect::Published
    );
    assert_eq!(record.intent.event_recorded, Obligation::Pending);
    assert!(!f.marker().exists());
    assert!(result.ledger.is_none());
    assert!(result.register.is_none());
}

#[test]
fn changed_full_mark_after_fixed_claim_discovery_is_preserved_before_intent_or_publish() {
    let f = Fixture::new(true);
    let allowed = Cell::new(true);
    let staged = f.staged(&allowed);
    let path = f.temp.path().join("health/retention-marks.json");
    let mut register = review_context::load_at_root(&f.root).unwrap();
    register.marks.values_mut().next().unwrap().marked_at = "2026-10-10T00:00:01Z".into();
    let changed = serde_json::to_vec(&register).unwrap();
    let expected = changed.clone();
    let changed_path = path.clone();
    let result = with_hook(
        Point::AfterInitialMark,
        move || fs::write(changed_path, changed).unwrap(),
        || staged.commit(2, options()),
    );
    assert!(matches!(
        result.hold,
        Some(Hold::Repair(RepairHold::MarkChanged))
    ));
    assert_eq!(fs::read(path).unwrap(), expected);
    assert!(!f.raw().exists());
    assert!(result.record.is_none());
    assert!(result.publication.is_none());
}

#[test]
fn subset_remainder_is_persisted_before_publication_and_typed_writer_hold_is_truthful() {
    let f = Fixture::new(true);
    let allowed = Cell::new(true);
    let original = fs::read(f.temp.path().join("health/retention-marks.json")).unwrap();
    let result = f.staged(&allowed).commit(2, options());
    assert!(matches!(result.hold, Some(Hold::Register)));
    let record = result.record.as_ref().unwrap();
    let crate::restore_intent::MarkRetirement::Exact { plan } = &record.intent.retirement else {
        panic!("no exact plan")
    };
    assert_eq!(plan.expected(), f.mark.as_ref().unwrap());
    assert_eq!(plan.replacement().unwrap().proposal.names, ["b.flac"]);
    assert_eq!(plan.replacement().unwrap().proposal.bytes, 7);
    assert_eq!(record.intent.event_recorded, Obligation::Complete);
    assert_eq!(record.intent.register, Obligation::Pending);
    assert_eq!(
        fs::read(f.temp.path().join("health/retention-marks.json")).unwrap(),
        original
    );
}

#[test]
fn missing_recorded_publication_never_advances_replay_marker_or_cas() {
    let f = Fixture::new(true);
    let allowed = Cell::new(true);
    let mut result = f.staged(&allowed).commit(2, options());
    let record = result.record.as_mut().unwrap();
    let id = record.intent.event.operation_id.clone();
    assert_eq!(record.intent.event_recorded, Obligation::Complete);
    fs::remove_file(f.raw()).unwrap();
    let before = fs::read(f.marker()).unwrap();
    let owner = result.owner;
    drop(result.record);
    let replayed = replay_local(owner, &id, 3, options());
    assert!(matches!(
        replayed.hold,
        Some(Hold::Repair(RepairHold::UncertainContent))
    ));
    assert_eq!(replayed.effects, AttemptEffects::default());
    assert!(replayed.register.is_none());
    assert_eq!(fs::read(f.marker()).unwrap(), before);
}

#[test]
fn unverified_publication_matching_bytes_never_becomes_successful_event_or_cas() {
    let f = Fixture::new(false);
    let allowed = Cell::new(true);
    let staged = f.staged(&allowed);
    let (result, fired) = run_with_media_hook(
        MediaPrimitive::AfterPublish,
        || Err(std::io::Error::other("postpublication cut")),
        || staged.commit(2, options()),
    );
    assert!(fired);
    assert!(matches!(
        result.hold,
        Some(Hold::Repair(RepairHold::UncertainContent))
    ));
    assert_eq!(result.effects, AttemptEffects { files: 1, bytes: 3 });
    let id = result
        .record
        .as_ref()
        .unwrap()
        .intent
        .event
        .operation_id
        .clone();
    assert_eq!(
        result.record.as_ref().unwrap().intent.event_recorded,
        Obligation::Pending
    );
    assert!(result.marker.is_some()); // invalidate the real effect without claiming restore success
    let before = fs::read(f.ledger()).unwrap();
    let owner = result.owner;
    drop(result.record);
    let replayed = replay_local(owner, &id, 3, options());
    assert!(matches!(
        replayed.hold,
        Some(Hold::Repair(RepairHold::UncertainContent))
    ));
    assert_eq!(replayed.effects, AttemptEffects::default());
    assert!(replayed.ledger.is_none());
    assert!(replayed.register.is_none());
    assert_eq!(fs::read(f.ledger()).unwrap(), before);
}

#[test]
fn published_parent_sync_failure_holds_event_and_replay_even_when_bytes_match() {
    let f = Fixture::new(false);
    let allowed = Cell::new(true);
    let staged = f.staged(&allowed);
    let (result, fired) = run_with_media_hook(
        MediaPrimitive::ParentSync,
        || Err(std::io::Error::other("publication parent sync cut")),
        || staged.commit(2, options()),
    );
    assert!(fired);
    assert!(matches!(
        result.hold,
        Some(Hold::Repair(RepairHold::UncertainContent))
    ));
    assert_eq!(result.effects, AttemptEffects { files: 1, bytes: 3 });
    let publication = result.publication.as_ref().unwrap();
    assert_eq!(publication.confirmation, Confirmation::Confirmed);
    assert_eq!(publication.parent_durability, Durability::Unproven);
    assert_eq!(fs::read(f.raw()).unwrap(), b"abc");
    let record = result.record.as_ref().unwrap();
    let id = record.intent.event.operation_id.clone();
    assert_eq!(record.intent.marker, Obligation::Complete);
    assert_eq!(record.intent.event_recorded, Obligation::Pending);
    assert!(result.ledger.is_none());
    assert!(result.register.is_none());
    let before = fs::read(f.ledger()).unwrap();
    let owner = result.owner;
    drop(result.record);
    let replayed = replay_local(owner, &id, 3, options());
    assert!(matches!(
        replayed.hold,
        Some(Hold::Repair(RepairHold::UncertainContent))
    ));
    assert_eq!(replayed.effects, AttemptEffects::default());
    assert!(replayed.ledger.is_none());
    assert!(replayed.register.is_none());
    assert_eq!(fs::read(f.ledger()).unwrap(), before);
}

#[test]
fn marker_failure_preserves_published_effect_and_pending_event() {
    let f = Fixture::new(false);
    let allowed = Cell::new(true);
    let staged = f.staged(&allowed);
    fs::create_dir_all(f.marker().parent().unwrap()).unwrap();
    fs::write(f.marker(), b"broken marker").unwrap();
    let result = staged.commit(2, options());
    assert!(matches!(result.hold, Some(Hold::Marker)));
    assert_eq!(result.effects, AttemptEffects { files: 1, bytes: 3 });
    let record = result.record.as_ref().unwrap();
    assert_eq!(
        record
            .persisted
            .as_ref()
            .unwrap()
            .outcome
            .as_ref()
            .unwrap()
            .effect,
        PublicationEffect::Published
    );
    assert_eq!(record.intent.marker, Obligation::Pending);
    assert_eq!(record.intent.event_recorded, Obligation::Pending);
    assert!(result.ledger.is_none());
    assert_eq!(fs::read(f.marker()).unwrap(), b"broken marker");
}

#[test]
fn late_degraded_catalog_still_invalidates_actual_publication_then_holds_event() {
    use std::io::Write;
    let f = Fixture::new(false);
    let allowed = Cell::new(true);
    let staged = f.staged(&allowed);
    let path = f.ledger();
    let result = with_hook(
        Point::AfterPublication,
        move || {
            fs::OpenOptions::new()
                .append(true)
                .open(path)
                .unwrap()
                .write_all(b"{\n")
                .unwrap();
        },
        || staged.commit(2, options()),
    );
    assert!(matches!(
        result.hold,
        Some(Hold::Repair(RepairHold::DegradedCatalog))
    ));
    assert_eq!(result.effects, AttemptEffects { files: 1, bytes: 3 });
    assert_eq!(
        result.record.as_ref().unwrap().intent.marker,
        Obligation::Complete
    );
    assert_eq!(
        result.record.as_ref().unwrap().intent.event_recorded,
        Obligation::Pending
    );
    assert!(result.ledger.is_none());
    assert!(fs::read(f.ledger()).unwrap().ends_with(b"{\n"));
}

#[test]
fn late_owner_delete_or_missing_segment_refuses_without_recreation_or_intent() {
    for missing in [false, true] {
        let f = Fixture::new(false);
        let allowed = Cell::new(true);
        let staged = f.staged(&allowed);
        let segment = f.raw().parent().unwrap().to_path_buf();
        if missing {
            fs::rename(&segment, segment.with_extension("moved")).unwrap();
        } else {
            fs::write(segment.join("tombstone.json"), b"owner deleted").unwrap();
        }
        let result = staged.commit(2, options());
        assert!(matches!(
            result.hold,
            Some(Hold::Repair(RepairHold::DestinationChanged))
        ));
        assert!(result.record.is_none());
        assert!(result.publication.is_none());
        assert_eq!(result.effects, AttemptEffects::default());
        if missing {
            assert!(!segment.exists());
        } else {
            assert_eq!(
                fs::read(segment.join("tombstone.json")).unwrap(),
                b"owner deleted"
            );
        }
    }
}
