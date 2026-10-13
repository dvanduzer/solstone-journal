// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Non-healing, read-only access to validated private agent-memory originals.

use std::error::Error;
use std::ffi::OsStr;
use std::io;
#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(windows)]
use std::os::windows::io::AsHandle;
use std::path::Path;
#[cfg(all(test, feature = "full-tests"))]
use std::path::PathBuf;
use std::time::{Duration, Instant};

use solstone_core_format::agent_memory::{
    ChainPredecessor, Coordinate, MAX_METADATA_BYTES, MAX_NOTE_BYTES, Origin, ReadyDocument,
    SourceKey, validate_complete_original, validate_coordinate,
};
use solstone_core_journal_io::errors::FlatDirectoryError;
use solstone_core_journal_io::journal_root::JournalRoot;
use solstone_core_journal_io::{
    ExactLookupError, FlatDirectoryError as DirectoryError, resolve_segment_exact,
    resolve_stream_exact,
};
use solstone_core_segment::{
    OwnerDeletionState, SegmentDir, owner_deletion_state, parse_agent_memory_chain,
};

/// Result of reading one original without healing or changing journal state.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum OriginalRead {
    Absent,
    Unready,
    Deleted,
    Staged,
    Corrupt,
    Unavailable {
        detail: String,
    },
    Ready {
        bytes: Vec<u8>,
        origin: Origin,
        ready: ReadyDocument,
    },
}

/// Read one source-bound original. A missing path is an observation only and
/// never creates a source namespace, chronicle directory, lock, or marker.
pub fn read_original(
    journal: &Path,
    source_key: &SourceKey,
    coordinate: &Coordinate,
) -> OriginalRead {
    read_original_until(
        journal,
        source_key,
        coordinate,
        Instant::now() + Duration::from_secs(5),
    )
}

/// The same original read under a caller's existing absolute deadline.
pub fn read_original_until(
    journal: &Path,
    source_key: &SourceKey,
    coordinate: &Coordinate,
    deadline: Instant,
) -> OriginalRead {
    read_original_observed(journal, source_key, coordinate, deadline, |_| {})
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadPhase {
    BeforeLiveBinding,
    AfterLiveBinding,
    AfterMarkers,
    AfterFileCheck,
}

fn read_original_observed(
    journal: &Path,
    source_key: &SourceKey,
    coordinate: &Coordinate,
    deadline: Instant,
    mut observe: impl FnMut(ReadPhase),
) -> OriginalRead {
    if source_key.validate().is_err() || validate_coordinate(coordinate, source_key).is_err() {
        return OriginalRead::Corrupt;
    }
    if matches!(std::fs::symlink_metadata(journal), Err(error) if error.kind() == io::ErrorKind::NotFound)
    {
        return OriginalRead::Absent;
    }

    let root = match JournalRoot::open(journal) {
        Ok(root) => root,
        Err(solstone_core_journal_io::JournalRootError::Io { source, .. })
            if source.kind() == io::ErrorKind::NotFound =>
        {
            return OriginalRead::Absent;
        }
        Err(error) => return unavailable(error),
    };
    let namespace = match std::fs::canonicalize(root.canonical_path()) {
        Ok(path) => path,
        Err(error) => return unavailable(error),
    };
    if let Err(error) = root.revalidate_canonical_binding() {
        return unavailable(error);
    }

    match resolve_stream_exact(&namespace, &coordinate.day, &coordinate.stream) {
        Ok(Some(_)) => {}
        Ok(None) => return OriginalRead::Absent,
        Err(error) => return lookup_unavailable_or_absent(error),
    }

    let segment_path = match resolve_segment_exact(
        &namespace,
        &coordinate.day,
        &coordinate.stream,
        &coordinate.segment,
    ) {
        Ok(Some(path)) => path,
        Ok(None) => {
            let expected = match SegmentDir::resolve(
                journal,
                &coordinate.day,
                &coordinate.segment,
                &coordinate.stream,
            ) {
                Ok(segment) => segment,
                Err(error) => return unavailable(error),
            };
            return match owner_deletion_state(expected.path()) {
                Ok(OwnerDeletionState::Deleted) => OriginalRead::Deleted,
                Ok(OwnerDeletionState::Staged) => OriginalRead::Staged,
                Ok(OwnerDeletionState::Live) => OriginalRead::Absent,
                Ok(OwnerDeletionState::Occupied) => OriginalRead::Unavailable {
                    detail: "deletion marker refused".into(),
                },
                Err(error) => unavailable(error),
            };
        }
        Err(error) => return lookup_unavailable_or_absent(error),
    };

    let segment = match SegmentDir::resolve(
        journal,
        &coordinate.day,
        &coordinate.segment,
        &coordinate.stream,
    ) {
        Ok(segment) if segment.path() == segment_path => segment,
        Ok(_) => {
            return OriginalRead::Unavailable {
                detail: "segment path changed".into(),
            };
        }
        Err(error) => return unavailable(error),
    };
    match owner_deletion_state(segment.path()) {
        Ok(OwnerDeletionState::Deleted) => return OriginalRead::Deleted,
        Ok(OwnerDeletionState::Staged) => return OriginalRead::Staged,
        Ok(OwnerDeletionState::Live) => {}
        Ok(OwnerDeletionState::Occupied) => {
            return OriginalRead::Unavailable {
                detail: "deletion marker refused".into(),
            };
        }
        Err(error) => return unavailable(error),
    }
    let directory = match open_segment(&root, coordinate) {
        Ok(Some(directory)) => directory,
        Ok(None) => return changed("segment disappeared during open"),
        Err(error) => return unavailable(error),
    };
    let files = [
        ("ready.json", MAX_METADATA_BYTES),
        ("note.txt", MAX_NOTE_BYTES),
        ("origin.json", MAX_METADATA_BYTES),
        ("stream.json", MAX_METADATA_BYTES),
    ];
    let mut originals = Vec::with_capacity(files.len());
    for (name, limit) in files {
        match read_file(&directory, OsStr::new(name), limit) {
            Ok(Some(observed)) => originals.push(observed),
            Ok(None) if name == "ready.json" => return OriginalRead::Unready,
            Ok(None) | Err(FlatDirectoryError::SizeLimitExceeded { .. }) => {
                return OriginalRead::Corrupt;
            }
            Err(error) => return unavailable(error),
        }
    }
    let ready: ReadyDocument = match serde_json::from_slice(&originals[0].bytes) {
        Ok(value) => value,
        Err(_) => return OriginalRead::Corrupt,
    };
    let bytes = &originals[1].bytes;
    let origin: Origin = match serde_json::from_slice(&originals[2].bytes) {
        Ok(value) => value,
        Err(_) => return OriginalRead::Corrupt,
    };
    let marker = match std::str::from_utf8(&originals[3].bytes) {
        Ok(marker) => marker,
        Err(_) => return OriginalRead::Corrupt,
    };
    let chain = match parse_agent_memory_chain(marker, &coordinate.stream) {
        Ok(chain) => ChainPredecessor {
            prev_day: chain.prev_day,
            prev_segment: chain.prev_segment,
            seq: chain.seq,
        },
        Err(_) => return OriginalRead::Corrupt,
    };
    if bytes.is_empty() || std::str::from_utf8(bytes).is_err() {
        return OriginalRead::Corrupt;
    }
    if validate_complete_original(&ready, source_key, coordinate, &origin, &chain, bytes).is_err() {
        return OriginalRead::Corrupt;
    }
    observe(ReadPhase::BeforeLiveBinding);
    // The fresh live-name acquisition is the read's linearization point,
    // conditional on the subsequent checks. Keep D alive to prevent inode reuse.
    match resolve_segment_exact(
        &namespace,
        &coordinate.day,
        &coordinate.stream,
        &coordinate.segment,
    ) {
        Ok(Some(path)) if path == segment_path => {}
        Ok(_) => return changed("live segment disappeared"),
        Err(error) => return unavailable(error),
    }
    let live = match open_segment(&root, coordinate) {
        Ok(Some(live)) if directory_identity(&live) == directory_identity(&directory) => live,
        Ok(_) => return changed("live segment identity changed"),
        Err(error) => return unavailable(error),
    };
    observe(ReadPhase::AfterLiveBinding);
    match owner_deletion_state(segment.path()) {
        Ok(OwnerDeletionState::Live) => {}
        Ok(OwnerDeletionState::Deleted) => return OriginalRead::Deleted,
        Ok(OwnerDeletionState::Staged) => return OriginalRead::Staged,
        Ok(OwnerDeletionState::Occupied) => return changed("deletion marker refused"),
        Err(error) => return unavailable(error),
    }
    // Also observe the retained directory: the live path may have moved since T.
    match read_file(&directory, OsStr::new("tombstone.json"), MAX_METADATA_BYTES) {
        Ok(None) => {}
        Ok(Some(_)) => return OriginalRead::Deleted,
        Err(error) => return unavailable(error),
    }
    observe(ReadPhase::AfterMarkers);
    // Reopen names, not old leaf handles. A completed removal returns the same
    // directory with its original leaves gone, so directory identity is not enough.
    for ((name, limit), original) in files.into_iter().zip(&originals) {
        match read_file(&directory, OsStr::new(name), limit) {
            Ok(Some(current)) if &current == original => {}
            Ok(_) => return changed("original changed during read"),
            Err(error) => return unavailable(error),
        }
        observe(ReadPhase::AfterFileCheck);
    }
    drop(live);
    if Instant::now() >= deadline {
        return OriginalRead::Unavailable {
            detail: "original read deadline exhausted".into(),
        };
    }
    if let Err(error) = root.revalidate_canonical_binding() {
        return unavailable(error);
    }
    OriginalRead::Ready {
        bytes: bytes.clone(),
        origin,
        ready,
    }
}

#[cfg(unix)]
type ReadDirectory = solstone_core_journal_io::FlatDirectory;
#[cfg(windows)]
type ReadDirectory = solstone_core_journal_io::WindowsFlatDirectory;

#[cfg(unix)]
fn open_child(
    parent: &impl AsFd,
    name: &str,
    diagnostic: &Path,
) -> Result<Option<ReadDirectory>, DirectoryError> {
    solstone_core_journal_io::open_flat_directory_bound(parent, OsStr::new(name), diagnostic)
}
#[cfg(windows)]
fn open_child(
    parent: &impl AsHandle,
    name: &str,
    diagnostic: &Path,
) -> Result<Option<ReadDirectory>, DirectoryError> {
    solstone_core_journal_io::open_windows_flat_directory_bound(
        parent,
        OsStr::new(name),
        diagnostic,
    )
}

fn open_segment(
    root: &JournalRoot,
    coordinate: &Coordinate,
) -> Result<Option<ReadDirectory>, DirectoryError> {
    let mut diagnostic = root.canonical_path().join("chronicle");
    let Some(mut directory) = open_child(root, "chronicle", root.canonical_path())? else {
        return Ok(None);
    };
    for name in [&coordinate.day, &coordinate.stream, &coordinate.segment] {
        let Some(child) = open_child(&directory, name, &diagnostic)? else {
            return Ok(None);
        };
        directory = child;
        diagnostic.push(name);
    }
    Ok(Some(directory))
}

#[cfg(unix)]
use solstone_core_journal_io::read_observed_file_bounded as read_file;
#[cfg(windows)]
use solstone_core_journal_io::read_windows_observed_file_bounded as read_file;

fn directory_identity(
    directory: &ReadDirectory,
) -> solstone_core_journal_io::journal_root::ObjectIdentity {
    #[cfg(unix)]
    {
        directory.identity()
    }
    #[cfg(windows)]
    {
        directory.object_identity()
    }
}

fn changed(detail: &str) -> OriginalRead {
    OriginalRead::Unavailable {
        detail: detail.to_owned(),
    }
}

fn lookup_unavailable_or_absent(error: ExactLookupError) -> OriginalRead {
    match error {
        ExactLookupError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => {
            OriginalRead::Absent
        }
        other => OriginalRead::Unavailable {
            detail: other.to_string(),
        },
    }
}

fn unavailable(error: impl Error) -> OriginalRead {
    OriginalRead::Unavailable {
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_coordinate_is_corrupt() {
        let source = SourceKey::from_verified_id("test-source");
        let coordinate = Coordinate {
            day: "20260102".into(),
            stream: "agent-memory-wrong".into(),
            segment: "030405_1".into(),
        };
        assert_eq!(
            read_original(Path::new("/var/tmp/not-opened"), &source, &coordinate),
            OriginalRead::Corrupt
        );
    }

    #[cfg(feature = "full-tests")]
    fn scratch_root() -> PathBuf {
        if cfg!(windows) {
            std::env::temp_dir()
        } else {
            PathBuf::from("/var/tmp")
        }
    }

    #[cfg(feature = "full-tests")]
    fn temporary_journal(label: &str) -> PathBuf {
        let root = scratch_root().join(format!("memory-original-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create temporary journal");
        root
    }

    #[cfg(feature = "full-tests")]
    fn ready_fixture(root: &Path) -> (SourceKey, Coordinate, PathBuf) {
        use solstone_core_format::agent_memory::{OperationRecord, digest, ready_document};

        let source = SourceKey::from_verified_id("test-source");
        let coordinate = Coordinate {
            day: "20260102".into(),
            stream: format!("agent-memory-{}", source.component()),
            segment: "030405_1".into(),
        };
        let segment = root
            .join("chronicle")
            .join(&coordinate.day)
            .join(&coordinate.stream)
            .join(&coordinate.segment);
        std::fs::create_dir_all(&segment).expect("create segment");
        let bytes = b"trusted note";
        let record: OperationRecord = serde_json::from_value(serde_json::json!({
            "operation_id": "operation-1",
            "digest": digest(bytes),
            "byte_count": bytes.len(),
            "created_at": "2026-01-02T03:04:05Z",
            "origin_kind": "agent_memory",
            "creation_label": "connection label",
            "coordinate": coordinate,
            "phase": "chained",
            "chain": {"prev_day": null, "prev_segment": null, "seq": 1}
        }))
        .expect("valid operation record");
        let ready = ready_document(&record, &source).unwrap();
        std::fs::write(segment.join("note.txt"), bytes).unwrap();
        std::fs::write(
            segment.join("origin.json"),
            serde_json::to_vec(&ready.origin).unwrap(),
        )
        .unwrap();
        std::fs::write(
            segment.join("ready.json"),
            serde_json::to_vec(&ready).unwrap(),
        )
        .unwrap();
        std::fs::write(
            segment.join("stream.json"),
            serde_json::json!({
                "stream": &coordinate.stream,
                "prev_day": null,
                "prev_segment": null,
                "seq": 1
            })
            .to_string(),
        )
        .unwrap();
        (source, coordinate, segment)
    }

    #[cfg(feature = "full-tests")]
    #[test]
    fn intact_original_survives_missing_lock_without_recreating_it() {
        let root = temporary_journal("missing-lock");
        let (source, coordinate, segment) = ready_fixture(&root);
        let lock = segment.with_extension("lock");
        std::fs::write(&lock, b"").unwrap();
        assert!(matches!(
            read_original(&root, &source, &coordinate),
            OriginalRead::Ready { .. }
        ));
        std::fs::remove_file(&lock).unwrap();
        assert!(matches!(
            read_original(&root, &source, &coordinate),
            OriginalRead::Ready { .. }
        ));
        assert!(!lock.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "full-tests")]
    fn delete_original(root: &Path, coordinate: &Coordinate) {
        use solstone_core_retention::door::remove_segments;
        use solstone_core_retention::{RemovalReason, Target};
        let outcome = remove_segments(
            root,
            &[Target {
                day: coordinate.day.clone(),
                stream: coordinate.stream.clone(),
                dir: coordinate.segment.clone(),
            }],
            "2026-01-03T00:00:00Z",
            RemovalReason::OwnerSegmentDelete,
            "memory-read-race",
        );
        assert_eq!(outcome.halted, None, "{outcome:?}");
        assert_eq!(outcome.targets.len(), 1, "{outcome:?}");
        assert!(!outcome.has_failures(), "{outcome:?}");
        assert!(!outcome.targets[0].removed.is_empty(), "{outcome:?}");
    }

    #[cfg(feature = "full-tests")]
    #[test]
    fn deletion_completes_with_retained_directory_during_final_validation() {
        for phase in [
            ReadPhase::BeforeLiveBinding,
            ReadPhase::AfterLiveBinding,
            ReadPhase::AfterMarkers,
            ReadPhase::AfterFileCheck,
        ] {
            let root = temporary_journal(&format!("delete-{phase:?}"));
            let (source, coordinate, segment) = ready_fixture(&root);
            let mut deleted = false;
            let result = read_original_observed(
                &root,
                &source,
                &coordinate,
                Instant::now() + Duration::from_secs(5),
                |observed| {
                    if !deleted && observed == phase {
                        delete_original(&root, &coordinate);
                        deleted = true;
                    }
                },
            );
            assert!(deleted);
            assert!(
                !matches!(result, OriginalRead::Ready { .. }),
                "{phase:?}: {result:?}"
            );
            assert!(segment.join("tombstone.json").is_file());
            assert!(!segment.join("note.txt").exists());
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[cfg(all(feature = "full-tests", not(windows)))]
    #[test]
    fn real_deletion_completes_while_bounded_reader_holds_leaf_open() {
        let root = temporary_journal("delete-open-leaf");
        let (source, coordinate, segment) = ready_fixture(&root);
        let hook_root = root.clone();
        let hook_coordinate = coordinate.clone();
        let ran = std::rc::Rc::new(std::cell::Cell::new(false));
        let hook_ran = ran.clone();
        solstone_core_journal_io::read_test_hook::after_open(segment.join("note.txt"), move || {
            delete_original(&hook_root, &hook_coordinate);
            hook_ran.set(true);
        });
        assert!(!matches!(
            read_original(&root, &source, &coordinate),
            OriginalRead::Ready { .. }
        ));
        assert!(ran.get());
        assert!(segment.join("tombstone.json").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(all(feature = "full-tests", windows))]
    #[test]
    fn real_deletion_completes_while_bounded_reader_holds_leaf_open() {
        let root = temporary_journal("delete-open-leaf");
        let (source, coordinate, segment) = ready_fixture(&root);
        let hook_root = root.clone();
        let hook_coordinate = coordinate.clone();
        let worker = std::rc::Rc::new(std::cell::RefCell::new(None));
        let hook_worker = worker.clone();
        solstone_core_journal_io::read_test_hook::after_open(segment.join("note.txt"), move || {
            let (retried, retry) = std::sync::mpsc::channel();
            *hook_worker.borrow_mut() = Some(std::thread::spawn(move || {
                solstone_core_retention::door::stage_test_hook::on_retry(move || {
                    retried.send(()).unwrap();
                });
                delete_original(&hook_root, &hook_coordinate);
            }));
            // Prove deletion encountered the held leaf, then let the real read
            // close it. A one-shot staging rename fails this handshake.
            retry.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let _read = read_original(&root, &source, &coordinate);
        worker.borrow_mut().take().unwrap().join().unwrap();
        assert_eq!(
            read_original(&root, &source, &coordinate),
            OriginalRead::Deleted
        );
        assert!(segment.join("tombstone.json").is_file());
        assert!(!segment.join("note.txt").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "full-tests")]
    #[test]
    fn staged_missing_replaced_and_changed_originals_are_refused() {
        for action in ["stage", "remove", "replace", "tombstone", "leaf"] {
            let root = temporary_journal(action);
            let (source, coordinate, segment) = ready_fixture(&root);
            let result = read_original_observed(
                &root,
                &source,
                &coordinate,
                Instant::now() + Duration::from_secs(5),
                |phase| {
                    if phase != ReadPhase::BeforeLiveBinding {
                        return;
                    }
                    match action {
                        "stage" => std::fs::rename(
                            &segment,
                            segment.with_file_name(format!(".removing_{}", coordinate.segment)),
                        )
                        .unwrap(),
                        "remove" => std::fs::remove_dir_all(&segment).unwrap(),
                        "replace" => {
                            std::fs::rename(&segment, segment.with_file_name("old-segment"))
                                .unwrap();
                            ready_fixture(&root);
                        }
                        "tombstone" => {
                            std::fs::write(segment.join("tombstone.json"), b"{}").unwrap()
                        }
                        "leaf" => std::fs::remove_file(segment.join("note.txt")).unwrap(),
                        _ => unreachable!(),
                    }
                },
            );
            assert!(
                !matches!(result, OriginalRead::Ready { .. }),
                "{action}: {result:?}"
            );
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[cfg(feature = "full-tests")]
    #[test]
    fn absent_parent_is_not_created() {
        let parent = scratch_root().join(format!("memory-original-absent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);
        let source = SourceKey::from_verified_id("test-source");
        let coordinate = Coordinate {
            day: "20260102".into(),
            stream: format!("agent-memory-{}", source.component()),
            segment: "030405_1".into(),
        };
        assert_eq!(
            read_original(&parent, &source, &coordinate),
            OriginalRead::Absent
        );
        assert!(!parent.exists());
    }

    #[cfg(feature = "full-tests")]
    #[test]
    fn tombstone_and_staged_sibling_are_reported_without_mutation() {
        let tombstone_root = temporary_journal("tombstone");
        let (source, coordinate, segment) = ready_fixture(&tombstone_root);
        std::fs::write(segment.join("tombstone.json"), b"{}").unwrap();
        assert_eq!(
            read_original(&tombstone_root, &source, &coordinate),
            OriginalRead::Deleted
        );
        assert!(segment.join("tombstone.json").exists());
        let _ = std::fs::remove_dir_all(tombstone_root);

        let staged_root = temporary_journal("staged");
        let (source, coordinate, segment) = ready_fixture(&staged_root);
        let staged = segment.with_file_name(format!(".removing_{}", coordinate.segment));
        std::fs::rename(&segment, &staged).unwrap();
        assert_eq!(
            read_original(&staged_root, &source, &coordinate),
            OriginalRead::Staged
        );
        assert!(staged.exists());
        assert!(!segment.exists());
        let _ = std::fs::remove_dir_all(staged_root);
    }

    #[cfg(all(feature = "full-tests", unix))]
    #[test]
    fn symlink_refusal_leaves_names_unchanged() {
        use std::os::unix::fs::symlink;

        let root = temporary_journal("symlink");
        let (source, coordinate, segment) = ready_fixture(&root);
        let outside = root.join("outside-ready.json");
        std::fs::write(&outside, b"untouched").unwrap();
        std::fs::remove_file(segment.join("ready.json")).unwrap();
        symlink(&outside, segment.join("ready.json")).unwrap();
        assert!(matches!(
            read_original(&root, &source, &coordinate),
            OriginalRead::Unavailable { .. }
        ));
        assert!(
            std::fs::symlink_metadata(segment.join("ready.json"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(outside).unwrap(), b"untouched");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(all(feature = "full-tests", windows))]
    #[test]
    fn windows_path_proof_refuses_symlinked_ready_marker() {
        use std::os::windows::fs::symlink_file;

        let root = temporary_journal("windows-path-proof");
        let (source, coordinate, segment) = ready_fixture(&root);
        let outside = root.join("outside-ready.json");
        std::fs::write(&outside, b"untouched").unwrap();
        std::fs::remove_file(segment.join("ready.json")).unwrap();
        symlink_file(&outside, segment.join("ready.json")).unwrap();
        assert!(matches!(
            read_original(&root, &source, &coordinate),
            OriginalRead::Unavailable { .. }
        ));
        assert_eq!(std::fs::read(outside).unwrap(), b"untouched");
        let _ = std::fs::remove_dir_all(root);
    }
}
