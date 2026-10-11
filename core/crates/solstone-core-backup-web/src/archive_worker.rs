// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Keep concrete archive owners independently of the displayed operation.
//! Actor execution consumes the original admitted read in its owning worker.

pub mod restore;

use std::sync::Arc;

use solstone_core_backup::archive_activity::{
    OwnedArchivePreparationFailure, OwnedPreparedArchiveHold,
};
use solstone_core_backup::archive_admission::{ArchiveBudget, ArchiveCancellation, Clock};
use solstone_core_backup::archive_readback::OwnedDirectReadHold;
use solstone_core_backup_runtime::{
    DirectReadFinishOwner, DirectReadFinishState, OwnedArchiveAdmissionFailure,
    OwnedDirectReadFinish,
};
use solstone_core_journal_io::journal_root::JournalRoot;
use solstone_core_offload::restore::owned::{OwnedRestoreFinishHold, SettledRestoreReport};

use crate::archive_operation::ArchiveBeginRefusal;
use crate::operation::{self, SharedOperationSlot, Terminal};

type OwnedClock = Arc<dyn Clock + Send + Sync>;

struct Reservation {
    generation: u64,
    root: Arc<JournalRoot>,
    clock: OwnedClock,
}

/// Only the original admission can return its reservation.
#[must_use]
pub struct ArchiveTicket(Arc<Reservation>);

impl ArchiveTicket {
    pub fn generation(&self) -> u64 {
        self.0.generation
    }
}

/// Original budget and clock, before any native preparation or transport.
#[must_use]
pub struct OwnedArchiveBegin {
    ticket: ArchiveTicket,
    budget: ArchiveBudget,
}

impl OwnedArchiveBegin {
    pub fn generation(&self) -> u64 {
        self.ticket.generation()
    }

    pub fn into_parts(self) -> (ArchiveTicket, Arc<JournalRoot>, ArchiveBudget, OwnedClock) {
        let root = Arc::clone(&self.ticket.0.root);
        let clock = Arc::clone(&self.ticket.0.clock);
        (self.ticket, root, self.budget, clock)
    }

    /// Consume only an admission that has not issued its preparation inputs.
    pub fn cancel_before_preparation(self, slot: &SharedOperationSlot) {
        if let Some(flag) = self.budget.cancellation() {
            flag.cancel();
        }
        let Ok(mut state) = slot.lock() else {
            return;
        };
        if !state.archive.matches(&self.ticket) {
            return;
        }
        if let Some(current) = state.current.as_mut()
            && current.generation == self.generation()
            && !current.worker_active
        {
            operation::apply_terminal(current, Terminal::error("cancelled"));
        }
        state.archive.reservation = None;
    }
}

/// Reserve the same shared mutex before creating the original budget.
pub fn begin_owned(
    slot: &SharedOperationSlot,
    kind: &str,
    root: Arc<JournalRoot>,
    clock: OwnedClock,
) -> Result<OwnedArchiveBegin, ArchiveBeginRefusal> {
    let mut state = operation::lock_for_begin(slot).map_err(|()| ArchiveBeginRefusal::Busy)?;
    let cancellation = ArchiveCancellation::new();
    let budget = ArchiveBudget::new_cancellable(clock.as_ref(), cancellation.clone())
        .map_err(ArchiveBeginRefusal::Admission)?;
    let generation = operation::install_slot(
        &mut state.current,
        kind,
        None,
        None,
        None,
        Some(cancellation),
    )
    .generation;
    let reservation = Arc::new(Reservation {
        generation,
        root,
        clock,
    });
    state.archive.reservation = Some(Arc::clone(&reservation));
    Ok(OwnedArchiveBegin {
        ticket: ArchiveTicket(reservation),
        budget,
    })
}

/// Closed concrete values only; no report can construct a native owner.
#[must_use]
pub enum ArchivePayload {
    Prepared(Box<OwnedPreparedArchiveHold>),
    PreparationFailure(Box<OwnedArchivePreparationFailure>),
    AdmissionFailure(OwnedArchiveAdmissionFailure),
    ReadHeld {
        read: Box<OwnedDirectReadHold>,
        outside: DirectReadFinishOwner,
    },
    NativeHeld {
        native: Box<OwnedRestoreFinishHold>,
        outside: DirectReadFinishOwner,
    },
    NativeFinished {
        report: SettledRestoreReport,
        runtime: OwnedDirectReadFinish,
    },
    Unknown {
        runtime: OwnedDirectReadFinish,
        report: Option<SettledRestoreReport>,
    },
}

impl ArchivePayload {
    fn completed(&self) -> bool {
        matches!(self, Self::NativeFinished { runtime, .. }
            if runtime.state() == DirectReadFinishState::Completed)
    }

    fn matches(&self, reservation: &Reservation) -> bool {
        let (root, clock) = match self {
            Self::Prepared(owner) => (owner.journal_root(), Some(owner.owned_clock())),
            Self::PreparationFailure(owner) => (owner.journal_root(), Some(owner.owned_clock())),
            Self::AdmissionFailure(owner) => (owner.journal_root(), None),
            Self::ReadHeld { read, outside } => {
                if !Arc::ptr_eq(read.carrier(), outside.carrier())
                    || !Arc::ptr_eq(read.journal_root(), outside.journal_root())
                    || !Arc::ptr_eq(read.owned_clock(), outside.owned_clock())
                {
                    return false;
                }
                (outside.journal_root(), Some(outside.owned_clock()))
            }
            Self::NativeHeld { native, outside } => {
                if !Arc::ptr_eq(native.carrier(), outside.carrier()) {
                    return false;
                }
                (outside.journal_root(), Some(outside.owned_clock()))
            }
            Self::NativeFinished { runtime, .. } | Self::Unknown { runtime, .. } => {
                (runtime.journal_root(), Some(runtime.owned_clock()))
            }
        };
        Arc::ptr_eq(root, &reservation.root)
            && clock.is_none_or(|clock| Arc::ptr_eq(clock, &reservation.clock))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveHandoffRefusal {
    Poisoned,
    Missing,
    Stale,
    Inactive,
    ReservationChanged,
    OwnerChanged,
    NonterminalCompletion,
}

/// A rejected handoff still owns the exact ticket, payload and effect projection.
#[must_use = "park this whole refusal before the worker returns"]
pub struct RejectedArchiveReturn {
    ticket: ArchiveTicket,
    payload: ArchivePayload,
    terminal: Terminal,
    reason: Option<ArchiveHandoffRefusal>,
}

impl RejectedArchiveReturn {
    pub fn reason(&self) -> Option<ArchiveHandoffRefusal> {
        self.reason
    }
    pub fn generation(&self) -> u64 {
        self.ticket.generation()
    }
    pub fn payload(&self) -> &ArchivePayload {
        &self.payload
    }
    pub fn terminal(&self) -> &Terminal {
        &self.terminal
    }
}

#[derive(Default)]
pub(crate) struct ArchiveCustody {
    reservation: Option<Arc<Reservation>>,
    // Never replace a retained return, even after a rejected/stale handoff.
    retained: Vec<RejectedArchiveReturn>,
}

impl ArchiveCustody {
    pub(crate) fn is_busy(&self) -> bool {
        self.reservation.is_some() || !self.retained.is_empty()
    }

    pub(crate) fn retained(&self) -> impl Iterator<Item = &RejectedArchiveReturn> {
        self.retained.iter()
    }

    fn matches(&self, ticket: &ArchiveTicket) -> bool {
        self.reservation
            .as_ref()
            .is_some_and(|original| Arc::ptr_eq(original, &ticket.0))
    }
}

/// Start only the worker owning this private admission ticket.
pub fn start_owned_worker(slot: &SharedOperationSlot, ticket: &ArchiveTicket) -> bool {
    let Ok(mut state) = slot.lock() else {
        return false;
    };
    if !state.archive.matches(ticket) || !state.archive.retained.is_empty() {
        return false;
    }
    let Some(current) = state.current.as_mut() else {
        return false;
    };
    if current.generation != ticket.generation()
        || current.worker_active
        || operation::is_terminal(&current.view.phase)
        || current
            .archive_cancellation
            .as_ref()
            .is_some_and(|flag| flag.is_cancelled())
        || current.pending_handoff_terminal.is_some()
        || current.handoff_poll.is_some()
    {
        return false;
    }
    #[cfg(windows)]
    if current.cleanup.is_some() {
        return false;
    }
    current.worker_active = true;
    true
}

/// Return a complete closed owner; only genuine matching completion releases it.
pub fn handoff(
    slot: &SharedOperationSlot,
    ticket: ArchiveTicket,
    payload: ArchivePayload,
    terminal: Terminal,
) -> Result<(), Box<RejectedArchiveReturn>> {
    fn refuse(
        ticket: ArchiveTicket,
        payload: ArchivePayload,
        terminal: Terminal,
        reason: ArchiveHandoffRefusal,
    ) -> Box<RejectedArchiveReturn> {
        Box::new(RejectedArchiveReturn {
            ticket,
            payload,
            terminal,
            reason: Some(reason),
        })
    }
    let Ok(mut state) = slot.lock() else {
        return Err(refuse(
            ticket,
            payload,
            terminal,
            ArchiveHandoffRefusal::Poisoned,
        ));
    };
    let reason = if !state.archive.matches(&ticket) || !state.archive.retained.is_empty() {
        Some(ArchiveHandoffRefusal::ReservationChanged)
    } else if !payload.matches(&ticket.0) {
        Some(ArchiveHandoffRefusal::OwnerChanged)
    } else {
        match &state.current {
            None => Some(ArchiveHandoffRefusal::Missing),
            Some(current) if current.generation != ticket.generation() => {
                Some(ArchiveHandoffRefusal::Stale)
            }
            Some(current)
                if !current.worker_active
                    || operation::is_terminal(&current.view.phase)
                    || current.pending_handoff_terminal.is_some()
                    || current.handoff_poll.is_some() =>
            {
                Some(ArchiveHandoffRefusal::Inactive)
            }
            Some(_) if payload.completed() && !operation::is_terminal(&terminal.phase) => {
                Some(ArchiveHandoffRefusal::NonterminalCompletion)
            }
            Some(_) => None,
        }
    };
    if let Some(reason) = reason {
        return Err(refuse(ticket, payload, terminal, reason));
    }
    let current = state.current.as_mut().expect("checked current");
    current.worker_active = false;
    if payload.completed() {
        operation::apply_terminal(current, terminal);
        // Keep admission excluded until the actual completed owners release their
        // leases. A destructor unwind poisons this same mutex and stays closed.
        drop(payload);
        drop(ticket);
        state.archive.reservation = None;
    } else {
        let phase = current.view.phase.clone();
        operation::apply_terminal(current, terminal.clone());
        current.view.phase = phase;
        state.archive.retained.push(RejectedArchiveReturn {
            ticket,
            payload,
            terminal,
            reason: None,
        });
    }
    Ok(())
}

/// Retention alone may recover a poisoned mutex. It never applies success.
pub fn park_rejected(slot: &SharedOperationSlot, rejected: Box<RejectedArchiveReturn>) {
    let mut state = slot.lock().unwrap_or_else(|poison| poison.into_inner());
    state.archive.retained.push(*rejected);
}

#[cfg(test)]
mod type_tests {
    use super::*;
    #[test]
    fn closed_archive_custody_is_send_static() {
        fn check<T: Send + 'static>() {}
        check::<ArchivePayload>();
        check::<RejectedArchiveReturn>();
        check::<OwnedArchiveBegin>();
    }
}

#[cfg(all(test, feature = "full-tests"))]
#[cfg(unix)]
mod tests;
