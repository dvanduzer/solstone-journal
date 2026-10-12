// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use solstone_core_journal_io::{LockError, LockOptions, contained_path, hold_lock};

use crate::trust_lock::hold_entity_trust_lock_with_options;
use crate::{EntityTrustLockError, hold_entity_trust_lock};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "solstone-core-entity-trust-lock-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn nested_lock_reuses_the_sidecar_until_the_outermost_drop() {
    let temporary = TempDir::new();
    let lock_path = trust_lock_path(temporary.path());
    let outer = hold_entity_trust_lock(temporary.path()).unwrap();
    let inner = hold_entity_trust_lock(temporary.path()).unwrap();

    drop(inner);
    assert_timeout(&lock_path);

    drop(outer);
    let released = hold_lock(&lock_path, short_options()).unwrap();
    drop(released);
    assert!(sidecar_path(&lock_path).is_file());
}

#[test]
fn equivalent_journal_root_spellings_reenter_the_same_lock() {
    let temporary = TempDir::new();
    let outer = hold_entity_trust_lock(temporary.path()).unwrap();
    let equivalent = temporary.path().join(".");

    let nested = hold_entity_trust_lock(&equivalent).unwrap();

    drop(nested);
    drop(outer);
}

#[test]
fn external_sidecar_contention_is_reported_as_a_timeout() {
    let temporary = TempDir::new();
    let lock_path = trust_lock_path(temporary.path());
    let held = hold_lock(&lock_path, LockOptions::default()).unwrap();

    let error = hold_entity_trust_lock_with_options(temporary.path(), short_options()).unwrap_err();

    assert!(matches!(
        error,
        EntityTrustLockError::Lock(LockError::Timeout(_))
    ));
    drop(held);
}

fn trust_lock_path(root: &Path) -> PathBuf {
    contained_path(root, "health/locks/entity-trust").unwrap()
}

fn sidecar_path(lock_path: &Path) -> PathBuf {
    let name = lock_path.file_name().unwrap().to_string_lossy();
    lock_path.parent().unwrap().join(format!("{name}.lock"))
}

fn assert_timeout(lock_path: &Path) {
    assert!(matches!(
        hold_lock(lock_path, short_options()),
        Err(LockError::Timeout(_))
    ));
}

fn short_options() -> LockOptions {
    LockOptions {
        timeout: Duration::from_millis(50),
        ..LockOptions::default()
    }
}

#[test]
fn after_release_work_waits_for_the_outermost_trust_guard_and_runs_once_per_key() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let temp = TempDir::new();
    let journal = temp.path.as_path();
    let ran = Rc::new(RefCell::new(Vec::new()));
    let record = |label: &'static str| {
        let ran = Rc::clone(&ran);
        move || ran.borrow_mut().push(label)
    };

    crate::after_entity_trust_released(journal, Path::new("unheld"), record("unheld"));
    assert_eq!(*ran.borrow(), ["unheld"]);

    let outer = crate::hold_facet_trust_lock(journal).unwrap();
    let inner = hold_entity_trust_lock(journal).unwrap();
    crate::after_entity_trust_released(journal, Path::new("a"), record("a"));
    crate::after_entity_trust_released(journal, Path::new("a"), record("a again"));
    crate::after_entity_trust_released(journal, Path::new("b"), record("b"));
    drop(inner);
    assert_eq!(*ran.borrow(), ["unheld"]);
    drop(outer);
    assert_eq!(*ran.borrow(), ["unheld", "a", "b"]);
}
