// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Finite forwarding over original read owners, without mutable generic extraction.

use solstone_core_backup::archive_readback::{
    ExactArchiveFile, OwnedReadOperation, ReadOperation, ReadbackFailure,
};
use solstone_core_journal_io::media::{OwnedMediaTarget, PreparedMedia};

pub(crate) trait RestoreRead<'clock> {
    fn as_read(&self) -> &ReadOperation<'clock>;
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
    fn read_exact(
        &mut self,
        file: &ExactArchiveFile,
        target: OwnedMediaTarget,
    ) -> Result<PreparedMedia, ReadbackFailure> {
        OwnedReadOperation::read_exact(self, file, target)
    }
}
