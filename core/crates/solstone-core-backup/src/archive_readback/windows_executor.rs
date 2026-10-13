// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::archive_admission::ArchiveCancellation;

pub trait WindowsArchiveCleanup: Send + Sync {
    fn quiescent(&self) -> bool;
}

pub struct WindowsArchiveRequest<'a> {
    pub program: &'a Path,
    pub argv: &'a [OsString],
    pub env: &'a BTreeMap<OsString, OsString>,
    pub timeout: Duration,
    pub cancellation: Option<&'a ArchiveCancellation>,
    pub retained: Vec<Arc<dyn Send + Sync>>,
}

#[derive(Clone)]
pub struct WindowsExecutionOutput {
    pub returncode: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub cleanup: Option<Arc<dyn WindowsArchiveCleanup>>,
}

impl WindowsExecutionOutput {
    pub fn quiescent(&self) -> bool {
        self.cleanup
            .as_ref()
            .is_none_or(|cleanup| cleanup.quiescent())
    }
    pub fn successful(&self) -> bool {
        self.returncode == 0 && self.quiescent()
    }
}

pub trait WindowsArchiveExecutor: Send + Sync {
    fn run_cancellable(
        &self,
        request: WindowsArchiveRequest<'_>,
        capture_limit: usize,
    ) -> WindowsExecutionOutput;

    fn run_dump(
        &self,
        request: WindowsArchiveRequest<'_>,
        stage: File,
        byte_bound: u64,
    ) -> WindowsExecutionOutput;
}
