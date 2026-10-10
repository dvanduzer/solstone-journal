// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{JournalRoot, MediaIdentity, unsupported};

pub(super) struct Directory {
    identity: MediaIdentity,
    path: PathBuf,
}
impl Directory {
    pub(super) fn open(_root: Arc<JournalRoot>, _names: &[OsString]) -> io::Result<Self> {
        Err(unsupported())
    }
    pub(super) fn open_child(&self, _name: &OsStr) -> io::Result<Self> {
        Err(unsupported())
    }
    pub(super) fn identity(&self) -> MediaIdentity {
        self.identity
    }
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
    pub(super) fn revalidate(&self) -> io::Result<()> {
        Err(unsupported())
    }
    pub(super) fn available_bytes(&self) -> io::Result<u64> {
        Err(unsupported())
    }
    pub(super) fn sync(&self) -> io::Result<()> {
        Err(unsupported())
    }
}
pub(super) fn supported_mutation() -> io::Result<()> {
    Err(unsupported())
}
pub(super) fn identity(_file: &File) -> io::Result<MediaIdentity> {
    Err(unsupported())
}
pub(super) type Stamp = (u64, i64, i64, i64, i64);
pub(super) fn stamp(_file: &File) -> io::Result<Stamp> {
    Err(unsupported())
}
pub(super) fn check_leaf(
    _directory: &Directory,
    _name: &OsStr,
    _identity: MediaIdentity,
) -> io::Result<()> {
    Err(unsupported())
}
pub(super) fn open_plain(_directory: &Directory, _name: &OsStr) -> io::Result<Option<File>> {
    Err(unsupported())
}
pub(super) fn create_directory(_parent: &Directory, _name: &OsStr) -> io::Result<()> {
    Err(unsupported())
}
pub(super) fn create_file(_parent: &Directory, _name: &OsStr) -> io::Result<File> {
    Err(unsupported())
}
pub(super) fn publish(
    _source: &Directory,
    _source_name: &OsStr,
    _file: &File,
    _destination: &Directory,
    _name: &OsStr,
) -> io::Result<()> {
    Err(unsupported())
}
pub(super) fn remove_leaf(_parent: &Directory, _name: &OsStr, _file: &File) -> io::Result<()> {
    Err(unsupported())
}
pub(super) fn remove_directory(
    _parent: &Directory,
    _name: &OsStr,
    _directory: &Directory,
) -> io::Result<()> {
    Err(unsupported())
}
