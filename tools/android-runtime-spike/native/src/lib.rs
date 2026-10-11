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

fn run(root: &Path, op: &str, count: usize) -> Result<Value, Box<dyn std::error::Error>> {
    if op == "identity" {
        return Ok(
            json!({"pid":std::process::id(),"uid":unsafe{libc::getuid()},"selinux":fs::read_to_string("/proc/self/attr/current")?,"maps":fs::read_to_string("/proc/self/maps")?.lines().filter(|l|l.contains("runtime_spike")||l.contains("journalbridge")).collect::<Vec<_>>(),"sqlite_version":rusqlite::version()}),
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
    fs::create_dir_all(root)?;
    match op {
        "authority" => {
            let authority = jio::JournalRoot::open(root)?;
            let id = authority.identity();
            let reopen = jio::JournalRoot::open(root)?;
            if id != reopen.identity() {
                return Err("root identity changed".into());
            }
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
                json!({"reopen_identity":true,"symlink_refused":symlink_refused,"overwrite_refused":overwrite_refused,"lock_held":held,"lock_released":released}),
            )
        }
        "publication" => {
            let authority = jio::JournalRoot::open(root)?;
            let name = format!("exclusive-{}.txt", std::process::id());
            let result = jio::write_bytes_exclusive_bound_detailed(
                &authority,
                OsStr::new(&name),
                b"complete synthetic publication",
                0o600,
            );
            Ok(json!({"publication":format!("{result:?}")}))
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

#[unsafe(no_mangle)]
pub unsafe extern "C" fn spike_run(
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
        Err(_) => json!({"ok":false,"error":"native panic"}),
    };
    CString::new(value.to_string()).unwrap().into_raw()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn spike_free(value: *mut c_char) {
    if !value.is_null() {
        drop(unsafe { CString::from_raw(value) });
    }
}
