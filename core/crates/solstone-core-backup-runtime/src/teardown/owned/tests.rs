// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use std::sync::atomic::{AtomicI64, Ordering};

struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn now_unix(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
    fn iso_week(&self) -> u8 {
        1
    }
}

#[test]
fn original_two_hour_budget_expiry_backward_clock_and_cancel_do_not_renew() {
    let clock = Arc::new(TestClock(AtomicI64::new(1000)));
    let mut operation = TeardownOperation::new(clock.clone(), ArchiveCancellation::new()).unwrap();
    assert!(operation.remaining().unwrap() <= Duration::from_secs(7200));
    clock.0.store(2000, Ordering::SeqCst);
    assert!(operation.remaining().unwrap() <= Duration::from_secs(6200));
    operation.budget.cap_credential_expiry(2100);
    assert_eq!(operation.remaining().unwrap(), Duration::from_secs(100));
    operation.budget.cap_credential_expiry(9999);
    assert_eq!(operation.remaining().unwrap(), Duration::from_secs(100));
    clock.0.store(1999, Ordering::SeqCst);
    assert_eq!(operation.remaining(), Err(AdmissionError::ClockChanged));
    clock.0.store(2001, Ordering::SeqCst);
    assert_eq!(operation.remaining(), Err(AdmissionError::ClockChanged));
    operation.cancellation.cancel();
    assert_eq!(operation.remaining(), Err(AdmissionError::Cancelled));

    let clock = Arc::new(TestClock(AtomicI64::new(1000)));
    let mut operation = TeardownOperation::new(clock.clone(), ArchiveCancellation::new()).unwrap();
    clock.0.store(8200, Ordering::SeqCst);
    assert!(operation.remaining().is_err());
    let clock = Arc::new(TestClock(AtomicI64::new(1000)));
    operation = TeardownOperation::new(clock, ArchiveCancellation::new()).unwrap();
    operation.deadline = Instant::now();
    assert_eq!(operation.remaining(), Err(AdmissionError::DeadlineExceeded));
    fn require_send_static<T: Send + 'static>() {}
    require_send_static::<TeardownOperation>();
}

#[test]
fn fresh_operation_preserves_prior_retention_without_claiming_current_remote_work() {
    let prior = RemoteFacts {
        version: 1,
        effect: RemoteEffect::SnapshotsPruned {
            admitted_snapshot_count: 4,
            remaining_snapshots: 0,
        },
        storage_retention: Some(StorageRetention::Keeps(StorageKeeps::Versions)),
    };
    let bytes = serde_json::to_vec(&prior).unwrap();
    let decoded = serde_json::from_slice(&bytes).unwrap();
    let clock = Arc::new(TestClock(AtomicI64::new(1000)));
    let mut operation = TeardownOperation::new(clock, ArchiveCancellation::new()).unwrap();
    operation.restore_prior_facts(decoded);
    let report = operation.into_report();
    assert_eq!(report.previous(), Some(&prior));
    assert_eq!(
        report.storage_retention(),
        Some(StorageRetention::Keeps(StorageKeeps::Versions))
    );
    assert_eq!(report.phase(), RemoteTeardownPhase::NotStarted);
    assert!(!report.broker_attempted());
    assert_eq!((report.delete_attempts(), report.abort_attempts()), (0, 0));
    assert_eq!(report.current_storage_retention(), None);
    assert_eq!(report.admitted_snapshot_count(), None);
    assert!(report.confirmed_facts().is_none());
}
