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
    ExactArchiveFile, OwnedReadOperation, OwnedSettledDirectRead, ReadOperation, ReadbackError,
    ReadbackFailure, RefusedDirectRead,
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
    cleanup: Cleanup,
}

enum HeldResource {
    Stage(Option<MediaStage>),
    Target(OwnedMediaTarget),
    Prepared(Box<PreparedRead>),
    Readback(Box<FailedRead>),
}

pub(crate) struct ArchiveReadStates<'a> {
    held: std::slice::Iter<'a, HeldResource>,
}

impl ArchiveReadStates<'_> {
    pub(crate) fn empty() -> Self {
        Self { held: [].iter() }
    }
}

impl<'a> Iterator for ArchiveReadStates<'a> {
    type Item = crate::remove_marked::ArchiveReadState<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        use crate::remove_marked::ArchiveReadState;
        self.held.next().map(|held| match held {
            HeldResource::Stage(stage) => ArchiveReadState::Stage(stage.as_ref()),
            HeldResource::Target(target) => ArchiveReadState::Target(target),
            HeldResource::Prepared(prepared) => ArchiveReadState::Prepared {
                file: &prepared.file,
                target_identity: &prepared.target_identity,
                media: &prepared.media,
                cleanup: prepared.cleanup,
            },
            HeldResource::Readback(failed) => ArchiveReadState::Readback {
                file: &failed.file,
                target_identity: &failed.target_identity,
                failure: &failed.failure,
                cleanup: failed.cleanup,
            },
        })
    }
}

struct ApprovalResources {
    carrier: Arc<RootActivityCarrier>,
    expected: Vec<Mark>,
    held: Vec<HeldResource>,
}

struct ApprovalOwner<R> {
    read: R,
    resources: ApprovalResources,
}

pub(crate) struct ArchiveApprovalBatch<'a> {
    owner: Box<ApprovalOwner<ReadOperation<'a>>>,
}

pub(crate) struct ArchiveApprovalFailure<'a> {
    owner: Box<ApprovalOwner<ReadOperation<'a>>>,
    reason: ArchiveApprovalError,
}

impl ApprovalResources {
    fn revalidate_admission(&self, read: &ReadOperation<'_>) -> Result<(), ArchiveApprovalError> {
        if !Arc::ptr_eq(read.carrier(), &self.carrier)
            || !Arc::ptr_eq(read.journal_root(), self.carrier.journal_root())
        {
            return Err(ArchiveApprovalError::RootChanged);
        }
        read.revalidate_admission()
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
    fn ready_for_finish(&self) -> bool {
        !self.held.is_empty() && self.held.iter().all(|held| {
            matches!(held, HeldResource::Prepared(read) if read.cleanup == Some(Cleanup::Complete))
        })
    }
}

/// Borrowed only from a genuine successful batch, never from decoded evidence.
#[derive(Clone, Copy)]
pub(crate) enum ArchiveApprovalRef<'a, 'op> {
    Generic(&'a ArchiveApprovalBatch<'op>),
    Owned(&'a OwnedArchiveApprovalBatch),
}
impl<'a, 'op> ArchiveApprovalRef<'a, 'op> {
    pub(crate) fn journal_root(&self) -> &'a Arc<JournalRoot> {
        match self {
            Self::Generic(batch) => batch.journal_root(),
            Self::Owned(batch) => batch.owner.read.journal_root(),
        }
    }
    pub(crate) fn activity(&self) -> &'a Arc<ActivityLease> {
        match self {
            Self::Generic(batch) => batch.activity(),
            Self::Owned(batch) => batch.owner.read.activity(),
        }
    }
    pub(crate) fn expected_marks(&self) -> &'a [Mark] {
        match self {
            Self::Generic(batch) => batch.expected_marks(),
            Self::Owned(batch) => &batch.owner.resources.expected,
        }
    }
    pub(crate) fn revalidate_admission(&self) -> Result<(), ArchiveApprovalError> {
        match self {
            Self::Generic(batch) => batch.revalidate_admission(),
            Self::Owned(batch) => batch
                .owner
                .resources
                .revalidate_admission(batch.owner.read.as_read()),
        }
    }
}

impl ArchiveApprovalBatch<'_> {
    pub(crate) fn view(&self) -> ArchiveApprovalRef<'_, '_> {
        ArchiveApprovalRef::Generic(self)
    }

    pub(crate) fn read_states(&self) -> ArchiveReadStates<'_> {
        ArchiveReadStates {
            held: self.owner.resources.held.iter(),
        }
    }

    pub(crate) fn journal_root(&self) -> &Arc<JournalRoot> {
        self.owner.resources.carrier.journal_root()
    }

    pub(crate) fn activity(&self) -> &Arc<ActivityLease> {
        self.owner.resources.carrier.activity()
    }

    pub(crate) fn expected_marks(&self) -> &[Mark] {
        &self.owner.resources.expected
    }

    pub(crate) fn revalidate_admission(&self) -> Result<(), ArchiveApprovalError> {
        self.owner.resources.revalidate_admission(&self.owner.read)
    }
}

impl ArchiveApprovalFailure<'_> {
    pub(crate) fn read_states(&self) -> ArchiveReadStates<'_> {
        ArchiveReadStates {
            held: self.owner.resources.held.iter(),
        }
    }

    pub(crate) fn reason(&self) -> &ArchiveApprovalError {
        &self.reason
    }

    pub(crate) fn journal_root(&self) -> &Arc<JournalRoot> {
        self.owner.resources.carrier.journal_root()
    }

    pub(crate) fn activity(&self) -> &Arc<ActivityLease> {
        self.owner.resources.carrier.activity()
    }

    pub(crate) fn expected_marks(&self) -> &[Mark] {
        &self.owner.resources.expected
    }
}

pub(crate) struct OwnedArchiveApproval {
    read: ClosedApprovalRead,
    _carrier: Arc<RootActivityCarrier>,
    expected: Vec<Mark>,
    held: Vec<HeldResource>,
    reason: Option<ArchiveApprovalError>,
}

enum ClosedApprovalRead {
    Read(Box<solstone_core_backup::archive_readback::OwnedDirectReadHold>),
    Held(solstone_core_backup::archive_readback::OwnedHeldDirectRead),
}

impl OwnedArchiveApproval {
    pub(crate) fn carrier(&self) -> &Arc<RootActivityCarrier> {
        match &self.read {
            ClosedApprovalRead::Read(read) => read.carrier(),
            ClosedApprovalRead::Held(read) => read.carrier(),
        }
    }
    pub(crate) fn read_states(&self) -> ArchiveReadStates<'_> {
        ArchiveReadStates {
            held: self.held.iter(),
        }
    }
    pub(crate) fn journal_root(&self) -> &Arc<JournalRoot> {
        match &self.read {
            ClosedApprovalRead::Read(read) => read.carrier().journal_root(),
            ClosedApprovalRead::Held(read) => read.carrier().journal_root(),
        }
    }
    pub(crate) fn reason(&self) -> Option<&ArchiveApprovalError> {
        self.reason.as_ref()
    }
    pub(crate) fn expected_marks(&self) -> &[Mark] {
        &self.expected
    }
    pub(crate) fn readback_error(&self) -> Option<ReadbackError> {
        match &self.read {
            ClosedApprovalRead::Read(_) => None,
            ClosedApprovalRead::Held(read) => Some(read.reason()),
        }
    }
    /// End a refused approval whose every stage is already removed and whose
    /// read launched no child still unreaped. Nothing here grants removal.
    pub(crate) fn release_refused(self) -> Result<RefusedDirectRead, Box<Self>> {
        let removed = self.held.iter().all(|held| match held {
            HeldResource::Stage(stage) => stage.is_none(),
            HeldResource::Target(_) => false,
            HeldResource::Prepared(read) => read.cleanup == Some(Cleanup::Complete),
            HeldResource::Readback(read) => read.cleanup == Cleanup::Complete,
        });
        if !removed {
            return Err(Box::new(self));
        }
        let Self {
            read,
            _carrier,
            expected,
            held,
            reason,
        } = self;
        let released = match read {
            ClosedApprovalRead::Read(read) => {
                (*read).release_refused().map_err(ClosedApprovalRead::Read)
            }
            ClosedApprovalRead::Held(read) => read
                .release_refused()
                .map_err(|read| ClosedApprovalRead::Held(*read)),
        };
        released.map_err(|read| {
            Box::new(Self {
                read,
                _carrier,
                expected,
                held,
                reason,
            })
        })
    }
}

fn close_approval<'a>(
    owner: Box<ApprovalOwner<ReadOperation<'a>>>,
) -> Result<OwnedArchiveApproval, Box<ApprovalOwner<ReadOperation<'a>>>> {
    let ApprovalOwner { read, resources } = *owner;
    match read.into_owned_hold() {
        Ok(read) => Ok(OwnedArchiveApproval {
            read: ClosedApprovalRead::Read(Box::new(read)),
            _carrier: resources.carrier,
            expected: resources.expected,
            held: resources.held,
            reason: None,
        }),
        Err(read) => Err(Box::new(ApprovalOwner {
            read: *read,
            resources,
        })),
    }
}

impl<'a> ArchiveApprovalBatch<'a> {
    pub(crate) fn into_owned_hold(self) -> Result<OwnedArchiveApproval, Self> {
        close_approval(self.owner).map_err(|owner| Self { owner })
    }
    pub(crate) fn ready_for_finish(&self) -> bool {
        self.owner.resources.ready_for_finish()
    }
    pub(crate) fn has_owned_clock(&self) -> bool {
        self.owner.read.owned_clock().is_some()
    }
    pub(crate) fn settle(self) -> ArchiveApprovalFinishing<'a> {
        let ApprovalOwner { read, resources } = *self.owner;
        let result = read.settle_direct();
        let error = result.as_ref().err().map(|read| read.reason());
        ArchiveApprovalFinishing {
            read: match result {
                Ok(read) => FinishingRead::Settled(Box::new(read)),
                Err(read) => FinishingRead::Held(read),
            },
            carrier: resources.carrier,
            expected: resources.expected,
            held: resources.held,
            error,
        }
    }
}

impl ArchiveApprovalFailure<'_> {
    pub(crate) fn into_owned_hold(self) -> Result<OwnedArchiveApproval, Self> {
        let Self { owner, reason } = self;
        match close_approval(owner) {
            Ok(mut closed) => {
                closed.reason = Some(reason);
                Ok(closed)
            }
            Err(owner) => Err(Self { owner, reason }),
        }
    }
}

enum FinishingRead<'a> {
    Settled(Box<solstone_core_backup::archive_readback::SettledDirectRead<'a>>),
    Held(Box<solstone_core_backup::archive_readback::HeldDirectRead<'a>>),
}

pub(crate) struct ArchiveApprovalFinishing<'a> {
    error: Option<ReadbackError>,
    read: FinishingRead<'a>,
    carrier: Arc<RootActivityCarrier>,
    expected: Vec<Mark>,
    held: Vec<HeldResource>,
}

impl<'a> ArchiveApprovalFinishing<'a> {
    pub(crate) fn carrier(&self) -> &Arc<RootActivityCarrier> {
        &self.carrier
    }
    pub(crate) fn expected_marks(&self) -> &[Mark] {
        &self.expected
    }
    pub(crate) fn read_states(&self) -> ArchiveReadStates<'_> {
        ArchiveReadStates {
            held: self.held.iter(),
        }
    }
    pub(crate) fn readback_error(&self) -> Option<&ReadbackError> {
        self.error.as_ref()
    }
    pub(crate) fn into_owned_hold(self) -> Result<OwnedArchiveApproval, Self> {
        let Self {
            read,
            carrier,
            expected,
            held,
            error,
        } = self;
        let read = match read {
            FinishingRead::Settled(read) => match read.into_owned_hold() {
                Ok(read) => ClosedApprovalRead::Read(Box::new(read)),
                Err(read) => {
                    return Err(Self {
                        read: FinishingRead::Settled(read),
                        carrier,
                        expected,
                        held,
                        error,
                    });
                }
            },
            FinishingRead::Held(read) => match read.into_owned_hold() {
                Ok(read) => ClosedApprovalRead::Held(read),
                Err(read) => {
                    return Err(Self {
                        read: FinishingRead::Held(read),
                        carrier,
                        expected,
                        held,
                        error,
                    });
                }
            },
        };
        Ok(OwnedArchiveApproval {
            read,
            _carrier: carrier,
            expected,
            held,
            reason: None,
        })
    }
    pub(crate) fn release(
        self,
    ) -> Result<
        (
            solstone_core_backup::archive_readback::SettledDirectRead<'a>,
            Vec<crate::remove_marked::ReviewedArchiveReadReport>,
        ),
        Self,
    > {
        if !self.held.iter().all(|held| {
            matches!(held,
            HeldResource::Prepared(read) if read.cleanup == Some(Cleanup::Complete))
        }) {
            return Err(self);
        }
        let FinishingRead::Settled(read) = self.read else {
            return Err(self);
        };
        let mut reports = Vec::new();
        for resource in self.held {
            if let HeldResource::Prepared(prepared) = resource {
                reports.push(
                    crate::remove_marked::ReviewedArchiveReadReport::from_actual(
                        prepared.file,
                        prepared.target_identity,
                        prepared.media.digest().clone(),
                        prepared.cleanup,
                    ),
                );
            }
        }
        Ok((*read, reports))
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
/// sequential disposal. A failed read's stage is cleaned only once its writer
/// was reaped. This module never retires a carrier, and both return variants
/// keep the original admission and resources.
enum ApprovalReadMut<'a, 'clock> {
    Generic(&'a mut ReadOperation<'clock>),
    Owned(&'a mut OwnedReadOperation),
}
impl ApprovalReadMut<'_, '_> {
    fn as_read(&self) -> &ReadOperation<'_> {
        match self {
            Self::Generic(read) => read,
            Self::Owned(read) => read.as_read(),
        }
    }
    fn read_exact(
        &mut self,
        file: &ExactArchiveFile,
        target: OwnedMediaTarget,
    ) -> Result<PreparedMedia, ReadbackFailure> {
        match self {
            Self::Generic(read) => read.read_exact(file, target),
            Self::Owned(read) => read.read_exact(file, target),
        }
    }
}

fn fill_approval(
    mut read: ApprovalReadMut<'_, '_>,
    resources: &mut ApprovalResources,
) -> Result<(), ArchiveApprovalError> {
    let files = selected_files(
        &resources.expected,
        &read.as_read().journal_root().identity().observation_id(),
        read.as_read().repository_id(),
    )?;
    resources.revalidate_admission(read.as_read())?;
    let parent = MediaDirectory::open(resources.carrier.journal_root().clone(), Path::new(""))
        .map_err(ArchiveApprovalError::Native)?;
    for file in files {
        resources.revalidate_admission(read.as_read())?;
        let stage = match MediaStage::create(parent.clone(), resources.carrier.activity().clone()) {
            Ok(stage) => stage,
            Err(failure) => {
                resources.held.push(HeldResource::Stage(failure.owned));
                return Err(ArchiveApprovalError::Native(failure.error));
            }
        };
        if let Err(reason) = resources.revalidate_admission(read.as_read()) {
            resources.held.push(HeldResource::Stage(Some(stage)));
            return Err(reason);
        }
        let target = match stage.create_target() {
            Ok(target) => target,
            Err(failure) => {
                resources
                    .held
                    .push(HeldResource::Stage(Some(failure.owned)));
                return Err(ArchiveApprovalError::Native(failure.error));
            }
        };
        if let Err(reason) = resources.revalidate_admission(read.as_read()) {
            resources.held.push(HeldResource::Target(target));
            return Err(reason);
        }
        let target_identity = target.identity();
        let media = match read.read_exact(&file, target) {
            Ok(media) => media,
            Err(mut failure) => {
                let reason = ArchiveApprovalError::Readback(failure.reason());
                // Nothing was published from this unpublished copy. Its cleanup
                // refuses unless the writer is known to have been reaped.
                let cleanup = failure.cleanup();
                resources
                    .held
                    .push(HeldResource::Readback(Box::new(FailedRead {
                        file,
                        target_identity,
                        failure,
                        cleanup,
                    })));
                return Err(reason);
            }
        };
        let mut prepared = Box::new(PreparedRead {
            file,
            target_identity,
            media,
            cleanup: None,
        });
        if let Err(reason) = resources.revalidate_admission(read.as_read()) {
            resources.held.push(HeldResource::Prepared(prepared));
            return Err(reason);
        }
        let cleanup = prepared.media.cleanup();
        prepared.cleanup = Some(cleanup);
        resources.held.push(HeldResource::Prepared(prepared));
        if cleanup != Cleanup::Complete {
            return Err(ArchiveApprovalError::Cleanup(cleanup));
        }
        resources.revalidate_admission(read.as_read())?;
    }
    Ok(())
}

pub(crate) fn create_archive_approval_batch<'a>(
    read: ReadOperation<'a>,
    carrier: Arc<RootActivityCarrier>,
    expected: &[Mark],
) -> Result<ArchiveApprovalBatch<'a>, ArchiveApprovalFailure<'a>> {
    let mut owner = Box::new(ApprovalOwner {
        read,
        resources: ApprovalResources {
            carrier,
            expected: expected.to_vec(),
            held: Vec::new(),
        },
    });
    match fill_approval(
        ApprovalReadMut::Generic(&mut owner.read),
        &mut owner.resources,
    ) {
        Ok(()) => Ok(ArchiveApprovalBatch { owner }),
        Err(reason) => Err(ArchiveApprovalFailure { owner, reason }),
    }
}

pub(crate) struct OwnedArchiveApprovalBatch {
    owner: Box<ApprovalOwner<OwnedReadOperation>>,
}
pub(crate) struct OwnedArchiveApprovalFailure {
    owner: Box<ApprovalOwner<OwnedReadOperation>>,
    reason: ArchiveApprovalError,
}

pub(crate) fn create_owned_archive_approval_batch(
    read: OwnedReadOperation,
    expected: &[Mark],
) -> Result<OwnedArchiveApprovalBatch, OwnedArchiveApprovalFailure> {
    let carrier = Arc::clone(read.carrier());
    let mut owner = Box::new(ApprovalOwner {
        read,
        resources: ApprovalResources {
            carrier,
            expected: expected.to_vec(),
            held: Vec::new(),
        },
    });
    match fill_approval(
        ApprovalReadMut::Owned(&mut owner.read),
        &mut owner.resources,
    ) {
        Ok(()) => Ok(OwnedArchiveApprovalBatch { owner }),
        Err(reason) => Err(OwnedArchiveApprovalFailure { owner, reason }),
    }
}

fn close_owned_approval(
    owner: ApprovalOwner<OwnedReadOperation>,
    reason: Option<ArchiveApprovalError>,
) -> OwnedArchiveApproval {
    OwnedArchiveApproval {
        read: ClosedApprovalRead::Read(Box::new(owner.read.into_owned_hold())),
        _carrier: owner.resources.carrier,
        expected: owner.resources.expected,
        held: owner.resources.held,
        reason,
    }
}
impl OwnedArchiveApprovalBatch {
    pub(crate) fn view(&self) -> ArchiveApprovalRef<'_, 'static> {
        ArchiveApprovalRef::Owned(self)
    }
    pub(crate) fn read_states(&self) -> ArchiveReadStates<'_> {
        ArchiveReadStates {
            held: self.owner.resources.held.iter(),
        }
    }
    pub(crate) fn ready_for_finish(&self) -> bool {
        self.owner.resources.ready_for_finish()
    }
    pub(crate) fn into_owned_hold(self) -> OwnedArchiveApproval {
        close_owned_approval(*self.owner, None)
    }
    pub(crate) fn settle(self) -> OwnedArchiveApprovalFinishing {
        let ApprovalOwner { read, resources } = *self.owner;
        OwnedArchiveApprovalFinishing {
            read: read.settle_direct().map(Box::new),
            resources,
        }
    }
}
impl OwnedArchiveApprovalFailure {
    pub(crate) fn journal_root(&self) -> &Arc<JournalRoot> {
        self.owner.read.journal_root()
    }
    pub(crate) fn activity(&self) -> &Arc<ActivityLease> {
        self.owner.read.activity()
    }
    pub(crate) fn expected_marks(&self) -> &[Mark] {
        &self.owner.resources.expected
    }

    pub(crate) fn read_states(&self) -> ArchiveReadStates<'_> {
        ArchiveReadStates {
            held: self.owner.resources.held.iter(),
        }
    }
    pub(crate) fn reason(&self) -> &ArchiveApprovalError {
        &self.reason
    }
    pub(crate) fn into_owned_hold(self) -> OwnedArchiveApproval {
        close_owned_approval(*self.owner, Some(self.reason))
    }
}

pub(crate) struct OwnedArchiveApprovalFinishing {
    read: Result<
        Box<OwnedSettledDirectRead>,
        solstone_core_backup::archive_readback::OwnedHeldDirectRead,
    >,
    resources: ApprovalResources,
}
impl OwnedArchiveApprovalFinishing {
    pub(crate) fn carrier(&self) -> &Arc<RootActivityCarrier> {
        &self.resources.carrier
    }
    pub(crate) fn readback_error(&self) -> Option<ReadbackError> {
        self.read.as_ref().err().map(|held| held.reason())
    }
    pub(crate) fn into_owned_hold(self) -> OwnedArchiveApproval {
        let read = match self.read {
            Ok(read) => ClosedApprovalRead::Read(Box::new(read.into_owned_hold())),
            Err(read) => ClosedApprovalRead::Held(read),
        };
        OwnedArchiveApproval {
            read,
            _carrier: self.resources.carrier,
            expected: self.resources.expected,
            held: self.resources.held,
            reason: None,
        }
    }
    pub(crate) fn release(
        self,
    ) -> Result<
        (
            OwnedSettledDirectRead,
            Vec<crate::remove_marked::ReviewedArchiveReadReport>,
        ),
        Self,
    > {
        if !self.resources.ready_for_finish() {
            return Err(self);
        }
        let Ok(read) = self.read else {
            return Err(self);
        };
        let reports = self
            .resources
            .held
            .into_iter()
            .filter_map(|resource| {
                if let HeldResource::Prepared(prepared) = resource {
                    Some(
                        crate::remove_marked::ReviewedArchiveReadReport::from_actual(
                            prepared.file,
                            prepared.target_identity,
                            prepared.media.digest().clone(),
                            prepared.cleanup,
                        ),
                    )
                } else {
                    None
                }
            })
            .collect();
        Ok((*read, reports))
    }
}

#[cfg(test)]
mod tests;
