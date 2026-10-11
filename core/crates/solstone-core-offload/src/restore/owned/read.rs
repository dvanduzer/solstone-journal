// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Finite forwarding over original read owners, without mutable generic extraction.

use solstone_core_backup::archive_readback::{
    ExactArchiveFile, HistoricalArchiveFile, HistoricalSnapshotFiles, OwnedReadOperation,
    ReadOperation, ReadbackError, ReadbackFailure,
};
use solstone_core_journal_io::media::{OwnedMediaTarget, PreparedMedia};

pub(crate) trait RestoreRead<'clock> {
    fn as_read(&self) -> &ReadOperation<'clock>;
    fn discover_historical_snapshot(
        &mut self,
        selector: &str,
        files: &[HistoricalArchiveFile<'_>],
    ) -> Result<HistoricalSnapshotFiles, ReadbackError>;
    fn read_exact(
        &mut self,
        file: &ExactArchiveFile,
        target: OwnedMediaTarget,
    ) -> Result<PreparedMedia, ReadbackFailure>;
}

impl<'clock> RestoreRead<'clock> for ReadOperation<'clock> {
    fn as_read(&self) -> &ReadOperation<'clock> {
        self
    }
    fn discover_historical_snapshot(
        &mut self,
        selector: &str,
        files: &[HistoricalArchiveFile<'_>],
    ) -> Result<HistoricalSnapshotFiles, ReadbackError> {
        ReadOperation::discover_historical_snapshot(self, selector, files)
    }
    fn read_exact(
        &mut self,
        file: &ExactArchiveFile,
        target: OwnedMediaTarget,
    ) -> Result<PreparedMedia, ReadbackFailure> {
        ReadOperation::read_exact(self, file, target)
    }
}
impl RestoreRead<'static> for OwnedReadOperation {
    fn as_read(&self) -> &ReadOperation<'static> {
        OwnedReadOperation::as_read(self)
    }
    fn discover_historical_snapshot(
        &mut self,
        selector: &str,
        files: &[HistoricalArchiveFile<'_>],
    ) -> Result<HistoricalSnapshotFiles, ReadbackError> {
        OwnedReadOperation::discover_historical_snapshot(self, selector, files)
    }
    fn read_exact(
        &mut self,
        file: &ExactArchiveFile,
        target: OwnedMediaTarget,
    ) -> Result<PreparedMedia, ReadbackFailure> {
        OwnedReadOperation::read_exact(self, file, target)
    }
}
