// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};

use super::*;

fn path(journal: &Path, day: &str) -> PathBuf {
    journal.join("health/offload").join(format!("{day}.jsonl"))
}

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
fn archive_v2(id: &str, name: &str) -> Value {
    json!({"event_kind":"offload_archive_v2", "schema_version":2, "time":2, "day":"20260101", "stream":"_default", "segment":"010000_001",
        "segment_dir":"010000_001_full", "layout":"direct", "journal_id":"native-root-v1:unix:0000000000000001:0000000000000002", "repository_id":"c".repeat(64),
        "snapshot_id":"b".repeat(64), "manifest_id":id,
        "files":[{"witness_id":format!("w-{id}"), "name":name, "bytes":7, "sha256":"a".repeat(64), "archive_path":format!("/stage/{name}"),
            "processing":{"record":null,"has_analysis_row":true}}]})
}
fn legacy_bytes(name: &str) -> Vec<u8> {
    format!("{}\n", archive("snapshot", &[name])).into_bytes()
}

fn retained_fixture() -> (tempfile::TempDir, Arc<JournalRoot>, PathBuf, Vec<u8>) {
    let journal = tempfile::tempdir().unwrap();
    let root = Arc::new(JournalRoot::open(journal.path()).unwrap());
    let ledger = path(journal.path(), "20260101");
    let bytes = legacy_bytes("original.flac");
    fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    fs::write(&ledger, &bytes).unwrap();
    (journal, root, ledger, bytes)
}

#[test]
fn retained_catalog_matches_fold_and_ignores_non_ledgers() {
    let (_journal, root, ledger, mut bytes) = retained_fixture();
    bytes.extend_from_slice(format!("{}\n", archive_v2("bound", "bound.flac")).as_bytes());
    fs::write(&ledger, &bytes).unwrap();
    fs::write(ledger.parent().unwrap().join("README"), b"not a ledger").unwrap();
    fs::create_dir(ledger.parent().unwrap().join("other")).unwrap();
    let expected = JournalCatalog {
        days: vec![fold_lines("20260101", &bytes)],
        ..Default::default()
    };
    assert!(!expected.degraded());
    assert_eq!(expected.days[0].segments[0].active.len(), 2);
    assert_eq!(read_complete_catalog(&root).unwrap(), expected);
    assert_eq!(fs::read(&ledger).unwrap(), bytes);
}

#[cfg(unix)]
#[test]
fn retained_catalog_refuses_permanent_and_temporary_ancestor_substitution() {
    for relative in ["health", "health/offload"] {
        for temporary in [false, true] {
            let (journal, root, ledger, original) = retained_fixture();
            let component = journal.path().join(relative);
            let saved = journal.path().join("saved");
            let replacement = journal.path().join("replacement");
            let replacement_bytes = legacy_bytes("replaced.flac");
            let mut substituted = false;
            let mut restored = false;
            let result = read_complete_catalog_with_checkpoint(&root, |point| {
                if point == CatalogReadCheckpoint::AfterListing {
                    fs::rename(&component, &saved).unwrap();
                    fs::create_dir_all(ledger.parent().unwrap()).unwrap();
                    fs::write(&ledger, &replacement_bytes).unwrap();
                    substituted = true;
                } else if point == CatalogReadCheckpoint::AfterRead && temporary {
                    fs::rename(&component, &replacement).unwrap();
                    fs::rename(&saved, &component).unwrap();
                    restored = true;
                }
            });
            assert!(substituted);
            assert!(result.is_err(), "{relative}, temporary={temporary}");
            if !restored {
                fs::rename(&component, &replacement).unwrap();
                fs::rename(&saved, &component).unwrap();
            }
            assert_eq!(fs::read(&ledger).unwrap(), original);
            let replacement_ledger = if relative == "health" {
                replacement.join("offload/20260101.jsonl")
            } else {
                replacement.join("20260101.jsonl")
            };
            assert_eq!(fs::read(replacement_ledger).unwrap(), replacement_bytes);
        }
    }
}

#[test]
fn retained_catalog_refuses_listed_leaf_removal_replacement_growth_and_shrink() {
    for change in ["remove", "replace", "grow", "shrink"] {
        let (journal, root, ledger, original) = retained_fixture();
        let saved = journal.path().join("saved.jsonl");
        let modified = fs::metadata(&ledger).unwrap().modified().unwrap();
        let mut changed_bytes = None;
        let mut changed = false;
        let result = read_complete_catalog_with_checkpoint(&root, |point| {
            if point != CatalogReadCheckpoint::AfterListing {
                return;
            }
            changed = true;
            match change {
                "remove" => fs::rename(&ledger, &saved).unwrap(),
                "replace" => {
                    fs::rename(&ledger, &saved).unwrap();
                    let replacement = legacy_bytes("replaced.flac");
                    assert_eq!(replacement.len(), original.len());
                    fs::write(&ledger, &replacement).unwrap();
                    fs::OpenOptions::new()
                        .write(true)
                        .open(&ledger)
                        .unwrap()
                        .set_times(fs::FileTimes::new().set_modified(modified))
                        .unwrap();
                    assert_eq!(fs::metadata(&ledger).unwrap().modified().unwrap(), modified);
                    changed_bytes = Some(replacement);
                }
                "grow" => {
                    let mut grown = original.clone();
                    grown.push(b' ');
                    fs::write(&ledger, &grown).unwrap();
                    changed_bytes = Some(grown);
                }
                "shrink" => {
                    let shrunk = original[..original.len() - 1].to_vec();
                    fs::write(&ledger, &shrunk).unwrap();
                    changed_bytes = Some(shrunk);
                }
                _ => unreachable!(),
            }
        });
        assert!(changed);
        assert!(result.is_err(), "{change}");
        if saved.exists() {
            assert_eq!(fs::read(&saved).unwrap(), original);
        }
        if let Some(bytes) = changed_bytes {
            assert_eq!(fs::read(&ledger).unwrap(), bytes);
        } else {
            assert!(!ledger.exists());
        }
    }
}

#[test]
fn retained_catalog_refuses_population_change_after_read() {
    for add_day in [false, true] {
        let (journal, root, ledger, original) = retained_fixture();
        let mut injected = false;
        let mut changed_file = ledger.clone();
        let mut changed_bytes = Vec::new();
        let result = read_complete_catalog_with_checkpoint(&root, |point| {
            if point != CatalogReadCheckpoint::AfterRead || injected {
                return;
            }
            injected = true;
            if add_day {
                let mut event = archive("snapshot", &["added.flac"]);
                event["day"] = "20260102".into();
                changed_file = path(journal.path(), "20260102");
                fs::write(&changed_file, format!("{event}\n")).unwrap();
            } else {
                let event = json!({"event_kind":"restore", "time":2, "day":"20260101", "stream":"_default", "segment":"010000_001"});
                solstone_core_journal_io::append_jsonl(&ledger, &event).unwrap();
            }
            changed_bytes = fs::read(&changed_file).unwrap();
        });
        assert!(injected);
        assert!(result.is_err(), "add_day={add_day}");
        assert_eq!(fs::read(changed_file).unwrap(), changed_bytes);
        if add_day {
            assert_eq!(fs::read(&ledger).unwrap(), original);
        }
    }
}

#[test]
fn retained_catalog_rechecks_missing_health_and_offload() {
    for missing_health in [false, true] {
        let journal = tempfile::tempdir().unwrap();
        let root = Arc::new(JournalRoot::open(journal.path()).unwrap());
        if !missing_health {
            fs::create_dir(journal.path().join("health")).unwrap();
        }
        let ledger = path(journal.path(), "20260101");
        let bytes = legacy_bytes("original.flac");
        let mut injected = false;
        let result = read_complete_catalog_with_checkpoint(&root, |point| {
            assert_eq!(point, CatalogReadCheckpoint::BeforeAbsentRecheck);
            fs::create_dir_all(ledger.parent().unwrap()).unwrap();
            fs::write(&ledger, &bytes).unwrap();
            injected = true;
        });
        assert!(injected);
        assert!(result.is_err());
        assert_eq!(fs::read(&ledger).unwrap(), bytes);
        assert_eq!(read_complete_catalog(&root).unwrap().days.len(), 1);
    }
}

#[cfg(unix)]
#[test]
fn retained_catalog_refuses_symlink_ancestors_and_selected_leaf() {
    for relative in ["health", "health/offload", "health/offload/20260101.jsonl"] {
        let (journal, root, _ledger, original) = retained_fixture();
        let component = journal.path().join(relative);
        let saved = journal.path().join("saved");
        fs::rename(&component, &saved).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_ledger = outside.path().join("offload/20260101.jsonl");
        fs::create_dir_all(outside_ledger.parent().unwrap()).unwrap();
        let replacement = legacy_bytes("replaced.flac");
        fs::write(&outside_ledger, &replacement).unwrap();
        let target = match relative {
            "health" => outside.path().to_path_buf(),
            "health/offload" => outside_ledger.parent().unwrap().to_path_buf(),
            _ => outside_ledger.clone(),
        };
        std::os::unix::fs::symlink(target, &component).unwrap();
        assert!(read_complete_catalog(&root).is_err(), "{relative}");
        let saved_ledger = match relative {
            "health" => saved.join("offload/20260101.jsonl"),
            "health/offload" => saved.join("20260101.jsonl"),
            _ => saved,
        };
        assert_eq!(fs::read(saved_ledger).unwrap(), original);
        assert_eq!(fs::read(outside_ledger).unwrap(), replacement);
    }
}

#[test]
fn complete_catalog_is_fresh_and_refuses_later_damage() {
    let journal = tempfile::tempdir().unwrap();
    let root = Arc::new(JournalRoot::open(journal.path()).unwrap());
    assert_eq!(
        read_complete_catalog(&root).unwrap(),
        JournalCatalog::default()
    );
    assert!(!journal.path().join("health").exists());
    let ledger = path(journal.path(), "20260101");
    fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    fs::write(&ledger, legacy_bytes("audio.flac")).unwrap();
    assert_eq!(
        read_complete_catalog(&root).unwrap().days[0].segments[0]
            .active
            .len(),
        1
    );
    let ledger = path(journal.path(), "20260101");
    for bytes in [b"{broken}\n".as_slice(), b"{torn".as_slice()] {
        fs::write(&ledger, bytes).unwrap();
        assert!(read_complete_catalog(&root).is_err());
        assert_eq!(fs::read(&ledger).unwrap(), bytes);
    }
    fs::remove_file(&ledger).unwrap();
    fs::create_dir(&ledger).unwrap();
    assert!(read_complete_catalog(&root).is_err());
}

#[cfg(unix)]
#[test]
fn complete_catalog_refuses_replaced_canonical_root_despite_retained_identity() {
    let parent = tempfile::tempdir().unwrap();
    let journal = parent.path().join("journal");
    fs::create_dir(&journal).unwrap();
    let root = Arc::new(JournalRoot::open(&journal).unwrap());
    fs::rename(&journal, parent.path().join("original")).unwrap();
    fs::create_dir(&journal).unwrap();
    root.revalidate().unwrap();
    assert!(root.revalidate_canonical_binding().is_err());
    assert!(read_complete_catalog(&root).is_err());
}
