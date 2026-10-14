// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
//! Development-only iOS helper bridge. Fault operations never enter a release binary.
use chrono::NaiveDate;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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
        LazyLock, Mutex, Once,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
static ACTIVE: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);
static ROOT: Mutex<Option<JournalRoot>> = Mutex::new(None);
static QUERY: Mutex<Option<OwnerIndex>> = Mutex::new(None);
static SERVING: Mutex<Option<journal_mobile_serving::MobileServing>> = Mutex::new(None);
static PAYLOAD: Mutex<Option<PathBuf>> = Mutex::new(None);
static GENERATE: LazyLock<solstone_core_generate_wire::EndpointRuntime> =
    LazyLock::new(solstone_core_generate_wire::EndpointRuntime::default);

// The development host has no desktop logger. Retain existing door warnings
// so a dropped TLS carrier can be correlated with its server-side cause.
struct DoorWarnings;
impl log::Log for DoorWarnings {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
            && metadata
                .target()
                .starts_with("solstone_core_convey_shell::door")
    }
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr().lock(),
                "ios-door time_ms={} pid={} {}: {}",
                chrono::Utc::now().timestamp_millis(),
                std::process::id(),
                record.level(),
                record.args()
            );
        }
    }
    fn flush(&self) {}
}
fn install_door_warnings() {
    static LOGGER: DoorWarnings = DoorWarnings;
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        if log::set_logger(&LOGGER).is_ok() {
            log::set_max_level(log::LevelFilter::Warn);
        }
    });
}
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
fn wifi_address() -> Result<std::net::Ipv4Addr, String> {
    let mut interfaces: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut interfaces) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let mut cursor = interfaces;
    let mut address = None;
    while !cursor.is_null() {
        let interface = unsafe { &*cursor };
        if !interface.ifa_addr.is_null()
            && unsafe { (*interface.ifa_addr).sa_family as i32 } == libc::AF_INET
            && unsafe { CStr::from_ptr(interface.ifa_name) }.to_bytes() == b"en0"
        {
            let socket = unsafe { &*(interface.ifa_addr as *const libc::sockaddr_in) };
            let ip = std::net::Ipv4Addr::from(socket.sin_addr.s_addr.to_ne_bytes());
            if ip.is_private() {
                address = Some(ip);
            }
        }
        cursor = interface.ifa_next;
    }
    unsafe { libc::freeifaddrs(interfaces) };
    let ip = address.ok_or("no private en0 IPv4 address")?;
    Ok(ip)
}
fn run(v: Value) -> Result<Value, String> {
    let op = v["op"].as_str().ok_or("missing operation")?;
    if op == "serve-crash" {
        std::process::abort();
    }
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
        "serve-start" => {
            let mut serving = SERVING.lock().map_err(|e| e.to_string())?;
            if serving.is_some() {
                return Err("serving already started".into());
            }
            let port = v["count"]
                .as_u64()
                .filter(|p| (1024..=65535).contains(p))
                .ok_or("port must be between 1024 and 65535")? as u16;
            let address = std::net::SocketAddrV4::new(wifi_address()?, port);
            let payload = PathBuf::from(v["payload"].as_str().ok_or("missing bundled payload")?);
            #[cfg(target_os = "ios")]
            {
                let target = std::env::temp_dir().join("callosum.sock");
                if !target.is_absolute() || target.as_os_str().len() >= 104 {
                    return Err("app-private Callosum target exceeds Unix socket path bound".into());
                }
                let alias = p.join("health/callosum.sock");
                std::fs::create_dir_all(alias.parent().ok_or("missing health directory")?)
                    .map_err(|e| e.to_string())?;
                match std::fs::symlink_metadata(&alias) {
                    Ok(metadata)
                        if metadata.file_type().is_symlink()
                            && std::fs::read_link(&alias).map_err(|e| e.to_string())? == target => {
                    }
                    // iOS changes the app container on reinstall. This alias is
                    // app-owned transient IPC, so replace its link (never its
                    // old target) with the current app-private endpoint.
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        let temporary = alias.with_file_name(format!(
                            ".callosum-alias-{}-{}",
                            std::process::id(),
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map_err(|e| e.to_string())?
                                .as_nanos()
                        ));
                        std::os::unix::fs::symlink(&target, &temporary)
                            .map_err(|e| e.to_string())?;
                        if let Err(error) = std::fs::rename(&temporary, &alias) {
                            let _ = std::fs::remove_file(temporary);
                            return Err(error.to_string());
                        }
                    }
                    Ok(_) => return Err("Callosum endpoint already has another binding".into()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        std::os::unix::fs::symlink(&target, &alias).map_err(|e| e.to_string())?
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
            let runtime = journal_mobile_serving::MobileServing::start_with_payload(
                p.to_owned(),
                address,
                payload.clone(),
            )?;
            *PAYLOAD.lock().map_err(|e| e.to_string())? = Some(payload);
            let status = runtime.status();
            *serving = Some(runtime);
            Ok(json!({"address":address.to_string(),"status":status}))
        }
        #[cfg(feature = "audio")]
        "audio-enable" => {
            let models = PathBuf::from(
                v["payload"]
                    .as_str()
                    .ok_or("missing bundled audio models")?,
            );
            let scratch = p.join(".audio-scratch");
            let model = match v["mode"].as_str().unwrap_or("reopen") {
                "missing-model" => scratch.join("missing.gguf"),
                "corrupt-model" => {
                    std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
                    let model = scratch.join("corrupt.gguf");
                    std::fs::write(&model, b"synthetic corrupt model")
                        .map_err(|e| e.to_string())?;
                    model
                }
                _ => models.join("tdt-0.6b-v3-q8_0.gguf"),
            };
            let mut serving = SERVING.lock().map_err(|e| e.to_string())?;
            let serving = serving.as_mut().ok_or("serving is stopped")?;
            serving.enable_audio(models.clone(), model, scratch)?;
            Ok(serving.status())
        }
        #[cfg(feature = "audio")]
        "audio-cancel" | "audio-resume" => {
            let serving = SERVING.lock().map_err(|e| e.to_string())?;
            let serving = serving.as_ref().ok_or("serving is stopped")?;
            if op == "audio-cancel" {
                serving.cancel_audio()?;
            } else {
                serving.resume_audio()?;
            }
            Ok(serving.status())
        }
        "serve-stop" => {
            if let Some(serving) = SERVING.lock().map_err(|e| e.to_string())?.take() {
                serving.shutdown();
            }
            GENERATE.drain_idle_pool();
            PAYLOAD.lock().map_err(|e| e.to_string())?.take();
            Ok(json!({"stopped":true}))
        }
        "serve-generate" | "serve-talent" => {
            let payload = PAYLOAD
                .lock()
                .map_err(|e| e.to_string())?
                .clone()
                .ok_or("serving is stopped")?;
            let raw = v["payload"].as_str().ok_or("missing thinking request")?;
            let generate = |request: &solstone_core_generate::GenerateRequest| {
                solstone_core_generate_wire::respond(request, &GENERATE, p, |_| None)
                    .map_err(solstone_core_generate::ClientError::Decode)
            };
            if op == "serve-generate" {
                let request = solstone_core_generate_wire::parse_one_shot_request(raw)?;
                let response = generate(&request).map_err(|e| e.to_string())?;
                let encoded = solstone_core_generate::encode_one_shot_response(&response)?;
                return serde_json::from_str(&encoded).map_err(|e| e.to_string());
            }
            let request = serde_json::from_str::<serde_json::Map<String, Value>>(raw)
                .map_err(|e| e.to_string())?;
            let paths = solstone_core_talent_runtime::prepare::RuntimePaths {
                talent_root: payload.join("solstone/talent"),
                apps_root: payload.join("solstone/apps"),
                templates_dir: payload.join("solstone/think/templates"),
            };
            let context = solstone_core_talent_runtime::ExecutionContext {
                journal: p.to_owned(),
            };
            let mut events = Vec::new();
            let outcome = solstone_core_talent_runtime::execute_request(
                request,
                &paths,
                &context,
                &generate,
                &mut events,
            );
            Ok(json!({"outcome":format!("{outcome:?}"),
                "events":String::from_utf8(events).map_err(|e|e.to_string())?}))
        }
        "serve-index" => {
            let report = solstone_core_indexer_store::scan::scan_journal(p, false)
                .map_err(|e| e.to_string())?;
            if report.failed != 0 {
                return Err(format!("index scan failed: {:?}", report.failures));
            }
            Ok(
                json!({"indexed":report.indexed,"removed":report.removed,"skipped":report.skipped,
                "failed":report.failed,"warnings":report.warnings}),
            )
        }
        "serve-status" | "serve-owner" => {
            let serving = SERVING.lock().map_err(|e| e.to_string())?;
            let serving = serving.as_ref().ok_or("serving is stopped")?;
            match op {
                "serve-status" => Ok(serving.status()),
                _ => {
                    let payload: Value =
                        serde_json::from_str(v["payload"].as_str().ok_or("missing owner request")?)
                            .map_err(|e| e.to_string())?;
                    let response = serving.request_owner(
                        payload["method"].as_str().ok_or("missing method")?,
                        payload["uri"].as_str().ok_or("missing URI")?,
                        payload["body"].as_str().unwrap_or(""),
                    )?;
                    Ok(json!({"status":response.status,"body":response.body}))
                }
            }
        }
        "serve-custody" => {
            let requested = v["root"].as_str().ok_or("missing custody authority")?;
            if std::fs::canonicalize(requested).map_err(|e| e.to_string())? != p {
                return Err("custody journal does not match admitted root".into());
            }
            Ok(json!({"root_validated":true}))
        }
        "serve-inventory" => {
            if SERVING.lock().map_err(|e| e.to_string())?.is_some() {
                return Err("stop serving before snapshotting files".into());
            }
            let mut files = serde_json::Map::new();
            for name in [
                "facets/work/activities/20261012.jsonl",
                "link/state.json",
                "link/authorized_clients.json",
            ] {
                files.insert(
                    name.into(),
                    json!(std::fs::read_to_string(p.join(name)).ok()),
                );
            }
            fn storage(path: &Path) -> Result<(u64, u64), String> {
                let mut sum = (0, 0);
                for entry in std::fs::read_dir(path).map_err(|e| e.to_string())? {
                    let entry = entry.map_err(|e| e.to_string())?;
                    let metadata = entry.metadata().map_err(|e| e.to_string())?;
                    let count = if metadata.is_dir() {
                        storage(&entry.path())?
                    } else {
                        (1, metadata.len())
                    };
                    sum.0 += count.0;
                    sum.1 += count.1;
                }
                Ok(sum)
            }
            let (count, bytes) = storage(p)?;
            Ok(json!({"files":files,"storage_files":count,"storage_bytes":bytes}))
        }
        "audio-inventory" => {
            if SERVING.lock().map_err(|e| e.to_string())?.is_some() {
                return Err("stop serving before snapshotting audio files".into());
            }
            fn collect(root: &Path, path: &Path, files: &mut Vec<Value>) -> Result<(), String> {
                for entry in std::fs::read_dir(path).map_err(|e| e.to_string())? {
                    let entry = entry.map_err(|e| e.to_string())?;
                    let kind = entry.file_type().map_err(|e| e.to_string())?;
                    if kind.is_symlink() {
                        if entry.path() == root.join("health/callosum.sock") {
                            files.push(json!({"path":"health/callosum.sock","socket_alias":
                                std::fs::read_link(entry.path()).map_err(|e| e.to_string())?}));
                            continue;
                        }
                        return Err("unexpected synthetic journal symlink".into());
                    }
                    if kind.is_dir() {
                        collect(root, &entry.path(), files)?;
                    } else if kind.is_file() {
                        let path = entry.path();
                        let mut file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
                        let size = file.metadata().map_err(|e| e.to_string())?.len();
                        let mut hash = Sha256::new();
                        std::io::copy(&mut file, &mut hash).map_err(|e| e.to_string())?;
                        let text = (path.starts_with(root.join("chronicle"))
                            && matches!(
                                path.extension().and_then(|e| e.to_str()),
                                Some("jsonl" | "json")
                            ))
                        .then(|| std::fs::read_to_string(&path))
                        .transpose()
                        .map_err(|e| e.to_string())?;
                        files.push(
                            json!({"path":path.strip_prefix(root).map_err(|e| e.to_string())?,
                            "bytes":size,"sha256":format!("{:x}",hash.finalize()),"text":text}),
                        );
                    }
                }
                Ok(())
            }
            let mut files = Vec::new();
            collect(p, p, &mut files)?;
            Ok(json!({"files":files}))
        }
        "prepare-export" => {
            if SERVING.lock().map_err(|e| e.to_string())?.is_some() {
                return Err("stop serving before exporting a snapshot".into());
            }
            let requested = v["root"].as_str().ok_or("missing export authority")?;
            if std::fs::canonicalize(requested).map_err(|e| e.to_string())? != p {
                return Err("export journal does not match admitted root".into());
            }
            // Release the actor reader before Swift snapshots the database and WAL.
            // Serving is stopped; the serial work queue excludes concurrent IO.
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
        install_door_warnings();
        if input.is_null() {
            return Err("null request".into());
        }
        let s = unsafe { CStr::from_ptr(input) }
            .to_str()
            .map_err(|e| e.to_string())?;
        let request = serde_json::from_str(s).map_err(|e| e.to_string())?;
        #[cfg(target_os = "ios")]
        {
            // GCD worker stacks are too small for the debug build's nested
            // router/TLS futures. Keep the caller's serial admission while
            // executing native work on an explicitly bounded Rust stack.
            let worker = std::thread::Builder::new()
                .name("journal-native-call".into())
                .stack_size(8 * 1024 * 1024)
                .spawn(move || run(request))
                .map_err(|e| e.to_string())?;
            match worker.join() {
                Ok(result) => result,
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }
        #[cfg(not(target_os = "ios"))]
        run(request)
    });
    let body = match result {
        Ok(Ok(v)) => json!({"ok":true,"value":v}),
        Ok(Err(e)) => json!({"ok":false,"error":e}),
        Err(payload) => {
            let detail = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            json!({"ok":false,"error":format!("native panic; reopen helper: {detail}")})
        }
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
