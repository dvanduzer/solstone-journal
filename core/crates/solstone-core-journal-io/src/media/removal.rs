// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;

pub(super) enum ClaimFailure {
    Occupied(io::Error),
    Uncertain(io::Error),
}

impl ClaimFailure {
    pub(super) fn native(error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::AlreadyExists {
            Self::Occupied(error)
        } else {
            Self::Uncertain(error)
        }
    }
}

impl From<io::Error> for ClaimFailure {
    fn from(error: io::Error) -> Self {
        Self::Uncertain(error)
    }
}

/// Persistable observations, never capabilities for replay or cleanup.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageObservation {
    pub journal_observation: String,
    pub parent_relative: Vec<String>,
    pub parent_identity: MediaIdentity,
    pub directory_name: String,
    pub directory_identity: MediaIdentity,
}

impl MediaStage {
    pub fn observation(&self) -> io::Result<StageObservation> {
        self.parent.revalidate()?;
        let directory = self.directory.as_ref().ok_or_else(changed)?;
        directory.revalidate()?;
        let root = self.parent.inner.journal_root();
        let relative = self
            .parent
            .diagnostic_path()
            .strip_prefix(root.canonical_path())
            .map_err(|_| changed())?;
        let parent_relative = components(relative)?
            .into_iter()
            .map(|name| name.into_string().map_err(|_| changed()))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(StageObservation {
            journal_observation: root.identity().observation_id(),
            parent_relative,
            parent_identity: self.parent.identity(),
            directory_name: self.name.to_str().ok_or_else(changed)?.to_owned(),
            directory_identity: directory.identity(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovalEffect {
    NotClaimed,
    ClaimUncertain,
    Claimed,
    DeletionUncertain,
    DeletePending,
    Deleted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginalName {
    Unverified,
    Absent,
    Occupied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemovalPhase {
    Mutation,
    FileSync,
    SourceSync,
    ClaimSync,
    StageParentSync,
    Cleanup,
    Completion,
}

#[derive(Debug)]
pub struct RemovalFailure {
    pub phase: RemovalPhase,
    pub error: io::Error,
}

/// Facts are independent. Neither pending nor uncertain effects earn deletion credit.
#[derive(Debug)]
pub struct RemovalReceipt {
    pub effect: RemovalEffect,
    pub original_name: OriginalName,
    pub claim_confirmation: Confirmation,
    pub file_durability: Durability,
    pub source_durability: Durability,
    pub claim_durability: Durability,
    pub stage_parent_durability: Durability,
    pub cleanup: Cleanup,
    pub claimed_identity: Option<MediaIdentity>,
    /// This leaf never attempts restoration or restart replay.
    pub restoration_attempted: bool,
    pub failures: Vec<RemovalFailure>,
}

/// Owns every retained claim. Drop only closes handles; no stage extraction exists.
/// Production use requires an owning durable preclaim obligation and stage census.
pub struct MediaRemoval {
    source: Arc<MediaDirectory>,
    name: OsString,
    expected: MediaObservation,
    stage: MediaStage,
    source_file: Option<File>,
    claim_file: Option<File>,
    stage_observation: Option<StageObservation>,
    receipt: RemovalReceipt,
}

impl MediaDirectory {
    /// Neutral effect leaf; policy admission and exclusion belong to the caller.
    pub fn remove_observed(
        self: &Arc<Self>,
        name: &OsStr,
        expected: &MediaObservation,
        stage: MediaStage,
    ) -> MediaRemoval {
        let mut removal = MediaRemoval {
            source: Arc::clone(self),
            name: name.to_owned(),
            expected: expected.clone(),
            stage,
            source_file: None,
            claim_file: None,
            stage_observation: None,
            receipt: RemovalReceipt {
                effect: RemovalEffect::NotClaimed,
                original_name: OriginalName::Unverified,
                claim_confirmation: Confirmation::Unverified,
                file_durability: Durability::Unproven,
                source_durability: Durability::Unproven,
                claim_durability: Durability::Unproven,
                stage_parent_durability: Durability::Unproven,
                cleanup: Cleanup::Pending,
                claimed_identity: None,
                restoration_attempted: false,
                failures: Vec::new(),
            },
        };
        if let Err(error) = removal.perform() {
            removal.receipt.claim_confirmation = Confirmation::Unverified;
            removal.failure(RemovalPhase::Mutation, error);
        }
        removal.finish();
        removal
    }
}

impl MediaRemoval {
    pub fn receipt(&self) -> &RemovalReceipt {
        &self.receipt
    }

    pub fn stage_observation(&self) -> Option<&StageObservation> {
        self.stage_observation.as_ref()
    }

    fn failure(&mut self, phase: RemovalPhase, error: io::Error) {
        self.receipt.failures.push(RemovalFailure { phase, error });
    }

    fn validate_context(&self) -> io::Result<()> {
        self.source.revalidate()?;
        self.stage.parent.revalidate()?;
        let directory = self.stage.directory.as_ref().ok_or_else(changed)?;
        directory.revalidate()?;
        let source_root = self.source.inner.journal_root();
        let stage_root = self.stage.parent.inner.journal_root();
        if source_root.identity() != stage_root.identity()
            || source_root.canonical_path() != stage_root.canonical_path()
            || self.source.diagnostic_path().parent() != Some(self.stage.parent.diagnostic_path())
            || !self.source.identity().same_volume(directory.identity())
        {
            return Err(invalid("removal stage is not a same-volume sibling"));
        }
        let source_name = self
            .source
            .diagnostic_path()
            .file_name()
            .ok_or_else(changed)?;
        let child = self
            .stage
            .parent
            .open_child(source_name)?
            .ok_or_else(changed)?;
        if child.identity() != self.source.identity() {
            return Err(changed());
        }
        self.source.revalidate()
    }

    fn check_file(
        directory: &MediaDirectory,
        name: &OsStr,
        file: &mut File,
        expected: &MediaObservation,
    ) -> io::Result<()> {
        directory.revalidate()?;
        if platform::identity(file)? != expected.identity {
            return Err(changed());
        }
        let stamp = platform::stamp(file)?;
        file.seek(SeekFrom::Start(0))?;
        let digest = stream(file, &mut io::sink(), Some(expected.digest.size))?;
        if digest != expected.digest
            || platform::stamp(file)? != stamp
            || platform::identity(file)? != expected.identity
        {
            return Err(changed());
        }
        platform::check_leaf(&directory.inner, name, expected.identity)?;
        directory.revalidate()
    }

    fn perform(&mut self) -> io::Result<()> {
        plain_name(&self.name)?;
        platform::supported_mutation()?;
        if self.stage.leaf.is_some() || self.stage.removed {
            return Err(invalid("removal requires an unused empty stage"));
        }
        self.validate_context()?;
        let directory = self.stage.directory.as_ref().ok_or_else(changed)?;
        if directory.entries(0)? != Some(Vec::new()) {
            return Err(invalid("removal stage is occupied"));
        }
        self.stage_observation = Some(self.stage.observation()?);
        self.source_file = platform::open_removal(&self.source.inner, &self.name)?;
        Self::check_file(
            &self.source,
            &self.name,
            self.source_file.as_mut().ok_or_else(changed)?,
            &self.expected,
        )?;
        step(MediaPrimitive::BeforeRemovalClaim)?;
        self.validate_context()?;
        Self::check_file(
            &self.source,
            &self.name,
            self.source_file.as_mut().ok_or_else(changed)?,
            &self.expected,
        )?;
        let directory = self.stage.directory.as_ref().ok_or_else(changed)?;
        // Record uncertainty before a syscall, including applied-but-error outcomes.
        self.receipt.effect = RemovalEffect::ClaimUncertain;
        match platform::claim(
            &self.source.inner,
            &self.name,
            self.source_file.as_ref().ok_or_else(changed)?,
            &directory.inner,
            &self.name,
        ) {
            Ok(()) => self.receipt.effect = RemovalEffect::Claimed,
            Err(ClaimFailure::Occupied(error)) => {
                self.receipt.effect = RemovalEffect::NotClaimed;
                return Err(error);
            }
            Err(ClaimFailure::Uncertain(error)) => return Err(error),
        }
        step(MediaPrimitive::AfterRemovalClaim)?;
        step(MediaPrimitive::BeforeClaimRead)?;
        self.validate_context()?;
        self.claim_file = platform::open_plain(&directory.inner, &self.name)?;
        let claim = self.claim_file.as_mut().ok_or_else(changed)?;
        self.receipt.claimed_identity = Some(platform::identity(claim)?);
        Self::check_file(directory, &self.name, claim, &self.expected)?;
        self.receipt.claim_confirmation = Confirmation::Confirmed;
        step(MediaPrimitive::BeforeClaimDelete)?;
        self.validate_context()?;
        let directory = self.stage.directory.as_ref().ok_or_else(changed)?;
        Self::check_file(
            directory,
            &self.name,
            self.claim_file.as_mut().ok_or_else(changed)?,
            &self.expected,
        )?;
        // Also retain and verify the original actual file through the final cut.
        if platform::identity(self.source_file.as_ref().ok_or_else(changed)?)?
            != self.expected.identity
        {
            return Err(changed());
        }
        #[cfg(unix)]
        match step(MediaPrimitive::FileSync)
            .and_then(|()| self.source_file.as_ref().ok_or_else(changed)?.sync_all())
        {
            Ok(()) => self.receipt.file_durability = Durability::Synced,
            Err(error) => self.receipt.failures.push(RemovalFailure {
                phase: RemovalPhase::FileSync,
                error,
            }),
        }
        Self::check_file(
            directory,
            &self.name,
            self.claim_file.as_mut().ok_or_else(changed)?,
            &self.expected,
        )?;
        self.receipt.effect = RemovalEffect::DeletionUncertain;
        platform::delete_claim(
            &directory.inner,
            &self.name,
            self.source_file.as_ref().ok_or_else(changed)?,
        )?;
        self.receipt.effect = RemovalEffect::DeletePending;
        self.receipt.claim_confirmation = Confirmation::Unverified;
        step(MediaPrimitive::AfterClaimDelete)?;
        // Windows delete-pending acceptance is not absence while handles remain.
        self.source_file = None;
        self.claim_file = None;
        platform::ensure_absent(&directory.inner, &self.name)?;
        self.validate_context()?;
        step(MediaPrimitive::RemovalCompletion)?;
        platform::ensure_absent(&directory.inner, &self.name)?;
        self.validate_context()?;
        self.receipt.effect = RemovalEffect::Deleted;
        self.receipt.claim_confirmation = Confirmation::Confirmed;
        Ok(())
    }

    fn sync(directory: &MediaDirectory, primitive: MediaPrimitive) -> io::Result<()> {
        directory.revalidate()?;
        step(primitive)?;
        directory.inner.sync()?;
        directory.revalidate()
    }

    fn finish(&mut self) {
        if self.receipt.effect != RemovalEffect::NotClaimed {
            match Self::sync(&self.source, MediaPrimitive::RemovalSourceSync) {
                Ok(()) => self.receipt.source_durability = Durability::Synced,
                Err(error) => self.failure(RemovalPhase::SourceSync, error),
            }
            if let Some(directory) = &self.stage.directory {
                match Self::sync(directory, MediaPrimitive::RemovalClaimSync) {
                    Ok(()) => self.receipt.claim_durability = Durability::Synced,
                    Err(error) => self.failure(RemovalPhase::ClaimSync, error),
                }
            }
        }
        self.cleanup();
        match Self::sync(&self.stage.parent, MediaPrimitive::RemovalParentSync) {
            Ok(()) => self.receipt.stage_parent_durability = Durability::Synced,
            Err(error) => self.failure(RemovalPhase::StageParentSync, error),
        }
        match plain_name(&self.name)
            .and_then(|()| self.source.revalidate())
            .and_then(|()| platform::entry(&self.source.inner, &self.name))
        {
            Ok(None) => self.receipt.original_name = OriginalName::Absent,
            Ok(Some(_)) => self.receipt.original_name = OriginalName::Occupied,
            Err(error) => {
                self.receipt.original_name = OriginalName::Unverified;
                self.failure(RemovalPhase::Completion, error);
            }
        }
        if let Err(error) = self.stage.parent.revalidate() {
            self.receipt.claim_confirmation = Confirmation::Unverified;
            self.failure(RemovalPhase::Completion, error);
        }
        let stage_confirmation = (|| {
            self.stage.parent.revalidate()?;
            if self.stage.removed {
                platform::ensure_absent(&self.stage.parent.inner, &self.stage.name)
            } else {
                let directory = self.stage.directory.as_ref().ok_or_else(changed)?;
                directory.revalidate()?;
                if self.receipt.effect == RemovalEffect::Deleted {
                    platform::ensure_absent(&directory.inner, &self.name)?;
                }
                Ok(())
            }
        })();
        if let Err(error) = stage_confirmation {
            self.receipt.claim_confirmation = Confirmation::Unverified;
            self.receipt.cleanup = Cleanup::Refused;
            self.failure(RemovalPhase::Completion, error);
        }
    }

    /// Only known empty stages may be retired. Never retries a claim deletion.
    pub fn cleanup(&mut self) -> Cleanup {
        if !matches!(
            self.receipt.effect,
            RemovalEffect::NotClaimed | RemovalEffect::Deleted
        ) {
            self.receipt.cleanup = Cleanup::Refused;
            return Cleanup::Refused;
        }
        if self.stage.removed {
            match self
                .stage
                .parent
                .revalidate()
                .and_then(|()| platform::ensure_absent(&self.stage.parent.inner, &self.stage.name))
            {
                Ok(()) => {
                    self.receipt.cleanup = Cleanup::Complete;
                    return Cleanup::Complete;
                }
                Err(error) => {
                    self.receipt.claim_confirmation = Confirmation::Unverified;
                    self.receipt.cleanup = Cleanup::Refused;
                    self.failure(RemovalPhase::Cleanup, error);
                    return Cleanup::Refused;
                }
            }
        }
        if let Some(directory) = &self.stage.directory {
            match directory.entries(0) {
                Ok(Some(entries)) if entries.is_empty() => {}
                Ok(_) => {
                    self.receipt.cleanup = Cleanup::Refused;
                    return Cleanup::Refused;
                }
                Err(error) => {
                    self.failure(RemovalPhase::Cleanup, error);
                    self.receipt.cleanup = Cleanup::Refused;
                    return Cleanup::Refused;
                }
            }
        }
        self.receipt.stage_parent_durability = Durability::Unproven;
        let cleanup = self.stage.cleanup();
        self.receipt.cleanup = cleanup;
        cleanup
    }
}
