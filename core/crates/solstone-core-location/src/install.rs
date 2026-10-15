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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};
use solstone_core_assets::{RuntimeFetchQuery, catalog, mint_runtime_fetch};
use solstone_core_journal_io::{
    AtomicWriteOptions, DEFAULT_LOCK_POLL_INTERVAL, LockError, LockOptions, atomic_replace,
    hold_daily_location_admission, hold_lock,
};

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
    /// Another install or refresh operation is in progress.
    Busy,
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
            Self::Busy => "accuracy_busy",
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
            Self::Busy => formatter.write_str("another location accuracy operation is in progress"),
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

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn files_match_offered(directory: &Path, offered: &Offered) -> bool {
    for file in &offered.files {
        let path = directory.join(&file.file);
        let Ok(meta) = fs::metadata(&path) else {
            return false;
        };
        if !meta.is_file() || meta.len() != file.size_bytes {
            return false;
        }
        let Ok(actual_sha) = sha256_file(&path) else {
            return false;
        };
        if actual_sha != file.sha256 {
            return false;
        }
    }
    true
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

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

/// Refresh installed accuracy packs if previously installed and catalog pins moved.
pub fn refresh_installed(journal: &Path) -> Result<(), InstallError> {
    if read_installed(journal).is_none() {
        return Ok(());
    }
    let Some(offered) = offered() else {
        return Ok(());
    };
    let installed = read_installed(journal).expect("checked above");
    if offered.is_installed(&installed) {
        return Ok(());
    }
    let mut prod_fetch =
        |file: &OfferedFile, destination: &Path, file_progress: &mut dyn FnMut(u64)| {
            let handle = mint_runtime_fetch(&RuntimeFetchQuery {
                origin_key: Some(file.origin_key),
                ..RuntimeFetchQuery::default()
            })
            .map_err(|_| InstallError::NothingOffered)?;
            solstone_core_artifact_download::download_runtime_fetch(
                &handle,
                destination,
                |received, _| file_progress(received),
            )
            .map_err(|error| InstallError::Download {
                file: file.file.clone(),
                detail: error.to_string(),
            })?;
            Ok(())
        };
    refresh_installed_with_fetch(journal, &offered, &mut prod_fetch)
}

fn refresh_installed_with_fetch(
    journal: &Path,
    offered: &Offered,
    fetch: &mut impl FnMut(&OfferedFile, &Path, &mut dyn FnMut(u64)) -> Result<(), InstallError>,
) -> Result<(), InstallError> {
    if read_installed(journal).is_none() {
        return Ok(());
    }
    let installed = read_installed(journal).expect("checked above");
    if offered.is_installed(&installed) {
        return Ok(());
    }
    install_offered(journal, offered, true, |_, _| {}, fetch)?;
    Ok(())
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
    let mut prod_fetch =
        |file: &OfferedFile, destination: &Path, file_progress: &mut dyn FnMut(u64)| {
            let handle = mint_runtime_fetch(&RuntimeFetchQuery {
                origin_key: Some(file.origin_key),
                ..RuntimeFetchQuery::default()
            })
            .map_err(|_| InstallError::NothingOffered)?;
            solstone_core_artifact_download::download_runtime_fetch(
                &handle,
                destination,
                |received, _| file_progress(received),
            )
            .map_err(|error| InstallError::Download {
                file: file.file.clone(),
                detail: error.to_string(),
            })?;
            Ok(())
        };
    let installed = install_offered(journal, &offered, false, &mut progress, &mut prod_fetch)?;
    installed.ok_or(InstallError::NothingOffered)
}

fn install_offered(
    journal: &Path,
    offered: &Offered,
    is_refresh: bool,
    mut progress: impl FnMut(u64, u64),
    fetch: &mut impl FnMut(&OfferedFile, &Path, &mut dyn FnMut(u64)) -> Result<(), InstallError>,
) -> Result<Option<Installed>, InstallError> {
    let root = accuracy_root(journal);
    fs::create_dir_all(&root).map_err(InstallError::Write)?;

    let lock_path = root.join(".worker.lock");
    let lock_options = LockOptions {
        timeout: Duration::ZERO,
        poll_interval: DEFAULT_LOCK_POLL_INTERVAL,
        mode: None,
    };
    let _worker_guard = match hold_lock(&lock_path, lock_options) {
        Ok(guard) => guard,
        Err(LockError::Timeout(_)) => {
            if is_refresh {
                log::warn!("location accuracy refresh lock contended; skipping this start");
                return Ok(None);
            } else {
                return Err(InstallError::Busy);
            }
        }
        Err(LockError::Io { source, .. }) => return Err(InstallError::Write(source)),
    };

    let incumbent = read_installed(journal);
    if let Some(installed) = &incumbent {
        let live_dir = installed.directory(journal);
        let target_dir = root.join(&offered.version);
        let can_adopt = live_dir == target_dir || !target_dir.exists();

        if can_adopt && files_match_offered(&live_dir, offered) {
            let _admission = hold_daily_location_admission(journal)
                .map_err(|error| InstallError::Write(io::Error::other(error.to_string())))?;
            let re_read = read_installed(journal);
            if is_refresh
                && (re_read.is_none()
                    || re_read
                        .as_ref()
                        .is_some_and(|inst| offered.is_installed(inst)))
            {
                return Ok(None);
            }
            let new_installed = Installed {
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
            let mut bytes = serde_json::to_vec_pretty(&new_installed)
                .map_err(|error| InstallError::Write(io::Error::other(error.to_string())))?;
            bytes.push(b'\n');

            let moved = live_dir != target_dir;
            if moved {
                fs::rename(&live_dir, &target_dir).map_err(InstallError::Write)?;
            }
            if let Err(error) = atomic_replace(
                root.join(INSTALLED_RECORD),
                &bytes,
                AtomicWriteOptions::default(),
            ) {
                if moved {
                    let _ = fs::rename(&target_dir, &live_dir);
                }
                return Err(InstallError::Write(io::Error::other(error.to_string())));
            }
            sweep(journal, Some(&new_installed.version));
            return Ok(Some(new_installed));
        }
    }

    let staging = root.join(format!(
        ".{}.stage.{}.{}",
        offered.version,
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&staging).map_err(InstallError::Write)?;

    let present: u64 = 0;
    let needed = offered.size_bytes() - present + HEADROOM_BYTES;
    let available = available_bytes(&root).map_err(InstallError::Write)?;
    if available < needed {
        let _ = fs::remove_dir_all(&staging);
        return Err(InstallError::NotEnoughSpace { needed, available });
    }

    let total = offered.size_bytes();
    let mut done = 0u64;
    for file in &offered.files {
        let destination = staging.join(&file.file);
        if let Some(dest_parent) = destination.parent()
            && let Err(error) = fs::create_dir_all(dest_parent)
        {
            let _ = fs::remove_dir_all(&staging);
            return Err(InstallError::Write(error));
        }
        let file_size = file.size_bytes;
        if let Err(error) = fetch(file, &destination, &mut |received| {
            progress(done + received, total);
        }) {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        done += file_size;
        progress(done, total);
    }

    let _admission = hold_daily_location_admission(journal)
        .map_err(|error| InstallError::Write(io::Error::other(error.to_string())))?;

    let current_record = read_installed(journal);
    if is_refresh && current_record.is_none() {
        let _ = fs::remove_dir_all(&staging);
        return Ok(None);
    }

    let target_dir = root.join(&offered.version);
    let aside = root.join(format!(".{}.aside", offered.version));

    if !target_dir.exists() && aside.exists() {
        let _ = fs::rename(&aside, &target_dir);
    }

    let new_installed = Installed {
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
    let mut bytes = serde_json::to_vec_pretty(&new_installed).map_err(|error| {
        let _ = fs::remove_dir_all(&staging);
        InstallError::Write(io::Error::other(error.to_string()))
    })?;
    bytes.push(b'\n');

    let had_live = target_dir.exists();
    if had_live && let Err(error) = fs::rename(&target_dir, &aside) {
        let _ = fs::remove_dir_all(&staging);
        return Err(InstallError::Write(error));
    }
    if let Err(error) = fs::rename(&staging, &target_dir) {
        if had_live {
            let _ = fs::rename(&aside, &target_dir);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(InstallError::Write(error));
    }

    if let Err(error) = atomic_replace(
        root.join(INSTALLED_RECORD),
        &bytes,
        AtomicWriteOptions::default(),
    ) {
        let _ = fs::remove_dir_all(&target_dir);
        if had_live {
            let _ = fs::rename(&aside, &target_dir);
        }
        return Err(InstallError::Write(io::Error::other(error.to_string())));
    }

    if aside.exists() {
        let _ = fs::remove_dir_all(&aside);
    }
    sweep(journal, Some(&new_installed.version));

    Ok(Some(new_installed))
}

/// Replace `installed.json` and sweep version directories the record does not name.
///
/// The daily location admission lock is held around both steps. Callers that
/// already downloaded files call this after the download returns.
pub fn publish_installed_record(journal: &Path, installed: &Installed) -> Result<(), InstallError> {
    let _admission = hold_daily_location_admission(journal)
        .map_err(|error| InstallError::Write(io::Error::other(error.to_string())))?;
    let root = accuracy_root(journal);
    fs::create_dir_all(&root).map_err(InstallError::Write)?;
    let mut bytes = serde_json::to_vec_pretty(installed).unwrap_or_default();
    bytes.push(b'\n');
    atomic_replace(
        root.join(INSTALLED_RECORD),
        &bytes,
        AtomicWriteOptions::default(),
    )
    .map_err(|error| InstallError::Write(io::Error::other(error.to_string())))?;
    sweep(journal, Some(&installed.version));
    Ok(())
}

/// Remove the download. Names fall back to the area tier on the next pass.
pub fn remove(journal: &Path) -> io::Result<()> {
    let _admission = hold_daily_location_admission(journal)
        .map_err(|error| io::Error::other(error.to_string()))?;
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
    #[cfg(feature = "full-tests")]
    use std::sync::mpsc;
    #[cfg(feature = "full-tests")]
    use std::thread;

    #[cfg(feature = "full-tests")]
    #[test]
    fn installed_record_publish_and_remove_wait_for_the_admission_guard() {
        let journal = tempfile::tempdir().unwrap();
        let root = accuracy_root(journal.path());
        fs::create_dir_all(root.join("old")).unwrap();
        fs::write(root.join("old/venues.slpk"), b"x").unwrap();
        let installed = Installed {
            version: "v9".to_owned(),
            files: vec![InstalledFile {
                file: "venues.slpk".to_owned(),
                sha256: "aa".to_owned(),
                size_bytes: 1,
            }],
        };
        let guard = hold_daily_location_admission(journal.path()).unwrap();
        let path = journal.path().to_path_buf();
        let published = installed.clone();
        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready) = mpsc::channel();
        let worker = thread::spawn(move || {
            ready_sender.send(()).unwrap();
            publish_installed_record(&path, &published).unwrap();
            sender.send(()).unwrap();
        });
        ready.recv().unwrap();
        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(!root.join(INSTALLED_RECORD).exists());
        assert!(root.join("old").exists());
        drop(guard);
        receiver.recv().unwrap();
        worker.join().unwrap();
        assert!(root.join(INSTALLED_RECORD).exists());
        assert!(!root.join("old").exists());

        let guard = hold_daily_location_admission(journal.path()).unwrap();
        let path = journal.path().to_path_buf();
        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready) = mpsc::channel();
        let worker = thread::spawn(move || {
            ready_sender.send(()).unwrap();
            remove(&path).unwrap();
            sender.send(()).unwrap();
        });
        ready.recv().unwrap();
        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(root.join(INSTALLED_RECORD).exists());
        drop(guard);
        receiver.recv().unwrap();
        worker.join().unwrap();
        assert!(!root.join(INSTALLED_RECORD).exists());
    }

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

    #[test]
    fn never_installed_and_removed_record_no_downloads_no_record() {
        let journal = tempfile::tempdir().unwrap();
        refresh_installed(journal.path()).unwrap();
        assert!(
            !accuracy_root(journal.path())
                .join(INSTALLED_RECORD)
                .exists()
        );

        let root = accuracy_root(journal.path());
        fs::create_dir_all(root.join("v1")).unwrap();
        fs::write(root.join("v1/venues.slpk"), b"x").unwrap();
        fs::write(root.join(INSTALLED_RECORD), b"{}").unwrap();
        remove(journal.path()).unwrap();
        assert!(!root.join(INSTALLED_RECORD).exists());

        refresh_installed(journal.path()).unwrap();
        assert!(!root.join(INSTALLED_RECORD).exists());
    }

    #[test]
    fn matching_offer_makes_zero_downloads() {
        let journal = tempfile::tempdir().unwrap();
        let offered = Offered {
            version: "v2".to_owned(),
            files: vec![OfferedFile {
                file: "venues.slpk".to_owned(),
                sha256: "aa".to_owned(),
                size_bytes: 3,
                origin_key: "assets/location-accuracy/v2/venues.slpk",
            }],
        };
        let installed = Installed {
            version: "v2".to_owned(),
            files: vec![InstalledFile {
                file: "venues.slpk".to_owned(),
                sha256: "aa".to_owned(),
                size_bytes: 3,
            }],
        };
        publish_installed_record(journal.path(), &installed).unwrap();

        let mut downloads = 0;
        let mut fetch = |_file: &OfferedFile, _dest: &Path, _progress: &mut dyn FnMut(u64)| {
            downloads += 1;
            Ok(())
        };
        refresh_installed_with_fetch(journal.path(), &offered, &mut fetch).unwrap();
        assert_eq!(downloads, 0);
    }

    #[test]
    fn live_files_matching_offer_with_behind_record_adopts_without_downloads() {
        let journal = tempfile::tempdir().unwrap();
        let root = accuracy_root(journal.path());
        let dir = root.join("v2");
        fs::create_dir_all(&dir).unwrap();
        let content = b"live venue data";
        let target_file = dir.join("venues.slpk");
        fs::write(&target_file, content).unwrap();
        let sha = sha256_file(&target_file).unwrap();

        let offered = Offered {
            version: "v2".to_owned(),
            files: vec![OfferedFile {
                file: "venues.slpk".to_owned(),
                sha256: sha.clone(),
                size_bytes: content.len() as u64,
                origin_key: "assets/location-accuracy/v2/venues.slpk",
            }],
        };
        let installed = Installed {
            version: "v2".to_owned(),
            files: vec![InstalledFile {
                file: "venues.slpk".to_owned(),
                sha256: "stale_sha".to_owned(),
                size_bytes: content.len() as u64,
            }],
        };
        publish_installed_record(journal.path(), &installed).unwrap();

        let mut downloads = 0;
        let mut fetch = |_file: &OfferedFile, _dest: &Path, _progress: &mut dyn FnMut(u64)| {
            downloads += 1;
            Ok(())
        };
        refresh_installed_with_fetch(journal.path(), &offered, &mut fetch).unwrap();
        assert_eq!(downloads, 0);

        let updated = read_installed(journal.path()).unwrap();
        assert_eq!(updated.files[0].sha256, sha);
    }

    #[test]
    fn same_version_failure_on_second_file_keeps_pre_call_live_and_record() {
        let journal = tempfile::tempdir().unwrap();
        let root = accuracy_root(journal.path());
        let dir = root.join("v2");
        fs::create_dir_all(&dir).unwrap();
        let file1 = dir.join("f1.bin");
        let file2 = dir.join("f2.bin");
        fs::write(&file1, b"initial1").unwrap();
        fs::write(&file2, b"initial2").unwrap();

        let installed = Installed {
            version: "v2".to_owned(),
            files: vec![
                InstalledFile {
                    file: "f1.bin".to_owned(),
                    sha256: "s1".to_owned(),
                    size_bytes: 8,
                },
                InstalledFile {
                    file: "f2.bin".to_owned(),
                    sha256: "s2".to_owned(),
                    size_bytes: 8,
                },
            ],
        };
        publish_installed_record(journal.path(), &installed).unwrap();
        let record_bytes_before = fs::read(root.join(INSTALLED_RECORD)).unwrap();

        let offered = Offered {
            version: "v2".to_owned(),
            files: vec![
                OfferedFile {
                    file: "f1.bin".to_owned(),
                    sha256: "new_s1".to_owned(),
                    size_bytes: 4,
                    origin_key: "k1",
                },
                OfferedFile {
                    file: "f2.bin".to_owned(),
                    sha256: "new_s2".to_owned(),
                    size_bytes: 4,
                    origin_key: "k2",
                },
            ],
        };

        let mut fetch = |file: &OfferedFile, dest: &Path, _progress: &mut dyn FnMut(u64)| {
            if file.file == "f1.bin" {
                fs::write(dest, b"new1").unwrap();
                Ok(())
            } else {
                Err(InstallError::Download {
                    file: file.file.clone(),
                    detail: "network interrupted".to_owned(),
                })
            }
        };

        let result = refresh_installed_with_fetch(journal.path(), &offered, &mut fetch);
        assert!(result.is_err());

        assert_eq!(fs::read(&file1).unwrap(), b"initial1");
        assert_eq!(fs::read(&file2).unwrap(), b"initial2");
        assert_eq!(
            fs::read(root.join(INSTALLED_RECORD)).unwrap(),
            record_bytes_before
        );
    }

    #[test]
    fn next_call_writing_all_files_succeeds_and_cleans_staging_and_aside() {
        let journal = tempfile::tempdir().unwrap();
        let root = accuracy_root(journal.path());
        let dir = root.join("v2");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f1.bin"), b"initial1").unwrap();
        fs::write(dir.join("f2.bin"), b"initial2").unwrap();

        let installed = Installed {
            version: "v2".to_owned(),
            files: vec![
                InstalledFile {
                    file: "f1.bin".to_owned(),
                    sha256: "s1".to_owned(),
                    size_bytes: 8,
                },
                InstalledFile {
                    file: "f2.bin".to_owned(),
                    sha256: "s2".to_owned(),
                    size_bytes: 8,
                },
            ],
        };
        publish_installed_record(journal.path(), &installed).unwrap();

        let offered = Offered {
            version: "v2".to_owned(),
            files: vec![
                OfferedFile {
                    file: "f1.bin".to_owned(),
                    sha256: "new_s1".to_owned(),
                    size_bytes: 4,
                    origin_key: "k1",
                },
                OfferedFile {
                    file: "f2.bin".to_owned(),
                    sha256: "new_s2".to_owned(),
                    size_bytes: 4,
                    origin_key: "k2",
                },
            ],
        };

        let mut fetch = |file: &OfferedFile, dest: &Path, _progress: &mut dyn FnMut(u64)| {
            if file.file == "f1.bin" {
                fs::write(dest, b"new1").unwrap();
            } else {
                fs::write(dest, b"new2").unwrap();
            }
            Ok(())
        };

        refresh_installed_with_fetch(journal.path(), &offered, &mut fetch).unwrap();

        assert_eq!(fs::read(dir.join("f1.bin")).unwrap(), b"new1");
        assert_eq!(fs::read(dir.join("f2.bin")).unwrap(), b"new2");
        let updated = read_installed(journal.path()).unwrap();
        assert_eq!(updated.files[0].sha256, "new_s1");
        assert_eq!(updated.files[1].sha256, "new_s2");

        for entry in fs::read_dir(&root).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            assert!(
                name == "v2"
                    || name == INSTALLED_RECORD
                    || name == ".worker.lock"
                    || name == ".worker.lock.lock",
                "unexpected artifact left: {name}"
            );
        }
    }

    #[test]
    fn refresh_commit_after_record_deleted_does_not_recreate_record() {
        let journal = tempfile::tempdir().unwrap();
        let root = accuracy_root(journal.path());
        let dir = root.join("v2");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("venues.slpk"), b"v2 content").unwrap();

        let installed = Installed {
            version: "v2".to_owned(),
            files: vec![InstalledFile {
                file: "venues.slpk".to_owned(),
                sha256: "old_sha".to_owned(),
                size_bytes: 10,
            }],
        };
        publish_installed_record(journal.path(), &installed).unwrap();

        let offered = Offered {
            version: "v2".to_owned(),
            files: vec![OfferedFile {
                file: "venues.slpk".to_owned(),
                sha256: "new_sha".to_owned(),
                size_bytes: 4,
                origin_key: "k",
            }],
        };

        let mut fetch = |_file: &OfferedFile, dest: &Path, _progress: &mut dyn FnMut(u64)| {
            let _ = fs::remove_file(accuracy_root(journal.path()).join(INSTALLED_RECORD));
            fs::write(dest, b"new4").unwrap();
            Ok(())
        };

        let result =
            install_offered(journal.path(), &offered, true, |_, _| {}, &mut fetch).unwrap();
        assert!(result.is_none());
        assert!(!root.join(INSTALLED_RECORD).exists());
        assert_eq!(fs::read(dir.join("venues.slpk")).unwrap(), b"v2 content");
    }

    #[test]
    fn worker_lock_held_makes_zero_downloads() {
        let journal = tempfile::tempdir().unwrap();
        let root = accuracy_root(journal.path());
        fs::create_dir_all(&root).unwrap();

        let installed = Installed {
            version: "v2".to_owned(),
            files: vec![InstalledFile {
                file: "venues.slpk".to_owned(),
                sha256: "different_sha".to_owned(),
                size_bytes: 3,
            }],
        };
        publish_installed_record(journal.path(), &installed).unwrap();

        let lock_path = root.join(".worker.lock");
        let _guard = hold_lock(&lock_path, LockOptions::default()).unwrap();

        let offered = Offered {
            version: "v2".to_owned(),
            files: vec![OfferedFile {
                file: "venues.slpk".to_owned(),
                sha256: "aa".to_owned(),
                size_bytes: 3,
                origin_key: "k",
            }],
        };

        let downloads = AtomicUsize::new(0);
        let mut fetch = |_file: &OfferedFile, _dest: &Path, _progress: &mut dyn FnMut(u64)| {
            downloads.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        refresh_installed_with_fetch(journal.path(), &offered, &mut fetch).unwrap();
        assert_eq!(downloads.load(Ordering::SeqCst), 0);

        let result = install_offered(journal.path(), &offered, false, |_, _| {}, &mut fetch);
        assert!(matches!(result, Err(InstallError::Busy)));
        assert_eq!(downloads.load(Ordering::SeqCst), 0);
    }
}
