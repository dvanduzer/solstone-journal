// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use tokio::sync::Notify;

struct Gate {
    lock: Arc<AsyncMutex<()>>,
    #[cfg(test)]
    waiters: AtomicUsize,
    #[cfg(test)]
    notify: Notify,
}

impl Gate {
    fn new() -> Self {
        Self {
            lock: Arc::new(AsyncMutex::new(())),
            #[cfg(test)]
            waiters: AtomicUsize::new(0),
            #[cfg(test)]
            notify: Notify::new(),
        }
    }
}

type LockMap = HashMap<(PathBuf, String), Arc<Gate>>;

static LOCK_MAP: Mutex<Option<LockMap>> = Mutex::new(None);

fn get_or_insert_gate(root: &Path, id: &str) -> Arc<Gate> {
    let mut guard = match LOCK_MAP.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let map = guard.get_or_insert_with(HashMap::new);
    let key = (root.to_path_buf(), id.to_owned());
    map.entry(key)
        .or_insert_with(|| Arc::new(Gate::new()))
        .clone()
}

pub(crate) async fn acquire(root: &Path, id: &str) -> OwnedMutexGuard<()> {
    let gate = get_or_insert_gate(root, id);
    #[cfg(test)]
    {
        gate.waiters.fetch_add(1, Ordering::SeqCst);
        gate.notify.notify_waiters();
        let guard = gate.lock.clone().lock_owned().await;
        gate.waiters.fetch_sub(1, Ordering::SeqCst);
        guard
    }
    #[cfg(not(test))]
    {
        gate.lock.clone().lock_owned().await
    }
}

#[cfg(test)]
pub(crate) fn start_lock_waiters(root: &Path, id: &str) -> usize {
    let gate = get_or_insert_gate(root, id);
    gate.waiters.load(Ordering::SeqCst)
}

#[cfg(test)]
pub(crate) async fn wait_for_start_lock_waiters(root: &Path, id: &str, min_waiters: usize) {
    let gate = get_or_insert_gate(root, id);
    loop {
        if gate.waiters.load(Ordering::SeqCst) >= min_waiters {
            return;
        }
        let notified = gate.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if gate.waiters.load(Ordering::SeqCst) >= min_waiters {
            return;
        }
        notified.await;
    }
}
