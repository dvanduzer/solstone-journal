// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Retained exclusion for the separately stored hosted rotation identity.

use crate::HostedBinding;
use solstone_core_journal_io::media::{MediaDirectory, MediaRead};
use solstone_core_journal_io::{BoundParentLock, JournalRoot};
use std::{ffi::OsStr, io, path::Path, sync::Arc, time::Duration};

#[cfg(unix)]
type Parent = solstone_core_journal_io::FlatDirectory;
#[cfg(windows)]
type Parent = solstone_core_journal_io::WindowsFlatDirectory;

#[must_use]
pub struct RotationBindingExclusion {
    #[cfg(any(unix, windows))]
    root: Arc<JournalRoot>,
    #[cfg(any(unix, windows))]
    directory: Arc<MediaDirectory>,
    #[cfg(any(unix, windows))]
    parent: Parent,
    #[cfg(any(unix, windows))]
    lock: BoundParentLock,
    #[cfg(any(unix, windows))]
    original: MediaRead,
}

fn refused(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

/// Acquire only the binding sidecar. Callers nesting this with config exclusion
/// must acquire config first and release this owner before reacquiring config.
pub fn hold_rotation_binding_at_root(
    root: &Arc<JournalRoot>,
    expected: &HostedBinding,
) -> io::Result<RotationBindingExclusion> {
    #[cfg(any(unix, windows))]
    {
        root.revalidate_canonical_binding().map_err(refused)?;
        let directory = MediaDirectory::open(root.clone(), Path::new("backup/hosted"))?;
        #[cfg(unix)]
        let parent = Parent::open(root, Path::new("backup/hosted")).map_err(refused)?;
        #[cfg(windows)]
        let parent = {
            use solstone_core_journal_io::open_windows_flat_directory_bound as open;
            let backup = open(&**root, OsStr::new("backup"), root.canonical_path())
                .map_err(refused)?
                .ok_or_else(|| io::Error::other("hosted backup parent missing"))?;
            open(
                &backup,
                OsStr::new("hosted"),
                &root.canonical_path().join("backup"),
            )
            .map_err(refused)?
            .ok_or_else(|| io::Error::other("hosted binding parent missing"))?
        };
        #[cfg(unix)]
        let identity = parent.identity();
        #[cfg(windows)]
        let identity = parent.object_identity();
        if identity != directory.object_identity() {
            return Err(io::Error::other("hosted binding parent changed"));
        }
        let name = OsStr::new("binding.json.lock");
        let lock = match solstone_core_journal_io::open_existing_parent_lock_bound(
            &parent,
            name,
            Duration::ZERO,
            Duration::ZERO,
        )
        .map_err(refused)?
        {
            Some(lock) => lock,
            None => solstone_core_journal_io::acquire_existing_parent_lock_bound(
                &parent,
                name,
                Duration::ZERO,
                Duration::ZERO,
            )
            .map_err(refused)?,
        };
        let original = directory
            .read_file_bounded(OsStr::new("binding.json"), 64 * 1024)?
            .ok_or_else(|| io::Error::other("hosted binding missing"))?;
        if crate::hosted::decode_hosted_binding(&original.bytes).as_ref() != Some(expected) {
            return Err(io::Error::other("hosted binding changed"));
        }
        let held = RotationBindingExclusion {
            root: root.clone(),
            directory,
            parent,
            lock,
            original,
        };
        held.revalidate()?;
        Ok(held)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (root, expected);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "hosted rotation exclusion unsupported",
        ))
    }
}

impl RotationBindingExclusion {
    pub fn revalidate(&self) -> io::Result<()> {
        #[cfg(any(unix, windows))]
        {
            self.root.revalidate_canonical_binding().map_err(refused)?;
            self.directory.revalidate()?;
            self.lock
                .revalidate_bound(&self.parent, OsStr::new("binding.json.lock"))
                .map_err(refused)?;
            if self
                .directory
                .read_file_bounded(OsStr::new("binding.json"), 64 * 1024)?
                .as_ref()
                != Some(&self.original)
            {
                return Err(io::Error::other("hosted rotation binding changed"));
            }
            self.directory.revalidate()?;
            self.root.revalidate_canonical_binding().map_err(refused)
        }
        #[cfg(not(any(unix, windows)))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "hosted rotation exclusion unsupported",
        ))
    }
}

#[cfg(all(test, unix, feature = "full-tests"))]
mod tests {
    use super::*;
    use crate::{
        PendingExecution, PendingKeyRevocation, backup_setup_fingerprint, candidate_binding,
        clear_pending_key_revocation_at_root, generate_and_store_keys, get_backup_config,
        save_hosted_binding, set_mode, stage_pending_key_revocation_at_root,
    };
    use solstone_core_journal_config::read_journal_config;
    use solstone_core_journal_config_write::BoundConfigOutcome;

    fn binding() -> HostedBinding {
        HostedBinding {
            broker_endpoint: "https://broker.example.invalid".into(),
            account_id: "account".into(),
            instance_id: "instance".into(),
            bucket: "bucket".into(),
            prefix: "prefix".into(),
            broker_token: "original-token".into(),
        }
    }

    #[test]
    fn same_destination_binding_drift_refuses_stage_and_completion() {
        let temp = tempfile::tempdir().unwrap();
        set_mode(temp.path(), "operated").unwrap();
        let keys = generate_and_store_keys(temp.path()).unwrap();
        let original = binding();
        save_hosted_binding(temp.path(), &original).unwrap();
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        let candidate = crate::generate_recovery_key().unwrap();
        let setup =
            backup_setup_fingerprint(&get_backup_config(temp.path()).unwrap(), Some(&original))
                .unwrap();
        let pending = PendingKeyRevocation {
            original_key_id: "original-key".into(),
            candidate_binding: candidate_binding(&keys.daily_key, &candidate).unwrap(),
            execution: PendingExecution {
                mode: "operated".into(),
                repository: "s3:https://s3.example.invalid/bucket/prefix".into(),
                backend: "s3".into(),
                credential_binding: setup.clone(),
            },
            setup_fingerprint: Some(setup),
        };
        let changed = HostedBinding {
            broker_token: "different-token".into(),
            ..original.clone()
        };
        let before = std::fs::read(temp.path().join("config/journal.json")).unwrap();
        let sha = read_journal_config(temp.path()).unwrap().sha256.unwrap();
        save_hosted_binding(temp.path(), &changed).unwrap();
        assert!(matches!(
            stage_pending_key_revocation_at_root(
                &root,
                &sha,
                &candidate,
                &pending,
                Some(&original)
            )
            .unwrap(),
            BoundConfigOutcome::Unchanged
        ));
        assert_eq!(
            std::fs::read(temp.path().join("config/journal.json")).unwrap(),
            before
        );
        save_hosted_binding(temp.path(), &original).unwrap();
        assert!(matches!(
            stage_pending_key_revocation_at_root(
                &root,
                &sha,
                &candidate,
                &pending,
                Some(&original)
            )
            .unwrap(),
            BoundConfigOutcome::Published(_)
        ));
        let staged = std::fs::read(temp.path().join("config/journal.json")).unwrap();
        let sha = read_journal_config(temp.path()).unwrap().sha256.unwrap();
        save_hosted_binding(temp.path(), &changed).unwrap();
        assert!(matches!(
            clear_pending_key_revocation_at_root(&root, &sha, &pending, Some(&original)).unwrap(),
            BoundConfigOutcome::Unchanged
        ));
        assert_eq!(
            std::fs::read(temp.path().join("config/journal.json")).unwrap(),
            staged
        );
        save_hosted_binding(temp.path(), &original).unwrap();
        assert!(matches!(
            clear_pending_key_revocation_at_root(&root, &sha, &pending, Some(&original)).unwrap(),
            BoundConfigOutcome::Published(_)
        ));
        assert!(
            !get_backup_config(temp.path())
                .unwrap()
                .contains_key("pending_key_revocation")
        );
    }

    #[test]
    fn retained_exclusion_detects_same_bytes_at_a_replaced_identity() {
        let temp = tempfile::tempdir().unwrap();
        let original = binding();
        save_hosted_binding(temp.path(), &original).unwrap();
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        let held = hold_rotation_binding_at_root(&root, &original).unwrap();
        held.revalidate().unwrap();
        let target = temp.path().join("backup/hosted/binding.json");
        let replacement = target.with_file_name("replacement.json");
        std::fs::copy(&target, &replacement).unwrap();
        std::fs::rename(&replacement, &target).unwrap();
        assert!(held.revalidate().is_err());
    }
}
