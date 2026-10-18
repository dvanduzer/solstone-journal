// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Closed scheduled commands for the foreground host.
//!
//! Each arm calls the writer the desktop command already uses. This module
//! does not own a second scheduler, ledger, or publication schema.

use std::io::{BufRead, BufReader, Write};
use std::time::Instant;

use chrono::{DateTime, FixedOffset, Local, Utc};
use serde_json::{Value, json};
use solstone_core_brain_owner::{
    OwnerRefreshOptions, OwnerRefreshOutcome, ProductionSppPrerequisite, RespondWithConfigProbe,
};
use solstone_core_journal_io::JournalRoot;
use solstone_core_journal_io::operational_log::{
    OplogFormat, OplogWriter, create_oplog_at, fold_oplogs,
};
use solstone_core_maintenance::EmbeddedBackupRoutine;
use solstone_core_speaker_resolve::candidate_tracker::{
    CandidateTracker, SpeakerPoolCatchUp, backfill_speaker_pool,
};
use solstone_core_speaker_resolve::discovery_helper::DiscoveryHelperError;
use solstone_core_speaker_resolve::discovery_scan::{
    DiscoveryRefresh, DiscoveryRefreshError, labels_as_i64, refresh_discovery_with_cluster,
};

use super::HostedCommand;

const HEARTBEAT_RECENCY_MS: i64 = 12 * 3_600_000;
const DISCOVERY_MIN_CLUSTER_SIZE: usize = 5;
const DISCOVERY_MIN_SAMPLES: usize = 3;

pub(super) fn run(hosted: &HostedCommand, args: &[String]) -> i32 {
    match args {
        [command] if command == "heartbeat" => heartbeat(hosted),
        [command] if command == "facet-candidates" => facet_candidates(hosted),
        [command, verb] if command == "brain" && verb == "refresh" => brain_refresh(hosted),
        [command, flag] if command == "indexer" && flag == "--rebuild-edges" => {
            rebuild_edges(hosted)
        }
        [command, verb, id] if command == "maintenance" && verb == "run" => maintenance(hosted, id),
        _ => 1,
    }
}

fn stopped(hosted: &HostedCommand) -> bool {
    (hosted.cancelled)()
}

fn heartbeat(hosted: &HostedCommand) -> i32 {
    if stopped(hosted) {
        return 1;
    }
    let now = Local::now().fixed_offset();
    if heartbeat_recent(&hosted.journal, now) {
        return 0;
    }
    if stopped(hosted) {
        return 1;
    }
    let started = Instant::now();
    let Ok(root) = JournalRoot::open(&hosted.journal) else {
        return 1;
    };
    let mut log = match create_oplog_at(root, "heartbeat", "pass", OplogFormat::Jsonl, now) {
        Ok(log) => log,
        Err(error) => {
            log::warn!("foreground heartbeat oplog: {error}");
            return 1;
        }
    };
    let pass = write_heartbeat_record(
        &mut log,
        &json!({
            "data_source_errors": [],
            "escalated_targets": [],
            "event": "pass",
            "fired": [],
            "ts": Utc::now().timestamp_millis(),
        }),
    );
    let outcome = if pass.is_ok() { "success" } else { "error" };
    let recorded = write_heartbeat_record(
        &mut log,
        &json!({
            "duration_seconds": started.elapsed().as_secs(),
            "event": "pass.outcome",
            "outcome": outcome,
            "ts": Utc::now().timestamp_millis(),
        }),
    );
    if pass.is_err() || recorded.is_err() {
        return 1;
    }
    0
}

fn heartbeat_recent(journal: &std::path::Path, now: DateTime<FixedOffset>) -> bool {
    let today = now.date_naive();
    let Some(previous) = today.pred_opt() else {
        return false;
    };
    let Ok(root) = JournalRoot::open(journal) else {
        return false;
    };
    let newest = fold_oplogs(
        root,
        &[previous, today],
        |newest: &mut Option<i64>, entry, input| {
            if entry.name().source().display_slug() != "heartbeat"
                || entry.name().run().display_slug() != "pass"
                || entry.name().format() != OplogFormat::Jsonl
            {
                return Ok(());
            }
            for line in BufReader::new(input).lines() {
                let line = line?;
                let Ok(Value::Object(row)) = serde_json::from_str(&line) else {
                    continue;
                };
                if row.get("event").and_then(Value::as_str) != Some("pass.outcome")
                    || row.get("outcome").and_then(Value::as_str) != Some("success")
                    || row
                        .get("duration_seconds")
                        .and_then(Value::as_u64)
                        .is_none()
                {
                    continue;
                }
                let Some(ts) = row.get("ts").and_then(Value::as_i64) else {
                    continue;
                };
                *newest = Some(newest.map_or(ts, |current| current.max(ts)));
            }
            Ok(())
        },
    )
    .ok()
    .flatten();
    newest.is_some_and(|ts| now.timestamp_millis().saturating_sub(ts) < HEARTBEAT_RECENCY_MS)
}

fn write_heartbeat_record(log: &mut OplogWriter, record: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *log, record).map_err(std::io::Error::other)?;
    log.write_all(b"\n")?;
    log.flush()
}

fn facet_candidates(hosted: &HostedCommand) -> i32 {
    if stopped(hosted) {
        return 1;
    }
    let zone = solstone_core_journal_config::owner_zone(&hosted.journal);
    let today = Utc::now().with_timezone(&zone).date_naive();
    let day = today.format("%Y%m%d").to_string();
    let candidates = match solstone_core_facets::aggregate_speculative_facets(
        &hosted.journal,
        today,
        solstone_core_facets::FACET_CANDIDATE_MIN_SEGMENTS,
    ) {
        Ok(candidates) => candidates,
        Err(error) => {
            log::warn!("foreground facet candidates: {error}");
            return 1;
        }
    };
    if candidates.is_empty() {
        return 0;
    }
    if stopped(hosted) {
        return 1;
    }
    match solstone_core_facets::record_facet_candidates(&hosted.journal, &day, &candidates) {
        Ok(_) => 0,
        Err(error) => {
            log::warn!("foreground facet candidates: {error}");
            1
        }
    }
}

fn brain_refresh(hosted: &HostedCommand) -> i32 {
    let cancelled = hosted.cancelled.as_ref();
    if cancelled() {
        return 2;
    }
    let probe = RespondWithConfigProbe {
        runtime: solstone_core_generate_wire::EndpointRuntime::default(),
    };
    let options = OwnerRefreshOptions {
        expected_fingerprint: None,
        expected_active_fingerprint: false,
        expect_active_fingerprint_absent: false,
    };
    let outcome = solstone_core_brain_owner::refresh(
        &hosted.journal,
        &options,
        &probe,
        &ProductionSppPrerequisite,
        cancelled,
    );
    if cancelled() {
        return 2;
    }
    match outcome {
        OwnerRefreshOutcome::OperationalFailure { .. } | OwnerRefreshOutcome::Cancelled => 2,
        OwnerRefreshOutcome::PersistedReady { view }
        | OwnerRefreshOutcome::PersistedNonReady { view, .. }
        | OwnerRefreshOutcome::Busy { view }
        | OwnerRefreshOutcome::StaleExpectedFingerprint { view }
        | OwnerRefreshOutcome::LostFence { view } => {
            i32::from(solstone_core_brain_owner::owner_exit_code(&view))
        }
    }
}

fn rebuild_edges(hosted: &HostedCommand) -> i32 {
    if stopped(hosted) {
        return 1;
    }
    match solstone_core_indexer_store::scan::rebuild_edges(&hosted.journal) {
        Ok(report) if report.failed == 0 => 0,
        Ok(_) => 1,
        Err(error) => {
            log::warn!("foreground edge rebuild: {error}");
            1
        }
    }
}

fn maintenance(hosted: &HostedCommand, id: &str) -> i32 {
    match id {
        "speakers:discover-voices" => discover_voices(hosted),
        "speakers:candidate-pair-suggestions" => candidate_pairs(hosted),
        "speakers:name-variants" => name_variants(hosted),
        "speakers:consolidate-pool" => consolidate_pool(hosted),
        "speakers:backfill-pool" => backfill_pool(hosted),
        "backup:run" => backup(hosted, EmbeddedBackupRoutine::Run),
        "backup:verify" => backup(hosted, EmbeddedBackupRoutine::Verify),
        "backup:prune" => backup(hosted, EmbeddedBackupRoutine::Prune),
        "health:mark-raw" | "health:prune-logs" => health(hosted, id),
        _ => 1,
    }
}

fn discover_voices(hosted: &HostedCommand) -> i32 {
    if stopped(hosted) {
        return 1;
    }
    match refresh_discovery_with_cluster(&hosted.journal, cluster_embeddings) {
        Ok(DiscoveryRefresh::IdentityInvalid | DiscoveryRefresh::NoConfirmedOwner) => 0,
        Ok(DiscoveryRefresh::Refreshed { .. }) => 0,
        Err(DiscoveryRefreshError::Helper(_)) => 2,
        Err(error) => {
            log::warn!("foreground speaker discovery: {error}");
            1
        }
    }
}

fn cluster_embeddings(rows: Vec<Vec<f32>>) -> Result<Vec<i64>, DiscoveryHelperError> {
    let cols = rows.first().map(Vec::len).unwrap_or(0);
    if rows.iter().any(|row| row.len() != cols) {
        return Err(DiscoveryHelperError {
            stage: "cluster",
            reason: "ragged embeddings".into(),
        });
    }
    let flat = rows.iter().flatten().copied().collect::<Vec<_>>();
    solstone_core_speakers::discovery::cluster_embeddings(
        &flat,
        rows.len(),
        cols,
        DISCOVERY_MIN_CLUSTER_SIZE,
        DISCOVERY_MIN_SAMPLES,
    )
    .map(|labels| labels_as_i64(&labels))
    .map_err(|error| DiscoveryHelperError {
        stage: "cluster",
        reason: error.to_string(),
    })
}

fn candidate_pairs(hosted: &HostedCommand) -> i32 {
    if stopped(hosted) {
        return 1;
    }
    match solstone_core_speaker_resolve::candidate_pair_suggestions::refresh_candidate_pair_suggestions(
        &hosted.journal,
    ) {
        Ok(_) => 0,
        Err(error) => {
            log::warn!("foreground speaker candidate pairs: {error}");
            1
        }
    }
}

fn name_variants(hosted: &HostedCommand) -> i32 {
    if stopped(hosted) {
        return 1;
    }
    let scan =
        match solstone_core_speaker_resolve::name_variant_scan::detect_name_variant_candidates(
            &hosted.journal,
        ) {
            Ok(scan) => scan,
            Err(error) => {
                log::warn!("foreground speaker name variants: {error}");
                return 1;
            }
        };
    for candidate in &scan.candidates {
        if stopped(hosted) {
            return 1;
        }
        if let Err(error) =
            solstone_core_speaker_resolve::speaker_review_candidates::record_name_variant_candidate(
                &hosted.journal,
                candidate,
            )
        {
            log::warn!("foreground speaker name variants: {error}");
            return 1;
        }
    }
    0
}

fn consolidate_pool(hosted: &HostedCommand) -> i32 {
    if stopped(hosted) {
        return 1;
    }
    match CandidateTracker::new(&hosted.journal).consolidate_dense_candidates() {
        Ok(_) => 0,
        Err(error) => {
            log::warn!("foreground speaker pool consolidation: {error}");
            1
        }
    }
}

fn backfill_pool(hosted: &HostedCommand) -> i32 {
    if stopped(hosted) {
        return 1;
    }
    match backfill_speaker_pool(&hosted.journal) {
        SpeakerPoolCatchUp::Refuse(refusal) => {
            log::warn!("foreground speaker pool backfill: {refusal}");
            1
        }
        SpeakerPoolCatchUp::Done(_) => {
            if stopped(hosted) {
                return 1;
            }
            match solstone_core_speaker_resolve::owner_voice::refresh_owner_voice(&hosted.journal) {
                Ok(_) => 0,
                Err(error) => {
                    log::warn!("foreground owner voice: {error}");
                    1
                }
            }
        }
    }
}

fn health(hosted: &HostedCommand, id: &str) -> i32 {
    if stopped(hosted) {
        return 1;
    }
    let services = solstone_core_maintenance::HealthServices {
        now: Utc::now(),
        zone: solstone_core_journal_config::owner_zone(&hosted.journal),
    };
    solstone_core_maintenance::bodies::health::run(id, &[], &hosted.journal, &services).exit_code
}

fn backup(hosted: &HostedCommand, routine: EmbeddedBackupRoutine) -> i32 {
    if stopped(hosted) {
        return 1;
    }
    let run = solstone_core_maintenance::run_embedded_backup(
        &hosted.journal,
        routine,
        hosted.backup.runner.as_ref(),
        hosted.backup.http.as_ref(),
        &hosted.backup.executable,
    );
    let disposition = crate::backup::backup_terminal(&hosted.root, routine, &run);
    crate::backup::log_backup_terminal(disposition, run.exit_code);
    match disposition {
        crate::backup::BackupDisposition::Success => 0,
        crate::backup::BackupDisposition::Refusal | crate::backup::BackupDisposition::Failure => {
            if run.exit_code == 0 {
                1
            } else {
                run.exit_code
            }
        }
    }
}
