// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Existing-only configuration replacement beneath one retained journal root.

use std::error::Error;
use std::fmt;
use std::io;
use std::sync::Arc;

use serde_json::{Map, Value};
use solstone_core_journal_config::ConfigLoadError;
use solstone_core_journal_io::{
    DetailedAtomicError, ExistingParentLockError, JournalRoot, LockOptions,
};

/// Maximum input and serialized replacement size for retained config updates.
pub const MAX_BOUND_CONFIG_BYTES: usize = 1024 * 1024;

/// Failure before publication. Native write failures retain stage cleanup facts.
#[derive(Debug)]
pub enum BoundConfigRefusal {
    Unsupported,
    Admission(Box<dyn Error + Send + Sync>),
    Load(ConfigLoadError),
    Missing,
    Conflict,
    Lock(ExistingParentLockError),
    Serialize(serde_json::Error),
    TooLarge,
    Write(DetailedAtomicError),
}

impl fmt::Display for BoundConfigRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Keep the pre-publication boundary explicit. In particular, never
        // format a decoded config or Native's byte-bearing FileObservation.
        match self {
            Self::Unsupported => formatter.write_str("retained config writes are unsupported"),
            Self::Admission(_) => formatter.write_str("retained config admission changed"),
            Self::Load(_) => formatter.write_str("retained config could not be read"),
            Self::Missing => formatter.write_str("retained config is missing"),
            Self::Conflict => formatter.write_str("retained config fingerprint changed"),
            Self::Lock(_) => formatter.write_str("retained config lock could not be acquired"),
            Self::Serialize(_) => formatter.write_str("retained config could not be serialized"),
            Self::TooLarge => formatter.write_str("retained config exceeds the byte limit"),
            Self::Write(error) => error.fmt(formatter),
        }
    }
}

impl Error for BoundConfigRefusal {}

/// An additional confirmation refusal after publication already happened.
#[derive(Debug)]
pub enum BoundConfigConfirmationRefusal {
    Admission(Box<dyn Error + Send + Sync>),
    Read(io::Error),
    Different,
}

/// Whether the parent directory entry of a publication was flushed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectoryFlush {
    Synced,
    /// The flush ran and failed; `durability_uncertainty` holds why.
    Failed,
    /// This platform has no directory flush (Windows), so none was attempted.
    Unavailable,
}

/// Independent facts after an actual retained-parent publication.
#[derive(Debug)]
pub struct BoundConfigPublication {
    /// Fingerprint of the actual serialized candidate, including its final newline.
    pub candidate_sha256: String,
    /// Metadata only. Configuration content is deliberately excluded. None only
    /// when the published file could not be observed again (Windows), in which
    /// case `confirmation_refusal` says why.
    pub published_file: Option<solstone_core_journal_io::FlatDirectoryEntry>,
    /// The full native identity of the published file, under the same rule.
    pub published_identity: Option<solstone_core_journal_io::media::MediaIdentity>,
    pub directory_flush: DirectoryFlush,
    pub durability_uncertainty: Option<io::Error>,
    pub observation_uncertainty: Option<io::Error>,
    pub confirmation_refusal: Option<BoundConfigConfirmationRefusal>,
}

/// A declined transformation or an actual publication, including uncertainty.
#[derive(Debug)]
pub enum BoundConfigOutcome {
    Unchanged,
    Published(BoundConfigPublication),
}

/// The normal config owner's locked transformation and actual publication facts.
#[derive(Debug)]
pub struct BoundConfigTransaction<T> {
    pub value: T,
    pub changed: bool,
    pub outcome: BoundConfigOutcome,
}

/// Normal config mutation through an admitted root, including first-use
/// defaults. Unlike the existing-only conditional API, this operation may
/// initialize the config leaf; it never creates or substitutes the journal root.
pub fn mutate_journal_config_at_root_checked<T>(
    root: &Arc<JournalRoot>,
    options: LockOptions,
    checkpoint: &dyn Fn() -> Result<(), String>,
    mutator: impl FnOnce(&mut Map<String, Value>) -> crate::JournalConfigMutation<T>,
) -> Result<BoundConfigTransaction<T>, BoundConfigRefusal> {
    mutate_journal_config_at_root_interlocked(root, options, checkpoint, &|| Ok(()), mutator)
}

/// Transition under both existing sidecars, immediately before native publication.
/// The callback must not recursively acquire either sidecar or perform provider work.
pub fn mutate_journal_config_at_root_interlocked<T>(
    root: &Arc<JournalRoot>,
    options: LockOptions,
    checkpoint: &dyn Fn() -> Result<(), String>,
    interlocked: &dyn Fn() -> Result<(), String>,
    mutator: impl FnOnce(&mut Map<String, Value>) -> crate::JournalConfigMutation<T>,
) -> Result<BoundConfigTransaction<T>, BoundConfigRefusal> {
    #[cfg(any(unix, windows))]
    {
        use std::ffi::OsStr;
        caller_checkpoint(checkpoint)?;
        root.revalidate_canonical_binding()
            .map_err(|error| BoundConfigRefusal::Admission(Box::new(error)))?;
        #[cfg(unix)]
        let parent = solstone_core_journal_io::create_or_open_flat_directory_bound(
            &**root,
            OsStr::new("config"),
            0o777,
            root.canonical_path(),
        );
        #[cfg(windows)]
        let parent = solstone_core_journal_io::create_or_open_windows_flat_directory_bound(
            &**root,
            OsStr::new("config"),
            root.canonical_path(),
        );
        let parent = parent.map_err(|error| BoundConfigRefusal::Admission(Box::new(error)))?;
        caller_checkpoint(checkpoint)?;
        root.revalidate_canonical_binding()
            .map_err(|error| BoundConfigRefusal::Admission(Box::new(error)))?;
        let directory = solstone_core_journal_io::media::MediaDirectory::open(
            root.clone(),
            std::path::Path::new("config"),
        )
        .map_err(|error| BoundConfigRefusal::Admission(Box::new(error)))?;
        #[cfg(unix)]
        let same_parent = parent.identity() == directory.object_identity();
        #[cfg(windows)]
        let same_parent = parent.object_identity() == directory.object_identity();
        if !same_parent {
            return Err(BoundConfigRefusal::Admission(Box::new(io::Error::other(
                "retained config parent changed",
            ))));
        }
        let result = std::cell::RefCell::new(None);
        let update = |config: &Map<String, Value>| {
            let mut config = config.clone();
            let mutation = mutator(&mut config);
            let changed = mutation.changed;
            *result.borrow_mut() = Some((mutation.value, changed));
            changed.then_some(config)
        };
        #[cfg(unix)]
        let outcome = unix::update(
            root,
            &directory,
            None,
            update,
            options,
            checkpoint,
            interlocked,
        )?;
        #[cfg(windows)]
        let outcome = windows::update(
            root,
            &directory,
            None,
            update,
            options,
            checkpoint,
            interlocked,
        )?;
        let (value, changed) = result
            .into_inner()
            .expect("successful config transaction transforms once");
        Ok(BoundConfigTransaction {
            value,
            changed,
            outcome,
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (root, options, checkpoint, interlocked, mutator);
        Err(BoundConfigRefusal::Unsupported)
    }
}

/// Conditionally transform one existing config using its original exact SHA.
///
/// The callback runs once on the locked strict map. `None` declines the update.
/// Only cooperating sidecar writers are serialized; this does not promise CAS
/// against arbitrary writers ignoring that lock. No missing parent is created,
/// no conflict is retried, and publication always targets the retained parent.
pub fn conditional_update_journal_config_at_root(
    root: &Arc<JournalRoot>,
    expected_sha256: &str,
    update: impl FnOnce(&Map<String, Value>) -> Option<Map<String, Value>>,
    options: LockOptions,
) -> Result<BoundConfigOutcome, BoundConfigRefusal> {
    conditional_update_journal_config_at_root_checked(
        root,
        expected_sha256,
        update,
        options,
        &|| Ok(()),
    )
}

/// Apply the existing retained-root update while renewing caller authority
/// during contention and immediately before native publication.
pub fn conditional_update_journal_config_at_root_checked(
    root: &Arc<JournalRoot>,
    expected_sha256: &str,
    update: impl FnOnce(&Map<String, Value>) -> Option<Map<String, Value>>,
    options: LockOptions,
    checkpoint: &dyn Fn() -> Result<(), String>,
) -> Result<BoundConfigOutcome, BoundConfigRefusal> {
    conditional_update_journal_config_at_root_interlocked(
        root,
        expected_sha256,
        update,
        options,
        checkpoint,
        &|| Ok(()),
    )
}

/// Transition under both existing sidecars, immediately before native publication.
/// The callback must not recursively acquire either sidecar or perform provider work.
pub fn conditional_update_journal_config_at_root_interlocked(
    root: &Arc<JournalRoot>,
    expected_sha256: &str,
    update: impl FnOnce(&Map<String, Value>) -> Option<Map<String, Value>>,
    options: LockOptions,
    checkpoint: &dyn Fn() -> Result<(), String>,
    interlocked: &dyn Fn() -> Result<(), String>,
) -> Result<BoundConfigOutcome, BoundConfigRefusal> {
    #[cfg(any(unix, windows))]
    {
        caller_checkpoint(checkpoint)?;
        root.revalidate_canonical_binding()
            .map_err(|error| BoundConfigRefusal::Admission(Box::new(error)))?;
        let directory = solstone_core_journal_io::media::MediaDirectory::open(
            root.clone(),
            std::path::Path::new("config"),
        )
        .map_err(|error| BoundConfigRefusal::Admission(Box::new(error)))?;
        conditional_update_journal_config_in_directory_interlocked(
            root,
            &directory,
            expected_sha256,
            update,
            options,
            checkpoint,
            interlocked,
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (
            root,
            expected_sha256,
            update,
            options,
            checkpoint,
            interlocked,
        );
        Err(BoundConfigRefusal::Unsupported)
    }
}

/// Apply the same locked update through the originally admitted config parent.
pub fn conditional_update_journal_config_in_directory(
    root: &Arc<JournalRoot>,
    directory: &solstone_core_journal_io::media::MediaDirectory,
    expected_sha256: &str,
    update: impl FnOnce(&Map<String, Value>) -> Option<Map<String, Value>>,
    options: LockOptions,
) -> Result<BoundConfigOutcome, BoundConfigRefusal> {
    conditional_update_journal_config_in_directory_checked(
        root,
        directory,
        expected_sha256,
        update,
        options,
        &|| Ok(()),
    )
}

/// Renew the caller's authority through the originally retained config parent.
pub fn conditional_update_journal_config_in_directory_checked(
    root: &Arc<JournalRoot>,
    directory: &solstone_core_journal_io::media::MediaDirectory,
    expected_sha256: &str,
    update: impl FnOnce(&Map<String, Value>) -> Option<Map<String, Value>>,
    options: LockOptions,
    checkpoint: &dyn Fn() -> Result<(), String>,
) -> Result<BoundConfigOutcome, BoundConfigRefusal> {
    conditional_update_journal_config_in_directory_interlocked(
        root,
        directory,
        expected_sha256,
        update,
        options,
        checkpoint,
        &|| Ok(()),
    )
}

/// Transition under both existing sidecars, immediately before native publication.
/// The callback must not recursively acquire either sidecar or perform provider work.
fn conditional_update_journal_config_in_directory_interlocked(
    root: &Arc<JournalRoot>,
    directory: &solstone_core_journal_io::media::MediaDirectory,
    expected_sha256: &str,
    update: impl FnOnce(&Map<String, Value>) -> Option<Map<String, Value>>,
    options: LockOptions,
    checkpoint: &dyn Fn() -> Result<(), String>,
    interlocked: &dyn Fn() -> Result<(), String>,
) -> Result<BoundConfigOutcome, BoundConfigRefusal> {
    #[cfg(unix)]
    {
        unix::update(
            root,
            directory,
            Some(expected_sha256),
            update,
            options,
            checkpoint,
            interlocked,
        )
    }
    #[cfg(windows)]
    {
        windows::update(
            root,
            directory,
            Some(expected_sha256),
            update,
            options,
            checkpoint,
            interlocked,
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (
            root,
            directory,
            expected_sha256,
            update,
            options,
            checkpoint,
            interlocked,
        );
        Err(BoundConfigRefusal::Unsupported)
    }
}

fn caller_checkpoint(
    checkpoint: &dyn Fn() -> Result<(), String>,
) -> Result<(), BoundConfigRefusal> {
    checkpoint().map_err(|detail| {
        BoundConfigRefusal::Admission(Box::new(io::Error::new(io::ErrorKind::Interrupted, detail)))
    })
}

#[cfg(unix)]
mod unix {
    use std::ffi::OsStr;
    use std::path::Path;

    use solstone_core_journal_config::{
        JournalConfigRead, fingerprint_journal_config_bytes, read_journal_config_in_directory,
    };
    use solstone_core_journal_io::media::{MediaDirectory, MediaIdentity};
    use solstone_core_journal_io::{
        BoundAtomicOutcome, FileObservation, FlatDirectory,
        acquire_existing_parent_lock_bound_checked, atomic_replace_bound_checked,
    };

    use super::*;

    pub(super) fn update(
        root: &Arc<JournalRoot>,
        directory: &MediaDirectory,
        expected: Option<&str>,
        update: impl FnOnce(&Map<String, Value>) -> Option<Map<String, Value>>,
        options: LockOptions,
        checkpoint: &dyn Fn() -> Result<(), String>,
        interlocked: &dyn Fn() -> Result<(), String>,
    ) -> Result<BoundConfigOutcome, BoundConfigRefusal> {
        let budget = crate::credential_interlock::PublicationBudget::new(options);
        caller_checkpoint(checkpoint)?;
        root.revalidate_canonical_binding().map_err(admission)?;
        directory.revalidate().map_err(admission)?;
        run_hook(Checkpoint::BeforeParentOpen);
        let parent = FlatDirectory::open(root, Path::new("config")).map_err(admission)?;
        run_hook(Checkpoint::AfterParentOpen);
        validate(root, directory, &parent).map_err(BoundConfigRefusal::Admission)?;
        let initial = read_journal_config_in_directory(directory, MAX_BOUND_CONFIG_BYTES)
            .map_err(BoundConfigRefusal::Load)?;
        require_expected(&initial, expected)?;
        run_hook(Checkpoint::AfterInitialRead);
        validate(root, directory, &parent).map_err(BoundConfigRefusal::Admission)?;
        let lock_checkpoint = || {
            validate(root, directory, &parent).map_err(|error| error.to_string())?;
            checkpoint()
        };
        let lock = acquire_existing_parent_lock_bound_checked(
            &parent,
            OsStr::new("journal.json.lock"),
            options.timeout,
            options.poll_interval,
            &lock_checkpoint,
        )
        .map_err(BoundConfigRefusal::Lock)?;
        validate(root, directory, &parent).map_err(BoundConfigRefusal::Admission)?;
        let locked = read_journal_config_in_directory(directory, MAX_BOUND_CONFIG_BYTES)
            .map_err(BoundConfigRefusal::Load)?;
        require_expected(&locked, expected)?;
        validate(root, directory, &parent).map_err(BoundConfigRefusal::Admission)?;
        let materialized = locked.config.is_none();
        let config = locked
            .config
            .unwrap_or_else(solstone_core_journal_config::materialized_defaults);
        caller_checkpoint(checkpoint)?;
        #[cfg(unix)]
        run_hook(Checkpoint::BeforeTransformation);
        // Even an unchanged transformation can have a caller-owned process
        // effect. Renew the root, parent, caller and original named guard
        // together immediately before that effect.
        caller_checkpoint(&lock_checkpoint)?;
        lock.revalidate_bound(&parent, OsStr::new("journal.json.lock"))
            .map_err(|error| BoundConfigRefusal::Admission(Box::new(error)))?;
        let replacement = match update(&config) {
            Some(replacement) => replacement,
            None if materialized => config.clone(),
            None => return Ok(BoundConfigOutcome::Unchanged),
        };
        let mut bytes =
            serde_json::to_vec_pretty(&replacement).map_err(BoundConfigRefusal::Serialize)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_BOUND_CONFIG_BYTES {
            return Err(BoundConfigRefusal::TooLarge);
        }
        let candidate_sha256 = fingerprint_journal_config_bytes(&bytes);
        let credential_interlock = crate::credential_interlock::acquire(
            root,
            &config,
            &replacement,
            &budget,
            &lock_checkpoint,
        )
        .map_err(admission)?;
        run_hook(Checkpoint::BeforePublication);
        validate(root, directory, &parent).map_err(BoundConfigRefusal::Admission)?;
        let publish_checkpoint = || {
            lock_checkpoint()?;
            if let Some(interlock) = &credential_interlock {
                interlock.checkpoint()?;
            }
            lock.revalidate_bound(&parent, OsStr::new("journal.json.lock"))
                .map_err(|error| error.to_string())
        };
        // The owner's transition requires the complete original C/B
        // authority now, before its effect; native publication renews it again.
        caller_checkpoint(&publish_checkpoint)?;
        if credential_interlock.is_some() {
            caller_checkpoint(interlocked)?;
        }
        let outcome = atomic_replace_bound_checked(
            &parent,
            OsStr::new("journal.json"),
            &bytes,
            0o600,
            &publish_checkpoint,
        )
        .map_err(BoundConfigRefusal::Write)?;
        let (observation, durability_uncertainty, observation_uncertainty) = match outcome {
            BoundAtomicOutcome::Published { observation } => (observation, None, None),
            BoundAtomicOutcome::PublishedDurabilityUncertain {
                observation,
                source,
            } => (observation, Some(source), None),
            BoundAtomicOutcome::PublishedObservationUncertain {
                observation,
                source,
                durability_source,
            } => (observation, durability_source, Some(source)),
        };
        run_hook(Checkpoint::BeforeConfirmation);
        let confirmation_refusal = confirm(root, directory, &parent, &observation, &bytes).err();
        let published_identity = MediaIdentity::Unix {
            device: observation.entry.device,
            inode: observation.entry.inode,
        };
        Ok(BoundConfigOutcome::Published(BoundConfigPublication {
            candidate_sha256,
            published_file: Some(observation.entry),
            published_identity: Some(published_identity),
            directory_flush: if durability_uncertainty.is_some() {
                DirectoryFlush::Failed
            } else {
                DirectoryFlush::Synced
            },
            durability_uncertainty,
            observation_uncertainty,
            confirmation_refusal,
        }))
    }

    fn require_expected(
        read: &JournalConfigRead,
        expected: Option<&str>,
    ) -> Result<(), BoundConfigRefusal> {
        let Some(expected) = expected else {
            return Ok(());
        };
        let actual = read.sha256.as_deref().ok_or(BoundConfigRefusal::Missing)?;
        if actual != expected {
            return Err(BoundConfigRefusal::Conflict);
        }
        Ok(())
    }

    fn admission(error: impl Error + Send + Sync + 'static) -> BoundConfigRefusal {
        BoundConfigRefusal::Admission(Box::new(error))
    }

    fn validate(
        root: &Arc<JournalRoot>,
        directory: &MediaDirectory,
        parent: &FlatDirectory,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        root.revalidate_canonical_binding()?;
        directory.revalidate()?;
        parent.revalidate()?;
        let current = FlatDirectory::open(root, Path::new("config"))?;
        if current.identity() != parent.identity()
            || directory.object_identity() != parent.identity()
        {
            return Err(io::Error::other("retained config parent changed").into());
        }
        root.revalidate_canonical_binding()?;
        Ok(())
    }

    fn confirm(
        root: &Arc<JournalRoot>,
        directory: &MediaDirectory,
        parent: &FlatDirectory,
        published: &FileObservation,
        bytes: &[u8],
    ) -> Result<(), BoundConfigConfirmationRefusal> {
        validate(root, directory, parent).map_err(BoundConfigConfirmationRefusal::Admission)?;
        let current = directory
            .read_file_bounded(OsStr::new("journal.json"), MAX_BOUND_CONFIG_BYTES)
            .map_err(BoundConfigConfirmationRefusal::Read)?
            .ok_or(BoundConfigConfirmationRefusal::Different)?;
        let identity = MediaIdentity::Unix {
            device: published.entry.device,
            inode: published.entry.inode,
        };
        if !current.complete
            || current.bytes != bytes
            || current.entry.identity != identity
            || current.entry.size != published.entry.size
        {
            return Err(BoundConfigConfirmationRefusal::Different);
        }
        validate(root, directory, parent).map_err(BoundConfigConfirmationRefusal::Admission)
    }
}

/// Windows publishes through the same retained parent and lock. It has no
/// directory flush, so a publication reports `DirectoryFlush::Unavailable`.
#[cfg(windows)]
mod windows {
    use std::ffi::OsStr;

    use solstone_core_journal_config::{
        JournalConfigRead, fingerprint_journal_config_bytes, read_journal_config_in_directory,
    };
    use solstone_core_journal_io::atomic::{
        BoundAtomicPublishError, atomic_replace_detailed_bound_checked,
    };
    use solstone_core_journal_io::media::MediaDirectory;
    use solstone_core_journal_io::{
        DetailedAtomicOutcome, WindowsFlatDirectory, acquire_existing_parent_lock_bound_checked,
        open_windows_flat_directory_bound, read_windows_observed_file_bounded,
    };

    use super::*;

    pub(super) fn update(
        root: &Arc<JournalRoot>,
        directory: &MediaDirectory,
        expected: Option<&str>,
        update: impl FnOnce(&Map<String, Value>) -> Option<Map<String, Value>>,
        options: LockOptions,
        checkpoint: &dyn Fn() -> Result<(), String>,
        interlocked: &dyn Fn() -> Result<(), String>,
    ) -> Result<BoundConfigOutcome, BoundConfigRefusal> {
        let budget = crate::credential_interlock::PublicationBudget::new(options);
        caller_checkpoint(checkpoint)?;
        root.revalidate_canonical_binding().map_err(admission)?;
        directory.revalidate().map_err(admission)?;
        let parent = open_parent(root).map_err(BoundConfigRefusal::Admission)?;
        validate(root, directory, &parent).map_err(BoundConfigRefusal::Admission)?;
        let initial = read_journal_config_in_directory(directory, MAX_BOUND_CONFIG_BYTES)
            .map_err(BoundConfigRefusal::Load)?;
        require_expected(&initial, expected)?;
        validate(root, directory, &parent).map_err(BoundConfigRefusal::Admission)?;
        let lock_checkpoint = || {
            validate(root, directory, &parent).map_err(|error| error.to_string())?;
            checkpoint()
        };
        let lock = acquire_existing_parent_lock_bound_checked(
            &parent,
            OsStr::new("journal.json.lock"),
            options.timeout,
            options.poll_interval,
            &lock_checkpoint,
        )
        .map_err(BoundConfigRefusal::Lock)?;
        validate(root, directory, &parent).map_err(BoundConfigRefusal::Admission)?;
        let locked = read_journal_config_in_directory(directory, MAX_BOUND_CONFIG_BYTES)
            .map_err(BoundConfigRefusal::Load)?;
        require_expected(&locked, expected)?;
        validate(root, directory, &parent).map_err(BoundConfigRefusal::Admission)?;
        let materialized = locked.config.is_none();
        let config = locked
            .config
            .unwrap_or_else(solstone_core_journal_config::materialized_defaults);
        caller_checkpoint(checkpoint)?;
        #[cfg(unix)]
        run_hook(Checkpoint::BeforeTransformation);
        // Even an unchanged transformation can have a caller-owned process
        // effect. Renew the root, parent, caller and original named guard
        // together immediately before that effect.
        caller_checkpoint(&lock_checkpoint)?;
        lock.revalidate_bound(&parent, OsStr::new("journal.json.lock"))
            .map_err(|error| BoundConfigRefusal::Admission(Box::new(error)))?;
        let replacement = match update(&config) {
            Some(replacement) => replacement,
            None if materialized => config.clone(),
            None => return Ok(BoundConfigOutcome::Unchanged),
        };
        let mut bytes =
            serde_json::to_vec_pretty(&replacement).map_err(BoundConfigRefusal::Serialize)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_BOUND_CONFIG_BYTES {
            return Err(BoundConfigRefusal::TooLarge);
        }
        let candidate_sha256 = fingerprint_journal_config_bytes(&bytes);
        let credential_interlock = crate::credential_interlock::acquire(
            root,
            &config,
            &replacement,
            &budget,
            &lock_checkpoint,
        )
        .map_err(admission)?;
        validate(root, directory, &parent).map_err(BoundConfigRefusal::Admission)?;
        let publish_checkpoint = || {
            lock_checkpoint()?;
            if let Some(interlock) = &credential_interlock {
                interlock.checkpoint()?;
            }
            lock.revalidate_bound(&parent, OsStr::new("journal.json.lock"))
                .map_err(|error| error.to_string())
        };
        caller_checkpoint(&publish_checkpoint)?;
        if credential_interlock.is_some() {
            caller_checkpoint(interlocked)?;
        }
        let outcome = atomic_replace_detailed_bound_checked(
            &parent,
            &lock,
            OsStr::new("journal.json"),
            &bytes,
            0o600,
            &publish_checkpoint,
        )
        .map_err(|error| match error {
            BoundAtomicPublishError::Atomic(error) => BoundConfigRefusal::Write(error),
            other => BoundConfigRefusal::Admission(Box::new(other)),
        })?;
        // Windows has no directory flush: `Published` here names a rename that
        // landed, never a synced parent.
        let (durability_uncertainty, path_uncertainty) = match outcome {
            DetailedAtomicOutcome::Published => (None, None),
            DetailedAtomicOutcome::PublishedDurabilityUncertain { source } => (Some(source), None),
            DetailedAtomicOutcome::PublishedParentPathRaced { sync_error } => (
                sync_error,
                Some(io::Error::other("retained config parent path changed")),
            ),
            DetailedAtomicOutcome::PublishedParentPathUnverified {
                observation,
                sync_error,
            } => (sync_error, Some(observation)),
        };
        let observed = (|| {
            validate(root, directory, &parent).map_err(io::Error::other)?;
            let current = directory
                .read_file_bounded(OsStr::new("journal.json"), MAX_BOUND_CONFIG_BYTES)?
                .ok_or_else(|| io::Error::other("published config is missing"))?;
            let flat = read_windows_observed_file_bounded(
                &parent,
                OsStr::new("journal.json"),
                MAX_BOUND_CONFIG_BYTES,
            )
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("published config is missing"))?;
            validate(root, directory, &parent).map_err(io::Error::other)?;
            Ok::<_, io::Error>((current, flat))
        })();
        // The rename landed: from here every outcome is a publication.
        let (published_file, published_identity, confirmation_refusal) = match observed {
            Ok((current, flat)) => {
                let refusal = (!current.complete
                    || current.bytes != bytes
                    || flat.bytes != bytes
                    || current.entry.size != flat.entry.size
                    || !current.entry.identity.matches_flat_entry(&flat.entry))
                .then_some(BoundConfigConfirmationRefusal::Different);
                (Some(flat.entry), Some(current.entry.identity), refusal)
            }
            Err(error) => (
                None,
                None,
                Some(BoundConfigConfirmationRefusal::Read(error)),
            ),
        };
        Ok(BoundConfigOutcome::Published(BoundConfigPublication {
            candidate_sha256,
            published_file,
            published_identity,
            directory_flush: if durability_uncertainty.is_some() {
                DirectoryFlush::Failed
            } else {
                DirectoryFlush::Unavailable
            },
            durability_uncertainty,
            observation_uncertainty: path_uncertainty,
            confirmation_refusal,
        }))
    }

    fn open_parent(
        root: &Arc<JournalRoot>,
    ) -> Result<WindowsFlatDirectory, Box<dyn Error + Send + Sync>> {
        open_windows_flat_directory_bound(&**root, OsStr::new("config"), root.canonical_path())?
            .ok_or_else(|| io::Error::other("retained config parent is missing").into())
    }

    fn require_expected(
        read: &JournalConfigRead,
        expected: Option<&str>,
    ) -> Result<(), BoundConfigRefusal> {
        let Some(expected) = expected else {
            return Ok(());
        };
        let actual = read.sha256.as_deref().ok_or(BoundConfigRefusal::Missing)?;
        if actual != expected {
            return Err(BoundConfigRefusal::Conflict);
        }
        Ok(())
    }

    fn admission(error: impl Error + Send + Sync + 'static) -> BoundConfigRefusal {
        BoundConfigRefusal::Admission(Box::new(error))
    }

    fn validate(
        root: &Arc<JournalRoot>,
        directory: &MediaDirectory,
        parent: &WindowsFlatDirectory,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        root.revalidate_canonical_binding()?;
        directory.revalidate()?;
        parent.revalidate_bound()?;
        let current = open_parent(root)?;
        if current.object_identity() != parent.object_identity()
            || directory.object_identity() != parent.object_identity()
        {
            return Err(io::Error::other("retained config parent changed").into());
        }
        root.revalidate_canonical_binding()?;
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum Checkpoint {
    BeforeParentOpen,
    AfterParentOpen,
    AfterInitialRead,
    BeforeTransformation,
    BeforePublication,
    BeforeConfirmation,
}

#[cfg(all(test, unix, feature = "full-tests"))]
type TestHook = (Checkpoint, Box<dyn FnOnce()>);

#[cfg(all(test, unix, feature = "full-tests"))]
thread_local! {
    static HOOK: std::cell::RefCell<Option<TestHook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(unix)]
fn run_hook(checkpoint: Checkpoint) {
    #[cfg(all(test, feature = "full-tests"))]
    {
        let hook = HOOK.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.as_ref().is_some_and(|(point, _)| *point == checkpoint) {
                slot.take()
            } else {
                None
            }
        });
        if let Some((_, callback)) = hook {
            callback();
        }
    }
    let _ = checkpoint;
}

#[cfg(unix)]
#[cfg(all(test, feature = "full-tests"))]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};
    use std::path::Path;
    use std::time::Duration;

    use serde_json::json;
    use solstone_core_journal_config::{
        fingerprint_journal_config_bytes, read_journal_config, read_journal_config_in_directory,
        read_journal_config_observed_in_directory,
    };
    use solstone_core_journal_io::media::MediaDirectory;
    use solstone_core_journal_io::{
        BoundPublicationPrimitive, hold_lock, is_publication_candidate_name,
        run_with_bound_publication_barrier, run_with_bound_publication_faults_and_barrier,
    };

    use super::*;

    fn fixture() -> (tempfile::TempDir, Arc<JournalRoot>, String) {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir(temporary.path().join("config")).unwrap();
        fs::write(
            temporary.path().join("config/journal.json"),
            b"{\"backup\":{\"enabled\":true},\"other\":7}\n",
        )
        .unwrap();
        let root = Arc::new(JournalRoot::open(temporary.path()).unwrap());
        let sha = read_journal_config(temporary.path())
            .unwrap()
            .sha256
            .unwrap();
        (temporary, root, sha)
    }

    fn replacement(config: &Map<String, Value>) -> Option<Map<String, Value>> {
        let mut config = config.clone();
        config.insert("backup".into(), json!({"enabled": false}));
        Some(config)
    }

    fn update(
        root: &Arc<JournalRoot>,
        sha: &str,
    ) -> Result<BoundConfigOutcome, BoundConfigRefusal> {
        conditional_update_journal_config_at_root(root, sha, replacement, LockOptions::default())
    }

    fn path(root: &Path) -> std::path::PathBuf {
        root.join("config/journal.json")
    }

    fn with_hook<T>(point: Checkpoint, hook: impl FnOnce() + 'static, op: impl FnOnce() -> T) -> T {
        HOOK.with(|slot| assert!(slot.replace(Some((point, Box::new(hook)))).is_none()));
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                HOOK.with(|slot| {
                    slot.borrow_mut().take();
                });
            }
        }
        let _guard = Guard;
        let result = op();
        HOOK.with(|slot| assert!(slot.borrow().is_none(), "requested barrier must run"));
        result
    }

    #[test]
    fn replaced_named_guard_refuses_before_read_only_transformation_effect() {
        let (temporary, root, sha) = fixture();
        let sidecar = temporary.path().join("config/journal.json.lock");
        let moved = temporary.path().join("held-original.lock");
        let before = fs::read(path(temporary.path())).unwrap();
        let effects = std::cell::Cell::new(0);
        let result = with_hook(
            Checkpoint::BeforeTransformation,
            move || {
                fs::rename(&sidecar, &moved).unwrap();
                fs::write(&sidecar, b"").unwrap();
            },
            || {
                conditional_update_journal_config_at_root_checked(
                    &root,
                    &sha,
                    |_| {
                        effects.set(effects.get() + 1);
                        None
                    },
                    LockOptions {
                        timeout: Duration::ZERO,
                        ..LockOptions::default()
                    },
                    &|| Ok(()),
                )
            },
        );
        assert!(result.is_err());
        assert_eq!(
            effects.get(),
            0,
            "a detached guard must never authorize process-state retirement"
        );
        assert_eq!(fs::read(path(temporary.path())).unwrap(), before);
    }

    #[test]
    fn replaced_root_or_parent_refuses_before_read_only_transformation_effect() {
        for replace_root in [false, true] {
            let temporary = tempfile::tempdir().unwrap();
            let journal = temporary.path().join("journal");
            fs::create_dir_all(journal.join("config")).unwrap();
            let before = b"{\"other\":7}\n";
            fs::write(path(&journal), before).unwrap();
            let root = Arc::new(JournalRoot::open(&journal).unwrap());
            let sha = read_journal_config(&journal).unwrap().sha256.unwrap();
            let replaced = if replace_root {
                journal.clone()
            } else {
                journal.join("config")
            };
            let retained = temporary.path().join("retained");
            let detached_config = if replace_root {
                retained.join("config")
            } else {
                retained.clone()
            };
            let replacement_journal = journal.clone();
            let effects = std::cell::Cell::new(0);
            let result = with_hook(
                Checkpoint::BeforeTransformation,
                move || {
                    fs::rename(&replaced, &retained).unwrap();
                    fs::create_dir_all(replacement_journal.join("config")).unwrap();
                    fs::write(path(&replacement_journal), before).unwrap();
                },
                || {
                    conditional_update_journal_config_at_root_checked(
                        &root,
                        &sha,
                        |_| {
                            effects.set(effects.get() + 1);
                            None
                        },
                        LockOptions {
                            timeout: Duration::ZERO,
                            ..LockOptions::default()
                        },
                        &|| Ok(()),
                    )
                },
            );
            assert!(matches!(result, Err(BoundConfigRefusal::Admission(_))));
            assert_eq!(
                effects.get(),
                0,
                "detached selectors cannot authorize retirement"
            );
            assert_eq!(fs::read(path(&journal)).unwrap(), before);
            assert_eq!(
                fs::read(detached_config.join("journal.json")).unwrap(),
                before
            );
            assert!(!journal.join("config/journal.json.lock").exists());
        }
    }

    #[test]
    fn normal_bound_mutation_materializes_defaults_and_preserves_unchanged_existing_bytes() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Arc::new(JournalRoot::open(temporary.path()).unwrap());
        let first = mutate_journal_config_at_root_checked(
            &root,
            LockOptions::default(),
            &|| Ok(()),
            |_| crate::JournalConfigMutation {
                changed: false,
                value: 42,
            },
        )
        .unwrap();
        assert_eq!(first.value, 42);
        assert!(!first.changed);
        assert!(matches!(first.outcome, BoundConfigOutcome::Published(_)));
        assert_eq!(
            read_journal_config(temporary.path())
                .unwrap()
                .config
                .unwrap(),
            solstone_core_journal_config::materialized_defaults()
        );
        assert_eq!(
            fs::metadata(path(temporary.path())).unwrap().mode() & 0o777,
            0o600
        );
        let before = fs::read(path(temporary.path())).unwrap();
        let second = mutate_journal_config_at_root_checked(
            &root,
            LockOptions::default(),
            &|| Ok(()),
            |_| crate::JournalConfigMutation {
                changed: false,
                value: 7,
            },
        )
        .unwrap();
        assert_eq!(second.value, 7);
        assert!(matches!(second.outcome, BoundConfigOutcome::Unchanged));
        assert_eq!(fs::read(path(temporary.path())).unwrap(), before);
    }

    #[test]
    fn first_use_bound_mutation_cancelled_at_rename_keeps_absence_and_can_retry() {
        use std::cell::Cell;
        use std::rc::Rc;
        let temporary = tempfile::tempdir().unwrap();
        let root = Arc::new(JournalRoot::open(temporary.path()).unwrap());
        let cancelled = Rc::new(Cell::new(false));
        let cut = cancelled.clone();
        let checkpoint = || {
            if cancelled.get() {
                Err("cancelled".to_owned())
            } else {
                Ok(())
            }
        };
        let (result, fired) = run_with_bound_publication_barrier(
            BoundPublicationPrimitive::Rename,
            1,
            move || cut.set(true),
            || {
                mutate_journal_config_at_root_checked(
                    &root,
                    LockOptions::default(),
                    &checkpoint,
                    |_| crate::JournalConfigMutation {
                        changed: false,
                        value: (),
                    },
                )
            },
        );
        assert!(fired);
        let Err(BoundConfigRefusal::Write(error)) = result else {
            panic!("rename must refuse");
        };
        assert_eq!(error.source.kind(), io::ErrorKind::Interrupted);
        assert!(error.orphan_stage.is_none());
        assert!(error.cleanup_error.is_none());
        assert!(!path(temporary.path()).exists());
        assert!(
            fs::read_dir(temporary.path().join("config"))
                .unwrap()
                .all(|entry| !is_publication_candidate_name(&entry.unwrap().file_name()))
        );
        cancelled.set(false);
        let retried = mutate_journal_config_at_root_checked(
            &root,
            LockOptions::default(),
            &checkpoint,
            |_| crate::JournalConfigMutation {
                changed: false,
                value: (),
            },
        )
        .unwrap();
        assert!(matches!(retried.outcome, BoundConfigOutcome::Published(_)));
    }

    #[test]
    fn retained_reader_preserves_exact_sha_and_refuses_invalid_symlink_and_oversize() {
        let (temporary, root, _) = fixture();
        let directory = MediaDirectory::open(root, Path::new("config")).unwrap();
        assert_eq!(
            read_journal_config_in_directory(&directory, 1024).unwrap(),
            read_journal_config(temporary.path()).unwrap()
        );
        fs::remove_file(path(temporary.path())).unwrap();
        assert_eq!(
            read_journal_config_in_directory(&directory, 1024).unwrap(),
            read_journal_config(temporary.path()).unwrap()
        );
        for bytes in [b"{".as_slice(), b"[]", &[b' '; 1025]] {
            fs::write(path(temporary.path()), bytes).unwrap();
            assert!(read_journal_config_in_directory(&directory, 1024).is_err());
        }
        fs::remove_file(path(temporary.path())).unwrap();
        fs::write(temporary.path().join("other.json"), b"{}").unwrap();
        symlink(temporary.path().join("other.json"), path(temporary.path())).unwrap();
        assert!(read_journal_config_in_directory(&directory, 1024).is_err());
    }

    #[test]
    fn matching_admission_publishes_private_pretty_config_and_retains_stable_lock() {
        let (temporary, root, sha) = fixture();
        let lock = hold_lock(
            path(temporary.path()),
            LockOptions {
                mode: Some(0o600),
                ..LockOptions::default()
            },
        )
        .unwrap();
        let sidecar = temporary.path().join("config/journal.json.lock");
        let identity = fs::metadata(&sidecar).unwrap().ino();
        drop(lock);
        let BoundConfigOutcome::Published(receipt) = update(&root, &sha).unwrap() else {
            panic!("must publish")
        };
        assert!(receipt.durability_uncertainty.is_none());
        assert!(receipt.observation_uncertainty.is_none());
        assert!(receipt.confirmation_refusal.is_none());
        let bytes = fs::read(path(temporary.path())).unwrap();
        assert!(bytes.ends_with(b"\n"));
        assert_eq!(
            receipt.candidate_sha256,
            fingerprint_journal_config_bytes(&bytes)
        );
        assert!(bytes.starts_with(b"{\n  "));
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"backup":{"enabled":false},"other":7})
        );
        assert_eq!(
            fs::metadata(path(temporary.path())).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::metadata(sidecar).unwrap().ino(), identity);
    }

    #[test]
    fn missing_corrupt_oversize_and_stale_original_admission_never_materialize_settings() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Arc::new(JournalRoot::open(temporary.path()).unwrap());
        assert!(update(&root, "sha256:missing").is_err());
        assert!(!temporary.path().join("config").exists());
        let removed = tempfile::tempdir().unwrap();
        let removed_root = Arc::new(JournalRoot::open(removed.path()).unwrap());
        fs::remove_dir(removed.path()).unwrap();
        assert!(matches!(
            update(&removed_root, "sha256:missing"),
            Err(BoundConfigRefusal::Admission(_))
        ));
        assert!(!removed.path().exists());
        fs::create_dir(temporary.path().join("config")).unwrap();
        assert!(matches!(
            update(&root, "sha256:missing"),
            Err(BoundConfigRefusal::Missing)
        ));
        assert!(!temporary.path().join("config/journal.json.lock").exists());
        for bytes in [b"{".to_vec(), vec![b' '; MAX_BOUND_CONFIG_BYTES + 1]] {
            fs::write(path(temporary.path()), &bytes).unwrap();
            assert!(matches!(
                update(&root, "sha256:missing"),
                Err(BoundConfigRefusal::Load(_))
            ));
            assert_eq!(fs::read(path(temporary.path())).unwrap(), bytes);
            assert!(!temporary.path().join("config/journal.json.lock").exists());
        }
        let (temporary, root, sha) = fixture();
        for bytes in [
            b"{\"backup\":{\"enabled\":true},\"other\":8}\n".as_slice(),
            b" {\"backup\":{\"enabled\":true},\"other\":7}\n",
        ] {
            fs::write(path(temporary.path()), bytes).unwrap();
            assert!(matches!(
                update(&root, &sha),
                Err(BoundConfigRefusal::Conflict)
            ));
            assert_eq!(fs::read(path(temporary.path())).unwrap(), bytes);
            assert!(!temporary.path().join("config/journal.json.lock").exists());
        }
    }

    #[test]
    fn declined_transform_and_serialized_expansion_refuse_publication() {
        let (temporary, root, sha) = fixture();
        let before = fs::read(path(temporary.path())).unwrap();
        assert!(matches!(
            conditional_update_journal_config_at_root(
                &root,
                &sha,
                |_| None,
                LockOptions::default()
            )
            .unwrap(),
            BoundConfigOutcome::Unchanged
        ));
        let oversized = |_: &Map<String, Value>| {
            Some(Map::from_iter([(
                "large".into(),
                json!("x".repeat(MAX_BOUND_CONFIG_BYTES)),
            )]))
        };
        assert!(matches!(
            conditional_update_journal_config_at_root(
                &root,
                &sha,
                oversized,
                LockOptions::default()
            ),
            Err(BoundConfigRefusal::TooLarge)
        ));
        let compact = json!({"many":vec![0; 200_000]});
        let bytes = serde_json::to_vec(&compact).unwrap();
        assert!(bytes.len() < MAX_BOUND_CONFIG_BYTES);
        fs::write(path(temporary.path()), &bytes).unwrap();
        let sha = read_journal_config(temporary.path())
            .unwrap()
            .sha256
            .unwrap();
        assert!(matches!(
            conditional_update_journal_config_at_root(
                &root,
                &sha,
                |map| Some(map.clone()),
                LockOptions::default()
            ),
            Err(BoundConfigRefusal::TooLarge)
        ));
        assert_eq!(fs::read(path(temporary.path())).unwrap(), bytes);
        assert_ne!(bytes, before);
        assert!(
            fs::read_dir(temporary.path().join("config"))
                .unwrap()
                .all(|entry| !is_publication_candidate_name(&entry.unwrap().file_name()))
        );
    }

    #[test]
    fn identical_root_or_config_parent_substitution_refuses_before_any_replacement_lock() {
        for point in [
            None,
            Some(Checkpoint::AfterInitialRead),
            Some(Checkpoint::BeforePublication),
        ] {
            let (temporary, root, sha) = fixture();
            let old_path = temporary.path().to_path_buf();
            let moved = old_path.with_extension("retained");
            let before = fs::read(path(&old_path)).unwrap();
            let replacement_bytes = before.clone();
            let swap = move || {
                fs::rename(&old_path, &moved).unwrap();
                fs::create_dir(&old_path).unwrap();
                fs::create_dir(old_path.join("config")).unwrap();
                fs::write(path(&old_path), replacement_bytes).unwrap();
            };
            let result = if let Some(point) = point {
                with_hook(point, swap, || update(&root, &sha))
            } else {
                swap();
                update(&root, &sha)
            };
            assert!(matches!(result, Err(BoundConfigRefusal::Admission(_))));
            assert_eq!(fs::read(path(temporary.path())).unwrap(), before);
            assert_eq!(
                fs::read(path(&temporary.path().with_extension("retained"))).unwrap(),
                before
            );
            assert!(!temporary.path().join("config/journal.json.lock").exists());
            fs::remove_dir_all(temporary.path().with_extension("retained")).unwrap();
        }
        let (temporary, root, sha) = fixture();
        let config = temporary.path().join("config");
        let moved = temporary.path().join("old-config");
        let before = fs::read(path(temporary.path())).unwrap();
        let other = before.clone();
        let result = with_hook(
            Checkpoint::AfterInitialRead,
            move || {
                fs::rename(&config, &moved).unwrap();
                fs::create_dir(&config).unwrap();
                fs::write(config.join("journal.json"), other).unwrap();
            },
            || update(&root, &sha),
        );
        assert!(matches!(result, Err(BoundConfigRefusal::Admission(_))));
        assert_eq!(fs::read(path(temporary.path())).unwrap(), before);
        assert_eq!(
            fs::read(temporary.path().join("old-config/journal.json")).unwrap(),
            before
        );
        assert!(!temporary.path().join("config/journal.json.lock").exists());
    }

    #[test]
    fn locked_reread_preserves_newer_bytes_and_real_path_lock_contention_is_honored() {
        let (temporary, root, sha) = fixture();
        let destination = path(temporary.path());
        let newer = b"{\"backup\":{\"enabled\":true},\"other\":9}";
        let result = with_hook(
            Checkpoint::AfterInitialRead,
            move || fs::write(destination, newer).unwrap(),
            || update(&root, &sha),
        );
        assert!(matches!(result, Err(BoundConfigRefusal::Conflict)));
        assert_eq!(fs::read(path(temporary.path())).unwrap(), newer);
        let (temporary, root, sha) = fixture();
        let before = fs::read(path(temporary.path())).unwrap();
        let _lock = hold_lock(
            path(temporary.path()),
            LockOptions {
                mode: Some(0o600),
                ..LockOptions::default()
            },
        )
        .unwrap();
        let options = LockOptions {
            timeout: Duration::ZERO,
            ..LockOptions::default()
        };
        let result = conditional_update_journal_config_at_root(
            &root,
            &sha,
            |_| panic!("must not transform without lock"),
            options,
        );
        assert!(matches!(
            result,
            Err(BoundConfigRefusal::Lock(ExistingParentLockError::Timeout(
                _
            )))
        ));
        assert_eq!(fs::read(path(temporary.path())).unwrap(), before);
    }

    #[test]
    fn supplied_original_parent_refuses_permanent_and_temporary_identical_substitution() {
        use std::cell::Cell;
        use std::rc::Rc;

        for temporary_swap in [false, true] {
            let (temp, root, sha) = fixture();
            let directory = MediaDirectory::open(root.clone(), Path::new("config")).unwrap();
            let original = temp.path().join("config");
            let moved = temp.path().join("old-config");
            let replacement_parent = temp.path().join("replacement-config");
            let bytes = fs::read(path(temp.path())).unwrap();
            let _lock = hold_lock(
                path(temp.path()),
                LockOptions {
                    mode: Some(0o600),
                    ..LockOptions::default()
                },
            )
            .unwrap();
            let lock_identity = fs::metadata(original.join("journal.json.lock"))
                .unwrap()
                .ino();
            drop(_lock);
            let fired = Rc::new(Cell::new(0));
            let observed = fired.clone();
            let swap = move || {
                fs::rename(&original, &moved).unwrap();
                fs::create_dir(&original).unwrap();
                fs::write(original.join("journal.json"), bytes).unwrap();
                observed.set(observed.get() + 1);
                if temporary_swap {
                    let hook = move || {
                        fs::rename(&original, &replacement_parent).unwrap();
                        fs::rename(&moved, &original).unwrap();
                        observed.set(observed.get() + 1);
                    };
                    HOOK.with(|slot| {
                        assert!(
                            slot.replace(Some((Checkpoint::AfterParentOpen, Box::new(hook))))
                                .is_none()
                        )
                    });
                }
            };
            let result = if temporary_swap {
                with_hook(Checkpoint::BeforeParentOpen, swap, || {
                    conditional_update_journal_config_in_directory(
                        &root,
                        &directory,
                        &sha,
                        replacement,
                        LockOptions::default(),
                    )
                })
            } else {
                swap();
                conditional_update_journal_config_in_directory(
                    &root,
                    &directory,
                    &sha,
                    replacement,
                    LockOptions::default(),
                )
            };
            assert!(matches!(result, Err(BoundConfigRefusal::Admission(_))));
            assert_eq!(fired.get(), if temporary_swap { 2 } else { 1 });
            let original_parent = temp.path().join(if temporary_swap {
                "config"
            } else {
                "old-config"
            });
            let replacement_parent = temp.path().join(if temporary_swap {
                "replacement-config"
            } else {
                "config"
            });
            assert_eq!(
                fs::metadata(original_parent.join("journal.json.lock"))
                    .unwrap()
                    .ino(),
                lock_identity
            );
            assert_eq!(
                fs::read(original_parent.join("journal.json")).unwrap(),
                fs::read(replacement_parent.join("journal.json")).unwrap()
            );
            assert!(!replacement_parent.join("journal.json.lock").exists());
            assert_eq!(fs::read_dir(replacement_parent).unwrap().count(), 1);
        }
    }

    #[test]
    fn supplied_parent_uses_same_existing_sidecar_under_zero_timeout_contention() {
        let (temp, root, sha) = fixture();
        let directory = MediaDirectory::open(root.clone(), Path::new("config")).unwrap();
        let before = fs::read(path(temp.path())).unwrap();
        let _lock = hold_lock(
            path(temp.path()),
            LockOptions {
                mode: Some(0o600),
                ..LockOptions::default()
            },
        )
        .unwrap();
        let sidecar = temp.path().join("config/journal.json.lock");
        let identity = fs::metadata(&sidecar).unwrap().ino();
        let result = conditional_update_journal_config_in_directory(
            &root,
            &directory,
            &sha,
            |_| panic!("contended writer cannot transform"),
            LockOptions {
                timeout: Duration::ZERO,
                ..LockOptions::default()
            },
        );
        assert!(matches!(
            result,
            Err(BoundConfigRefusal::Lock(ExistingParentLockError::Timeout(
                _
            )))
        ));
        assert_eq!(fs::metadata(sidecar).unwrap().ino(), identity);
        assert_eq!(fs::read(path(temp.path())).unwrap(), before);
    }

    #[test]
    fn observed_reader_returns_one_complete_buffer_and_refuses_native_late_mutation() {
        use solstone_core_journal_io::media::{MediaIdentity, MediaPrimitive, run_with_media_hook};

        let (temp, root, sha) = fixture();
        let directory = MediaDirectory::open(root, Path::new("config")).unwrap();
        let (read, entry) = read_journal_config_observed_in_directory(&directory, 1024).unwrap();
        let entry = entry.unwrap();
        let metadata = fs::metadata(path(temp.path())).unwrap();
        assert_eq!(read.sha256.as_deref(), Some(sha.as_str()));
        assert_eq!(
            entry.identity,
            MediaIdentity::Unix {
                device: metadata.dev(),
                inode: metadata.ino()
            }
        );
        let changed = path(temp.path());
        let (result, fired) = run_with_media_hook(
            MediaPrimitive::AfterPrefixRead,
            move || fs::write(changed, b"{}"),
            || read_journal_config_observed_in_directory(&directory, 1024),
        );
        assert!(fired);
        assert!(result.is_err());
        fs::remove_file(path(temp.path())).unwrap();
        let (missing, entry) = read_journal_config_observed_in_directory(&directory, 1024).unwrap();
        assert!(!missing.present);
        assert!(entry.is_none());
        fs::write(path(temp.path()), vec![b' '; 1025]).unwrap();
        assert!(read_journal_config_observed_in_directory(&directory, 1024).is_err());
    }

    #[test]
    fn checked_caller_refusal_at_native_publication_preserves_bytes_and_allows_retry() {
        use std::cell::Cell;
        use std::rc::Rc;

        for primitive in [
            BoundPublicationPrimitive::TempCreate,
            BoundPublicationPrimitive::FileSync,
            BoundPublicationPrimitive::Rename,
        ] {
            let (temporary, root, sha) = fixture();
            let before = fs::read(path(temporary.path())).unwrap();
            let cancelled = Rc::new(Cell::new(false));
            let cut = cancelled.clone();
            let checkpoint = || {
                if cancelled.get() {
                    Err("caller cancelled".to_owned())
                } else {
                    Ok(())
                }
            };
            let config = temporary.path().join("config");
            let (result, fired) = run_with_bound_publication_barrier(
                primitive,
                1,
                move || {
                    if primitive == BoundPublicationPrimitive::Rename {
                        assert!(fs::read_dir(&config).unwrap().any(|entry| {
                            is_publication_candidate_name(&entry.unwrap().file_name())
                        }));
                    }
                    cut.set(true);
                },
                || {
                    conditional_update_journal_config_at_root_checked(
                        &root,
                        &sha,
                        replacement,
                        LockOptions::default(),
                        &checkpoint,
                    )
                },
            );
            assert!(fired);
            let Err(BoundConfigRefusal::Write(error)) = result else {
                panic!("caller refusal must prevent native publication");
            };
            assert_eq!(error.source.kind(), io::ErrorKind::Interrupted);
            assert!(error.orphan_stage.is_none());
            assert!(error.cleanup_error.is_none());
            assert_eq!(fs::read(path(temporary.path())).unwrap(), before);
            assert!(
                fs::read_dir(temporary.path().join("config"))
                    .unwrap()
                    .all(|entry| !is_publication_candidate_name(&entry.unwrap().file_name()))
            );

            cancelled.set(false);
            let BoundConfigOutcome::Published(receipt) =
                conditional_update_journal_config_at_root_checked(
                    &root,
                    &sha,
                    replacement,
                    LockOptions::default(),
                    &checkpoint,
                )
                .unwrap()
            else {
                panic!("unchanged original fingerprint must permit a normal retry");
            };
            assert!(receipt.durability_uncertainty.is_none());
            assert!(receipt.observation_uncertainty.is_none());
            assert!(receipt.confirmation_refusal.is_none());
            assert_eq!(
                read_journal_config(temporary.path())
                    .unwrap()
                    .config
                    .unwrap(),
                json!({"backup":{"enabled":false},"other":7})
                    .as_object()
                    .unwrap()
                    .clone()
            );
        }
    }

    #[test]
    fn final_native_rename_refuses_replaced_root_without_mutating_either_journal() {
        let (temporary, root, sha) = fixture();
        let original = temporary.path().to_path_buf();
        let moved = original.with_extension("retained");
        let before = fs::read(path(&original)).unwrap();
        let other = before.clone();
        let (result, fired) = run_with_bound_publication_barrier(
            BoundPublicationPrimitive::Rename,
            1,
            move || {
                fs::rename(&original, &moved).unwrap();
                fs::create_dir(&original).unwrap();
                fs::create_dir(original.join("config")).unwrap();
                fs::write(path(&original), other).unwrap();
            },
            || update(&root, &sha),
        );
        assert!(fired);
        let Err(BoundConfigRefusal::Write(error)) = result else {
            panic!("replaced root must refuse before native rename");
        };
        assert_eq!(error.source.kind(), io::ErrorKind::Interrupted);
        assert!(error.orphan_stage.is_none());
        assert!(error.cleanup_error.is_none());
        let moved = temporary.path().with_extension("retained");
        assert_eq!(fs::read(path(temporary.path())).unwrap(), before);
        assert_eq!(fs::read(path(&moved)).unwrap(), before);
        assert_eq!(
            fs::read_dir(temporary.path().join("config"))
                .unwrap()
                .count(),
            1
        );
        assert!(
            fs::read_dir(moved.join("config"))
                .unwrap()
                .all(|entry| !is_publication_candidate_name(&entry.unwrap().file_name()))
        );
        fs::remove_dir_all(moved).unwrap();
    }

    #[test]
    fn final_native_rename_refuses_replaced_sidecar_and_cleans_stage() {
        let (temporary, root, sha) = fixture();
        let before = fs::read(path(temporary.path())).unwrap();
        let sidecar = temporary.path().join("config/journal.json.lock");
        let retired_sidecar = temporary.path().join("config/retired.lock");
        let replacement_sidecar = temporary.path().join("config/replacement.lock");
        use std::os::unix::fs::PermissionsExt;
        fs::write(&replacement_sidecar, b"").unwrap();
        fs::set_permissions(&replacement_sidecar, fs::Permissions::from_mode(0o600)).unwrap();
        let replacement_identity = fs::metadata(&replacement_sidecar).unwrap().ino();
        let old_sidecar = sidecar.clone();
        let retired = retired_sidecar.clone();
        let (result, fired) = run_with_bound_publication_barrier(
            BoundPublicationPrimitive::Rename,
            1,
            move || {
                fs::rename(&old_sidecar, retired).unwrap();
                fs::rename(replacement_sidecar, old_sidecar).unwrap();
            },
            || update(&root, &sha),
        );
        assert!(fired);
        let Err(BoundConfigRefusal::Write(error)) = result else {
            panic!("lost sidecar authority must prevent native rename");
        };
        assert_eq!(error.source.kind(), io::ErrorKind::Interrupted);
        assert!(error.orphan_stage.is_none());
        assert!(error.cleanup_error.is_none());
        assert_eq!(fs::read(path(temporary.path())).unwrap(), before);
        assert_eq!(fs::metadata(&sidecar).unwrap().ino(), replacement_identity);
        assert_ne!(
            fs::metadata(&retired_sidecar).unwrap().ino(),
            replacement_identity
        );
        assert!(
            fs::read_dir(temporary.path().join("config"))
                .unwrap()
                .all(|entry| !is_publication_candidate_name(&entry.unwrap().file_name()))
        );
        assert!(matches!(
            update(&root, &sha).unwrap(),
            BoundConfigOutcome::Published(_)
        ));
    }

    #[test]
    fn post_rename_root_change_preserves_actual_publication_receipt() {
        let (temporary, root, sha) = fixture();
        let original = temporary.path().to_path_buf();
        let moved = original.with_extension("retained");
        let before = fs::read(path(&original)).unwrap();
        let other = before.clone();
        let (result, fired) = run_with_bound_publication_barrier(
            BoundPublicationPrimitive::ParentSync,
            1,
            move || {
                fs::rename(&original, &moved).unwrap();
                fs::create_dir(&original).unwrap();
                fs::create_dir(original.join("config")).unwrap();
                fs::write(path(&original), other).unwrap();
            },
            || update(&root, &sha),
        );
        assert!(fired);
        let BoundConfigOutcome::Published(receipt) = result.unwrap() else {
            panic!("rename landed")
        };
        assert!(receipt.confirmation_refusal.is_some());
        assert_eq!(fs::read(path(temporary.path())).unwrap(), before);
        let moved = temporary.path().with_extension("retained");
        assert_eq!(
            read_journal_config(&moved).unwrap().config.unwrap()["backup"]["enabled"],
            false
        );
        assert!(!temporary.path().join("config/journal.json.lock").exists());
        fs::remove_dir_all(moved).unwrap();
    }

    #[test]
    fn valid_different_confirmation_keeps_publication_and_native_prepublication_cleanup_facts() {
        let (temporary, root, sha) = fixture();
        let destination = path(temporary.path());
        let changed = b"{\"valid\":\"newer\"}";
        let result = with_hook(
            Checkpoint::BeforeConfirmation,
            move || fs::write(destination, changed).unwrap(),
            || update(&root, &sha),
        );
        let BoundConfigOutcome::Published(receipt) = result.unwrap() else {
            panic!("rename landed")
        };
        assert!(receipt.confirmation_refusal.is_some());
        assert_eq!(fs::read(path(temporary.path())).unwrap(), changed);
        let (temporary, root, sha) = fixture();
        let destination = path(temporary.path());
        let substitute = temporary.path().join("substitute.json");
        fs::write(&substitute, b"{}").unwrap();
        let result = with_hook(
            Checkpoint::BeforeConfirmation,
            move || {
                fs::remove_file(&destination).unwrap();
                symlink(substitute, destination).unwrap();
            },
            || update(&root, &sha),
        );
        let BoundConfigOutcome::Published(receipt) = result.unwrap() else {
            panic!("rename landed before confirmation read failed")
        };
        assert!(matches!(
            receipt.confirmation_refusal,
            Some(BoundConfigConfirmationRefusal::Read(_))
        ));
        assert!(
            fs::symlink_metadata(path(temporary.path()))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let (temporary, root, sha) = fixture();
        let before = fs::read(path(temporary.path())).unwrap();
        let directory = temporary.path().join("config");
        let (result, consumed, fired) = run_with_bound_publication_faults_and_barrier(
            [(BoundPublicationPrimitive::Rename, 1, 5)],
            BoundPublicationPrimitive::FileSync,
            1,
            move || {
                let stage = fs::read_dir(directory)
                    .unwrap()
                    .map(|entry| entry.unwrap())
                    .find(|entry| is_publication_candidate_name(&entry.file_name()))
                    .unwrap()
                    .path();
                fs::remove_file(&stage).unwrap();
                fs::create_dir(stage).unwrap();
            },
            || update(&root, &sha),
        );
        assert_eq!(consumed, 1);
        assert!(fired);
        let Err(BoundConfigRefusal::Write(error)) = result else {
            panic!("must preserve native failure")
        };
        assert!(error.orphan_stage.is_some());
        assert!(error.cleanup_error.is_some());
        assert_eq!(fs::read(path(temporary.path())).unwrap(), before);
        assert!(
            temporary
                .path()
                .join("config")
                .join(error.orphan_stage.unwrap())
                .is_dir()
        );
    }
}

#[cfg(all(test, feature = "full-tests"))]
#[cfg(windows)]
mod windows_tests {
    use std::fs;

    use serde_json::json;
    use solstone_core_journal_config::read_journal_config_observed_in_directory;
    use solstone_core_journal_io::media::MediaDirectory;

    use super::*;

    fn fixture() -> (
        tempfile::TempDir,
        Arc<JournalRoot>,
        Arc<MediaDirectory>,
        String,
    ) {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("config")).unwrap();
        fs::write(
            temp.path().join("config/journal.json"),
            br#"{"backup":{"enabled":true},"other":7}"#,
        )
        .unwrap();
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        let directory = MediaDirectory::open(root.clone(), std::path::Path::new("config")).unwrap();
        let (read, _) =
            read_journal_config_observed_in_directory(&directory, MAX_BOUND_CONFIG_BYTES).unwrap();
        (temp, root, directory, read.sha256.unwrap())
    }

    #[test]
    fn windows_publishes_unflushed_through_the_retained_parent_and_confirms_it() {
        let (temp, root, directory, sha) = fixture();
        let outcome = conditional_update_journal_config_in_directory(
            &root,
            &directory,
            &sha,
            |config| {
                let mut config = config.clone();
                config.insert("backup".into(), json!({"enabled": false}));
                Some(config)
            },
            LockOptions::default(),
        )
        .unwrap();
        let BoundConfigOutcome::Published(publication) = outcome else {
            panic!("expected a publication");
        };
        assert_eq!(publication.directory_flush, DirectoryFlush::Unavailable);
        assert!(publication.durability_uncertainty.is_none());
        assert!(publication.observation_uncertainty.is_none());
        assert!(publication.confirmation_refusal.is_none());
        let (read, entry) =
            read_journal_config_observed_in_directory(&directory, MAX_BOUND_CONFIG_BYTES).unwrap();
        assert_eq!(
            read.sha256.as_deref(),
            Some(publication.candidate_sha256.as_str())
        );
        let entry = entry.unwrap();
        assert_eq!(Some(entry.identity), publication.published_identity);
        assert!(
            publication
                .published_identity
                .unwrap()
                .matches_flat_entry(publication.published_file.as_ref().unwrap())
        );
        let written = fs::read_to_string(temp.path().join("config/journal.json")).unwrap();
        assert!(written.contains("\"other\": 7") && written.contains("\"enabled\": false"));
    }

    #[test]
    fn windows_refuses_a_changed_fingerprint_without_writing() {
        let (temp, root, directory, _) = fixture();
        let before = fs::read(temp.path().join("config/journal.json")).unwrap();
        let refused = conditional_update_journal_config_in_directory(
            &root,
            &directory,
            "sha256:0000",
            |config| Some(config.clone()),
            LockOptions::default(),
        );
        assert!(matches!(refused, Err(BoundConfigRefusal::Conflict)));
        assert_eq!(
            fs::read(temp.path().join("config/journal.json")).unwrap(),
            before
        );
    }
}

#[cfg(all(test, feature = "full-tests", any(unix, windows)))]
mod credential_publication_tests {
    use super::*;
    use serde_json::json;
    use solstone_core_journal_io::{LockError, hold_lock};
    use std::cell::Cell;
    use std::fs;
    use std::time::Duration;

    #[test]
    fn credential_transition_runs_once_under_both_existing_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        let calls = Cell::new(0);
        let hook = || {
            calls.set(calls.get() + 1);
            for relative in ["config/journal.json", "health/brain.json"] {
                let attempt = hold_lock(
                    temp.path().join(relative),
                    LockOptions {
                        timeout: Duration::ZERO,
                        ..LockOptions::default()
                    },
                );
                assert!(
                    matches!(attempt, Err(LockError::Timeout(_))),
                    "{relative} must already be locked"
                );
            }
            Ok(())
        };
        let transaction = mutate_journal_config_at_root_interlocked(
            &root,
            LockOptions::default(),
            &|| Ok(()),
            &hook,
            |config| {
                config.insert(
                    "env".to_owned(),
                    json!({"OPENAI_API_KEY":"synthetic-locked-transition"}),
                );
                crate::JournalConfigMutation {
                    changed: true,
                    value: (),
                }
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert!(matches!(
            transaction.outcome,
            BoundConfigOutcome::Published(_)
        ));
        assert!(!temp.path().join("health/brain.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn substituted_config_sidecar_refuses_before_any_credential_transition() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("config")).unwrap();
        let path = temp.path().join("config/journal.json");
        let before = b"{\"env\":{}}\n";
        fs::write(&path, before).unwrap();
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        let calls = Cell::new(0);
        let result = mutate_journal_config_at_root_interlocked(
            &root,
            LockOptions::default(),
            &|| Ok(()),
            &|| {
                calls.set(calls.get() + 1);
                Ok(())
            },
            |config| {
                let sidecar = temp.path().join("config/journal.json.lock");
                fs::rename(&sidecar, temp.path().join("config/detached-original.lock")).unwrap();
                fs::write(sidecar, b"").unwrap();
                config.insert(
                    "env".to_owned(),
                    json!({"OPENAI_API_KEY":"synthetic-sidecar-substitution"}),
                );
                crate::JournalConfigMutation {
                    changed: true,
                    value: (),
                }
            },
        );
        assert!(result.is_err());
        assert_eq!(
            calls.get(),
            0,
            "detached config guard must not authorize the transition"
        );
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn refused_locked_transition_keeps_original_config_bytes() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("config")).unwrap();
        let path = temp.path().join("config/journal.json");
        let before = b"{\"env\":{}}\n";
        fs::write(&path, before).unwrap();
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        let calls = Cell::new(0);
        let result = mutate_journal_config_at_root_interlocked(
            &root,
            LockOptions::default(),
            &|| Ok(()),
            &|| {
                calls.set(calls.get() + 1);
                Err("superseded before accepted transition".to_owned())
            },
            |config| {
                config.insert(
                    "env".to_owned(),
                    json!({"OPENAI_API_KEY":"synthetic-refused"}),
                );
                crate::JournalConfigMutation {
                    changed: true,
                    value: (),
                }
            },
        );
        assert!(matches!(result, Err(BoundConfigRefusal::Admission(_))));
        assert_eq!(calls.get(), 1);
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn unrelated_mutation_has_no_credential_transition_or_brain_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        mutate_journal_config_at_root_interlocked(
            &root,
            LockOptions::default(),
            &|| Ok(()),
            &|| {
                panic!("unrelated settings must not gain credential transition dependency");
            },
            |config| {
                config.insert("unrelated-synthetic-field".to_owned(), json!(true));
                crate::JournalConfigMutation {
                    changed: true,
                    value: (),
                }
            },
        )
        .unwrap();
        assert!(!temp.path().join("health").exists());
    }
}
