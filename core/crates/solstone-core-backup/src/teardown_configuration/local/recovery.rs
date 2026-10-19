// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Existing-only local finish re-admission. Saved observations grant no handles.

use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingObservationData {
    pub parent: MediaIdentity,
    pub file: MediaIdentity,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalTeardownObservation {
    pub version: u32,
    pub root: String,
    pub config_parent: MediaIdentity,
    pub original_file: MediaIdentity,
    pub original_size: u64,
    pub original_sha256: String,
    pub clear_sha256: String,
    pub binding: Option<BindingObservationData>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedClearObservation {
    pub sha256: String,
    pub file: MediaIdentity,
    pub size: u64,
}

fn fingerprint(bytes: &[u8]) -> String {
    solstone_core_journal_config::fingerprint_journal_config_bytes(bytes)
}

impl TeardownConfiguration {
    pub fn local_observation(&self) -> Result<LocalTeardownObservation, Refusal> {
        self.revalidate()?;
        let (read, entry) =
            read_journal_config_observed_in_directory(&self.directory, MAX_BOUND_CONFIG_BYTES)
                .map_err(|_| Refusal::ConfigurationUnavailable)?;
        let entry = entry.ok_or(Refusal::ConfigurationUnavailable)?;
        if read.sha256.as_deref() != Some(self.sha256())
            || entry.kind != JournalEntryKind::RegularFile
        {
            return Err(Refusal::ConfigurationChanged);
        }
        let binding = self
            .binding_observation
            .as_ref()
            .map(|binding| BindingObservationData {
                parent: binding.directory.identity(),
                file: binding.read.entry.identity,
                size: binding.read.entry.size,
                sha256: fingerprint(&binding.read.bytes),
            });
        let observation = LocalTeardownObservation {
            version: 1,
            root: self.root.identity().observation_id(),
            config_parent: self.directory.identity(),
            original_file: entry.identity,
            original_size: entry.size,
            original_sha256: self.sha256.clone(),
            clear_sha256: self
                .expected_clear_sha256
                .clone()
                .ok_or(Refusal::ConfigurationUnavailable)?,
            binding,
        };
        self.revalidate()?;
        Ok(observation)
    }

    pub fn capture_original_retry(
        lease: &Arc<solstone_core_journal_io::activity::TeardownLease>,
        expected: &LocalTeardownObservation,
    ) -> Result<Self, Refusal> {
        lease.revalidate().map_err(|_| Refusal::RootChanged)?;
        let captured = Self::capture(lease.journal_root())?;
        if captured.local_observation()? != *expected {
            return Err(Refusal::ConfigurationChanged);
        }
        Ok(captured)
    }

    pub fn published_clear_observation(
        &self,
        publication: &BoundConfigPublication,
    ) -> Result<PublishedClearObservation, Refusal> {
        if publication.durability_uncertainty.is_some()
            || publication.observation_uncertainty.is_some()
            || publication.confirmation_refusal.is_some()
            || self.expected_clear_sha256.as_deref() != Some(publication.candidate_sha256.as_str())
        {
            return Err(Refusal::ConfigurationChanged);
        }
        self.revalidate_published_clear(publication)?;
        Ok(PublishedClearObservation {
            sha256: publication.candidate_sha256.clone(),
            file: publication
                .published_identity
                .ok_or(Refusal::ConfigurationChanged)?,
            size: publication
                .published_file
                .as_ref()
                .ok_or(Refusal::ConfigurationChanged)?
                .size,
        })
    }

    /// Acquire actual existing parents and files under the caller's actual
    /// teardown lease. No remote work or original secrets are reconstructed.
    pub fn capture_local_finish(
        lease: &Arc<solstone_core_journal_io::activity::TeardownLease>,
        expected: &LocalTeardownObservation,
        cleared: &PublishedClearObservation,
        binding_removed: bool,
    ) -> Result<Self, Refusal> {
        lease.revalidate().map_err(|_| Refusal::RootChanged)?;
        let root = lease.journal_root();
        root.revalidate_canonical_binding()
            .map_err(|_| Refusal::RootChanged)?;
        if expected.version != 1
            || root.identity().observation_id() != expected.root
            || cleared.sha256 != expected.clear_sha256
        {
            return Err(Refusal::ConfigurationChanged);
        }
        let directory = MediaDirectory::open(root.clone(), Path::new("config"))
            .map_err(|_| Refusal::ConfigurationUnavailable)?;
        if directory.identity() != expected.config_parent {
            return Err(Refusal::ConfigurationChanged);
        }
        let (read, entry) =
            read_journal_config_observed_in_directory(&directory, MAX_BOUND_CONFIG_BYTES)
                .map_err(|_| Refusal::ConfigurationUnavailable)?;
        let entry = entry.ok_or(Refusal::ConfigurationUnavailable)?;
        if read.sha256.as_deref() != Some(cleared.sha256.as_str())
            || entry.identity != cleared.file
            || entry.size != cleared.size
            || entry.kind != JournalEntryKind::RegularFile
        {
            return Err(Refusal::ConfigurationChanged);
        }
        let raw = read.config.ok_or(Refusal::ConfigurationUnavailable)?;
        let backup = crate::merge_backup_config(&raw);
        if backup != crate::backup_defaults() {
            return Err(Refusal::ConfigurationChanged);
        }
        let (binding, binding_observation, retired_binding_parent) = match &expected.binding {
            None => (None, None, None),
            Some(original) => {
                let parent = MediaDirectory::open(root.clone(), Path::new("backup/hosted"))
                    .map_err(|_| Refusal::BindingUnavailable)?;
                if parent.identity() != original.parent {
                    return Err(Refusal::BindingChanged);
                }
                if binding_removed {
                    require_binding_absent(&parent)?;
                    (None, None, Some(parent))
                } else {
                    let read = read_binding(&parent)?;
                    if read.entry.identity != original.file
                        || read.entry.size != original.size
                        || fingerprint(&read.bytes) != original.sha256
                    {
                        return Err(Refusal::BindingChanged);
                    }
                    let binding = crate::hosted::decode_hosted_binding(&read.bytes)
                        .ok_or(Refusal::BindingUnavailable)?;
                    (
                        Some(binding),
                        Some(BindingObservation {
                            directory: parent,
                            read,
                        }),
                        None,
                    )
                }
            }
        };
        let captured = Self {
            sha256: cleared.sha256.clone(),
            backup,
            keys: None,
            destination: None,
            binding,
            root: root.clone(),
            directory,
            binding_observation,
            retired_binding_parent,
            expected_clear_sha256: Some(expected.clear_sha256.clone()),
        };
        captured.revalidate()?;
        lease.revalidate().map_err(|_| Refusal::RootChanged)?;
        Ok(captured)
    }
}

pub(super) fn require_binding_absent(parent: &MediaDirectory) -> Result<(), Refusal> {
    parent.revalidate().map_err(|_| Refusal::BindingChanged)?;
    let entries = parent
        .entries(64)
        .map_err(|_| Refusal::BindingUnavailable)?
        .ok_or(Refusal::BindingUnavailable)?;
    for entry in entries {
        // A missing unconfirmed original/claim remains held. Once a confirmed
        // removal was durably recorded, only its stable cooperation lock remains.
        if entry.name != OsStr::new("binding.json.lock") {
            return Err(Refusal::BindingChanged);
        }
    }
    parent.revalidate().map_err(|_| Refusal::BindingChanged)
}
