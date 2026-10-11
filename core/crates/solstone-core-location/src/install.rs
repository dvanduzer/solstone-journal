// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Downloading and removing the location-accuracy packs.
//!
//! The files come from the journal's own update origin through the same pinned
//! fetch every downloaded model uses: each one is checked against the size and
//! SHA-256 this build was made with before it is used. Every owner fetches the
//! same bytes, and nothing about location is in the request.
//!
//! A new version downloads beside the one in use, and `installed.json` is
//! written last, so an interrupted download leaves the old one working.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use solstone_core_assets::{RuntimeFetchQuery, catalog, mint_runtime_fetch};
use solstone_core_journal_io::{AtomicWriteOptions, atomic_replace};

use crate::accuracy::{
    INSTALLED_RECORD, Installed, InstalledFile, accuracy_root, read_installed, release_loaded,
};

/// The catalog unit the two files belong to.
pub const UNIT: &str = "location-accuracy";
/// Room left on the disk after a download, beyond the files themselves.
const HEADROOM_BYTES: u64 = 512 * 1024 * 1024;

/// One file this build offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferedFile {
    pub file: String,
    pub sha256: String,
    pub size_bytes: u64,
    origin_key: &'static str,
}

/// What this build offers to download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offered {
    pub version: String,
    pub files: Vec<OfferedFile>,
}

impl Offered {
    pub fn size_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.size_bytes).sum()
    }

    /// Whether `installed` already holds exactly these files.
    pub fn is_installed(&self, installed: &Installed) -> bool {
        installed.version == self.version
            && self.files.iter().all(|offered| {
                installed.files.iter().any(|file| {
                    file.file == offered.file
                        && file.sha256 == offered.sha256
                        && file.size_bytes == offered.size_bytes
                })
            })
    }
}

/// The packs this build can download on this computer, if any.
pub fn offered() -> Option<Offered> {
    let mut files: Vec<OfferedFile> = catalog()
        .iter()
        .filter(|row| row.unit == UNIT)
        .filter(|row| {
            mint_runtime_fetch(&RuntimeFetchQuery {
                origin_key: Some(row.origin_key),
                ..RuntimeFetchQuery::default()
            })
            .is_ok()
        })
        .map(|row| OfferedFile {
            file: row.filename.to_owned(),
            sha256: row.sha256.to_owned(),
            size_bytes: row.size_bytes,
            origin_key: row.origin_key,
        })
        .collect();
    let version = catalog()
        .iter()
        .find(|row| row.unit == UNIT)
        .map(|row| row.version.to_owned())?;
    files.sort_by(|a, b| a.file.cmp(&b.file));
    (!files.is_empty()).then_some(Offered { version, files })
}

/// Why an install stopped.
#[derive(Debug)]
pub enum InstallError {
    /// This build has nothing to offer on this computer.
    NothingOffered,
    /// Not enough free space; the numbers are bytes.
    NotEnoughSpace {
        needed: u64,
        available: u64,
    },
    Download {
        file: String,
        detail: String,
    },
    Write(io::Error),
}

impl InstallError {
    /// A stable code for the map app.
    pub fn reason_code(&self) -> &'static str {
        match self {
            Self::NothingOffered => "nothing_offered",
            Self::NotEnoughSpace { .. } => "not_enough_space",
            Self::Download { .. } => "download_failed",
            Self::Write(_) => "write_failed",
        }
    }
}

impl fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NothingOffered => formatter.write_str("no location accuracy download is offered"),
            Self::NotEnoughSpace { needed, available } => write!(
                formatter,
                "needs {needed} bytes free, {available} available"
            ),
            Self::Download { file, detail } => write!(formatter, "{file}: {detail}"),
            Self::Write(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for InstallError {}

#[cfg(unix)]
fn available_bytes(path: &Path) -> io::Result<u64> {
    let stat = nix::sys::statvfs::statvfs(path).map_err(io::Error::from)?;
    #[allow(
        clippy::unnecessary_cast,
        reason = "fsblkcnt_t is 32-bit on macOS and 64-bit on Linux"
    )]
    let blocks = stat.blocks_available() as u64;
    Ok(blocks.saturating_mul(stat.fragment_size()))
}

#[cfg(windows)]
fn available_bytes(path: &Path) -> io::Result<u64> {
    solstone_core_journal_io::windows_available_disk_bytes(path)
}

/// Remove version directories `installed.json` does not name. Best effort: a
/// file this process still has open is removed on a later pass.
fn sweep(journal: &Path, keep: Option<&str>) {
    let Ok(entries) = fs::read_dir(accuracy_root(journal)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if entry.path().is_dir() && Some(name.to_string_lossy().as_ref()) != keep {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Download what [`offered`] lists. `progress` gets bytes received across
/// all files and the total. Returns what is installed afterwards.
pub fn install(
    journal: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<Installed, InstallError> {
    let offered = offered().ok_or(InstallError::NothingOffered)?;
    if let Some(installed) = read_installed(journal)
        && offered.is_installed(&installed)
    {
        return Ok(installed);
    }
    let root = accuracy_root(journal);
    fs::create_dir_all(&root).map_err(InstallError::Write)?;
    let directory = root.join(&offered.version);
    fs::create_dir_all(&directory).map_err(InstallError::Write)?;
    let present: u64 = offered
        .files
        .iter()
        .filter(|file| {
            fs::metadata(directory.join(&file.file))
                .map(|meta| meta.len())
                .ok()
                == Some(file.size_bytes)
        })
        .map(|file| file.size_bytes)
        .sum();
    let needed = offered.size_bytes() - present + HEADROOM_BYTES;
    let available = available_bytes(&root).map_err(InstallError::Write)?;
    if available < needed {
        return Err(InstallError::NotEnoughSpace { needed, available });
    }
    let total = offered.size_bytes();
    let mut done = 0u64;
    for file in &offered.files {
        let handle = mint_runtime_fetch(&RuntimeFetchQuery {
            origin_key: Some(file.origin_key),
            ..RuntimeFetchQuery::default()
        })
        .map_err(|_| InstallError::NothingOffered)?;
        solstone_core_artifact_download::download_runtime_fetch(
            &handle,
            &directory.join(&file.file),
            |received, _| progress(done + received, total),
        )
        .map_err(|error| InstallError::Download {
            file: file.file.clone(),
            detail: error.to_string(),
        })?;
        done += file.size_bytes;
        progress(done, total);
    }
    let installed = Installed {
        version: offered.version.clone(),
        files: offered
            .files
            .iter()
            .map(|file| InstalledFile {
                file: file.file.clone(),
                sha256: file.sha256.clone(),
                size_bytes: file.size_bytes,
            })
            .collect(),
    };
    let mut bytes = serde_json::to_vec_pretty(&installed).unwrap_or_default();
    bytes.push(b'\n');
    atomic_replace(
        root.join(INSTALLED_RECORD),
        &bytes,
        AtomicWriteOptions::default(),
    )
    .map_err(|error| InstallError::Write(io::Error::other(error.to_string())))?;
    sweep(journal, Some(&offered.version));
    Ok(installed)
}

/// Remove the download. Names fall back to the area tier on the next pass.
pub fn remove(journal: &Path) -> io::Result<()> {
    release_loaded();
    match fs::remove_file(accuracy_root(journal).join(INSTALLED_RECORD)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    sweep(journal, None);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_clears_the_record_and_the_files() {
        let journal = tempfile::tempdir().unwrap();
        let root = accuracy_root(journal.path());
        fs::create_dir_all(root.join("v1")).unwrap();
        fs::write(root.join("v1/venues.slpk"), b"x").unwrap();
        fs::write(root.join(INSTALLED_RECORD), b"{}").unwrap();
        remove(journal.path()).unwrap();
        assert!(!root.join(INSTALLED_RECORD).exists());
        assert!(!root.join("v1").exists());
        remove(journal.path()).unwrap();
    }

    #[test]
    fn this_build_offers_the_venue_file() {
        let offered = offered().expect("this build offers location accuracy");
        assert!(offered.files.iter().any(|file| file.file == "venues.slpk"));
        assert!(offered.size_bytes() > 0);
    }

    #[test]
    fn an_offer_is_installed_only_when_every_file_matches() {
        let offered = Offered {
            version: "v2".to_owned(),
            files: vec![OfferedFile {
                file: "venues.slpk".to_owned(),
                sha256: "aa".to_owned(),
                size_bytes: 3,
                origin_key: "assets/location-accuracy/v2/venues.slpk",
            }],
        };
        let mut installed = Installed {
            version: "v2".to_owned(),
            files: vec![InstalledFile {
                file: "venues.slpk".to_owned(),
                sha256: "aa".to_owned(),
                size_bytes: 3,
            }],
        };
        assert!(offered.is_installed(&installed));
        installed.version = "v1".to_owned();
        assert!(!offered.is_installed(&installed));
        installed.version = "v2".to_owned();
        installed.files[0].sha256 = "bb".to_owned();
        assert!(!offered.is_installed(&installed));
    }
}
