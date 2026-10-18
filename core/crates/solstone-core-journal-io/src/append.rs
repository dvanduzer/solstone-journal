// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Durable append-only text and JSONL writers.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use serde::Serialize;

#[cfg(unix)]
use crate::atomic::{fsync_dir, sync_file};
use crate::errors::AppendError;

#[cfg(windows)]
fn sync_file(file: &fs::File) -> io::Result<()> {
    // `File::sync_all` is `FlushFileBuffers` on Windows.  The existing Unix append contract
    // only requires the record-file sync to succeed; parent-directory sync after a create is
    // best effort there, and Windows has no corresponding directory-handle operation here.
    file.sync_all()
}

/// Append one newline-terminated text record through a single raw write.
///
/// A successful return means exactly one complete record was appended and
/// synced. A returned error can still leave a partial write on disk when the
/// underlying single write reports a short byte count.
#[cfg(any(unix, windows))]
pub fn append_text(path: impl AsRef<Path>, text: &str) -> Result<(), AppendError> {
    let mut contents = Vec::with_capacity(text.len() + 1);
    contents.extend_from_slice(text.as_bytes());
    contents.push(b'\n');
    append_record(path.as_ref(), &contents)
}

/// Serialize and append one newline-terminated JSON record through a single raw write.
///
/// A successful return means exactly one complete record was appended and
/// synced. A returned error can still leave a partial write on disk when the
/// underlying single write reports a short byte count.
pub fn append_jsonl<T: Serialize>(path: impl AsRef<Path>, record: &T) -> Result<(), AppendError> {
    let path = path.as_ref();
    let mut contents = serde_json::to_vec(record)
        .map_err(|source| io_error(path, io::Error::new(io::ErrorKind::InvalidData, source)))?;
    contents.push(b'\n');
    append_record(path, &contents)
}

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

/// Serialize and append one newline-terminated JSON record through a single raw write,
/// refusing symlink traversal and refusing to create missing parent directories.
#[cfg(any(unix, windows))]
pub fn append_jsonl_no_follow<T: Serialize>(
    path: impl AsRef<Path>,
    record: &T,
) -> Result<(), AppendError> {
    append_jsonl_no_follow_checked(path, record, &|| Ok(()))
}

#[cfg(any(unix, windows))]
pub fn append_jsonl_no_follow_checked<T: Serialize>(
    path: impl AsRef<Path>,
    record: &T,
    checkpoint: &dyn Fn() -> Result<(), String>,
) -> Result<(), AppendError> {
    let path = path.as_ref();
    let mut contents = serde_json::to_vec(record)
        .map_err(|source| io_error(path, io::Error::new(io::ErrorKind::InvalidData, source)))?;
    contents.push(b'\n');
    append_record_no_follow(path, &contents, checkpoint)
}

#[cfg(unix)]
fn open_no_follow(path: &Path) -> io::Result<fs::File> {
    OpenOptions::new()
        .append(true)
        .create(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
fn open_no_follow(path: &Path) -> io::Result<fs::File> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, OPEN_ALWAYS, SYNCHRONIZE,
    };
    let file = crate::locking::open_windows_path(
        path,
        FILE_APPEND_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        OPEN_ALWAYS,
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
    )?;
    let attributes = crate::locking::attribute_tag_windows(&file)?;
    if crate::locking::is_reparse_point_windows(attributes) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to append through a Windows reparse point",
        ));
    }
    Ok(file)
}

fn write_and_sync(path: &Path, mut file: fs::File, contents: &[u8]) -> Result<(), AppendError> {
    let written = file
        .write(contents)
        .map_err(|source| io_error(path, source))?;
    if written != contents.len() {
        return Err(io_error(
            path,
            io::Error::new(
                io::ErrorKind::WriteZero,
                "append record was only partially written",
            ),
        ));
    }
    sync_file(&file).map_err(|source| io_error(path, source))?;
    Ok(())
}

fn append_record_no_follow(
    path: &Path,
    contents: &[u8],
    checkpoint: &dyn Fn() -> Result<(), String>,
) -> Result<(), AppendError> {
    let check = || {
        checkpoint()
            .map_err(|detail| io_error(path, io::Error::new(io::ErrorKind::Interrupted, detail)))
    };
    check()?;
    // A newly created file's directory entry is made durable too, as `append_record`
    // does. `symlink_metadata` does not follow the leaf, so a link is not "new".
    #[cfg(unix)]
    let is_new = matches!(
        fs::symlink_metadata(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    );
    let file = open_no_follow(path).map_err(|source| io_error(path, source))?;
    check()?;
    write_and_sync(path, file, contents)?;
    #[cfg(unix)]
    if is_new {
        fsync_dir(parent_dir(path));
    }
    Ok(())
}

fn append_record(path: &Path, contents: &[u8]) -> Result<(), AppendError> {
    let parent = parent_dir(path);
    fs::create_dir_all(parent).map_err(|source| io_error(path, source))?;
    #[cfg(unix)]
    let is_new = !path.exists();
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|source| io_error(path, source))?;
    write_and_sync(path, file, contents)?;
    #[cfg(unix)]
    if is_new {
        fsync_dir(parent);
    }
    Ok(())
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn io_error(path: &Path, source: io::Error) -> AppendError {
    AppendError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(unix)]
pub use bound_append::{
    BoundAppendConfirmation, BoundAppendDurability, BoundAppendEffect, BoundAppendReceipt,
    append_jsonl_bound,
};

#[cfg(unix)]
mod bound_append {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::path::Path;

    use nix::fcntl::{AtFlags, OFlag, openat};
    use nix::sys::stat::{FileStat, Mode, fstat, fstatat};
    use serde::Serialize;

    use crate::flat_directory::entry_from_stat;
    use crate::journal_root::JournalEntryKind;
    use crate::observation::{FileObservation, same_entry_metadata};
    use crate::{FlatDirectory, read_observed_file_bounded};

    const INPUT_LIMIT: usize = 32 * 1024 * 1024;
    const RECORD_LIMIT: usize = 1024 * 1024;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum BoundAppendEffect {
        NotAttempted,
        /// The open may have created a leaf; a failed write may have changed bytes.
        Attempted {
            written: Option<usize>,
        },
        Appended {
            bytes: usize,
        },
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum BoundAppendDurability {
        Unproven,
        Synced,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum BoundAppendConfirmation {
        Unverified,
        Confirmed,
    }

    /// Physical facts beneath the retained parent, without canonical-root authority.
    #[must_use]
    #[derive(Debug)]
    pub struct BoundAppendReceipt {
        effect: BoundAppendEffect,
        file_durability: BoundAppendDurability,
        parent_durability: BoundAppendDurability,
        confirmation: BoundAppendConfirmation,
        failures: Vec<io::Error>,
    }

    impl BoundAppendReceipt {
        pub fn effect(&self) -> BoundAppendEffect {
            self.effect
        }
        pub fn file_durability(&self) -> BoundAppendDurability {
            self.file_durability
        }
        pub fn parent_durability(&self) -> BoundAppendDurability {
            self.parent_durability
        }
        pub fn confirmation(&self) -> BoundAppendConfirmation {
            self.confirmation
        }
        pub fn failures(&self) -> &[io::Error] {
            &self.failures
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(super) enum Point {
        BeforeOpen,
        BeforeWrite,
        AfterWrite,
        FileSync,
        ParentSync,
        AfterConfirmationRead,
    }

    pub(super) trait Hooks {
        fn checkpoint(&mut self, _point: Point) {}
        fn write(&mut self, file: &mut File, contents: &[u8]) -> io::Result<usize> {
            file.write(contents)
        }
        fn sync(&mut self, file: &impl std::os::fd::AsFd, _point: Point) -> io::Result<()> {
            nix::unistd::fsync(file).map_err(io::Error::from)
        }
    }
    struct NoHooks;
    impl Hooks for NoHooks {}

    fn invalid() -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "bound append observation changed",
        )
    }
    fn observed_error(error: impl std::fmt::Display) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, error.to_string())
    }

    struct RecordWriter(Vec<u8>);
    impl Write for RecordWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self
                .0
                .len()
                .checked_add(bytes.len())
                .is_none_or(|len| len >= RECORD_LIMIT)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "bound append record exceeds limit",
                ));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn same_stat(left: &FileStat, right: &FileStat) -> bool {
        left.st_dev == right.st_dev
            && left.st_ino == right.st_ino
            && left.st_mode == right.st_mode
            && left.st_nlink == right.st_nlink
            && left.st_size == right.st_size
            && left.st_mtime == right.st_mtime
            && left.st_mtime_nsec == right.st_mtime_nsec
            && left.st_ctime == right.st_ctime
            && left.st_ctime_nsec == right.st_ctime_nsec
    }

    fn named_stat(parent: &FlatDirectory, name: &OsStr) -> io::Result<FileStat> {
        fstatat(parent, name, AtFlags::AT_SYMLINK_NOFOLLOW).map_err(io::Error::from)
    }

    fn opened_observation(
        file: &mut File,
        name: &OsStr,
    ) -> io::Result<(FileObservation, FileStat)> {
        let before = fstat(&*file).map_err(io::Error::from)?;
        if before.st_nlink != 1 || before.st_size < 0 {
            return Err(invalid());
        }
        let entry =
            entry_from_stat(name.to_owned(), &before, Path::new(name)).map_err(observed_error)?;
        if entry.kind != JournalEntryKind::RegularFile || entry.size > INPUT_LIMIT as u64 {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        file.seek(SeekFrom::Start(0))?;
        Read::by_ref(file)
            .take((INPUT_LIMIT as u64) + 1)
            .read_to_end(&mut bytes)?;
        let after = fstat(&*file).map_err(io::Error::from)?;
        let final_entry =
            entry_from_stat(name.to_owned(), &after, Path::new(name)).map_err(observed_error)?;
        if after.st_nlink != 1
            || !same_stat(&before, &after)
            || !same_entry_metadata(&entry, &final_entry)
            || u64::try_from(bytes.len()).ok() != Some(entry.size)
        {
            return Err(invalid());
        }
        Ok((FileObservation { entry, bytes }, after))
    }

    /// Append once through the retained parent and preserve independent effect facts.
    /// The caller retains and revalidates canonical ancestry and its owning exclusions.
    pub fn append_jsonl_bound<T: Serialize>(
        parent: &FlatDirectory,
        name: &OsStr,
        record: &T,
    ) -> BoundAppendReceipt {
        append_with_hooks(parent, name, record, &mut NoHooks)
    }

    pub(super) fn append_with_hooks<T: Serialize>(
        parent: &FlatDirectory,
        name: &OsStr,
        record: &T,
        hooks: &mut impl Hooks,
    ) -> BoundAppendReceipt {
        let mut receipt = BoundAppendReceipt {
            effect: BoundAppendEffect::NotAttempted,
            file_durability: BoundAppendDurability::Unproven,
            parent_durability: BoundAppendDurability::Unproven,
            confirmation: BoundAppendConfirmation::Unverified,
            failures: Vec::new(),
        };
        let preparation = (|| -> io::Result<(Vec<u8>, Option<FileObservation>)> {
            let mut encoded = RecordWriter(Vec::new());
            serde_json::to_writer(&mut encoded, record).map_err(observed_error)?;
            encoded.0.push(b'\n');
            parent.revalidate().map_err(observed_error)?;
            let baseline =
                read_observed_file_bounded(parent, name, INPUT_LIMIT).map_err(observed_error)?;
            if baseline.as_ref().is_some_and(|observed| {
                !observed.bytes.is_empty() && observed.bytes.last() != Some(&b'\n')
            }) {
                return Err(invalid());
            }
            Ok((encoded.0, baseline))
        })();
        let (contents, baseline) = match preparation {
            Ok(value) => value,
            Err(error) => {
                receipt.failures.push(error);
                return receipt;
            }
        };
        let opened = (|| -> io::Result<(File, Vec<u8>)> {
            hooks.checkpoint(Point::BeforeOpen);
            parent.revalidate().map_err(observed_error)?;
            receipt.effect = BoundAppendEffect::Attempted { written: None };
            let fd = openat(
                parent,
                name,
                OFlag::O_APPEND
                    | OFlag::O_RDWR
                    | OFlag::O_CREAT
                    | OFlag::O_NOFOLLOW
                    | OFlag::O_CLOEXEC
                    | OFlag::O_NONBLOCK,
                Mode::from_bits_truncate(0o600),
            )
            .map_err(io::Error::from)?;
            let mut file = File::from(fd);
            hooks.checkpoint(Point::BeforeWrite);
            let (opened, opened_stat) = opened_observation(&mut file, name)?;
            if baseline
                .as_ref()
                .is_some_and(|baseline| baseline != &opened)
                || (baseline.is_none() && !opened.bytes.is_empty())
                || !same_stat(&opened_stat, &named_stat(parent, name)?)
            {
                return Err(invalid());
            }
            let mut expected = opened.bytes;
            expected
                .len()
                .checked_add(contents.len())
                .ok_or_else(invalid)?;
            expected.extend_from_slice(&contents);
            parent.revalidate().map_err(observed_error)?;
            if !same_stat(&opened_stat, &named_stat(parent, name)?) {
                return Err(invalid());
            }
            Ok((file, expected))
        })();
        let (mut file, expected) = match opened {
            Ok(value) => value,
            Err(error) => {
                receipt.failures.push(error);
                return receipt;
            }
        };
        match hooks.write(&mut file, &contents) {
            Ok(written) if written == contents.len() => {
                receipt.effect = BoundAppendEffect::Appended { bytes: written }
            }
            Ok(written) => {
                receipt.effect = BoundAppendEffect::Attempted {
                    written: Some(written),
                };
                receipt.failures.push(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "bound append record was only partially written",
                ));
            }
            Err(error) => receipt.failures.push(error),
        }
        hooks.checkpoint(Point::AfterWrite);
        match hooks.sync(&file, Point::FileSync) {
            Ok(()) => receipt.file_durability = BoundAppendDurability::Synced,
            Err(error) => receipt.failures.push(error),
        }
        match hooks.sync(parent, Point::ParentSync) {
            Ok(()) => receipt.parent_durability = BoundAppendDurability::Synced,
            Err(error) => receipt.failures.push(error),
        }
        let confirmation = (|| -> io::Result<()> {
            let before = fstat(&file).map_err(io::Error::from)?;
            let observed = read_observed_file_bounded(parent, name, expected.len())
                .map_err(observed_error)?
                .ok_or_else(invalid)?;
            let retained = fstat(&file).map_err(io::Error::from)?;
            let retained_entry = entry_from_stat(name.to_owned(), &retained, Path::new(name))
                .map_err(observed_error)?;
            if retained.st_nlink != 1
                || !same_stat(&before, &retained)
                || observed.bytes != expected
                || !same_entry_metadata(&observed.entry, &retained_entry)
            {
                return Err(invalid());
            }
            hooks.checkpoint(Point::AfterConfirmationRead);
            if !same_stat(&retained, &named_stat(parent, name)?) {
                return Err(invalid());
            }
            parent.revalidate().map_err(observed_error)?;
            Ok(())
        })();
        match confirmation {
            Ok(()) if matches!(receipt.effect, BoundAppendEffect::Appended { .. }) => {
                receipt.confirmation = BoundAppendConfirmation::Confirmed
            }
            Ok(()) => {}
            Err(error) => receipt.failures.push(error),
        }
        receipt
    }
}

#[cfg(all(test, unix, feature = "full-tests"))]
#[path = "../tests/support/append_bound.rs"]
mod bound_tests;

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn appends_one_complete_newline_terminated_record_per_call() {
        let temporary = TempDir::new();
        let path = temporary.path().join("records.jsonl");
        append_text(&path, "first").unwrap();
        let first_len = fs::metadata(&path).unwrap().len();
        append_jsonl(&path, &serde_json::json!({"second": true})).unwrap();
        let contents = fs::read_to_string(&path).unwrap();

        assert_eq!(first_len, "first\n".len() as u64);
        assert_eq!(
            contents.lines().collect::<Vec<_>>(),
            vec!["first", r#"{"second":true}"#]
        );
        assert!(contents.ends_with('\n'));
    }

    #[test]
    fn appends_jsonl_as_one_complete_newline_terminated_record_per_call() {
        let temporary = TempDir::new();
        let path = temporary.path().join("records.jsonl");
        append_jsonl(&path, &serde_json::json!({"first": true})).unwrap();
        let first_len = fs::metadata(&path).unwrap().len();
        append_jsonl(&path, &serde_json::json!({"second": true})).unwrap();
        let contents = fs::read_to_string(&path).unwrap();

        assert_eq!(first_len, r#"{"first":true}"#.len() as u64 + 1);
        assert_eq!(
            contents.lines().collect::<Vec<_>>(),
            vec![r#"{"first":true}"#, r#"{"second":true}"#]
        );
        assert!(contents.ends_with('\n'));
    }

    #[test]
    fn appends_empty_text_as_one_complete_newline_terminated_record() {
        let temporary = TempDir::new();
        let path = temporary.path().join("records.jsonl");
        append_text(&path, "").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"\n");
    }

    #[test]
    fn appends_nonempty_text_as_one_complete_newline_terminated_record() {
        let temporary = TempDir::new();
        let path = temporary.path().join("records.jsonl");
        append_text(&path, "first").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first\n");
    }

    #[test]
    fn appends_repeated_text_records_accumulate_in_call_order() {
        let temporary = TempDir::new();
        let path = temporary.path().join("records.jsonl");
        append_text(&path, "a").unwrap();
        append_text(&path, "b").unwrap();
        append_text(&path, "c").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"a\nb\nc\n");
    }

    #[test]
    fn appends_text_into_a_missing_nested_parent_directory() {
        let temporary = TempDir::new();
        let path = temporary.path().join("nested/a/b/records.jsonl");
        append_text(&path, "first").unwrap();
        assert!(path.parent().unwrap().is_dir());
        assert_eq!(fs::read(&path).unwrap(), b"first\n");
    }

    #[test]
    fn append_jsonl_no_follow_appends_records_in_order() {
        let temporary = TempDir::new();
        let path = temporary.path().join("records.jsonl");
        append_jsonl_no_follow(&path, &serde_json::json!({"first": true})).unwrap();
        append_jsonl_no_follow(&path, &serde_json::json!({"second": 2})).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(
            contents.lines().collect::<Vec<_>>(),
            vec![r#"{"first":true}"#, r#"{"second":2}"#]
        );
        assert!(contents.ends_with('\n'));
    }

    #[cfg(unix)]
    #[test]
    fn append_jsonl_no_follow_refuses_symlinked_leaf_on_unix() {
        let temporary = TempDir::new();
        let target = temporary.path().join("target.jsonl");
        let link = temporary.path().join("link.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let error =
            append_jsonl_no_follow(&link, &serde_json::json!({"blocked": true})).unwrap_err();
        assert!(matches!(error, AppendError::Io { .. }));
        assert!(!target.exists() || fs::read(&target).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn append_jsonl_no_follow_refuses_missing_parent_and_creates_nothing_on_unix() {
        let temporary = TempDir::new();
        let missing_parent = temporary.path().join("missing_dir");
        let path = missing_parent.join("records.jsonl");

        let error =
            append_jsonl_no_follow(&path, &serde_json::json!({"blocked": true})).unwrap_err();
        assert!(matches!(error, AppendError::Io { .. }));
        assert!(!missing_parent.exists());
        assert!(!path.exists());
    }

    #[cfg(windows)]
    #[test]
    fn append_jsonl_no_follow_accumulates_records_in_order_on_windows() {
        let temporary = TempDir::new();
        let path = temporary.path().join("records.jsonl");
        append_jsonl_no_follow(&path, &serde_json::json!({"first": 1})).unwrap();
        append_jsonl_no_follow(&path, &serde_json::json!({"second": 2})).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(
            contents.lines().collect::<Vec<_>>(),
            vec![r#"{"first":1}"#, r#"{"second":2}"#]
        );
        assert!(contents.ends_with('\n'));
    }

    #[cfg(windows)]
    #[test]
    fn append_jsonl_no_follow_refuses_symlinked_leaf_on_windows() {
        let temporary = TempDir::new();
        let target = temporary.path().join("target.jsonl");
        fs::write(&target, b"").unwrap();
        let link = temporary.path().join("link.jsonl");
        if std::os::windows::fs::symlink_file(&target, &link).is_err() {
            return;
        }

        let error =
            append_jsonl_no_follow(&link, &serde_json::json!({"blocked": true})).unwrap_err();
        assert!(matches!(error, AppendError::Io { .. }));
        assert!(fs::read(&target).unwrap().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn append_jsonl_no_follow_refuses_dangling_symlink_on_windows() {
        let temporary = TempDir::new();
        let target = temporary.path().join("nonexistent_target.jsonl");
        let link = temporary.path().join("link.jsonl");
        if std::os::windows::fs::symlink_file(&target, &link).is_err() {
            return;
        }

        let error =
            append_jsonl_no_follow(&link, &serde_json::json!({"blocked": true})).unwrap_err();
        assert!(matches!(error, AppendError::Io { .. }));
        assert!(!target.exists());
    }

    #[cfg(windows)]
    #[test]
    fn append_jsonl_no_follow_refuses_missing_parent_and_creates_nothing_on_windows() {
        let temporary = TempDir::new();
        let missing_parent = temporary.path().join("missing_dir");
        let path = missing_parent.join("records.jsonl");

        let error =
            append_jsonl_no_follow(&path, &serde_json::json!({"blocked": true})).unwrap_err();
        assert!(matches!(error, AppendError::Io { .. }));
        assert!(!missing_parent.exists());
        assert!(!path.exists());
    }
}
