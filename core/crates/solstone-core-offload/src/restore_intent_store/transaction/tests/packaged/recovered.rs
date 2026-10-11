// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Genuine preboot publication and immutable-record recovered-CAS controls.
#![cfg(all(test, feature = "full-tests", target_os = "linux"))]

use super::*;
use crate::restore_recovered::{RecoveredRestoreState, recover_restored_register};
use solstone_core_backup::archive_activity::OwnedPreparedArchiveOperation;
use solstone_core_backup_runtime::{
    UreqHttpTransport, admit_prepared_archive_owned_read_installed,
};
use solstone_core_journal_io::activity::ActivityAdmissionError;
use solstone_core_journal_io::activity_obligations::intent_filename;
use solstone_core_journal_io::archive_recovery::RecoveredArchiveReservation;
use solstone_core_journal_io::atomic::run_with_bound_publication_barrier;
use solstone_core_journal_io::flat_directory::FlatDirectory;
use solstone_core_journal_io::locking::acquire_existing_parent_lock_bound;
use solstone_core_retention::archive_proof::AcceptedProcessingObservation;
use solstone_core_retention::mark_mutation::RecoveredRestoredMarkRefusal;
use std::ffi::OsStr;
use std::rc::Rc;
use std::time::Instant;

struct OwnedClock(Instant);
impl Clock for OwnedClock {
    fn now_unix(&self) -> i64 {
        10_000 + self.0.elapsed().as_secs() as i64
    }
    fn iso_week(&self) -> u8 {
        1
    }
}

#[test]
#[ignore = "requires a disposable copied installed package before a real guest reboot"]
fn packaged_native_recovery_preboot_entry() {
    let input: FinishInput = serde_json::from_slice(
        &fs::read(std::env::var_os("NATIVE_FINISH_PROBE_JSON").unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(input.mode, "recovery_preboot");
    let root = Arc::new(JournalRoot::open(&input.journal).unwrap());
    let proof = OffloadProof {
        journal_id: root.identity().observation_id(),
        manifest_id: "native-recovered-manifest".into(),
        target: solstone_core_retention::Target {
            day: "20261009".into(),
            stream: "_default".into(),
            dir: "120000_60_fixture".into(),
        },
        layout: solstone_core_journal_io::paths::SegmentLayout::Direct,
        repository_id: input.repository_id.clone(),
        snapshot_id: input.snapshot_id.clone(),
        files: vec![ArchiveFileProof {
            witness_id: "native-recovered-witness".into(),
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
    let relative = format!("chronicle/{}/{}", proof.target.day, proof.target.dir);
    fs::create_dir_all(input.journal.join(&relative)).unwrap();
    let archived = crate::ledger::writer::append_archive_witness(&root, &proof, 1).unwrap();
    assert_eq!(archived.confirmation(), LedgerConfirmation::Confirmed);
    assert_eq!(archived.durability(), [LedgerSync::Confirmed; 4]);
    // Synthetic complete typed Mark metadata; publication/readback/event below
    // are real effects by the original installed operation and Native owner.
    let names = vec!["audio.flac".into()];
    let mark = Mark {
        id: MarkId::derive(RemovalClass::OffloadRawRelease, &proof.target, &names),
        class: RemovalClass::OffloadRawRelease,
        target: proof.target.clone(),
        marked_at: "2026-10-09T12:00:00Z".into(),
        proposal: Proposal {
            names,
            bytes: input.bytes,
            reason: "synthetic recovery fixture".into(),
        },
        state: MarkState::Marked,
        offload: Some(OffloadOffer::new(proof.clone()).unwrap()),
    };
    let register = Register {
        version: 2,
        marks: std::collections::BTreeMap::from([(mark.id.clone(), mark)]),
    };
    let register_path = input.journal.join("health/retention-marks.json");
    fs::write(&register_path, serde_json::to_vec(&register).unwrap()).unwrap();
    let original_register = fs::read(&register_path).unwrap();
    let clock: Arc<dyn Clock + Send + Sync> = Arc::new(OwnedClock(Instant::now()));
    let prepared = OwnedPreparedArchiveOperation::prepare(
        Arc::clone(&root),
        ArchiveBudget::new(clock.as_ref()).unwrap(),
        Arc::clone(&clock),
    )
    .unwrap_or_else(|_| panic!("original owned preparation refused"));
    let installed = admit_prepared_archive_owned_read_installed(
        prepared,
        input.repository_id.clone(),
        &input.restic_path,
        "2.0.40",
        &UreqHttpTransport,
    )
    .unwrap_or_else(|_| panic!("actual installed admission refused"));
    let (mut read, finish) = installed.into_parts();
    let binding = finish.observe_recovery_binding(&read).unwrap();
    binding.revalidate_for_read(&read).unwrap();
    let segment = iter_segments(root.canonical_path(), PathOrDay::Day(&proof.target.day))
        .unwrap()
        .remove(0);
    let destination = PreparedRestore::admit(Arc::clone(&root), &proof.target.day, &segment)
        .unwrap_or_else(|error| panic!("destination {:?}", error.reason));
    let witness = read_complete_catalog(&root)
        .unwrap()
        .active_witness(&proof, &proof.files[0])
        .unwrap()
        .clone();
    let required = read
        .as_read()
        .offload_required_bytes(
            input.bytes,
            review_context::MAX_REGISTER_BYTES as u64 + 4 * 1024 * 1024,
        )
        .unwrap();
    let staged = StagedRestore::read_with_recovery(
        destination,
        &mut read,
        witness,
        required,
        Some(&binding),
    )
    .unwrap_or_else(|error| panic!("actual readback {:?}", error.hold));
    let locked = Rc::new(RefCell::new(None));
    let held = Rc::clone(&locked);
    let lock_root = Arc::clone(&root);
    let mut result = with_hook(
        Point::BeforeRestoredCas,
        move || {
            let parent = FlatDirectory::open(&lock_root, std::path::Path::new("health")).unwrap();
            *held.borrow_mut() = Some(
                acquire_existing_parent_lock_bound(
                    &parent,
                    OsStr::new("retention-marks.json.lock"),
                    Duration::ZERO,
                    Duration::ZERO,
                )
                .unwrap(),
            );
        },
        || staged.commit(2, options()),
    );
    assert!(locked.borrow().is_some());
    assert!(matches!(result.hold, Some(Hold::Register)));
    assert!(result.register.as_ref().unwrap().is_err());
    assert_eq!(fs::read(&register_path).unwrap(), original_register);
    assert_eq!(
        result.effects,
        AttemptEffects {
            files: 1,
            bytes: input.bytes
        }
    );
    let publication = result.publication.as_ref().unwrap();
    assert_eq!(publication.effect, PublicationEffect::Published);
    assert_eq!(publication.confirmation, Confirmation::Confirmed);
    assert_eq!(publication.file_durability, Durability::Synced);
    assert_eq!(publication.parent_durability, Durability::Synced);
    assert!(result.marker.as_ref().unwrap().is_ok());
    assert!(result.marker_ancestor_sync.iter().all(Result::is_ok));
    assert_eq!(
        result
            .ledger
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .confirmation(),
        LedgerConfirmation::Confirmed
    );
    assert_eq!(
        result
            .ledger
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .durability(),
        [LedgerSync::Confirmed; 4]
    );
    let cleanup = result.owner.media.as_mut().unwrap().cleanup();
    assert_eq!(cleanup, Cleanup::Complete);
    let parent_sync = result.owner.destination.stage_parent().sync();
    assert!(parent_sync.is_ok());
    let record = result.record.as_ref().unwrap();
    assert_eq!(record.persisted.as_ref(), Some(&record.intent));
    assert_eq!(record.intent.marker, Obligation::Complete);
    assert_eq!(record.intent.event_recorded, Obligation::Complete);
    assert_eq!(record.intent.register, Obligation::Pending);
    assert_eq!(
        record.intent.recovery_binding.as_ref(),
        Some(binding.data())
    );
    let last = record.writes.last().unwrap();
    assert!(last.fully_confirmed());
    let actual = record
        .store
        .as_ref()
        .unwrap()
        .read(&record.intent.event.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(&actual.read, last.confirmation.as_ref().unwrap());
    assert_eq!(actual.intent, record.intent);
    let raw = input.journal.join(&relative).join("audio.flac");
    assert_eq!(
        format!("{:x}", Sha256::digest(fs::read(raw).unwrap())),
        input.sha256
    );
    let census = observe_with_carrier(&root, result.owner.carrier.observation()).unwrap();
    assert_eq!(census.unresolved_entries().count(), 1);
    assert!(matches!(
        ActivityLease::try_acquire(Arc::clone(&root)),
        Err(ActivityAdmissionError::Busy)
    ));
    let checkpoint = serde_json::json!({
        "journal":input.journal, "operation_id":record.intent.event.operation_id,
        "record_name":actual.read.entry.name.to_str().unwrap(),
        "record_sha256":format!("{:x}",Sha256::digest(&actual.read.bytes)),
        "record_identity":actual.read.entry.identity,
        "record_bytes":actual.read.bytes.len(), "binding":binding.data(),
        "publication":"Published/Synced/Synced/Confirmed", "published_bytes":input.bytes,
        "media_sha256":input.sha256, "marker_complete":true,"event_complete":true,
        "register_pending":true,"register_unchanged":true,
        "intent_last_write_confirmed":true,"intent_last_write_syncs":4,
        "last_confirmed_named_read_equal":true,"owned_stage_cleanup":"Complete",
        "stage_parent_sync":"Synced","unresolved_intents":1,
        "held_lease_busy":true,"synthetic_completed_postpublication_coordinate":true,
    });
    fs::write(
        std::env::var_os("NATIVE_RECOVERY_CHECKPOINT").unwrap(),
        serde_json::to_vec_pretty(&checkpoint).unwrap(),
    )
    .unwrap();
    println!("NATIVE_RECOVERY_PREBOOT {}", checkpoint);
    // Drop closes original handles only. No settlement, saved-ID cleanup,
    // intent rewrite/removal or root retirement is performed at process exit.
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryInput {
    mode: String,
    journal: PathBuf,
    checkpoint: PathBuf,
}

#[test]
#[ignore = "requires an actual original completed intent after a disposable guest reboot"]
fn packaged_native_recovered_register_entry() {
    let input: RecoveryInput = serde_json::from_slice(
        &fs::read(std::env::var_os("NATIVE_RECOVERY_PROBE_JSON").unwrap()).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        input.mode.as_str(),
        "complete" | "changed_mark" | "postmutation" | "empty_remainder_missing"
    ));
    let checkpoint: serde_json::Value =
        serde_json::from_slice(&fs::read(&input.checkpoint).unwrap()).unwrap();
    let operation = checkpoint["operation_id"].as_str().unwrap();
    let root = Arc::new(JournalRoot::open(&input.journal).unwrap());
    let directory = MediaDirectory::open(
        Arc::clone(&root),
        std::path::Path::new("health/offload/restore-intents"),
    )
    .unwrap();
    let named = directory
        .read_file_bounded(
            &intent_filename(operation).unwrap(),
            solstone_core_journal_io::activity_obligations::MAX_INTENT_BYTES,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(&named.bytes)),
        checkpoint["record_sha256"].as_str().unwrap()
    );
    let intent = RestoreIntent::decode(Ok(&named.bytes)).unwrap();
    let binding = intent.recovery_binding.clone().unwrap();
    let reservation = RecoveredArchiveReservation::admit(Arc::clone(&root), operation, binding)
        .unwrap_or_else(|failure| panic!("actual recovered admission {:?}", failure.error()));
    assert_eq!(reservation.record(), &named);
    let register_path = input.journal.join("health/retention-marks.json");
    if input.mode == "changed_mark" {
        let mut changed = review_context::load_at_root(&root).unwrap();
        changed.marks.values_mut().next().unwrap().marked_at = "2026-10-09T12:00:01Z".into();
        fs::write(&register_path, serde_json::to_vec(&changed).unwrap()).unwrap();
    }
    let before_register = fs::read(&register_path).unwrap();
    let (outcome, barrier) = if input.mode == "postmutation" {
        let unrelated = input
            .journal
            .join(".tmp-offload-11111111111111111111111111111111");
        run_with_bound_publication_barrier(
            BoundPublicationPrimitive::ParentSync,
            1,
            move || fs::create_dir(unrelated).unwrap(),
            || recover_restored_register(reservation, options()),
        )
    } else {
        (recover_restored_register(reservation, options()), false)
    };
    eprintln!(
        "NATIVE_RECOVERY_DIAGNOSTIC mode={} state={:?} refusal={:?} attempted={} register={:?} populations_ok={:?}",
        input.mode,
        outcome.state(),
        outcome.refusal(),
        outcome.register_attempted(),
        outcome.register(),
        outcome
            .populations()
            .iter()
            .map(Result::is_ok)
            .collect::<Vec<_>>()
    );
    assert_eq!(outcome.files_restored(), 0);
    assert_eq!(outcome.bytes_restored(), 0);
    assert_eq!(outcome.reservation().record(), &named);
    assert_eq!(outcome.reservation().read_current_record().unwrap(), named);
    assert!(matches!(
        ActivityLease::try_acquire(Arc::clone(&root)),
        Err(ActivityAdmissionError::Busy)
    ));
    let current = fs::read(&register_path).unwrap();
    match input.mode.as_str() {
        "complete" => {
            assert_eq!(outcome.state(), RecoveredRestoreState::RegisterConfirmed);
            assert_eq!(outcome.disposition(), RestoreDisposition::AlreadyPresent);
            assert!(
                matches!(outcome.register(), Some(Ok(BoundMutationOutcome::Published(value))) if value.status == MutationStatus::Applied)
            );
            assert!(
                review_context::load_at_root(&root)
                    .unwrap()
                    .marks
                    .is_empty()
            );
            assert_ne!(current, before_register);
        }
        "empty_remainder_missing" => {
            // No replacement means generic absence remains Missing. A later
            // admission has no opaque prior Applied result and stays held.
            assert_eq!(outcome.state(), RecoveredRestoreState::Held);
            assert!(matches!(
                outcome.refusal(),
                Some(crate::restore_recovered::RecoveredRestoreRefusal::Register)
            ));
            assert!(
                matches!(outcome.register(), Some(Ok(BoundMutationOutcome::Unchanged(value))) if value.status == MutationStatus::Missing)
            );
            assert_eq!(current, before_register);
        }
        "changed_mark" => {
            assert_eq!(outcome.state(), RecoveredRestoreState::Held);
            assert_eq!(current, before_register);
        }
        "postmutation" => {
            assert!(barrier);
            assert_eq!(outcome.state(), RecoveredRestoreState::Held);
            assert!(
                matches!(outcome.register(), Some(Err(RecoveredRestoredMarkRefusal::PostMutation { outcome, .. })) if matches!(outcome.as_ref(), BoundMutationOutcome::Published(value) if value.status == MutationStatus::Applied))
            );
            assert!(
                review_context::load_at_root(&root)
                    .unwrap()
                    .marks
                    .is_empty()
            );
            assert_ne!(current, before_register);
        }
        _ => unreachable!(),
    }
    let report = serde_json::json!({"mode":input.mode,"state":format!("{:?}",outcome.state()),
        "bytes_restored":0,"files_restored":0,"original_record_unchanged":true,
        "held_lease_busy":true,"register_attempted":outcome.register_attempted(),
        "register_sha256":format!("{:x}",Sha256::digest(&current)),"postmutation":barrier,
        "intent_retired":false,"root_retired":false});
    let custody = std::thread::spawn(move || outcome).join().unwrap();
    assert!(matches!(
        ActivityLease::try_acquire(Arc::clone(&root)),
        Err(ActivityAdmissionError::Busy)
    ));
    assert_eq!(custody.reservation().read_current_record().unwrap(), named);
    println!("NATIVE_RECOVERY_RESULT {}", report);
}

#[test]
#[ignore = "requires a fresh actual original completed intent after a disposable guest reboot"]
fn packaged_native_recovered_retirement_entry() {
    use crate::restore_intent_store::recovered::tests::{self as faults, Checkpoint};
    use crate::restore_recovered::retirement::{
        RecoveredRetirementState, retire_recovered_restore,
    };
    use solstone_core_journal_io::activity_obligations::observe_obligations;
    use solstone_core_journal_io::media::{MediaPrimitive, RemovalEffect, run_with_media_hook};

    let input: RecoveryInput = serde_json::from_slice(
        &fs::read(std::env::var_os("NATIVE_RECOVERY_PROBE_JSON").unwrap()).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        input.mode.as_str(),
        "retire_complete" | "retire_replace" | "retire_sync" | "retire_unrelated" | "retire_unwind"
    ));
    let checkpoint: serde_json::Value =
        serde_json::from_slice(&fs::read(&input.checkpoint).unwrap()).unwrap();
    let operation = checkpoint["operation_id"].as_str().unwrap();
    let root = Arc::new(JournalRoot::open(&input.journal).unwrap());
    let directory = MediaDirectory::open(
        Arc::clone(&root),
        std::path::Path::new("health/offload/restore-intents"),
    )
    .unwrap();
    let name = intent_filename(operation).unwrap();
    let named = directory
        .read_file_bounded(
            &name,
            solstone_core_journal_io::activity_obligations::MAX_INTENT_BYTES,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(&named.bytes)),
        checkpoint["record_sha256"].as_str().unwrap()
    );
    let intent = RestoreIntent::decode(Ok(&named.bytes)).unwrap();
    let raw_path = input
        .journal
        .join(intent.locator.relative_segment())
        .join(selected_name(&intent));
    let ledger_path = input
        .journal
        .join("health/offload")
        .join(format!("{}.jsonl", intent.locator.target.day));
    let raw_before = fs::read(&raw_path).unwrap();
    let ledger_before = fs::read(&ledger_path).unwrap();
    let reservation = RecoveredArchiveReservation::admit(
        Arc::clone(&root),
        operation,
        intent.recovery_binding.clone().unwrap(),
    )
    .unwrap_or_else(|failure| panic!("actual recovered admission {:?}", failure.error()));
    let carrier_path = input.journal.join(reservation.binding().reservation_name());
    let prior = recover_restored_register(reservation, options());
    assert_eq!(prior.state(), RecoveredRestoreState::RegisterConfirmed);
    assert!(
        matches!(prior.register(), Some(Ok(BoundMutationOutcome::Published(value))) if value.status == MutationStatus::Applied)
    );
    let register_after_applied =
        fs::read(input.journal.join("health/retention-marks.json")).unwrap();
    let (outcome, hook) = match input.mode.as_str() {
        "retire_replace" => {
            let path = input
                .journal
                .join("health/offload/restore-intents")
                .join(&name);
            let saved = input.journal.join("health/offload/saved-successor");
            faults::with_hook(
                Checkpoint::SuccessorReturned,
                move || {
                    let bytes = fs::read(&path).unwrap();
                    fs::rename(&path, saved).unwrap();
                    fs::write(&path, bytes).unwrap();
                },
                || retire_recovered_restore(prior, options()),
            )
        }
        "retire_sync" => {
            faults::with_hook(Checkpoint::AfterRemoval, faults::fail_next_sync, || {
                retire_recovered_restore(prior, options())
            })
        }
        "retire_unrelated" => {
            let unrelated = input
                .journal
                .join(".tmp-offload-22222222222222222222222222222222");
            faults::with_hook(
                Checkpoint::AfterRemoval,
                move || fs::create_dir(unrelated).unwrap(),
                || retire_recovered_restore(prior, options()),
            )
        }
        "retire_unwind" => run_with_media_hook(
            MediaPrimitive::CarrierRetirementRootSync,
            || panic!("injected INSIDE actual consumed root retirement AFTER native rmdir"),
            || retire_recovered_restore(prior, options()),
        ),
        _ => (retire_recovered_restore(prior, options()), false),
    };
    eprintln!(
        "NATIVE_RETIREMENT_DIAGNOSTIC mode={} state={:?} refusal={:?} repeated={:?} successor={} removal={:?} root_attempted={} root_returned={} store_failures={:?}",
        input.mode,
        outcome.state(),
        outcome.refusal(),
        outcome.register(),
        outcome.successor_confirmation().is_some(),
        outcome.metadata_removal().map(|removal| removal.receipt()),
        outcome.root_retirement_attempted(),
        outcome.root_retirement().is_some(),
        outcome.store_io_failures().collect::<Vec<_>>()
    );
    assert_eq!(outcome.files_restored(), 0);
    assert_eq!(outcome.bytes_restored(), 0);
    assert_eq!(outcome.original_record(), &named);
    assert!(
        matches!(outcome.prior_register(), Some(Ok(BoundMutationOutcome::Published(value))) if value.status == MutationStatus::Applied)
    );
    assert!(
        matches!(outcome.register(), Some(Ok(BoundMutationOutcome::Unchanged(value))) if value.status == MutationStatus::Missing)
    );
    assert_eq!(
        outcome.successor_durability(),
        Some(&[Some(Durability::Synced); 4])
    );
    assert_eq!(outcome.successor().unwrap().register, Obligation::Complete);
    assert_eq!(outcome.successor().unwrap().cleanup, Obligation::Complete);
    assert_eq!(fs::read(&raw_path).unwrap(), raw_before);
    assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);
    assert_eq!(
        fs::read(input.journal.join("health/retention-marks.json")).unwrap(),
        register_after_applied
    );
    assert!(matches!(
        ActivityLease::try_acquire(Arc::clone(&root)),
        Err(ActivityAdmissionError::Busy)
    ));
    match input.mode.as_str() {
        "retire_complete" => {
            assert_eq!(outcome.state(), RecoveredRetirementState::Finished);
            assert_eq!(outcome.disposition(), RestoreDisposition::AlreadyPresent);
            assert!(outcome.root_retirement().unwrap().is_complete());
            assert!(
                outcome
                    .root_retirement()
                    .unwrap()
                    .final_census()
                    .unwrap()
                    .entries()
                    .is_empty()
            );
            assert!(!carrier_path.exists());
            assert_eq!(outcome.metadata_named_absence(), Some(true));
            assert!(observe_obligations(&root).unwrap().entries().is_empty());
        }
        "retire_replace" => {
            assert!(hook);
            assert_eq!(outcome.state(), RecoveredRetirementState::Held);
            let replacement = outcome.named_reads().last().unwrap();
            assert_eq!(
                replacement.bytes,
                outcome.successor_confirmation().unwrap().bytes
            );
            assert_ne!(
                replacement.entry,
                outcome.successor_confirmation().unwrap().entry
            );
            assert!(outcome.metadata_removal().is_none());
            assert!(!outcome.root_retirement_attempted());
            assert!(carrier_path.is_dir());
        }
        "retire_sync" => {
            assert!(hook);
            assert_eq!(outcome.state(), RecoveredRetirementState::Held);
            assert_eq!(
                outcome.metadata_removal().unwrap().receipt().effect,
                RemovalEffect::Deleted
            );
            assert_eq!(
                outcome.metadata_removal().unwrap().receipt().cleanup,
                Cleanup::Complete
            );
            assert_ne!(
                outcome.metadata_ancestor_durability(),
                Some(&[Durability::Synced; 4])
            );
            assert!(
                outcome
                    .store_io_failures()
                    .any(|error| error.contains("injected recovered intent ancestor sync failure"))
            );
            assert!(!outcome.root_retirement_attempted());
            assert!(carrier_path.is_dir());
        }
        "retire_unrelated" => {
            assert!(hook);
            assert_eq!(outcome.state(), RecoveredRetirementState::Held);
            assert_eq!(
                outcome.metadata_removal().unwrap().receipt().effect,
                RemovalEffect::Deleted
            );
            assert_eq!(
                outcome
                    .walks()
                    .last()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .unresolved
                    .len(),
                1
            );
            assert!(
                input
                    .journal
                    .join(".tmp-offload-22222222222222222222222222222222")
                    .is_dir()
            );
            assert!(!outcome.root_retirement_attempted());
            assert!(carrier_path.is_dir());
        }
        "retire_unwind" => {
            assert!(hook);
            assert_eq!(outcome.state(), RecoveredRetirementState::Unknown);
            assert_eq!(
                outcome.metadata_removal().unwrap().receipt().effect,
                RemovalEffect::Deleted
            );
            assert!(outcome.root_retirement_attempted());
            assert!(outcome.root_retirement().is_none());
            assert!(!carrier_path.exists());
        }
        _ => unreachable!(),
    }
    let report = serde_json::json!({"mode": input.mode, "state": format!("{:?}", outcome.state()),
        "files_restored":0, "bytes_restored":0,"prior_applied_owned":true,"fresh_missing_paired":true,
        "successor_confirmed":true,"original_raw_event_register_preserved":true,"lease_busy":true,
        "metadata_deleted":outcome.metadata_removal().is_some_and(|removal| removal.receipt().effect == RemovalEffect::Deleted),
        "root_attempted":outcome.root_retirement_attempted(),"root_returned":outcome.root_retirement().is_some(),"fault_hook":hook});
    let mut custody = Some(std::thread::spawn(move || outcome).join().unwrap());
    assert!(matches!(
        ActivityLease::try_acquire(Arc::clone(&root)),
        Err(ActivityAdmissionError::Busy)
    ));
    if input.mode == "retire_complete" {
        drop(custody.take());
        let next = ActivityLease::try_acquire(Arc::clone(&root)).unwrap();
        next.revalidate().unwrap();
    }
    println!("NATIVE_RETIREMENT_RESULT {}", report);
}
