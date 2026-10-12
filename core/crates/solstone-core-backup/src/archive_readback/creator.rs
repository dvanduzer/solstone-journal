// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Captured archive predecessors and the fixed installed direct creator.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use solstone_core_journal_io::media::{MediaDigest, MediaIdentity};

use super::*;

pub struct ArchiveEndpointCapture {
    pub destination: Destination,
    pub password: String,
    pub backend_env: BTreeMap<String, Option<String>>,
    pub global_options: Vec<String>,
}

pub struct ArchivePredecessorCapture {
    pub restic_path: PathBuf,
    pub write: ArchiveEndpointCapture,
    pub read: ArchiveEndpointCapture,
    pub broker_requested: bool,
    pub write_repository_requested: bool,
    pub read_repository_requested: bool,
    pub write_repository_id: Option<String>,
    pub read_repository_id: Option<String>,
    pub repository_id: Option<String>,
    pub archives: Vec<ArchiveWriterObservation>,
}

pub struct ArchiveWriterObservation {
    pub ordinal: u64,
    pub requested_argv: Vec<OsString>,
    pub sources: Vec<ArchiveWriterSourceObservation>,
    pub effects: DirectArchiveEffects,
}

pub struct ArchiveWriterSourceObservation {
    pub name: String,
    pub identity: MediaIdentity,
    pub digest: MediaDigest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DirectArchiveEffects {
    NotAttempted,
    Attempted {
        returncode: Option<i32>,
        observed_snapshot_id: Option<String>,
    },
}

pub struct DirectArchiveSnapshot {
    pub repository_id: String,
    pub snapshot_id: String,
}

pub struct DirectArchiveFailure {
    pub reason: ReadbackError,
    pub effects: DirectArchiveEffects,
}

enum CreatorState {
    Unknown,
    Admission,
    Direct(WriterProgress),
}
enum WriterProgress {
    NoWriter,
    Submitted,
    Accepted,
    Refused(ReadbackError),
}

pub(super) struct ArchivePredecessorCustody {
    capture: ArchivePredecessorCapture,
    state: CreatorState,
    pending: u64,
}
impl ArchivePredecessorCustody {
    pub(super) fn unknown(capture: ArchivePredecessorCapture) -> Self {
        Self {
            capture,
            state: CreatorState::Unknown,
            pending: 0,
        }
    }
    fn requested(capture: ArchivePredecessorCapture) -> Self {
        Self {
            capture,
            state: CreatorState::Admission,
            pending: 0,
        }
    }
    pub(super) fn creator_executor(&self) -> bool {
        !matches!(self.state, CreatorState::Unknown)
    }
    /// This creator never submitted an archive writer, and owes no earlier one.
    pub(super) fn unwritten(&self) -> bool {
        self.creator_executor() && self.pending == 0 && self.capture.archives.is_empty()
    }
    #[cfg(unix)]
    pub(super) fn validate_input(&self, input: &ReadAdmission<'_>) -> Result<(), ReadbackError> {
        if !self.creator_executor() {
            return Ok(());
        }
        let capture = &self.capture;
        if input.operation.owned_clock().is_none()
            || input.operation.configuration().binding().is_some()
            || input.operation.configuration().mode() != Some("byo")
            || capture.broker_requested
            || capture.write_repository_requested
            || capture.read_repository_requested
            || capture.write_repository_id.is_some()
            || capture.read_repository_id.is_some()
            || capture.repository_id.is_some()
            || !capture.archives.is_empty()
            || capture.restic_path != input.expected_restic_path
        {
            return Err(ReadbackError::UnsupportedBackend);
        }
        for endpoint in [&capture.write, &capture.read] {
            if endpoint.destination != input.destination
                || endpoint.password != input.password
                || !endpoint.global_options.is_empty()
            {
                return Err(ReadbackError::UnsupportedBackend);
            }
            let expected = crate::assemble_backend_env(&endpoint.destination)
                .map_err(|_| ReadbackError::CredentialsUnavailable)?
                .into_iter()
                .map(|(key, value)| {
                    value
                        .as_str()
                        .map(|value| (key, Some(value.to_owned())))
                        .ok_or(ReadbackError::CredentialsUnavailable)
                })
                .collect::<Result<BTreeMap<_, _>, _>>()?;
            if endpoint.backend_env != expected {
                return Err(ReadbackError::CredentialsUnavailable);
            }
            direct_environment(&endpoint.destination, &endpoint.password)?;
        }
        Ok(())
    }
    #[cfg(unix)]
    pub(super) fn validate_member(
        &self,
        member: &CurrentInstalledMember,
    ) -> Result<(), ReadbackError> {
        if !self.creator_executor() {
            return Ok(());
        }
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            if member.bytes() == 30_924_962
                && member.sha256()
                    == "ae7fe58ab3511f830fd31d157158620b209522ff1332b119199d2e938d72338c"
            {
                Ok(())
            } else {
                Err(ReadbackError::UnsupportedToolVersion)
            }
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            let _ = member;
            Err(ReadbackError::Unsupported)
        }
    }
    pub(super) fn mark_read_config(&mut self) {
        if matches!(self.state, CreatorState::Admission) {
            self.capture.read_repository_requested = true;
        }
    }
    pub(super) fn confirm_read_config(&mut self, id: &str) {
        if matches!(self.state, CreatorState::Admission) {
            self.capture.read_repository_id = Some(id.to_owned());
            self.capture.repository_id = Some(id.to_owned());
        }
    }
    pub(super) fn qualify_settlement(&self) -> Result<(), ReadbackError> {
        match self.state {
            CreatorState::Direct(WriterProgress::NoWriter | WriterProgress::Accepted)
                if self.pending == 0 =>
            {
                Ok(())
            }
            CreatorState::Direct(WriterProgress::Refused(reason)) => Err(reason),
            _ => Err(ReadbackError::ToolFailed),
        }
    }
    fn refuse(&mut self, reason: ReadbackError) {
        if !matches!(self.state, CreatorState::Direct(WriterProgress::Refused(_))) {
            self.state = CreatorState::Direct(WriterProgress::Refused(reason));
        }
    }
    fn submit(&mut self) -> Result<(), ReadbackError> {
        self.pending = self
            .pending
            .checked_add(1)
            .ok_or(ReadbackError::ToolFailed)?;
        if !matches!(self.state, CreatorState::Direct(WriterProgress::Refused(_))) {
            self.state = CreatorState::Direct(WriterProgress::Submitted);
        }
        Ok(())
    }
    #[cfg(unix)]
    fn accept(&mut self, child: CompletedChild) {
        if !child.successful() || self.pending == 0 {
            self.refuse(ReadbackError::ToolFailed);
            return;
        }
        self.pending -= 1;
        if !matches!(self.state, CreatorState::Direct(WriterProgress::Refused(_))) {
            self.state = CreatorState::Direct(WriterProgress::Accepted);
        }
        drop(child);
    }
    fn admission_effects(&self) -> (bool, bool) {
        (
            self.capture.write_repository_requested,
            self.capture.read_repository_requested,
        )
    }
}

impl OwnedReadbackAdmissionError {
    pub fn archive_admission_effects(&self) -> Option<(bool, bool)> {
        match &self.owned {
            ClosedAdmissionOwner::Input(input) => input._predecessor.as_ref(),
            ClosedAdmissionOwner::Admitted(read) => read._execution.predecessor.as_ref(),
        }
        .map(ArchivePredecessorCustody::admission_effects)
    }
}

#[must_use]
pub struct OwnedArchiveCreatorAdmission {
    input: OwnedReadAdmission,
}
impl OwnedPreparedArchiveOperation {
    pub fn bind_archive_creator(
        self,
        capture: ArchivePredecessorCapture,
    ) -> OwnedArchiveCreatorAdmission {
        let (operation, clock) = self.into_parts();
        let mut input = ReadAdmission::capture_current(
            operation,
            capture.read.destination.clone(),
            capture.read.password.clone(),
            capture.restic_path.clone(),
        );
        input.predecessor = Some(ArchivePredecessorCustody::requested(capture));
        OwnedArchiveCreatorAdmission {
            input: OwnedReadAdmission { input, clock },
        }
    }
}
impl OwnedArchiveCreatorAdmission {
    pub fn admit(self) -> Result<OwnedArchiveCreator, OwnedReadbackAdmissionError> {
        let read = self.input.admit()?;
        #[cfg(unix)]
        let paired = (|| {
            if let Some(custody) = read.read.execution.borrow_mut().predecessor.as_mut() {
                custody.capture.write_repository_requested = true;
            }
            read.read.check_repository()?;
            read.read.revalidate_admission()?;
            if let Some(custody) = read.read.execution.borrow_mut().predecessor.as_mut() {
                custody.capture.write_repository_id = Some(read.repository_id().to_owned());
                custody.state = CreatorState::Direct(WriterProgress::NoWriter);
            }
            Ok(())
        })();
        #[cfg(not(unix))]
        let paired: Result<(), ReadbackError> = Err(ReadbackError::Unsupported);
        if let Err(reason) = paired {
            if let Some(custody) = read.read.execution.borrow_mut().predecessor.as_mut() {
                custody.refuse(reason);
            }
            return Err(OwnedReadbackAdmissionError {
                reason,
                owned: ClosedAdmissionOwner::Admitted(Box::new(read.into_owned_hold())),
            });
        }
        Ok(OwnedArchiveCreator { read })
    }
}

#[must_use]
pub struct OwnedArchiveCreator {
    read: OwnedReadOperation,
}
impl OwnedArchiveCreator {
    pub fn prepared_operation(&self) -> &PreparedArchiveOperation<'static> {
        &self.read.read.operation
    }
    pub fn carrier(&self) -> &Arc<RootActivityCarrier> {
        self.read.carrier()
    }
    pub fn activity(&self) -> &Arc<ActivityLease> {
        self.read.activity()
    }
    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        self.read.journal_root()
    }
    pub fn repository_id(&self) -> &str {
        self.read.repository_id()
    }
    pub fn revalidate_local(&self) -> Result<(), ReadbackError> {
        self.read.revalidate_admission()
    }
    pub fn current_time(&self) -> Result<i64, ReadbackError> {
        self.revalidate_local()?;
        Ok(self.prepared_operation().clock().now_unix())
    }
    pub fn into_owned_hold(self) -> OwnedDirectReadHold {
        self.read.into_owned_hold()
    }
    pub fn into_read_operation(self) -> Result<OwnedReadOperation, OwnedReadbackAdmissionError> {
        if let Err(reason) = self.read.revalidate_admission() {
            return Err(OwnedReadbackAdmissionError {
                reason,
                owned: ClosedAdmissionOwner::Admitted(Box::new(self.read.into_owned_hold())),
            });
        }
        // Failed predecessor history moves intact; only settlement qualifies it.
        Ok(self.read)
    }
    pub fn archive_owned(
        &mut self,
        sources: &[ArchiveSource<'_>],
    ) -> Result<DirectArchiveSnapshot, DirectArchiveFailure> {
        let mut argv = vec![
            "backup".into(),
            "--force".into(),
            "--no-cache".into(),
            "--tag".into(),
            super::OFFLOAD_ARCHIVE_TAG.into(),
            "--retry-lock".into(),
            "30m".into(),
        ];
        argv.extend(
            sources
                .iter()
                .map(|source| source.path().as_os_str().to_owned()),
        );
        argv.push("--json".into());
        let facts = sources
            .iter()
            .filter_map(|source| {
                source
                    .name()
                    .to_str()
                    .map(|name| ArchiveWriterSourceObservation {
                        name: name.to_owned(),
                        identity: source.identity(),
                        digest: source.digest().clone(),
                    })
            })
            .collect();
        let index = {
            let mut execution = self.read.read.execution.borrow_mut();
            let Some(custody) = execution.predecessor.as_mut() else {
                return Err(DirectArchiveFailure {
                    reason: ReadbackError::Unsupported,
                    effects: DirectArchiveEffects::NotAttempted,
                });
            };
            let index = custody.capture.archives.len();
            let ordinal = match u64::try_from(index)
                .ok()
                .and_then(|index| index.checked_add(1))
            {
                Some(ordinal) => ordinal,
                None => {
                    custody.refuse(ReadbackError::ToolFailed);
                    return Err(DirectArchiveFailure {
                        reason: ReadbackError::ToolFailed,
                        effects: DirectArchiveEffects::NotAttempted,
                    });
                }
            };
            custody.capture.archives.push(ArchiveWriterObservation {
                ordinal,
                requested_argv: argv.clone(),
                sources: facts,
                effects: DirectArchiveEffects::NotAttempted,
            });
            index
        };
        let mut effects = DirectArchiveEffects::NotAttempted;
        let result = self.archive_inner(sources, argv, index, &mut effects);
        if let Some(custody) = self.read.read.execution.borrow_mut().predecessor.as_mut() {
            custody.capture.archives[index].effects = effects.clone();
            if let Err(reason) = result {
                custody.refuse(reason);
            }
        }
        result.map_err(|reason| DirectArchiveFailure { reason, effects })
    }
    #[cfg(not(unix))]
    fn archive_inner(
        &mut self,
        _: &[ArchiveSource<'_>],
        _: Vec<OsString>,
        _: usize,
        _: &mut DirectArchiveEffects,
    ) -> Result<DirectArchiveSnapshot, ReadbackError> {
        Err(ReadbackError::Unsupported)
    }
    #[cfg(unix)]
    fn revalidate_sources(&self, sources: &[ArchiveSource<'_>]) -> Result<(), ReadbackError> {
        self.revalidate_local()?;
        let result = sources.iter().try_for_each(|source| {
            source
                .revalidate()
                .map_err(|_| ReadbackError::NativeFailure)
        });
        self.revalidate_local()?;
        result
    }
    #[cfg(unix)]
    fn archive_inner(
        &mut self,
        sources: &[ArchiveSource<'_>],
        argv: Vec<OsString>,
        index: usize,
        effects: &mut DirectArchiveEffects,
    ) -> Result<DirectArchiveSnapshot, ReadbackError> {
        let mut names = std::collections::BTreeSet::new();
        if sources.is_empty()
            || sources.iter().any(|source| {
                !source.path().is_absolute()
                    || source.path().to_str().is_none()
                    || source.path().file_name() != Some(source.name())
                    || source
                        .name()
                        .to_str()
                        .is_none_or(|name| !plain_name(name) || !names.insert(name))
                    || source.descriptor().is_none()
            })
        {
            return Err(ReadbackError::InvalidSelection);
        }
        self.revalidate_sources(sources)?;
        self.read.read.check_repository()?;
        self.read.read.check_repository()?;
        self.revalidate_sources(sources)?;
        let timeout = self
            .prepared_operation()
            .budget()
            .remaining(self.prepared_operation().clock())?;
        let request = Request {
            program: self.read.read.member.path().as_os_str().to_owned(),
            argv,
            env: self.read.read.env.clone(),
            timeout: Some(timeout),
            pass_fds: sources
                .iter()
                .filter_map(ArchiveSource::descriptor)
                .collect(),
        };
        {
            let mut execution = self.read.read.execution.borrow_mut();
            let custody = execution
                .predecessor
                .as_mut()
                .ok_or(ReadbackError::Unsupported)?;
            custody.submit()?;
            // This conservative call boundary records attempted effects even if spawn fails.
            *effects = DirectArchiveEffects::Attempted {
                returncode: None,
                observed_snapshot_id: None,
            };
            custody.capture.archives[index].effects = effects.clone();
        }
        let child = bounded_child::run_owned_archive_creator(
            &request,
            Some(64 * 1024 * 1024),
            self.prepared_operation().budget().cancellation(),
        );
        let observed = child.as_ref().ok().map(|child| child.output().returncode);
        let snapshot = child
            .as_ref()
            .ok()
            .and_then(|child| parse_summary(&child.output().stdout).ok());
        *effects = DirectArchiveEffects::Attempted {
            returncode: observed,
            observed_snapshot_id: snapshot.clone(),
        };
        self.revalidate_sources(sources)?;
        let child = child.map_err(|_| self.read.read.execution_error())?;
        self.read.read.check_repository()?;
        self.read.read.check_repository()?;
        self.revalidate_sources(sources)?;
        if !child.successful() {
            return Err(ReadbackError::ToolFailed);
        }
        let snapshot_id = snapshot.ok_or(ReadbackError::SnapshotNodeUnavailable)?;
        if let Some(custody) = self.read.read.execution.borrow_mut().predecessor.as_mut() {
            custody.accept(child);
        }
        Ok(DirectArchiveSnapshot {
            repository_id: self.repository_id().to_owned(),
            snapshot_id,
        })
    }
}

#[cfg(unix)]
fn parse_summary(output: &[u8]) -> Result<String, ReadbackError> {
    let mut summary = None;
    for line in output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
    {
        let value: Value =
            serde_json::from_slice(line).map_err(|_| ReadbackError::SnapshotNodeUnavailable)?;
        if value.get("message_type").and_then(Value::as_str) == Some("summary") {
            let id = value
                .get("snapshot_id")
                .and_then(Value::as_str)
                .filter(|id| full_hex_identity(id))
                .ok_or(ReadbackError::SnapshotNodeUnavailable)?;
            if summary.replace(id.to_owned()).is_some() {
                return Err(ReadbackError::SnapshotNodeUnavailable);
            }
        }
    }
    summary.ok_or(ReadbackError::SnapshotNodeUnavailable)
}
