// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Non-secret restart observations. Every effect still requires actual retained
//! root, configuration, native directory and original carrier authorities.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solstone_core_journal_io::activity::TeardownLease;
use solstone_core_journal_io::media::teardown_preparation::{
    MARKER, MARKER_CAP, PREPARATIONS, PreparationObservation, TeardownPreparation,
};
use solstone_core_journal_io::media::{MediaDirectory, StageObservation};
use solstone_core_journal_io::{
    BoundAtomicOutcome, ClaimName, ClaimRemovalOutcome, FileObservation, FlatDirectory,
    FlatDirectoryError, atomic_replace_bound, claim_and_remove_observed,
    read_observed_file_bounded, write_bytes_exclusive_bound,
};

use super::TEARDOWN_RECOVERY_FILENAME;
use super::{RemoteEffect, RemoteFacts};
use crate::BackupError;
use crate::teardown_configuration::{LocalTeardownObservation, PublishedClearObservation};

const CAP: usize = 384 * 1024;
static CLAIM: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeDestination {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
}

impl NativeDestination {
    pub fn namespace(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("destination contains only strings");
        format!("{:x}", Sha256::digest(bytes))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeRecordObservation {
    pub version: u32,
    pub present: bool,
    pub device: u64,
    pub inode: u64,
    pub id: String,
    pub operation: i32,
    pub active: bool,
    pub scope: String,
    pub origin: String,
    pub reconciled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeStorageObservation {
    pub installation: String,
    pub family: String,
    pub directory: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeCustodyObservation {
    pub destination: NativeDestination,
    pub storage: NativeStorageObservation,
    pub record: NativeRecordObservation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Byo,
    Operated,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub version: u32,
    pub mode: Mode,
    pub local: LocalTeardownObservation,
    pub carrier: StageObservation,
    pub offload_was_recorded: bool,
    pub preparation: PreparationObservation,
    pub preparation_ready: bool,
    pub native: Option<NativeCustodyObservation>,
    pub origin: String,
    pub repository_id: Option<String>,
    pub snapshot_ids: Vec<String>,
    pub remote_settled: bool,
    pub remote_facts: Option<RemoteFacts>,
    pub clear: Option<PublishedClearObservation>,
    pub binding_removed: bool,
    pub native_retired: bool,
    pub carrier_retired: bool,
}

pub fn lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Shared scope encoding for nonsecret records and actual typed admission.
pub fn destructive_scope(
    config_sha256: &str,
    destination: &NativeDestination,
    repository_id: &str,
    origin: &str,
    ids: &[String],
) -> Result<String, BackupError> {
    if !config_sha256
        .strip_prefix("sha256:")
        .is_some_and(|s| lower_hex(s, 64))
        || !lower_hex(repository_id, 64)
        || !lower_hex(origin, 32)
        || ids.len() > 4096
    {
        return Err(BackupError::TeardownRecoveryCorrupt);
    }
    let mut ordered = ids.to_vec();
    ordered.sort();
    if ordered.iter().any(|id| !lower_hex(id, 64))
        || ordered.windows(2).any(|pair| pair[0] == pair[1])
    {
        return Err(BackupError::TeardownRecoveryCorrupt);
    }
    let mut digest = Sha256::new();
    digest.update(b"solstone.ios-teardown.v1\0");
    for field in [
        config_sha256,
        &destination.endpoint,
        &destination.region,
        &destination.bucket,
        &destination.prefix,
        repository_id,
        origin,
    ]
    .into_iter()
    .chain(ordered.iter().map(String::as_str))
    {
        if field.as_bytes().contains(&0) {
            return Err(BackupError::TeardownRecoveryCorrupt);
        }
        digest.update(field.as_bytes());
        digest.update([0]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

impl Record {
    pub fn preflight(
        mode: Mode,
        local: LocalTeardownObservation,
        offload_was_recorded: bool,
        preparation: PreparationObservation,
    ) -> Result<Self, BackupError> {
        let value = Self {
            version: 2,
            mode,
            local,
            carrier: preparation.destination.clone(),
            preparation,
            preparation_ready: false,
            offload_was_recorded,
            native: None,
            origin: String::new(),
            repository_id: None,
            snapshot_ids: vec![],
            remote_settled: false,
            remote_facts: None,
            clear: None,
            binding_removed: false,
            native_retired: false,
            carrier_retired: false,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), BackupError> {
        let sha = |s: &str| s.strip_prefix("sha256:").is_some_and(|s| lower_hex(s, 64));
        let bounded = |s: &str, cap| s.len() <= cap && !s.contains(['\0', '\r', '\n']);
        if self.version != 2
            || self.local.version != 1
            || self.local.root != self.carrier.journal_observation
            || !sha(&self.local.original_sha256)
            || !sha(&self.local.clear_sha256)
            || !self.carrier.parent_relative.is_empty()
            || !self
                .carrier
                .directory_name
                .strip_prefix(".tmp-offload-")
                .is_some_and(|s| lower_hex(s, 32))
            || self.preparation.version != 1
            || self.preparation.destination != self.carrier
            || self.preparation.source.journal_observation != self.local.root
            || self.preparation.source.directory_identity != self.carrier.directory_identity
            || self.preparation.source.parent_relative != ["backup", PREPARATIONS]
            || self.preparation.source.directory_name != self.carrier.directory_name
            || self.remote_settled != self.remote_facts.is_some()
            || self.snapshot_ids.len() > 4096
            || self.snapshot_ids.iter().any(|s| !lower_hex(s, 64))
            || self.snapshot_ids.windows(2).any(|p| p[0] >= p[1])
            || self
                .clear
                .as_ref()
                .is_some_and(|c| !self.remote_settled || c.sha256 != self.local.clear_sha256)
            || (self.binding_removed || self.native_retired || self.carrier_retired)
                && self.clear.is_none()
            || self.carrier_retired && (!self.binding_removed || !self.native_retired)
        {
            return Err(BackupError::TeardownRecoveryCorrupt);
        }
        let Some(native) = &self.native else {
            if !self.origin.is_empty()
                || self.repository_id.is_some()
                || !self.snapshot_ids.is_empty()
                || self.remote_settled
                || self.clear.is_some()
                || self.binding_removed
                || self.native_retired
                || self.carrier_retired
            {
                return Err(BackupError::TeardownRecoveryCorrupt);
            }
            return Ok(());
        };
        if !self.preparation_ready
            || !bounded(&native.destination.endpoint, 1024)
            || !bounded(&native.destination.region, 64)
            || !bounded(&native.destination.bucket, 63)
            || !bounded(&native.destination.prefix, 512)
            || [
                &native.storage.installation,
                &native.storage.family,
                &native.storage.directory,
            ]
            .iter()
            .any(|s| s.is_empty() || !bounded(s, 256))
            || native.record.version != 1
        {
            return Err(BackupError::TeardownRecoveryCorrupt);
        }
        if let Some(facts) = &self.remote_facts {
            let consistent = facts.version == 1
                && match &facts.effect {
                    RemoteEffect::SnapshotsPruned {
                        admitted_snapshot_count,
                        remaining_snapshots,
                    } => {
                        self.mode == Mode::Byo
                            && *admitted_snapshot_count as usize == self.snapshot_ids.len()
                            && *remaining_snapshots == 0
                            && facts.storage_retention.is_some()
                    }
                    RemoteEffect::OperatedWiped {
                        delete_attempts,
                        delete_confirmations,
                        abort_attempts,
                        abort_confirmations,
                        objects_empty,
                        uploads_empty,
                    } => {
                        self.mode == Mode::Operated
                            && *objects_empty
                            && *uploads_empty
                            && delete_confirmations <= delete_attempts
                            && abort_confirmations <= abort_attempts
                    }
                };
            if !consistent {
                return Err(BackupError::TeardownRecoveryCorrupt);
            }
        }
        match self.mode {
            Mode::Byo
                if !native.record.present
                    || !lower_hex(&self.origin, 32)
                    || !self
                        .repository_id
                        .as_ref()
                        .is_some_and(|id| lower_hex(id, 64)) =>
            {
                Err(BackupError::TeardownRecoveryCorrupt)
            }
            Mode::Operated if self.repository_id.is_some() || !self.snapshot_ids.is_empty() => {
                Err(BackupError::TeardownRecoveryCorrupt)
            }
            Mode::Byo if self.remote_settled => {
                let expected = destructive_scope(
                    &self.local.original_sha256,
                    &native.destination,
                    self.repository_id
                        .as_deref()
                        .ok_or(BackupError::TeardownRecoveryCorrupt)?,
                    &self.origin,
                    &self.snapshot_ids,
                )?;
                if !native.record.present
                    || native.record.active
                    || native.record.operation != 11
                    || !native.record.reconciled
                    || native.record.scope != expected
                    || native.record.origin != self.origin
                {
                    return Err(BackupError::TeardownRecoveryCorrupt);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub fn complete(&self) -> bool {
        self.remote_settled
            && self.clear.is_some()
            && self.binding_removed
            && self.native_retired
            && self.carrier_retired
    }
}

/// Keeps the actual original parent and exact current record, under the newly
/// acquired teardown exclusion. Decode alone cannot create this owner.
pub struct Store {
    lease: Arc<TeardownLease>,
    directory: FlatDirectory,
    observed: FileObservation,
    record: Record,
    preparation_removal:
        Option<Result<ClaimRemovalOutcome, solstone_core_journal_io::ClaimRemovalError>>,
}

pub struct DiscoveredPreparation {
    pub record: Record,
    pub preparation: TeardownPreparation,
}

impl Store {
    /// Complete bounded discovery on every invocation, including when the
    /// canonical record exists. Markerless metadata staging stays Held.
    pub fn discover_preparation(
        lease: Arc<TeardownLease>,
        canonical: Option<&Record>,
    ) -> Result<Option<DiscoveredPreparation>, BackupError> {
        let scan = || -> std::io::Result<Option<DiscoveredPreparation>> {
            lease.revalidate().map_err(std::io::Error::other)?;
            let root = MediaDirectory::open(lease.journal_root().clone(), Path::new(""))?;
            let metadata = root.open_child(OsStr::new("backup"))?;
            let source = metadata
                .as_ref()
                .map(|m| m.open_child(OsStr::new(PREPARATIONS)))
                .transpose()?
                .flatten();
            let mut found = Vec::new();
            for (parent, staging, cap) in source
                .iter()
                .map(|p| (p, true, 128))
                .chain(std::iter::once((&root, false, 16384)))
            {
                let entries = parent
                    .entries_keeping_any_name(cap)?
                    .ok_or_else(|| std::io::Error::other("teardown discovery limit"))?;
                for entry in entries {
                    if !entry
                        .name
                        .to_str()
                        .is_some_and(|s| s.starts_with(".tmp-offload-"))
                    {
                        if staging {
                            return Err(std::io::Error::other("unexpected preparation entry"));
                        }
                        continue;
                    }
                    let child = parent
                        .open_child(&entry.name)?
                        .ok_or_else(|| std::io::Error::other("preparation disappeared"))?;
                    let Some(marker) = child.read_file_bounded(OsStr::new(MARKER), MARKER_CAP)?
                    else {
                        if staging {
                            return Err(std::io::Error::other("incomplete preparation"));
                        }
                        continue;
                    };
                    if child.entries(1)?.is_none() {
                        return Err(std::io::Error::other("preparation residue"));
                    }
                    let record: Record =
                        serde_json::from_slice(&marker.bytes).map_err(std::io::Error::other)?;
                    record.validate().map_err(std::io::Error::other)?;
                    let coordinate = if staging {
                        &record.preparation.source
                    } else {
                        &record.preparation.destination
                    };
                    if record.preparation_ready
                        || record.native.is_some()
                        || entry.name.to_str() != Some(coordinate.directory_name.as_str())
                        || coordinate.parent_identity != parent.identity()
                        || coordinate.directory_identity != child.identity()
                    {
                        return Err(std::io::Error::other("preparation identity changed"));
                    }
                    let preparation =
                        TeardownPreparation::reopen(lease.clone(), &record.preparation)?;
                    if preparation.marker()?.as_ref() != Some(&marker) {
                        return Err(std::io::Error::other("preparation marker changed"));
                    }
                    found.push(DiscoveredPreparation {
                        record,
                        preparation,
                    });
                }
            }
            if found.len() > 1 {
                return Err(std::io::Error::other("ambiguous preparations"));
            }
            lease.revalidate().map_err(std::io::Error::other)?;
            let found = found.pop();
            if let (Some(canonical), Some(found)) = (canonical, &found)
                && canonical != &found.record
            {
                return Err(std::io::Error::other("conflicting canonical preparation"));
            }
            Ok(found)
        };
        scan().map_err(|_| BackupError::TeardownRecoveryRead)
    }

    /// Requires actual published-directory custody, strict record agreement,
    /// exact observed marker claim/removal and its directory sync. An applied
    /// but unconfirmed unlink leaves this canonical record unready and Held.
    pub fn complete_preparation(
        &mut self,
        preparation: &TeardownPreparation,
    ) -> Result<(), BackupError> {
        self.revalidate()?;
        if self.record.preparation_ready
            || self.record.native.is_some()
            || preparation.observation() != &self.record.preparation
        {
            return Err(BackupError::TeardownRecoveryWrite);
        }
        let retained = preparation
            .published_directory()
            .map_err(|_| BackupError::TeardownRecoveryRead)?;
        let expected = preparation
            .marker()
            .map_err(|_| BackupError::TeardownRecoveryRead)?
            .ok_or(BackupError::TeardownRecoveryRead)?;
        if serde_json::from_slice::<Record>(&expected.bytes)
            .map_err(|_| BackupError::TeardownRecoveryCorrupt)?
            != self.record
        {
            return Err(BackupError::TeardownRecoveryCorrupt);
        }
        let directory = FlatDirectory::open(
            self.lease.journal_root(),
            Path::new(&self.record.carrier.directory_name),
        )
        .map_err(|_| BackupError::TeardownRecoveryRead)?;
        if directory.identity() != retained.object_identity() {
            return Err(BackupError::TeardownRecoveryRead);
        }
        let observed = read_observed_file_bounded(&directory, OsStr::new(MARKER), MARKER_CAP)
            .map_err(|_| BackupError::TeardownRecoveryRead)?
            .ok_or(BackupError::TeardownRecoveryRead)?;
        if observed.bytes != expected.bytes
            || !expected.entry.identity.matches_flat_entry(&observed.entry)
            || expected.entry.size != observed.entry.size
        {
            return Err(BackupError::TeardownRecoveryRead);
        }
        self.revalidate()?;
        preparation
            .revalidate()
            .map_err(|_| BackupError::TeardownRecoveryRead)?;
        let claim = ClaimName::parse(&format!(
            "!solstone-claim-{:08x}-{:016x}",
            std::process::id(),
            CLAIM.fetch_add(1, Ordering::Relaxed)
        ))
        .map_err(|_| BackupError::TeardownRecoveryDelete)?;
        self.preparation_removal = Some(claim_and_remove_observed(
            &directory,
            OsStr::new(MARKER),
            &observed,
            &claim,
        ));
        if !matches!(
            self.preparation_removal,
            Some(Ok(ClaimRemovalOutcome::Removed))
        ) {
            return Err(BackupError::TeardownRecoveryDelete);
        }
        preparation
            .revalidate()
            .map_err(|_| BackupError::TeardownRecoveryRead)?;
        if preparation
            .marker()
            .map_err(|_| BackupError::TeardownRecoveryRead)?
            .is_some()
        {
            return Err(BackupError::TeardownRecoveryRead);
        }
        let mut next = self.record.clone();
        next.preparation_ready = true;
        self.update(next)
    }

    pub fn load(lease: Arc<TeardownLease>) -> Result<Option<Self>, BackupError> {
        lease
            .revalidate()
            .map_err(|_| BackupError::TeardownRecoveryRead)?;
        let directory = match FlatDirectory::open(lease.journal_root(), Path::new("backup")) {
            Ok(value) => value,
            Err(FlatDirectoryError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(None);
            }
            Err(_) => return Err(BackupError::TeardownRecoveryRead),
        };
        let Some(observed) =
            read_observed_file_bounded(&directory, OsStr::new(TEARDOWN_RECOVERY_FILENAME), CAP)
                .map_err(|_| BackupError::TeardownRecoveryRead)?
        else {
            return Ok(None);
        };
        let record: Record = serde_json::from_slice(&observed.bytes)
            .map_err(|_| BackupError::TeardownRecoveryCorrupt)?;
        record.validate()?;
        let value = Self {
            lease,
            directory,
            observed,
            record,
            preparation_removal: None,
        };
        value.revalidate()?;
        Ok(Some(value))
    }

    pub fn create(lease: Arc<TeardownLease>, record: Record) -> Result<Self, BackupError> {
        record.validate()?;
        lease
            .revalidate()
            .map_err(|_| BackupError::TeardownRecoveryWrite)?;
        let root = lease.journal_root();
        let directory = FlatDirectory::open(root, Path::new("backup"))
            .map_err(|_| BackupError::TeardownRecoveryWrite)?;
        let metadata = MediaDirectory::open(lease.journal_root().clone(), Path::new("backup"))
            .map_err(|_| BackupError::TeardownRecoveryWrite)?;
        if metadata.identity() != record.preparation.metadata_parent
            || metadata.object_identity() != directory.identity()
        {
            return Err(BackupError::TeardownRecoveryWrite);
        }
        let bytes = serde_json::to_vec(&record).map_err(|_| BackupError::TeardownRecoveryWrite)?;
        if bytes.len() > CAP {
            return Err(BackupError::TeardownRecoveryWrite);
        }
        write_bytes_exclusive_bound(
            &directory,
            OsStr::new(TEARDOWN_RECOVERY_FILENAME),
            &bytes,
            0o600,
        )
        .map_err(|_| BackupError::TeardownRecoveryWrite)?;
        let observed =
            read_observed_file_bounded(&directory, OsStr::new(TEARDOWN_RECOVERY_FILENAME), CAP)
                .map_err(|_| BackupError::TeardownRecoveryRead)?
                .ok_or(BackupError::TeardownRecoveryRead)?;
        if observed.bytes != bytes {
            return Err(BackupError::TeardownRecoveryWrite);
        }
        let value = Self {
            lease,
            directory,
            observed,
            record,
            preparation_removal: None,
        };
        value.revalidate()?;
        Ok(value)
    }

    pub fn record(&self) -> &Record {
        &self.record
    }

    pub fn preparation_removal(
        &self,
    ) -> Option<&Result<ClaimRemovalOutcome, solstone_core_journal_io::ClaimRemovalError>> {
        self.preparation_removal.as_ref()
    }

    /// A durable phase fact plus exact current object/parents and empty-child
    /// census, under this actual new lease. Absence alone never grants this.
    pub fn confirm_carrier_ready(&self) -> Result<&StageObservation, BackupError> {
        self.revalidate()?;
        if !self.record.preparation_ready || self.record.carrier_retired {
            return Err(BackupError::TeardownRecoveryRead);
        }
        let preparation = TeardownPreparation::reopen(self.lease.clone(), &self.record.preparation)
            .map_err(|_| BackupError::TeardownRecoveryRead)?;
        preparation
            .published_directory()
            .map_err(|_| BackupError::TeardownRecoveryRead)?;
        if preparation
            .marker()
            .map_err(|_| BackupError::TeardownRecoveryRead)?
            .is_some()
        {
            return Err(BackupError::TeardownRecoveryRead);
        }
        Ok(&self.record.carrier)
    }

    pub fn revalidate(&self) -> Result<(), BackupError> {
        self.lease
            .revalidate()
            .map_err(|_| BackupError::TeardownRecoveryRead)?;
        self.directory
            .revalidate()
            .map_err(|_| BackupError::TeardownRecoveryRead)?;
        let metadata = MediaDirectory::open(self.lease.journal_root().clone(), Path::new("backup"))
            .map_err(|_| BackupError::TeardownRecoveryRead)?;
        if metadata.identity() != self.record.preparation.metadata_parent
            || metadata.object_identity() != self.directory.identity()
        {
            return Err(BackupError::TeardownRecoveryRead);
        }
        let source = metadata
            .open_child(OsStr::new(PREPARATIONS))
            .map_err(|_| BackupError::TeardownRecoveryRead)?
            .ok_or(BackupError::TeardownRecoveryRead)?;
        if source.identity() != self.record.preparation.source.parent_identity {
            return Err(BackupError::TeardownRecoveryRead);
        }
        let actual = read_observed_file_bounded(
            &self.directory,
            OsStr::new(TEARDOWN_RECOVERY_FILENAME),
            CAP,
        )
        .map_err(|_| BackupError::TeardownRecoveryRead)?
        .ok_or(BackupError::TeardownRecoveryRead)?;
        if actual != self.observed
            || self.record.local.root != self.lease.journal_root().identity().observation_id()
        {
            return Err(BackupError::TeardownRecoveryRead);
        }
        self.lease
            .revalidate()
            .map_err(|_| BackupError::TeardownRecoveryRead)
    }

    pub fn update(&mut self, next: Record) -> Result<(), BackupError> {
        next.validate()?;
        // Immutable destructive cohort and original local/carrier authority.
        let prior = &self.record;
        if next.mode != prior.mode
            || next.local != prior.local
            || next.carrier != prior.carrier
            || next.offload_was_recorded != prior.offload_was_recorded
            || next.preparation != prior.preparation
            || prior.preparation_ready && !next.preparation_ready
            || match (&prior.native, &next.native) {
                (Some(prior_native), Some(next_native)) => {
                    next_native.destination != prior_native.destination
                        || next_native.storage != prior_native.storage
                        || next.origin != prior.origin
                        || next.repository_id != prior.repository_id
                        || next.snapshot_ids != prior.snapshot_ids
                }
                (Some(_), None) => true,
                (None, _) => prior.remote_settled || prior.clear.is_some(),
            }
            || prior.remote_settled && !next.remote_settled
            || prior.clear.is_some() && next.clear.is_none()
            || prior.binding_removed && !next.binding_removed
            || prior.native_retired && !next.native_retired
            || prior.carrier_retired && !next.carrier_retired
        {
            return Err(BackupError::TeardownRecoveryWrite);
        }
        self.revalidate()?;
        let bytes = serde_json::to_vec(&next).map_err(|_| BackupError::TeardownRecoveryWrite)?;
        if bytes.len() > CAP {
            return Err(BackupError::TeardownRecoveryWrite);
        }
        let outcome = atomic_replace_bound(
            &self.directory,
            OsStr::new(TEARDOWN_RECOVERY_FILENAME),
            &bytes,
            0o600,
        )
        .map_err(|_| BackupError::TeardownRecoveryWrite)?;
        let BoundAtomicOutcome::Published { observation } = outcome else {
            return Err(BackupError::TeardownRecoveryWrite);
        };
        self.observed = observation;
        self.record = next;
        self.revalidate()
    }

    pub fn remove(self) -> Result<(), BackupError> {
        if !self.record.complete() {
            return Err(BackupError::TeardownRecoveryDelete);
        }
        self.revalidate()?;
        let claim = ClaimName::parse(&format!(
            "!solstone-claim-{:08x}-{:016x}",
            std::process::id(),
            CLAIM.fetch_add(1, Ordering::Relaxed)
        ))
        .map_err(|_| BackupError::TeardownRecoveryDelete)?;
        let result = claim_and_remove_observed(
            &self.directory,
            OsStr::new(TEARDOWN_RECOVERY_FILENAME),
            &self.observed,
            &claim,
        )
        .map_err(|_| BackupError::TeardownRecoveryDelete)?;
        if result != ClaimRemovalOutcome::Removed {
            return Err(BackupError::TeardownRecoveryDelete);
        }
        self.directory
            .revalidate()
            .map_err(|_| BackupError::TeardownRecoveryDelete)?;
        if read_observed_file_bounded(&self.directory, OsStr::new(TEARDOWN_RECOVERY_FILENAME), CAP)
            .map_err(|_| BackupError::TeardownRecoveryDelete)?
            .is_some()
        {
            return Err(BackupError::TeardownRecoveryDelete);
        }
        self.lease
            .revalidate()
            .map_err(|_| BackupError::TeardownRecoveryDelete)
    }
}

#[cfg(all(test, feature = "full-tests"))]
mod tests;
