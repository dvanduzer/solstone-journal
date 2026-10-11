// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use crate::{archive_worker::begin_owned, operation};
use chrono::Datelike;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solstone_core_backup::archive_admission::Clock;
use solstone_core_journal_io::{
    activity::{ActivityLease, TeardownLease},
    atomic::{BoundPublicationPrimitive, run_with_bound_publication_fault},
    journal_root::JournalRoot,
    media::{MediaPrimitive, run_with_media_hook},
    paths::SegmentLayout,
};
use solstone_core_retention::{
    Target,
    archive_catalog::codec,
    archive_proof::{AcceptedProcessingObservation, ArchiveFileProof, OffloadOffer, OffloadProof},
    marks::{Mark, MarkId, MarkState, Proposal, Register, RemovalClass},
};
use std::{
    cell::{Cell, RefCell},
    fs,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    thread,
};

#[derive(Clone, Copy, PartialEq)]
enum Cut {
    None,
    CancelAfterPreflight,
    ActorPanic,
}
thread_local! {
    static CUT: Cell<Cut> = const { Cell::new(Cut::None) };
    static CONTROL: RefCell<Option<UnixStream>> = const { RefCell::new(None) };
}
fn cut(label: &[u8; 8]) {
    CONTROL.with(|control| {
        if let Some(control) = control.borrow_mut().as_mut() {
            control.write_all(label).unwrap();
            if label == b"prepared" {
                control
                    .write_all(&std::process::id().to_be_bytes())
                    .unwrap();
            }
            let mut ack = [0];
            control.read_exact(&mut ack).unwrap();
            assert_eq!(ack, [b'a']);
        }
    });
}
pub(super) fn prepared() {
    cut(b"prepared");
}
pub(super) fn admitted() {
    cut(b"admitted");
}
pub(super) fn after_preflight(slot: &SharedOperationSlot) {
    cut(b"captured");
    if CUT.with(|cut| cut.get()) == Cut::CancelAfterPreflight {
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
}
pub(super) fn after_actor() {
    cut(b"created_");
    if CUT.with(|cut| cut.get()) == Cut::ActorPanic {
        panic!("after actual offload projection");
    }
}

struct ActualClock;
impl Clock for ActualClock {
    fn now_unix(&self) -> i64 {
        Utc::now().timestamp()
    }
    fn iso_week(&self) -> u8 {
        Utc::now().iso_week().week() as u8
    }
}
fn clock() -> super::super::OwnedClock {
    Arc::new(ActualClock)
}
fn blocked(slot: &SharedOperationSlot, root: &Arc<JournalRoot>, clock: &super::super::OwnedClock) {
    assert!(operation::is_busy(slot));
    assert!(operation::begin(slot, "restore", None, None, None).is_err());
    assert!(begin_owned(slot, "offload", Arc::clone(root), Arc::clone(clock)).is_err());
    assert!(ActivityLease::try_acquire(Arc::clone(root)).is_err());
    assert!(TeardownLease::try_acquire(Arc::clone(root)).is_err());
}
fn config(journal: &Path, floor: Option<u64>) {
    fs::create_dir_all(journal.join("config")).unwrap();
    fs::write(journal.join("config/journal.json"), serde_json::to_vec(&json!({"retention":{"empty_audio":"keep"},"backup":{"enabled":true,"mode":"byo","daily_key":"synthetic-readback-password","recovery_key":"0".repeat(64),"offload":{"enabled":true,"floor_bytes":floor,"budget_bytes":0},"destination":{"backend":"s3","repository":"s3:bucket/prefix","credentials":{"access_key_id":"synthetic-access","secret_access_key":"synthetic-secret"}}}})).unwrap()).unwrap();
}
fn selection(names: Vec<String>) -> OffloadOfferSelection {
    OffloadOfferSelection {
        target: Target {
            day: "20261009".into(),
            stream: "_default".into(),
            dir: "120000_60_fixture".into(),
        },
        layout: SegmentLayout::Direct,
        names,
    }
}
fn seed_offer(
    root: &Arc<JournalRoot>,
    journal: &Path,
    names: &[String],
    repository: &str,
    snapshot: &str,
    archive_paths: &[String],
    empty: bool,
) -> MarkId {
    let selection = selection(names.to_vec());
    let raw_dir = journal.join("chronicle/20261009/120000_60_fixture");
    let files = names
        .iter()
        .zip(archive_paths)
        .map(|(name, archive_path)| {
            let raw = fs::read(raw_dir.join(name)).unwrap();
            let sidecar = fs::read_to_string(raw_dir.join(name).with_extension("jsonl")).unwrap();
            let header: serde_json::Value =
                serde_json::from_str(sidecar.lines().next().unwrap()).unwrap();
            ArchiveFileProof {
                witness_id: format!("worker-fixture-{name}"),
                name: name.clone(),
                bytes: raw.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&raw)),
                archive_path: archive_path.clone(),
                processing: AcceptedProcessingObservation {
                    record: Some(header["_solstone_processing"].clone()),
                    has_analysis_row: !empty,
                },
            }
        })
        .collect::<Vec<_>>();
    let proof = OffloadProof {
        journal_id: root.identity().observation_id(),
        manifest_id: "worker-fixture-manifest".into(),
        target: selection.target.clone(),
        layout: selection.layout,
        repository_id: repository.into(),
        snapshot_id: snapshot.into(),
        files,
    };
    proof.validate().unwrap();
    let mark = Mark {
        id: MarkId::derive(RemovalClass::OffloadRawRelease, &selection.target, names),
        class: RemovalClass::OffloadRawRelease,
        target: selection.target.clone(),
        marked_at: "2026-10-10T00:00:00Z".into(),
        proposal: Proposal {
            bytes: proof.bytes().unwrap(),
            reason: "synthetic archived recording".into(),
            names: names.to_vec(),
        },
        state: MarkState::Marked,
        offload: Some(OffloadOffer::new(proof.clone()).unwrap()),
    };
    let event = codec::Event::ArchiveV2 {
        schema_version: 2,
        time: Utc::now().timestamp().try_into().unwrap(),
        day: selection.target.day,
        stream: selection.target.stream,
        segment: "120000_60".into(),
        segment_dir: selection.target.dir,
        layout: selection.layout,
        journal_id: proof.journal_id,
        manifest_id: proof.manifest_id,
        repository_id: proof.repository_id,
        snapshot_id: proof.snapshot_id,
        files: proof.files,
    };
    fs::create_dir_all(journal.join("health/offload")).unwrap();
    let mut bytes = serde_json::to_vec(&event).unwrap();
    bytes.push(b'\n');
    fs::write(journal.join("health/offload/20261009.jsonl"), bytes).unwrap();
    let id = mark.id.clone();
    // Test-only owning codecs seed a current full offer; no production factory.
    let register = Register {
        version: 2,
        marks: [(id.clone(), mark)].into_iter().collect(),
    };
    fs::write(
        journal.join("health/retention-marks.json"),
        serde_json::to_vec(&register).unwrap(),
    )
    .unwrap();
    id
}

#[test]
fn offload_preflight_and_ready_admission_refusals_keep_original_owners() {
    for case in ["selected", "planner_floor", "ready_cancel"] {
        let dir = tempfile::tempdir().unwrap();
        config(
            dir.path(),
            if case == "planner_floor" {
                None
            } else {
                Some(1)
            },
        );
        let root = Arc::new(JournalRoot::open(dir.path()).unwrap());
        let id = if case != "selected" {
            let segment = dir.path().join("chronicle/20261009/120000_60_fixture");
            fs::create_dir_all(&segment).unwrap();
            fs::write(segment.join("empty.flac"), b"empty").unwrap();
            fs::write(segment.join("empty.jsonl"),format!("{}\n",json!({"_solstone_processing":{"schema":"solstone.processing.v1","handler":"transcribe","state":"empty","reason_code":"no_decodable_audio","input_size":5,"attempted_at":"2026-10-09T00:00:00Z"}}))).unwrap();
            Some(seed_offer(
                &root,
                dir.path(),
                &["empty.flac".into()],
                &"a".repeat(64),
                &"b".repeat(64),
                &["/owned/empty.flac".into()],
                true,
            ))
        } else {
            None
        };
        let slot = operation::new_slot();
        let clock = clock();
        let begin = begin_owned(&slot, "offload", Arc::clone(&root), Arc::clone(&clock)).unwrap();
        let worker_slot = Arc::clone(&slot);
        thread::spawn(move || {
            if case == "ready_cancel" {
                CUT.with(|cut| cut.set(Cut::CancelAfterPreflight));
            }
            if case == "selected" {
                create_selected(
                    &worker_slot,
                    begin,
                    vec![],
                    Utc::now().date_naive(),
                    Utc::now(),
                    Path::new("/must-not-open/restic"),
                );
            } else {
                create_planned(
                    &worker_slot,
                    begin,
                    Utc::now().fixed_offset(),
                    Path::new("/must-not-open/restic"),
                );
            }
        })
        .join()
        .unwrap();
        blocked(&slot, &root, &clock);
        let state = slot.lock().unwrap();
        assert_eq!(state.archive.retained.len(), 1);
        let retained = &state.archive.retained[0];
        let guard = retained.custody().unwrap();
        assert!(Arc::ptr_eq(guard.journal_root(), &root));
        assert!(Arc::ptr_eq(guard.owned_clock(), &clock));
        let ArchivePayload::Offload(owner) = retained.payload() else {
            panic!(
                "whole actual offload return: {:?}",
                retained.terminal().report
            )
        };
        assert!(owner.runtime().is_none());
        match case {
            "selected" => assert!(owner.selected_input_failure().is_some()),
            "planner_floor" => assert!(owner.planned_input_failure().is_some()),
            "ready_cancel" => {
                let ready = owner
                    .planned_ready()
                    .expect("actual ready survives normal admission Err");
                let failure = owner
                    .admission_failure()
                    .expect("whole actual admission failure");
                assert_eq!(
                    failure.error(),
                    solstone_core_backup_runtime::ArchiveError::Cancelled
                );
                assert!(Arc::ptr_eq(ready.carrier(), failure.carrier().unwrap()));
                assert!(Arc::ptr_eq(failure.journal_root(), &root));
                assert!(Arc::ptr_eq(failure.owned_clock().unwrap(), &clock));
                assert!(
                    !failure.effects().broker_requested
                        && !failure.effects().write_repository_requested
                        && !failure.effects().read_repository_requested
                );
            }
            _ => unreachable!(),
        }
        if let Some(id) = id {
            let register = solstone_core_retention::review_context::load_at_root(&root).unwrap();
            assert!(!register.marks.contains_key(&id));
            let groups = retained.terminal().report.as_ref().unwrap()["planning"]["reconciliation"]
                ["groups"]
                .as_array()
                .unwrap();
            assert!(
                groups
                    .iter()
                    .any(|group| group["disposition"]["publication"]["effect"] == "published")
            );
            assert_eq!(
                fs::read(
                    dir.path()
                        .join("chronicle/20261009/120000_60_fixture/empty.flac")
                )
                .unwrap(),
                b"empty"
            );
        }
    }
}

#[test]
fn closed_offload_returns_are_send_static() {
    fn check<T: Send + 'static>() {}
    check::<OffloadReturn>();
    check::<SettledReport>();
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum InstalledCut {
    SelectedComplete,
    SelectedUncertain,
    SelectedActorPanic,
    PlannedComplete,
    PlannedFinishPanic,
}
#[derive(Deserialize)]
struct Input {
    case: InstalledCut,
    journal: PathBuf,
    repository_id: String,
    snapshot_id: String,
    archive_paths: Vec<String>,
    expected_restic_path: PathBuf,
    control: PathBuf,
}

#[test]
#[ignore = "requires exact copied package and synthetic owning offload worker fixture"]
fn packaged_owned_offload_worker() {
    let input: Input = serde_json::from_slice(
        &fs::read(std::env::var_os("OWNED_OFFLOAD_WORKER_JSON").unwrap()).unwrap(),
    )
    .unwrap();
    let root = Arc::new(JournalRoot::open(&input.journal).unwrap());
    let planned = matches!(
        input.case,
        InstalledCut::PlannedComplete | InstalledCut::PlannedFinishPanic
    );
    let names = vec!["file1.flac".into(), "file2.flac".into()];
    if planned {
        seed_offer(
            &root,
            &input.journal,
            &names,
            &input.repository_id,
            &input.snapshot_id,
            &input.archive_paths,
            false,
        );
    }
    let clock = clock();
    let slot = operation::new_slot();
    let begin = begin_owned(&slot, "offload", Arc::clone(&root), Arc::clone(&clock)).unwrap();
    let worker_slot = Arc::clone(&slot);
    let mode = input.case;
    let restic = input.expected_restic_path.clone();
    let worker = thread::spawn(move || {
        let stream = UnixStream::connect(input.control).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(170)))
            .unwrap();
        stream
            .set_write_timeout(Some(std::time::Duration::from_secs(30)))
            .unwrap();
        CONTROL.with(|control| *control.borrow_mut() = Some(stream));
        if mode == InstalledCut::SelectedActorPanic {
            CUT.with(|cut| cut.set(Cut::ActorPanic));
        }
        let run = || {
            if planned {
                create_planned(&worker_slot, begin, Utc::now().fixed_offset(), &restic);
            } else {
                create_selected(
                    &worker_slot,
                    begin,
                    vec![selection(names)],
                    NaiveDate::from_ymd_opt(2026, 10, 10).unwrap(),
                    Utc::now(),
                    &restic,
                );
            }
        };
        if mode == InstalledCut::SelectedUncertain {
            let (_, fired) =
                run_with_bound_publication_fault(BoundPublicationPrimitive::ParentSync, 1, 13, run);
            assert!(fired);
        } else if mode == InstalledCut::PlannedFinishPanic {
            let (_, fired) = run_with_media_hook(
                MediaPrimitive::CarrierRetirementRootSync,
                || panic!("actual consuming Runtime finish cut"),
                run,
            );
            assert!(fired);
        } else {
            run();
        }
        cut(b"returned");
        CONTROL.with(|control| *control.borrow_mut() = None);
    });
    worker.join().unwrap();
    let state = slot.lock().unwrap();
    let current = state.current.as_ref().unwrap();
    let view = current.view.clone();
    assert!(!current.worker_active);
    let complete = matches!(
        mode,
        InstalledCut::SelectedComplete | InstalledCut::PlannedComplete
    );
    let report = view.report.as_ref().unwrap();
    if complete {
        assert_eq!(view.phase, "done", "actual report={report}");
        assert!(state.archive.retained.is_empty());
        assert_eq!(report["runtime"]["state"], "completed");
        assert!(state.archive.reservation.is_none());
    } else {
        assert_eq!(state.archive.retained.len(), 1);
        let retained = &state.archive.retained[0];
        assert!(retained.custody().is_some());
        let ArchivePayload::Offload(owner) = retained.payload() else {
            panic!("actual offload owner")
        };
        match mode {
            InstalledCut::SelectedUncertain => {
                let hold = owner.creation_hold().unwrap();
                assert!(hold.offers().unwrap().publication().is_some());
                assert_eq!(
                    report["offers"]["publication"]["effect"],
                    "published_uncertain"
                );
                assert_eq!(report["source_copies"].as_array().unwrap().len(), 2);
                assert_eq!(
                    report["offers"]["archive_reads"].as_array().unwrap().len(),
                    2
                );
            }
            InstalledCut::SelectedActorPanic => {
                assert_eq!(
                    owner.runtime().unwrap().state(),
                    DirectReadFinishState::Unknown
                );
                assert_eq!(report["offers"]["publication"]["effect"], "published");
                assert_eq!(report["source_copies"].as_array().unwrap().len(), 2);
                assert_eq!(report["settlement"]["state"], "unknown");
            }
            InstalledCut::PlannedFinishPanic => {
                assert!(owner.plan_report().is_some());
                assert!(owner.runtime().is_none());
                assert_eq!(report["runtime"]["state"], "unknown");
                assert_eq!(report["settlement"]["state"], "settled");
            }
            _ => unreachable!(),
        }
    }
    if planned {
        assert_eq!(report["planning"]["phase"], "already_satisfied");
        assert!(report["creation"].is_null());
        assert_eq!(report["planning"]["local"]["total"]["total_bytes"], 4194324);
        assert_eq!(
            report["planning"]["initial_projection"]["eligible_bytes"],
            4194324
        );
        assert_eq!(
            report["planning"]["initial_projection"]["projected_local_bytes"],
            0
        );
    } else if complete {
        assert_eq!(report["source_copies"].as_array().unwrap().len(), 2);
        assert!(
            report["source_copies"]
                .as_array()
                .unwrap()
                .iter()
                .all(|copy| copy["cleanup"] == "complete")
        );
        assert_eq!(
            report["offers"]["archive_reads"].as_array().unwrap().len(),
            2
        );
    }
    let facts = json!({"case":mode,"harness_pid":std::process::id(),"report":report,"complete":complete,"retained":state.archive.retained.len(),"original_guard":!complete});
    drop(state);
    if complete {
        assert!(!operation::is_busy(&slot));
        let next = begin_owned(&slot, "next", Arc::clone(&root), Arc::clone(&clock)).unwrap();
        next.cancel_before_preparation(&slot);
        let lease = ActivityLease::try_acquire(Arc::clone(&root)).unwrap();
        drop(lease);
    } else {
        blocked(&slot, &root, &clock);
    }
    println!("OWNED_OFFLOAD_WORKER_JSON={facts}");
    std::hint::black_box(&slot);
}
