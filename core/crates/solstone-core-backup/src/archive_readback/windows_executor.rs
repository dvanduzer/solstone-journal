// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::path::Path;
use std::time::Duration;

use crate::archive_admission::ArchiveCancellation;
use crate::archive_readback::ReadbackError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowsExecutionOutput {
    pub returncode: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub quiescent: bool,
}

impl WindowsExecutionOutput {
    pub fn successful(&self) -> bool {
        self.returncode == 0
    }
}

pub trait WindowsArchiveExecutor: Send + Sync {
    fn run_cancellable(
        &self,
        program: &Path,
        argv: &[OsString],
        env: &BTreeMap<OsString, OsString>,
        timeout: Duration,
        capture_limit: usize,
        cancellation: Option<&ArchiveCancellation>,
    ) -> Result<WindowsExecutionOutput, ReadbackError>;

    fn run_dump(
        &self,
        program: &Path,
        argv: &[OsString],
        env: &BTreeMap<OsString, OsString>,
        timeout: Duration,
        stage: File,
        byte_bound: u64,
        cancellation: Option<&ArchiveCancellation>,
    ) -> Result<WindowsExecutionOutput, ReadbackError>;
}
