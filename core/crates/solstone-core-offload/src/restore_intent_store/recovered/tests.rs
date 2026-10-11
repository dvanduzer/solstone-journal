// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::cell::{Cell, RefCell};
use std::io;
use std::os::fd::AsFd;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Checkpoint {
    SuccessorReturned,
    AfterRemoval,
}

type Hook = Box<dyn FnOnce()>;
thread_local! { static CHECKPOINT: RefCell<Option<(Checkpoint, Hook)>> = RefCell::new(None); }
thread_local! { static FAIL_SYNC: Cell<bool> = const { Cell::new(false) }; }

pub(crate) struct Hooks;
impl super::super::unix::Hooks for Hooks {
    fn sync(&mut self, directory: &impl AsFd, index: usize) -> io::Result<()> {
        if FAIL_SYNC.with(|fault| fault.replace(false)) {
            return Err(io::Error::other(
                "injected recovered intent ancestor sync failure",
            ));
        }
        super::super::unix::Hooks::sync(&mut super::super::unix::NativeHooks, directory, index)
    }
}

pub(crate) fn fail_next_sync() {
    FAIL_SYNC.with(|fault| fault.set(true));
}

pub(crate) fn with_hook<T>(
    point: Checkpoint,
    hook: impl FnOnce() + 'static,
    operation: impl FnOnce() -> T,
) -> (T, bool) {
    CHECKPOINT.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some((point, Box::new(hook)));
    });
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            CHECKPOINT.with(|slot| {
                slot.borrow_mut().take();
            });
            FAIL_SYNC.with(|fault| fault.set(false));
        }
    }
    let _guard = Guard;
    let result = operation();
    let consumed = CHECKPOINT.with(|slot| slot.borrow().is_none());
    (result, consumed)
}

pub(crate) fn checkpoint(point: Checkpoint) {
    let hook = CHECKPOINT.with(|current| {
        let mut current = current.borrow_mut();
        if current
            .as_ref()
            .is_some_and(|(expected, _)| *expected == point)
        {
            current.take()
        } else {
            None
        }
    });
    if let Some((_, hook)) = hook {
        hook();
    }
}
