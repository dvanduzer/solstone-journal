// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Retained backup configuration and its exact conditional clear.

use std::sync::Arc;

use serde_json::{Map, Value};
use solstone_core_journal_config_write::{
    BoundConfigOutcome, BoundConfigPublication, BoundConfigRefusal,
};
use solstone_core_journal_io::JournalRoot;

use crate::{BackupKeys, Destination, HostedBinding};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeardownConfigurationRefusal {
    Unsupported,
    RootChanged,
    ConfigurationUnavailable,
    ConfigurationChanged,
    KeysUnavailable,
    BindingUnavailable,
    BindingChanged,
}

/// Original full-config fingerprint and Backup-owned interpretation.
/// No decoded settings or binding bytes are included in diagnostic formatting.
pub struct TeardownConfiguration {
    sha256: String,
    backup: Map<String, Value>,
    keys: Option<BackupKeys>,
    destination: Option<Destination>,
    binding: Option<HostedBinding>,
    #[cfg(unix)]
    root: Arc<JournalRoot>,
    #[cfg(unix)]
    directory: Arc<solstone_core_journal_io::media::MediaDirectory>,
    #[cfg(unix)]
    binding_observation: Option<unix::BindingObservation>,
    #[cfg(unix)]
    expected_clear_sha256: Option<String>,
}

#[cfg(unix)]
pub use unix::binding::{BindingFinalization, BindingFinalizationRefusal, BindingRemovalReceipt};

impl TeardownConfiguration {
    /// Retain both original-parent locks after this exact clear has been confirmed.
    /// The returned value supplies local observations, not remote settlement authority.
    #[cfg(unix)]
    pub fn begin_binding_finalization(
        &self,
        publication: &BoundConfigPublication,
    ) -> Result<BindingFinalization, BindingFinalizationRefusal> {
        unix::binding::begin(self, publication)
    }

    /// Capture existing state only; never creates parents or materializes defaults.
    pub fn capture(root: &Arc<JournalRoot>) -> Result<Self, TeardownConfigurationRefusal> {
        #[cfg(unix)]
        {
            unix::capture(root)
        }
        #[cfg(not(unix))]
        {
            let _ = root;
            Err(TeardownConfigurationRefusal::Unsupported)
        }
    }

    /// Recheck the original root, parents, full-config bytes and binding observation.
    pub fn revalidate(&self) -> Result<(), TeardownConfigurationRefusal> {
        #[cfg(unix)]
        {
            unix::revalidate(self)
        }
        #[cfg(not(unix))]
        {
            Err(TeardownConfigurationRefusal::Unsupported)
        }
    }

    /// Invoke the bound clear through this admission's original config parent.
    pub fn clear_configuration(&self) -> Result<BoundConfigOutcome, BoundConfigRefusal> {
        #[cfg(unix)]
        {
            solstone_core_journal_config_write::conditional_update_journal_config_in_directory(
                &self.root,
                &self.directory,
                self.sha256(),
                |config| crate::state::cleared_backup_configuration(config, self.backup()),
                crate::state::mutation_lock_options(),
            )
        }
        #[cfg(not(unix))]
        {
            Err(BoundConfigRefusal::Unsupported)
        }
    }

    /// Observe an exact published candidate. Descriptive receipts confer no phase authority.
    pub fn revalidate_published_clear(
        &self,
        publication: &BoundConfigPublication,
    ) -> Result<(), TeardownConfigurationRefusal> {
        #[cfg(unix)]
        {
            unix::revalidate_published_clear(self, publication)
        }
        #[cfg(not(unix))]
        {
            let _ = publication;
            Err(TeardownConfigurationRefusal::Unsupported)
        }
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn backup(&self) -> &Map<String, Value> {
        &self.backup
    }

    pub fn keys(&self) -> Option<&BackupKeys> {
        self.keys.as_ref()
    }

    pub fn destination(&self) -> Option<&Destination> {
        self.destination.as_ref()
    }

    pub fn binding(&self) -> Option<&HostedBinding> {
        self.binding.as_ref()
    }
}

#[cfg(unix)]
mod unix {
    use std::ffi::OsStr;
    use std::path::Path;

    use solstone_core_journal_config::{
        read_journal_config_in_directory, read_journal_config_observed_in_directory,
    };
    use solstone_core_journal_config_write::MAX_BOUND_CONFIG_BYTES;
    use solstone_core_journal_io::journal_root::JournalEntryKind;
    use solstone_core_journal_io::media::{MediaDirectory, MediaIdentity, MediaRead};

    use super::*;
    use TeardownConfigurationRefusal as Refusal;

    pub(super) mod binding;

    const MAX_BINDING_BYTES: usize = 1024 * 1024;

    pub(super) struct BindingObservation {
        directory: Arc<MediaDirectory>,
        read: MediaRead,
    }

    pub(super) fn capture(root: &Arc<JournalRoot>) -> Result<TeardownConfiguration, Refusal> {
        root.revalidate_canonical_binding()
            .map_err(|_| Refusal::RootChanged)?;
        let directory = MediaDirectory::open(root.clone(), Path::new("config"))
            .map_err(|_| Refusal::ConfigurationUnavailable)?;
        capture_in_directory(root.clone(), directory)
    }

    fn capture_in_directory(
        root: Arc<JournalRoot>,
        directory: Arc<MediaDirectory>,
    ) -> Result<TeardownConfiguration, Refusal> {
        root.revalidate_canonical_binding()
            .map_err(|_| Refusal::RootChanged)?;
        let read = read_journal_config_in_directory(&directory, MAX_BOUND_CONFIG_BYTES)
            .map_err(|_| Refusal::ConfigurationUnavailable)?;
        let sha256 = read.sha256.ok_or(Refusal::ConfigurationUnavailable)?;
        let raw = read.config.ok_or(Refusal::ConfigurationUnavailable)?;
        let backup = crate::merge_backup_config(&raw);
        let keys = crate::state::build_keys(backup.get("daily_key"), backup.get("recovery_key"))
            .map_err(|_| Refusal::KeysUnavailable)?;
        let destination = crate::state::destination_from_config(&backup);
        let expected_clear_sha256 = expected_clear_fingerprint(&raw, &backup);
        let (binding, binding_observation) =
            if backup.get("mode").and_then(Value::as_str) == Some("operated") {
                let parent = MediaDirectory::open(root.clone(), Path::new(""))
                    .map_err(|_| Refusal::BindingUnavailable)?;
                let backup_parent = parent
                    .open_child(OsStr::new("backup"))
                    .map_err(|_| Refusal::BindingUnavailable)?
                    .ok_or(Refusal::BindingUnavailable)?;
                let directory = backup_parent
                    .open_child(OsStr::new("hosted"))
                    .map_err(|_| Refusal::BindingUnavailable)?
                    .ok_or(Refusal::BindingUnavailable)?;
                let read = read_binding(&directory)?;
                let binding = crate::hosted::decode_hosted_binding(&read.bytes)
                    .ok_or(Refusal::BindingUnavailable)?;
                (Some(binding), Some(BindingObservation { directory, read }))
            } else {
                (None, None)
            };
        let captured = TeardownConfiguration {
            sha256,
            backup,
            keys,
            destination,
            binding,
            root,
            directory,
            binding_observation,
            expected_clear_sha256,
        };
        revalidate(&captured)?;
        Ok(captured)
    }

    fn read_binding(directory: &MediaDirectory) -> Result<MediaRead, Refusal> {
        directory
            .read_file_bounded(OsStr::new("binding.json"), MAX_BINDING_BYTES)
            .map_err(|_| Refusal::BindingUnavailable)?
            .filter(|read| read.complete)
            .ok_or(Refusal::BindingUnavailable)
    }

    fn expected_clear_fingerprint(
        raw: &Map<String, Value>,
        backup: &Map<String, Value>,
    ) -> Option<String> {
        use std::io::Write;
        struct Candidate(Vec<u8>);
        impl Write for Candidate {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self
                    .0
                    .len()
                    .checked_add(bytes.len())
                    .is_none_or(|size| size > MAX_BOUND_CONFIG_BYTES)
                {
                    return Err(std::io::Error::other("clear candidate exceeds byte limit"));
                }
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let clear = crate::state::cleared_backup_configuration(raw, backup)?;
        let mut candidate = Candidate(Vec::new());
        serde_json::to_writer_pretty(&mut candidate, &clear).ok()?;
        candidate.write_all(b"\n").ok()?;
        Some(solstone_core_journal_config::fingerprint_journal_config_bytes(&candidate.0))
    }

    pub(super) fn revalidate(captured: &TeardownConfiguration) -> Result<(), Refusal> {
        revalidate_parents(captured)?;
        let current = read_journal_config_in_directory(&captured.directory, MAX_BOUND_CONFIG_BYTES)
            .map_err(|_| Refusal::ConfigurationUnavailable)?;
        if current.sha256.as_deref() != Some(captured.sha256()) {
            return Err(Refusal::ConfigurationChanged);
        }
        revalidate_binding_and_parents(captured)
    }

    fn revalidate_parents(captured: &TeardownConfiguration) -> Result<(), Refusal> {
        captured
            .root
            .revalidate_canonical_binding()
            .map_err(|_| Refusal::RootChanged)?;
        captured
            .directory
            .revalidate()
            .map_err(|_| Refusal::ConfigurationChanged)?;
        Ok(())
    }

    pub(super) fn revalidate_published_clear(
        captured: &TeardownConfiguration,
        publication: &BoundConfigPublication,
    ) -> Result<(), Refusal> {
        revalidate_parents(captured)?;
        let (current, entry) =
            read_journal_config_observed_in_directory(&captured.directory, MAX_BOUND_CONFIG_BYTES)
                .map_err(|_| Refusal::ConfigurationUnavailable)?;
        let entry = entry.ok_or(Refusal::ConfigurationUnavailable)?;
        if current.sha256.as_deref() != Some(publication.candidate_sha256.as_str())
            || entry.name != OsStr::new("journal.json")
            || entry.kind != JournalEntryKind::RegularFile
            || entry.identity
                != (MediaIdentity::Unix {
                    device: publication.published_file.device,
                    inode: publication.published_file.inode,
                })
            || entry.size != publication.published_file.size
        {
            return Err(Refusal::ConfigurationChanged);
        }
        revalidate_binding_and_parents(captured)
    }

    fn revalidate_binding_and_parents(captured: &TeardownConfiguration) -> Result<(), Refusal> {
        if let Some(binding) = &captured.binding_observation
            && read_binding(&binding.directory)? != binding.read
        {
            return Err(Refusal::BindingChanged);
        }
        captured
            .directory
            .revalidate()
            .map_err(|_| Refusal::ConfigurationChanged)?;
        captured
            .root
            .revalidate_canonical_binding()
            .map_err(|_| Refusal::RootChanged)
    }

    #[cfg(all(test, feature = "full-tests"))]
    mod tests;
}

#[cfg(all(test, feature = "full-tests"))]
mod unsupported_tests {
    #[cfg(not(unix))]
    #[test]
    fn capture_refuses_before_reading_or_creating_state() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        assert!(matches!(
            TeardownConfiguration::capture(&root),
            Err(TeardownConfigurationRefusal::Unsupported)
        ));
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }
}
