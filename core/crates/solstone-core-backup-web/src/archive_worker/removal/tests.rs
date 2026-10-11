// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use std::{cell::Cell, fs, path::PathBuf, sync::Arc, thread, time::Instant};

use chrono::TimeZone;
use serde::Deserialize;
use solstone_core_backup::archive_activity::OwnedPreparedArchiveOperation;
use solstone_core_backup::archive_admission::Clock;
use solstone_core_backup_runtime::{
    DirectReadFinishState, UreqHttpTransport, admit_prepared_archive_owned_read_installed,
};
use solstone_core_journal_io::JournalRoot;
use solstone_core_journal_io::activity::{ActivityAdmissionError, ActivityLease, TeardownLease};
use solstone_core_journal_io::media::{MediaDirectory, RemovalEffect};
use solstone_core_journal_io::paths::SegmentLayout;
use solstone_core_retention::archive_catalog::{codec, read_complete_catalog};
use solstone_core_retention::archive_proof::{
    AcceptedProcessingObservation, ArchiveFileProof, OffloadOffer, OffloadProof,
};
use solstone_core_retention::content::{ClosedHandlerSet, JournalMedia};
use solstone_core_retention::marks::{Mark, MarkId, MarkState, Proposal, Register, RemovalClass};
use solstone_core_retention::remove_marked::{
    OwnedReviewedArchiveRun, ReviewedArchiveFinishRefusal,
};
use solstone_core_retention::review_context::{
    check_review_context, load_at_root, read_review_context,
};

use crate::archive_operation::ArchiveBeginRefusal;
use crate::archive_worker::{begin_owned, start_owned_worker};
use crate::operation;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Cut {
    Complete,
    Stale,
    Unproven,
    ActorPanic,
    CancelBeforeActor,
    CancelBeforeFinish,
}
thread_local! { static CONTROL: Cell<Option<Cut>> = const { Cell::new(None) }; }

pub(super) fn after_actor(proof: &OwnedReviewedArchiveRun) {
    if CONTROL.get() == Some(Cut::ActorPanic) {
        assert_eq!(proof.outcome().removed_paths().count(), 1);
        assert_eq!(proof.attempts().len(), 1);
        assert_eq!(
            proof.attempts()[0].native().receipt().effect,
            RemovalEffect::Deleted
        );
        assert_eq!(proof.archive_reads().count(), 1);
        assert!(proof.mutation().unwrap().is_ok());
        panic!("actor unwind while actual removal and archive owners remain held");
    }
}
fn cancel(slot: &SharedOperationSlot) {
    slot.lock()
        .unwrap()
        .current
        .as_ref()
        .unwrap()
        .archive_cancellation
        .as_ref()
        .unwrap()
        .cancel();
}
pub(super) fn before_finish(slot: &SharedOperationSlot) {
    if CONTROL.get() == Some(Cut::CancelBeforeFinish) {
        cancel(slot);
    }
}
pub(super) fn before_handoff(slot: &SharedOperationSlot) {
    if CONTROL.get() == Some(Cut::Stale) {
        let mut state = slot.lock().unwrap();
        let current = state.current.as_mut().unwrap();
        current.generation += 1;
        current.view.report = Some(json!({"replacement":true}));
    }
}

#[derive(Deserialize)]
struct Input {
    mode: Cut,
    journal: PathBuf,
    restic_path: PathBuf,
    repository_id: String,
    snapshot_id: String,
    archive_path: String,
    bytes: u64,
    sha256: String,
}
struct LiveClock(Instant);
impl Clock for LiveClock {
    fn now_unix(&self) -> i64 {
        10_000 + self.0.elapsed().as_secs() as i64
    }
    fn iso_week(&self) -> u8 {
        1
    }
}

fn seed(input: &Input, root: &Arc<JournalRoot>) -> (Mark, PathBuf) {
    let target = solstone_core_retention::Target {
        day: "20261009".into(),
        stream: "_default".into(),
        dir: "120000_60_fixture".into(),
    };
    let relative =
        solstone_core_retention::layout::segment_rel(&target.day, &target.stream, &target.dir);
    let segment = input.journal.join(&relative);
    fs::create_dir_all(&segment).unwrap();
    fs::create_dir_all(input.journal.join("chronicle/20261009/health")).unwrap();
    fs::write(
        segment.join("audio.flac"),
        fs::read(&input.archive_path).unwrap(),
    )
    .unwrap();
    fs::write(segment.join("audio.jsonl"), format!(
        "{{\"_solstone_processing\":{{\"schema\":\"solstone.processing.v1\",\"state\":\"analyzed\",\"reason_code\":\"ok\",\"handler\":\"transcribe\",\"input_size\":{},\"attempted_at\":\"2026-10-09T12:00:00Z\"}}}}\n{{\"start\":0,\"text\":\"synthetic transcript\"}}\n",input.bytes
    )).unwrap();
    let directory =
        MediaDirectory::open(Arc::clone(root), std::path::Path::new(&relative)).unwrap();
    let scan = solstone_core_retention::scan::scan_segment_bound(
        &directory,
        &ClosedHandlerSet,
        &JournalMedia,
        65_536,
    )
    .unwrap();
    let file = &scan.files()[0];
    let processing = AcceptedProcessingObservation::capture(
        &ClosedHandlerSet,
        &target,
        &file.name,
        file.size,
        &file.sidecar,
    )
    .unwrap();
    let observed = directory
        .observe_file(std::ffi::OsStr::new("audio.flac"))
        .unwrap()
        .unwrap();
    assert_eq!(observed.digest.size, input.bytes);
    assert_eq!(observed.digest.sha256, input.sha256);
    let proof = OffloadProof {
        journal_id: root.identity().observation_id(),
        manifest_id: "synthetic-worker-manifest".into(),
        target: target.clone(),
        layout: SegmentLayout::Direct,
        repository_id: input.repository_id.clone(),
        snapshot_id: input.snapshot_id.clone(),
        files: vec![ArchiveFileProof {
            witness_id: "synthetic-worker-witness".into(),
            name: "audio.flac".into(),
            bytes: input.bytes,
            sha256: input.sha256.clone(),
            archive_path: input.archive_path.clone(),
            processing,
        }],
    };
    let event = codec::Event::ArchiveV2 {
        schema_version: 2,
        time: 1,
        day: target.day.clone(),
        stream: target.stream.clone(),
        segment: "120000_60".into(),
        segment_dir: target.dir.clone(),
        layout: proof.layout,
        journal_id: proof.journal_id.clone(),
        manifest_id: proof.manifest_id.clone(),
        repository_id: proof.repository_id.clone(),
        snapshot_id: proof.snapshot_id.clone(),
        files: proof.files.clone(),
    };
    let ledger = input.journal.join("health/offload");
    fs::create_dir_all(&ledger).unwrap();
    let mut bytes = serde_json::to_vec(&event).unwrap();
    bytes.push(b'\n');
    fs::write(ledger.join("20261009.jsonl"), bytes).unwrap();
    let names = vec!["audio.flac".into()];
    let mark = Mark {
        id: MarkId::derive(RemovalClass::OffloadRawRelease, &target, &names),
        target,
        class: RemovalClass::OffloadRawRelease,
        marked_at: "2026-10-09T12:00:00Z".into(),
        proposal: Proposal {
            names,
            bytes: input.bytes,
            reason: "synthetic control".into(),
        },
        state: MarkState::Marked,
        offload: Some(OffloadOffer::new(proof).unwrap()),
    };
    let mut register = Register {
        version: 2,
        marks: Default::default(),
    };
    register.marks.insert(mark.id.clone(), mark.clone());
    fs::write(
        input.journal.join("health/retention-marks.json"),
        serde_json::to_vec(&register).unwrap(),
    )
    .unwrap();
    assert!(
        read_complete_catalog(root)
            .unwrap()
            .active_witness(
                &mark.offload.as_ref().unwrap().proof,
                &mark.offload.as_ref().unwrap().proof.files[0]
            )
            .is_some()
    );
    (mark, segment)
}

fn blocked(slot: &SharedOperationSlot, root: &Arc<JournalRoot>, clock: &super::super::OwnedClock) {
    assert!(operation::is_busy(slot));
    assert!(operation::begin(slot, "archive", None, None, None).is_err());
    assert!(matches!(
        crate::archive_operation::begin(slot, "archive", clock.as_ref()),
        Err(ArchiveBeginRefusal::Busy)
    ));
    assert!(matches!(
        begin_owned(slot, "archive", Arc::clone(root), Arc::clone(clock)),
        Err(ArchiveBeginRefusal::Busy)
    ));
    assert!(matches!(
        ActivityLease::try_acquire(Arc::clone(root)),
        Err(ActivityAdmissionError::Busy)
    ));
    assert!(matches!(
        TeardownLease::try_acquire(Arc::clone(root)),
        Err(ActivityAdmissionError::Busy)
    ));
}
fn positive(report: &Value) {
    assert_eq!(report["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(report["attempts"][0]["native"]["effect"], "deleted");
    assert_eq!(report["attempts"][0]["append"]["effect"], "appended");
    assert_eq!(
        report["attempts"][0]["marker"]["publication"]["effect"],
        "published"
    );
    assert_eq!(report["mutation"]["publication"], "published");
    assert_eq!(report["archive_reads"][0]["cleanup"], "complete");
    assert_eq!(
        report["outcome"]["targets"][0]["removed"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
#[ignore = "requires copied package and synthetic installed reviewed-removal fixture"]
fn packaged_owned_removal_worker() {
    let input: Input = serde_json::from_slice(
        &fs::read(std::env::var_os("OWNED_REMOVAL_WORKER_JSON").unwrap()).unwrap(),
    )
    .unwrap();
    let root = Arc::new(JournalRoot::open(&input.journal).unwrap());
    let (mark, segment) = seed(&input, &root);
    let context = read_review_context(&root, std::slice::from_ref(&mark.id)).unwrap();
    let checked = check_review_context(Arc::clone(&root), &context).unwrap();
    if input.mode == Cut::Unproven {
        fs::copy(segment.join("audio.flac"), segment.join("extra.flac")).unwrap();
    }
    let slot = operation::new_slot();
    let clock: super::super::OwnedClock = Arc::new(LiveClock(Instant::now()));
    let begin = begin_owned(
        &slot,
        "offload_approve",
        Arc::clone(&root),
        Arc::clone(&clock),
    )
    .unwrap();
    let generation = begin.generation();
    assert!(start_owned_worker(&slot, &begin.ticket));
    let worker_slot = Arc::clone(&slot);
    let restic = input.restic_path.clone();
    let repository = input.repository_id.clone();
    let mode = input.mode;
    let worker = thread::spawn(move || {
        let (ticket, root, budget, clock) = begin.into_parts();
        let prepared = OwnedPreparedArchiveOperation::prepare(root, budget, clock)
            .unwrap_or_else(|_| panic!("original preparation"));
        let carrier = Arc::downgrade(prepared.carrier());
        let lease = Arc::downgrade(prepared.activity());
        let installed = admit_prepared_archive_owned_read_installed(
            prepared,
            repository,
            &restic,
            env!("CARGO_PKG_VERSION"),
            &UreqHttpTransport,
        )
        .unwrap_or_else(|_| panic!("installed admission"));
        CONTROL.set(Some(mode));
        if mode == Cut::CancelBeforeActor {
            cancel(&worker_slot);
        }
        remove(
            &worker_slot,
            ticket,
            installed,
            checked,
            NaiveDate::from_ymd_opt(2026, 10, 11).unwrap(),
            Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0)
                .single()
                .unwrap(),
        );
        CONTROL.set(None);
        (carrier, lease)
    });
    let (carrier, lease) = worker.join().unwrap();
    let guard = slot.lock().unwrap();
    let current = guard.current.as_ref().unwrap();
    let mut result = json!({"mode":format!("{mode:?}"),"view":current.view});
    if mode == Cut::Complete {
        assert!(guard.archive.retained.is_empty() && guard.archive.reservation.is_none());
        assert_eq!(current.view.phase, "done");
        let report = current.view.report.as_ref().unwrap();
        positive(report);
        assert_eq!(report["archive_reads"][0]["state"], "confirmed");
        assert_eq!(report["runtime"]["state"], "completed");
        assert_eq!(report["runtime"]["retirement"]["effect"], "removed");
        assert!(carrier.upgrade().is_none() && lease.upgrade().is_none());
        drop(guard);
        assert!(ActivityLease::try_acquire(Arc::clone(&root)).is_ok());
        assert!(TeardownLease::try_acquire(Arc::clone(&root)).is_ok());
        let next = begin_owned(&slot, "archive", Arc::clone(&root), Arc::clone(&clock)).unwrap();
        assert!(start_owned_worker(&slot, &next.ticket));
        let (ticket, root, budget, clock) = next.into_parts();
        let prepared = OwnedPreparedArchiveOperation::prepare(root, budget, clock)
            .unwrap_or_else(|_| panic!("successor preparation"));
        let installed = admit_prepared_archive_owned_read_installed(
            prepared,
            input.repository_id,
            &input.restic_path,
            env!("CARGO_PKG_VERSION"),
            &UreqHttpTransport,
        )
        .unwrap_or_else(|_| panic!("successor installed admission"));
        let (read, outside) = installed.into_owned_hold();
        return_owned(
            &slot,
            ticket,
            ArchivePayload::ReadHeld {
                read: Box::new(read),
                outside,
            },
            Terminal::error("cancelled"),
        );
        result["next_installed_admission"] = json!(true);
    } else {
        assert_eq!(guard.archive.retained.len(), 1);
        let retained = &guard.archive.retained[0];
        assert_eq!(retained.generation(), generation);
        if mode == Cut::Stale {
            assert_eq!(
                retained.reason(),
                Some(super::super::ArchiveHandoffRefusal::Stale)
            );
            assert_eq!(current.view.report, Some(json!({"replacement":true})));
        } else {
            assert!(retained.reason().is_none());
        }
        let facts = retained.terminal().report.as_ref();
        match retained.payload() {
            ArchivePayload::ReadHeld { read, outside } => {
                assert_eq!(mode, Cut::CancelBeforeActor);
                assert!(Arc::ptr_eq(read.carrier(), outside.carrier()));
                assert!(Arc::ptr_eq(outside.journal_root(), &root));
                assert!(Arc::ptr_eq(outside.owned_clock(), &clock));
                assert!(facts.is_none());
                assert!(segment.join("audio.flac").exists());
                assert_eq!(
                    load_at_root(&root).unwrap().marks.get(&mark.id),
                    Some(&mark)
                );
            }
            ArchivePayload::ProofHeld { proof, outside } => {
                assert_eq!(mode, Cut::Unproven);
                assert!(Arc::ptr_eq(proof.archive_carrier(), outside.carrier()));
                assert!(Arc::ptr_eq(outside.journal_root(), &root));
                assert!(Arc::ptr_eq(outside.owned_clock(), &clock));
                assert!(matches!(
                    proof.finish_refusal(),
                    Some(ReviewedArchiveFinishRefusal::IncompleteSelection)
                ));
                assert!(proof.refusal().is_none());
                assert!(proof.attempts().is_empty());
                assert_eq!(proof.selected_marks(), std::slice::from_ref(&mark));
                assert!(segment.join("audio.flac").exists());
                assert_eq!(
                    load_at_root(&root).unwrap().marks.get(&mark.id),
                    Some(&mark)
                );
                let report = facts.unwrap();
                assert_eq!(report["finish_refusal"]["kind"], "incomplete_selection");
                assert_eq!(report["attempts"], json!([]));
                assert_eq!(report["mutation"]["publication"], "unchanged");
                assert_eq!(report["settlement"]["state"], "held");
                assert_eq!(proof.archive_reads().count(), 1);
            }
            ArchivePayload::ProofFinished { report, runtime } => {
                assert!(matches!(mode, Cut::Stale | Cut::CancelBeforeFinish));
                positive(facts.unwrap());
                assert_eq!(facts.unwrap()["archive_reads"][0]["state"], "confirmed");
                assert_eq!(report.outcome().removed_paths().count(), 1);
                assert!(Arc::ptr_eq(runtime.journal_root(), &root));
                assert!(Arc::ptr_eq(runtime.owned_clock(), &clock));
                assert_eq!(
                    runtime.state(),
                    if mode == Cut::Stale {
                        DirectReadFinishState::Completed
                    } else {
                        DirectReadFinishState::Held
                    }
                );
                if mode == Cut::Stale {
                    assert!(std::ptr::eq(
                        Arc::as_ptr(runtime.retirement().unwrap().owned.activity()),
                        lease.as_ptr()
                    ));
                } else {
                    assert!(runtime.reason().is_some());
                }
            }
            ArchivePayload::ProofUnknown { runtime } => {
                assert_eq!(mode, Cut::ActorPanic);
                positive(facts.unwrap());
                assert_eq!(facts.unwrap()["archive_reads"][0]["state"], "prepared");
                assert_eq!(runtime.state(), DirectReadFinishState::Unknown);
                assert!(runtime.retirement().is_none());
                assert!(Arc::ptr_eq(runtime.journal_root(), &root));
                assert!(Arc::ptr_eq(runtime.owned_clock(), &clock));
                assert_eq!(facts.unwrap()["settlement"]["state"], "unknown");
            }
            _ => panic!("actual concrete Proof custody required"),
        }
        result["terminal"] = json!({"phase":retained.terminal().phase,"reason_code":retained.terminal().reason_code,"report":retained.terminal().report});
        result["held"] = json!(true);
        drop(guard);
        blocked(&slot, &root, &clock);
        assert!(lease.upgrade().is_some());
        if mode != Cut::Stale {
            assert!(carrier.upgrade().is_some());
        }
    }
    if matches!(
        mode,
        Cut::Complete | Cut::Stale | Cut::ActorPanic | Cut::CancelBeforeFinish
    ) {
        assert!(!segment.join("audio.flac").exists());
        assert!(load_at_root(&root).unwrap().marks.is_empty());
    }
    println!("OWNED_REMOVAL_WORKER_RESULT {}", result);
}
