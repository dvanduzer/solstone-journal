// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
//! Development-only iOS helper bridge. Fault operations never enter a release binary.
use chrono::NaiveDate;
use serde_json::{Value, json};
use solstone_core_indexer_query::{
    OwnerBoundary, OwnerIndex, SearchRequest, open_owner_index, search,
};
use solstone_core_journal_io::{
    JournalRoot,
    atomic::{self, BoundAtomicOutcome, BoundPublicationPrimitive},
};
use std::{
    ffi::{CStr, CString, OsStr, c_char},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
static ACTIVE: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);
static ROOT: Mutex<Option<JournalRoot>> = Mutex::new(None);
static QUERY: Mutex<Option<OwnerIndex>> = Mutex::new(None);
fn fixture(i: u64, revision: u64) -> String {
    format!(
        "# Synthetic memory {i}\n\nauroraruntime document {i}: copper kite revision {revision}.\n"
    )
}
fn query_limit(root: &Path, limit: usize) -> Result<Value, String> {
    query_with(root, limit, true)
}
fn query_with(root: &Path, limit: usize, reuse: bool) -> Result<Value, String> {
    let request = SearchRequest {
        query: "auroraruntime".into(),
        limit,
        ..Default::default()
    };
    let date = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
    let response = if reuse {
        let mut guard = QUERY.lock().map_err(|e| e.to_string())?;
        if guard.is_none() {
            *guard = Some(open_owner_index(root, OwnerBoundary).map_err(|e| e.to_string())?);
        }
        guard.as_mut().unwrap().search(&request, date)
    } else {
        search(root, OwnerBoundary, &request, date)
    }
    .map_err(|e| e.to_string())?;
    serde_json::to_value(response).map_err(|e| e.to_string())
}
fn query(root: &Path) -> Result<Value, String> {
    query_limit(root, 4096)
}
fn replace(root: &JournalRoot, name: &str, data: &[u8]) -> Result<(), String> {
    root.revalidate_canonical_binding()
        .map_err(|e| e.to_string())?;
    match atomic::atomic_replace_bound(root, OsStr::new(name), data, 0o600)
        .map_err(|e| e.to_string())?
    {
        BoundAtomicOutcome::Published { .. } => Ok(()),
        other => Err(format!("publication not fully confirmed: {other:?}")),
    }
}
fn create(root: &JournalRoot, name: &str, data: &[u8]) -> Result<(), String> {
    root.revalidate_canonical_binding()
        .map_err(|e| e.to_string())?;
    let result = atomic::write_bytes_exclusive_bound_detailed(root, OsStr::new(name), data, 0o600)
        .map_err(|e| e.to_string())?;
    if result.is_fully_confirmed() {
        Ok(())
    } else {
        Err(format!("publication not fully confirmed: {result:?}"))
    }
}
fn inventory(root: &Path) -> Result<Value, String> {
    let files=std::fs::read_dir(root.join("chronicle/20261012/talents")).map_err(|e|e.to_string())?.map(|e|{
        let e=e.map_err(|e|e.to_string())?;
        Ok(json!({"name":e.file_name().to_string_lossy(),"text":std::fs::read_to_string(e.path()).map_err(|e|e.to_string())?}))
    }).collect::<Result<Vec<_>,String>>()?;
    let entries = std::fs::read_dir(root)
        .map_err(|e| e.to_string())?
        .map(|e| {
            e.map(|e| e.file_name().to_string_lossy().into_owned())
                .map_err(|e| e.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut sizes = serde_json::Map::new();
    for name in [
        "indexer/journal.sqlite",
        "indexer/journal.sqlite-wal",
        "indexer/journal.sqlite-shm",
    ] {
        sizes.insert(
            name.into(),
            json!(
                std::fs::metadata(root.join(name))
                    .map(|m| m.len())
                    .unwrap_or(0)
            ),
        );
    }
    Ok(
        json!({"files":files,"entries":entries,"sizes":sizes,"checkpoint":std::fs::read_to_string(root.join("checkpoint.bin")).ok(),"committed":std::fs::read_to_string(root.join("committed.bin")).ok(),"exclusive":std::fs::read_to_string(root.join("exclusive.bin")).ok()}),
    )
}
fn run(v: Value) -> Result<Value, String> {
    let op = v["op"].as_str().ok_or("missing operation")?;
    if op == "activity" {
        return Ok(json!({"active": ACTIVE.load(Ordering::SeqCst)}));
    }
    if op == "cancel" {
        CANCEL.store(true, Ordering::SeqCst);
        return Ok(json!({"cancel_requested":true}));
    }
    let mut guard = ROOT.lock().map_err(|e| e.to_string())?;
    if op == "open" {
        let requested = PathBuf::from(v["root"].as_str().ok_or("missing helper-owned root")?);
        if let Some(current) = guard.as_ref() {
            current
                .revalidate_canonical_binding()
                .map_err(|e| e.to_string())?;
            if current.canonical_path()
                != std::fs::canonicalize(&requested).map_err(|e| e.to_string())?
            {
                return Err("worker already bound to another journal".into());
            }
        } else {
            std::fs::create_dir_all(&requested).map_err(|e| e.to_string())?;
            *guard = Some(JournalRoot::open(&requested).map_err(|e| e.to_string())?);
        }
    }
    let root = guard.as_ref().ok_or("open the journal first")?;
    root.revalidate_canonical_binding()
        .map_err(|e| e.to_string())?;
    let p = root.canonical_path();
    match op {
        "prepare-export" => {
            let requested = v["root"].as_str().ok_or("missing export authority")?;
            if std::fs::canonicalize(requested).map_err(|e| e.to_string())? != p {
                return Err("export journal does not match admitted root".into());
            }
            // Release the actor reader before Swift snapshots the database and WAL.
            // The serial work queue excludes concurrent IO.
            QUERY.lock().map_err(|e| e.to_string())?.take();
            Ok(json!({"validated":true}))
        }
        "open" => Ok(json!({"root":p,"identity":root.identity().observation_id()})),
        "seed" => {
            let count = v["count"].as_u64().unwrap_or(128).clamp(1, 4096);
            let dir = p.join("chronicle/20261012/talents");
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let bound = JournalRoot::open(&dir).map_err(|e| e.to_string())?;
            for i in 0..count {
                create(
                    &bound,
                    &format!("fixture{i:04}.md"),
                    fixture(i, 0).as_bytes(),
                )?;
            }
            replace(root, "checkpoint.bin", b"prior complete checkpoint")?;
            Ok(json!({"seeded":count}))
        }
        "index" => {
            let r = solstone_core_indexer_store::scan::scan_journal(p, true)
                .map_err(|e| e.to_string())?;
            Ok(
                json!({"indexed":r.indexed,"failed":r.failed,"warnings":r.warnings,"query":query(p)?}),
            )
        }
        "query" => query(p),
        "inventory" => inventory(p),
        "create" => {
            create(root, "exclusive.bin", b"create once")?;
            Ok(json!({"created":true}))
        }
        "crash-write" => {
            let _ = atomic::run_with_bound_publication_barrier(
                BoundPublicationPrimitive::Rename,
                1,
                || std::process::abort(),
                || replace(root, "checkpoint.bin", b"unpublished replacement"),
            );
            Err("write fault did not fire".into())
        }
        "commit-disconnect" => {
            create(root, "committed.bin", b"committed before reply loss")?;
            std::process::abort()
        }
        "recover" => {
            let before = inventory(p)?;
            replace(root, "checkpoint.bin", b"recovered complete checkpoint")?;
            Ok(json!({"before":before,"query":query(p)?}))
        }
        "binding-control" => {
            let moved = p.with_extension("binding-control");
            if moved.exists() {
                return Err("binding control destination exists".into());
            }
            std::fs::rename(p, &moved).map_err(|e| e.to_string())?;
            std::fs::create_dir(p).map_err(|e| e.to_string())?;
            let refusal = replace(root, "must-not-publish", b"no").is_err();
            let absent =
                !p.join("must-not-publish").exists() && !moved.join("must-not-publish").exists();
            std::fs::remove_dir(p).map_err(|e| e.to_string())?;
            std::fs::rename(&moved, p).map_err(|e| e.to_string())?;
            root.revalidate_canonical_binding()
                .map_err(|e| e.to_string())?;
            Ok(json!({"refused":refusal,"both_absent":absent}))
        }
        "active" | "control" => {
            CANCEL.store(false, Ordering::SeqCst);
            ACTIVE.store(true, Ordering::SeqCst);
            struct ActiveGuard;
            impl Drop for ActiveGuard {
                fn drop(&mut self) {
                    ACTIVE.store(false, Ordering::SeqCst);
                }
            }
            let _active = ActiveGuard;
            let seconds = v["seconds"].as_u64().unwrap_or(10).clamp(1, 120);
            let mode = v["mode"].as_str().unwrap_or("reopen");
            // Controls remove the normal actor reader, then vary only reader lifetime.
            QUERY.lock().map_err(|e| e.to_string())?.take();
            // Retain a store reader while still calling the one-shot search API.
            let retained = if mode == "retained" {
                Some(
                    solstone_core_indexer_store::db::open_index_reader(p)
                        .map_err(|e| e.to_string())?,
                )
            } else {
                None
            };
            if let Some(c) = &retained {
                c.query_row("SELECT count(*) FROM chunks", [], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(|e| e.to_string())?;
            }
            let start = Instant::now();
            let mut n = 0;
            while start.elapsed() < Duration::from_secs(seconds) && !CANCEL.load(Ordering::SeqCst) {
                query_with(p, 128, mode == "session")?;
                n += 1;
            }
            QUERY.lock().map_err(|e| e.to_string())?.take();
            Ok(
                json!({"queries":n,"seconds":start.elapsed().as_secs_f64(),"cancelled":CANCEL.load(Ordering::SeqCst),"mode":mode}),
            )
        }
        "mixed" => {
            let i = v["count"].as_u64().unwrap_or(0) % 32;
            let revision = v["revision"].as_u64().unwrap_or(1);
            let bound = JournalRoot::open(&p.join("chronicle/20261012/talents"))
                .map_err(|e| e.to_string())?;
            replace(
                &bound,
                &format!("fixture{i:04}.md"),
                fixture(i, revision).as_bytes(),
            )?;
            replace(
                root,
                "checkpoint.bin",
                format!("mixed revision {revision}").as_bytes(),
            )?;
            let r = solstone_core_indexer_store::scan::scan_journal(p, true)
                .map_err(|e| e.to_string())?;
            Ok(
                json!({"revision":revision,"failed":r.failed,"warnings":r.warnings,"query":query_limit(p,8)?}),
            )
        }
        _ => Err(format!("unknown operation {op}")),
    }
}
/// # Safety
/// Input is a live NUL-terminated UTF-8 string; free the returned string once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn journal_runtime(input: *const c_char) -> *mut c_char {
    let start = Instant::now();
    let result = std::panic::catch_unwind(|| {
        if input.is_null() {
            return Err("null request".into());
        }
        let s = unsafe { CStr::from_ptr(input) }
            .to_str()
            .map_err(|e| e.to_string())?;
        run(serde_json::from_str(s).map_err(|e| e.to_string())?)
    });
    let body = match result {
        Ok(Ok(v)) => json!({"ok":true,"value":v}),
        Ok(Err(e)) => json!({"ok":false,"error":e}),
        Err(_) => json!({"ok":false,"error":"native panic; reopen helper"}),
    };
    CString::new(json!({"pid":std::process::id(),"elapsed_ms":start.elapsed().as_secs_f64()*1000.,"result":body}).to_string()).unwrap().into_raw()
}
/// # Safety
/// Pointer was returned by journal_runtime and has not been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn journal_runtime_free(p: *mut c_char) {
    if !p.is_null() {
        drop(unsafe { CString::from_raw(p) });
    }
}
