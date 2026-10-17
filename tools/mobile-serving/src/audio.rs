// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! A bounded foreground processing pass over durable input, with no job ledger.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solstone_core_transcribe::{
    TranscribeOutcome, discover_audio_files,
    embedded_already_processed, embedded_retry_context, process_embedded,
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashSet, VecDeque},
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
    recent_failures: Mutex<VecDeque<Value>>,
    wake: Condvar,
}

impl State {
    fn input_failed(&self, generation: u64, input: &Path, error: &str) {
        // Private qualification/status evidence, bounded and process-local.
        // A later input can replace the headline without erasing this attempt.
        let mut failures = self.recent_failures.lock().unwrap();
        if failures.len() == 64 {
            failures.pop_front();
        }
        failures.push_back(json!({"generation":generation,"input":input,"error":error}));
    }

    fn snapshot(&self) -> Value {
        let mut status = self.status.lock().unwrap().clone();
        status["generation"] = json!(self.generation.load(Ordering::Acquire));
        status["recent_failures"] = json!(self.recent_failures.lock().unwrap().clone());
        status
    }

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
            recent_failures: Mutex::new(VecDeque::new()),
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
        self.state.snapshot()
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

#[derive(Debug, PartialEq)]
pub(crate) enum InputDecision {
    Skip,
    Hold(String),
    Process(Option<solstone_core_transcribe::RetryContext>),
}

pub(crate) fn decide_input(
    root: &Path,
    input: &Path,
    held: &HashSet<PathBuf>,
) -> InputDecision {
    if held.contains(input) {
        return InputDecision::Skip;
    }
    let retry = match embedded_retry_context(root, input) {
        Ok(context) => context,
        Err(error) => return InputDecision::Hold(error),
    };
    if embedded_already_processed(root, input, retry.as_ref()) {
        return InputDecision::Skip;
    }
    match accepted_input(root, input) {
        Ok(true) => InputDecision::Process(retry),
        Ok(false) => InputDecision::Skip,
        Err(error) => InputDecision::Hold(error),
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
    let mut held = HashSet::new();
    let mut retry_generation = state.generation.load(Ordering::Acquire);
    while !state.stop.load(Ordering::Acquire) {
        authority
            .revalidate_canonical_binding()
            .map_err(|e| e.to_string())?;
        let current_generation = state.generation.load(Ordering::Acquire);
        if current_generation != retry_generation {
            held.clear();
            retry_generation = current_generation;
        }
        if state.cancel.load(Ordering::Acquire) {
            let mut status = state.status.lock().unwrap();
            if status["state"] != "failed" {
                status["state"] = json!("cancelled");
            }
        } else {
            let mut day_errors = Vec::new();
            let inputs = discover_audio_files(root, &mut |_| {}, &mut |day, error| {
                day_errors.push(format!("{}: {error}", day.display()))
            })
            .collect::<Vec<_>>();
            let pass_day_error = if !day_errors.is_empty() {
                let err_str = day_errors.join("; ");
                let mut status = state.status.lock().unwrap();
                status["day_error"] = json!(err_str.clone());
                status["error"] = json!(err_str.clone());
                Some(err_str)
            } else {
                None
            };
            let set_status = |mut obj: serde_json::Value| {
                if let Some(ref err) = pass_day_error {
                    obj["day_error"] = json!(err);
                }
                *state.status.lock().unwrap() = obj;
            };
            for input in inputs {
                if state.stop.load(Ordering::Acquire) || state.cancel.load(Ordering::Acquire) {
                    break;
                }
                match decide_input(root, &input, &held) {
                    InputDecision::Skip => continue,
                    InputDecision::Hold(error) => {
                        let generation = state.generation.load(Ordering::Acquire);
                        state.input_failed(generation, &input, &error);
                        held.insert(input.clone());
                        set_status(json!({
                            "state": "failed",
                            "input": input,
                            "error": error,
                            "failed_inputs": held.len(),
                        }));
                        continue;
                    }
                    InputDecision::Process(retry) => {
                        let generation = state.generation.load(Ordering::Acquire);
                        let start = Instant::now();
                        let prior = state.status.lock().unwrap().clone();
                        set_status(json!({
                            "state": "processing",
                            "input": input,
                            "previous": prior.get("last"),
                            "model_load_ms": asr.load_ms.get(),
                        }));
                        let result = process_embedded(
                            &input,
                            root,
                            models,
                            scratch,
                            &asr,
                            &|| state.cancelled(generation),
                            retry.as_ref(),
                        );
                        if state.cancelled(generation) {
                            state.status.lock().unwrap()["state"] = json!("cancelled");
                            break;
                        }
                        match result {
                            Ok(result) if result.outcome == TranscribeOutcome::Skipped => {
                                *state.status.lock().unwrap() = prior;
                            }
                            Ok(result) => {
                                let last = json!({
                                    "input": input,
                                    "outcome": format!("{:?}", result.outcome),
                                    "transcript": result.transcript_path,
                                    "processing_ms": start.elapsed().as_secs_f64() * 1000.,
                                    "failed_inputs": held.len(),
                                });
                                set_status(json!({
                                    "state": "ready",
                                    "model_load_ms": asr.load_ms.get(),
                                    "last": last,
                                }));
                            }
                            Err(error) => {
                                held.insert(input.clone());
                                state.input_failed(generation, &input, &error.to_string());
                                set_status(json!({
                                    "state": "failed",
                                    "input": input,
                                    "error": error.to_string(),
                                    "model_load_ms": asr.load_ms.get(),
                                    "failed_inputs": held.len(),
                                }));
                            }
                        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use solstone_core_callosum::{
        DeviceIngestEvent, DurableEvent, FileDescriptor, append_durable_event,
    };
    use solstone_core_transcribe::TranscribeError;

    fn setup_fixture_audio(
        root: &Path,
        day: &str,
        stream: &str,
        segment_name: &str,
        filename: &str,
        content: &[u8],
    ) -> PathBuf {
        let segment = root.join("chronicle").join(day).join(stream).join(segment_name);
        std::fs::create_dir_all(&segment).unwrap();
        let audio_path = segment.join(filename);
        std::fs::write(&audio_path, content).unwrap();
        let event = DeviceIngestEvent {
            record_type: "device_ingest".into(),
            record_version: 1,
            outcome: "accepted".into(),
            protocol_version: 3,
            cid: "fixture".into(),
            source: "audio".into(),
            stream: stream.into(),
            day: day.into(),
            segment: segment_name.into(),
            meta: Default::default(),
            extra: Default::default(),
            files: vec![FileDescriptor {
                submitted: filename.into(),
                written: filename.into(),
                size: content.len() as u64,
                sha256: format!("{:x}", Sha256::digest(content)),
                extra: Default::default(),
            }],
        };
        append_durable_event(&segment, &DurableEvent::DeviceIngest(event)).unwrap();
        audio_path
    }

    #[test]
    fn foreground_scan_reenters_transient_decode_under_bound() {
        let temp = tempfile::tempdir().unwrap();
        let audio_path = setup_fixture_audio(
            temp.path(),
            "20261012",
            "device_audio",
            "120000_1",
            "audio.wav",
            b"speech",
        );
        let jsonl_path = audio_path.with_extension("jsonl");
        let header = json!({
            "raw": "audio.wav",
            "_solstone_processing": {
                "schema": "solstone.processing.v1",
                "state": "failed",
                "handler": "transcribe",
                "reason_code": "decode_transient",
                "attempts": 1,
            }
        });
        std::fs::write(&jsonl_path, format!("{header}\n")).unwrap();

        let held = HashSet::new();
        let decision = decide_input(temp.path(), &audio_path, &held);
        assert!(matches!(decision, InputDecision::Process(_)));
        assert!(!held.contains(&audio_path));
    }

    #[test]
    fn foreground_scan_skips_analyzed_and_empty_proofs() {
        let temp = tempfile::tempdir().unwrap();
        let audio_path = setup_fixture_audio(
            temp.path(),
            "20261012",
            "device_audio",
            "120000_1",
            "audio.wav",
            b"speech",
        );
        let jsonl_path = audio_path.with_extension("jsonl");

        // Analyzed proof
        let header_analyzed = json!({
            "raw": "audio.wav",
            "_solstone_processing": {
                "schema": "solstone.processing.v1",
                "state": "analyzed",
                "handler": "transcribe",
            }
        });
        std::fs::write(&jsonl_path, format!("{header_analyzed}\n")).unwrap();
        let held = HashSet::new();
        let decision = decide_input(temp.path(), &audio_path, &held);
        assert_eq!(decision, InputDecision::Skip);
        assert!(!held.contains(&audio_path));

        // Empty proof
        let header_empty = json!({
            "raw": "audio.wav",
            "_solstone_processing": {
                "schema": "solstone.processing.v1",
                "state": "empty",
                "handler": "transcribe",
            }
        });
        std::fs::write(&jsonl_path, format!("{header_empty}\n")).unwrap();
        let decision = decide_input(temp.path(), &audio_path, &held);
        assert_eq!(decision, InputDecision::Skip);
        assert!(!held.contains(&audio_path));
    }

    #[test]
    fn foreground_scan_skips_exhausted_failures() {
        let temp = tempfile::tempdir().unwrap();
        let audio_path = setup_fixture_audio(
            temp.path(),
            "20261012",
            "device_audio",
            "120000_1",
            "audio.wav",
            b"speech",
        );
        let jsonl_path = audio_path.with_extension("jsonl");
        let held = HashSet::new();

        // Exhausted by attempts bound (3)
        let header_exhausted = json!({
            "raw": "audio.wav",
            "_solstone_processing": {
                "schema": "solstone.processing.v1",
                "state": "failed",
                "handler": "transcribe",
                "reason_code": "decode_transient",
                "attempts": 3,
            }
        });
        std::fs::write(&jsonl_path, format!("{header_exhausted}\n")).unwrap();
        let decision = decide_input(temp.path(), &audio_path, &held);
        assert_eq!(decision, InputDecision::Skip);
        assert!(!held.contains(&audio_path));

        // Exhausted by corrupt_input
        let header_corrupt = json!({
            "raw": "audio.wav",
            "_solstone_processing": {
                "schema": "solstone.processing.v1",
                "state": "failed",
                "handler": "transcribe",
                "reason_code": "corrupt_input",
                "attempts": 1,
            }
        });
        std::fs::write(&jsonl_path, format!("{header_corrupt}\n")).unwrap();
        let decision = decide_input(temp.path(), &audio_path, &held);
        assert_eq!(decision, InputDecision::Skip);
        assert!(!held.contains(&audio_path));

        // Exhausted by no_audio_stream
        let header_nostream = json!({
            "raw": "audio.wav",
            "_solstone_processing": {
                "schema": "solstone.processing.v1",
                "state": "failed",
                "handler": "transcribe",
                "reason_code": "no_audio_stream",
                "attempts": 1,
            }
        });
        std::fs::write(&jsonl_path, format!("{header_nostream}\n")).unwrap();
        let decision = decide_input(temp.path(), &audio_path, &held);
        assert_eq!(decision, InputDecision::Skip);
        assert!(!held.contains(&audio_path));
    }

    #[test]
    fn foreground_scan_holds_pre_record_custody_and_budget_errors() {
        let temp = tempfile::tempdir().unwrap();
        let segment = temp.path().join("chronicle/20261012/device_audio/120000_1");
        std::fs::create_dir_all(&segment).unwrap();
        let audio_path = segment.join("audio.wav");
        let file = File::create(&audio_path).unwrap();
        file.set_len(16 * 1024 * 1024 + 1).unwrap();

        let state = State {
            stop: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            status: Mutex::new(json!({"state":"ready"})),
            recent_failures: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
        };
        let mut held = HashSet::new();

        // Pre-record custody / oversize error
        let decision = decide_input(temp.path(), &audio_path, &held);
        let error_msg = match decision {
            InputDecision::Hold(msg) => msg,
            other => panic!("expected Hold, got {other:?}"),
        };
        state.input_failed(0, &audio_path, &error_msg);
        held.insert(audio_path.clone());
        *state.status.lock().unwrap() = json!({
            "state": "failed",
            "input": audio_path,
            "error": error_msg,
            "failed_inputs": held.len(),
        });

        assert!(held.contains(&audio_path));
        let snapshot = state.snapshot();
        assert_eq!(snapshot["state"], "failed");
        assert_eq!(snapshot["recent_failures"].as_array().unwrap().len(), 1);

        // Later scan in same generation skips held input
        let next = decide_input(temp.path(), &audio_path, &held);
        assert_eq!(next, InputDecision::Skip);

        // Budget error from process_embedded
        let audio_path2 = segment.join("audio2.wav");
        std::fs::write(&audio_path2, b"speech2").unwrap();
        let budget_err = TranscribeError::ParakeetCppDeferred {
            reason: "embedded-duration-budget".into(),
            detail: "budget exceeded".into(),
        };
        held.insert(audio_path2.clone());
        state.input_failed(0, &audio_path2, &budget_err.to_string());
        assert!(held.contains(&audio_path2));
        assert_eq!(decide_input(temp.path(), &audio_path2, &held), InputDecision::Skip);
    }

    #[test]
    fn foreground_scan_day_discovery_error_does_not_abort_other_days() {
        let temp = tempfile::tempdir().unwrap();
        let valid_audio = setup_fixture_audio(
            temp.path(),
            "20261012",
            "device_audio",
            "120000_1",
            "audio.wav",
            b"speech",
        );
        let bad_day = temp.path().join("chronicle/20261011");
        std::fs::create_dir_all(&bad_day).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bad_day, std::fs::Permissions::from_mode(0o000)).unwrap();
        }

        let mut day_errors = Vec::new();
        let inputs = discover_audio_files(temp.path(), &mut |_| {}, &mut |day, error| {
            day_errors.push(format!("{}: {error}", day.display()));
        })
        .collect::<Vec<_>>();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bad_day, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        assert!(!day_errors.is_empty());
        assert_eq!(inputs, vec![valid_audio]);

        let state = State {
            stop: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            status: Mutex::new(json!({"state":"ready"})),
            recent_failures: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
        };
        let err_str = day_errors.join("; ");
        let mut status = state.status.lock().unwrap();
        status["day_error"] = json!(err_str.clone());
        status["error"] = json!(err_str);
        drop(status);

        let snap = state.snapshot();
        assert_eq!(snap["day_error"], json!(err_str));
        assert_eq!(snap["state"], "ready");
    }

    #[test]
    fn foreground_scan_binding_failure_aborts_run() {
        let state = State {
            stop: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            status: Mutex::new(json!({"state":"ready"})),
            recent_failures: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
        };
        assert!(run(
            &state,
            Path::new("/nonexistent/journal/path"),
            Path::new("unused"),
            Path::new("unused"),
            Path::new("unused"),
        )
        .is_err());
    }

    #[test]
    fn resume_cannot_erase_cancellation_of_an_active_inference() {
        let state = State {
            stop: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            status: Mutex::new(json!({})),
            recent_failures: Mutex::new(VecDeque::new()),
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
    fn input_failure_survives_headline_replacement_and_resume_identifies_old_attempt() {
        let worker = AudioWorker {
            state: Arc::new(State {
                stop: AtomicBool::new(false),
                cancel: AtomicBool::new(false),
                generation: AtomicU64::new(7),
                status: Mutex::new(json!({"state":"processing"})),
                recent_failures: Mutex::new(VecDeque::new()),
                wake: Condvar::new(),
            }),
            thread: None,
        };
        worker.state.input_failed(
            7,
            Path::new("target/audio.wav"),
            "size differs from pinned model",
        );
        worker.state.input_failed(
            7,
            Path::new("older/audio.m4a"),
            "duration exceeds 30 seconds",
        );
        *worker.state.status.lock().unwrap() = json!({"state":"failed","input":"older/audio.m4a","error":"duration exceeds 30 seconds"});
        let snapshot = worker.status();
        assert_eq!(snapshot["input"], "older/audio.m4a");
        assert_eq!(snapshot["generation"], 7);
        assert_eq!(snapshot["recent_failures"][0]["input"], "target/audio.wav");
        assert_eq!(
            snapshot["recent_failures"][0]["error"],
            "size differs from pinned model"
        );
        worker.resume();
        let resumed = worker.status();
        assert_eq!(resumed["generation"], 8);
        assert_eq!(resumed["recent_failures"][0]["generation"], 7);
        for index in 0..70 {
            worker.state.input_failed(
                8,
                Path::new(&format!("input-{index}/audio.wav")),
                "fixture failure",
            );
        }
        let bounded = worker.status();
        let history = bounded["recent_failures"].as_array().unwrap();
        assert_eq!(history.len(), 64);
        assert_eq!(history[0]["input"], "input-6/audio.wav");
        assert_eq!(history[63]["input"], "input-69/audio.wav");
        assert!(history.iter().all(|row| row["generation"] == 8));
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
