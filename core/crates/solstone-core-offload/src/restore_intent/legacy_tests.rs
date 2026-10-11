// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use solstone_core_retention::archive_catalog::{ArchiveManifest, LegacyRestoreFileBinding};

fn fixture(with_mark: bool) -> RestoreIntent {
    let mut intent = tests::fixture(with_mark);
    let mut witness = intent.witness.clone();
    witness.manifest_id = "v1:20261009:1".into();
    witness.witness_id = "v1:20261009:1:a.flac".into();
    witness.snapshot_id = "bbbbbbbb".into();
    witness.repository_id = None;
    witness.archive_path = None;
    witness.journal_id = None;
    witness.layout = None;
    witness.segment_dir = None;
    witness.processing = None;
    let mut other = witness.clone();
    other.witness_id = "v1:20261009:1:b.flac".into();
    other.file.name = "b.flac".into();
    other.file.bytes = 7;
    let manifest = ArchiveManifest {
        manifest_id: witness.manifest_id.clone(),
        files: vec![witness.clone(), other],
    };
    let mut binding = LegacyRestoreBinding {
        adoption_id: "d".repeat(64),
        revision: String::new(),
        target: intent.locator.target.clone(),
        layout: intent.locator.layout,
        repository_id: "c".repeat(64),
        snapshot_id: "b".repeat(64),
        files: manifest
            .files
            .iter()
            .map(|original| LegacyRestoreFileBinding {
                witness_id: original.witness_id.clone(),
                name: original.file.name.clone(),
                bytes: original.file.bytes,
                sha256: original.file.sha256.clone(),
                archive_path: format!("/legacy/{}", original.file.name),
            })
            .collect(),
        legacy_manifest: manifest,
    };
    binding.revision = binding.canonical_revision().unwrap();
    let expected = intent.retirement.expected().cloned().map(|mut mark| {
        mark.offload = None;
        Box::new(mark)
    });
    intent.event.files[0].witness_id = witness.witness_id.clone();
    intent.witness = witness;
    intent.legacy_binding = Some(binding);
    intent.retirement = MarkRetirement::PreserveLegacy { expected };
    intent.register = Obligation::NotRequired;
    intent.validate().unwrap();
    intent
}

fn catalog(intent: &RestoreIntent) -> SegmentCatalog {
    let binding = intent.legacy_binding.as_ref().unwrap();
    SegmentCatalog {
        history: vec![binding.legacy_manifest.clone()],
        restore_bindings: vec![binding.clone()],
        active: binding.legacy_manifest.files.clone(),
        ..tests::catalog(intent)
    }
}

fn inspect(
    intent: &RestoreIntent,
    catalog: &SegmentCatalog,
    register: &Register,
) -> Result<RecoveryDecision, RepairHold> {
    inspect_recovery(
        intent,
        &intent.destination,
        catalog,
        register,
        CurrentContent::Missing,
    )
}

#[test]
fn legacy_intent_is_strict_and_bound_wire_omits_the_extension() {
    for with_mark in [false, true] {
        let bound = tests::fixture(with_mark);
        let old = bound.encode().unwrap();
        let mut explicit_none: serde_json::Value = serde_json::from_slice(&old).unwrap();
        assert!(explicit_none.get("legacy_binding").is_none());
        explicit_none["legacy_binding"] = serde_json::Value::Null;
        let decoded =
            RestoreIntent::decode(Ok(&serde_json::to_vec(&explicit_none).unwrap())).unwrap();
        assert_eq!(decoded.encode().unwrap(), old);
        let legacy = fixture(with_mark);
        let bytes = legacy.encode().unwrap();
        assert_eq!(RestoreIntent::decode(Ok(&bytes)).unwrap(), legacy);
        let mut unknown: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        unknown["legacy_binding"]["legacy_manifest"]["future"] = serde_json::json!(1);
        assert!(RestoreIntent::decode(Ok(&serde_json::to_vec(&unknown).unwrap())).is_err());
        assert!(legacy.witness.repository_id.is_none());
        assert!(legacy.witness.processing.is_none());
        assert_eq!(legacy.register, Obligation::NotRequired);
        assert_eq!(
            legacy.output_paths(Path::new("journal")).len(),
            if with_mark { 2 } else { 1 }
        );
    }
}

#[test]
fn legacy_binding_and_preserved_mark_cannot_be_mixed_with_bound_intents_or_cas() {
    let original = fixture(true);
    let mut changed = original.clone();
    changed.legacy_binding = None;
    assert!(changed.validate().is_err());
    changed = original.clone();
    changed.retirement = MarkRetirement::NoMark;
    assert!(changed.validate().is_err());
    changed = original.clone();
    changed.register = Obligation::Complete;
    assert!(changed.validate().is_err());
    changed = original.clone();
    changed.witness = tests::fixture(true).witness;
    changed.event.files[0].witness_id = changed.witness.witness_id.clone();
    assert!(changed.validate().is_err());
    changed = original.clone();
    let binding = changed.legacy_binding.as_mut().unwrap();
    binding.adoption_id = "e".repeat(64);
    binding.revision = binding.canonical_revision().unwrap();
    changed.validate().unwrap();
    assert_eq!(
        original.validate_successor(&changed),
        Err(RepairHold::ImmutableEvent)
    );
}

#[test]
fn exact_adoption_reference_and_entire_manifest_are_required_for_recovery() {
    let intent = fixture(false);
    let register = tests::register(&intent);
    let correct = catalog(&intent);
    assert_eq!(
        inspect(&intent, &correct, &register).unwrap().register,
        RegisterDecision::NoMark
    );
    let mut changed = correct.clone();
    changed.restore_bindings.clear();
    assert_eq!(
        inspect(&intent, &changed, &register).unwrap_err(),
        RepairHold::CatalogChanged
    );
    changed = correct.clone();
    changed.history[0].files[1].file.bytes += 1;
    assert_eq!(
        inspect(&intent, &changed, &register).unwrap_err(),
        RepairHold::CatalogChanged
    );
    changed = correct.clone();
    changed.restores.push(intent.event.clone());
    assert_eq!(
        inspect(&intent, &changed, &register).unwrap_err(),
        RepairHold::OperationCollision
    );
    changed
        .legacy_restores
        .push(intent.legacy_operation().unwrap());
    assert!(inspect(&intent, &changed, &register).is_ok());
    changed.legacy_restores[0].adoption_id = "e".repeat(64);
    assert_eq!(
        inspect(&intent, &changed, &register).unwrap_err(),
        RepairHold::OperationCollision
    );
}

#[test]
fn recorded_legacy_success_still_preserves_the_entire_original_mark() {
    for with_mark in [false, true] {
        let mut intent = fixture(with_mark);
        intent.event.files[0].disposition = RestoreDisposition::AlreadyPresent;
        intent.confirmed = Some(ConfirmedContent {
            identity: tests::identity(7),
            digest: intent.expected_digest(),
        });
        intent.marker = Obligation::Complete;
        intent.event_recorded = Obligation::Complete;
        intent.cleanup = Obligation::Complete;
        intent.validate().unwrap();
        let mut history = catalog(&intent);
        history.active.retain(|witness| witness != &intent.witness);
        history.restores.push(intent.event.clone());
        history
            .legacy_restores
            .push(intent.legacy_operation().unwrap());
        let mut register = tests::register(&intent);
        let before = register.clone();
        assert_eq!(
            inspect(&intent, &history, &register).unwrap().register,
            RegisterDecision::NoMark
        );
        assert_eq!(register, before);
        if let Some(expected) = intent.retirement.expected() {
            register
                .marks
                .get_mut(&expected.id)
                .unwrap()
                .proposal
                .reason
                .push_str(" changed");
        } else {
            let marked = fixture(true);
            let mark = marked.retirement.expected().unwrap().clone();
            register.marks.insert(mark.id.clone(), mark);
        }
        assert_eq!(
            inspect(&intent, &history, &register).unwrap_err(),
            RepairHold::MarkChanged
        );
    }
}
