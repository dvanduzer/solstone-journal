// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! A receipt for saved-file publication, never evidence of model completion.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solstone_core_indexer_store::{SavedPublicationOutcome, attempt_saved_publication};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Publication {
    version: u32,
    source_mtime_ms: Option<i64>,
    files: BTreeMap<String, String>,
}

fn receipt_path(segment: &Path) -> PathBuf {
    segment.join("talents/publication.json")
}

fn snapshot(
    journal: &Path,
    segment: &Path,
    source_mtime_ms: Option<i64>,
) -> Result<Publication, String> {
    let mut files = BTreeMap::new();
    for directory in [segment.to_path_buf(), segment.join("talents")] {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        for entry in entries {
            let entry = entry.map_err(|error| error.to_string())?;
            if !entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_file()
            {
                continue;
            }
            let path = entry.path();
            let rel = solstone_core_format::paths::relative_to_journal(journal, &path)
                .ok_or("publication outside journal")?;
            if solstone_core_format::content::resolve_spec(&rel).is_none()
                && solstone_core_indexer::edges::registry::edge_source_for_rel(&rel)
                    .map_err(|error| error.to_string())?
                    .is_none()
            {
                continue;
            }
            let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
            let physical = path
                .strip_prefix(journal)
                .map_err(|error| error.to_string())?
                .to_str()
                .ok_or("publication path is not UTF-8")?
                .to_owned();
            files.insert(physical, format!("{:x}", Sha256::digest(bytes)));
        }
    }
    Ok(Publication {
        version: 1,
        source_mtime_ms,
        files,
    })
}

pub fn is_current(
    journal: &Path,
    segment: &Path,
    source_mtime_ms: Option<i64>,
) -> Result<bool, String> {
    let saved: Publication = match std::fs::read(receipt_path(segment)) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(saved) => saved,
            Err(_) => return Ok(false),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.to_string()),
    };
    Ok(saved == snapshot(journal, segment, source_mtime_ms)?)
}

/// Publish existing outputs without calling a model or rewriting those outputs.
/// A changed source/output or any failed/declined publication remains owed.
pub fn publish(
    journal: &Path,
    segment: &Path,
    source_mtime_ms: Option<i64>,
    mut rescan: impl FnMut(
        &Path,
        &Path,
    ) -> Result<solstone_core_indexer_store::scan::RescanFileStatus, String>,
) -> Result<(), String> {
    let before = snapshot(journal, segment, source_mtime_ms)?;
    for relative in before.files.keys() {
        let attempt =
            attempt_saved_publication(journal, &journal.join(relative), |journal, path| {
                rescan(journal, path)
            });
        if !matches!(
            attempt.outcome,
            SavedPublicationOutcome::Indexed | SavedPublicationOutcome::Excluded
        ) {
            return Err(format!(
                "saved publication {}: {}",
                attempt.path,
                attempt.cause.as_deref().unwrap_or(attempt.outcome.as_str())
            ));
        }
    }
    if before != snapshot(journal, segment, source_mtime_ms)? {
        return Err("saved outputs changed during publication".into());
    }
    solstone_core_journal_io::write_json(
        receipt_path(segment),
        &before,
        solstone_core_journal_io::JsonWriteOptions::default(),
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use solstone_core_indexer_store::scan::RescanFileStatus;

    #[test]
    fn failed_publication_retries_saved_text_and_receipt_is_bound_to_input_and_output() {
        let journal = tempfile::tempdir().unwrap();
        let segment = journal.path().join("chronicle/20260101/desktop/120000_300");
        std::fs::create_dir_all(segment.join("talents")).unwrap();
        let output = segment.join("talents/activity.md");
        std::fs::write(&output, "a saved summary").unwrap();
        assert!(
            publish(journal.path(), &segment, Some(1), |_, _| Err(
                "index unavailable".into()
            ))
            .is_err()
        );
        assert!(!is_current(journal.path(), &segment, Some(1)).unwrap());
        let mut calls = 0;
        publish(journal.path(), &segment, Some(1), |_, path| {
            calls += 1;
            assert_eq!(std::fs::read_to_string(path).unwrap(), "a saved summary");
            Ok(RescanFileStatus::Indexed {
                warnings: Vec::new(),
            })
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert!(is_current(journal.path(), &segment, Some(1)).unwrap());
        assert!(!is_current(journal.path(), &segment, Some(2)).unwrap());
        std::fs::write(&output, "the owner's edit").unwrap();
        assert!(!is_current(journal.path(), &segment, Some(1)).unwrap());
        publish(journal.path(), &segment, Some(1), |_, _| {
            Ok(RescanFileStatus::Indexed {
                warnings: Vec::new(),
            })
        })
        .unwrap();
        assert_eq!(std::fs::read_to_string(output).unwrap(), "the owner's edit");
    }
}
