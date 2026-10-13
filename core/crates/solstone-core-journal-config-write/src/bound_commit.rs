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
    #[cfg(any(unix, windows))]
    {
        root.revalidate_canonical_binding()
            .map_err(|error| BoundConfigRefusal::Admission(Box::new(error)))?;
        let directory = solstone_core_journal_io::media::MediaDirectory::open(
            root.clone(),
            std::path::Path::new("config"),
        )
        .map_err(|error| BoundConfigRefusal::Admission(Box::new(error)))?;
        conditional_update_journal_config_in_directory(
            root,
            &directory,
            expected_sha256,
            update,
            options,
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (root, expected_sha256, update, options);
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
    #[cfg(unix)]
    {
        unix::update(root, directory, expected_sha256, update, options)
    }
    #[cfg(windows)]
    {
        windows::update(root, directory, expected_sha256, update, options)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (root, directory, expected_sha256, update, options);
        Err(BoundConfigRefusal::Unsupported)
    }
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
        BoundAtomicOutcome, FileObservation, FlatDirectory, acquire_existing_parent_lock_bound,
        atomic_replace_bound,
    };

    use super::*;

    pub(super) fn update(
        root: &Arc<JournalRoot>,
        directory: &MediaDirectory,
        expected: &str,
        update: impl FnOnce(&Map<String, Value>) -> Option<Map<String, Value>>,
        options: LockOptions,
    ) -> Result<BoundConfigOutcome, BoundConfigRefusal> {
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
        let _lock = acquire_existing_parent_lock_bound(
            &parent,
            OsStr::new("journal.json.lock"),
            options.timeout,
            options.poll_interval,
        )
        .map_err(BoundConfigRefusal::Lock)?;
        validate(root, directory, &parent).map_err(BoundConfigRefusal::Admission)?;
        let locked = read_journal_config_in_directory(directory, MAX_BOUND_CONFIG_BYTES)
            .map_err(BoundConfigRefusal::Load)?;
        require_expected(&locked, expected)?;
        validate(root, directory, &parent).map_err(BoundConfigRefusal::Admission)?;
        let config = locked.config.as_ref().ok_or(BoundConfigRefusal::Missing)?;
        let Some(replacement) = update(config) else {
            return Ok(BoundConfigOutcome::Unchanged);
        };
        let mut bytes =
            serde_json::to_vec_pretty(&replacement).map_err(BoundConfigRefusal::Serialize)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_BOUND_CONFIG_BYTES {
            return Err(BoundConfigRefusal::TooLarge);
        }
        let candidate_sha256 = fingerprint_journal_config_bytes(&bytes);
        run_hook(Checkpoint::BeforePublication);
        validate(root, directory, &parent).map_err(BoundConfigRefusal::Admission)?;
        let outcome = atomic_replace_bound(&parent, OsStr::new("journal.json"), &bytes, 0o600)
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
        expected: &str,
    ) -> Result<(), BoundConfigRefusal> {
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
        BoundAtomicPublishError, atomic_replace_detailed_bound,
    };
    use solstone_core_journal_io::media::MediaDirectory;
    use solstone_core_journal_io::{
        DetailedAtomicOutcome, WindowsFlatDirectory, acquire_existing_parent_lock_bound,
        open_windows_flat_directory_bound, read_windows_observed_file_bounded,
    };

    use super::*;

    pub(super) fn update(
        root: &Arc<JournalRoot>,
        directory: &MediaDirectory,
        expected: &str,
        update: impl FnOnce(&Map<String, Value>) -> Option<Map<String, Value>>,
        options: LockOptions,
    ) -> Result<BoundConfigOutcome, BoundConfigRefusal> {
        root.revalidate_canonical_binding().map_err(admission)?;
        directory.revalidate().map_err(admission)?;
        let parent = open_parent(root).map_err(BoundConfigRefusal::Admission)?;
        validate(root, directory, &parent).map_err(BoundConfigRefusal::Admission)?;
        let initial = read_journal_config_in_directory(directory, MAX_BOUND_CONFIG_BYTES)
            .map_err(BoundConfigRefusal::Load)?;
        require_expected(&initial, expected)?;
        validate(root, directory, &parent).map_err(BoundConfigRefusal::Admission)?;
        let lock = acquire_existing_parent_lock_bound(
            &parent,
            OsStr::new("journal.json.lock"),
            options.timeout,
            options.poll_interval,
        )
        .map_err(BoundConfigRefusal::Lock)?;
        validate(root, directory, &parent).map_err(BoundConfigRefusal::Admission)?;
        let locked = read_journal_config_in_directory(directory, MAX_BOUND_CONFIG_BYTES)
            .map_err(BoundConfigRefusal::Load)?;
        require_expected(&locked, expected)?;
        validate(root, directory, &parent).map_err(BoundConfigRefusal::Admission)?;
        let config = locked.config.as_ref().ok_or(BoundConfigRefusal::Missing)?;
        let Some(replacement) = update(config) else {
            return Ok(BoundConfigOutcome::Unchanged);
        };
        let mut bytes =
            serde_json::to_vec_pretty(&replacement).map_err(BoundConfigRefusal::Serialize)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_BOUND_CONFIG_BYTES {
            return Err(BoundConfigRefusal::TooLarge);
        }
        let candidate_sha256 = fingerprint_journal_config_bytes(&bytes);
        validate(root, directory, &parent).map_err(BoundConfigRefusal::Admission)?;
        let outcome = atomic_replace_detailed_bound(
            &parent,
            &lock,
            OsStr::new("journal.json"),
            &bytes,
            0o600,
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
        expected: &str,
    ) -> Result<(), BoundConfigRefusal> {
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
    fn final_native_rename_keeps_original_parent_and_preserves_actual_publication_receipt() {
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
