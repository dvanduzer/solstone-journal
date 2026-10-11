// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Archive history and fresh retained catalog coverage. Historical records and
//! decoded models describe storage; they do not authorize removal.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use solstone_core_journal_io::{
    JournalEntryKind, JournalRoot, media::MediaDirectory, paths::SegmentLayout,
};

use crate::archive_proof::{AcceptedProcessingObservation, ArchiveFileProof, OffloadProof};
use codec::{fold_lines, valid_day};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OffloadFile {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}
/// A single archived generation. Historical records leave unstored bindings unknown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveWitness {
    pub manifest_id: String,
    pub witness_id: String,
    pub snapshot_id: String,
    pub repository_id: Option<String>,
    pub archive_path: Option<String>,
    pub journal_id: Option<String>,
    pub layout: Option<SegmentLayout>,
    pub segment_dir: Option<String>,
    /// Accepted historical observation, never current release authority.
    pub processing: Option<AcceptedProcessingObservation>,
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

    /// Finds one exact active historical witness. This data comparison does not
    /// establish current input, consent, catalog coverage or release authority.
    pub fn active_witness(
        &self,
        proof: &OffloadProof,
        file: &ArchiveFileProof,
    ) -> Option<&ArchiveWitness> {
        let key = solstone_core_format::segment::segment_key(&proof.target.dir)?;
        let mut matches = self
            .days
            .iter()
            .filter(|day| day.day == proof.target.day)
            .flat_map(|day| &day.segments)
            .filter(|segment| segment.stream == proof.target.stream && segment.segment == key)
            .flat_map(|segment| &segment.active)
            .filter(|witness| exact_witness(witness, proof, file));
        let witness = matches.next()?;
        matches.next().is_none().then_some(witness)
    }

    /// Finds one exact historical witness, including generations already restored.
    /// This data comparison supplies no current input or mutation authority.
    pub fn historical_witness(
        &self,
        proof: &OffloadProof,
        file: &ArchiveFileProof,
    ) -> Option<&ArchiveWitness> {
        let key = solstone_core_format::segment::segment_key(&proof.target.dir)?;
        let mut matches = self
            .days
            .iter()
            .filter(|day| day.day == proof.target.day)
            .flat_map(|day| &day.segments)
            .filter(|segment| segment.stream == proof.target.stream && segment.segment == key)
            .flat_map(|segment| &segment.history)
            .flat_map(|manifest| &manifest.files)
            .filter(|witness| exact_witness(witness, proof, file));
        let witness = matches.next()?;
        matches.next().is_none().then_some(witness)
    }
}

fn exact_witness(witness: &ArchiveWitness, proof: &OffloadProof, file: &ArchiveFileProof) -> bool {
    witness.manifest_id == proof.manifest_id
        && witness.witness_id == file.witness_id
        && witness.snapshot_id == proof.snapshot_id
        && witness.journal_id.as_deref() == Some(proof.journal_id.as_str())
        && witness.layout == Some(proof.layout)
        && witness.segment_dir.as_deref() == Some(proof.target.dir.as_str())
        && witness.repository_id.as_deref() == Some(proof.repository_id.as_str())
        && witness.archive_path.as_deref() == Some(file.archive_path.as_str())
        && witness.file.name == file.name
        && witness.file.bytes == file.bytes
        && witness.file.sha256 == file.sha256
        && witness.processing.as_ref() == Some(&file.processing)
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreFileV2 {
    pub witness_id: String,
    pub name: String,
    pub disposition: RestoreDisposition,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreOperation {
    pub operation_id: String,
    pub files: Vec<RestoreFileV2>,
}
/// Shared storage codec for archive adapters. Decoded values carry no authority.
#[doc(hidden)]
pub mod codec {
    use std::collections::{BTreeMap, BTreeSet};

    use serde::{Deserialize, Serialize};
    use solstone_core_journal_io::paths::SegmentLayout;

    use super::{
        ArchiveManifest, ArchiveWitness, DayCatalog, OffloadFile, RestoreDisposition,
        RestoreFileV2, RestoreOperation, SegmentCatalog,
    };
    use crate::archive_proof::{ArchiveFileProof, OffloadProof};
    use crate::receipt::Target;

    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(tag = "event_kind", deny_unknown_fields)]
    pub enum Event {
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
            files: Vec<ArchiveFileProof>,
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
    pub fn valid_day(day: &str) -> bool {
        day.len() == 8 && day.bytes().all(|byte| byte.is_ascii_digit())
    }
    pub fn component(name: &str) -> bool {
        !name.is_empty() && !matches!(name, "." | "..") && !name.contains(['/', '\\', ':', '\0'])
    }
    fn digest(value: &str) -> bool {
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }
    pub fn valid_file(file: &OffloadFile) -> bool {
        component(&file.name) && digest(&file.sha256)
    }
    #[derive(Default)]
    struct Fold {
        catalog: SegmentCatalog,
        // Layout and exact basename are part of the target, not inferred from stream/key.
        active: BTreeMap<(Option<SegmentLayout>, Option<String>, String), ArchiveWitness>,
        latest_legacy_scope: Vec<String>,
    }
    pub fn fold_lines(day: &str, bytes: &[u8]) -> DayCatalog {
        let mut folds = BTreeMap::<(String, String), Fold>::new();
        let mut skipped = 0_u64;
        let mut manifests = BTreeSet::new();
        let mut witnesses = BTreeSet::new();
        let mut operations = BTreeSet::new();
        let mut archive_records = BTreeSet::new();
        for (index, line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
            if !line.ends_with(b"\n") {
                skipped = skipped.saturating_add(1);
                continue;
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let Ok(event) = serde_json::from_slice::<Event>(line) else {
                skipped = skipped.saturating_add(1);
                continue;
            };
            if matches!(&event, Event::Archive { .. } | Event::ArchiveV2 { .. }) {
                let Ok(canonical) = serde_json::to_string(&event) else {
                    skipped = skipped.saturating_add(1);
                    continue;
                };
                if !archive_records.insert(canonical) {
                    skipped = skipped.saturating_add(1);
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
                skipped = skipped.saturating_add(1);
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
            let archive =
                match event {
                    Event::Archive {
                        snapshot_id,
                        files,
                        time: _time,
                        ..
                    } => {
                        if snapshot_id.is_empty()
                            || files.is_empty()
                            || files.iter().any(|f| !valid_file(f))
                            || files.iter().map(|f| &f.name).collect::<BTreeSet<_>>().len()
                                != files.len()
                        {
                            skipped = skipped.saturating_add(1);
                            continue;
                        }
                        let Some(ordinal) = index.checked_add(1) else {
                            skipped = skipped.saturating_add(1);
                            continue;
                        };
                        let manifest_id = format!("v1:{day}:{ordinal}");
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
                                processing: None,
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
                        time: _time,
                        ..
                    } => {
                        let proof = OffloadProof {
                            journal_id,
                            manifest_id,
                            target: Target {
                                day: day.into(),
                                stream: fold.catalog.stream.clone(),
                                dir: segment_dir,
                            },
                            layout,
                            repository_id,
                            snapshot_id,
                            files,
                        };
                        if schema_version != 2
                        || proof.manifest_id.starts_with("v1:")
                        || manifests.contains(&proof.manifest_id)
                        || !component(&proof.target.dir)
                        // Keep names representable by the catalog's restore decoder.
                        || proof.files.iter().any(|f| {
                            f.witness_id.starts_with("v1:")
                                || witnesses.contains(&f.witness_id)
                                || !component(&f.name)
                        })
                        || proof.validate().is_err()
                        {
                            skipped = skipped.saturating_add(1);
                            continue;
                        }
                        let OffloadProof {
                            journal_id,
                            manifest_id,
                            target,
                            layout,
                            repository_id,
                            snapshot_id,
                            files,
                        } = proof;
                        let files = files
                            .into_iter()
                            .map(|f| ArchiveWitness {
                                manifest_id: manifest_id.clone(),
                                witness_id: f.witness_id,
                                snapshot_id: snapshot_id.clone(),
                                repository_id: Some(repository_id.clone()),
                                journal_id: Some(journal_id.clone()),
                                layout: Some(layout),
                                segment_dir: Some(target.dir.clone()),
                                archive_path: Some(f.archive_path),
                                processing: Some(f.processing),
                                file: OffloadFile {
                                    name: f.name,
                                    bytes: f.bytes,
                                    sha256: f.sha256,
                                },
                            })
                            .collect();
                        Some(ArchiveManifest { manifest_id, files })
                    }
                    Event::Restore { time: _time, .. } => {
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
                        time: _time,
                        ..
                    } => {
                        if schema_version != 2
                            || operation_id.is_empty()
                            || files.is_empty()
                            || operations.contains(&operation_id)
                            || files.iter().any(|f| {
                                !component(&f.name)
                                    || f.witness_id.is_empty()
                                    || !fold.catalog.history.iter().flat_map(|m| &m.files).any(
                                        |w| w.witness_id == f.witness_id && w.file.name == f.name,
                                    )
                            })
                            || files
                                .iter()
                                .map(|f| &f.witness_id)
                                .collect::<BTreeSet<_>>()
                                .len()
                                != files.len()
                            || files.iter().map(|f| &f.name).collect::<BTreeSet<_>>().len()
                                != files.len()
                        {
                            skipped = skipped.saturating_add(1);
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
                fold.latest_legacy_scope =
                    if archive.files.iter().all(|w| w.repository_id.is_none()) {
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
}

/// Fresh catalog coverage for an operation retaining its own admitted root.
///
/// Retained directories and exact leaf observations provide read integrity, not
/// an immutable snapshot or backup-safety capability. Callers retain their own
/// policy/admission and read again at each boundary.
/// Local byte identity and unresolved restore obligations are not checked here.
pub fn read_complete_catalog(root: &Arc<JournalRoot>) -> Result<JournalCatalog, String> {
    read_complete_catalog_with_checkpoint(root, |_| {})
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CatalogReadCheckpoint {
    AfterListing,
    AfterRead,
    BeforeAbsentRecheck,
}

fn read_complete_catalog_with_checkpoint(
    root: &Arc<JournalRoot>,
    mut checkpoint: impl FnMut(CatalogReadCheckpoint),
) -> Result<JournalCatalog, String> {
    root.revalidate_canonical_binding()
        .map_err(|error| error.to_string())?;
    let mut directory =
        MediaDirectory::open(Arc::clone(root), Path::new("")).map_err(|error| error.to_string())?;
    for name in ["health", "offload"] {
        let name = OsStr::new(name);
        match directory
            .open_child(name)
            .map_err(|error| error.to_string())?
        {
            Some(child) => directory = child,
            None => {
                checkpoint(CatalogReadCheckpoint::BeforeAbsentRecheck);
                if directory
                    .open_child(name)
                    .map_err(|error| error.to_string())?
                    .is_some()
                {
                    return Err("offload catalog changed during read".into());
                }
                directory.revalidate().map_err(|error| error.to_string())?;
                root.revalidate_canonical_binding()
                    .map_err(|error| error.to_string())?;
                return Ok(JournalCatalog::default());
            }
        }
    }
    let entries = directory
        .entries(usize::MAX)
        .map_err(|error| error.to_string())?
        .ok_or("offload catalog coverage is incomplete")?;
    checkpoint(CatalogReadCheckpoint::AfterListing);
    let mut catalog = JournalCatalog::default();
    for entry in &entries {
        let file = Path::new(&entry.name);
        if file
            .extension()
            .is_none_or(|extension| extension != "jsonl")
        {
            continue;
        }
        let day = file
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|day| valid_day(day))
            .ok_or("offload catalog has an invalid ledger name")?;
        if entry.kind != JournalEntryKind::RegularFile {
            return Err("offload catalog ledger is not a plain file".into());
        }
        let maximum = usize::try_from(entry.size).map_err(|_| "offload ledger is too large")?;
        let observed = directory
            .read_file_bounded(&entry.name, maximum)
            .map_err(|error| error.to_string())?
            .ok_or("offload catalog changed during read")?;
        if !observed.complete || observed.entry != *entry || observed.bytes.len() != maximum {
            return Err("offload catalog changed during read".into());
        }
        let day = fold_lines(day, &observed.bytes);
        if day.degraded() {
            return Err("offload catalog coverage is incomplete".into());
        }
        catalog.days.push(day);
        checkpoint(CatalogReadCheckpoint::AfterRead);
    }
    let current = directory
        .entries(usize::MAX)
        .map_err(|error| error.to_string())?
        .ok_or("offload catalog coverage is incomplete")?;
    if entries != current {
        return Err("offload catalog changed during read".into());
    }
    directory.revalidate().map_err(|error| error.to_string())?;
    root.revalidate_canonical_binding()
        .map_err(|error| error.to_string())?;
    catalog.days.sort_by(|a, b| a.day.cmp(&b.day));
    Ok(catalog)
}

#[cfg(all(test, feature = "full-tests"))]
#[allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::unreachable
)]
#[path = "../tests/support/archive_catalog_reader.rs"]
mod reader_tests;
