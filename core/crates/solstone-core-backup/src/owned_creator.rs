// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! The macOS archive creator entry. macOS has no parent-death signal, so the
//! journal starts `restic backup` through this entry of its own installed core.
//! It runs only the installed restic, in its own process group, and kills that
//! whole group when the journal exits. A `restic backup` left running could
//! otherwise upload a snapshot no ledger ever records.

use std::ffi::OsString;

pub(crate) const ENTRY: &str = "__owned-archive-creator-v1";
pub(crate) const CORE_MEMBER: &str = "bin/solstone-core";

/// Exact early dispatch, before logging or any journal/service/config resolution.
pub fn dispatch(args: &[OsString]) -> Option<u8> {
    if args.first().is_none_or(|arg| arg != ENTRY) {
        return None;
    }
    Some(run_entry(args))
}

fn run_entry(args: &[OsString]) -> u8 {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::{getpgrp, getpid};
    use std::os::unix::process::ExitStatusExt;
    if args.len() < 3 || getpgrp() != getpid() {
        return 91;
    }
    let Ok(member) = solstone_core_installed_payload::resolve_current_member(
        crate::archive_readback::RESTIC_MEMBER,
    ) else {
        return 92;
    };
    if member.path().as_os_str() != args[1] || member.revalidate().is_err() {
        return 92;
    }
    // Held across the spawn, so a stop never runs while restic is joining the group.
    static SPAWNING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn stop_group() -> ! {
        let _spawned = SPAWNING.lock();
        let _ = killpg(getpgrp(), Signal::SIGKILL);
        std::process::exit(90)
    }
    if !crate::parent_watch::armed("owned-creator-parent", stop_group) {
        return 90;
    }
    let child = {
        let _spawning = SPAWNING.lock();
        std::process::Command::new(&args[1])
            .args(&args[2..])
            .spawn()
    };
    match child.and_then(|mut child| child.wait()) {
        Ok(status) => status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .or_else(|| {
                status
                    .signal()
                    .and_then(|signal| u8::try_from(128 + signal).ok())
            })
            .unwrap_or(1),
        Err(_) => 95,
    }
}
