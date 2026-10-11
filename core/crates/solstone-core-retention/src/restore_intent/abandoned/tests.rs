// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use solstone_core_journal_io::JournalRoot;
use solstone_core_journal_io::activity_obligations::observe_obligations;
use solstone_core_journal_io::media::{MediaIdentity, digest_reader};

use super::*;
use crate::door::remove_segments;
use crate::restore_intent::tests::fixture;
use crate::{RemovalReason, Target};

const OPERATION: &str = "ffffffffffffffffffffffffffffffff";
const RESTORED: &[u8] = b"restored audio bytes";

fn identity(path: &Path) -> MediaIdentity {
    let metadata = fs::symlink_metadata(path).unwrap();
    MediaIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

struct Interrupted {
    _temp: tempfile::TempDir,
    journal: PathBuf,
    root: Arc<JournalRoot>,
    segment: PathBuf,
    stage: PathBuf,
    intent: PathBuf,
    target: Target,
}

/// A crash-like drop after staging: the stage holds the unpublished copy and
/// the intent is durable, but nothing was published, recorded or marked.
fn interrupted() -> Interrupted {
    let temp = tempfile::tempdir().unwrap();
    let journal = temp.path().to_owned();
    let mut intent = fixture(false);
    let segment = journal.join(intent.locator.relative_segment());
    fs::create_dir_all(&segment).unwrap();
    fs::write(segment.join("other.jsonl"), b"{}\n").unwrap();
    let parent = segment.parent().unwrap().to_owned();
    let stage = parent.join(&intent.stage.stage_name);
    fs::create_dir(&stage).unwrap();
    fs::write(stage.join("payload"), RESTORED).unwrap();
    let root = Arc::new(JournalRoot::open(&journal).unwrap());
    let digest = digest_reader(&mut &RESTORED[..]).unwrap();
    intent.witness.file.bytes = digest.size;
    intent.witness.file.sha256 = digest.sha256;
    intent.destination.root = root.identity().observation_id();
    intent.destination.parent = identity(&parent);
    intent.destination.segment = identity(&segment);
    intent.stage.parent = identity(&parent);
    intent.stage.directory = identity(&stage);
    intent.stage.leaf = identity(&stage.join("payload"));
    let intents = journal.join("health/offload/restore-intents");
    fs::create_dir_all(&intents).unwrap();
    let path = intents.join(format!("{OPERATION}.json"));
    fs::write(&path, intent.encode().unwrap()).unwrap();
    let target = intent.locator.target.clone();
    Interrupted {
        _temp: temp,
        journal,
        root,
        segment,
        stage,
        intent: path,
        target,
    }
}

fn delete(fixture: &Interrupted) -> crate::Outcome {
    remove_segments(
        &fixture.journal,
        std::slice::from_ref(&fixture.target),
        "2026-10-11T00:00:00Z",
        RemovalReason::OwnerSegmentDelete,
        "fixture",
    )
}

fn recover(fixture: &Interrupted) -> Vec<AbandonedRestore> {
    let activity = ActivityLease::try_acquire(Arc::clone(&fixture.root)).unwrap();
    recover_abandoned_restores(&activity)
}

#[test]
fn unpublished_restore_is_undone_and_owner_deletion_then_proceeds() {
    let interrupted = interrupted();
    assert_eq!(
        observe_obligations(&interrupted.root)
            .unwrap()
            .entries()
            .len(),
        2
    );
    // Owner deletion itself recovers the interrupted restore under its lease.
    let removed = delete(&interrupted);
    assert!(removed.targets[0].not_removed.is_empty(), "{removed:?}");
    assert!(!removed.targets[0].removed.is_empty());
    assert!(!interrupted.stage.exists());
    assert!(!interrupted.intent.exists());
    assert!(
        observe_obligations(&interrupted.root)
            .unwrap()
            .entries()
            .is_empty()
    );
}

#[test]
fn published_restores_are_held_and_never_touched() {
    for (content, expected) in [
        (RESTORED, AbandonedRestoreHold::PublishedAwaitingCompletion),
        (
            &b"other content"[..],
            AbandonedRestoreHold::PublishedContentMismatch,
        ),
    ] {
        let interrupted = interrupted();
        let published = interrupted.segment.join("a.flac");
        fs::write(&published, content).unwrap();
        let outcome = recover(&interrupted);
        assert_eq!(outcome.len(), 1);
        assert_eq!(outcome[0].operation_id.as_deref(), Some(OPERATION));
        assert_eq!(outcome[0].outcome, Err(expected));
        let held = delete(&interrupted);
        assert!(held.targets[0].removed.is_empty());
        assert_eq!(fs::read(&published).unwrap(), content);
        assert_eq!(
            fs::read(interrupted.stage.join("payload")).unwrap(),
            RESTORED
        );
        assert!(interrupted.intent.exists());
    }
}

#[test]
fn a_stage_holding_anything_else_is_held() {
    let interrupted = interrupted();
    fs::write(interrupted.stage.join("unexpected"), b"keep").unwrap();
    let outcome = recover(&interrupted);
    assert_eq!(outcome[0].outcome, Err(AbandonedRestoreHold::StageChanged));
    assert_eq!(
        fs::read(interrupted.stage.join("payload")).unwrap(),
        RESTORED
    );
    assert_eq!(
        fs::read(interrupted.stage.join("unexpected")).unwrap(),
        b"keep"
    );
    assert!(interrupted.intent.exists());
    // A replaced copy is not the restore's own and is never removed either.
    let interrupted = self::interrupted();
    let original = interrupted.journal.join("original-payload");
    fs::rename(interrupted.stage.join("payload"), &original).unwrap();
    fs::write(interrupted.stage.join("payload"), RESTORED).unwrap();
    let outcome = recover(&interrupted);
    assert_eq!(outcome[0].outcome, Err(AbandonedRestoreHold::StageChanged));
    assert!(interrupted.stage.join("payload").exists());
    assert!(interrupted.intent.exists());
}
