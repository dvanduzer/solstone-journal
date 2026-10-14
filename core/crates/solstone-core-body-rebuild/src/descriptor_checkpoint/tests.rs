// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, OnceLock};

use super::*;
use crate::test_support::reserve_temp_path;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Boundary {
    Fullname,
    Open,
}

#[derive(Default)]
struct Metrics {
    opened: AtomicUsize,
    closed: AtomicUsize,
    shm: AtomicUsize,
    inject_close_error: AtomicBool,
    close_errors: AtomicUsize,
}

type EntryHook = (Boundary, Box<dyn FnOnce()>);

thread_local! {
    static METRICS: RefCell<Option<Arc<Metrics>>> = const { RefCell::new(None) };
    static HOOK: RefCell<Option<EntryHook>> = const { RefCell::new(None) };
}

pub(super) fn entry(boundary: Boundary) -> bool {
    let callback = HOOK.with(|hook| {
        let mut hook = hook.borrow_mut();
        if hook.as_ref().is_some_and(|(wanted, _)| *wanted == boundary) {
            hook.take().map(|(_, callback)| callback)
        } else {
            None
        }
    });
    // A test failure must never unwind through the C ABI.
    callback.is_none_or(|callback| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback)).is_ok()
    })
}

struct Spy {
    original: usize,
    _methods: Box<ffi::sqlite3_io_methods>,
    metrics: Arc<Metrics>,
}

fn spies() -> &'static Mutex<BTreeMap<usize, Spy>> {
    static SPIES: OnceLock<Mutex<BTreeMap<usize, Spy>>> = OnceLock::new();
    SPIES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

// SAFETY: only called after a successful native open. The copied table stays
// boxed in SPIES until actual xClose, which restores the original table first.
pub(super) unsafe fn instrument_io(file: *mut ffi::sqlite3_file) {
    let Some(metrics) = METRICS.with(|slot| slot.borrow().clone()) else {
        return;
    };
    let original = unsafe { (*file).pMethods };
    let mut methods = Box::new(unsafe { *original });
    methods.xClose = Some(spy_close);
    methods.xShmMap = Some(spy_shm_map);
    methods.xShmLock = Some(spy_shm_lock);
    methods.xShmBarrier = Some(spy_shm_barrier);
    methods.xShmUnmap = Some(spy_shm_unmap);
    unsafe { (*file).pMethods = &*methods };
    metrics.opened.fetch_add(1, Ordering::Relaxed);
    spies().lock().unwrap_or_else(|e| e.into_inner()).insert(
        file as usize,
        Spy {
            original: original as usize,
            _methods: methods,
            metrics,
        },
    );
}

unsafe extern "C" fn spy_close(file: *mut ffi::sqlite3_file) -> c_int {
    let spy = spies()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(file as usize));
    let Some(spy) = spy else {
        return ffi::SQLITE_IOERR;
    };
    let original = spy.original as *const ffi::sqlite3_io_methods;
    // SAFETY: original is SQLite's static native table. Restore before invoking
    // native close; keep the copied table alive until that invocation returns.
    unsafe { (*file).pMethods = original };
    let result = match unsafe { (*original).xClose } {
        Some(close) => unsafe { close(file) },
        None => ffi::SQLITE_IOERR,
    };
    spy.metrics.closed.fetch_add(1, Ordering::Relaxed);
    if spy.metrics.inject_close_error.load(Ordering::Relaxed) {
        spy.metrics.close_errors.fetch_add(1, Ordering::Relaxed);
        return ffi::SQLITE_IOERR;
    }
    result
}

fn record_shm(file: *mut ffi::sqlite3_file) {
    if let Some(spy) = spies()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(file as usize))
    {
        spy.metrics.shm.fetch_add(1, Ordering::Relaxed);
    }
}

unsafe extern "C" fn spy_shm_map(
    file: *mut ffi::sqlite3_file,
    _: c_int,
    _: c_int,
    _: c_int,
    _: *mut *mut c_void,
) -> c_int {
    record_shm(file);
    ffi::SQLITE_IOERR
}
unsafe extern "C" fn spy_shm_lock(
    file: *mut ffi::sqlite3_file,
    _: c_int,
    _: c_int,
    _: c_int,
) -> c_int {
    record_shm(file);
    ffi::SQLITE_IOERR
}
unsafe extern "C" fn spy_shm_barrier(file: *mut ffi::sqlite3_file) {
    record_shm(file);
}
unsafe extern "C" fn spy_shm_unmap(file: *mut ffi::sqlite3_file, _: c_int) -> c_int {
    record_shm(file);
    ffi::SQLITE_IOERR
}

fn observed<T>(operation: impl FnOnce() -> T) -> T {
    let metrics = Arc::new(Metrics::default());
    METRICS.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&metrics)));
    let result = operation();
    METRICS.with(|slot| slot.borrow_mut().take());
    assert!(metrics.opened.load(Ordering::Relaxed) > 0);
    assert_eq!(
        metrics.opened.load(Ordering::Relaxed),
        metrics.closed.load(Ordering::Relaxed)
    );
    assert_eq!(
        metrics.shm.load(Ordering::Relaxed),
        0,
        "checkpoint must never enter shared WAL index"
    );
    result
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = reserve_temp_path("solstone-body-descriptor-checkpoint");
        fs::create_dir_all(root.join("imports")).unwrap();
        Self(root)
    }
    fn database(&self) -> PathBuf {
        self.0.join("imports/health-dedupe.sqlite")
    }
    fn seed(&self) {
        Connection::open(self.database())
            .unwrap()
            .execute_batch(
                "CREATE TABLE previous(value TEXT); INSERT INTO previous VALUES ('owner-record');",
            )
            .unwrap();
    }
    fn alias(&self) -> (File, PathBuf) {
        let file = File::open(&self.0).unwrap();
        let alias = PathBuf::from(format!("/proc/self/fd/{}/.", file.as_raw_fd()));
        (file, alias)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn repeated_descriptor_rebuild_and_ordinary_existing_checkpoint_succeed() {
    let fixture = Fixture::new();
    let codec: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../fixtures/body_source_codec_rows.json"
    ))
    .unwrap();
    let mut row = codec["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "legacy_normalized_v1")
        .unwrap()["row"]
        .clone();
    row["import_id"] = "synthetic-import".into();
    row["normalized_ref"] = "imports/synthetic-import/normalized/2026-01.jsonl#L1".into();
    let normalized = fixture.0.join("imports/synthetic-import/normalized");
    fs::create_dir_all(&normalized).unwrap();
    fs::write(normalized.join("2026-01.jsonl"), format!("{row}\n")).unwrap();
    let (_root, alias) = fixture.alias();
    let first = crate::rebuild_body_store(&alias).unwrap();
    assert_eq!(first.rows(), 1);
    assert_eq!(
        observed(|| crate::rebuild_body_store(&alias)).unwrap(),
        first
    );
    observed(|| checkpoint(&fixture.database())).unwrap();
    assert_eq!(
        Connection::open(fixture.database())
            .unwrap()
            .query_row("SELECT count(*) FROM health_dedupe", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn database_and_each_sidecar_symlink_refuse_without_outside_mutation() {
    for leaf in LEAVES {
        let fixture = Fixture::new();
        fixture.seed();
        let outside = fixture.0.join("outside.sqlite");
        Connection::open(&outside)
            .unwrap()
            .execute_batch(
                "CREATE TABLE outside(value TEXT); INSERT INTO outside VALUES ('preserved');",
            )
            .unwrap();
        let before = fs::read(&outside).unwrap();
        let identity = fs::metadata(&outside).unwrap().ino();
        let target = fixture.0.join("imports").join(leaf.to_str().unwrap());
        if target.exists() {
            fs::remove_file(&target).unwrap();
        }
        symlink(&outside, target).unwrap();
        assert!(checkpoint(&fixture.database()).is_err());
        assert_eq!(fs::read(&outside).unwrap(), before);
        assert_eq!(fs::metadata(&outside).unwrap().ino(), identity);
    }
}

#[test]
fn imports_symlink_refuses() {
    let fixture = Fixture::new();
    fixture.seed();
    fs::rename(fixture.0.join("imports"), fixture.0.join("outside-imports")).unwrap();
    let before = fs::read(fixture.0.join("outside-imports/health-dedupe.sqlite")).unwrap();
    symlink(fixture.0.join("outside-imports"), fixture.0.join("imports")).unwrap();
    assert!(checkpoint(&fixture.database()).is_err());
    assert_eq!(
        fs::read(fixture.0.join("outside-imports/health-dedupe.sqlite")).unwrap(),
        before
    );
}

#[test]
fn actual_fullname_and_open_refuse_substituted_symlink_with_reusable_descriptor() {
    for boundary in [Boundary::Fullname, Boundary::Open] {
        let fixture = Fixture::new();
        fixture.seed();
        let outside = fixture.0.join("outside.sqlite");
        let reader = Connection::open(&outside).unwrap();
        reader.execute_batch("CREATE TABLE outside(value TEXT); INSERT INTO outside VALUES ('preserved'); BEGIN; SELECT * FROM outside;").unwrap();
        let second = Connection::open(&outside).unwrap();
        // Unix keeps this descriptor pending while reader's shared lock lives.
        drop(second);
        let inode = fs::metadata(&outside).unwrap().ino();
        let retained_fds = fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                fs::metadata(entry.path()).is_ok_and(|metadata| metadata.ino() == inode)
            })
            .count();
        assert!(retained_fds >= 2, "positive deferred-descriptor witness");
        let before = fs::read(&outside).unwrap();
        let registration = Registration::admit(&fixture.database()).unwrap();
        let database = fixture.database();
        let outside_clone = outside.clone();
        HOOK.with(|hook| {
            *hook.borrow_mut() = Some((
                boundary,
                Box::new(move || {
                    fs::remove_file(&database).unwrap();
                    symlink(&outside_clone, &database).unwrap();
                }),
            ))
        });
        assert!(registration.run().is_err());
        assert!(
            HOOK.with(|hook| hook.borrow().is_none()),
            "actual entry hook ran"
        );
        // A caught fixture panic must not pass as the intended VFS refusal.
        assert!(
            fs::symlink_metadata(fixture.database())
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(fixture.database()).unwrap(), outside);
        drop(registration);
        assert_eq!(fs::read(&outside).unwrap(), before);
        assert_eq!(fs::metadata(&outside).unwrap().ino(), inode);
        reader.execute_batch("ROLLBACK").unwrap();
    }
}

#[test]
fn acquired_imports_rename_checkpoint_never_uses_replacement() {
    let fixture = Fixture::new();
    fixture.seed();
    let registration = Registration::admit(&fixture.database()).unwrap();
    fs::rename(
        fixture.0.join("imports"),
        fixture.0.join("original-imports"),
    )
    .unwrap();
    fs::create_dir(fixture.0.join("imports")).unwrap();
    fs::write(fixture.database(), b"replacement-must-not-be-opened").unwrap();
    let before = fs::read(fixture.database()).unwrap();
    let identity = fs::metadata(fixture.database()).unwrap().ino();
    observed(|| registration.run()).unwrap();
    assert_eq!(fs::read(fixture.database()).unwrap(), before);
    assert_eq!(fs::metadata(fixture.database()).unwrap().ino(), identity);
}

#[test]
fn sidecars_substituted_after_preflight_refuse_at_open() {
    for leaf in [LEAVES[1], LEAVES[2]] {
        let fixture = Fixture::new();
        fixture.seed();
        let registration = Registration::admit(&fixture.database()).unwrap();
        let outside = fixture.0.join("outside");
        fs::write(&outside, b"preserved outside bytes").unwrap();
        let identity = fs::metadata(&outside).unwrap().ino();
        symlink(
            &outside,
            fixture.0.join("imports").join(leaf.to_str().unwrap()),
        )
        .unwrap();
        let suffix = if leaf == LEAVES[1] {
            "-wal"
        } else {
            "-journal"
        };
        let filename = CString::new(format!(
            "{}{suffix}",
            registration.database.to_str().unwrap()
        ))
        .unwrap();
        let mut file = ffi::sqlite3_file {
            pMethods: ptr::null(),
        };
        // The wrong-kind check must return before native xOpen touches its
        // larger storage or interprets SQLite's filename metadata allocation.
        let result = unsafe {
            bound_open(
                registration.vfs.get(),
                filename.as_ptr(),
                &mut file,
                ffi::SQLITE_OPEN_READWRITE,
                ptr::null_mut(),
            )
        };
        assert_eq!(result, ffi::SQLITE_CANTOPEN);
        assert!(file.pMethods.is_null());
        assert_eq!(fs::read(&outside).unwrap(), b"preserved outside bytes");
        assert_eq!(fs::metadata(&outside).unwrap().ino(), identity);
    }
}

#[test]
fn descriptor_wal_only_commits_survive_confirmed_later_installer_failure() {
    let fixture = Fixture::new();
    let source_path = fixture.0.join("source.sqlite");
    let source = Connection::open(&source_path).unwrap();
    source.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE previous(value TEXT); INSERT INTO previous VALUES ('first'), ('second');").unwrap();
    fs::copy(&source_path, fixture.database()).unwrap();
    fs::copy(
        format!("{}-wal", source_path.display()),
        format!("{}-wal", fixture.database().display()),
    )
    .unwrap();
    let main_only = fixture.0.join("main-only.sqlite");
    fs::copy(&source_path, &main_only).unwrap();
    assert!(
        Connection::open(main_only)
            .unwrap()
            .query_row("SELECT count(*) FROM previous", [], |row| row
                .get::<_, i64>(0))
            .is_err()
    );
    drop(source);
    let (_root, alias) = fixture.alias();
    let installed = std::cell::Cell::new(0);
    let result = observed(|| {
        crate::publish_database_with_installer(
            &alias,
            &solstone_core_body_store::BodyDedupeState::new(),
            |_, _| {
                installed.set(installed.get() + 1);
                Err(error(
                    BodyRebuildErrorKind::Publication,
                    "injected_descriptor_install_failure",
                ))
            },
        )
    })
    .unwrap_err();
    assert_eq!(installed.get(), 1);
    assert_eq!(result.stage(), "injected_descriptor_install_failure");
    let reader = Connection::open(fixture.database()).unwrap();
    let values: Vec<String> = reader
        .prepare("SELECT value FROM previous ORDER BY value")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(values, ["first", "second"]);
}

#[test]
fn busy_wal_reader_refuses_without_shm_then_release_retries() {
    let fixture = Fixture::new();
    let reader = Connection::open(fixture.database()).unwrap();
    reader.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE previous(value TEXT); INSERT INTO previous VALUES ('committed'); BEGIN; SELECT * FROM previous;").unwrap();
    let wal = PathBuf::from(format!("{}-wal", fixture.database().display()));
    let before = fs::read(&wal).unwrap();
    observed(|| checkpoint(&fixture.database())).unwrap_err();
    assert_eq!(fs::read(&wal).unwrap(), before);
    reader.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    observed(|| checkpoint(&fixture.database())).unwrap();
    assert_eq!(
        Connection::open(fixture.database())
            .unwrap()
            .query_row("SELECT value FROM previous", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "committed"
    );
}

#[test]
fn corrupt_database_closes_and_unregisters_without_sidecar_cleanup() {
    let fixture = Fixture::new();
    fs::write(fixture.database(), b"not a database").unwrap();
    let registration = Registration::admit(&fixture.database()).unwrap();
    let name = registration.name.clone();
    observed(|| registration.run()).unwrap_err();
    drop(registration);
    assert!(unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) }.is_null());
    assert_eq!(fs::read(fixture.database()).unwrap(), b"not a database");
}

#[test]
fn sqlite_masks_native_close_error_after_actual_close_and_registration_releases() {
    let fixture = Fixture::new();
    fixture.seed();
    let registration = Registration::admit(&fixture.database()).unwrap();
    let name = registration.name.clone();
    let metrics = Arc::new(Metrics::default());
    metrics.inject_close_error.store(true, Ordering::Relaxed);
    METRICS.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&metrics)));
    // The injected error follows the actual native close. SQLite's pager close
    // masks xClose errors; it does not become a Connection::close refusal.
    registration.run().unwrap();
    METRICS.with(|slot| slot.borrow_mut().take());
    assert!(metrics.opened.load(Ordering::Relaxed) > 0);
    assert_eq!(
        metrics.opened.load(Ordering::Relaxed),
        metrics.closed.load(Ordering::Relaxed)
    );
    assert_eq!(
        metrics.closed.load(Ordering::Relaxed),
        metrics.close_errors.load(Ordering::Relaxed)
    );
    assert_eq!(metrics.shm.load(Ordering::Relaxed), 0);
    drop(registration);
    assert!(unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) }.is_null());
    assert_eq!(
        Connection::open(fixture.database())
            .unwrap()
            .query_row("SELECT value FROM previous", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "owner-record"
    );
}

#[test]
fn independent_concurrent_registrations_checkpoint_and_unregister() {
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let threads: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    let fixture = Fixture::new();
                    fixture.seed();
                    let registration = Registration::admit(&fixture.database()).unwrap();
                    let name = registration.name.clone();
                    barrier.wait();
                    observed(|| registration.run()).unwrap();
                    drop(registration);
                    assert!(unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) }.is_null());
                    name
                })
            })
            .collect();
        let names: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_ne!(names[0], names[1]);
    });
}

#[test]
fn same_process_reader_descriptor_reuse_runs_in_isolated_process() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "descriptor_checkpoint::tests::isolated_reader_descriptor_reuse",
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("descriptor-reuse-witness-passed"));
}

#[test]
#[ignore = "invoked alone by the descriptor-reuse test to avoid unrelated FD allocation races"]
fn isolated_reader_descriptor_reuse() {
    use std::os::fd::{FromRawFd, OwnedFd};
    let fixture = Fixture::new();
    let reader = Connection::open(fixture.database()).unwrap();
    reader.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE previous(value TEXT); INSERT INTO previous VALUES ('committed'); BEGIN; SELECT * FROM previous;").unwrap();
    let registration = Registration::admit(&fixture.database()).unwrap();
    let descriptor = registration.directory.as_fd().as_raw_fd();
    let name = registration.name.clone();
    observed(|| registration.run()).unwrap_err();
    drop(registration);
    assert!(unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) }.is_null());
    let unrelated = Fixture::new();
    fs::write(
        unrelated.0.join("health-dedupe.sqlite-shm"),
        b"unrelated retained witness",
    )
    .unwrap();
    let parent = File::open(&unrelated.0).unwrap();
    // F_DUPFD never overwrites an occupied descriptor. Isolation makes exact
    // lowest-free-number reuse deterministic without clobbering another test.
    let _reuse_guard = if parent.as_raw_fd() == descriptor {
        None // The unrelated parent open already reused the exact number.
    } else {
        let reused =
            nix::fcntl::fcntl(&parent, nix::fcntl::FcntlArg::F_DUPFD_CLOEXEC(descriptor)).unwrap();
        assert_eq!(reused, descriptor);
        // SAFETY: fcntl returned a new owned descriptor, adopted exactly once.
        Some(unsafe { OwnedFd::from_raw_fd(reused) })
    };
    assert_eq!(
        fs::metadata(format!("/proc/self/fd/{descriptor}"))
            .unwrap()
            .ino(),
        fs::metadata(&unrelated.0).unwrap().ino()
    );
    reader.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    assert_eq!(
        fs::read(unrelated.0.join("health-dedupe.sqlite-shm")).unwrap(),
        b"unrelated retained witness"
    );
    observed(|| checkpoint(&fixture.database())).unwrap();
    println!("descriptor-reuse-witness-passed");
}

struct ReaderChild(std::process::Child);
impl Drop for ReaderChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn separate_process_wal_reader_refuses_then_release_retries() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    let fixture = Fixture::new();
    let seed = Connection::open(fixture.database()).unwrap();
    seed.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE previous(value TEXT); INSERT INTO previous VALUES ('committed');").unwrap();
    drop(seed);
    let mut child = ReaderChild(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "descriptor_checkpoint::tests::isolated_remote_reader",
                "--ignored",
                "--test-threads=1",
                "--nocapture",
            ])
            .env("SOLSTONE_BODY_REBUILD_TEST_DATABASE", fixture.database())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut stdout = BufReader::new(child.0.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert!(
            stdout.read_line(&mut line).unwrap() > 0,
            "reader exited before ready"
        );
        if line.contains("remote-reader-ready") {
            break;
        }
    }
    let database_before = fs::read(fixture.database()).unwrap();
    let wal = PathBuf::from(format!("{}-wal", fixture.database().display()));
    let wal_before = fs::read(&wal).unwrap();
    observed(|| checkpoint(&fixture.database())).unwrap_err();
    assert_eq!(fs::read(fixture.database()).unwrap(), database_before);
    assert_eq!(fs::read(&wal).unwrap(), wal_before);
    child.0.stdin.take().unwrap().write_all(b"x").unwrap();
    assert!(child.0.wait().unwrap().success());
    observed(|| checkpoint(&fixture.database())).unwrap();
    assert_eq!(
        Connection::open(fixture.database())
            .unwrap()
            .query_row("SELECT value FROM previous", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "committed"
    );
}

#[test]
#[ignore = "controlled reader child of separate_process_wal_reader_refuses_then_release_retries"]
fn isolated_remote_reader() {
    use std::io::{Read, Write};
    let database = std::env::var_os("SOLSTONE_BODY_REBUILD_TEST_DATABASE").unwrap();
    let reader = Connection::open(database).unwrap();
    reader
        .execute_batch("BEGIN; SELECT * FROM previous;")
        .unwrap();
    println!("remote-reader-ready");
    std::io::stdout().flush().unwrap();
    std::io::stdin().read_exact(&mut [0_u8]).unwrap();
    reader.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn failed_open_unregisters_and_later_retry_is_available() {
    let fixture = Fixture::new();
    fixture.seed();
    let registration = Registration::admit(&fixture.database()).unwrap();
    let name = registration.name.clone();
    let database = fixture.database();
    HOOK.with(|hook| {
        *hook.borrow_mut() = Some((
            Boundary::Open,
            Box::new(move || {
                fs::remove_file(database).unwrap();
            }),
        ))
    });
    assert!(registration.run().is_err());
    assert!(HOOK.with(|hook| hook.borrow().is_none()));
    assert_eq!(
        fs::symlink_metadata(fixture.database()).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    drop(registration);
    assert!(unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) }.is_null());
    fixture.seed();
    observed(|| checkpoint(&fixture.database())).unwrap();
}
