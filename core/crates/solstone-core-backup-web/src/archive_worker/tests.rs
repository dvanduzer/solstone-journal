// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use solstone_core_backup::archive_activity::PreparedArchiveOperation;
use solstone_core_journal_io::activity::{ActivityAdmissionError, ActivityLease, TeardownLease};
use std::{sync::mpsc, thread, time::Duration};

struct FixedClock;
impl Clock for FixedClock {
    fn now_unix(&self) -> i64 {
        100
    }
    fn iso_week(&self) -> u8 {
        1
    }
}

fn fixture() -> (
    tempfile::TempDir,
    Arc<JournalRoot>,
    OwnedClock,
    SharedOperationSlot,
) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("config")).unwrap();
    std::fs::write(dir.path().join("config/journal.json"), br#"{"backup":{"enabled":true,"mode":"byo","daily_key":"daily","recovery_key":"0123456789ABCDEFGHJKMNPQRSTVWXYZ0123456789ABCDEFGHJKMNPQRSTVWXYZ","destination":{"repository":"s3:repo","backend":"s3","credentials":{"access_key_id":"access","secret_access_key":"secret"}}}}"#).unwrap();
    let root = Arc::new(JournalRoot::open(dir.path()).unwrap());
    (dir, root, Arc::new(FixedClock), operation::new_slot())
}

fn closed(begin: OwnedArchiveBegin) -> (ArchiveTicket, ArchivePayload) {
    let (ticket, root, budget, clock) = begin.into_parts();
    let prepared = PreparedArchiveOperation::prepare_owned(root, budget, clock).unwrap();
    let owner = match prepared.into_owned_hold() {
        Ok(owner) => owner,
        Err(_) => panic!("actual prepare_owned must retain its owned clock"),
    };
    (ticket, ArchivePayload::Prepared(Box::new(owner)))
}

fn terminal() -> Terminal {
    Terminal {
        phase: "degraded".into(),
        reason_code: Some("held".into()),
        detail: Some("earlier effects".into()),
        guidance: Some("remaining work".into()),
        recording_failure: Some("recording".into()),
        report: Some(serde_json::json!({"files_restored":1,"bytes_restored":7})),
    }
}

fn blocked(slot: &SharedOperationSlot, root: &Arc<JournalRoot>, clock: &OwnedClock) {
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
}

#[test]
fn real_worker_retains_closed_owner_after_matching_and_rejected_handoffs() {
    for reason in [
        None,
        Some(ArchiveHandoffRefusal::Missing),
        Some(ArchiveHandoffRefusal::Stale),
        Some(ArchiveHandoffRefusal::Inactive),
        Some(ArchiveHandoffRefusal::Poisoned),
    ] {
        let (_dir, root, clock, slot) = fixture();
        let begin = begin_owned(
            &slot,
            "offload_restore",
            Arc::clone(&root),
            Arc::clone(&clock),
        )
        .unwrap();
        let generation = begin.generation();
        assert!(start_owned_worker(&slot, &begin.ticket));
        assert!(!operation::start_worker(&slot, generation));
        let (entered, observed) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let worker_slot = Arc::clone(&slot);
        let worker = thread::spawn(move || {
            let (ticket, payload) = closed(begin);
            let ArchivePayload::Prepared(owner) = &payload else {
                unreachable!()
            };
            entered.send(Arc::downgrade(owner.carrier())).unwrap();
            released.recv_timeout(Duration::from_secs(10)).unwrap();
            if let Err(rejected) = handoff(&worker_slot, ticket, payload, terminal()) {
                // This is the actual worker caller, before thread return.
                park_rejected(&worker_slot, rejected);
            }
        });
        let carrier = observed.recv_timeout(Duration::from_secs(10)).unwrap();
        match reason {
            Some(ArchiveHandoffRefusal::Missing) => {
                slot.lock().unwrap().current = None;
            }
            Some(ArchiveHandoffRefusal::Stale) => {
                slot.lock().unwrap().current.as_mut().unwrap().generation += 1;
            }
            Some(ArchiveHandoffRefusal::Inactive) => {
                slot.lock().unwrap().current.as_mut().unwrap().worker_active = false;
            }
            Some(ArchiveHandoffRefusal::Poisoned) => {
                let poison = Arc::clone(&slot);
                assert!(
                    thread::spawn(move || {
                        let _guard = poison.lock().unwrap();
                        panic!("poison control");
                    })
                    .join()
                    .is_err()
                );
            }
            None => {}
            other => panic!("unexpected control {other:?}"),
        }
        blocked(&slot, &root, &clock);
        release.send(()).unwrap();
        worker.join().unwrap();
        blocked(&slot, &root, &clock);
        assert!(carrier.upgrade().is_some());
        assert!(matches!(
            ActivityLease::try_acquire(Arc::clone(&root)),
            Err(ActivityAdmissionError::Busy)
        ));
        assert!(matches!(
            TeardownLease::try_acquire(Arc::clone(&root)),
            Err(ActivityAdmissionError::Busy)
        ));
        let guard = slot.lock().unwrap_or_else(|poison| poison.into_inner());
        assert_eq!(guard.archive.retained.len(), 1);
        let retained = &guard.archive.retained[0];
        assert_eq!(retained.reason(), reason);
        assert_eq!(retained.generation(), generation);
        assert_eq!(retained.terminal(), &terminal());
        let ArchivePayload::Prepared(owner) = retained.payload() else {
            panic!("whole actual owner");
        };
        assert!(Arc::ptr_eq(owner.journal_root(), &root));
        assert!(Arc::ptr_eq(owner.owned_clock(), &clock));
        assert!(Arc::ptr_eq(owner.carrier(), &carrier.upgrade().unwrap()));
        if reason.is_none() {
            let current = guard.current.as_ref().unwrap();
            assert!(!current.worker_active);
            assert_eq!(current.view.phase, "restoring");
            assert_eq!(current.view.report, terminal().report);
        }
        drop(guard);
        assert_eq!(
            slot.is_poisoned(),
            reason == Some(ArchiveHandoffRefusal::Poisoned)
        );
    }
}

#[test]
fn live_reservation_blocks_absent_terminal_and_replacement_view_before_handoff() {
    let (_dir, root, clock, slot) = fixture();
    let begin = begin_owned(
        &slot,
        "offload_restore",
        Arc::clone(&root),
        Arc::clone(&clock),
    )
    .unwrap();
    assert!(start_owned_worker(&slot, &begin.ticket));
    let (entered, observed) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let worker_slot = Arc::clone(&slot);
    let worker = thread::spawn(move || {
        let (ticket, payload) = closed(begin);
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(10)).unwrap();
        if let Err(refusal) = handoff(&worker_slot, ticket, payload, terminal()) {
            park_rejected(&worker_slot, refusal);
        }
    });
    observed.recv_timeout(Duration::from_secs(10)).unwrap();
    let mut original = slot.lock().unwrap().current.take().unwrap();
    blocked(&slot, &root, &clock);
    original.view.phase = "done".into();
    slot.lock().unwrap().current = Some(original);
    blocked(&slot, &root, &clock);
    slot.lock().unwrap().current.as_mut().unwrap().generation += 1;
    blocked(&slot, &root, &clock);
    release.send(()).unwrap();
    worker.join().unwrap();
    blocked(&slot, &root, &clock);
    assert_eq!(
        slot.lock().unwrap().archive.retained[0].reason(),
        Some(ArchiveHandoffRefusal::Stale)
    );
}

#[test]
fn intact_original_begin_cancels_only_before_preparation_and_releases_reservation() {
    let (_dir, root, clock, slot) = fixture();
    let begin = begin_owned(
        &slot,
        "offload_restore",
        Arc::clone(&root),
        Arc::clone(&clock),
    )
    .unwrap();
    let generation = begin.generation();
    assert!(matches!(
        crate::archive_operation::request_cancellation(&slot, generation),
        crate::archive_operation::ArchiveCancellationRequest::Requested
    ));
    blocked(&slot, &root, &clock);
    assert!(!start_owned_worker(&slot, &begin.ticket));
    begin.cancel_before_preparation(&slot);
    assert!(!operation::is_busy(&slot));
    assert_eq!(
        operation::current(&slot).unwrap().reason_code.as_deref(),
        Some("cancelled")
    );
    assert!(
        solstone_core_journal_io::activity_obligations::observe_obligations(&root)
            .unwrap()
            .entries()
            .is_empty()
    );
    assert!(operation::begin(&slot, "restore", None, None, None).is_ok());
}
