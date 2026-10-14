// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Checkpoint one existing database without resolving its retained parent to a pathname.
//!
//! SQLite's Unix fullname resolver rejects the intentional /proc/self/fd prefix
//! with NOFOLLOW. Only that internally constructed prefix is exempted here.
//! Fixed leaves are checked at fullname/open; native Unix locks and NOFOLLOW
//! remain enabled. Like the normal Unix VFS, this is not an atomic leaf-swap guard.

use std::cell::UnsafeCell;
use std::ffi::{CStr, CString, c_char, c_int};
use std::fs::OpenOptions;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use nix::errno::Errno;
use nix::fcntl::AtFlags;
use nix::sys::stat::{SFlag, fstatat};
use rusqlite::{Connection, OpenFlags, ffi};
use solstone_core_journal_io::flat_directory::{FlatDirectory, open_flat_directory_bound};

use super::{BodyRebuildError, BodyRebuildErrorKind, error};

static NEXT_VFS: AtomicU64 = AtomicU64::new(0);
const LEAVES: [&CStr; 4] = [
    c"health-dedupe.sqlite",
    c"health-dedupe.sqlite-wal",
    c"health-dedupe.sqlite-journal",
    c"health-dedupe.sqlite-shm",
];

fn failure() -> BodyRebuildError {
    error(BodyRebuildErrorKind::Publication, "checkpoint_database")
}

fn regular_leaf(directory: &FlatDirectory, leaf: &CStr, required: bool) -> bool {
    match fstatat(directory, leaf, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) => SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT == SFlag::S_IFREG,
        Err(Errno::ENOENT) => !required,
        Err(_) => false,
    }
}

// SQLite may mutate the intrusive VFS list link even while callbacks borrow our
// immutable state. Only the first field is exposed as writable C memory.
#[repr(C)]
struct Registration {
    vfs: UnsafeCell<ffi::sqlite3_vfs>,
    base: *mut ffi::sqlite3_vfs,
    name: CString,
    database: CString,
    directory: FlatDirectory,
    registered: bool,
}

impl Registration {
    fn admit(path: &Path) -> Result<Box<Self>, BodyRebuildError> {
        let journal = path.parent().and_then(Path::parent).ok_or_else(failure)?;
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY)
            .open(journal)
            .map_err(|_| failure())?;
        let directory = open_flat_directory_bound(&root, std::ffi::OsStr::new("imports"), journal)
            .map_err(|_| failure())?
            .ok_or_else(failure)?;
        for (index, leaf) in LEAVES.iter().enumerate() {
            if !regular_leaf(&directory, leaf, index == 0) {
                return Err(failure());
            }
        }
        let sequence = NEXT_VFS
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| failure())?;
        let name =
            CString::new(format!("solstone-body-checkpoint-{sequence}")).map_err(|_| failure())?;
        let database = CString::new(format!(
            "/proc/self/fd/{}/health-dedupe.sqlite",
            directory.as_fd().as_raw_fd()
        ))
        .map_err(|_| failure())?;
        // SAFETY: initialization and the built-in named VFS are SQLite's public
        // API. The Unix VFS lives for SQLite's initialized process lifetime.
        let base = unsafe {
            if ffi::sqlite3_initialize() != ffi::SQLITE_OK {
                return Err(failure());
            }
            ffi::sqlite3_vfs_find(c"unix".as_ptr())
        };
        if base.is_null() {
            return Err(failure());
        }
        // SAFETY: base is the built-in live VFS. Copy only immutable fields;
        // pNext belongs to SQLite's synchronized registry and is never read.
        let vfs = unsafe {
            ffi::sqlite3_vfs {
                iVersion: (*base).iVersion,
                szOsFile: (*base).szOsFile,
                mxPathname: (*base).mxPathname,
                pNext: ptr::null_mut(),
                zName: name.as_ptr(),
                pAppData: (*base).pAppData,
                xOpen: Some(bound_open),
                xDelete: (*base).xDelete,
                xAccess: (*base).xAccess,
                xFullPathname: Some(bound_fullname),
                xDlOpen: (*base).xDlOpen,
                xDlError: (*base).xDlError,
                xDlSym: (*base).xDlSym,
                xDlClose: (*base).xDlClose,
                xRandomness: (*base).xRandomness,
                xSleep: (*base).xSleep,
                xCurrentTime: (*base).xCurrentTime,
                xGetLastError: (*base).xGetLastError,
                xCurrentTimeInt64: (*base).xCurrentTimeInt64,
                xSetSystemCall: (*base).xSetSystemCall,
                xGetSystemCall: (*base).xGetSystemCall,
                xNextSystemCall: (*base).xNextSystemCall,
            }
        };
        let mut registration = Box::new(Self {
            vfs: UnsafeCell::new(vfs),
            base,
            name,
            database,
            directory,
            registered: false,
        });
        // SAFETY: the Box and all CString allocations stay stable until after
        // our private connection is closed. This never replaces the default.
        if unsafe { ffi::sqlite3_vfs_register(registration.vfs.get(), 0) } != ffi::SQLITE_OK {
            return Err(failure());
        }
        registration.registered = true;
        Ok(registration)
    }

    fn run(&self) -> Result<(), BodyRebuildError> {
        let connection = Connection::open_with_flags_and_vfs(
            self.database.to_str().map_err(|_| failure())?,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            self.name.as_c_str(),
        )
        .map_err(|_| failure())?;
        // Must precede the first database access: pagerOpenWal takes a real
        // EXCLUSIVE file lock and uses a heap WAL index, never inode-shared SHM
        // with a filename that could outlive our retained directory descriptor.
        let locking: String = connection
            .query_row("PRAGMA locking_mode=EXCLUSIVE", [], |row| row.get(0))
            .map_err(|_| failure())?;
        if !locking.eq_ignore_ascii_case("exclusive") {
            return Err(failure());
        }
        let mode: String = connection
            .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))
            .map_err(|_| failure())?;
        if !mode.eq_ignore_ascii_case("delete") {
            return Err(failure());
        }
        connection.close().map_err(|_| failure())
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if self.registered {
            // SAFETY: run never lets a connection escape. On query errors its
            // local Connection drops first; on close errors the returned owner
            // also drops before run returns. Only query_row creates statements,
            // and it finalizes them before returning; no VDBE/backup can escape
            // and make sqlite3_close return BUSY with live file callbacks.
            // SQLite unregister returns OK even
            // if a name is already absent and never closes files itself.
            unsafe { ffi::sqlite3_vfs_unregister(self.vfs.get()) };
        }
    }
}

pub(super) fn checkpoint(path: &Path) -> Result<(), BodyRebuildError> {
    let registration = Registration::admit(path)?;
    registration.run()
}

unsafe extern "C" fn bound_fullname(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    capacity: c_int,
    output: *mut c_char,
) -> c_int {
    if vfs.is_null() || name.is_null() || output.is_null() || capacity <= 0 {
        return ffi::SQLITE_CANTOPEN;
    }
    // SAFETY: SQLite supplies a live registered VFS, a NUL-terminated filename,
    // and capacity bytes of writable output. repr(C) puts vfs at offset zero.
    let state = unsafe { &*vfs.cast::<Registration>() };
    let requested = unsafe { CStr::from_ptr(name) };
    #[cfg(all(test, feature = "full-tests"))]
    if !tests::entry(tests::Boundary::Fullname) {
        return ffi::SQLITE_CANTOPEN;
    }
    let bytes = state.database.as_bytes_with_nul();
    if requested != state.database.as_c_str()
        || (capacity as usize) < bytes.len()
        || !regular_leaf(&state.directory, LEAVES[0], true)
    {
        return ffi::SQLITE_CANTOPEN;
    }
    // SAFETY: bytes includes its terminator and the bound above fits output.
    unsafe { ptr::copy_nonoverlapping(bytes.as_ptr().cast(), output, bytes.len()) };
    ffi::SQLITE_OK
}

unsafe extern "C" fn bound_open(
    vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    output_flags: *mut c_int,
) -> c_int {
    if vfs.is_null() || name.is_null() || file.is_null() {
        return ffi::SQLITE_CANTOPEN;
    }
    // SAFETY: SQLite supplies live VFS/file storage and a terminated filename.
    let state = unsafe { &*vfs.cast::<Registration>() };
    let requested = unsafe { CStr::from_ptr(name) }.to_bytes();
    unsafe { (*file).pMethods = ptr::null() };
    #[cfg(all(test, feature = "full-tests"))]
    if !tests::entry(tests::Boundary::Open) {
        return ffi::SQLITE_CANTOPEN;
    }
    let Some(suffix) = requested.strip_prefix(state.database.as_bytes()) else {
        return ffi::SQLITE_CANTOPEN;
    };
    let leaf = match suffix {
        b"" => LEAVES[0],
        b"-wal" => LEAVES[1],
        b"-journal" => LEAVES[2],
        _ => return ffi::SQLITE_CANTOPEN,
    };
    if !regular_leaf(&state.directory, leaf, suffix.is_empty()) {
        return ffi::SQLITE_CANTOPEN;
    }
    // SAFETY: the original built-in VFS accepts this same native file size,
    // SQLite filename allocation (including its metadata), flags/output. Give
    // it its original VFS/pAppData; its file methods then retain that base VFS.
    let Some(open) = (unsafe { (*state.base).xOpen }) else {
        return ffi::SQLITE_CANTOPEN;
    };
    let result = unsafe { open(state.base, name, file, flags, output_flags) };
    #[cfg(all(test, feature = "full-tests"))]
    if result == ffi::SQLITE_OK {
        // SAFETY: a successful native open installed its live I/O methods.
        unsafe { tests::instrument_io(file) };
    }
    result
}

#[cfg(all(test, feature = "full-tests"))]
mod tests;
