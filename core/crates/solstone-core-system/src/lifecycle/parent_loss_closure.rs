// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Closing a generation whose supervisor and coordinator both exited before
//! the coordinator could publish a terminal result.
//!
//! The incident shape: a service manager's stop timeout SIGKILLs the whole
//! control group, or the machine loses power. The supervisor and its
//! coordinator die together, the generation stays open, and every later start
//! refused it as `CoordinatorNotLive` until an operator moved
//! `health/parent-loss` aside by hand.
//!
//! Liveness here is proven by lock, and only corroborated by identity. The
//! caller holds the coordinator lease (`ParentLossLedger::acquire_coordinator_lease`,
//! an advisory `flock` the kernel releases on death of any kind) and its parent
//! supervisor holds the supervisor singleton lock, so neither recorded
//! authority can still be running. Both identities are still observed, and a
//! `SameLive` observation refuses. ⛔ "Cannot observe" is never read as death
//! on its own: the lock is the proof, the observation is the veto.
//!
//! Every process the dead generation admitted has an exact identity in its
//! `admissions/` directory. Each one is proven exited, retired here with an
//! exact signal, or the start refuses with a reason that converges once the
//! process is gone. Two instances of a service never run against one journal.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use solstone_core_journal_io::{LockOptions, hold_lock, write_json};

use super::HostedServiceKind;
use super::parent_loss_admission::{
    AdmissionIdentity, AdmissionResultState, admission_directory,
    read_parent_loss_admission_acknowledgement_in, read_parent_loss_admission_intent,
    read_parent_loss_admission_result, read_parent_loss_admission_spawn_identity_in,
    validate_launch_id,
};
use super::parent_loss_coordinator::{
    PARENT_LOSS_COORDINATOR_RETIREMENT_DEADLINE, PARENT_LOSS_COORDINATOR_TERMINAL_WRITE_BUDGET,
    SealedAdmission, clear_supervisor_heartbeat,
};
use super::parent_loss_ledger::{
    BootstrapRecoveryReason, PARENT_LOSS_LEDGER_SCHEMA_V1, ParentLossGeneration,
    ParentLossGenerationRecord, ParentLossLedger, ParentLossLedgerError, ParentLossPhase,
    ParentLossTerminalDisposition, ParentLossUnresolvedReason, digest_bytes, json_options,
};
#[cfg(target_os = "linux")]
use crate::process::InspectResult;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::process::{DescendantTerminationOutcome, terminate_descendants_exact};
use crate::process::{
    InstanceVerdict, ProcessInstance, ProcessInstanceSource, ProcessOwner, SignalKind,
    SystemProcessInstanceSource, TerminationError, process_owner, signal_exact_instance,
};

pub const PARENT_LOSS_CLOSURE_SCHEMA_V1: u32 = 1;
pub(crate) const PARENT_LOSS_RECOVERY_SCHEMA_V1: u32 = 1;
pub(crate) const RECOVERY_CHECKPOINT_BATCH: usize = 32;
/// How long a transient `Unverifiable` (a pid exiting between two procfs
/// reads) is re-observed before it is recorded as unobservable.
const OBSERVATION_SETTLE_WINDOW: Duration = Duration::from_millis(500);
const OBSERVATION_POLL: Duration = Duration::from_millis(25);
const ADMISSION_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
/// Time kept back from the deadline for SIGKILL escalation and its reap.
const ESCALATION_RESERVE: Duration = Duration::from_millis(1_500);

/// How a recorded authority was found when the generation was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityObservation {
    /// The recorded pid+birth observed `NotSameOrExited`.
    Exited,
    /// The recorded pid could not be inspected within the settle window. The
    /// lock it would have held was free, which is the proof relied on; the
    /// observation is recorded, not acted on.
    Unobservable,
    /// The reservation died before this identity was ever persisted.
    NeverRecorded,
}

/// What became of one admission the dead generation recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionFinding {
    /// The exact identity observed `NotSameOrExited` before any signal.
    Exited,
    /// The exact identity was still live and was signalled here.
    Retired {
        escalated: bool,
    },
    /// An intent with neither a result nor an acknowledgement: the child never
    /// completed admission, so it never bound a listener or served.
    NeverAcknowledged,
    SpawnFailed,
    RejectedAndReaped,
    /// The pid could not be inspected, and the process table says it now
    /// belongs to another user: a reused pid, not ours. A positive
    /// observation, never an inference from "cannot tell".
    PidReusedByAnotherUser,
    /// The drop carried no identity we could act on.
    Unidentified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionClosure {
    pub launch_id: String,
    pub service: Option<HostedServiceKind>,
    pub instance: Option<ProcessInstance>,
    /// The uid the admitted process ran as, from its admission identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    pub finding: AdmissionFinding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeartbeatRetirement {
    Removed,
    Absent,
    /// The record predates the heartbeat field; the stale-heartbeat collector
    /// retires it after its own floor.
    NotRecorded,
    NotCleared,
}

/// The durable account of a closure, written into the closed generation's
/// record beside its terminal disposition. `journal doctor` names these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbandonedGenerationClosure {
    pub schema: u32,
    /// The successor coordinator that closed it.
    pub closed_by: ProcessInstance,
    pub successor_generation: ParentLossGeneration,
    /// Unix seconds.
    pub closed_at: u64,
    pub supervisor: AuthorityObservation,
    pub coordinator: AuthorityObservation,
    pub heartbeat: HeartbeatRetirement,
    /// Bookkeeping files set aside beside the record, never deleted.
    pub set_aside: Vec<String>,
    /// Anomalies worth a human's eye, in plain words.
    pub notes: Vec<String>,
    pub admissions: Vec<AdmissionClosure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecoveryEntryState {
    Completed,
    Provisional,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecoveryEntry {
    pub(crate) launch_id: String,
    pub(crate) state: RecoveryEntryState,
    pub(crate) service: Option<HostedServiceKind>,
    pub(crate) instance: Option<ProcessInstance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parent_launch_id: Option<String>,
    pub(crate) finding: AdmissionFinding,
    pub(crate) intent_digest: Option<String>,
    pub(crate) result_digest: Option<String>,
    pub(crate) ack_digest: Option<String>,
    pub(crate) spawn_identity_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecoveryEscalation {
    pub(crate) launch_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) service: Option<HostedServiceKind>,
    pub(crate) instance: ProcessInstance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parent_launch_id: Option<String>,
    pub(crate) escalate_at_unix_ms: u64,
    pub(crate) descendant_escalated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ParentLossClosureRecovery {
    pub(crate) schema: u32,
    pub(crate) generation: ParentLossGeneration,
    pub(crate) sealed_phase: ParentLossPhase,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) set_aside: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) escalation: Option<RecoveryEscalation>,
    pub(crate) entries: Vec<RecoveryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct AdmissionDropDigests {
    pub(crate) intent: Option<String>,
    pub(crate) result: Option<String>,
    pub(crate) ack: Option<String>,
    pub(crate) spawn_identity: Option<String>,
}

impl AdmissionDropDigests {
    pub(crate) fn is_all_absent(&self) -> bool {
        self.intent.is_none()
            && self.result.is_none()
            && self.ack.is_none()
            && self.spawn_identity.is_none()
    }
}

/// Sends exact signals during retirement and answers who owns a pid.
/// Injected so every refuse path is testable without a live process to kill.
pub trait AdmissionRetirer: Send + Sync {
    fn signal(&self, instance: ProcessInstance, signal: SignalKind)
    -> Result<(), TerminationError>;
    fn owner(&self, pid: u32) -> ProcessOwner;
}

/// Production retirer: every signal is guarded by a fresh exact observation.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemAdmissionRetirer;

impl AdmissionRetirer for SystemAdmissionRetirer {
    fn signal(
        &self,
        instance: ProcessInstance,
        signal: SignalKind,
    ) -> Result<(), TerminationError> {
        signal_exact_instance(instance, signal, &SystemProcessInstanceSource)
    }

    fn owner(&self, pid: u32) -> ProcessOwner {
        process_owner(pid)
    }
}

/// What the closer needs beyond the ledger and the lease it is called under.
pub struct ClosingAuthority<'a> {
    /// The successor coordinator's own identity.
    pub closed_by: ProcessInstance,
    pub source: &'a dyn ProcessInstanceSource,
    pub retirer: &'a dyn AdmissionRetirer,
    /// Retirement must finish by here or the start refuses.
    pub deadline: Instant,
}

/// The generation being closed, as the caller found it under the active lock.
pub(crate) struct AbandonedGeneration {
    pub generation: ParentLossGeneration,
    pub supervisor: Option<ProcessInstance>,
    pub coordinator: Option<ProcessInstance>,
    /// `None` when the record was missing or was set aside as unreadable.
    pub record: Option<ParentLossGenerationRecord>,
    pub set_aside: Vec<String>,
    pub notes: Vec<String>,
}

pub(crate) enum ClosureOutcome {
    /// The closure is written into the generation's record.
    Closed,
    Refused(BootstrapRecoveryReason),
}

/// Read a generation's record for closure. A record that cannot be parsed or
/// names another generation is bookkeeping we cannot trust and is set aside
/// beside itself; the admissions directory is the evidence that survives it.
pub(crate) fn load_record_for_closure(
    ledger: &ParentLossLedger,
    generation: ParentLossGeneration,
) -> Result<(Option<ParentLossGenerationRecord>, Vec<String>), ParentLossLedgerError> {
    use solstone_core_journal_io::durability::{
        ArtifactId, DurableRead, read_json_durable_validated,
    };
    let path = ledger.record_path(generation);
    match read_json_durable_validated::<ParentLossGenerationRecord>(
        ArtifactId::ParentLossRecord,
        &path,
        |record| {
            if record.generation == generation {
                Ok(())
            } else {
                Err(format!(
                    "generation mismatch: expected {generation}, found {}",
                    record.generation
                ))
            }
        },
    ) {
        Ok(DurableRead::Present(record)) => Ok((Some(record), Vec::new())),
        Ok(DurableRead::SetAside(aside)) => {
            let aside = aside
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            Ok((None, vec![aside]))
        }
        Ok(DurableRead::Absent | DurableRead::Unreadable { .. }) => Ok((None, Vec::new())),
        Err(error) => Err(error.into()),
    }
}

/// Compute grace duration as:
/// PARENT_LOSS_COORDINATOR_RETIREMENT_DEADLINE - PARENT_LOSS_COORDINATOR_TERMINAL_WRITE_BUDGET - ESCALATION_RESERVE.
pub(crate) fn grace_duration() -> Duration {
    PARENT_LOSS_COORDINATOR_RETIREMENT_DEADLINE
        .saturating_sub(PARENT_LOSS_COORDINATOR_TERMINAL_WRITE_BUDGET)
        .saturating_sub(ESCALATION_RESERVE)
}

pub(crate) fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or_default()
}

pub(crate) fn recovery_entry_state(finding: &AdmissionFinding) -> RecoveryEntryState {
    match finding {
        AdmissionFinding::Exited
        | AdmissionFinding::Retired { .. }
        | AdmissionFinding::SpawnFailed
        | AdmissionFinding::RejectedAndReaped
        | AdmissionFinding::PidReusedByAnotherUser => RecoveryEntryState::Completed,
        AdmissionFinding::NeverAcknowledged | AdmissionFinding::Unidentified => {
            RecoveryEntryState::Provisional
        }
    }
}

pub(crate) fn recovery_skip_authorized(
    entry: &RecoveryEntry,
    launch_id: &str,
    current_digests: &AdmissionDropDigests,
) -> bool {
    if entry.launch_id.is_empty() || entry.launch_id != launch_id {
        return false;
    }
    if entry.state != RecoveryEntryState::Completed {
        return false;
    }
    if current_digests.is_all_absent() {
        return false;
    }
    if entry.intent_digest != current_digests.intent
        || entry.result_digest != current_digests.result
        || entry.ack_digest != current_digests.ack
        || entry.spawn_identity_digest != current_digests.spawn_identity
    {
        return false;
    }
    match entry.finding {
        AdmissionFinding::Exited
        | AdmissionFinding::Retired { .. }
        | AdmissionFinding::PidReusedByAnotherUser => entry.instance.is_some(),
        AdmissionFinding::SpawnFailed | AdmissionFinding::RejectedAndReaped => {
            current_digests.result.is_some()
        }
        AdmissionFinding::NeverAcknowledged | AdmissionFinding::Unidentified => false,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_escalation_deadline(
    stored: Option<&RecoveryEscalation>,
    launch_id: &str,
    service: Option<HostedServiceKind>,
    instance: ProcessInstance,
    uid: Option<u32>,
    parent_launch_id: Option<String>,
    descendant_escalated: bool,
    now_ms: u64,
    grace: Duration,
) -> RecoveryEscalation {
    if let Some(stored) = stored
        && stored.launch_id == launch_id
        && stored.instance == instance
    {
        // The escalation deadline is not renewed.
        let mut reused = stored.clone();
        if descendant_escalated {
            reused.descendant_escalated = true;
        }
        return reused;
    }
    let grace_ms = grace.as_millis().min(u64::MAX as u128) as u64;
    RecoveryEscalation {
        launch_id: launch_id.to_string(),
        service,
        instance,
        uid,
        parent_launch_id,
        escalate_at_unix_ms: now_ms.saturating_add(grace_ms),
        descendant_escalated,
    }
}

pub(crate) fn load_recovery_checkpoint(
    ledger: &ParentLossLedger,
    generation: ParentLossGeneration,
) -> Result<(Option<ParentLossClosureRecovery>, Vec<String>), ParentLossLedgerError> {
    use solstone_core_journal_io::durability::{
        ArtifactId, DurableRead, read_json_durable_validated,
    };
    let path = ledger.closure_recovery_path(generation);
    match read_json_durable_validated::<ParentLossClosureRecovery>(
        ArtifactId::ParentLossClosureRecovery,
        &path,
        |recovery| {
            if recovery.schema != PARENT_LOSS_RECOVERY_SCHEMA_V1 {
                return Err(format!("unexpected recovery schema: {}", recovery.schema));
            }
            if recovery.generation != generation {
                return Err(format!(
                    "generation mismatch: expected {generation}, found {}",
                    recovery.generation
                ));
            }
            if recovery.sealed_phase != ParentLossPhase::Sealed {
                return Err(format!(
                    "expected sealed phase Sealed, found {:?}",
                    recovery.sealed_phase
                ));
            }
            Ok(())
        },
    ) {
        Ok(DurableRead::Present(recovery)) => Ok((Some(recovery), Vec::new())),
        Ok(DurableRead::SetAside(aside)) => {
            let aside = aside
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            Ok((None, vec![aside]))
        }
        Ok(DurableRead::Absent | DurableRead::Unreadable { .. }) => Ok((None, Vec::new())),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn write_recovery_checkpoint(
    ledger: &ParentLossLedger,
    generation: ParentLossGeneration,
    checkpoint: &ParentLossClosureRecovery,
) -> Result<(), ParentLossLedgerError> {
    let path = ledger.closure_recovery_path(generation);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_json(&path, checkpoint, json_options())?;
    Ok(())
}

fn read_drop_bytes(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn upsert_recovery_entry(checkpoint: &mut ParentLossClosureRecovery, entry: RecoveryEntry) {
    if let Some(pos) = checkpoint
        .entries
        .iter()
        .position(|e| e.launch_id == entry.launch_id)
    {
        checkpoint.entries[pos] = entry;
    } else {
        checkpoint.entries.push(entry);
    }
}

enum SingleExitVerdict {
    Exited,
    Settled(AdmissionFinding),
    Unverifiable,
    SameLive,
}

fn wait_single_for_exit(
    authority: &ClosingAuthority<'_>,
    instance: ProcessInstance,
    uid: Option<u32>,
    until: Instant,
) -> SingleExitVerdict {
    loop {
        match authority.source.observe(&instance) {
            InstanceVerdict::NotSameOrExited => return SingleExitVerdict::Exited,
            InstanceVerdict::SameLive { .. } => {
                if Instant::now() >= until {
                    return SingleExitVerdict::SameLive;
                }
            }
            InstanceVerdict::Unverifiable => {
                match settle_unverifiable(authority, instance.pid, uid) {
                    Some(finding) => return SingleExitVerdict::Settled(finding),
                    None => {
                        if Instant::now() >= until {
                            return SingleExitVerdict::Unverifiable;
                        }
                    }
                }
            }
        }
        thread::sleep(OBSERVATION_POLL.min(until.saturating_duration_since(Instant::now())));
    }
}

enum RetirementOutcome {
    Completed,
    UnclassifiedSameLive,
    Refused(ClosureOutcome),
}

#[allow(clippy::too_many_arguments)]
fn retire_admission_identity(
    ledger: &ParentLossLedger,
    generation: ParentLossGeneration,
    authority: &ClosingAuthority<'_>,
    checkpoint: &mut ParentLossClosureRecovery,
    launch_id: &str,
    service: Option<HostedServiceKind>,
    instance: ProcessInstance,
    uid: Option<u32>,
    parent_launch_id: Option<String>,
    current_digests: &AdmissionDropDigests,
) -> Result<RetirementOutcome, ParentLossLedgerError> {
    #[cfg(any(test, feature = "test-hooks"))]
    if crate::process::retirement_skip_test_fault()
        && matches!(
            observe_settled(authority.source, &instance, authority.deadline),
            InstanceVerdict::SameLive { .. }
        )
    {
        write_recovery_checkpoint(ledger, generation, checkpoint)?;
        return Ok(RetirementOutcome::Refused(ClosureOutcome::Refused(
            BootstrapRecoveryReason::AbandonedAdmissionLive,
        )));
    }

    match observe_settled(authority.source, &instance, authority.deadline) {
        InstanceVerdict::NotSameOrExited => {
            let finding = if checkpoint
                .escalation
                .as_ref()
                .is_some_and(|e| e.launch_id == launch_id && e.descendant_escalated)
            {
                AdmissionFinding::Retired { escalated: true }
            } else {
                AdmissionFinding::Exited
            };
            if checkpoint
                .escalation
                .as_ref()
                .is_some_and(|e| e.launch_id == launch_id)
            {
                checkpoint.escalation = None;
            }
            let entry = RecoveryEntry {
                launch_id: launch_id.to_string(),
                state: RecoveryEntryState::Completed,
                service,
                instance: Some(instance),
                uid,
                parent_launch_id,
                finding,
                intent_digest: current_digests.intent.clone(),
                result_digest: current_digests.result.clone(),
                ack_digest: current_digests.ack.clone(),
                spawn_identity_digest: current_digests.spawn_identity.clone(),
            };
            upsert_recovery_entry(checkpoint, entry);
            Ok(RetirementOutcome::Completed)
        }
        InstanceVerdict::Unverifiable => match settle_unverifiable(authority, instance.pid, uid) {
            Some(finding) => {
                if checkpoint
                    .escalation
                    .as_ref()
                    .is_some_and(|e| e.launch_id == launch_id)
                {
                    checkpoint.escalation = None;
                }
                let entry = RecoveryEntry {
                    launch_id: launch_id.to_string(),
                    state: RecoveryEntryState::Completed,
                    service,
                    instance: Some(instance),
                    uid,
                    parent_launch_id,
                    finding,
                    intent_digest: current_digests.intent.clone(),
                    result_digest: current_digests.result.clone(),
                    ack_digest: current_digests.ack.clone(),
                    spawn_identity_digest: current_digests.spawn_identity.clone(),
                };
                upsert_recovery_entry(checkpoint, entry);
                Ok(RetirementOutcome::Completed)
            }
            None => {
                write_recovery_checkpoint(ledger, generation, checkpoint)?;
                Ok(RetirementOutcome::Refused(ClosureOutcome::Refused(
                    BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
                )))
            }
        },
        InstanceVerdict::SameLive { .. } => {
            if checkpoint
                .escalation
                .as_ref()
                .is_some_and(|c| c.launch_id != launch_id)
            {
                return Ok(RetirementOutcome::UnclassifiedSameLive);
            }

            let claim = resolve_escalation_deadline(
                checkpoint.escalation.as_ref(),
                launch_id,
                service,
                instance,
                uid,
                parent_launch_id.clone(),
                false,
                unix_millis(),
                grace_duration(),
            );
            checkpoint.escalation = Some(claim.clone());
            // Persist checkpoint with claim before any wait or signal.
            write_recovery_checkpoint(ledger, generation, checkpoint)?;

            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let descendant_escalated = if let Some(target_uid) = uid {
                let now = Instant::now();
                let now_ms = unix_millis();
                let escalate_at_instant = if claim.escalate_at_unix_ms > now_ms {
                    now + Duration::from_millis(claim.escalate_at_unix_ms - now_ms)
                } else {
                    now
                };
                let census_deadline = escalate_at_instant.min(authority.deadline);
                let descendant_budget = census_deadline.saturating_duration_since(now);
                match terminate_descendants_exact(
                    instance,
                    target_uid,
                    descendant_budget,
                    authority.source,
                    || {},
                ) {
                    Ok(DescendantTerminationOutcome::Graceful) => false,
                    Ok(DescendantTerminationOutcome::EscalatedAndReaped) => true,
                    Err(_) => {
                        write_recovery_checkpoint(ledger, generation, checkpoint)?;
                        return Ok(RetirementOutcome::Refused(ClosureOutcome::Refused(
                            BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
                        )));
                    }
                }
            } else {
                false
            };
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            let descendant_escalated = false;

            if descendant_escalated && let Some(esc) = checkpoint.escalation.as_mut() {
                esc.descendant_escalated = true;
            }

            if Instant::now() >= authority.deadline {
                write_recovery_checkpoint(ledger, generation, checkpoint)?;
                return Ok(RetirementOutcome::Refused(ClosureOutcome::Refused(
                    BootstrapRecoveryReason::CoordinatorNotLive,
                )));
            }

            // Complete census: Terminate once.
            let _ = authority.retirer.signal(instance, SignalKind::Terminate);

            let now = Instant::now();
            let now_ms = unix_millis();
            let escalate_at_instant = if claim.escalate_at_unix_ms > now_ms {
                now + Duration::from_millis(claim.escalate_at_unix_ms - now_ms)
            } else {
                now
            };
            let first_wait = escalate_at_instant.min(authority.deadline);
            let mut exit_verdict = wait_single_for_exit(authority, instance, uid, first_wait);

            let mut was_killed = false;
            if matches!(exit_verdict, SingleExitVerdict::SameLive) {
                let now_ms = unix_millis();
                if now_ms >= claim.escalate_at_unix_ms {
                    let _ = authority.retirer.signal(instance, SignalKind::Kill);
                    was_killed = true;
                    exit_verdict =
                        wait_single_for_exit(authority, instance, uid, authority.deadline);
                }
            }

            match exit_verdict {
                SingleExitVerdict::Exited => {
                    let finding = AdmissionFinding::Retired {
                        escalated: was_killed
                            || descendant_escalated
                            || checkpoint
                                .escalation
                                .as_ref()
                                .is_some_and(|e| e.descendant_escalated),
                    };
                    checkpoint.escalation = None;
                    let entry = RecoveryEntry {
                        launch_id: launch_id.to_string(),
                        state: RecoveryEntryState::Completed,
                        service,
                        instance: Some(instance),
                        uid,
                        parent_launch_id,
                        finding,
                        intent_digest: current_digests.intent.clone(),
                        result_digest: current_digests.result.clone(),
                        ack_digest: current_digests.ack.clone(),
                        spawn_identity_digest: current_digests.spawn_identity.clone(),
                    };
                    upsert_recovery_entry(checkpoint, entry);
                    write_recovery_checkpoint(ledger, generation, checkpoint)?;
                    Ok(RetirementOutcome::Completed)
                }
                SingleExitVerdict::Settled(finding) => {
                    checkpoint.escalation = None;
                    let entry = RecoveryEntry {
                        launch_id: launch_id.to_string(),
                        state: RecoveryEntryState::Completed,
                        service,
                        instance: Some(instance),
                        uid,
                        parent_launch_id,
                        finding,
                        intent_digest: current_digests.intent.clone(),
                        result_digest: current_digests.result.clone(),
                        ack_digest: current_digests.ack.clone(),
                        spawn_identity_digest: current_digests.spawn_identity.clone(),
                    };
                    upsert_recovery_entry(checkpoint, entry);
                    write_recovery_checkpoint(ledger, generation, checkpoint)?;
                    Ok(RetirementOutcome::Completed)
                }
                SingleExitVerdict::Unverifiable => {
                    write_recovery_checkpoint(ledger, generation, checkpoint)?;
                    Ok(RetirementOutcome::Refused(ClosureOutcome::Refused(
                        BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
                    )))
                }
                SingleExitVerdict::SameLive => {
                    write_recovery_checkpoint(ledger, generation, checkpoint)?;
                    if was_killed {
                        Ok(RetirementOutcome::Refused(ClosureOutcome::Refused(
                            BootstrapRecoveryReason::AbandonedAdmissionLive,
                        )))
                    } else {
                        Ok(RetirementOutcome::Refused(ClosureOutcome::Refused(
                            BootstrapRecoveryReason::CoordinatorNotLive,
                        )))
                    }
                }
            }
        }
    }
}

/// Close one abandoned generation. The caller holds the active-generation lock
/// and the coordinator lease, and its parent holds the supervisor lock.
pub(crate) fn close_abandoned_generation(
    ledger: &ParentLossLedger,
    abandoned: AbandonedGeneration,
    successor_generation: ParentLossGeneration,
    authority: &ClosingAuthority<'_>,
) -> Result<ClosureOutcome, ParentLossLedgerError> {
    let AbandonedGeneration {
        generation,
        supervisor,
        coordinator,
        record,
        mut set_aside,
        mut notes,
    } = abandoned;

    // 1. Existing SameLive vetoes.
    let supervisor_observation = match supervisor {
        None => AuthorityObservation::NeverRecorded,
        Some(instance) => match observe_settled(authority.source, &instance, authority.deadline) {
            InstanceVerdict::SameLive { .. } => {
                return Ok(ClosureOutcome::Refused(
                    BootstrapRecoveryReason::SupervisorLive,
                ));
            }
            InstanceVerdict::NotSameOrExited => AuthorityObservation::Exited,
            InstanceVerdict::Unverifiable => AuthorityObservation::Unobservable,
        },
    };
    let coordinator_observation = match coordinator {
        None => AuthorityObservation::NeverRecorded,
        Some(instance) => match observe_settled(authority.source, &instance, authority.deadline) {
            InstanceVerdict::SameLive { .. } => {
                return Ok(ClosureOutcome::Refused(
                    BootstrapRecoveryReason::ActiveCoordinator,
                ));
            }
            InstanceVerdict::NotSameOrExited => AuthorityObservation::Exited,
            InstanceVerdict::Unverifiable => AuthorityObservation::Unobservable,
        },
    };

    // 2. Seal before scan, so a late acknowledgement cannot serve.
    ledger.mark_sealed_for_closure(generation)?;

    // 3. Load the checkpoint with read_json_durable_validated.
    let (loaded_checkpoint, checkpoint_set_aside) = load_recovery_checkpoint(ledger, generation)?;
    for aside in checkpoint_set_aside {
        if !set_aside.contains(&aside) {
            set_aside.push(aside);
        }
    }
    let mut checkpoint = loaded_checkpoint.unwrap_or_else(|| ParentLossClosureRecovery {
        schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
        generation,
        sealed_phase: ParentLossPhase::Sealed,
        set_aside: Vec::new(),
        escalation: None,
        entries: Vec::new(),
    });
    for aside in &checkpoint.set_aside {
        if !set_aside.contains(aside) {
            set_aside.push(aside.clone());
        }
    }
    checkpoint.set_aside = set_aside.clone();

    // If an escalation claim was already stored, check if it already exited between attempts.
    if let Some(claim) = checkpoint.escalation.clone() {
        match authority.source.observe(&claim.instance) {
            InstanceVerdict::NotSameOrExited => {
                let finding = AdmissionFinding::Retired {
                    escalated: claim.descendant_escalated,
                };
                checkpoint.escalation = None;
                let admission_dir = admission_directory(ledger, generation).join(&claim.launch_id);
                let intent_bytes = read_drop_bytes(&admission_dir.join("intent.json"))?;
                let result_bytes = read_drop_bytes(&admission_dir.join("result.json"))?;
                let ack_bytes = read_drop_bytes(&admission_dir.join("acknowledgement.json"))?;
                let spawn_bytes = read_drop_bytes(&admission_dir.join("spawn-identity.json"))?;
                let digests = AdmissionDropDigests {
                    intent: intent_bytes.as_ref().map(|b| digest_bytes(b)),
                    result: result_bytes.as_ref().map(|b| digest_bytes(b)),
                    ack: ack_bytes.as_ref().map(|b| digest_bytes(b)),
                    spawn_identity: spawn_bytes.as_ref().map(|b| digest_bytes(b)),
                };
                let entry = RecoveryEntry {
                    launch_id: claim.launch_id.clone(),
                    state: RecoveryEntryState::Completed,
                    service: claim.service,
                    instance: Some(claim.instance),
                    uid: claim.uid,
                    parent_launch_id: claim.parent_launch_id.clone(),
                    finding,
                    intent_digest: digests.intent,
                    result_digest: digests.result,
                    ack_digest: digests.ack,
                    spawn_identity_digest: digests.spawn_identity,
                };
                upsert_recovery_entry(&mut checkpoint, entry);
            }
            InstanceVerdict::Unverifiable => {
                if let Some(finding) = settle_unverifiable(authority, claim.instance.pid, claim.uid)
                {
                    checkpoint.escalation = None;
                    let admission_dir =
                        admission_directory(ledger, generation).join(&claim.launch_id);
                    let intent_bytes = read_drop_bytes(&admission_dir.join("intent.json"))?;
                    let result_bytes = read_drop_bytes(&admission_dir.join("result.json"))?;
                    let ack_bytes = read_drop_bytes(&admission_dir.join("acknowledgement.json"))?;
                    let spawn_bytes = read_drop_bytes(&admission_dir.join("spawn-identity.json"))?;
                    let digests = AdmissionDropDigests {
                        intent: intent_bytes.as_ref().map(|b| digest_bytes(b)),
                        result: result_bytes.as_ref().map(|b| digest_bytes(b)),
                        ack: ack_bytes.as_ref().map(|b| digest_bytes(b)),
                        spawn_identity: spawn_bytes.as_ref().map(|b| digest_bytes(b)),
                    };
                    let entry = RecoveryEntry {
                        launch_id: claim.launch_id.clone(),
                        state: RecoveryEntryState::Completed,
                        service: claim.service,
                        instance: Some(claim.instance),
                        uid: claim.uid,
                        parent_launch_id: claim.parent_launch_id.clone(),
                        finding,
                        intent_digest: digests.intent,
                        result_digest: digests.result,
                        ack_digest: digests.ack,
                        spawn_identity_digest: digests.spawn_identity,
                    };
                    upsert_recovery_entry(&mut checkpoint, entry);
                }
            }
            InstanceVerdict::SameLive { .. } => {}
        }
    }

    // 4. Hold the admissions lock.
    let _lock = hold_lock(
        ledger.admission_lock_path(generation),
        LockOptions {
            timeout: ADMISSION_LOCK_TIMEOUT,
            poll_interval: Duration::from_millis(10),
            mode: Some(0o600),
        },
    )?;

    // 5. If an escalation claim is still live, retire that stored claim before read_dir.
    if let Some(claim) = checkpoint.escalation.clone() {
        if Instant::now() >= authority.deadline {
            write_recovery_checkpoint(ledger, generation, &checkpoint)?;
            return Ok(ClosureOutcome::Refused(
                BootstrapRecoveryReason::CoordinatorNotLive,
            ));
        }

        let admission_dir = admission_directory(ledger, generation).join(&claim.launch_id);
        if !admission_dir.is_dir() {
            checkpoint.escalation = None;
            write_recovery_checkpoint(ledger, generation, &checkpoint)?;
        } else {
            let intent_bytes = read_drop_bytes(&admission_dir.join("intent.json"))?;
            let result_bytes = read_drop_bytes(&admission_dir.join("result.json"))?;
            let ack_bytes = read_drop_bytes(&admission_dir.join("acknowledgement.json"))?;
            let spawn_bytes = read_drop_bytes(&admission_dir.join("spawn-identity.json"))?;
            let current_digests = AdmissionDropDigests {
                intent: intent_bytes.as_ref().map(|b| digest_bytes(b)),
                result: result_bytes.as_ref().map(|b| digest_bytes(b)),
                ack: ack_bytes.as_ref().map(|b| digest_bytes(b)),
                spawn_identity: spawn_bytes.as_ref().map(|b| digest_bytes(b)),
            };

            let intent = read_parent_loss_admission_intent(ledger, generation, &claim.launch_id)
                .ok()
                .flatten();
            let service = claim
                .service
                .or_else(|| intent.as_ref().and_then(|i| i.service));
            let spawn_identity =
                read_parent_loss_admission_spawn_identity_in(ledger, generation, &claim.launch_id)
                    .ok()
                    .flatten();
            let uid = claim
                .uid
                .or_else(|| spawn_identity.as_ref().map(|s| s.identity.uid));
            let parent_launch_id = claim
                .parent_launch_id
                .clone()
                .or_else(|| spawn_identity.and_then(|s| s.identity.parent_launch_id));

            match retire_admission_identity(
                ledger,
                generation,
                authority,
                &mut checkpoint,
                &claim.launch_id,
                service,
                claim.instance,
                uid,
                parent_launch_id,
                &current_digests,
            )? {
                RetirementOutcome::Completed => {
                    write_recovery_checkpoint(ledger, generation, &checkpoint)?;
                }
                RetirementOutcome::UnclassifiedSameLive => {
                    unreachable!("claim cannot conflict with itself")
                }
                RetirementOutcome::Refused(outcome) => {
                    return Ok(outcome);
                }
            }
        }
    }

    // 6. Stream fs::read_dir.
    let directory = admission_directory(ledger, generation);
    let dir_entries = match fs::read_dir(&directory) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };

    let mut unflushed_count = 0usize;
    let mut directory_launch_ids = HashSet::new();
    let mut validated_launch_ids = HashSet::new();

    if let Some(entries) = dir_entries {
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let launch_id = entry.file_name().to_string_lossy().into_owned();
            validate_launch_id(&launch_id)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            directory_launch_ids.insert(launch_id.clone());

            if Instant::now() >= authority.deadline {
                write_recovery_checkpoint(ledger, generation, &checkpoint)?;
                return Ok(ClosureOutcome::Refused(
                    BootstrapRecoveryReason::CoordinatorNotLive,
                ));
            }

            let admission_dir = entry.path();
            let intent_bytes = read_drop_bytes(&admission_dir.join("intent.json"))?;
            let result_bytes = read_drop_bytes(&admission_dir.join("result.json"))?;
            let ack_bytes = read_drop_bytes(&admission_dir.join("acknowledgement.json"))?;
            let spawn_bytes = read_drop_bytes(&admission_dir.join("spawn-identity.json"))?;

            let current_digests = AdmissionDropDigests {
                intent: intent_bytes.as_ref().map(|b| digest_bytes(b)),
                result: result_bytes.as_ref().map(|b| digest_bytes(b)),
                ack: ack_bytes.as_ref().map(|b| digest_bytes(b)),
                spawn_identity: spawn_bytes.as_ref().map(|b| digest_bytes(b)),
            };

            // Check if existing checkpoint authorizes a skip
            if let Some(existing) = checkpoint.entries.iter().find(|e| e.launch_id == launch_id)
                && recovery_skip_authorized(existing, &launch_id, &current_digests)
            {
                // Provisional findings are not exit proofs.
                validated_launch_ids.insert(launch_id.clone());
                continue;
            }

            // Otherwise classify with the same match arms as today's scan_admissions
            let intent = read_parent_loss_admission_intent(ledger, generation, &launch_id)
                .ok()
                .flatten();
            let service = intent.as_ref().and_then(|i| i.service);
            let result = read_parent_loss_admission_result(ledger, generation, &launch_id)
                .ok()
                .flatten();
            let acknowledgement =
                read_parent_loss_admission_acknowledgement_in(ledger, generation, &launch_id)
                    .ok()
                    .flatten();
            let spawn_identity =
                read_parent_loss_admission_spawn_identity_in(ledger, generation, &launch_id)
                    .ok()
                    .flatten();

            let (identity, finding) = match result {
                Some(result) => match result.state {
                    AdmissionResultState::Admitted
                    | AdmissionResultState::RejectedUnreaped { .. } => {
                        match result
                            .identity
                            .or_else(|| acknowledgement.map(|ack| ack.identity))
                            .or_else(|| spawn_identity.map(|s| s.identity))
                        {
                            Some(identity) => (Some(identity), None),
                            None => (None, Some(AdmissionFinding::Unidentified)),
                        }
                    }
                    AdmissionResultState::SpawnFailed { .. } => {
                        (None, Some(AdmissionFinding::SpawnFailed))
                    }
                    AdmissionResultState::RejectedAndReaped { .. } => {
                        (None, Some(AdmissionFinding::RejectedAndReaped))
                    }
                },
                None => match acknowledgement {
                    Some(acknowledgement) => (Some(acknowledgement.identity), None),
                    None => match spawn_identity {
                        Some(spawn_identity) => (Some(spawn_identity.identity), None),
                        None => {
                            if Instant::now() >= authority.deadline {
                                write_recovery_checkpoint(ledger, generation, &checkpoint)?;
                                return Ok(ClosureOutcome::Refused(
                                    BootstrapRecoveryReason::CoordinatorNotLive,
                                ));
                            }
                            #[cfg(target_os = "linux")]
                            {
                                match find_live_process_by_launch_env(
                                    ledger.canonical_root(),
                                    generation,
                                    &launch_id,
                                ) {
                                    Some(id) => (Some(id), None),
                                    None => (None, Some(AdmissionFinding::NeverAcknowledged)),
                                }
                            }
                            #[cfg(not(target_os = "linux"))]
                            (None, Some(AdmissionFinding::NeverAcknowledged))
                        }
                    },
                },
            };

            let Some(identity) = identity else {
                let finding = finding.unwrap_or(AdmissionFinding::Unidentified);
                let state = recovery_entry_state(&finding);
                let entry = RecoveryEntry {
                    launch_id: launch_id.clone(),
                    state,
                    service,
                    instance: None,
                    uid: None,
                    parent_launch_id: None,
                    finding,
                    intent_digest: current_digests.intent.clone(),
                    result_digest: current_digests.result.clone(),
                    ack_digest: current_digests.ack.clone(),
                    spawn_identity_digest: current_digests.spawn_identity.clone(),
                };
                upsert_recovery_entry(&mut checkpoint, entry);
                validated_launch_ids.insert(launch_id.clone());
                unflushed_count += 1;
                if unflushed_count >= RECOVERY_CHECKPOINT_BATCH {
                    write_recovery_checkpoint(ledger, generation, &checkpoint)?;
                    unflushed_count = 0;
                }
                continue;
            };

            let instance = identity.instance;
            let uid = Some(identity.uid);
            let parent_launch_id = identity.parent_launch_id.clone();

            match retire_admission_identity(
                ledger,
                generation,
                authority,
                &mut checkpoint,
                &launch_id,
                service,
                instance,
                uid,
                parent_launch_id,
                &current_digests,
            )? {
                RetirementOutcome::Completed => {
                    validated_launch_ids.insert(launch_id.clone());
                    unflushed_count += 1;
                    if unflushed_count >= RECOVERY_CHECKPOINT_BATCH {
                        write_recovery_checkpoint(ledger, generation, &checkpoint)?;
                        unflushed_count = 0;
                    }
                }
                RetirementOutcome::UnclassifiedSameLive => {
                    // Leave that other launch id unclassified (no completed row).
                    // Keep walking until the stored claim's launch id is serviced.
                    continue;
                }
                RetirementOutcome::Refused(outcome) => {
                    return Ok(outcome);
                }
            }
        }
    }

    // 6. Finalization
    // Flush any unflushed rows
    write_recovery_checkpoint(ledger, generation, &checkpoint)?;

    // If an escalation claim is still live, do not publish!
    if checkpoint.escalation.is_some() {
        return Ok(ClosureOutcome::Refused(
            BootstrapRecoveryReason::CoordinatorNotLive,
        ));
    }

    // A directory id with no validated row means do not publish.
    for dir_launch_id in &directory_launch_ids {
        if !validated_launch_ids.contains(dir_launch_id) {
            return Ok(ClosureOutcome::Refused(
                BootstrapRecoveryReason::CoordinatorNotLive,
            ));
        }
    }

    // Assemble AdmissionClosure and SealedAdmission from launch ids read from the
    // admission directory in this attempt, not from every checkpoint row.
    // A checkpoint row with no directory is not part of the account.
    let mut closures = Vec::new();
    let mut sealed = Vec::new();
    for launch_id in &directory_launch_ids {
        let Some(entry) = checkpoint
            .entries
            .iter()
            .find(|e| &e.launch_id == launch_id)
        else {
            return Ok(ClosureOutcome::Refused(
                BootstrapRecoveryReason::CoordinatorNotLive,
            ));
        };
        if let (Some(instance), Some(uid)) = (entry.instance, entry.uid) {
            sealed.push(SealedAdmission {
                launch_id: entry.launch_id.clone(),
                service: entry.service,
                identity: AdmissionIdentity {
                    generation,
                    launch_id: entry.launch_id.clone(),
                    instance,
                    uid,
                    parent_launch_id: entry.parent_launch_id.clone(),
                },
            });
        }
        closures.push(AdmissionClosure {
            launch_id: entry.launch_id.clone(),
            service: entry.service,
            instance: entry.instance,
            uid: entry.uid,
            finding: entry.finding,
        });
    }
    sealed.sort_by(|left, right| left.launch_id.cmp(&right.launch_id));
    closures.sort_by(|left, right| left.launch_id.cmp(&right.launch_id));

    let heartbeat = match (
        record
            .as_ref()
            .and_then(|record| record.supervisor_heartbeat.clone()),
        supervisor,
    ) {
        (Some(filename), Some(instance)) => clear_supervisor_heartbeat(ledger, &filename, instance),
        _ => HeartbeatRetirement::NotRecorded,
    };

    let ledger_path = ledger.sealed_ledger_path(generation);
    let sealed_ledger_digest = match fs::read(&ledger_path) {
        Ok(bytes) => digest_bytes(&bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            ledger.write_sealed_ledger(generation, &sealed)?
        }
        Err(error) => return Err(error.into()),
    };

    let mut record = record.unwrap_or_else(|| ParentLossGenerationRecord {
        schema: PARENT_LOSS_LEDGER_SCHEMA_V1,
        generation,
        coordinator,
        supervisor: supervisor.unwrap_or(authority.closed_by),
        sealed_ledger_digest: None,
        terminal: None,
        supervisor_heartbeat: None,
        closure: None,
    });
    if supervisor.is_none() {
        notes.push("no supervisor identity was recorded; the record names the closer".to_owned());
    }
    if record.terminal.is_none() {
        record.terminal = Some(ParentLossTerminalDisposition::Unresolved {
            reason: ParentLossUnresolvedReason::AuthoritiesLost,
        });
    }
    if record.sealed_ledger_digest.is_none() {
        record.sealed_ledger_digest = Some(sealed_ledger_digest);
    }
    let closure = AbandonedGenerationClosure {
        schema: PARENT_LOSS_CLOSURE_SCHEMA_V1,
        closed_by: authority.closed_by,
        successor_generation,
        closed_at: unix_seconds(),
        supervisor: supervisor_observation,
        coordinator: coordinator_observation,
        heartbeat,
        set_aside,
        notes,
        admissions: closures,
    };
    record.closure = Some(closure);
    let record_path = ledger.record_path(generation);
    fs::create_dir_all(record_path.parent().expect("generation record parent"))?;
    write_json(&record_path, &record, json_options())?;
    Ok(ClosureOutcome::Closed)
}

#[cfg(target_os = "linux")]
fn find_live_process_by_launch_env(
    journal: &std::path::Path,
    generation: ParentLossGeneration,
    launch_id: &str,
) -> Option<AdmissionIdentity> {
    use std::os::unix::fs::MetadataExt;
    let own_uid = nix::unistd::getuid().as_raw();
    let proc_dir = fs::read_dir("/proc").ok()?;
    for entry in proc_dir.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Ok(pid) = name_str.parse::<u32>() else {
            continue;
        };
        if pid == 0 {
            continue;
        }
        let proc_path = entry.path();
        let Ok(meta) = fs::metadata(&proc_path) else {
            continue;
        };
        if meta.uid() != own_uid {
            continue;
        }
        let environ_path = proc_path.join("environ");
        let Ok(environ_bytes) = fs::read(&environ_path) else {
            continue;
        };
        let mut has_journal = false;
        let mut has_launch_id = false;
        for var in environ_bytes.split(|&b| b == 0) {
            if var.is_empty() {
                continue;
            }
            if let Ok(var_str) = std::str::from_utf8(var)
                && let Some((k, v)) = var_str.split_once('=')
            {
                if k == "SOLSTONE_JOURNAL" && std::path::Path::new(v) == journal {
                    has_journal = true;
                } else if k == "SOL_PARENT_LOSS_LAUNCH_ID" && v == launch_id {
                    has_launch_id = true;
                }
            }
        }
        if has_journal && has_launch_id {
            let source = SystemProcessInstanceSource;
            if let InspectResult::Present { instance, uid, .. } = source.inspect(pid) {
                return Some(AdmissionIdentity {
                    generation,
                    launch_id: launch_id.to_string(),
                    instance,
                    uid,
                    parent_launch_id: None,
                });
            }
        }
    }
    None
}

/// What a pid `inspect` cannot read may still be settled by: the process
/// table's owner for that pid. Gone is exited; another user's is a reused
/// pid; our own uid or an instrument failure settles nothing.
fn settle_unverifiable(
    authority: &ClosingAuthority<'_>,
    pid: u32,
    recorded_uid: Option<u32>,
) -> Option<AdmissionFinding> {
    match authority.retirer.owner(pid) {
        ProcessOwner::Absent => Some(AdmissionFinding::Exited),
        ProcessOwner::Uid(uid) if recorded_uid.is_some_and(|recorded| recorded != uid) => {
            Some(AdmissionFinding::PidReusedByAnotherUser)
        }
        ProcessOwner::Uid(_) | ProcessOwner::Unknown => None,
    }
}

/// Re-observe a transient `Unverifiable` for a bounded window, so a pid that
/// exits between two procfs reads is not recorded as unobservable.
fn observe_settled(
    source: &dyn ProcessInstanceSource,
    instance: &ProcessInstance,
    deadline: Instant,
) -> InstanceVerdict {
    let settle = (Instant::now() + OBSERVATION_SETTLE_WINDOW).min(deadline);
    loop {
        let verdict = source.observe(instance);
        if verdict != InstanceVerdict::Unverifiable || Instant::now() >= settle {
            return verdict;
        }
        thread::sleep(OBSERVATION_POLL);
    }
}

pub(crate) fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use tempfile::TempDir;

    use super::*;
    use crate::lifecycle::parent_loss_admission::{
        AdmissionIdentity, AdmissionIntent, AdmissionResult, acknowledge_parent_loss_admission,
        write_parent_loss_admission_intent, write_parent_loss_admission_result,
    };
    use crate::lifecycle::parent_loss_ledger::{
        ActiveGeneration, CoordinatorLease, ParentLossReaderOutcome, read_parent_loss_outcome,
    };
    use crate::lifecycle::{HEARTBEAT_SCHEMA_V2, HeartbeatV2, RunId, WriterId};
    use crate::process::{CensusRow, ExecutionState, InspectResult, InstanceCensus, ProcessBirth};

    fn instance(pid: u32, birth: u64) -> ProcessInstance {
        ProcessInstance {
            pid,
            birth: ProcessBirth::linux(birth, 1, 100),
        }
    }

    /// A process table under test control: a pid is live with one exact
    /// identity, unverifiable, or absent.
    #[derive(Default)]
    struct FakeTable {
        live: HashMap<u32, (ProcessInstance, u32)>,
        unverifiable: HashSet<u32>,
        incomplete_census: bool,
        /// What the process table says owns a pid `inspect` cannot read.
        owners: HashMap<u32, ProcessOwner>,
        observed: Vec<ProcessInstance>,
        observed_pids: Vec<u32>,
    }

    #[derive(Clone, Default)]
    struct FakeSource(Arc<Mutex<FakeTable>>);

    impl ProcessInstanceSource for FakeSource {
        fn inspect(&self, pid: u32) -> InspectResult {
            let mut table = self.0.lock().expect("fake table");
            table.observed_pids.push(pid);
            let live_inst = table.live.get(&pid).map(|(i, _)| *i);
            if let Some(instance) = live_inst {
                table.observed.push(instance);
            }
            if table.unverifiable.contains(&pid) {
                return InspectResult::Unverifiable;
            }
            match table.live.get(&pid) {
                Some((instance, uid)) => InspectResult::Present {
                    instance: *instance,
                    uid: *uid,
                    execution: ExecutionState::Running,
                    ppid: Some(1),
                    pgid: Some(1),
                },
                None => InspectResult::Absent,
            }
        }

        fn census(&self) -> InstanceCensus {
            let table = self.0.lock().expect("fake table");
            let rows = table
                .live
                .values()
                .map(|(instance, uid)| CensusRow {
                    instance: *instance,
                    uid: *uid,
                    ppid: 1,
                    pgid: 1,
                    execution: ExecutionState::Running,
                })
                .collect();
            if table.incomplete_census {
                InstanceCensus::Incomplete(rows)
            } else {
                InstanceCensus::Complete(rows)
            }
        }
    }

    /// Records every signal; a process exits on `Terminate` only when it is
    /// in `obeys_terminate`, and on `Kill` unless it is `immortal`.
    struct FakeRetirer {
        table: Arc<Mutex<FakeTable>>,
        signals: Mutex<Vec<(u32, &'static str)>>,
        obeys_terminate: HashSet<u32>,
        immortal: HashSet<u32>,
    }

    impl AdmissionRetirer for FakeRetirer {
        fn signal(
            &self,
            instance: ProcessInstance,
            signal: SignalKind,
        ) -> Result<(), TerminationError> {
            let name = match signal {
                SignalKind::Terminate => "term",
                SignalKind::Kill => "kill",
            };
            self.signals
                .lock()
                .expect("signals")
                .push((instance.pid, name));
            let exits = match signal {
                SignalKind::Terminate => self.obeys_terminate.contains(&instance.pid),
                SignalKind::Kill => !self.immortal.contains(&instance.pid),
            };
            if exits {
                self.table
                    .lock()
                    .expect("fake table")
                    .live
                    .remove(&instance.pid);
            }
            Ok(())
        }

        fn owner(&self, pid: u32) -> ProcessOwner {
            let table = self.table.lock().expect("fake table");
            if let Some(owner) = table.owners.get(&pid) {
                return *owner;
            }
            match table.live.get(&pid) {
                Some((_, uid)) => ProcessOwner::Uid(*uid),
                None => ProcessOwner::Absent,
            }
        }
    }

    fn fake(
        live: &[(ProcessInstance, u32)],
        obeys_terminate: &[u32],
        immortal: &[u32],
    ) -> (FakeSource, FakeRetirer) {
        let table = Arc::new(Mutex::new(FakeTable {
            live: live.iter().map(|(i, uid)| (i.pid, (*i, *uid))).collect(),
            unverifiable: HashSet::new(),
            incomplete_census: false,
            owners: HashMap::new(),
            observed: Vec::new(),
            observed_pids: Vec::new(),
        }));
        let retirer = FakeRetirer {
            table: Arc::clone(&table),
            signals: Mutex::new(Vec::new()),
            obeys_terminate: obeys_terminate.iter().copied().collect(),
            immortal: immortal.iter().copied().collect(),
        };
        (FakeSource(table), retirer)
    }

    /// An open, admitting generation whose supervisor and coordinator are
    /// synthetic identities nothing on this host can match.
    fn open_generation(directory: &TempDir) -> (ParentLossLedger, ActiveGeneration) {
        let ledger = ParentLossLedger::open(directory.path()).expect("ledger");
        let active = ledger
            .reserve_generation(instance(10, 1), [HostedServiceKind::Sense])
            .expect("reserve generation");
        ledger.initialize_record(&active).expect("record");
        let coordinator = instance(20, 2);
        let active = ledger
            .persist_coordinator_identity(active.generation, coordinator)
            .expect("coordinator identity");
        let active = ledger
            .mark_admitting(active.generation, coordinator)
            .expect("admitting state");
        (ledger, active)
    }

    fn admit(
        journal: &Path,
        generation: ParentLossGeneration,
        launch_id: &str,
        service: Option<HostedServiceKind>,
        identity: Option<ProcessInstance>,
    ) -> Option<AdmissionIdentity> {
        write_parent_loss_admission_intent(
            journal,
            &AdmissionIntent::new(generation, launch_id, service, None),
        )
        .expect("intent");
        let identity = identity.map(|instance| AdmissionIdentity {
            generation,
            launch_id: launch_id.to_owned(),
            instance,
            uid: 501,
            parent_launch_id: None,
        });
        if let Some(identity) = identity.as_ref() {
            acknowledge_parent_loss_admission(journal, identity.clone()).expect("ack");
            write_parent_loss_admission_result(
                journal,
                generation,
                launch_id,
                &AdmissionResult {
                    schema: 1,
                    identity: Some(identity.clone()),
                    state: AdmissionResultState::Admitted,
                },
            )
            .expect("result");
        }
        identity
    }

    fn lease(ledger: &ParentLossLedger) -> CoordinatorLease {
        ledger
            .acquire_coordinator_lease()
            .expect("lease acquisition")
            .expect("lease free")
    }

    fn authority<'a>(
        source: &'a dyn ProcessInstanceSource,
        retirer: &'a dyn AdmissionRetirer,
        budget: Duration,
    ) -> ClosingAuthority<'a> {
        ClosingAuthority {
            closed_by: instance(30, 3),
            source,
            retirer,
            deadline: Instant::now() + budget,
        }
    }

    fn closure_of(
        ledger: &ParentLossLedger,
        generation: ParentLossGeneration,
    ) -> AbandonedGenerationClosure {
        ledger
            .record(generation)
            .expect("record readable")
            .expect("record exists")
            .closure
            .expect("closure recorded")
    }

    #[test]
    fn an_open_generation_whose_authorities_are_gone_is_closed_and_the_successor_boots() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let exited = instance(4242, 42);
        admit(
            directory.path(),
            active.generation,
            "sense-a",
            Some(HostedServiceKind::Sense),
            Some(exited),
        );
        let (source, retirer) = fake(&[], &[], &[]);
        let lease = lease(&ledger);

        let successor = ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("an abandoned generation must not brick the journal");

        assert_eq!(successor.generation, active.generation + 1);
        let record = ledger
            .record(active.generation)
            .expect("record")
            .expect("record kept");
        assert_eq!(
            record.terminal,
            Some(ParentLossTerminalDisposition::Unresolved {
                reason: ParentLossUnresolvedReason::AuthoritiesLost,
            })
        );
        let closure = record.closure.expect("closure recorded");
        assert_eq!(closure.successor_generation, successor.generation);
        assert_eq!(closure.supervisor, AuthorityObservation::Exited);
        assert_eq!(closure.coordinator, AuthorityObservation::Exited);
        assert_eq!(closure.admissions.len(), 1);
        assert_eq!(closure.admissions[0].finding, AdmissionFinding::Exited);
        assert!(retirer.signals.lock().expect("signals").is_empty());
        assert!(
            ledger.sealed_ledger_path(active.generation).is_file(),
            "the closed generation carries a sealed ledger"
        );
        // The successor now reads as the ordinary reservation seam.
        assert!(matches!(
            read_parent_loss_outcome(directory.path()).expect("outcome"),
            ParentLossReaderOutcome::BootstrapRecoveryRequired { .. }
        ));
    }

    /// 🔒 The lock is the proof and the observation is the veto: a coordinator
    /// that is genuinely live -- this test process -- refuses the closer even
    /// though the test holds the lease.
    #[test]
    fn a_live_coordinator_still_refuses_the_closer() {
        let directory = TempDir::new().expect("temporary root");
        let ledger = ParentLossLedger::open(directory.path()).expect("ledger");
        let active = ledger
            .reserve_generation(instance(10, 1), [])
            .expect("reserve generation");
        ledger.initialize_record(&active).expect("record");
        let live = match SystemProcessInstanceSource.inspect(std::process::id()) {
            InspectResult::Present { instance, .. } => instance,
            _ => panic!("this test process must be observable"),
        };
        ledger
            .persist_coordinator_identity(active.generation, live)
            .expect("coordinator identity");
        let lease = lease(&ledger);

        let result = ledger.reserve_generation_closing_abandoned(
            instance(11, 3),
            [],
            &lease,
            &authority(
                &SystemProcessInstanceSource,
                &SystemAdmissionRetirer,
                Duration::from_secs(2),
            ),
        );

        assert!(matches!(
            result,
            Err(ParentLossLedgerError::RecoveryRequired(
                BootstrapRecoveryReason::ActiveCoordinator
            ))
        ));
        assert!(
            ledger
                .record(active.generation)
                .expect("record")
                .expect("record")
                .closure
                .is_none(),
            "a refused closure writes nothing"
        );
    }

    #[test]
    fn a_live_supervisor_still_refuses_the_closer() {
        let directory = TempDir::new().expect("temporary root");
        let ledger = ParentLossLedger::open(directory.path()).expect("ledger");
        let live = match SystemProcessInstanceSource.inspect(std::process::id()) {
            InspectResult::Present { instance, .. } => instance,
            _ => panic!("this test process must be observable"),
        };
        let active = ledger
            .reserve_generation(live, [])
            .expect("reserve generation");
        ledger.initialize_record(&active).expect("record");
        ledger
            .persist_coordinator_identity(active.generation, instance(20, 2))
            .expect("coordinator identity");
        let lease = lease(&ledger);

        let result = ledger.reserve_generation_closing_abandoned(
            instance(11, 3),
            [],
            &lease,
            &authority(
                &SystemProcessInstanceSource,
                &SystemAdmissionRetirer,
                Duration::from_secs(2),
            ),
        );

        assert!(matches!(
            result,
            Err(ParentLossLedgerError::RecoveryRequired(
                BootstrapRecoveryReason::SupervisorLive
            ))
        ));
    }

    #[cfg(all(test, feature = "full-tests"))]
    /// 🔒 Safety over convergence: a process that survives SIGKILL within the
    /// deadline keeps this start refused, the generation open, and nothing
    /// written. The next start retries.
    #[test]
    fn an_admission_that_outlives_the_deadline_refuses_this_start_and_leaves_the_generation_open() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let immortal = instance(5151, 51);
        admit(
            directory.path(),
            active.generation,
            "sense-immortal",
            Some(HostedServiceKind::Sense),
            Some(immortal),
        );
        let (source, retirer) = fake(&[(immortal, 501)], &[], &[immortal.pid]);
        let lease = lease(&ledger);

        let result = ledger.reserve_generation_closing_abandoned(
            instance(11, 3),
            [],
            &lease,
            &authority(&source, &retirer, Duration::from_millis(400)),
        );

        assert!(matches!(
            result,
            Err(ParentLossLedgerError::RecoveryRequired(
                BootstrapRecoveryReason::AbandonedAdmissionLive
            ))
        ));
        let signals = retirer.signals.lock().expect("signals").clone();
        assert_eq!(
            signals,
            vec![(immortal.pid, "term"), (immortal.pid, "kill")]
        );
        let record = ledger
            .record(active.generation)
            .expect("record")
            .expect("record");
        assert!(record.terminal.is_none(), "the generation stays open");
        assert!(record.closure.is_none());
        assert_eq!(
            ledger
                .active_generation()
                .expect("pointer")
                .expect("pointer kept")
                .generation,
            active.generation
        );
    }

    #[test]
    fn a_child_that_obeys_sigterm_is_not_escalated_and_the_signals_are_recorded() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let polite = instance(6161, 61);
        admit(
            directory.path(),
            active.generation,
            "spl-polite",
            Some(HostedServiceKind::Spl),
            Some(polite),
        );
        let (source, retirer) = fake(&[(polite, 501)], &[polite.pid], &[]);
        let lease = lease(&ledger);

        ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("closed");

        assert_eq!(
            retirer.signals.lock().expect("signals").clone(),
            vec![(polite.pid, "term")]
        );
        assert_eq!(
            closure_of(&ledger, active.generation).admissions[0].finding,
            AdmissionFinding::Retired { escalated: false }
        );
    }

    /// 🔒 A complete descendant observation is part of the proof, not a
    /// best-effort prelude to retiring the recorded root. Otherwise an escaped
    /// child can retain a port after the root exits and the successor boots.
    #[test]
    fn incomplete_descendant_coverage_refuses_without_retiring_the_root() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let live = instance(6262, 62);
        admit(
            directory.path(),
            active.generation,
            "spl-incomplete-tree",
            Some(HostedServiceKind::Spl),
            Some(live),
        );
        let (source, retirer) = fake(&[(live, 501)], &[live.pid], &[]);
        source.0.lock().expect("fake table").incomplete_census = true;
        let lease = lease(&ledger);

        let result = ledger.reserve_generation_closing_abandoned(
            instance(11, 3),
            [],
            &lease,
            &authority(&source, &retirer, Duration::from_millis(400)),
        );

        assert!(matches!(
            result,
            Err(ParentLossLedgerError::RecoveryRequired(
                BootstrapRecoveryReason::AbandonedAdmissionUnverifiable
            ))
        ));
        assert!(retirer.signals.lock().expect("signals").is_empty());
        let record = ledger
            .record(active.generation)
            .expect("record")
            .expect("record");
        assert!(record.terminal.is_none(), "the generation stays open");
        assert!(record.closure.is_none());
    }

    #[cfg(all(test, feature = "full-tests"))]
    /// 🔒 A pid `inspect` cannot read is settled only by a positive owner
    /// reading: another user's pid is a reused pid; our own uid, or no
    /// answer, refuses this start rather than reading "cannot tell" as death.
    #[test]
    fn an_uninspectable_pid_is_settled_by_its_owner_or_refuses() {
        for (owner, expected) in [
            (
                ProcessOwner::Uid(0),
                Ok(AdmissionFinding::PidReusedByAnotherUser),
            ),
            (ProcessOwner::Absent, Ok(AdmissionFinding::Exited)),
            (
                ProcessOwner::Uid(501),
                Err(BootstrapRecoveryReason::AbandonedAdmissionUnverifiable),
            ),
            (
                ProcessOwner::Unknown,
                Err(BootstrapRecoveryReason::AbandonedAdmissionUnverifiable),
            ),
        ] {
            let directory = TempDir::new().expect("temporary root");
            let (ledger, active) = open_generation(&directory);
            let foreign = instance(7171, 71);
            admit(
                directory.path(),
                active.generation,
                "sense-foreign",
                Some(HostedServiceKind::Sense),
                Some(foreign),
            );
            let (source, retirer) = fake(&[], &[], &[]);
            {
                let mut table = source.0.lock().expect("fake table");
                table.unverifiable.insert(foreign.pid);
                table.owners.insert(foreign.pid, owner);
            }
            let lease = lease(&ledger);

            let result = ledger.reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_millis(600)),
            );

            assert!(retirer.signals.lock().expect("signals").is_empty());
            match expected {
                Ok(finding) => {
                    result.expect("settled by a positive owner reading");
                    assert_eq!(
                        closure_of(&ledger, active.generation).admissions[0].finding,
                        finding,
                        "{owner:?}"
                    );
                }
                Err(reason) => {
                    assert!(
                        matches!(result, Err(ParentLossLedgerError::RecoveryRequired(r)) if r == reason),
                        "{owner:?} must refuse"
                    );
                    assert!(
                        ledger
                            .record(active.generation)
                            .expect("record")
                            .expect("record")
                            .closure
                            .is_none()
                    );
                }
            }
        }
    }

    /// The seal lands before the scan, so an acknowledgement written after the
    /// scan meets a sealed pointer.
    #[test]
    fn the_pointer_is_sealed_before_the_admissions_are_scanned() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let (source, retirer) = fake(&[], &[], &[]);
        let lease = lease(&ledger);
        let closing = ClosingAuthority {
            closed_by: instance(30, 3),
            source: &source,
            retirer: &retirer,
            deadline: Instant::now() + Duration::from_secs(2),
        };
        let abandoned = AbandonedGeneration {
            generation: active.generation,
            supervisor: Some(active.supervisor),
            coordinator: active.coordinator,
            record: ledger.record(active.generation).expect("record"),
            set_aside: Vec::new(),
            notes: Vec::new(),
        };
        let _ = lease;
        assert!(matches!(
            close_abandoned_generation(&ledger, abandoned, active.generation + 1, &closing)
                .expect("closed"),
            ClosureOutcome::Closed
        ));
        assert_eq!(
            ledger
                .active_generation()
                .expect("pointer")
                .expect("pointer")
                .phase,
            crate::lifecycle::ParentLossPhase::Sealed
        );
    }

    #[test]
    fn an_intent_without_acknowledgement_is_recorded_never_acknowledged() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        admit(
            directory.path(),
            active.generation,
            "sense-unacked",
            Some(HostedServiceKind::Sense),
            None,
        );
        let (source, retirer) = fake(&[], &[], &[]);
        let lease = lease(&ledger);

        ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("closed");

        let closure = closure_of(&ledger, active.generation);
        assert_eq!(
            closure.admissions[0].finding,
            AdmissionFinding::NeverAcknowledged
        );
        assert!(closure.admissions[0].instance.is_none());
    }

    #[test]
    fn a_missing_record_is_created_and_closed() {
        let directory = TempDir::new().expect("temporary root");
        let ledger = ParentLossLedger::open(directory.path()).expect("ledger");
        let active = ledger
            .reserve_generation(instance(10, 1), [])
            .expect("reservation without a record");
        let (source, retirer) = fake(&[], &[], &[]);
        let lease = lease(&ledger);

        let successor = ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("a reservation that died before its record must not brick");

        assert_eq!(successor.generation, active.generation + 1);
        let closure = closure_of(&ledger, active.generation);
        assert_eq!(closure.coordinator, AuthorityObservation::NeverRecorded);
        assert_eq!(closure.supervisor, AuthorityObservation::Exited);
    }

    #[test]
    fn a_malformed_record_is_set_aside_and_closed() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let live = instance(8181, 81);
        admit(
            directory.path(),
            active.generation,
            "sense-live",
            Some(HostedServiceKind::Sense),
            Some(live),
        );
        std::fs::write(ledger.record_path(active.generation), b"{ not json")
            .expect("corrupt the record");
        let (source, retirer) = fake(&[(live, 501)], &[live.pid], &[]);
        let lease = lease(&ledger);

        ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("a malformed record must not brick");

        let closure = closure_of(&ledger, active.generation);
        assert_eq!(closure.set_aside.len(), 1);
        assert!(closure.set_aside[0].starts_with("record.wedged-"));
        assert!(
            ledger
                .generation_path(active.generation)
                .join(&closure.set_aside[0])
                .is_file(),
            "the damaged record is preserved beside the new one"
        );
        assert_eq!(
            closure.admissions[0].finding,
            AdmissionFinding::Retired { escalated: false },
            "the admissions directory outlives the record and still retires"
        );
    }

    #[test]
    fn a_set_aside_pointer_closes_the_open_generation_on_disk() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let live = instance(9191, 91);
        admit(
            directory.path(),
            active.generation,
            "cortex-live",
            Some(HostedServiceKind::Cortex),
            Some(live),
        );
        std::fs::write(ledger.active_path(), b"{ this is not json").expect("corrupt the pointer");
        let (source, retirer) = fake(&[(live, 501)], &[live.pid], &[]);
        let lease = lease(&ledger);

        let successor = ledger
            .reserve_generation_closing_abandoned(
                instance(12, 4),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("a corrupt pointer must not brick");

        assert!(successor.generation > active.generation);
        let closure = closure_of(&ledger, active.generation);
        assert_eq!(
            closure.admissions[0].finding,
            AdmissionFinding::Retired { escalated: false },
            "the generation the pointer named is still retired from disk"
        );
    }

    #[test]
    fn the_dead_supervisors_heartbeat_is_retired_with_the_generation() {
        let directory = TempDir::new().expect("temporary root");
        let ledger = ParentLossLedger::open(directory.path()).expect("ledger");
        let supervisor = instance(10, 1);
        let active = ledger
            .reserve_generation(supervisor, [])
            .expect("reserve generation");
        let writer = WriterId::parse("0123456789abcdef0123456789abcdef").expect("writer id");
        let run = RunId::generate().expect("run id");
        let filename = crate::lifecycle::v2_heartbeat_filename(&writer, &run);
        ledger
            .initialize_record_with_heartbeat(&active, Some(filename.clone()))
            .expect("record");
        let coordinator = instance(20, 2);
        ledger
            .persist_coordinator_identity(active.generation, coordinator)
            .expect("coordinator identity");
        let sync = directory.path().join("health/sync");
        std::fs::create_dir_all(&sync).expect("sync directory");
        let heartbeat = HeartbeatV2::new(
            writer,
            run,
            "host".to_owned(),
            supervisor.pid,
            "1.0".to_owned(),
            "test".to_owned(),
            15,
            directory.path().display().to_string(),
        );
        assert_eq!(heartbeat.schema, HEARTBEAT_SCHEMA_V2);
        std::fs::write(
            sync.join(&filename),
            serde_json::to_vec(&heartbeat).expect("heartbeat json"),
        )
        .expect("heartbeat file");
        let (source, retirer) = fake(&[], &[], &[]);
        let lease = lease(&ledger);

        ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("closed");

        assert_eq!(
            closure_of(&ledger, active.generation).heartbeat,
            HeartbeatRetirement::Removed
        );
        assert!(
            !sync.join(&filename).exists(),
            "the dead run's heartbeat is gone"
        );
    }

    /// A record written before the heartbeat field existed says so, and the
    /// stale-heartbeat collector keeps its job.
    #[test]
    fn a_record_without_a_heartbeat_name_records_that_nothing_was_retired() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let (source, retirer) = fake(&[], &[], &[]);
        let lease = lease(&ledger);
        ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("closed");
        assert_eq!(
            closure_of(&ledger, active.generation).heartbeat,
            HeartbeatRetirement::NotRecorded
        );
    }

    /// A sealed `unresolved` generation used to be advanced past with nothing
    /// retired. It is still advanced past, and now its admissions are retired
    /// and its record carries the closure; the terminal disposition is kept.
    #[test]
    fn a_sealed_unresolved_generation_is_advanced_past_with_its_admissions_retired() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let coordinator = active.coordinator.expect("coordinator");
        let lingering = instance(3131, 31);
        admit(
            directory.path(),
            active.generation,
            "sense-lingering",
            Some(HostedServiceKind::Sense),
            Some(lingering),
        );
        ledger.seal(active.generation, coordinator).expect("seal");
        let digest = ledger
            .write_sealed_ledger(active.generation, &Vec::<String>::new())
            .expect("sealed ledger");
        ledger
            .write_terminal_with_digest(
                active.generation,
                coordinator,
                ParentLossTerminalDisposition::Unresolved {
                    reason: ParentLossUnresolvedReason::RetirementDeadlineExceeded {
                        deadline_seconds: 15,
                    },
                },
                Some(digest),
            )
            .expect("terminal");
        let (source, retirer) = fake(&[(lingering, 501)], &[lingering.pid], &[]);
        let lease = lease(&ledger);

        let successor = ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("advanced");

        assert_eq!(successor.generation, active.generation + 1);
        let record = ledger
            .record(active.generation)
            .expect("record")
            .expect("record");
        assert!(matches!(
            record.terminal,
            Some(ParentLossTerminalDisposition::Unresolved {
                reason: ParentLossUnresolvedReason::RetirementDeadlineExceeded { .. }
            })
        ));
        let closure = record.closure.expect("closure");
        assert_eq!(
            closure.admissions[0].finding,
            AdmissionFinding::Retired { escalated: false }
        );
        assert!(
            closure
                .notes
                .iter()
                .any(|note| note.contains("sealed unresolved"))
        );
    }

    #[test]
    fn a_completed_predecessor_is_not_touched_by_the_closer() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let coordinator = active.coordinator.expect("coordinator");
        ledger.seal(active.generation, coordinator).expect("seal");
        let digest = ledger
            .write_sealed_ledger(active.generation, &Vec::<String>::new())
            .expect("sealed ledger");
        ledger
            .write_terminal_with_digest(
                active.generation,
                coordinator,
                ParentLossTerminalDisposition::CancelledBeforeAdmission,
                Some(digest),
            )
            .expect("terminal");
        let (source, retirer) = fake(&[], &[], &[]);
        let lease = lease(&ledger);

        ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("advanced");

        assert!(
            ledger
                .record(active.generation)
                .expect("record")
                .expect("record")
                .closure
                .is_none()
        );
    }

    #[cfg(all(test, feature = "full-tests"))]
    /// The lease is a real flock: a second holder cannot take it while the
    /// first is alive, and the ledger reports that as contention, not error.
    #[test]
    fn the_coordinator_lease_is_exclusive_while_held() {
        let directory = TempDir::new().expect("temporary root");
        let ledger = ParentLossLedger::open(directory.path()).expect("ledger");
        let held = lease(&ledger);
        assert!(
            ledger
                .acquire_coordinator_lease()
                .expect("acquisition")
                .is_none()
        );
        drop(held);
        assert!(
            ledger
                .acquire_coordinator_lease()
                .expect("acquisition")
                .is_some()
        );
    }

    #[test]
    fn recovery_checkpoint_missing_launch_id_not_completed() {
        let digests = AdmissionDropDigests {
            intent: Some("hash1".to_string()),
            ..Default::default()
        };
        let mut entry = RecoveryEntry {
            launch_id: "".to_string(),
            state: RecoveryEntryState::Completed,
            service: None,
            instance: Some(instance(1, 1)),
            uid: None,
            parent_launch_id: None,
            finding: AdmissionFinding::Exited,
            intent_digest: Some("hash1".to_string()),
            result_digest: None,
            ack_digest: None,
            spawn_identity_digest: None,
        };
        assert!(!recovery_skip_authorized(&entry, "", &digests));
        assert!(!recovery_skip_authorized(&entry, "launch-1", &digests));
        entry.launch_id = "other-launch".to_string();
        assert!(!recovery_skip_authorized(&entry, "launch-1", &digests));
    }

    #[test]
    fn recovery_checkpoint_rebound_digests_invalidates_cached_finding() {
        let entry = RecoveryEntry {
            launch_id: "launch-1".to_string(),
            state: RecoveryEntryState::Completed,
            service: None,
            instance: Some(instance(1, 1)),
            uid: None,
            parent_launch_id: None,
            finding: AdmissionFinding::Exited,
            intent_digest: Some("old_hash".to_string()),
            result_digest: None,
            ack_digest: None,
            spawn_identity_digest: None,
        };
        let rebound_digests = AdmissionDropDigests {
            intent: Some("new_hash".to_string()),
            ..Default::default()
        };
        assert!(!recovery_skip_authorized(
            &entry,
            "launch-1",
            &rebound_digests
        ));
    }

    #[test]
    fn recovery_checkpoint_all_absent_digests_cannot_complete_exited() {
        let entry = RecoveryEntry {
            launch_id: "launch-1".to_string(),
            state: RecoveryEntryState::Completed,
            service: None,
            instance: Some(instance(1, 1)),
            uid: None,
            parent_launch_id: None,
            finding: AdmissionFinding::Exited,
            intent_digest: None,
            result_digest: None,
            ack_digest: None,
            spawn_identity_digest: None,
        };
        let absent_digests = AdmissionDropDigests::default();
        assert!(absent_digests.is_all_absent());
        assert!(!recovery_skip_authorized(
            &entry,
            "launch-1",
            &absent_digests
        ));
    }

    #[test]
    fn recovery_checkpoint_live_or_unverifiable_cannot_be_completed() {
        assert_eq!(
            recovery_entry_state(&AdmissionFinding::NeverAcknowledged),
            RecoveryEntryState::Provisional
        );
        assert_eq!(
            recovery_entry_state(&AdmissionFinding::Unidentified),
            RecoveryEntryState::Provisional
        );

        let digests = AdmissionDropDigests {
            intent: Some("hash".to_string()),
            ..Default::default()
        };
        let provisional_entry = RecoveryEntry {
            launch_id: "launch-1".to_string(),
            state: RecoveryEntryState::Provisional,
            service: None,
            instance: None,
            uid: None,
            parent_launch_id: None,
            finding: AdmissionFinding::NeverAcknowledged,
            intent_digest: Some("hash".to_string()),
            result_digest: None,
            ack_digest: None,
            spawn_identity_digest: None,
        };
        assert!(!recovery_skip_authorized(
            &provisional_entry,
            "launch-1",
            &digests
        ));

        let completed_never_ack = RecoveryEntry {
            launch_id: "launch-1".to_string(),
            state: RecoveryEntryState::Completed,
            service: None,
            instance: None,
            uid: None,
            parent_launch_id: None,
            finding: AdmissionFinding::NeverAcknowledged,
            intent_digest: Some("hash".to_string()),
            result_digest: None,
            ack_digest: None,
            spawn_identity_digest: None,
        };
        assert!(!recovery_skip_authorized(
            &completed_never_ack,
            "launch-1",
            &digests
        ));
    }

    #[test]
    fn recovery_checkpoint_stored_escalation_deadline_not_moved_later() {
        let inst = instance(42, 1);
        let stored = RecoveryEscalation {
            launch_id: "launch-1".to_string(),
            service: None,
            instance: inst,
            uid: Some(1000),
            parent_launch_id: None,
            escalate_at_unix_ms: 10_000,
            descendant_escalated: false,
        };

        let resolved = resolve_escalation_deadline(
            Some(&stored),
            "launch-1",
            None,
            inst,
            Some(1000),
            None,
            false,
            50_000,
            Duration::from_secs(12),
        );
        assert_eq!(resolved.escalate_at_unix_ms, 10_000);
    }

    #[test]
    fn recovery_checkpoint_budget_refusal_is_non_advancing_reason() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);

        let inst = instance(101, 1);
        admit(
            directory.path(),
            active.generation,
            "launch-budget-refusal",
            Some(HostedServiceKind::Sense),
            Some(inst),
        );

        let (source, retirer) = fake(&[], &[], &[]);
        let lease = lease(&ledger);

        let expired_authority = ClosingAuthority {
            closed_by: instance(11, 3),
            source: &source,
            retirer: &retirer,
            deadline: Instant::now() - Duration::from_millis(50),
        };

        let err = ledger
            .reserve_generation_closing_abandoned(instance(11, 3), [], &lease, &expired_authority)
            .expect_err("budget expired refusal");

        match err {
            ParentLossLedgerError::RecoveryRequired(reason) => {
                assert_eq!(reason, BootstrapRecoveryReason::CoordinatorNotLive);
            }
            other => panic!("expected RecoveryRequired, got: {other:?}"),
        }

        let current_active = ledger.active_generation().expect("active").expect("active");
        assert_eq!(current_active.generation, active.generation);
        let record = ledger
            .record(active.generation)
            .expect("record")
            .expect("record");
        assert!(
            record.closure.is_none(),
            "no closure record should be written"
        );
        assert!(!matches!(
            record.terminal,
            Some(
                ParentLossTerminalDisposition::Completed { .. }
                    | ParentLossTerminalDisposition::RetiredExpected
                    | ParentLossTerminalDisposition::CancelledBeforeAdmission
            )
        ));
    }

    #[test]
    fn recovery_checkpoint_resumes_without_reobserving_valid_cached_rows() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);

        let inst1 = instance(101, 10);
        let inst2 = instance(102, 20);

        admit(
            directory.path(),
            active.generation,
            "launch-01",
            Some(HostedServiceKind::Sense),
            Some(inst1),
        );
        admit(
            directory.path(),
            active.generation,
            "launch-02",
            Some(HostedServiceKind::Sense),
            Some(inst2),
        );

        let admission_dir = admission_directory(&ledger, active.generation).join("launch-01");
        let intent_bytes = fs::read(admission_dir.join("intent.json")).ok();
        let result_bytes = fs::read(admission_dir.join("result.json")).ok();
        let ack_bytes = fs::read(admission_dir.join("acknowledgement.json")).ok();
        let spawn_bytes = fs::read(admission_dir.join("spawn-identity.json")).ok();

        let checkpoint = ParentLossClosureRecovery {
            schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
            generation: active.generation,
            sealed_phase: ParentLossPhase::Sealed,
            set_aside: Vec::new(),
            escalation: None,
            entries: vec![RecoveryEntry {
                launch_id: "launch-01".to_string(),
                state: RecoveryEntryState::Completed,
                service: Some(HostedServiceKind::Sense),
                instance: Some(inst1),
                uid: Some(501),
                parent_launch_id: None,
                finding: AdmissionFinding::Exited,
                intent_digest: intent_bytes.as_ref().map(|b| digest_bytes(b)),
                result_digest: result_bytes.as_ref().map(|b| digest_bytes(b)),
                ack_digest: ack_bytes.as_ref().map(|b| digest_bytes(b)),
                spawn_identity_digest: spawn_bytes.as_ref().map(|b| digest_bytes(b)),
            }],
        };
        write_recovery_checkpoint(&ledger, active.generation, &checkpoint)
            .expect("checkpoint write");

        let (source, retirer) = fake(&[(inst2, 501)], &[inst2.pid], &[]);
        let lease = lease(&ledger);
        let closer_2 = instance(777, 88);
        let auth = ClosingAuthority {
            closed_by: closer_2,
            source: &source,
            retirer: &retirer,
            deadline: Instant::now() + Duration::from_secs(2),
        };

        let successor = ledger
            .reserve_generation_closing_abandoned(closer_2, [], &lease, &auth)
            .expect("closed");

        assert_eq!(successor.generation, active.generation + 1);

        let table = source.0.lock().expect("table");
        assert!(
            !table.observed.contains(&inst1),
            "inst1 was skipped and not observed"
        );
        assert!(table.observed.contains(&inst2), "inst2 was observed");

        let record = ledger
            .record(active.generation)
            .expect("record")
            .expect("record");
        let closure = record.closure.expect("closure");
        assert_eq!(closure.closed_by, closer_2);
        assert_eq!(closure.admissions.len(), 2);
        assert_eq!(closure.admissions[0].launch_id, "launch-01");
        assert_eq!(closure.admissions[0].finding, AdmissionFinding::Exited);
        assert_eq!(closure.admissions[1].launch_id, "launch-02");
        assert_eq!(
            closure.admissions[1].finding,
            AdmissionFinding::Retired { escalated: false }
        );

        assert!(admission_dir.join("intent.json").exists());
        assert!(admission_dir.join("result.json").exists());
    }

    #[test]
    fn kill_at_checkpoint_seam_and_resume_with_new_closer() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);

        let inst1 = instance(501, 1);
        let inst2 = instance(502, 2);

        admit(
            directory.path(),
            active.generation,
            "launch-01",
            Some(HostedServiceKind::Sense),
            Some(inst1),
        );
        admit(
            directory.path(),
            active.generation,
            "launch-02",
            Some(HostedServiceKind::Sense),
            Some(inst2),
        );

        let admission_dir = admission_directory(&ledger, active.generation).join("launch-01");
        let intent_bytes = fs::read(admission_dir.join("intent.json")).ok();
        let result_bytes = fs::read(admission_dir.join("result.json")).ok();
        let ack_bytes = fs::read(admission_dir.join("acknowledgement.json")).ok();
        let spawn_bytes = fs::read(admission_dir.join("spawn-identity.json")).ok();

        let checkpoint = ParentLossClosureRecovery {
            schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
            generation: active.generation,
            sealed_phase: ParentLossPhase::Sealed,
            set_aside: Vec::new(),
            escalation: None,
            entries: vec![RecoveryEntry {
                launch_id: "launch-01".to_string(),
                state: RecoveryEntryState::Completed,
                service: Some(HostedServiceKind::Sense),
                instance: Some(inst1),
                uid: Some(501),
                parent_launch_id: None,
                finding: AdmissionFinding::Exited,
                intent_digest: intent_bytes.as_ref().map(|b| digest_bytes(b)),
                result_digest: result_bytes.as_ref().map(|b| digest_bytes(b)),
                ack_digest: ack_bytes.as_ref().map(|b| digest_bytes(b)),
                spawn_identity_digest: spawn_bytes.as_ref().map(|b| digest_bytes(b)),
            }],
        };
        write_recovery_checkpoint(&ledger, active.generation, &checkpoint)
            .expect("checkpoint write");

        let (source, retirer) = fake(&[(inst2, 501)], &[inst2.pid], &[]);
        let lease = lease(&ledger);
        let closer_2 = instance(999, 9);
        let auth = ClosingAuthority {
            closed_by: closer_2,
            source: &source,
            retirer: &retirer,
            deadline: Instant::now() + Duration::from_secs(5),
        };

        let successor = ledger
            .reserve_generation_closing_abandoned(closer_2, [], &lease, &auth)
            .expect("closed");
        assert_eq!(successor.generation, active.generation + 1);

        let record = ledger
            .record(active.generation)
            .expect("record")
            .expect("record");
        let closure = record.closure.expect("closure");
        assert_eq!(closure.closed_by, closer_2);
        assert_eq!(closure.admissions.len(), 2);
    }

    #[cfg(all(test, feature = "full-tests"))]
    #[test]
    fn exact_live_child_ignores_sigterm_among_exited_history() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);

        for i in 1..=5 {
            let inst = instance(600 + i, i as u64);
            admit(
                directory.path(),
                active.generation,
                &format!("launch-exited-{i:02}"),
                Some(HostedServiceKind::Sense),
                Some(inst),
            );
        }

        let live_inst = instance(700, 10);
        admit(
            directory.path(),
            active.generation,
            "launch-live-sigterm-ignore",
            Some(HostedServiceKind::Sense),
            Some(live_inst),
        );

        let (source, retirer) = fake(&[(live_inst, 501)], &[], &[]);
        let lease = lease(&ledger);
        let closer = instance(888, 1);
        let auth = authority(&source, &retirer, Duration::from_secs(30));

        let successor = ledger
            .reserve_generation_closing_abandoned(closer, [], &lease, &auth)
            .expect("closed");
        assert_eq!(successor.generation, active.generation + 1);

        let record = ledger
            .record(active.generation)
            .expect("record")
            .expect("record");
        let closure = record.closure.expect("closure");
        let live_closure = closure
            .admissions
            .iter()
            .find(|a| a.launch_id == "launch-live-sigterm-ignore")
            .expect("found");
        assert_eq!(
            live_closure.finding,
            AdmissionFinding::Retired { escalated: true }
        );
    }

    #[test]
    fn provisional_row_gains_acknowledgement_between_attempts_retired_under_seal() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);

        let launch_id = "launch-gain-ack";
        let inst = instance(801, 1);

        crate::lifecycle::write_parent_loss_admission_intent(
            directory.path(),
            &crate::lifecycle::AdmissionIntent::new(
                active.generation,
                launch_id,
                Some(HostedServiceKind::Sense),
                None,
            ),
        )
        .expect("intent");

        let (source, retirer) = fake(&[], &[], &[]);
        let lease = lease(&ledger);
        let closer = instance(999, 1);
        let expired_auth = ClosingAuthority {
            closed_by: closer,
            source: &source,
            retirer: &retirer,
            deadline: Instant::now() - Duration::from_millis(10),
        };

        let err = ledger
            .reserve_generation_closing_abandoned(closer, [], &lease, &expired_auth)
            .expect_err("refused");
        assert!(matches!(err, ParentLossLedgerError::RecoveryRequired(_)));

        let identity = crate::lifecycle::AdmissionIdentity {
            generation: active.generation,
            launch_id: launch_id.to_string(),
            instance: inst,
            uid: 501,
            parent_launch_id: None,
        };
        crate::lifecycle::acknowledge_parent_loss_admission(directory.path(), identity)
            .expect("ack");

        let (source2, retirer2) = fake(&[(inst, 501)], &[inst.pid], &[]);
        let auth2 = authority(&source2, &retirer2, Duration::from_secs(5));
        let successor = ledger
            .reserve_generation_closing_abandoned(closer, [], &lease, &auth2)
            .expect("closed");
        assert_eq!(successor.generation, active.generation + 1);

        let record = ledger
            .record(active.generation)
            .expect("record")
            .expect("record");
        let closure = record.closure.expect("closure");
        let admission = closure
            .admissions
            .iter()
            .find(|a| a.launch_id == launch_id)
            .expect("admission");
        assert_eq!(
            admission.finding,
            AdmissionFinding::Retired { escalated: false }
        );
    }

    #[test]
    fn generation_left_retiring_acknowledged_without_sealed_ledger_finishes() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let coordinator = active.coordinator.expect("coordinator");

        let inst = instance(901, 1);
        admit(
            directory.path(),
            active.generation,
            "launch-retiring-ack",
            Some(HostedServiceKind::Sense),
            Some(inst),
        );

        ledger
            .acknowledge_retirement(active.generation, coordinator)
            .expect("mark retiring acknowledged");

        let (source, retirer) = fake(&[(inst, 501)], &[inst.pid], &[]);
        let lease = lease(&ledger);
        let closer = instance(999, 1);
        let auth = authority(&source, &retirer, Duration::from_secs(5));

        let successor = ledger
            .reserve_generation_closing_abandoned(closer, [], &lease, &auth)
            .expect("closed");
        assert_eq!(successor.generation, active.generation + 1);

        let record = ledger
            .record(active.generation)
            .expect("record")
            .expect("record");
        assert!(record.closure.is_some());
        assert!(ledger.sealed_ledger_path(active.generation).exists());
    }

    #[test]
    fn malformed_wrong_generation_and_stale_seal_checkpoints_set_aside() {
        use solstone_core_journal_io::durability::SET_ASIDE_MARKER;

        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);

        let inst = instance(951, 1);
        admit(
            directory.path(),
            active.generation,
            "launch-setaside",
            Some(HostedServiceKind::Sense),
            Some(inst),
        );

        let intent_path =
            admission_directory(&ledger, active.generation).join("launch-setaside/intent.json");
        let original_intent_bytes = fs::read(&intent_path).expect("intent read");

        // 1. Malformed JSON byte string
        let recovery_path = ledger.closure_recovery_path(active.generation);
        fs::create_dir_all(recovery_path.parent().unwrap()).expect("create dir");
        fs::write(&recovery_path, b"not valid json bytes\n").expect("write malformed");

        let (loaded, set_aside) =
            load_recovery_checkpoint(&ledger, active.generation).expect("load");
        assert!(loaded.is_none());
        assert_eq!(set_aside.len(), 1);
        let aside_name = &set_aside[0];
        assert!(
            aside_name.contains(SET_ASIDE_MARKER),
            "set-aside name must carry SET_ASIDE_MARKER: {aside_name}"
        );
        let aside_path = recovery_path.parent().unwrap().join(aside_name);
        assert!(aside_path.exists(), "wedged file must still be there");
        let current_intent_bytes = fs::read(&intent_path).expect("intent read");
        assert_eq!(original_intent_bytes, current_intent_bytes);

        // 2. Checkpoint with wrong generation
        let wrong_gen_checkpoint = serde_json::json!({
            "schema": 1,
            "generation": active.generation + 999,
            "sealed_phase": "sealed",
            "set_aside": [],
            "entries": []
        });
        fs::write(
            &recovery_path,
            serde_json::to_vec(&wrong_gen_checkpoint).unwrap(),
        )
        .expect("write wrong gen");
        let (loaded, set_aside) =
            load_recovery_checkpoint(&ledger, active.generation).expect("load");
        assert!(loaded.is_none());
        assert_eq!(set_aside.len(), 1);
        let aside_name_2 = &set_aside[0];
        assert!(
            aside_name_2.contains(SET_ASIDE_MARKER),
            "wrong gen set-aside name must carry SET_ASIDE_MARKER: {aside_name_2}"
        );
        let aside_path_2 = recovery_path.parent().unwrap().join(aside_name_2);
        assert!(aside_path_2.exists(), "wedged file must still be there");
        let current_intent_bytes = fs::read(&intent_path).expect("intent read");
        assert_eq!(original_intent_bytes, current_intent_bytes);

        // 3. Checkpoint with sealed_phase != Sealed (e.g. Admitting)
        let wrong_phase_checkpoint = serde_json::json!({
            "schema": 1,
            "generation": active.generation,
            "sealed_phase": "admitting",
            "set_aside": [],
            "entries": []
        });
        fs::write(
            &recovery_path,
            serde_json::to_vec(&wrong_phase_checkpoint).unwrap(),
        )
        .expect("write wrong phase");
        let (loaded, set_aside) =
            load_recovery_checkpoint(&ledger, active.generation).expect("load");
        assert!(loaded.is_none());
        assert_eq!(set_aside.len(), 1);
        let aside_name_3 = &set_aside[0];
        assert!(
            aside_name_3.contains(SET_ASIDE_MARKER),
            "wrong phase set-aside name must carry SET_ASIDE_MARKER: {aside_name_3}"
        );
        let aside_path_3 = recovery_path.parent().unwrap().join(aside_name_3);
        assert!(aside_path_3.exists(), "wedged file must still be there");
        let current_intent_bytes = fs::read(&intent_path).expect("intent read");
        assert_eq!(original_intent_bytes, current_intent_bytes);

        // 4. Structurally valid checkpoint that merely omits a launch id:
        // Must NOT be set aside; that omitted id is reprocessed!
        admit(
            directory.path(),
            active.generation,
            "launch-omitted",
            Some(HostedServiceKind::Sense),
            Some(instance(952, 2)),
        );
        let valid_omitting_checkpoint = ParentLossClosureRecovery {
            schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
            generation: active.generation,
            sealed_phase: ParentLossPhase::Sealed,
            set_aside: vec![
                aside_name.clone(),
                aside_name_2.clone(),
                aside_name_3.clone(),
            ],
            escalation: None,
            entries: vec![RecoveryEntry {
                launch_id: "launch-setaside".to_string(),
                state: RecoveryEntryState::Completed,
                service: Some(HostedServiceKind::Sense),
                instance: Some(inst),
                uid: Some(501),
                parent_launch_id: None,
                finding: AdmissionFinding::Exited,
                intent_digest: Some(digest_bytes(&original_intent_bytes)),
                result_digest: None,
                ack_digest: None,
                spawn_identity_digest: None,
            }],
        };
        write_recovery_checkpoint(&ledger, active.generation, &valid_omitting_checkpoint)
            .expect("write valid checkpoint");

        let gen_dir = ledger.generation_path(active.generation);
        let count_wedged_before = fs::read_dir(&gen_dir)
            .expect("read_dir")
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(SET_ASIDE_MARKER))
            .count();

        let inst2 = instance(952, 2);
        let (source, retirer) = fake(&[(inst, 501), (inst2, 501)], &[inst.pid, inst2.pid], &[]);
        let lease = lease(&ledger);
        let closer = instance(999, 1);
        let auth = authority(&source, &retirer, Duration::from_secs(5));

        let successor = ledger
            .reserve_generation_closing_abandoned(closer, [], &lease, &auth)
            .expect("closed");
        assert_eq!(successor.generation, active.generation + 1);

        let count_wedged_after = fs::read_dir(&gen_dir)
            .expect("read_dir")
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(SET_ASIDE_MARKER))
            .count();
        assert_eq!(
            count_wedged_before, count_wedged_after,
            "no new wedged file should be created"
        );

        let record = ledger
            .record(active.generation)
            .expect("record")
            .expect("record");
        let closure = record.closure.expect("closure");
        assert_eq!(
            closure.admissions.len(),
            2,
            "both admissions present in closure"
        );
        assert!(
            closure
                .admissions
                .iter()
                .any(|a| a.launch_id == "launch-setaside")
        );
        assert!(
            closure
                .admissions
                .iter()
                .any(|a| a.launch_id == "launch-omitted")
        );

        // All wedged files must still be on disk
        assert!(aside_path.exists());
        assert!(aside_path_2.exists());
        assert!(aside_path_3.exists());
    }

    #[test]
    fn stored_escalation_claim_is_retired_before_streaming_directory() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);

        let inst1 = instance(101, 10);
        let inst2 = instance(102, 20);

        admit(
            directory.path(),
            active.generation,
            "claimed-child",
            Some(HostedServiceKind::Sense),
            Some(inst1),
        );
        admit(
            directory.path(),
            active.generation,
            "second-child",
            Some(HostedServiceKind::Sense),
            Some(inst2),
        );

        // inst1 ignores Terminate and dies on Kill; inst2 obeys Terminate.
        let live = &[(inst1, 501), (inst2, 501)];
        let (source, retirer) = fake(live, &[102], &[]);

        let checkpoint = ParentLossClosureRecovery {
            schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
            generation: active.generation,
            sealed_phase: ParentLossPhase::Sealed,
            set_aside: Vec::new(),
            escalation: Some(RecoveryEscalation {
                launch_id: "claimed-child".to_string(),
                service: Some(HostedServiceKind::Sense),
                instance: inst1,
                uid: Some(501),
                parent_launch_id: None,
                escalate_at_unix_ms: unix_millis().saturating_sub(10_000),
                descendant_escalated: false,
            }),
            entries: Vec::new(),
        };
        let recovery_path = ledger.closure_recovery_path(active.generation);
        if let Some(parent) = recovery_path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        write_json(&recovery_path, &checkpoint, json_options()).expect("write checkpoint");

        let closing = authority(&source, &retirer, Duration::from_secs(5));
        let lease = lease(&ledger);

        let successor = ledger
            .reserve_generation_closing_abandoned(instance(11, 3), [], &lease, &closing)
            .expect("close and reserve");
        assert_eq!(successor.generation, active.generation + 1);

        let signals = retirer.signals.lock().expect("signals");
        assert!(!signals.is_empty(), "signals must be recorded");
        assert_eq!(
            signals[0].0, 101,
            "the first signal pid must be the claimed pid"
        );
        assert_eq!(signals[0].1, "term");
        assert_eq!(
            signals[1].0, 101,
            "the second signal must be kill on the claimed pid"
        );
        assert_eq!(signals[1].1, "kill");

        let first_102_idx = signals.iter().position(|(pid, _)| *pid == 102);
        assert!(first_102_idx.is_some(), "second child must be signalled");
        assert!(
            first_102_idx.unwrap() > 1,
            "second child must not be signalled before claim is killed"
        );

        let record = ledger
            .record(active.generation)
            .expect("record readable")
            .expect("record exists");
        let closure = record.closure.expect("closure recorded");
        let claimed_entry = closure
            .admissions
            .iter()
            .find(|a| a.launch_id == "claimed-child")
            .expect("claimed admission in closure");
        assert_eq!(
            claimed_entry.finding,
            AdmissionFinding::Retired { escalated: true },
            "claimed child must be Retired {{ escalated: true }}"
        );
        let second_entry = closure
            .admissions
            .iter()
            .find(|a| a.launch_id == "second-child")
            .expect("second admission in closure");
        assert_eq!(
            second_entry.finding,
            AdmissionFinding::Retired { escalated: false },
            "second child must be Retired {{ escalated: false }}"
        );
    }
}
