// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! One concrete teardown owner across remote work, local effects and final custody.

use std::path::Path;
use std::sync::Arc;

use solstone_core_backup::archive_admission::{AdmissionError, ArchiveCancellation, Clock};
#[cfg(any(unix, windows))]
use solstone_core_backup::teardown_configuration::BindingRemovalReceipt;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "android",
    windows
))]
use solstone_core_backup_runtime::resolve_restic_member;
use solstone_core_backup_runtime::teardown::owned::{
    RemoteTeardownReport, TeardownOperation, TeardownRemoteRefusal,
};
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "android",
    windows
))]
use solstone_core_backup_runtime::teardown_admission::admit_teardown;
#[cfg(any(unix, windows))]
use solstone_core_backup_runtime::teardown_admission::{
    BindingTeardownAdmission, BoundTeardownAdmission, TeardownBindingFailure,
    TeardownBindingRefusal, TeardownFinishAdmission,
};
use solstone_core_backup_runtime::teardown_admission::{
    CarrierTeardownAdmission, ClearedTeardownAdmission, TeardownAdmission,
    TeardownAdmissionRefusal, TeardownCarrierFailure, TeardownClearFailure,
};
use solstone_core_journal_config_write::BoundConfigPublication;
use solstone_core_journal_io::activity::TeardownLease;
use solstone_core_journal_io::media::{
    ActivityCarrierRetirement, ActivityCarrierRetirementReceipt, RootTeardownCarrier,
};
use solstone_core_journal_io::{JournalRoot, JournalRootError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeardownRunPhase {
    Prelaunch,
    Admission,
    Preparation,
    Remote,
    Clear,
    Binding,
    FinishAdmission,
    CarrierTransfer,
    Retirement,
    FinalChecks,
    Finished,
}

pub enum TeardownRunRefusal {
    UnsupportedPlatform,
    UnsupportedDestination,
    /// Own storage only: the package's restic could not be found.
    ToolUnavailable,
    Root(JournalRootError),
    Budget(AdmissionError),
    Admission(TeardownAdmissionRefusal),
    CarrierPreparation,
    Remote(TeardownRemoteRefusal),
    Clear,
    Binding,
    #[cfg(any(unix, windows))]
    Finish(TeardownBindingRefusal),
    CarrierShared,
    RetirementUncertain,
}

/// Calls submitted to an owning local phase, independent of native effect receipts.
#[derive(Default, Debug)]
pub struct TeardownAttemptJournal {
    clear: bool,
    binding: bool,
    retirement: bool,
}

impl TeardownAttemptJournal {
    pub fn clear(&self) -> bool {
        self.clear
    }
    pub fn binding(&self) -> bool {
        self.binding
    }
    pub fn retirement(&self) -> bool {
        self.retirement
    }
}

/// Actual resource-free observations. Success releases the guards and lease;
/// these receipts cannot construct another admission or authorize any effect.
pub struct SettledTeardownReport {
    remote: RemoteTeardownReport,
    attempts: TeardownAttemptJournal,
    publication: Arc<BoundConfigPublication>,
    /// None for own storage, which has no hosted binding.
    #[cfg(any(unix, windows))]
    binding: Option<Arc<BindingRemovalReceipt>>,
    retirement: ActivityCarrierRetirementReceipt,
}

impl SettledTeardownReport {
    pub fn remote(&self) -> &RemoteTeardownReport {
        &self.remote
    }
    pub fn attempts(&self) -> &TeardownAttemptJournal {
        &self.attempts
    }
    pub fn publication(&self) -> &BoundConfigPublication {
        &self.publication
    }
    #[cfg(any(unix, windows))]
    pub fn binding(&self) -> Option<&BindingRemovalReceipt> {
        self.binding.as_deref()
    }
    pub fn retirement(&self) -> &ActivityCarrierRetirementReceipt {
        &self.retirement
    }
}

struct Outside {
    _clock: Arc<dyn Clock + Send + Sync>,
    cancellation: ArchiveCancellation,
    operation: Option<TeardownOperation>,
    root: Option<Arc<JournalRoot>>,
    lease: Option<Arc<TeardownLease>>,
    carrier: Option<Arc<RootTeardownCarrier>>,
    attempts: TeardownAttemptJournal,
    publication: Option<Arc<BoundConfigPublication>>,
    #[cfg(any(unix, windows))]
    binding: Option<Arc<BindingRemovalReceipt>>,
    retirement: Option<ActivityCarrierRetirement<RootTeardownCarrier>>,
}

impl Outside {
    fn new(clock: Arc<dyn Clock + Send + Sync>, cancellation: ArchiveCancellation) -> Self {
        Self {
            _clock: clock,
            cancellation,
            operation: None,
            root: None,
            lease: None,
            carrier: None,
            attempts: TeardownAttemptJournal::default(),
            publication: None,
            #[cfg(any(unix, windows))]
            binding: None,
            retirement: None,
        }
    }
    #[cfg(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "android",
        windows
    ))]
    fn remaining(&self) -> Result<(), TeardownRunRefusal> {
        self.operation
            .as_ref()
            .expect("original operation")
            .remaining()
            .map(|_| ())
            .map_err(TeardownRunRefusal::Budget)
    }
}

// Refused platforms expose the same closed custody views without constructing
// executing phases.
#[cfg_attr(
    not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "android",
        windows
    )),
    allow(dead_code)
)]
enum PhaseOwner {
    Admission(Box<TeardownAdmission>),
    CarrierFailure(Box<TeardownCarrierFailure>),
    Carrier(Box<CarrierTeardownAdmission>),
    ClearFailure(Box<TeardownClearFailure>),
    Cleared(Box<ClearedTeardownAdmission>),
    #[cfg(any(unix, windows))]
    Guarded(Box<BindingTeardownAdmission>),
    #[cfg(any(unix, windows))]
    Bound(Box<BoundTeardownAdmission>),
    #[cfg(any(unix, windows))]
    BindingFailure(Box<TeardownBindingFailure>),
    #[cfg(any(unix, windows))]
    Finish(Box<TeardownFinishAdmission>),
    #[cfg(any(unix, windows))]
    CarrierMismatch {
        finish: Box<TeardownFinishAdmission>,
        operational: Arc<RootTeardownCarrier>,
    },
}

pub struct HeldTeardown {
    phase: TeardownRunPhase,
    refusal: Option<TeardownRunRefusal>,
    outside: Outside,
    owner: Option<PhaseOwner>,
}

/// Retain the whole finite value through caller projection and guarded handoff.
/// Unknown never reconstructs an owner consumed before an unwind.
#[must_use]
pub enum OwnedTeardownOutcome {
    Finished(Box<SettledTeardownReport>),
    Held(Box<HeldTeardown>),
    Unknown(Box<HeldTeardown>),
}

impl OwnedTeardownOutcome {
    pub fn phase(&self) -> TeardownRunPhase {
        match self {
            Self::Finished(_) => TeardownRunPhase::Finished,
            Self::Held(held) | Self::Unknown(held) => held.phase,
        }
    }
    pub fn settled(&self) -> Option<&SettledTeardownReport> {
        if let Self::Finished(report) = self {
            Some(report)
        } else {
            None
        }
    }
    pub fn refusal(&self) -> Option<&TeardownRunRefusal> {
        self.held().and_then(|held| held.refusal.as_ref())
    }
    pub fn remote(&self) -> Option<&RemoteTeardownReport> {
        match self {
            Self::Finished(report) => Some(report.remote()),
            _ => self
                .held()?
                .outside
                .operation
                .as_ref()
                .map(TeardownOperation::report),
        }
    }
    pub fn attempts(&self) -> &TeardownAttemptJournal {
        match self {
            Self::Finished(report) => report.attempts(),
            Self::Held(held) | Self::Unknown(held) => &held.outside.attempts,
        }
    }
    pub fn publication(&self) -> Option<&BoundConfigPublication> {
        match self {
            Self::Finished(report) => Some(report.publication()),
            _ => self.held()?.outside.publication.as_deref(),
        }
    }
    #[cfg(any(unix, windows))]
    pub fn binding(&self) -> Option<&BindingRemovalReceipt> {
        match self {
            Self::Finished(report) => report.binding(),
            _ => self.held()?.outside.binding.as_deref(),
        }
    }
    pub fn retirement(&self) -> Option<&ActivityCarrierRetirementReceipt> {
        match self {
            Self::Finished(report) => Some(report.retirement()),
            _ => self
                .held()?
                .outside
                .retirement
                .as_ref()
                .map(|retirement| &retirement.receipt),
        }
    }
    pub fn journal_root(&self) -> Option<&Arc<JournalRoot>> {
        self.held()?.outside.root.as_ref()
    }
    pub fn teardown_lease(&self) -> Option<&Arc<TeardownLease>> {
        self.held()?.outside.lease.as_ref()
    }
    pub fn carrier(&self) -> Option<&Arc<RootTeardownCarrier>> {
        self.held()?.outside.carrier.as_ref()
    }
    pub fn cancellation(&self) -> Option<&ArchiveCancellation> {
        Some(&self.held()?.outside.cancellation)
    }
    pub fn preparation_failure(&self) -> Option<&TeardownCarrierFailure> {
        match self.held()?.owner.as_ref()? {
            PhaseOwner::CarrierFailure(owner) => Some(owner),
            _ => None,
        }
    }
    pub fn admitted(&self) -> Option<&TeardownAdmission> {
        match self.held()?.owner.as_ref()? {
            PhaseOwner::Admission(owner) => Some(owner),
            _ => None,
        }
    }
    pub fn carrier_admission(&self) -> Option<&CarrierTeardownAdmission> {
        match self.held()?.owner.as_ref()? {
            PhaseOwner::Carrier(owner) => Some(owner),
            _ => None,
        }
    }
    pub fn cleared(&self) -> Option<&ClearedTeardownAdmission> {
        match self.held()?.owner.as_ref()? {
            PhaseOwner::Cleared(owner) => Some(owner),
            _ => None,
        }
    }
    #[cfg(any(unix, windows))]
    pub fn guarded(&self) -> Option<&BindingTeardownAdmission> {
        match self.held()?.owner.as_ref()? {
            PhaseOwner::Guarded(owner) => Some(owner),
            _ => None,
        }
    }
    #[cfg(any(unix, windows))]
    pub fn bound(&self) -> Option<&BoundTeardownAdmission> {
        match self.held()?.owner.as_ref()? {
            PhaseOwner::Bound(owner) => Some(owner),
            _ => None,
        }
    }
    #[cfg(any(unix, windows))]
    pub fn finish_admission(&self) -> Option<&TeardownFinishAdmission> {
        match self.held()?.owner.as_ref()? {
            PhaseOwner::Finish(owner) => Some(owner),
            PhaseOwner::CarrierMismatch { finish, .. } => Some(finish),
            _ => None,
        }
    }
    #[cfg(any(unix, windows))]
    pub fn additional_carrier(&self) -> Option<&Arc<RootTeardownCarrier>> {
        match self.held()?.owner.as_ref()? {
            PhaseOwner::CarrierMismatch { operational, .. } => Some(operational),
            _ => None,
        }
    }
    pub fn clear_failure(&self) -> Option<&TeardownClearFailure> {
        match self.held()?.owner.as_ref()? {
            PhaseOwner::ClearFailure(owner) => Some(owner),
            _ => None,
        }
    }
    #[cfg(any(unix, windows))]
    pub fn binding_failure(&self) -> Option<&TeardownBindingFailure> {
        match self.held()?.owner.as_ref()? {
            PhaseOwner::BindingFailure(owner) => Some(owner),
            _ => None,
        }
    }
    /// Held before any carrier reservation, remote request or local effect.
    /// Dropping such an outcome only releases its admission; nothing needs recovery.
    pub fn refused_before_effects(&self) -> bool {
        use solstone_core_backup_runtime::teardown::owned::RemoteTeardownPhase;
        let Self::Held(held) = self else {
            return false;
        };
        let owner_unreserved = match held.owner.as_ref() {
            None | Some(PhaseOwner::Admission(_)) => true,
            Some(PhaseOwner::CarrierFailure(failure)) => {
                failure.preparation().is_none()
                    && failure.preparation_error().is_none()
                    && failure.reservation_path().is_none()
            }
            Some(_) => false,
        };
        let remote_untouched = self.remote().is_none_or(|remote| {
            remote.phase() == RemoteTeardownPhase::NotStarted && !remote.broker_attempted()
        });
        #[cfg(any(unix, windows))]
        let no_binding = held.outside.binding.is_none();
        #[cfg(not(any(unix, windows)))]
        let no_binding = true;
        owner_unreserved
            && remote_untouched
            && no_binding
            && held.outside.carrier.is_none()
            && held.outside.publication.is_none()
            && held.outside.retirement.is_none()
            && !held.outside.attempts.clear
            && !held.outside.attempts.binding
            && !held.outside.attempts.retirement
    }
    fn held(&self) -> Option<&HeldTeardown> {
        match self {
            Self::Held(held) | Self::Unknown(held) => Some(held),
            _ => None,
        }
    }
}

fn held(
    outside: Outside,
    owner: Option<PhaseOwner>,
    phase: TeardownRunPhase,
    refusal: TeardownRunRefusal,
) -> OwnedTeardownOutcome {
    OwnedTeardownOutcome::Held(Box::new(HeldTeardown {
        phase,
        refusal: Some(refusal),
        outside,
        owner,
    }))
}

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "android",
    windows
))]
fn unknown(
    outside: Outside,
    owner: Option<PhaseOwner>,
    phase: TeardownRunPhase,
) -> OwnedTeardownOutcome {
    OwnedTeardownOutcome::Unknown(Box::new(HeldTeardown {
        phase,
        refusal: None,
        outside,
        owner,
    }))
}

/// Every journal writer that could change cataloged media takes the same exclusive
/// activity lock this teardown holds, and the coverage reader runs again at each
/// destructive boundary.
pub fn run_owned_teardown(
    journal: &Path,
    clock: Arc<dyn Clock + Send + Sync>,
    cancellation: ArchiveCancellation,
    version: &str,
) -> OwnedTeardownOutcome {
    let outside = Outside::new(clock, cancellation);
    #[cfg(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "android",
        windows
    ))]
    {
        execute(journal, outside, version)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "android",
        windows
    )))]
    {
        let _ = (journal, version);
        held(
            outside,
            None,
            TeardownRunPhase::Prelaunch,
            TeardownRunRefusal::UnsupportedPlatform,
        )
    }
}

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "android",
    windows
))]
fn execute(journal: &Path, mut outside: Outside, version: &str) -> OwnedTeardownOutcome {
    use solstone_core_journal_io::media::{
        ActivityCarrierRetirementEffect, Confirmation, Durability,
    };
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let mut reader = super::read_teardown_coverage;
    outside.operation =
        match TeardownOperation::new(outside._clock.clone(), outside.cancellation.clone()) {
            Ok(operation) => Some(operation),
            Err(refusal) => {
                return held(
                    outside,
                    None,
                    TeardownRunPhase::Admission,
                    TeardownRunRefusal::Budget(refusal),
                );
            }
        };
    let root = match JournalRoot::open(journal) {
        Ok(root) => Arc::new(root),
        Err(refusal) => {
            return held(
                outside,
                None,
                TeardownRunPhase::Admission,
                TeardownRunRefusal::Root(refusal),
            );
        }
    };
    outside.root = Some(root.clone());
    let admitted = match admit_teardown(root) {
        Ok(admitted) => admitted,
        Err(refusal) => {
            return held(
                outside,
                None,
                TeardownRunPhase::Admission,
                TeardownRunRefusal::Admission(refusal),
            );
        }
    };
    outside.lease = Some(admitted.teardown_lease().clone());
    let operated = admitted
        .configuration()
        .backup()
        .get("mode")
        .and_then(serde_json::Value::as_str)
        == Some("operated");
    // Own storage is addressed by its destination and key; operated storage by its binding.
    let own_storage = if operated {
        if admitted.configuration().binding().is_none() {
            return held(
                outside,
                Some(PhaseOwner::Admission(Box::new(admitted))),
                TeardownRunPhase::Admission,
                TeardownRunRefusal::Binding,
            );
        }
        None
    } else {
        let addressable = admitted.configuration().binding().is_none()
            && admitted.configuration().destination().is_some()
            && admitted.configuration().keys().is_some();
        if !addressable {
            return held(
                outside,
                Some(PhaseOwner::Admission(Box::new(admitted))),
                TeardownRunPhase::Admission,
                TeardownRunRefusal::UnsupportedDestination,
            );
        }
        let restic = std::env::current_exe()
            .ok()
            .and_then(|executable| resolve_restic_member(&executable).ok());
        match restic {
            Some(restic) => Some(restic),
            None => {
                return held(
                    outside,
                    Some(PhaseOwner::Admission(Box::new(admitted))),
                    TeardownRunPhase::Admission,
                    TeardownRunRefusal::ToolUnavailable,
                );
            }
        }
    };
    if let Err(refusal) = outside.remaining() {
        return held(
            outside,
            Some(PhaseOwner::Admission(Box::new(admitted))),
            TeardownRunPhase::Preparation,
            refusal,
        );
    }
    let carrier = match catch_unwind(AssertUnwindSafe(|| admitted.prepare_carrier(&mut reader))) {
        Ok(Ok(carrier)) => carrier,
        Ok(Err(failure)) => {
            return held(
                outside,
                Some(PhaseOwner::CarrierFailure(Box::new(failure))),
                TeardownRunPhase::Preparation,
                TeardownRunRefusal::CarrierPreparation,
            );
        }
        Err(_) => return unknown(outside, None, TeardownRunPhase::Preparation),
    };
    outside.carrier = Some(carrier.carrier().clone());
    let remote = catch_unwind(AssertUnwindSafe(|| {
        outside
            .operation
            .as_mut()
            .expect("original operation")
            .run_remote_for(&carrier, &mut reader, version, own_storage.as_deref())
    }));
    match remote {
        Ok(Ok(_)) => {}
        Ok(Err(refusal)) => {
            return held(
                outside,
                Some(PhaseOwner::Carrier(Box::new(carrier))),
                TeardownRunPhase::Remote,
                TeardownRunRefusal::Remote(refusal),
            );
        }
        Err(_) => {
            return unknown(
                outside,
                Some(PhaseOwner::Carrier(Box::new(carrier))),
                TeardownRunPhase::Remote,
            );
        }
    }
    if catch_unwind(AssertUnwindSafe(|| test_cut(TeardownRunPhase::Remote))).is_err() {
        return unknown(
            outside,
            Some(PhaseOwner::Carrier(Box::new(carrier))),
            TeardownRunPhase::Remote,
        );
    }
    if let Err(refusal) = outside.remaining() {
        return held(
            outside,
            Some(PhaseOwner::Carrier(Box::new(carrier))),
            TeardownRunPhase::Clear,
            refusal,
        );
    }
    outside.attempts.clear = true;
    let cleared: ClearedTeardownAdmission = match catch_unwind(AssertUnwindSafe(|| {
        carrier.clear_configuration(&mut reader)
    })) {
        Ok(Ok(cleared)) => cleared,
        Ok(Err(failure)) => {
            outside.publication = failure.publication_receipt().cloned();
            return held(
                outside,
                Some(PhaseOwner::ClearFailure(Box::new(failure))),
                TeardownRunPhase::Clear,
                TeardownRunRefusal::Clear,
            );
        }
        Err(_) => return unknown(outside, None, TeardownRunPhase::Clear),
    };
    outside.publication = Some(cleared.publication_receipt().clone());
    if let Err(refusal) = outside.remaining() {
        return held(
            outside,
            Some(PhaseOwner::Cleared(Box::new(cleared))),
            TeardownRunPhase::Binding,
            refusal,
        );
    }
    let (finish, operational) = if own_storage.is_some() {
        match catch_unwind(AssertUnwindSafe(|| {
            cleared.into_finish_without_binding(&mut reader)
        })) {
            Ok(Ok(pair)) => pair,
            Ok(Err(failure)) => {
                return held(
                    outside,
                    Some(PhaseOwner::BindingFailure(Box::new(failure))),
                    TeardownRunPhase::FinishAdmission,
                    TeardownRunRefusal::Binding,
                );
            }
            Err(_) => return unknown(outside, None, TeardownRunPhase::FinishAdmission),
        }
    } else {
        let guarded = match catch_unwind(AssertUnwindSafe(|| {
            cleared.begin_binding_finalization(&mut reader)
        })) {
            Ok(Ok(guarded)) => guarded,
            Ok(Err(failure)) => {
                return held(
                    outside,
                    Some(PhaseOwner::BindingFailure(Box::new(failure))),
                    TeardownRunPhase::Binding,
                    TeardownRunRefusal::Binding,
                );
            }
            Err(_) => return unknown(outside, None, TeardownRunPhase::Binding),
        };
        if let Err(refusal) = outside.remaining() {
            return held(
                outside,
                Some(PhaseOwner::Guarded(Box::new(guarded))),
                TeardownRunPhase::Binding,
                refusal,
            );
        }
        outside.attempts.binding = true;
        let bound = match catch_unwind(AssertUnwindSafe(|| guarded.remove_binding(&mut reader))) {
            Ok(Ok(bound)) => bound,
            Ok(Err(failure)) => {
                outside.binding = failure.binding_receipt().cloned();
                return held(
                    outside,
                    Some(PhaseOwner::BindingFailure(Box::new(failure))),
                    TeardownRunPhase::Binding,
                    TeardownRunRefusal::Binding,
                );
            }
            Err(_) => return unknown(outside, None, TeardownRunPhase::Binding),
        };
        outside.binding = Some(bound.binding_receipt().clone());
        if let Err(refusal) = outside.remaining() {
            return held(
                outside,
                Some(PhaseOwner::Bound(Box::new(bound))),
                TeardownRunPhase::FinishAdmission,
                refusal,
            );
        }
        match catch_unwind(AssertUnwindSafe(|| bound.into_finish(&mut reader))) {
            Ok(Ok(pair)) => pair,
            Ok(Err(failure)) => {
                return held(
                    outside,
                    Some(PhaseOwner::BindingFailure(Box::new(failure))),
                    TeardownRunPhase::FinishAdmission,
                    TeardownRunRefusal::Binding,
                );
            }
            Err(_) => return unknown(outside, None, TeardownRunPhase::FinishAdmission),
        }
    };
    if !Arc::ptr_eq(
        &operational,
        outside.carrier.as_ref().expect("original carrier"),
    ) {
        return held(
            outside,
            Some(PhaseOwner::CarrierMismatch {
                finish: Box::new(finish),
                operational,
            }),
            TeardownRunPhase::CarrierTransfer,
            TeardownRunRefusal::CarrierShared,
        );
    }
    drop(operational);
    let before = catch_unwind(AssertUnwindSafe(|| {
        finish
            .check_before_retirement(
                outside.carrier.as_ref().expect("original carrier"),
                &mut reader,
            )
            .map_err(TeardownRunRefusal::Finish)?;
        outside.remaining()
    }));
    match before {
        Ok(Ok(())) => {}
        Ok(Err(refusal)) => {
            return held(
                outside,
                Some(PhaseOwner::Finish(Box::new(finish))),
                TeardownRunPhase::CarrierTransfer,
                refusal,
            );
        }
        Err(_) => {
            return unknown(
                outside,
                Some(PhaseOwner::Finish(Box::new(finish))),
                TeardownRunPhase::CarrierTransfer,
            );
        }
    }
    let carrier = outside.carrier.take().expect("original carrier");
    let carrier = match Arc::try_unwrap(carrier) {
        Ok(carrier) => carrier,
        Err(carrier) => {
            outside.carrier = Some(carrier);
            return held(
                outside,
                Some(PhaseOwner::Finish(Box::new(finish))),
                TeardownRunPhase::CarrierTransfer,
                TeardownRunRefusal::CarrierShared,
            );
        }
    };
    outside.attempts.retirement = true;
    match catch_unwind(AssertUnwindSafe(|| {
        test_cut(TeardownRunPhase::CarrierTransfer);
        carrier.retire_after_settlement()
    })) {
        Ok(retirement) => outside.retirement = Some(retirement),
        Err(_) => {
            return unknown(
                outside,
                Some(PhaseOwner::Finish(Box::new(finish))),
                TeardownRunPhase::Retirement,
            );
        }
    }
    let receipt = &outside
        .retirement
        .as_ref()
        .expect("actual retirement")
        .receipt;
    // Windows has no directory flush, so there an exactly removed and
    // confirmed carrier is accepted with its root unflushed.
    if receipt.effect() != ActivityCarrierRetirementEffect::Removed
        || !(receipt.root_durability() == Durability::Synced || cfg!(windows))
        || receipt.confirmation() != Confirmation::Confirmed
        || !receipt.failures().is_empty()
    {
        return held(
            outside,
            Some(PhaseOwner::Finish(Box::new(finish))),
            TeardownRunPhase::Retirement,
            TeardownRunRefusal::RetirementUncertain,
        );
    }
    let final_checks = catch_unwind(AssertUnwindSafe(|| {
        test_cut(TeardownRunPhase::FinalChecks);
        outside.remaining()?;
        finish
            .check_after_retirement(&mut reader)
            .map_err(TeardownRunRefusal::Finish)?;
        outside.remaining()
    }));
    match final_checks {
        Ok(Ok(())) => {}
        Ok(Err(refusal)) => {
            return held(
                outside,
                Some(PhaseOwner::Finish(Box::new(finish))),
                TeardownRunPhase::FinalChecks,
                refusal,
            );
        }
        Err(_) => {
            return unknown(
                outside,
                Some(PhaseOwner::Finish(Box::new(finish))),
                TeardownRunPhase::FinalChecks,
            );
        }
    }
    let retirement = outside.retirement.take().expect("actual retirement");
    let report = SettledTeardownReport {
        remote: outside
            .operation
            .take()
            .expect("original operation")
            .into_report(),
        attempts: std::mem::take(&mut outside.attempts),
        publication: outside.publication.take().expect("actual publication"),
        binding: outside.binding.take(),
        retirement: retirement.receipt,
    };
    drop(retirement.owned);
    drop(finish);
    drop(outside);
    OwnedTeardownOutcome::Finished(Box::new(report))
}

#[cfg(all(
    test,
    feature = "full-tests",
    any(
        target_os = "linux",
        target_os = "macos",
        target_os = "android",
        windows
    )
))]
thread_local! {
    #[cfg(target_os = "android")]
    static PANIC_AT: Box<std::cell::Cell<Option<TeardownRunPhase>>> = Box::new(std::cell::Cell::new(None));
    #[cfg(not(target_os = "android"))]
    static PANIC_AT: std::cell::Cell<Option<TeardownRunPhase>> = const { std::cell::Cell::new(None) };
}

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "android",
    windows
))]
fn test_cut(phase: TeardownRunPhase) {
    #[cfg(all(test, feature = "full-tests"))]
    PANIC_AT.with(|cut| {
        if cut.get() == Some(phase) {
            cut.set(None);
            panic!("owned teardown cut: {phase:?}");
        }
    });
    let _ = phase;
}

#[cfg(all(
    test,
    feature = "full-tests",
    any(
        target_os = "linux",
        target_os = "macos",
        target_os = "android",
        windows
    )
))]
mod tests;
