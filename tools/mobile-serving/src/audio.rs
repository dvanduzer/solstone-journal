// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! A bounded foreground processing pass over durable input, with no job ledger.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solstone_core_transcribe::{TranscribeOutcome, discover_audio_files, process_embedded};
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

// Keep local inference available without loading its model when the owner has
// chosen confidential transcription. The model is verified before local use.
struct LocalAsr<'a> {
    model: &'a Path,
    scratch: &'a Path,
    loaded: RefCell<Option<journal_mobile_audio::Parakeet>>,
    load_ms: Cell<Option<f64>>,
}

impl solstone_core_transcribe::EmbeddedAsr for LocalAsr<'_> {
    fn transcribe(&self, samples: &[f32]) -> Result<Value, String> {
        let mut loaded = self.loaded.borrow_mut();
        if loaded.is_none() {
            let started = Instant::now();
            *loaded = Some(journal_mobile_audio::Parakeet::new(
                self.model,
                self.scratch,
            )?);
            self.load_ms
                .set(Some(started.elapsed().as_secs_f64() * 1000.));
        }
        loaded.as_ref().unwrap().transcribe(samples)
    }

    fn model_info(&self) -> solstone_core_transcribe::EmbeddedModelInfo {
        journal_mobile_audio::Parakeet::pinned_model_info()
    }
}

struct State {
    stop: AtomicBool,
    cancel: AtomicBool,
    generation: AtomicU64,
    status: Mutex<Value>,
    wake: Condvar,
}

impl State {
    fn request_cancel(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.cancel.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    fn cancelled(&self, generation: u64) -> bool {
        self.cancel.load(Ordering::Acquire) || self.generation.load(Ordering::Acquire) != generation
    }
}

pub(super) struct AudioWorker {
    state: Arc<State>,
    thread: Option<JoinHandle<()>>,
}

impl AudioWorker {
    pub fn start(
        root: PathBuf,
        models: PathBuf,
        model: PathBuf,
        scratch: PathBuf,
    ) -> Result<Self, String> {
        let state = Arc::new(State {
            stop: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            status: Mutex::new(json!({"state":"starting"})),
            wake: Condvar::new(),
        });
        let shared = state.clone();
        let thread = thread::Builder::new()
            .name("journal-audio".into())
            .spawn(move || {
                while !shared.stop.load(Ordering::Acquire) {
                    if let Err(error) = run(&shared, &root, &models, &model, &scratch) {
                        shared.cancel.store(true, Ordering::Release);
                        *shared.status.lock().unwrap() = json!({"state":"failed","error":error});
                        let mut status = shared.status.lock().unwrap();
                        while shared.cancel.load(Ordering::Acquire)
                            && !shared.stop.load(Ordering::Acquire)
                        {
                            status = shared.wake.wait(status).unwrap();
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            state,
            thread: Some(thread),
        })
    }

    pub fn status(&self) -> Value {
        self.state.status.lock().unwrap().clone()
    }
    pub fn cancel(&self) {
        self.state.request_cancel();
    }
    pub fn stop(&self) {
        self.state.stop.store(true, Ordering::Release);
        self.cancel();
    }
    pub fn resume(&self) {
        // A repaired input/model must retry even when failure did not cancel
        // the worker. Advancing also preserves cancellation of old inferences.
        self.state.generation.fetch_add(1, Ordering::AcqRel);
        self.state.cancel.store(false, Ordering::Release);
        self.state.wake.notify_all();
    }
}

impl Drop for AudioWorker {
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    state: &State,
    root: &Path,
    models: &Path,
    model: &Path,
    scratch: &Path,
) -> Result<(), String> {
    let authority = solstone_core_journal_io::JournalRoot::open(root).map_err(|e| e.to_string())?;
    let started = Instant::now();
    let asr = LocalAsr {
        model,
        scratch,
        loaded: RefCell::new(None),
        load_ms: Cell::new(None),
    };
    let worker_start_ms = started.elapsed().as_secs_f64() * 1000.;
    *state.status.lock().unwrap() = json!({"state":"ready","worker_start_ms":worker_start_ms,"model_load_ms":asr.load_ms.get()});
    let mut failures = HashSet::new();
    let mut retry_generation = state.generation.load(Ordering::Acquire);
    while !state.stop.load(Ordering::Acquire) {
        authority
            .revalidate_canonical_binding()
            .map_err(|e| e.to_string())?;
        let current_generation = state.generation.load(Ordering::Acquire);
        if current_generation != retry_generation {
            failures.clear();
            retry_generation = current_generation;
        }
        if state.cancel.load(Ordering::Acquire) {
            let mut status = state.status.lock().unwrap();
            if status["state"] != "failed" {
                status["state"] = json!("cancelled");
            }
        } else {
            let mut errors = Vec::new();
            let inputs = discover_audio_files(root, &mut |_| {}, &mut |day, error| {
                errors.push(format!("{}: {error}", day.display()))
            })
            .collect::<Vec<_>>();
            if !errors.is_empty() {
                return Err(errors.join("; "));
            }
            for input in inputs {
                if state.stop.load(Ordering::Acquire) || state.cancel.load(Ordering::Acquire) {
                    break;
                }
                if failures.contains(&input) {
                    continue;
                }
                match accepted_input(root, &input) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => {
                        failures.insert(input.clone());
                        *state.status.lock().unwrap() = json!({"state":"failed","input":input,"error":error,"failed_inputs":failures.len()});
                        continue;
                    }
                }
                let generation = state.generation.load(Ordering::Acquire);
                let start = Instant::now();
                let prior = state.status.lock().unwrap().clone();
                *state.status.lock().unwrap() = json!({"state":"processing","input":input,"previous":prior.get("last"),"model_load_ms":asr.load_ms.get()});
                let result = process_embedded(&input, root, models, scratch, &asr, &|| {
                    state.cancelled(generation)
                });
                if state.cancelled(generation) {
                    state.status.lock().unwrap()["state"] = json!("cancelled");
                    break;
                }
                match result {
                    Ok(result) if result.outcome == TranscribeOutcome::Skipped => {
                        *state.status.lock().unwrap() = prior;
                    }
                    Ok(result) => {
                        let last = json!({"input":input,"outcome":format!("{:?}",result.outcome),"transcript":result.transcript_path,"processing_ms":start.elapsed().as_secs_f64()*1000.,"failed_inputs":failures.len()});
                        *state.status.lock().unwrap() =
                            json!({"state":"ready","model_load_ms":asr.load_ms.get(),"last":last});
                    }
                    Err(error) => {
                        // Keep the original and allow subsequent inputs to proceed.
                        // Cancellation/resume or restart retries failed inputs.
                        failures.insert(input.clone());
                        *state.status.lock().unwrap() = json!({"state":"failed","input":input,"error":error.to_string(),"model_load_ms":asr.load_ms.get(),"failed_inputs":failures.len()});
                    }
                }
            }
        }
        let status = state.status.lock().unwrap();
        let _ = state
            .wake
            .wait_timeout(status, Duration::from_secs(2))
            .unwrap();
    }
    Ok(())
}

/// Publication before receipt persistence is not a completed network input.
/// Match the accepted custody receipt before giving these bytes to a processor.
fn accepted_input(root: &Path, input: &Path) -> Result<bool, String> {
    let segment = input.parent().ok_or("audio segment missing")?;
    let relative = segment
        .strip_prefix(root.join("chronicle"))
        .map_err(|e| e.to_string())?;
    let parts = relative
        .iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>();
    if parts.len() != 3 {
        return Ok(false);
    }
    let report =
        solstone_core_callosum::read_device_ingest_events(segment).map_err(|e| e.to_string())?;
    if report.unparseable != 0 {
        return Err(format!(
            "unreadable audio custody receipt: {}",
            segment.display()
        ));
    }
    let name = input
        .file_name()
        .ok_or("audio filename missing")?
        .to_string_lossy();
    let files = report
        .records
        .iter()
        .filter(|r| {
            r.outcome == "accepted"
                && r.day == parts[0]
                && r.stream == parts[1]
                && r.segment == parts[2]
        })
        .flat_map(|r| &r.files)
        .filter(|f| f.written == name)
        .collect::<Vec<_>>();
    if files.is_empty() {
        return Ok(false);
    }
    let mut file = File::open(input).map_err(|e| e.to_string())?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    if size > 16 * 1024 * 1024 {
        return Err("embedded audio exceeds 16 MiB encoded input limit".into());
    }
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("audio input is not a regular file".into());
    }
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 65536];
    loop {
        let n = file.read(&mut bytes).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    let hash = format!("{:x}", hash.finalize());
    Ok(files.iter().any(|f| f.size == size && f.sha256 == hash))
}

#[cfg(all(test, feature = "full-tests"))]
mod tests {
    use super::*;
    use solstone_core_callosum::{
        DeviceIngestEvent, DurableEvent, FileDescriptor, append_durable_event,
    };

    #[test]
    fn resume_cannot_erase_cancellation_of_an_active_inference() {
        let state = State {
            stop: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            status: Mutex::new(json!({})),
            wake: Condvar::new(),
        };
        let active = state.generation.load(Ordering::Acquire);
        assert!(!state.cancelled(active));
        state.request_cancel();
        let worker = AudioWorker {
            state: Arc::new(state),
            thread: None,
        };
        worker.resume(); // resume before inference returns
        let state = &worker.state;
        assert!(state.cancelled(active));
        assert!(!state.cancelled(state.generation.load(Ordering::Acquire)));
        // Per-input failures leave cancel false. Resume must still retire that
        // attempt, causing the scan's failure cache to clear before retry.
        let failed_attempt = state.generation.load(Ordering::Acquire);
        worker.resume();
        assert!(state.cancelled(failed_attempt));
        assert!(!state.cancelled(state.generation.load(Ordering::Acquire)));
    }

    #[test]
    fn only_accepted_identical_input_is_admitted_and_oversize_is_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let segment = temp.path().join("chronicle/20261012/device_audio/120000_3");
        std::fs::create_dir_all(&segment).unwrap();
        let input = segment.join("audio.wav");
        std::fs::write(&input, b"speech").unwrap();
        assert!(!accepted_input(temp.path(), &input).unwrap());
        let mut event = DeviceIngestEvent {
            record_type: "device_ingest".into(),
            record_version: 1,
            outcome: "partial".into(),
            protocol_version: 3,
            cid: "fixture".into(),
            source: "audio".into(),
            stream: "device_audio".into(),
            day: "20261012".into(),
            segment: "120000_3".into(),
            meta: Default::default(),
            extra: Default::default(),
            files: vec![FileDescriptor {
                submitted: "audio.wav".into(),
                written: "audio.wav".into(),
                size: 6,
                sha256: format!("{:x}", Sha256::digest(b"speech")),
                extra: Default::default(),
            }],
        };
        append_durable_event(&segment, &DurableEvent::DeviceIngest(event.clone())).unwrap();
        assert!(!accepted_input(temp.path(), &input).unwrap());
        event.outcome = "accepted".into();
        append_durable_event(&segment, &DurableEvent::DeviceIngest(event)).unwrap();
        assert!(accepted_input(temp.path(), &input).unwrap());
        std::fs::write(&input, b"changed").unwrap();
        assert!(!accepted_input(temp.path(), &input).unwrap());
        File::options()
            .write(true)
            .open(&input)
            .unwrap()
            .set_len(16 * 1024 * 1024 + 1)
            .unwrap();
        assert!(
            accepted_input(temp.path(), &input)
                .unwrap_err()
                .contains("16 MiB")
        );
    }
}
