// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! One-shot, thread-local test interleaving while a bounded reader holds a leaf.
use std::cell::RefCell;
use std::path::{Path, PathBuf};

type Hook = (PathBuf, Box<dyn FnOnce()>);
thread_local! {
    static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
}

/// Run the callback when this thread opens the named leaf, before reading it.
pub fn after_open(path: PathBuf, callback: impl FnOnce() + 'static) {
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    HOOK.with(|hook| *hook.borrow_mut() = Some((path, Box::new(callback))));
}

pub(crate) fn opened(path: &Path) {
    let callback = HOOK.with(|hook| {
        let mut hook = hook.borrow_mut();
        if hook.as_ref().is_some_and(|(expected, _)| expected == path) {
            hook.take().map(|(_, callback)| callback)
        } else {
            None
        }
    });
    if let Some(callback) = callback {
        callback();
    }
}
