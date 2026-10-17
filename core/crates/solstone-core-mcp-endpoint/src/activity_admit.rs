// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Admit one indexed activity only when its source file still matches the proof.

use std::path::Path;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use solstone_core_format::content::{ContentResolution, Family, classify};
use solstone_core_indexer_query::{
    ActivityObservation, ActivityObservationRead, ConnectionBoundary, IndexAccessError,
    QueryBoundary, read_activity_observation,
};
use solstone_core_journal_io::{JournalRoot, read_relative_file_bounded};

/// Largest activity file an agent read will open. A larger file is withheld.
pub(crate) const ACTIVITY_SOURCE_BUDGET: u64 = 4 * 1024 * 1024;

pub(crate) struct AdmittedActivity {
    pub text: String,
    pub hidden: bool,
    pub proof: String,
    pub record: Map<String, Value>,
}

pub(crate) enum ActivityAdmit {
    NotActivity,
    Admitted(AdmittedActivity),
    Withheld { cursor_proof: Option<String> },
}

pub(crate) enum ActivityCursorProof {
    NotActivity,
    Matches,
    Reject,
}

pub(crate) fn is_activity_path(path: &str) -> bool {
    classify(path) == ContentResolution::Indexed(Family::Activity)
}

/// Grant admission has already succeeded. Verify the indexed observation, then
/// the source file. Withhold text, hidden, and links on every failure.
#[allow(clippy::too_many_arguments)]
pub(crate) fn admit_activity(
    journal_root: &Path,
    boundary: &ConnectionBoundary,
    path: &str,
    idx: i64,
    row_id: i64,
    candidate: &str,
    required_proof: Option<&str>,
    max_bytes: u64,
) -> Result<ActivityAdmit, IndexAccessError> {
    pause_activity_read_seam();
    let read = read_activity_observation(
        journal_root,
        QueryBoundary::Connection(boundary.clone()),
        path,
        idx,
        row_id,
        max_bytes,
    )?;
    if !indexed_activity(&read, path) {
        return Ok(ActivityAdmit::NotActivity);
    }
    let ActivityObservationRead::Ready(observation) = read else {
        return Ok(ActivityAdmit::Withheld { cursor_proof: None });
    };
    if observation.content != candidate {
        return Ok(ActivityAdmit::Withheld { cursor_proof: None });
    }
    let proof = hex_encode(&observation.digest);
    if required_proof.is_some_and(|required| required != proof) {
        return Ok(ActivityAdmit::Withheld {
            cursor_proof: Some(proof),
        });
    }
    let Some(bytes) = read_contained_source(journal_root, path) else {
        return Ok(ActivityAdmit::Withheld {
            cursor_proof: Some(proof),
        });
    };
    if Sha256::digest(&bytes).as_slice() != observation.digest.as_slice() {
        return Ok(ActivityAdmit::Withheld {
            cursor_proof: Some(proof),
        });
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(ActivityAdmit::Withheld {
            cursor_proof: Some(proof),
        });
    };
    let Some(record) = kept_record(&text, idx) else {
        return Ok(ActivityAdmit::Withheld {
            cursor_proof: Some(proof),
        });
    };
    Ok(ActivityAdmit::Admitted(AdmittedActivity {
        text: observation.content,
        hidden: observation.hidden,
        proof,
        record,
    }))
}

/// Proof check for a cursor. Does not open the source file.
#[allow(clippy::too_many_arguments)]
pub(crate) fn activity_cursor_proof_matches(
    journal_root: &Path,
    boundary: &ConnectionBoundary,
    path: &str,
    idx: i64,
    row_id: i64,
    candidate: &str,
    proof: &str,
    max_bytes: u64,
) -> Result<ActivityCursorProof, IndexAccessError> {
    pause_activity_read_seam();
    let read = read_activity_observation(
        journal_root,
        QueryBoundary::Connection(boundary.clone()),
        path,
        idx,
        row_id,
        max_bytes,
    )?;
    if !indexed_activity(&read, path) {
        return Ok(ActivityCursorProof::NotActivity);
    }
    let ActivityObservationRead::Ready(observation) = read else {
        return Ok(ActivityCursorProof::Reject);
    };
    Ok(if observation_matches(&observation, candidate, proof) {
        ActivityCursorProof::Matches
    } else {
        ActivityCursorProof::Reject
    })
}

/// Whether a missing activity proof on the token must fail closed.
///
/// This reads the index only. It does not open the source or a shape sidecar.
pub(crate) fn activity_proof_required(
    journal_root: &Path,
    boundary: &ConnectionBoundary,
    path: &str,
    idx: i64,
    row_id: i64,
    max_bytes: u64,
) -> Result<bool, IndexAccessError> {
    let read = read_activity_observation(
        journal_root,
        QueryBoundary::Connection(boundary.clone()),
        path,
        idx,
        row_id,
        max_bytes,
    )?;
    Ok(indexed_activity(&read, path))
}

fn indexed_activity(read: &ActivityObservationRead, path: &str) -> bool {
    match read {
        ActivityObservationRead::Ready(_) | ActivityObservationRead::Unavailable => true,
        ActivityObservationRead::Unproven | ActivityObservationRead::Absent => {
            is_activity_path(path)
        }
    }
}

fn observation_matches(observation: &ActivityObservation, candidate: &str, proof: &str) -> bool {
    observation.content == candidate && hex_encode(&observation.digest) == proof
}

fn kept_record(text: &str, idx: i64) -> Option<Map<String, Value>> {
    let index = usize::try_from(idx).ok()?;
    let mut kept = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(Value::Object(record)) => kept.push(record),
            _ => return None,
        }
    }
    kept.into_iter().nth(index)
}

fn read_contained_source(journal_root: &Path, rel: &str) -> Option<Vec<u8>> {
    let root = JournalRoot::open(journal_root).ok()?;
    let maximum = usize::try_from(ACTIVITY_SOURCE_BUDGET).ok()?;
    match read_relative_file_bounded(&root, Path::new(rel), maximum) {
        Ok(Some(observation)) => Some(observation.bytes),
        Ok(None) | Err(_) => None,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0xf) as usize] as char);
    }
    encoded
}

#[cfg(all(test, any(feature = "full-tests", feature = "test-hooks")))]
mod seam {
    use std::sync::{Condvar, Mutex};
    use std::thread::ThreadId;
    use std::time::Duration;

    struct SeamState {
        thread: ThreadId,
        entered: bool,
        release: bool,
    }

    static SEAM: Mutex<Option<SeamState>> = Mutex::new(None);
    static SEAM_CV: Condvar = Condvar::new();

    pub(crate) struct ActivityReadSeamGuard {
        thread: ThreadId,
    }

    impl Drop for ActivityReadSeamGuard {
        fn drop(&mut self) {
            let mut guard = SEAM.lock().expect("activity read seam");
            if guard
                .as_ref()
                .is_some_and(|state| state.thread == self.thread)
            {
                *guard = None;
                SEAM_CV.notify_all();
            }
        }
    }

    pub(super) fn pause() {
        let id = std::thread::current().id();
        let mut guard = SEAM.lock().expect("activity read seam");
        if guard.as_ref().is_none_or(|state| state.thread != id) {
            return;
        }
        if let Some(state) = guard.as_mut() {
            state.entered = true;
        }
        SEAM_CV.notify_all();
        while guard
            .as_ref()
            .is_some_and(|state| state.thread == id && !state.release)
        {
            guard = SEAM_CV.wait(guard).expect("activity read seam");
        }
    }

    pub(crate) fn arm(thread: ThreadId) -> ActivityReadSeamGuard {
        let mut guard = SEAM.lock().expect("activity read seam");
        *guard = Some(SeamState {
            thread,
            entered: false,
            release: false,
        });
        SEAM_CV.notify_all();
        ActivityReadSeamGuard { thread }
    }

    pub(crate) fn wait_until_entered(thread: ThreadId) {
        let mut guard = SEAM.lock().expect("activity read seam");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if guard
                .as_ref()
                .is_some_and(|state| state.thread == thread && state.entered)
            {
                return;
            }
            let now = std::time::Instant::now();
            assert!(
                now < deadline,
                "activity read seam was not reached for the registered thread"
            );
            let (next, timeout) = SEAM_CV
                .wait_timeout(guard, deadline.saturating_duration_since(now))
                .expect("activity read seam");
            guard = next;
            if timeout.timed_out()
                && !guard
                    .as_ref()
                    .is_some_and(|state| state.thread == thread && state.entered)
            {
                panic!("activity read seam was not reached for the registered thread");
            }
        }
    }

    pub(crate) fn release(thread: ThreadId) {
        let mut guard = SEAM.lock().expect("activity read seam");
        if let Some(state) = guard.as_mut().filter(|state| state.thread == thread) {
            state.release = true;
        }
        SEAM_CV.notify_all();
    }
}

#[cfg(all(test, any(feature = "full-tests", feature = "test-hooks")))]
fn pause_activity_read_seam() {
    seam::pause();
}

#[cfg(not(all(test, any(feature = "full-tests", feature = "test-hooks"))))]
fn pause_activity_read_seam() {}

#[cfg(all(test, any(feature = "full-tests", feature = "test-hooks")))]
pub(crate) use seam::{
    arm as arm_activity_read_seam, release as release_activity_read_seam,
    wait_until_entered as wait_until_activity_read_seamed,
};

#[cfg(all(test, feature = "full-tests"))]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::path::Path;

    use super::{ACTIVITY_SOURCE_BUDGET, read_contained_source};

    fn root() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("activity-source-")
            .tempdir_in(crate::test_scratch())
            .unwrap()
    }

    fn write_file(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = fs::File::create(path).unwrap();
        file.write_all(bytes).unwrap();
    }

    #[test]
    fn contained_activity_read_refuses_escape_and_oversized_files() {
        let journal = root();
        let rel = "facets/alpha/activities/20261010.jsonl";
        let bytes = b"{\"id\":\"kept\"}\n";
        write_file(&journal.path().join(rel), bytes);
        assert_eq!(
            read_contained_source(journal.path(), rel).as_deref(),
            Some(bytes.as_slice())
        );
        assert!(
            read_contained_source(journal.path(), "facets/../activities/20261010.jsonl").is_none()
        );
        assert!(read_contained_source(journal.path(), "/etc/passwd").is_none());
        assert!(read_contained_source(journal.path(), "").is_none());

        let big = journal
            .path()
            .join("facets/alpha/activities/20261011.jsonl");
        write_file(&big, &vec![b'a'; ACTIVITY_SOURCE_BUDGET as usize + 1]);
        assert!(
            read_contained_source(journal.path(), "facets/alpha/activities/20261011.jsonl")
                .is_none()
        );

        let dir = journal
            .path()
            .join("facets/alpha/activities/20261012.jsonl");
        fs::create_dir_all(&dir).unwrap();
        assert!(
            read_contained_source(journal.path(), "facets/alpha/activities/20261012.jsonl")
                .is_none()
        );

        #[cfg(unix)]
        {
            let target_rel = "facets/alpha/activities/real.jsonl";
            write_file(&journal.path().join(target_rel), bytes);
            let link = journal
                .path()
                .join("facets/alpha/activities/20261013.jsonl");
            std::os::unix::fs::symlink(journal.path().join(target_rel), &link).unwrap();
            assert!(
                read_contained_source(journal.path(), "facets/alpha/activities/20261013.jsonl")
                    .is_none()
            );
            let linked_dir = journal.path().join("linked-facet");
            fs::create_dir_all(linked_dir.join("activities")).unwrap();
            write_file(&linked_dir.join("activities/20261014.jsonl"), bytes);
            std::os::unix::fs::symlink(&linked_dir, journal.path().join("facets/beta")).unwrap();
            assert!(
                read_contained_source(journal.path(), "facets/beta/activities/20261014.jsonl")
                    .is_none()
            );
        }
    }
}
