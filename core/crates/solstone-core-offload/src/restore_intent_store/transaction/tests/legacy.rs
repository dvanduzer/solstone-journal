// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use solstone_core_retention::archive_catalog::{LegacyRestoreFileBinding, codec::Event};

fn fixture() -> (Fixture, LegacyRestoreBinding) {
    let mut f = Fixture::new(true);
    fs::write(f.temp.path().join("source/b.flac"), b"abc").unwrap();
    let mut other = f.witness.file.clone();
    other.name = "b.flac".into();
    let mut original = serde_json::to_vec(&Event::Archive {
        time: 1,
        day: f.locator.target.day.clone(),
        stream: f.locator.target.stream.clone(),
        segment: f.locator.segment.clone(),
        snapshot_id: "bbbbbbbb".into(),
        files: vec![f.witness.file.clone(), other],
    })
    .unwrap();
    original.push(b'\n');
    fs::write(f.ledger(), original).unwrap();
    let mut mark = f.mark.take().unwrap();
    mark.offload = None;
    let mut register = Register::empty();
    register.marks.insert(mark.id.clone(), mark.clone());
    fs::write(
        f.temp.path().join("health/retention-marks.json"),
        serde_json::to_vec(&register).unwrap(),
    )
    .unwrap();
    f.mark = Some(mark);
    let catalog = read_complete_catalog(&f.root).unwrap();
    assert!(!catalog.degraded());
    let manifest = catalog.days[0].segments[0].history[0].clone();
    f.witness = manifest.files[0].clone();
    let mut binding = LegacyRestoreBinding {
        adoption_id: "d".repeat(64),
        revision: String::new(),
        target: f.locator.target.clone(),
        layout: f.locator.layout,
        repository_id: "c".repeat(64),
        snapshot_id: "b".repeat(64),
        files: manifest
            .files
            .iter()
            .map(|witness| LegacyRestoreFileBinding {
                witness_id: witness.witness_id.clone(),
                name: witness.file.name.clone(),
                bytes: witness.file.bytes,
                sha256: witness.file.sha256.clone(),
                archive_path: format!("/legacy/{}", witness.file.name),
            })
            .collect(),
        legacy_manifest: manifest,
    };
    binding.revision = binding.canonical_revision().unwrap();
    let appended =
        crate::ledger::writer::append_legacy_restore_binding(&f.root, &binding, 2).unwrap();
    assert_eq!(appended.confirmation(), LedgerConfirmation::Confirmed);
    assert_eq!(appended.durability(), [LedgerSync::Confirmed; 4]);
    (f, binding)
}

fn staged<'a>(
    f: &mut Fixture,
    binding: &LegacyRestoreBinding,
    index: usize,
    allowed: &'a Cell<bool>,
    carrier: Arc<RootActivityCarrier>,
) -> StagedRestore<'a, 'static> {
    f.witness = binding.legacy_manifest.files[index].clone();
    let mut result = f.staged_with_carrier(allowed, carrier);
    result.legacy_binding = Some(binding.clone());
    result
}

fn carrier(f: &Fixture) -> Arc<RootActivityCarrier> {
    let activity = ActivityLease::try_acquire(Arc::clone(&f.root)).unwrap();
    Arc::new(RootActivityCarrier::prepare(activity).unwrap())
}

#[test]
fn legacy_sequential_native_publication_records_adoption_and_preserves_whole_mark() {
    for first_conflict in [false, true] {
        let (mut f, binding) = fixture();
        let allowed = Cell::new(true);
        let carrier = carrier(&f);
        let path = f.temp.path().join("health/retention-marks.json");
        let original = fs::read(&path).unwrap();
        if first_conflict {
            fs::write(f.raw(), b"old").unwrap();
        }
        let mut outcomes = Vec::new();
        for index in 0..2 {
            let outcome = staged(&mut f, &binding, index, &allowed, Arc::clone(&carrier))
                .commit(3, options())
                .retire_intent(options());
            assert!(outcome.hold.is_none(), "{:?}", outcome.hold);
            assert!(outcome.register.is_none());
            let intent = &outcome.record.as_ref().unwrap().intent;
            assert_eq!(intent.register, Obligation::NotRequired);
            assert_eq!(intent.retirement.expected(), f.mark.as_ref());
            assert_eq!(intent.legacy_binding.as_ref(), Some(&binding));
            assert!(intent.witness.repository_id.is_none());
            assert!(intent.witness.processing.is_none());
            if first_conflict && index == 0 {
                assert_eq!(outcome.effects, AttemptEffects::default());
                assert!(outcome.marker.is_none());
                assert!(outcome.no_effect_cleanup.as_ref().unwrap().confirmed());
                assert!(!f.marker().exists());
            } else {
                assert_eq!(outcome.effects, AttemptEffects { files: 1, bytes: 3 });
            }
            assert_eq!(fs::read(&path).unwrap(), original);
            let outcome = outcome.into_owned();
            assert!(outcome.completed());
            let census = observe_with_carrier(&f.root, carrier.observation()).unwrap();
            assert!(census.unresolved_entries().next().is_none());
            outcomes.push(outcome);
        }
        let catalog = read_complete_catalog(&f.root).unwrap();
        let segment = &catalog.days[0].segments[0];
        assert_eq!(segment.history, vec![binding.legacy_manifest.clone()]);
        assert_eq!(segment.restore_bindings, vec![binding.clone()]);
        assert_eq!(segment.legacy_restores.len(), 2);
        assert!(
            segment
                .legacy_restores
                .iter()
                .all(|operation| operation.adoption_id == binding.adoption_id)
        );
        assert_eq!(segment.restores.len(), 2);
        assert_eq!(segment.active.len(), usize::from(first_conflict));
        assert_eq!(
            outcomes
                .iter()
                .map(|outcome| outcome.effects().files)
                .sum::<u64>(),
            if first_conflict { 1 } else { 2 }
        );
    }
}

#[test]
fn legacy_late_whole_mark_change_invalidates_actual_publication_but_holds_event_and_cas() {
    let (mut f, binding) = fixture();
    let allowed = Cell::new(true);
    let carrier = carrier(&f);
    let stage = staged(&mut f, &binding, 0, &allowed, carrier);
    let path = f.temp.path().join("health/retention-marks.json");
    let mut changed = review_context::load_at_root(&f.root).unwrap();
    changed
        .marks
        .values_mut()
        .next()
        .unwrap()
        .proposal
        .reason
        .push_str(" changed");
    let bytes = serde_json::to_vec(&changed).unwrap();
    let expected = bytes.clone();
    let result = with_hook(
        Point::AfterPublication,
        move || fs::write(path, bytes).unwrap(),
        || stage.commit(3, options()),
    );
    assert!(matches!(
        result.hold,
        Some(Hold::Repair(RepairHold::MarkChanged))
    ));
    assert_eq!(result.effects, AttemptEffects { files: 1, bytes: 3 });
    assert!(result.marker.as_ref().is_some_and(Result::is_ok));
    assert_eq!(
        result.record.as_ref().unwrap().intent.marker,
        Obligation::Complete
    );
    assert!(result.ledger.is_none());
    assert!(result.register.is_none());
    assert_eq!(fs::read(f.raw()).unwrap(), b"abc");
    assert_eq!(
        fs::read(f.temp.path().join("health/retention-marks.json")).unwrap(),
        expected
    );
    assert!(result.record.as_ref().unwrap().store.is_some());
    assert!(result.retirement.is_none());
}

#[test]
fn legacy_changed_recorded_binding_refuses_before_native_publication() {
    let (mut f, binding) = fixture();
    let allowed = Cell::new(true);
    let carrier = carrier(&f);
    let stage = staged(&mut f, &binding, 0, &allowed, carrier);
    let mut rows = fs::read_to_string(f.ledger())
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    rows.pop().unwrap();
    fs::write(f.ledger(), rows.join("\n") + "\n").unwrap();
    let result = stage.commit(3, options());
    assert!(matches!(
        result.hold,
        Some(Hold::Repair(RepairHold::CatalogChanged))
    ));
    assert!(result.publication.is_none());
    assert!(result.record.is_none());
    assert!(result.owner.media.is_some());
    assert!(!f.raw().exists());
}

#[test]
fn legacy_cross_day_operation_collision_holds_confirmed_intent_before_retirement() {
    let (mut f, binding) = fixture();
    let allowed = Cell::new(true);
    let carrier = carrier(&f);
    let result = staged(&mut f, &binding, 0, &allowed, carrier).commit(3, options());
    assert!(result.hold.is_none(), "{:?}", result.hold);
    let operation_id = result
        .record
        .as_ref()
        .unwrap()
        .intent
        .event
        .operation_id
        .clone();
    let mut other = binding.clone();
    other.target.day = "20261008".into();
    other.adoption_id = "e".repeat(64);
    other.legacy_manifest.manifest_id = "v1:20261008:1".into();
    for (witness, file) in other.legacy_manifest.files.iter_mut().zip(&mut other.files) {
        witness.manifest_id = other.legacy_manifest.manifest_id.clone();
        witness.witness_id = format!("{}:{}", witness.manifest_id, witness.file.name);
        file.witness_id = witness.witness_id.clone();
    }
    other.revision = other.canonical_revision().unwrap();
    other.validate().unwrap();
    let events = [
        Event::Archive {
            time: 1,
            day: other.target.day.clone(),
            stream: other.target.stream.clone(),
            segment: f.locator.segment.clone(),
            snapshot_id: "bbbbbbbb".into(),
            files: other
                .legacy_manifest
                .files
                .iter()
                .map(|witness| witness.file.clone())
                .collect(),
        },
        Event::RestoreBindingV1 {
            schema_version: 1,
            time: 2,
            day: other.target.day.clone(),
            stream: other.target.stream.clone(),
            segment: f.locator.segment.clone(),
            binding: other.clone(),
        },
        Event::LegacyRestoreV1 {
            schema_version: 1,
            time: 3,
            day: other.target.day.clone(),
            stream: other.target.stream.clone(),
            segment: f.locator.segment.clone(),
            segment_dir: other.target.dir.clone(),
            layout: other.layout,
            adoption_id: other.adoption_id.clone(),
            operation_id: operation_id.clone(),
            files: vec![solstone_core_retention::archive_catalog::RestoreFileV2 {
                witness_id: other.files[0].witness_id.clone(),
                name: other.files[0].name.clone(),
                disposition: RestoreDisposition::AlreadyPresent,
            }],
        },
    ];
    let mut bytes = Vec::new();
    for event in events {
        bytes.extend(serde_json::to_vec(&event).unwrap());
        bytes.push(b'\n');
    }
    fs::write(f.temp.path().join("health/offload/20261008.jsonl"), bytes).unwrap();
    let catalog = read_complete_catalog(&f.root).unwrap();
    assert!(!catalog.degraded());
    assert_eq!(
        catalog
            .days
            .iter()
            .flat_map(|day| &day.segments)
            .flat_map(|segment| &segment.restores)
            .filter(|operation| operation.operation_id == operation_id)
            .count(),
        2
    );
    let result = result.retire_intent(options());
    assert!(matches!(
        result.hold,
        Some(Hold::Repair(RepairHold::OperationCollision))
    ));
    assert!(result.retirement.is_none());
    assert!(result.record.as_ref().unwrap().store.is_some());
    assert_eq!(result.effects, AttemptEffects { files: 1, bytes: 3 });
}
