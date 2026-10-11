// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Exact local finalization. No remote, lease or carrier completion is inferred.

use super::*;
use solstone_core_journal_io::{
    BoundParentLock, ClaimName, ClaimRemovalError, ClaimRemovalOutcome, ExistingParentLockError,
    FlatDirectory, FlatDirectoryEntry, FlatDirectoryError, acquire_existing_parent_lock_bound,
    claim_and_remove_observed_bounded, open_existing_parent_lock_bound, read_observed_file_bounded,
};
use std::io;
use std::time::Duration;

const CONFIG_LOCK: &str = "journal.json.lock";
const BINDING_LOCK: &str = "binding.json.lock";
const BINDING: &str = "binding.json";

pub enum BindingFinalizationRefusal {
    Configuration(Refusal),
    PublicationUncertain,
    MissingOriginalBinding,
    ParentChanged,
    BindingChanged,
    RemovalUnconfirmed,
    Native(io::Error),
    Flat(FlatDirectoryError),
    Lock(ExistingParentLockError),
}

impl std::fmt::Debug for BindingFinalizationRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Configuration(_) => "Configuration",
            Self::PublicationUncertain => "PublicationUncertain",
            Self::MissingOriginalBinding => "MissingOriginalBinding",
            Self::ParentChanged => "ParentChanged",
            Self::BindingChanged => "BindingChanged",
            Self::RemovalUnconfirmed => "RemovalUnconfirmed",
            Self::Native(_) => "Native",
            Self::Flat(_) => "Flat",
            Self::Lock(_) => "Lock",
        })
    }
}

/// Actual one-shot removal facts. Neither decoding nor cloning a handle to
/// these facts can recreate the retained guards or permit carrier retirement.
pub struct BindingRemovalReceipt {
    attempted: bool,
    native: Option<Result<ClaimRemovalOutcome, ClaimRemovalError>>,
    confirmation: Result<(), BindingFinalizationRefusal>,
}

impl BindingRemovalReceipt {
    pub fn attempted(&self) -> bool {
        self.attempted
    }
    pub fn native(&self) -> Option<&Result<ClaimRemovalOutcome, ClaimRemovalError>> {
        self.native.as_ref()
    }
    pub fn confirmation(&self) -> &Result<(), BindingFinalizationRefusal> {
        &self.confirmation
    }
    pub fn complete(&self) -> bool {
        matches!(self.native, Some(Ok(ClaimRemovalOutcome::Removed))) && self.confirmation.is_ok()
    }
}

/// Keeps config→binding exclusion and the actual original parent chains until
/// explicitly consumed. Dropping it releases guards without cleaning entries.
#[must_use]
pub struct BindingFinalization {
    root: Arc<JournalRoot>,
    config: Arc<MediaDirectory>,
    config_parent: FlatDirectory,
    config_lock: BoundParentLock,
    binding: Arc<MediaDirectory>,
    binding_parent: FlatDirectory,
    binding_lock: BoundParentLock,
    expected: MediaRead,
    candidate_sha256: String,
    published_file: FlatDirectoryEntry,
    claim: ClaimName,
    receipt: Option<Arc<BindingRemovalReceipt>>,
}

pub(in crate::teardown_configuration) fn begin(
    captured: &TeardownConfiguration,
    publication: &BoundConfigPublication,
) -> Result<BindingFinalization, BindingFinalizationRefusal> {
    use BindingFinalizationRefusal as Failure;
    if publication.durability_uncertainty.is_some()
        || publication.observation_uncertainty.is_some()
        || publication.confirmation_refusal.is_some()
    {
        return Err(Failure::PublicationUncertain);
    }
    if captured.expected_clear_sha256.as_deref() != Some(publication.candidate_sha256.as_str()) {
        return Err(Failure::Configuration(Refusal::ConfigurationChanged));
    }
    captured
        .revalidate_published_clear(publication)
        .map_err(Failure::Configuration)?;
    let original = captured
        .binding_observation
        .as_ref()
        .ok_or(Failure::MissingOriginalBinding)?;
    let config_parent =
        FlatDirectory::open(&captured.root, Path::new("config")).map_err(Failure::Flat)?;
    let binding_parent =
        FlatDirectory::open(&captured.root, Path::new("backup/hosted")).map_err(Failure::Flat)?;
    if config_parent.identity() != captured.directory.object_identity()
        || binding_parent.identity() != original.directory.object_identity()
    {
        return Err(Failure::ParentChanged);
    }
    captured
        .revalidate_published_clear(publication)
        .map_err(Failure::Configuration)?;
    // Clear has returned and released its own lock. Never nest an acquisition.
    let config_lock = lock(&config_parent, CONFIG_LOCK)?;
    captured
        .revalidate_published_clear(publication)
        .map_err(Failure::Configuration)?;
    let binding_lock = lock(&binding_parent, BINDING_LOCK)?;
    captured
        .revalidate_published_clear(publication)
        .map_err(Failure::Configuration)?;
    let mut entropy = [0u8; 8];
    getrandom::fill(&mut entropy)
        .map_err(|_| Failure::Native(io::Error::other("binding claim entropy unavailable")))?;
    let claim = ClaimName::parse(&format!(
        "!solstone-claim-{:08x}-{:016x}",
        std::process::id(),
        u64::from_le_bytes(entropy)
    ))
    .map_err(|_| Failure::Native(io::Error::other("binding claim name refused")))?;
    let held = BindingFinalization {
        root: captured.root.clone(),
        config: captured.directory.clone(),
        config_parent,
        config_lock,
        binding: original.directory.clone(),
        binding_parent,
        binding_lock,
        expected: original.read.clone(),
        candidate_sha256: publication.candidate_sha256.clone(),
        published_file: publication.published_file.clone(),
        claim,
        receipt: None,
    };
    held.validate_original()?;
    Ok(held)
}

fn lock(parent: &FlatDirectory, name: &str) -> Result<BoundParentLock, BindingFinalizationRefusal> {
    let name = OsStr::new(name);
    match open_existing_parent_lock_bound(parent, name, Duration::ZERO, Duration::ZERO)
        .map_err(BindingFinalizationRefusal::Lock)?
    {
        Some(guard) => Ok(guard),
        None => acquire_existing_parent_lock_bound(parent, name, Duration::ZERO, Duration::ZERO)
            .map_err(BindingFinalizationRefusal::Lock),
    }
}

impl BindingFinalization {
    fn validate_guards(&self) -> Result<(), BindingFinalizationRefusal> {
        use BindingFinalizationRefusal as Failure;
        self.root
            .revalidate_canonical_binding()
            .map_err(|_| Failure::Configuration(Refusal::RootChanged))?;
        self.config.revalidate().map_err(Failure::Native)?;
        self.binding.revalidate().map_err(Failure::Native)?;
        self.config_parent.revalidate().map_err(Failure::Flat)?;
        self.binding_parent.revalidate().map_err(Failure::Flat)?;
        if self.config_parent.identity() != self.config.object_identity()
            || self.binding_parent.identity() != self.binding.object_identity()
        {
            return Err(Failure::ParentChanged);
        }
        self.config_lock
            .revalidate_bound(&self.config_parent, OsStr::new(CONFIG_LOCK))
            .map_err(Failure::Lock)?;
        self.binding_lock
            .revalidate_bound(&self.binding_parent, OsStr::new(BINDING_LOCK))
            .map_err(Failure::Lock)?;
        let (read, entry) =
            read_journal_config_observed_in_directory(&self.config, MAX_BOUND_CONFIG_BYTES)
                .map_err(|_| Failure::Configuration(Refusal::ConfigurationUnavailable))?;
        let entry = entry.ok_or(Failure::Configuration(Refusal::ConfigurationChanged))?;
        if read.sha256.as_deref() != Some(self.candidate_sha256.as_str())
            || entry.name != OsStr::new("journal.json")
            || entry.kind != JournalEntryKind::RegularFile
            || entry.identity
                != (MediaIdentity::Unix {
                    device: self.published_file.device,
                    inode: self.published_file.inode,
                })
            || entry.size != self.published_file.size
        {
            return Err(Failure::Configuration(Refusal::ConfigurationChanged));
        }
        self.config.revalidate().map_err(Failure::Native)?;
        self.binding.revalidate().map_err(Failure::Native)?;
        self.root
            .revalidate_canonical_binding()
            .map_err(|_| Failure::Configuration(Refusal::RootChanged))
    }

    fn validate_original(&self) -> Result<(), BindingFinalizationRefusal> {
        self.validate_guards()?;
        if read_binding(&self.binding).map_err(BindingFinalizationRefusal::Configuration)?
            != self.expected
        {
            return Err(BindingFinalizationRefusal::BindingChanged);
        }
        self.validate_guards()
    }

    fn validate_absence(&self) -> Result<(), BindingFinalizationRefusal> {
        self.validate_guards()?;
        for name in [OsStr::new(BINDING), self.claim.as_os_str()] {
            if self
                .binding
                .read_file_bounded(name, MAX_BINDING_BYTES)
                .map_err(BindingFinalizationRefusal::Native)?
                .is_some()
            {
                return Err(BindingFinalizationRefusal::RemovalUnconfirmed);
            }
        }
        self.validate_guards()
    }

    /// Attempt at most once; later calls borrow the exact original receipt.
    pub fn remove_binding(&mut self) -> &Arc<BindingRemovalReceipt> {
        if self.receipt.is_none() {
            let prior = (|| {
                self.validate_original()?;
                let prior = read_observed_file_bounded(
                    &self.binding_parent,
                    OsStr::new(BINDING),
                    MAX_BINDING_BYTES,
                )
                .map_err(BindingFinalizationRefusal::Flat)?
                .ok_or(BindingFinalizationRefusal::BindingChanged)?;
                self.validate_original()?;
                let expected = &self.expected;
                if prior.entry.name != expected.entry.name
                    || prior.entry.kind != expected.entry.kind
                    || prior.entry.size != expected.entry.size
                    || prior.bytes != expected.bytes
                    || expected.entry.identity
                        != (MediaIdentity::Unix {
                            device: prior.entry.device,
                            inode: prior.entry.inode,
                        })
                {
                    return Err(BindingFinalizationRefusal::BindingChanged);
                }
                Ok(prior)
            })();
            let receipt = match prior {
                Err(refusal) => BindingRemovalReceipt {
                    attempted: false,
                    native: None,
                    confirmation: Err(refusal),
                },
                Ok(prior) => {
                    let native = claim_and_remove_observed_bounded(
                        &self.binding_parent,
                        OsStr::new(BINDING),
                        &prior,
                        &self.claim,
                        MAX_BINDING_BYTES,
                    );
                    let confirmation = self.validate_absence();
                    BindingRemovalReceipt {
                        attempted: true,
                        native: Some(native),
                        confirmation,
                    }
                }
            };
            self.receipt = Some(Arc::new(receipt));
        }
        self.receipt
            .as_ref()
            .expect("one-shot receipt stored above")
    }

    pub fn receipt(&self) -> Option<&Arc<BindingRemovalReceipt>> {
        self.receipt.as_ref()
    }

    /// Fresh local checks while both original locks remain held.
    pub fn revalidate_removed(&self) -> Result<(), BindingFinalizationRefusal> {
        if !self
            .receipt
            .as_ref()
            .is_some_and(|receipt| receipt.complete())
        {
            return Err(BindingFinalizationRefusal::RemovalUnconfirmed);
        }
        self.validate_absence()
    }

    /// Release local handles only. This never removes or repairs an entry.
    pub fn into_receipt(self) -> Option<Arc<BindingRemovalReceipt>> {
        self.receipt
    }
}

#[cfg(all(test, feature = "full-tests"))]
mod tests;
