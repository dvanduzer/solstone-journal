// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use crate::archive_admission::ArchiveCancellation;
use solstone_core_journal_io::activity::{ActivityAdmissionError, TeardownLease};
use solstone_core_journal_io::media::{
    ActivityCarrierRetirementEffect, Durability, MediaPrimitive, run_with_media_hook,
};
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};

struct FixedClock;
impl Clock for FixedClock {
    fn now_unix(&self) -> i64 {
        10_000
    }
    fn iso_week(&self) -> u8 {
        1
    }
}

fn fixture() -> (
    tempfile::TempDir,
    Arc<JournalRoot>,
    Arc<dyn Clock + Send + Sync>,
) {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("config")).unwrap();
    fs::write(
        temp.path().join("config/journal.json"),
        br#"{"backup":{"enabled":true,"mode":"byo"}}"#,
    )
    .unwrap();
    let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
    (temp, root, Arc::new(FixedClock))
}

#[cfg(target_os = "linux")]
#[test]
fn unsupported_origin_keeps_original_closed_preparation() {
    let temp = tempfile::Builder::new()
        .prefix("archive-origin-unavailable-")
        .tempdir_in("/dev/shm")
        .unwrap();
    fs::create_dir(temp.path().join("config")).unwrap();
    fs::write(
        temp.path().join("config/journal.json"),
        br#"{"backup":{"enabled":true,"mode":"byo"}}"#,
    )
    .unwrap();
    let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
    let clock: Arc<dyn Clock + Send + Sync> = Arc::new(FixedClock);
    let prepared = OwnedPreparedArchiveOperation::prepare(
        Arc::clone(&root),
        ArchiveBudget::new(clock.as_ref()).unwrap(),
        Arc::clone(&clock),
    )
    .unwrap_or_else(|e| panic!("ordinary preparation refused: {}", e.reason()));
    let origin = prepared.as_prepared().activity.origin().unwrap_err();
    assert!(matches!(
        origin.error(),
        solstone_core_journal_io::archive_origin::OriginError::Unsupported(_)
    ));
    assert!(Arc::ptr_eq(origin.root(), &root));
    let hold = prepared.into_owned_hold();
    let hold = std::thread::spawn(move || {
        let origin = hold.activity.origin().unwrap_err();
        assert!(Arc::ptr_eq(origin.root(), hold.journal_root()));
        busy(hold.journal_root());
        hold
    })
    .join()
    .unwrap();
    assert!(Arc::ptr_eq(hold.journal_root(), &root));
    busy(&root);
    drop(hold);
}

fn busy(root: &Arc<JournalRoot>) {
    assert!(matches!(
        ActivityLease::try_acquire(Arc::clone(root)),
        Err(ActivityAdmissionError::Busy)
    ));
    assert!(matches!(
        TeardownLease::try_acquire(Arc::clone(root)),
        Err(ActivityAdmissionError::Busy)
    ));
}

fn capture(
    root: &Arc<JournalRoot>,
    clock: &Arc<dyn Clock + Send + Sync>,
) -> (OwnedArchivePreparationInput, ArchivePreparationCustody) {
    match OwnedArchivePreparationInput::capture(
        Arc::clone(root),
        ArchiveBudget::new(clock.as_ref()).unwrap(),
        Arc::clone(clock),
    ) {
        Ok(pair) => pair,
        Err(failure) => panic!("capture: {}", failure.reason()),
    }
}

#[test]
fn custody_keeps_original_lease_without_blocking_carrier_unwrap_and_retirement() {
    let (_temp, root, clock) = fixture();
    let (input, custody) = capture(&root, &clock);
    assert!(Arc::ptr_eq(custody.journal_root(), &root));
    assert!(Arc::ptr_eq(custody.owned_clock(), &clock));
    busy(&root);
    let prepared = match input.prepare() {
        Ok(prepared) => prepared,
        Err(failure) => panic!("prepare: {}", failure.reason()),
    };
    assert!(std::ptr::eq(
        custody.activity(),
        prepared.activity().as_ref()
    ));
    assert!(Arc::ptr_eq(prepared.owned_clock(), &clock));
    let (
        PreparedArchiveOperation {
            activity,
            configuration,
            budget,
            clock: original_clock,
        },
        owned_clock,
    ) = prepared.into_parts();
    drop((configuration, budget, original_clock, owned_clock));
    let ArchiveActivity { carrier, origin } = activity;
    drop(origin);
    let carrier = match Arc::try_unwrap(carrier) {
        Ok(carrier) => carrier,
        Err(_) => panic!("outside custody must not retain a carrier share"),
    };
    // This controlled local fixture has no children or other namespace actors.
    let retirement = carrier.retire_after_settlement();
    assert_eq!(
        retirement.receipt.effect(),
        ActivityCarrierRetirementEffect::Removed
    );
    assert_eq!(retirement.receipt.root_durability(), Durability::Synced);
    assert!(retirement.receipt.failures().is_empty());
    assert!(std::ptr::eq(
        custody.activity(),
        retirement.owned.activity().as_ref()
    ));
    drop(retirement);
    busy(&root);
    custody.revalidate().unwrap();
    drop(custody);
    ActivityLease::try_acquire(root).unwrap();
}

#[test]
fn preparation_unwind_keeps_real_stage_and_original_exclusion_across_thread_return() {
    let (_temp, root, clock) = fixture();
    let (input, custody) = capture(&root, &clock);
    let reached = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&reached);
    let panic = catch_unwind(AssertUnwindSafe(|| {
        run_with_media_hook(
            MediaPrimitive::CarrierRootSync,
            move || {
                observed.store(true, Ordering::SeqCst);
                panic!("after actual stage creation");
            },
            || input.prepare(),
        )
    }));
    assert!(panic.is_err());
    assert!(reached.load(Ordering::SeqCst));
    assert!(!observe_obligations(&root).unwrap().entries().is_empty());
    let custody = std::thread::spawn(move || custody).join().unwrap();
    assert!(Arc::ptr_eq(custody.journal_root(), &root));
    assert!(Arc::ptr_eq(custody.owned_clock(), &clock));
    custody.revalidate().unwrap();
    busy(&root);
}

#[test]
fn cancellation_preserves_prelease_and_postconfiguration_original_failure_graphs() {
    let (_temp, root, clock) = fixture();
    let cancellation = ArchiveCancellation::new();
    let budget = ArchiveBudget::new_cancellable(clock.as_ref(), cancellation.clone()).unwrap();
    cancellation.cancel();
    let failure = match OwnedArchivePreparationInput::capture(
        Arc::clone(&root),
        budget,
        Arc::clone(&clock),
    ) {
        Err(failure) => failure,
        Ok(_) => panic!("prelease cancellation must refuse"),
    };
    assert_eq!(failure.reason(), AdmissionError::Cancelled);
    assert!(failure.preparation().is_none());
    assert!(failure.carrier().is_none());
    assert!(Arc::ptr_eq(failure.owned_clock(), &clock));
    drop(ActivityLease::try_acquire(Arc::clone(&root)).unwrap());
    let cancellation = ArchiveCancellation::new();
    let budget = ArchiveBudget::new_cancellable(clock.as_ref(), cancellation.clone()).unwrap();
    let (input, custody) = match OwnedArchivePreparationInput::capture(
        Arc::clone(&root),
        budget,
        Arc::clone(&clock),
    ) {
        Ok(pair) => pair,
        Err(failure) => panic!("capture: {}", failure.reason()),
    };
    let (prepared, consumed) = run_with_media_hook(
        MediaPrimitive::CarrierConfirmation,
        move || {
            cancellation.cancel();
            Ok(())
        },
        || input.prepare(),
    );
    assert!(consumed);
    let failure = match prepared {
        Err(failure) => failure,
        Ok(_) => panic!("original cancellation must survive config capture"),
    };
    assert_eq!(failure.reason(), AdmissionError::Cancelled);
    assert!(failure.configuration().is_some());
    assert!(std::ptr::eq(
        custody.activity(),
        failure.carrier().unwrap().activity().as_ref()
    ));
    assert!(Arc::ptr_eq(failure.owned_clock(), &clock));
    assert!(!observe_obligations(&root).unwrap().entries().is_empty());
    let held = std::thread::spawn(move || (failure, custody))
        .join()
        .unwrap();
    assert!(Arc::ptr_eq(held.0.journal_root(), &root));
    busy(&root);
}

#[test]
fn lock_replacement_refuses_original_custody_and_cannot_alias_fresh_allocation() {
    use std::os::unix::fs::PermissionsExt;
    let (temp, root, clock) = fixture();
    let (input, custody) = capture(&root, &clock);
    let original = input.into_owned_hold();
    fs::rename(
        temp.path().join("backup-activity.lock"),
        temp.path().join("old.lock"),
    )
    .unwrap();
    fs::write(temp.path().join("backup-activity.lock"), b"replacement").unwrap();
    fs::set_permissions(
        temp.path().join("backup-activity.lock"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let (fresh, fresh_custody) = capture(&root, &clock);
    let fresh = fresh.into_owned_hold();
    assert!(custody.revalidate().is_err());
    fresh_custody.revalidate().unwrap();
    assert!(std::ptr::eq(custody.activity(), original.activity()));
    assert!(!std::ptr::eq(custody.activity(), fresh.activity()));
    assert!(Arc::ptr_eq(
        custody.journal_root(),
        fresh_custody.journal_root()
    ));
    assert!(Arc::ptr_eq(
        custody.owned_clock(),
        fresh_custody.owned_clock()
    ));
    assert_eq!(fs::read(temp.path().join("old.lock")).unwrap(), b"");
    assert_eq!(
        fs::read(temp.path().join("backup-activity.lock")).unwrap(),
        b"replacement"
    );
    busy(&root);
}
