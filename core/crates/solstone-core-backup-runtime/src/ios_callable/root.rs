// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Explicit app-root custody. This wrapper grants no native build admission.

use std::io;
use std::sync::Arc;

use solstone_core_backup::archive_admission::ArchiveCancellation;
use solstone_core_journal_io::{JournalRoot, JournalRootError};

use crate::runner::{ToolOutput, ToolRequest, ToolRunner};

pub struct IosRootToolRunner {
    root: Arc<JournalRoot>,
    inner: Arc<dyn ToolRunner + Send + Sync>,
}

impl IosRootToolRunner {
    pub fn new(
        root: Arc<JournalRoot>,
        inner: Arc<dyn ToolRunner + Send + Sync>,
    ) -> Result<Self, JournalRootError> {
        root.revalidate_canonical_binding()?;
        Ok(Self { root, inner })
    }
}

impl ToolRunner for IosRootToolRunner {
    fn run(&self, request: &ToolRequest<'_>) -> io::Result<ToolOutput> {
        self.inner.run(request)
    }

    fn run_cancellable(
        &self,
        request: &ToolRequest<'_>,
        cancellation: &ArchiveCancellation,
    ) -> io::Result<ToolOutput> {
        self.inner.run_cancellable(request, cancellation)
    }

    fn cancellation(&self) -> Option<&ArchiveCancellation> {
        self.inner.cancellation()
    }

    fn ios_journal_root(&self) -> Option<&Arc<JournalRoot>> {
        Some(&self.root)
    }
}
