// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Archive raw media, record the pre-mark ledger witness, then mint an offload mark.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset, Local, TimeZone, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};
use solstone_core_backup::{get_backup_config, record_offload_result};
use solstone_core_backup_runtime::{
    BackupServices, check_archive_snapshot_files, run_archive_backup,
};
use solstone_core_journal_io::check_record_identities;
use solstone_core_journal_io::paths::{PathOrDay, day_dirs, iter_segments};
use solstone_core_retention::{
    RawRelease, Target,
    content::{ClosedHandlerSet, JournalMedia},
    eligibility::resolve as resolve_segment_gate,
    marks::{decline_offload, load as load_marks},
    policy::policy_from_journal_config,
    scan::scan_segment,
    sweep::would_empty_audio_offload_mark_still_release,
    upsert_offload,
};

use crate::ledger::append_offload_event;
use crate::marks::OffloadMarkIndex;
use crate::measurement::measure_local_media;
use crate::pruning_audit::{PruneAuditWriter, open_prune_audit, write_prune_audit};
use solstone_core_retention::archive_catalog::OffloadFile;

pub use current::{
    CurrentFileIdentity, CurrentOffloadAvailability, CurrentOffloadDecision, CurrentOffloadRefusal,
    CurrentOffloadRefusalReason, EligibleOffloadFile, inspect_current_offload,
};

mod offer_creation;
pub use offer_creation::{
    OffloadCreation, OffloadCreationNative, OffloadCreationOwner, OffloadCreationRefusal,
    OffloadLedgerEffect, OffloadLedgerSync, OffloadLedgerWrite, OffloadPlanning,
    OffloadPlanningPhase, OffloadPlanningRefusal, create_offload_offers,
    create_planned_offload_offers,
};

#[cfg(all(test, unix, feature = "full-tests"))]
pub(crate) fn with_payload_capacity_fault<T>(run: impl FnOnce() -> T) -> (T, bool) {
    offer_creation::with_payload_capacity_fault(run)
}

pub const OFFLOAD_STALL_BACKUP_NOT_READY: &str = "backup_not_ready";
pub const OFFLOAD_STALL_BACKUP_FAILING: &str = "backup_failing";
pub const OFFLOAD_STALL_VERIFICATION_MISSING: &str = "verification_missing";
pub const OFFLOAD_STALL_VERIFICATION_OVERDUE: &str = "verification_overdue";
pub const OFFLOAD_STALL_VERIFICATION_FAILED: &str = "verification_failed";
pub const OFFLOAD_STALL_LOCKED: &str = "locked";
pub const OFFLOAD_STALL_ARCHIVE_FAILED: &str = "archive_failed";
pub const OFFLOAD_STALL_CONFIRM_FAILED: &str = "confirm_failed";
pub const OFFLOAD_STALL_CONFIRM_TOOL_FAILED: &str = "confirm_tool_failed";
pub const OFFLOAD_STALL_SEGMENT_IDENTITY: &str = "segment_identity";
pub const OFFLOAD_STALL_UNEXPECTED_ERROR: &str = "unexpected_error";
pub const OFFLOAD_STALL_REASONS: [&str; 11] = [
    OFFLOAD_STALL_BACKUP_NOT_READY,
    OFFLOAD_STALL_BACKUP_FAILING,
    OFFLOAD_STALL_VERIFICATION_MISSING,
    OFFLOAD_STALL_VERIFICATION_OVERDUE,
    OFFLOAD_STALL_VERIFICATION_FAILED,
    OFFLOAD_STALL_LOCKED,
    OFFLOAD_STALL_ARCHIVE_FAILED,
    OFFLOAD_STALL_CONFIRM_FAILED,
    OFFLOAD_STALL_CONFIRM_TOOL_FAILED,
    OFFLOAD_STALL_SEGMENT_IDENTITY,
    OFFLOAD_STALL_UNEXPECTED_ERROR,
];
pub const OFFLOAD_OK_BUDGET_ALREADY_SATISFIED: &str = "budget_already_satisfied";
pub const OFFLOAD_OK_BUDGET_SATISFIED: &str = "budget_satisfied";
pub const OFFLOAD_OK_MARKABLE_MEDIA_EXHAUSTED: &str = "markable_media_exhausted";
pub const VERIFICATION_INTEGRITY_REASONS: [&str; 3] =
    ["integrity_failed", "auth_failed", "repo_missing"];
pub const VERIFICATION_MAX_AGE_SECONDS: i64 = 14 * 86400;
pub const OFFLOAD_MAX_RUNTIME: &str = "7h";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OffloadSegmentDetail {
    pub day: String,
    pub stream: String,
    pub segment: String,
    pub files: u64,
    pub bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OffloadResult {
    pub status: String,
    pub reason: Option<String>,
    pub files_marked: u64,
    pub bytes_marked: u64,
    pub files_already_marked: u64,
    pub bytes_already_marked: u64,
    pub ran_out_of_markable_media: bool,
    pub dry_run: bool,
    /// Extra identity detail when `reason` is `segment_identity`. The reason
    /// token itself stays `segment_identity`.
    pub reason_detail: Option<String>,
    pub details: Vec<OffloadSegmentDetail>,
    /// Best-effort failure while creating or appending the invocation audit oplog.
    pub audit_recording_failure: Option<String>,
    /// Failure while recording the backup ledger's `last_offload` state.
    pub recording_failure: Option<String>,
}

pub fn format_offload_result(result: &OffloadResult) -> String {
    let line = if result.dry_run {
        if result.status == "stalled" {
            format!(
                "backup offload: stalled reason={}{} dry_run=true",
                offload_reason_text(
                    result
                        .reason
                        .as_deref()
                        .unwrap_or(OFFLOAD_STALL_UNEXPECTED_ERROR)
                ),
                result
                    .reason_detail
                    .as_deref()
                    .map(|detail| format!(" detail={detail}"))
                    .unwrap_or_default()
            )
        } else if result.status == "ok" {
            format!(
                "backup offload: ok reason={} dry_run=true",
                result
                    .reason
                    .as_deref()
                    .map(offload_reason_text)
                    .unwrap_or_default()
            )
        } else {
            format!("backup offload: {} dry_run=true", result.status)
        }
    } else if result.status == "ok" {
        format!(
            "backup offload: ok reason={} files_marked={} bytes_marked={} files_already_marked={} bytes_already_marked={} bytes_released=0 ran_out_of_markable_media={}",
            result
                .reason
                .as_deref()
                .map(offload_reason_text)
                .unwrap_or_default(),
            result.files_marked,
            result.bytes_marked,
            result.files_already_marked,
            result.bytes_already_marked,
            result.ran_out_of_markable_media
        )
    } else if result.status == "skipped" {
        "backup offload: skipped".to_owned()
    } else {
        format!(
            "backup offload: stalled reason={}{} files_marked={} bytes_marked={} bytes_released=0 ran_out_of_markable_media={}",
            offload_reason_text(
                result
                    .reason
                    .as_deref()
                    .unwrap_or(OFFLOAD_STALL_UNEXPECTED_ERROR)
            ),
            result
                .reason_detail
                .as_deref()
                .map(|detail| format!(" detail={detail}"))
                .unwrap_or_default(),
            result.files_marked,
            result.bytes_marked,
            result.ran_out_of_markable_media
        )
    };
    let line = match result.recording_failure.as_deref() {
        Some(detail) => format!("{line} recording_failed={detail}"),
        None => line,
    };
    match result.audit_recording_failure.as_deref() {
        Some(detail) => format!("{line} audit_recording_failed={detail}"),
        None => line,
    }
}

/// Turn an offload-run reason code into something an owner can read.
///
/// 🔴 The set is closed but spans two places: the 11 `OFFLOAD_STALL_REASONS`
/// and 3 `OFFLOAD_OK_*` codes this module produces itself, plus
/// `restic_unavailable`/`rclone_unavailable`, which `solstone-core-maintenance`
/// hands this same formatter when it can't resolve backup tools before a real
/// offload run ever starts (`format_backup_resolution_error`). Every value
/// reaching this function is a `&'static str` literal from one of those two
/// crates -- never external/untrusted text -- so the fallback below prints the
/// code itself rather than sanitizing it.
///
/// ⛔ The fallback still prints the code rather than swallowing it, matching
/// `backup_failure_reason`/`restore_failure_reason` in `solstone-core-backup-cli`.
fn offload_reason_text(reason: &str) -> String {
    match reason {
        OFFLOAD_STALL_BACKUP_NOT_READY => {
            "your backup isn't set up or hasn't completed a successful run yet".into()
        }
        OFFLOAD_STALL_BACKUP_FAILING => "your last backup didn't complete successfully".into(),
        OFFLOAD_STALL_VERIFICATION_MISSING => "no backup verification has completed yet".into(),
        OFFLOAD_STALL_VERIFICATION_OVERDUE => {
            "backup verification hasn't succeeded recently enough".into()
        }
        OFFLOAD_STALL_VERIFICATION_FAILED => {
            "the last backup verification found a problem with the backup's integrity".into()
        }
        // Same wording as `backup_failure_reason`'s "locked" arm in
        // solstone-core-backup-cli, so the two surfaces read alike.
        OFFLOAD_STALL_LOCKED => {
            "the backup at the destination is locked, usually by a run that's still going or one \
             that was interrupted"
                .into()
        }
        OFFLOAD_STALL_ARCHIVE_FAILED => "the archive step of the offload didn't complete".into(),
        OFFLOAD_STALL_CONFIRM_FAILED => {
            "the offload couldn't confirm the archived files actually reached the backup".into()
        }
        OFFLOAD_STALL_CONFIRM_TOOL_FAILED => {
            "the check that confirms an offload archive couldn't complete".into()
        }
        OFFLOAD_STALL_SEGMENT_IDENTITY => "one of your journal's recordings couldn't be verified \
             against its own identity record"
            .into(),
        OFFLOAD_STALL_UNEXPECTED_ERROR => "something unexpected interrupted the offload".into(),
        OFFLOAD_OK_BUDGET_ALREADY_SATISFIED => {
            "the offload budget was already met, so nothing needed to be marked for release".into()
        }
        // ⛔ Never "moved" or "released": offload only marks raw media for a
        // later owner-approved release step and never deletes it itself
        // (`run_offload`'s own doc comment, and the CLI line this accompanies
        // prints `bytes_released=0` in the very same breath).
        OFFLOAD_OK_BUDGET_SATISFIED => {
            "enough media has been marked for release to meet the offload budget".into()
        }
        OFFLOAD_OK_MARKABLE_MEDIA_EXHAUSTED => {
            "every eligible file was already checked; nothing more can be marked for release \
             right now"
                .into()
        }
        "restic_unavailable" => "the restic backup program isn't available".into(),
        "rclone_unavailable" => "the rclone program isn't available".into(),
        other => format!("stopped without a reason ({other})"),
    }
}

fn stall(
    reason: &str,
    dry_run: bool,
    details: Vec<OffloadSegmentDetail>,
    files: u64,
    bytes: u64,
    files_already_marked: u64,
    bytes_already_marked: u64,
) -> OffloadResult {
    OffloadResult {
        status: "stalled".into(),
        reason: Some(reason.into()),
        files_marked: files,
        bytes_marked: bytes,
        files_already_marked,
        bytes_already_marked,
        ran_out_of_markable_media: false,
        dry_run,
        reason_detail: None,
        details,
        audit_recording_failure: None,
        recording_failure: None,
    }
}

impl OffloadResult {
    fn with_reason_detail(mut self, detail: String) -> Self {
        self.reason_detail = Some(detail);
        self
    }

    fn with_audit_recording_failure(mut self, failure: Option<String>) -> Self {
        self.audit_recording_failure = failure;
        self
    }
}
fn prepare(paths: &[PathBuf]) -> Option<Vec<OffloadFile>> {
    paths
        .iter()
        .map(|path| {
            let bytes = path.metadata().ok()?.len();
            let digest = Sha256::digest(fs::read(path).ok()?);
            Some(OffloadFile {
                name: path.file_name()?.to_str()?.to_owned(),
                bytes,
                sha256: format!("{digest:x}"),
            })
        })
        .collect()
}
fn budget_satisfied(start_raw_bytes: u64, freed_bytes: u64, budget_bytes: Option<u64>) -> bool {
    budget_bytes.is_none_or(|budget| start_raw_bytes.saturating_sub(freed_bytes) <= budget)
}
fn local_instant(now_unix: i64) -> Option<DateTime<FixedOffset>> {
    Local
        .timestamp_opt(now_unix, 0)
        .single()
        .map(|time| time.fixed_offset())
}
fn offload_days(journal: &Path) -> Result<Vec<String>, String> {
    let mut days = day_dirs(journal)
        .unwrap_or_default()
        .into_keys()
        .collect::<Vec<_>>();
    days.sort();
    // A segment key is a time of day, so `(stream, key)` is unique only within
    // one day; every day is still checked before the first release.
    for day in &days {
        let listed = iter_segments(journal, PathOrDay::Day(day)).unwrap_or_default();
        if let Err(error) = check_record_identities(&listed) {
            return Err(format!("day {day}: {error}"));
        }
    }
    Ok(days)
}
fn precondition(config: &serde_json::Map<String, Value>, now: i64) -> Option<&'static str> {
    if config.get("enabled") != Some(&Value::Bool(true)) {
        return Some(OFFLOAD_STALL_BACKUP_NOT_READY);
    }
    let Some(backup) = config.get("last_backup").and_then(Value::as_object) else {
        return Some(OFFLOAD_STALL_BACKUP_NOT_READY);
    };
    if backup.get("status") != Some(&Value::String("ok".into())) {
        return Some(OFFLOAD_STALL_BACKUP_FAILING);
    }
    if backup
        .get("snapshot_id")
        .and_then(Value::as_str)
        .is_none_or(|snapshot| snapshot.trim().is_empty())
    {
        return Some(OFFLOAD_STALL_BACKUP_NOT_READY);
    }
    let Some(verification) = config.get("last_verification").and_then(Value::as_object) else {
        return Some(OFFLOAD_STALL_VERIFICATION_MISSING);
    };
    if !matches!(
        verification.get("status").and_then(Value::as_str),
        Some("ok" | "skipped" | "error")
    ) {
        return Some(OFFLOAD_STALL_VERIFICATION_MISSING);
    }
    if verification.get("status") == Some(&Value::String("error".into()))
        && verification
            .get("reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| VERIFICATION_INTEGRITY_REASONS.contains(&reason))
    {
        return Some(OFFLOAD_STALL_VERIFICATION_FAILED);
    }
    let Some(last_ok) = verification.get("last_ok_time").and_then(Value::as_i64) else {
        return Some(OFFLOAD_STALL_VERIFICATION_MISSING);
    };
    let Some(age) = now.checked_sub(last_ok) else {
        return Some(OFFLOAD_STALL_VERIFICATION_OVERDUE);
    };
    if age < 0 {
        return Some(OFFLOAD_STALL_VERIFICATION_MISSING);
    }
    if age > VERIFICATION_MAX_AGE_SECONDS {
        return Some(OFFLOAD_STALL_VERIFICATION_OVERDUE);
    }
    None
}

/// Offload only records pending owner-approved release; it never deletes raw files.
pub fn run_offload(journal: &Path, services: &BackupServices<'_>, dry_run: bool) -> OffloadResult {
    let config = match get_backup_config(journal) {
        Ok(config) => config,
        Err(_) => return stall(OFFLOAD_STALL_UNEXPECTED_ERROR, dry_run, vec![], 0, 0, 0, 0),
    };
    finish_offload(
        journal,
        services,
        run_offload_body(journal, services, dry_run, config),
    )
}

fn finish_offload(
    journal: &Path,
    services: &BackupServices<'_>,
    mut result: OffloadResult,
) -> OffloadResult {
    if result.dry_run {
        return result;
    }
    match record_offload_result(
        journal,
        &result.status,
        serde_json::json!(services.clock.now_unix()),
        result.reason.clone().map_or(Value::Null, Value::String),
        serde_json::json!(result.files_marked),
        serde_json::json!(result.bytes_marked),
        serde_json::json!(result.ran_out_of_markable_media),
    ) {
        Ok(()) => result,
        Err(error) => {
            result.recording_failure = Some(error.to_string());
            result
        }
    }
}

fn run_offload_body(
    journal: &Path,
    services: &BackupServices<'_>,
    dry_run: bool,
    config: serde_json::Map<String, Value>,
) -> OffloadResult {
    if config.get("offload").and_then(|value| value.get("enabled")) != Some(&Value::Bool(true)) {
        return OffloadResult {
            status: "skipped".into(),
            reason: None,
            files_marked: 0,
            bytes_marked: 0,
            files_already_marked: 0,
            bytes_already_marked: 0,
            ran_out_of_markable_media: false,
            dry_run,
            reason_detail: None,
            details: vec![],
            audit_recording_failure: None,
            recording_failure: None,
        };
    }
    if let Some(reason) = precondition(&config, services.clock.now_unix()) {
        return stall(reason, dry_run, vec![], 0, 0, 0, 0);
    }
    let mark_index = match load_marks(journal) {
        Ok(register) => OffloadMarkIndex::from_register(&register),
        Err(_) => return stall(OFFLOAD_STALL_UNEXPECTED_ERROR, dry_run, vec![], 0, 0, 0, 0),
    };
    // Policy for reconciling stale OffloadRawRelease marks below. ⚠ `config`
    // above is `get_backup_config`'s backup-scoped view (only the "backup" key,
    // merged with backup's own defaults) — it never carries "retention" or
    // "transcribe", so a policy built from it would silently see every owner's
    // policy as the untouched default. Read the real top-level journal config
    // instead, same as `health::mark_raw` does. An unreadable config falls back
    // to the default policy (release-once-processed) rather than failing the
    // run: the only consumer below is a decline, so the safe failure direction
    // is "reconcile nothing this pass," identical to today's behaviour.
    let retention_config = solstone_core_journal_config::read_journal_config(journal)
        .ok()
        .and_then(|read| read.config)
        .unwrap_or_default();
    let policy = policy_from_journal_config(&retention_config);
    // `today` is a UTC calendar date rather than the owner's configured
    // timezone: the primary case this reconciles — `empty_audio_rule` toggled
    // to keep-forever — has a `None` period and returns `KeptForever` without
    // consulting age at all, so the imprecision only ever reaches an age-based
    // ordinary-media rule, and both anchors already round toward keeping
    // content longer.
    let now =
        DateTime::<Utc>::from_timestamp(services.clock.now_unix(), 0).unwrap_or_else(Utc::now);
    let today = now.date_naive();
    // Withdraw any existing OffloadRawRelease mark for an entirely empty-audio
    // segment that the "keep audio with no speech" policy no longer agrees with
    // (e.g. the owner turned it on after this segment was marked) —
    // unconditionally, before the budget check below, because once budget is
    // satisfied the day/segment loop that owns the rest of this function never
    // runs again for an already-marked segment. Release was always safe either
    // way (it re-checks policy at approval time); this is about the owner's
    // review list staying honest, not a data-safety gap.
    //
    // Deliberately empty-audio-only: see `would_empty_audio_offload_mark_still_release`'s
    // own doc for why applying the *general* retention.raw_media age policy to
    // ordinary-media offload marks here would create an unbounded
    // mark/decline/remark cycle instead of a fix, given that policy's shipped
    // default is keep-forever.
    //
    // Deliberately does not touch `mark_index`, `files_already_marked` or
    // `bytes_already_marked` below, which the day/segment loop's own
    // already-marked skip and this run's budget accounting still read from the
    // pre-reconciliation snapshot: re-deriving them here would let the segment
    // loop immediately re-mark, in this same pass, exactly what this step just
    // withdrew for failing today's policy.
    if !dry_run {
        for mark in &mark_index.entries {
            let Ok(segments) = iter_segments(journal, PathOrDay::Day(&mark.day)) else {
                continue;
            };
            let Some(segment) = segments.iter().find(|segment| {
                segment.record_identity().is_ok_and(|identity| {
                    identity.stream == mark.stream && identity.key == mark.dir
                })
            }) else {
                continue;
            };
            let registry = ClosedHandlerSet;
            let classifier = JournalMedia;
            let found = scan_segment(segment.path(), &registry, &classifier);
            if would_empty_audio_offload_mark_still_release(
                &policy,
                &registry,
                found,
                &mark.stream,
                &mark.day,
                today,
                now,
            ) {
                continue;
            }
            let target = Target {
                day: mark.day.clone(),
                stream: mark.stream.clone(),
                dir: mark.dir.clone(),
            };
            let _ = decline_offload(journal, &target, &mark.names);
        }
    }
    let files_already_marked = mark_index
        .entries
        .iter()
        .map(|mark| mark.names.len() as u64)
        .sum();
    let bytes_already_marked = mark_index.entries.iter().map(|mark| mark.bytes).sum();
    let start_raw_bytes = match measure_local_media(journal).usage() {
        Ok(usage) => usage.total_bytes,
        Err(_) => {
            let mut failure = stall(
                OFFLOAD_STALL_UNEXPECTED_ERROR,
                dry_run,
                vec![],
                0,
                0,
                files_already_marked,
                bytes_already_marked,
            );
            if let Err(detail) = offload_days(journal) {
                failure.reason = Some(OFFLOAD_STALL_SEGMENT_IDENTITY.into());
                failure.reason_detail = Some(detail);
            }
            return failure;
        }
    };
    let budget = config
        .get("offload")
        .and_then(|value| value.get("budget_bytes"))
        .and_then(Value::as_u64);
    let mut details = vec![];
    let (mut files_marked, mut bytes_marked) = (0, 0);
    if budget_satisfied(start_raw_bytes, bytes_already_marked, budget) {
        return OffloadResult {
            status: "ok".into(),
            reason: Some(OFFLOAD_OK_BUDGET_ALREADY_SATISFIED.into()),
            files_marked,
            bytes_marked,
            files_already_marked,
            bytes_already_marked,
            ran_out_of_markable_media: false,
            dry_run,
            reason_detail: None,
            details,
            audit_recording_failure: None,
            recording_failure: None,
        };
    }
    let days = match offload_days(journal) {
        Ok(days) => days,
        Err(detail) => {
            return stall(
                OFFLOAD_STALL_SEGMENT_IDENTITY,
                dry_run,
                vec![],
                0,
                0,
                files_already_marked,
                bytes_already_marked,
            )
            .with_reason_detail(detail);
        }
    };
    let (mut audit, mut audit_recording_failure): (Option<PruneAuditWriter>, Option<String>) =
        if dry_run {
            (None, None)
        } else if let Some(opened) = local_instant(services.clock.now_unix()) {
            let audit = open_prune_audit(journal, opened);
            let failure = audit.recording_failure();
            (Some(audit), failure)
        } else {
            (
                None,
                Some("failed to create offload audit oplog: local time unavailable".into()),
            )
        };
    for day in days {
        for segment in iter_segments(journal, PathOrDay::Day(&day)).unwrap_or_default() {
            let identity = match segment.record_identity() {
                Ok(identity) => identity,
                Err(error) => {
                    return stall(
                        OFFLOAD_STALL_SEGMENT_IDENTITY,
                        dry_run,
                        details,
                        files_marked,
                        bytes_marked,
                        files_already_marked,
                        bytes_already_marked,
                    )
                    .with_reason_detail(error.to_string())
                    .with_audit_recording_failure(audit_recording_failure);
                }
            };
            let registry = ClosedHandlerSet;
            let classifier = JournalMedia;
            let found = scan_segment(segment.path(), &registry, &classifier);
            let RawRelease::Releasable(proven) = resolve_segment_gate(
                &registry,
                &classifier,
                &day,
                identity.stream,
                identity.key,
                &found,
            ) else {
                continue;
            };
            let mut paths = proven
                .iter()
                .map(|file| segment.path().join(file.name()))
                .collect::<Vec<_>>();
            paths.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
            if paths.is_empty() {
                continue;
            }
            let Some(files) = prepare(&paths) else {
                return stall(
                    OFFLOAD_STALL_UNEXPECTED_ERROR,
                    dry_run,
                    details,
                    files_marked,
                    bytes_marked,
                    files_already_marked,
                    bytes_already_marked,
                )
                .with_audit_recording_failure(audit_recording_failure);
            };
            let bytes = files.iter().map(|file| file.bytes).sum::<u64>();
            let names = files
                .iter()
                .map(|file| file.name.clone())
                .collect::<Vec<_>>();
            if mark_index
                .matches(&day, identity.stream, identity.key, &names)
                .is_some()
            {
                continue;
            }
            // `floor_bytes` is intentionally ignored: marking preserves bytes on disk; only status/restore use it.
            let effective_marked_bytes = if dry_run {
                details
                    .iter()
                    .map(|detail: &OffloadSegmentDetail| detail.bytes)
                    .sum()
            } else {
                bytes_marked
            };
            if budget_satisfied(
                start_raw_bytes,
                bytes_already_marked.saturating_add(effective_marked_bytes),
                budget,
            ) {
                return OffloadResult {
                    status: "ok".into(),
                    reason: Some(OFFLOAD_OK_BUDGET_SATISFIED.into()),
                    files_marked,
                    bytes_marked,
                    files_already_marked,
                    bytes_already_marked,
                    ran_out_of_markable_media: false,
                    dry_run,
                    reason_detail: None,
                    details,
                    audit_recording_failure,
                    recording_failure: None,
                };
            }
            let detail = OffloadSegmentDetail {
                day: day.clone(),
                stream: identity.stream.to_owned(),
                segment: identity.key.to_owned(),
                files: files.len() as u64,
                bytes,
            };
            details.push(detail);
            if dry_run {
                continue;
            }
            let archive = run_archive_backup(journal, services, &paths);
            let Some(snapshot) = archive.snapshot_id else {
                return stall(
                    if archive.error_reason.as_deref() == Some(OFFLOAD_STALL_LOCKED) {
                        OFFLOAD_STALL_LOCKED
                    } else {
                        OFFLOAD_STALL_ARCHIVE_FAILED
                    },
                    false,
                    details,
                    files_marked,
                    bytes_marked,
                    files_already_marked,
                    bytes_already_marked,
                )
                .with_audit_recording_failure(audit_recording_failure);
            };
            let expected = paths
                .iter()
                .zip(&files)
                .map(|(path, file)| (path.clone(), file.bytes))
                .collect::<BTreeMap<_, _>>();
            let check = check_archive_snapshot_files(journal, services, &snapshot, &expected);
            let confirmation_reason = match check.status.as_str() {
                "skipped" => Some(OFFLOAD_STALL_BACKUP_NOT_READY),
                "error" if check.error_reason.as_deref() == Some(OFFLOAD_STALL_LOCKED) => {
                    Some(OFFLOAD_STALL_LOCKED)
                }
                "error" => Some(OFFLOAD_STALL_CONFIRM_TOOL_FAILED),
                "ok" => match check.verdicts.as_ref() {
                    None => Some(OFFLOAD_STALL_CONFIRM_TOOL_FAILED),
                    Some(verdicts) if verdicts.iter().any(|verdict| !verdict.confirmed) => {
                        Some(OFFLOAD_STALL_CONFIRM_FAILED)
                    }
                    Some(_) => None,
                },
                _ => Some(OFFLOAD_STALL_CONFIRM_TOOL_FAILED),
            };
            if let Some(reason) = confirmation_reason {
                return stall(
                    reason,
                    false,
                    details,
                    files_marked,
                    bytes_marked,
                    files_already_marked,
                    bytes_already_marked,
                )
                .with_audit_recording_failure(audit_recording_failure);
            }
            if append_offload_event(
                journal,
                &day,
                identity.stream,
                identity.key,
                &snapshot,
                &files,
                services.clock.now_unix() as u64,
            )
            .is_err()
            {
                return stall(
                    OFFLOAD_STALL_UNEXPECTED_ERROR,
                    false,
                    details,
                    files_marked,
                    bytes_marked,
                    files_already_marked,
                    bytes_already_marked,
                )
                .with_audit_recording_failure(audit_recording_failure);
            }
            let Some(marked_at) = DateTime::<Utc>::from_timestamp(services.clock.now_unix(), 0)
            else {
                return stall(
                    OFFLOAD_STALL_UNEXPECTED_ERROR,
                    false,
                    details,
                    files_marked,
                    bytes_marked,
                    files_already_marked,
                    bytes_already_marked,
                )
                .with_audit_recording_failure(audit_recording_failure);
            };
            if upsert_offload(
                journal,
                &Target {
                    day: day.clone(),
                    stream: identity.stream.to_owned(),
                    dir: identity.key.to_owned(),
                },
                names,
                bytes,
                format!("restic-snapshot:{snapshot}"),
                marked_at,
            )
            .is_err()
            {
                return stall(
                    OFFLOAD_STALL_UNEXPECTED_ERROR,
                    false,
                    details,
                    files_marked,
                    bytes_marked,
                    files_already_marked,
                    bytes_already_marked,
                )
                .with_audit_recording_failure(audit_recording_failure);
            }
            let message = format!(
                "raw-media offload: archived and marked {} raw media file(s)",
                files.len()
            );
            let record = serde_json::json!({
                "event": "raw_media_offload",
                "kind": "raw_media_offload",
                "outcome": "success",
                "stream": identity.stream,
                "segment": identity.key,
                "bytes_marked": bytes,
                "ts": services.clock.now_unix().saturating_mul(1_000),
            });
            if let Some(audit) = audit.as_mut() {
                let outcome = write_prune_audit(audit, &record, &day, &message);
                if audit_recording_failure.is_none() {
                    audit_recording_failure = outcome.recording_failure;
                }
            }
            files_marked += files.len() as u64;
            bytes_marked += bytes;
        }
    }
    OffloadResult {
        status: "ok".into(),
        reason: Some(OFFLOAD_OK_MARKABLE_MEDIA_EXHAUSTED.into()),
        files_marked,
        bytes_marked,
        files_already_marked,
        bytes_already_marked,
        ran_out_of_markable_media: true,
        dry_run,
        reason_detail: None,
        details,
        audit_recording_failure,
        recording_failure: None,
    }
}

mod current {
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use chrono::{DateTime, FixedOffset, Utc};
    use serde_json::{Map, Value};
    use solstone_core_backup_runtime::AdmittedArchive;
    use solstone_core_journal_io::journal_root::JournalRoot;
    use solstone_core_journal_io::locking::LockOptions;
    use solstone_core_journal_io::media::{MediaDirectory, MediaIdentity, MediaObservation};
    use solstone_core_journal_io::paths::SegmentLayout;
    use solstone_core_retention::Target;
    use solstone_core_retention::archive_proof::{ApprovalRevision, ArchiveFileProof};
    use solstone_core_retention::content::{ClosedHandlerSet, ContentName, JournalMedia};
    use solstone_core_retention::grouped::{DecisionPurpose, GroupVerdict, inspect_grouped};
    use solstone_core_retention::marks::{Mark, MarkId, MarkState, RemovalClass};
    use solstone_core_retention::policy::{Policy, policy_from_journal_config};
    use solstone_core_retention::review_context::load_at_root;
    use solstone_core_retention::scan::{CheckedSegment, scan_segment_bound};
    use solstone_core_segment::output_attempt::OutputClaims;

    use solstone_core_retention::archive_catalog::read_complete_catalog;

    pub(super) const MAX_CURRENT_ENTRIES: usize = 100_000;
    const MAX_CURRENT_CONFIG_BYTES: usize = 1024 * 1024;

    #[cfg(test)]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(super) enum Checkpoint {
        BeforeHash,
        AfterGroup,
        BeforeFinish,
    }
    #[cfg(test)]
    type TestCheckpoint = Box<dyn FnMut(Checkpoint, &Path)>;
    #[cfg(test)]
    thread_local! {
        static CHECKPOINT: std::cell::RefCell<Option<TestCheckpoint>> = const { std::cell::RefCell::new(None) };
    }
    #[cfg(test)]
    pub(super) fn with_checkpoint<T>(
        hook: impl FnMut(Checkpoint, &Path) + 'static,
        run: impl FnOnce() -> T,
    ) -> T {
        CHECKPOINT.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Box::new(hook));
        });
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                CHECKPOINT.with(|slot| {
                    slot.borrow_mut().take();
                });
            }
        }
        let _reset = Reset;
        run()
    }
    #[cfg(test)]
    fn checkpoint(phase: Checkpoint, path: &Path) {
        CHECKPOINT.with(|slot| {
            if let Some(hook) = slot.borrow_mut().as_mut() {
                hook(phase, path);
            }
        });
    }

    /// Whether the requested observation completed with known refusal decisions.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum CurrentOffloadAvailability {
        Available,
        Unavailable,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub enum CurrentOffloadRefusalReason {
        Busy,
        Kept { names: Vec<String> },
        Stale,
        Failed,
        Missing,
        Unbound,
        WrongClass,
        Unproven { names: Vec<String> },
        Incomplete,
        AdmissionChanged,
        ExecutionUnavailable,
        Prerequisite { reason: &'static str },
    }

    #[derive(Debug)]
    pub struct CurrentOffloadRefusal {
        mark_id: Option<MarkId>,
        reason: CurrentOffloadRefusalReason,
    }

    impl CurrentOffloadRefusal {
        pub fn mark_id(&self) -> Option<&MarkId> {
            self.mark_id.as_ref()
        }
        pub fn reason(&self) -> &CurrentOffloadRefusalReason {
            &self.reason
        }
    }

    /// Lossless native identity observed by this batch, without a public factory.
    #[derive(Debug, Eq, PartialEq)]
    pub struct CurrentFileIdentity(MediaIdentity);

    /// A current projection observation, never a release capability.
    #[derive(Debug)]
    pub struct EligibleOffloadFile {
        mark: Arc<Mark>,
        file: ArchiveFileProof,
        layout: SegmentLayout,
        revision: ApprovalRevision,
        identity: CurrentFileIdentity,
    }

    impl EligibleOffloadFile {
        fn file(&self) -> &ArchiveFileProof {
            &self.file
        }
        pub fn target(&self) -> &Target {
            &self.mark.target
        }
        pub fn layout(&self) -> SegmentLayout {
            self.layout
        }
        pub fn name(&self) -> &str {
            &self.file().name
        }
        pub fn bytes(&self) -> u64 {
            self.file().bytes
        }
        pub fn revision(&self) -> &ApprovalRevision {
            &self.revision
        }
        pub fn class(&self) -> RemovalClass {
            self.mark.class
        }
        pub fn identity(&self) -> &CurrentFileIdentity {
            &self.identity
        }
        pub fn witness_id(&self) -> &str {
            &self.file().witness_id
        }
        pub fn mark(&self) -> &Mark {
            &self.mark
        }
    }

    pub struct CurrentOffloadDecision {
        root: Arc<JournalRoot>,
        availability: CurrentOffloadAvailability,
        eligible: Vec<EligibleOffloadFile>,
        refusals: Vec<CurrentOffloadRefusal>,
    }

    impl CurrentOffloadDecision {
        pub fn availability(&self) -> CurrentOffloadAvailability {
            self.availability
        }
        pub fn eligible(&self) -> impl Iterator<Item = &EligibleOffloadFile> {
            self.eligible.iter()
        }
        pub fn refusals(&self) -> &[CurrentOffloadRefusal] {
            &self.refusals
        }
        pub fn root_observation(&self) -> String {
            self.root.identity().observation_id()
        }
        fn refuse(&mut self, mark: Option<&Mark>, reason: CurrentOffloadRefusalReason) {
            self.refusals.push(CurrentOffloadRefusal {
                mark_id: mark.map(|mark| mark.id.clone()),
                reason,
            });
        }
        fn unavailable(&mut self, reason: CurrentOffloadRefusalReason) {
            self.availability = CurrentOffloadAvailability::Unavailable;
            self.eligible.clear();
            self.refuse(None, reason);
        }
    }

    #[derive(PartialEq)]
    pub(super) struct CurrentPolicyObservation {
        prerequisites: Map<String, Value>,
        retention: Option<Value>,
        preserve_all: Option<Value>,
    }

    struct CurrentGroupObservation {
        directory: Arc<MediaDirectory>,
        scan: CheckedSegment,
        marks: Vec<Arc<Mark>>,
        layout: SegmentLayout,
    }

    fn same_scan(before: &CheckedSegment, after: &CheckedSegment) -> bool {
        before.files().len() == after.files().len()
            && before
                .files()
                .iter()
                .zip(after.files())
                .all(|(before_file, after_file)| {
                    before_file.name == after_file.name
                        && before_file.size == after_file.size
                        && before_file.sidecar.record == after_file.sidecar.record
                        && before_file.sidecar.has_analysis_row
                            == after_file.sidecar.has_analysis_row
                        && before.bound_entry(&before_file.name)
                            == after.bound_entry(&after_file.name)
                })
    }

    impl CurrentPolicyObservation {
        pub(super) fn capture(
            root: &Arc<JournalRoot>,
            now: i64,
        ) -> Result<Self, CurrentOffloadRefusalReason> {
            let directory = MediaDirectory::open(Arc::clone(root), Path::new("config"))
                .map_err(|_| CurrentOffloadRefusalReason::Incomplete)?;
            let read = directory
                .read_file_bounded(OsStr::new("journal.json"), MAX_CURRENT_CONFIG_BYTES)
                .map_err(|_| CurrentOffloadRefusalReason::Incomplete)?
                .ok_or(CurrentOffloadRefusalReason::Incomplete)?;
            if !read.complete {
                return Err(CurrentOffloadRefusalReason::Incomplete);
            }
            let config: Map<String, Value> = serde_json::from_slice(&read.bytes)
                .map_err(|_| CurrentOffloadRefusalReason::Incomplete)?;
            let backup = config
                .get("backup")
                .and_then(Value::as_object)
                .ok_or(CurrentOffloadRefusalReason::Incomplete)?;
            if let Some(reason) = super::precondition(backup, now) {
                return Err(CurrentOffloadRefusalReason::Prerequisite { reason });
            }
            if let Some(retention) = config.get("retention") {
                let retention = retention
                    .as_object()
                    .ok_or(CurrentOffloadRefusalReason::Incomplete)?;
                if let Some(streams) = retention.get("per_stream") {
                    let streams = streams
                        .as_object()
                        .ok_or(CurrentOffloadRefusalReason::Incomplete)?;
                    if streams.values().any(|stream| !stream.is_object()) {
                        return Err(CurrentOffloadRefusalReason::Incomplete);
                    }
                }
            }
            let preserve_all = match config.get("transcribe") {
                None => None,
                Some(value) => value
                    .as_object()
                    .ok_or(CurrentOffloadRefusalReason::Incomplete)?
                    .get("preserve_all")
                    .cloned(),
            };
            directory
                .revalidate()
                .map_err(|_| CurrentOffloadRefusalReason::Incomplete)?;
            Ok(Self {
                prerequisites: ["enabled", "last_backup", "last_verification"]
                    .into_iter()
                    .filter_map(|key| {
                        backup
                            .get(key)
                            .cloned()
                            .map(|value| (key.to_owned(), value))
                    })
                    .collect(),
                retention: config.get("retention").cloned(),
                preserve_all,
            })
        }

        pub(super) fn policy(&self) -> Policy {
            let mut config = Map::new();
            if let Some(retention) = &self.retention {
                config.insert("retention".into(), retention.clone());
            }
            if let Some(preserve_all) = &self.preserve_all {
                config.insert(
                    "transcribe".into(),
                    serde_json::json!({"preserve_all": preserve_all}),
                );
            }
            policy_from_journal_config(&config)
        }

        pub(super) fn revalidate(
            &self,
            operation: &AdmittedArchive<'_>,
        ) -> Result<(), CurrentOffloadRefusalReason> {
            let now = operation
                .current_time()
                .map_err(|_| CurrentOffloadRefusalReason::AdmissionChanged)?;
            if Self::capture(operation.journal_root(), now)? != *self {
                return Err(CurrentOffloadRefusalReason::AdmissionChanged);
            }
            Ok(())
        }
    }

    fn relative_segment(target: &Target, layout: SegmentLayout) -> PathBuf {
        let day = Path::new("chronicle").join(&target.day);
        match layout {
            SegmentLayout::Direct => day.join(&target.dir),
            SegmentLayout::Named => day.join(&target.stream).join(&target.dir),
        }
    }

    fn check_mark(
        root: &JournalRoot,
        repository: &str,
        current: &solstone_core_retention::marks::Register,
        expected: &Mark,
    ) -> Result<(), CurrentOffloadRefusalReason> {
        if expected.class != RemovalClass::OffloadRawRelease {
            return Err(CurrentOffloadRefusalReason::WrongClass);
        }
        let actual = current
            .marks
            .get(&expected.id)
            .ok_or(CurrentOffloadRefusalReason::Missing)?;
        if actual != expected {
            return Err(CurrentOffloadRefusalReason::Stale);
        }
        if !matches!(actual.state, MarkState::Marked) {
            return Err(CurrentOffloadRefusalReason::Failed);
        }
        let offer = actual
            .offload
            .as_ref()
            .ok_or(CurrentOffloadRefusalReason::Unbound)?;
        if offer.validate().is_err()
            || offer.proof.journal_id != root.identity().observation_id()
            || offer.proof.repository_id != repository
        {
            return Err(CurrentOffloadRefusalReason::Unbound);
        }
        Ok(())
    }

    /// Inspect one requested batch using fresh bound proof and a per-call hash set.
    /// Busy groups earn no credit. Incomplete/global observations make the whole
    /// result unavailable, rather than representing unavailable P as zero.
    /// `at` supplies the owner's calendar offset and may not be in the admitted
    /// clock's future; prerequisites are checked against that actual clock.
    pub fn inspect_current_offload(
        operation: &mut AdmittedArchive<'_>,
        expected: &[Mark],
        at: DateTime<FixedOffset>,
    ) -> CurrentOffloadDecision {
        let root = Arc::clone(operation.journal_root());
        let mut decision = CurrentOffloadDecision {
            root: Arc::clone(&root),
            availability: CurrentOffloadAvailability::Available,
            eligible: Vec::new(),
            refusals: Vec::new(),
        };
        // The exact writable archive-readback transport currently supports Unix only.
        // Known prelaunch refusal cannot become actionable projected relief.
        if !cfg!(unix) {
            decision.unavailable(CurrentOffloadRefusalReason::ExecutionUnavailable);
            return decision;
        }
        let inspect = || -> Result<CurrentPolicyObservation, CurrentOffloadRefusalReason> {
            operation
                .revalidate()
                .map_err(|_| CurrentOffloadRefusalReason::AdmissionChanged)?;
            let now = operation
                .current_time()
                .map_err(|_| CurrentOffloadRefusalReason::AdmissionChanged)?;
            if at.timestamp() > now {
                return Err(CurrentOffloadRefusalReason::AdmissionChanged);
            }
            CurrentPolicyObservation::capture(&root, now)
        };
        let observation = match inspect() {
            Ok(observation) => observation,
            Err(reason) => {
                decision.unavailable(reason);
                return decision;
            }
        };
        let current = match load_at_root(&root) {
            Ok(current) => current,
            Err(_) => {
                decision.unavailable(CurrentOffloadRefusalReason::Incomplete);
                return decision;
            }
        };
        if read_complete_catalog(&root).is_err() {
            decision.unavailable(CurrentOffloadRefusalReason::Incomplete);
            return decision;
        }
        let mut ids = BTreeSet::new();
        let mut groups: BTreeMap<(String, String, String, SegmentLayout), Vec<Arc<Mark>>> =
            BTreeMap::new();
        for mark in expected {
            if !ids.insert(mark.id.clone()) {
                decision.unavailable(CurrentOffloadRefusalReason::Incomplete);
                return decision;
            }
            if let Err(reason) = check_mark(&root, operation.repository_id(), &current, mark) {
                decision.refuse(Some(mark), reason);
                continue;
            }
            if let Some(offer) = &mark.offload {
                let target = &mark.target;
                groups
                    .entry((
                        target.day.clone(),
                        target.stream.clone(),
                        target.dir.clone(),
                        offer.proof.layout,
                    ))
                    .or_default()
                    .push(Arc::new(mark.clone()));
            }
        }
        let policy = observation.policy();
        let mut observed_groups = Vec::new();
        for ((_, _, _, layout), marks) in groups {
            match inspect_group(
                operation,
                &observation,
                &policy,
                &root,
                &marks,
                layout,
                at,
                &mut decision,
            ) {
                Ok(Some(group)) => observed_groups.push(group),
                Ok(None) => {}
                Err(reason) => {
                    decision.unavailable(reason);
                    return decision;
                }
            }
        }
        #[cfg(test)]
        checkpoint(Checkpoint::BeforeFinish, root.canonical_path());
        if operation.revalidate().is_err() || observation.revalidate(operation).is_err() {
            decision.unavailable(CurrentOffloadRefusalReason::AdmissionChanged);
            return decision;
        }
        let final_register = load_at_root(&root);
        let final_catalog = read_complete_catalog(&root);
        let (Ok(final_register), Ok(final_catalog)) = (final_register, final_catalog) else {
            decision.unavailable(CurrentOffloadRefusalReason::Incomplete);
            return decision;
        };
        let prior = std::mem::take(&mut decision.eligible);
        for file in prior {
            let mark = &file.mark;
            let result = check_mark(&root, operation.repository_id(), &final_register, mark);
            if let Err(reason) = result {
                decision.refuse(Some(mark), reason);
                continue;
            }
            let Some(offer) = &mark.offload else {
                decision.unavailable(CurrentOffloadRefusalReason::Incomplete);
                return decision;
            };
            let proof = &offer.proof;
            if final_catalog.active_witness(proof, file.file()).is_none() {
                decision.refuse(Some(mark), CurrentOffloadRefusalReason::Unbound);
                continue;
            }
            decision.eligible.push(file);
        }
        for group in observed_groups {
            if let Err(reason) = recheck_group(operation, &observation, &root, group, &mut decision)
            {
                decision.unavailable(reason);
                return decision;
            }
        }
        if observation.revalidate(operation).is_err() {
            decision.unavailable(CurrentOffloadRefusalReason::AdmissionChanged);
        }
        decision
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "explicit current batch admission and observations"
    )]
    fn inspect_group(
        operation: &AdmittedArchive<'_>,
        observation: &CurrentPolicyObservation,
        policy: &Policy,
        root: &Arc<JournalRoot>,
        marks: &[Arc<Mark>],
        layout: SegmentLayout,
        at: DateTime<FixedOffset>,
        decision: &mut CurrentOffloadDecision,
    ) -> Result<Option<CurrentGroupObservation>, CurrentOffloadRefusalReason> {
        let Some(first) = marks.first() else {
            return Ok(None);
        };
        let target = &first.target;
        let relative = relative_segment(target, layout);
        let path = root.canonical_path().join(&relative);
        let directory = MediaDirectory::open(Arc::clone(root), &relative)
            .map_err(|_| CurrentOffloadRefusalReason::Incomplete)?;
        let options = LockOptions {
            timeout: Duration::ZERO,
            ..LockOptions::default()
        };
        let claims = match OutputClaims::discover(&path, options) {
            Ok(claims) => claims,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                for mark in marks {
                    decision.refuse(Some(mark), CurrentOffloadRefusalReason::Busy);
                }
                return Ok(None);
            }
            Err(_) => return Err(CurrentOffloadRefusalReason::Incomplete),
        };
        if claims
            .segment_identity(&path)
            .map_err(|_| CurrentOffloadRefusalReason::Incomplete)?
            != directory.object_identity()
        {
            return Err(CurrentOffloadRefusalReason::Incomplete);
        }
        let commit = match claims.commit(&path) {
            Ok(commit) => commit,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                for mark in marks {
                    decision.refuse(Some(mark), CurrentOffloadRefusalReason::Busy);
                }
                return Ok(None);
            }
            Err(_) => return Err(CurrentOffloadRefusalReason::Incomplete),
        };
        observation.revalidate(operation)?;
        let register = load_at_root(root).map_err(|_| CurrentOffloadRefusalReason::Incomplete)?;
        let catalog =
            read_complete_catalog(root).map_err(|_| CurrentOffloadRefusalReason::Incomplete)?;
        let scan = scan_segment_bound(
            &directory,
            &ClosedHandlerSet,
            &JournalMedia,
            MAX_CURRENT_ENTRIES,
        )
        .map_err(|_| CurrentOffloadRefusalReason::Incomplete)?;
        let grouped = inspect_grouped(
            &scan,
            target,
            layout,
            policy,
            at.date_naive(),
            at.with_timezone(&Utc),
            DecisionPurpose::OffloadProposal,
        );
        let mut hashes: BTreeMap<String, MediaObservation> = BTreeMap::new();
        let start = decision.eligible.len();
        for mark in marks {
            if let Err(reason) = check_mark(root, operation.repository_id(), &register, mark) {
                decision.refuse(Some(mark), reason);
                continue;
            }
            let Some(offer) = &mark.offload else {
                return Err(CurrentOffloadRefusalReason::Incomplete);
            };
            let proof = &offer.proof;
            for file in &proof.files {
                let current = grouped
                    .eligible()
                    .find(|candidate| candidate.name() == file.name);
                let Some(current) = current else {
                    let group = [grouped.empty(), grouped.ordinary()]
                        .into_iter()
                        .find(|group| group.members().contains(&file.name));
                    let reason = match group.map(|group| group.verdict()) {
                        Some(GroupVerdict::Kept(_)) => CurrentOffloadRefusalReason::Kept {
                            names: vec![file.name.clone()],
                        },
                        _ => CurrentOffloadRefusalReason::Unproven {
                            names: vec![file.name.clone()],
                        },
                    };
                    decision.refuse(Some(mark), reason);
                    continue;
                };
                if current.bytes() != file.bytes
                    || current.processing() != &file.processing
                    || catalog.active_witness(proof, file).is_none()
                {
                    decision.refuse(Some(mark), CurrentOffloadRefusalReason::Unbound);
                    continue;
                }
                if !hashes.contains_key(&file.name) {
                    let name = ContentName::new(&file.name)
                        .ok_or(CurrentOffloadRefusalReason::Incomplete)?;
                    let entry = scan
                        .bound_entry(&name)
                        .ok_or(CurrentOffloadRefusalReason::Incomplete)?;
                    let before = directory
                        .read_prefix(OsStr::new(&file.name), 0)
                        .map_err(|_| CurrentOffloadRefusalReason::Incomplete)?
                        .ok_or(CurrentOffloadRefusalReason::Incomplete)?;
                    if before.entry != *entry {
                        return Err(CurrentOffloadRefusalReason::Incomplete);
                    }
                    #[cfg(test)]
                    checkpoint(Checkpoint::BeforeHash, &path.join(&file.name));
                    let hashed = directory
                        .observe_file(OsStr::new(&file.name))
                        .map_err(|_| CurrentOffloadRefusalReason::Incomplete)?
                        .ok_or(CurrentOffloadRefusalReason::Incomplete)?;
                    let after = directory
                        .read_prefix(OsStr::new(&file.name), 0)
                        .map_err(|_| CurrentOffloadRefusalReason::Incomplete)?
                        .ok_or(CurrentOffloadRefusalReason::Incomplete)?;
                    if after.entry != *entry
                        || hashed.identity != entry.identity
                        || hashed.digest.size != entry.size
                    {
                        return Err(CurrentOffloadRefusalReason::Incomplete);
                    }
                    hashes.insert(file.name.clone(), hashed);
                }
                let hashed = hashes
                    .get(&file.name)
                    .ok_or(CurrentOffloadRefusalReason::Incomplete)?;
                if hashed.digest.size != file.bytes || hashed.digest.sha256 != file.sha256 {
                    decision.refuse(Some(mark), CurrentOffloadRefusalReason::Unbound);
                    continue;
                }
                decision.eligible.push(EligibleOffloadFile {
                    mark: Arc::clone(mark),
                    file: file.clone(),
                    layout,
                    revision: offer.revision.clone(),
                    identity: CurrentFileIdentity(hashed.identity),
                });
            }
        }
        let final_scan = scan_segment_bound(
            &directory,
            &ClosedHandlerSet,
            &JournalMedia,
            MAX_CURRENT_ENTRIES,
        )
        .map_err(|_| CurrentOffloadRefusalReason::Incomplete)?;
        if !same_scan(&scan, &final_scan) {
            return Err(CurrentOffloadRefusalReason::Incomplete);
        }
        for file in decision
            .eligible
            .get(start..)
            .ok_or(CurrentOffloadRefusalReason::Incomplete)?
        {
            let name =
                ContentName::new(file.name()).ok_or(CurrentOffloadRefusalReason::Incomplete)?;
            if scan.bound_entry(&name) != final_scan.bound_entry(&name) {
                return Err(CurrentOffloadRefusalReason::Incomplete);
            }
        }
        observation.revalidate(operation)?;
        commit
            .revalidate()
            .map_err(|_| CurrentOffloadRefusalReason::Incomplete)?;
        directory
            .revalidate()
            .map_err(|_| CurrentOffloadRefusalReason::Incomplete)?;
        #[cfg(test)]
        checkpoint(Checkpoint::AfterGroup, &path);
        if decision.eligible.len() == start {
            return Ok(None);
        }
        Ok(Some(CurrentGroupObservation {
            directory,
            scan,
            marks: marks.to_vec(),
            layout,
        }))
    }

    fn recheck_group(
        operation: &AdmittedArchive<'_>,
        observation: &CurrentPolicyObservation,
        root: &Arc<JournalRoot>,
        group: CurrentGroupObservation,
        decision: &mut CurrentOffloadDecision,
    ) -> Result<(), CurrentOffloadRefusalReason> {
        let Some(first) = group.marks.first() else {
            return Err(CurrentOffloadRefusalReason::Incomplete);
        };
        let matches = |file: &EligibleOffloadFile| {
            file.target() == &first.target && file.layout() == group.layout
        };
        if !decision.eligible.iter().any(matches) {
            return Ok(());
        }
        group
            .directory
            .revalidate()
            .map_err(|_| CurrentOffloadRefusalReason::Incomplete)?;
        let path = root
            .canonical_path()
            .join(relative_segment(&first.target, group.layout));
        let options = LockOptions {
            timeout: Duration::ZERO,
            ..LockOptions::default()
        };
        let busy = |decision: &mut CurrentOffloadDecision| {
            decision.eligible.retain(|file| !matches(file));
            for mark in &group.marks {
                decision.refuse(Some(mark), CurrentOffloadRefusalReason::Busy);
            }
        };
        let claims = match OutputClaims::discover(&path, options) {
            Ok(claims) => claims,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                busy(decision);
                return Ok(());
            }
            Err(_) => return Err(CurrentOffloadRefusalReason::Incomplete),
        };
        if claims
            .segment_identity(&path)
            .map_err(|_| CurrentOffloadRefusalReason::Incomplete)?
            != group.directory.object_identity()
        {
            return Err(CurrentOffloadRefusalReason::Incomplete);
        }
        let commit = match claims.commit(&path) {
            Ok(commit) => commit,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                busy(decision);
                return Ok(());
            }
            Err(_) => return Err(CurrentOffloadRefusalReason::Incomplete),
        };
        observation.revalidate(operation)?;
        let scan = scan_segment_bound(
            &group.directory,
            &ClosedHandlerSet,
            &JournalMedia,
            MAX_CURRENT_ENTRIES,
        )
        .map_err(|_| CurrentOffloadRefusalReason::Incomplete)?;
        if !same_scan(&group.scan, &scan) {
            return Err(CurrentOffloadRefusalReason::Incomplete);
        }
        let register = load_at_root(root).map_err(|_| CurrentOffloadRefusalReason::Incomplete)?;
        let catalog =
            read_complete_catalog(root).map_err(|_| CurrentOffloadRefusalReason::Incomplete)?;
        let prior = std::mem::take(&mut decision.eligible);
        for file in prior {
            if matches(&file) {
                if let Err(reason) =
                    check_mark(root, operation.repository_id(), &register, &file.mark)
                {
                    decision.refuse(Some(&file.mark), reason);
                    continue;
                }
                let Some(offer) = &file.mark.offload else {
                    return Err(CurrentOffloadRefusalReason::Incomplete);
                };
                if catalog.active_witness(&offer.proof, file.file()).is_none() {
                    decision.refuse(Some(&file.mark), CurrentOffloadRefusalReason::Unbound);
                    continue;
                }
            }
            decision.eligible.push(file);
        }
        observation.revalidate(operation)?;
        commit
            .revalidate()
            .map_err(|_| CurrentOffloadRefusalReason::Incomplete)?;
        group
            .directory
            .revalidate()
            .map_err(|_| CurrentOffloadRefusalReason::Incomplete)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io;

    use serde_json::json;
    use solstone_core_backup::{
        Destination, generate_and_store_keys, record_backup_result, record_verification_result,
        set_destination, set_enabled, set_offload,
    };
    use solstone_core_backup_runtime::hosted_runtime::HttpError;
    use solstone_core_backup_runtime::{
        Clock, HttpRequest, HttpResponse, HttpTransport, JournalMaintenance,
        JournalMaintenanceError, ToolOutput, ToolRequest, ToolRunner,
    };
    use solstone_core_journal_io::{
        JournalRoot, MalformedPolicy,
        operational_log::{OplogFormat, catalog_oplogs},
        readers::read_jsonl_with_report,
    };

    #[cfg(all(test, unix, feature = "full-tests"))]
    mod planner;

    #[test]
    fn prerequisites_refuse_missing_malformed_future_and_expired_observations() {
        let now = 2_000_000;
        let ready = json!({
            "enabled":true,
            "last_backup":{"status":"ok","snapshot_id":"ready"},
            "last_verification":{"status":"ok","last_ok_time":now}
        });
        let check = |value: &Value| precondition(value.as_object().unwrap(), now);
        assert_eq!(check(&ready), None);
        for key in ["last_backup", "last_verification"] {
            let reason = if key == "last_backup" {
                OFFLOAD_STALL_BACKUP_NOT_READY
            } else {
                OFFLOAD_STALL_VERIFICATION_MISSING
            };
            for value in [Value::Null, json!([]), json!("ok"), json!(1)] {
                let mut changed = ready.clone();
                changed[key] = value;
                assert_eq!(check(&changed), Some(reason));
            }
            let mut missing = ready.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert_eq!(check(&missing), Some(reason));
        }
        for value in [Value::Null, json!("ready"), json!(1.5), json!(now + 1)] {
            let mut changed = ready.clone();
            changed["last_verification"]["last_ok_time"] = value;
            assert_eq!(check(&changed), Some(OFFLOAD_STALL_VERIFICATION_MISSING));
        }
        for key in ["status", "last_ok_time"] {
            let mut missing = ready.clone();
            missing["last_verification"]
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert_eq!(check(&missing), Some(OFFLOAD_STALL_VERIFICATION_MISSING));
        }
        for value in [Value::Null, json!(true), json!("unknown")] {
            let mut changed = ready.clone();
            changed["last_verification"]["status"] = value;
            assert_eq!(check(&changed), Some(OFFLOAD_STALL_VERIFICATION_MISSING));
        }
        for snapshot in [Value::Null, json!(""), json!(" \t"), json!(7)] {
            let mut changed = ready.clone();
            changed["last_backup"]["snapshot_id"] = snapshot;
            assert_eq!(check(&changed), Some(OFFLOAD_STALL_BACKUP_NOT_READY));
        }
        for (time, expected) in [
            (now - VERIFICATION_MAX_AGE_SECONDS, None),
            (
                now - VERIFICATION_MAX_AGE_SECONDS - 1,
                Some(OFFLOAD_STALL_VERIFICATION_OVERDUE),
            ),
            (i64::MIN, Some(OFFLOAD_STALL_VERIFICATION_OVERDUE)),
        ] {
            let mut changed = ready.clone();
            changed["last_verification"]["last_ok_time"] = json!(time);
            assert_eq!(check(&changed), expected);
        }
        for reason in VERIFICATION_INTEGRITY_REASONS {
            let mut changed = ready.clone();
            changed["last_verification"]["status"] = json!("error");
            changed["last_verification"]["reason"] = json!(reason);
            assert_eq!(check(&changed), Some(OFFLOAD_STALL_VERIFICATION_FAILED));
        }
        let mut transient = ready;
        transient["last_verification"]["status"] = json!("error");
        transient["last_verification"]["reason"] = json!("locked");
        assert_eq!(check(&transient), None);
    }

    struct Script {
        outputs: RefCell<VecDeque<ToolOutput>>,
        calls: RefCell<Vec<Vec<String>>>,
    }
    impl ToolRunner for Script {
        fn run(&self, request: &ToolRequest<'_>) -> io::Result<ToolOutput> {
            self.calls.borrow_mut().push(
                request
                    .argv
                    .iter()
                    .map(|value| value.to_string_lossy().into_owned())
                    .collect(),
            );
            Ok(self
                .outputs
                .borrow_mut()
                .pop_front()
                .expect("fixture output"))
        }
    }
    struct Http;
    impl HttpTransport for Http {
        fn execute(&self, _: &HttpRequest) -> Result<HttpResponse, HttpError> {
            unreachable!("BYO offload does not use HTTP")
        }
    }
    struct TestClock {
        now: i64,
    }
    impl Clock for TestClock {
        fn now_unix(&self) -> i64 {
            self.now
        }
        fn iso_week(&self) -> u8 {
            1
        }
    }
    struct Maintenance;
    impl JournalMaintenance for Maintenance {
        fn rebuild_body_history(&self, _: &Path) -> Result<(), JournalMaintenanceError> {
            Ok(())
        }
        fn full_scan(&self, _: &Path) -> Result<(), JournalMaintenanceError> {
            Ok(())
        }
    }
    fn output(stdout: String) -> ToolOutput {
        ToolOutput {
            returncode: 0,
            stdout: stdout.into_bytes(),
            stderr: vec![],
        }
    }

    fn output_with_code(returncode: i32, stdout: String) -> ToolOutput {
        ToolOutput {
            returncode,
            stdout: stdout.into_bytes(),
            stderr: vec![],
        }
    }

    fn write_eligible_sidecar(raw: &Path) {
        let size = raw.metadata().unwrap().len();
        let header = json!({"_solstone_processing": {
            "schema":"solstone.processing.v1",
            "state":"empty",
            "reason_code":"no_decodable_frames",
            "handler":"describe",
            "attempted_at":"2026-01-01T00:00:00Z",
            "input_size":size
        }});
        fs::write(raw.with_extension("jsonl"), format!("{header}\n")).unwrap();
    }

    /// Same shape as `write_eligible_sidecar`, but the `MediaClass::NoDecodableAudio`
    /// side: `.flac` extension and `no_decodable_audio` reason code, so `retention`'s
    /// `empty_audio_rule` (not the ordinary-media default rule) is what a policy
    /// check against this file consults.
    fn write_empty_audio_sidecar(raw: &Path) {
        let size = raw.metadata().unwrap().len();
        let header = json!({"_solstone_processing": {
            "schema":"solstone.processing.v1",
            "state":"empty",
            "reason_code":"no_decodable_audio",
            "handler":"transcribe",
            "attempted_at":"2026-01-01T00:00:00Z",
            "input_size":size
        }});
        fs::write(raw.with_extension("jsonl"), format!("{header}\n")).unwrap();
    }

    fn configure_offload(journal: &Path, now: i64, budget_bytes: Option<u64>) {
        set_destination(
            journal,
            &Destination {
                repository: "s3:bucket/prefix".into(),
                backend: "s3".into(),
                credentials:
                    serde_json::json!({"access_key_id":"ACCESS","secret_access_key":"SECRET"})
                        .as_object()
                        .unwrap()
                        .clone(),
            },
        )
        .unwrap();
        generate_and_store_keys(journal).unwrap();
        set_enabled(journal, true).unwrap();
        record_backup_result(journal, "ok", json!(now), json!("ready"), Value::Null, None).unwrap();
        record_verification_result(journal, "ok", json!(now), Value::Null, json!("1/52")).unwrap();
        set_offload(
            journal,
            &serde_json::json!({"enabled":true,"budget_bytes":budget_bytes,"floor_bytes":1})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
    }
    fn services<'a>(
        runner: &'a Script,
        http: &'a Http,
        clock: &'a TestClock,
        maintenance: &'a Maintenance,
    ) -> BackupServices<'a> {
        BackupServices {
            runner,
            http,
            clock,
            restic_path: Some(Path::new("/fixture/bin/restic")),
            rclone_path: None,
            version: "test",
            journal_maintenance: maintenance,
        }
    }

    fn successful_offload(
        floor_bytes: u64,
    ) -> (tempfile::TempDir, Vec<(PathBuf, Vec<u8>)>, OffloadResult) {
        let journal = tempfile::tempdir().unwrap();
        let first = journal
            .path()
            .join("chronicle/20260101/010000_001/raw.webm");
        let second = journal
            .path()
            .join("chronicle/20260102/020000_002/raw.webm");
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        fs::write(&first, b"one").unwrap();
        fs::write(&second, b"0123456").unwrap();
        write_eligible_sidecar(&first);
        write_eligible_sidecar(&second);
        let first_bytes = fs::read(&first).unwrap();
        let second_bytes = fs::read(&second).unwrap();

        configure_offload(journal.path(), 100, Some(1));
        // Marking releases no bytes. A huge floor would suppress work if this
        // run path consulted it, so this fixture pins that it deliberately does not.
        set_offload(
            journal.path(),
            &serde_json::json!({"enabled":true,"budget_bytes":1,"floor_bytes":floor_bytes})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();

        let nodes = json!([
            {"message_type":"snapshot","id":"snapshot"},
            {"message_type":"node","path":first.display().to_string(),"size":3},
            {"message_type":"node","path":second.display().to_string(),"size":7}
        ]);
        let runner = Script {
            outputs: RefCell::new(VecDeque::from([
                output(String::new()),
                output("[{\"message_type\":\"summary\",\"snapshot_id\":\"snapshot\"}]".into()),
                output(nodes.to_string()),
                output(String::new()),
                output("[{\"message_type\":\"summary\",\"snapshot_id\":\"snapshot\"}]".into()),
                output(nodes.to_string()),
            ])),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock { now: 100 };
        let maintenance = Maintenance;
        let result = run_offload(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
            false,
        );

        (
            journal,
            vec![(first, first_bytes), (second, second_bytes)],
            result,
        )
    }

    fn audit_rows(journal: &Path, day: chrono::NaiveDate) -> Vec<Value> {
        let snapshot = catalog_oplogs(JournalRoot::open(journal).unwrap(), &[day]).unwrap();
        let entries = snapshot
            .entries()
            .iter()
            .filter(|entry| {
                entry.name().source().display_slug() == "offload"
                    && entry.name().run().display_slug() == "raw-media-offload"
                    && entry.name().format() == OplogFormat::Jsonl
            })
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        read_jsonl_with_report::<Value>(
            journal
                .join("chronicle")
                .join(entries[0].day())
                .join("health")
                .join(entries[0].leaf()),
            Vec::new(),
            MalformedPolicy::Raise,
        )
        .unwrap()
        .records
        .into_iter()
        .map(|record| record.value)
        .collect()
    }

    #[test]
    fn floor_is_ignored_when_marking_raw_media() {
        // Marking releases no bytes. A huge floor would suppress work if this
        // run path consulted it, so this fixture pins that it deliberately does not.
        let (_journal, _files, result) = successful_offload(999_999_999_999);

        assert_eq!(result.status, "ok");
        assert_eq!(result.bytes_marked, 10);
        assert_eq!(result.details.len(), 2);
    }

    #[test]
    fn successful_offload_keeps_distinct_raw_media_bytes() {
        let (_journal, files, result) = successful_offload(1);

        assert_eq!(result.status, "ok");
        // Success is pending owner-approved release, never deletion of raw media.
        for (path, expected) in files {
            assert_eq!(fs::read(path).unwrap(), expected);
        }
    }

    #[test]
    fn successful_offload_writes_per_segment_rows_to_one_invocation_oplog() {
        let (journal, _, result) = successful_offload(1);

        assert_eq!(result.audit_recording_failure, None);
        let rows = audit_rows(journal.path(), local_instant(100).unwrap().date_naive());
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| {
            row["event"] == "raw_media_offload"
                && row["kind"] == "raw_media_offload"
                && row["outcome"] == "success"
                && row["message"]
                    .as_str()
                    .is_some_and(|message| message.starts_with("raw-media offload:"))
        }));
        let mut days = rows
            .iter()
            .map(|row| row["day"].as_str().unwrap())
            .collect::<Vec<_>>();
        days.sort_unstable();
        assert_eq!(days, ["20260101", "20260102"]);
    }

    #[test]
    fn partial_pruning_audit_failure_does_not_change_success() {
        let journal = tempfile::tempdir().unwrap();
        let raw = journal
            .path()
            .join("chronicle/20260103/030000_003/audit.webm");
        fs::create_dir_all(raw.parent().unwrap()).unwrap();
        fs::write(&raw, b"audit").unwrap();
        write_eligible_sidecar(&raw);
        let clock = TestClock { now: 1_767_225_600 };
        configure_offload(journal.path(), clock.now, Some(1));
        let audit_day = local_instant(clock.now)
            .unwrap()
            .date_naive()
            .format("%Y%m%d")
            .to_string();
        let audit_parent = journal.path().join("chronicle").join(&audit_day);
        fs::create_dir_all(&audit_parent).unwrap();
        // A file where the invocation's health directory belongs makes canonical
        // oplog creation fail, but a completed archive and mark must still succeed.
        fs::write(audit_parent.join("health"), b"blocked").unwrap();

        let nodes = json!([
            {"message_type":"snapshot","id":"snapshot"},
            {"message_type":"node","path":raw.display().to_string(),"size":5}
        ]);
        let runner = Script {
            outputs: RefCell::new(VecDeque::from([
                output(String::new()),
                output("[{\"message_type\":\"summary\",\"snapshot_id\":\"snapshot\"}]".into()),
                output(nodes.to_string()),
            ])),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let maintenance = Maintenance;
        let result = run_offload(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
            false,
        );

        assert_eq!(result.status, "ok");
        assert_eq!(result.files_marked, 1);
        assert_eq!(result.bytes_marked, 5);
        assert!(
            result
                .audit_recording_failure
                .as_deref()
                .is_some_and(|error| error.starts_with("failed to create offload audit oplog:"))
        );
        assert!(format_offload_result(&result).contains("audit_recording_failed="));
    }

    #[test]
    fn second_run_skips_current_marks_without_rearchiving() {
        let (journal, _, first) = successful_offload(1);
        assert_eq!(first.status, "ok");
        let runner = Script {
            outputs: RefCell::new(VecDeque::new()),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock { now: 100 };
        let maintenance = Maintenance;

        let second = run_offload(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
            false,
        );

        assert_eq!(second.status, "ok");
        assert_eq!(second.files_marked, 0);
        assert_eq!(second.bytes_marked, 0);
        assert_eq!(second.files_already_marked, 2);
        assert_eq!(second.bytes_already_marked, 10);
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn a_stale_offload_mark_is_declined_when_the_owner_turns_on_keep() {
        // media-lifecycle candidate 2: an OffloadRawRelease mark
        // must not keep looking approvable once retention policy would no
        // longer release it. `.flac` + `no_decodable_audio` puts this segment on
        // the empty-audio side of the policy split, not the ordinary-media side.
        let journal = tempfile::tempdir().unwrap();
        let raw = journal
            .path()
            .join("chronicle/20260101/010000_001/audio.flac");
        fs::create_dir_all(raw.parent().unwrap()).unwrap();
        fs::write(&raw, b"silence").unwrap();
        write_empty_audio_sidecar(&raw);
        let raw_bytes = fs::read(&raw).unwrap();

        configure_offload(journal.path(), 100, Some(1));

        let nodes = json!([
            {"message_type":"snapshot","id":"snapshot"},
            {"message_type":"node","path":raw.display().to_string(),"size":7}
        ]);
        let runner = Script {
            outputs: RefCell::new(VecDeque::from([
                output(String::new()),
                output("[{\"message_type\":\"summary\",\"snapshot_id\":\"snapshot\"}]".into()),
                output(nodes.to_string()),
            ])),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock { now: 100 };
        let maintenance = Maintenance;
        let first = run_offload(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
            false,
        );
        assert_eq!(first.status, "ok");
        assert_eq!(first.files_marked, 1);
        assert_eq!(
            load_marks(journal.path()).unwrap().marks.len(),
            1,
            "the segment should be marked, pending owner approval"
        );

        // The owner turns on "keep audio with no speech" between runs. Merge
        // into the existing config rather than replacing it wholesale --
        // `configure_offload` already wrote the backup/destination settings the
        // second run still needs to reach the reconciliation code at all.
        let config_path = journal.path().join("config/journal.json");
        let mut config: serde_json::Map<String, Value> =
            serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        config.insert("retention".to_owned(), json!({"empty_audio": "keep"}));
        fs::write(&config_path, serde_json::to_string(&config).unwrap()).unwrap();

        let second_runner = Script {
            outputs: RefCell::new(VecDeque::new()),
            calls: RefCell::new(vec![]),
        };
        let second = run_offload(
            journal.path(),
            &services(&second_runner, &http, &clock, &maintenance),
            false,
        );

        assert_eq!(second.status, "ok");
        assert_eq!(
            second.files_marked, 0,
            "declining a stale mark must not archive anything new"
        );
        assert!(
            second_runner.calls.borrow().is_empty(),
            "declining a stale mark must not touch the archive tool at all"
        );
        assert!(
            load_marks(journal.path()).unwrap().marks.is_empty(),
            "the stale mark must be withdrawn once policy says keep"
        );
        // Declining a mark never touches the byte it was pending on.
        assert_eq!(fs::read(&raw).unwrap(), raw_bytes);
    }

    #[test]
    fn a_still_eligible_offload_mark_survives_reconciliation() {
        // Negative twin of the test above: an unrelated ordinary-media mark
        // (not empty-audio) must not be declined just because reconciliation ran.
        let (journal, files, first) = successful_offload(1);
        assert_eq!(first.status, "ok");
        assert_eq!(load_marks(journal.path()).unwrap().marks.len(), 2);

        let second_runner = Script {
            outputs: RefCell::new(VecDeque::new()),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock { now: 100 };
        let maintenance = Maintenance;
        let second = run_offload(
            journal.path(),
            &services(&second_runner, &http, &clock, &maintenance),
            false,
        );

        assert_eq!(second.status, "ok");
        assert_eq!(
            load_marks(journal.path()).unwrap().marks.len(),
            2,
            "still-eligible marks must survive a reconciliation pass"
        );
        for (path, expected) in files {
            assert_eq!(fs::read(path).unwrap(), expected);
        }
    }

    #[test]
    fn existing_marks_satisfy_the_budget_before_new_archives() {
        let journal = tempfile::tempdir().unwrap();
        let marked = journal
            .path()
            .join("chronicle/20260101/010000_001/marked.webm");
        let pending = journal
            .path()
            .join("chronicle/20260102/020000_002/pending.webm");
        fs::create_dir_all(marked.parent().unwrap()).unwrap();
        fs::create_dir_all(pending.parent().unwrap()).unwrap();
        fs::write(&marked, b"marked").unwrap();
        fs::write(&pending, b"pending").unwrap();
        write_eligible_sidecar(&marked);
        write_eligible_sidecar(&pending);
        configure_offload(journal.path(), 100, Some(7));
        upsert_offload(
            journal.path(),
            &Target {
                day: "20260101".into(),
                stream: "default".into(),
                dir: "010000_001".into(),
            },
            vec!["marked.webm".into()],
            6,
            "restic-snapshot:existing".into(),
            DateTime::<Utc>::from_timestamp(100, 0).unwrap(),
        )
        .unwrap();
        let runner = Script {
            outputs: RefCell::new(VecDeque::new()),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock { now: 100 };
        let maintenance = Maintenance;

        let result = run_offload(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
            false,
        );

        // 13 bytes raw minus the 6-byte current mark is within the 7-byte budget.
        assert_eq!(result.status, "ok");
        assert!(!result.ran_out_of_markable_media);
        assert_eq!(result.bytes_already_marked, 6);
        assert_eq!(result.files_marked, 0);
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn last_offload_records_mid_loop_budget_satisfaction() {
        let journal = tempfile::tempdir().unwrap();
        let first = journal
            .path()
            .join("chronicle/20260101/010000_001/raw.webm");
        let second = journal
            .path()
            .join("chronicle/20260102/020000_002/raw.webm");
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        fs::write(&first, b"one").unwrap();
        fs::write(&second, b"0123456").unwrap();
        write_eligible_sidecar(&first);
        write_eligible_sidecar(&second);
        configure_offload(journal.path(), 100, Some(7));
        let nodes = json!([
            {"message_type":"snapshot","id":"snapshot"},
            {"message_type":"node","path":first.display().to_string(),"size":3}
        ]);
        let runner = Script {
            outputs: RefCell::new(VecDeque::from([
                output(String::new()),
                output("[{\"message_type\":\"summary\",\"snapshot_id\":\"snapshot\"}]".into()),
                output(nodes.to_string()),
            ])),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock { now: 100 };
        let maintenance = Maintenance;
        let result = run_offload(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
            false,
        );

        assert_eq!(result.status, "ok");
        assert_eq!(result.files_marked, 1);
        assert_eq!(result.bytes_marked, 3);
        assert!(!result.ran_out_of_markable_media);
        let register = load_marks(journal.path()).unwrap();
        assert_eq!(register.marks.len(), 1);
        let mark = register.marks.values().next().unwrap();
        assert_eq!(mark.proposal.names.len(), 1);
        assert_eq!(mark.proposal.bytes, 3);
        let last = get_backup_config(journal.path()).unwrap()["last_offload"].clone();
        assert_eq!(last["status"], "ok");
        assert_eq!(last["files_marked"], 1);
        assert_eq!(last["bytes_marked"], 3);
        assert_eq!(last["reason"], OFFLOAD_OK_BUDGET_SATISFIED);
        assert_eq!(last["ran_out_of_markable_media"], false);
    }

    fn last_offload(journal: &Path) -> Value {
        get_backup_config(journal).unwrap()["last_offload"].clone()
    }

    fn raw_config_bytes(journal: &Path) -> Vec<u8> {
        fs::read(journal.join("config/journal.json")).unwrap()
    }

    fn raw_config(journal: &Path) -> Value {
        serde_json::from_slice(&raw_config_bytes(journal)).unwrap()
    }

    fn raw_last_ok_time(journal: &Path) -> Value {
        raw_config(journal)["backup"]["last_offload"]["last_ok_time"].clone()
    }

    fn marks_path(journal: &Path) -> PathBuf {
        journal.join("health/retention-marks.json")
    }

    fn assert_dry_run_wrote_nothing(journal: &Path, before: &[u8]) {
        assert_eq!(raw_config_bytes(journal), before);
        assert!(!marks_path(journal).exists());
    }

    fn run_offload_dry(journal: &Path) -> OffloadResult {
        let runner = Script {
            outputs: RefCell::new(VecDeque::new()),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock { now: 100 };
        let maintenance = Maintenance;
        run_offload(
            journal,
            &services(&runner, &http, &clock, &maintenance),
            true,
        )
    }

    fn seed_last_ok(journal: &Path, last_ok: i64) {
        record_offload_result(
            journal,
            "ok",
            json!(last_ok),
            Value::Null,
            json!(0),
            json!(0),
            json!(false),
        )
        .unwrap();
    }

    fn eligible_pair(journal: &Path) -> (PathBuf, PathBuf) {
        let first = journal.join("chronicle/20260101/010000_001/raw.webm");
        let second = journal.join("chronicle/20260102/020000_002/raw.webm");
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        fs::write(&first, b"one").unwrap();
        fs::write(&second, b"0123456").unwrap();
        write_eligible_sidecar(&first);
        write_eligible_sidecar(&second);
        (first, second)
    }

    #[cfg(unix)]
    struct RestoreMode {
        path: PathBuf,
        previous: fs::Permissions,
    }

    #[cfg(unix)]
    impl RestoreMode {
        fn chmod(path: &Path, mode: u32) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let previous = fs::metadata(path).unwrap().permissions();
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
            Self {
                path: path.to_path_buf(),
                previous,
            }
        }
    }

    #[cfg(unix)]
    impl Drop for RestoreMode {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.path, self.previous.clone());
        }
    }

    #[test]
    fn last_offload_records_skipped_when_disabled() {
        let journal = tempfile::tempdir().unwrap();
        configure_offload(journal.path(), 100, Some(1));
        set_offload(
            journal.path(),
            &serde_json::json!({"enabled":false,"budget_bytes":1,"floor_bytes":1})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
        let runner = Script {
            outputs: RefCell::new(VecDeque::new()),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock { now: 100 };
        let maintenance = Maintenance;
        let result = run_offload(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
            false,
        );
        assert_eq!(result.status, "skipped");
        assert_eq!(result.reason, None);
        let last = last_offload(journal.path());
        assert_eq!(last["status"], "skipped");
        assert_eq!(last["time"], 100);
        assert!(last["reason"].is_null());
    }

    #[test]
    fn budget_uses_checked_local_media_and_refuses_incomplete_coverage() {
        fn checked_run(journal: &Path) -> OffloadResult {
            let runner = Script {
                outputs: RefCell::new(VecDeque::new()),
                calls: RefCell::new(vec![]),
            };
            let result = run_offload(
                journal,
                &services(&runner, &Http, &TestClock { now: 100 }, &Maintenance),
                false,
            );
            assert!(runner.calls.borrow().is_empty());
            result
        }
        for populated in [false, true] {
            let journal = tempfile::tempdir().unwrap();
            configure_offload(journal.path(), 100, Some(3));
            let raw = journal
                .path()
                .join("chronicle/20260101/work/120000_60/raw.webm");
            if populated {
                fs::create_dir_all(raw.parent().unwrap()).unwrap();
                fs::write(&raw, b"raw").unwrap();
                fs::write(
                    raw.with_file_name("notes.txt"),
                    b"not owner media; larger than budget",
                )
                .unwrap();
            }
            let result = checked_run(journal.path());
            assert_eq!(result.status, "ok");
            assert_eq!(
                result.reason.as_deref(),
                Some(OFFLOAD_OK_BUDGET_ALREADY_SATISFIED)
            );
            assert_eq!(result.files_marked, 0);
            assert!(!marks_path(journal.path()).exists());
            if populated {
                assert_eq!(fs::read(&raw).unwrap(), b"raw");
                let duplicate = raw.parent().unwrap().with_file_name("120000_60_duplicate");
                fs::create_dir(&duplicate).unwrap();
                fs::write(duplicate.join("raw.webm"), b"raw").unwrap();
                configure_offload(journal.path(), 100, Some(6));
                let result = checked_run(journal.path());
                assert_eq!(
                    result.reason.as_deref(),
                    Some(OFFLOAD_OK_BUDGET_ALREADY_SATISFIED)
                );
                assert!(!marks_path(journal.path()).exists());
            }
        }
        for budget in [None, Some(i64::MAX as u64)] {
            let journal = tempfile::tempdir().unwrap();
            configure_offload(journal.path(), 100, budget);
            let chronicle = journal.path().join("chronicle");
            fs::write(&chronicle, b"unchanged non-directory").unwrap();
            let result = checked_run(journal.path());
            assert_eq!(result.status, "stalled");
            assert_eq!(
                result.reason.as_deref(),
                Some(OFFLOAD_STALL_UNEXPECTED_ERROR)
            );
            assert_eq!(result.files_marked, 0);
            assert!(!marks_path(journal.path()).exists());
            assert_eq!(fs::read(chronicle).unwrap(), b"unchanged non-directory");
        }
    }

    #[test]
    fn last_offload_records_budget_already_satisfied_at_entry() {
        let journal = tempfile::tempdir().unwrap();
        eligible_pair(journal.path());
        configure_offload(journal.path(), 100, Some(999_999));
        let runner = Script {
            outputs: RefCell::new(VecDeque::new()),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock { now: 100 };
        let maintenance = Maintenance;
        let result = run_offload(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
            false,
        );
        assert_eq!(result.status, "ok");
        assert_eq!(
            result.reason.as_deref(),
            Some(OFFLOAD_OK_BUDGET_ALREADY_SATISFIED)
        );
        assert!(!result.ran_out_of_markable_media);
        let last = last_offload(journal.path());
        assert_eq!(last["status"], "ok");
        assert_eq!(last["reason"], OFFLOAD_OK_BUDGET_ALREADY_SATISFIED);
        assert_eq!(last["files_marked"], 0);
        assert_eq!(last["ran_out_of_markable_media"], false);
    }

    #[test]
    fn last_offload_records_markable_media_exhausted() {
        let (journal, _, result) = successful_offload(1);
        assert_eq!(result.status, "ok");
        assert_eq!(
            result.reason.as_deref(),
            Some(OFFLOAD_OK_MARKABLE_MEDIA_EXHAUSTED)
        );
        assert!(result.ran_out_of_markable_media);
        let last = last_offload(journal.path());
        assert_eq!(last["status"], "ok");
        assert_eq!(last["reason"], OFFLOAD_OK_MARKABLE_MEDIA_EXHAUSTED);
        assert_eq!(last["files_marked"], 2);
        assert_eq!(last["bytes_marked"], 10);
        assert_eq!(last["ran_out_of_markable_media"], true);
    }

    #[test]
    fn last_offload_records_stall_after_a_mark_and_preserves_last_ok_time() {
        let journal = tempfile::tempdir().unwrap();
        let (first, second) = eligible_pair(journal.path());
        configure_offload(journal.path(), 100, Some(1));
        seed_last_ok(journal.path(), 50);
        let prior = raw_last_ok_time(journal.path());
        assert_eq!(prior, 50);
        let first_nodes = json!([
            {"message_type":"snapshot","id":"snapshot"},
            {"message_type":"node","path":first.display().to_string(),"size":3}
        ]);
        let runner = Script {
            outputs: RefCell::new(VecDeque::from([
                output(String::new()),
                output("[{\"message_type\":\"summary\",\"snapshot_id\":\"snapshot\"}]".into()),
                output(first_nodes.to_string()),
                output(String::new()),
                output("[{\"message_type\":\"summary\",\"snapshot_id\":\"snapshot\"}]".into()),
                output_with_code(1, String::new()),
            ])),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock { now: 100 };
        let maintenance = Maintenance;
        let result = run_offload(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
            false,
        );
        assert_eq!(result.status, "stalled");
        assert_eq!(
            result.reason.as_deref(),
            Some(OFFLOAD_STALL_CONFIRM_TOOL_FAILED)
        );
        assert_eq!(result.files_marked, 1);
        assert_eq!(result.bytes_marked, 3);
        assert_eq!(load_marks(journal.path()).unwrap().marks.len(), 1);
        let last = last_offload(journal.path());
        assert_eq!(last["status"], "stalled");
        assert_eq!(last["reason"], OFFLOAD_STALL_CONFIRM_TOOL_FAILED);
        assert_eq!(last["files_marked"], 1);
        assert_eq!(last["bytes_marked"], 3);
        assert_eq!(last["last_ok_time"], prior);
        assert_eq!(raw_last_ok_time(journal.path()), prior);
        let _ = second;
    }

    #[test]
    fn no_offload_run_reason_code_reaches_the_owner_as_an_identifier() {
        const EVERY_REASON: &[&str] = &[
            OFFLOAD_STALL_BACKUP_NOT_READY,
            OFFLOAD_STALL_BACKUP_FAILING,
            OFFLOAD_STALL_VERIFICATION_MISSING,
            OFFLOAD_STALL_VERIFICATION_OVERDUE,
            OFFLOAD_STALL_VERIFICATION_FAILED,
            OFFLOAD_STALL_LOCKED,
            OFFLOAD_STALL_ARCHIVE_FAILED,
            OFFLOAD_STALL_CONFIRM_FAILED,
            OFFLOAD_STALL_CONFIRM_TOOL_FAILED,
            OFFLOAD_STALL_SEGMENT_IDENTITY,
            OFFLOAD_STALL_UNEXPECTED_ERROR,
            OFFLOAD_OK_BUDGET_ALREADY_SATISFIED,
            OFFLOAD_OK_BUDGET_SATISFIED,
            OFFLOAD_OK_MARKABLE_MEDIA_EXHAUSTED,
            "restic_unavailable",
            "rclone_unavailable",
        ];
        for reason in EVERY_REASON {
            let rendered = offload_reason_text(reason);
            assert_ne!(rendered, *reason, "{reason} reaches the owner verbatim");
        }
        // ⛔ An unmapped code still reaches support rather than being swallowed.
        let unmapped = offload_reason_text("some_new_code");
        assert!(unmapped.contains("some_new_code"), "{unmapped}");
    }

    #[test]
    fn ok_reason_tokens_are_pairwise_distinct_and_match_ran_out() {
        assert_ne!(
            OFFLOAD_OK_BUDGET_ALREADY_SATISFIED,
            OFFLOAD_OK_BUDGET_SATISFIED
        );
        assert_ne!(
            OFFLOAD_OK_BUDGET_ALREADY_SATISFIED,
            OFFLOAD_OK_MARKABLE_MEDIA_EXHAUSTED
        );
        assert_ne!(
            OFFLOAD_OK_BUDGET_SATISFIED,
            OFFLOAD_OK_MARKABLE_MEDIA_EXHAUSTED
        );
        let journal = tempfile::tempdir().unwrap();
        configure_offload(journal.path(), 100, Some(1));
        seed_last_ok(journal.path(), 50);
        let prior = raw_last_ok_time(journal.path());
        record_verification_result(
            journal.path(),
            "ok",
            json!(100 - VERIFICATION_MAX_AGE_SECONDS - 1),
            Value::Null,
            json!("1/52"),
        )
        .unwrap();
        let runner = Script {
            outputs: RefCell::new(VecDeque::new()),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock { now: 100 };
        let maintenance = Maintenance;
        let result = run_offload(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
            false,
        );
        assert_eq!(result.status, "stalled");
        assert_eq!(
            result.reason.as_deref(),
            Some(OFFLOAD_STALL_VERIFICATION_OVERDUE)
        );
        let last = last_offload(journal.path());
        assert_eq!(last["status"], "stalled");
        assert_eq!(last["reason"], OFFLOAD_STALL_VERIFICATION_OVERDUE);
        assert_eq!(last["last_ok_time"], prior);
        assert_eq!(raw_last_ok_time(journal.path()), prior);
    }

    #[test]
    fn dry_run_does_not_write_last_offload_for_reachable_outcomes() {
        {
            let journal = tempfile::tempdir().unwrap();
            configure_offload(journal.path(), 100, Some(1));
            set_offload(
                journal.path(),
                &serde_json::json!({"enabled":false,"budget_bytes":1,"floor_bytes":1})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .unwrap();
            let before = raw_config_bytes(journal.path());
            let result = run_offload_dry(journal.path());
            assert_eq!(result.status, "skipped");
            assert_eq!(result.reason, None);
            assert_dry_run_wrote_nothing(journal.path(), &before);
        }
        {
            let journal = tempfile::tempdir().unwrap();
            configure_offload(journal.path(), 100, Some(1));
            record_verification_result(
                journal.path(),
                "ok",
                json!(100 - VERIFICATION_MAX_AGE_SECONDS - 1),
                Value::Null,
                json!("1/52"),
            )
            .unwrap();
            let before = raw_config_bytes(journal.path());
            let result = run_offload_dry(journal.path());
            assert_eq!(result.status, "stalled");
            assert_eq!(
                result.reason.as_deref(),
                Some(OFFLOAD_STALL_VERIFICATION_OVERDUE)
            );
            assert_dry_run_wrote_nothing(journal.path(), &before);
        }
        {
            let journal = tempfile::tempdir().unwrap();
            eligible_pair(journal.path());
            configure_offload(journal.path(), 100, Some(999_999));
            let before = raw_config_bytes(journal.path());
            let result = run_offload_dry(journal.path());
            assert_eq!(result.status, "ok");
            assert_eq!(
                result.reason.as_deref(),
                Some(OFFLOAD_OK_BUDGET_ALREADY_SATISFIED)
            );
            assert_eq!(result.files_marked, 0);
            assert!(!result.ran_out_of_markable_media);
            assert_dry_run_wrote_nothing(journal.path(), &before);
        }
        {
            let journal = tempfile::tempdir().unwrap();
            eligible_pair(journal.path());
            configure_offload(journal.path(), 100, Some(7));
            let before = raw_config_bytes(journal.path());
            let result = run_offload_dry(journal.path());
            assert_eq!(result.status, "ok");
            assert_eq!(result.reason.as_deref(), Some(OFFLOAD_OK_BUDGET_SATISFIED));
            assert_eq!(result.files_marked, 0);
            assert_eq!(result.details.len(), 1);
            assert!(!result.ran_out_of_markable_media);
            assert_dry_run_wrote_nothing(journal.path(), &before);
        }
        {
            let journal = tempfile::tempdir().unwrap();
            eligible_pair(journal.path());
            configure_offload(journal.path(), 100, Some(1));
            let before = raw_config_bytes(journal.path());
            let result = run_offload_dry(journal.path());
            assert_eq!(result.status, "ok");
            assert_eq!(
                result.reason.as_deref(),
                Some(OFFLOAD_OK_MARKABLE_MEDIA_EXHAUSTED)
            );
            assert_eq!(result.files_marked, 0);
            assert_eq!(result.details.len(), 2);
            assert!(result.ran_out_of_markable_media);
            assert_dry_run_wrote_nothing(journal.path(), &before);
        }
    }

    #[cfg(unix)]
    #[test]
    fn config_read_failure_stalls_without_last_offload() {
        let journal = tempfile::tempdir().unwrap();
        configure_offload(journal.path(), 100, Some(1));
        let config_path = journal.path().join("config/journal.json");
        let _restore = RestoreMode::chmod(&config_path, 0o000);
        let runner = Script {
            outputs: RefCell::new(VecDeque::new()),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock { now: 100 };
        let maintenance = Maintenance;
        let result = run_offload(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
            false,
        );
        assert_eq!(result.status, "stalled");
        assert_eq!(
            result.reason.as_deref(),
            Some(OFFLOAD_STALL_UNEXPECTED_ERROR)
        );
        drop(_restore);
        let backup = &raw_config(journal.path())["backup"];
        assert!(backup.get("last_offload").is_none(), "{backup:?}");
    }

    #[cfg(unix)]
    #[test]
    fn recording_failure_after_marks_when_config_dir_is_unwritable() {
        let journal = tempfile::tempdir().unwrap();
        let (first, second) = eligible_pair(journal.path());
        configure_offload(journal.path(), 100, Some(1));
        let first_nodes = json!([
            {"message_type":"snapshot","id":"snapshot"},
            {"message_type":"node","path":first.display().to_string(),"size":3},
            {"message_type":"node","path":second.display().to_string(),"size":7}
        ]);
        let runner = Script {
            outputs: RefCell::new(VecDeque::from([
                output(String::new()),
                output("[{\"message_type\":\"summary\",\"snapshot_id\":\"snapshot\"}]".into()),
                output(first_nodes.to_string()),
                output(String::new()),
                output("[{\"message_type\":\"summary\",\"snapshot_id\":\"snapshot\"}]".into()),
                output(first_nodes.to_string()),
            ])),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock { now: 100 };
        let maintenance = Maintenance;
        let config_dir = journal.path().join("config");
        let before = raw_config_bytes(journal.path());
        let _restore = RestoreMode::chmod(&config_dir, 0o555);
        let result = run_offload(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
            false,
        );
        assert_eq!(result.status, "ok");
        assert!(result.files_marked > 0);
        assert!(result.recording_failure.is_some());
        assert_eq!(
            result.reason.as_deref(),
            Some(OFFLOAD_OK_MARKABLE_MEDIA_EXHAUSTED)
        );
        assert!(
            format_offload_result(&result).contains("recording_failed="),
            "{}",
            format_offload_result(&result)
        );
        assert!(marks_path(journal.path()).exists());
        assert_eq!(load_marks(journal.path()).unwrap().marks.len(), 2);
        drop(_restore);
        assert_eq!(raw_config_bytes(journal.path()), before);
    }

    #[test]
    fn offload_mark_uses_rfc3339_from_clock() {
        let (journal, _, result) = successful_offload(1);
        assert_eq!(result.status, "ok");
        let register = load_marks(journal.path()).unwrap();
        for mark in register.marks.values() {
            assert_eq!(mark.marked_at, "1970-01-01T00:01:40Z");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn identity_stall_keeps_reason_token_and_carries_detail() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let journal = tempfile::tempdir().unwrap();
        let raw = journal
            .path()
            .join("chronicle/20260101/010000_001/raw.webm");
        fs::create_dir_all(raw.parent().unwrap()).unwrap();
        fs::write(&raw, b"raw-bytes").unwrap();
        write_eligible_sidecar(&raw);
        let unreadable = journal
            .path()
            .join("chronicle/20260101")
            .join(OsStr::from_bytes(b"s\xff"))
            .join("020000_002");
        fs::create_dir_all(&unreadable).unwrap();
        fs::write(unreadable.join("raw.webm"), b"other").unwrap();
        configure_offload(journal.path(), 100, Some(1));
        let runner = Script {
            outputs: RefCell::new(VecDeque::new()),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock { now: 100 };
        let maintenance = Maintenance;

        let result = run_offload(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
            true,
        );

        assert_eq!(result.status, "stalled");
        assert_eq!(result.reason.as_deref(), Some("segment_identity"));
        let detail = result.reason_detail.expect("identity detail");
        assert!(detail.contains("not UTF-8 representable"), "{detail}");
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn same_key_on_two_days_does_not_stall_offload() {
        let journal = tempfile::tempdir().unwrap();
        for day in ["20260101", "20260102"] {
            let raw = journal
                .path()
                .join(format!("chronicle/{day}/010000_001/raw.webm"));
            fs::create_dir_all(raw.parent().unwrap()).unwrap();
            fs::write(&raw, b"raw-bytes").unwrap();
            write_eligible_sidecar(&raw);
        }
        configure_offload(journal.path(), 100, Some(1));
        let runner = Script {
            outputs: RefCell::new(VecDeque::new()),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock { now: 100 };
        let maintenance = Maintenance;

        let result = run_offload(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
            true,
        );

        assert_ne!(
            result.reason.as_deref(),
            Some("segment_identity"),
            "{result:?}"
        );
    }

    #[test]
    fn two_directories_sharing_one_key_on_one_day_stall_offload() {
        let journal = tempfile::tempdir().unwrap();
        for name in ["010000_001", "010000_001_copy"] {
            let raw = journal
                .path()
                .join(format!("chronicle/20260101/{name}/raw.webm"));
            fs::create_dir_all(raw.parent().unwrap()).unwrap();
            fs::write(&raw, b"raw-bytes").unwrap();
            write_eligible_sidecar(&raw);
        }
        configure_offload(journal.path(), 100, Some(1));
        let runner = Script {
            outputs: RefCell::new(VecDeque::new()),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock { now: 100 };
        let maintenance = Maintenance;

        let result = run_offload(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
            true,
        );

        assert_eq!(result.status, "stalled");
        assert_eq!(result.reason.as_deref(), Some("segment_identity"));
        let detail = result.reason_detail.expect("identity detail");
        assert!(detail.contains("day 20260101"), "{detail}");
        assert!(
            detail.contains("multiple segments share stream"),
            "{detail}"
        );
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn held_segment_is_not_prepared_archived_or_marked() {
        let journal = tempfile::tempdir().unwrap();
        let raw = journal
            .path()
            .join("chronicle/20260103/030000_003/photo.png");
        fs::create_dir_all(raw.parent().unwrap()).unwrap();
        fs::write(&raw, b"owner-image").unwrap();
        configure_offload(journal.path(), 100, Some(1));
        let runner = Script {
            outputs: RefCell::new(VecDeque::new()),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock { now: 100 };
        let maintenance = Maintenance;

        let result = run_offload(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
            false,
        );

        assert_eq!(result.status, "ok");
        assert!(result.details.is_empty());
        assert_eq!(result.files_marked, 0);
        assert!(runner.calls.borrow().is_empty());
        assert!(!journal.path().join("health/retention-marks.json").exists());
    }

    #[cfg(unix)]
    const CURRENT_NOW: i64 = 1_767_355_200;
    #[cfg(unix)]
    const CURRENT_REPO: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[cfg(unix)]
    struct CurrentFixture {
        journal: tempfile::TempDir,
        root: std::sync::Arc<JournalRoot>,
        marks: Vec<solstone_core_retention::marks::Mark>,
        segment: PathBuf,
        ledger: PathBuf,
    }

    #[cfg(unix)]
    impl CurrentFixture {
        fn new(layout: solstone_core_journal_io::paths::SegmentLayout) -> Self {
            use solstone_core_retention::archive_proof::{
                ArchiveFileProof, OffloadOffer, OffloadProof,
            };
            use solstone_core_retention::marks::{Mark, MarkId, MarkState, Proposal, RemovalClass};
            let journal = tempfile::tempdir().unwrap();
            configure_offload(journal.path(), CURRENT_NOW, Some(1));
            let target = Target {
                day: "20260101".into(),
                stream: "_default".into(),
                dir: "010000_001_exact".into(),
            };
            let relative = match layout {
                solstone_core_journal_io::paths::SegmentLayout::Direct => {
                    "chronicle/20260101/010000_001_exact"
                }
                solstone_core_journal_io::paths::SegmentLayout::Named => {
                    "chronicle/20260101/_default/010000_001_exact"
                }
            };
            let segment = journal.path().join(relative);
            fs::create_dir_all(&segment).unwrap();
            for (name, bytes) in [("first.webm", &b"one"[..]), ("second.webm", &b"four"[..])] {
                let raw = segment.join(name);
                fs::write(&raw, bytes).unwrap();
                write_eligible_sidecar(&raw);
            }
            let unmarked = journal
                .path()
                .join("chronicle/20260101/020000_001/unmarked.webm");
            fs::create_dir_all(unmarked.parent().unwrap()).unwrap();
            fs::write(unmarked, [7; 16]).unwrap();
            let root = std::sync::Arc::new(JournalRoot::open(journal.path()).unwrap());
            let directory = solstone_core_journal_io::media::MediaDirectory::open(
                std::sync::Arc::clone(&root),
                Path::new(relative),
            )
            .unwrap();
            let scan = solstone_core_retention::scan::scan_segment_bound(
                &directory,
                &ClosedHandlerSet,
                &JournalMedia,
                100,
            )
            .unwrap();
            let policy = policy_from_journal_config(&serde_json::Map::new());
            let at = DateTime::from_timestamp(CURRENT_NOW, 0).unwrap();
            let inspected = solstone_core_retention::grouped::inspect_grouped(
                &scan,
                &target,
                layout,
                &policy,
                at.date_naive(),
                at,
                solstone_core_retention::grouped::DecisionPurpose::OffloadProposal,
            );
            assert_eq!(inspected.eligible().count(), 2);
            let files = inspected
                .eligible()
                .map(|file| ArchiveFileProof {
                    witness_id: format!("witness-{}", file.name()),
                    name: file.name().into(),
                    bytes: file.bytes(),
                    sha256: format!(
                        "{:x}",
                        Sha256::digest(fs::read(segment.join(file.name())).unwrap())
                    ),
                    archive_path: format!("/owned/input/{}", file.name()),
                    processing: file.processing().clone(),
                })
                .collect::<Vec<_>>();
            let proof = OffloadProof {
                journal_id: root.identity().observation_id(),
                manifest_id: "current-manifest".into(),
                target: target.clone(),
                layout,
                repository_id: CURRENT_REPO.into(),
                snapshot_id: "b".repeat(64),
                files,
            };
            proof.validate().unwrap();
            let ledger = journal.path().join("health/offload/20260101.jsonl");
            fs::create_dir_all(ledger.parent().unwrap()).unwrap();
            let event = json!({"event_kind":"offload_archive_v2","schema_version":2,"time":CURRENT_NOW,
                "day":target.day,"stream":target.stream,"segment":"010000_001","segment_dir":target.dir,
                "layout":layout,"journal_id":proof.journal_id,"manifest_id":proof.manifest_id,
                "repository_id":proof.repository_id,"snapshot_id":proof.snapshot_id,"files":proof.files});
            fs::write(&ledger, format!("{event}\n")).unwrap();
            let mut marks = Vec::new();
            for count in [2, 1] {
                let mut selected = proof.clone();
                selected.files.truncate(count);
                let names = selected
                    .files
                    .iter()
                    .map(|file| file.name.clone())
                    .collect::<Vec<_>>();
                let bytes = selected.bytes().unwrap();
                marks.push(Mark {
                    id: MarkId::derive(RemovalClass::OffloadRawRelease, &target, &names),
                    class: RemovalClass::OffloadRawRelease,
                    target: target.clone(),
                    marked_at: "2026-01-02T12:00:00Z".into(),
                    proposal: Proposal {
                        bytes,
                        reason: "archived recording".into(),
                        names,
                    },
                    state: MarkState::Marked,
                    offload: Some(OffloadOffer::new(selected).unwrap()),
                });
            }
            let fixture = Self {
                journal,
                root,
                marks,
                segment,
                ledger,
            };
            fixture.save(&fixture.marks);
            fixture
        }
        fn save(&self, marks: &[solstone_core_retention::marks::Mark]) {
            let register = solstone_core_retention::marks::Register {
                version: if marks.iter().any(|mark| mark.offload.is_some()) {
                    2
                } else {
                    1
                },
                marks: marks
                    .iter()
                    .cloned()
                    .map(|mark| (mark.id.clone(), mark))
                    .collect(),
            };
            fs::write(
                self.journal.path().join("health/retention-marks.json"),
                serde_json::to_vec(&register).unwrap(),
            )
            .unwrap();
        }
        fn change_config(&self, change: impl FnOnce(&mut Value)) {
            let path = self.journal.path().join("config/journal.json");
            let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            change(&mut config);
            fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
        }
        fn add_offer(&mut self, proof: solstone_core_retention::archive_proof::OffloadProof) {
            use solstone_core_retention::marks::{Mark, MarkId, MarkState, Proposal, RemovalClass};
            use std::io::Write;
            let target = proof.target.clone();
            let event = json!({"event_kind":"offload_archive_v2","schema_version":2,"time":CURRENT_NOW,
                "day":target.day,"stream":target.stream,"segment":solstone_core_format::segment::segment_key(&target.dir).unwrap(),"segment_dir":target.dir,
                "layout":proof.layout,"journal_id":proof.journal_id,"manifest_id":proof.manifest_id,"repository_id":proof.repository_id,"snapshot_id":proof.snapshot_id,"files":proof.files});
            writeln!(
                fs::OpenOptions::new()
                    .append(true)
                    .open(&self.ledger)
                    .unwrap(),
                "{event}"
            )
            .unwrap();
            let names = proof
                .files
                .iter()
                .map(|file| file.name.clone())
                .collect::<Vec<_>>();
            let bytes = proof.bytes().unwrap();
            self.marks.push(Mark {
                id: MarkId::derive(RemovalClass::OffloadRawRelease, &target, &names),
                class: RemovalClass::OffloadRawRelease,
                target,
                marked_at: "2026-01-02T12:00:00Z".into(),
                proposal: Proposal {
                    bytes,
                    reason: "archived recording".into(),
                    names,
                },
                state: MarkState::Marked,
                offload: Some(
                    solstone_core_retention::archive_proof::OffloadOffer::new(proof).unwrap(),
                ),
            });
            self.save(&self.marks);
        }
        fn observe_offer(
            &self,
            layout: solstone_core_journal_io::paths::SegmentLayout,
            dir: &str,
            names: &[&str],
            manifest: &str,
        ) -> solstone_core_retention::archive_proof::OffloadProof {
            use solstone_core_journal_io::paths::SegmentLayout;
            use solstone_core_retention::archive_proof::{ArchiveFileProof, OffloadProof};
            let target = Target {
                dir: dir.into(),
                ..self.marks[0].target.clone()
            };
            let day = Path::new("chronicle").join(&target.day);
            let relative = match layout {
                SegmentLayout::Direct => day.join(dir),
                SegmentLayout::Named => day.join(&target.stream).join(dir),
            };
            let directory = solstone_core_journal_io::media::MediaDirectory::open(
                std::sync::Arc::clone(&self.root),
                &relative,
            )
            .unwrap();
            let scan = solstone_core_retention::scan::scan_segment_bound(
                &directory,
                &ClosedHandlerSet,
                &JournalMedia,
                100,
            )
            .unwrap();
            let at = DateTime::from_timestamp(CURRENT_NOW, 0).unwrap();
            let grouped = solstone_core_retention::grouped::inspect_grouped(
                &scan,
                &target,
                layout,
                &policy_from_journal_config(&serde_json::Map::new()),
                at.date_naive(),
                at,
                solstone_core_retention::grouped::DecisionPurpose::OffloadProposal,
            );
            let files = names
                .iter()
                .map(|name| {
                    let current = grouped
                        .eligible()
                        .find(|file| file.name() == *name)
                        .unwrap();
                    ArchiveFileProof {
                        witness_id: format!("{manifest}-{name}"),
                        name: (*name).into(),
                        bytes: current.bytes(),
                        sha256: format!(
                            "{:x}",
                            Sha256::digest(
                                fs::read(self.journal.path().join(&relative).join(name)).unwrap()
                            )
                        ),
                        archive_path: format!("/owned/{manifest}/{name}"),
                        processing: current.processing().clone(),
                    }
                })
                .collect();
            let proof = OffloadProof {
                journal_id: self.root.identity().observation_id(),
                manifest_id: manifest.into(),
                target,
                layout,
                repository_id: CURRENT_REPO.into(),
                snapshot_id: "b".repeat(64),
                files,
            };
            proof.validate().unwrap();
            proof
        }
        fn inspect(
            &self,
            expected: &[solstone_core_retention::marks::Mark],
        ) -> CurrentOffloadDecision {
            let runner = Script {
                outputs: RefCell::new(
                    (0..6)
                        .map(|_| output(json!({"id":CURRENT_REPO}).to_string()))
                        .collect(),
                ),
                calls: RefCell::new(vec![]),
            };
            let clock = TestClock { now: CURRENT_NOW };
            let services = services(&runner, &Http, &clock, &Maintenance);
            let mut operation = solstone_core_backup_runtime::admit_archive(
                std::sync::Arc::clone(&self.root),
                &services,
            )
            .unwrap();
            let carrier = std::sync::Arc::clone(operation.carrier());
            let decision = inspect_current_offload(
                &mut operation,
                expected,
                DateTime::from_timestamp(CURRENT_NOW, 0)
                    .unwrap()
                    .fixed_offset(),
            );
            // Each inspection is its own admission; retire its root activity
            // carrier so the next admission's census starts clean.
            drop(operation);
            let retirement = std::sync::Arc::try_unwrap(carrier)
                .unwrap_or_else(|_| panic!("inspection must release its activity carrier"))
                .retire_after_settlement();
            assert_eq!(
                retirement.receipt.effect(),
                solstone_core_journal_io::media::ActivityCarrierRetirementEffect::Removed
            );
            assert!(retirement.receipt.failures().is_empty());
            decision
        }
        fn project(
            &self,
            decision: &CurrentOffloadDecision,
        ) -> Result<Option<crate::measurement::ProposedRelief>, String> {
            crate::measurement::project_current_offload(
                &measure_local_media(self.journal.path()),
                decision,
            )
        }
    }

    #[test]
    #[cfg(unix)]
    fn current_batch_uses_actual_admission_and_hashes_overlaps_once_per_call() {
        use solstone_core_journal_io::paths::SegmentLayout;
        use std::rc::Rc;
        for layout in [SegmentLayout::Direct, SegmentLayout::Named] {
            let fixture = CurrentFixture::new(layout);
            let hashes = Rc::new(RefCell::new(Vec::new()));
            let seen = Rc::clone(&hashes);
            let decision = current::with_checkpoint(
                move |phase, path| {
                    if phase == current::Checkpoint::BeforeHash {
                        seen.borrow_mut().push(path.to_path_buf());
                    }
                },
                || fixture.inspect(&fixture.marks),
            );
            assert_eq!(
                decision.availability(),
                CurrentOffloadAvailability::Available
            );
            assert!(decision.refusals().is_empty());
            assert_eq!(hashes.borrow().len(), 2);
            assert_eq!(decision.eligible().count(), 3);
            let projected = fixture.project(&decision).unwrap().unwrap();
            assert_eq!(projected.local.total_bytes, 23);
            assert_eq!(projected.eligible_bytes, 7);
            assert_eq!(projected.eligible_files, 2);
            assert_eq!(projected.projected_local_bytes, 16);
            assert_eq!(
                decision.root_observation(),
                fixture.root.identity().observation_id()
            );
            for file in decision.eligible() {
                assert_eq!(file.layout(), layout);
                assert_eq!(
                    file.class(),
                    solstone_core_retention::marks::RemovalClass::OffloadRawRelease
                );
                assert_eq!(
                    file.revision(),
                    &file.mark().offload.as_ref().unwrap().revision
                );
                assert!(file.witness_id().starts_with("witness-"));
            }
            let seen = Rc::clone(&hashes);
            let second = current::with_checkpoint(
                move |phase, path| {
                    if phase == current::Checkpoint::BeforeHash {
                        seen.borrow_mut().push(path.to_path_buf());
                    }
                },
                || fixture.inspect(&fixture.marks),
            );
            assert_eq!(fixture.project(&second).unwrap().unwrap().eligible_bytes, 7);
            assert_eq!(hashes.borrow().len(), 4);
        }
    }

    #[test]
    #[cfg(unix)]
    fn current_batch_distinguishes_available_zero_from_unavailable() {
        let fixture = CurrentFixture::new(solstone_core_journal_io::paths::SegmentLayout::Direct);
        let zero = fixture.inspect(&[]);
        assert_eq!(zero.availability(), CurrentOffloadAvailability::Available);
        assert_eq!(zero.eligible().count(), 0);
        let projected = fixture.project(&zero).unwrap().unwrap();
        assert_eq!(projected.eligible_bytes, 0);
        assert_eq!(projected.projected_local_bytes, 23);
        fixture.change_config(|config| config["retention"] = json!([]));
        let unavailable = fixture.inspect(&fixture.marks);
        assert_eq!(
            unavailable.availability(),
            CurrentOffloadAvailability::Unavailable
        );
        assert_eq!(unavailable.eligible().count(), 0);
        assert!(!unavailable.refusals().is_empty());
        assert_eq!(fixture.project(&unavailable).unwrap(), None);
    }

    #[test]
    #[cfg(unix)]
    fn current_batch_refuses_changed_missing_failed_legacy_and_other_classes() {
        use solstone_core_retention::marks::{Failure, MarkId, MarkState, RemovalClass};
        for case in 0..5 {
            let fixture =
                CurrentFixture::new(solstone_core_journal_io::paths::SegmentLayout::Direct);
            let mut selected = fixture.marks[0].clone();
            let reason = match case {
                0 => {
                    let mut changed = selected.clone();
                    changed.proposal.reason = "new review".into();
                    fixture.save(&[changed]);
                    CurrentOffloadRefusalReason::Stale
                }
                1 => {
                    fixture.save(&[]);
                    CurrentOffloadRefusalReason::Missing
                }
                2 => {
                    selected.state = MarkState::Failed(Failure {
                        at: "2026-01-02T12:00:00Z".into(),
                        reason: "interrupted".into(),
                        staged: None,
                    });
                    fixture.save(&[selected.clone()]);
                    CurrentOffloadRefusalReason::Failed
                }
                3 => {
                    selected.offload = None;
                    fixture.save(&[selected.clone()]);
                    CurrentOffloadRefusalReason::Unbound
                }
                _ => {
                    selected.offload = None;
                    selected.class = RemovalClass::PolicyRawRelease;
                    selected.id =
                        MarkId::derive(selected.class, &selected.target, &selected.proposal.names);
                    fixture.save(&[selected.clone()]);
                    CurrentOffloadRefusalReason::WrongClass
                }
            };
            let decision = fixture.inspect(&[selected]);
            assert_eq!(
                decision.availability(),
                CurrentOffloadAvailability::Available,
                "case {case}"
            );
            assert_eq!(decision.eligible().count(), 0, "case {case}");
            let projected = fixture.project(&decision).unwrap().unwrap();
            assert_eq!(projected.local.total_bytes, 23, "case {case}");
            assert_eq!(projected.eligible_bytes, 0, "case {case}");
            assert_eq!(projected.projected_local_bytes, 23, "case {case}");
            assert!(
                decision
                    .refusals()
                    .iter()
                    .any(|refusal| refusal.reason() == &reason),
                "case {case}"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn current_batch_refuses_wrong_bytes_processing_sibling_and_missing_witness() {
        for case in 0..5 {
            let fixture =
                CurrentFixture::new(solstone_core_journal_io::paths::SegmentLayout::Direct);
            match case {
                0 => fs::write(fixture.segment.join("first.webm"), b"bad").unwrap(),
                1 => {
                    let raw = fixture.segment.join("first.webm");
                    write_eligible_sidecar(&raw);
                    let path = raw.with_extension("jsonl");
                    let text = fs::read_to_string(&path)
                        .unwrap()
                        .replace("2026-01-01T00:00:00Z", "2026-01-01T00:00:01Z");
                    fs::write(path, text).unwrap();
                }
                2 => {
                    fs::write(fixture.segment.join("unselected.webm"), b"unproven sibling").unwrap()
                }
                3 => fs::write(&fixture.ledger, b"").unwrap(),
                _ => fs::remove_file(fixture.segment.join("first.webm")).unwrap(),
            }
            let decision = fixture.inspect(&fixture.marks[1..]);
            assert_eq!(
                decision.availability(),
                CurrentOffloadAvailability::Available,
                "case {case}"
            );
            assert_eq!(decision.eligible().count(), 0, "case {case}");
            let projected = fixture.project(&decision).unwrap().unwrap();
            assert_eq!(projected.eligible_bytes, 0, "case {case}");
            assert_eq!(
                projected.projected_local_bytes, projected.local.total_bytes,
                "case {case}"
            );
            if case == 4 {
                assert_eq!(projected.local.total_bytes, 20);
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn current_batch_refuses_kept_empty_independently_from_ordinary_siblings() {
        let mut fixture =
            CurrentFixture::new(solstone_core_journal_io::paths::SegmentLayout::Direct);
        let empty = fixture.segment.join("empty.flac");
        fs::write(&empty, b"empty").unwrap();
        write_empty_audio_sidecar(&empty);
        let proof = fixture.observe_offer(
            solstone_core_journal_io::paths::SegmentLayout::Direct,
            "010000_001_exact",
            &["empty.flac"],
            "empty-manifest",
        );
        fixture.add_offer(proof);
        fixture.change_config(|config| config["transcribe"] = json!({"preserve_all":true}));
        // The unrelated empty class must not hold the proven ordinary offer.
        let decision = fixture.inspect(&fixture.marks);
        assert_eq!(
            decision.availability(),
            CurrentOffloadAvailability::Available
        );
        let projected = fixture.project(&decision).unwrap().unwrap();
        assert_eq!(projected.local.total_bytes, 28);
        assert_eq!(projected.eligible_bytes, 7);
        assert_eq!(projected.projected_local_bytes, 21);
        assert!(
            decision
                .refusals()
                .iter()
                .any(|refusal| matches!(refusal.reason(),
            CurrentOffloadRefusalReason::Kept {names} if names == &["empty.flac"]))
        );
        assert!(!decision.eligible().any(|file| file.name() == "empty.flac"));
        let kept = fixture.inspect(&fixture.marks[2..]);
        let projected = fixture.project(&kept).unwrap().unwrap();
        assert_eq!(projected.eligible_bytes, 0);
        assert_eq!(projected.projected_local_bytes, 28);
    }

    #[test]
    #[cfg(unix)]
    fn current_batch_keeps_same_names_at_distinct_exact_locators() {
        use solstone_core_journal_io::paths::SegmentLayout;
        let mut fixture = CurrentFixture::new(SegmentLayout::Direct);
        // Keep both overlapping Direct offers. The Named two-file offer has a
        // distinct Mark ID while its first file differs only by exact layout.
        for (layout, dir, bytes, manifest) in [
            (
                SegmentLayout::Named,
                "010000_001_exact",
                &b"named"[..],
                "named-manifest",
            ),
            (
                SegmentLayout::Direct,
                "010000_001_other",
                &b"other!"[..],
                "other-manifest",
            ),
        ] {
            let day = fixture.journal.path().join("chronicle/20260101");
            let segment = match layout {
                SegmentLayout::Direct => day.join(dir),
                SegmentLayout::Named => day.join("_default").join(dir),
            };
            fs::create_dir_all(&segment).unwrap();
            let raw = segment.join("first.webm");
            fs::write(&raw, bytes).unwrap();
            write_eligible_sidecar(&raw);
            let names = if layout == SegmentLayout::Named {
                let raw = segment.join("named-only.webm");
                fs::write(&raw, b"x").unwrap();
                write_eligible_sidecar(&raw);
                vec!["first.webm", "named-only.webm"]
            } else {
                vec!["first.webm"]
            };
            fixture.add_offer(fixture.observe_offer(layout, dir, &names, manifest));
        }
        let decision = fixture.inspect(&fixture.marks);
        assert_eq!(
            decision.availability(),
            CurrentOffloadAvailability::Available
        );
        assert!(decision.refusals().is_empty(), "{:?}", decision.refusals());
        assert_eq!(decision.eligible().count(), 6);
        let projected = fixture.project(&decision).unwrap().unwrap();
        assert_eq!(projected.local.total_bytes, 35);
        assert_eq!(projected.eligible_bytes, 19);
        assert_eq!(projected.eligible_files, 5);
        assert_eq!(projected.projected_local_bytes, 16);
        assert_eq!(
            decision
                .eligible()
                .filter(|file| file.name() == "first.webm")
                .count(),
            4
        );
    }

    #[test]
    #[cfg(unix)]
    fn current_batch_refuses_post_group_media_and_processing_drift_without_rehash() {
        use std::rc::Rc;
        for case in 0..4 {
            let fixture =
                CurrentFixture::new(solstone_core_journal_io::paths::SegmentLayout::Direct);
            let segment = fixture.segment.clone();
            let hashes = Rc::new(RefCell::new(0));
            let seen = Rc::clone(&hashes);
            let decision = current::with_checkpoint(
                move |phase, _| {
                    if phase == current::Checkpoint::BeforeHash {
                        *seen.borrow_mut() += 1;
                    }
                    if phase != current::Checkpoint::AfterGroup {
                        return;
                    }
                    match case {
                        0 => fs::write(segment.join("first.webm"), b"bad").unwrap(),
                        1 => {
                            let path = segment.join("first.jsonl");
                            let text = fs::read_to_string(&path)
                                .unwrap()
                                .replace("2026-01-01T00:00:00Z", "2026-01-01T00:00:01Z");
                            fs::write(path, text).unwrap();
                        }
                        2 => fs::write(segment.join("late.webm"), b"new unprocessed sibling")
                            .unwrap(),
                        _ => fs::remove_file(segment.join("first.jsonl")).unwrap(),
                    }
                },
                || fixture.inspect(&fixture.marks),
            );
            assert_eq!(
                decision.availability(),
                CurrentOffloadAvailability::Unavailable,
                "case {case}"
            );
            assert_eq!(decision.eligible().count(), 0, "case {case}");
            assert_eq!(fixture.project(&decision).unwrap(), None);
            assert_eq!(*hashes.borrow(), 2, "case {case}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn current_batch_refuses_reoffered_nonce_and_retired_witness() {
        use std::io::Write;
        let fixture = CurrentFixture::new(solstone_core_journal_io::paths::SegmentLayout::Direct);
        let expected = fixture.marks[0].clone();
        let mut replacement = expected.clone();
        replacement.offload = Some(
            solstone_core_retention::archive_proof::OffloadOffer::new(
                expected.offload.as_ref().unwrap().proof.clone(),
            )
            .unwrap(),
        );
        assert_ne!(
            replacement.offload.as_ref().unwrap().nonce,
            expected.offload.as_ref().unwrap().nonce
        );
        fixture.save(&[replacement]);
        let stale = fixture.inspect(std::slice::from_ref(&expected));
        assert_eq!(stale.availability(), CurrentOffloadAvailability::Available);
        assert_eq!(stale.eligible().count(), 0);
        assert!(
            stale
                .refusals()
                .iter()
                .any(|refusal| refusal.reason() == &CurrentOffloadRefusalReason::Stale)
        );
        fixture.save(&fixture.marks);
        let proof = &expected.offload.as_ref().unwrap().proof;
        let files = proof.files.iter().map(|file| json!({"witness_id":file.witness_id,"name":file.name,"disposition":"published"})).collect::<Vec<_>>();
        let event = json!({"event_kind":"offload_restore_v2","schema_version":2,"time":CURRENT_NOW,
            "day":expected.target.day,"stream":expected.target.stream,"segment":"010000_001",
            "operation_id":"restored","files":files});
        writeln!(
            fs::OpenOptions::new()
                .append(true)
                .open(&fixture.ledger)
                .unwrap(),
            "{event}"
        )
        .unwrap();
        let retired = fixture.inspect(&fixture.marks);
        assert_eq!(
            retired.availability(),
            CurrentOffloadAvailability::Available
        );
        assert_eq!(retired.eligible().count(), 0);
        assert_eq!(
            fixture.project(&retired).unwrap().unwrap().eligible_bytes,
            0
        );
        assert!(
            retired
                .refusals()
                .iter()
                .any(|refusal| refusal.reason() == &CurrentOffloadRefusalReason::Unbound)
        );
    }

    #[test]
    #[cfg(unix)]
    fn current_batch_erases_prefix_credit_on_late_config_or_catalog_failure() {
        for case in 0..3 {
            let fixture =
                CurrentFixture::new(solstone_core_journal_io::paths::SegmentLayout::Direct);
            let config = fixture.journal.path().join("config/journal.json");
            let ledger = fixture.ledger.clone();
            let decision = current::with_checkpoint(
                move |phase, _| {
                    if phase != current::Checkpoint::AfterGroup {
                        return;
                    }
                    if case == 0 {
                        fs::write(&ledger, b"broken\n").unwrap();
                    } else {
                        let mut value: Value =
                            serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
                        if case == 1 {
                            value["transcribe"] = json!({"preserve_all":true});
                        } else {
                            value["backup"]["last_verification"]["status"] = json!("error");
                            value["backup"]["last_verification"]["reason"] =
                                json!("integrity_failed");
                        }
                        fs::write(&config, serde_json::to_vec(&value).unwrap()).unwrap();
                    }
                },
                || fixture.inspect(&fixture.marks),
            );
            assert_eq!(
                decision.availability(),
                CurrentOffloadAvailability::Unavailable,
                "case {case}"
            );
            assert_eq!(decision.eligible().count(), 0);
            assert_eq!(fixture.project(&decision).unwrap(), None);
        }
    }

    #[test]
    #[cfg(unix)]
    fn current_batch_busy_group_does_not_block_unrelated_group() {
        use solstone_core_retention::marks::MarkId;
        use solstone_core_segment::output_attempt::OutputClaims;
        let mut fixture =
            CurrentFixture::new(solstone_core_journal_io::paths::SegmentLayout::Direct);
        let other = fixture.journal.path().join("chronicle/20260101/030000_001");
        fs::create_dir(&other).unwrap();
        let raw = other.join("third.webm");
        fs::write(&raw, b"other").unwrap();
        write_eligible_sidecar(&raw);
        let mut mark = fixture.marks[1].clone();
        mark.target.dir = "030000_001".into();
        mark.proposal.names = vec!["third.webm".into()];
        mark.proposal.bytes = 5;
        let offer = mark.offload.as_mut().unwrap();
        let mut proof = offer.proof.clone();
        proof.target = mark.target.clone();
        proof.manifest_id = "other-manifest".into();
        let file = &mut proof.files[0];
        file.name = "third.webm".into();
        file.witness_id = "other-witness".into();
        file.bytes = 5;
        file.sha256 = format!("{:x}", Sha256::digest(b"other"));
        file.archive_path = "/owned/input/third.webm".into();
        file.processing.record.as_mut().unwrap()["input_size"] = json!(5);
        *offer = solstone_core_retention::archive_proof::OffloadOffer::new(proof.clone()).unwrap();
        mark.id = MarkId::derive(mark.class, &mark.target, &mark.proposal.names);
        let event = json!({"event_kind":"offload_archive_v2","schema_version":2,"time":CURRENT_NOW,"day":mark.target.day,"stream":mark.target.stream,"segment":"030000_001","segment_dir":mark.target.dir,"layout":"direct","journal_id":proof.journal_id,"manifest_id":proof.manifest_id,"repository_id":proof.repository_id,"snapshot_id":proof.snapshot_id,"files":proof.files});
        use std::io::Write;
        let mut ledger = fs::OpenOptions::new()
            .append(true)
            .open(&fixture.ledger)
            .unwrap();
        writeln!(ledger, "{event}").unwrap();
        fixture.marks.push(mark);
        fixture.save(&fixture.marks);
        let _busy = OutputClaims::discover(
            &fixture.segment,
            solstone_core_journal_io::LockOptions {
                timeout: std::time::Duration::ZERO,
                ..Default::default()
            },
        )
        .unwrap();
        let decision = fixture.inspect(&fixture.marks);
        assert_eq!(
            decision.availability(),
            CurrentOffloadAvailability::Available
        );
        let projected = fixture.project(&decision).unwrap().unwrap();
        assert_eq!(projected.local.total_bytes, 28);
        assert_eq!(projected.eligible_bytes, 5);
        assert_eq!(projected.projected_local_bytes, 23);
        assert!(
            decision
                .refusals()
                .iter()
                .any(|refusal| refusal.reason() == &CurrentOffloadRefusalReason::Busy)
        );
    }

    #[test]
    #[cfg(unix)]
    fn current_batch_projection_requires_complete_exact_local_intersection() {
        use crate::measurement::project_current_offload;
        use solstone_core_journal_io::paths::SegmentLayout;
        for layout in [SegmentLayout::Direct, SegmentLayout::Named] {
            let fixture = CurrentFixture::new(layout);
            let decision = fixture.inspect(&fixture.marks);
            let local = measure_local_media(fixture.journal.path());
            for case in 0..6 {
                let mut changed = local.clone();
                let file = changed
                    .files
                    .iter_mut()
                    .find(|file| file.key.name == "first.webm")
                    .unwrap();
                match case {
                    0 => file.key.day = "20260102".into(),
                    1 => file.key.stream = "other".into(),
                    2 => file.key.segment_dir = "010000_001_other".into(),
                    3 => {
                        file.key.layout = match layout {
                            SegmentLayout::Direct => SegmentLayout::Named,
                            SegmentLayout::Named => SegmentLayout::Direct,
                        }
                    }
                    4 => file.key.name = "renamed.webm".into(),
                    _ => file.bytes += 1,
                }
                let projected = project_current_offload(&changed, &decision)
                    .unwrap()
                    .unwrap();
                assert_eq!(projected.eligible_bytes, 4, "case {case}");
                assert_eq!(projected.eligible_files, 1, "case {case}");
                assert_eq!(
                    projected.projected_local_bytes,
                    projected.local.total_bytes - 4
                );
            }
            let mut metadata = local.clone();
            metadata
                .files
                .iter_mut()
                .find(|file| file.key.name == "first.webm")
                .unwrap()
                .key
                .segment = "grouping metadata only".into();
            assert_eq!(
                project_current_offload(&metadata, &decision)
                    .unwrap()
                    .unwrap()
                    .eligible_bytes,
                7
            );
            let mut duplicate = local.clone();
            duplicate.files.push(duplicate.files[0].clone());
            assert!(project_current_offload(&duplicate, &decision).is_err());
            let mut incomplete = local.clone();
            incomplete.unknown_paths.push(fixture.segment.clone());
            assert!(project_current_offload(&incomplete, &decision).is_err());
            let mut overflow = local;
            overflow.files[0].bytes = u64::MAX;
            assert!(project_current_offload(&overflow, &decision).is_err());
        }
    }

    #[test]
    #[cfg(windows)]
    fn current_batch_is_unavailable_without_writable_readback_transport() {
        let journal = tempfile::tempdir().unwrap();
        configure_offload(journal.path(), 100, Some(1));
        let root = std::sync::Arc::new(JournalRoot::open(journal.path()).unwrap());
        let runner = Script {
            outputs: RefCell::new(
                (0..2)
                    .map(|_| output(json!({"id":"a".repeat(64)}).to_string()))
                    .collect(),
            ),
            calls: RefCell::new(vec![]),
        };
        let clock = TestClock { now: 100 };
        let services = services(&runner, &Http, &clock, &Maintenance);
        let mut operation = solstone_core_backup_runtime::admit_archive(root, &services).unwrap();
        let decision = inspect_current_offload(
            &mut operation,
            &[],
            DateTime::from_timestamp(100, 0).unwrap().fixed_offset(),
        );
        assert_eq!(
            decision.availability(),
            CurrentOffloadAvailability::Unavailable
        );
        assert_eq!(decision.eligible().count(), 0);
        assert_eq!(
            decision.refusals()[0].reason(),
            &CurrentOffloadRefusalReason::ExecutionUnavailable
        );
        assert_eq!(runner.calls.borrow().len(), 2);
    }

    fn confirmation_failure(check: ToolOutput) -> OffloadResult {
        let journal = tempfile::tempdir().unwrap();
        let raw = journal
            .path()
            .join("chronicle/20260104/040000_004/confirm.webm");
        fs::create_dir_all(raw.parent().unwrap()).unwrap();
        fs::write(&raw, b"confirm").unwrap();
        write_eligible_sidecar(&raw);
        configure_offload(journal.path(), 100, Some(1));
        let runner = Script {
            outputs: RefCell::new(VecDeque::from([
                output(String::new()),
                output("[{\"message_type\":\"summary\",\"snapshot_id\":\"snapshot\"}]".into()),
                check,
            ])),
            calls: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock { now: 100 };
        let maintenance = Maintenance;
        run_offload(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
            false,
        )
    }

    #[test]
    fn archive_content_mismatch_is_confirm_failed() {
        let result = confirmation_failure(output(
            serde_json::json!([{"message_type":"snapshot","id":"snapshot"}]).to_string(),
        ));

        assert_eq!(result.status, "stalled");
        assert_eq!(result.reason.as_deref(), Some("confirm_failed"));
    }

    #[test]
    fn archive_check_tool_failure_is_confirm_tool_failed() {
        let result = confirmation_failure(output_with_code(1, String::new()));

        assert_eq!(result.status, "stalled");
        assert_eq!(result.reason.as_deref(), Some("confirm_tool_failed"));
    }
}
