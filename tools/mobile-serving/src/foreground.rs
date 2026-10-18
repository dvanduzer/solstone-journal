// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! App-owned processing using the maintained discovery, Sense, think and schedule owners.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use serde_json::{Map, Value};
use solstone_core_cortex::{CallableCortex, CallableRoots};
use solstone_core_cortex_client::{
    CortexRequest, CortexRequestClient, CortexRequestPolicy, WaitForUsesReport,
};
use solstone_core_depict::WireClient;
use solstone_core_generate::{ClientError, GenerateExecutor, GenerateRequest, GenerateResponse};
use solstone_core_indexer_store::scan::RescanFileStatus;
use solstone_core_journal_config::work_priority::{WorkBand, WorkPriority};
use solstone_core_journal_io::JournalRoot;
use solstone_core_sense::{SenseExecutor, SenseJobResult};
use solstone_core_system::cap::DefaultCapResolver;
use solstone_core_system::process::ChildLaunchContext;
use solstone_core_system::queue::{
    CallableQueueWorker, CallableTaskContext, ProcessState, ProcessStateProbe, TaskQueue,
    TaskQueueEvent, TaskQueueEventSink, TaskQueueOptions,
};
use solstone_core_system::request::{ExecutionRequest, ScheduledRequest};
use solstone_core_system::schedule::{
    CheckReport, ScheduleEngine, ScheduleError, ScheduleNow, ScheduleSubmissionSink,
};
use solstone_core_think_cli::planner::{
    FlushState, PendingFlush, WorkDiscovery, check_activity_settle, check_segment_flush,
    day_still_closing,
};
use solstone_core_think_cli::{CortexBoundary, DispatchFailure, IndexBoundary, ThinkContext};

static HOST_ACTIVE: AtomicBool = AtomicBool::new(false);
type Checkpoint = Arc<dyn Fn() -> bool + Send + Sync>;

struct NoProcesses;
impl ProcessStateProbe for NoProcesses {
    fn state(&self, _: u32) -> ProcessState {
        panic!("a callable queue must not launch a child process")
    }
}

pub struct ProductionGenerateExecutor {
    root: Arc<JournalRoot>,
    cancelled: Checkpoint,
    priority: WorkPriority,
}
impl GenerateExecutor for ProductionGenerateExecutor {
    fn execute(&self, request: &GenerateRequest) -> Result<GenerateResponse, ClientError> {
        let cancellation = solstone_core_generate::Cancellation::from_fn(self.cancelled.as_ref());
        let runtime = solstone_core_generate_wire::EndpointRuntime::default();
        solstone_core_generate_wire::respond_with_context(
            request,
            &runtime,
            self.root.canonical_path(),
            |_| None,
            &solstone_core_generate_wire::GenerateCallContext {
                priority: self.priority,
                cancellation: &cancellation,
            },
        )
        .map_err(ClientError::Decode)
    }
}

#[derive(Clone)]
struct HostGenerator {
    root: Arc<JournalRoot>,
    injected: Option<Arc<dyn GenerateExecutor + Send + Sync>>,
}
impl HostGenerator {
    fn executor(
        &self,
        cancelled: Checkpoint,
        priority: WorkPriority,
    ) -> Arc<dyn GenerateExecutor + Send + Sync> {
        self.injected.clone().unwrap_or_else(|| {
            Arc::new(ProductionGenerateExecutor {
                root: Arc::clone(&self.root),
                cancelled,
                priority,
            })
        })
    }
}

struct HostWireClient<'a> {
    generator: &'a HostGenerator,
    checkpoint: &'a (dyn Fn() -> bool + Send + Sync),
    priority: WorkPriority,
}
impl WireClient for HostWireClient<'_> {
    fn execute(&self, request: &GenerateRequest) -> Result<GenerateResponse, ClientError> {
        if (self.checkpoint)() {
            return Ok(GenerateResponse::Cancelled);
        }
        if let Some(executor) = self.generator.injected.as_ref() {
            return executor.execute(request);
        }
        let cancellation = solstone_core_generate::Cancellation::from_fn(self.checkpoint);
        solstone_core_generate_wire::respond_with_context(
            request,
            &solstone_core_generate_wire::EndpointRuntime::default(),
            self.generator.root.canonical_path(),
            |_| None,
            &solstone_core_generate_wire::GenerateCallContext {
                priority: self.priority,
                cancellation: &cancellation,
            },
        )
        .map_err(ClientError::Decode)
    }
}
struct NoDetector;
impl solstone_core_depict::Detector for NoDetector {
    fn detect(&self, _: &[u8]) -> Result<Option<Value>, String> {
        Ok(None)
    }
}
struct HostIndexBoundary {
    cancelled: Checkpoint,
}
impl IndexBoundary for HostIndexBoundary {
    fn rescan_file(&self, journal: &Path, path: &Path) -> Result<RescanFileStatus, String> {
        if (self.cancelled)() {
            return Err("thinking cancelled".into());
        }
        solstone_core_indexer_store::scan::rescan_file(journal, path).map_err(|e| e.to_string())
    }
}

struct HostCortexBoundary {
    journal: PathBuf,
    talent_root: PathBuf,
    apps_root: PathBuf,
    templates_dir: PathBuf,
    generator: Arc<dyn GenerateExecutor + Send + Sync>,
    client: CortexRequestClient,
    cancelled: Checkpoint,
    owner: Arc<CallableCortex>,
}
impl CortexBoundary for HostCortexBoundary {
    fn dispatch(
        &self,
        runtime: &tokio::runtime::Runtime,
        request: &CortexRequest,
    ) -> Result<String, DispatchFailure> {
        self.dispatch_prepared(runtime, request, None, &mut |_| Ok(()))
    }
    fn dispatch_prepared(
        &self,
        _: &tokio::runtime::Runtime,
        request: &CortexRequest,
        reserved: Option<&str>,
        prepare: &mut (dyn FnMut(&str) -> std::io::Result<()> + Send),
    ) -> Result<String, DispatchFailure> {
        if (self.cancelled)() {
            return Err(DispatchFailure::Unavailable);
        }
        let now_ms = now_ms();
        let use_id = match reserved {
            Some(id) => id.to_owned(),
            None => {
                solstone_core_journal_io::cortex_use::allocate_cortex_use_id(&self.journal, now_ms)
                    .map_err(|_| DispatchFailure::Unavailable)?
                    .to_string()
            }
        };
        prepare(&use_id).map_err(|_| DispatchFailure::Unavailable)?;
        if (self.cancelled)() {
            return Err(DispatchFailure::Unavailable);
        }
        let mut extra = Map::new();
        extra.insert("prompt".into(), Value::String(request.prompt.clone()));
        extra.extend(request.config.clone());
        extra.insert("name".into(), Value::String(request.name.clone()));
        extra.insert("use_id".into(), Value::String(use_id.clone()));
        let outcome = self
            .owner
            .execute(
                CallableRoots {
                    talent_root: &self.talent_root,
                    apps_root: &self.apps_root,
                    templates_dir: &self.templates_dir,
                },
                extra,
                self.generator.as_ref(),
                now_ms,
                self.cancelled.as_ref(),
            )
            .map_err(|_| DispatchFailure::Unavailable)?;
        if !outcome.completed || (self.cancelled)() {
            return Err(DispatchFailure::Unavailable);
        }
        Ok(use_id)
    }
    fn wait(
        &self,
        _: &tokio::runtime::Runtime,
        ids: &[String],
        _: Option<Duration>,
    ) -> Result<WaitForUsesReport, String> {
        if (self.cancelled)() {
            return Err("thinking cancelled".into());
        }
        self.client
            .wait_for_uses_from_disk(ids)
            .map_err(|e| format!("cortex wait failed: {e:?}"))
    }
}

struct HostScheduleSink {
    queue: TaskQueue,
}
impl ScheduleSubmissionSink for HostScheduleSink {
    fn submit(&self, request: ScheduledRequest) -> bool {
        !matches!(
            self.queue.submit(ExecutionRequest::Scheduled(request)),
            solstone_core_system::queue::SubmitOutcome::Rejected
        )
    }
}
struct HostQueueSink {
    schedule_engine: Arc<Mutex<ScheduleEngine>>,
    flush: Arc<Mutex<FlushState>>,
    cancelled: Checkpoint,
}
impl TaskQueueEventSink for HostQueueSink {
    fn emit(&self, event: TaskQueueEvent) {
        if (self.cancelled)() {
            return;
        }
        if let TaskQueueEvent::Stopped {
            reference,
            exit_code,
            command,
            ..
        } = event
        {
            if exit_code == 0
                && let Some(args) = admit_argv(&command)
                && !args
                    .iter()
                    .any(|arg| matches!(arg.as_str(), "--flush" | "--settle"))
            {
                let value = |flag: &str| {
                    args.windows(2)
                        .find(|pair| pair[0] == flag)
                        .map(|pair| pair[1].clone())
                };
                if let (Some(day), Some(segment)) = (value("--day"), value("--segment"))
                    && let Ok(mut flush) = self.flush.lock()
                    && !(self.cancelled)()
                {
                    let stream = value("--stream");
                    if flush
                        .pending
                        .get(&stream)
                        .is_none_or(|pending| (&day, &segment) >= (&pending.day, &pending.segment))
                    {
                        flush.pending.insert(
                            stream,
                            PendingFlush {
                                last_segment_ts: Instant::now(),
                                day,
                                segment,
                            },
                        );
                    }
                }
            }
            let parts = reference.split(':').collect::<Vec<_>>();
            if parts.len() >= 3
                && parts[0] == "sched"
                && let Ok(engine) = self.schedule_engine.lock()
            {
                if (self.cancelled)() {
                    return;
                }
                let ended_at = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0., |d| d.as_secs_f64());
                let _ = engine.record_completion(
                    parts[1],
                    ended_at,
                    if exit_code == 0 { "success" } else { "failed" },
                    &reference,
                );
            }
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}
fn admit_argv(args: &[String]) -> Option<Vec<String>> {
    let mut args = args.to_vec();
    while args
        .first()
        .is_some_and(|s| s == "solstone" || s == "journal")
    {
        args.remove(0);
    }
    if args.first().map(String::as_str) != Some("think") {
        return None;
    }
    args.remove(0);
    if args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--weekly" | "--daily-only" | "--refresh" | "--from-scratch" | "--segments"
        )
    }) {
        return None;
    }
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    let value = |flag: &str| {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|i| args.get(i + 1))
    };
    if has("--cadence")
        || (has("--settle") && value("--stream").is_some())
        || (has("--segment") && value("--day").is_some() && value("--stream").is_some())
        || (has("--activity") && value("--day").is_some() && value("--facet").is_some())
        || (value("--retry-unit").is_some_and(|v| !v.is_empty())
            && value("--retry-cause").is_some()
            && value("--day").is_some())
    {
        Some(args)
    } else {
        None
    }
}
fn admit_command(journal: &Path, raw: &[String]) -> bool {
    let Some(args) = admit_argv(raw) else {
        return false;
    };
    if let Some(i) = args.iter().position(|arg| arg == "--retry-unit") {
        let Some(id) = args.get(i + 1) else {
            return false;
        };
        return matches!(
            solstone_core_system::work_retry::read(journal, id),
            Ok(Some(record)) if !matches!(record.unit, solstone_core_system::work_retry::Unit::Daily { .. })
        );
    }
    true
}

#[cfg(feature = "audio")]
#[derive(Clone)]
struct AudioConfig {
    worker: Arc<crate::audio::AudioWorker>,
}

struct HostSenseExecutor {
    journal: PathBuf,
    generator: HostGenerator,
    admitted_priority: Option<WorkPriority>,
    cancelled: Checkpoint,
    failed: Arc<AtomicBool>,
    #[cfg(feature = "audio")]
    audio_state: Arc<Mutex<Option<AudioConfig>>>,
}
impl SenseExecutor for HostSenseExecutor {
    fn cancelled(&self) -> bool {
        (self.cancelled)()
    }
    fn execute(
        &self,
        handler: &str,
        input: &Path,
        context: &solstone_core_sense::work::SegmentContext,
        attempt: Option<&solstone_core_system::work_retry::Attempt>,
        stop: &AtomicBool,
    ) -> SenseJobResult {
        let checkpoint = || {
            (self.cancelled)()
                || stop.load(Ordering::Acquire)
                || (handler == "depict"
                    && !crate::custody::accepted_input(&self.journal, input).unwrap_or(false))
        };
        if checkpoint() || !solstone_core_system::processing::processing_allowed(&self.journal) {
            return SenseJobResult::Hold;
        }
        let mut priority = self.admitted_priority.unwrap_or_else(|| {
            solstone_core_system::priority::classify(
                &[
                    "journal".into(),
                    "think".into(),
                    "--day".into(),
                    context.key.day.clone(),
                    "--segment".into(),
                    context.key.segment.clone(),
                ],
                Utc::now()
                    .with_timezone(&solstone_core_journal_config::owner_zone(&self.journal))
                    .date_naive(),
            )
        });
        if self.admitted_priority.is_none()
            && let Some(attempt) = attempt
        {
            priority.band = if attempt.cause.starts_with("owner:") {
                WorkBand::Waiting
            } else {
                WorkBand::Completing
            };
        }
        if handler == "depict"
            && !solstone_core_system::work_policy::WorkPolicy::read(&self.journal)
                .is_ok_and(|policy| policy.permits(priority.band))
        {
            return SenseJobResult::Hold;
        }
        let result = match handler {
            "depict" => match solstone_core_depict::run_with_clients_and_checkpoint(
                input,
                false,
                &HostWireClient {
                    generator: &self.generator,
                    checkpoint: &checkpoint,
                    priority,
                },
                &NoDetector,
                &checkpoint,
                attempt,
            ) {
                Ok(solstone_core_depict::RunOutcome::Written) => SenseJobResult::Finished(0),
                Ok(_) => SenseJobResult::Hold,
                Err(_) => SenseJobResult::Finished(1),
            },
            "transcribe" => {
                #[cfg(feature = "audio")]
                {
                    let config = self.audio_state.lock().unwrap().clone();
                    match config {
                        Some(config) => {
                            match config
                                .worker
                                .process_host_input(input, attempt, &checkpoint)
                            {
                                Ok(true) => SenseJobResult::Finished(0),
                                Ok(false) => SenseJobResult::Hold,
                                Err(_) => SenseJobResult::Finished(1),
                            }
                        }
                        None => SenseJobResult::Hold,
                    }
                }
                #[cfg(not(feature = "audio"))]
                SenseJobResult::Hold
            }
            _ => SenseJobResult::Hold,
        };
        if checkpoint() {
            SenseJobResult::Hold
        } else {
            result
        }
    }
}

struct HostedCommand {
    journal: PathBuf,
    talent_root: PathBuf,
    apps_root: PathBuf,
    templates_dir: PathBuf,
    generator: HostGenerator,
    cancelled: Checkpoint,
    cortex_owner: Arc<CallableCortex>,
    failed: Arc<AtomicBool>,
    #[cfg(feature = "audio")]
    audio_state: Arc<Mutex<Option<AudioConfig>>>,
}
pub struct ForegroundProcessingHost {
    journal: PathBuf,
    root: Arc<JournalRoot>,
    queue: TaskQueue,
    schedule_engine: Arc<Mutex<ScheduleEngine>>,
    flush: Arc<Mutex<FlushState>>,
    discovery: Mutex<Option<WorkDiscovery>>,
    sense_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    schedule_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    stop: Arc<AtomicBool>,
    cancelled: Checkpoint,
    failed: Arc<AtomicBool>,
    generator: HostGenerator,
    #[cfg(test)]
    worker: CallableQueueWorker,
    #[cfg(feature = "audio")]
    audio_state: Arc<Mutex<Option<AudioConfig>>>,
}

impl ForegroundProcessingHost {
    pub fn open(journal: &Path, payload: &Path) -> Result<Self, String> {
        let now = ScheduleNow::in_zone(
            Utc::now(),
            solstone_core_journal_config::owner_zone(journal),
        );
        Self::open_internal(journal, payload, now, None)
    }
    pub fn open_at(journal: &Path, payload: &Path, now: ScheduleNow) -> Result<Self, String> {
        Self::open_internal(journal, payload, now, None)
    }
    pub fn open_with_executor(
        journal: &Path,
        payload: &Path,
        now: ScheduleNow,
        executor: Arc<dyn GenerateExecutor + Send + Sync>,
    ) -> Result<Self, String> {
        Self::open_internal(journal, payload, now, Some(executor))
    }
    fn open_internal(
        journal: &Path,
        payload: &Path,
        now: ScheduleNow,
        executor: Option<Arc<dyn GenerateExecutor + Send + Sync>>,
    ) -> Result<Self, String> {
        let root = Arc::new(JournalRoot::open(journal).map_err(|e| e.to_string())?);
        Self::open_bound(
            root,
            payload,
            now,
            executor,
            Arc::new(|| false),
            Arc::new(AtomicBool::new(false)),
        )
    }
    pub fn open_with_cancellation(
        root: Arc<JournalRoot>,
        payload: &Path,
        external: Checkpoint,
        stop: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let now = ScheduleNow::in_zone(
            Utc::now(),
            solstone_core_journal_config::owner_zone(root.canonical_path()),
        );
        Self::open_bound(root, payload, now, None, external, stop)
    }
    fn open_bound(
        root: Arc<JournalRoot>,
        payload: &Path,
        now: ScheduleNow,
        executor: Option<Arc<dyn GenerateExecutor + Send + Sync>>,
        external: Checkpoint,
        stop: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        if HOST_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("foreground processing host already active".into());
        }
        let result = (|| {
            root.revalidate_canonical_binding()
                .map_err(|e| e.to_string())?;
            let journal = root.canonical_path().to_path_buf();
            let failed = Arc::new(AtomicBool::new(false));
            let cancelled: Checkpoint = {
                let stop = Arc::clone(&stop);
                let failed = Arc::clone(&failed);
                let root = Arc::clone(&root);
                Arc::new(move || {
                    if failed.load(Ordering::Acquire)
                        || external()
                        || root.revalidate_canonical_binding().is_err()
                    {
                        stop.store(true, Ordering::Release);
                    }
                    stop.load(Ordering::Acquire)
                })
            };
            if cancelled() {
                return Err("thinking cancelled".into());
            }
            // Recovery is a startup action, before any callable use can be live.
            let cortex_owner = Arc::new(CallableCortex::open(&journal)?);
            let generator = HostGenerator {
                root: Arc::clone(&root),
                injected: executor,
            };
            let config = journal.join("config/schedules.json");
            let state = journal.join("health/scheduler.json");
            solstone_core_system::schedule::initialize_schedule_config(&config)
                .map_err(|e| e.to_string())?;
            let (mut engine, _) =
                ScheduleEngine::init(&config, &state, now).map_err(|e| e.to_string())?;
            engine.register_defaults().map_err(|e| e.to_string())?;
            let schedule_engine = Arc::new(Mutex::new(engine));
            let flush = Arc::new(Mutex::new(FlushState::default()));
            #[cfg(feature = "audio")]
            let audio_state = Arc::new(Mutex::new(None));
            let hosted = Arc::new(HostedCommand {
                journal: journal.clone(),
                talent_root: payload.join("solstone/talent"),
                apps_root: payload.join("solstone/apps"),
                templates_dir: payload.join("solstone/think/templates"),
                generator: generator.clone(),
                cancelled: Arc::clone(&cancelled),
                cortex_owner,
                failed: Arc::clone(&failed),
                #[cfg(feature = "audio")]
                audio_state: Arc::clone(&audio_state),
            });
            let worker: CallableQueueWorker =
                Arc::new(move |args, stop| Self::execute_command(&hosted, args, stop));
            let admitted_journal = journal.clone();
            let admitted_cancelled = Arc::clone(&cancelled);
            let queue = TaskQueue::with_callable_worker_and_admission(
                TaskQueueOptions {
                    #[cfg(windows)]
                    read_file_grants: Vec::new(),
                    journal_root: journal.clone(),
                    cap_resolver: Arc::new(DefaultCapResolver::default()),
                    process_state_probe: Arc::new(NoProcesses),
                    queue_sink: Some(Arc::new(HostQueueSink {
                        schedule_engine: Arc::clone(&schedule_engine),
                        flush: Arc::clone(&flush),
                        cancelled: Arc::clone(&cancelled),
                    })),
                    process_sink: None,
                    ready: true,
                    before_deadline_commit: None,
                    child_environment: BTreeMap::new(),
                    task_binary: None,
                },
                Arc::clone(&worker),
                Arc::new(move |args| {
                    !admitted_cancelled() && admit_command(&admitted_journal, args)
                }),
            );
            let discovery_checkpoint = Arc::clone(&cancelled);
            let discovery = WorkDiscovery::new_with_talent_roots_and_checkpoint(
                journal.clone(),
                queue.clone(),
                true,
                solstone_core_think_cli::planner::DiscoveryTalentRoots {
                    talent: payload.join("solstone/talent"),
                    apps: payload.join("solstone/apps"),
                },
                Arc::new(move || {
                    if discovery_checkpoint() {
                        Err("thinking cancelled".into())
                    } else {
                        Ok(())
                    }
                }),
            );
            Ok(Self {
                journal,
                root,
                queue,
                schedule_engine,
                flush,
                discovery: Mutex::new(Some(discovery)),
                sense_handle: Mutex::new(None),
                schedule_handle: Mutex::new(None),
                stop,
                cancelled,
                failed,
                generator,
                #[cfg(test)]
                worker,
                #[cfg(feature = "audio")]
                audio_state,
            })
        })();
        if result.is_err() {
            HOST_ACTIVE.store(false, Ordering::Release);
        }
        result
    }
    fn execute_command(
        hosted: &HostedCommand,
        raw: &[String],
        control: CallableTaskContext,
    ) -> i32 {
        if !admit_command(&hosted.journal, raw) || (hosted.cancelled)() || control.is_cancelled() {
            return 1;
        }
        let args = admit_argv(raw).expect("admitted command");
        let value = |flag: &str| {
            args.iter()
                .position(|arg| arg == flag)
                .and_then(|i| args.get(i + 1))
                .cloned()
        };
        let day = value("--day").unwrap_or_else(|| {
            Utc::now()
                .with_timezone(&solstone_core_journal_config::owner_zone(&hosted.journal))
                .format("%Y%m%d")
                .to_string()
        });
        let cancelled: Checkpoint = {
            let host = Arc::clone(&hosted.cancelled);
            let control = control.clone();
            Arc::new(move || control.is_cancelled() || host())
        };
        let cortex = Arc::new(HostCortexBoundary {
            journal: hosted.journal.clone(),
            talent_root: hosted.talent_root.clone(),
            apps_root: hosted.apps_root.clone(),
            templates_dir: hosted.templates_dir.clone(),
            generator: hosted
                .generator
                .executor(Arc::clone(&cancelled), control.priority),
            client: CortexRequestClient::new(&hosted.journal, CortexRequestPolicy::think()),
            cancelled: Arc::clone(&cancelled),
            owner: Arc::clone(&hosted.cortex_owner),
        });
        let executor = Arc::new(HostSenseExecutor {
            journal: hosted.journal.clone(),
            generator: hosted.generator.clone(),
            admitted_priority: Some(control.priority),
            cancelled: Arc::clone(&cancelled),
            failed: Arc::clone(&hosted.failed),
            #[cfg(feature = "audio")]
            audio_state: Arc::clone(&hosted.audio_state),
        });
        let context = ThinkContext::hosted(
            &hosted.journal,
            day.clone(),
            hosted.journal.join("chronicle").join(&day),
            now_ms(),
            hosted.talent_root.clone(),
            hosted.apps_root.clone(),
            cortex,
            Arc::new(HostIndexBoundary {
                cancelled: Arc::clone(&cancelled),
            }),
            Arc::new(now_ms),
            None,
            cancelled,
        )
        .with_sense_executor(executor);
        solstone_core_think_cli::run_hosted_command(&context, &args, &ChildLaunchContext::default())
            .exit_code
    }
    #[cfg(test)]
    pub fn execute_worker_command(&self, args: &[String]) -> i32 {
        (self.worker)(
            args,
            CallableTaskContext {
                cancelled: Arc::clone(&self.stop),
                priority: solstone_core_system::priority::classify(
                    args,
                    Utc::now()
                        .with_timezone(&solstone_core_journal_config::owner_zone(&self.journal))
                        .date_naive(),
                ),
                deadline: Instant::now() + Duration::from_secs(480),
            },
        )
    }
    fn executor(&self) -> Arc<HostSenseExecutor> {
        Arc::new(HostSenseExecutor {
            journal: self.journal.clone(),
            generator: self.generator.clone(),
            admitted_priority: None,
            cancelled: Arc::clone(&self.cancelled),
            failed: Arc::clone(&self.failed),
            #[cfg(feature = "audio")]
            audio_state: Arc::clone(&self.audio_state),
        })
    }
    pub fn start_workers(&mut self) -> Result<(), String> {
        if self.sense_handle.lock().unwrap().is_some()
            || self.schedule_handle.lock().unwrap().is_some()
        {
            return Err("foreground workers already active".into());
        }
        if let Some(discovery) = self.discovery.lock().unwrap().as_ref() {
            discovery.request();
        }
        let journal = self.journal.clone();
        let executor = self.executor();
        let cancelled = Arc::clone(&self.cancelled);
        let sense = std::thread::Builder::new()
            .name("journal-sense".into())
            .spawn(move || {
                while !cancelled() {
                    if let Err(error) = Self::sense_pass(&journal, executor.clone()) {
                        log::warn!("foreground Sense: {error}");
                    }
                    if !cancelled() {
                        std::thread::park_timeout(Duration::from_secs(2));
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        *self.sense_handle.lock().unwrap() = Some(sense);
        let engine = Arc::clone(&self.schedule_engine);
        let queue = self.queue.clone();
        let journal = self.journal.clone();
        let cancelled = Arc::clone(&self.cancelled);
        let flush = Arc::clone(&self.flush);
        let schedule = std::thread::Builder::new()
            .name("journal-schedule".into())
            .spawn(move || {
                let mut last_settle_scan = Instant::now();
                while !cancelled() {
                    let tick = Instant::now();
                    queue.resume_processing();
                    let today = Utc::now()
                        .with_timezone(&solstone_core_journal_config::owner_zone(&journal))
                        .format("%Y%m%d")
                        .to_string();
                    if let Ok(mut flush) = flush.lock()
                        && !cancelled()
                    {
                        let force = day_still_closing(&journal, &queue, SystemTime::now());
                        check_segment_flush(&journal, &queue, &mut flush, force.as_deref(), tick);
                    }
                    if !cancelled() {
                        check_activity_settle(
                            &journal,
                            &queue,
                            &mut last_settle_scan,
                            tick,
                            now_ms(),
                            &today,
                        );
                    }
                    if let Ok(mut engine) = engine.lock() {
                        if cancelled() {
                            break;
                        }
                        let now = ScheduleNow::in_zone(
                            Utc::now(),
                            solstone_core_journal_config::owner_zone(&journal),
                        );
                        if let Err(error) = engine.check(
                            now,
                            &HostScheduleSink {
                                queue: queue.clone(),
                            },
                        ) {
                            log::warn!("foreground schedule: {error}");
                        }
                    }
                    if !cancelled() {
                        std::thread::park_timeout(Duration::from_secs(1));
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        *self.schedule_handle.lock().unwrap() = Some(schedule);
        Ok(())
    }
    pub fn check_schedule(&self, now: ScheduleNow) -> Result<CheckReport, ScheduleError> {
        self.schedule_engine.lock().unwrap().check(
            now,
            &HostScheduleSink {
                queue: self.queue.clone(),
            },
        )
    }
    pub fn queue(&self) -> &TaskQueue {
        &self.queue
    }
    #[cfg(feature = "audio")]
    pub fn enable_audio(
        &self,
        models: PathBuf,
        model: PathBuf,
        scratch: PathBuf,
    ) -> Result<(), String> {
        if (self.cancelled)() {
            return Err("thinking cancelled".into());
        }
        let mut state = self.audio_state.lock().unwrap();
        if (self.cancelled)() {
            return Err("thinking cancelled".into());
        }
        if state.is_none() {
            *state = Some(AudioConfig {
                worker: Arc::new(crate::audio::AudioWorker::for_host(
                    Arc::clone(&self.root),
                    models,
                    model,
                    scratch,
                    Arc::clone(&self.cancelled),
                )),
            });
        }
        if let Some(handle) = self.sense_handle.lock().unwrap().as_ref() {
            handle.thread().unpark();
        }
        Ok(())
    }
    #[cfg(feature = "audio")]
    pub fn cancel_audio(&self) {
        if let Some(config) = self.audio_state.lock().unwrap().as_ref() {
            config.worker.cancel();
        }
    }
    #[cfg(feature = "audio")]
    pub fn resume_audio(&self) {
        if let Some(config) = self.audio_state.lock().unwrap().as_ref() {
            config.worker.resume();
        }
    }
    #[cfg(feature = "audio")]
    pub fn audio_status(&self) -> Value {
        self.audio_state
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.worker.status())
            .unwrap_or_else(|| serde_json::json!({"state":"disabled"}))
    }
    pub fn run_sense_pass(&self) -> Result<(), String> {
        Self::sense_pass(&self.journal, self.executor())
    }
    fn sense_pass(journal: &Path, executor: Arc<HostSenseExecutor>) -> Result<(), String> {
        if executor.cancelled() {
            return Ok(());
        }
        let chronicle = journal.join("chronicle");
        if !chronicle.exists() {
            return Ok(());
        }
        let mut days = fs::read_dir(&chronicle)
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        days.sort_by_key(|e| e.file_name());
        for day in days {
            if executor.cancelled() {
                break;
            }
            if !day.file_type().map_err(|e| e.to_string())?.is_dir() {
                continue;
            }
            let day = day.file_name().to_string_lossy().into_owned();
            let request = solstone_core_sense::batch::BatchRequest {
                day,
                jobs: 1,
                reprocess: None,
                segment: None,
                stream: None,
                dry_run: false,
                verbose: false,
                debug: false,
            };
            match solstone_core_sense::batch::run_batch_with_executor(
                journal,
                &request,
                &ChildLaunchContext::default(),
                None,
                executor.clone(),
            ) {
                Ok(()) | Err(solstone_core_sense::batch::BatchError::LeaseContended { .. }) => {}
                Err(solstone_core_sense::batch::BatchError::WorkerJoin) => {
                    executor.failed.store(true, Ordering::Release);
                    return Err("Sense worker did not stop cleanly".into());
                }
                Err(error) => log::warn!("foreground Sense day: {error}"),
            }
        }
        Ok(())
    }
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire) || self.queue.callable_workers_failed()
    }
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(discovery) = self.discovery.lock().unwrap().as_ref() {
            discovery.stop();
        }
        for slot in [&self.sense_handle, &self.schedule_handle] {
            if let Some(handle) = slot.lock().unwrap().as_ref() {
                handle.thread().unpark();
            }
        }
    }
    pub fn shutdown(&self) -> Result<(), String> {
        self.request_stop();
        let mut failed = false;
        if let Some(mut discovery) = self.discovery.lock().unwrap().take() {
            failed |= discovery.shutdown_checked().is_err();
        }
        let _ = self.queue.shutdown();
        failed |= self.queue.join_callable_workers().is_err();
        for slot in [&self.sense_handle, &self.schedule_handle] {
            if let Some(handle) = slot.lock().unwrap().take() {
                failed |= handle.join().is_err();
            }
        }
        if failed || self.failed() {
            return Err("foreground worker failed to join".into());
        }
        HOST_ACTIVE.store(false, Ordering::Release);
        Ok(())
    }
}
impl Drop for ForegroundProcessingHost {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(all(test, feature = "processing", feature = "full-tests"))]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use solstone_core_generate::GeneratedResponse;
    use solstone_core_system::work_retry::{self, Unit};

    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    fn write_image(path: &Path) {
        use sha2::{Digest, Sha256};
        use solstone_core_callosum::{
            DeviceIngestEvent, DurableEvent, FileDescriptor, append_durable_event,
        };
        fs::write(path, TINY_PNG).unwrap();
        let segment = path.parent().unwrap();
        let stream = segment.parent().unwrap();
        let day = stream.parent().unwrap();
        let filename = path.file_name().unwrap().to_str().unwrap();
        append_durable_event(
            segment,
            &DurableEvent::DeviceIngest(DeviceIngestEvent {
                record_type: "device_ingest".into(),
                record_version: 1,
                outcome: "accepted".into(),
                protocol_version: 3,
                cid: "fixture".into(),
                source: "image".into(),
                day: day.file_name().unwrap().to_str().unwrap().into(),
                stream: stream.file_name().unwrap().to_str().unwrap().into(),
                segment: segment.file_name().unwrap().to_str().unwrap().into(),
                meta: Default::default(),
                extra: Default::default(),
                files: vec![FileDescriptor {
                    submitted: filename.into(),
                    written: filename.into(),
                    size: TINY_PNG.len() as u64,
                    sha256: format!("{:x}", Sha256::digest(TINY_PNG)),
                    extra: Default::default(),
                }],
            }),
        )
        .unwrap();
    }

    struct StubGenerateExecutor;

    impl GenerateExecutor for StubGenerateExecutor {
        fn execute(&self, request: &GenerateRequest) -> Result<GenerateResponse, ClientError> {
            let text = if request.context == "observe.depict" {
                "foreground summary phrase".to_string()
            } else {
                serde_json::json!({
                    "density": "active",
                    "content_type": "writing",
                    "activity_summary": "foreground summary phrase",
                    "entities": [],
                    "facets": [],
                    "speculative_facet": null,
                    "meeting_detected": false,
                    "speakers": [],
                    "recommend": {
                        "screen_record": false,
                        "speaker_attribution": false
                    },
                    "emotional_register": "neutral",
                    "topics": [],
                    "quotes": []
                })
                .to_string()
            };
            Ok(GenerateResponse::Generated(Box::new(GeneratedResponse {
                id: None,
                text,
                model: "stub-model".to_string(),
                usage: serde_json::json!({}),
                finish_reason: "stop".to_string(),
                thinking: None,
                schema_validation: None,
                input_budget: None,
                request_budget: None,
                inference: None,
                hints_applied: Vec::new(),
            })))
        }
    }

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn setup_test_journal() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("journal-payload");
        fs::create_dir_all(payload.join("solstone/talent")).unwrap();
        fs::create_dir_all(payload.join("solstone/apps")).unwrap();
        fs::create_dir_all(payload.join("solstone/think/templates")).unwrap();
        fs::create_dir_all(temp.path().join("config")).unwrap();
        fs::write(
            temp.path().join("config/journal.json"),
            serde_json::json!({
                "identity": { "timezone": "UTC" },
                "providers": { "active": { "provider": "openai", "model": "fixture" } },
                "processing": { "history_confirmed": true }
            })
            .to_string(),
        )
        .unwrap();
        (temp, payload)
    }

    #[test]
    fn schedule_check_submits_cadence_and_records_outcome_without_writing_cadence_timestamp() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (temp, payload) = setup_test_journal();
        let start_time = ScheduleNow::in_zone(
            Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap(),
            chrono_tz::UTC,
        );
        let host = ForegroundProcessingHost::open_with_executor(
            temp.path(),
            &payload,
            start_time,
            Arc::new(StubGenerateExecutor),
        )
        .unwrap();

        let check_time = ScheduleNow::in_zone(
            Utc.with_ymd_and_hms(2026, 10, 17, 12, 5, 0).unwrap(),
            chrono_tz::UTC,
        );
        let report = host.check_schedule(check_time).unwrap();
        assert!(
            report.submitted.contains(&"cadence".to_string()),
            "cadence entry should be submitted at 12:05"
        );
        wait_for_file(&temp.path().join("health/scheduler.json"));
        host.shutdown().unwrap();

        // A no-input twin writes no segment output and no health/cadence.json timestamp
        assert!(
            !temp.path().join("health/cadence.json").exists(),
            "health/cadence.json must not be written by host or no-input cadence"
        );

        // The scheduler state still records that command's outcome in health/scheduler.json
        assert!(
            temp.path().join("health/scheduler.json").exists(),
            "health/scheduler.json must exist"
        );
    }

    #[test]
    fn admitted_image_runs_depict_and_indexes_phrase_while_audio_phrase_is_absent() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (temp, payload) = setup_test_journal();
        fs::write(
            payload.join("solstone/talent/sense.md"),
            "{\n\"type\": \"generate\",\n\"schedule\": \"segment\",\n\"priority\": 1,\n\"output\": \"json\",\n\"max_output_tokens\": 1024\n}\n\nAnalyze segment\n",
        )
        .unwrap();
        let segment_dir = temp.path().join("chronicle/20261012/notes/120000_1");
        fs::create_dir_all(&segment_dir).unwrap();

        let image_path = segment_dir.join("depict.png");
        write_image(&image_path);

        let audio_path = segment_dir.join("audio.jsonl");
        fs::write(
            &audio_path,
            b"{}\n{\"start\":\"00:00:01\",\"text\":\"exclusive audio only phrase transcript\"}\n",
        )
        .unwrap();

        let now = ScheduleNow::in_zone(
            Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap(),
            chrono_tz::UTC,
        );
        let host = ForegroundProcessingHost::open_with_executor(
            temp.path(),
            &payload,
            now,
            Arc::new(StubGenerateExecutor),
        )
        .unwrap();

        host.run_sense_pass().unwrap();

        let depict_output = segment_dir.join("depict.jsonl");
        assert!(
            depict_output.exists(),
            "depict.jsonl must be written for admitted image"
        );
        let depict_content = fs::read_to_string(&depict_output).unwrap();
        assert!(
            depict_content.contains("foreground summary phrase"),
            "depict output must contain foreground summary phrase"
        );

        let think_res = host.execute_worker_command(&[
            "think".into(),
            "--day".into(),
            "20261012".into(),
            "--segment".into(),
            "120000_1".into(),
            "--stream".into(),
            "notes".into(),
        ]);
        assert_eq!(think_res, 0, "segment think command must succeed");

        let activity_path = segment_dir.join("talents/activity.md");
        assert!(
            activity_path.exists(),
            "talents/activity.md must exist after segment think"
        );
        let activity_content = fs::read_to_string(&activity_path).unwrap();
        assert!(
            activity_content.contains("foreground summary phrase"),
            "activity.md must contain foreground summary phrase"
        );

        solstone_core_indexer_store::scan::scan_journal(temp.path(), true).unwrap();

        let conn = solstone_core_indexer_store::open_index_reader(temp.path()).unwrap();
        let mut stmt = conn
            .prepare("SELECT content, path FROM chunks WHERE content MATCH ?")
            .unwrap();
        let rows = stmt
            .query_map(["\"foreground summary phrase\""], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(
            !rows.is_empty(),
            "index must contain an entry for foreground summary phrase"
        );
        assert!(
            rows.iter()
                .any(|(_content, path)| path.contains("120000_1")),
            "indexed row path must contain the source segment identity 120000_1"
        );

        // A phrase that exists only in an audio.jsonl transcript is absent from depict's chunks
        let mut audio_stmt = conn
            .prepare("SELECT content FROM chunks WHERE content MATCH ?")
            .unwrap();
        let audio_rows = audio_stmt
            .query_map(["\"exclusive audio only phrase transcript\""], |row| {
                row.get::<_, String>(0)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            audio_rows.is_empty(),
            "phrase that exists only in audio.jsonl must be absent from depict index"
        );
    }

    #[test]
    fn cancel_leaves_paths_unchanged_while_uncancelled_writes_them() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (temp, payload) = setup_test_journal();
        let segment_dir = temp.path().join("chronicle/20261012/notes/120000_2");
        fs::create_dir_all(&segment_dir).unwrap();

        let image_path = segment_dir.join("depict.png");
        write_image(&image_path);

        let now = ScheduleNow::in_zone(
            Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap(),
            chrono_tz::UTC,
        );
        let host = ForegroundProcessingHost::open_with_executor(
            temp.path(),
            &payload,
            now,
            Arc::new(StubGenerateExecutor),
        )
        .unwrap();

        // 1. Cancel true leaves paths unchanged
        host.stop.store(true, Ordering::Release);
        host.run_sense_pass().unwrap();

        let depict_output = segment_dir.join("depict.jsonl");
        assert!(
            !depict_output.exists(),
            "depict.jsonl must not be written when cancelled"
        );

        // 2. Uncancelled twin writes them
        host.stop.store(false, Ordering::Release);
        host.run_sense_pass().unwrap();
        assert!(
            depict_output.exists(),
            "depict.jsonl must be written when uncancelled"
        );
    }

    #[test]
    fn missing_sense_json_exits_early_and_leaves_seeded_cadence_timestamp_unchanged() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (temp, payload) = setup_test_journal();
        let segment_dir = temp.path().join("chronicle/20261012/notes/120000_3");
        fs::create_dir_all(&segment_dir).unwrap();

        // Seed health/cadence.json with a timestamp
        fs::create_dir_all(temp.path().join("health")).unwrap();
        let cadence_path = temp.path().join("health/cadence.json");
        let seeded = serde_json::json!({
            "talents": {
                "schedule": 1700000000
            }
        });
        fs::write(&cadence_path, seeded.to_string()).unwrap();

        let now = ScheduleNow::in_zone(
            Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap(),
            chrono_tz::UTC,
        );
        let host = ForegroundProcessingHost::open_with_executor(
            temp.path(),
            &payload,
            now,
            Arc::new(StubGenerateExecutor),
        )
        .unwrap();

        // Submit segment think command where sense.json does NOT exist
        let _ = host.execute_worker_command(&[
            "think".into(),
            "--day".into(),
            "20261012".into(),
            "--segment".into(),
            "120000_3".into(),
            "--stream".into(),
            "notes".into(),
        ]);

        // A later talent (e.g. schedule.jsonl) is not published
        assert!(
            !segment_dir.join("schedule.jsonl").exists(),
            "later talent schedule.jsonl must not be published when sense.json is missing"
        );

        // Seeded health/cadence.json timestamp stays put
        let current_cadence: Value =
            serde_json::from_str(&fs::read_to_string(&cadence_path).unwrap()).unwrap();
        assert_eq!(
            current_cadence["talents"]["schedule"], 1700000000,
            "seeded cadence timestamp must remain unchanged"
        );
    }

    #[test]
    fn unbound_retry_permission_cannot_publish_or_consume() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (temp, payload) = setup_test_journal();
        let segment = temp.path().join("chronicle/20261012/notes/120000_4");
        fs::create_dir_all(&segment).unwrap();
        write_image(&segment.join("depict.png"));
        let unit = Unit::Raw {
            day: "20261012".into(),
            stream: "notes".into(),
            segment: "120000_4".into(),
            handler: "depict".into(),
            output: "depict.jsonl".into(),
            direct: false,
        };
        work_retry::observe_held(temp.path(), &unit, "generate-wire-failed", "unbound-input")
            .unwrap();
        work_retry::request(temp.path(), &unit).unwrap();
        let id = unit.id().unwrap();
        let record = work_retry::read(temp.path(), &id).unwrap().unwrap();
        let permission = record.permission("unbound-input", &[]).unwrap();
        let ledger = temp
            .path()
            .join("health/work-retry")
            .join(format!("{id}.json"));
        let before = fs::read(&ledger).unwrap();
        let host = ForegroundProcessingHost::open_with_executor(
            temp.path(),
            &payload,
            ScheduleNow::in_zone(
                Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap(),
                chrono_tz::UTC,
            ),
            Arc::new(StubGenerateExecutor),
        )
        .unwrap();
        assert_ne!(
            host.execute_worker_command(&[
                "think".into(),
                "--day".into(),
                "20261012".into(),
                "--retry-unit".into(),
                id,
                "--retry-cause".into(),
                permission.cause,
            ]),
            0
        );
        assert!(!segment.join("depict.jsonl").exists());
        assert_eq!(fs::read(ledger).unwrap(), before);
    }

    #[test]
    fn describe_and_daily_only_think_stay_held() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (temp, payload) = setup_test_journal();
        let segment_dir = temp.path().join("chronicle/20261012/notes/120000_5");
        fs::create_dir_all(&segment_dir).unwrap();

        let unit = Unit::Raw {
            day: "20261012".into(),
            stream: "notes".into(),
            segment: "120000_5".into(),
            handler: "describe".into(),
            output: "describe.jsonl".into(),
            direct: false,
        };
        let id = unit.id().unwrap();
        work_retry::observe_held(temp.path(), &unit, "generate-wire-failed", "sample.png").unwrap();

        let now = ScheduleNow::in_zone(
            Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap(),
            chrono_tz::UTC,
        );
        let host = ForegroundProcessingHost::open_with_executor(
            temp.path(),
            &payload,
            now,
            Arc::new(StubGenerateExecutor),
        )
        .unwrap();

        // Submit describe command - should be held (exit code non-zero)
        let describe_res = host.execute_worker_command(&["describe".into(), "sample.png".into()]);
        assert_ne!(
            describe_res, 0,
            "describe command must exit with non-zero code"
        );

        // Submit daily-only think command - should be held (exit code non-zero)
        let daily_res =
            host.execute_worker_command(&["think".into(), "--day".into(), "20261012".into()]);
        assert_ne!(
            daily_res, 0,
            "daily-only think must exit with non-zero code"
        );

        // The retry record is still held
        let record = work_retry::read(temp.path(), &id).unwrap().unwrap();
        assert!(record.held, "retry record must remain held");
    }

    #[test]
    fn retry_raw_unit_depict_executes_in_foreground_host_and_clears_permission() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (temp, payload) = setup_test_journal();
        let segment_dir = temp.path().join("chronicle/20261012/notes/120000_6");
        fs::create_dir_all(&segment_dir).unwrap();
        let image_path = segment_dir.join("depict.png");
        write_image(&image_path);

        let unit = Unit::Raw {
            day: "20261012".into(),
            stream: "notes".into(),
            segment: "120000_6".into(),
            handler: "depict".into(),
            output: "depict.jsonl".into(),
            direct: false,
        };
        let id = unit.id().unwrap();

        let depict_output = segment_dir.join("depict.jsonl");
        let header = serde_json::json!({
            "raw": "depict.png",
            "_solstone_processing": {
                "schema": "solstone.processing.v1",
                "state": "failed",
                "handler": "depict",
                "reason_code": "generate-wire-failed",
                "attempts": 3,
            }
        });
        fs::write(&depict_output, format!("{header}\n")).unwrap();

        let evidence =
            solstone_core_system::work_retry::raw::inspect_output(temp.path(), &depict_output)
                .unwrap()
                .unwrap();
        solstone_core_system::work_retry::raw::observe(temp.path(), &evidence).unwrap();
        let record = work_retry::read(temp.path(), &id).unwrap().unwrap();
        assert!(record.held, "retry record must be held");

        work_retry::request(temp.path(), &unit).unwrap();
        let perm = work_retry::read(temp.path(), &id)
            .unwrap()
            .unwrap()
            .permission(&evidence.input, &[])
            .expect("retry permission must be granted");

        let now = ScheduleNow::in_zone(
            Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap(),
            chrono_tz::UTC,
        );
        let host = ForegroundProcessingHost::open_with_executor(
            temp.path(),
            &payload,
            now,
            Arc::new(StubGenerateExecutor),
        )
        .unwrap();

        let exit_code = host.execute_worker_command(&[
            "think".into(),
            "--day".into(),
            "20261012".into(),
            "--retry-unit".into(),
            id.clone(),
            "--retry-cause".into(),
            perm.cause.clone(),
        ]);
        assert_eq!(exit_code, 0, "worker retry depict command must exit with 0");

        assert!(depict_output.exists(), "depict.jsonl must exist");
        let content = fs::read_to_string(&depict_output).unwrap();
        assert!(
            content.contains("foreground summary phrase"),
            "depict.jsonl must contain foreground summary phrase"
        );

        let post_record = work_retry::read(temp.path(), &id).unwrap().unwrap();
        assert!(
            !post_record.held,
            "retry record must not be held after finish"
        );
        assert!(
            post_record.permission(&evidence.input, &[]).is_none(),
            "permission must be gone after finish"
        );

        // Run identical command a second time
        let second_exit = host.execute_worker_command(&[
            "think".into(),
            "--day".into(),
            "20261012".into(),
            "--retry-unit".into(),
            id.clone(),
            "--retry-cause".into(),
            perm.cause.clone(),
        ]);
        assert_ne!(
            second_exit, 0,
            "second retry attempt must not finish successfully"
        );
        let second_content = fs::read_to_string(&depict_output).unwrap();
        assert_eq!(
            content, second_content,
            "output bytes must not be rewritten on second run"
        );
    }

    #[test]
    fn retry_raw_unit_describe_stays_held_in_foreground_host() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (temp, payload) = setup_test_journal();
        let segment_dir = temp.path().join("chronicle/20261012/notes/120000_7");
        fs::create_dir_all(&segment_dir).unwrap();
        let image_path = segment_dir.join("sample.png");
        write_image(&image_path);

        let unit = Unit::Raw {
            day: "20261012".into(),
            stream: "notes".into(),
            segment: "120000_7".into(),
            handler: "describe".into(),
            output: "describe.jsonl".into(),
            direct: false,
        };
        let id = unit.id().unwrap();

        work_retry::observe_held(temp.path(), &unit, "generate-wire-failed", "sample.png").unwrap();
        let record = work_retry::read(temp.path(), &id).unwrap().unwrap();
        assert!(record.held, "retry record must be held");

        work_retry::request(temp.path(), &unit).unwrap();
        let perm = work_retry::read(temp.path(), &id)
            .unwrap()
            .unwrap()
            .permission("sample.png", &[])
            .expect("retry permission must be granted");

        let now = ScheduleNow::in_zone(
            Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap(),
            chrono_tz::UTC,
        );
        let host = ForegroundProcessingHost::open_with_executor(
            temp.path(),
            &payload,
            now,
            Arc::new(StubGenerateExecutor),
        )
        .unwrap();

        let exit_code = host.execute_worker_command(&[
            "think".into(),
            "--day".into(),
            "20261012".into(),
            "--retry-unit".into(),
            id.clone(),
            "--retry-cause".into(),
            perm.cause.clone(),
        ]);
        assert_ne!(exit_code, 0, "describe retry command must exit non-zero");

        let describe_output = segment_dir.join("describe.jsonl");
        assert!(!describe_output.exists(), "describe.jsonl must not exist");

        let post_record = work_retry::read(temp.path(), &id).unwrap().unwrap();
        assert!(post_record.held, "retry record must remain held");
    }

    #[test]
    fn retry_daily_unit_exits_before_run_and_leaves_permission_file_unchanged() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (temp, payload) = setup_test_journal();

        let unit = Unit::Daily {
            day: "20261012".into(),
            talent: "sense".into(),
            facet: None,
        };
        let id = unit.id().unwrap();

        work_retry::observe_held(temp.path(), &unit, "generate-wire-failed", "sense_input")
            .unwrap();

        let retry_file = temp
            .path()
            .join("health/work-retry")
            .join(format!("{id}.json"));
        let before_bytes = fs::read(&retry_file).unwrap();

        let now = ScheduleNow::in_zone(
            Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap(),
            chrono_tz::UTC,
        );
        let host = ForegroundProcessingHost::open_with_executor(
            temp.path(),
            &payload,
            now,
            Arc::new(StubGenerateExecutor),
        )
        .unwrap();

        let exit_code = host.execute_worker_command(&[
            "think".into(),
            "--day".into(),
            "20261012".into(),
            "--retry-unit".into(),
            id.clone(),
            "--retry-cause".into(),
            "owner:unused".into(),
        ]);
        assert_ne!(exit_code, 0, "daily retry command must exit non-zero");

        let after_bytes = fs::read(&retry_file).unwrap();
        assert_eq!(
            before_bytes, after_bytes,
            "retry file bytes must remain identical"
        );

        let record = work_retry::read(temp.path(), &id).unwrap().unwrap();
        assert!(record.held, "record must still be held");
        assert!(
            record.attempt.is_none(),
            "record attempt must still be None"
        );

        let health_dir = temp.path().join("chronicle/20261012/health");
        let oplogs = if health_dir.exists() {
            fs::read_dir(&health_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.contains("oplog") || name.contains("think")
                })
                .count()
        } else {
            0
        };
        assert_eq!(
            oplogs, 0,
            "chronicle/20261012/health must contain no think oplog"
        );
    }
    fn wait_for_file(path: &Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while !path.is_file() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            path.is_file(),
            "automatic publication missing: {}",
            path.display()
        );
    }

    #[test]
    fn sense_worker_processes_new_input_after_its_initial_batch() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (temp, payload) = setup_test_journal();
        let first = temp
            .path()
            .join("chronicle/20261012/notes/120000_1/depict.png");
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        write_image(&first);
        let mut host = ForegroundProcessingHost::open_with_executor(
            temp.path(),
            &payload,
            ScheduleNow::in_zone(Utc::now(), chrono_tz::UTC),
            Arc::new(StubGenerateExecutor),
        )
        .unwrap();
        // Isolate the Sense loop: discovery must not supply a second execution.
        host.discovery
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .shutdown_checked()
            .unwrap();
        host.start_workers().unwrap();
        wait_for_file(&first.with_extension("jsonl"));
        let second = temp
            .path()
            .join("chronicle/20261013/notes/120000_2/depict.png");
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        write_image(&second);
        wait_for_file(&second.with_extension("jsonl"));
        host.shutdown().unwrap();
        assert!(host.sense_handle.lock().unwrap().is_none());
        assert!(host.schedule_handle.lock().unwrap().is_none());
    }

    #[test]
    fn image_waits_for_accepted_identical_custody_before_generate() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (temp, payload) = setup_test_journal();
        let image = temp
            .path()
            .join("chronicle/20261012/notes/120000_1/depict.png");
        fs::create_dir_all(image.parent().unwrap()).unwrap();
        fs::write(&image, TINY_PNG).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&calls);
        let executor = move |request: &GenerateRequest| {
            count.fetch_add(1, Ordering::SeqCst);
            StubGenerateExecutor.execute(request)
        };
        let host = ForegroundProcessingHost::open_with_executor(
            temp.path(),
            &payload,
            ScheduleNow::in_zone(Utc::now(), chrono_tz::UTC),
            Arc::new(executor),
        )
        .unwrap();
        host.run_sense_pass().unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "pre-receipt bytes cannot enter Generate"
        );
        assert!(!image.with_extension("jsonl").exists());
        write_image(&image);
        let mut changed = TINY_PNG.to_vec();
        changed[30] ^= 1;
        fs::write(&image, changed).unwrap();
        host.run_sense_pass().unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a different hash cannot borrow the accepted receipt"
        );
        assert!(!image.with_extension("jsonl").exists());
        fs::write(&image, TINY_PNG).unwrap();
        host.run_sense_pass().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(image.with_extension("jsonl").is_file());
        host.shutdown().unwrap();
    }

    #[test]
    fn stop_signals_held_generate_and_keeps_root_until_actual_worker_join() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (temp, payload) = setup_test_journal();
        let image = temp
            .path()
            .join("chronicle/20261012/notes/120000_1/depict.png");
        fs::create_dir_all(image.parent().unwrap()).unwrap();
        write_image(&image);
        let (entered_send, entered) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let released = Mutex::new(released);
        let executor = move |request: &GenerateRequest| {
            entered_send.send(()).unwrap();
            released.lock().unwrap().recv().unwrap();
            StubGenerateExecutor.execute(request)
        };
        let mut host = ForegroundProcessingHost::open_with_executor(
            temp.path(),
            &payload,
            ScheduleNow::in_zone(Utc::now(), chrono_tz::UTC),
            Arc::new(executor),
        )
        .unwrap();
        host.discovery
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .shutdown_checked()
            .unwrap();
        let root = Arc::downgrade(&host.root);
        let stop = Arc::clone(&host.stop);
        host.start_workers().unwrap();
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        // This is the native early Stop token, signalled without the host mutex.
        stop.store(true, Ordering::Release);
        let (drained_send, drained) = std::sync::mpsc::channel();
        let retirement = std::thread::spawn(move || {
            let result = host.shutdown();
            drained_send.send(result).unwrap();
        });
        assert!(drained.recv_timeout(Duration::from_millis(50)).is_err());
        assert!(root.upgrade().is_some());
        assert!(!image.with_extension("jsonl").exists());
        release.send(()).unwrap();
        drained
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        retirement.join().unwrap();
        assert!(!image.with_extension("jsonl").exists());
        assert!(
            root.upgrade().is_none(),
            "original root may drop only after actual drain"
        );
    }
}
