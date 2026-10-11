// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use std::{cell::RefCell, fs, path::PathBuf, thread, time::Instant};

use serde::Deserialize;
use solstone_core_backup::archive_admission::Clock;
use solstone_core_journal_io::activity::{ActivityAdmissionError, ActivityLease, TeardownLease};
use solstone_core_journal_io::journal_root::JournalRoot;
use solstone_core_journal_io::media::{MediaPrimitive, run_with_media_hook};
use solstone_core_journal_io::paths::SegmentLayout;
use solstone_core_retention::archive_catalog::{RestoreDisposition, codec, read_complete_catalog};
use solstone_core_retention::archive_proof::{
    AcceptedProcessingObservation, ArchiveFileProof, OffloadProof,
};

use crate::archive_operation::ArchiveBeginRefusal;
use crate::archive_worker::{begin_owned, prepare_read};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Cut {
    Complete,
    Stale,
    LateDestination,
    ActorPanic,
    CancelBeforeActor,
    CancelBeforeFinish,
}

thread_local! {
    static CONTROL: RefCell<Option<(Cut, PathBuf)>> = const { RefCell::new(None) };
}

fn control() -> Option<(Cut, PathBuf)> {
    CONTROL.with(|control| control.borrow().clone())
}

pub(super) fn after_actor(native: &OwnedRestoreRunOutcome) {
    if matches!(control(), Some((Cut::ActorPanic, _))) {
        assert_eq!(native.report().files_restored, 1);
        let file = native.files().next().unwrap();
        assert_eq!(file.disposition, RestoreDisposition::Published);
        let owned::FileEvidence::Transaction(facts) = file.evidence else {
            panic!("actual native resources required at unwind cut");
        };
        assert!(facts.retained_media);
        assert!(facts.retirement.as_ref().unwrap().retired);
        assert!(facts.settlement_cleanup.is_none());
        panic!("actor unwind while native resources remain owned");
    }
}

pub(super) fn settle(
    native: OwnedRestoreRunOutcome,
) -> Result<(OwnedSettledDirectRead, SettledRestoreReport), Box<OwnedRestoreFinishHold>> {
    if let Some((Cut::LateDestination, segment)) = control() {
        let (settled, fired) = run_with_media_hook(
            MediaPrimitive::ParentSync,
            move || fs::write(segment.join("tombstone.json"), b"{}"),
            || native.settle_direct(),
        );
        assert!(fired);
        settled
    } else {
        native.settle_direct()
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
    if matches!(control(), Some((Cut::CancelBeforeFinish, _))) {
        cancel(slot);
    }
}

pub(super) fn before_handoff(slot: &SharedOperationSlot) {
    if matches!(control(), Some((Cut::Stale, _))) {
        let mut state = slot.lock().unwrap();
        let current = state.current.as_mut().unwrap();
        current.generation += 1;
        current.view.report = Some(json!({"replacement":true}));
    }
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn seed(input: &Input, root: &Arc<JournalRoot>) -> PathBuf {
    let proof = OffloadProof {
        journal_id: root.identity().observation_id(),
        manifest_id: "native-finish-manifest".into(),
        target: solstone_core_retention::Target {
            day: "20261009".into(),
            stream: "_default".into(),
            dir: "120000_60_fixture".into(),
        },
        layout: SegmentLayout::Direct,
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
    let ledger = input.journal.join("health/offload");
    fs::create_dir_all(&ledger).unwrap();
    // Owning data codec only, for this synthetic snapshot-backed fixture.
    let event = codec::Event::ArchiveV2 {
        schema_version: 2,
        time: 1,
        day: proof.target.day.clone(),
        stream: proof.target.stream.clone(),
        segment: "120000_60".into(),
        segment_dir: proof.target.dir.clone(),
        layout: proof.layout,
        journal_id: proof.journal_id,
        manifest_id: proof.manifest_id,
        repository_id: proof.repository_id,
        snapshot_id: proof.snapshot_id,
        files: proof.files,
    };
    let mut bytes = serde_json::to_vec(&event).unwrap();
    bytes.push(b'\n');
    fs::write(ledger.join("20261009.jsonl"), bytes).unwrap();
    let catalog = read_complete_catalog(root).unwrap();
    assert_eq!(catalog.days.len(), 1);
    segment
}

fn blocked(slot: &SharedOperationSlot, root: &Arc<JournalRoot>, clock: &super::super::OwnedClock) {
    assert!(operation::is_busy(slot));
    assert!(operation::begin(slot, "restore", None, None, None).is_err());
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

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires a copied current package and synthetic direct restore fixture"]
fn packaged_owned_restore_worker() {
    use sha2::{Digest, Sha256};
    let input: Input = serde_json::from_slice(
        &fs::read(std::env::var_os("OWNED_RESTORE_WORKER_JSON").unwrap()).unwrap(),
    )
    .unwrap();
    let root = Arc::new(JournalRoot::open(&input.journal).unwrap());
    let segment = seed(&input, &root);
    let slot = operation::new_slot();
    let clock: super::super::OwnedClock = Arc::new(LiveClock(Instant::now()));
    let begin = begin_owned(
        &slot,
        "offload_restore",
        Arc::clone(&root),
        Arc::clone(&clock),
    )
    .unwrap();
    let generation = begin.generation();
    let worker_slot = Arc::clone(&slot);
    let worker_segment = segment.clone();
    let restic = input.restic_path.clone();
    let repository = input.repository_id.clone();
    let mode = input.mode;
    let worker = thread::spawn(move || {
        let worker = prepare_read(&worker_slot, begin, repository, &restic)
            .unwrap_or_else(|| panic!("actual original worker entrance"));
        let carrier = Arc::downgrade(worker.outside.carrier());
        let lease = Arc::downgrade(worker.outside.carrier().activity());
        CONTROL.with(|control| *control.borrow_mut() = Some((mode, worker_segment)));
        if mode == Cut::CancelBeforeActor {
            cancel(&worker_slot);
        }
        day(&worker_slot, worker, "20261009", 2);
        CONTROL.with(|control| *control.borrow_mut() = None);
        (carrier, lease)
    });
    let (carrier, lease) = worker.join().unwrap();
    let guard = slot.lock().unwrap();
    let current = guard.current.as_ref().unwrap();
    let view = serde_json::to_value(&current.view).unwrap();
    let mut result = json!({"mode":format!("{mode:?}"), "view":view});
    if mode == Cut::Complete {
        assert!(guard.archive.retained.is_empty());
        assert!(guard.archive.reservation.is_none());
        assert_eq!(current.view.phase, "done");
        let report = current.view.report.as_ref().unwrap();
        assert_eq!(report["bytes_restored"], input.bytes);
        assert_eq!(report["runtime"]["state"], "completed");
        assert_eq!(report["runtime"]["retirement"]["effect"], "removed");
        assert_eq!(
            report["files"][0]["evidence"]["settlement_cleanup"]["cleanup"],
            "complete"
        );
        assert!(carrier.upgrade().is_none() && lease.upgrade().is_none());
        drop(guard);
        assert!(ActivityLease::try_acquire(Arc::clone(&root)).is_ok());
        assert!(TeardownLease::try_acquire(Arc::clone(&root)).is_ok());
        let next = begin_owned(
            &slot,
            "offload_restore",
            Arc::clone(&root),
            Arc::clone(&clock),
        )
        .unwrap();
        let next = prepare_read(&slot, next, input.repository_id, &input.restic_path)
            .unwrap_or_else(|| panic!("successor original installed entrance"));
        return_owned(
            &slot,
            next.ticket,
            Some(next.custody),
            ArchivePayload::ReadHeld {
                read: Box::new(next.read.into_owned_hold()),
                outside: next.outside,
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
        let report = retained.terminal().report.as_ref();
        if mode != Cut::CancelBeforeActor {
            assert_eq!(report.unwrap()["bytes_restored"], input.bytes);
            assert_eq!(report.unwrap()["files"][0]["disposition"], "published");
        }
        match retained.payload() {
            ArchivePayload::ReadHeld { read, outside } => {
                assert_eq!(mode, Cut::CancelBeforeActor);
                assert!(Arc::ptr_eq(read.carrier(), outside.carrier()));
                assert!(Arc::ptr_eq(outside.journal_root(), &root));
                assert!(Arc::ptr_eq(outside.owned_clock(), &clock));
                assert_eq!(
                    retained.terminal().reason_code.as_deref(),
                    Some("cancelled")
                );
                assert!(!segment.join("audio.flac").exists());
                assert!(report.is_none());
            }
            ArchivePayload::NativeHeld { native, outside } => {
                assert_eq!(mode, Cut::LateDestination);
                assert!(matches!(
                    native.refusal(),
                    RestoreFinishRefusal::Native {
                        reason: "settlement_destination_changed",
                        ..
                    }
                ));
                assert!(Arc::ptr_eq(native.carrier(), outside.carrier()));
                assert_eq!(native.report().bytes_restored, input.bytes);
                assert_eq!(report.unwrap()["settlement"]["state"], "held");
                assert_eq!(
                    report.unwrap()["files"][0]["evidence"]["settlement_cleanup"]["cleanup"],
                    "complete"
                );
            }
            ArchivePayload::NativeFinished {
                report: native,
                runtime,
            } => {
                assert_eq!(native.report().bytes_restored, input.bytes);
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
                assert_eq!(
                    report.unwrap()["files"][0]["evidence"]["settlement_cleanup"]["cleanup"],
                    "complete"
                );
                if mode == Cut::Stale {
                    let retirement = runtime.retirement().unwrap();
                    assert!(std::ptr::eq(
                        Arc::as_ptr(retirement.owned.activity()),
                        lease.as_ptr()
                    ));
                } else {
                    assert!(runtime.reason().is_some());
                }
            }
            ArchivePayload::Unknown {
                runtime,
                report: native,
            } => {
                assert_eq!(mode, Cut::ActorPanic);
                assert_eq!(runtime.state(), DirectReadFinishState::Unknown);
                assert!(runtime.retirement().is_none() && native.is_none());
                assert!(Arc::ptr_eq(runtime.journal_root(), &root));
                assert!(Arc::ptr_eq(runtime.owned_clock(), &clock));
                assert_eq!(report.unwrap()["settlement"]["state"], "unknown");
                assert!(report.unwrap()["files"][0]["evidence"]["settlement_cleanup"].is_null());
            }
            _ => panic!("actual whole worker payload required"),
        }
        let terminal = retained.terminal();
        result["terminal"] = json!({
            "phase":terminal.phase, "reason_code":terminal.reason_code, "detail":terminal.detail,
            "guidance":terminal.guidance, "recording_failure":terminal.recording_failure,
            "report":terminal.report,
        });
        result["held"] = json!(true);
        drop(guard);
        blocked(&slot, &root, &clock);
        assert!(lease.upgrade().is_some());
        // Genuine retirement moves the carrier out of its Arc into the owning
        // receipt. Its activity lease remains retained by rejected completion.
        if mode != Cut::Stale {
            assert!(carrier.upgrade().is_some());
        }
    }
    if mode != Cut::CancelBeforeActor {
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(fs::read(segment.join("audio.flac")).unwrap())
            ),
            input.sha256
        );
    }
    println!("OWNED_RESTORE_WORKER_RESULT {}", result);
}
