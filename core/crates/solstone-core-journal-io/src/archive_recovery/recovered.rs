// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use crate::activity::{ActivityAdmissionError, ActivityLease};
use crate::activity_obligations::{
    self, IncompleteRecoveryObservation, IntentRead, MAX_INTENT_BYTES, NativeObligation,
    NativeObligations,
};
use crate::archive_origin::{LocalArchiveOrigin, OriginObservationFailure};
use crate::journal_root::{JournalEntryKind, JournalRoot};
use crate::media::{MediaDirectory, MediaRead, RootCarrierObservation};
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::Arc;

mod retirement;
pub use retirement::RecoveredArchiveRetirement;

/// Physical refusal only. Intent semantics remain the consuming domain's work.
#[derive(Debug)]
pub enum RecoveryAdmissionError {
    Unsupported,
    InvalidSelector,
    SameBoot,
    Activity(ActivityAdmissionError),
    Origin(OriginError),
    OriginObservation(Box<OriginObservationFailure>),
    Native(io::Error),
    LockChanged { current: RecoveryLockObservation },
    IncompleteCensus,
}

impl fmt::Display for RecoveryAdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Activity(error) => error.fmt(f),
            Self::Origin(error) => error.fmt(f),
            Self::OriginObservation(failure) => failure.error().fmt(f),
            Self::Native(_) => f.write_str("recovered reservation native validation failed"),
            Self::LockChanged { .. } => f.write_str("recorded activity entry changed"),
            Self::Unsupported => f.write_str("recovered reservation is unsupported"),
            Self::InvalidSelector => f.write_str("invalid recovered record selector"),
            Self::SameBoot => f.write_str("recovered reservation requires a different boot"),
            Self::IncompleteCensus => f.write_str("recovered reservation census is incomplete"),
        }
    }
}

impl Error for RecoveryAdmissionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Activity(error) => Some(error),
            Self::Origin(error) => Some(error),
            Self::OriginObservation(failure) => Some(failure.error()),
            Self::Native(error) => Some(error),
            _ => None,
        }
    }
}

struct Resources {
    root: Arc<JournalRoot>,
    binding: ArchiveRecoveryBinding,
    operation_id: String,
    record_name: Option<OsString>,
    activity: Option<Arc<ActivityLease>>,
    origin: Option<LocalArchiveOrigin>,
    #[cfg(target_os = "linux")]
    root_directory: Option<Arc<MediaDirectory>>,
    #[cfg(target_os = "linux")]
    reservation: Option<Arc<MediaDirectory>>,
    record_chain: Vec<Arc<MediaDirectory>>,
    record: Option<MediaRead>,
    reread: Option<MediaRead>,
    current_lock: Option<RecoveryLockObservation>,
    initial: Option<NativeObligations>,
    final_census: Option<NativeObligations>,
    incomplete: Option<Box<IncompleteRecoveryObservation>>,
}

impl Resources {
    fn new(root: Arc<JournalRoot>, operation_id: &str, binding: ArchiveRecoveryBinding) -> Self {
        let record_name = activity_obligations::intent_filename(operation_id);
        Self {
            root,
            binding,
            operation_id: if record_name.is_some() {
                operation_id.into()
            } else {
                String::new()
            },
            record_name,
            activity: None,
            origin: None,
            #[cfg(target_os = "linux")]
            root_directory: None,
            #[cfg(target_os = "linux")]
            reservation: None,
            record_chain: Vec::new(),
            record: None,
            reread: None,
            current_lock: None,
            initial: None,
            final_census: None,
            incomplete: None,
        }
    }

    fn native(message: &'static str) -> RecoveryAdmissionError {
        RecoveryAdmissionError::Native(io::Error::new(io::ErrorKind::InvalidData, message))
    }

    fn activity(&self) -> &Arc<ActivityLease> {
        self.activity
            .as_ref()
            .expect("actual recovered lease retained")
    }

    #[cfg(target_os = "linux")]
    fn revalidate(&self) -> Result<(), RecoveryAdmissionError> {
        self.revalidate_guards()?;
        self.revalidate_directories()?;
        self.activity()
            .revalidate()
            .map_err(RecoveryAdmissionError::Activity)
    }

    #[cfg(target_os = "linux")]
    fn revalidate_guards(&self) -> Result<(), RecoveryAdmissionError> {
        self.activity()
            .revalidate()
            .map_err(RecoveryAdmissionError::Activity)?;
        self.origin
            .as_ref()
            .expect("actual current origin retained")
            .revalidate_reboot_of(self.binding.origin())
            .map_err(RecoveryAdmissionError::Origin)?;
        let current = self
            .activity()
            .recovery_lock_observation()
            .map_err(RecoveryAdmissionError::Activity)?;
        if current != self.binding.0.activity_lock {
            return Err(RecoveryAdmissionError::LockChanged { current });
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn revalidate(&self) -> Result<(), RecoveryAdmissionError> {
        Err(RecoveryAdmissionError::Unsupported)
    }

    #[cfg(target_os = "linux")]
    fn revalidate_directories(&self) -> Result<(), RecoveryAdmissionError> {
        self.revalidate_root_directory()?;
        let directory = self
            .reservation
            .as_ref()
            .expect("physically opened reservation");
        directory
            .revalidate()
            .map_err(RecoveryAdmissionError::Native)?;
        if directory.identity() != self.binding.reservation_identity()
            || directory
                .entries(0)
                .map_err(RecoveryAdmissionError::Native)?
                .is_none()
        {
            return Err(Self::native("recorded reservation changed or is not empty"));
        }
        self.revalidate_record_chain()
    }

    #[cfg(target_os = "linux")]
    fn revalidate_root_directory(&self) -> Result<(), RecoveryAdmissionError> {
        let root = self
            .root_directory
            .as_ref()
            .expect("retained root directory");
        root.revalidate().map_err(RecoveryAdmissionError::Native)?;
        if root.object_identity() != self.root.identity() {
            return Err(Self::native("reservation parent is not the original root"));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn revalidate_record_chain(&self) -> Result<(), RecoveryAdmissionError> {
        for directory in &self.record_chain {
            directory
                .revalidate()
                .map_err(RecoveryAdmissionError::Native)?;
        }
        self.root_directory
            .as_ref()
            .expect("retained root directory")
            .revalidate()
            .map_err(RecoveryAdmissionError::Native)
    }

    fn read_record(&self) -> Result<MediaRead, RecoveryAdmissionError> {
        let parent = self
            .record_chain
            .last()
            .expect("retained fixed record chain");
        let read = parent
            .read_file_bounded(
                self.record_name.as_ref().expect("plain record selector"),
                MAX_INTENT_BYTES,
            )
            .map_err(RecoveryAdmissionError::Native)?
            .ok_or_else(|| Self::native("selected recovery record is absent"))?;
        Ok(read)
    }

    #[cfg(target_os = "linux")]
    fn census(&mut self, final_walk: bool) -> Result<(), RecoveryAdmissionError> {
        match activity_obligations::observe_retained(&self.root) {
            Ok(census) => {
                if final_walk {
                    self.final_census = Some(census);
                } else {
                    self.initial = Some(census);
                }
                Ok(())
            }
            Err(failure) => {
                self.incomplete = Some(failure);
                Err(RecoveryAdmissionError::IncompleteCensus)
            }
        }
    }

    fn require_population(&self, census: &NativeObligations) -> Result<(), RecoveryAdmissionError> {
        if census.entries().len() != 2 {
            return Err(Self::native("recovery census contains other obligations"));
        }
        let record = self
            .record
            .as_ref()
            .expect("actual original record retained");
        if !record.complete || record.entry.kind != JournalEntryKind::RegularFile {
            return Err(Self::native(
                "selected recovery record is not complete and plain",
            ));
        }
        let relative = Path::new("health/offload/restore-intents")
            .join(self.record_name.as_ref().expect("plain selector"));
        let mut stages = 0;
        let mut intents = 0;
        for entry in census.entries() {
            match entry {
                NativeObligation::Stage { relative, entry }
                    if relative == Path::new(self.binding.reservation_name())
                        && entry.name == OsStr::new(self.binding.reservation_name())
                        && entry.identity == self.binding.reservation_identity()
                        && entry.kind == JournalEntryKind::Directory =>
                {
                    stages += 1
                }
                NativeObligation::Intent {
                    relative: path,
                    entry,
                    contents:
                        IntentRead::Complete {
                            operation_id,
                            bytes,
                        },
                } if path == &relative
                    && entry == &record.entry
                    && operation_id == &self.operation_id
                    && bytes == &record.bytes =>
                {
                    intents += 1
                }
                _ => {
                    return Err(Self::native(
                        "recovery census does not match retained entries",
                    ));
                }
            }
        }
        if stages != 1 || intents != 1 {
            return Err(Self::native(
                "recovery census does not uniquely cover the owners",
            ));
        }
        Ok(())
    }

    // Called only after an actual current origin passes the private boot relation.
    #[cfg(target_os = "linux")]
    fn open_native(&mut self) -> Result<(), RecoveryAdmissionError> {
        self.current_lock = Some(
            self.activity()
                .recovery_lock_observation()
                .map_err(RecoveryAdmissionError::Activity)?,
        );
        if self.current_lock.as_ref() != Some(&self.binding.0.activity_lock) {
            return Err(RecoveryAdmissionError::LockChanged {
                current: self
                    .current_lock
                    .as_ref()
                    .expect("actual lock observed")
                    .clone(),
            });
        }
        let root = MediaDirectory::open(Arc::clone(&self.root), Path::new(""))
            .map_err(RecoveryAdmissionError::Native)?;
        self.root_directory = Some(Arc::clone(&root));
        self.reservation = root
            .open_child(OsStr::new(self.binding.reservation_name()))
            .map_err(RecoveryAdmissionError::Native)?;
        if self.reservation.is_none() {
            return Err(Self::native("original reservation is absent"));
        }
        self.revalidate_directories()?;
        self.census(false)?;
        self.record_chain.push(root);
        for name in ["health", "offload", "restore-intents"] {
            let parent = self.record_chain.last().expect("retained current parent");
            let child = parent
                .open_child(OsStr::new(name))
                .map_err(RecoveryAdmissionError::Native)?
                .ok_or_else(|| Self::native("fixed recovery record parent is absent"))?;
            self.record_chain.push(child);
        }
        self.record = Some(self.read_record()?);
        self.require_population(self.initial.as_ref().expect("initial census retained"))
    }

    #[cfg(target_os = "linux")]
    fn admit(&mut self) -> Result<(), RecoveryAdmissionError> {
        if self.record_name.is_none() {
            return Err(RecoveryAdmissionError::InvalidSelector);
        }
        match ActivityLease::try_acquire_existing(Arc::clone(&self.root)) {
            Ok(lease) => self.activity = Some(lease),
            Err(failure) => {
                self.activity = failure.lease;
                return Err(RecoveryAdmissionError::Activity(failure.error));
            }
        }
        self.origin = Some(
            crate::archive_origin::observe_archive_origin(Arc::clone(&self.root))
                .map_err(|failure| RecoveryAdmissionError::OriginObservation(Box::new(failure)))?,
        );
        let origin = self.origin.as_ref().expect("observed origin retained");
        if origin.evidence().boot_id() == self.binding.origin().boot_id() {
            return Err(RecoveryAdmissionError::SameBoot);
        }
        origin
            .revalidate_reboot_of(self.binding.origin())
            .map_err(RecoveryAdmissionError::Origin)?;
        self.open_native()?;
        self.finish_admission_with(|_| {})
    }

    #[cfg(target_os = "linux")]
    fn finish_admission_with(
        &mut self,
        before_final_walk: impl FnOnce(&Resources),
    ) -> Result<(), RecoveryAdmissionError> {
        self.revalidate()?;
        self.reread = Some(self.read_record()?);
        if self.reread != self.record {
            return Err(Self::native("original named record changed"));
        }
        self.revalidate()?;
        before_final_walk(self);
        self.census(true)?;
        self.require_population(self.final_census.as_ref().expect("final census retained"))?;
        if self
            .final_census
            .as_ref()
            .expect("final census retained")
            .entries()
            != self
                .initial
                .as_ref()
                .expect("initial census retained")
                .entries()
        {
            return Err(Self::native("recovery obligations changed"));
        }
        self.revalidate()
    }
}

/// Existing physical reservation and actual named bytes. This owner grants no
/// restore, CAS or settlement authority. Explicit retirement requires the
/// caller's actual settlement/cooperation premises. Drop closes only.
pub struct RecoveredArchiveReservation {
    resources: Resources,
}

impl RecoveredArchiveReservation {
    pub fn admit(
        root: Arc<JournalRoot>,
        operation_id: &str,
        binding: ArchiveRecoveryBinding,
    ) -> Result<Self, Box<ArchiveRecoveryAdmissionFailure>> {
        let resources = Resources::new(root, operation_id, binding);
        #[cfg(target_os = "linux")]
        let mut resources = resources;
        #[cfg(target_os = "linux")]
        let result = resources.admit();
        #[cfg(not(target_os = "linux"))]
        let result = Err(RecoveryAdmissionError::Unsupported);
        match result {
            Ok(()) => Ok(Self { resources }),
            Err(error) => Err(Box::new(ArchiveRecoveryAdmissionFailure {
                resources,
                error,
            })),
        }
    }

    pub fn root(&self) -> &Arc<JournalRoot> {
        &self.resources.root
    }
    pub fn activity(&self) -> &Arc<ActivityLease> {
        self.resources.activity()
    }
    pub fn binding(&self) -> &ArchiveRecoveryBinding {
        &self.resources.binding
    }
    pub fn operation_id(&self) -> &str {
        &self.resources.operation_id
    }
    pub fn record(&self) -> &MediaRead {
        self.resources
            .record
            .as_ref()
            .expect("original record retained")
    }
    pub fn census(&self) -> &NativeObligations {
        self.resources
            .final_census
            .as_ref()
            .expect("complete census retained")
    }
    pub fn revalidate(&self) -> Result<(), RecoveryAdmissionError> {
        self.resources.revalidate()
    }
    pub fn read_current_record(&self) -> Result<MediaRead, RecoveryAdmissionError> {
        self.revalidate()?;
        let read = self.resources.read_record()?;
        self.revalidate()?;
        Ok(read)
    }

    /// Fresh exact original record and population check. The caller keeps this
    /// physical owner on every branch; returned evidence grants no domain effect.
    pub fn revalidate_current_population(
        &self,
    ) -> Result<NativeObligations, Box<RecoveryPopulationFailure>> {
        self.current_population_with(|| {}, || {})
    }

    fn current_population_with(
        &self,
        before_walk: impl FnOnce(),
        before_final_read: impl FnOnce(),
    ) -> Result<NativeObligations, Box<RecoveryPopulationFailure>> {
        let mut failure = RecoveryPopulationFailure {
            first_read: None,
            final_read: None,
            census: None,
            incomplete: None,
            error: RecoveryAdmissionError::Unsupported,
        };
        let result = (|| {
            self.revalidate()?;
            failure.first_read = Some(self.resources.read_record()?);
            if failure.first_read.as_ref() != Some(self.record()) {
                return Err(Resources::native("original recovery record changed"));
            }
            before_walk();
            match activity_obligations::observe_retained(self.root()) {
                Ok(census) => failure.census = Some(census),
                Err(incomplete) => {
                    failure.incomplete = Some(incomplete);
                    return Err(RecoveryAdmissionError::IncompleteCensus);
                }
            }
            self.resources.require_population(
                failure
                    .census
                    .as_ref()
                    .expect("actual complete walk retained"),
            )?;
            self.revalidate()?;
            before_final_read();
            failure.final_read = Some(self.resources.read_record()?);
            if failure.final_read.as_ref() != Some(self.record()) {
                return Err(Resources::native("original recovery record changed"));
            }
            self.revalidate()
        })();
        match result {
            Ok(()) => Ok(failure.census.expect("successful exact walk retained")),
            Err(error) => {
                failure.error = error;
                Err(Box::new(failure))
            }
        }
    }
    pub fn observation(&self) -> RootCarrierObservation<'_> {
        RootCarrierObservation::recovered(self)
    }

    pub(crate) fn match_root(
        &self,
        root: &Arc<JournalRoot>,
    ) -> io::Result<(OsString, MediaIdentity)> {
        if !Arc::ptr_eq(root, self.root()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "recovered reservation belongs to another root",
            ));
        }
        self.revalidate().map_err(io::Error::other)?;
        Ok((
            self.binding().reservation_name().into(),
            self.binding().reservation_identity(),
        ))
    }
}

/// Actual read/walk evidence on a refused borrowed check. This diagnostic never
/// substitutes for retaining the original RecoveredArchiveReservation owner.
pub struct RecoveryPopulationFailure {
    first_read: Option<MediaRead>,
    final_read: Option<MediaRead>,
    census: Option<NativeObligations>,
    incomplete: Option<Box<IncompleteRecoveryObservation>>,
    error: RecoveryAdmissionError,
}

impl RecoveryPopulationFailure {
    pub fn first_read(&self) -> Option<&MediaRead> {
        self.first_read.as_ref()
    }
    pub fn final_read(&self) -> Option<&MediaRead> {
        self.final_read.as_ref()
    }
    pub fn census(&self) -> Option<&NativeObligations> {
        self.census.as_ref()
    }
    pub fn incomplete(&self) -> Option<&IncompleteRecoveryObservation> {
        self.incomplete.as_deref()
    }
    pub fn error(&self) -> &RecoveryAdmissionError {
        &self.error
    }
}

impl fmt::Debug for RecoveryPopulationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecoveryPopulationFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

pub struct ArchiveRecoveryAdmissionFailure {
    resources: Resources,
    error: RecoveryAdmissionError,
}

impl ArchiveRecoveryAdmissionFailure {
    pub fn root(&self) -> &Arc<JournalRoot> {
        &self.resources.root
    }
    pub fn activity(&self) -> Option<&Arc<ActivityLease>> {
        self.resources.activity.as_ref()
    }
    pub fn binding(&self) -> &ArchiveRecoveryBinding {
        &self.resources.binding
    }
    pub fn operation_id(&self) -> &str {
        &self.resources.operation_id
    }
    pub fn record(&self) -> Option<&MediaRead> {
        self.resources.record.as_ref()
    }
    pub fn reread(&self) -> Option<&MediaRead> {
        self.resources.reread.as_ref()
    }
    pub fn initial_census(&self) -> Option<&NativeObligations> {
        self.resources.initial.as_ref()
    }
    pub fn final_census(&self) -> Option<&NativeObligations> {
        self.resources.final_census.as_ref()
    }
    pub fn incomplete(&self) -> Option<&IncompleteRecoveryObservation> {
        self.resources.incomplete.as_deref()
    }
    pub fn error(&self) -> &RecoveryAdmissionError {
        &self.error
    }
    pub fn current_origin(&self) -> Option<&LocalArchiveOrigin> {
        self.resources.origin.as_ref()
    }
    pub fn current_lock_observation(&self) -> Option<&RecoveryLockObservation> {
        self.resources.current_lock.as_ref()
    }
}

impl fmt::Debug for ArchiveRecoveryAdmissionFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArchiveRecoveryAdmissionFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, target_os = "linux", feature = "full-tests"))]
mod tests;
