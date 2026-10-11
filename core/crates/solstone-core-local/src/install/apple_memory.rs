// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Shared Apple Silicon total memory threshold and host probe.

pub const APPLE_SILICON_TOTAL_MEMORY_MIN_BYTES: u64 = 16 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppleTotalMemoryVerdict {
    Eligible,
    BelowBar,
    Unreadable,
}

pub fn apple_total_memory_verdict(total_bytes: Option<u64>) -> AppleTotalMemoryVerdict {
    match total_bytes {
        None => AppleTotalMemoryVerdict::Unreadable,
        Some(bytes) if bytes < APPLE_SILICON_TOTAL_MEMORY_MIN_BYTES => {
            AppleTotalMemoryVerdict::BelowBar
        }
        Some(_) => AppleTotalMemoryVerdict::Eligible,
    }
}

pub fn apple_physical_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        let output = match Command::new("/usr/sbin/sysctl")
            .args(["-n", "hw.memsize"])
            .output()
        {
            Ok(output) => output,
            Err(error) => {
                log::warn!("could not run sysctl for hw.memsize: {error}");
                return None;
            }
        };
        if !output.status.success() {
            log::warn!("sysctl hw.memsize exited with status {}", output.status);
            return None;
        }
        match std::str::from_utf8(&output.stdout) {
            Ok(text) => match text.trim().parse::<u64>() {
                Ok(bytes) => Some(bytes),
                Err(error) => {
                    log::warn!("could not parse sysctl hw.memsize output: {error}");
                    None
                }
            },
            Err(error) => {
                log::warn!("sysctl hw.memsize output was not utf-8: {error}");
                None
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apple_total_memory_verdict() {
        assert_eq!(
            apple_total_memory_verdict(None),
            AppleTotalMemoryVerdict::Unreadable
        );
        assert_eq!(
            apple_total_memory_verdict(Some(APPLE_SILICON_TOTAL_MEMORY_MIN_BYTES - 1)),
            AppleTotalMemoryVerdict::BelowBar
        );
        assert_eq!(
            apple_total_memory_verdict(Some(APPLE_SILICON_TOTAL_MEMORY_MIN_BYTES)),
            AppleTotalMemoryVerdict::Eligible
        );
        assert_eq!(
            apple_total_memory_verdict(Some(APPLE_SILICON_TOTAL_MEMORY_MIN_BYTES + 1024)),
            AppleTotalMemoryVerdict::Eligible
        );
    }
}
