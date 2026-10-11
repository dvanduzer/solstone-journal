// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Read-only observations of the owned stage and restore-intent namespaces.
//! Empty observations are not a tree snapshot or exclusion against creators.
//! Destructive consumers require independently admitted cooperation and must
//! repeat this observation at each boundary. Saved observations grant no IO.

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use solstone_core_journal_io::journal_root::{JournalEntryKind, JournalRoot};
use solstone_core_journal_io::media::{MediaDirectory, MediaEntry};

use crate::restore_intent::{MAX_INTENT_BYTES, RestoreIntent};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryBudget {
    DirectoryEntries,
    Directories,
    TotalEntries,
    IntentBytes,
    Obligations,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryObservationFailure {
    Io,
    Changed,
    NotDirectory,
    Budget(RecoveryBudget),
}

#[derive(Debug)]
pub struct RecoveryObservationError {
    pub failure: RecoveryObservationFailure,
    pub relative: PathBuf,
    pub source: Option<io::Error>,
}

impl fmt::Display for RecoveryObservationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "recovery observation {:?} at {}",
            self.failure,
            self.relative.display()
        )
    }
}

impl std::error::Error for RecoveryObservationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|error| error as _)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntentProblem {
    UnknownEntry,
    NotPlainFile,
    Oversized,
    InvalidRecord,
    OperationNameMismatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IntentObservation {
    Recorded { operation_id: String },
    Uncertain(IntentProblem),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryObligation {
    Stage {
        relative: PathBuf,
        entry: MediaEntry,
    },
    Intent {
        relative: PathBuf,
        entry: MediaEntry,
        observation: IntentObservation,
    },
}

/// Namespace evidence only. Even serialized-complete records remain obligations.
#[derive(Debug)]
pub struct RecoveryObligations {
    entries: Vec<RecoveryObligation>,
}

impl RecoveryObligations {
    pub fn entries(&self) -> &[RecoveryObligation] {
        &self.entries
    }
}

#[derive(Clone, Copy)]
struct Limits {
    entries: usize,
    directories: usize,
    total_entries: usize,
    intent_bytes: usize,
    obligations: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            entries: 65_536,
            directories: 131_072,
            total_entries: 4_194_304,
            intent_bytes: 16_777_216,
            obligations: 4_096,
        }
    }
}

impl Limits {
    fn maximum(self, budget: RecoveryBudget) -> usize {
        match budget {
            RecoveryBudget::DirectoryEntries => self.entries,
            RecoveryBudget::Directories => self.directories,
            RecoveryBudget::TotalEntries => self.total_entries,
            RecoveryBudget::IntentBytes => self.intent_bytes,
            RecoveryBudget::Obligations => self.obligations,
        }
    }
}

#[derive(Default)]
struct Budget {
    directories: usize,
    entries: usize,
    intent_bytes: usize,
    obligations: usize,
}

fn error(failure: RecoveryObservationFailure, relative: &Path) -> RecoveryObservationError {
    RecoveryObservationError {
        failure,
        relative: relative.into(),
        source: None,
    }
}

fn io_error(relative: &Path, source: io::Error) -> RecoveryObservationError {
    RecoveryObservationError {
        failure: RecoveryObservationFailure::Io,
        relative: relative.into(),
        source: Some(source),
    }
}

fn charge(
    used: &mut usize,
    amount: usize,
    maximum: usize,
    budget: RecoveryBudget,
    relative: &Path,
) -> Result<(), RecoveryObservationError> {
    *used = used
        .checked_add(amount)
        .filter(|value| *value <= maximum)
        .ok_or_else(|| error(RecoveryObservationFailure::Budget(budget), relative))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum Role {
    Root,
    Chronicle(u8),
    Health,
    Offload,
    Intents,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Checkpoint {
    Listed,
    Read,
    Missing,
    BeforeFinalList,
    FinishedDirectory,
}

struct Reader<F> {
    limits: Limits,
    budget: Budget,
    obligations: Vec<RecoveryObligation>,
    checkpoint: F,
}

/// Census stage names in root/chronicle/day/day-child and health/offload,
/// and all entries in health/offload/restore-intents.
/// No directory or file is created, rewritten, removed, followed or repaired.
pub fn observe_obligations(
    root: &Arc<JournalRoot>,
) -> Result<RecoveryObligations, RecoveryObservationError> {
    observe_with(root, Limits::default(), |_, _| {})
}

// Private instrumentation cannot return admission or grant mutation authority.
fn observe_with(
    root: &Arc<JournalRoot>,
    limits: Limits,
    checkpoint: impl FnMut(Checkpoint, &Path),
) -> Result<RecoveryObligations, RecoveryObservationError> {
    root.revalidate_canonical_binding()
        .map_err(|source| io_error(Path::new(""), io::Error::other(source)))?;
    let directory = MediaDirectory::open(Arc::clone(root), Path::new(""))
        .map_err(|source| io_error(Path::new(""), source))?;
    let mut reader = Reader {
        limits,
        budget: Budget::default(),
        obligations: Vec::new(),
        checkpoint,
    };
    reader.walk(&directory, Path::new(""), Role::Root)?;
    root.revalidate_canonical_binding()
        .map_err(|source| io_error(Path::new(""), io::Error::other(source)))?;
    Ok(RecoveryObligations {
        entries: reader.obligations,
    })
}

impl<F: FnMut(Checkpoint, &Path)> Reader<F> {
    fn entries(
        &mut self,
        directory: &MediaDirectory,
        relative: &Path,
    ) -> Result<Vec<MediaEntry>, RecoveryObservationError> {
        let entries = directory
            .entries(self.limits.maximum(RecoveryBudget::DirectoryEntries))
            .map_err(|source| io_error(relative, source))?
            .ok_or_else(|| {
                error(
                    RecoveryObservationFailure::Budget(RecoveryBudget::DirectoryEntries),
                    relative,
                )
            })?;
        charge(
            &mut self.budget.entries,
            entries.len(),
            self.limits.maximum(RecoveryBudget::TotalEntries),
            RecoveryBudget::TotalEntries,
            relative,
        )?;
        Ok(entries)
    }

    fn add(
        &mut self,
        obligation: RecoveryObligation,
        relative: &Path,
    ) -> Result<(), RecoveryObservationError> {
        charge(
            &mut self.budget.obligations,
            1,
            self.limits.maximum(RecoveryBudget::Obligations),
            RecoveryBudget::Obligations,
            relative,
        )?;
        self.obligations.push(obligation);
        Ok(())
    }

    fn child(
        &mut self,
        directory: &MediaDirectory,
        entry: &MediaEntry,
        relative: &Path,
        role: Role,
    ) -> Result<(), RecoveryObservationError> {
        let relative = relative.join(&entry.name);
        if entry.kind != JournalEntryKind::Directory {
            return Err(error(RecoveryObservationFailure::NotDirectory, &relative));
        }
        let child = directory
            .open_child(&entry.name)
            .map_err(|source| io_error(&relative, source))?
            .ok_or_else(|| error(RecoveryObservationFailure::Changed, &relative))?;
        if child.identity() != entry.identity {
            return Err(error(RecoveryObservationFailure::Changed, &relative));
        }
        self.walk(&child, &relative, role)
    }

    fn frontier(
        &mut self,
        directory: &MediaDirectory,
        entries: &[MediaEntry],
        relative: &Path,
        name: &str,
        role: Role,
        missing: &mut Vec<String>,
    ) -> Result<(), RecoveryObservationError> {
        if let Some(entry) = entries.iter().find(|entry| entry.name == OsStr::new(name)) {
            return self.child(directory, entry, relative, role);
        }
        let child_relative = relative.join(name);
        if directory
            .open_child(OsStr::new(name))
            .map_err(|source| io_error(&child_relative, source))?
            .is_some()
        {
            return Err(error(RecoveryObservationFailure::Changed, &child_relative));
        }
        missing.push(name.into());
        (self.checkpoint)(Checkpoint::Missing, &child_relative);
        Ok(())
    }

    fn intent(
        &mut self,
        directory: &MediaDirectory,
        entry: &MediaEntry,
        relative: &Path,
    ) -> Result<(), RecoveryObservationError> {
        let relative = relative.join(&entry.name);
        let operation = entry
            .name
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
            .filter(|id| {
                id.len() == 32
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            });
        let observation = match operation {
            None => IntentObservation::Uncertain(IntentProblem::UnknownEntry),
            Some(_) if entry.kind != JournalEntryKind::RegularFile => {
                IntentObservation::Uncertain(IntentProblem::NotPlainFile)
            }
            Some(_) if entry.size > MAX_INTENT_BYTES as u64 => {
                IntentObservation::Uncertain(IntentProblem::Oversized)
            }
            Some(operation) => {
                charge(
                    &mut self.budget.intent_bytes,
                    entry.size as usize,
                    self.limits.maximum(RecoveryBudget::IntentBytes),
                    RecoveryBudget::IntentBytes,
                    &relative,
                )?;
                let read = directory
                    .read_file_bounded(&entry.name, MAX_INTENT_BYTES)
                    .map_err(|source| io_error(&relative, source))?
                    .ok_or_else(|| error(RecoveryObservationFailure::Changed, &relative))?;
                if !read.complete || read.entry != *entry {
                    return Err(error(RecoveryObservationFailure::Changed, &relative));
                }
                (self.checkpoint)(Checkpoint::Read, &relative);
                match RestoreIntent::decode(Ok(&read.bytes)) {
                    Ok(intent) if intent.event.operation_id == operation => {
                        IntentObservation::Recorded {
                            operation_id: operation.into(),
                        }
                    }
                    Ok(_) => IntentObservation::Uncertain(IntentProblem::OperationNameMismatch),
                    Err(_) => IntentObservation::Uncertain(IntentProblem::InvalidRecord),
                }
            }
        };
        self.add(
            RecoveryObligation::Intent {
                relative: relative.clone(),
                entry: entry.clone(),
                observation,
            },
            &relative,
        )
    }

    fn walk(
        &mut self,
        directory: &Arc<MediaDirectory>,
        relative: &Path,
        role: Role,
    ) -> Result<(), RecoveryObservationError> {
        charge(
            &mut self.budget.directories,
            1,
            self.limits.maximum(RecoveryBudget::Directories),
            RecoveryBudget::Directories,
            relative,
        )?;
        let entries = self.entries(directory, relative)?;
        (self.checkpoint)(Checkpoint::Listed, relative);
        let mut missing = Vec::new();
        for entry in &entries {
            if matches!(role, Role::Intents) {
                self.intent(directory, entry, relative)?;
            } else if entry
                .name
                .to_str()
                .is_some_and(|name| name.starts_with(".tmp-offload-"))
            {
                let stage_relative = relative.join(&entry.name);
                self.add(
                    RecoveryObligation::Stage {
                        relative: stage_relative.clone(),
                        entry: entry.clone(),
                    },
                    &stage_relative,
                )?;
            } else if let Role::Chronicle(depth @ 0..=1) = role {
                match entry.kind {
                    JournalEntryKind::Directory => {
                        self.child(directory, entry, relative, Role::Chronicle(depth + 1))?
                    }
                    JournalEntryKind::RegularFile => {}
                    _ => {
                        return Err(error(
                            RecoveryObservationFailure::NotDirectory,
                            &relative.join(&entry.name),
                        ));
                    }
                }
            }
        }
        match role {
            Role::Root => {
                self.frontier(
                    directory,
                    &entries,
                    relative,
                    "chronicle",
                    Role::Chronicle(0),
                    &mut missing,
                )?;
                self.frontier(
                    directory,
                    &entries,
                    relative,
                    "health",
                    Role::Health,
                    &mut missing,
                )?;
            }
            Role::Health => self.frontier(
                directory,
                &entries,
                relative,
                "offload",
                Role::Offload,
                &mut missing,
            )?,
            Role::Offload => self.frontier(
                directory,
                &entries,
                relative,
                "restore-intents",
                Role::Intents,
                &mut missing,
            )?,
            _ => {}
        }
        (self.checkpoint)(Checkpoint::BeforeFinalList, relative);
        if self.entries(directory, relative)? != entries {
            return Err(error(RecoveryObservationFailure::Changed, relative));
        }
        for name in missing {
            let child_relative = relative.join(&name);
            if directory
                .open_child(OsStr::new(&name))
                .map_err(|source| io_error(&child_relative, source))?
                .is_some()
            {
                return Err(error(RecoveryObservationFailure::Changed, &child_relative));
            }
        }
        directory
            .revalidate()
            .map_err(|source| io_error(relative, source))?;
        (self.checkpoint)(Checkpoint::FinishedDirectory, relative);
        Ok(())
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn actual_production_budget_limits_accept_exactly_and_refuse_one_more() {
        let limits = Limits::default();
        for budget in [
            RecoveryBudget::DirectoryEntries,
            RecoveryBudget::Directories,
            RecoveryBudget::TotalEntries,
            RecoveryBudget::IntentBytes,
            RecoveryBudget::Obligations,
        ] {
            let maximum = limits.maximum(budget);
            let mut used = 0;
            charge(&mut used, maximum, maximum, budget, Path::new("fixture")).unwrap();
            assert_eq!(used, maximum);
            assert_eq!(
                charge(&mut used, 1, maximum, budget, Path::new("fixture"))
                    .unwrap_err()
                    .failure,
                RecoveryObservationFailure::Budget(budget)
            );
        }
        let mut used = usize::MAX;
        assert!(
            charge(
                &mut used,
                1,
                usize::MAX,
                RecoveryBudget::TotalEntries,
                Path::new("")
            )
            .is_err()
        );
    }
}

#[cfg(all(test, feature = "full-tests"))]
mod tests {
    use super::*;
    use std::fs;

    use crate::ledger::RestoreDisposition;
    use crate::restore_intent::tests::fixture;
    use crate::restore_intent::{ConfirmedContent, Obligation};

    struct Fixture {
        temp: tempfile::TempDir,
        root: Arc<JournalRoot>,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
            Self { temp, root }
        }
        fn path(&self, relative: &str) -> PathBuf {
            self.temp.path().join(relative)
        }
        fn directory(&self, relative: &str) {
            fs::create_dir_all(self.path(relative)).unwrap();
        }
        fn observed(&self) -> Result<RecoveryObligations, RecoveryObservationError> {
            observe_obligations(&self.root)
        }
        fn intents(&self) -> PathBuf {
            self.directory("health/offload/restore-intents");
            self.path("health/offload/restore-intents")
        }
    }

    #[test]
    fn absent_empty_and_ordinary_media_are_complete_without_mutation() {
        let f = Fixture::new();
        assert!(f.observed().unwrap().entries().is_empty());
        assert_eq!(fs::read_dir(f.temp.path()).unwrap().count(), 0);
        f.directory("chronicle/20260101/_default/010000_001");
        f.directory("chronicle/20260101/020000_001");
        fs::write(f.path("chronicle/20260101/020000_001/.hidden.flac"), b"raw").unwrap();
        f.directory("health/offload/restore-intents");
        fs::write(
            f.path("health/offload/20260101.jsonl"),
            b"legacy history belongs to Catalog",
        )
        .unwrap();
        assert!(f.observed().unwrap().entries().is_empty());
        assert_eq!(
            fs::read(f.path("chronicle/20260101/020000_001/.hidden.flac")).unwrap(),
            b"raw"
        );
    }

    #[test]
    fn every_stage_kind_and_segment_like_stream_remains_held() {
        let f = Fixture::new();
        for name in [
            ".tmp-offload-root",
            "chronicle/.tmp-offload-chronicle",
            "chronicle/20260101/.tmp-offload-bad-suffix",
            "chronicle/20260101/_default/.tmp-offload-named",
            "chronicle/20260101/000000_300/.tmp-offload-ambiguous",
        ] {
            f.directory(name);
        }
        fs::write(f.path("chronicle/20260101/.tmp-offload-file"), b"only raw").unwrap();
        fs::write(
            f.path("chronicle/20260101/000000_300/.tmp-offload-ambiguous/payload"),
            b"sole raw",
        )
        .unwrap();
        let observed = f.observed().unwrap();
        assert_eq!(observed.entries().len(), 6);
        assert!(
            observed
                .entries()
                .iter()
                .all(|entry| matches!(entry, RecoveryObligation::Stage { .. }))
        );
        drop(observed);
        assert_eq!(
            fs::read(f.path("chronicle/20260101/000000_300/.tmp-offload-ambiguous/payload"))
                .unwrap(),
            b"sole raw"
        );
        assert_eq!(
            fs::read(f.path("chronicle/20260101/.tmp-offload-file")).unwrap(),
            b"only raw"
        );
    }

    #[test]
    fn strict_pending_complete_and_copied_root_intents_all_hold() {
        for complete in [false, true] {
            let f = Fixture::new();
            let mut intent = fixture(false);
            assert_ne!(intent.destination.root, f.root.identity().observation_id());
            if complete {
                intent.event.files[0].disposition = RestoreDisposition::AlreadyPresent;
                intent.confirmed = Some(ConfirmedContent {
                    identity: intent.stage.leaf,
                    digest: intent.expected_digest(),
                });
                intent.marker = Obligation::Complete;
                intent.event_recorded = Obligation::Complete;
                intent.cleanup = Obligation::Complete;
            }
            let bytes = intent.encode().unwrap();
            let path = f
                .intents()
                .join(format!("{}.json", intent.event.operation_id));
            fs::write(&path, &bytes).unwrap();
            let observed = f.observed().unwrap();
            assert!(matches!(observed.entries(), [RecoveryObligation::Intent {
                observation: IntentObservation::Recorded { operation_id }, ..
            }] if operation_id == &intent.event.operation_id));
            drop(observed);
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }

    #[test]
    fn torn_unknown_oversized_and_unrecognized_carriers_are_preserved_holds() {
        let f = Fixture::new();
        let intent = fixture(false);
        let bytes = intent.encode().unwrap();
        let directory = f.intents();
        let path = directory.join(format!("{}.json", intent.event.operation_id));
        let mut unknown: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        unknown["future"] = serde_json::json!(1);
        let mut version: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        version["version"] = serde_json::json!(999);
        for data in [
            bytes[..bytes.len() - 1].to_vec(),
            serde_json::to_vec(&unknown).unwrap(),
            serde_json::to_vec(&version).unwrap(),
            vec![b' '; MAX_INTENT_BYTES + 1],
        ] {
            fs::write(&path, &data).unwrap();
            assert!(matches!(
                f.observed().unwrap().entries(),
                [RecoveryObligation::Intent {
                    observation: IntentObservation::Uncertain(_),
                    ..
                }]
            ));
            assert_eq!(fs::read(&path).unwrap(), data);
        }
        fs::remove_file(&path).unwrap();
        fs::write(directory.join("unknown.lock"), b"unknown").unwrap();
        fs::create_dir(directory.join("empty")).unwrap();
        fs::write(
            directory.join("00000000000000000000000000000000.json"),
            &bytes,
        )
        .unwrap();
        assert_eq!(f.observed().unwrap().entries().len(), 3);
    }

    #[test]
    fn missing_frontier_creation_and_late_stage_append_refuse_empty_coverage() {
        for missing in [
            "chronicle",
            "health",
            "health/offload",
            "health/offload/restore-intents",
        ] {
            let f = Fixture::new();
            if let Some(parent) = Path::new(missing).parent() {
                f.directory(parent.to_str().unwrap());
            }
            let mut hit = false;
            let result = observe_with(&f.root, Limits::default(), |point, relative| {
                if point == Checkpoint::Missing && relative == Path::new(missing) {
                    fs::create_dir(f.path(missing)).unwrap();
                    hit = true;
                }
            });
            assert!(hit);
            assert!(result.is_err());
            assert!(f.path(missing).is_dir());
        }
        let f = Fixture::new();
        f.directory("chronicle/20260101/stream");
        let stage = f.path("chronicle/20260101/stream/.tmp-offload-late");
        let result = observe_with(&f.root, Limits::default(), |point, relative| {
            if point == Checkpoint::BeforeFinalList
                && relative == Path::new("chronicle/20260101/stream")
            {
                fs::create_dir(&stage).unwrap();
            }
        });
        assert!(result.is_err());
        assert!(stage.is_dir());
    }

    #[test]
    fn sequential_observations_do_not_exclude_a_later_deep_stage() {
        let f = Fixture::new();
        f.directory("chronicle/20260101/stream");
        let stage = f.path("chronicle/20260101/stream/.tmp-offload-after-final");
        let mut hit = false;
        let observed = observe_with(&f.root, Limits::default(), |point, relative| {
            if point == Checkpoint::FinishedDirectory && relative == Path::new("chronicle/20260101")
            {
                fs::create_dir(&stage).unwrap();
                hit = true;
            }
        })
        .unwrap();
        assert!(hit);
        assert!(observed.entries().is_empty());
        assert!(stage.is_dir());
        assert_eq!(f.observed().unwrap().entries().len(), 1);
    }

    #[test]
    fn intent_replacement_growth_shrink_and_removal_refuse_complete_observation() {
        for change in 0..4 {
            let f = Fixture::new();
            let intent = fixture(false);
            let path = f
                .intents()
                .join(format!("{}.json", intent.event.operation_id));
            fs::write(&path, intent.encode().unwrap()).unwrap();
            let mut hit = false;
            let observed = observe_with(&f.root, Limits::default(), |point, _| {
                if point == Checkpoint::Read {
                    match change {
                        0 => {
                            fs::rename(&path, path.with_extension("saved")).unwrap();
                            fs::write(&path, b"competitor").unwrap();
                        }
                        1 => {
                            use std::io::Write;
                            fs::OpenOptions::new()
                                .append(true)
                                .open(&path)
                                .unwrap()
                                .write_all(b" ")
                                .unwrap();
                        }
                        2 => fs::write(&path, b"short").unwrap(),
                        _ => fs::remove_file(&path).unwrap(),
                    }
                    hit = true;
                }
            });
            assert!(hit);
            assert!(observed.is_err());
            if change == 0 {
                assert_eq!(fs::read(&path).unwrap(), b"competitor");
            }
        }
    }

    #[test]
    fn actual_traversal_propagates_every_budget_and_native_read_failure() {
        let f = Fixture::new();
        let intent = fixture(false);
        let path = f
            .intents()
            .join(format!("{}.json", intent.event.operation_id));
        fs::write(&path, intent.encode().unwrap()).unwrap();
        for budget in [
            RecoveryBudget::DirectoryEntries,
            RecoveryBudget::Directories,
            RecoveryBudget::TotalEntries,
            RecoveryBudget::IntentBytes,
            RecoveryBudget::Obligations,
        ] {
            let mut limits = Limits::default();
            match budget {
                RecoveryBudget::DirectoryEntries => limits.entries = 0,
                RecoveryBudget::Directories => limits.directories = 0,
                RecoveryBudget::TotalEntries => limits.total_entries = 0,
                RecoveryBudget::IntentBytes => limits.intent_bytes = 0,
                RecoveryBudget::Obligations => limits.obligations = 0,
            }
            assert_eq!(
                observe_with(&f.root, limits, |_, _| {})
                    .unwrap_err()
                    .failure,
                RecoveryObservationFailure::Budget(budget)
            );
        }
        for replacement in 0..3 {
            let f = Fixture::new();
            let path = f
                .intents()
                .join(format!("{}.json", intent.event.operation_id));
            fs::write(&path, intent.encode().unwrap()).unwrap();
            let mut hit = false;
            let observed = observe_with(&f.root, Limits::default(), |cut, relative| {
                if !hit
                    && cut == Checkpoint::Listed
                    && relative == Path::new("health/offload/restore-intents")
                {
                    if replacement == 2 {
                        fs::rename(&path, path.with_extension("saved")).unwrap();
                        fs::write(&path, intent.encode().unwrap()).unwrap();
                    } else {
                        fs::remove_file(&path).unwrap();
                        if replacement == 1 {
                            fs::create_dir(&path).unwrap();
                        }
                    }
                    hit = true;
                }
            });
            assert!(hit);
            assert!(observed.is_err());
            match replacement {
                0 => assert!(!path.exists()),
                1 => assert!(path.is_dir()),
                _ => assert_eq!(fs::read(&path).unwrap(), intent.encode().unwrap()),
            }
        }
    }

    #[test]
    fn occupied_required_directories_refuse_without_changing_contents() {
        for relative in [
            "chronicle",
            "health",
            "health/offload",
            "health/offload/restore-intents",
        ] {
            let f = Fixture::new();
            if let Some(parent) = Path::new(relative).parent() {
                f.directory(parent.to_str().unwrap());
            }
            fs::write(f.path(relative), b"occupied").unwrap();
            assert_eq!(
                f.observed().unwrap_err().failure,
                RecoveryObservationFailure::NotDirectory
            );
            assert_eq!(fs::read(f.path(relative)).unwrap(), b"occupied");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_stage_holds_but_symlink_traversal_is_uncertain_and_never_followed() {
        let f = Fixture::new();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("sentinel"), b"outside").unwrap();
        f.directory("chronicle/20260101");
        std::os::unix::fs::symlink(
            outside.path(),
            f.path("chronicle/20260101/.tmp-offload-link"),
        )
        .unwrap();
        assert_eq!(f.observed().unwrap().entries().len(), 1);
        std::os::unix::fs::symlink(outside.path(), f.path("chronicle/20260101/stream")).unwrap();
        assert!(f.observed().is_err());
        assert_eq!(
            fs::read(outside.path().join("sentinel")).unwrap(),
            b"outside"
        );
    }

    #[cfg(unix)]
    #[test]
    fn parent_and_root_replacement_preserve_competitors_and_refuse() {
        for root_swap in [false, true] {
            let f = Fixture::new();
            f.directory("chronicle/20260101/stream");
            let original = if root_swap {
                f.temp.path().to_path_buf()
            } else {
                f.path("chronicle/20260101")
            };
            let saved = original.with_extension("saved-census");
            let mut hit = false;
            let observed = observe_with(&f.root, Limits::default(), |point, relative| {
                if !hit
                    && point == Checkpoint::Listed
                    && relative == Path::new("chronicle/20260101/stream")
                {
                    fs::rename(&original, &saved).unwrap();
                    fs::create_dir(&original).unwrap();
                    fs::write(original.join("sentinel"), b"replacement").unwrap();
                    hit = true;
                }
            });
            assert!(hit);
            assert!(observed.is_err());
            assert_eq!(fs::read(original.join("sentinel")).unwrap(), b"replacement");
            if root_swap {
                fs::remove_dir_all(saved).unwrap();
            }
        }
    }
}
