// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! One permanent resolver; a late lookup owns no journal or request authority.

use solstone_core_backup::archive_admission::ArchiveCancellation;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, channel, sync_channel},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use ureq::config::IpFamily;

const MAX_ADDRESSES: usize = 16;
const OBSERVE_INTERVAL: Duration = Duration::from_millis(5);

#[cfg(all(target_os = "android", feature = "test-support"))]
type Lookup = fn(&str, u16, IpFamily) -> io::Result<Vec<SocketAddr>>;
#[cfg(all(target_os = "android", feature = "test-support"))]
static STARTUP_LOOKUP: OnceLock<Lookup> = OnceLock::new();
static OWNER: OnceLock<io::Result<ResolverOwner>> = OnceLock::new();

/// Fresh-process fixtures select the lookup before its first production use.
/// This cannot reset the permanent owner or replace an active/failed worker.
#[cfg(all(target_os = "android", feature = "test-support"))]
pub(crate) fn fixture_lookup_before_start(lookup: Lookup) -> Result<(), &'static str> {
    STARTUP_LOOKUP
        .set(lookup)
        .map_err(|_| "Android DNS lookup already selected")
}

/// Read-only observation of actual lookup return; never resets admission.
#[cfg(all(target_os = "android", feature = "test-support"))]
pub(crate) fn fixture_idle() -> bool {
    matches!(OWNER.get(), Some(Ok(owner)) if !owner.state.busy.load(Ordering::Acquire))
}

#[derive(Default)]
struct State {
    busy: AtomicBool,
    failed: AtomicBool,
}

struct Job {
    host: String,
    port: u16,
    family: IpFamily,
    end: Instant,
    cancelled: Arc<AtomicBool>,
    // ArchiveCancellation contains only shared atomic refusal flags, no owner.
    cancellation: ArchiveCancellation,
    response: std::sync::mpsc::Sender<io::Result<Vec<SocketAddr>>>,
}

struct ResolverOwner {
    state: Arc<State>,
    sender: SyncSender<Job>,
    // Process lifetime: neither a request nor an observer joins/drops/replaces it.
    worker: JoinHandle<()>,
}

struct WorkerExit(Arc<State>);
impl Drop for WorkerExit {
    fn drop(&mut self) {
        // This flag also covers panic before TLS destruction. No replacement is
        // permitted, so reporting failure never needs to wait for native join.
        self.0.failed.store(true, Ordering::Release);
    }
}

struct RequestEnded(Arc<AtomicBool>);
impl Drop for RequestEnded {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn check(end: Instant, cancelled: bool) -> io::Result<()> {
    if cancelled {
        Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "HTTP DNS cancelled",
        ))
    } else if Instant::now() >= end {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "HTTP DNS deadline reached",
        ))
    } else {
        Ok(())
    }
}

fn accepts(family: IpFamily, address: &SocketAddr) -> bool {
    match family {
        IpFamily::Any => true,
        IpFamily::Ipv4Only => address.is_ipv4(),
        IpFamily::Ipv6Only => address.is_ipv6(),
    }
}

fn system_lookup(host: &str, port: u16, family: IpFamily) -> io::Result<Vec<SocketAddr>> {
    // Preserve ureq's authority representation, including bracketed IPv6.
    let addresses = format!("{host}:{port}").to_socket_addrs()?;
    Ok(addresses
        .filter(|address| accepts(family, address))
        .take(MAX_ADDRESSES)
        .collect())
}

impl ResolverOwner {
    fn start(
        lookup: impl Fn(&str, u16, IpFamily) -> io::Result<Vec<SocketAddr>> + Send + 'static,
    ) -> io::Result<Self> {
        let state = Arc::new(State::default());
        let (sender, receiver): (SyncSender<Job>, Receiver<Job>) = sync_channel(1);
        let worker_state = Arc::clone(&state);
        let worker = std::thread::Builder::new()
            .name("android-backup-dns".into())
            .spawn(move || {
                let _exit = WorkerExit(Arc::clone(&worker_state));
                while let Ok(job) = receiver.recv() {
                    let result = check(
                        job.end,
                        job.cancelled.load(Ordering::Acquire) || job.cancellation.is_cancelled(),
                    )
                    .and_then(|()| lookup(&job.host, job.port, job.family));
                    // Only this worker releases admission, after lookup return.
                    // A caller's timeout/drop cannot admit an active+queued pair.
                    let response = job.response;
                    drop((job.host, job.cancelled, job.cancellation));
                    worker_state.busy.store(false, Ordering::Release);
                    let _ = response.send(result);
                }
            })?;
        Ok(Self {
            state,
            sender,
            worker,
        })
    }

    fn resolve(
        &self,
        host: &str,
        port: u16,
        family: IpFamily,
        end: Instant,
        cancellation: &ArchiveCancellation,
    ) -> io::Result<Vec<SocketAddr>> {
        check(end, cancellation.is_cancelled())?;
        if host.is_empty() || host.len() > 255 || host.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid HTTP DNS host",
            ));
        }
        if self.state.failed.load(Ordering::Acquire) || self.worker.is_finished() {
            return Err(io::Error::other("HTTP DNS worker unavailable"));
        }
        if self
            .state
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "HTTP DNS worker busy",
            ));
        }
        if let Err(error) = check(end, cancellation.is_cancelled()) {
            // No job or worker owns this unused admission yet.
            self.state.busy.store(false, Ordering::Release);
            return Err(error);
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let _ended = RequestEnded(Arc::clone(&cancelled));
        let (response, receiver) = channel();
        let job = Job {
            host: host.to_owned(),
            port,
            family,
            end,
            cancelled,
            cancellation: cancellation.clone(),
            response,
        };
        if let Err(error) = self.sender.try_send(job) {
            // A full queue after exclusive admission is a broken owner; a
            // disconnected worker cannot be replaced in this process either.
            self.state.failed.store(true, Ordering::Release);
            self.state.busy.store(false, Ordering::Release);
            return Err(io::Error::other(match error {
                TrySendError::Full(_) => "HTTP DNS admission failed",
                TrySendError::Disconnected(_) => "HTTP DNS worker unavailable",
            }));
        }
        loop {
            check(end, cancellation.is_cancelled())?;
            match receiver
                .recv_timeout(OBSERVE_INTERVAL.min(end.saturating_duration_since(Instant::now())))
            {
                Ok(result) => {
                    check(end, cancellation.is_cancelled())?;
                    let addresses = result?;
                    if addresses.is_empty() {
                        return Err(io::Error::other("HTTP DNS returned no addresses"));
                    }
                    return Ok(addresses);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other("HTTP DNS worker ended"));
                }
            }
        }
    }
}

pub(super) fn resolve(
    host: &str,
    port: u16,
    family: IpFamily,
    end: Instant,
    cancellation: &ArchiveCancellation,
) -> io::Result<Vec<SocketAddr>> {
    check(end, cancellation.is_cancelled())?;
    match OWNER.get_or_init(|| {
        #[cfg(all(target_os = "android", feature = "test-support"))]
        let lookup = *STARTUP_LOOKUP.get_or_init(|| system_lookup);
        #[cfg(not(all(target_os = "android", feature = "test-support")))]
        let lookup = system_lookup;
        ResolverOwner::start(lookup)
    }) {
        Ok(owner) => owner.resolve(host, port, family, end, cancellation),
        Err(_) => Err(io::Error::other("HTTP DNS worker creation failed")),
    }
}

#[cfg(all(test, feature = "full-tests"))]
mod tests;
