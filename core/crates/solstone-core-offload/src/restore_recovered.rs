// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Immutable-record register recovery. This phase never retires intent or media.

use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use solstone_core_journal_io::LockOptions;
use solstone_core_journal_io::activity_obligations::NativeObligations;
use solstone_core_journal_io::archive_recovery::{
    RecoveredArchiveReservation, RecoveryPopulationFailure,
};
use solstone_core_journal_io::media::MediaDirectory;
use solstone_core_retention::archive_catalog::RestoreDisposition;
#[cfg(any(target_os = "linux", test))]
use solstone_core_retention::mark_mutation::MutationStatus;
use solstone_core_retention::mark_mutation::{BoundMutationOutcome, RecoveredRestoredMarkRefusal};
use solstone_core_retention::restore_intent::{RepairHold, RestoreIntent};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveredRestoreState {
    Held,
    Unknown,
    RegisterConfirmed,
}

#[derive(Debug)]
pub enum RecoveredRestoreRefusal {
    Unsupported,
    Population,
    Intent(RepairHold),
    Claims(io::Error),
    Destination(io::Error),
    Register,
    Unwind,
}

/// Keeps the same physical reservation and every returned effect. Confirmation
/// here grants no intent removal, stage cleanup, reservation retirement or retry.
#[must_use = "Retain the whole recovered result and its original reservation"]
pub struct RecoveredRestoreOutcome {
    reservation: RecoveredArchiveReservation,
    directory: Option<Arc<MediaDirectory>>,
    intent: Option<RestoreIntent>,
    populations: Vec<Result<NativeObligations, Box<RecoveryPopulationFailure>>>,
    register_attempted: bool,
    register: Option<Result<BoundMutationOutcome, RecoveredRestoredMarkRefusal>>,
    state: RecoveredRestoreState,
    refusal: Option<RecoveredRestoreRefusal>,
}

impl RecoveredRestoreOutcome {
    pub fn state(&self) -> RecoveredRestoreState {
        self.state
    }
    pub fn disposition(&self) -> RestoreDisposition {
        if self.state == RecoveredRestoreState::RegisterConfirmed {
            RestoreDisposition::AlreadyPresent
        } else {
            RestoreDisposition::Unverified
        }
    }
    pub fn files_restored(&self) -> u64 {
        0
    }
    pub fn bytes_restored(&self) -> u64 {
        0
    }
    pub fn reservation(&self) -> &RecoveredArchiveReservation {
        &self.reservation
    }
    pub fn directory(&self) -> Option<&Arc<MediaDirectory>> {
        self.directory.as_ref()
    }
    pub fn intent(&self) -> Option<&RestoreIntent> {
        self.intent.as_ref()
    }
    pub fn populations(&self) -> &[Result<NativeObligations, Box<RecoveryPopulationFailure>>] {
        &self.populations
    }
    pub fn register_attempted(&self) -> bool {
        self.register_attempted
    }
    pub fn register(&self) -> Option<&Result<BoundMutationOutcome, RecoveredRestoredMarkRefusal>> {
        self.register.as_ref()
    }
    pub fn refusal(&self) -> Option<&RecoveredRestoreRefusal> {
        self.refusal.as_ref()
    }
}

/// Consume only an actually admitted physical owner; decoded observations cannot
/// construct this input. The original named record remains immutable throughout.
pub fn recover_restored_register(
    reservation: RecoveredArchiveReservation,
    options: LockOptions,
) -> RecoveredRestoreOutcome {
    let mut result = RecoveredRestoreOutcome {
        reservation,
        directory: None,
        intent: None,
        populations: Vec::new(),
        register_attempted: false,
        register: None,
        state: RecoveredRestoreState::Held,
        refusal: None,
    };
    let attempted = catch_unwind(AssertUnwindSafe(|| execute(&mut result, options)));
    match attempted {
        Ok(Ok(())) => result.state = RecoveredRestoreState::RegisterConfirmed,
        Ok(Err(refusal)) => result.refusal = Some(refusal),
        Err(_) => {
            result.state = RecoveredRestoreState::Unknown;
            result.refusal = Some(RecoveredRestoreRefusal::Unwind);
        }
    }
    result
}

#[cfg(any(target_os = "linux", test))]
fn register_confirmed(result: &Result<BoundMutationOutcome, RecoveredRestoredMarkRefusal>) -> bool {
    matches!(result,
        Ok(BoundMutationOutcome::Published(value) | BoundMutationOutcome::Unchanged(value))
            if matches!(value.status, MutationStatus::Applied | MutationStatus::AlreadyApplied))
}

#[cfg(not(target_os = "linux"))]
fn execute(_: &mut RecoveredRestoreOutcome, _: LockOptions) -> Result<(), RecoveredRestoreRefusal> {
    Err(RecoveredRestoreRefusal::Unsupported)
}

#[cfg(target_os = "linux")]
fn execute(
    result: &mut RecoveredRestoreOutcome,
    options: LockOptions,
) -> Result<(), RecoveredRestoreRefusal> {
    use solstone_core_retention::mark_mutation::{
        RecoveredRestoredMarkMutation, conditional_retire_recovered_restored,
    };
    use solstone_core_segment::output_attempt::OutputClaims;

    population(result)?;
    let original = result.reservation.record();
    if !original.complete {
        return Err(RecoveredRestoreRefusal::Intent(
            RepairHold::UnreadableIntent,
        ));
    }
    result.intent =
        Some(RestoreIntent::decode(Ok(&original.bytes)).map_err(RecoveredRestoreRefusal::Intent)?);
    let intent = result.intent.as_ref().expect("decoded original record");
    let root = Arc::clone(result.reservation.root());
    let outputs = intent.output_paths(root.canonical_path());
    let directory = MediaDirectory::open(Arc::clone(&root), &intent.locator.relative_segment())
        .map_err(RecoveredRestoreRefusal::Destination)?;
    result.directory = Some(Arc::clone(&directory));
    let claims =
        OutputClaims::acquire(outputs.clone(), options).map_err(RecoveredRestoreRefusal::Claims)?;
    let commit = claims
        .commit(directory.diagnostic_path())
        .map_err(RecoveredRestoreRefusal::Claims)?;
    commit
        .require_outputs(&claims, directory.diagnostic_path(), outputs.clone())
        .map_err(RecoveredRestoreRefusal::Claims)?;
    if claims
        .segment_identity(directory.diagnostic_path())
        .map_err(RecoveredRestoreRefusal::Claims)?
        != directory.object_identity()
    {
        return Err(RecoveredRestoreRefusal::Intent(
            RepairHold::DestinationChanged,
        ));
    }
    directory
        .revalidate()
        .map_err(RecoveredRestoreRefusal::Destination)?;
    result.register_attempted = true;
    result.register = Some(conditional_retire_recovered_restored(
        RecoveredRestoredMarkMutation {
            reservation: &result.reservation,
            claims: &claims,
            commit: &commit,
        },
    ));
    // Store the whole result before another fallible guard. A late refusal can
    // never erase actual publication or Proof's PostMutation outcome/raw cause.
    population(result)?;
    commit
        .require_outputs(&claims, directory.diagnostic_path(), outputs)
        .map_err(RecoveredRestoreRefusal::Claims)?;
    directory
        .revalidate()
        .map_err(RecoveredRestoreRefusal::Destination)?;
    if !register_confirmed(result.register.as_ref().expect("actual CAS returned")) {
        return Err(RecoveredRestoreRefusal::Register);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn population(result: &mut RecoveredRestoreOutcome) -> Result<(), RecoveredRestoreRefusal> {
    let observed = result.reservation.revalidate_current_population();
    let complete = observed.is_ok();
    result.populations.push(observed);
    if complete {
        Ok(())
    } else {
        Err(RecoveredRestoreRefusal::Population)
    }
}

#[cfg(test)]
mod tests;
