// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Explicit copied-package Native restore controls; no root finish authority.
#![cfg(all(test, feature = "full-tests"))]
#![cfg(unix)]

use super::*;
use serde::Deserialize;
use solstone_core_backup::archive_activity::PreparedArchiveOperation;
use solstone_core_backup::archive_admission::{ArchiveBudget, Clock};
use solstone_core_backup::archive_readback::ReadAdmission;
use solstone_core_journal_io::atomic::{
    BoundPublicationPrimitive, run_with_bound_publication_fault,
};
use solstone_core_retention::archive_catalog::codec::Event;
use solstone_core_retention::mark_mutation::{PlannedMarkMutation, RestoredMarkRefusal};
use solstone_core_retention::{MarkId, MarkState, Proposal};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    case: String,
    journal: PathBuf,
    repository_id: String,
    expected_restic_path: PathBuf,
    marks: Vec<Mark>,
    control: PathBuf,
}

struct ProbeClock(i64);
impl Clock for ProbeClock {
    fn now_unix(&self) -> i64 {
        self.0
    }
    fn iso_week(&self) -> u8 {
        1
    }
}

#[cfg(all(test, feature = "full-tests"))]
fn cut(input: &Input, message: &[u8]) {
    let mut stream = UnixStream::connect(&input.control).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream.write_all(message).unwrap();
    let mut reply = [0];
    stream.read_exact(&mut reply).unwrap();
    assert_eq!(reply, [b'a']);
}

fn other_day_duplicate(
    root: &std::path::Path,
    proof: &OffloadProof,
    operation: &solstone_core_retention::archive_catalog::RestoreOperation,
) {
    let mut other = proof.clone();
    other.target.day = "20261008".into();
    other.manifest_id = "native-test-other-manifest".into();
    other.files.truncate(1);
    other.files[0].witness_id = "native-test-other-witness".into();
    other.validate().unwrap();
    let ledger = root.join("health/offload/20261008.jsonl");
    solstone_core_journal_io::append_jsonl(
        &ledger,
        &Event::ArchiveV2 {
            schema_version: 2,
            time: 1,
            day: other.target.day.clone(),
            stream: other.target.stream.clone(),
            segment: solstone_core_format::segment::segment_key(&other.target.dir).unwrap(),
            segment_dir: other.target.dir.clone(),
            layout: other.layout,
            journal_id: other.journal_id.clone(),
            manifest_id: other.manifest_id.clone(),
            repository_id: other.repository_id.clone(),
            snapshot_id: other.snapshot_id.clone(),
            files: other.files.clone(),
        },
    )
    .unwrap();
    // This second-day record is explicitly synthetic hostile history. The first
    // day's publication/event below are actual Native transaction effects.
    solstone_core_journal_io::append_jsonl(
        &ledger,
        &Event::RestoreV2 {
            schema_version: 2,
            time: 2,
            day: other.target.day,
            stream: other.target.stream,
            segment: solstone_core_format::segment::segment_key(&other.target.dir).unwrap(),
            operation_id: operation.operation_id.clone(),
            files: vec![solstone_core_retention::archive_catalog::RestoreFileV2 {
                witness_id: other.files[0].witness_id.clone(),
                name: other.files[0].name.clone(),
                disposition: RestoreDisposition::AlreadyPresent,
            }],
        },
    )
    .unwrap();
}

#[test]
#[ignore = "requires explicit disposable copied package and instrumented repository"]
fn packaged_native_restored_mark_entry() {
    let input: Input = serde_json::from_slice(
        &fs::read(std::env::var_os("NATIVE_RESTORE_PROBE_JSON").unwrap()).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        input.case.as_str(),
        "native_remainder" | "native_already_applied_late_event"
    ));
    assert_eq!(input.marks.len(), 2);
    let root = Arc::new(JournalRoot::open(&input.journal).unwrap());
    let mut proof = input.marks[0].offload.as_ref().unwrap().proof.clone();
    proof
        .files
        .extend(input.marks[1].offload.as_ref().unwrap().proof.files.clone());
    proof.validate().unwrap();
    let names = proof
        .files
        .iter()
        .map(|file| file.name.clone())
        .collect::<Vec<_>>();
    let mark = Mark {
        id: MarkId::derive(RemovalClass::OffloadRawRelease, &proof.target, &names),
        class: RemovalClass::OffloadRawRelease,
        target: proof.target.clone(),
        marked_at: "2026-10-09T12:00:00Z".into(),
        proposal: Proposal {
            names,
            bytes: proof.bytes().unwrap(),
            reason: "synthetic archive fixture".into(),
        },
        state: MarkState::Marked,
        offload: Some(OffloadOffer::new(proof.clone()).unwrap()),
    };
    assert_eq!(
        proof.layout,
        solstone_core_journal_io::paths::SegmentLayout::Direct
    );
    let relative = format!("chronicle/{}/{}", proof.target.day, proof.target.dir);
    fs::create_dir_all(input.journal.join(&relative)).unwrap();
    let archive = crate::ledger::writer::append_archive_witness(&root, &proof, 1).unwrap();
    assert_eq!(archive.confirmation(), LedgerConfirmation::Confirmed);
    let register_path = input.journal.join("health/retention-marks.json");
    let register = Register {
        version: 2,
        marks: std::collections::BTreeMap::from([(mark.id.clone(), mark.clone())]),
    };
    fs::write(&register_path, serde_json::to_vec(&register).unwrap()).unwrap();
    let catalog = read_complete_catalog(&root).unwrap();
    let first = proof.files[0].clone();
    let witness = catalog.active_witness(&proof, &first).unwrap().clone();
    let remaining = vec![proof.files[1].name.clone()];
    let plan = PlannedMarkMutation::remainder(&mark, &remaining).unwrap();
    let expected = Register {
        version: 2,
        marks: std::collections::BTreeMap::from([(
            plan.replacement().unwrap().id.clone(),
            plan.replacement().unwrap().clone(),
        )]),
    };
    let clock = ProbeClock(
        i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap(),
    );
    let prepared = PreparedArchiveOperation::prepare(
        Arc::clone(&root),
        ArchiveBudget::new(&clock).unwrap(),
        &clock,
    )
    .unwrap_or_else(|error| panic!("prepare {error:?}"));
    let destination = prepared.configuration().destination().unwrap();
    let password = prepared.configuration().keys().unwrap().unwrap().daily_key;
    let mut read = ReadOperation::admit(ReadAdmission::bind(
        prepared,
        destination,
        password,
        input.repository_id.clone(),
        input.expected_restic_path.clone(),
    ))
    .unwrap_or_else(|error| panic!("admit {error}"));
    let carrier = Arc::clone(read.carrier());
    cut(&input, b"admitted");
    let segment = iter_segments(root.canonical_path(), PathOrDay::Day(&proof.target.day))
        .unwrap()
        .remove(0);
    let destination = PreparedRestore::admit(Arc::clone(&root), &proof.target.day, &segment)
        .unwrap_or_else(|error| panic!("destination {:?}", error.reason));
    let required = read
        .offload_required_bytes(
            first.bytes,
            review_context::MAX_REGISTER_BYTES as u64 + 4 * 1024 * 1024,
        )
        .unwrap();
    let staged = StagedRestore::read(destination, &mut read, witness, required)
        .unwrap_or_else(|error| panic!("read {:?}", error.hold));
    cut(&input, b"readback");
    let result = if input.case == "native_already_applied_late_event" {
        let (result, fired) = run_with_bound_publication_fault(
            BoundPublicationPrimitive::Write,
            8,
            nix::libc::EIO,
            || staged.commit(2, options()),
        );
        assert!(fired, "post-CAS intent write fault must fire");
        assert!(matches!(result.hold, Some(Hold::StorePublication)));
        assert_eq!(
            result
                .record
                .as_ref()
                .unwrap()
                .persisted
                .as_ref()
                .unwrap()
                .register,
            Obligation::Pending
        );
        result
    } else {
        staged.commit(2, options())
    };
    assert_eq!(
        result.effects,
        AttemptEffects {
            files: 1,
            bytes: first.bytes
        }
    );
    assert_eq!(
        result.publication.as_ref().unwrap().effect,
        PublicationEffect::Published
    );
    assert!(
        matches!(&result.register, Some(Ok(BoundMutationOutcome::Published(value))) if value.status == MutationStatus::Applied)
    );
    assert_eq!(review_context::load_at_root(&root).unwrap(), expected);
    let operation = result.record.as_ref().unwrap().intent.event.clone();
    let raw = input.journal.join(&relative).join(&first.name);
    assert_eq!(
        fs::read(&raw).unwrap(),
        fs::read(&first.archive_path).unwrap()
    );
    let catalog = read_complete_catalog(&root).unwrap();
    assert!(catalog.historical_witness(&proof, &first).is_some());
    assert!(catalog.active_witness(&proof, &first).is_none());
    assert!(catalog.active_witness(&proof, &proof.files[1]).is_some());
    if input.case == "native_remainder" {
        assert!(result.hold.is_none(), "{:?}", result.hold);
        let detached = result.retire_intent(options()).into_owned();
        assert!(detached.completed(), "{:?}", detached.facts().hold);
        assert!(!detached.facts().store_exclusion_retained);
        let census = observe_with_carrier(&root, carrier.observation()).unwrap();
        assert!(census.unresolved_entries().next().is_none());
        println!(
            "NATIVE_RESTORE_CAS_EVIDENCE {}",
            serde_json::json!({"case":input.case,"files":1,"bytes":first.bytes,"event":operation,"retired":true,"carrier":carrier.diagnostic_path(),"parent_refusal":false})
        );
    } else {
        let before = fs::read(&register_path).unwrap();
        let record = result.record.unwrap();
        let id = operation.operation_id.clone();
        let owner = result.owner;
        drop(record);
        let changed_root = input.journal.clone();
        let changed_proof = proof.clone();
        let changed_operation = operation.clone();
        let replayed = with_hook(
            Point::BeforeRestoredCas,
            move || other_day_duplicate(&changed_root, &changed_proof, &changed_operation),
            || replay_local(owner, &id, 3, options()),
        );
        assert!(matches!(replayed.hold, Some(Hold::Register)));
        assert!(matches!(
            replayed.register,
            Some(Err(RestoreRegisterRefusal::Restored(
                RestoredMarkRefusal::Changed
            )))
        ));
        assert_eq!(
            replayed.record.as_ref().unwrap().intent.register,
            Obligation::Pending
        );
        assert_eq!(
            replayed
                .record
                .as_ref()
                .unwrap()
                .persisted
                .as_ref()
                .unwrap()
                .register,
            Obligation::Pending
        );
        assert_eq!(fs::read(&register_path).unwrap(), before);
        assert_eq!(
            fs::read(&raw).unwrap(),
            fs::read(&first.archive_path).unwrap()
        );
        assert_eq!(replayed.effects, AttemptEffects::default());
        let detached = replayed.into_owned();
        assert!(!detached.completed());
        assert!(detached.facts().store_exclusion_retained);
        println!(
            "NATIVE_RESTORE_CAS_EVIDENCE {}",
            serde_json::json!({"case":input.case,"files":1,"bytes":first.bytes,"event":operation,"retired":false,"carrier":carrier.diagnostic_path(),"parent_refusal":true})
        );
    }
}
