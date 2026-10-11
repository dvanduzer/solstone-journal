// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Explicit copied-package Native restore and consuming finish controls.
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

#[cfg(target_os = "linux")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FinishInput {
    mode: String,
    journal: PathBuf,
    restic_path: PathBuf,
    repository_id: String,
    snapshot_id: String,
    archive_path: String,
    bytes: u64,
    sha256: String,
}

#[cfg(target_os = "linux")]
fn finished_native_facts(
    file: crate::restore::owned::RestoreFileFacts<'_>,
    bytes: u64,
    retains_media: bool,
) -> serde_json::Value {
    use crate::restore::owned::FileEvidence;
    use solstone_core_journal_io::media::{OriginalName, RemovalEffect};

    assert_eq!(file.disposition, RestoreDisposition::Published);
    assert_eq!((file.files_restored, file.bytes_restored), (1, bytes));
    let FileEvidence::Transaction(facts) = file.evidence else {
        panic!("actual Native transaction receipt required");
    };
    assert!(facts.completed);
    assert!(facts.hold.is_none());
    assert!(!facts.store_exclusion_retained);
    assert_eq!(facts.retained_media, retains_media);
    assert!(facts.register.is_none());
    let publication = facts.publication.unwrap();
    assert_eq!(publication.effect, PublicationEffect::Published);
    assert_eq!(publication.confirmation, Confirmation::Confirmed);
    assert_eq!(publication.file_durability, Durability::Synced);
    assert_eq!(publication.parent_durability, Durability::Synced);
    assert!(publication.identity.is_some() && publication.error.is_none());
    assert!(facts.intent_writes.iter().all(|write| {
        write.named_read_confirmed && write.durability == [Some(Durability::Synced); 4]
    }));
    assert!(facts.marker.as_ref().unwrap().is_ok());
    assert_eq!(facts.marker_ancestor_sync.len(), 3);
    assert!(facts.marker_ancestor_sync.iter().all(Result::is_ok));
    let ledger = facts.ledger.unwrap();
    assert!(ledger.confirmed && ledger.durability == [true; 4]);
    assert!(ledger.failures.is_empty());
    let retired = facts.retirement.unwrap();
    assert!(retired.retired && !retired.retained_stage);
    assert_eq!(retired.ancestors, [Durability::Synced; 4]);
    assert!(retired.failures.is_empty());
    let removal = retired.removal.unwrap();
    assert_eq!(removal.effect, RemovalEffect::Deleted);
    assert_eq!(removal.original_name, OriginalName::Absent);
    assert_eq!(removal.claim_confirmation, Confirmation::Confirmed);
    assert_eq!(removal.cleanup, Cleanup::Complete);
    assert!(removal.failures.is_empty());
    let cleanup = facts.settlement_cleanup.unwrap();
    assert_eq!(cleanup.cleanup, Cleanup::Complete);
    assert!(cleanup.parent_sync.unwrap().is_ok());
    serde_json::json!({
        "disposition":"Published", "files":1, "bytes":bytes,
        "operation_id":facts.operation_id.unwrap(),
        "publication":"Published/Synced/Synced/Confirmed",
        "event_confirmed":true, "intent_retired":true,
        "removal":"Deleted/Absent/Confirmed/Complete",
        "settlement_cleanup":"Complete/Synced"
    })
}

#[cfg(all(test, feature = "full-tests", target_os = "linux"))]
#[test]
#[ignore = "requires a copied current package and synthetic direct Native finish fixture"]
fn packaged_native_restore_finish_entry() {
    use crate::restore::owned::{
        RestoreFinishRefusal, restore_owned_bound_day, restore_owned_bound_day_with_recovery,
    };
    use solstone_core_backup_runtime::{
        DirectReadFinishState, UreqHttpTransport, admit_prepared_archive_owned_read_installed,
    };
    use solstone_core_journal_io::activity::ActivityAdmissionError;
    use solstone_core_retention::archive_proof::AcceptedProcessingObservation;
    use std::time::Instant;

    use solstone_core_backup::archive_activity::OwnedPreparedArchiveOperation;

    struct OwnedClock(Instant);
    impl Clock for OwnedClock {
        fn now_unix(&self) -> i64 {
            10_000 + self.0.elapsed().as_secs() as i64
        }
        fn iso_week(&self) -> u8 {
            1
        }
    }
    let input: FinishInput = serde_json::from_slice(
        &fs::read(std::env::var_os("NATIVE_FINISH_PROBE_JSON").unwrap()).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        input.mode.as_str(),
        "complete" | "late_destination" | "recovery_complete" | "recovery_drift"
    ));
    let root = Arc::new(JournalRoot::open(&input.journal).unwrap());
    // Synthetic historical metadata backed by the fixture's actual snapshot,
    // full LS path and bytes. The public entry still performs real exact dump.
    let proof = OffloadProof {
        journal_id: root.identity().observation_id(),
        manifest_id: "native-finish-manifest".into(),
        target: solstone_core_retention::Target {
            day: "20261009".into(),
            stream: "_default".into(),
            dir: "120000_60_fixture".into(),
        },
        layout: solstone_core_journal_io::paths::SegmentLayout::Direct,
        repository_id: input.repository_id.clone(),
        snapshot_id: input.snapshot_id.clone(),
        files: vec![ArchiveFileProof {
            witness_id: "native-finish-witness".into(),
            name: "audio.flac".into(),
            bytes: input.bytes,
            sha256: input.sha256.clone(),
            archive_path: input.archive_path.clone(),
            processing: AcceptedProcessingObservation {
                record: None,
                has_analysis_row: true,
            },
        }],
    };
    proof.validate().unwrap();
    let segment = input.journal.join("chronicle/20261009/120000_60_fixture");
    fs::create_dir_all(&segment).unwrap();
    let ledger = crate::ledger::writer::append_archive_witness(&root, &proof, 1).unwrap();
    assert_eq!(ledger.confirmation(), LedgerConfirmation::Confirmed);
    let clock: Arc<dyn Clock + Send + Sync> = Arc::new(OwnedClock(Instant::now()));
    let prepared = OwnedPreparedArchiveOperation::prepare(
        Arc::clone(&root),
        ArchiveBudget::new(clock.as_ref()).unwrap(),
        Arc::clone(&clock),
    )
    .unwrap_or_else(|_| panic!("typed prepared fixture admission failed"));
    let lease = Arc::downgrade(prepared.activity());
    let carrier = Arc::downgrade(prepared.carrier());
    let installed = admit_prepared_archive_owned_read_installed(
        prepared,
        input.repository_id.clone(),
        &input.restic_path,
        "2.0.40",
        &UreqHttpTransport,
    )
    .unwrap_or_else(|_| panic!("typed installed fixture admission failed"));
    let (read, finish) = installed.into_parts();
    assert!(Arc::ptr_eq(read.carrier(), finish.carrier()));
    assert!(Arc::ptr_eq(read.owned_clock(), &clock));
    assert!(Arc::ptr_eq(finish.owned_clock(), &clock));
    let initial_checked = std::rc::Rc::new(std::cell::Cell::new(false));
    let outcome = if input.mode.starts_with("recovery_") {
        let binding = finish
            .observe_recovery_binding(&read)
            .unwrap_or_else(|error| panic!("actual recovery observer refused: {error:?}"));
        binding.revalidate_for_read(&read).unwrap();
        let expected = binding.data().clone();
        let check_root = input.journal.clone();
        let checked = std::rc::Rc::clone(&initial_checked);
        let drift = input.mode == "recovery_drift";
        with_hook(
            Point::AfterInitialIntent,
            move || {
                let directory = check_root.join("health/offload/restore-intents");
                let paths: Vec<_> = fs::read_dir(directory)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .collect();
                assert_eq!(paths.len(), 1);
                let original = RestoreIntent::decode(Ok(&fs::read(&paths[0]).unwrap())).unwrap();
                assert_eq!(original.recovery_binding.as_ref(), Some(&expected));
                assert!(original.outcome.is_none());
                assert_eq!(
                    original.event.files[0].disposition,
                    RestoreDisposition::Unattempted
                );
                assert!(
                    !check_root
                        .join("chronicle/20261009/120000_60_fixture/audio.flac")
                        .exists()
                );
                checked.set(true);
                if drift {
                    fs::write(check_root.join("backup-activity.lock"), b"changed stamp").unwrap();
                }
            },
            || restore_owned_bound_day_with_recovery(read, binding, &proof.target.day, 2),
        )
    } else {
        restore_owned_bound_day(read, &proof.target.day, 2)
    };
    if input.mode == "recovery_drift" {
        assert!(initial_checked.get());
        assert_eq!(outcome.report().files_restored, 0);
        assert_eq!(outcome.report().bytes_restored, 0);
        assert!(!segment.join("audio.flac").exists());
        let file = outcome.files().next().unwrap();
        assert_eq!(file.disposition, RestoreDisposition::Unattempted);
        let crate::restore::owned::FileEvidence::Transaction(facts) = file.evidence else {
            panic!("durable intent evidence lost");
        };
        assert!(facts.publication.is_none());
        assert!(
            facts
                .intent_writes
                .iter()
                .any(|write| write.named_read_confirmed)
        );
        assert!(facts.store_exclusion_retained && facts.retained_media && facts.hold.is_some());
        let held = outcome.into_owned_hold();
        let unknown = finish.into_unknown();
        let custody = std::thread::spawn(move || (held, unknown)).join().unwrap();
        assert!(std::ptr::eq(
            Arc::as_ptr(custody.0.carrier()),
            carrier.as_ptr()
        ));
        assert_eq!(custody.1.state(), DirectReadFinishState::Unknown);
        assert!(custody.1.retirement().is_none());
        assert!(lease.upgrade().is_some());
        assert!(matches!(
            ActivityLease::try_acquire(Arc::clone(&root)),
            Err(ActivityAdmissionError::Busy)
        ));
        println!(
            "NATIVE_FINISH_RESULT {}",
            serde_json::json!({"mode":input.mode,"result":{
            "state":"NativeHeld","initial_binding_confirmed":true,"published_bytes":0,
            "closed_transfer":true,"same_original_carrier":true,"held_lease_busy":true,"runtime":"Unknown","retirement":false}})
        );
        return;
    }
    assert_eq!(outcome.report().status, "ok", "{:?}", outcome.report());
    assert_eq!(outcome.report().files_restored, 1);
    assert_eq!(outcome.report().bytes_restored, input.bytes);
    assert_eq!(outcome.files().len(), 1);
    assert_eq!(
        outcome.files().next().unwrap().disposition,
        RestoreDisposition::Published
    );
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(fs::read(segment.join("audio.flac")).unwrap())
        ),
        input.sha256
    );
    let result = if matches!(input.mode.as_str(), "complete" | "recovery_complete") {
        let (settled, report) = match outcome.settle_direct() {
            Ok(settled) => settled,
            Err(_) => panic!("genuine Native settlement remained held"),
        };
        let facts = finished_native_facts(report.files().next().unwrap(), input.bytes, false);
        let completed = finish.finish_owned(settled);
        assert_eq!(completed.state(), DirectReadFinishState::Completed);
        assert!(Arc::ptr_eq(completed.owned_clock(), &clock));
        let retirement = completed.retirement().unwrap();
        assert!(retirement.receipt.failures().is_empty());
        let retirement = format!(
            "{:?}/{:?}/{:?}",
            retirement.receipt.effect(),
            retirement.receipt.root_durability(),
            retirement.receipt.confirmation()
        );
        assert!(matches!(
            ActivityLease::try_acquire(Arc::clone(&root)),
            Err(ActivityAdmissionError::Busy)
        ));
        assert!(
            OwnedPreparedArchiveOperation::prepare(
                Arc::clone(&root),
                ArchiveBudget::new(clock.as_ref()).unwrap(),
                Arc::clone(&clock)
            )
            .is_err()
        );
        drop(completed);
        assert!(
            lease.upgrade().is_none(),
            "settled report must retain no activity lease"
        );
        assert!(
            carrier.upgrade().is_none(),
            "settled report must retain no carrier"
        );
        let next = OwnedPreparedArchiveOperation::prepare(
            Arc::clone(&root),
            ArchiveBudget::new(clock.as_ref()).unwrap(),
            Arc::clone(&clock),
        )
        .unwrap_or_else(|_| panic!("typed next fixture admission failed"));
        let second = admit_prepared_archive_owned_read_installed(
            next,
            input.repository_id.clone(),
            &input.restic_path,
            "2.0.40",
            &UreqHttpTransport,
        )
        .unwrap_or_else(|_| panic!("typed second fixture admission failed"));
        let (second_read, second_finish) = second.into_parts();
        let second_read = match second_read.settle_direct() {
            Ok(value) => value,
            Err(_) => panic!("fresh installed read must settle"),
        };
        let second = second_finish.finish_owned(second_read);
        assert_eq!(second.state(), DirectReadFinishState::Completed);
        drop(second);
        assert_eq!(
            finished_native_facts(report.files().next().unwrap(), input.bytes, false),
            facts
        );
        assert_eq!(report.report().bytes_restored, input.bytes);
        serde_json::json!({"state":"Completed","facts":facts,"retirement":retirement,
            "held_lease_busy":true,"next_installed_admission":true,"next_completed":true,
            "settled_report_retained":true,"initial_binding_confirmed":initial_checked.get()})
    } else {
        let tombstone = segment.join("tombstone.json");
        let (held, fired) = run_with_media_hook(
            MediaPrimitive::ParentSync,
            move || fs::write(tombstone, b"{}"),
            || outcome.settle_direct(),
        );
        assert!(fired);
        let held = match held {
            Err(value) => value,
            Ok(_) => panic!("late owner tombstone must refuse Native settlement"),
        };
        assert!(matches!(
            held.refusal(),
            RestoreFinishRefusal::Native {
                file: Some(0),
                reason: "settlement_destination_changed",
                ..
            }
        ));
        assert_eq!(
            held.report().status,
            "ok",
            "finish refusal is independent of prior report"
        );
        let before = finished_native_facts(held.files().next().unwrap(), input.bytes, true);
        // Already closed by the typed Native return; no generic conversion exists here.
        let held = *held;
        assert!(std::ptr::eq(Arc::as_ptr(held.carrier()), carrier.as_ptr()));
        let unknown = finish.into_unknown();
        let custody = std::thread::spawn(move || (held, unknown)).join().unwrap();
        assert!(matches!(
            custody.0.refusal(),
            RestoreFinishRefusal::Native {
                file: Some(0),
                reason: "settlement_destination_changed",
                ..
            }
        ));
        assert_eq!(
            finished_native_facts(custody.0.files().next().unwrap(), input.bytes, true),
            before
        );
        assert_eq!(custody.1.state(), DirectReadFinishState::Unknown);
        assert!(custody.1.retirement().is_none());
        assert!(Arc::ptr_eq(custody.1.owned_clock(), &clock));
        assert!(Arc::ptr_eq(custody.1.journal_root(), &root));
        assert!(std::ptr::eq(
            Arc::as_ptr(custody.0.carrier()),
            carrier.as_ptr()
        ));
        assert!(lease.upgrade().is_some());
        assert!(matches!(
            ActivityLease::try_acquire(Arc::clone(&root)),
            Err(ActivityAdmissionError::Busy)
        ));
        assert!(
            OwnedPreparedArchiveOperation::prepare(
                Arc::clone(&root),
                ArchiveBudget::new(clock.as_ref()).unwrap(),
                Arc::clone(&clock)
            )
            .is_err()
        );
        assert_eq!(fs::read(segment.join("tombstone.json")).unwrap(), b"{}");
        serde_json::json!({"state":"NativeHeld","reason":"settlement_destination_changed",
            "facts":before,"closed_transfer":true,"runtime":"Unknown",
            "same_original_carrier":true,"same_runtime_clock_root":true,
            "held_lease_busy":true,"retirement":false})
    };
    println!(
        "NATIVE_FINISH_RESULT {}",
        serde_json::json!({"mode":input.mode,"result":result})
    );
}

#[cfg(all(test, feature = "full-tests", target_os = "linux"))]
mod recovered;
