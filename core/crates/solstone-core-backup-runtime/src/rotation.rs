// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Recovery-key rotation with platform-specific publication contracts.

use std::fmt;
use std::path::Path;

use crate::engine::BackupServices;
use crate::runner::reason_for_returncode;

pub const ROTATION_TIMEOUT_SECONDS: u64 = 5 * 60;

#[derive(Clone, PartialEq, Eq)]
pub struct RotationResult {
    pub status: String,
    pub reason_code: Option<String>,
    pub recovery_key: Option<String>,
    pub recovery_key_display: Option<String>,
}
impl fmt::Debug for RotationResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RotationResult")
            .field("status", &self.status)
            .field("reason_code", &self.reason_code)
            .field(
                "recovery_key",
                &self.recovery_key.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "recovery_key_display",
                &self.recovery_key_display.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

fn error(reason: impl Into<String>) -> RotationResult {
    RotationResult {
        status: "error".into(),
        reason_code: Some(reason.into()),
        recovery_key: None,
        recovery_key_display: None,
    }
}
fn map_error(repo_error: crate::repo::RepoError) -> RotationResult {
    match repo_error {
        crate::repo::RepoError::Key(key) => error(reason_for_returncode(key.returncode)),
        _ => error("failed"),
    }
}

#[cfg(feature = "test-hooks")]
static REMOVAL_PAUSE_HOOK: std::sync::Mutex<
    Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
> = std::sync::Mutex::new(None);

#[cfg(feature = "test-hooks")]
pub fn arm_rotation_pause() -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *REMOVAL_PAUSE_HOOK.lock().unwrap() = Some((entered_tx, release_rx));
    (entered_rx, release_tx)
}

#[cfg(feature = "test-hooks")]
pub fn pause_after_remote_removal() {
    let pair = {
        let mut guard = REMOVAL_PAUSE_HOOK.lock().unwrap();
        guard.take()
    };
    if let Some((entered_tx, release_rx)) = pair {
        let _ = entered_tx.send(());
        let _ = release_rx.recv();
    }
}

mod transaction;

/// Publish and confirm replacement material before revoking the recorded original.
pub fn rotate_recovery_key(journal: &Path, services: &BackupServices<'_>) -> RotationResult {
    transaction::rotate_recovery_key(journal, services)
}
