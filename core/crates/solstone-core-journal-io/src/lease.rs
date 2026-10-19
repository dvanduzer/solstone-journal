// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Advisory file leases held for the lifetime of their guard.

use std::ffi::OsStr;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io;
#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use nix::errno::Errno;
#[cfg(unix)]
use nix::fcntl::{FcntlArg, Flock, FlockArg, OFlag, fcntl, openat};
#[cfg(unix)]
use nix::sys::stat::{Mode, SFlag, fchmod, fstat};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT, OPEN_ALWAYS,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use crate::errors::LeaseError;
#[cfg(windows)]
use crate::windows_lock::{
    WindowsLockGuard, WindowsLockHeld, is_contention as windows_contention, try_lock_exclusive,
    try_lock_exclusive_held,
};

/// Python-compatible number of nonblocking acquisition attempts.
pub const DEFAULT_LEASE_ATTEMPTS: usize = 5;
/// Python-compatible total retry window, shared by every attempt.
pub const DEFAULT_LEASE_RETRY_MAX: Duration = Duration::from_millis(250);
/// Mode applied to every lease file, including files that predate this acquire.
pub const DEFAULT_LEASE_MODE: u32 = 0o600;

/// Options for acquiring a process-lifetime advisory lease.
#[derive(Debug, Clone, Copy)]
pub struct LeaseOptions {
    /// Number of acquisition attempts; zero is treated as one.
    pub attempts: usize,
    /// Total retry window; the deadline is set once before the first attempt.
    pub retry_max: Duration,
    /// Lease-file permission bits. Windows does not apply this field; it has no ACL equivalent.
    pub mode: u32,
}

impl Default for LeaseOptions {
    fn default() -> Self {
        Self {
            attempts: DEFAULT_LEASE_ATTEMPTS,
            retry_max: DEFAULT_LEASE_RETRY_MAX,
            mode: DEFAULT_LEASE_MODE,
        }
    }
}

/// An exclusive lease on the lease file itself. Dropping it closes and unlocks it.
#[derive(Debug)]
pub struct FileLease {
    #[cfg(unix)]
    _guard: Flock<File>,
    #[cfg(windows)]
    _guard: WindowsLockGuard,
    path: PathBuf,
}

/// Exclusive lease taken on an already-open file's own descriptor.
///
/// Dropping this value closes the lease descriptor. It does not close the
/// caller's original `File`. The advisory lock is released when the last
/// descriptor of this open file description (Unix) or locked handle (Windows)
/// closes; there is no explicit unlock on drop.
#[derive(Debug)]
pub struct SelfLease {
    #[cfg(unix)]
    #[allow(dead_code)]
    file: File,
    #[cfg(windows)]
    _held: WindowsLockHeld,
}

/// Result of a non-blocking exclusive try-lock used as a liveness probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaseProbe {
    /// An exclusive lock is currently held.
    Active,
    /// An exclusive lock was acquired and released immediately.
    Released,
    /// The probe could not classify the lease.
    Indeterminate,
}

impl FileLease {
    /// The lease file protected by this guard.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Prove the original self-locked regular file still occupies this direct
    /// name beneath the retained parent. Does not create or take another lease.
    #[cfg(unix)]
    pub fn revalidate_bound(&self, parent: &impl AsFd, name: &OsStr) -> Result<(), LeaseError> {
        validate_bound_name(name)?;
        let path = Path::new(name);
        let named = openat(
            parent,
            name,
            OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| io_error(path, io::Error::from_raw_os_error(error as i32)))?;
        let held = fstat(&*self._guard)
            .map_err(|error| io_error(path, io::Error::from_raw_os_error(error as i32)))?;
        let current = fstat(&named)
            .map_err(|error| io_error(path, io::Error::from_raw_os_error(error as i32)))?;
        if SFlag::from_bits_truncate(held.st_mode).intersection(SFlag::S_IFMT) != SFlag::S_IFREG
            || SFlag::from_bits_truncate(current.st_mode).intersection(SFlag::S_IFMT)
                != SFlag::S_IFREG
            || held.st_dev != current.st_dev
            || held.st_ino != current.st_ino
        {
            return Err(io_error(path, io::Error::other("lease entry changed")));
        }
        Ok(())
    }

    #[cfg(windows)]
    pub fn revalidate_bound(&self, parent: &impl AsHandle, name: &OsStr) -> Result<(), LeaseError> {
        validate_bound_name(name)?;
        let path = Path::new(name);
        let named = open_bound_lease_windows(
            parent,
            name,
            windows_sys::Wdk::Storage::FileSystem::FILE_OPEN,
        )?;
        let identity = |file: &File| {
            crate::windows_identity::file_identity(file.as_raw_handle())
                .map_err(|error| io_error(path, error))
        };
        if identity(self._guard.file())? != identity(&named)? {
            return Err(io_error(path, io::Error::other("lease entry changed")));
        }
        Ok(())
    }

    /// Duplicate this lease descriptor for inheritance across an `exec`.
    ///
    /// Callers pair the inherited descriptor with their own authenticated token
    /// in the lease file and close the returned descriptor when their process
    /// tree no longer needs it.
    #[cfg(unix)]
    pub fn duplicate_for_inheritance(&self) -> Result<i32, LeaseError> {
        fcntl(&*self._guard, FcntlArg::F_DUPFD(3))
            .map_err(|error| io_error(&self.path, io::Error::from_raw_os_error(error as i32)))
    }
}

fn validate_bound_name(name: &OsStr) -> Result<(), LeaseError> {
    if name
        .to_str()
        .is_none_or(|name| crate::name_admission::check_portable_component(name).is_err())
    {
        return Err(io_error(
            Path::new(name),
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "lease name must be one portable component",
            ),
        ));
    }
    Ok(())
}

/// Acquire the lease file itself beneath a retained parent with the existing
/// attempt/retry/mode policy. Never opens or creates a parent by pathname.
#[cfg(unix)]
pub fn acquire_file_lease_bound_checked(
    parent: &impl AsFd,
    name: &OsStr,
    options: LeaseOptions,
    checkpoint: &dyn Fn() -> Result<(), String>,
) -> Result<Option<FileLease>, LeaseError> {
    validate_bound_name(name)?;
    let path = Path::new(name);
    let check = || {
        checkpoint()
            .map_err(|detail| io_error(path, io::Error::new(io::ErrorKind::Interrupted, detail)))
    };
    let deadline = Instant::now()
        .checked_add(options.retry_max)
        .unwrap_or_else(Instant::now);
    let attempts = options.attempts.max(1);
    for attempt in 0..attempts {
        check()?;
        let file = File::from(
            openat(
                parent,
                name,
                OFlag::O_RDWR
                    | OFlag::O_CREAT
                    | OFlag::O_NONBLOCK
                    | OFlag::O_CLOEXEC
                    | OFlag::O_NOFOLLOW,
                Mode::from_bits_truncate(options.mode as nix::libc::mode_t),
            )
            .map_err(|error| io_error(path, io::Error::from_raw_os_error(error as i32)))?,
        );
        check()?;
        let stat = fstat(&file)
            .map_err(|error| io_error(path, io::Error::from_raw_os_error(error as i32)))?;
        if SFlag::from_bits_truncate(stat.st_mode).intersection(SFlag::S_IFMT) != SFlag::S_IFREG {
            return Err(io_error(
                path,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "lease entry is not a regular file",
                ),
            ));
        }
        fchmod(
            &file,
            Mode::from_bits_truncate(options.mode as nix::libc::mode_t),
        )
        .map_err(|error| io_error(path, io::Error::from_raw_os_error(error as i32)))?;
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(guard) => {
                let lease = FileLease {
                    _guard: guard,
                    path: path.to_path_buf(),
                };
                check()?;
                lease.revalidate_bound(parent, name)?;
                return Ok(Some(lease));
            }
            Err((file, error)) if is_contention(error) => {
                drop(file);
                check()?;
                if attempt == attempts - 1 || Instant::now() >= deadline {
                    return Ok(None);
                }
                thread::sleep(retry_delay(deadline));
            }
            Err((file, error)) => {
                drop(file);
                return Err(io_error(path, io::Error::from_raw_os_error(error as i32)));
            }
        }
    }
    Ok(None)
}

#[cfg(windows)]
fn open_bound_lease_windows(
    parent: &impl AsHandle,
    name: &OsStr,
    disposition: u32,
) -> Result<File, LeaseError> {
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_NON_DIRECTORY_FILE, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
    };
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    let path = Path::new(name);
    let file = crate::windows_ntcreate::nt_create_relative(
        parent.as_handle().as_raw_handle(),
        name,
        GENERIC_READ | GENERIC_WRITE | SYNCHRONIZE,
        disposition,
        FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
    )
    .map(File::from)
    .map_err(|error| io_error(path, error))?;
    crate::windows_sync_dir::validate_windows_regular_handle(file.as_raw_handle(), path).map_err(
        |error| match error {
            crate::errors::FlatDirectoryError::Io { source, .. } => io_error(path, source),
            other => io_error(
                path,
                io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
            ),
        },
    )?;
    Ok(file)
}

/// Acquire the lease file itself beneath a retained parent, preserving the
/// existing attempt/retry policy and Windows mode semantics.
#[cfg(windows)]
pub fn acquire_file_lease_bound_checked(
    parent: &impl AsHandle,
    name: &OsStr,
    options: LeaseOptions,
    checkpoint: &dyn Fn() -> Result<(), String>,
) -> Result<Option<FileLease>, LeaseError> {
    validate_bound_name(name)?;
    let path = Path::new(name);
    let check = || {
        checkpoint()
            .map_err(|detail| io_error(path, io::Error::new(io::ErrorKind::Interrupted, detail)))
    };
    let deadline = Instant::now()
        .checked_add(options.retry_max)
        .unwrap_or_else(Instant::now);
    let attempts = options.attempts.max(1);
    for attempt in 0..attempts {
        check()?;
        let file = open_bound_lease_windows(
            parent,
            name,
            windows_sys::Wdk::Storage::FileSystem::FILE_OPEN_IF,
        )?;
        check()?;
        match try_lock_exclusive(file) {
            Ok(guard) => {
                let lease = FileLease {
                    _guard: guard,
                    path: path.to_path_buf(),
                };
                check()?;
                lease.revalidate_bound(parent, name)?;
                return Ok(Some(lease));
            }
            Err((file, error)) if windows_contention(&error) => {
                drop(file);
                check()?;
                if attempt == attempts - 1 || Instant::now() >= deadline {
                    return Ok(None);
                }
                thread::sleep(retry_delay(deadline));
            }
            Err((file, error)) => {
                drop(file);
                return Err(io_error(path, error));
            }
        }
    }
    Ok(None)
}

/// Probe an exclusive nonblocking flock on `fd` without unlocking afterward.
///
/// This issues `flock(LOCK_EX | LOCK_NB)` on the borrowed descriptor and never
/// follows it with `LOCK_UN` or an owning flock guard. Wrapping `fd` in
/// [`Flock`] would `LOCK_UN` on drop and, for a duplicate of a held open file
/// description, would release the owner's lock as well.
///
/// Returns `Ok(true)` when the exclusive flock succeeds (same-OFD idempotent
/// re-lock, or the lock was free and is now held on this OFD). Returns
/// `Ok(false)` on contention. Any other OS error is `Err`.
#[cfg(unix)]
pub fn probe_exclusive_flock_no_release(fd: RawFd) -> io::Result<bool> {
    // SAFETY: the caller supplies a live descriptor; `LOCK_NB` makes the
    // call non-blocking; this function never unlocks.
    #[allow(unsafe_code)]
    let result = unsafe { nix::libc::flock(fd, nix::libc::LOCK_EX | nix::libc::LOCK_NB) };
    if result == 0 {
        Ok(true)
    } else {
        let error = Errno::last();
        if is_contention(error) {
            Ok(false)
        } else {
            Err(io::Error::from_raw_os_error(error as i32))
        }
    }
}

/// Acquire an exclusive nonblocking lease with bounded jittered retries.
///
/// Unlike [`crate::hold_lock`], this locks `path` itself and returns a guard
/// meant to stay alive across work outside a short critical section.
#[cfg(unix)]
pub fn acquire_file_lease(
    path: impl AsRef<Path>,
    options: LeaseOptions,
) -> Result<Option<FileLease>, LeaseError> {
    acquire_file_lease_checked(path, options, &|| Ok(()))
}

/// Acquire a lease while renewing the caller's authority before opening,
/// during contention, and after acquiring the lock.
#[cfg(unix)]
pub fn acquire_file_lease_checked(
    path: impl AsRef<Path>,
    options: LeaseOptions,
    checkpoint: &dyn Fn() -> Result<(), String>,
) -> Result<Option<FileLease>, LeaseError> {
    let path = path.as_ref();
    let check = || {
        checkpoint()
            .map_err(|detail| io_error(path, io::Error::new(io::ErrorKind::Interrupted, detail)))
    };
    check()?;
    fs::create_dir_all(parent_dir(path)).map_err(|source| io_error(path, source))?;
    let attempts = options.attempts.max(1);
    let deadline = Instant::now()
        .checked_add(options.retry_max)
        .unwrap_or_else(Instant::now);

    for attempt in 0..attempts {
        check()?;
        let mut open_options = OpenOptions::new();
        open_options.read(true).write(true).create(true);
        #[cfg(unix)]
        open_options.mode(options.mode);
        let file = open_options
            .open(path)
            .map_err(|source| io_error(path, source))?;
        check()?;
        // `mode_t` is u32 on Linux and u16 on Apple targets, so the bitflags
        // constructor takes a different width per platform. Narrowing through
        // the libc type keeps one expression correct on both rather than
        // pinning whichever one the host happens to be.
        fchmod(
            &file,
            Mode::from_bits_truncate(options.mode as nix::libc::mode_t),
        )
        .map_err(|source| io_error(path, io::Error::from_raw_os_error(source as i32)))?;

        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(guard) => {
                check()?;
                return Ok(Some(FileLease {
                    _guard: guard,
                    path: path.to_path_buf(),
                }));
            }
            Err((file, error)) if is_contention(error) => {
                drop(file);
                check()?;
                if attempt == attempts - 1 || Instant::now() >= deadline {
                    return Ok(None);
                }
                thread::sleep(retry_delay(deadline));
            }
            Err((file, error)) => {
                drop(file);
                return Err(io_error(path, io::Error::from_raw_os_error(error as i32)));
            }
        }
    }
    Ok(None)
}

/// Take an exclusive nonblocking lease on `file` itself, without a companion path.
pub fn acquire_self_lease(file: &File) -> Result<Option<SelfLease>, LeaseError> {
    #[cfg(unix)]
    {
        acquire_self_lease_unix(file)
    }
    #[cfg(windows)]
    {
        acquire_self_lease_windows(file)
    }
}

/// Non-blocking exclusive try-lock used as a liveness probe on an open handle.
pub fn probe_file_lease(file: &File) -> LeaseProbe {
    match acquire_self_lease(file) {
        Ok(Some(lease)) => {
            drop(lease);
            LeaseProbe::Released
        }
        Ok(None) => LeaseProbe::Active,
        Err(_) => LeaseProbe::Indeterminate,
    }
}

#[cfg(unix)]
fn acquire_self_lease_unix(file: &File) -> Result<Option<SelfLease>, LeaseError> {
    let duplicated = fcntl(file, FcntlArg::F_DUPFD_CLOEXEC(3)).map_err(|error| {
        io_error(
            Path::new("self-lease"),
            io::Error::from_raw_os_error(error as i32),
        )
    })?;
    // SAFETY: `duplicated` is a freshly allocated descriptor owned by this function.
    #[allow(unsafe_code)]
    let owned = unsafe { OwnedFd::from_raw_fd(duplicated) };
    let file = File::from(owned);
    // SAFETY: `file` owns a live descriptor; `LOCK_NB` makes this call non-blocking.
    #[allow(unsafe_code)]
    let result =
        unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_EX | nix::libc::LOCK_NB) };
    if result == 0 {
        Ok(Some(SelfLease { file }))
    } else {
        let error = Errno::last();
        drop(file);
        if is_contention(error) {
            Ok(None)
        } else {
            Err(io_error(
                Path::new("self-lease"),
                io::Error::from_raw_os_error(error as i32),
            ))
        }
    }
}

#[cfg(windows)]
fn acquire_self_lease_windows(file: &File) -> Result<Option<SelfLease>, LeaseError> {
    let duplicated = duplicate_windows_handle(file)?;
    match try_lock_exclusive_held(duplicated) {
        Ok(held) => Ok(Some(SelfLease { _held: held })),
        Err((file, error)) if windows_contention(&error) => {
            drop(file);
            Ok(None)
        }
        Err((file, error)) => {
            drop(file);
            Err(io_error(Path::new("self-lease"), error))
        }
    }
}

#[cfg(windows)]
fn duplicate_windows_handle(file: &File) -> Result<File, LeaseError> {
    let mut duplicated = INVALID_HANDLE_VALUE;
    // SAFETY: source handle is a live `File`; output pointer is a local HANDLE.
    #[allow(unsafe_code)]
    let result = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            file.as_raw_handle(),
            GetCurrentProcess(),
            &mut duplicated,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if result == 0 || duplicated == INVALID_HANDLE_VALUE {
        return Err(io_error(
            Path::new("self-lease"),
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: `duplicated` is an owned handle returned by DuplicateHandle.
    #[allow(unsafe_code)]
    Ok(File::from(unsafe {
        OwnedHandle::from_raw_handle(duplicated)
    }))
}

#[cfg(unix)]
fn is_contention(error: Errno) -> bool {
    error == Errno::EACCES || error == Errno::EAGAIN || error == Errno::EWOULDBLOCK
}

fn retry_delay(deadline: Instant) -> Duration {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let minimum = Duration::from_millis(10);
    let maximum = Duration::from_millis(250);
    let jitter = if minimum >= maximum {
        maximum
    } else {
        let span = maximum - minimum;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let entropy = now ^ u128::from(std::process::id());
        minimum + Duration::from_nanos((entropy % (span.as_nanos() + 1)) as u64)
    };
    remaining.min(jitter)
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn io_error(path: &Path, source: io::Error) -> LeaseError {
    LeaseError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(windows)]
pub fn acquire_file_lease(
    path: impl AsRef<Path>,
    options: LeaseOptions,
) -> Result<Option<FileLease>, LeaseError> {
    acquire_file_lease_checked(path, options, &|| Ok(()))
}

/// Acquire a lease while renewing the caller's authority before opening,
/// during contention, and after acquiring the lock.
#[cfg(windows)]
pub fn acquire_file_lease_checked(
    path: impl AsRef<Path>,
    options: LeaseOptions,
    checkpoint: &dyn Fn() -> Result<(), String>,
) -> Result<Option<FileLease>, LeaseError> {
    let path = path.as_ref();
    let check = || {
        checkpoint()
            .map_err(|detail| io_error(path, io::Error::new(io::ErrorKind::Interrupted, detail)))
    };
    check()?;
    fs::create_dir_all(parent_dir(path)).map_err(|source| io_error(path, source))?;
    let attempts = options.attempts.max(1);
    let deadline = Instant::now()
        .checked_add(options.retry_max)
        .unwrap_or_else(Instant::now);

    for attempt in 0..attempts {
        check()?;
        let file = crate::locking::open_windows_path(
            path,
            GENERIC_READ | GENERIC_WRITE,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
        )
        .map_err(|source| io_error(path, source))?;
        check()?;
        match try_lock_exclusive(file) {
            Ok(guard) => {
                check()?;
                return Ok(Some(FileLease {
                    _guard: guard,
                    path: path.to_path_buf(),
                }));
            }
            Err((file, error)) if windows_contention(&error) => {
                drop(file);
                check()?;
                if attempt == attempts - 1 || Instant::now() >= deadline {
                    return Ok(None);
                }
                thread::sleep(retry_delay(deadline));
            }
            Err((file, error)) => {
                drop(file);
                return Err(io_error(path, error));
            }
        }
    }
    Ok(None)
}

#[cfg(all(test, feature = "full-tests"))]
mod bound_lease_tests {
    use super::*;
    use crate::{JournalRoot, test_support::TempDir};

    fn immediate() -> LeaseOptions {
        LeaseOptions {
            attempts: 1,
            retry_max: Duration::ZERO,
            ..LeaseOptions::default()
        }
    }

    #[test]
    fn checked_bound_lease_contends_with_the_original_path_lease_and_releases() {
        let temporary = TempDir::new();
        let parent = JournalRoot::open(temporary.path()).unwrap();
        let name = OsStr::new("brain-refresh.lease");
        let path = temporary.path().join(name);
        let bound = acquire_file_lease_bound_checked(&parent, name, immediate(), &|| Ok(()))
            .unwrap()
            .unwrap();
        bound.revalidate_bound(&parent, name).unwrap();
        assert!(acquire_file_lease(&path, immediate()).unwrap().is_none());
        drop(bound);
        let original = acquire_file_lease(&path, immediate()).unwrap().unwrap();
        assert!(
            acquire_file_lease_bound_checked(&parent, name, immediate(), &|| Ok(()))
                .unwrap()
                .is_none()
        );
        drop(original);
        assert!(
            acquire_file_lease_bound_checked(&parent, name, immediate(), &|| Ok(()))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn checked_bound_lease_revalidation_refuses_replacement_without_unlocking_the_original() {
        let temporary = TempDir::new();
        let parent = JournalRoot::open(temporary.path()).unwrap();
        let name = OsStr::new("brain-refresh.lease");
        let path = temporary.path().join(name);
        let old = temporary.path().join("original.lease");
        let lease = acquire_file_lease_bound_checked(&parent, name, immediate(), &|| Ok(()))
            .unwrap()
            .unwrap();
        fs::rename(&path, &old).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert!(lease.revalidate_bound(&parent, name).is_err());
        assert!(acquire_file_lease(&old, immediate()).unwrap().is_none());
        drop(lease);
        assert!(acquire_file_lease(&old, immediate()).unwrap().is_some());
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
    }

    #[test]
    fn checked_bound_lease_root_replacement_after_check_receives_no_lease() {
        let temporary = TempDir::new();
        let journal = temporary.path().join("journal");
        let displaced = temporary.path().join("displaced");
        fs::create_dir(&journal).unwrap();
        let parent = JournalRoot::open(&journal).unwrap();
        let replaced = std::cell::Cell::new(false);
        let result = acquire_file_lease_bound_checked(
            &parent,
            OsStr::new("brain-refresh.lease"),
            immediate(),
            &|| {
                parent
                    .revalidate_canonical_binding()
                    .map_err(|error| error.to_string())?;
                if !replaced.get() {
                    fs::rename(&journal, &displaced).unwrap();
                    fs::create_dir(&journal).unwrap();
                    replaced.set(true);
                }
                Ok(())
            },
        );
        assert!(replaced.get());
        assert!(matches!(result, Err(LeaseError::Io { source, .. })
            if source.kind() == io::ErrorKind::Interrupted));
        assert!(fs::read_dir(&journal).unwrap().next().is_none());
        assert!(displaced.join("brain-refresh.lease").is_file());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn acquire_release_and_default_mode() {
        let temporary = TempDir::new();
        let path = temporary.path().join("health/refresh.lease");
        let lease = acquire_file_lease(&path, LeaseOptions::default())
            .unwrap()
            .expect("first acquire succeeds");
        assert_eq!(lease.path(), path);
        assert!(path.exists());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(lease);

        assert!(
            acquire_file_lease(&path, LeaseOptions::default())
                .unwrap()
                .is_some()
        );
    }

    #[cfg(feature = "full-tests")]
    #[test]
    fn checked_lease_rechecks_authority_after_acquisition_and_releases_on_refusal() {
        let temporary = TempDir::new();
        let path = temporary.path().join("refresh.lease");
        let result = acquire_file_lease_checked(&path, LeaseOptions::default(), &|| {
            let file = match File::open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.to_string()),
            };
            if probe_file_lease(&file) == LeaseProbe::Active {
                Err("authority revoked after acquisition".to_owned())
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(LeaseError::Io { source, .. })
            if source.kind() == io::ErrorKind::Interrupted));
        let lease = acquire_file_lease_checked(&path, LeaseOptions::default(), &|| Ok(()))
            .unwrap()
            .expect("refused lease was released");
        assert_eq!(
            probe_file_lease(&File::open(&path).unwrap()),
            LeaseProbe::Active
        );
        drop(lease);
        assert_eq!(
            probe_file_lease(&File::open(&path).unwrap()),
            LeaseProbe::Released
        );
    }

    #[test]
    fn configured_mode_is_applied() {
        let temporary = TempDir::new();
        let path = temporary.path().join("refresh.lease");
        let lease = acquire_file_lease(
            &path,
            LeaseOptions {
                mode: 0o640,
                ..LeaseOptions::default()
            },
        )
        .unwrap()
        .expect("acquire succeeds");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        drop(lease);
    }

    #[cfg(unix)]
    #[test]
    fn inherited_descriptor_is_a_live_duplicate() {
        let temporary = TempDir::new();
        let lease = acquire_file_lease(
            temporary.path().join("generation.lock"),
            LeaseOptions::default(),
        )
        .unwrap()
        .expect("lease");
        let descriptor = lease
            .duplicate_for_inheritance()
            .expect("duplicate descriptor");
        assert!(Path::new("/dev/fd").join(descriptor.to_string()).exists());
        nix::unistd::close(descriptor).expect("close duplicate");
    }

    #[cfg(unix)]
    #[test]
    fn exclusive_flock_probe_on_duplicate_does_not_release_owner_lock() {
        use std::fs::OpenOptions;
        use std::os::unix::io::AsRawFd;

        let temporary = TempDir::new();
        let path = temporary.path().join("generation.lock");
        let lease = acquire_file_lease(&path, LeaseOptions::default())
            .unwrap()
            .expect("owner lease");
        let duplicate = lease
            .duplicate_for_inheritance()
            .expect("duplicate descriptor");
        assert!(
            probe_exclusive_flock_no_release(duplicate).expect("probe duplicate"),
            "same-OFD re-flock is an idempotent success"
        );
        assert!(
            acquire_file_lease(
                &path,
                LeaseOptions {
                    attempts: 1,
                    retry_max: Duration::ZERO,
                    ..LeaseOptions::default()
                },
            )
            .unwrap()
            .is_none(),
            "owner FileLease must still hold the lock after the no-release probe"
        );
        let independent = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("independent open");
        assert!(
            !probe_exclusive_flock_no_release(independent.as_raw_fd())
                .expect("probe independent open"),
            "an independent OFD must observe contention while the owner holds the lock"
        );
        assert!(
            acquire_file_lease(
                &path,
                LeaseOptions {
                    attempts: 1,
                    retry_max: Duration::ZERO,
                    ..LeaseOptions::default()
                },
            )
            .unwrap()
            .is_none(),
            "independent-open probe must not unlock the owner either"
        );
        nix::unistd::close(duplicate).expect("close duplicate");
        drop(independent);
        drop(lease);
        assert!(
            acquire_file_lease(&path, LeaseOptions::default())
                .unwrap()
                .is_some(),
            "lock becomes free only after the owner lease drops"
        );
    }
}
