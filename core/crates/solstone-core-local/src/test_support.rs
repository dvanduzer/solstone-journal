// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! What unit tests need because they share one process with tests that start
//! processes.
//!
//! A child started on any test thread holds a copy of every descriptor this
//! process has open until the child execs. So an executable this process has
//! just written can refuse to run ("Text file busy", os error 26) while a
//! child still holds its write descriptor, and a file lock this process has
//! just released still reads as held while a child holds a copy of the
//! descriptor that took it.

use std::thread;
use std::time::{Duration, Instant};

/// Write an executable without this process ever opening it for writing, so
/// no other test's child can hold a writer when a test runs it. A short-lived
/// `sh` writes the bytes under a staging name and has exited before the file
/// is renamed into place.
#[cfg(all(test, feature = "full-tests"))]
#[cfg(unix)]
pub(crate) fn write_executable(path: &std::path::Path, bytes: &[u8]) {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    let mut staging = path.as_os_str().to_owned();
    staging.push(".staging");
    let mut writer = Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\"", "sh"])
        .arg(&staging)
        .stdin(Stdio::piped())
        .spawn()
        .expect("start the executable writer");
    writer
        .stdin
        .take()
        .expect("executable writer stdin")
        .write_all(bytes)
        .expect("write the executable");
    assert!(
        writer.wait().expect("executable writer").success(),
        "executable writer failed"
    );
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o755)).expect("make executable");
    fs::rename(&staging, path).expect("place the executable");
}

/// A Windows child inherits only the handles it is given, so a plain write is
/// enough there.
#[cfg(all(test, feature = "full-tests"))]
#[cfg(not(unix))]
pub(crate) fn write_executable(path: &std::path::Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write the executable");
}

/// Whether `released` reads true within five seconds, for a check made just
/// after this process released a lock. Once the lock reads free it stays
/// free: it ends with the last copy of its descriptor, and nothing can copy a
/// descriptor this process has closed.
pub(crate) fn lock_released(mut released: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !released() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
    true
}
