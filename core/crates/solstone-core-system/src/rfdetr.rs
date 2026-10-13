// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Unix RF-DETR execution. Callers own installation admission and failure policy.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DetectorError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Clock(#[from] std::time::SystemTimeError),
    #[error("{0}")]
    Output(#[from] serde_json::Error),
    #[error("rfdetr-cli detect failed")]
    Failed,
    #[error("rfdetr-cli detect timed out after {0}ms")]
    TimedOut(u128),
}

/// Run an admitted detector, keeping its temporary files alive until the child
/// exits or is killed and reaped. A failed exit never admits leftover output.
pub fn detect(
    full_png: &[u8],
    binary: &Path,
    model: &Path,
    timeout: Duration,
) -> Result<Value, DetectorError> {
    let temp = TempDir::new()?;
    let input = temp.path.join("input.png");
    let output = temp.path.join("output.json");
    fs::write(&input, full_png)?;
    let mut child = Command::new(binary)
        .args(["detect", "--model"])
        .arg(model)
        .args(["--input"])
        .arg(&input)
        .args(["--output"])
        .arg(&output)
        .args(["--threshold", "0.25", "--threads", "4"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if !wait_for_child(&mut child, timeout)?.success() {
        return Err(DetectorError::Failed);
    }
    Ok(serde_json::from_slice(&fs::read(output)?)?)
}

/// Wait for the detector child, terminating and reaping it at the deadline.
#[doc(hidden)]
pub fn wait_for_child(child: &mut Child, timeout: Duration) -> Result<ExitStatus, DetectorError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            child.wait()?;
            return Err(DetectorError::TimedOut(timeout.as_millis()));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Result<Self, DetectorError> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = env::temp_dir().join(format!("solstone-rfdetr-{}-{nonce}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(Self { path })
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
