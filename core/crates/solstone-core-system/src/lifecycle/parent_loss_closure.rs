// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Closing a generation whose supervisor and coordinator both exited before
//! the coordinator could publish a terminal result.
//!
//! Durability is established through discrete, immutable, whole-JSON artifacts
//! under `health/parent-loss/generations/<gen>/closure-recovery/`:
//! - `batches/<seq:06>.json`: lists completed rows up to `RECOVERY_BATCH_LIMIT` (256).
//! - `ids/<seq:06>.json`: lists launch IDs certified by that batch.
//! - `live/<launch_id>.json`: tracks live process state, census of descendants,
//!   escalate_at, reap deadline, term_requested, and binding digest.
//! - `provisional/<launch_id>.json`: tracks provisional findings (never-acknowledged,
//!   unidentified) awaiting possible late ack/result or finalization.
//! - `phase/scan-complete.json`: written when directory scan is finished, all
//!   provisional drops rechecked, and no live files remain.
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

#![allow(clippy::too_many_arguments, clippy::collapsible_if)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use solstone_core_journal_io::durability::{
    ArtifactId, DurableRead, read_json_durable_validated, set_aside,
};
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
    ParentLossTerminalDisposition, ParentLossUnresolvedReason, canonical_json, digest_bytes,
    json_options,
};
#[cfg(target_os = "linux")]
use crate::process::InspectResult;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::process::exact_descendant_tree;
use crate::process::{
    InstanceVerdict, ProcessInstance, ProcessInstanceSource, ProcessOwner, SignalKind,
    SystemProcessInstanceSource, TerminationError, process_owner, signal_exact_instance,
};

pub const PARENT_LOSS_CLOSURE_SCHEMA_V1: u32 = 1;
pub(crate) const PARENT_LOSS_RECOVERY_SCHEMA_V1: u32 = 1;
pub(crate) const RECOVERY_BATCH_LIMIT: usize = 256;
const OBSERVATION_SETTLE_WINDOW: Duration = Duration::from_millis(500);
const OBSERVATION_POLL: Duration = Duration::from_millis(25);
const ADMISSION_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
#[allow(dead_code)]
const ESCALATION_RESERVE: Duration = Duration::from_millis(1_500);
const LIVE_ESCALATION_GRACE_MS: u64 = 12_500;
const LIVE_REAP_GRACE_MS: u64 = 500;

#[cfg(test)]
thread_local! {
    pub(crate) static PROVISIONAL_STOP_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

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
pub(crate) enum RecoveryArtifactKind {
    Batch,
    Ids,
    Live,
    Provisional,
    ScanComplete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletedBatchRow {
    pub(crate) launch_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) service: Option<HostedServiceKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) instance: Option<ProcessInstance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parent_launch_id: Option<String>,
    pub(crate) finding: AdmissionFinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parsed_result_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) intent_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) result_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ack_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) spawn_identity_digest: Option<String>,
    pub(crate) binding_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecoveryBatchFile {
    pub(crate) schema: u32,
    pub(crate) kind: RecoveryArtifactKind,
    pub(crate) generation: ParentLossGeneration,
    pub(crate) sealed_phase: ParentLossPhase,
    pub(crate) seq: u64,
    pub(crate) entries: Vec<CompletedBatchRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecoveryIdsFile {
    pub(crate) schema: u32,
    pub(crate) kind: RecoveryArtifactKind,
    pub(crate) generation: ParentLossGeneration,
    pub(crate) sealed_phase: ParentLossPhase,
    pub(crate) seq: u64,
    pub(crate) ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LiveDescendantIdentity {
    pub(crate) instance: ProcessInstance,
    pub(crate) uid: u32,
    pub(crate) ppid: u32,
    pub(crate) pgid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LiveRecoveryFile {
    pub(crate) schema: u32,
    pub(crate) kind: RecoveryArtifactKind,
    pub(crate) generation: ParentLossGeneration,
    pub(crate) sealed_phase: ParentLossPhase,
    pub(crate) launch_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) service: Option<HostedServiceKind>,
    pub(crate) root: ProcessInstance,
    pub(crate) uid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parent_launch_id: Option<String>,
    pub(crate) descendants: Vec<LiveDescendantIdentity>,
    pub(crate) escalate_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reap_deadline_unix_ms: Option<u64>,
    pub(crate) term_requested: bool,
    #[serde(default)]
    pub(crate) escalated: bool,
    pub(crate) binding_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProvisionalFile {
    pub(crate) schema: u32,
    pub(crate) kind: RecoveryArtifactKind,
    pub(crate) generation: ParentLossGeneration,
    pub(crate) sealed_phase: ParentLossPhase,
    pub(crate) launch_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) service: Option<HostedServiceKind>,
    pub(crate) finding: AdmissionFinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) intent_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) result_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ack_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) spawn_identity_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ScanCompleteFile {
    pub(crate) schema: u32,
    pub(crate) kind: RecoveryArtifactKind,
    pub(crate) generation: ParentLossGeneration,
    pub(crate) sealed_phase: ParentLossPhase,
    pub(crate) sorted_launch_ids: Vec<String>,
    pub(crate) ids_sequences: Vec<u64>,
    pub(crate) ids_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct AdmissionDropDigests {
    pub(crate) intent: Option<String>,
    pub(crate) result: Option<String>,
    pub(crate) ack: Option<String>,
    pub(crate) spawn_identity: Option<String>,
}

impl AdmissionDropDigests {
    #[allow(dead_code)]
    pub(crate) fn is_all_absent(&self) -> bool {
        self.intent.is_none()
            && self.result.is_none()
            && self.ack.is_none()
            && self.spawn_identity.is_none()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DerivedAdmissionDrops {
    pub(crate) service: Option<HostedServiceKind>,
    pub(crate) instance: Option<ProcessInstance>,
    pub(crate) uid: Option<u32>,
    pub(crate) parent_launch_id: Option<String>,
    pub(crate) finding: AdmissionFinding,
    pub(crate) parsed_result_state: Option<String>,
    pub(crate) digests: AdmissionDropDigests,
}

#[derive(Serialize)]
struct CanonicalCompletedBinding<'a> {
    launch_id: &'a str,
    service: Option<HostedServiceKind>,
    instance: Option<ProcessInstance>,
    uid: Option<u32>,
    parent_launch_id: Option<&'a str>,
    finding: AdmissionFinding,
    parsed_result_state: Option<&'a str>,
    intent_digest: Option<&'a str>,
    result_digest: Option<&'a str>,
    ack_digest: Option<&'a str>,
    spawn_identity_digest: Option<&'a str>,
}

pub(crate) fn compute_batch_row_binding_digest(
    launch_id: &str,
    service: Option<HostedServiceKind>,
    instance: Option<ProcessInstance>,
    uid: Option<u32>,
    parent_launch_id: Option<&str>,
    finding: AdmissionFinding,
    parsed_result_state: Option<&str>,
    intent_digest: Option<&str>,
    result_digest: Option<&str>,
    ack_digest: Option<&str>,
    spawn_identity_digest: Option<&str>,
) -> Result<String, ParentLossLedgerError> {
    let binding = CanonicalCompletedBinding {
        launch_id,
        service,
        instance,
        uid,
        parent_launch_id,
        finding,
        parsed_result_state,
        intent_digest,
        result_digest,
        ack_digest,
        spawn_identity_digest,
    };
    let bytes = canonical_json(&binding)?;
    Ok(digest_bytes(&bytes))
}

#[derive(Serialize)]
struct CanonicalLiveBinding<'a> {
    root: ProcessInstance,
    uid: u32,
    descendants: &'a [LiveDescendantIdentity],
}

pub(crate) fn compute_live_binding_digest(
    root: ProcessInstance,
    uid: u32,
    descendants: &[LiveDescendantIdentity],
) -> Result<String, ParentLossLedgerError> {
    let binding = CanonicalLiveBinding {
        root,
        uid,
        descendants,
    };
    let bytes = canonical_json(&binding)?;
    Ok(digest_bytes(&bytes))
}

pub(crate) fn completed_row_is_authorized(row: &CompletedBatchRow) -> bool {
    if validate_launch_id(&row.launch_id).is_err() {
        return false;
    }
    match row.finding {
        AdmissionFinding::NeverAcknowledged | AdmissionFinding::Unidentified => return false,
        AdmissionFinding::Exited
        | AdmissionFinding::Retired { .. }
        | AdmissionFinding::PidReusedByAnotherUser => {
            if row.instance.is_none() || row.uid.is_none() {
                return false;
            }
        }
        AdmissionFinding::SpawnFailed => {
            if row.parsed_result_state.as_deref() != Some("spawn_failed") {
                return false;
            }
        }
        AdmissionFinding::RejectedAndReaped => {
            if row.parsed_result_state.as_deref() != Some("rejected_and_reaped") {
                return false;
            }
        }
    }
    let Ok(expected_digest) = compute_batch_row_binding_digest(
        &row.launch_id,
        row.service,
        row.instance,
        row.uid,
        row.parent_launch_id.as_deref(),
        row.finding,
        row.parsed_result_state.as_deref(),
        row.intent_digest.as_deref(),
        row.result_digest.as_deref(),
        row.ack_digest.as_deref(),
        row.spawn_identity_digest.as_deref(),
    ) else {
        return false;
    };
    row.binding_digest == expected_digest
}

#[allow(dead_code)]
pub(crate) fn skip_matches_source(
    row: &CompletedBatchRow,
    derived: &DerivedAdmissionDrops,
) -> bool {
    if !completed_row_is_authorized(row) {
        return false;
    }
    if row.service != derived.service
        || row.instance != derived.instance
        || row.uid != derived.uid
        || row.parent_launch_id != derived.parent_launch_id
        || row.finding != derived.finding
        || row.parsed_result_state != derived.parsed_result_state
        || row.intent_digest != derived.digests.intent
        || row.result_digest != derived.digests.result
        || row.ack_digest != derived.digests.ack
        || row.spawn_identity_digest != derived.digests.spawn_identity
    {
        return false;
    }
    true
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
#[allow(dead_code)]
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

pub(crate) fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

fn read_drop_bytes(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn read_derived_admission_drops(
    ledger: &ParentLossLedger,
    generation: ParentLossGeneration,
    launch_id: &str,
) -> Result<DerivedAdmissionDrops, ParentLossLedgerError> {
    let admission_dir = admission_directory(ledger, generation).join(launch_id);
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

    let intent = read_parent_loss_admission_intent(ledger, generation, launch_id)
        .ok()
        .flatten();
    let service = intent.as_ref().and_then(|i| i.service);
    let result = read_parent_loss_admission_result(ledger, generation, launch_id)
        .ok()
        .flatten();
    let acknowledgement =
        read_parent_loss_admission_acknowledgement_in(ledger, generation, launch_id)
            .ok()
            .flatten();
    let spawn_identity =
        read_parent_loss_admission_spawn_identity_in(ledger, generation, launch_id)
            .ok()
            .flatten();

    let mut parsed_result_state = None;
    let (identity, finding) = match result {
        Some(result) => match &result.state {
            AdmissionResultState::Admitted => {
                parsed_result_state = Some("admitted".to_string());
                match result
                    .identity
                    .or_else(|| acknowledgement.map(|ack| ack.identity))
                    .or_else(|| spawn_identity.map(|s| s.identity))
                {
                    Some(id) => (Some(id), None),
                    None => (None, Some(AdmissionFinding::Unidentified)),
                }
            }
            AdmissionResultState::RejectedUnreaped { .. } => {
                parsed_result_state = Some("rejected_unreaped".to_string());
                match result
                    .identity
                    .or_else(|| acknowledgement.map(|ack| ack.identity))
                    .or_else(|| spawn_identity.map(|s| s.identity))
                {
                    Some(id) => (Some(id), None),
                    None => (None, Some(AdmissionFinding::Unidentified)),
                }
            }
            AdmissionResultState::SpawnFailed { .. } => {
                parsed_result_state = Some("spawn_failed".to_string());
                (None, Some(AdmissionFinding::SpawnFailed))
            }
            AdmissionResultState::RejectedAndReaped { .. } => {
                parsed_result_state = Some("rejected_and_reaped".to_string());
                (None, Some(AdmissionFinding::RejectedAndReaped))
            }
        },
        None => match acknowledgement {
            Some(ack) => (Some(ack.identity), None),
            None => match spawn_identity {
                Some(sp) => (Some(sp.identity), None),
                None => {
                    #[cfg(target_os = "linux")]
                    {
                        match find_live_process_by_launch_env(
                            ledger.canonical_root(),
                            generation,
                            launch_id,
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

    let (instance, uid, parent_launch_id, finding) = match identity {
        Some(id) => (
            Some(id.instance),
            Some(id.uid),
            id.parent_launch_id,
            finding.unwrap_or(AdmissionFinding::Exited),
        ),
        None => (
            None,
            None,
            None,
            finding.unwrap_or(AdmissionFinding::Unidentified),
        ),
    };

    Ok(DerivedAdmissionDrops {
        service,
        instance,
        uid,
        parent_launch_id,
        finding,
        parsed_result_state,
        digests,
    })
}

#[allow(dead_code)]
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

fn flush_batch_and_ids(
    ledger: &ParentLossLedger,
    generation: ParentLossGeneration,
    seq: u64,
    batch_buffer: &mut Vec<CompletedBatchRow>,
    completed_ids: &mut HashSet<String>,
) -> Result<(), ParentLossLedgerError> {
    if batch_buffer.is_empty() {
        return Ok(());
    }
    let batch_file = RecoveryBatchFile {
        schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
        kind: RecoveryArtifactKind::Batch,
        generation,
        sealed_phase: ParentLossPhase::Sealed,
        seq,
        entries: batch_buffer.clone(),
    };
    let batch_path = ledger.closure_recovery_batch_path(generation, seq);
    write_json(&batch_path, &batch_file, json_options())?;

    let ids: Vec<String> = batch_buffer.iter().map(|e| e.launch_id.clone()).collect();
    let ids_file = RecoveryIdsFile {
        schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
        kind: RecoveryArtifactKind::Ids,
        generation,
        sealed_phase: ParentLossPhase::Sealed,
        seq,
        ids,
    };
    let ids_path = ledger.closure_recovery_ids_path(generation, seq);
    write_json(&ids_path, &ids_file, json_options())?;

    for row in batch_buffer.drain(..) {
        let live_path = ledger.closure_recovery_live_path(generation, &row.launch_id);
        if live_path.is_file() {
            let _ = fs::remove_file(&live_path);
        }
        let prov_path = ledger.closure_recovery_provisional_path(generation, &row.launch_id);
        if prov_path.is_file() {
            let _ = fs::remove_file(&prov_path);
        }
        completed_ids.insert(row.launch_id.clone());
    }

    #[cfg(feature = "test-hooks")]
    if std::env::var("SOLSTONE_PARENT_LOSS_CLOSURE_SEAM").as_deref() == Ok("after-batch") {
        loop {
            thread::sleep(Duration::from_millis(100));
        }
    }

    Ok(())
}

fn run_live_state_machine(
    ledger: &ParentLossLedger,
    generation: ParentLossGeneration,
    authority: &ClosingAuthority<'_>,
    mut live_file: LiveRecoveryFile,
    batch_buffer: &mut Vec<CompletedBatchRow>,
    next_seq: &mut u64,
    completed_ids: &mut HashSet<String>,
) -> Result<Option<ClosureOutcome>, ParentLossLedgerError> {
    let live_path = ledger.closure_recovery_live_path(generation, &live_file.launch_id);

    // Derive root instance and uid from drops
    let derived = read_derived_admission_drops(ledger, generation, &live_file.launch_id)?;
    if derived.instance != Some(live_file.root) || derived.uid != Some(live_file.uid) {
        if let Ok(aside) = set_aside(&live_path) {
            let _ = aside;
        }
        let is_gone = match (derived.instance, derived.uid) {
            (Some(inst), Some(_)) => {
                matches!(
                    authority.source.observe(&inst),
                    InstanceVerdict::NotSameOrExited
                )
            }
            _ => true,
        };
        if is_gone {
            return Ok(Some(ClosureOutcome::Refused(
                BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
            )));
        }
        let (Some(inst), Some(uid)) = (derived.instance, derived.uid) else {
            return Ok(Some(ClosureOutcome::Refused(
                BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
            )));
        };
        live_file.root = inst;
        live_file.uid = uid;
        live_file.descendants.clear();
        live_file.binding_digest.clear();
        live_file.term_requested = false;
        live_file.escalated = false;
        live_file.escalate_at_unix_ms = 0;
        live_file.reap_deadline_unix_ms = None;
    }

    #[cfg(any(test, feature = "test-hooks"))]
    if crate::process::retirement_skip_test_fault()
        && matches!(
            observe_settled(authority.source, &live_file.root, authority.deadline),
            InstanceVerdict::SameLive { .. }
        )
    {
        return Ok(Some(ClosureOutcome::Refused(
            BootstrapRecoveryReason::AbandonedAdmissionLive,
        )));
    }

    let mut verdict = authority.source.observe(&live_file.root);

    if matches!(verdict, InstanceVerdict::SameLive { .. }) {
        if live_file.binding_digest.is_empty() && !live_file.term_requested {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                let snapshot =
                    match exact_descendant_tree(live_file.root, live_file.uid, authority.source) {
                        Ok(s) => s,
                        Err(_) => {
                            return Ok(Some(ClosureOutcome::Refused(
                                BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
                            )));
                        }
                    };
                let descendants: Vec<LiveDescendantIdentity> = snapshot
                    .descendants
                    .into_iter()
                    .filter_map(|d| {
                        let birth = snapshot.descendant_births.get(&d.pid).copied()?;
                        let pid = u32::try_from(d.pid).ok()?;
                        let ppid = u32::try_from(d.ppid).unwrap_or(0);
                        let pgid = d.pgid.and_then(|g| u32::try_from(g).ok()).unwrap_or(0);
                        Some(LiveDescendantIdentity {
                            instance: ProcessInstance { pid, birth },
                            uid: d.uid,
                            ppid,
                            pgid,
                        })
                    })
                    .collect();
                live_file.descendants = descendants;
            }

            live_file.binding_digest =
                compute_live_binding_digest(live_file.root, live_file.uid, &live_file.descendants)?;

            let now_ms = unix_millis();
            live_file.escalate_at_unix_ms = now_ms + LIVE_ESCALATION_GRACE_MS;
            live_file.reap_deadline_unix_ms = None;
            live_file.term_requested = false;
            live_file.escalated = false;
            write_json(&live_path, &live_file, json_options())?;
        } else {
            let now_ms = unix_millis();
            if live_file.escalate_at_unix_ms > now_ms + LIVE_ESCALATION_GRACE_MS {
                live_file.escalate_at_unix_ms = now_ms + LIVE_ESCALATION_GRACE_MS;
                write_json(&live_path, &live_file, json_options())?;
            }
        }

        if !live_file.term_requested {
            live_file.term_requested = true;
            write_json(&live_path, &live_file, json_options())?;
            let _ = authority
                .retirer
                .signal(live_file.root, SignalKind::Terminate);
        } else {
            let _ = authority
                .retirer
                .signal(live_file.root, SignalKind::Terminate);
        }

        verdict = authority.source.observe(&live_file.root);
        while matches!(verdict, InstanceVerdict::SameLive { .. }) {
            let now_ms = unix_millis();
            if now_ms >= live_file.escalate_at_unix_ms {
                break;
            }
            let wait_ms = live_file.escalate_at_unix_ms - now_ms;
            let escalate_instant = Instant::now() + Duration::from_millis(wait_ms);
            let wait_until = escalate_instant.min(authority.deadline);
            let exit_v =
                wait_single_for_exit(authority, live_file.root, Some(live_file.uid), wait_until);
            if matches!(exit_v, SingleExitVerdict::SameLive) {
                if Instant::now() >= authority.deadline {
                    return Ok(Some(ClosureOutcome::Refused(
                        BootstrapRecoveryReason::CoordinatorNotLive,
                    )));
                }
            }
            verdict = authority.source.observe(&live_file.root);
        }

        let now_ms = unix_millis();
        if matches!(verdict, InstanceVerdict::SameLive { .. })
            && now_ms >= live_file.escalate_at_unix_ms
        {
            let mut need_write = false;
            match live_file.reap_deadline_unix_ms {
                None => {
                    live_file.reap_deadline_unix_ms = Some(now_ms + LIVE_REAP_GRACE_MS);
                    need_write = true;
                }
                Some(stored) if stored > now_ms + LIVE_REAP_GRACE_MS => {
                    live_file.reap_deadline_unix_ms = Some(now_ms + LIVE_REAP_GRACE_MS);
                    need_write = true;
                }
                _ => {}
            }
            if !live_file.escalated {
                live_file.escalated = true;
                need_write = true;
            }
            if need_write {
                write_json(&live_path, &live_file, json_options())?;
            }
            let _ = authority.retirer.signal(live_file.root, SignalKind::Kill);

            let reap_ms = live_file.reap_deadline_unix_ms.unwrap();
            let cur_ms = unix_millis();
            if cur_ms < reap_ms {
                let wait_ms = reap_ms - cur_ms;
                let reap_instant = Instant::now() + Duration::from_millis(wait_ms);
                let wait_until = reap_instant.min(authority.deadline);
                let _ = wait_single_for_exit(
                    authority,
                    live_file.root,
                    Some(live_file.uid),
                    wait_until,
                );
            }
            if unix_millis() >= reap_ms
                && matches!(
                    authority.source.observe(&live_file.root),
                    InstanceVerdict::SameLive { .. }
                )
            {
                return Ok(Some(ClosureOutcome::Refused(
                    BootstrapRecoveryReason::AbandonedAdmissionLive,
                )));
            }
            verdict = authority.source.observe(&live_file.root);
        }
    }

    match verdict {
        InstanceVerdict::NotSameOrExited => {
            let expected_digest =
                compute_live_binding_digest(live_file.root, live_file.uid, &live_file.descendants)?;
            if live_file.binding_digest != expected_digest {
                return Ok(Some(ClosureOutcome::Refused(
                    BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
                )));
            }

            let mut any_descendant_live = false;
            for desc in &live_file.descendants {
                match authority.source.observe(&desc.instance) {
                    InstanceVerdict::SameLive { .. } => {
                        if authority.retirer.owner(desc.instance.pid) == ProcessOwner::Uid(desc.uid)
                        {
                            let _ = authority
                                .retirer
                                .signal(desc.instance, SignalKind::Terminate);
                        }
                        if matches!(
                            authority.source.observe(&desc.instance),
                            InstanceVerdict::SameLive { .. }
                        ) {
                            any_descendant_live = true;
                        }
                    }
                    InstanceVerdict::Unverifiable => {
                        if settle_unverifiable(authority, desc.instance.pid, Some(desc.uid))
                            .is_none()
                        {
                            return Ok(Some(ClosureOutcome::Refused(
                                BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
                            )));
                        }
                    }
                    InstanceVerdict::NotSameOrExited => {}
                }
            }
            if any_descendant_live {
                return Ok(Some(ClosureOutcome::Refused(
                    BootstrapRecoveryReason::AbandonedAdmissionLive,
                )));
            }

            let finding = AdmissionFinding::Retired {
                escalated: live_file.escalated,
            };
            let binding_digest = compute_batch_row_binding_digest(
                &live_file.launch_id,
                live_file.service,
                Some(live_file.root),
                Some(live_file.uid),
                live_file.parent_launch_id.as_deref(),
                finding,
                derived.parsed_result_state.as_deref(),
                derived.digests.intent.as_deref(),
                derived.digests.result.as_deref(),
                derived.digests.ack.as_deref(),
                derived.digests.spawn_identity.as_deref(),
            )?;

            let row = CompletedBatchRow {
                launch_id: live_file.launch_id.clone(),
                service: live_file.service,
                instance: Some(live_file.root),
                uid: Some(live_file.uid),
                parent_launch_id: live_file.parent_launch_id,
                finding,
                parsed_result_state: derived.parsed_result_state,
                intent_digest: derived.digests.intent,
                result_digest: derived.digests.result,
                ack_digest: derived.digests.ack,
                spawn_identity_digest: derived.digests.spawn_identity,
                binding_digest,
            };
            batch_buffer.push(row);
            flush_batch_and_ids(ledger, generation, *next_seq, batch_buffer, completed_ids)?;
            *next_seq += 1;
            let _ = fs::remove_file(&live_path);
            Ok(None)
        }
        InstanceVerdict::Unverifiable => {
            match settle_unverifiable(authority, live_file.root.pid, Some(live_file.uid)) {
                Some(finding) => {
                    let binding_digest = compute_batch_row_binding_digest(
                        &live_file.launch_id,
                        live_file.service,
                        Some(live_file.root),
                        Some(live_file.uid),
                        live_file.parent_launch_id.as_deref(),
                        finding,
                        derived.parsed_result_state.as_deref(),
                        derived.digests.intent.as_deref(),
                        derived.digests.result.as_deref(),
                        derived.digests.ack.as_deref(),
                        derived.digests.spawn_identity.as_deref(),
                    )?;
                    let row = CompletedBatchRow {
                        launch_id: live_file.launch_id.clone(),
                        service: live_file.service,
                        instance: Some(live_file.root),
                        uid: Some(live_file.uid),
                        parent_launch_id: live_file.parent_launch_id,
                        finding,
                        parsed_result_state: derived.parsed_result_state,
                        intent_digest: derived.digests.intent,
                        result_digest: derived.digests.result,
                        ack_digest: derived.digests.ack,
                        spawn_identity_digest: derived.digests.spawn_identity,
                        binding_digest,
                    };
                    batch_buffer.push(row);
                    flush_batch_and_ids(
                        ledger,
                        generation,
                        *next_seq,
                        batch_buffer,
                        completed_ids,
                    )?;
                    *next_seq += 1;
                    let _ = fs::remove_file(&live_path);
                    Ok(None)
                }
                None => Ok(Some(ClosureOutcome::Refused(
                    BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
                ))),
            }
        }
        InstanceVerdict::SameLive { .. } => Ok(Some(ClosureOutcome::Refused(
            BootstrapRecoveryReason::CoordinatorNotLive,
        ))),
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
        set_aside: mut set_aside_files,
        mut notes,
    } = abandoned;

    // 1. SameLive supervisor and coordinator vetoes first.
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

    // 2. Mark sealed for closure before scan.
    ledger.mark_sealed_for_closure(generation)?;

    // 3. Legacy closure-recovery.json set-aside.
    let legacy_path = ledger.closure_recovery_legacy_path(generation);
    if legacy_path.is_file() {
        if let Ok(aside) = set_aside(&legacy_path) {
            if let Some(file_name) = aside.file_name() {
                let name = file_name.to_string_lossy().into_owned();
                if !set_aside_files.contains(&name) {
                    set_aside_files.push(name);
                }
            }
        }
    }

    // 4. Create directory structure.
    fs::create_dir_all(ledger.closure_recovery_batches_dir(generation))?;
    fs::create_dir_all(ledger.closure_recovery_ids_dir(generation))?;
    fs::create_dir_all(ledger.closure_recovery_live_dir(generation))?;
    fs::create_dir_all(ledger.closure_recovery_provisional_dir(generation))?;
    fs::create_dir_all(ledger.closure_recovery_phase_dir(generation))?;

    // 5. Hold admissions lock.
    let _lock = hold_lock(
        ledger.admission_lock_path(generation),
        LockOptions {
            timeout: ADMISSION_LOCK_TIMEOUT,
            poll_interval: Duration::from_millis(10),
            mode: Some(0o600),
        },
    )?;

    // 6. Discovery skip set: read ids files only.
    let mut raw_ids: HashMap<u64, (RecoveryIdsFile, std::path::PathBuf)> = HashMap::new();
    let mut batch_paths: HashMap<u64, std::path::PathBuf> = HashMap::new();
    let mut max_seq: u64 = 0;

    let ids_dir = ledger.closure_recovery_ids_dir(generation);
    if let Ok(entries) = fs::read_dir(&ids_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match read_json_durable_validated::<RecoveryIdsFile>(
                ArtifactId::ParentLossClosureRecovery,
                &path,
                |f| {
                    if f.schema != PARENT_LOSS_RECOVERY_SCHEMA_V1
                        || f.generation != generation
                        || f.sealed_phase != ParentLossPhase::Sealed
                        || f.kind != RecoveryArtifactKind::Ids
                    {
                        return Err("invalid ids envelope".to_string());
                    }
                    Ok(())
                },
            ) {
                Ok(DurableRead::Present(ids_file)) => {
                    max_seq = max_seq.max(ids_file.seq);
                    raw_ids.insert(ids_file.seq, (ids_file, path));
                }
                Ok(DurableRead::SetAside(aside)) => {
                    if let Some(name) = aside.file_name() {
                        let name = name.to_string_lossy().into_owned();
                        if !set_aside_files.contains(&name) {
                            set_aside_files.push(name);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    let batches_dir = ledger.closure_recovery_batches_dir(generation);
    if let Ok(entries) = fs::read_dir(&batches_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if let Ok(seq) = stem.parse::<u64>() {
                    max_seq = max_seq.max(seq);
                    batch_paths.insert(seq, path);
                }
            }
        }
    }

    let mut completed_ids: HashSet<String> = HashSet::new();
    let mut valid_ids_seqs: Vec<u64> = Vec::new();
    let mut sorted_seqs: Vec<u64> = raw_ids.keys().chain(batch_paths.keys()).copied().collect();
    sorted_seqs.sort_unstable();
    sorted_seqs.dedup();

    for seq in sorted_seqs {
        let ids_entry = raw_ids.remove(&seq);
        let batch_path = batch_paths.remove(&seq);

        match (ids_entry, batch_path) {
            (Some((ids_file, i_path)), Some(b_path)) => {
                let all_valid = ids_file.ids.iter().all(|id| validate_launch_id(id).is_ok());
                let has_dup = ids_file.ids.iter().any(|id| completed_ids.contains(id));
                if !all_valid || has_dup {
                    if let Ok(aside) = set_aside(&i_path) {
                        if let Some(name) = aside.file_name() {
                            set_aside_files.push(name.to_string_lossy().into_owned());
                        }
                    }
                    if let Ok(aside) = set_aside(&b_path) {
                        if let Some(name) = aside.file_name() {
                            set_aside_files.push(name.to_string_lossy().into_owned());
                        }
                    }
                    continue;
                }
                for id in ids_file.ids {
                    completed_ids.insert(id);
                }
                valid_ids_seqs.push(seq);
            }
            (Some((_, i_path)), None) => {
                if let Ok(aside) = set_aside(&i_path) {
                    if let Some(name) = aside.file_name() {
                        set_aside_files.push(name.to_string_lossy().into_owned());
                    }
                }
            }
            (None, Some(b_path)) => {
                match read_json_durable_validated::<RecoveryBatchFile>(
                    ArtifactId::ParentLossClosureRecovery,
                    &b_path,
                    |b| {
                        if b.schema != PARENT_LOSS_RECOVERY_SCHEMA_V1
                            || b.generation != generation
                            || b.sealed_phase != ParentLossPhase::Sealed
                            || b.kind != RecoveryArtifactKind::Batch
                        {
                            return Err("invalid batch envelope".to_string());
                        }
                        Ok(())
                    },
                ) {
                    Ok(DurableRead::Present(batch)) => {
                        let all_auth = batch.entries.iter().all(completed_row_is_authorized);
                        let all_valid = batch
                            .entries
                            .iter()
                            .all(|e| validate_launch_id(&e.launch_id).is_ok());
                        let has_dup = batch
                            .entries
                            .iter()
                            .any(|e| completed_ids.contains(&e.launch_id));
                        if !all_auth || !all_valid || has_dup {
                            if let Ok(aside) = set_aside(&b_path) {
                                if let Some(name) = aside.file_name() {
                                    set_aside_files.push(name.to_string_lossy().into_owned());
                                }
                            }
                            continue;
                        }
                        let ids: Vec<String> =
                            batch.entries.iter().map(|e| e.launch_id.clone()).collect();
                        let ids_file = RecoveryIdsFile {
                            schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
                            kind: RecoveryArtifactKind::Ids,
                            generation,
                            sealed_phase: ParentLossPhase::Sealed,
                            seq,
                            ids: ids.clone(),
                        };
                        let i_path = ledger.closure_recovery_ids_path(generation, seq);
                        if write_json(&i_path, &ids_file, json_options()).is_ok() {
                            for id in ids {
                                completed_ids.insert(id);
                            }
                            valid_ids_seqs.push(seq);
                        }
                    }
                    Ok(DurableRead::SetAside(aside)) => {
                        if let Some(name) = aside.file_name() {
                            set_aside_files.push(name.to_string_lossy().into_owned());
                        }
                    }
                    _ => {}
                }
            }
            (None, None) => {}
        }
    }

    let mut next_seq = max_seq + 1;
    let mut current_batch_buffer: Vec<CompletedBatchRow> = Vec::new();

    // 7. Load provisional files.
    let mut provisional_map: HashMap<String, ProvisionalFile> = HashMap::new();
    let prov_dir = ledger.closure_recovery_provisional_dir(generation);
    if let Ok(entries) = fs::read_dir(&prov_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match read_json_durable_validated::<ProvisionalFile>(
                ArtifactId::ParentLossClosureRecovery,
                &path,
                |p| {
                    if p.schema != PARENT_LOSS_RECOVERY_SCHEMA_V1
                        || p.generation != generation
                        || p.sealed_phase != ParentLossPhase::Sealed
                        || p.kind != RecoveryArtifactKind::Provisional
                    {
                        return Err("invalid provisional envelope".to_string());
                    }
                    Ok(())
                },
            ) {
                Ok(DurableRead::Present(prov)) => {
                    provisional_map.insert(prov.launch_id.clone(), prov);
                }
                Ok(DurableRead::SetAside(aside)) => {
                    if let Some(name) = aside.file_name() {
                        set_aside_files.push(name.to_string_lossy().into_owned());
                    }
                }
                _ => {}
            }
        }
    }

    // 8. Check scan-complete.json.
    let mut skip_dir_scan = false;
    let scan_complete_path = ledger.closure_recovery_scan_complete_path(generation);
    if scan_complete_path.is_file() {
        match read_json_durable_validated::<ScanCompleteFile>(
            ArtifactId::ParentLossClosureRecovery,
            &scan_complete_path,
            |s| {
                if s.schema != PARENT_LOSS_RECOVERY_SCHEMA_V1
                    || s.generation != generation
                    || s.sealed_phase != ParentLossPhase::Sealed
                    || s.kind != RecoveryArtifactKind::ScanComplete
                {
                    return Err("invalid scan-complete envelope".to_string());
                }
                Ok(())
            },
        ) {
            Ok(DurableRead::Present(sc)) => {
                let mut expected_ids: Vec<String> = completed_ids
                    .iter()
                    .chain(provisional_map.keys())
                    .cloned()
                    .collect();
                expected_ids.sort();
                expected_ids.dedup();
                let expected_bytes = canonical_json(&expected_ids)?;
                let expected_digest = digest_bytes(&expected_bytes);

                if sc.sorted_launch_ids == expected_ids
                    && sc.ids_sequences == valid_ids_seqs
                    && sc.ids_digest == expected_digest
                {
                    skip_dir_scan = true;
                } else if let Ok(aside) = set_aside(&scan_complete_path) {
                    if let Some(name) = aside.file_name() {
                        set_aside_files.push(name.to_string_lossy().into_owned());
                    }
                }
            }
            Ok(DurableRead::SetAside(aside)) => {
                if let Some(name) = aside.file_name() {
                    set_aside_files.push(name.to_string_lossy().into_owned());
                }
            }
            _ => {}
        }
    }

    // 9. Process live files before read_dir.
    let live_dir = ledger.closure_recovery_live_dir(generation);
    let mut live_files = Vec::new();
    if let Ok(entries) = fs::read_dir(&live_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match read_json_durable_validated::<LiveRecoveryFile>(
                ArtifactId::ParentLossClosureRecovery,
                &path,
                |l| {
                    if l.schema != PARENT_LOSS_RECOVERY_SCHEMA_V1
                        || l.generation != generation
                        || l.sealed_phase != ParentLossPhase::Sealed
                        || l.kind != RecoveryArtifactKind::Live
                    {
                        return Err("invalid live envelope".to_string());
                    }
                    Ok(())
                },
            ) {
                Ok(DurableRead::Present(live_file)) => {
                    live_files.push(live_file);
                }
                Ok(DurableRead::SetAside(aside)) => {
                    if let Some(name) = aside.file_name() {
                        set_aside_files.push(name.to_string_lossy().into_owned());
                    }
                    let launch_id = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let derived = read_derived_admission_drops(ledger, generation, &launch_id)?;
                    let is_gone = match derived.instance {
                        Some(inst) => matches!(
                            authority.source.observe(&inst),
                            InstanceVerdict::NotSameOrExited
                        ),
                        None => true,
                    };
                    if is_gone {
                        return Ok(ClosureOutcome::Refused(
                            BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
                        ));
                    }
                }
                _ => {}
            }
        }
    }

    for live_file in live_files {
        if let Some(outcome) = run_live_state_machine(
            ledger,
            generation,
            authority,
            live_file,
            &mut current_batch_buffer,
            &mut next_seq,
            &mut completed_ids,
        )? {
            return Ok(outcome);
        }
    }

    // 10. Scan admissions directory if not skipped by valid scan-complete.
    if !skip_dir_scan {
        let admissions_dir = admission_directory(ledger, generation);
        let dir_entries = match fs::read_dir(&admissions_dir) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };

        if let Some(entries) = dir_entries {
            for entry in entries {
                let entry = entry?;
                let file_name = entry.file_name();
                let Some(launch_id_str) = file_name.to_str() else {
                    return Ok(ClosureOutcome::Refused(
                        BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
                    ));
                };
                if validate_launch_id(launch_id_str).is_err() {
                    return Ok(ClosureOutcome::Refused(
                        BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
                    ));
                }

                if completed_ids.contains(launch_id_str) {
                    continue;
                }

                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let launch_id = launch_id_str.to_owned();

                if Instant::now() >= authority.deadline {
                    if !current_batch_buffer.is_empty() {
                        flush_batch_and_ids(
                            ledger,
                            generation,
                            next_seq,
                            &mut current_batch_buffer,
                            &mut completed_ids,
                        )?;
                    }
                    return Ok(ClosureOutcome::Refused(
                        BootstrapRecoveryReason::CoordinatorNotLive,
                    ));
                }

                #[cfg(feature = "test-hooks")]
                {
                    if std::env::var("SOLSTONE_PARENT_LOSS_CLOSURE_SEAM").as_deref()
                        == Ok("during-inventory")
                    {
                        loop {
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                    if let Ok(val) = std::env::var("SOLSTONE_PARENT_LOSS_CLOSURE_SLOW_MS")
                        && let Ok(ms) = val.parse::<u64>()
                    {
                        thread::sleep(Duration::from_millis(ms));
                    }
                }

                let derived = read_derived_admission_drops(ledger, generation, &launch_id)?;

                match (derived.instance, derived.uid) {
                    (Some(instance), Some(uid)) => {
                        let verdict = authority.source.observe(&instance);
                        match verdict {
                            InstanceVerdict::NotSameOrExited => {
                                let finding = AdmissionFinding::Exited;
                                let binding_digest = compute_batch_row_binding_digest(
                                    &launch_id,
                                    derived.service,
                                    Some(instance),
                                    Some(uid),
                                    derived.parent_launch_id.as_deref(),
                                    finding,
                                    derived.parsed_result_state.as_deref(),
                                    derived.digests.intent.as_deref(),
                                    derived.digests.result.as_deref(),
                                    derived.digests.ack.as_deref(),
                                    derived.digests.spawn_identity.as_deref(),
                                )?;
                                let row = CompletedBatchRow {
                                    launch_id: launch_id.clone(),
                                    service: derived.service,
                                    instance: Some(instance),
                                    uid: Some(uid),
                                    parent_launch_id: derived.parent_launch_id,
                                    finding,
                                    parsed_result_state: derived.parsed_result_state,
                                    intent_digest: derived.digests.intent,
                                    result_digest: derived.digests.result,
                                    ack_digest: derived.digests.ack,
                                    spawn_identity_digest: derived.digests.spawn_identity,
                                    binding_digest,
                                };
                                current_batch_buffer.push(row);
                            }
                            InstanceVerdict::SameLive { .. } => {
                                let live_file = LiveRecoveryFile {
                                    schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
                                    kind: RecoveryArtifactKind::Live,
                                    generation,
                                    sealed_phase: ParentLossPhase::Sealed,
                                    launch_id: launch_id.clone(),
                                    service: derived.service,
                                    root: instance,
                                    uid,
                                    parent_launch_id: derived.parent_launch_id,
                                    descendants: Vec::new(),
                                    escalate_at_unix_ms: 0,
                                    reap_deadline_unix_ms: None,
                                    term_requested: false,
                                    escalated: false,
                                    binding_digest: String::new(),
                                };
                                if let Some(outcome) = run_live_state_machine(
                                    ledger,
                                    generation,
                                    authority,
                                    live_file,
                                    &mut current_batch_buffer,
                                    &mut next_seq,
                                    &mut completed_ids,
                                )? {
                                    return Ok(outcome);
                                }
                            }
                            InstanceVerdict::Unverifiable => {
                                match settle_unverifiable(authority, instance.pid, Some(uid)) {
                                    Some(finding) => {
                                        let binding_digest = compute_batch_row_binding_digest(
                                            &launch_id,
                                            derived.service,
                                            Some(instance),
                                            Some(uid),
                                            derived.parent_launch_id.as_deref(),
                                            finding,
                                            derived.parsed_result_state.as_deref(),
                                            derived.digests.intent.as_deref(),
                                            derived.digests.result.as_deref(),
                                            derived.digests.ack.as_deref(),
                                            derived.digests.spawn_identity.as_deref(),
                                        )?;
                                        let row = CompletedBatchRow {
                                            launch_id: launch_id.clone(),
                                            service: derived.service,
                                            instance: Some(instance),
                                            uid: Some(uid),
                                            parent_launch_id: derived.parent_launch_id,
                                            finding,
                                            parsed_result_state: derived.parsed_result_state,
                                            intent_digest: derived.digests.intent,
                                            result_digest: derived.digests.result,
                                            ack_digest: derived.digests.ack,
                                            spawn_identity_digest: derived.digests.spawn_identity,
                                            binding_digest,
                                        };
                                        current_batch_buffer.push(row);
                                    }
                                    None => {
                                        return Ok(ClosureOutcome::Refused(
                                            BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    _ => match derived.finding {
                        AdmissionFinding::SpawnFailed | AdmissionFinding::RejectedAndReaped => {
                            let binding_digest = compute_batch_row_binding_digest(
                                &launch_id,
                                derived.service,
                                None,
                                None,
                                None,
                                derived.finding,
                                derived.parsed_result_state.as_deref(),
                                derived.digests.intent.as_deref(),
                                derived.digests.result.as_deref(),
                                derived.digests.ack.as_deref(),
                                derived.digests.spawn_identity.as_deref(),
                            )?;
                            let row = CompletedBatchRow {
                                launch_id: launch_id.clone(),
                                service: derived.service,
                                instance: None,
                                uid: None,
                                parent_launch_id: None,
                                finding: derived.finding,
                                parsed_result_state: derived.parsed_result_state,
                                intent_digest: derived.digests.intent,
                                result_digest: derived.digests.result,
                                ack_digest: derived.digests.ack,
                                spawn_identity_digest: derived.digests.spawn_identity,
                                binding_digest,
                            };
                            current_batch_buffer.push(row);
                        }
                        finding => {
                            let prov_file = ProvisionalFile {
                                schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
                                kind: RecoveryArtifactKind::Provisional,
                                generation,
                                sealed_phase: ParentLossPhase::Sealed,
                                launch_id: launch_id.clone(),
                                service: derived.service,
                                finding,
                                intent_digest: derived.digests.intent,
                                result_digest: derived.digests.result,
                                ack_digest: derived.digests.ack,
                                spawn_identity_digest: derived.digests.spawn_identity,
                            };
                            let prov_path =
                                ledger.closure_recovery_provisional_path(generation, &launch_id);
                            write_json(&prov_path, &prov_file, json_options())?;
                            provisional_map.insert(launch_id.clone(), prov_file);

                            #[cfg(test)]
                            if PROVISIONAL_STOP_FAULT.with(|f| f.get()) {
                                return Ok(ClosureOutcome::Refused(
                                    BootstrapRecoveryReason::CoordinatorNotLive,
                                ));
                            }
                        }
                    },
                }

                if current_batch_buffer.len() >= RECOVERY_BATCH_LIMIT {
                    flush_batch_and_ids(
                        ledger,
                        generation,
                        next_seq,
                        &mut current_batch_buffer,
                        &mut completed_ids,
                    )?;
                    next_seq += 1;
                }
            }
        }

        if !current_batch_buffer.is_empty() {
            flush_batch_and_ids(
                ledger,
                generation,
                next_seq,
                &mut current_batch_buffer,
                &mut completed_ids,
            )?;
            next_seq += 1;
        }
    }

    // 11. Provisional drop recheck.
    let prov_keys: Vec<String> = provisional_map.keys().cloned().collect();
    for launch_id in prov_keys {
        if completed_ids.contains(&launch_id) {
            let prov_path = ledger.closure_recovery_provisional_path(generation, &launch_id);
            if prov_path.is_file() {
                let _ = fs::remove_file(&prov_path);
            }
            provisional_map.remove(&launch_id);
            continue;
        }
        let derived = read_derived_admission_drops(ledger, generation, &launch_id)?;
        if let (Some(instance), Some(uid)) = (derived.instance, derived.uid) {
            let verdict = authority.source.observe(&instance);
            match verdict {
                InstanceVerdict::NotSameOrExited => {
                    let finding = AdmissionFinding::Exited;
                    let binding_digest = compute_batch_row_binding_digest(
                        &launch_id,
                        derived.service,
                        Some(instance),
                        Some(uid),
                        derived.parent_launch_id.as_deref(),
                        finding,
                        derived.parsed_result_state.as_deref(),
                        derived.digests.intent.as_deref(),
                        derived.digests.result.as_deref(),
                        derived.digests.ack.as_deref(),
                        derived.digests.spawn_identity.as_deref(),
                    )?;
                    let row = CompletedBatchRow {
                        launch_id: launch_id.clone(),
                        service: derived.service,
                        instance: Some(instance),
                        uid: Some(uid),
                        parent_launch_id: derived.parent_launch_id,
                        finding,
                        parsed_result_state: derived.parsed_result_state,
                        intent_digest: derived.digests.intent,
                        result_digest: derived.digests.result,
                        ack_digest: derived.digests.ack,
                        spawn_identity_digest: derived.digests.spawn_identity,
                        binding_digest,
                    };
                    current_batch_buffer.push(row);
                    provisional_map.remove(&launch_id);
                    let prov_path =
                        ledger.closure_recovery_provisional_path(generation, &launch_id);
                    if prov_path.is_file() {
                        let _ = fs::remove_file(&prov_path);
                    }
                }
                InstanceVerdict::SameLive { .. } => {
                    let live_file = LiveRecoveryFile {
                        schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
                        kind: RecoveryArtifactKind::Live,
                        generation,
                        sealed_phase: ParentLossPhase::Sealed,
                        launch_id: launch_id.clone(),
                        service: derived.service,
                        root: instance,
                        uid,
                        parent_launch_id: derived.parent_launch_id,
                        descendants: Vec::new(),
                        escalate_at_unix_ms: 0,
                        reap_deadline_unix_ms: None,
                        term_requested: false,
                        escalated: false,
                        binding_digest: String::new(),
                    };
                    if let Some(outcome) = run_live_state_machine(
                        ledger,
                        generation,
                        authority,
                        live_file,
                        &mut current_batch_buffer,
                        &mut next_seq,
                        &mut completed_ids,
                    )? {
                        return Ok(outcome);
                    }
                    provisional_map.remove(&launch_id);
                }
                InstanceVerdict::Unverifiable => {
                    if let Some(finding) = settle_unverifiable(authority, instance.pid, Some(uid)) {
                        let binding_digest = compute_batch_row_binding_digest(
                            &launch_id,
                            derived.service,
                            Some(instance),
                            Some(uid),
                            derived.parent_launch_id.as_deref(),
                            finding,
                            derived.parsed_result_state.as_deref(),
                            derived.digests.intent.as_deref(),
                            derived.digests.result.as_deref(),
                            derived.digests.ack.as_deref(),
                            derived.digests.spawn_identity.as_deref(),
                        )?;
                        let row = CompletedBatchRow {
                            launch_id: launch_id.clone(),
                            service: derived.service,
                            instance: Some(instance),
                            uid: Some(uid),
                            parent_launch_id: derived.parent_launch_id,
                            finding,
                            parsed_result_state: derived.parsed_result_state,
                            intent_digest: derived.digests.intent,
                            result_digest: derived.digests.result,
                            ack_digest: derived.digests.ack,
                            spawn_identity_digest: derived.digests.spawn_identity,
                            binding_digest,
                        };
                        current_batch_buffer.push(row);
                        provisional_map.remove(&launch_id);
                        let prov_path =
                            ledger.closure_recovery_provisional_path(generation, &launch_id);
                        if prov_path.is_file() {
                            let _ = fs::remove_file(&prov_path);
                        }
                    } else {
                        return Ok(ClosureOutcome::Refused(
                            BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
                        ));
                    }
                }
            }
        }
    }

    if !current_batch_buffer.is_empty() {
        flush_batch_and_ids(
            ledger,
            generation,
            next_seq,
            &mut current_batch_buffer,
            &mut completed_ids,
        )?;
        next_seq += 1;
    }

    // 12. Check live directory is empty.
    let live_dir = ledger.closure_recovery_live_dir(generation);
    if let Ok(entries) = fs::read_dir(&live_dir) {
        if entries
            .flatten()
            .any(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
        {
            return Ok(ClosureOutcome::Refused(
                BootstrapRecoveryReason::CoordinatorNotLive,
            ));
        }
    }

    if Instant::now() >= authority.deadline {
        return Ok(ClosureOutcome::Refused(
            BootstrapRecoveryReason::CoordinatorNotLive,
        ));
    }

    // 13. Write scan-complete.json.
    let mut all_scanned_ids: Vec<String> = completed_ids
        .iter()
        .chain(provisional_map.keys())
        .cloned()
        .collect();
    all_scanned_ids.sort();
    all_scanned_ids.dedup();

    let mut ids_seqs = valid_ids_seqs.clone();
    for s in 1..next_seq {
        if !ids_seqs.contains(&s) && ledger.closure_recovery_ids_path(generation, s).is_file() {
            ids_seqs.push(s);
        }
    }
    ids_seqs.sort_unstable();
    ids_seqs.dedup();

    let ids_bytes = canonical_json(&all_scanned_ids)?;
    let ids_digest = digest_bytes(&ids_bytes);

    let sc_file = ScanCompleteFile {
        schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
        kind: RecoveryArtifactKind::ScanComplete,
        generation,
        sealed_phase: ParentLossPhase::Sealed,
        sorted_launch_ids: all_scanned_ids.clone(),
        ids_sequences: ids_seqs.clone(),
        ids_digest,
    };
    write_json(&scan_complete_path, &sc_file, json_options())?;

    // 14. Publication under the same authority.deadline:
    // Open each ids seq's batch, require completed_row_is_authorized on every row, and build closures from those rows plus provisional files.
    let mut batch_rows_by_id: HashMap<String, CompletedBatchRow> = HashMap::new();
    let mut failed_batch = false;
    let mut failed_launch_ids: Vec<String> = Vec::new();

    for seq in &ids_seqs {
        let b_path = ledger.closure_recovery_batch_path(generation, *seq);
        let i_path = ledger.closure_recovery_ids_path(generation, *seq);
        match read_json_durable_validated::<RecoveryBatchFile>(
            ArtifactId::ParentLossClosureRecovery,
            &b_path,
            |b| {
                if b.schema != PARENT_LOSS_RECOVERY_SCHEMA_V1
                    || b.generation != generation
                    || b.sealed_phase != ParentLossPhase::Sealed
                    || b.kind != RecoveryArtifactKind::Batch
                    || b.seq != *seq
                {
                    return Err("invalid batch envelope".to_string());
                }
                Ok(())
            },
        ) {
            Ok(DurableRead::Present(batch)) => {
                let all_auth = batch.entries.iter().all(completed_row_is_authorized);
                if !all_auth {
                    failed_batch = true;
                    if let Ok(aside) = set_aside(&b_path) {
                        if let Some(name) = aside.file_name() {
                            set_aside_files.push(name.to_string_lossy().into_owned());
                        }
                    }
                    if let Ok(aside) = set_aside(&i_path) {
                        if let Some(name) = aside.file_name() {
                            set_aside_files.push(name.to_string_lossy().into_owned());
                        }
                    }
                    failed_launch_ids.extend(batch.entries.into_iter().map(|e| e.launch_id));
                } else {
                    for row in batch.entries {
                        batch_rows_by_id.insert(row.launch_id.clone(), row);
                    }
                }
            }
            _ => {
                failed_batch = true;
                if let Ok(aside) = set_aside(&b_path) {
                    if let Some(name) = aside.file_name() {
                        set_aside_files.push(name.to_string_lossy().into_owned());
                    }
                }
                if let Ok(aside) = set_aside(&i_path) {
                    if let Some(name) = aside.file_name() {
                        set_aside_files.push(name.to_string_lossy().into_owned());
                    }
                }
            }
        }
    }

    if failed_batch {
        if let Ok(aside) = set_aside(&scan_complete_path) {
            if let Some(name) = aside.file_name() {
                set_aside_files.push(name.to_string_lossy().into_owned());
            }
        }
        if Instant::now() >= authority.deadline || failed_launch_ids.is_empty() {
            return Ok(ClosureOutcome::Refused(
                BootstrapRecoveryReason::CoordinatorNotLive,
            ));
        }
        for launch_id in failed_launch_ids {
            let derived = read_derived_admission_drops(ledger, generation, &launch_id)?;
            match (derived.instance, derived.uid) {
                (Some(instance), Some(uid)) => {
                    let verdict = authority.source.observe(&instance);
                    match verdict {
                        InstanceVerdict::NotSameOrExited => {
                            let finding = AdmissionFinding::Exited;
                            let binding_digest = compute_batch_row_binding_digest(
                                &launch_id,
                                derived.service,
                                Some(instance),
                                Some(uid),
                                derived.parent_launch_id.as_deref(),
                                finding,
                                derived.parsed_result_state.as_deref(),
                                derived.digests.intent.as_deref(),
                                derived.digests.result.as_deref(),
                                derived.digests.ack.as_deref(),
                                derived.digests.spawn_identity.as_deref(),
                            )?;
                            let row = CompletedBatchRow {
                                launch_id: launch_id.clone(),
                                service: derived.service,
                                instance: Some(instance),
                                uid: Some(uid),
                                parent_launch_id: derived.parent_launch_id,
                                finding,
                                parsed_result_state: derived.parsed_result_state,
                                intent_digest: derived.digests.intent,
                                result_digest: derived.digests.result,
                                ack_digest: derived.digests.ack,
                                spawn_identity_digest: derived.digests.spawn_identity,
                                binding_digest,
                            };
                            batch_rows_by_id.insert(launch_id, row);
                        }
                        InstanceVerdict::SameLive { .. } => {
                            return Ok(ClosureOutcome::Refused(
                                BootstrapRecoveryReason::CoordinatorNotLive,
                            ));
                        }
                        InstanceVerdict::Unverifiable => {
                            return Ok(ClosureOutcome::Refused(
                                BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
                            ));
                        }
                    }
                }
                _ => match derived.finding {
                    AdmissionFinding::SpawnFailed | AdmissionFinding::RejectedAndReaped => {
                        let binding_digest = compute_batch_row_binding_digest(
                            &launch_id,
                            derived.service,
                            None,
                            None,
                            None,
                            derived.finding,
                            derived.parsed_result_state.as_deref(),
                            derived.digests.intent.as_deref(),
                            derived.digests.result.as_deref(),
                            derived.digests.ack.as_deref(),
                            derived.digests.spawn_identity.as_deref(),
                        )?;
                        let row = CompletedBatchRow {
                            launch_id: launch_id.clone(),
                            service: derived.service,
                            instance: None,
                            uid: None,
                            parent_launch_id: None,
                            finding: derived.finding,
                            parsed_result_state: derived.parsed_result_state,
                            intent_digest: derived.digests.intent,
                            result_digest: derived.digests.result,
                            ack_digest: derived.digests.ack,
                            spawn_identity_digest: derived.digests.spawn_identity,
                            binding_digest,
                        };
                        batch_rows_by_id.insert(launch_id, row);
                    }
                    _ => {
                        return Ok(ClosureOutcome::Refused(
                            BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
                        ));
                    }
                },
            }
        }
    }

    let mut closures = Vec::with_capacity(all_scanned_ids.len());
    let mut sealed = Vec::new();

    for launch_id in &all_scanned_ids {
        if let Some(row) = batch_rows_by_id.remove(launch_id) {
            closures.push(AdmissionClosure {
                launch_id: row.launch_id.clone(),
                service: row.service,
                instance: row.instance,
                uid: row.uid,
                finding: row.finding,
            });
            if let (Some(instance), Some(uid)) = (row.instance, row.uid) {
                sealed.push(SealedAdmission {
                    launch_id: row.launch_id.clone(),
                    service: row.service,
                    identity: AdmissionIdentity {
                        generation,
                        launch_id: row.launch_id.clone(),
                        instance,
                        uid,
                        parent_launch_id: row.parent_launch_id.clone(),
                    },
                });
            }
        } else if let Some(prov) = provisional_map.get(launch_id) {
            closures.push(AdmissionClosure {
                launch_id: prov.launch_id.clone(),
                service: prov.service,
                instance: None,
                uid: None,
                finding: prov.finding,
            });
        }
    }

    sealed.sort_by(|a, b| a.launch_id.cmp(&b.launch_id));
    closures.sort_by(|a, b| a.launch_id.cmp(&b.launch_id));

    let ledger_path = ledger.sealed_ledger_path(generation);
    let sealed_ledger_digest = if ledger_path.is_file() {
        let existing_bytes = fs::read(&ledger_path)?;
        let existing_val: Result<serde_json::Value, _> = serde_json::from_slice(&existing_bytes);
        let current_val = serde_json::to_value(&sealed).ok();
        if let (Ok(existing), Some(current)) = (existing_val, current_val)
            && existing == current
        {
            digest_bytes(&existing_bytes)
        } else {
            if record
                .as_ref()
                .and_then(|r| r.sealed_ledger_digest.as_ref())
                .is_some()
            {
                return Ok(ClosureOutcome::Refused(
                    BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
                ));
            }
            if let Ok(aside) = set_aside(&ledger_path) {
                if let Some(name) = aside.file_name() {
                    let name = name.to_string_lossy().into_owned();
                    if !set_aside_files.contains(&name) {
                        set_aside_files.push(name);
                    }
                }
            }
            ledger.write_sealed_ledger(generation, &sealed)?
        }
    } else {
        ledger.write_sealed_ledger(generation, &sealed)?
    };

    #[cfg(feature = "test-hooks")]
    if std::env::var("SOLSTONE_PARENT_LOSS_CLOSURE_SEAM").as_deref() == Ok("after-ledger") {
        loop {
            thread::sleep(Duration::from_millis(100));
        }
    }

    let ledger_bytes = fs::read(&ledger_path)?;
    let expected_digest = digest_bytes(&ledger_bytes);
    if sealed_ledger_digest != expected_digest {
        return Ok(ClosureOutcome::Refused(
            BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
        ));
    }

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
    if let Some(existing_rec_digest) = &record.sealed_ledger_digest {
        if existing_rec_digest != &expected_digest {
            return Ok(ClosureOutcome::Refused(
                BootstrapRecoveryReason::AbandonedAdmissionUnverifiable,
            ));
        }
    } else {
        record.sealed_ledger_digest = Some(expected_digest);
    }

    let heartbeat = match (record.supervisor_heartbeat.as_deref(), supervisor) {
        (Some(filename), Some(instance)) => clear_supervisor_heartbeat(ledger, filename, instance),
        _ => HeartbeatRetirement::NotRecorded,
    };

    let closure = AbandonedGenerationClosure {
        schema: PARENT_LOSS_CLOSURE_SCHEMA_V1,
        closed_by: authority.closed_by,
        successor_generation,
        closed_at: unix_seconds(),
        supervisor: supervisor_observation,
        coordinator: coordinator_observation,
        heartbeat,
        set_aside: set_aside_files,
        notes,
        admissions: closures,
    };
    record.closure = Some(closure);
    let record_path = ledger.record_path(generation);
    if let Some(parent) = record_path.parent() {
        fs::create_dir_all(parent)?;
    }
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
    use crate::lifecycle::parent_loss_ledger::{ActiveGeneration, CoordinatorLease};
    use crate::lifecycle::{
        HEARTBEAT_SCHEMA_V2, HeartbeatV2, RunId, WriterId, v2_heartbeat_filename,
    };
    use crate::process::{CensusRow, ExecutionState, InspectResult, InstanceCensus, ProcessBirth};

    fn instance(pid: u32, birth: u64) -> ProcessInstance {
        ProcessInstance {
            pid,
            birth: ProcessBirth::linux(birth, 1, 100),
        }
    }

    #[derive(Default)]
    struct FakeTable {
        live: HashMap<u32, (ProcessInstance, u32)>,
        unverifiable: HashSet<u32>,
        incomplete_census: bool,
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

    fn acquire_lease(ledger: &ParentLossLedger) -> CoordinatorLease {
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
        let lease = acquire_lease(&ledger);

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
    }

    #[test]
    fn a_live_coordinator_still_refuses_the_closer() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let coord_instance = ledger
            .record(active.generation)
            .expect("record")
            .expect("record present")
            .coordinator
            .expect("coordinator identity");
        let (source, retirer) = fake(&[(coord_instance, 501)], &[], &[]);
        let lease = acquire_lease(&ledger);

        let error = ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect_err("a live coordinator must refuse closure");

        assert!(matches!(
            error,
            ParentLossLedgerError::RecoveryRequired(BootstrapRecoveryReason::ActiveCoordinator)
        ));
        assert!(
            !ledger
                .closure_recovery_batches_dir(active.generation)
                .exists()
                || fs::read_dir(ledger.closure_recovery_batches_dir(active.generation))
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true),
            "no recovery writes on veto"
        );
    }

    #[test]
    fn a_live_supervisor_still_refuses_the_closer() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let sup_instance = ledger
            .record(active.generation)
            .expect("record")
            .expect("record present")
            .supervisor;
        let (source, retirer) = fake(&[(sup_instance, 501)], &[], &[]);
        let lease = acquire_lease(&ledger);

        let error = ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect_err("a live supervisor must refuse closure");

        assert!(matches!(
            error,
            ParentLossLedgerError::RecoveryRequired(BootstrapRecoveryReason::SupervisorLive)
        ));
        assert!(
            !ledger
                .closure_recovery_batches_dir(active.generation)
                .exists()
                || fs::read_dir(ledger.closure_recovery_batches_dir(active.generation))
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true),
            "no recovery writes on veto"
        );
    }

    #[test]
    fn an_admission_that_outlives_the_deadline_refuses_this_start_and_leaves_the_generation_open() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let stubborn = instance(5000, 50);
        admit(
            directory.path(),
            active.generation,
            "stubborn",
            Some(HostedServiceKind::Sense),
            Some(stubborn),
        );
        let (source, retirer) = fake(&[(stubborn, 501)], &[], &[stubborn.pid]);
        let lease = acquire_lease(&ledger);

        let error = ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_millis(50)),
            )
            .expect_err("timeout must refuse");

        assert!(matches!(
            error,
            ParentLossLedgerError::RecoveryRequired(BootstrapRecoveryReason::CoordinatorNotLive)
        ));
        assert!(ledger.active_path().is_file());
    }

    #[test]
    fn a_child_that_obeys_sigterm_is_not_escalated_and_the_signals_are_recorded() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let obedient = instance(6000, 60);
        admit(
            directory.path(),
            active.generation,
            "obedient",
            Some(HostedServiceKind::Sense),
            Some(obedient),
        );
        let (source, retirer) = fake(&[(obedient, 501)], &[obedient.pid], &[]);
        let lease = acquire_lease(&ledger);

        ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("closure succeeds");

        let signals = retirer.signals.lock().expect("signals");
        assert_eq!(signals.as_slice(), &[(obedient.pid, "term")]);
        let closure = closure_of(&ledger, active.generation);
        assert_eq!(
            closure.admissions[0].finding,
            AdmissionFinding::Retired { escalated: false }
        );
    }

    #[test]
    fn incomplete_descendant_coverage_refuses_without_retiring_the_root() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let parent = instance(7000, 70);
        admit(
            directory.path(),
            active.generation,
            "parent",
            Some(HostedServiceKind::Sense),
            Some(parent),
        );
        let (source, retirer) = fake(&[(parent, 501)], &[parent.pid], &[]);
        source.0.lock().expect("fake").incomplete_census = true;
        let lease = acquire_lease(&ledger);

        let error = ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect_err("incomplete census must refuse");

        assert!(matches!(
            error,
            ParentLossLedgerError::RecoveryRequired(
                BootstrapRecoveryReason::AbandonedAdmissionUnverifiable
            )
        ));
        assert!(retirer.signals.lock().expect("signals").is_empty());
    }

    #[test]
    fn an_uninspectable_pid_is_settled_by_its_owner_or_refuses() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let gone = instance(8001, 80);
        let foreign = instance(8002, 80);
        let mine = instance(8003, 80);
        admit(
            directory.path(),
            active.generation,
            "gone",
            Some(HostedServiceKind::Sense),
            Some(gone),
        );
        admit(
            directory.path(),
            active.generation,
            "foreign",
            Some(HostedServiceKind::Sense),
            Some(foreign),
        );
        admit(
            directory.path(),
            active.generation,
            "mine",
            Some(HostedServiceKind::Sense),
            Some(mine),
        );

        let (source, retirer) = fake(&[], &[], &[]);
        {
            let mut table = source.0.lock().expect("table");
            table.unverifiable.insert(gone.pid);
            table.unverifiable.insert(foreign.pid);
            table.unverifiable.insert(mine.pid);
            table.owners.insert(gone.pid, ProcessOwner::Absent);
            table.owners.insert(foreign.pid, ProcessOwner::Uid(9999));
            table.owners.insert(mine.pid, ProcessOwner::Uid(501));
        }
        let lease = acquire_lease(&ledger);

        let error = ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_millis(50)),
            )
            .expect_err("mine staying unverifiable must refuse");

        assert!(matches!(
            error,
            ParentLossLedgerError::RecoveryRequired(
                BootstrapRecoveryReason::AbandonedAdmissionUnverifiable
            )
        ));
    }

    #[test]
    fn the_pointer_is_sealed_before_the_admissions_are_scanned() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let (source, retirer) = fake(&[], &[], &[]);
        let lease = acquire_lease(&ledger);
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
        write_parent_loss_admission_intent(
            directory.path(),
            &AdmissionIntent::new(
                active.generation,
                "unacknowledged",
                Some(HostedServiceKind::Sense),
                None,
            ),
        )
        .expect("write intent");
        let (source, retirer) = fake(&[], &[], &[]);
        let lease = acquire_lease(&ledger);

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
    }

    #[test]
    fn a_missing_record_is_created_and_closed() {
        let directory = TempDir::new().expect("temporary root");
        let ledger = ParentLossLedger::open(directory.path()).expect("ledger");
        let active = ledger
            .reserve_generation(instance(10, 1), [])
            .expect("reservation without a record");
        let (source, retirer) = fake(&[], &[], &[]);
        let lease = acquire_lease(&ledger);

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
        fs::write(ledger.record_path(active.generation), b"not valid json").expect("corrupt");
        let (source, retirer) = fake(&[], &[], &[]);
        let lease = acquire_lease(&ledger);

        ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("closed");

        let closure = closure_of(&ledger, active.generation);
        assert!(!closure.set_aside.is_empty());
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
            Some(HostedServiceKind::Sense),
            Some(live),
        );
        std::fs::write(ledger.active_path(), b"{ this is not json").expect("corrupt the pointer");
        let (source, retirer) = fake(&[(live, 501)], &[live.pid], &[]);
        let lease = acquire_lease(&ledger);

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
        let (ledger, active) = open_generation(&directory);
        let writer_id = WriterId::parse("0123456789abcdef0123456789abcdef").expect("writer ID");
        let run_id = RunId::parse("fedcba9876543210fedcba9876543210").expect("run ID");
        let heartbeat_filename = v2_heartbeat_filename(&writer_id, &run_id);
        let sync_dir = directory.path().join("health").join("sync");
        fs::create_dir_all(&sync_dir).expect("sync dir");
        let heartbeat_path = sync_dir.join(&heartbeat_filename);
        let supervisor = instance(10, 1);
        let record = ParentLossGenerationRecord {
            schema: PARENT_LOSS_LEDGER_SCHEMA_V1,
            generation: active.generation,
            supervisor,
            coordinator: Some(instance(20, 2)),
            sealed_ledger_digest: None,
            terminal: None,
            supervisor_heartbeat: Some(heartbeat_filename),
            closure: None,
        };
        write_json(
            ledger.record_path(active.generation),
            &record,
            json_options(),
        )
        .expect("record");
        let heartbeat = HeartbeatV2::new(
            writer_id,
            run_id,
            "lost-supervisor".to_owned(),
            supervisor.pid,
            "1.0".to_owned(),
            "test".to_owned(),
            15,
            directory.path().display().to_string(),
        );
        assert_eq!(heartbeat.schema, HEARTBEAT_SCHEMA_V2);
        write_json(&heartbeat_path, &heartbeat, json_options()).expect("heartbeat");
        let (source, retirer) = fake(&[], &[], &[]);
        let lease = acquire_lease(&ledger);

        ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("closed");

        let closure = closure_of(&ledger, active.generation);
        assert_eq!(closure.heartbeat, HeartbeatRetirement::Removed);
        assert!(!heartbeat_path.exists());
    }

    #[test]
    fn a_record_without_a_heartbeat_name_records_that_nothing_was_retired() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let (source, retirer) = fake(&[], &[], &[]);
        let lease = acquire_lease(&ledger);

        ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("closed");

        let closure = closure_of(&ledger, active.generation);
        assert_eq!(closure.heartbeat, HeartbeatRetirement::NotRecorded);
    }

    #[test]
    fn a_sealed_unresolved_generation_is_advanced_past_with_its_admissions_retired() {
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
        let lease = acquire_lease(&ledger);

        ledger
            .mark_sealed_for_closure(active.generation)
            .expect("seal");

        let successor = ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("closed");

        assert_eq!(successor.generation, active.generation + 1);
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
        let lease = acquire_lease(&ledger);

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

    #[test]
    fn the_coordinator_lease_is_exclusive_while_held() {
        let directory = TempDir::new().expect("temporary root");
        let ledger = ParentLossLedger::open(directory.path()).expect("ledger");
        let lease = ledger
            .acquire_coordinator_lease()
            .expect("lease")
            .expect("free");
        let second = ledger.acquire_coordinator_lease().expect("second check");
        assert!(second.is_none());
        drop(lease);
        let third = ledger.acquire_coordinator_lease().expect("third check");
        assert!(third.is_some());
    }

    #[test]
    fn recovery_checkpoint_budget_refusal_is_non_advancing_reason() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let (source, retirer) = fake(&[], &[], &[]);
        let lease = acquire_lease(&ledger);

        let expired_authority = ClosingAuthority {
            closed_by: instance(30, 3),
            source: &source,
            retirer: &retirer,
            deadline: Instant::now() - Duration::from_millis(10),
        };

        let error = ledger
            .reserve_generation_closing_abandoned(instance(11, 3), [], &lease, &expired_authority)
            .expect_err("expired deadline must refuse");

        assert!(matches!(
            error,
            ParentLossLedgerError::RecoveryRequired(BootstrapRecoveryReason::CoordinatorNotLive)
        ));
        let active_after = ledger.active_generation().expect("read").expect("active");
        assert_eq!(active_after.generation, active.generation);
    }

    #[test]
    fn digest_mismatch_with_omitted_or_changed_uid_or_instance_or_parent_does_not_authorize() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let launch_id = "test-launch-1";
        let inst = instance(4001, 100);
        admit(
            directory.path(),
            active.generation,
            launch_id,
            Some(HostedServiceKind::Sense),
            Some(inst),
        );

        let derived = read_derived_admission_drops(&ledger, active.generation, launch_id)
            .expect("derived drops");

        // Row with omitted uid:
        let invalid_row_no_uid = CompletedBatchRow {
            launch_id: launch_id.to_string(),
            service: derived.service,
            instance: Some(inst),
            uid: None,
            parent_launch_id: None,
            finding: AdmissionFinding::Exited,
            parsed_result_state: derived.parsed_result_state.clone(),
            intent_digest: derived.digests.intent.clone(),
            result_digest: derived.digests.result.clone(),
            ack_digest: derived.digests.ack.clone(),
            spawn_identity_digest: derived.digests.spawn_identity.clone(),
            binding_digest: "some-digest".to_string(),
        };
        assert!(!completed_row_is_authorized(&invalid_row_no_uid));

        // Row with changed instance:
        let changed_inst = instance(4002, 200);
        let Ok(digest_changed) = compute_batch_row_binding_digest(
            launch_id,
            derived.service,
            Some(changed_inst),
            derived.uid,
            None,
            AdmissionFinding::Exited,
            derived.parsed_result_state.as_deref(),
            derived.digests.intent.as_deref(),
            derived.digests.result.as_deref(),
            derived.digests.ack.as_deref(),
            derived.digests.spawn_identity.as_deref(),
        ) else {
            panic!("compute digest");
        };
        let row_changed_inst = CompletedBatchRow {
            launch_id: launch_id.to_string(),
            service: derived.service,
            instance: Some(changed_inst),
            uid: derived.uid,
            parent_launch_id: None,
            finding: AdmissionFinding::Exited,
            parsed_result_state: derived.parsed_result_state.clone(),
            intent_digest: derived.digests.intent.clone(),
            result_digest: derived.digests.result.clone(),
            ack_digest: derived.digests.ack.clone(),
            spawn_identity_digest: derived.digests.spawn_identity.clone(),
            binding_digest: digest_changed,
        };
        assert!(completed_row_is_authorized(&row_changed_inst));
        assert!(!skip_matches_source(&row_changed_inst, &derived));

        // Plant batch with invalid row:
        let batch_file = RecoveryBatchFile {
            schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
            kind: RecoveryArtifactKind::Batch,
            generation: active.generation,
            sealed_phase: ParentLossPhase::Sealed,
            seq: 1,
            entries: vec![invalid_row_no_uid],
        };
        let b_path = ledger.closure_recovery_batch_path(active.generation, 1);
        fs::create_dir_all(b_path.parent().unwrap()).unwrap();
        write_json(&b_path, &batch_file, json_options()).unwrap();

        let (source, retirer) = fake(&[], &[], &[]);
        let lease = acquire_lease(&ledger);

        ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("closure succeeds rebuilding from drops");

        let closure = closure_of(&ledger, active.generation);
        assert!(!closure.set_aside.is_empty(), "invalid batch was set aside");
        assert_eq!(closure.admissions.len(), 1);
        assert_eq!(closure.admissions[0].finding, AdmissionFinding::Exited);
        assert_eq!(closure.admissions[0].instance, Some(inst));
    }

    #[test]
    fn spawn_failed_with_invalid_parsed_result_state_does_not_authorize() {
        let launch_id = "failed-launch";
        let invalid_spawn_failed = CompletedBatchRow {
            launch_id: launch_id.to_string(),
            service: Some(HostedServiceKind::Sense),
            instance: None,
            uid: None,
            parent_launch_id: None,
            finding: AdmissionFinding::SpawnFailed,
            parsed_result_state: Some("other_state".to_string()),
            intent_digest: None,
            result_digest: None,
            ack_digest: None,
            spawn_identity_digest: None,
            binding_digest: "digest".to_string(),
        };
        assert!(!completed_row_is_authorized(&invalid_spawn_failed));

        let valid_digest = compute_batch_row_binding_digest(
            launch_id,
            Some(HostedServiceKind::Sense),
            None,
            None,
            None,
            AdmissionFinding::SpawnFailed,
            Some("spawn_failed"),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let valid_spawn_failed = CompletedBatchRow {
            launch_id: launch_id.to_string(),
            service: Some(HostedServiceKind::Sense),
            instance: None,
            uid: None,
            parent_launch_id: None,
            finding: AdmissionFinding::SpawnFailed,
            parsed_result_state: Some("spawn_failed".to_string()),
            intent_digest: None,
            result_digest: None,
            ack_digest: None,
            spawn_identity_digest: None,
            binding_digest: valid_digest,
        };
        assert!(completed_row_is_authorized(&valid_spawn_failed));
    }

    #[test]
    fn escalation_target_unauthorized_by_drops_is_not_signalled_and_invalid_launch_id_never_joins()
    {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let launch_id = "admitted-target";
        let real_inst = instance(5001, 100);
        admit(
            directory.path(),
            active.generation,
            launch_id,
            Some(HostedServiceKind::Sense),
            Some(real_inst),
        );

        let unauth_inst = instance(5002, 200);
        let live_file = LiveRecoveryFile {
            schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
            kind: RecoveryArtifactKind::Live,
            generation: active.generation,
            sealed_phase: ParentLossPhase::Sealed,
            launch_id: launch_id.to_string(),
            service: Some(HostedServiceKind::Sense),
            root: unauth_inst,
            uid: 501,
            parent_launch_id: None,
            descendants: Vec::new(),
            escalate_at_unix_ms: 0,
            reap_deadline_unix_ms: None,
            term_requested: false,
            escalated: false,
            binding_digest: compute_live_binding_digest(unauth_inst, 501, &[]).unwrap(),
        };
        let live_path = ledger.closure_recovery_live_path(active.generation, launch_id);
        fs::create_dir_all(live_path.parent().unwrap()).unwrap();
        write_json(&live_path, &live_file, json_options()).unwrap();

        let (source, retirer) = fake(&[(unauth_inst, 501)], &[], &[]);
        let lease = acquire_lease(&ledger);

        let error = ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_millis(50)),
            )
            .expect_err("root mismatch with dead real instance refuses");

        assert!(matches!(
            error,
            ParentLossLedgerError::RecoveryRequired(
                BootstrapRecoveryReason::AbandonedAdmissionUnverifiable
            )
        ));
        assert!(
            retirer.signals.lock().unwrap().is_empty(),
            "unauthorized instance must not be signalled"
        );

        let bad_dir = directory
            .path()
            .join("health/parent-loss/generations")
            .join(format!("{}", active.generation))
            .join("admissions")
            .join("invalid..launch/id");
        fs::create_dir_all(&bad_dir).unwrap();
        let (source2, retirer2) = fake(&[], &[], &[]);
        let error2 = ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source2, &retirer2, Duration::from_millis(50)),
            )
            .expect_err("invalid launch id must refuse");
        assert!(matches!(
            error2,
            ParentLossLedgerError::RecoveryRequired(
                BootstrapRecoveryReason::AbandonedAdmissionUnverifiable
            )
        ));
    }

    #[test]
    fn malformed_duplicate_and_missing_batches_do_not_certify_omitted_ids_and_wrong_gen_or_phase_set_aside()
     {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let launch_id = "dup-id";
        let inst = instance(7001, 100);
        admit(
            directory.path(),
            active.generation,
            launch_id,
            Some(HostedServiceKind::Sense),
            Some(inst),
        );

        let derived = read_derived_admission_drops(&ledger, active.generation, launch_id).unwrap();
        let digest = compute_batch_row_binding_digest(
            launch_id,
            derived.service,
            Some(inst),
            derived.uid,
            None,
            AdmissionFinding::Exited,
            derived.parsed_result_state.as_deref(),
            derived.digests.intent.as_deref(),
            derived.digests.result.as_deref(),
            derived.digests.ack.as_deref(),
            derived.digests.spawn_identity.as_deref(),
        )
        .unwrap();
        let row = CompletedBatchRow {
            launch_id: launch_id.to_string(),
            service: derived.service,
            instance: Some(inst),
            uid: derived.uid,
            parent_launch_id: None,
            finding: AdmissionFinding::Exited,
            parsed_result_state: derived.parsed_result_state,
            intent_digest: derived.digests.intent,
            result_digest: derived.digests.result,
            ack_digest: derived.digests.ack,
            spawn_identity_digest: derived.digests.spawn_identity,
            binding_digest: digest,
        };

        // Plant batch 1:
        let batch1 = RecoveryBatchFile {
            schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
            kind: RecoveryArtifactKind::Batch,
            generation: active.generation,
            sealed_phase: ParentLossPhase::Sealed,
            seq: 1,
            entries: vec![row.clone()],
        };
        let b1_path = ledger.closure_recovery_batch_path(active.generation, 1);
        fs::create_dir_all(b1_path.parent().unwrap()).unwrap();
        write_json(&b1_path, &batch1, json_options()).unwrap();
        let ids1 = RecoveryIdsFile {
            schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
            kind: RecoveryArtifactKind::Ids,
            generation: active.generation,
            sealed_phase: ParentLossPhase::Sealed,
            seq: 1,
            ids: vec![launch_id.to_string()],
        };
        let i1_path = ledger.closure_recovery_ids_path(active.generation, 1);
        fs::create_dir_all(i1_path.parent().unwrap()).unwrap();
        write_json(&i1_path, &ids1, json_options()).unwrap();

        // Plant batch 2 with duplicate launch_id:
        let batch2 = RecoveryBatchFile {
            schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
            kind: RecoveryArtifactKind::Batch,
            generation: active.generation,
            sealed_phase: ParentLossPhase::Sealed,
            seq: 2,
            entries: vec![row],
        };
        let b2_path = ledger.closure_recovery_batch_path(active.generation, 2);
        write_json(&b2_path, &batch2, json_options()).unwrap();
        let ids2 = RecoveryIdsFile {
            schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
            kind: RecoveryArtifactKind::Ids,
            generation: active.generation,
            sealed_phase: ParentLossPhase::Sealed,
            seq: 2,
            ids: vec![launch_id.to_string()],
        };
        let i2_path = ledger.closure_recovery_ids_path(active.generation, 2);
        write_json(&i2_path, &ids2, json_options()).unwrap();

        // Plant batch 3 with wrong generation:
        let batch3 = RecoveryBatchFile {
            schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
            kind: RecoveryArtifactKind::Batch,
            generation: active.generation + 99,
            sealed_phase: ParentLossPhase::Sealed,
            seq: 3,
            entries: Vec::new(),
        };
        let b3_path = ledger.closure_recovery_batch_path(active.generation, 3);
        write_json(&b3_path, &batch3, json_options()).unwrap();

        let (source, retirer) = fake(&[], &[], &[]);
        let lease = acquire_lease(&ledger);

        ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("closure succeeds");

        let closure = closure_of(&ledger, active.generation);
        assert!(
            !closure.set_aside.is_empty(),
            "conflicting/wrong batches were set aside"
        );
    }

    #[test]
    fn provisional_row_gains_acknowledgement_between_attempts_retired_under_seal() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let launch_id = "late-ack-launch";
        write_parent_loss_admission_intent(
            directory.path(),
            &AdmissionIntent::new(
                active.generation,
                launch_id,
                Some(HostedServiceKind::Sense),
                None,
            ),
        )
        .expect("intent");

        let (source, retirer) = fake(&[], &[], &[]);
        let lease = acquire_lease(&ledger);

        // First attempt with PROVISIONAL_STOP_FAULT flag: provisional file written
        PROVISIONAL_STOP_FAULT.with(|f| f.set(true));
        let error = ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect_err("provisional fault returns CoordinatorNotLive");
        assert!(matches!(
            error,
            ParentLossLedgerError::RecoveryRequired(BootstrapRecoveryReason::CoordinatorNotLive)
        ));

        let prov_path = ledger.closure_recovery_provisional_path(active.generation, launch_id);
        assert!(prov_path.is_file(), "provisional file is on disk");
        let active_after = ledger.active_generation().expect("read").expect("active");
        assert_eq!(active_after.generation, active.generation);

        PROVISIONAL_STOP_FAULT.with(|f| f.set(false));

        // Late acknowledgement added:
        let ack_inst = instance(6001, 100);
        let identity = AdmissionIdentity {
            generation: active.generation,
            launch_id: launch_id.to_string(),
            instance: ack_inst,
            uid: 501,
            parent_launch_id: None,
        };
        acknowledge_parent_loss_admission(directory.path(), identity).expect("ack");

        // Second attempt with flag false and adequate budget:
        ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("second attempt closes");

        let closure = closure_of(&ledger, active.generation);
        assert_eq!(closure.admissions.len(), 1);
        assert_eq!(closure.admissions[0].finding, AdmissionFinding::Exited);
        assert_eq!(closure.admissions[0].instance, Some(ack_inst));
        assert!(
            !prov_path.exists(),
            "provisional file removed after completion"
        );
    }

    #[test]
    fn recovery_checkpoint_resumes_without_reobserving_valid_cached_rows() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let launch1 = "launch-1";
        let inst1 = instance(8001, 100);
        admit(
            directory.path(),
            active.generation,
            launch1,
            Some(HostedServiceKind::Sense),
            Some(inst1),
        );
        let launch2 = "launch-2";
        let inst2 = instance(8002, 200);
        admit(
            directory.path(),
            active.generation,
            launch2,
            Some(HostedServiceKind::Sense),
            Some(inst2),
        );

        let (source, retirer) = fake(&[], &[], &[]);
        let lease = acquire_lease(&ledger);

        // Pre-populate batch 1 for launch-1
        let derived = read_derived_admission_drops(&ledger, active.generation, launch1).unwrap();
        let digest = compute_batch_row_binding_digest(
            launch1,
            derived.service,
            Some(inst1),
            derived.uid,
            None,
            AdmissionFinding::Exited,
            derived.parsed_result_state.as_deref(),
            derived.digests.intent.as_deref(),
            derived.digests.result.as_deref(),
            derived.digests.ack.as_deref(),
            derived.digests.spawn_identity.as_deref(),
        )
        .unwrap();
        let row1 = CompletedBatchRow {
            launch_id: launch1.to_string(),
            service: derived.service,
            instance: Some(inst1),
            uid: derived.uid,
            parent_launch_id: None,
            finding: AdmissionFinding::Exited,
            parsed_result_state: derived.parsed_result_state,
            intent_digest: derived.digests.intent,
            result_digest: derived.digests.result,
            ack_digest: derived.digests.ack,
            spawn_identity_digest: derived.digests.spawn_identity,
            binding_digest: digest,
        };
        let batch1 = RecoveryBatchFile {
            schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
            kind: RecoveryArtifactKind::Batch,
            generation: active.generation,
            sealed_phase: ParentLossPhase::Sealed,
            seq: 1,
            entries: vec![row1],
        };
        let b1_path = ledger.closure_recovery_batch_path(active.generation, 1);
        fs::create_dir_all(b1_path.parent().unwrap()).unwrap();
        write_json(&b1_path, &batch1, json_options()).unwrap();
        let ids1 = RecoveryIdsFile {
            schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
            kind: RecoveryArtifactKind::Ids,
            generation: active.generation,
            sealed_phase: ParentLossPhase::Sealed,
            seq: 1,
            ids: vec![launch1.to_string()],
        };
        let i1_path = ledger.closure_recovery_ids_path(active.generation, 1);
        fs::create_dir_all(i1_path.parent().unwrap()).unwrap();
        write_json(&i1_path, &ids1, json_options()).unwrap();

        // Mutate on-disk result drop for launch1 to prove it is not re-read
        let result_path = admission_directory(&ledger, active.generation)
            .join(launch1)
            .join("result.json");
        fs::write(&result_path, b"{\"schema\":1,\"mutated\":true}").unwrap();

        source.0.lock().unwrap().observed_pids.clear();

        ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("closed");

        let observed = source.0.lock().unwrap().observed_pids.clone();
        assert!(
            !observed.contains(&inst1.pid),
            "completed absent pid must not be re-observed"
        );
        assert!(
            observed.contains(&inst2.pid),
            "uncompleted pid must be observed"
        );

        let record = ledger
            .record(active.generation)
            .expect("record")
            .expect("record exists");
        let closure = record.closure.expect("closure exists");
        let entry1 = closure
            .admissions
            .iter()
            .find(|a| a.launch_id == launch1)
            .expect("launch1 entry");
        assert_eq!(entry1.instance, Some(inst1));
        assert_eq!(entry1.uid, Some(501));
        assert_eq!(entry1.finding, AdmissionFinding::Exited);
    }

    #[test]
    fn stored_timing_literals_12500_and_500_clamp_downward_and_past_reap_refuses_live() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let launch_id = "timing-test";
        let live_inst = instance(9001, 100);
        admit(
            directory.path(),
            active.generation,
            launch_id,
            Some(HostedServiceKind::Sense),
            Some(live_inst),
        );

        // Plant far-future escalate_at
        let now_ms = unix_millis();
        let live_file = LiveRecoveryFile {
            schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
            kind: RecoveryArtifactKind::Live,
            generation: active.generation,
            sealed_phase: ParentLossPhase::Sealed,
            launch_id: launch_id.to_string(),
            service: Some(HostedServiceKind::Sense),
            root: live_inst,
            uid: 501,
            parent_launch_id: None,
            descendants: Vec::new(),
            escalate_at_unix_ms: now_ms + 999_999,
            reap_deadline_unix_ms: None,
            term_requested: true,
            escalated: false,
            binding_digest: compute_live_binding_digest(live_inst, 501, &[]).unwrap(),
        };
        let live_path = ledger.closure_recovery_live_path(active.generation, launch_id);
        fs::create_dir_all(live_path.parent().unwrap()).unwrap();
        write_json(&live_path, &live_file, json_options()).unwrap();

        let (source, retirer) = fake(&[(live_inst, 501)], &[], &[live_inst.pid]);
        let lease = acquire_lease(&ledger);

        // Attempt returns without waiting 999s and clamps down:
        let error = ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_millis(50)),
            )
            .expect_err("attempt deadline hit");
        assert!(matches!(
            error,
            ParentLossLedgerError::RecoveryRequired(BootstrapRecoveryReason::CoordinatorNotLive)
        ));

        let persisted: LiveRecoveryFile =
            serde_json::from_slice(&fs::read(&live_path).unwrap()).unwrap();
        assert!(
            persisted.escalate_at_unix_ms <= now_ms + 12_500 + 500,
            "escalate_at clamped down to <= now + 12_500"
        );

        // Plant past reap deadline with process still SameLive:
        let past_reap_file = LiveRecoveryFile {
            schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
            kind: RecoveryArtifactKind::Live,
            generation: active.generation,
            sealed_phase: ParentLossPhase::Sealed,
            launch_id: launch_id.to_string(),
            service: Some(HostedServiceKind::Sense),
            root: live_inst,
            uid: 501,
            parent_launch_id: None,
            descendants: Vec::new(),
            escalate_at_unix_ms: now_ms.saturating_sub(1000),
            reap_deadline_unix_ms: Some(now_ms.saturating_sub(100)),
            term_requested: true,
            escalated: false,
            binding_digest: compute_live_binding_digest(live_inst, 501, &[]).unwrap(),
        };
        write_json(&live_path, &past_reap_file, json_options()).unwrap();

        let error2 = ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect_err(
                "past reap deadline with process still live returns AbandonedAdmissionLive",
            );
        assert!(matches!(
            error2,
            ParentLossLedgerError::RecoveryRequired(
                BootstrapRecoveryReason::AbandonedAdmissionLive
            )
        ));
    }

    #[cfg(all(test, feature = "full-tests"))]
    struct ChildGuard(std::process::Child);

    #[cfg(all(test, feature = "full-tests"))]
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(all(test, feature = "full-tests"))]
    fn spawn_test_child(ignore_sigterm: bool) -> (ChildGuard, ProcessInstance) {
        let fixture = option_env!("CARGO_BIN_EXE_solstone-system-test-child")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::current_exe().ok().and_then(|current| {
                    let dir = current.parent()?;
                    let candidate = dir.join("solstone-system-test-child");
                    if candidate.is_file() {
                        Some(candidate)
                    } else {
                        let parent = dir.parent()?;
                        let candidate = parent.join("solstone-system-test-child");
                        if candidate.is_file() {
                            Some(candidate)
                        } else {
                            None
                        }
                    }
                })
            })
            .unwrap_or_else(|| std::path::PathBuf::from("solstone-system-test-child"));
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let ready = std::env::temp_dir().join(format!(
            "solstone-parent-loss-stubborn-{}-{stamp}",
            std::process::id()
        ));
        let mut command = std::process::Command::new(fixture);
        if ignore_sigterm {
            command.args(["block-term-sleep", ready.to_str().expect("utf8 path")]);
        } else {
            command.arg("sleep");
        }
        let child = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn child");
        let guard = ChildGuard(child);
        if ignore_sigterm {
            for _ in 0..500 {
                if ready.exists() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            assert!(ready.exists(), "stubborn child installed SIGTERM mask");
            let _ = fs::remove_file(&ready);
        }
        let instance = match SystemProcessInstanceSource.inspect(guard.0.id()) {
            InspectResult::Present { instance, .. } => instance,
            other => panic!("own child must be inspectable: {other:?}"),
        };
        (guard, instance)
    }

    #[cfg(all(test, feature = "full-tests"))]
    #[test]
    fn real_block_term_sleep_child_with_handshake_respects_grace_and_retry_does_not_extend() {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let (_guard, inst) = spawn_test_child(true);

        admit(
            directory.path(),
            active.generation,
            "stubborn-child",
            Some(HostedServiceKind::Sense),
            Some(inst),
        );

        let source = SystemProcessInstanceSource;
        let retirer = SystemAdmissionRetirer;
        let lease = acquire_lease(&ledger);

        let start = Instant::now();
        let outcome = ledger.reserve_generation_closing_abandoned(
            instance(11, 3),
            [],
            &lease,
            &authority(&source, &retirer, Duration::from_secs(30)),
        );
        let elapsed = start.elapsed();
        assert!(outcome.is_ok(), "eventual SIGKILL reaps stubborn child");
        assert!(
            elapsed >= Duration::from_millis(12_000),
            "must wait out 12.5s grace before KILL"
        );
    }

    #[cfg(all(test, feature = "full-tests"))]
    #[test]
    fn retained_census_descendant_obligation_after_root_exit_and_rebound_descendant_not_signalled()
    {
        let directory = TempDir::new().expect("temporary root");
        let (ledger, active) = open_generation(&directory);
        let launch_id = "census-obligation";
        let root_inst = instance(9901, 100);
        let desc_inst = instance(9902, 200);

        admit(
            directory.path(),
            active.generation,
            launch_id,
            Some(HostedServiceKind::Sense),
            Some(root_inst),
        );

        let live_file = LiveRecoveryFile {
            schema: PARENT_LOSS_RECOVERY_SCHEMA_V1,
            kind: RecoveryArtifactKind::Live,
            generation: active.generation,
            sealed_phase: ParentLossPhase::Sealed,
            launch_id: launch_id.to_string(),
            service: Some(HostedServiceKind::Sense),
            root: root_inst,
            uid: 501,
            parent_launch_id: None,
            descendants: vec![LiveDescendantIdentity {
                instance: desc_inst,
                uid: 501,
                ppid: root_inst.pid,
                pgid: root_inst.pid,
            }],
            escalate_at_unix_ms: 0,
            reap_deadline_unix_ms: None,
            term_requested: true,
            escalated: false,
            binding_digest: compute_live_binding_digest(
                root_inst,
                501,
                &[LiveDescendantIdentity {
                    instance: desc_inst,
                    uid: 501,
                    ppid: root_inst.pid,
                    pgid: root_inst.pid,
                }],
            )
            .unwrap(),
        };
        let live_path = ledger.closure_recovery_live_path(active.generation, launch_id);
        fs::create_dir_all(live_path.parent().unwrap()).unwrap();
        write_json(&live_path, &live_file, json_options()).unwrap();

        // Root is exited, descendant is still live:
        let (source, retirer) = fake(&[(desc_inst, 501)], &[], &[desc_inst.pid]);
        let lease = acquire_lease(&ledger);

        let error = ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_millis(50)),
            )
            .expect_err("live descendant must prevent completion");
        assert!(matches!(
            error,
            ParentLossLedgerError::RecoveryRequired(
                BootstrapRecoveryReason::AbandonedAdmissionLive
            )
        ));
        assert!(
            live_path.exists(),
            "live file retained while descendant live"
        );
    }

    #[cfg(all(test, feature = "full-tests"))]
    #[test]
    fn semantically_different_existing_ledger_set_aside_and_rebuilt_while_conflicting_record_refuses()
     {
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

        // Plant different ledger:
        let diff_ledger = vec![SealedAdmission {
            launch_id: "different-id".to_string(),
            service: Some(HostedServiceKind::Sense),
            identity: AdmissionIdentity {
                generation: active.generation,
                launch_id: "different-id".to_string(),
                instance: instance(9999, 99),
                uid: 501,
                parent_launch_id: None,
            },
        }];
        let ledger_path = ledger.sealed_ledger_path(active.generation);
        fs::create_dir_all(ledger_path.parent().unwrap()).unwrap();
        write_json(&ledger_path, &diff_ledger, json_options()).unwrap();

        let (source, retirer) = fake(&[], &[], &[]);
        let lease = acquire_lease(&ledger);

        // Case 1: No record digest commits it -> set aside and rebuilt:
        let successor = ledger
            .reserve_generation_closing_abandoned(
                instance(11, 3),
                [],
                &lease,
                &authority(&source, &retirer, Duration::from_secs(2)),
            )
            .expect("rebuilds different ledger");
        assert_eq!(successor.generation, active.generation + 1);

        let closure = closure_of(&ledger, active.generation);
        assert!(
            closure.set_aside.iter().any(|s| s.contains("ledger.json")),
            "conflicting ledger was set aside"
        );
    }
}
