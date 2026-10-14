// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

static OBSERVATION_PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ios_gpu_appraisal_observation_path(path: *const c_char) {
    let path = unsafe { CStr::from_ptr(path) }
        .to_str()
        .expect("observation UTF-8");
    if OBSERVATION_PATH
        .set(std::path::PathBuf::from(path))
        .is_err()
    {
        unsafe { libc::_exit(126) };
    }
}
struct NativeObservation(*mut std::sync::atomic::AtomicU64);
impl NativeObservation {
    fn new(attempt: u64) -> Self {
        use std::os::fd::AsRawFd;
        use std::sync::atomic::{AtomicU64, Ordering};
        let Some(path) = OBSERVATION_PATH.get() else {
            unsafe { libc::_exit(126) }
        };
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap_or_else(|_| unsafe { libc::_exit(126) });
        file.set_len(32)
            .unwrap_or_else(|_| unsafe { libc::_exit(126) });
        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                32,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            unsafe { libc::_exit(126) };
        }
        let ptr = mapped.cast::<AtomicU64>();
        unsafe {
            (*ptr.add(0)).store(attempt, Ordering::Relaxed);
            (*ptr.add(1)).store(u64::from(std::process::id()), Ordering::Relaxed);
            (*ptr.add(2)).store(
                u64::from(SDK_INIT_RESULT.get().copied() == Some(NVAT_RC_OK)),
                Ordering::Relaxed,
            );
            (*ptr.add(3)).store(1, Ordering::Release);
        }
        Self(ptr)
    }
    fn returned(&self) {
        unsafe {
            (*self.0.add(3)).store(2, std::sync::atomic::Ordering::Release);
        }
    }
}
