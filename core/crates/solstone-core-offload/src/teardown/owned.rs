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
    target_os = "ios",
    windows,
    test
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
    retirement: Option<ActivityCarrierRetirementReceipt>,
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
    pub fn retirement(&self) -> Option<&ActivityCarrierRetirementReceipt> {
        self.retirement.as_ref()
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
        target_os = "ios",
        windows,
        test
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
        target_os = "ios",
        windows,
        test
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
            Self::Finished(report) => report.retirement(),
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
    target_os = "ios",
    windows,
    test
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
        retirement: Some(retirement.receipt),
    };
    drop(retirement.owned);
    drop(finish);
    drop(outside);
    OwnedTeardownOutcome::Finished(Box::new(report))
}

#[cfg(feature = "test-hooks")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OwnedTeardownInterrupt {
    AfterClear,
}

#[cfg(feature = "test-hooks")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnedTeardownInterruptArm {
    identity: solstone_core_journal_io::ObjectIdentity,
    boundary: OwnedTeardownInterrupt,
}

#[cfg(feature = "test-hooks")]
impl OwnedTeardownInterruptArm {
    pub fn identity(&self) -> solstone_core_journal_io::ObjectIdentity {
        self.identity
    }
    pub fn boundary(&self) -> OwnedTeardownInterrupt {
        self.boundary
    }
}

#[cfg(feature = "test-hooks")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnedTeardownInterruptObservation {
    pub armed: bool,
    pub fired: bool,
}

#[cfg(feature = "test-hooks")]
impl OwnedTeardownInterruptObservation {
    pub fn armed(&self) -> bool {
        self.armed
    }
    pub fn fired(&self) -> bool {
        self.fired
    }
}

#[cfg(feature = "test-hooks")]
#[derive(Clone, Copy, Debug, Default)]
struct InterruptState {
    armed: bool,
    fired: bool,
}

#[cfg(feature = "test-hooks")]
static INTERRUPT_SLOTS: std::sync::Mutex<
    Vec<(
        (
            solstone_core_journal_io::ObjectIdentity,
            OwnedTeardownInterrupt,
        ),
        InterruptState,
    )>,
> = std::sync::Mutex::new(Vec::new());

#[cfg(feature = "test-hooks")]
pub fn arm_owned_teardown_interrupt(
    root: &JournalRoot,
    boundary: OwnedTeardownInterrupt,
) -> OwnedTeardownInterruptArm {
    let identity = root.identity();
    let mut guard = INTERRUPT_SLOTS.lock().unwrap();
    if let Some((_, state)) = guard.iter_mut().find(|(k, _)| *k == (identity, boundary)) {
        state.armed = true;
        state.fired = false;
    } else {
        guard.push((
            (identity, boundary),
            InterruptState {
                armed: true,
                fired: false,
            },
        ));
    }
    OwnedTeardownInterruptArm { identity, boundary }
}

#[cfg(feature = "test-hooks")]
pub fn owned_teardown_interrupt_observation(
    root: &JournalRoot,
    boundary: OwnedTeardownInterrupt,
) -> OwnedTeardownInterruptObservation {
    let identity = root.identity();
    let guard = INTERRUPT_SLOTS.lock().unwrap();
    if let Some((_, state)) = guard.iter().find(|(k, _)| *k == (identity, boundary)) {
        OwnedTeardownInterruptObservation {
            armed: state.armed,
            fired: state.fired,
        }
    } else {
        OwnedTeardownInterruptObservation {
            armed: false,
            fired: false,
        }
    }
}

#[cfg(feature = "test-hooks")]
fn consume_interrupt(root: &JournalRoot, boundary: OwnedTeardownInterrupt) -> bool {
    let identity = root.identity();
    let mut guard = INTERRUPT_SLOTS.lock().unwrap();
    if let Some((_, state)) = guard.iter_mut().find(|(k, _)| *k == (identity, boundary)) {
        if state.armed {
            state.armed = false;
            state.fired = true;
            return true;
        }
    }
    false
}

#[cfg(all(test, not(target_os = "ios")))]
pub mod test_seam {
    use std::cell::RefCell;

    #[derive(Default)]
    pub struct TestSeamState {
        pub list_ids: Option<Result<Vec<String>, String>>,
        pub teardown_result: Option<Result<Vec<String>, String>>,
        pub teardown_called: bool,
        pub teardown_admitted_ids: Vec<String>,
        pub retire_s3_result: Option<Result<(), String>>,
        pub retire_s3_called: bool,
    }

    thread_local! {
        static STATE: RefCell<Option<TestSeamState>> = const { RefCell::new(None) };
    }

    pub fn install(state: TestSeamState) {
        STATE.with(|slot| *slot.borrow_mut() = Some(state));
    }

    pub fn clear() {
        STATE.with(|slot| *slot.borrow_mut() = None);
    }

    pub fn with_state<R>(f: impl FnOnce(&mut TestSeamState) -> R) -> Option<R> {
        STATE.with(|slot| {
            let mut slot = slot.borrow_mut();
            slot.as_mut().map(f)
        })
    }

    pub fn list_snapshots() -> Result<Vec<String>, String> {
        STATE.with(|slot| {
            let mut slot = slot.borrow_mut();
            match slot.as_mut().and_then(|s| s.list_ids.take()) {
                Some(res) => res,
                None => Err("seam refusal: uninstalled or no list_ids".to_owned()),
            }
        })
    }

    pub fn teardown_entry(admitted_ids: &[String]) -> Result<Vec<String>, String> {
        STATE.with(|slot| {
            let mut slot = slot.borrow_mut();
            match slot.as_mut() {
                Some(s) => {
                    s.teardown_called = true;
                    s.teardown_admitted_ids = admitted_ids.to_vec();
                    s.teardown_result
                        .take()
                        .unwrap_or_else(|| Err("seam refusal: no teardown_result".to_owned()))
                }
                None => Err("seam refusal: uninstalled".to_owned()),
            }
        })
    }

    pub fn retire_s3() -> Result<(), String> {
        STATE.with(|slot| {
            let mut slot = slot.borrow_mut();
            match slot.as_mut() {
                Some(s) => {
                    s.retire_s3_called = true;
                    s.retire_s3_result
                        .take()
                        .unwrap_or_else(|| Err("seam refusal: no retire_s3_result".to_owned()))
                }
                None => Err("seam refusal: uninstalled".to_owned()),
            }
        })
    }
}

#[cfg(any(target_os = "ios", test))]
pub fn run_owned_teardown_with_root(
    root: &Arc<JournalRoot>,
    clock: Arc<dyn Clock + Send + Sync>,
    cancellation: ArchiveCancellation,
    version: &str,
) -> OwnedTeardownOutcome {
    use solstone_core_backup::teardown_recovery::{
        PruneObligation, TeardownRecoveryRecord, delete_teardown_recovery_at_root,
        load_teardown_recovery_at_root, stage_teardown_recovery_at_root,
        update_teardown_recovery_at_root,
    };
    use solstone_core_journal_io::media::{
        ActivityCarrierRetirementEffect, Confirmation, Durability, RootTeardownCarrier,
    };
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let mut outside = Outside::new(clock, cancellation);
    let mut reader = super::read_teardown_coverage;

    if let Err(err) = root.revalidate_canonical_binding() {
        return held(
            outside,
            None,
            TeardownRunPhase::Prelaunch,
            TeardownRunRefusal::Root(err),
        );
    }

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
    outside.root = Some(Arc::clone(root));

    let existing_record = match load_teardown_recovery_at_root(root) {
        Ok(record) => record,
        Err(_) => {
            return held(
                outside,
                None,
                TeardownRunPhase::Admission,
                TeardownRunRefusal::CarrierPreparation,
            );
        }
    };

    // If clear was already confirmed, resume from the recovery record directly without admit_teardown.
    if existing_record.as_ref().is_some_and(|r| r.clear_confirmed) {
        let mut record = existing_record.unwrap();
        let flush = match record.directory_flush {
            Some(flush) => flush,
            None => {
                return held(
                    outside,
                    None,
                    TeardownRunPhase::Clear,
                    TeardownRunRefusal::Clear,
                );
            }
        };
        let lease = match TeardownLease::try_acquire(Arc::clone(root)) {
            Ok(lease) => lease,
            Err(error) => {
                return held(
                    outside,
                    None,
                    TeardownRunPhase::Admission,
                    TeardownRunRefusal::Admission(match error {
                        solstone_core_journal_io::activity::ActivityAdmissionError::Busy => {
                            TeardownAdmissionRefusal::ActivityBusy
                        }
                        solstone_core_journal_io::activity::ActivityAdmissionError::Unavailable(
                            _,
                        ) => TeardownAdmissionRefusal::ActivityUnavailable,
                    }),
                );
            }
        };
        outside.lease = Some(lease.clone());

        if !record.binding_released {
            if record.mode == "operated" {
                #[cfg(any(unix, windows))]
                if solstone_core_backup::remove_hosted_binding(root.canonical_path()).is_err() {
                    return held(
                        outside,
                        None,
                        TeardownRunPhase::Binding,
                        TeardownRunRefusal::Binding,
                    );
                }
            }
            record.binding_released = true;
            if update_teardown_recovery_at_root(root, &record).is_err() {
                record.binding_released = false;
                return held(
                    outside,
                    None,
                    TeardownRunPhase::Binding,
                    TeardownRunRefusal::Binding,
                );
            }
        }

        let retirement_receipt = if !record.carrier_retired {
            let carrier = match RootTeardownCarrier::prepare(lease.clone()) {
                Ok(carrier) => carrier,
                Err(_) => {
                    return held(
                        outside,
                        None,
                        TeardownRunPhase::Retirement,
                        TeardownRunRefusal::RetirementUncertain,
                    );
                }
            };
            let ret = carrier.retire_after_settlement();
            let receipt = ret.receipt;
            if receipt.effect() != ActivityCarrierRetirementEffect::Removed
                || !(receipt.root_durability() == Durability::Synced || cfg!(windows))
                || receipt.confirmation() != Confirmation::Confirmed
                || !receipt.failures().is_empty()
            {
                return held(
                    outside,
                    None,
                    TeardownRunPhase::Retirement,
                    TeardownRunRefusal::RetirementUncertain,
                );
            }
            record.carrier_retired = true;
            if update_teardown_recovery_at_root(root, &record).is_err() {
                record.carrier_retired = false;
                return held(
                    outside,
                    None,
                    TeardownRunPhase::Retirement,
                    TeardownRunRefusal::RetirementUncertain,
                );
            }
            Some(receipt)
        } else {
            None
        };

        if !record.destination_secret_retired {
            let is_s3 = record
                .repository_locator
                .as_deref()
                .is_some_and(|locator| locator.starts_with("s3:"))
                || record.bucket.is_some();
            if record.mode != "operated" && is_s3 {
                #[cfg(target_os = "ios")]
                let retired = {
                    let dest =
                        solstone_core_backup_runtime::ios_callable::repository::Destination {
                            endpoint: record.endpoint.clone().unwrap_or_default(),
                            region: record.region.clone().unwrap_or_default(),
                            bucket: record.bucket.clone().unwrap_or_default(),
                            prefix: record.prefix.clone().unwrap_or_default(),
                        };
                    if let Ok(authority) =
                        solstone_core_backup_runtime::ios_callable::admitted_ios_backup(Arc::clone(
                            root,
                        ))
                    {
                        solstone_core_backup_runtime::ios_callable::retire_s3_destination(
                            &authority, &dest,
                        )
                        .is_ok()
                    } else {
                        false
                    }
                };
                #[cfg(all(test, not(target_os = "ios")))]
                let retired = test_seam::retire_s3().is_ok();

                if !retired {
                    return held(
                        outside,
                        None,
                        TeardownRunPhase::Retirement,
                        TeardownRunRefusal::RetirementUncertain,
                    );
                }
            }
            record.destination_secret_retired = true;
            if update_teardown_recovery_at_root(root, &record).is_err() {
                record.destination_secret_retired = false;
                return held(
                    outside,
                    None,
                    TeardownRunPhase::Retirement,
                    TeardownRunRefusal::RetirementUncertain,
                );
            }
        }

        if !record.is_complete() || delete_teardown_recovery_at_root(root).is_err() {
            return held(
                outside,
                None,
                TeardownRunPhase::FinalChecks,
                TeardownRunRefusal::RetirementUncertain,
            );
        }

        let publication = Arc::new(BoundConfigPublication {
            candidate_sha256: record.original_config_sha256.clone(),
            published_file: None,
            published_identity: None,
            directory_flush: flush,
            durability_uncertainty: None,
            observation_uncertainty: None,
            confirmation_refusal: None,
        });

        let mut attempts = std::mem::take(&mut outside.attempts);
        attempts.clear = true;
        attempts.binding = true;
        attempts.retirement = true;

        let report = SettledTeardownReport {
            remote: outside
                .operation
                .take()
                .expect("original operation")
                .into_report(),
            attempts,
            publication,
            #[cfg(any(unix, windows))]
            binding: None,
            retirement: retirement_receipt,
        };
        return OwnedTeardownOutcome::Finished(Box::new(report));
    }

    let admitted = match admit_teardown(Arc::clone(root)) {
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

    let own_storage = if operated {
        if admitted.configuration().binding().is_none() {
            return held(
                outside,
                Some(PhaseOwner::Admission(Box::new(admitted))),
                TeardownRunPhase::Admission,
                TeardownRunRefusal::Binding,
            );
        }
        false
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
        let dest = admitted.configuration().destination().unwrap();

        #[cfg(target_os = "ios")]
        {
            let mut env_map = std::collections::BTreeMap::new();
            if let Ok(env) = solstone_core_backup::assemble_backend_env(&dest) {
                for (k, v) in env {
                    if let Some(s) = v.as_str() {
                        env_map.insert(k, Some(s.to_owned()));
                    }
                }
            }
            if solstone_core_backup_runtime::ios_callable::repository::parse(
                &dest.repository,
                Some(&env_map),
            )
            .is_err()
            {
                return held(
                    outside,
                    Some(PhaseOwner::Admission(Box::new(admitted))),
                    TeardownRunPhase::Admission,
                    TeardownRunRefusal::UnsupportedDestination,
                );
            }
            if solstone_core_backup_runtime::ios_callable::admitted_ios_backup(Arc::clone(root))
                .is_err()
            {
                return held(
                    outside,
                    Some(PhaseOwner::Admission(Box::new(admitted))),
                    TeardownRunPhase::Admission,
                    TeardownRunRefusal::ToolUnavailable,
                );
            }
        }

        #[cfg(all(test, not(target_os = "ios")))]
        {
            let repo_str = &dest.repository;
            let is_addressable = Path::new(repo_str).is_absolute() || repo_str.starts_with("s3:");
            if !is_addressable {
                return held(
                    outside,
                    Some(PhaseOwner::Admission(Box::new(admitted))),
                    TeardownRunPhase::Admission,
                    TeardownRunRefusal::UnsupportedDestination,
                );
            }
        }
        true
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

    let mut record = existing_record.unwrap_or_else(|| {
        let mode = if operated {
            "operated".to_owned()
        } else {
            carrier
                .configuration()
                .backup()
                .get("mode")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("byo")
                .to_owned()
        };
        let mut rec = TeardownRecoveryRecord::new(
            carrier.configuration().sha256().to_owned(),
            mode,
            vec![],
            PruneObligation::None,
        );
        if operated {
            if let Some(binding) = carrier.configuration().binding() {
                rec.account_id = Some(binding.account_id.clone());
                rec.instance_id = Some(binding.instance_id.clone());
                rec.bucket = Some(binding.bucket.clone());
                rec.prefix = Some(binding.prefix.clone());
                rec.broker_token = Some(binding.broker_token.clone());
            }
        } else if let Some(dest) = carrier.configuration().destination() {
            rec.repository_locator = Some(dest.repository.clone());
            rec.bucket = dest
                .credentials
                .get("bucket")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned);
            rec.prefix = dest
                .credentials
                .get("prefix")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned);
            rec.endpoint = dest
                .credentials
                .get("endpoint")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned);
            rec.region = dest
                .credentials
                .get("region")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned);
            rec.s3_secret = dest
                .credentials
                .get("secret_access_key")
                .or_else(|| dest.credentials.get("s3_secret"))
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned);
            if rec.bucket.is_none() {
                if let Some(raw) = dest.repository.strip_prefix("s3:") {
                    let raw = raw
                        .strip_prefix("https://")
                        .or_else(|| raw.strip_prefix("//"))
                        .unwrap_or(raw);
                    let parts: Vec<&str> = raw.split('/').collect();
                    if parts.len() == 2 && !parts[0].contains('.') && !parts[0].contains(':') {
                        rec.bucket = Some(parts[0].to_owned());
                        rec.prefix = Some(parts[1].to_owned());
                    } else if parts.len() >= 3 {
                        rec.bucket = Some(parts[1].to_owned());
                        rec.prefix = Some(parts[2..].join("/"));
                    }
                }
            }
        }
        if let Some(keys) = carrier.configuration().keys() {
            rec.daily_key = Some(keys.daily_key.clone());
            rec.recovery_key = Some(keys.recovery_key.clone());
        }
        rec
    });

    if stage_teardown_recovery_at_root(root, &record).is_err() {
        return held(
            outside,
            Some(PhaseOwner::Carrier(Box::new(carrier))),
            TeardownRunPhase::Preparation,
            TeardownRunRefusal::CarrierPreparation,
        );
    }

    if operated {
        let remote = catch_unwind(AssertUnwindSafe(|| {
            outside
                .operation
                .as_mut()
                .expect("original operation")
                .run_remote(&carrier, &mut reader, version)
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
    } else {
        #[cfg(target_os = "ios")]
        {
            let dest = carrier.configuration().destination().unwrap();
            if let Err(refusal) = outside
                .operation
                .as_mut()
                .expect("original operation")
                .observe_ios_retention(&dest)
            {
                return held(
                    outside,
                    Some(PhaseOwner::Carrier(Box::new(carrier))),
                    TeardownRunPhase::Remote,
                    TeardownRunRefusal::Remote(refusal),
                );
            }
        }

        #[cfg(target_os = "ios")]
        let list_result: Result<Vec<String>, String> = {
            let dest = carrier.configuration().destination().unwrap();
            let mut env_map = std::collections::BTreeMap::new();
            if let Ok(env) = solstone_core_backup::assemble_backend_env(&dest) {
                for (k, v) in env {
                    if let Some(s) = v.as_str() {
                        env_map.insert(k, Some(s.to_owned()));
                    }
                }
            }
            let authority = match solstone_core_backup_runtime::ios_callable::admitted_ios_backup(Arc::clone(root)) {
                Ok(a) => a,
                Err(_) => return held(
                    outside,
                    Some(PhaseOwner::Carrier(Box::new(carrier))),
                    TeardownRunPhase::Remote,
                    TeardownRunRefusal::Remote(TeardownRemoteRefusal::Protocol(
                        solstone_core_backup_runtime::teardown::owned::TeardownProtocolRefusal::Malformed,
                    )),
                ),
            };
            let password = carrier
                .configuration()
                .keys()
                .map(|k| k.daily_key.as_str())
                .unwrap_or("");
            solstone_core_backup_runtime::ios_callable::list_snapshot_ids(
                &authority,
                &dest.repository,
                password,
                Some(&env_map),
                None,
                Some(&outside.cancellation),
            )
            .map_err(|e| format!("{e:?}"))
        };
        #[cfg(all(test, not(target_os = "ios")))]
        let list_result = test_seam::list_snapshots();

        let live_ids = match list_result {
            Ok(ids) => ids,
            Err(_) => {
                return held(
                    outside,
                    Some(PhaseOwner::Carrier(Box::new(carrier))),
                    TeardownRunPhase::Remote,
                    TeardownRunRefusal::Remote(TeardownRemoteRefusal::Protocol(
                        solstone_core_backup_runtime::teardown::owned::TeardownProtocolRefusal::Malformed,
                    )),
                );
            }
        };

        if record.prune_obligation == PruneObligation::Admitted {
            #[cfg(target_os = "ios")]
            let entry_result: Result<Vec<String>, String> = {
                let dest = carrier.configuration().destination().unwrap();
                let mut env_map = std::collections::BTreeMap::new();
                if let Ok(env) = solstone_core_backup::assemble_backend_env(&dest) {
                    for (k, v) in env {
                        if let Some(s) = v.as_str() {
                            env_map.insert(k, Some(s.to_owned()));
                        }
                    }
                }
                let authority =
                    match solstone_core_backup_runtime::ios_callable::admitted_ios_backup(
                        Arc::clone(root),
                    ) {
                        Ok(a) => a,
                        Err(_) => {
                            return held(
                                outside,
                                Some(PhaseOwner::Carrier(Box::new(carrier))),
                                TeardownRunPhase::Remote,
                                TeardownRunRefusal::Remote(TeardownRemoteRefusal::Residual),
                            );
                        }
                    };
                let repo = match solstone_core_backup_runtime::ios_callable::repository::parse(
                    &dest.repository,
                    Some(&env_map),
                ) {
                    Ok(r) => r,
                    Err(_) => {
                        return held(
                            outside,
                            Some(PhaseOwner::Carrier(Box::new(carrier))),
                            TeardownRunPhase::Remote,
                            TeardownRunRefusal::Remote(TeardownRemoteRefusal::Residual),
                        );
                    }
                };
                let password = carrier
                    .configuration()
                    .keys()
                    .map(|k| k.daily_key.as_str())
                    .unwrap_or("");
                solstone_core_backup_runtime::ios_callable::run_teardown_entry(
                    &authority,
                    repo,
                    password,
                    &record.admitted_snapshot_ids,
                    None,
                    Some(&outside.cancellation),
                )
                .map_err(|e| format!("{e:?}"))
            };
            #[cfg(all(test, not(target_os = "ios")))]
            let entry_result = test_seam::teardown_entry(&record.admitted_snapshot_ids);

            let remaining_ids = match entry_result {
                Ok(ids) => ids,
                Err(_) => {
                    return held(
                        outside,
                        Some(PhaseOwner::Carrier(Box::new(carrier))),
                        TeardownRunPhase::Remote,
                        TeardownRunRefusal::Remote(TeardownRemoteRefusal::Residual),
                    );
                }
            };
            let residual = record
                .admitted_snapshot_ids
                .iter()
                .any(|id| remaining_ids.contains(id))
                || !remaining_ids.is_empty();
            if residual {
                return held(
                    outside,
                    Some(PhaseOwner::Carrier(Box::new(carrier))),
                    TeardownRunPhase::Remote,
                    TeardownRunRefusal::Remote(TeardownRemoteRefusal::Residual),
                );
            }
            record.prune_obligation = PruneObligation::Reconciled;
            if update_teardown_recovery_at_root(root, &record).is_err() {
                return held(
                    outside,
                    Some(PhaseOwner::Carrier(Box::new(carrier))),
                    TeardownRunPhase::Remote,
                    TeardownRunRefusal::Remote(TeardownRemoteRefusal::Residual),
                );
            }
        } else if live_ids.is_empty() {
            record.prune_obligation = PruneObligation::None;
            if update_teardown_recovery_at_root(root, &record).is_err() {
                return held(
                    outside,
                    Some(PhaseOwner::Carrier(Box::new(carrier))),
                    TeardownRunPhase::Remote,
                    TeardownRunRefusal::Remote(TeardownRemoteRefusal::Residual),
                );
            }
        } else {
            record.admitted_snapshot_ids = live_ids.clone();
            record.prune_obligation = PruneObligation::Admitted;
            if update_teardown_recovery_at_root(root, &record).is_err() {
                return held(
                    outside,
                    Some(PhaseOwner::Carrier(Box::new(carrier))),
                    TeardownRunPhase::Remote,
                    TeardownRunRefusal::Remote(TeardownRemoteRefusal::Residual),
                );
            }

            #[cfg(target_os = "ios")]
            let entry_result: Result<Vec<String>, String> = {
                let dest = carrier.configuration().destination().unwrap();
                let mut env_map = std::collections::BTreeMap::new();
                if let Ok(env) = solstone_core_backup::assemble_backend_env(&dest) {
                    for (k, v) in env {
                        if let Some(s) = v.as_str() {
                            env_map.insert(k, Some(s.to_owned()));
                        }
                    }
                }
                let authority =
                    match solstone_core_backup_runtime::ios_callable::admitted_ios_backup(
                        Arc::clone(root),
                    ) {
                        Ok(a) => a,
                        Err(_) => {
                            return held(
                                outside,
                                Some(PhaseOwner::Carrier(Box::new(carrier))),
                                TeardownRunPhase::Remote,
                                TeardownRunRefusal::Remote(TeardownRemoteRefusal::Residual),
                            );
                        }
                    };
                let repo = match solstone_core_backup_runtime::ios_callable::repository::parse(
                    &dest.repository,
                    Some(&env_map),
                ) {
                    Ok(r) => r,
                    Err(_) => {
                        return held(
                            outside,
                            Some(PhaseOwner::Carrier(Box::new(carrier))),
                            TeardownRunPhase::Remote,
                            TeardownRunRefusal::Remote(TeardownRemoteRefusal::Residual),
                        );
                    }
                };
                let password = carrier
                    .configuration()
                    .keys()
                    .map(|k| k.daily_key.as_str())
                    .unwrap_or("");
                solstone_core_backup_runtime::ios_callable::run_teardown_entry(
                    &authority,
                    repo,
                    password,
                    &record.admitted_snapshot_ids,
                    None,
                    Some(&outside.cancellation),
                )
                .map_err(|e| format!("{e:?}"))
            };
            #[cfg(all(test, not(target_os = "ios")))]
            let entry_result = test_seam::teardown_entry(&record.admitted_snapshot_ids);

            let remaining_ids = match entry_result {
                Ok(ids) => ids,
                Err(_) => {
                    return held(
                        outside,
                        Some(PhaseOwner::Carrier(Box::new(carrier))),
                        TeardownRunPhase::Remote,
                        TeardownRunRefusal::Remote(TeardownRemoteRefusal::Residual),
                    );
                }
            };
            let residual = record
                .admitted_snapshot_ids
                .iter()
                .any(|id| remaining_ids.contains(id))
                || !remaining_ids.is_empty();
            if residual {
                return held(
                    outside,
                    Some(PhaseOwner::Carrier(Box::new(carrier))),
                    TeardownRunPhase::Remote,
                    TeardownRunRefusal::Remote(TeardownRemoteRefusal::Residual),
                );
            }
            record.prune_obligation = PruneObligation::Reconciled;
            if update_teardown_recovery_at_root(root, &record).is_err() {
                return held(
                    outside,
                    Some(PhaseOwner::Carrier(Box::new(carrier))),
                    TeardownRunPhase::Remote,
                    TeardownRunRefusal::Remote(TeardownRemoteRefusal::Residual),
                );
            }
        }
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
    let pub_receipt = cleared.publication_receipt().clone();
    outside.publication = Some(pub_receipt.clone());
    record.clear_confirmed = true;
    record.directory_flush = Some(pub_receipt.directory_flush);
    if update_teardown_recovery_at_root(root, &record).is_err() {
        record.clear_confirmed = false;
        record.directory_flush = None;
        return held(
            outside,
            Some(PhaseOwner::Cleared(Box::new(cleared))),
            TeardownRunPhase::Clear,
            TeardownRunRefusal::Clear,
        );
    }

    #[cfg(feature = "test-hooks")]
    if consume_interrupt(root, OwnedTeardownInterrupt::AfterClear) {
        return held(
            outside,
            Some(PhaseOwner::Cleared(Box::new(cleared))),
            TeardownRunPhase::Binding,
            TeardownRunRefusal::Binding,
        );
    }

    if let Err(refusal) = outside.remaining() {
        return held(
            outside,
            Some(PhaseOwner::Cleared(Box::new(cleared))),
            TeardownRunPhase::Binding,
            refusal,
        );
    }

    let (finish, operational) = if own_storage {
        let pair = match catch_unwind(AssertUnwindSafe(|| {
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
        };
        record.binding_released = true;
        if update_teardown_recovery_at_root(root, &record).is_err() {
            record.binding_released = false;
            return held(
                outside,
                Some(PhaseOwner::Finish(Box::new(pair.0))),
                TeardownRunPhase::Binding,
                TeardownRunRefusal::Binding,
            );
        }
        pair
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
        record.binding_released = true;
        if update_teardown_recovery_at_root(root, &record).is_err() {
            record.binding_released = false;
            return held(
                outside,
                Some(PhaseOwner::Bound(Box::new(bound))),
                TeardownRunPhase::Binding,
                TeardownRunRefusal::Binding,
            );
        }

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
    record.carrier_retired = true;
    if update_teardown_recovery_at_root(root, &record).is_err() {
        record.carrier_retired = false;
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

    let is_s3 = record
        .repository_locator
        .as_deref()
        .is_some_and(|locator| locator.starts_with("s3:"))
        || record.bucket.is_some();
    if own_storage && is_s3 {
        #[cfg(target_os = "ios")]
        let retired = {
            let dest = solstone_core_backup_runtime::ios_callable::repository::Destination {
                endpoint: record.endpoint.clone().unwrap_or_default(),
                region: record.region.clone().unwrap_or_default(),
                bucket: record.bucket.clone().unwrap_or_default(),
                prefix: record.prefix.clone().unwrap_or_default(),
            };
            if let Ok(authority) =
                solstone_core_backup_runtime::ios_callable::admitted_ios_backup(Arc::clone(root))
            {
                solstone_core_backup_runtime::ios_callable::retire_s3_destination(&authority, &dest)
                    .is_ok()
            } else {
                false
            }
        };
        #[cfg(all(test, not(target_os = "ios")))]
        let retired = test_seam::retire_s3().is_ok();

        if !retired {
            return held(
                outside,
                Some(PhaseOwner::Finish(Box::new(finish))),
                TeardownRunPhase::Retirement,
                TeardownRunRefusal::RetirementUncertain,
            );
        }
    }
    record.destination_secret_retired = true;
    if update_teardown_recovery_at_root(root, &record).is_err() {
        record.destination_secret_retired = false;
        return held(
            outside,
            Some(PhaseOwner::Finish(Box::new(finish))),
            TeardownRunPhase::Retirement,
            TeardownRunRefusal::RetirementUncertain,
        );
    }

    if !record.is_complete() || delete_teardown_recovery_at_root(root).is_err() {
        return held(
            outside,
            Some(PhaseOwner::Finish(Box::new(finish))),
            TeardownRunPhase::FinalChecks,
            TeardownRunRefusal::RetirementUncertain,
        );
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
        #[cfg(any(unix, windows))]
        binding: outside.binding.take(),
        retirement: Some(retirement.receipt),
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
    any(
        target_os = "linux",
        target_os = "macos",
        target_os = "android",
        windows
    )
))]
mod tests;
