// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Cooperative journal activity exclusion. A lease is not writer settlement,
//! recovery coverage or authority to retire its persistent entry.

use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::errors::ExistingParentLockError;
use crate::journal_root::{JournalRoot, JournalRootError};
use crate::locking::{BoundParentLock, acquire_existing_parent_lock_bound};

const ENTRY: &str = "backup-activity.lock";

#[derive(Debug)]
pub enum ActivityUnavailable {
    Root(JournalRootError),
    Entry(ExistingParentLockError),
    Poisoned,
}

#[derive(Debug)]
pub enum ActivityAdmissionError {
    Busy,
    Unavailable(ActivityUnavailable),
}

impl fmt::Display for ActivityAdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("journal activity admission is busy"),
            Self::Unavailable(ActivityUnavailable::Root(error)) => error.fmt(f),
            Self::Unavailable(ActivityUnavailable::Entry(error)) => error.fmt(f),
            Self::Unavailable(ActivityUnavailable::Poisoned) => {
                f.write_str("journal activity ownership is unavailable")
            }
        }
    }
}

impl Error for ActivityAdmissionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unavailable(ActivityUnavailable::Root(error)) => Some(error),
            Self::Unavailable(ActivityUnavailable::Entry(error)) => Some(error),
            Self::Busy | Self::Unavailable(ActivityUnavailable::Poisoned) => None,
        }
    }
}

fn entry_error(error: ExistingParentLockError) -> ActivityAdmissionError {
    match error {
        ExistingParentLockError::Timeout(_) => ActivityAdmissionError::Busy,
        error => ActivityAdmissionError::Unavailable(ActivityUnavailable::Entry(error)),
    }
}

impl ActivityAdmissionError {
    pub(crate) fn into_io(self) -> std::io::Error {
        let kind = match &self {
            Self::Busy => std::io::ErrorKind::WouldBlock,
            Self::Unavailable(ActivityUnavailable::Entry(ExistingParentLockError::Io {
                source,
                ..
            }))
            | Self::Unavailable(ActivityUnavailable::Root(JournalRootError::Io {
                source, ..
            })) => source.kind(),
            Self::Unavailable(ActivityUnavailable::Root(JournalRootError::Unsupported {
                ..
            })) => std::io::ErrorKind::Unsupported,
            Self::Unavailable(ActivityUnavailable::Poisoned) => std::io::ErrorKind::Other,
            _ => std::io::ErrorKind::InvalidData,
        };
        std::io::Error::new(kind, self)
    }
}

struct Admission {
    root: Arc<JournalRoot>,
    // WindowsLockGuard is Send, not Sync. Only local validation borrows this
    // mutex; the actual native lock is retained for the whole ownership lifetime.
    guard: Mutex<BoundParentLock>,
}

impl Admission {
    fn acquire(root: Arc<JournalRoot>) -> Result<Self, ActivityAdmissionError> {
        root.revalidate_canonical_binding()
            .map_err(|e| ActivityAdmissionError::Unavailable(ActivityUnavailable::Root(e)))?;
        let guard = acquire_existing_parent_lock_bound(
            root.as_ref(),
            OsStr::new(ENTRY),
            Duration::ZERO,
            Duration::ZERO,
        )
        .map_err(entry_error)?;
        let admitted = Self {
            root,
            guard: Mutex::new(guard),
        };
        admitted.revalidate()?;
        Ok(admitted)
    }

    fn revalidate(&self) -> Result<(), ActivityAdmissionError> {
        self.root
            .revalidate_canonical_binding()
            .map_err(|e| ActivityAdmissionError::Unavailable(ActivityUnavailable::Root(e)))?;
        self.guard
            .lock()
            .map_err(|_| ActivityAdmissionError::Unavailable(ActivityUnavailable::Poisoned))?
            .revalidate_bound(self.root.as_ref(), OsStr::new(ENTRY))
            .map_err(entry_error)?;
        self.root
            .revalidate_canonical_binding()
            .map_err(|e| ActivityAdmissionError::Unavailable(ActivityUnavailable::Root(e)))
    }
}

/// Original admission for one cooperating activity and all its owned resources.
/// Cloning its Arc does not acquire another native lock or renew any budget.
pub struct ActivityLease {
    inner: Admission,
}

impl ActivityLease {
    pub fn try_acquire(root: Arc<JournalRoot>) -> Result<Arc<Self>, ActivityAdmissionError> {
        Ok(Arc::new(Self {
            inner: Admission::acquire(root)?,
        }))
    }

    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        &self.inner.root
    }

    pub fn revalidate(&self) -> Result<(), ActivityAdmissionError> {
        self.inner.revalidate()
    }
}

/// Exclusive admission for fresh checks and cooperating destructive boundaries.
/// Only its opaque root carrier may reserve an empty stage; it grants no
/// writable media stage and does not certify an empty census.
pub struct TeardownLease {
    inner: Admission,
}

impl TeardownLease {
    pub fn try_acquire(root: Arc<JournalRoot>) -> Result<Arc<Self>, ActivityAdmissionError> {
        Ok(Arc::new(Self {
            inner: Admission::acquire(root)?,
        }))
    }

    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        &self.inner.root
    }

    pub fn revalidate(&self) -> Result<(), ActivityAdmissionError> {
        self.inner.revalidate()
    }
}

#[cfg(all(test, feature = "full-tests"))]
mod tests;
