// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::append_jsonl_bound;
use super::bound_append::{
    BoundAppendConfirmation as Confirmation, BoundAppendDurability as Durability,
    BoundAppendEffect as Effect, Hooks, Point, append_with_hooks,
};
use crate::{FlatDirectory, JournalRoot};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

fn parent(root: &tempfile::TempDir) -> FlatDirectory {
    fs::create_dir(root.path().join("metadata")).unwrap();
    let root = JournalRoot::open(root.path()).unwrap();
    FlatDirectory::open(&root, Path::new("metadata")).unwrap()
}
fn full(receipt: &super::BoundAppendReceipt) {
    assert!(matches!(receipt.effect(), Effect::Appended { .. }));
    assert_eq!(receipt.file_durability(), Durability::Synced);
    assert_eq!(receipt.parent_durability(), Durability::Synced);
    assert_eq!(receipt.confirmation(), Confirmation::Confirmed);
    assert!(receipt.failures().is_empty());
}

#[test]
fn bound_append_absent_existing_and_exact_wire() {
    let root = tempfile::tempdir().unwrap();
    let parent = parent(&root);
    let name = OsStr::new("events.jsonl");
    let record =
        serde_json::json!({"tract":"retention","event":"original_deleted","name":"audio.flac"});
    full(&append_jsonl_bound(&parent, name, &record));
    full(&append_jsonl_bound(&parent, name, &record));
    let expected = format!("{record}\n{record}\n");
    assert_eq!(
        fs::read(root.path().join("metadata/events.jsonl")).unwrap(),
        expected.as_bytes()
    );
}

struct Faults {
    failures: Vec<Point>,
    short: bool,
    writes: usize,
    syncs: Vec<Point>,
    checkpoint: Option<(Point, Box<dyn FnOnce()>)>,
    fired: bool,
}
impl Faults {
    fn new(failures: Vec<Point>) -> Self {
        Self {
            failures,
            short: false,
            writes: 0,
            syncs: Vec::new(),
            checkpoint: None,
            fired: false,
        }
    }
}
impl Hooks for Faults {
    fn checkpoint(&mut self, point: Point) {
        if self
            .checkpoint
            .as_ref()
            .is_some_and(|(expected, _)| *expected == point)
        {
            let (_, callback) = self.checkpoint.take().unwrap();
            self.fired = true;
            callback();
        }
    }
    fn write(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        if self.short {
            self.short = false;
            file.write(&bytes[..3])
        } else {
            file.write(bytes)
        }
    }
    fn sync(&mut self, file: &impl std::os::fd::AsFd, point: Point) -> io::Result<()> {
        self.syncs.push(point);
        if self.failures.contains(&point) {
            Err(io::Error::other("injected sync failure"))
        } else {
            nix::unistd::fsync(file).map_err(io::Error::from)
        }
    }
}

#[test]
fn bound_append_sync_facts_and_confirmation_are_independent() {
    for failures in [
        vec![Point::FileSync],
        vec![Point::ParentSync],
        vec![Point::FileSync, Point::ParentSync],
    ] {
        let root = tempfile::tempdir().unwrap();
        let parent = parent(&root);
        let mut faults = Faults::new(failures.clone());
        let receipt = append_with_hooks(
            &parent,
            OsStr::new("events.jsonl"),
            &serde_json::json!({"x":1}),
            &mut faults,
        );
        assert!(matches!(receipt.effect(), Effect::Appended { .. }));
        assert_eq!(receipt.confirmation(), Confirmation::Confirmed);
        assert_eq!(faults.writes, 1);
        assert_eq!(faults.syncs, vec![Point::FileSync, Point::ParentSync]);
        assert_eq!(receipt.failures().len(), failures.len());
        assert_eq!(
            receipt.file_durability(),
            if failures.contains(&Point::FileSync) {
                Durability::Unproven
            } else {
                Durability::Synced
            }
        );
        assert_eq!(
            receipt.parent_durability(),
            if failures.contains(&Point::ParentSync) {
                Durability::Unproven
            } else {
                Durability::Synced
            }
        );
    }
}

#[test]
fn bound_append_actual_short_write_keeps_prefix_and_never_repairs() {
    let root = tempfile::tempdir().unwrap();
    let parent = parent(&root);
    let mut faults = Faults::new(vec![]);
    faults.short = true;
    let receipt = append_with_hooks(
        &parent,
        OsStr::new("events.jsonl"),
        &serde_json::json!({"x":1}),
        &mut faults,
    );
    assert_eq!(faults.writes, 1);
    assert!(!faults.short);
    assert_eq!(receipt.effect(), Effect::Attempted { written: Some(3) });
    assert_eq!(receipt.confirmation(), Confirmation::Unverified);
    assert_eq!(
        fs::read(root.path().join("metadata/events.jsonl")).unwrap(),
        b"{\"x"
    );
    assert_eq!(faults.syncs, vec![Point::FileSync, Point::ParentSync]);
    assert!(!receipt.failures().is_empty());
}

#[test]
fn bound_append_refuses_invalid_inputs_without_changing_them() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let parent = parent(&root);
    let dir = root.path().join("metadata");
    fs::write(dir.join("torn"), b"{\"old\":1}").unwrap();
    fs::create_dir(dir.join("directory")).unwrap();
    fs::write(root.path().join("outside"), b"outside\n").unwrap();
    symlink(root.path().join("outside"), dir.join("link")).unwrap();
    fs::hard_link(root.path().join("outside"), dir.join("hard")).unwrap();
    let record = serde_json::json!({"x":1});
    for name in ["../outside", "link", "directory", "torn", "hard"] {
        let receipt = append_jsonl_bound(&parent, OsStr::new(name), &record);
        assert_ne!(receipt.confirmation(), Confirmation::Confirmed);
        assert!(!receipt.failures().is_empty());
    }
    assert_eq!(fs::read(root.path().join("outside")).unwrap(), b"outside\n");
    assert_eq!(fs::read(dir.join("torn")).unwrap(), b"{\"old\":1}");
    let receipt = append_jsonl_bound(
        &parent,
        OsStr::new("too-big-record"),
        &"x".repeat(1024 * 1024),
    );
    assert_eq!(receipt.effect(), Effect::NotAttempted);
    assert!(!dir.join("too-big-record").exists());
}

#[test]
fn bound_append_boundary_input_is_confirmable_and_oversize_is_untouched() {
    let root = tempfile::tempdir().unwrap();
    let parent = parent(&root);
    let path = root.path().join("metadata/events.jsonl");
    let mut bytes = vec![b' '; 32 * 1024 * 1024];
    *bytes.last_mut().unwrap() = b'\n';
    fs::write(&path, &bytes).unwrap();
    full(&append_jsonl_bound(&parent, OsStr::new("events.jsonl"), &0));
    assert_eq!(fs::metadata(&path).unwrap().len(), (bytes.len() + 2) as u64);
    let receipt = append_jsonl_bound(&parent, OsStr::new("events.jsonl"), &1);
    assert_eq!(receipt.effect(), Effect::NotAttempted);
    assert_eq!(fs::metadata(&path).unwrap().len(), (bytes.len() + 2) as u64);
}

#[test]
fn bound_append_substitution_at_open_and_confirmation_refuses_credit() {
    for point in [
        Point::BeforeOpen,
        Point::BeforeWrite,
        Point::AfterWrite,
        Point::AfterConfirmationRead,
    ] {
        let root = tempfile::tempdir().unwrap();
        let parent = parent(&root);
        let path = root.path().join("metadata/events.jsonl");
        let original = root.path().join("original");
        fs::write(&path, b"old\n").unwrap();
        let callback_path = path.clone();
        let callback_original = original.clone();
        let mut faults = Faults::new(vec![]);
        faults.checkpoint = Some((
            point,
            Box::new(move || {
                let old = fs::metadata(&callback_path).unwrap().modified().unwrap();
                fs::rename(&callback_path, &callback_original).unwrap();
                fs::write(&callback_path, b"new\n").unwrap();
                File::options()
                    .write(true)
                    .open(&callback_path)
                    .unwrap()
                    .set_modified(old)
                    .unwrap();
            }),
        ));
        let receipt = append_with_hooks(&parent, OsStr::new("events.jsonl"), &0, &mut faults);
        assert!(faults.fired);
        assert_eq!(receipt.confirmation(), Confirmation::Unverified);
        assert_eq!(fs::read(path).unwrap(), b"new\n");
        if matches!(point, Point::BeforeOpen | Point::BeforeWrite) {
            assert_eq!(faults.writes, 0);
            assert_eq!(fs::read(original).unwrap(), b"old\n");
        } else {
            assert_eq!(faults.writes, 1);
            assert_eq!(receipt.effect(), Effect::Appended { bytes: 2 });
            assert_eq!(fs::read(original).unwrap(), b"old\n0\n");
        }
    }
}

#[test]
fn bound_append_retained_parent_is_never_redirected_by_path_replacement() {
    let root = tempfile::tempdir().unwrap();
    let parent = parent(&root);
    fs::rename(root.path().join("metadata"), root.path().join("original")).unwrap();
    fs::create_dir(root.path().join("metadata")).unwrap();
    fs::write(root.path().join("metadata/events.jsonl"), b"competitor\n").unwrap();
    full(&append_jsonl_bound(&parent, OsStr::new("events.jsonl"), &0));
    assert_eq!(
        fs::read(root.path().join("metadata/events.jsonl")).unwrap(),
        b"competitor\n"
    );
    assert_eq!(
        fs::read(root.path().join("original/events.jsonl")).unwrap(),
        b"0\n"
    );
}

#[test]
fn bound_append_same_inode_edits_with_restored_mtime_never_confirm() {
    use std::os::unix::fs::MetadataExt;
    for point in [Point::BeforeWrite, Point::AfterConfirmationRead] {
        let root = tempfile::tempdir().unwrap();
        let parent = parent(&root);
        let path = root.path().join("metadata/events.jsonl");
        fs::write(&path, b"old\n").unwrap();
        let changed = path.clone();
        let mut faults = Faults::new(vec![]);
        faults.checkpoint = Some((
            point,
            Box::new(move || {
                let before = fs::metadata(&changed).unwrap();
                let mut bytes = fs::read(&changed).unwrap();
                bytes[0] = b'n';
                std::thread::sleep(std::time::Duration::from_millis(1));
                fs::write(&changed, &bytes).unwrap();
                File::options()
                    .write(true)
                    .open(&changed)
                    .unwrap()
                    .set_modified(before.modified().unwrap())
                    .unwrap();
                let after = fs::metadata(&changed).unwrap();
                assert_eq!(
                    (
                        before.dev(),
                        before.ino(),
                        before.len(),
                        before.modified().unwrap()
                    ),
                    (
                        after.dev(),
                        after.ino(),
                        after.len(),
                        after.modified().unwrap()
                    )
                );
                assert_ne!(
                    (before.ctime(), before.ctime_nsec()),
                    (after.ctime(), after.ctime_nsec())
                );
            }),
        ));
        let receipt = append_with_hooks(&parent, OsStr::new("events.jsonl"), &0, &mut faults);
        assert!(faults.fired);
        assert_eq!(receipt.confirmation(), Confirmation::Unverified);
        if point == Point::BeforeWrite {
            assert_eq!(faults.writes, 0);
        } else {
            assert_eq!(faults.writes, 1);
        }
        assert_eq!(fs::read(&path).unwrap()[0], b'n');
    }
}
