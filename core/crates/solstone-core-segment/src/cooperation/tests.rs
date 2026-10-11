// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "native cooperation fixtures"
)]

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;

use super::*;
use crate::test_support::TempDir;
use crate::{Relocation, RelocationEnd, RelocationRefusal, RepairOutcome};
use solstone_core_journal_io::media::{MediaDirectory, MediaStage};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Phase {
    AfterPivot,
    AfterTranscript,
}
type Checkpoint = Box<dyn FnMut(Phase)>;
thread_local! { static CHECKPOINT: RefCell<Option<Checkpoint>> = const { RefCell::new(None) }; }
pub(crate) fn checkpoint(phase: Phase) {
    CHECKPOINT.with(|slot| {
        if let Some(callback) = slot.borrow_mut().as_mut() {
            callback(phase);
        }
    });
}
fn with_checkpoint<T>(callback: impl FnMut(Phase) + 'static, action: impl FnOnce() -> T) -> T {
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            CHECKPOINT.with(|slot| *slot.borrow_mut() = None);
        }
    }
    CHECKPOINT.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(Box::new(callback));
    });
    let _clear = Clear;
    action()
}
fn activity(root: &Path) -> Arc<ActivityLease> {
    ActivityLease::try_acquire(Arc::new(JournalRoot::open(root).unwrap())).unwrap()
}
fn stage(activity: &Arc<ActivityLease>, relative: &str) -> MediaStage {
    let directory =
        MediaDirectory::open(Arc::clone(activity.journal_root()), Path::new(relative)).unwrap();
    MediaStage::create(directory, Arc::clone(activity)).unwrap()
}
fn end(root: &Path, day: &str, stream: Option<&str>, basename: &str) -> RelocationEnd {
    let parent_rel = stream.map_or_else(
        || format!("chronicle/{day}"),
        |stream| format!("chronicle/{day}/{stream}"),
    );
    let disk_rel = format!("{parent_rel}/{basename}");
    RelocationEnd {
        day: day.into(),
        segment: "070000_17".into(),
        path: root.join(&disk_rel),
        disk_rel,
        parent_rel,
    }
}
fn request<'a>(
    root: &'a Path,
    source: &'a RelocationEnd,
    destination: &'a RelocationEnd,
) -> Relocation<'a> {
    Relocation {
        journal: root,
        source,
        destination,
        successor: None,
        stream: "audio",
        sequence: 1,
        locks: crate::LockOptions::default(),
    }
}
fn seed(end: &RelocationEnd) {
    fs::create_dir_all(&end.path).unwrap();
    fs::write(end.path.join("audio.flac"), b"raw").unwrap();
    fs::write(
        end.path.join("stream.json"),
        "{\"stream\":\"audio\",\"seq\":1}",
    )
    .unwrap();
    fs::write(
        end.path.join("events.jsonl"),
        "{\"day\":\"old\",\"segment\":\"old\"}\n",
    )
    .unwrap();
}

#[test]
fn relocation_preserves_exact_direct_and_named_dot_locations() {
    for stream in [None, Some(".Audio")] {
        let root = TempDir::new();
        let source = end(root.path(), "20261010", stream, "prefix.070000_17.suffix");
        let destination = end(root.path(), "20261011", stream, "other.070000_17.suffix");
        seed(&source);
        let outcome =
            crate::relocate_segment(&request(root.path(), &source, &destination)).unwrap();
        assert!(!source.path.exists());
        assert_eq!(
            fs::read(destination.path.join("audio.flac")).unwrap(),
            b"raw"
        );
        assert_eq!(outcome.events, Ok(1));
        assert!(
            fs::read_to_string(destination.path.join("events.jsonl"))
                .unwrap()
                .contains("\"segment\":\"070000_17\"")
        );
        assert_eq!(outcome.mutated_days.len(), 2);
    }
}

#[test]
fn relocation_refuses_wrapped_reservations_with_empty_census() {
    for reserved in [
        ".removing_.tmp-offload-owned",
        ".removing_.removing_.tmp-offload-owned",
    ] {
        for named_parent in [false, true] {
            for reserved_source in [false, true] {
                let root = TempDir::new();
                let ordinary_source = end(root.path(), "20261010", None, "070000_17");
                let ordinary_destination = end(root.path(), "20261011", None, "070000_17");
                let reserved_end = end(
                    root.path(),
                    if reserved_source {
                        "20261010"
                    } else {
                        "20261011"
                    },
                    named_parent.then_some(reserved),
                    if named_parent { "070000_17" } else { reserved },
                );
                let (source, destination) = if reserved_source {
                    (&reserved_end, &ordinary_destination)
                } else {
                    (&ordinary_source, &reserved_end)
                };
                seed(source);
                let original = activity(root.path());
                assert!(
                    solstone_core_journal_io::activity_obligations::observe_obligations(
                        original.journal_root()
                    )
                    .unwrap()
                    .entries()
                    .is_empty()
                );
                drop(original);
                assert!(matches!(
                    crate::relocate_segment(&request(root.path(), source, destination)),
                    Err(RelocationRefusal::Failed(_))
                ));
                assert_eq!(fs::read(source.path.join("audio.flac")).unwrap(), b"raw");
                assert!(!root.path().join("chronicle/20261011").exists());
            }
        }
    }

    let root = TempDir::new();
    let source = end(
        root.path(),
        "20261010",
        Some(".removing_.Audio"),
        ".removing_.ordinary",
    );
    let destination = end(
        root.path(),
        "20261011",
        Some(".removing_.Audio"),
        ".removing_.ordinary",
    );
    seed(&source);
    let outcome = crate::relocate_segment(&request(root.path(), &source, &destination)).unwrap();
    assert!(!source.path.exists());
    assert_eq!(
        fs::read(destination.path.join("audio.flac")).unwrap(),
        b"raw"
    );
    assert_eq!(outcome.events, Ok(1));
}

#[test]
fn relocation_holds_busy_named_stage_reserved_and_disagreeing_paths() {
    let root = TempDir::new();
    let source = end(root.path(), "20261010", Some("audio"), "070000_17");
    let destination = end(root.path(), "20261011", Some("audio"), "070000_17");
    seed(&source);
    let original = activity(root.path());
    assert!(crate::relocate_segment(&request(root.path(), &source, &destination)).is_err());
    let owned = stage(&original, &source.parent_rel);
    drop(original);
    drop(owned); // Drop leaves the physical obligation; it does not clean it.
    assert!(crate::relocate_segment(&request(root.path(), &source, &destination)).is_err());
    assert!(source.path.join("audio.flac").exists());
    assert!(!root.path().join(&destination.parent_rel).exists());

    let clean = TempDir::new();
    let source = end(clean.path(), "20261010", None, "070000_17");
    seed(&source);
    let mut destination = end(clean.path(), "20261011", None, "070000_17");
    destination.path = clean.path().join("chronicle/20261011/another");
    assert!(matches!(
        crate::relocate_segment(&request(clean.path(), &source, &destination)),
        Err(RelocationRefusal::Failed(_))
    ));
    destination = end(
        clean.path(),
        "20261011",
        Some(".tmp-offload-reserved"),
        "070000_17",
    );
    assert!(crate::relocate_segment(&request(clean.path(), &source, &destination)).is_err());
    assert!(!clean.path().join("chronicle/20261011").exists());
}

#[test]
fn postpivot_refusal_preserves_move_and_failed_later_phases() {
    let root = TempDir::new();
    let source = end(root.path(), "20261010", None, "070000_17");
    let destination = end(root.path(), "20261011", None, "070000_17");
    seed(&source);
    let obligation = root.path().join(".tmp-offload-late");
    let outcome = with_checkpoint(
        move |phase| {
            if phase == Phase::AfterPivot {
                fs::create_dir(&obligation).unwrap();
            }
        },
        || crate::relocate_segment(&request(root.path(), &source, &destination)).unwrap(),
    );
    assert!(!source.path.exists());
    assert!(destination.path.join("audio.flac").exists());
    assert!(outcome.events.is_err());
    assert!(matches!(outcome.tail, RepairOutcome::WriteFailed));
    assert_eq!(outcome.mutated_days.len(), 2);
}

fn pdf(root: &Path, segment: &str) -> PathBuf {
    let directory = root.join(format!("chronicle/20261010/.documents/{segment}"));
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("document.jsonl"),
        "{\"kind\":\"document\",\"raw\":\"document.pdf\"}\n{\"text\":\"readable text\"}\n",
    )
    .unwrap();
    directory
}

#[test]
fn public_migrations_hold_existing_activity_and_stage() {
    let root = TempDir::new();
    let source = end(root.path(), "20261010", None, "070000_17");
    seed(&source);
    pdf(root.path(), "070000_17");
    let original = activity(root.path());
    assert!(crate::restructure_segments_by_stream(root.path(), false).is_err());
    assert!(crate::migrate_agent_layout(root.path(), false).is_err());
    assert!(crate::migrate_pdf_extractions(root.path()).is_err());
    let owned = stage(&original, "chronicle/20261010/.documents");
    drop(original);
    drop(owned);
    assert!(crate::restructure_segments_by_stream(root.path(), false).is_err());
    assert!(crate::migrate_agent_layout(root.path(), false).is_err());
    assert!(crate::migrate_pdf_extractions(root.path()).is_err());
    assert!(source.path.join("audio.flac").exists());
}

#[test]
fn pdf_hidden_names_survive_but_reserved_component_refuses() {
    let root = TempDir::new();
    let source = pdf(root.path(), ".old");
    assert_eq!(
        crate::migrate_pdf_extractions(root.path())
            .unwrap()
            .converted_documents,
        1
    );
    assert!(source.join("document_transcript.md").exists());
    let reserved = pdf(root.path(), ".tmp-offload-source");
    assert!(crate::migrate_pdf_extractions(root.path()).is_err());
    assert!(reserved.join("document.jsonl").exists());
    assert!(!reserved.join("document_transcript.md").exists());
}

#[test]
fn late_pdf_hold_keeps_current_transcript_source_and_previous_count() {
    let root = TempDir::new();
    let first = pdf(root.path(), "a");
    let second = pdf(root.path(), "b");
    let third = pdf(root.path(), "c");
    let obligation = root.path().join(".tmp-offload-late");
    let mut written = 0;
    let error = with_checkpoint(
        move |phase| {
            if phase == Phase::AfterTranscript {
                written += 1;
                if written == 2 {
                    fs::create_dir(&obligation).unwrap();
                }
            }
        },
        || crate::migrate_pdf_extractions(root.path()).unwrap_err(),
    );
    assert!(error.to_string().contains("1 documents converted"));
    assert!(error.to_string().contains("current transcript was written"));
    assert!(!first.join("document.jsonl").exists());
    assert!(second.join("document_transcript.md").exists());
    assert!(second.join("document.jsonl").exists());
    assert!(third.join("document.jsonl").exists());
    assert!(!third.join("document_transcript.md").exists());
}
