// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! macOS has no parent-death signal. An installed entry watches its parent
//! with kqueue instead, and a parent that already left (reparented to
//! launchd) is refused.

use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};

/// Arm a watch that runs `on_exit` on its own thread once the parent exits.
pub(crate) fn armed(name: &str, on_exit: fn() -> !) -> bool {
    let parent = nix::unistd::getppid();
    if parent.as_raw() <= 1 {
        return false;
    }
    let Ok(queue) = Kqueue::new() else {
        return false;
    };
    let registration = KEvent::new(
        parent.as_raw() as usize,
        EventFilter::EVFILT_PROC,
        EvFlags::EV_ADD | EvFlags::EV_ENABLE | EvFlags::EV_CLEAR | EvFlags::EV_RECEIPT,
        FilterFlag::NOTE_EXIT,
        0,
        0,
    );
    let mut receipt = [KEvent::new(
        0,
        EventFilter::EVFILT_PROC,
        EvFlags::empty(),
        FilterFlag::empty(),
        0,
        0,
    )];
    match queue.kevent(&[registration], &mut receipt, None) {
        Ok(1) if receipt[0].data() == 0 => {}
        _ => return false,
    }
    // Registered first, then rechecked: an exit in between is either seen here
    // or delivered to the watch.
    if nix::unistd::getppid() != parent {
        return false;
    }
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let mut event = receipt;
            // An interrupted wait waits again; any other failure ends as an exit would.
            while let Err(nix::errno::Errno::EINTR) = queue.kevent(&[], &mut event, None) {}
            on_exit();
        })
        .is_ok()
}
