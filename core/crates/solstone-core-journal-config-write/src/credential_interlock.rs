// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Mechanical coordination with Brain's existing record sidecar. Configuration
//! owns only its config publication; all Brain record data stays with Brain.
//! Call exclusively after acquiring the config lock. Brain-only fallback reads
//! config without taking its lock, so the order remains config -> Brain.

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use serde_json::{Map, Value};
#[cfg(unix)]
use solstone_core_journal_io::FlatDirectory as HealthDirectory;
#[cfg(windows)]
use solstone_core_journal_io::WindowsFlatDirectory as HealthDirectory;
use solstone_core_journal_io::media::MediaDirectory;
use solstone_core_journal_io::{BoundParentLock, JournalRoot, LockOptions};

/// One budget for both sidecar waits, preserving the caller's existing limit.
pub(crate) struct PublicationBudget {
    deadline: Instant,
    options: LockOptions,
}

impl PublicationBudget {
    pub(crate) fn new(options: LockOptions) -> Self {
        Self {
            deadline: Instant::now() + options.timeout,
            options,
        }
    }
}

pub(crate) struct CredentialInterlock {
    root: Arc<JournalRoot>,
    directory: Arc<MediaDirectory>,
    parent: HealthDirectory,
    lock: BoundParentLock,
}

impl CredentialInterlock {
    pub(crate) fn checkpoint(&self) -> Result<(), String> {
        self.root
            .revalidate_canonical_binding()
            .map_err(|error| error.to_string())?;
        self.directory
            .revalidate()
            .map_err(|error| error.to_string())?;
        self.lock
            .revalidate_bound(&self.parent, OsStr::new("brain.json.lock"))
            .map_err(|error| error.to_string())
    }
}

/// This predicate is the shared coordination contract, not credential admission
/// or validation. Preserve model independence: model-only selection changes do
/// not create or settle an observation. Local configuration is compared whole
/// because endpoint completeness and confidential selection affect its lane.
fn changes_credential_authority(before: &Map<String, Value>, after: &Map<String, Value>) -> bool {
    let env = |config: &Map<String, Value>, field: &str| {
        config
            .get("env")
            .and_then(Value::as_object)
            .and_then(|env| env.get(field))
            .cloned()
    };
    let provider = |config: &Map<String, Value>, field: &str| {
        config
            .get("providers")
            .and_then(Value::as_object)
            .and_then(|providers| providers.get(field))
            .cloned()
    };
    let active = |config: &Map<String, Value>| {
        config
            .get("providers")
            .and_then(|providers| providers.get("active"))
            .and_then(|active| active.get("provider"))
            .cloned()
    };
    let confidential = |config: &Map<String, Value>| {
        config
            .get("services")
            .and_then(|services| services.get("confidential"))
            .cloned()
    };
    ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GOOGLE_API_KEY"]
        .into_iter()
        .any(|field| env(before, field) != env(after, field))
        || active(before) != active(after)
        || provider(before, "local") != provider(after, "local")
        || provider(before, "key_validation") != provider(after, "key_validation")
        || confidential(before) != confidential(after)
}

/// Preserve a legacy writer's existing root before its pathname lock/mutator.
/// Its historical missing-root first use is admitted just after that owner
/// creates the root, before arbitrary mutation. This does not upgrade legacy
/// pathname publication to the retained-parent API's stronger guarantee.
pub(crate) fn existing_root(journal: &Path) -> Result<Option<Arc<JournalRoot>>, io::Error> {
    let absolute = std::path::absolute(journal)?;
    match std::fs::symlink_metadata(&absolute) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    // Once an entry was observed, a failed root admission (including a later
    // disappearance) cannot be reinterpreted as a new first-use transaction.
    JournalRoot::open(&absolute)
        .map(|root| Some(Arc::new(root)))
        .map_err(io::Error::other)
}

pub(crate) fn created_root(
    journal: &Path,
    existing: Option<Arc<JournalRoot>>,
) -> Result<Arc<JournalRoot>, io::Error> {
    match existing {
        Some(root) => {
            root.revalidate_canonical_binding()
                .map_err(io::Error::other)?;
            Ok(root)
        }
        None => existing_root(journal)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "journal root was not created")),
    }
}

pub(crate) fn acquire(
    root: &Arc<JournalRoot>,
    before: &Map<String, Value>,
    after: &Map<String, Value>,
    budget: &PublicationBudget,
    checkpoint: &dyn Fn() -> Result<(), String>,
) -> Result<Option<CredentialInterlock>, io::Error> {
    if !changes_credential_authority(before, after) {
        return Ok(None);
    }
    let check = || {
        checkpoint()?;
        root.revalidate_canonical_binding()
            .map_err(|error| error.to_string())
    };
    check().map_err(io::Error::other)?;
    #[cfg(unix)]
    let parent = solstone_core_journal_io::create_or_open_flat_directory_bound(
        &**root,
        OsStr::new("health"),
        0o777,
        root.canonical_path(),
    );
    #[cfg(windows)]
    let parent = solstone_core_journal_io::create_or_open_windows_flat_directory_bound(
        &**root,
        OsStr::new("health"),
        root.canonical_path(),
    );
    let parent = parent.map_err(io::Error::other)?;
    let directory =
        MediaDirectory::open(root.clone(), Path::new("health")).map_err(io::Error::other)?;
    #[cfg(unix)]
    let same_parent = parent.identity() == directory.object_identity();
    #[cfg(windows)]
    let same_parent = parent.object_identity() == directory.object_identity();
    if !same_parent {
        return Err(io::Error::other(
            "credential interlock health parent changed",
        ));
    }
    let lock_checkpoint = || {
        check()?;
        directory.revalidate().map_err(|error| error.to_string())
    };
    // ZERO is the existing nonblocking lock contract: one uncontended attempt
    // is allowed. A positive budget exhausted by the first wait is not rearmed.
    let remaining = if budget.options.timeout.is_zero() {
        std::time::Duration::ZERO
    } else {
        budget
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "configuration lock budget exhausted",
                )
            })?
    };
    let lock = solstone_core_journal_io::acquire_existing_parent_lock_bound_checked(
        &parent,
        OsStr::new("brain.json.lock"),
        remaining,
        budget.options.poll_interval,
        &lock_checkpoint,
    )
    .map_err(io::Error::other)?;
    let guard = CredentialInterlock {
        root: root.clone(),
        directory,
        parent,
        lock,
    };
    guard.checkpoint().map_err(io::Error::other)?;
    Ok(Some(guard))
}
