// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fs;
use std::io;

use sha2::{Digest, Sha256};
use solstone_core_journal_io::{AtomicWriteError, AtomicWriteOptions, write_bytes_exclusive};

use crate::{ContentName, SegmentDir, SegmentError};

/// Immutable content facts used for collision outcomes and identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentDescriptor {
    pub name: ContentName,
    pub sha256: String,
    pub size: u64,
}

/// Result of a single create-exclusive content write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContentWriteOutcome {
    Written(ContentDescriptor),
    AlreadyHeld(ContentDescriptor),
    Conflict {
        incoming: ContentDescriptor,
        existing: ContentDescriptor,
    },
}

/// Write one content file without overwriting a held filename.
pub fn write_content(
    segment: &SegmentDir,
    name: ContentName,
    bytes: &[u8],
) -> Result<ContentWriteOutcome, SegmentError> {
    let path = segment.path.join(name.as_str());
    let incoming = descriptor(name.clone(), bytes);
    let coordination = |source| SegmentError::Io {
        path: path.clone(),
        source,
    };
    crate::output_attempt::prepare_segment(segment.path()).map_err(coordination)?;
    let claims = crate::output_attempt::OutputClaims::acquire(
        [path.with_extension("jsonl")],
        solstone_core_journal_io::LockOptions::default(),
    )
    .map_err(coordination)?;
    let _commit = claims.commit(segment.path()).map_err(coordination)?;
    match write_bytes_exclusive(&path, bytes, AtomicWriteOptions::default()) {
        Ok(()) => Ok(ContentWriteOutcome::Written(incoming)),
        Err(AtomicWriteError::Io { source, .. })
            if source.kind() == io::ErrorKind::AlreadyExists =>
        {
            let existing_bytes = fs::read(&path).map_err(|source| SegmentError::Io {
                path: path.clone(),
                source,
            })?;
            let existing = descriptor(name, &existing_bytes);
            if existing.sha256 == incoming.sha256 && existing.size == incoming.size {
                Ok(ContentWriteOutcome::AlreadyHeld(incoming))
            } else {
                Ok(ContentWriteOutcome::Conflict { incoming, existing })
            }
        }
        Err(error) => Err(SegmentError::Atomic(error)),
    }
}

pub(crate) fn descriptor(name: ContentName, bytes: &[u8]) -> ContentDescriptor {
    ContentDescriptor {
        name,
        sha256: format!("{:x}", Sha256::digest(bytes)),
        size: bytes.len() as u64,
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use crate::test_support::TempDir;

    use super::*;

    fn segment(root: &Path) -> SegmentDir {
        SegmentDir::resolve(root, "20260804", "120000_60", "workstation").unwrap()
    }

    #[test]
    fn classifies_same_length_conflict_without_overwriting_bytes() {
        let temporary = TempDir::new();
        let segment = segment(temporary.path());
        let name = ContentName::new("audio.flac").unwrap();
        assert!(matches!(
            write_content(&segment, name.clone(), b"abcd").unwrap(),
            ContentWriteOutcome::Written(_)
        ));

        let outcome = write_content(&segment, name.clone(), b"abce").unwrap();
        assert!(matches!(outcome, ContentWriteOutcome::Conflict { .. }));
        assert_eq!(fs::read(segment.path.join(name.as_str())).unwrap(), b"abcd");
    }

    #[test]
    fn classifies_identical_bytes_as_already_held_without_overwriting() {
        let temporary = TempDir::new();
        let segment = segment(temporary.path());
        let name = ContentName::new("audio.flac").unwrap();
        write_content(&segment, name.clone(), b"same").unwrap();

        let outcome = write_content(&segment, name.clone(), b"same").unwrap();
        assert!(matches!(outcome, ContentWriteOutcome::AlreadyHeld(_)));
        assert_eq!(fs::read(segment.path.join(name.as_str())).unwrap(), b"same");
    }

    #[test]
    fn owner_deleted_content_is_not_recreated() {
        let temporary = TempDir::new();
        let segment = segment(temporary.path());
        crate::output_attempt::prepare_segment(segment.path()).unwrap();
        fs::write(segment.path().join("tombstone.json"), b"{}").unwrap();
        assert!(write_content(&segment, ContentName::new("audio.flac").unwrap(), b"raw").is_err());
        assert!(!segment.path().join("audio.flac").exists());
    }

    #[cfg(feature = "full-tests")]
    #[test]
    fn raw_content_waits_for_the_same_stem_attempt_without_retaining_segment_exclusion() {
        use std::sync::mpsc;
        use std::time::Duration;
        let temporary = TempDir::new();
        let segment = segment(temporary.path());
        crate::output_attempt::prepare_segment(segment.path()).unwrap();
        let claim = crate::output_attempt::OutputClaims::acquire(
            [segment.path().join("audio.jsonl")],
            Default::default(),
        )
        .unwrap();
        let directory = segment.path().to_path_buf();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx
                .send(write_content(
                    &segment,
                    ContentName::new("audio.flac").unwrap(),
                    b"raw",
                ))
                .unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        assert!(
            solstone_core_journal_io::hold_lock(
                &directory,
                solstone_core_journal_io::LockOptions {
                    timeout: Duration::ZERO,
                    ..Default::default()
                }
            )
            .is_ok()
        );
        drop(claim);
        assert!(matches!(
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap(),
            ContentWriteOutcome::Written(_)
        ));
        worker.join().unwrap();
        assert_eq!(fs::read(directory.join("audio.flac")).unwrap(), b"raw");
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_collision_is_an_io_error() {
        let temporary = TempDir::new();
        let segment = segment(temporary.path());
        let name = ContentName::new("audio.flac").unwrap();
        write_content(&segment, name.clone(), b"held").unwrap();
        let path = segment.path.join(name.as_str());
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o000);
        fs::set_permissions(&path, permissions).unwrap();

        let result = write_content(&segment, name, b"held");
        let mut restore = fs::metadata(&path).unwrap().permissions();
        restore.set_mode(0o600);
        fs::set_permissions(&path, restore).unwrap();
        assert!(matches!(result, Err(SegmentError::Io { .. })));
    }
}
