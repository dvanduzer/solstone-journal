// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use serde_json::json;
use solstone_core_journal_io::activity::ActivityLease;
use solstone_core_journal_io::activity_obligations::{observe_obligations, observe_with_carrier};
use solstone_core_journal_io::{
    ClaimRemovalOutcome, ClaimRemovalPrimitive, run_with_claim_removal_fault,
};
use std::fs;

fn fixture() -> (tempfile::TempDir, Arc<JournalRoot>) {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("config")).unwrap();
    fs::write(
        temp.path().join("config/journal.json"),
        serde_json::to_vec(&json!({
            "backup":{"enabled":true,"mode":"operated"}, "other":7
        }))
        .unwrap(),
    )
    .unwrap();
    solstone_core_backup::save_hosted_binding(
        temp.path(),
        &solstone_core_backup::HostedBinding {
            broker_endpoint: "https://example.invalid".into(),
            account_id: "account".into(),
            instance_id: "instance".into(),
            bucket: "bucket".into(),
            prefix: "prefix/".into(),
            broker_token: "secret".into(),
        },
    )
    .unwrap();
    let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
    (temp, root)
}

fn coverage(
    root: &Arc<JournalRoot>,
    _: &TeardownConfiguration,
    carrier: Option<RootCarrierObservation<'_>>,
) -> Result<(), TeardownReaderRefusal> {
    let held = match carrier {
        Some(carrier) => observe_with_carrier(root, carrier)
            .map_err(|_| TeardownReaderRefusal::RecoveryUnavailable)?
            .unresolved_entries()
            .next()
            .is_some(),
        None => !observe_obligations(root)
            .map_err(|_| TeardownReaderRefusal::RecoveryUnavailable)?
            .entries()
            .is_empty(),
    };
    if held {
        Err(TeardownReaderRefusal::RecoveryHeld)
    } else {
        Ok(())
    }
}

fn cleared(root: &Arc<JournalRoot>) -> ClearedTeardownAdmission {
    admit_teardown(root.clone())
        .unwrap()
        .prepare_carrier(&mut coverage)
        .unwrap()
        .clear_configuration(&mut coverage)
        .unwrap()
}

fn assert_busy(root: &Arc<JournalRoot>) {
    assert!(matches!(
        ActivityLease::try_acquire(root.clone()),
        Err(ActivityAdmissionError::Busy)
    ));
    assert!(matches!(
        TeardownLease::try_acquire(root.clone()),
        Err(ActivityAdmissionError::Busy)
    ));
}

#[test]
fn consumed_local_phases_keep_same_carrier_publication_and_actual_receipt() {
    let (temp, root) = fixture();
    let cleared = cleared(&root);
    let outside_carrier = cleared.carrier().clone();
    let outside_lease = cleared.teardown_lease().clone();
    let publication = cleared.publication_receipt().clone();
    let reservation = outside_carrier.diagnostic_path();
    let guarded = cleared.begin_binding_finalization(&mut coverage).unwrap();
    assert!(Arc::ptr_eq(guarded.carrier(), &outside_carrier));
    let bound = guarded.remove_binding(&mut coverage).unwrap();
    assert!(Arc::ptr_eq(bound.carrier(), &outside_carrier));
    assert!(Arc::ptr_eq(bound.teardown_lease(), &outside_lease));
    assert!(Arc::ptr_eq(bound.publication(), &publication));
    assert!(bound.binding_receipt().complete());
    assert!(!temp.path().join("backup/hosted/binding.json").exists());
    bound.check(&mut coverage).unwrap();
    assert_busy(&root);
    drop(bound);
    assert_busy(&root);
    drop(outside_carrier);
    drop(outside_lease);
    drop(TeardownLease::try_acquire(root).unwrap());
    assert!(
        reservation.is_dir(),
        "local phase does not retire its reservation"
    );
    fn send_static<T: Send + 'static>() {}
    send_static::<BoundTeardownAdmission>();
    send_static::<TeardownBindingFailure>();
}

#[test]
fn late_obligation_before_or_after_binding_attempt_retains_original_transition() {
    for after_removal in [false, true] {
        let (temp, root) = fixture();
        let cleared = cleared(&root);
        let publication = cleared.publication_receipt().clone();
        let guarded = cleared.begin_binding_finalization(&mut coverage).unwrap();
        let binding = temp.path().join("backup/hosted/binding.json");
        let late = temp.path().join(".tmp-offload-late");
        if !after_removal {
            fs::create_dir(&late).unwrap();
        }
        let mut inserted = false;
        let failure = guarded
            .remove_binding(&mut |root, config, carrier| {
                if after_removal && !binding.exists() && !inserted {
                    fs::create_dir(&late).unwrap();
                    inserted = true;
                }
                coverage(root, config, carrier)
            })
            .err()
            .unwrap();
        assert!(Arc::ptr_eq(failure.publication(), &publication));
        assert_eq!(binding.exists(), !after_removal);
        assert_eq!(failure.binding_receipt().is_some(), after_removal);
        if after_removal {
            assert!(failure.binding_receipt().unwrap().complete());
        }
        assert!(matches!(
            failure.refusal(),
            TeardownBindingRefusal::Admission(TeardownAdmissionRefusal::Reader(
                TeardownReaderRefusal::RecoveryHeld
            ))
        ));
        assert_busy(&root);
        assert!(late.is_dir());
    }
}

#[test]
fn native_sync_uncertainty_preserves_clear_and_actual_removal_without_bound_phase() {
    let (temp, root) = fixture();
    let cleared = cleared(&root);
    let publication = cleared.publication_receipt().clone();
    let guarded = cleared.begin_binding_finalization(&mut coverage).unwrap();
    let (result, fired) = run_with_claim_removal_fault(
        ClaimRemovalPrimitive::DirectorySync,
        1,
        nix::libc::EIO,
        || guarded.remove_binding(&mut coverage),
    );
    assert!(fired);
    let failure = result.err().unwrap();
    assert!(Arc::ptr_eq(failure.publication(), &publication));
    let receipt = failure.binding_receipt().unwrap();
    assert!(receipt.attempted());
    assert!(matches!(
        receipt.native(),
        Some(Ok(ClaimRemovalOutcome::RemovedDurabilityUncertain))
    ));
    assert!(receipt.confirmation().is_ok());
    assert!(!receipt.complete());
    assert!(matches!(failure.refusal(), TeardownBindingRefusal::Removal));
    assert!(!temp.path().join("backup/hosted/binding.json").exists());
    assert_busy(&root);
}
