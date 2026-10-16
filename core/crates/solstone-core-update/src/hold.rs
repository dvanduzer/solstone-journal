// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const HOLD_FILENAME: &str = "update-hold";
pub const HOLD_CONTENTS: &[u8] = b"held=yes\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldState {
    Held,
    Unheld,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldError {
    pub message: String,
}

impl std::fmt::Display for HoldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for HoldError {}

pub fn read_hold_state(data_dir: &Path) -> HoldState {
    let hold_path = data_dir.join(HOLD_FILENAME);
    let meta = match fs::symlink_metadata(&hold_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HoldState::Unheld,
        Err(_) => return HoldState::Unknown,
    };

    if !meta.file_type().is_file() || meta.file_type().is_symlink() {
        return HoldState::Unknown;
    }

    if meta.len() != HOLD_CONTENTS.len() as u64 {
        return HoldState::Unknown;
    }

    match fs::read(&hold_path) {
        Ok(bytes) if bytes == HOLD_CONTENTS => HoldState::Held,
        _ => HoldState::Unknown,
    }
}

pub fn set_hold(data_dir: &Path) -> Result<(), HoldError> {
    fs::create_dir_all(data_dir).map_err(|e| HoldError {
        message: e.to_string(),
    })?;
    let parent_meta = fs::symlink_metadata(data_dir).map_err(|e| HoldError {
        message: format!("cannot access data directory {}: {e}", data_dir.display()),
    })?;

    if !parent_meta.is_dir() || parent_meta.file_type().is_symlink() {
        return Err(HoldError {
            message: format!(
                "data directory {} is invalid or a symlink",
                data_dir.display()
            ),
        });
    }

    let hold_path = data_dir.join(HOLD_FILENAME);
    if let Ok(meta) = fs::symlink_metadata(&hold_path)
        && meta.file_type().is_symlink()
    {
        return Err(HoldError {
            message: format!(
                "refusing to write hold over symlink at {}",
                hold_path.display()
            ),
        });
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp_name = format!(".{HOLD_FILENAME}.tmp.{}.{nanos}", std::process::id());
    let temp_path = data_dir.join(temp_name);

    let write_res = (|| -> std::io::Result<()> {
        let mut f = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)?;
        f.write_all(HOLD_CONTENTS)?;
        f.sync_all()?;
        Ok(())
    })();

    if let Err(e) = write_res {
        let _ = fs::remove_file(&temp_path);
        return Err(HoldError {
            message: format!("failed to write temporary hold file: {e}"),
        });
    }

    if let Err(e) = fs::rename(&temp_path, &hold_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(HoldError {
            message: format!("failed to persist hold file atomically: {e}"),
        });
    }

    Ok(())
}

pub fn remove_hold(data_dir: &Path) -> Result<(), HoldError> {
    if let Ok(meta) = fs::symlink_metadata(data_dir)
        && (meta.file_type().is_symlink() || !meta.is_dir())
    {
        return Err(HoldError {
            message: "the update preference directory is not a real directory".to_owned(),
        });
    }
    let hold_path = data_dir.join(HOLD_FILENAME);
    let meta = match fs::symlink_metadata(&hold_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(HoldError {
                message: format!("cannot inspect hold path {}: {e}", hold_path.display()),
            });
        }
    };

    if meta.file_type().is_symlink() {
        return Err(HoldError {
            message: format!(
                "refusing to remove hold through symlink at {}",
                hold_path.display()
            ),
        });
    }

    if !meta.file_type().is_file() {
        return Err(HoldError {
            message: format!("hold path {} is not a regular file", hold_path.display()),
        });
    }

    fs::remove_file(&hold_path).map_err(|e| HoldError {
        message: format!("failed to remove hold file: {e}"),
    })
}

#[cfg(all(test, feature = "full-tests", target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn hold_reports_unheld_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_hold_state(dir.path()), HoldState::Unheld);
    }

    #[test]
    fn hold_reports_held_when_content_matches_exact_bytes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(HOLD_FILENAME), b"held=yes\n").unwrap();
        assert_eq!(read_hold_state(dir.path()), HoldState::Held);
    }

    #[test]
    fn hold_reports_unknown_on_corrupt_content_or_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let hold_path = dir.path().join(HOLD_FILENAME);

        // Corrupt content
        fs::write(&hold_path, b"held=no\n").unwrap();
        assert_eq!(read_hold_state(dir.path()), HoldState::Unknown);

        fs::write(&hold_path, b"held=yes").unwrap(); // missing newline
        assert_eq!(read_hold_state(dir.path()), HoldState::Unknown);

        fs::remove_file(&hold_path).unwrap();

        // Symlink
        let target = dir.path().join("target");
        fs::write(&target, b"held=yes\n").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &hold_path).unwrap();
            assert_eq!(read_hold_state(dir.path()), HoldState::Unknown);
        }
    }

    #[test]
    fn hold_writer_atomically_persists_preference() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_hold_state(dir.path()), HoldState::Unheld);

        set_hold(dir.path()).expect("set_hold should succeed");
        assert_eq!(read_hold_state(dir.path()), HoldState::Held);

        let content = fs::read(dir.path().join(HOLD_FILENAME)).unwrap();
        assert_eq!(content, b"held=yes\n");
    }

    #[test]
    fn unhold_writer_removes_regular_file_and_refuses_symlink() {
        let dir = tempfile::tempdir().unwrap();
        set_hold(dir.path()).unwrap();
        assert_eq!(read_hold_state(dir.path()), HoldState::Held);

        remove_hold(dir.path()).expect("remove_hold should succeed");
        assert_eq!(read_hold_state(dir.path()), HoldState::Unheld);

        // Symlink refusal
        #[cfg(unix)]
        {
            let target = dir.path().join("target");
            fs::write(&target, b"held=yes\n").unwrap();
            let hold_path = dir.path().join(HOLD_FILENAME);
            std::os::unix::fs::symlink(&target, &hold_path).unwrap();

            let err = remove_hold(dir.path()).unwrap_err();
            assert!(
                err.message
                    .contains("refusing to remove hold through symlink")
            );
        }
    }
}
