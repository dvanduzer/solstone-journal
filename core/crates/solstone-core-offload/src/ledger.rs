// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Durable JSONL pre-mark witness for media offload.
//!
//! This is JSONL rather than SQLite because backups exclude `*.sqlite*` but
//! include `health/`. Appends are fsynced by journal-io: this record exists
//! before a pending-release mark is minted. Folding is append order, never
//! timestamp order; malformed reads degrade rather than creating a trusted zero.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use solstone_core_journal_io::{append_jsonl, paths::SegmentLayout};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OffloadFile {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentOffloadSummary {
    pub day: String,
    pub stream: String,
    pub segment: String,
    pub currently_offloaded: bool,
    pub snapshot_id: Option<String>,
    pub files: Vec<OffloadFile>,
    pub offloaded_bytes: u64,
    pub offloaded_file_count: u64,
    pub skipped_records: u64,
    pub unreadable_ledgers: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DayOffloadSummary {
    pub day: String,
    pub segments: Vec<SegmentOffloadSummary>,
    pub offloaded_bytes: u64,
    pub offloaded_file_count: u64,
    pub offloaded_segments: u64,
    pub skipped_records: u64,
    pub unreadable_ledgers: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalOffloadSummary {
    pub days: Vec<DayOffloadSummary>,
    pub offloaded_bytes: u64,
    pub offloaded_file_count: u64,
    pub offloaded_segments: u64,
    pub offloaded_days: u64,
    pub skipped_records: u64,
    pub unreadable_ledgers: Vec<String>,
}
impl SegmentOffloadSummary {
    pub fn degraded(&self) -> bool {
        self.skipped_records > 0 || !self.unreadable_ledgers.is_empty()
    }
}
impl DayOffloadSummary {
    pub fn degraded(&self) -> bool {
        self.skipped_records > 0 || !self.unreadable_ledgers.is_empty()
    }
}
impl JournalOffloadSummary {
    pub fn degraded(&self) -> bool {
        self.skipped_records > 0 || !self.unreadable_ledgers.is_empty()
    }
}

/// A single archived generation. Historical records leave unstored bindings unknown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveWitness {
    pub manifest_id: String,
    pub witness_id: String,
    pub snapshot_id: String,
    pub repository_id: Option<String>,
    pub archive_path: Option<String>,
    pub journal_id: Option<String>,
    pub layout: Option<SegmentLayout>,
    pub segment_dir: Option<String>,
    pub file: OffloadFile,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveManifest {
    pub manifest_id: String,
    pub files: Vec<ArchiveWitness>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SegmentCatalog {
    pub day: String,
    pub stream: String,
    pub segment: String,
    pub history: Vec<ArchiveManifest>,
    pub restores: Vec<RestoreOperation>,
    pub active: Vec<ArchiveWitness>,
    pub skipped_records: u64,
    pub unreadable_ledgers: Vec<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DayCatalog {
    pub day: String,
    pub segments: Vec<SegmentCatalog>,
    pub skipped_records: u64,
    pub unreadable_ledgers: Vec<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JournalCatalog {
    pub days: Vec<DayCatalog>,
    pub skipped_records: u64,
    pub unreadable_ledgers: Vec<String>,
}
impl SegmentCatalog {
    pub fn degraded(&self) -> bool {
        self.skipped_records > 0 || !self.unreadable_ledgers.is_empty()
    }
}
impl DayCatalog {
    pub fn degraded(&self) -> bool {
        self.skipped_records > 0 || !self.unreadable_ledgers.is_empty()
    }
}
impl JournalCatalog {
    pub fn degraded(&self) -> bool {
        self.skipped_records > 0 || !self.unreadable_ledgers.is_empty()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreDisposition {
    Published,
    AlreadyPresent,
    Conflict,
    Unverified,
    Unattempted,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveFileV2 {
    pub witness_id: String,
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
    pub archive_path: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreFileV2 {
    pub witness_id: String,
    pub name: String,
    pub disposition: RestoreDisposition,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreOperation {
    pub operation_id: String,
    pub files: Vec<RestoreFileV2>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event_kind", deny_unknown_fields)]
enum Event {
    #[serde(rename = "offload")]
    Archive {
        time: u64,
        day: String,
        stream: String,
        segment: String,
        snapshot_id: String,
        files: Vec<OffloadFile>,
    },
    #[serde(rename = "restore")]
    Restore {
        time: u64,
        day: String,
        stream: String,
        segment: String,
    },
    #[serde(rename = "offload_archive_v2")]
    ArchiveV2 {
        schema_version: u32,
        time: u64,
        day: String,
        stream: String,
        segment: String,
        segment_dir: String,
        layout: SegmentLayout,
        journal_id: String,
        manifest_id: String,
        repository_id: String,
        snapshot_id: String,
        files: Vec<ArchiveFileV2>,
    },
    #[serde(rename = "offload_restore_v2")]
    RestoreV2 {
        schema_version: u32,
        time: u64,
        day: String,
        stream: String,
        segment: String,
        operation_id: String,
        files: Vec<RestoreFileV2>,
    },
}
fn valid_day(day: &str) -> bool {
    day.len() == 8 && day.bytes().all(|byte| byte.is_ascii_digit())
}
fn component(name: &str) -> bool {
    !name.is_empty() && !matches!(name, "." | "..") && !name.contains(['/', '\\', ':', '\0'])
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_file(file: &OffloadFile) -> bool {
    component(&file.name) && digest(&file.sha256)
}
fn path(journal: &Path, day: &str) -> PathBuf {
    journal.join("health/offload").join(format!("{day}.jsonl"))
}
pub fn ledger_path_for_day(journal: &Path, day: &str) -> Result<PathBuf, String> {
    if !valid_day(day) {
        return Err("day must be in YYYYMMDD format".into());
    }
    Ok(path(journal, day))
}
pub fn append_offload_event(
    journal: &Path,
    day: &str,
    stream: &str,
    segment: &str,
    snapshot_id: &str,
    files: &[OffloadFile],
    time: u64,
) -> Result<(), String> {
    if !valid_day(day)
        || !component(stream)
        || !component(segment)
        || snapshot_id.is_empty()
        || files.is_empty()
        || files.iter().any(|file| !valid_file(file))
        || files.iter().map(|f| &f.name).collect::<BTreeSet<_>>().len() != files.len()
    {
        return Err("invalid offload ledger event".into());
    }
    let record = serde_json::json!({"event_kind":"offload","time":time,"day":day,"stream":stream,"segment":segment,"snapshot_id":snapshot_id,"files":files});
    append_jsonl(path(journal, day), &record).map_err(|error| error.to_string())
}
pub fn append_restore_event(
    journal: &Path,
    day: &str,
    stream: &str,
    segment: &str,
    time: u64,
) -> Result<(), String> {
    if !valid_day(day) || !component(stream) || !component(segment) {
        return Err("invalid restore ledger event".into());
    }
    append_jsonl(path(journal,day),&serde_json::json!({"event_kind":"restore","time":time,"day":day,"stream":stream,"segment":segment})).map_err(|error|error.to_string())
}

// A missing optional store is empty; a non-directory, symlink or IO failure is unknown.
fn ledger_directory(journal: &Path) -> Result<bool, PathBuf> {
    for dir in [
        journal.to_path_buf(),
        journal.join("health"),
        journal.join("health/offload"),
    ] {
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && dir != journal => {
                return Ok(false);
            }
            _ => return Err(dir),
        }
    }
    Ok(true)
}
#[derive(Default)]
struct Fold {
    catalog: SegmentCatalog,
    // Layout and exact basename are part of the target, not inferred from stream/key.
    active: BTreeMap<(Option<SegmentLayout>, Option<String>, String), ArchiveWitness>,
    latest_legacy_scope: Vec<String>,
}
fn fold_lines(day: &str, bytes: &[u8]) -> DayCatalog {
    let mut folds = BTreeMap::<(String, String), Fold>::new();
    let mut skipped = 0;
    let mut manifests = BTreeSet::new();
    let mut witnesses = BTreeSet::new();
    let mut operations = BTreeSet::new();
    let mut archive_records = BTreeSet::new();
    for (index, line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
        if !line.ends_with(b"\n") {
            skipped += 1;
            continue;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(event) = serde_json::from_slice::<Event>(line) else {
            skipped += 1;
            continue;
        };
        if matches!(&event, Event::Archive { .. } | Event::ArchiveV2 { .. }) {
            let canonical = serde_json::to_string(&event).expect("ledger event serialization");
            if !archive_records.insert(canonical) {
                skipped += 1;
                continue;
            }
        }
        let (event_day, stream, segment) = match &event {
            Event::Archive {
                day,
                stream,
                segment,
                ..
            }
            | Event::Restore {
                day,
                stream,
                segment,
                ..
            }
            | Event::ArchiveV2 {
                day,
                stream,
                segment,
                ..
            }
            | Event::RestoreV2 {
                day,
                stream,
                segment,
                ..
            } => (day, stream, segment),
        };
        if event_day != day || !component(stream) || !component(segment) {
            skipped += 1;
            continue;
        }
        let key = (stream.clone(), segment.clone());
        let fold = folds.entry(key).or_insert_with(|| Fold {
            catalog: SegmentCatalog {
                day: day.into(),
                stream: stream.clone(),
                segment: segment.clone(),
                ..Default::default()
            },
            ..Default::default()
        });
        let archive = match event {
            Event::Archive {
                snapshot_id,
                files,
                time,
                ..
            } => {
                let _ = time;
                if snapshot_id.is_empty()
                    || files.is_empty()
                    || files.iter().any(|f| !valid_file(f))
                    || files.iter().map(|f| &f.name).collect::<BTreeSet<_>>().len() != files.len()
                {
                    skipped += 1;
                    continue;
                }
                let manifest_id = format!("v1:{day}:{}", index + 1);
                let files = files
                    .into_iter()
                    .map(|file| ArchiveWitness {
                        witness_id: format!("{manifest_id}:{}", file.name),
                        manifest_id: manifest_id.clone(),
                        snapshot_id: snapshot_id.clone(),
                        repository_id: None,
                        archive_path: None,
                        journal_id: None,
                        layout: None,
                        segment_dir: None,
                        file,
                    })
                    .collect();
                Some(ArchiveManifest { manifest_id, files })
            }
            Event::ArchiveV2 {
                schema_version,
                manifest_id,
                repository_id,
                snapshot_id,
                journal_id,
                layout,
                segment_dir,
                files,
                time,
                ..
            } => {
                let _ = time;
                if schema_version != 2
                    || manifest_id.is_empty()
                    || manifest_id.starts_with("v1:")
                    || manifests.contains(&manifest_id)
                    || repository_id.is_empty()
                    || journal_id.is_empty()
                    || !digest(&snapshot_id)
                    || !component(&segment_dir)
                    || files.is_empty()
                    || (layout == SegmentLayout::Direct && fold.catalog.stream != "_default")
                    || files.iter().any(|f| {
                        f.witness_id.is_empty()
                            || f.witness_id.starts_with("v1:")
                            || witnesses.contains(&f.witness_id)
                            || !component(&f.name)
                            || !digest(&f.sha256)
                            || !f.archive_path.starts_with('/')
                            || f.archive_path.contains(['\\', '\0'])
                            || f.archive_path.split('/').any(|p| matches!(p, "." | ".."))
                    })
                    || files.iter().map(|f| &f.name).collect::<BTreeSet<_>>().len() != files.len()
                    || files
                        .iter()
                        .map(|f| &f.witness_id)
                        .collect::<BTreeSet<_>>()
                        .len()
                        != files.len()
                {
                    skipped += 1;
                    continue;
                }
                let files = files
                    .into_iter()
                    .map(|f| ArchiveWitness {
                        manifest_id: manifest_id.clone(),
                        witness_id: f.witness_id,
                        snapshot_id: snapshot_id.clone(),
                        repository_id: Some(repository_id.clone()),
                        journal_id: Some(journal_id.clone()),
                        layout: Some(layout),
                        segment_dir: Some(segment_dir.clone()),
                        archive_path: Some(f.archive_path),
                        file: OffloadFile {
                            name: f.name,
                            bytes: f.bytes,
                            sha256: f.sha256,
                        },
                    })
                    .collect();
                Some(ArchiveManifest { manifest_id, files })
            }
            Event::Restore { time, .. } => {
                let _ = time;
                // Repeated historical restore records reuse this scope, never cascade backwards.
                let scope = &fold.latest_legacy_scope;
                fold.active
                    .retain(|_, witness| !scope.contains(&witness.witness_id));
                None
            }
            Event::RestoreV2 {
                schema_version,
                operation_id,
                files,
                time,
                ..
            } => {
                let _ = time;
                if schema_version != 2
                    || operation_id.is_empty()
                    || files.is_empty()
                    || operations.contains(&operation_id)
                    || files.iter().any(|f| {
                        !component(&f.name)
                            || f.witness_id.is_empty()
                            || !fold
                                .catalog
                                .history
                                .iter()
                                .flat_map(|m| &m.files)
                                .any(|w| w.witness_id == f.witness_id && w.file.name == f.name)
                    })
                    || files
                        .iter()
                        .map(|f| &f.witness_id)
                        .collect::<BTreeSet<_>>()
                        .len()
                        != files.len()
                    || files.iter().map(|f| &f.name).collect::<BTreeSet<_>>().len() != files.len()
                {
                    skipped += 1;
                    continue;
                }
                operations.insert(operation_id.clone());
                fold.catalog.restores.push(RestoreOperation {
                    operation_id,
                    files: files.clone(),
                });
                for file in files {
                    if matches!(
                        file.disposition,
                        RestoreDisposition::Published | RestoreDisposition::AlreadyPresent
                    ) {
                        fold.active.retain(|_, w| w.witness_id != file.witness_id);
                    }
                }
                None
            }
        };
        if let Some(archive) = archive {
            // A legacy restore has authority only over a preceding legacy manifest.
            fold.latest_legacy_scope = if archive.files.iter().all(|w| w.repository_id.is_none()) {
                archive.files.iter().map(|w| w.witness_id.clone()).collect()
            } else {
                Vec::new()
            };
            manifests.insert(archive.manifest_id.clone());
            for witness in &archive.files {
                witnesses.insert(witness.witness_id.clone());
                fold.active.insert(
                    (
                        witness.layout,
                        witness.segment_dir.clone(),
                        witness.file.name.clone(),
                    ),
                    witness.clone(),
                );
            }
            fold.catalog.history.push(archive);
        }
    }
    DayCatalog {
        day: day.into(),
        skipped_records: skipped,
        unreadable_ledgers: vec![],
        segments: folds
            .into_values()
            .map(|mut fold| {
                fold.catalog.active = fold.active.into_values().collect();
                fold.catalog.skipped_records = skipped;
                fold.catalog
            })
            .collect(),
    }
}
pub fn catalog_day(journal: &Path, day: &str) -> Result<DayCatalog, String> {
    if !valid_day(day) {
        return Err("day must be in YYYYMMDD format".into());
    }
    let mut result = DayCatalog {
        day: day.into(),
        ..Default::default()
    };
    match ledger_directory(journal) {
        Ok(false) => return Ok(result),
        Err(dir) => {
            result.unreadable_ledgers.push(dir.display().to_string());
            return Ok(result);
        }
        Ok(true) => {}
    }
    let file = path(journal, day);
    match fs::symlink_metadata(&file) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(result),
        Ok(meta) if meta.is_file() => {}
        _ => {
            result.unreadable_ledgers.push(file.display().to_string());
            return Ok(result);
        }
    }
    match fs::read(&file) {
        Ok(bytes) => Ok(fold_lines(day, &bytes)),
        Err(_) => {
            result.unreadable_ledgers.push(file.display().to_string());
            Ok(result)
        }
    }
}
pub fn catalog_segment(
    journal: &Path,
    day: &str,
    stream: &str,
    segment: &str,
) -> Result<SegmentCatalog, String> {
    let catalog = catalog_day(journal, day)?;
    Ok(catalog
        .segments
        .into_iter()
        .find(|s| s.stream == stream && s.segment == segment)
        .unwrap_or(SegmentCatalog {
            day: day.into(),
            stream: stream.into(),
            segment: segment.into(),
            skipped_records: catalog.skipped_records,
            unreadable_ledgers: catalog.unreadable_ledgers,
            ..Default::default()
        }))
}
pub fn catalog_journal(journal: &Path) -> JournalCatalog {
    let mut result = JournalCatalog::default();
    match ledger_directory(journal) {
        Ok(false) => return result,
        Err(dir) => {
            result.unreadable_ledgers.push(dir.display().to_string());
            return result;
        }
        Ok(true) => {}
    }
    let directory = journal.join("health/offload");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(_) => {
            result
                .unreadable_ledgers
                .push(directory.display().to_string());
            return result;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                result
                    .unreadable_ledgers
                    .push(directory.display().to_string());
                continue;
            }
        };
        let file = entry.path();
        if file.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let Some(day) = file
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|day| valid_day(day))
        else {
            result.unreadable_ledgers.push(file.display().to_string());
            continue;
        };
        match catalog_day(journal, day) {
            Ok(day) => {
                result.skipped_records += day.skipped_records;
                result
                    .unreadable_ledgers
                    .extend(day.unreadable_ledgers.clone());
                result.days.push(day);
            }
            Err(_) => result.unreadable_ledgers.push(file.display().to_string()),
        }
    }
    result.days.sort_by(|a, b| a.day.cmp(&b.day));
    result
}
fn summary(catalog: SegmentCatalog) -> SegmentOffloadSummary {
    let manifests = catalog
        .active
        .iter()
        .map(|w| &w.manifest_id)
        .collect::<BTreeSet<_>>();
    // The old consumer understands neither per-file snapshots nor repository bindings.
    let representable =
        manifests.len() <= 1 && catalog.active.iter().all(|w| w.repository_id.is_none());
    let selectable = representable && !catalog.degraded();
    SegmentOffloadSummary {
        day: catalog.day,
        stream: catalog.stream,
        segment: catalog.segment,
        currently_offloaded: !catalog.active.is_empty(),
        snapshot_id: selectable
            .then(|| catalog.active.first().map(|w| w.snapshot_id.clone()))
            .flatten(),
        files: if selectable {
            catalog.active.iter().map(|w| w.file.clone()).collect()
        } else {
            vec![]
        },
        offloaded_bytes: catalog
            .active
            .iter()
            .fold(0_u64, |sum, w| sum.saturating_add(w.file.bytes)),
        offloaded_file_count: catalog.active.len() as u64,
        skipped_records: catalog.skipped_records + u64::from(!representable),
        unreadable_ledgers: catalog.unreadable_ledgers,
    }
}
pub fn summarize_segment(
    journal: &Path,
    day: &str,
    stream: &str,
    segment: &str,
) -> Result<SegmentOffloadSummary, String> {
    catalog_segment(journal, day, stream, segment).map(summary)
}
fn day_summary(catalog: DayCatalog) -> DayOffloadSummary {
    let segments = catalog
        .segments
        .into_iter()
        .filter(|s| !s.active.is_empty())
        .map(summary)
        .collect::<Vec<_>>();
    DayOffloadSummary {
        day: catalog.day,
        offloaded_bytes: segments
            .iter()
            .fold(0_u64, |n, s| n.saturating_add(s.offloaded_bytes)),
        offloaded_file_count: segments.iter().map(|s| s.offloaded_file_count).sum(),
        offloaded_segments: segments.len() as u64,
        skipped_records: catalog.skipped_records
            + segments
                .iter()
                .filter(|s| s.skipped_records > catalog.skipped_records)
                .count() as u64,
        unreadable_ledgers: catalog.unreadable_ledgers,
        segments,
    }
}
pub fn summarize_day(journal: &Path, day: &str) -> Result<DayOffloadSummary, String> {
    catalog_day(journal, day).map(day_summary)
}
pub fn summarize_journal(journal: &Path) -> JournalOffloadSummary {
    let catalog = catalog_journal(journal);
    let days = catalog
        .days
        .into_iter()
        .map(day_summary)
        .collect::<Vec<_>>();
    JournalOffloadSummary {
        offloaded_bytes: days
            .iter()
            .fold(0_u64, |n, d| n.saturating_add(d.offloaded_bytes)),
        offloaded_file_count: days.iter().map(|d| d.offloaded_file_count).sum(),
        offloaded_segments: days.iter().map(|d| d.offloaded_segments).sum(),
        offloaded_days: days.iter().filter(|d| d.offloaded_segments > 0).count() as u64,
        skipped_records: days.iter().map(|d| d.skipped_records).sum(),
        unreadable_ledgers: catalog.unreadable_ledgers,
        days,
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use serde_json::{Value, json};

    fn file(name: &str) -> OffloadFile {
        OffloadFile {
            name: name.into(),
            bytes: 7,
            sha256: "a".repeat(64),
        }
    }
    fn archive(snapshot: &str, names: &[&str]) -> Value {
        json!({"event_kind":"offload", "time":1, "day":"20260101", "stream":"_default", "segment":"010000_001",
            "snapshot_id":snapshot, "files":names.iter().map(|name| file(name)).collect::<Vec<_>>()})
    }
    fn restore() -> Value {
        json!({"event_kind":"restore", "time":0, "day":"20260101", "stream":"_default", "segment":"010000_001"})
    }
    fn archive_v2(id: &str, name: &str) -> Value {
        json!({"event_kind":"offload_archive_v2", "schema_version":2, "time":2, "day":"20260101", "stream":"_default", "segment":"010000_001",
            "segment_dir":"010000_001_full", "layout":"direct", "journal_id":"journal", "repository_id":"repo",
            "snapshot_id":"b".repeat(64), "manifest_id":id,
            "files":[{"witness_id":format!("w-{id}"), "name":name, "bytes":7, "sha256":"a".repeat(64), "archive_path":format!("/stage/{name}")}]})
    }
    fn folded(records: &[Value]) -> DayCatalog {
        let lines = records.iter().map(|r| format!("{r}\n")).collect::<String>();
        fold_lines("20260101", lines.as_bytes())
    }
    #[test]
    fn disjoint_generations_survive_legacy_restore_of_last_manifest() {
        let day = folded(&[
            archive("a", &["a.webm"]),
            archive("b", &["b.webm"]),
            restore(),
            restore(),
        ]);
        assert!(!day.degraded());
        assert_eq!(day.segments[0].history.len(), 2);
        assert_eq!(
            day.segments[0]
                .active
                .iter()
                .map(|w| w.file.name.as_str())
                .collect::<Vec<_>>(),
            ["a.webm"]
        );
        assert_eq!(day.segments[0].active[0].snapshot_id, "a");
    }
    #[test]
    fn superseded_generation_never_returns_after_restoring_newest() {
        let day = folded(&[
            archive("a", &["a.webm"]),
            archive("b", &["a.webm"]),
            restore(),
        ]);
        assert!(day.segments[0].active.is_empty());
        assert_eq!(day.segments[0].history.len(), 2);
    }
    #[test]
    fn multi_manifest_old_summary_refuses_selection_and_preserves_totals() {
        let day = folded(&[archive("a", &["a.webm"]), archive("b", &["b.webm"])]);
        let old_summary = summary(day.segments[0].clone());
        assert!(old_summary.degraded());
        assert!(old_summary.snapshot_id.is_none());
        assert!(old_summary.files.is_empty());
        assert_eq!(old_summary.offloaded_file_count, 2);
        assert_eq!(old_summary.offloaded_bytes, 14);
        let single = summary_for_test(&[archive("a", &["a.webm", "b.webm"])]);
        assert!(!single.degraded());
        assert_eq!(single.files.len(), 2);
        assert_eq!(single.snapshot_id.as_deref(), Some("a"));
    }
    fn summary_for_test(records: &[Value]) -> SegmentOffloadSummary {
        summary(folded(records).segments.remove(0))
    }
    #[test]
    fn v2_exact_dispositions_do_not_retire_newer_or_unpublished_witnesses() {
        for disposition in [
            "published",
            "already_present",
            "conflict",
            "unverified",
            "unattempted",
        ] {
            let event = json!({"event_kind":"offload_restore_v2", "schema_version":2, "time":3, "day":"20260101", "stream":"_default", "segment":"010000_001",
                "operation_id":"operation", "files":[{"witness_id":"w-a", "name":"a.webm", "disposition":disposition}]});
            let old = folded(&[archive_v2("a", "a.webm"), event.clone()]);
            assert!(!old.degraded());
            assert_eq!(
                old.segments[0].active.is_empty(),
                matches!(disposition, "published" | "already_present")
            );
            let newer = folded(&[archive_v2("a", "a.webm"), archive_v2("b", "a.webm"), event]);
            assert!(!newer.degraded());
            assert_eq!(newer.segments[0].active[0].witness_id, "w-b");
        }
    }
    #[test]
    fn malformed_unknown_duplicate_and_torn_evidence_degrades_read_only_fold() {
        let valid = archive("a", &["a.webm"]);
        let cases = [
            json!({"event_kind":"future"}),
            archive("a", &["a.webm", "a.webm"]),
            json!({"event_kind":"restore", "time":"bad", "day":"20260101", "stream":"_default", "segment":"010000_001"}),
        ];
        for bad in cases {
            let day = folded(&[valid.clone(), bad]);
            assert_eq!(day.skipped_records, 1);
            assert!(day.segments[0].degraded());
        }
        for tail in [format!("{valid}"), "{torn".into()] {
            let day = fold_lines("20260101", format!("{valid}\n{tail}").as_bytes());
            assert_eq!(day.skipped_records, 1);
        }
        assert!(folded(&[valid.clone(), valid]).degraded());
        let duplicate = folded(&[archive_v2("a", "a.webm"), archive_v2("a", "b.webm")]);
        assert_eq!(duplicate.skipped_records, 1);
    }
    #[test]
    fn v2_bindings_are_required_and_legacy_bindings_remain_unknown() {
        let valid = archive_v2("a", "a.webm");
        let day = folded(std::slice::from_ref(&valid));
        assert!(!day.degraded());
        assert_eq!(
            day.segments[0].active[0].segment_dir.as_deref(),
            Some("010000_001_full")
        );
        for key in ["journal_id", "layout", "segment_dir", "repository_id"] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(folded(&[missing]).degraded());
            let mut malformed = valid.clone();
            malformed[key] = json!(false);
            assert!(folded(&[malformed]).degraded());
        }
        let legacy = folded(&[archive("a", &["a.webm"])]);
        let witness = &legacy.segments[0].active[0];
        assert!(witness.repository_id.is_none());
        assert!(witness.layout.is_none());
        assert!(witness.segment_dir.is_none());
        assert_eq!(witness.witness_id, "v1:20260101:1:a.webm");
    }
}

#[cfg(all(test, feature = "full-tests"))]
mod tests {
    use super::*;
    #[test]
    fn damaged_ledger_bytes_are_preserved_and_directory_failures_are_unknown() {
        let journal = tempfile::tempdir().unwrap();
        assert!(!catalog_journal(journal.path()).degraded());
        fs::write(journal.path().join("health"), b"blocked").unwrap();
        assert!(catalog_journal(journal.path()).degraded());
        fs::remove_file(journal.path().join("health")).unwrap();
        let ledger = path(journal.path(), "20260101");
        fs::create_dir_all(ledger.parent().unwrap()).unwrap();
        fs::write(&ledger, b"{broken}\n{torn").unwrap();
        assert_eq!(
            catalog_day(journal.path(), "20260101")
                .unwrap()
                .skipped_records,
            2
        );
        assert_eq!(fs::read(&ledger).unwrap(), b"{broken}\n{torn");
        assert_eq!(fs::read_dir(ledger.parent().unwrap()).unwrap().count(), 1);
        fs::remove_file(&ledger).unwrap();
        fs::create_dir(&ledger).unwrap();
        assert!(catalog_journal(journal.path()).degraded());
    }

    #[test]
    fn append_order_beats_timestamp_and_read_is_read_only() {
        let journal = tempfile::tempdir().unwrap();
        let file = OffloadFile {
            name: "raw.webm".into(),
            bytes: 3,
            sha256: "a".repeat(64),
        };
        append_offload_event(
            journal.path(),
            "20260101",
            "_default",
            "010000",
            "snapshot",
            &[file],
            20,
        )
        .unwrap();
        append_restore_event(journal.path(), "20260101", "_default", "010000", 10).unwrap();
        let before = fs::read(path(journal.path(), "20260101")).unwrap();
        assert!(
            !summarize_segment(journal.path(), "20260101", "_default", "010000")
                .unwrap()
                .currently_offloaded
        );
        assert_eq!(before, fs::read(path(journal.path(), "20260101")).unwrap());
    }

    #[test]
    fn duplicate_and_reordered_events_fold_in_append_order() {
        let journal = tempfile::tempdir().unwrap();
        let file = OffloadFile {
            name: "raw.webm".into(),
            bytes: 7,
            sha256: "b".repeat(64),
        };
        append_restore_event(journal.path(), "20260102", "_default", "020000_001", 99).unwrap();
        append_offload_event(
            journal.path(),
            "20260102",
            "_default",
            "020000_001",
            "first",
            std::slice::from_ref(&file),
            1,
        )
        .unwrap();
        append_offload_event(
            journal.path(),
            "20260102",
            "_default",
            "020000_001",
            "last",
            &[file],
            2,
        )
        .unwrap();

        let summary =
            summarize_segment(journal.path(), "20260102", "_default", "020000_001").unwrap();
        assert!(summary.currently_offloaded);
        assert_eq!(summary.snapshot_id.as_deref(), Some("last"));
    }

    #[test]
    fn malformed_and_wrong_identity_records_degrade_without_summary_writes() {
        let journal = tempfile::tempdir().unwrap();
        let ledger = path(journal.path(), "20260103");
        fs::create_dir_all(ledger.parent().unwrap()).unwrap();
        fs::write(
            &ledger,
            concat!(
                "{\"event_kind\":\"offload\",\"time\":1,\"day\":\"20260103\",\"stream\":\"_default\",\"segment\":\"030000_001\",\"snapshot_id\":\"snapshot\",\"files\":[{\"name\":\"raw.webm\",\"bytes\":3,\"sha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}]}\n",
                "{\"event_kind\":\"offload\",\"time\":2,\"day\":\"wrong\",\"stream\":\"_default\",\"segment\":\"030000_001\",\"snapshot_id\":\"snapshot\",\"files\":[]}\n",
                "{\"event_kind\":\"offload\",\"time\":3,\"day\":\"20260103\",\"stream\":\"_default\",\"segment\":\"030000_001\",\"snapshot_id\":\"snapshot\",\"files\":[{\"name\":\"../wrong.webm\",\"bytes\":3,\"sha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}]}\n",
                "{\"event_kind\":\"offload\"\n"
            ),
        )
        .unwrap();

        let summary =
            summarize_segment(journal.path(), "20260103", "_default", "030000_001").unwrap();
        assert!(summary.currently_offloaded);
        assert_eq!(summary.skipped_records, 3);
        assert!(summary.degraded());
        assert!(summary.files.is_empty());
    }
}
