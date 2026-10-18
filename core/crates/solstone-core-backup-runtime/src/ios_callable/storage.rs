// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Startup-selected private storage and fixed native inode protection policy.
#![allow(unsafe_code)] // Fixed Foundation startup path and retained-fd policy only.

use std::ffi::{CStr, OsStr, c_char};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use solstone_core_journal_io::{FlatDirectory, JournalRoot, create_or_open_flat_directory_bound};

use super::Refusal;

unsafe extern "C" {
    fn journal_ios_backup_private_root() -> *mut c_char;
    fn journal_ios_backup_private_root_free(value: *mut c_char);
    // From the Go archive, which only the app links (`ios-linked-bridge`).
    #[cfg(feature = "ios-linked-bridge")]
    fn sb_s3_owner_file_policy(fd: i32, directory: i32, apply: i32) -> i32;
}

/// Without the linked archive no policy can be verified, so storage refuses.
#[cfg(not(feature = "ios-linked-bridge"))]
unsafe fn sb_s3_owner_file_policy(_: i32, _: i32, _: i32) -> i32 {
    1
}

fn policy(directory: &impl AsFd, apply: bool) -> Result<(), Refusal> {
    // The caller supplies an actually retained, internally acquired directory.
    // The existing native policy verifies its kind/owner/mode and inode binding.
    if unsafe { sb_s3_owner_file_policy(directory.as_fd().as_raw_fd(), 1, i32::from(apply)) } != 0 {
        return Err(Refusal::Storage);
    }
    Ok(())
}

pub(super) struct Storage {
    root: Arc<JournalRoot>,
}

pub(super) struct Directory {
    root: Arc<JournalRoot>,
    relative: PathBuf,
    held: FlatDirectory,
    path: String,
}

impl AsFd for Directory {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.held.as_fd()
    }
}

impl Directory {
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn revalidate(&self) -> Result<(), Refusal> {
        self.root
            .revalidate_canonical_binding()
            .map_err(|_| Refusal::Storage)?;
        self.held.revalidate().map_err(|_| Refusal::Storage)?;
        let actual =
            FlatDirectory::open(&self.root, &self.relative).map_err(|_| Refusal::Storage)?;
        if actual.identity() != self.held.identity() {
            return Err(Refusal::Storage);
        }
        policy(&self.held, false)
    }
}

impl Storage {
    pub fn create_private_storage() -> Result<Self, Refusal> {
        let pointer = unsafe { journal_ios_backup_private_root() };
        if pointer.is_null() {
            return Err(Refusal::Storage);
        }
        let bytes = unsafe { CStr::from_ptr(pointer) }.to_bytes();
        let path = std::str::from_utf8(bytes)
            .ok()
            .filter(|path| path.len() <= 4096)
            .map(PathBuf::from);
        unsafe {
            journal_ios_backup_private_root_free(pointer);
        }
        let path = path.ok_or(Refusal::Storage)?;
        let root = Arc::new(JournalRoot::open(&path).map_err(|_| Refusal::Storage)?);
        let storage = Self { root };
        storage.revalidate()?;
        Ok(storage)
    }

    pub fn revalidate(&self) -> Result<(), Refusal> {
        self.root
            .revalidate_canonical_binding()
            .map_err(|_| Refusal::Storage)?;
        policy(self.root.as_ref(), false)
    }

    pub fn same_installation(&self, other: &Self) -> bool {
        self.root.identity() == other.root.identity()
    }

    pub fn create_workspace(&self) -> Result<Directory, Refusal> {
        self.create_directory("workspaces", None)
    }

    pub fn create_state(&self, namespace: &str) -> Result<Directory, Refusal> {
        if namespace.len() != 64
            || !namespace
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Refusal::Storage);
        }
        self.create_directory("destinations", Some(namespace))
    }

    fn create_directory(
        &self,
        family: &str,
        namespace: Option<&str>,
    ) -> Result<Directory, Refusal> {
        self.revalidate()?;
        let family_dir = create_or_open_flat_directory_bound(
            self.root.as_ref(),
            OsStr::new(family),
            0o700,
            self.root.canonical_path(),
        )
        .map_err(|_| Refusal::Storage)?;
        policy(&family_dir, true)?;
        nix::unistd::fsync(self.root.as_ref()).map_err(|_| Refusal::Storage)?;
        let relative = match namespace {
            Some(name) => Path::new(family).join(name),
            None => PathBuf::from(family),
        };
        let held = if let Some(namespace) = namespace {
            let directory = create_or_open_flat_directory_bound(
                &family_dir,
                OsStr::new(namespace),
                0o700,
                &self.root.canonical_path().join(family),
            )
            .map_err(|_| Refusal::Storage)?;
            policy(&directory, true)?;
            nix::unistd::fsync(&family_dir).map_err(|_| Refusal::Storage)?;
            directory
        } else {
            family_dir
        };
        let path = self
            .root
            .canonical_path()
            .join(&relative)
            .to_str()
            .filter(|p| p.len() <= 4096)
            .ok_or(Refusal::Storage)?
            .to_owned();
        let directory = Directory {
            root: Arc::clone(&self.root),
            relative,
            held,
            path,
        };
        directory.revalidate()?;
        Ok(directory)
    }
}
