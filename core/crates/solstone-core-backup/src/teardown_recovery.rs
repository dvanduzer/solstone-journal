// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Non-secret observations for reacquiring original whole-backup teardown custody.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Descriptive storage facts only; these never authorize retry or retirement.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKeeps {
    Versions,
    ObjectLock,
    FileLock,
    HiddenFiles,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageRetention {
    Removes,
    Keeps(StorageKeeps),
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteEffect {
    SnapshotsPruned {
        admitted_snapshot_count: u32,
        remaining_snapshots: u32,
    },
    OperatedWiped {
        delete_attempts: u64,
        delete_confirmations: u64,
        abort_attempts: u64,
        abort_confirmations: u64,
        objects_empty: bool,
        uploads_empty: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteFacts {
    pub version: u32,
    pub effect: RemoteEffect,
    pub storage_retention: Option<StorageRetention>,
}

pub const TEARDOWN_RECOVERY_VERSION: u32 = 2;
pub const TEARDOWN_RECOVERY_FILENAME: &str = "teardown-recovery.json";

pub fn teardown_recovery_path(journal: &Path) -> PathBuf {
    journal.join("backup").join(TEARDOWN_RECOVERY_FILENAME)
}

#[cfg(unix)]
pub mod retained;
