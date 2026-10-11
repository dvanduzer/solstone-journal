// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Run an already admitted reviewed removal and retain its whole owning return.

use std::panic::{AssertUnwindSafe, catch_unwind};

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{Value, json};
use solstone_core_retention::Outcome;
use solstone_core_retention::remove_marked::{
    OwnedReviewedArchiveFinish, remove_reviewed_with_owned_archive,
};
use solstone_core_retention::review_context::CheckedReviewContext;

use super::{ArchivePayload, PreparedArchiveWorker, entry, return_owned, runtime_facts};
use crate::operation::{SharedOperationSlot, Terminal};
use crate::removal_report;

/// Called synchronously inside the worker holding the original admission.
pub fn remove(
    slot: &SharedOperationSlot,
    worker: PreparedArchiveWorker,
    checked: CheckedReviewContext,
    today: NaiveDate,
    now: DateTime<Utc>,
) {
    // The outside owner never enters the actor's unwind boundary.
    let PreparedArchiveWorker {
        ticket,
        custody,
        read,
        outside,
    } = worker;
    if let Err(reason) = entry(slot, &ticket, &read, &outside) {
        return return_owned(
            slot,
            ticket,
            Some(custody),
            ArchivePayload::ReadHeld {
                read: Box::new(read.into_owned_hold()),
                outside,
            },
            Terminal::error(reason),
        );
    }
    let mut terminal = Terminal::error("failed");
    let actor = catch_unwind(AssertUnwindSafe(|| {
        let proof = remove_reviewed_with_owned_archive(checked, read, today, now);
        project(
            &mut terminal,
            proof.outcome(),
            serde_json::to_value(removal_report::owned_archive(&proof))
                .expect("borrowed removal facts serialize"),
        );
        #[cfg(all(test, feature = "full-tests", target_os = "linux"))]
        tests::after_actor(&proof);
        match proof.finish() {
            OwnedReviewedArchiveFinish::Held(proof) => {
                project(
                    &mut terminal,
                    proof.outcome(),
                    serde_json::to_value(removal_report::held_archive(&proof))
                        .expect("closed removal facts serialize"),
                );
                terminal.phase = if proof.outcome().removed_paths().next().is_some() {
                    "degraded"
                } else {
                    "refused"
                }
                .into();
                terminal.reason_code = Some("failed".into());
                if let Some(report) = terminal.report.as_mut() {
                    report["settlement"] = json!({"state":"held"});
                }
                Err(proof)
            }
            OwnedReviewedArchiveFinish::Settled(proof) => {
                let (read, report) = proof.into_parts();
                project(
                    &mut terminal,
                    report.outcome(),
                    serde_json::to_value(removal_report::settled_archive(&report))
                        .expect("settled removal facts serialize"),
                );
                if let Some(view) = terminal.report.as_mut() {
                    view["settlement"] = json!({"state":"settled"});
                }
                Ok((read, report))
            }
        }
    }));
    let payload = match actor {
        Ok(Err(proof)) => ArchivePayload::ProofHeld { proof, outside },
        Ok(Ok((read, report))) => {
            #[cfg(all(test, feature = "full-tests", target_os = "linux"))]
            tests::before_finish(slot);
            // Keep the genuine settled report and original custody outside
            // consuming Runtime finish; an unwind cannot certify retirement.
            match catch_unwind(AssertUnwindSafe(|| outside.finish_owned(read))) {
                Ok(runtime) => {
                    if let Some(view) = terminal.report.as_mut() {
                        view["runtime"] = runtime_facts(&runtime);
                    }
                    #[cfg(all(test, feature = "full-tests", target_os = "linux"))]
                    tests::before_handoff(slot);
                    ArchivePayload::ProofFinished { report, runtime }
                }
                Err(_) => {
                    terminal.phase = "error".into();
                    terminal.reason_code = Some("failed".into());
                    if let Some(view) = terminal.report.as_mut() {
                        view["runtime"] = json!({"state":"unknown"});
                    }
                    ArchivePayload::ProofFinishUnknown { report }
                }
            }
        }
        Err(_) => {
            let runtime = outside.into_unknown();
            if let Some(report) = terminal.report.as_mut() {
                report["settlement"] = json!({"state":"unknown"});
                report["runtime"] = runtime_facts(&runtime);
            }
            terminal.phase = "error".into();
            terminal.reason_code = Some("failed".into());
            ArchivePayload::ProofUnknown { runtime }
        }
    };
    return_owned(slot, ticket, Some(custody), payload, terminal);
}

fn project(terminal: &mut Terminal, outcome: &Outcome, report: Value) {
    let phase = if outcome.halted.is_some() {
        "error"
    } else if outcome.has_failures() {
        "degraded"
    } else {
        "done"
    };
    *terminal = Terminal::phase(phase, (phase != "done").then(|| "failed".into()));
    terminal.report = Some(report);
}

#[cfg(all(test, feature = "full-tests"))]
#[cfg(target_os = "linux")]
mod tests;
