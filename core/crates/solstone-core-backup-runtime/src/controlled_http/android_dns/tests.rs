// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use std::sync::{Barrier, Condvar, Mutex, atomic::AtomicUsize};

#[derive(Default)]
struct Latch {
    released: Mutex<bool>,
    wake: Condvar,
}
impl Latch {
    fn wait(&self) {
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.wake.wait(released).unwrap();
        }
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}
struct Release(Arc<Latch>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

fn join(owner: ResolverOwner) {
    drop(owner.sender);
    owner.worker.join().unwrap();
}

fn address() -> Vec<SocketAddr> {
    vec!["127.0.0.1:443".parse().unwrap()]
}

#[test]
fn production_entry_resolves_literal_and_filters_family() {
    let end = Instant::now() + Duration::from_secs(3);
    let result = resolve(
        "127.0.0.1",
        443,
        IpFamily::Ipv4Only,
        end,
        &ArchiveCancellation::new(),
    )
    .unwrap();
    assert_eq!(result, address());
    assert!(
        system_lookup("127.0.0.1", 443, IpFamily::Ipv6Only)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn completed_results_allow_immediate_same_worker_reuse() {
    let owner = ResolverOwner::start(system_lookup).unwrap();
    let id = owner.worker.thread().id();
    for _ in 0..64 {
        assert_eq!(
            owner
                .resolve(
                    "127.0.0.1",
                    443,
                    IpFamily::Ipv4Only,
                    Instant::now() + Duration::from_secs(1),
                    &ArchiveCancellation::new()
                )
                .unwrap(),
            address()
        );
        assert_eq!(owner.worker.thread().id(), id);
    }
    join(owner);
}

#[test]
fn cancelled_lookup_retains_busy_slot_and_recovers_on_same_worker() {
    let latch = Arc::new(Latch::default());
    let _release = Release(Arc::clone(&latch));
    let calls = Arc::new(AtomicUsize::new(0));
    let (entered, receiver) = channel();
    let hold = Arc::clone(&latch);
    let count = Arc::clone(&calls);
    let owner = ResolverOwner::start(move |_, _, _| {
        count.fetch_add(1, Ordering::SeqCst);
        entered.send(std::thread::current().id()).unwrap();
        hold.wait();
        Ok(address())
    })
    .unwrap();
    let worker_id = owner.worker.thread().id();
    let cancellation = ArchiveCancellation::new();
    let end = Instant::now() + Duration::from_secs(5);
    let barrier = Barrier::new(9);
    std::thread::scope(|scope| {
        let original =
            scope.spawn(|| owner.resolve("held.invalid", 443, IpFamily::Any, end, &cancellation));
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            worker_id
        );
        cancellation.cancel();
        let start = Instant::now();
        assert_eq!(
            original.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        let attempts: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    owner.resolve(
                        "replacement.invalid",
                        443,
                        IpFamily::Any,
                        end,
                        &ArchiveCancellation::new(),
                    )
                })
            })
            .collect();
        barrier.wait();
        for attempt in attempts {
            assert_eq!(
                attempt.join().unwrap().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(owner.state.busy.load(Ordering::Acquire));
        latch.release();
    });
    // Observe actual lookup completion, not an elapsed success assumption.
    let deadline = Instant::now() + Duration::from_secs(2);
    while owner.state.busy.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(
        owner
            .resolve(
                "recovered.invalid",
                443,
                IpFamily::Any,
                deadline,
                &ArchiveCancellation::new()
            )
            .unwrap(),
        address()
    );
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
        worker_id
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    join(owner);
}

#[test]
fn deadline_keeps_original_worker_busy_until_lookup_returns() {
    let latch = Arc::new(Latch::default());
    let _release = Release(Arc::clone(&latch));
    let (entered, receiver) = channel();
    let hold = Arc::clone(&latch);
    let owner = ResolverOwner::start(move |_, _, _| {
        entered.send(()).unwrap();
        hold.wait();
        Ok(address())
    })
    .unwrap();
    let end = Instant::now() + Duration::from_millis(100);
    std::thread::scope(|scope| {
        let attempt = scope.spawn(|| {
            owner.resolve(
                "held.invalid",
                443,
                IpFamily::Any,
                end,
                &ArchiveCancellation::new(),
            )
        });
        receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            attempt.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(Instant::now().duration_since(end) < Duration::from_secs(1));
        assert!(owner.state.busy.load(Ordering::Acquire));
        assert_eq!(
            owner
                .resolve(
                    "late.invalid",
                    443,
                    IpFamily::Any,
                    Instant::now() + Duration::from_secs(2),
                    &ArchiveCancellation::new()
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        latch.release();
    });
    join(owner);
}

#[test]
fn expired_or_cancelled_admission_starts_no_lookup_and_empty_result_recovers() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&calls);
    let owner = ResolverOwner::start(move |_, _, _| {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    })
    .unwrap();
    let cancellation = ArchiveCancellation::new();
    cancellation.cancel();
    assert!(
        owner
            .resolve(
                "unused.invalid",
                443,
                IpFamily::Any,
                Instant::now() + Duration::from_secs(1),
                &cancellation
            )
            .is_err()
    );
    assert!(
        owner
            .resolve(
                "unused.invalid",
                443,
                IpFamily::Any,
                Instant::now(),
                &ArchiveCancellation::new()
            )
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        owner
            .resolve(
                "empty.invalid",
                443,
                IpFamily::Any,
                Instant::now() + Duration::from_secs(1),
                &ArchiveCancellation::new()
            )
            .is_err()
    );
    join(owner);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn panic_with_held_tls_never_replaces_failed_worker() {
    struct HeldTls(Arc<Latch>, std::sync::mpsc::Sender<()>);
    impl Drop for HeldTls {
        fn drop(&mut self) {
            self.1.send(()).unwrap();
            self.0.wait();
        }
    }
    thread_local! { static TLS: std::cell::RefCell<Vec<HeldTls>> = std::cell::RefCell::new(Vec::with_capacity(1)); }
    let latch = Arc::new(Latch::default());
    let _release = Release(Arc::clone(&latch));
    let (entered, receiver) = channel();
    let hold = Arc::clone(&latch);
    let owner = ResolverOwner::start(move |_, _, _| {
        TLS.with(|tls| {
            tls.borrow_mut()
                .push(HeldTls(Arc::clone(&hold), entered.clone()))
        });
        panic!("held DNS worker fixture")
    })
    .unwrap();
    assert!(
        owner
            .resolve(
                "panic.invalid",
                443,
                IpFamily::Any,
                Instant::now() + Duration::from_secs(2),
                &ArchiveCancellation::new()
            )
            .is_err()
    );
    receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(owner.state.failed.load(Ordering::Acquire));
    for _ in 0..3 {
        assert!(
            owner
                .resolve(
                    "replacement.invalid",
                    443,
                    IpFamily::Any,
                    Instant::now() + Duration::from_secs(1),
                    &ArchiveCancellation::new()
                )
                .is_err()
        );
    }
    latch.release();
    drop(owner.sender);
    assert!(owner.worker.join().is_err());
}

#[cfg(target_os = "android")]
#[test]
fn actual_worker_creation_refusal_stays_unavailable_in_fresh_process() {
    use nix::sys::resource::{Resource, getrlimit, setrlimit};
    let (_, hard) = getrlimit(Resource::RLIMIT_NPROC).unwrap();
    setrlimit(Resource::RLIMIT_NPROC, 0, hard).unwrap();
    for _ in 0..3 {
        let error = resolve(
            "127.0.0.1",
            443,
            IpFamily::Ipv4Only,
            Instant::now() + Duration::from_secs(1),
            &ArchiveCancellation::new(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "HTTP DNS worker creation failed");
    }
    println!("DNS_REAL_THREAD_CREATION_REFUSED");
}

#[test]
fn cancelled_and_expired_jobs_are_refused_before_worker_lookup() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&calls);
    let owner = ResolverOwner::start(move |_, _, _| {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(address())
    })
    .unwrap();
    for cancelled in [false, true] {
        let cancellation = ArchiveCancellation::new();
        if cancelled {
            cancellation.cancel();
        }
        let (response, receiver) = channel();
        owner.state.busy.store(true, Ordering::Release);
        owner
            .sender
            .send(Job {
                host: "unused.invalid".into(),
                port: 443,
                family: IpFamily::Any,
                end: if cancelled {
                    Instant::now() + Duration::from_secs(1)
                } else {
                    Instant::now()
                },
                cancelled: Arc::new(AtomicBool::new(false)),
                cancellation,
                response,
            })
            .unwrap();
        assert!(
            receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_err()
        );
        // Wait for this actual worker's completion before direct next-job injection.
        let end = Instant::now() + Duration::from_secs(1);
        while owner.state.busy.load(Ordering::Acquire) {
            assert!(Instant::now() < end);
            std::thread::yield_now();
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    join(owner);
}
