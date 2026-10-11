// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Run an already admitted restore and retain its whole owning return.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use serde_json::{Value, json};
use solstone_core_backup::archive_readback::{OwnedReadOperation, OwnedSettledDirectRead};
use solstone_core_backup_runtime::{
    DirectReadFinishOwner, DirectReadFinishState, OwnedDirectReadFinish, OwnedInstalledArchiveRead,
};
use solstone_core_offload::restore::RestoreResult;
use solstone_core_offload::restore::owned::{
    self, OwnedRestoreFinishHold, OwnedRestoreRunOutcome, RestoreFinishRefusal,
    SettledRestoreReport,
};

use super::{ArchivePayload, ArchiveTicket, handoff, park_rejected};
use crate::operation::{self, SharedOperationSlot, Terminal};
use crate::restore_report;

/// Called synchronously inside the worker holding the original admission.
pub fn day(
    slot: &SharedOperationSlot,
    ticket: ArchiveTicket,
    installed: OwnedInstalledArchiveRead,
    day: &str,
    now: u64,
) {
    run(slot, ticket, installed, Some(day), now);
}

/// Called synchronously inside the worker holding the original admission.
pub fn all(
    slot: &SharedOperationSlot,
    ticket: ArchiveTicket,
    installed: OwnedInstalledArchiveRead,
    now: u64,
) {
    run(slot, ticket, installed, None, now);
}

fn entry(
    slot: &SharedOperationSlot,
    ticket: &ArchiveTicket,
    read: &OwnedReadOperation,
    outside: &DirectReadFinishOwner,
) -> Result<(), &'static str> {
    if !Arc::ptr_eq(read.journal_root(), &ticket.0.root)
        || !Arc::ptr_eq(read.owned_clock(), &ticket.0.clock)
        || !Arc::ptr_eq(read.carrier(), outside.carrier())
        || !Arc::ptr_eq(read.journal_root(), outside.journal_root())
        || !Arc::ptr_eq(read.owned_clock(), outside.owned_clock())
    {
        return Err("failed");
    }
    let state = slot.lock().map_err(|_| "failed")?;
    if !state.archive.matches(ticket) || !state.archive.retained.is_empty() {
        return Err("failed");
    }
    let current = state.current.as_ref().ok_or("failed")?;
    if current.generation != ticket.generation()
        || !current.worker_active
        || operation::is_terminal(&current.view.phase)
        || current.pending_handoff_terminal.is_some()
        || current.handoff_poll.is_some()
    {
        return Err("failed");
    }
    #[cfg(windows)]
    if current.cleanup.is_some() {
        return Err("failed");
    }
    if current
        .archive_cancellation
        .as_ref()
        .is_some_and(|flag| flag.is_cancelled())
    {
        return Err("cancelled");
    }
    Ok(())
}

fn run(
    slot: &SharedOperationSlot,
    ticket: ArchiveTicket,
    installed: OwnedInstalledArchiveRead,
    day: Option<&str>,
    now: u64,
) {
    // The outside owner never enters the actor's unwind boundary.
    let (read, outside) = installed.into_parts();
    if let Err(reason) = entry(slot, &ticket, &read, &outside) {
        return return_owned(
            slot,
            ticket,
            ArchivePayload::ReadHeld {
                read: Box::new(read.into_owned_hold()),
                outside,
            },
            Terminal::error(reason),
        );
    }
    let mut terminal = Terminal::error("failed");
    let actor = catch_unwind(AssertUnwindSafe(|| {
        let native = match day {
            Some(day) => owned::restore_owned_bound_day(read, day, now),
            None => owned::restore_owned_bound_all(read, now),
        };
        // This snapshot remains available outside the closure if consuming
        // cleanup unwinds before a final held/settled projection returns.
        project(
            &mut terminal,
            native.report(),
            restore_report::active(&native),
        );
        #[cfg(all(test, feature = "full-tests", target_os = "linux"))]
        tests::after_actor(&native);
        match settle(native) {
            Err(native) => {
                project(
                    &mut terminal,
                    native.report(),
                    restore_report::held(&native),
                );
                let refusal = native_refusal(native.refusal());
                if let Some(report) = terminal.report.as_mut() {
                    report["settlement"] = json!({"state":"held", "refusal":refusal});
                }
                Err(native)
            }
            Ok((settled, report)) => {
                project(
                    &mut terminal,
                    report.report(),
                    restore_report::settled(&report),
                );
                if let Some(view) = terminal.report.as_mut() {
                    view["settlement"] = json!({"state":"settled"});
                }
                Ok((settled, report))
            }
        }
    }));
    let payload = match actor {
        Ok(Err(native)) => ArchivePayload::NativeHeld { native, outside },
        Ok(Ok((settled, report))) => {
            #[cfg(all(test, feature = "full-tests", target_os = "linux"))]
            tests::before_finish(slot);
            // Runtime finish has a separate consuming boundary; actor unwind
            // protection does not claim to cover a panic inside this call.
            let runtime = outside.finish_owned(settled);
            if let Some(view) = terminal.report.as_mut() {
                view["runtime"] = runtime_facts(&runtime);
            }
            #[cfg(all(test, feature = "full-tests", target_os = "linux"))]
            tests::before_handoff(slot);
            ArchivePayload::NativeFinished { report, runtime }
        }
        Err(_) => {
            let runtime = outside.into_unknown();
            // Preserve genuinely projected effects; unavailable native owners
            // and any unreturned settled report remain unknown.
            if let Some(report) = terminal.report.as_mut() {
                report["settlement"] = json!({"state":"unknown"});
                report["runtime"] = runtime_facts(&runtime);
            }
            terminal.phase = "error".into();
            terminal.reason_code = Some("failed".into());
            ArchivePayload::Unknown {
                runtime,
                report: None,
            }
        }
    };
    return_owned(slot, ticket, payload, terminal);
}

fn settle(
    native: OwnedRestoreRunOutcome,
) -> Result<(OwnedSettledDirectRead, SettledRestoreReport), Box<OwnedRestoreFinishHold>> {
    #[cfg(all(test, feature = "full-tests", target_os = "linux"))]
    return tests::settle(native);
    #[cfg(not(all(test, feature = "full-tests", target_os = "linux")))]
    native.settle_direct()
}

fn project(terminal: &mut Terminal, result: &RestoreResult, report: Value) {
    let phase = match result.status.as_str() {
        "ok" | "no_op" => "done",
        "refused" => "refused",
        "degraded" => "degraded",
        _ => "error",
    };
    *terminal = Terminal::phase(phase, result.reason.clone());
    terminal.report = Some(report);
}

fn native_refusal(refusal: RestoreFinishRefusal<'_>) -> Value {
    match refusal {
        RestoreFinishRefusal::Native {
            file,
            reason,
            source,
            readback,
        } => json!({
            "kind":"native", "file":file, "reason":reason,
            "error":source.map(|error| error.kind().to_string()),
            "readback":readback.map(|error| error.to_string()),
        }),
        RestoreFinishRefusal::Lower(reason) => json!({"kind":"lower", "reason":reason.to_string()}),
    }
}

fn runtime_facts(runtime: &OwnedDirectReadFinish) -> Value {
    use solstone_core_journal_io::media::ActivityCarrierRetirementEffect as Effect;
    json!({
        "state":match runtime.state() {
            DirectReadFinishState::Completed => "completed",
            DirectReadFinishState::Held => "held",
            DirectReadFinishState::Unknown => "unknown",
        },
        "reason":runtime.reason().map(|reason| reason.to_string()),
        "retirement":runtime.retirement().map(|retirement| json!({
            "effect":match retirement.receipt.effect() {
                Effect::NotAttempted => "not_attempted",
                Effect::Attempted => "attempted",
                Effect::Removed => "removed",
            },
            "root_durability":retirement.receipt.root_durability(),
            "confirmation":retirement.receipt.confirmation(),
            "failures":retirement.receipt.failures().iter().map(|failure| json!({
                "error":failure.kind().to_string(),
            })).collect::<Vec<_>>(),
        })),
    })
}

fn return_owned(
    slot: &SharedOperationSlot,
    ticket: ArchiveTicket,
    payload: ArchivePayload,
    terminal: Terminal,
) {
    if let Err(rejected) = handoff(slot, ticket, payload, terminal) {
        park_rejected(slot, rejected);
    }
}

#[cfg(all(test, feature = "full-tests"))]
#[cfg(target_os = "linux")]
mod tests;
