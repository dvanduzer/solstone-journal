// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use serde_json::{Value, json};
use solstone_core_indexer_query::{OwnerBoundary, SearchRequest};
use solstone_core_journal_io as jio;
use std::ffi::{CStr, CString, OsStr, c_char};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static CANCEL: AtomicBool = AtomicBool::new(false);
static SERVING: std::sync::Mutex<Option<journal_mobile_serving::MobileServing>> =
    std::sync::Mutex::new(None);

fn serving(root: &Path, command: &str) -> Result<Value, String> {
    let command: Value = serde_json::from_str(command).map_err(|e| e.to_string())?;
    let mut server = SERVING.lock().map_err(|_| "serving lock poisoned")?;
    match command["op"].as_str().ok_or("serving op required")? {
        "start" => {
            if server.is_some() {
                return Err("server already started".into());
            }
            let address = command["address"]
                .as_str()
                .ok_or("interface address required")?
                .parse()
                .map_err(|e: std::net::AddrParseError| e.to_string())?;
            // Android loads the packaged runtime from its read-only native library directory.
            let library = command["onnx_library"]
                .as_str()
                .ok_or("ONNX runtime library required")?;
            ort::init_from(library).map_err(|e| e.to_string())?.commit();
            *server = Some(journal_mobile_serving::MobileServing::start_with_payload(
                root.to_path_buf(),
                address,
                root.parent()
                    .ok_or("app directory missing")?
                    .join("journal-payload"),
            )?);
            Ok(server.as_ref().unwrap().status())
        }
        "stop" => {
            *server = None;
            Ok(json!({"stopped":true}))
        }
        "status" => Ok(server.as_ref().ok_or("server stopped")?.status()),
        "index" => server.as_ref().ok_or("server stopped")?.index(),
        "audio-enable" => {
            let files = root.parent().ok_or("app files missing")?;
            server.as_mut().ok_or("server stopped")?.enable_audio(
                files.join("audio-models"),
                files.join("audio-models/tdt-0.6b-v3-q8_0.gguf"),
                files.join("audio-scratch"),
            )?;
            Ok(server.as_ref().unwrap().status())
        }
        "audio-cancel" => {
            server.as_ref().ok_or("server stopped")?.cancel_audio()?;
            Ok(json!({"cancel_requested":true}))
        }
        "audio-resume" => {
            server.as_ref().ok_or("server stopped")?.resume_audio()?;
            Ok(json!({"resumed":true}))
        }
        "owner" => {
            let response = server.as_ref().ok_or("server stopped")?.request_owner(
                command["method"].as_str().ok_or("method required")?,
                command["uri"].as_str().ok_or("uri required")?,
                command["body"].as_str().unwrap_or(""),
            )?;
            Ok(json!({"status":response.status,"body":response.body}))
        }
        _ => Err("unknown serving operation".into()),
    }
}

fn seed_mixed(root: &Path, turn: usize) -> Result<(), Box<dyn std::error::Error>> {
    // Synthetic fixture generation belongs only to this development harness.
    // Production activity writes continue through the facets authority.
    let directory = root.join("facets/work/activities");
    fs::create_dir_all(&directory)?;
    let authority = jio::JournalRoot::open(&directory)?;
    let days: Vec<usize> = if turn == 0 {
        (1..=16).collect()
    } else {
        vec![turn % 16 + 1]
    };
    for day in days {
        let mut bytes = Vec::new();
        for row in 0..256 {
            let record = json!({"id":format!("mixed-{day}-{row}"),"title":format!("Synthetic day {day} row {row}"),
                "story":{"body":format!("androidsentinel day{day} uniqueentry{row} revision{turn}")}});
            writeln!(&mut bytes, "{record}")?;
        }
        let name = format!("202609{day:02}.jsonl");
        let result = jio::atomic_replace_bound(&authority, OsStr::new(&name), &bytes, 0o600)?;
        if !matches!(result, jio::BoundAtomicOutcome::Published { .. }) {
            return Err(format!("fixture publication unconfirmed: {result:?}").into());
        }
    }
    Ok(())
}

fn publication_race(root: &Path) -> Result<Value, Box<dyn std::error::Error>> {
    use std::sync::{Arc, Barrier};
    let dir = root.join(format!("publication-race-{}", std::process::id()));
    fs::create_dir(&dir)?;
    let barrier = Arc::new(Barrier::new(17));
    let finished = Arc::new(AtomicBool::new(false));
    let observer_dir = dir.clone();
    let observer_barrier = barrier.clone();
    let observer_finished = finished.clone();
    let observer = std::thread::spawn(move || -> Result<usize, String> {
        observer_barrier.wait();
        let start = Instant::now();
        let mut observations = 0;
        while start.elapsed().as_secs() < 5 {
            match fs::read(observer_dir.join("winner")) {
                Ok(bytes) => {
                    if bytes.len() != 256 * 1024
                        || bytes[0] >= 16
                        || !bytes.iter().all(|v| *v == bytes[0])
                    {
                        return Err("reader observed partial publication".into());
                    }
                    observations += 1;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
            if observer_finished.load(Ordering::Acquire) && observations > 0 {
                return Ok(observations);
            }
            std::thread::yield_now();
        }
        Err("publication observer timed out".into())
    });
    let mut threads = Vec::new();
    for i in 0..16_u8 {
        let dir = dir.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            let bytes = vec![i; 256 * 1024];
            barrier.wait();
            let authority = jio::JournalRoot::open(&dir).map_err(|e| e.to_string())?;
            match jio::write_bytes_exclusive_bound_detailed(
                &authority,
                OsStr::new("winner"),
                &bytes,
                0o600,
            ) {
                Ok(result) if result.is_fully_confirmed() => Ok(true),
                Ok(result) => Err(format!("publication unconfirmed: {result:?}")),
                Err(error) if error.source.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                Err(error) => Err(error.to_string()),
            }
        }));
    }
    let results: Vec<_> = threads.into_iter().map(|thread| thread.join()).collect();
    finished.store(true, Ordering::Release);
    let observations = observer.join().map_err(|_| "observer panicked")??;
    let mut winners = 0;
    for result in results {
        winners += usize::from(result.map_err(|_| "publication thread panicked")??);
    }
    let bytes = fs::read(dir.join("winner"))?;
    if winners != 1 || bytes.len() != 256 * 1024 || !bytes.iter().all(|v| *v == bytes[0]) {
        return Err("concurrent publication violated single-complete-winner".into());
    }
    Ok(
        json!({"contenders":16,"winners":winners,"bytes":bytes.len(),"winner_byte":bytes[0],"complete_reader_observations":observations}),
    )
}

fn publication_faults(root: &Path) -> Result<Value, Box<dyn std::error::Error>> {
    let authority = jio::JournalRoot::open(root)?;
    let mut checked = Vec::new();
    for (i, primitive) in [
        jio::BoundPublicationPrimitive::FileSync,
        jio::BoundPublicationPrimitive::ExclusivePublish,
        jio::BoundPublicationPrimitive::ParentSync,
        jio::BoundPublicationPrimitive::CompletionObserve,
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("fault-{}-{i}", std::process::id());
        let (result, consumed) =
            jio::run_with_bound_publication_fault(primitive, 1, libc::EIO, || {
                jio::write_bytes_exclusive_bound_detailed(
                    &authority,
                    OsStr::new(&name),
                    b"complete fault fixture",
                    0o600,
                )
            });
        if !consumed {
            return Err("publication fault not consumed".into());
        }
        if i < 2 {
            if result.is_ok() || root.join(&name).exists() {
                return Err("pre-publication failure exposed destination".into());
            }
        } else {
            let outcome = result?;
            if outcome.is_fully_confirmed()
                || !matches!(outcome.cleanup, jio::StageCleanup::Removed)
                || fs::read(root.join(&name))? != b"complete fault fixture"
            {
                return Err("publication failure incorrectly confirmed".into());
            }
            if i == 2 && !matches!(outcome.durability, jio::MetadataDurability::Unproven { .. }) {
                return Err("directory sync failure lost uncertainty".into());
            }
            if i == 3
                && !matches!(
                    outcome.final_name,
                    jio::FinalNameConfirmation::Unverified { .. }
                )
            {
                return Err("final observation failure lost uncertainty".into());
            }
        }
        checked.push(format!("{primitive:?}"));
    }
    Ok(json!({"passed":true,"checked":checked}))
}

fn work(root: &Path, scan: bool) -> Result<Value, Box<dyn std::error::Error>> {
    let begin = Instant::now();
    let authority = jio::JournalRoot::open(root)?;
    authority.revalidate_canonical_binding()?;
    let atomic = jio::atomic_replace_bound(
        &authority,
        OsStr::new("checkpoint.json"),
        b"{\"committed\":true}",
        0o600,
    )?;
    if !matches!(atomic, jio::BoundAtomicOutcome::Published { .. }) {
        return Err(format!("checkpoint publication unconfirmed: {atomic:?}").into());
    }
    let scan_start = Instant::now();
    let report = if scan {
        format!(
            "{:?}",
            solstone_core_indexer_store::scan::scan_journal(root, false)?
        )
    } else {
        "not_requested".into()
    };
    let scan_ms = scan_start.elapsed().as_secs_f64() * 1000.;
    let request = SearchRequest {
        query: "androidsentinel".into(),
        limit: 8,
        counts: true,
        ..Default::default()
    };
    let query_start = Instant::now();
    let result = solstone_core_indexer_query::search(
        root,
        OwnerBoundary,
        &request,
        chrono::NaiveDate::from_ymd_opt(2026, 10, 11).unwrap(),
    )?;
    let query_ms = query_start.elapsed().as_secs_f64() * 1000.;
    let db = solstone_core_indexer_store::db::open_index_reader(root)?;
    let integrity: String = db.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    let mode: String = db.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
    Ok(
        json!({"scan":report,"scan_ms":scan_ms,"query_ms":query_ms,"total_ms":begin.elapsed().as_secs_f64()*1000.,"result":result,"sqlite_check":integrity,"journal_mode":mode,"atomic":format!("{atomic:?}"),"sqlite_version":rusqlite::version(),"proc_status":fs::read_to_string("/proc/self/status")?.lines().filter(|l| l.starts_with("VmRSS:")||l.starts_with("VmHWM:")||l.starts_with("Threads:")).collect::<Vec<_>>()}),
    )
}

fn resources(root: &Path) -> Result<Value, Box<dyn std::error::Error>> {
    fn size(dir: &Path) -> std::io::Result<u64> {
        let mut bytes = 0;
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            bytes += if meta.is_dir() {
                size(&entry.path())?
            } else {
                meta.len()
            };
        }
        Ok(bytes)
    }
    Ok(
        json!({"smaps_rollup":fs::read_to_string("/proc/self/smaps_rollup")?,
        "fd_count":fs::read_dir("/proc/self/fd")?.count(),"journal_bytes":size(root)?,
        "status":fs::read_to_string("/proc/self/status")?}),
    )
}

fn run(root: &Path, op: &str, count: usize) -> Result<Value, Box<dyn std::error::Error>> {
    if op == "identity" {
        return Ok(
            json!({"pid":std::process::id(),"uid":unsafe{libc::getuid()},"selinux":fs::read_to_string("/proc/self/attr/current")?,"maps":fs::read_to_string("/proc/self/maps")?.lines().filter(|l|l.contains("journal_runtime")||l.contains("journalbridge")).collect::<Vec<_>>(),"sqlite_version":rusqlite::version()}),
        );
    }
    if op == "crash" {
        std::process::abort();
    }
    if op == "cancel" {
        CANCEL.store(true, Ordering::Release);
        return Ok(json!({"cancel_requested":true}));
    }
    if op == "begin" {
        CANCEL.store(false, Ordering::Release);
        return Ok(json!({"cancel_reset":true}));
    }
    if let Some(command) = op.strip_prefix("serve:") {
        return serving(root, command).map_err(Into::into);
    }
    fs::create_dir_all(root)?;
    match op {
        "resources" => resources(root),
        "authority" => {
            let authority = jio::JournalRoot::open(root)?;
            let id = authority.identity();
            let reopen = jio::JournalRoot::open(root)?;
            if id != reopen.identity() {
                return Err("root identity changed".into());
            }
            let probe = root.join(format!("binding-{}", std::process::id()));
            let moved = root.join(format!("binding-moved-{}", std::process::id()));
            fs::create_dir(&probe)?;
            let bound = jio::JournalRoot::open(&probe)?;
            jio::write_bytes_exclusive_bound(&bound, OsStr::new("record"), b"bound inode", 0o600)?;
            fs::rename(&probe, &moved)?;
            fs::create_dir(&probe)?;
            let replacement_refused = bound.revalidate_canonical_binding().is_err();
            let original_retained = jio::read_bytes_bound(&bound, OsStr::new("record"))?.as_deref()
                == Some(b"bound inode".as_slice());
            let link = root.join("symlink-negative");
            if !link.exists() {
                std::os::unix::fs::symlink("checkpoint.json", &link)?;
            }
            let symlink_refused =
                jio::read_bytes_bound(&authority, OsStr::new("symlink-negative")).is_err();
            let overwrite_refused = jio::write_bytes_exclusive_bound(
                &authority,
                OsStr::new("checkpoint.json"),
                b"must not overwrite",
                0o600,
            )
            .is_err();
            let target = root.join("authority-lock");
            let lock = jio::hold_lock(&target, jio::LockOptions::default())?;
            let held = jio::lock_is_held(&target)?;
            drop(lock);
            let released = !jio::lock_is_held(&target)?;
            Ok(
                json!({"reopen_identity":true,"root_replacement_refused":replacement_refused,"bound_original_retained":original_retained,"symlink_refused":symlink_refused,"overwrite_refused":overwrite_refused,"lock_held":held,"lock_released":released}),
            )
        }
        "seed-mixed" => {
            seed_mixed(root, 0)?;
            work(root, true)
        }
        "mutate" => {
            seed_mixed(root, count)?;
            work(root, true)
        }
        "publication-race" => publication_race(root),
        "publication-faults" => publication_faults(root),
        "publication" => {
            let authority = jio::JournalRoot::open(root)?;
            let name = format!("exclusive-{}-{count}.txt", std::process::id());
            let result = jio::write_bytes_exclusive_bound_detailed(
                &authority,
                OsStr::new(&name),
                b"complete synthetic publication",
                0o600,
            )?;
            if !result.is_fully_confirmed() {
                return Err(format!("publication unconfirmed: {result:?}").into());
            }
            let refused = jio::write_bytes_exclusive_bound_detailed(
                &authority,
                OsStr::new(&name),
                b"replacement",
                0o600,
            )
            .unwrap_err();
            if refused.source.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(refused.into());
            }
            Ok(
                json!({"publication":format!("{result:?}"),"confirmed":true,"existing_refused":true}),
            )
        }
        "work" => work(root, true),
        "query" => work(root, false),
        "repeat" => {
            let start = Instant::now();
            let mut elapsed = Vec::new();
            let mut last = Value::Null;
            for _ in 0..count.min(100_000) {
                if CANCEL.load(Ordering::Acquire) {
                    break;
                }
                last = work(root, true)?;
                elapsed.push(last["total_ms"].as_f64().unwrap());
            }
            Ok(
                json!({"iterations":elapsed.len(),"cancelled":CANCEL.load(Ordering::Acquire),"elapsed_ms":start.elapsed().as_secs_f64()*1000.,"iteration_ms":elapsed,"last":last}),
            )
        }
        "atomic-crash" => {
            let authority = jio::JournalRoot::open(root)?;
            let marker_path = root.join("atomic-barrier.json");
            let _ = jio::run_with_bound_publication_barrier(
                jio::BoundPublicationPrimitive::Rename,
                1,
                move || {
                    let mut marker = fs::File::create(marker_path).unwrap();
                    marker
                        .write_all(b"before rename, staged file synced")
                        .unwrap();
                    marker.sync_all().unwrap();
                    // Uncatchable process death at the real publication barrier.
                    unsafe {
                        libc::kill(libc::getpid(), libc::SIGKILL);
                    }
                },
                || {
                    jio::atomic_replace_bound(
                        &authority,
                        OsStr::new("checkpoint.json"),
                        b"interrupted replacement",
                        0o600,
                    )
                },
            );
            Err("atomic crash barrier unexpectedly returned".into())
        }
        "exclusive-crash-before" | "exclusive-crash-after" => {
            let authority = jio::JournalRoot::open(root)?;
            let after = op.ends_with("after");
            let name = if after {
                "exclusive-after"
            } else {
                "exclusive-before"
            };
            // A unique fixture seam on each full run, never owner data.
            if root.join(name).exists() {
                fs::remove_file(root.join(name))?;
            }
            let primitive = if after {
                jio::BoundPublicationPrimitive::PostPublishObserve
            } else {
                jio::BoundPublicationPrimitive::ExclusivePublish
            };
            let _ = jio::run_with_bound_publication_barrier(
                primitive,
                1,
                || unsafe {
                    libc::kill(libc::getpid(), libc::SIGKILL);
                },
                || {
                    jio::write_bytes_exclusive_bound_detailed(
                        &authority,
                        OsStr::new(name),
                        b"complete exclusive crash fixture",
                        0o600,
                    )
                },
            );
            Err("exclusive crash barrier unexpectedly returned".into())
        }
        "recovery" => {
            let authority = jio::JournalRoot::open(root)?;
            let checkpoint = jio::read_bytes_bound(&authority, OsStr::new("checkpoint.json"))?;
            let db = solstone_core_indexer_store::db::open_index_reader(root)?;
            let ghosts: i64 = db.query_row(
                "SELECT count(*) FROM chunks WHERE content='uncommittedghost'",
                [],
                |r| r.get(0),
            )?;
            Ok(
                json!({"checkpoint":String::from_utf8(checkpoint.ok_or("checkpoint missing")?)?,"uncommitted_ghost_rows":ghosts}),
            )
        }
        "transaction-crash" => {
            // Deliberate failure seam in the existing real SQLite index, not a new job ledger.
            let db = rusqlite::Connection::open(solstone_core_indexer_store::db::db_path(root))?;
            db.execute_batch("PRAGMA journal_mode=WAL; BEGIN IMMEDIATE;")?;
            let changed = db.execute(
                "UPDATE chunks SET content=?1 WHERE rowid=(SELECT min(rowid) FROM chunks)",
                ["uncommittedghost"],
            )?;
            let mut marker = fs::File::create(root.join("transaction-open.json"))?;
            marker.write_all(
                json!({"changed":changed,"pid":std::process::id()})
                    .to_string()
                    .as_bytes(),
            )?;
            marker.sync_all()?;
            unsafe {
                libc::kill(libc::getpid(), libc::SIGKILL);
            }
            Err("transaction crash unexpectedly returned".into())
        }
        "snapshot" => {
            let db = rusqlite::Connection::open(solstone_core_indexer_store::db::db_path(root))?;
            db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
            Ok(json!({"checkpointed":true}))
        }
        _ => Err("unknown operation".into()),
    }
}

/// # Safety
/// `root` and `op` must point to live NUL-terminated strings for this call.
/// Release the returned owned string exactly once with `journal_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn journal_run(
    root: *const c_char,
    op: *const c_char,
    count: usize,
) -> *mut c_char {
    let output = std::panic::catch_unwind(|| {
        let root = unsafe { CStr::from_ptr(root) }
            .to_str()
            .map_err(|e| e.to_string())?;
        let op = unsafe { CStr::from_ptr(op) }
            .to_str()
            .map_err(|e| e.to_string())?;
        run(Path::new(root), op, count).map_err(|e| e.to_string())
    });
    let value = match output {
        Ok(Ok(v)) => json!({"ok":true,"value":v}),
        Ok(Err(e)) => json!({"ok":false,"error":e}),
        Err(panic) => {
            json!({"ok":false,"error":format!("native panic: {}", panic.downcast_ref::<String>().map(String::as_str).or_else(|| panic.downcast_ref::<&str>().copied()).unwrap_or("unknown payload"))})
        }
    };
    CString::new(value.to_string()).unwrap().into_raw()
}

/// # Safety
/// `value` must be null or an unreleased pointer returned by `journal_run`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn journal_free(value: *mut c_char) {
    if !value.is_null() {
        drop(unsafe { CString::from_raw(value) });
    }
}
