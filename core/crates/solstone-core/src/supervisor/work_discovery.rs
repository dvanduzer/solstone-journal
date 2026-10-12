// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! One coalescing discovery worker. It reconstructs work; TaskQueue executes it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant, SystemTime};

use solstone_core_import::thinking::{Coordinate, ImportThinking};
use solstone_core_journal_config::work_priority::{WorkBand, WorkPriority};
use solstone_core_system::{
    daily_coverage::{self, CoverageState},
    partition::canonical_journal_command,
    queue::{DiscoveredTask, TaskQueue},
    request::{BusTaskRequest, DailyCatchupProvenance, TaskArgv},
    work_intent::{self, IntentState, OwnerIntent},
    work_inventory::{WorkHold, WorkInventory},
    work_policy::WorkPolicy,
    work_retry::{self, Unit},
};

/// The least time between automatic passes.
const MIN_REST: Duration = Duration::from_secs(60);
/// An automatic pass then rests nine times as long as it ran, so on any journal
/// discovery holds at most a tenth of one core. A request still wakes it at once.
const REST_PER_PASS: u32 = 9;
/// How often a pass publishes its progress before it finishes.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

fn rest_after(pass: Duration) -> Duration {
    MIN_REST.max(pass * REST_PER_PASS)
}

pub(crate) struct WorkDiscovery {
    stopped: Arc<AtomicBool>,
    wake: mpsc::SyncSender<()>,
}

impl WorkDiscovery {
    pub fn new(journal: PathBuf, queue: TaskQueue, automatic: bool) -> Self {
        Self::with_reconciler(journal, queue, automatic, move |journal, queue, stopped| {
            reconcile_pass(journal, queue, stopped, automatic)
        })
    }

    fn with_reconciler(
        journal: PathBuf,
        queue: TaskQueue,
        automatic: bool,
        mut reconcile_pass: impl FnMut(&Path, &TaskQueue, &AtomicBool) -> Result<(), String>
        + Send
        + 'static,
    ) -> Self {
        let stopped = Arc::new(AtomicBool::new(false));
        let (wake, receiver) = mpsc::sync_channel(1);
        let worker_stop = Arc::clone(&stopped);
        let spawned = std::thread::Builder::new()
            .name("journal-work-discovery".into())
            .spawn(move || {
                if receiver.recv().is_err() {
                    return;
                }
                let mut initial = true;
                loop {
                    if worker_stop.load(Ordering::Acquire) {
                        return;
                    }
                    let started = Instant::now();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if initial {
                            solstone_core_system::catchup::reconcile_stale_catchup_attempts(
                                &journal,
                                SystemTime::now(),
                            )
                            .map_err(|error| error.to_string())?;
                            initial = false;
                        }
                        reconcile_pass(&journal, &queue, &worker_stop)
                    }));
                    let error = match result {
                        Ok(Ok(())) => None,
                        Ok(Err(error)) => Some(error),
                        Err(_) => Some("work discovery panicked".into()),
                    };
                    if let Some(error) = error {
                        log::warn!("work discovery: {error}");
                        if !worker_stop.load(Ordering::Acquire) {
                            let mut report = WorkInventory::default();
                            report.unreadable.push(error);
                            let _ = report.publish(&journal);
                        }
                    }
                    if worker_stop.load(Ordering::Acquire) {
                        return;
                    }
                    if automatic {
                        let rest = rest_after(started.elapsed());
                        match receiver.recv_timeout(rest) {
                            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                            Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    } else if receiver.recv().is_err() {
                        return;
                    }
                }
            });
        if let Err(error) = spawned {
            stopped.store(true, Ordering::Release);
            log::warn!("could not start work discovery: {error}");
        }
        Self { stopped, wake }
    }

    pub fn request(&self) {
        let _ = self.wake.try_send(());
    }
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.request();
    }
}

impl Drop for WorkDiscovery {
    fn drop(&mut self) {
        self.stop();
    }
}

struct DayWork {
    candidates: Vec<DiscoveredTask>,
    completed_segments: BTreeSet<Coordinate>,
    daily_current: bool,
}

fn priority(
    day: &str,
    segment: Option<&str>,
    today: chrono::NaiveDate,
    explicit: Option<WorkBand>,
) -> WorkPriority {
    let mut command = vec!["journal".into(), "think".into(), "--day".into(), day.into()];
    if let Some(segment) = segment {
        command.extend(["--segment".into(), segment.into()]);
    }
    let mut priority = solstone_core_system::priority::classify(&command, today);
    if let Some(band) = explicit {
        priority.band = band;
    }
    priority
}

fn task(
    day: &str,
    segment: Option<(&str, &str)>,
    priority: WorkPriority,
    intent: Option<&OwnerIntent>,
    confirmed_import: bool,
) -> DiscoveredTask {
    let mut tail = vec!["think".to_owned(), "--day".into(), day.into()];
    let from_scratch = intent.is_some_and(|intent| intent.from_scratch);
    if let Some((segment, stream)) = segment {
        tail.extend([
            "--segment".into(),
            segment.into(),
            "--stream".into(),
            stream.into(),
            "--repair-segment".into(),
        ]);
    } else if let Some(intent) = intent.filter(|intent| intent.from_scratch) {
        tail.extend([
            "--from-scratch".into(),
            "--work-intent".into(),
            intent.id.clone(),
        ]);
    } else {
        tail.push("--daily-only".into());
    }
    let suffix = segment
        .map(|(segment, stream)| format!("{stream}-{segment}"))
        .unwrap_or_else(|| {
            intent
                .filter(|i| i.from_scratch)
                .map(|i| i.id.clone())
                .unwrap_or_else(|| "daily".into())
        });
    DiscoveredTask {
        guard: None,
        request: BusTaskRequest {
            cmd: TaskArgv::from_wire(canonical_journal_command(tail))
                .expect("nonempty journal command"),
            reference: format!("supervisor-work-{day}-{suffix}"),
            day: Some(day.into()),
            scheduler_name: None,
            queue_if_active_cmd_differs: false,
            priority: Some(priority),
            daily_catchup_provenance: (segment.is_none() && !from_scratch)
                .then(|| DailyCatchupProvenance { day: day.into() }),
        },
        confirmed_import,
    }
}

fn held(
    report: &mut WorkInventory,
    day: &str,
    segment: Option<(&str, &str)>,
    band: WorkBand,
    reason: &str,
) {
    report.holds.push(WorkHold {
        retry_unit: None,
        day: day.into(),
        segment: segment.map(|(s, _)| s.into()),
        stream: segment.map(|(_, s)| s.into()),
        band,
        reason: reason.into(),
    });
}

fn retry_imported(unit: &Unit, imports: &[ImportThinking]) -> bool {
    imports.iter().any(|import| match unit {
        Unit::Raw {
            day,
            segment,
            stream,
            ..
        }
        | Unit::Segment {
            day,
            segment,
            stream,
            ..
        } => import.segments.contains(&Coordinate {
            day: day.clone(),
            segment: segment.clone(),
            stream: stream.clone(),
        }),
        Unit::Daily { day, talent, .. } if talent != "daily_schedule" => import.days.contains(day),
        _ => false,
    })
}

/// Read native writer evidence on the discovery worker. The permission ledger
/// keeps the original failed input; it does not make this scan authoritative.
#[allow(clippy::too_many_arguments)]
fn raw_retries(
    journal: &Path,
    day: &str,
    segment: &solstone_core_system_health::DaySegment,
    intent: Option<&OwnerIntent>,
    imports: &[ImportThinking],
    policy: WorkPolicy,
    capped: &BTreeSet<solstone_core_system_health::TerminalUnit>,
    report: &mut WorkInventory,
) -> Result<Vec<DiscoveredTask>, String> {
    let directory = match segment.stream_layout {
        solstone_core_journal_io::SegmentLayout::Direct => {
            journal.join("chronicle").join(day).join(&segment.key)
        }
        solstone_core_journal_io::SegmentLayout::Named => journal
            .join("chronicle")
            .join(day)
            .join(&segment.stream)
            .join(&segment.key),
    };
    let today = chrono::Utc::now()
        .with_timezone(&solstone_core_journal_config::owner_zone(journal))
        .date_naive();
    let mut tasks = Vec::new();
    for talent in solstone_core_system_health::SEGMENT_FLOOR_TALENTS {
        let unit = Unit::Segment {
            day: day.into(),
            stream: segment.stream.clone(),
            segment: segment.key.clone(),
            talent: (*talent).into(),
            direct: matches!(
                segment.stream_layout,
                solstone_core_journal_io::SegmentLayout::Direct
            ),
        };
        if work_retry::read(journal, &unit.id()?)?.is_some() {
            continue;
        }
        if capped.contains(&solstone_core_system_health::TerminalUnit {
            day: day.into(),
            mode: "segment".into(),
            name: (*talent).into(),
            facet: None,
            stream: Some(segment.stream.clone()),
            segment: Some(segment.key.clone()),
            activity: None,
        }) {
            match work_retry::input::current(journal, &unit) {
                Ok(input) => work_retry::observe_held(journal, &unit, "floor_failed", &input)?,
                Err(error) => report.unreadable.push(error),
            }
        }
    }
    for entry in std::fs::read_dir(directory).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().and_then(|p| p.to_str()) != Some("jsonl") {
            continue;
        }
        let evidence = match work_retry::raw::inspect_output(journal, &path) {
            Ok(Some(evidence)) => evidence,
            Ok(None) => continue,
            Err(error) => {
                report
                    .unreadable
                    .push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        work_retry::raw::observe(journal, &evidence)?;
        let id = evidence.unit.id()?;
        // A durable day ask is expanded idempotently, never renewed by ticks.
        if let Some(intent) = intent {
            work_retry::request_with_id(journal, &evidence.unit, &intent.id)?;
        }
        let record = work_retry::read(journal, &id)?.ok_or("missing native hold")?;
        let grant = (!evidence.protected)
            .then(|| record.permission(&evidence.input, work_retry::DECLARED_FIXES))
            .flatten();
        let imported = retry_imported(&record.unit, imports);
        let explicit = match grant.as_ref().map(|a| a.cause.as_str()) {
            Some(cause) if cause.starts_with("owner:") => Some(WorkBand::Waiting),
            Some(cause) if cause.starts_with("fix:") => Some(WorkBand::Completing),
            _ => imported.then_some(WorkBand::Import),
        };
        let p = priority(day, Some(&segment.key), today, explicit);
        report.holds.push(WorkHold {
            retry_unit: Some(id.clone()),
            day: day.into(),
            segment: Some(segment.key.clone()),
            stream: Some(segment.stream.clone()),
            band: p.band,
            reason: record.reason.clone(),
        });
        let Some(grant) = grant else {
            continue;
        };
        if !policy.permits(p.band, imported) {
            report.band(p.band).permission_held += 1;
            continue;
        }
        report.band(p.band).eligible += 1;
        tasks.push(DiscoveredTask {
            guard: None,
            confirmed_import: imported,
            request: BusTaskRequest {
                cmd: TaskArgv::from_wire(canonical_journal_command([
                    "think",
                    "--day",
                    day,
                    "--retry-unit",
                    &id,
                    "--retry-cause",
                    &grant.cause,
                ]))
                .expect("native retry command"),
                reference: format!("supervisor-retry-raw-{day}-{id}"),
                day: Some(day.into()),
                scheduler_name: None,
                queue_if_active_cmd_differs: false,
                priority: Some(p),
                daily_catchup_provenance: None,
            },
        });
    }
    Ok(tasks)
}

#[allow(clippy::too_many_arguments)]
fn daily_retries(
    journal: &Path,
    day: &str,
    coverage: &daily_coverage::DailyCoverage,
    intent: Option<&OwnerIntent>,
    imports: &[ImportThinking],
    policy: WorkPolicy,
    ready: bool,
    report: &mut WorkInventory,
) -> Result<Vec<DiscoveredTask>, String> {
    let today = chrono::Utc::now()
        .with_timezone(&solstone_core_journal_config::owner_zone(journal))
        .date_naive();
    let mut candidates = Vec::new();
    let maintenance = coverage
        .maintenance
        .as_ref()
        .filter(|_| day == today.format("%Y%m%d").to_string());
    for covered in coverage.units.iter().chain(maintenance) {
        let unit = Unit::Daily {
            day: day.into(),
            talent: covered.identity.name.clone(),
            facet: covered.identity.facet.clone(),
        };
        let Some(input) = work_retry::daily::observe(journal, &unit)? else {
            continue;
        };
        let id = unit.id()?;
        if let Some(intent) = intent {
            work_retry::request_with_id(journal, &unit, &intent.id)?;
        }
        let record = work_retry::read(journal, &id)?.ok_or("daily permission missing")?;
        let grant = record.permission(&input, work_retry::DECLARED_FIXES);
        let imported = retry_imported(&record.unit, imports);
        let explicit = match grant.as_ref().map(|a| a.cause.as_str()) {
            Some(cause) if cause.starts_with("owner:") => Some(WorkBand::Waiting),
            Some(cause) if cause.starts_with("fix:") => Some(WorkBand::Completing),
            _ => imported.then_some(WorkBand::Import),
        };
        let p = priority(day, None, today, explicit);
        if !covered.state.is_owed() {
            report.band(p.band).daily_units_owed += 1;
        }
        report.holds.push(WorkHold {
            retry_unit: Some(id.clone()),
            day: day.into(),
            segment: None,
            stream: None,
            band: p.band,
            reason: record.reason.clone(),
        });
        let Some(grant) = grant else {
            continue;
        };
        if !ready {
            report.band(p.band).prerequisite_held += 1;
            continue;
        }
        if !policy.permits(p.band, imported) {
            report.band(p.band).permission_held += 1;
            continue;
        }
        report.band(p.band).eligible += 1;
        candidates.push(DiscoveredTask {
            guard: None,
            confirmed_import: imported,
            request: BusTaskRequest {
                cmd: TaskArgv::from_wire(canonical_journal_command([
                    "think",
                    "--day",
                    day,
                    "--retry-unit",
                    &id,
                    "--retry-cause",
                    &grant.cause,
                ]))
                .expect("daily retry command"),
                reference: format!("supervisor-retry-daily-{day}-{id}"),
                day: Some(day.into()),
                scheduler_name: None,
                queue_if_active_cmd_differs: false,
                priority: Some(p),
                daily_catchup_provenance: None,
            },
        });
    }
    Ok(candidates)
}

fn talent_retries(
    journal: &Path,
    day: &str,
    intent: Option<&OwnerIntent>,
    imports: &[ImportThinking],
    policy: WorkPolicy,
    report: &mut WorkInventory,
) -> Result<Vec<DiscoveredTask>, String> {
    let (records, errors) = work_retry::list(journal);
    report.unreadable.extend(errors);
    let today = chrono::Utc::now()
        .with_timezone(&solstone_core_journal_config::owner_zone(journal))
        .date_naive();
    let mut candidates = Vec::new();
    for record in records.into_iter().filter(|r| {
        r.unit.day() == day && matches!(r.unit, Unit::Segment { .. } | Unit::Activity { .. })
    }) {
        let id = record.unit.id()?;
        let current = match &record.unit {
            Unit::Activity { .. } => {
                solstone_core_think_cli::activity_held_input(journal, &record.unit)
            }
            _ => work_retry::input::current(journal, &record.unit).map(Some),
        };
        let input = match current {
            Ok(Some(input)) => Some(input),
            Ok(None) => continue,
            Err(error) => {
                report.unreadable.push(format!("{day}: {error}"));
                None
            }
        };
        if let Some(intent) = intent {
            work_retry::request_with_id(journal, &record.unit, &intent.id)?;
        }
        let record = work_retry::read(journal, &id)?.ok_or("missing talent retry")?;
        let grant = input
            .as_ref()
            .and_then(|input| record.permission(input, work_retry::DECLARED_FIXES));
        let imported = retry_imported(&record.unit, imports);
        let explicit = match grant.as_ref().map(|a| a.cause.as_str()) {
            Some(cause) if cause.starts_with("owner:") => Some(WorkBand::Waiting),
            Some(cause) if cause.starts_with("fix:") => Some(WorkBand::Completing),
            _ => imported.then_some(WorkBand::Import),
        };
        let (segment, stream) = match &record.unit {
            Unit::Segment {
                segment, stream, ..
            } => (Some(segment.clone()), Some(stream.clone())),
            _ => (None, None),
        };
        let p = priority(day, segment.as_deref(), today, explicit);
        report.holds.push(WorkHold {
            retry_unit: Some(id.clone()),
            day: day.into(),
            segment,
            stream,
            band: p.band,
            reason: record.reason.clone(),
        });
        if matches!(record.unit, Unit::Activity { .. }) {
            report.band(p.band).activities_owed += 1;
        }
        let Some(grant) = grant else {
            continue;
        };
        if !policy.permits(p.band, imported) {
            report.band(p.band).permission_held += 1;
            continue;
        }
        report.band(p.band).eligible += 1;
        candidates.push(DiscoveredTask {
            guard: None,
            confirmed_import: imported,
            request: BusTaskRequest {
                cmd: TaskArgv::from_wire(canonical_journal_command([
                    "think",
                    "--day",
                    day,
                    "--retry-unit",
                    &id,
                    "--retry-cause",
                    &grant.cause,
                ]))
                .expect("talent retry command"),
                reference: format!("supervisor-retry-talent-{day}-{id}"),
                day: Some(day.into()),
                scheduler_name: None,
                queue_if_active_cmd_differs: false,
                priority: Some(p),
                daily_catchup_provenance: None,
            },
        });
    }
    Ok(candidates)
}

fn inspect_day(
    journal: &Path,
    day: &str,
    queue: &TaskQueue,
    intent: Option<&OwnerIntent>,
    imports: &[ImportThinking],
    policy: WorkPolicy,
    report: &mut WorkInventory,
) -> Result<DayWork, String> {
    let now = chrono::Utc::now();
    let today = now
        .with_timezone(&solstone_core_journal_config::owner_zone(journal))
        .date_naive();
    let mut work = DayWork {
        candidates: Vec::new(),
        completed_segments: BTreeSet::new(),
        daily_current: false,
    };
    if let Some(intent) = intent.filter(|intent| intent.from_scratch) {
        if intent.state == IntentState::Queued {
            let p = priority(day, None, today, Some(WorkBand::Waiting));
            report.band(p.band).owner_requests += 1;
            if policy.permits(p.band, false) {
                work.candidates
                    .push(task(day, None, p, Some(intent), false));
                report.band(p.band).eligible += 1;
            } else {
                report.band(p.band).permission_held += 1;
                held(report, day, None, p.band, "thinking_unavailable");
            }
        } else {
            held(
                report,
                day,
                None,
                WorkBand::Waiting,
                if intent.state == IntentState::Claimed {
                    "interrupted_from_scratch"
                } else {
                    "failed_from_scratch"
                },
            );
        }
        return Ok(work);
    }
    let (_, _, segments) = solstone_core_system_health::scan_day(
        &solstone_core_system_health::FilesystemSegmentSource,
        journal,
        day,
        now,
    )
    .map_err(|error| error.to_string())?;
    let progress = solstone_core_system_health::read_segment_progress(
        &solstone_core_system_health::FilesystemHealthLogSource::new(journal),
        day,
    )
    .map_err(|error| error.to_string())?;
    if progress.malformed_line_count != 0 {
        return Err("segment progress contains unreadable records".into());
    }
    // One read of the day's logs; never one per segment.
    let capped = solstone_core_system_health::read_capped_segment_talents(
        &solstone_core_system_health::FilesystemHealthLogSource::new(journal),
        day,
    )
    .map_err(|error| error.to_string())?;
    if capped.malformed_line_count != 0 {
        return Err("floor failure evidence is unreadable".into());
    }
    let mut prerequisites_ready = true;
    for segment in &segments {
        if super::tick::is_mcp_audit_segment(Some(&segment.stream)) {
            continue;
        }
        work.candidates.extend(raw_retries(
            journal,
            day,
            segment,
            intent,
            imports,
            policy,
            &capped.value,
            report,
        )?);
        let coordinate = Coordinate {
            day: day.into(),
            stream: segment.stream.clone(),
            segment: segment.key.clone(),
        };
        let imported = imports
            .iter()
            .any(|import| import.segments.contains(&coordinate));
        let explicit = intent
            .map(|_| WorkBand::Waiting)
            .or_else(|| imported.then_some(WorkBand::Import));
        let p = priority(day, Some(&segment.key), today, explicit);
        if solstone_core_system_health::segment_work_ready(journal, day, segment, &progress.value)?
        {
            report.band(p.band).segments_complete += 1;
            work.completed_segments.insert(coordinate);
            continue;
        }
        prerequisites_ready = false;
        report.band(p.band).segments_owed += 1;
        if solstone_core_system_health::segment_repair_protected(
            journal,
            day,
            segment,
            &progress.value,
        )? {
            held(
                report,
                day,
                Some((&segment.key, &segment.stream)),
                p.band,
                solstone_core_system::segment_output::PROTECTED,
            );
        } else if policy.permits(p.band, imported) {
            report.band(p.band).eligible += 1;
            work.candidates.push(task(
                day,
                Some((&segment.key, &segment.stream)),
                p,
                None,
                imported,
            ));
        } else {
            report.band(p.band).permission_held += 1;
            held(
                report,
                day,
                Some((&segment.key, &segment.stream)),
                p.band,
                "spending_confirmation",
            );
        }
    }
    let imported = imports.iter().any(|import| import.days.contains(day));
    let explicit = intent
        .map(|_| WorkBand::Waiting)
        .or_else(|| imported.then_some(WorkBand::Import));
    let p = priority(day, None, today, explicit);
    let coverage = daily_coverage::read_daily_coverage(journal, day)?;
    work.candidates.extend(daily_retries(
        journal,
        day,
        &coverage,
        intent,
        imports,
        policy,
        prerequisites_ready
            && super::tick::day_still_closing(journal, queue, SystemTime::now()).as_deref()
                != Some(day),
        report,
    )?);
    if day >= today.format("%Y%m%d").to_string().as_str() {
        return Ok(work);
    }
    let owed = coverage
        .units
        .iter()
        .filter(|unit| unit.state.is_owed())
        .count() as u64;
    report.band(p.band).daily_units_complete += coverage
        .units
        .iter()
        .filter(|unit| unit.state.is_current())
        .count() as u64;
    if coverage
        .units
        .iter()
        .any(|unit| unit.state == CoverageState::HistoricalUnverified)
    {
        report
            .unreadable
            .push(format!("{day}: unsupported daily evidence contract"));
    }
    report.band(p.band).daily_units_owed += owed;
    work.daily_current = coverage.state.is_current()
        && prerequisites_ready
        && !report.holds.iter().any(|hold| hold.retry_unit.is_some());
    if coverage.state == CoverageState::Unreadable {
        return Err("daily coverage is unreadable".into());
    }
    if owed == 0 {
        return Ok(work);
    }
    if !prerequisites_ready
        || super::tick::day_still_closing(journal, queue, SystemTime::now()).as_deref() == Some(day)
    {
        report.band(p.band).prerequisite_held += owed;
        held(report, day, None, p.band, "segment_prerequisites");
    } else if !policy.permits(p.band, imported) {
        report.band(p.band).permission_held += owed;
        held(report, day, None, p.band, "spending_confirmation");
    } else if intent.is_none()
        && !solstone_core_system::catchup::eligible_or_fail_open(journal, day, SystemTime::now())
    {
        held(report, day, None, p.band, "retry_wait");
    } else {
        report.band(p.band).eligible += owed;
        work.candidates.push(task(day, None, p, None, imported));
    }
    Ok(work)
}

fn retain_best(candidates: &mut Vec<DiscoveredTask>, new: Vec<DiscoveredTask>) {
    candidates.extend(new);
    candidates.sort_by_key(|candidate| {
        candidate
            .request
            .priority
            .expect("discovery priority")
            .sort_key()
    });
    candidates.dedup_by(|a, b| a.request.cmd == b.request.cmd);
    candidates.truncate(4);
}

fn still_owed(
    journal: &Path,
    candidate: &DiscoveredTask,
    imports: &[ImportThinking],
) -> Result<bool, String> {
    let day = candidate.request.day.as_deref().ok_or("work has no day")?;
    let argv = candidate.request.cmd.as_wire();
    let argument = |flag: &str| {
        argv.windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1].as_str())
    };
    if let Some(id) = argument("--retry-unit") {
        let Some(record) = work_retry::read(journal, id)? else {
            return Ok(false);
        };
        if candidate.confirmed_import && !retry_imported(&record.unit, imports) {
            return Ok(false);
        }
        let (input, reason) = match &record.unit {
            Unit::Raw { output, .. } => {
                let output = record.unit.segment_directory(journal)?.join(output);
                let Some(evidence) = work_retry::raw::inspect_output(journal, &output)? else {
                    return Ok(false);
                };
                if evidence.protected {
                    return Ok(false);
                }
                (evidence.input, evidence.reason)
            }
            Unit::Daily { .. } => {
                if !solstone_core_system_health::day_segments_ready(journal, day)? {
                    return Ok(false);
                }
                let Some(input) = work_retry::daily::observe(journal, &record.unit)? else {
                    return Ok(false);
                };
                (input, record.reason.clone())
            }
            Unit::Segment { .. } => (
                work_retry::input::current(journal, &record.unit)?,
                record.reason.clone(),
            ),
            Unit::Activity { .. } => {
                let Some(input) =
                    solstone_core_think_cli::activity_held_input(journal, &record.unit)?
                else {
                    return Ok(false);
                };
                (input, record.reason.clone())
            }
        };
        return Ok(record
            .permission(&input, work_retry::DECLARED_FIXES)
            .is_some_and(|a| {
                Some(a.cause.as_str()) == argument("--retry-cause") && a.reason == reason
            }));
    }
    if let Some(activity) = argument("--activity") {
        return solstone_core_think_cli::activity_retry_due(
            journal,
            &solstone_core_think_cli::ActivityRetry {
                day: day.into(),
                facet: argument("--facet").ok_or("activity has no facet")?.into(),
                activity: activity.into(),
            },
            chrono::Utc::now().timestamp_millis(),
        );
    }
    if let Some(id) = argument("--work-intent") {
        return Ok(work_intent::read(journal)?.get(day).is_some_and(|intent| {
            intent.id == id && intent.from_scratch && intent.state == IntentState::Queued
        }));
    }
    if let Some(segment) = argument("--segment") {
        let stream = argument("--stream").ok_or("work has no stream")?;
        if candidate.confirmed_import
            && !imports.iter().any(|import| {
                import.segments.contains(&Coordinate {
                    day: day.into(),
                    stream: stream.into(),
                    segment: segment.into(),
                })
            })
        {
            return Ok(false);
        }
        let (_, _, segments) = solstone_core_system_health::scan_day(
            &solstone_core_system_health::FilesystemSegmentSource,
            journal,
            day,
            chrono::Utc::now(),
        )
        .map_err(|error| error.to_string())?;
        let Some(segment) = segments
            .iter()
            .find(|entry| entry.key == segment && entry.stream == stream)
        else {
            return Ok(false);
        };
        let progress = solstone_core_system_health::read_segment_progress(
            &solstone_core_system_health::FilesystemHealthLogSource::new(journal),
            day,
        )
        .map_err(|error| error.to_string())?;
        if progress.malformed_line_count != 0 {
            return Err("segment progress contains unreadable records".into());
        }
        return solstone_core_system_health::segment_work_ready(
            journal,
            day,
            segment,
            &progress.value,
        )
        .map(|ready| !ready);
    }
    if candidate.confirmed_import && !imports.iter().any(|import| import.days.contains(day)) {
        return Ok(false);
    }
    Ok(daily_coverage::read_daily_coverage(journal, day)?
        .state
        .is_owed()
        && solstone_core_system_health::day_segments_ready(journal, day)?)
}

fn admit(
    journal: &Path,
    queue: &TaskQueue,
    stopped: &AtomicBool,
    candidates: Vec<DiscoveredTask>,
    report: &mut WorkInventory,
) -> Result<(), String> {
    if stopped.load(Ordering::Acquire) {
        return Ok(());
    }
    let imports = solstone_core_import::thinking::read(journal)?;
    let mut current = Vec::new();
    for error in &imports.unreadable {
        if !report.unreadable.contains(error) {
            report.unreadable.push(error.clone());
        }
    }
    if !imports.unreadable.is_empty() {
        report.complete = false;
    }
    for mut candidate in candidates {
        if stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        match still_owed(journal, &candidate, &imports.imports) {
            Ok(true) => {
                if candidate.confirmed_import {
                    let day = candidate.request.day.clone().expect("discovered day");
                    let argv = candidate.request.cmd.as_wire();
                    let argument =
                        |flag: &str| argv.windows(2).find(|p| p[0] == flag).map(|p| p[1].clone());
                    let segment = if let Some(id) = argument("--retry-unit") {
                        match work_retry::read(journal, &id)?.map(|r| r.unit) {
                            Some(
                                Unit::Raw {
                                    segment, stream, ..
                                }
                                | Unit::Segment {
                                    segment, stream, ..
                                },
                            ) => Some((segment, stream)),
                            _ => None,
                        }
                    } else {
                        argument("--segment").zip(argument("--stream"))
                    };
                    let grants = imports
                        .imports
                        .iter()
                        .filter(|import| match &segment {
                            Some((segment, stream)) => import.segments.contains(&Coordinate {
                                day: day.clone(),
                                segment: segment.clone(),
                                stream: stream.clone(),
                            }),
                            None => import.days.contains(&day),
                        })
                        .map(|import| (import.id.clone(), import.publication.clone()))
                        .collect::<BTreeSet<_>>();
                    let root = journal.to_path_buf();
                    candidate.guard = Some(Arc::new(move || {
                        solstone_core_import::thinking::grant_current(
                            &root,
                            &grants,
                            &day,
                            segment
                                .as_ref()
                                .map(|(segment, stream)| (segment.as_str(), stream.as_str())),
                        )
                    }));
                }
                current.push(candidate);
            }
            Ok(false) => {}
            Err(error) => {
                report.unreadable.push(error);
                report.complete = false;
            }
        }
    }
    if !stopped.load(Ordering::Acquire) {
        queue.replace_discovered(current);
    }
    Ok(())
}

/// Fold the per-day snapshots into the published totals. Built only when it
/// is about to be published: the holds grow with the journal.
fn aggregate(
    report: &mut WorkInventory,
    day_reports: &BTreeMap<String, WorkInventory>,
    import_errors: &[String],
    days: &BTreeMap<String, PathBuf>,
    days_inspected: usize,
) {
    report.bands = std::array::from_fn(|_| Default::default());
    report.holds.clear();
    report.unreadable = import_errors.to_vec();
    for day_report in day_reports.values() {
        for (total, day) in report.bands.iter_mut().zip(&day_report.bands) {
            total.add(day);
        }
        report.holds.extend(day_report.holds.iter().cloned());
        report
            .unreadable
            .extend(day_report.unreadable.iter().cloned());
    }
    report.days_inspected = days_inspected;
    report.days_total = days.len();
    report.oldest_day = days.keys().next().cloned();
    report.newest_day = days.keys().next_back().cloned();
    report.as_of_ms = chrono::Utc::now().timestamp_millis();
}

#[cfg(test)]
fn reconcile(journal: &Path, queue: &TaskQueue, stopped: &AtomicBool) -> Result<(), String> {
    reconcile_pass(journal, queue, stopped, false)
}

/// An automatic pass says when the next one is due, so its snapshot stays
/// current while discovery rests rather than being rewritten to look fresh.
fn reconcile_pass(
    journal: &Path,
    queue: &TaskQueue,
    stopped: &AtomicBool,
    automatic: bool,
) -> Result<(), String> {
    let pass_started = Instant::now();
    let policy = WorkPolicy::read(journal)?;
    let mut days = solstone_core_journal_io::day_dirs(journal)
        .map_err(|error| error.to_string())?
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    let imports = solstone_core_import::thinking::read(journal)?;
    let (activities, activity_errors) = solstone_core_think_cli::discover_activity_retries(
        journal,
        chrono::Utc::now().timestamp_millis(),
    );
    let (held_activities, held_errors) = solstone_core_think_cli::discover_held_activities(journal);
    let (held_records, retry_errors) = work_retry::list(journal);
    let today = daily_coverage::local_day(journal, chrono::Utc::now());
    let maintenance =
        solstone_core_journal_io::DailyUnitIdentity::new(&today, "daily_schedule", None);
    if solstone_core_journal_io::load_daily_unit_record(journal, &maintenance)
        .map_err(|e| e.to_string())?
        .is_some_and(|record| record.status == solstone_core_journal_io::DailyUnitStatus::Capped)
    {
        days.entry(today.clone())
            .or_insert_with(|| journal.join("chronicle").join(today));
    }

    for unit in held_activities
        .iter()
        .chain(held_records.iter().map(|r| &r.unit))
    {
        days.entry(unit.day().into())
            .or_insert_with(|| journal.join("chronicle").join(unit.day()));
    }
    for activity in &activities {
        days.entry(activity.day.clone())
            .or_insert_with(|| journal.join("chronicle").join(&activity.day));
    }
    let mut report = WorkInventory {
        days_total: days.len(),
        oldest_day: days.keys().next().cloned(),
        newest_day: days.keys().next_back().cloned(),
        unreadable: imports.unreadable,
        ..WorkInventory::default()
    };
    report.unreadable.extend(activity_errors);
    report.unreadable.extend(held_errors);
    report.unreadable.extend(retry_errors);
    report.publish(journal)?;
    let mut last_publish = Instant::now();
    let mut candidates = Vec::new();
    let mut day_reports = BTreeMap::<String, WorkInventory>::new();
    let import_errors = report.unreadable.clone();
    let mut inspected = BTreeSet::new();
    let mut observed_intents = BTreeMap::new();
    let mut fulfilled_segments = BTreeSet::new();
    let mut fulfilled_days = BTreeSet::new();
    // Check owner requests before the history pass and between days. Source
    // reads run only here, never on the supervisor's event loop.
    let mut ordered = days.keys().rev().cloned().collect::<VecDeque<_>>();
    loop {
        if stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let intents = work_intent::read(journal)?;
        let owner_day = intents
            .iter()
            .rev()
            .find(|(day, intent)| observed_intents.get(*day) != Some(&intent.id))
            .map(|(day, _)| day.clone());
        let owner_wakeup = owner_day.is_some();
        let day = match owner_day.or_else(|| {
            while let Some(day) = ordered.pop_front() {
                if !inspected.contains(&day) {
                    return Some(day);
                }
            }
            None
        }) {
            Some(day) => day,
            None => break,
        };
        if let Some(intent) = intents.get(&day) {
            observed_intents.insert(day.clone(), intent.id.clone());
        }
        // An owner can replace an intent while its day is being inspected.
        // Replace that day's snapshot and queued contribution; never add it twice.
        candidates
            .retain(|candidate: &DiscoveredTask| candidate.request.day.as_deref() != Some(&day));
        fulfilled_segments.retain(|coordinate: &Coordinate| coordinate.day != day);
        fulfilled_days.remove(&day);
        days.entry(day.clone())
            .or_insert_with(|| journal.join("chronicle").join(&day));
        let mut day_report = WorkInventory::default();
        if let Err(error) = solstone_core_think_cli::seed_activity_retries(
            journal,
            &day,
            chrono::Utc::now().timestamp_millis(),
        ) {
            day_report.unreadable.push(format!("{day}: {error}"));
        }
        match inspect_day(
            journal,
            &day,
            queue,
            intents.get(&day),
            &imports.imports,
            policy,
            &mut day_report,
        ) {
            Ok(work) => {
                fulfilled_segments.extend(work.completed_segments);
                if work.daily_current {
                    fulfilled_days.insert(day.clone());
                }
                retain_best(&mut candidates, work.candidates);
            }
            Err(error) => day_report.unreadable.push(format!("{day}: {error}")),
        }
        if !intents.get(&day).is_some_and(|intent| intent.from_scratch) {
            match talent_retries(
                journal,
                &day,
                intents.get(&day),
                &imports.imports,
                policy,
                &mut day_report,
            ) {
                Ok(work) => retain_best(&mut candidates, work),
                Err(error) => day_report.unreadable.push(format!("{day}: {error}")),
            }
        }
        if day_report
            .holds
            .iter()
            .any(|hold| hold.retry_unit.is_some())
        {
            fulfilled_days.remove(&day);
        }
        if fulfilled_days.contains(&day)
            && !activities.iter().any(|a| a.day == day)
            && let Some(intent) = intents.get(&day).filter(|intent| !intent.from_scratch)
        {
            work_intent::retire_current(journal, &day, &intent.id)?;
        }
        for activity in activities.iter().filter(|a| {
            a.day == day && !intents.get(&day).is_some_and(|intent| intent.from_scratch)
        }) {
            let today = chrono::Utc::now()
                .with_timezone(&solstone_core_journal_config::owner_zone(journal))
                .date_naive();
            let p = priority(
                &day,
                None,
                today,
                intents.get(&day).map(|_| WorkBand::Waiting),
            );
            day_report.band(p.band).activities_owed += 1;
            if !policy.permits(p.band, false) {
                day_report.band(p.band).permission_held += 1;
                held(&mut day_report, &day, None, p.band, "spending_confirmation");
                continue;
            }
            day_report.band(p.band).eligible += 1;
            let identity = solstone_core_system::work_retry::Unit::Activity {
                day: day.clone(),
                facet: activity.facet.clone(),
                activity: activity.activity.clone(),
                talent: "activity".into(),
            };
            let reference = format!("supervisor-activity-{day}-{}", identity.id()?);
            let candidate = DiscoveredTask {
                guard: None,
                confirmed_import: false,
                request: BusTaskRequest {
                    cmd: TaskArgv::from_wire(canonical_journal_command([
                        "think",
                        "--day",
                        &day,
                        "--facet",
                        &activity.facet,
                        "--activity",
                        &activity.activity,
                    ]))
                    .expect("activity command"),
                    reference,
                    day: Some(day.clone()),
                    scheduler_name: None,
                    queue_if_active_cmd_differs: false,
                    priority: Some(p),
                    daily_catchup_provenance: None,
                },
            };
            retain_best(&mut candidates, vec![candidate]);
        }
        inspected.insert(day.clone());
        day_reports.insert(day, day_report);
        if stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        // Progress, not a rewrite of the whole inventory after every day.
        if last_publish.elapsed() >= PROGRESS_INTERVAL {
            aggregate(
                &mut report,
                &day_reports,
                &import_errors,
                &days,
                inspected.len(),
            );
            report.publish(journal)?;
            last_publish = Instant::now();
        }
        if owner_wakeup
            || candidates.iter().any(|task| {
                task.request
                    .priority
                    .is_some_and(|p| p.band <= WorkBand::Waiting)
            })
        {
            // Reconstruct an independent copy; TaskQueue owns submissions.
            let early = candidates
                .iter()
                .filter(|task| {
                    task.request
                        .priority
                        .is_some_and(|p| p.band <= WorkBand::Waiting)
                })
                .map(|task| DiscoveredTask {
                    guard: task.guard.clone(),
                    request: task.request.clone(),
                    confirmed_import: task.confirmed_import,
                })
                .collect();
            admit(journal, queue, stopped, early, &mut report)?;
        }
    }
    aggregate(
        &mut report,
        &day_reports,
        &import_errors,
        &days,
        inspected.len(),
    );
    for import in imports.imports {
        let segments = import
            .segments
            .intersection(&fulfilled_segments)
            .cloned()
            .collect::<BTreeSet<_>>();
        let days = import
            .days
            .intersection(&fulfilled_days)
            .cloned()
            .collect::<BTreeSet<_>>();
        if !segments.is_empty() || !days.is_empty() {
            solstone_core_import::thinking::fulfill(journal, &import, segments, days)?;
        }
    }
    report.complete = report.unreadable.is_empty() && report.days_inspected == report.days_total;
    admit(journal, queue, stopped, candidates, &mut report)?;
    if stopped.load(Ordering::Acquire) {
        return Ok(());
    }
    report.as_of_ms = chrono::Utc::now().timestamp_millis();
    report.next_pass_ms =
        automatic.then(|| report.as_of_ms + rest_after(pass_started.elapsed()).as_millis() as i64);
    report.publish(journal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solstone_core_system::queue::{ProcessState, ProcessStateProbe, TaskQueueOptions};

    struct NoProcesses;
    impl ProcessStateProbe for NoProcesses {
        fn state(&self, _: u32) -> ProcessState {
            panic!("unready fixture must not launch")
        }
    }
    fn queue(journal: &Path) -> TaskQueue {
        TaskQueue::new(TaskQueueOptions {
            #[cfg(windows)]
            read_file_grants: Vec::new(),
            journal_root: journal.into(),
            cap_resolver: Arc::new(solstone_core_system::cap::DefaultCapResolver::default()),
            process_state_probe: Arc::new(NoProcesses),
            queue_sink: None,
            process_sink: None,
            ready: false,
            before_deadline_commit: None,
            child_environment: BTreeMap::new(),
            task_binary: None,
        })
    }
    fn fixture(provider: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("config")).unwrap();
        std::fs::write(root.path().join("config/journal.json"), serde_json::json!({
            "identity":{"timezone":"UTC"}, "providers":{"active":{"provider":provider,"model":"fixture"}}
        }).to_string()).unwrap();
        root
    }
    fn segment(root: &Path, day: &str, key: &str) {
        let path = root.join("chronicle").join(day).join("notes").join(key);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join("audio.jsonl"),
            "{\"text\":\"A meeting about a garden.\"}\n",
        )
        .unwrap();
    }

    #[test]
    fn capped_global_maintenance_is_discovered_on_today_without_recordings() {
        let root = fixture("local");
        let today = daily_coverage::local_day(root.path(), chrono::Utc::now());
        let unit = Unit::Daily {
            day: today.clone(),
            talent: "daily_schedule".into(),
            facet: None,
        };
        let evidence = work_retry::daily::evidence(root.path(), &unit).unwrap();
        let mut record = solstone_core_journal_io::DailyUnitRecord::new(
            work_retry::daily::identity(&unit).unwrap(),
            &evidence.revision,
            &evidence.contract,
        );
        record.status = solstone_core_journal_io::DailyUnitStatus::Capped;
        record.reason_code = Some("schema_invalid".into());
        solstone_core_journal_io::save_daily_unit_record(root.path(), &record).unwrap();
        let queue = queue(root.path());
        reconcile(root.path(), &queue, &AtomicBool::new(false)).unwrap();
        let id = unit.id().unwrap();
        assert!(work_retry::read(root.path(), &id).unwrap().unwrap().held);
        work_retry::request(root.path(), &unit).unwrap();
        reconcile(root.path(), &queue, &AtomicBool::new(false)).unwrap();
        assert!(queue.contains_reference(&format!("supervisor-retry-daily-{today}-{id}")));
        assert!(!queue.contains_reference(&format!("supervisor-work-{today}-daily")));
    }

    #[test]
    fn native_readmissions_keep_today_owner_and_background_priority_and_spend() {
        use solstone_core_system::work_policy::ThinkingLane;
        let root = fixture("local");
        let today = daily_coverage::local_day(root.path(), chrono::Utc::now());
        let local = WorkPolicy {
            lane: ThinkingLane::Bundled,
            history_confirmed: false,
        };
        let own_keys = WorkPolicy {
            lane: ThinkingLane::OwnKeys,
            history_confirmed: false,
        };
        let confidential = WorkPolicy {
            lane: ThinkingLane::Confidential,
            history_confirmed: false,
        };
        let mut selected = Vec::new();
        for (day, expected) in [
            (today.as_str(), WorkBand::Now),
            ("19990101", WorkBand::Completing),
        ] {
            let directory = root
                .path()
                .join("chronicle")
                .join(day)
                .join("notes/120000_60");
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("audio.wav"), "changed source").unwrap();
            std::fs::write(directory.join("audio.jsonl"), format!("{}\n", serde_json::json!({
                "raw":"audio.wav", "_solstone_processing": {
                    "schema":"solstone.processing.v1",
                    "handler":"transcribe", "state":"failed", "reason_code":"analysis_failed", "attempts":3,
                    "input_sha256":"a".repeat(64)
                }
            }))).unwrap();
            let (_, _, segments) = solstone_core_system_health::scan_day(
                &solstone_core_system_health::FilesystemSegmentSource,
                root.path(),
                day,
                chrono::Utc::now(),
            )
            .unwrap();
            assert_eq!(segments.len(), 1);
            let mut report = WorkInventory::default();
            let tasks = raw_retries(
                root.path(),
                day,
                &segments[0],
                None,
                &[],
                local,
                &BTreeSet::new(),
                &mut report,
            )
            .unwrap();
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].request.priority.unwrap().band, expected);
            selected.extend(tasks);
            if day != today {
                for policy in [own_keys, confidential] {
                    let mut report = WorkInventory::default();
                    assert!(
                        raw_retries(
                            root.path(),
                            day,
                            &segments[0],
                            None,
                            &[],
                            policy,
                            &BTreeSet::new(),
                            &mut report
                        )
                        .unwrap()
                        .is_empty()
                    );
                    assert_eq!(report.bands[4].permission_held, 1);
                }
                let intent = work_intent::register(root.path(), day, false).unwrap();
                let tasks = raw_retries(
                    root.path(),
                    day,
                    &segments[0],
                    Some(&intent),
                    &[],
                    own_keys,
                    &BTreeSet::new(),
                    &mut WorkInventory::default(),
                )
                .unwrap();
                assert_eq!(tasks.len(), 1);
                assert_eq!(tasks[0].request.priority.unwrap().band, WorkBand::Waiting);
                selected.extend(tasks);
            }
        }
        let mut ranked = Vec::new();
        retain_best(&mut ranked, selected);
        assert_eq!(
            ranked
                .iter()
                .map(|task| task.request.priority.unwrap().band)
                .collect::<Vec<_>>(),
            [WorkBand::Now, WorkBand::Waiting, WorkBand::Completing]
        );
    }

    #[test]
    fn discovery_reaches_old_segments_with_no_adoption_and_never_queues_their_day_first() {
        let root = fixture("local");
        for day in [
            "19990101", "20000101", "20010101", "20020101", "20030101", "20040101",
        ] {
            segment(root.path(), day, "120000_60");
        }
        let queue = queue(root.path());
        for state in [
            None,
            Some("{"),
            Some("{\"version\":999,\"first_closed_day\":\"20990101\"}"),
        ] {
            if let Some(state) = state {
                std::fs::write(root.path().join("health/daily-adoption.json"), state).unwrap();
            }
            reconcile(root.path(), &queue, &AtomicBool::new(false)).unwrap();
            let report = solstone_core_system::work_inventory::read(root.path())
                .unwrap()
                .unwrap();
            assert_eq!(report.days_total, 6);
            assert_eq!(report.days_inspected, 6);
            assert_eq!(report.oldest_day.as_deref(), Some("19990101"));
            assert_eq!(report.bands[4].segments_owed, 6);
            assert_eq!(queue.collect_queue_counts()["pending"], 4);
            assert!(queue.contains_reference("supervisor-work-20040101-notes-120000_60"));
            assert!(!queue.contains_reference("supervisor-work-19990101-notes-120000_60"));
            assert!(!queue.contains_reference("supervisor-work-20040101-daily"));
        }
    }

    #[test]
    fn other_importers_records_and_oversized_capture_rows_leave_the_inventory_complete() {
        let root = fixture("local");
        segment(root.path(), "19990101", "120000_60");
        // A captured page whose first row is longer than any processing header.
        let row = serde_json::json!({"text": "x".repeat(64 * 1024)});
        std::fs::write(
            root.path()
                .join("chronicle/19990101/notes/120000_60/browser_mail-example-com.jsonl"),
            format!("{row}\n"),
        )
        .unwrap();
        let import = root.path().join("imports/19990101_120000");
        std::fs::create_dir_all(&import).unwrap();
        std::fs::write(
            import.join("imported.json"),
            serde_json::json!({
                "processed_timestamp": "19990101_120000",
                "target_day": "19990101",
                "segments": ["120000_60"]
            })
            .to_string(),
        )
        .unwrap();
        reconcile(root.path(), &queue(root.path()), &AtomicBool::new(false)).unwrap();
        let report = solstone_core_system::work_inventory::read(root.path())
            .unwrap()
            .unwrap();
        assert!(report.unreadable.is_empty(), "{:?}", report.unreadable);
        assert_eq!(report.days_inspected, report.days_total);
        assert!(report.complete);
    }

    #[test]
    fn historical_activity_retries_share_the_bound_and_survive_a_malformed_sibling() {
        use sha2::{Digest, Sha256};
        let root = fixture("local");
        let directory = root.path().join("health/activity-work");
        std::fs::create_dir_all(&directory).unwrap();
        for day in [
            "19990101", "20000101", "20010101", "20020101", "20030101", "20040101",
        ] {
            let identity = solstone_core_think_cli::ActivityRetry {
                day: day.into(),
                facet: "work".into(),
                activity: "reading_090000".into(),
            };
            let path = directory.join(format!(
                "{:x}.json",
                Sha256::digest(serde_json::to_vec(&identity).unwrap())
            ));
            std::fs::write(path, serde_json::json!({"version":1,"identity":identity,"input_hash":"fixture","remaining":["participation"],"uses":{},"attempts":1,"next_attempt_ms":0}).to_string()).unwrap();
        }
        std::fs::write(directory.join("broken.json"), "{").unwrap();
        let queue = queue(root.path());
        for _ in 0..3 {
            reconcile(root.path(), &queue, &AtomicBool::new(false)).unwrap();
            assert_eq!(queue.collect_queue_counts()["pending"], 4);
            let report = solstone_core_system::work_inventory::read(root.path())
                .unwrap()
                .unwrap();
            assert!(!report.complete);
            assert!(
                report
                    .unreadable
                    .iter()
                    .any(|error| error.contains("broken.json"))
            );
            assert_eq!(report.bands[5].activities_owed, 6);
            assert!(
                queue.any_reference(
                    |reference| reference.starts_with("supervisor-activity-20040101-")
                )
            );
            assert!(
                !queue.any_reference(
                    |reference| reference.starts_with("supervisor-activity-19990101-")
                )
            );
        }
        work_intent::register(root.path(), "19990101", false).unwrap();
        reconcile(root.path(), &queue, &AtomicBool::new(false)).unwrap();
        assert!(
            queue.any_reference(|reference| reference.starts_with("supervisor-activity-19990101-"))
        );
        let report = solstone_core_system::work_inventory::read(root.path())
            .unwrap()
            .unwrap();
        assert_eq!(report.bands[1].activities_owed, 1);
        assert_eq!(queue.collect_queue_counts()["pending"], 4);
    }

    #[test]
    fn owner_request_replaces_old_waiting_work_and_own_key_history_waits_for_confirmation() {
        let root = fixture("openai");
        segment(root.path(), "19990101", "120000_60");
        let queue = queue(root.path());
        reconcile(root.path(), &queue, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            queue
                .collect_queue_counts()
                .get("pending")
                .copied()
                .unwrap_or(0),
            0
        );
        let report = solstone_core_system::work_inventory::read(root.path())
            .unwrap()
            .unwrap();
        assert_eq!(report.bands[4].permission_held, 1);
        work_intent::register(root.path(), "19990101", false).unwrap();
        reconcile(root.path(), &queue, &AtomicBool::new(false)).unwrap();
        assert!(queue.contains_reference("supervisor-work-19990101-notes-120000_60"));
        let report = solstone_core_system::work_inventory::read(root.path())
            .unwrap()
            .unwrap();
        assert_eq!(report.bands[1].segments_owed, 1);
        assert_eq!(report.bands[4].segments_owed, 0);
    }

    #[test]
    fn claimed_from_scratch_is_visible_and_shutdown_prevents_admission() {
        let root = fixture("local");
        segment(root.path(), "19990101", "120000_60");
        let intent = work_intent::register(root.path(), "19990101", true).unwrap();
        work_intent::claim_from_scratch(root.path(), "19990101", &intent.id).unwrap();
        let queue = queue(root.path());
        reconcile(root.path(), &queue, &AtomicBool::new(false)).unwrap();
        let report = solstone_core_system::work_inventory::read(root.path())
            .unwrap()
            .unwrap();
        assert!(
            report
                .holds
                .iter()
                .any(|hold| hold.reason == "interrupted_from_scratch")
        );
        assert_eq!(
            queue
                .collect_queue_counts()
                .get("pending")
                .copied()
                .unwrap_or(0),
            0
        );
        work_intent::register(root.path(), "19990101", true).unwrap();
        reconcile(root.path(), &queue, &AtomicBool::new(true)).unwrap();
        assert_eq!(
            queue
                .collect_queue_counts()
                .get("pending")
                .copied()
                .unwrap_or(0),
            0
        );
    }

    #[test]
    fn confirmed_import_does_not_authorize_an_unrelated_segment_on_the_same_day() {
        use solstone_core_import::publish::*;
        let root = fixture("openai");
        let day = "19990101";
        segment(root.path(), day, "120000_60");
        segment(root.path(), day, "130000_60");
        let directory = root.path().join("imports/fixture");
        std::fs::create_dir_all(&directory).unwrap();
        let record = PublicationRecord {
            schema: PUBLICATION_SCHEMA.into(),
            status: PublicationStatus::Success,
            segments: vec![SegmentPublicationOutcome {
                day: day.into(),
                stream: "notes".into(),
                segment: "120000_60".into(),
                outcome: SegmentBindingOutcome::Bound {
                    prev_day: None,
                    prev_segment: None,
                    seq: 1,
                },
            }],
            indexing: IndexPublicationOutcomes::default(),
            day_markers: Vec::new(),
        };
        write_publication_record(Some(&directory), &record).unwrap();
        let queue = queue(root.path());
        reconcile(root.path(), &queue, &AtomicBool::new(false)).unwrap();
        assert!(queue.contains_reference("supervisor-work-19990101-notes-120000_60"));
        assert!(!queue.contains_reference("supervisor-work-19990101-notes-130000_60"));
        assert!(!queue.contains_reference("supervisor-work-19990101-daily"));
        let report = solstone_core_system::work_inventory::read(root.path())
            .unwrap()
            .unwrap();
        assert_eq!(report.bands[3].segments_owed, 1);
        assert_eq!(report.bands[4].permission_held, 1);
        // Unknown existing output is protected even after spending permission.
        let protected = root
            .path()
            .join("chronicle/19990101/notes/120000_60/talents/activity.md");
        std::fs::create_dir_all(protected.parent().unwrap()).unwrap();
        std::fs::write(&protected, "owner text without generated evidence").unwrap();
        reconcile(root.path(), &queue, &AtomicBool::new(false)).unwrap();
        assert!(!queue.contains_reference("supervisor-work-19990101-notes-120000_60"));
        let report = solstone_core_system::work_inventory::read(root.path())
            .unwrap()
            .unwrap();
        assert!(
            report
                .holds
                .iter()
                .any(|h| h.segment.as_deref() == Some("120000_60")
                    && h.reason == "owner_output_protected")
        );
        assert_eq!(
            std::fs::read_to_string(protected).unwrap(),
            "owner text without generated evidence"
        );
    }
    #[cfg(all(unix, feature = "full-tests"))]
    #[test]
    fn blocked_discovery_coalesces_wakes_and_does_not_hold_task_admission_or_status() {
        use solstone_core_system::request::ExecutionRequest;
        let root = fixture("local");
        use solstone_core_system::lifecycle::{
            CoordinatorBootstrap, DeclaredParent, ParentLossCoordinator,
        };
        let parent = DeclaredParent::capture_current().unwrap();
        let (_coordinator, _) = ParentLossCoordinator::bootstrap(CoordinatorBootstrap {
            journal: root.path().into(),
            supervisor: parent.instance(),
            enabled: Vec::new(),
            supervisor_heartbeat_filename: format!(
                "solstone-v2-discovery-test-{}.check",
                std::process::id()
            ),
            capability: format!("discovery-test-{}", root.path().display()).into_bytes(),
        })
        .unwrap();
        use solstone_core_system::queue::{
            SystemProcessStateProbe, TaskQueueEvent, TaskQueueEventSink,
        };
        struct Events(mpsc::Sender<TaskQueueEvent>);
        impl TaskQueueEventSink for Events {
            fn emit(&self, event: TaskQueueEvent) {
                let _ = self.0.send(event);
            }
        }
        let (events, observed) = mpsc::channel();
        let queue = TaskQueue::new(TaskQueueOptions {
            journal_root: root.path().into(),
            cap_resolver: Arc::new(solstone_core_system::cap::DefaultCapResolver::default()),
            process_state_probe: Arc::new(SystemProcessStateProbe),
            queue_sink: Some(Arc::new(Events(events))),
            process_sink: None,
            ready: true,
            before_deadline_commit: None,
            child_environment: BTreeMap::new(),
            task_binary: None,
        });
        let (entered, received) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let worker = WorkDiscovery::with_reconciler(
            root.path().into(),
            queue.clone(),
            false,
            move |_, _, _| {
                entered.send(()).unwrap();
                blocked.recv().unwrap();
                Ok(())
            },
        );
        worker.request();
        received.recv_timeout(Duration::from_secs(5)).unwrap();
        for _ in 0..100 {
            worker.request();
        }
        let mut live = task(
            "20261011",
            Some(("120000_60", "desktop")),
            WorkPriority {
                band: WorkBand::Now,
                source_time: 20261011120000,
            },
            None,
            false,
        );
        // A real bounded child reaches the worker while the discovery read is
        // blocked. The separate supervisor race harness covers capture routing.
        live.request.cmd = TaskArgv::from_wire(vec!["/bin/sleep".into(), "0.1".into()]).unwrap();
        let reference = live.request.reference.clone();
        assert_eq!(
            queue.submit(ExecutionRequest::Bus(live.request)),
            solstone_core_system::queue::SubmitOutcome::Dispatched,
            "{:?}",
            queue.collect_status_snapshot(std::time::Instant::now())
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match observed
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .unwrap()
            {
                TaskQueueEvent::Started {
                    reference: started, ..
                } if started == reference => break,
                _ => {}
            }
        }
        let _status = queue.collect_status_snapshot(std::time::Instant::now());
        assert!(std::time::Instant::now() < deadline);
        release.send(()).unwrap();
        received.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.stop();
        release.send(()).unwrap();
        assert!(received.recv_timeout(Duration::from_secs(5)).is_err());
        queue.shutdown();
    }
}
