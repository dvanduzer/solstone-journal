// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Generated-byte receipts for segment output writers. Indexing never writes these.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solstone_core_journal_io::{JsonWriteOptions, LockOptions, hold_lock, write_json};
use std::{collections::BTreeMap, path::Path};

const RECEIPT: &str = ".generated-outputs.json";
pub const PROTECTED: &str = "owner_output_protected";

#[derive(Serialize, Deserialize)]
struct Generated {
    version: u32,
    files: BTreeMap<String, String>,
}

fn read(directory: &Path) -> Result<Generated, String> {
    match std::fs::read(directory.join(RECEIPT)) {
        Ok(bytes) => {
            let saved: Generated =
                serde_json::from_slice(&bytes).map_err(|e| format!("{PROTECTED}: {e}"))?;
            if saved.version != 1
                || saved.files.iter().any(|(name, digest)| {
                    name.is_empty()
                        || name.starts_with('.')
                        || name.contains(['/', '\\', ':'])
                        || digest.len() != 64
                        || !digest.bytes().all(|b| b.is_ascii_hexdigit())
                })
            {
                return Err(format!("{PROTECTED}: unsupported generated-output receipt"));
            }
            Ok(saved)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Generated {
            version: 1,
            files: BTreeMap::new(),
        }),
        Err(e) => Err(e.to_string()),
    }
}

fn matches_generated(path: &Path, saved: Option<&String>) -> Result<bool, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(saved.is_some_and(|hash| hash == &format!("{:x}", Sha256::digest(bytes)))),
        // A missing previously generated output is an owner deletion.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(saved.is_none()),
        Err(e) => Err(e.to_string()),
    }
}

/// Retained source can only be replayed as generated input if these exact bytes
/// were recorded by its writer. This never creates or repairs a receipt.
pub fn read_generated(path: &Path) -> Result<Vec<u8>, String> {
    let saved = read(path.parent().ok_or("segment output has no parent")?)?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("invalid output name")?;
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if saved.files.get(name) != Some(&format!("{:x}", Sha256::digest(&bytes))) {
        return Err(format!("{PROTECTED}: {}", path.display()));
    }
    Ok(bytes)
}

/// Inspect the splitter's targets and every recorded generated target, including
/// deletions. Files owned by other domains (such as speaker corrections) do not
/// prevent segment repair; each configured talent also checks its own write.
pub fn has_protected_outputs(directory: &Path) -> Result<bool, String> {
    let saved = read(directory)?;
    let mut targets = saved
        .files
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(!targets.is_empty()),
        Err(e) => return Err(e.to_string()),
    };
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if matches!(
            name.as_str(),
            "sense.json"
                | "facets.json"
                | "speakers.json"
                | "density.json"
                | "change.json"
                | "activity.md"
                | "sense.md"
                | "topics.md"
        ) {
            targets.insert(name);
        }
    }
    for name in targets {
        if !matches_generated(&directory.join(&name), saved.files.get(&name))? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The caller owns the output write; this boundary owns its generated-byte receipt.
/// Check after model work and before publication, serializing cooperating writers.
/// A crash between body and receipt cannot adopt current bytes as generated evidence.
pub fn commit<T>(
    path: &Path,
    generated: &[u8],
    protect: bool,
    publish: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let directory = path.parent().ok_or("segment output has no parent")?;
    std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let _lock =
        hold_lock(directory.join(RECEIPT), LockOptions::default()).map_err(|e| e.to_string())?;
    let mut saved = match read(directory) {
        Ok(saved) => saved,
        Err(error) if protect => return Err(error),
        Err(_) => Generated {
            version: 1,
            files: BTreeMap::new(),
        },
    };
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("segment output name is not UTF-8")?;
    if protect && !matches_generated(path, saved.files.get(name))? {
        return Err(format!("{PROTECTED}: {}", path.display()));
    }
    let result = publish()?;
    saved
        .files
        .insert(name.into(), format!("{:x}", Sha256::digest(generated)));
    write_json(directory.join(RECEIPT), &saved, JsonWriteOptions::default())
        .map_err(|e| e.to_string())?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn publish(path: &Path, text: &str, protect: bool) -> Result<(), String> {
        commit(path, text.as_bytes(), protect, || {
            std::fs::write(path, text).map_err(|e| e.to_string())
        })
    }
    #[test]
    fn only_writer_bytes_authorize_refresh_and_edits_deletions_or_corrupt_receipts_hold() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("activity.md");
        publish(&path, "first", true).unwrap();
        publish(&path, "second", true).unwrap();
        assert!(!has_protected_outputs(root.path()).unwrap());
        std::fs::write(&path, "owner edit during generation").unwrap();
        assert!(has_protected_outputs(root.path()).unwrap());
        assert!(publish(&path, "late model result", true).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "owner edit during generation"
        );
        publish(&path, "explicit override", false).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(has_protected_outputs(root.path()).unwrap());
        assert!(publish(&path, "restore", true).is_err());
        publish(&path, "explicit restore", false).unwrap();
        std::fs::write(root.path().join(RECEIPT), "broken").unwrap();
        assert!(publish(&path, "replace", true).is_err());
        std::fs::remove_file(root.path().join(RECEIPT)).unwrap();
        assert!(publish(&path, "replace", true).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "explicit restore");
    }

    #[test]
    fn failed_receipt_does_not_adopt_an_output_and_failed_writer_does_not_advance_receipt() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("sense.json");
        publish(&path, "initial", true).unwrap();
        assert!(
            commit(&path, b"not written", true, || Err::<(), _>(
                "write failed".into()
            ))
            .is_err()
        );
        assert_eq!(read_generated(&path).unwrap(), b"initial");
        assert!(
            commit(&path, b"written before interrupted receipt", true, || {
                std::fs::write(&path, b"written before interrupted receipt").unwrap();
                std::fs::remove_file(root.path().join(RECEIPT)).unwrap();
                std::fs::create_dir(root.path().join(RECEIPT)).unwrap();
                Ok(())
            })
            .is_err()
        );
        assert!(read_generated(&path).is_err());
        assert!(publish(&path, "automatic retry", true).is_err());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"written before interrupted receipt"
        );
    }

    #[test]
    fn unrelated_domain_outputs_do_not_block_the_segment_writer() {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "speaker_labels.json",
            "speaker_corrections.json",
            "detection_outcome.json",
            "owner-note.md",
        ] {
            std::fs::write(root.path().join(name), "owned elsewhere").unwrap();
        }
        assert!(!has_protected_outputs(root.path()).unwrap());
        let custom = root.path().join("custom.md");
        publish(&custom, "generated", true).unwrap();
        std::fs::write(custom, "owner edit").unwrap();
        assert!(has_protected_outputs(root.path()).unwrap());
    }

    #[cfg(feature = "full-tests")]
    #[test]
    fn concurrent_segment_writers_keep_both_generated_receipts() {
        let root = tempfile::tempdir().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles = ["sense.json", "documents.md"].map(|name| {
            let path = root.path().join(name);
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                publish(&path, name, true).unwrap();
            })
        });
        for handle in handles {
            handle.join().unwrap();
        }
        for name in ["sense.json", "documents.md"] {
            assert_eq!(
                read_generated(&root.path().join(name)).unwrap(),
                name.as_bytes()
            );
        }
    }
}
