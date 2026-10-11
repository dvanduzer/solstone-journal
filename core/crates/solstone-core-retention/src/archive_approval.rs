// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Private, operation-owned exact archive confirmation. This supplies no release
//! authority. Its consumer must establish actual namespace cooperation and
//! child/HTTP settlement before success-only disposal; a lease cannot certify
//! those runtime facts. Unqualified cooperation holds production invocation.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::io;
use std::path::Path;
use std::sync::Arc;

use solstone_core_backup::archive_readback::{
    ExactArchiveFile, ReadOperation, ReadbackError, ReadbackFailure,
};
use solstone_core_journal_io::JournalRoot;
use solstone_core_journal_io::activity::ActivityLease;
use solstone_core_journal_io::media::{
    Cleanup, MediaDirectory, MediaIdentity, MediaStage, OwnedMediaTarget, PreparedMedia,
    RootActivityCarrier,
};

use crate::marks::{Mark, MarkState, Register, RemovalClass, validate};

#[derive(Debug)]
pub(crate) enum ArchiveApprovalError {
    InvalidSelection,
    SourceRootChanged,
    RepositoryChanged,
    RootChanged,
    Native(io::Error),
    Readback(ReadbackError),
    Cleanup(Cleanup),
}

struct PreparedRead {
    file: ExactArchiveFile,
    target_identity: MediaIdentity,
    media: PreparedMedia,
    cleanup: Option<Cleanup>,
}

struct FailedRead {
    file: ExactArchiveFile,
    target_identity: MediaIdentity,
    failure: ReadbackFailure,
}

enum HeldResource {
    Stage(Option<MediaStage>),
    Target(OwnedMediaTarget),
    Prepared(Box<PreparedRead>),
    Readback(Box<FailedRead>),
}

struct ApprovalOwner<'a> {
    read: ReadOperation<'a>,
    carrier: RootActivityCarrier,
    expected: Vec<Mark>,
    held: Vec<HeldResource>,
}

pub(crate) struct ArchiveApprovalBatch<'a> {
    owner: Box<ApprovalOwner<'a>>,
}

pub(crate) struct ArchiveApprovalFailure<'a> {
    owner: Box<ApprovalOwner<'a>>,
    reason: ArchiveApprovalError,
}

impl ApprovalOwner<'_> {
    fn revalidate_admission(&self) -> Result<(), ArchiveApprovalError> {
        if !Arc::ptr_eq(self.read.journal_root(), self.carrier.journal_root()) {
            return Err(ArchiveApprovalError::RootChanged);
        }
        self.read
            .revalidate_admission()
            .map_err(ArchiveApprovalError::Readback)?;
        self.carrier
            .journal_root()
            .revalidate_canonical_binding()
            .map_err(|error| ArchiveApprovalError::Native(io::Error::other(error)))?;
        self.carrier
            .activity()
            .revalidate()
            .map_err(|error| ArchiveApprovalError::Native(io::Error::other(error)))?;
        self.carrier
            .revalidate()
            .map_err(ArchiveApprovalError::Native)
    }
}

impl ArchiveApprovalBatch<'_> {
    pub(crate) fn journal_root(&self) -> &Arc<JournalRoot> {
        self.owner.carrier.journal_root()
    }

    pub(crate) fn activity(&self) -> &Arc<ActivityLease> {
        self.owner.carrier.activity()
    }

    pub(crate) fn expected_marks(&self) -> &[Mark] {
        &self.owner.expected
    }

    pub(crate) fn revalidate_admission(&self) -> Result<(), ArchiveApprovalError> {
        self.owner.revalidate_admission()
    }
}

impl ArchiveApprovalFailure<'_> {
    pub(crate) fn reason(&self) -> &ArchiveApprovalError {
        &self.reason
    }

    pub(crate) fn journal_root(&self) -> &Arc<JournalRoot> {
        self.owner.carrier.journal_root()
    }

    pub(crate) fn activity(&self) -> &Arc<ActivityLease> {
        self.owner.carrier.activity()
    }

    pub(crate) fn expected_marks(&self) -> &[Mark] {
        &self.owner.expected
    }
}

fn selected_files(
    expected: &[Mark],
    source_observation: &str,
    repository_id: &str,
) -> Result<Vec<ExactArchiveFile>, ArchiveApprovalError> {
    if expected.is_empty() {
        return Err(ArchiveApprovalError::InvalidSelection);
    }
    let mut register = Register {
        version: 2,
        marks: BTreeMap::new(),
    };
    for mark in expected {
        if mark.class != RemovalClass::OffloadRawRelease
            || !matches!(&mark.state, MarkState::Marked)
            || mark.offload.is_none()
        {
            return Err(ArchiveApprovalError::InvalidSelection);
        }
        match register.marks.entry(mark.id.clone()) {
            Entry::Occupied(_) => return Err(ArchiveApprovalError::InvalidSelection),
            Entry::Vacant(entry) => {
                entry.insert(mark.clone());
            }
        }
    }
    validate(&register).map_err(|_| ArchiveApprovalError::InvalidSelection)?;
    let mut files = Vec::new();
    for mark in expected {
        let offer = mark
            .offload
            .as_ref()
            .ok_or(ArchiveApprovalError::InvalidSelection)?;
        if offer.proof.journal_id != source_observation {
            return Err(ArchiveApprovalError::SourceRootChanged);
        }
        if offer.proof.repository_id != repository_id {
            return Err(ArchiveApprovalError::RepositoryChanged);
        }
        for offered in &offer.proof.files {
            let file = ExactArchiveFile {
                repository_id: offer.proof.repository_id.clone(),
                snapshot_id: offer.proof.snapshot_id.clone(),
                name: offered.name.clone(),
                archive_path: offered.archive_path.clone(),
                bytes: offered.bytes,
                sha256: offered.sha256.clone(),
            };
            file.validate()
                .map_err(|_| ArchiveApprovalError::InvalidSelection)?;
            files.push(file);
        }
    }
    Ok(files)
}

/// The parent owns real namespace cooperation and child/HTTP settlement before
/// entering this composition. Only actual readback success permits explicit
/// sequential disposal. This module never cleans a failed writer or retires a
/// carrier, and both return variants keep the original admission and resources.
pub(crate) fn create_archive_approval_batch<'a>(
    read: ReadOperation<'a>,
    carrier: RootActivityCarrier,
    expected: &[Mark],
) -> Result<ArchiveApprovalBatch<'a>, ArchiveApprovalFailure<'a>> {
    let mut owner = Box::new(ApprovalOwner {
        read,
        carrier,
        expected: expected.to_vec(),
        held: Vec::new(),
    });
    let files = match selected_files(
        &owner.expected,
        &owner.read.journal_root().identity().observation_id(),
        owner.read.repository_id(),
    ) {
        Ok(files) => files,
        Err(reason) => return Err(ArchiveApprovalFailure { owner, reason }),
    };
    if let Err(reason) = owner.revalidate_admission() {
        return Err(ArchiveApprovalFailure { owner, reason });
    }
    let parent = match MediaDirectory::open(owner.carrier.journal_root().clone(), Path::new("")) {
        Ok(parent) => parent,
        Err(error) => {
            return Err(ArchiveApprovalFailure {
                owner,
                reason: ArchiveApprovalError::Native(error),
            });
        }
    };
    for file in files {
        if let Err(reason) = owner.revalidate_admission() {
            return Err(ArchiveApprovalFailure { owner, reason });
        }
        let stage = match MediaStage::create(parent.clone(), owner.carrier.activity().clone()) {
            Ok(stage) => stage,
            Err(failure) => {
                owner.held.push(HeldResource::Stage(failure.owned));
                return Err(ArchiveApprovalFailure {
                    owner,
                    reason: ArchiveApprovalError::Native(failure.error),
                });
            }
        };
        if let Err(reason) = owner.revalidate_admission() {
            owner.held.push(HeldResource::Stage(Some(stage)));
            return Err(ArchiveApprovalFailure { owner, reason });
        }
        let target = match stage.create_target() {
            Ok(target) => target,
            Err(failure) => {
                owner.held.push(HeldResource::Stage(Some(failure.owned)));
                return Err(ArchiveApprovalFailure {
                    owner,
                    reason: ArchiveApprovalError::Native(failure.error),
                });
            }
        };
        if let Err(reason) = owner.revalidate_admission() {
            owner.held.push(HeldResource::Target(target));
            return Err(ArchiveApprovalFailure { owner, reason });
        }
        let target_identity = target.identity();
        let media = match owner.read.read_exact(&file, target) {
            Ok(media) => media,
            Err(failure) => {
                let reason = ArchiveApprovalError::Readback(failure.reason());
                owner.held.push(HeldResource::Readback(Box::new(FailedRead {
                    file,
                    target_identity,
                    failure,
                })));
                return Err(ArchiveApprovalFailure { owner, reason });
            }
        };
        let mut prepared = Box::new(PreparedRead {
            file,
            target_identity,
            media,
            cleanup: None,
        });
        if let Err(reason) = owner.revalidate_admission() {
            owner.held.push(HeldResource::Prepared(prepared));
            return Err(ArchiveApprovalFailure { owner, reason });
        }
        let cleanup = prepared.media.cleanup();
        prepared.cleanup = Some(cleanup);
        owner.held.push(HeldResource::Prepared(prepared));
        if cleanup != Cleanup::Complete {
            return Err(ArchiveApprovalFailure {
                owner,
                reason: ArchiveApprovalError::Cleanup(cleanup),
            });
        }
        // This check also runs for the final file: completion of cleanup cannot
        // bypass the original deadline/cancellation or retained root admission.
        if let Err(reason) = owner.revalidate_admission() {
            return Err(ArchiveApprovalFailure { owner, reason });
        }
    }
    Ok(ArchiveApprovalBatch { owner })
}

#[cfg(test)]
mod tests;
