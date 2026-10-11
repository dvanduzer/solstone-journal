// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Execute approved raw-release marks after proving them again from disk.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use solstone_core_journal_io::{LockOptions, hold_lock};

use crate::class::partition_empty_audio;
use crate::content::{ClosedHandlerSet, JournalMedia};
use crate::door::release_raw;
use crate::eligibility::{Blocker, FoundContent, ProvenRaw};
use crate::grouped::{DecisionPurpose, SideDecision};
use crate::mark_mutation::{PlannedMarkMutation, conditional_mutate};
use crate::marks::{
    Failure, Mark, MarkId, PreflightMarks, PreflightRefusal, RemovalClass, load as load_marks,
};
use crate::original_deletion::RawReleaseClass;
use crate::receipt::{NotRemoved, Outcome, RunHalt, TargetOutcome};
use crate::scan::scan_segment;
use crate::{Eligibility, Policy};

const TOO_YOUNG: &str = "this one isn't old enough to delete yet.";
const KEPT_FOREVER: &str = "this one is kept indefinitely.";
const ANCHOR_MISSING: &str = "there's no date on this one, so it can't be deleted.";
const NOT_ON_REMOVAL_LIST: &str =
    "this file was proven releasable but is not on the removal list, so it is left in place";
const NO_LONGER_PRESENT: &str = "this file was on the removal list but is no longer present";
const NO_LONGER_ON_REMOVAL_LIST: &str =
    "this entry is no longer on the removal list, so nothing was removed";

struct RemovalContext<'a> {
    policy: &'a Policy,
    today: NaiveDate,
    now: DateTime<Utc>,
    register_errors: &'a mut Vec<String>,
}

/// Execute marks already proven valid by preflight.
///
/// A row describes a target the run reached; `halted` describes the target it
/// could not start and every requested target after it.
pub fn remove_marked(
    journal: &Path,
    marks: &PreflightMarks,
    policy: &Policy,
    today: NaiveDate,
    now: DateTime<Utc>,
    register_errors: &mut Vec<String>,
) -> Outcome {
    let mut context = RemovalContext {
        policy,
        today,
        now,
        register_errors,
    };
    let mut outcome = Outcome {
        targets: Vec::new(),
        halted: None,
    };
    for (index, (id, mark)) in marks.as_slice().iter().enumerate() {
        match remove_one(journal, id, mark, &mut context) {
            Ok(row) => outcome.targets.push(row),
            Err(()) => {
                let remaining = marks.as_slice().len().saturating_sub(index);
                let reason = format!(
                    "the originals for {} couldn't be started on because something else is using them. the rest of the removal list wasn't attempted ({remaining} remaining).",
                    id.as_str()
                );
                if outcome.targets.is_empty() {
                    return Outcome::halted_before_start(reason);
                }
                outcome.halted = Some(RunHalt { reason });
                break;
            }
        }
    }
    outcome
}

fn remove_one(
    journal: &Path,
    id: &MarkId,
    mark: &Mark,
    context: &mut RemovalContext<'_>,
) -> Result<TargetOutcome, ()> {
    let target = &mark.target;
    if mark.offload.is_some() {
        return Ok(refused(
            mark,
            &PreflightRefusal::ArchiveBound { id: id.clone() }.to_string(),
        ));
    }
    let live = crate::layout::segment_rel(&target.day, &target.stream, &target.dir);
    let _segment_lock = hold_lock(journal.join(&live), LockOptions::default()).map_err(|_| ())?;
    let (row, complete, staged) = {
        let register = match load_marks(journal) {
            Ok(register) => register,
            Err(error) => return Ok(refused(mark, &error.to_string())),
        };
        if register.version != 1 {
            return Ok(refused(
                mark,
                &crate::marks::StoreError::WriterHeld.to_string(),
            ));
        }
        if register.marks.get(id) != Some(mark) {
            return Ok(refused(mark, NO_LONGER_ON_REMOVAL_LIST));
        }
        let raw_class = match mark.class {
            RemovalClass::PolicyRawRelease => RawReleaseClass::Policy,
            RemovalClass::OffloadRawRelease => RawReleaseClass::Offload,
            RemovalClass::OwnerRawRelease | RemovalClass::OwnerSegmentRemoval => {
                let reason =
                    "this mark is not an original-media deletion that can be approved from the removal list"
                        .to_owned();
                let not_removed = mark
                    .proposal
                    .names
                    .iter()
                    .map(|name| NotRemoved {
                        entry: crate::layout::content_rel(
                            &target.day,
                            &target.stream,
                            &target.dir,
                            name,
                        ),
                        reason: reason.clone(),
                        staged: None,
                    })
                    .collect();
                return Ok(TargetOutcome {
                    target: target.clone(),
                    removed: Vec::new(),
                    not_removed,
                    post_commit_failure: None,
                });
            }
        };
        let found = scan_segment(&journal.join(&live), &ClosedHandlerSet, &JournalMedia);
        let found_names = found
            .iter()
            .map(|item| item.name.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        let (empty, ordinary) = partition_empty_audio(found, &ClosedHandlerSet);
        let empty_side = decide_side(&empty, target, context);
        let ordinary_side = decide_side(&ordinary, target, context);
        let mut side_refusal = BTreeMap::new();
        record_side_refusal(&empty, &empty_side, &mut side_refusal);
        record_side_refusal(&ordinary, &ordinary_side, &mut side_refusal);
        let ready = ready_union(empty_side, ordinary_side);
        let proved_names = ready
            .iter()
            .map(|item| item.name().to_owned())
            .collect::<BTreeSet<_>>();
        let desired = mark
            .proposal
            .names
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let not_approved = ready
            .iter()
            .filter(|item| !desired.contains(item.name()))
            .map(|item| item.name().to_owned())
            .collect::<Vec<_>>();
        let ready = ready
            .into_iter()
            .filter(|item| desired.contains(item.name()))
            .collect::<Vec<_>>();
        let (partial, _) = release_raw(journal, &ready, raw_class, context.now);
        let mut row = partial.targets.into_iter().next().unwrap_or(TargetOutcome {
            target: target.clone(),
            removed: Vec::new(),
            not_removed: Vec::new(),
            post_commit_failure: None,
        });
        for name in not_approved {
            row.not_removed.push(NotRemoved {
                entry: crate::layout::content_rel(&target.day, &target.stream, &target.dir, &name),
                reason: NOT_ON_REMOVAL_LIST.to_owned(),
                staged: None,
            });
        }
        for name in &mark.proposal.names {
            if !found_names.contains(name.as_str()) {
                row.not_removed.push(NotRemoved {
                    entry: crate::layout::content_rel(
                        &target.day,
                        &target.stream,
                        &target.dir,
                        name,
                    ),
                    reason: NO_LONGER_PRESENT.to_owned(),
                    staged: None,
                });
            }
        }
        for name in &mark.proposal.names {
            if found_names.contains(name.as_str())
                && !proved_names.contains(name)
                && let Some(reason) = side_refusal.get(name)
            {
                row.not_removed.push(NotRemoved {
                    entry: crate::layout::content_rel(
                        &target.day,
                        &target.stream,
                        &target.dir,
                        name,
                    ),
                    reason: reason.clone(),
                    staged: None,
                });
            }
        }
        let removed = row
            .removed
            .iter()
            .map(|path| path.as_str())
            .collect::<BTreeSet<_>>();
        let accounted = mark.proposal.names.iter().all(|name| {
            let rel = crate::layout::content_rel(&target.day, &target.stream, &target.dir, name);
            removed.contains(rel.as_str())
                || row
                    .not_removed
                    .iter()
                    .any(|item| item.reason == NO_LONGER_PRESENT && item.entry == rel)
        });
        let complete = accounted && !row.not_removed.iter().any(|item| item.staged.is_some());
        let staged = row.not_removed.iter().find_map(|item| {
            item.staged
                .as_ref()
                .map(|path| (path.to_owned(), item.reason.to_owned()))
        });
        (row, complete, staged)
    };
    // Segment lock, then register lock, always: marks owns the second lock internally.
    let plan = if complete {
        Some(PlannedMarkMutation::resolve(mark))
    } else {
        staged.map(|(staged, reason)| {
            PlannedMarkMutation::failure(
                mark,
                Failure {
                    at: context.now.to_rfc3339_opts(SecondsFormat::Secs, true),
                    reason,
                    staged: Some(staged),
                },
            )
        })
    };
    if let Some(plan) = plan
        && let Err(error) = plan.and_then(|plan| conditional_mutate(journal, &plan))
    {
        context.register_errors.push(error.to_string());
    }
    Ok(row)
}

enum SideOutcome {
    None,
    Policy(Eligibility),
    Held(Vec<Blocker>),
    Proved(Vec<ProvenRaw>),
}

fn decide_side(
    side: &[FoundContent],
    target: &crate::Target,
    context: &RemovalContext<'_>,
) -> SideOutcome {
    match crate::grouped::decide_side(
        side,
        &ClosedHandlerSet,
        &JournalMedia,
        context.policy,
        target,
        context.today,
        context.now,
        DecisionPurpose::PolicyRelease,
    ) {
        SideDecision::NoMedia => SideOutcome::None,
        SideDecision::Kept(verdict) => SideOutcome::Policy(verdict),
        SideDecision::Unproven(blockers) => SideOutcome::Held(blockers),
        SideDecision::Eligible { proven, .. } => SideOutcome::Proved(proven),
    }
}

fn ready_union(empty: SideOutcome, ordinary: SideOutcome) -> Vec<ProvenRaw> {
    let mut ready = match empty {
        SideOutcome::Proved(proven) => proven,
        _ => Vec::new(),
    };
    if let SideOutcome::Proved(proven) = ordinary {
        ready.extend(proven);
    }
    ready
}

fn record_side_refusal(
    side: &[FoundContent],
    outcome: &SideOutcome,
    side_refusal: &mut BTreeMap<String, String>,
) {
    let reason = match outcome {
        SideOutcome::Policy(Eligibility::TooYoung { .. }) => TOO_YOUNG.to_owned(),
        SideOutcome::Policy(Eligibility::KeptForever) => KEPT_FOREVER.to_owned(),
        SideOutcome::Policy(Eligibility::AnchorMissing { .. }) => ANCHOR_MISSING.to_owned(),
        SideOutcome::Policy(Eligibility::Eligible { .. })
        | SideOutcome::None
        | SideOutcome::Proved(_) => {
            return;
        }
        SideOutcome::Held(blockers) => format!(
            "the current processing proof no longer permits this release: {}",
            blockers
                .iter()
                .map(|blocker| blocker.name())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    for item in side {
        side_refusal.insert(item.name.as_str().to_owned(), reason.clone());
    }
}

fn refused(mark: &Mark, reason: &str) -> TargetOutcome {
    TargetOutcome {
        target: mark.target.clone(),
        removed: Vec::new(),
        not_removed: vec![NotRemoved {
            entry: crate::layout::segment_rel(
                &mark.target.day,
                &mark.target.stream,
                &mark.target.dir,
            ),
            reason: reason.to_owned(),
            staged: None,
        }],
        post_commit_failure: None,
    }
}

const REVIEW_REFUSED: &str =
    "the originals couldn't be deleted because their current state couldn't be confirmed.";
#[cfg(unix)]
const REVIEW_UNCERTAIN: &str = "deletion couldn't be completed or confirmed. recovery is needed.";
const REVIEW_POST_COMMIT: &str =
    "this file was deleted, but the change or its records couldn't be fully saved.";
const REVIEW_HALTED: &str =
    "the remaining originals were left in place because the current removal couldn't be completed.";

/// Diagnostic facts; no variant supplies release or cleanup authority.
#[derive(Debug)]
pub enum ReviewedRemovalRefusal {
    Unsupported,
    InvalidSelection,
    Io(std::io::Error),
    Mutation(crate::mark_mutation::BoundMutationRefusal),
    Activity(solstone_core_journal_io::activity::ActivityAdmissionError),
    Census(solstone_core_journal_io::activity_obligations::RecoveryObservationError),
    Obligations(solstone_core_journal_io::activity_obligations::NativeObligations),
    Configuration(solstone_core_journal_config::ConfigLoadError),
    #[cfg(unix)]
    Directory(solstone_core_journal_io::FlatDirectoryError),
    Changed,
    CarrierPreparation,
}

impl From<std::io::Error> for ReviewedRemovalRefusal {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
#[cfg(unix)]
impl From<solstone_core_journal_io::FlatDirectoryError> for ReviewedRemovalRefusal {
    fn from(value: solstone_core_journal_io::FlatDirectoryError) -> Self {
        Self::Directory(value)
    }
}
impl From<crate::mark_mutation::BoundMutationRefusal> for ReviewedRemovalRefusal {
    fn from(value: crate::mark_mutation::BoundMutationRefusal) -> Self {
        Self::Mutation(value)
    }
}

/// Actual per-file owners and bookkeeping. Drop never repairs an uncertain claim.
pub struct ReviewedFileRemoval {
    relative: String,
    class: RemovalClass,
    native: solstone_core_journal_io::media::MediaRemoval,
    #[cfg(unix)]
    append: Option<solstone_core_journal_io::append::BoundAppendReceipt>,
    #[cfg(unix)]
    marker: Option<
        Result<
            solstone_core_journal_io::health_marker::BoundStreamMarkerPublication,
            solstone_core_journal_io::health_marker::BoundStreamMarkerError,
        >,
    >,
    binding: Option<std::io::Error>,
    #[cfg(unix)]
    settled: bool,
}

impl ReviewedFileRemoval {
    pub fn relative(&self) -> &str {
        &self.relative
    }
    pub fn class(&self) -> RemovalClass {
        self.class
    }
    pub fn native(&self) -> &solstone_core_journal_io::media::MediaRemoval {
        &self.native
    }
    #[cfg(unix)]
    pub fn append(&self) -> Option<&solstone_core_journal_io::append::BoundAppendReceipt> {
        self.append.as_ref()
    }
    #[cfg(unix)]
    pub fn marker(
        &self,
    ) -> Option<
        &Result<
            solstone_core_journal_io::health_marker::BoundStreamMarkerPublication,
            solstone_core_journal_io::health_marker::BoundStreamMarkerError,
        >,
    > {
        self.marker.as_ref()
    }
    pub fn binding_error(&self) -> Option<&std::io::Error> {
        self.binding.as_ref()
    }
}

struct ReviewedExecution {
    outcome: Outcome,
    refusal: Option<ReviewedRemovalRefusal>,
    carrier: Option<solstone_core_journal_io::media::RootActivityCarrier>,
    carrier_failure: Option<solstone_core_journal_io::media::ActivityCarrierPreparationFailure>,
    attempts: Vec<ReviewedFileRemoval>,
    stage_failures: Vec<
        solstone_core_journal_io::media::OwnedMediaFailure<
            Option<solstone_core_journal_io::media::MediaStage>,
        >,
    >,
    mutation: Option<
        Result<
            crate::mark_mutation::BoundMutationOutcome,
            crate::mark_mutation::BoundMutationRefusal,
        >,
    >,
}

/// Owns the original review and every local effect through the caller's settlement.
/// This additive entry does not retire its carrier or certify hold-free recovery.
#[must_use]
pub struct ReviewedRemoval {
    checked: crate::review_context::CheckedReviewContext,
    execution: ReviewedExecution,
}

impl ReviewedRemoval {
    pub fn outcome(&self) -> &Outcome {
        &self.execution.outcome
    }
    pub fn refusal(&self) -> Option<&ReviewedRemovalRefusal> {
        self.execution.refusal.as_ref()
    }
    pub fn attempts(&self) -> &[ReviewedFileRemoval] {
        &self.execution.attempts
    }
    pub fn stage_failures(
        &self,
    ) -> &[solstone_core_journal_io::media::OwnedMediaFailure<
        Option<solstone_core_journal_io::media::MediaStage>,
    >] {
        &self.execution.stage_failures
    }
    pub fn mutation(
        &self,
    ) -> Option<
        &Result<
            crate::mark_mutation::BoundMutationOutcome,
            crate::mark_mutation::BoundMutationRefusal,
        >,
    > {
        self.execution.mutation.as_ref()
    }
    pub fn carrier_failure(
        &self,
    ) -> Option<&solstone_core_journal_io::media::ActivityCarrierPreparationFailure> {
        self.execution.carrier_failure.as_ref()
    }
    pub fn carrier_preparation(
        &self,
    ) -> Option<&solstone_core_journal_io::media::ActivityCarrierPreparationReceipt> {
        self.execution.carrier.as_ref().map(|c| c.preparation())
    }
    pub fn journal_root(&self) -> &solstone_core_journal_io::JournalRoot {
        self.checked.root()
    }
}

/// Consume rendered ordinary actions beneath their original retained root.
/// Typed writers and platform/carrier settlement activation remain held.
pub fn remove_reviewed(
    checked: crate::review_context::CheckedReviewContext,
    today: NaiveDate,
    now: DateTime<Utc>,
) -> ReviewedRemoval {
    let mut execution = ReviewedExecution {
        outcome: Outcome {
            targets: Vec::new(),
            halted: None,
        },
        refusal: None,
        carrier: None,
        carrier_failure: None,
        attempts: Vec::new(),
        stage_failures: Vec::new(),
        mutation: None,
    };
    #[cfg(unix)]
    let result = execute_reviewed(&checked, today, now, &mut execution);
    #[cfg(not(unix))]
    let result: Result<(), ReviewedRemovalRefusal> = {
        let _parameters = (today, now);
        Err(ReviewedRemovalRefusal::Unsupported)
    };
    if let Err(error) = result {
        execution.refusal = Some(error);
        reviewed_mark_unsaved_deletions(&mut execution.outcome);
        execution.outcome.halted = Some(RunHalt {
            reason: if execution.attempts.is_empty() {
                REVIEW_REFUSED
            } else {
                REVIEW_HALTED
            }
            .to_owned(),
        });
    }
    ReviewedRemoval { checked, execution }
}

fn reviewed_mark_unsaved_deletions(outcome: &mut Outcome) {
    for row in &mut outcome.targets {
        if let Some(removed) = row.removed.first() {
            row.post_commit_failure = Some(crate::receipt::PostCommitFailure {
                entry: removed.as_str().to_owned(),
                reason: REVIEW_POST_COMMIT.to_owned(),
            });
        }
    }
}

#[cfg(unix)]
const REVIEW_MAX_ENTRIES: usize = 65_536;
#[cfg(unix)]
const REVIEW_MAX_CONFIG: usize = 1024 * 1024;

#[cfg(unix)]
struct ReviewedPolicy {
    root: std::sync::Arc<solstone_core_journal_io::JournalRoot>,
    directory: std::sync::Arc<solstone_core_journal_io::media::MediaDirectory>,
    parent: solstone_core_journal_io::FlatDirectory,
    lock: solstone_core_journal_io::BoundParentLock,
    read: solstone_core_journal_config::JournalConfigRead,
    entry: Option<solstone_core_journal_io::media::MediaEntry>,
    policy: Policy,
}

#[cfg(unix)]
impl ReviewedPolicy {
    fn capture(
        root: &std::sync::Arc<solstone_core_journal_io::JournalRoot>,
    ) -> Result<Self, ReviewedRemovalRefusal> {
        use solstone_core_journal_io::media::MediaDirectory;
        use solstone_core_journal_io::{
            FlatDirectory, acquire_existing_parent_lock_bound, open_existing_parent_lock_bound,
        };
        use std::ffi::OsStr;
        use std::time::Duration;
        let directory = MediaDirectory::open(std::sync::Arc::clone(root), Path::new("config"))?;
        let parent = FlatDirectory::open(root, Path::new("config"))?;
        if parent.identity() != directory.object_identity() {
            return Err(ReviewedRemovalRefusal::Changed);
        }
        let name = OsStr::new("journal.json.lock");
        let lock =
            match open_existing_parent_lock_bound(&parent, name, Duration::ZERO, Duration::ZERO)
                .map_err(|e| ReviewedRemovalRefusal::Io(std::io::Error::other(e)))?
            {
                Some(lock) => lock,
                None => acquire_existing_parent_lock_bound(
                    &parent,
                    name,
                    Duration::ZERO,
                    Duration::ZERO,
                )
                .map_err(|e| ReviewedRemovalRefusal::Io(std::io::Error::other(e)))?,
            };
        lock.revalidate_bound(&parent, name)
            .map_err(|e| ReviewedRemovalRefusal::Io(std::io::Error::other(e)))?;
        let (read, entry) =
            solstone_core_journal_config::read_journal_config_observed_in_directory(
                &directory,
                REVIEW_MAX_CONFIG,
            )
            .map_err(ReviewedRemovalRefusal::Configuration)?;
        let config = read
            .config
            .as_ref()
            .ok_or(ReviewedRemovalRefusal::Changed)?;
        let policy = crate::policy::policy_from_journal_config(config);
        let admitted = Self {
            root: std::sync::Arc::clone(root),
            directory,
            parent,
            lock,
            read,
            entry,
            policy,
        };
        admitted.revalidate()?;
        Ok(admitted)
    }
    fn revalidate(&self) -> Result<(), ReviewedRemovalRefusal> {
        self.root
            .revalidate_canonical_binding()
            .map_err(|e| ReviewedRemovalRefusal::Io(std::io::Error::other(e)))?;
        self.directory.revalidate()?;
        self.parent.revalidate()?;
        let current_parent =
            solstone_core_journal_io::FlatDirectory::open(&self.root, Path::new("config"))?;
        if current_parent.identity() != self.parent.identity() {
            return Err(ReviewedRemovalRefusal::Changed);
        }
        self.lock
            .revalidate_bound(&self.parent, std::ffi::OsStr::new("journal.json.lock"))
            .map_err(|e| ReviewedRemovalRefusal::Io(std::io::Error::other(e)))?;
        let (read, entry) =
            solstone_core_journal_config::read_journal_config_observed_in_directory(
                &self.directory,
                REVIEW_MAX_CONFIG,
            )
            .map_err(ReviewedRemovalRefusal::Configuration)?;
        if read != self.read || entry != self.entry {
            return Err(ReviewedRemovalRefusal::Changed);
        }
        self.directory.revalidate()?;
        Ok(())
    }
}

#[cfg(unix)]
struct ReviewedGroup {
    target: crate::Target,
    directory: std::sync::Arc<solstone_core_journal_io::media::MediaDirectory>,
    parent: std::sync::Arc<solstone_core_journal_io::media::MediaDirectory>,
    events: solstone_core_journal_io::FlatDirectory,
    health: solstone_core_journal_io::FlatDirectory,
    scan: crate::scan::CheckedSegment,
    files: BTreeMap<String, (ProvenRaw, solstone_core_journal_io::media::MediaObservation)>,
    refusals: BTreeMap<String, String>,
    deleted: BTreeSet<String>,
}

#[cfg(unix)]
fn reviewed_scan(
    directory: &solstone_core_journal_io::media::MediaDirectory,
) -> std::io::Result<crate::scan::CheckedSegment> {
    crate::scan::scan_segment_bound(
        directory,
        &ClosedHandlerSet,
        &JournalMedia,
        REVIEW_MAX_ENTRIES,
    )
}

#[cfg(unix)]
fn revalidate_reviewed_group(group: &ReviewedGroup) -> Result<(), ReviewedRemovalRefusal> {
    group.directory.revalidate()?;
    group.parent.revalidate()?;
    let current = reviewed_scan(&group.directory)?;
    let expected = group
        .scan
        .files()
        .iter()
        .filter(|file| !group.deleted.contains(file.name.as_str()))
        .collect::<Vec<_>>();
    if expected.len() != current.files().len()
        || expected.iter().zip(current.files()).any(|(a, b)| {
            a.name != b.name
                || a.size != b.size
                || a.sidecar.record != b.sidecar.record
                || a.sidecar.has_analysis_row != b.sidecar.has_analysis_row
                || group.scan.bound_entry(&a.name) != current.bound_entry(&b.name)
        })
    {
        return Err(ReviewedRemovalRefusal::Changed);
    }
    Ok(())
}

#[cfg(unix)]
fn valid_reviewed_selection(marks: &[Mark]) -> bool {
    use crate::content::{ContentName, MediaClassifier};
    let plain = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && !s.chars().any(|c| c.is_control() || c == '/' || c == '\\')
    };
    !marks.is_empty()
        && marks.iter().all(|mark| {
            mark.offload.is_none()
                && matches!(
                    mark.class,
                    RemovalClass::PolicyRawRelease | RemovalClass::OffloadRawRelease
                )
                && mark.target.day.len() == 8
                && mark.target.day.bytes().all(|b| b.is_ascii_digit())
                && NaiveDate::parse_from_str(&mark.target.day, "%Y%m%d").is_ok()
                && plain(&mark.target.stream)
                && plain(&mark.target.dir)
                && mark.proposal.names.iter().all(|name| {
                    plain(name)
                        && ContentName::new(name)
                            .is_some_and(|name| JournalMedia.is_owner_media(&name))
                })
        })
}

/// Minted only by the current ordinary executor after its full retained checks.
#[cfg(unix)]
pub(crate) struct OrdinaryRelease {
    source: std::sync::Arc<solstone_core_journal_io::media::MediaDirectory>,
    proven: ProvenRaw,
    observation: solstone_core_journal_io::media::MediaObservation,
}

#[cfg(unix)]
impl OrdinaryRelease {
    pub(crate) fn into_parts(
        self,
    ) -> (
        std::sync::Arc<solstone_core_journal_io::media::MediaDirectory>,
        ProvenRaw,
        solstone_core_journal_io::media::MediaObservation,
    ) {
        (self.source, self.proven, self.observation)
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReviewedCheckpoint {
    Hashed,
    BeforeRaw,
    AfterRaw,
    BeforeBookkeeping,
    BeforeFinalCas,
}

#[cfg(unix)]
fn execute_reviewed(
    checked: &crate::review_context::CheckedReviewContext,
    today: NaiveDate,
    now: DateTime<Utc>,
    execution: &mut ReviewedExecution,
) -> Result<(), ReviewedRemovalRefusal> {
    execute_reviewed_with(checked, today, now, execution, |_, _| {})
}

#[cfg(unix)]
fn execute_reviewed_with(
    checked: &crate::review_context::CheckedReviewContext,
    today: NaiveDate,
    now: DateTime<Utc>,
    execution: &mut ReviewedExecution,
    mut checkpoint: impl FnMut(ReviewedCheckpoint, &Path),
) -> Result<(), ReviewedRemovalRefusal> {
    use crate::mark_mutation::CheckedMarkGuard;
    use solstone_core_journal_io::activity::ActivityLease;
    use solstone_core_journal_io::media::{
        Cleanup, Confirmation, MediaDirectory, MediaStage, OriginalName, RemovalEffect,
        RootActivityCarrier,
    };
    use solstone_core_segment::output_attempt::OutputClaims;
    use std::ffi::OsStr;
    use std::sync::Arc;
    use std::time::Duration;

    if !valid_reviewed_selection(checked.marks()) {
        return Err(ReviewedRemovalRefusal::InvalidSelection);
    }
    let root = Arc::clone(checked.root_arc());
    let activity =
        ActivityLease::try_acquire(Arc::clone(&root)).map_err(ReviewedRemovalRefusal::Activity)?;
    let obligations = solstone_core_journal_io::activity_obligations::observe_obligations(&root)
        .map_err(ReviewedRemovalRefusal::Census)?;
    if !obligations.entries().is_empty() {
        return Err(ReviewedRemovalRefusal::Obligations(obligations));
    }
    match RootActivityCarrier::prepare(Arc::clone(&activity)) {
        Ok(carrier) => execution.carrier = Some(carrier),
        Err(failure) => {
            execution.carrier_failure = Some(failure);
            return Err(ReviewedRemovalRefusal::CarrierPreparation);
        }
    }
    let mut targets = checked
        .marks()
        .iter()
        .map(|m| m.target.clone())
        .collect::<Vec<_>>();
    targets.sort_by_key(|t| crate::layout::segment_rel(&t.day, &t.stream, &t.dir));
    targets.dedup();
    let options = LockOptions {
        timeout: Duration::ZERO,
        ..LockOptions::default()
    };
    let mut claims = Vec::new();
    let mut directories = Vec::new();
    for target in &targets {
        let relative = crate::layout::segment_rel(&target.day, &target.stream, &target.dir);
        let directory = MediaDirectory::open(Arc::clone(&root), Path::new(&relative))?;
        let path = root.canonical_path().join(&relative);
        let claim = OutputClaims::discover(&path, options)?;
        if claim.segment_identity(&path)? != directory.object_identity() {
            return Err(ReviewedRemovalRefusal::Changed);
        }
        claims.push(claim);
        directories.push(directory);
    }
    let mut commits = Vec::new();
    for (target, claim) in targets.iter().zip(&claims) {
        let relative = crate::layout::segment_rel(&target.day, &target.stream, &target.dir);
        commits.push(claim.commit(&root.canonical_path().join(relative))?);
    }
    let policy = ReviewedPolicy::capture(&root)?;
    let mut groups = Vec::new();
    for (target, directory) in targets.into_iter().zip(directories) {
        let parent = MediaDirectory::open(
            Arc::clone(&root),
            Path::new(&crate::layout::stream_rel(&target.day, &target.stream)),
        )?;
        let events = solstone_core_journal_io::FlatDirectory::open(
            &root,
            Path::new(&crate::layout::segment_rel(
                &target.day,
                &target.stream,
                &target.dir,
            )),
        )?;
        let health = solstone_core_journal_io::FlatDirectory::open(
            &root,
            Path::new(&crate::layout::day_health_rel(&target.day)),
        )?;
        if events.identity() != directory.object_identity() {
            return Err(ReviewedRemovalRefusal::Changed);
        }
        let scan = reviewed_scan(&directory)?;
        let (empty, ordinary) = partition_empty_audio(scan.files().to_vec(), &ClosedHandlerSet);
        let decide = |side: &[FoundContent]| match crate::grouped::decide_side(
            side,
            &ClosedHandlerSet,
            &JournalMedia,
            &policy.policy,
            &target,
            today,
            now,
            DecisionPurpose::PolicyRelease,
        ) {
            SideDecision::NoMedia => SideOutcome::None,
            SideDecision::Kept(verdict) => SideOutcome::Policy(verdict),
            SideDecision::Unproven(blockers) => SideOutcome::Held(blockers),
            SideDecision::Eligible { proven, .. } => SideOutcome::Proved(proven),
        };
        let empty_decision = decide(&empty);
        let ordinary_decision = decide(&ordinary);
        let mut refusals = BTreeMap::new();
        record_side_refusal(&empty, &empty_decision, &mut refusals);
        record_side_refusal(&ordinary, &ordinary_decision, &mut refusals);
        let desired = checked
            .marks()
            .iter()
            .filter(|m| m.target == target)
            .flat_map(|m| m.proposal.names.iter().cloned())
            .collect::<BTreeSet<_>>();
        let mut files = BTreeMap::new();
        for proven in ready_union(empty_decision, ordinary_decision)
            .into_iter()
            .filter(|p| desired.contains(p.name()))
        {
            let name = crate::content::ContentName::new(proven.name())
                .ok_or(ReviewedRemovalRefusal::InvalidSelection)?;
            let entry = scan
                .bound_entry(&name)
                .ok_or(ReviewedRemovalRefusal::Changed)?;
            let before = directory
                .read_prefix(OsStr::new(proven.name()), 0)?
                .ok_or(ReviewedRemovalRefusal::Changed)?;
            if before.entry != *entry {
                return Err(ReviewedRemovalRefusal::Changed);
            }
            let observed = directory
                .observe_file(OsStr::new(proven.name()))?
                .ok_or(ReviewedRemovalRefusal::Changed)?;
            let after = directory
                .read_prefix(OsStr::new(proven.name()), 0)?
                .ok_or(ReviewedRemovalRefusal::Changed)?;
            if after.entry != *entry
                || observed.identity != entry.identity
                || observed.digest.size != proven.size()
            {
                return Err(ReviewedRemovalRefusal::Changed);
            }
            checkpoint(
                ReviewedCheckpoint::Hashed,
                &directory.diagnostic_path().join(proven.name()),
            );
            files.insert(proven.name().to_owned(), (proven, observed));
        }
        let group = ReviewedGroup {
            target,
            directory,
            parent,
            events,
            health,
            scan,
            files,
            refusals,
            deleted: BTreeSet::new(),
        };
        revalidate_reviewed_group(&group)?;
        groups.push(group);
    }
    for commit in &commits {
        commit.revalidate()?;
    }
    policy.revalidate()?;
    execution
        .carrier
        .as_ref()
        .ok_or(ReviewedRemovalRefusal::Changed)?
        .revalidate()?;
    let mut attempted: BTreeMap<(usize, String), usize> = BTreeMap::new();
    let mut stage_failed: BTreeMap<(usize, String), usize> = BTreeMap::new();
    let mut stop = false;
    // This guard ends before any event/marker bookkeeping or final reacquisition.
    {
        let register = CheckedMarkGuard::acquire(checked)?;
        for mark in checked.marks() {
            let group_index = groups
                .iter()
                .position(|g| g.target == mark.target)
                .ok_or(ReviewedRemovalRefusal::Changed)?;
            let group = groups
                .get_mut(group_index)
                .ok_or(ReviewedRemovalRefusal::Changed)?;
            let mut row = TargetOutcome {
                target: mark.target.clone(),
                removed: Vec::new(),
                not_removed: Vec::new(),
                post_commit_failure: None,
            };
            for name in &mark.proposal.names {
                if stop {
                    row.not_removed
                        .push(reviewed_not_removed(mark, name, REVIEW_HALTED, None));
                    continue;
                }
                if attempted.contains_key(&(group_index, name.clone()))
                    || stage_failed.contains_key(&(group_index, name.clone()))
                {
                    continue;
                }
                if !group.scan.files().iter().any(|f| f.name.as_str() == name) {
                    row.not_removed
                        .push(reviewed_not_removed(mark, name, NO_LONGER_PRESENT, None));
                    continue;
                }
                let Some((proven, observed)) = group.files.get(name).cloned() else {
                    row.not_removed.push(reviewed_not_removed(
                        mark,
                        name,
                        group
                            .refusals
                            .get(name)
                            .map_or(REVIEW_REFUSED, String::as_str),
                        None,
                    ));
                    continue;
                };
                checkpoint(
                    ReviewedCheckpoint::BeforeRaw,
                    &group.directory.diagnostic_path().join(name),
                );
                let recheck = (|| {
                    register.revalidate()?;
                    policy.revalidate()?;
                    execution
                        .carrier
                        .as_ref()
                        .ok_or(ReviewedRemovalRefusal::Changed)?
                        .revalidate()?;
                    revalidate_reviewed_group(group)
                })();
                if let Err(error) = recheck {
                    execution.refusal = Some(error);
                    stop = true;
                    row.not_removed
                        .push(reviewed_not_removed(mark, name, REVIEW_REFUSED, None));
                    continue;
                }
                let stage =
                    match MediaStage::create(Arc::clone(&group.parent), Arc::clone(&activity)) {
                        Ok(stage) => stage,
                        Err(failure) => {
                            let staged = failure.owned.as_ref().map(|stage| {
                                stage
                                    .diagnostic_path()
                                    .strip_prefix(root.canonical_path())
                                    .unwrap_or_else(|_| Path::new(""))
                                    .to_string_lossy()
                                    .into_owned()
                            });
                            let index = execution.stage_failures.len();
                            execution.stage_failures.push(failure);
                            stage_failed.insert((group_index, name.clone()), index);
                            row.not_removed.push(reviewed_not_removed(
                                mark,
                                name,
                                REVIEW_UNCERTAIN,
                                staged,
                            ));
                            stop = true;
                            continue;
                        }
                    };
                let release = OrdinaryRelease {
                    source: Arc::clone(&group.directory),
                    proven,
                    observation: observed,
                };
                let released = crate::door::release_reviewed_ordinary(release, stage);
                let confirmed = released.removed.is_some();
                if let Some(removed) = released.removed {
                    row.removed.push(removed);
                    group.deleted.insert(name.clone());
                }
                let receipt = released.native.receipt();
                let unresolved = !matches!(
                    receipt.effect,
                    RemovalEffect::NotClaimed | RemovalEffect::Deleted
                ) || receipt.cleanup != Cleanup::Complete
                    || (receipt.effect == RemovalEffect::Deleted && !confirmed)
                    || receipt.original_name == OriginalName::Unverified;
                let staged = if receipt.cleanup != Cleanup::Complete {
                    reviewed_stage_path(&released.native)
                } else {
                    None
                };
                if !confirmed {
                    row.not_removed.push(reviewed_not_removed(
                        mark,
                        name,
                        REVIEW_UNCERTAIN,
                        staged,
                    ));
                }
                let index = execution.attempts.len();
                execution.attempts.push(ReviewedFileRemoval {
                    relative: crate::layout::content_rel(
                        &mark.target.day,
                        &mark.target.stream,
                        &mark.target.dir,
                        name,
                    ),
                    class: mark.class,
                    native: released.native,
                    append: None,
                    marker: None,
                    binding: None,
                    settled: false,
                });
                attempted.insert((group_index, name.clone()), index);
                checkpoint(
                    ReviewedCheckpoint::AfterRaw,
                    &group.directory.diagnostic_path().join(name),
                );
                if unresolved {
                    stop = true;
                }
            }
            execution.outcome.targets.push(row);
        }
    }
    checkpoint(ReviewedCheckpoint::BeforeBookkeeping, root.canonical_path());
    for ((group_index, name), index) in &attempted {
        let group = groups
            .get(*group_index)
            .ok_or(ReviewedRemovalRefusal::Changed)?;
        let attempt = execution
            .attempts
            .get_mut(*index)
            .ok_or(ReviewedRemovalRefusal::Changed)?;
        let native = attempt.native.receipt();
        if native.effect == RemovalEffect::Deleted
            && native.original_name == OriginalName::Absent
            && native.claim_confirmation == Confirmation::Confirmed
        {
            reviewed_bookkeeping(&root, group, name, now, attempt);
        }
    }
    // All overlapping rows share the single physical operation's settlement facts.
    let mut plans = Vec::new();
    for (mark, row) in checked.marks().iter().zip(&mut execution.outcome.targets) {
        let group_index = groups
            .iter()
            .position(|g| g.target == mark.target)
            .ok_or(ReviewedRemovalRefusal::Changed)?;
        let group = groups
            .get(group_index)
            .ok_or(ReviewedRemovalRefusal::Changed)?;
        let mut complete = true;
        let mut failed = false;
        let mut staged = None;
        for name in &mark.proposal.names {
            if let Some(index) = attempted.get(&(group_index, name.clone())) {
                let attempt = execution
                    .attempts
                    .get(*index)
                    .ok_or(ReviewedRemovalRefusal::Changed)?;
                if !attempt.settled {
                    complete = false;
                    let receipt = attempt.native.receipt();
                    if receipt.effect != RemovalEffect::NotClaimed
                        || receipt.cleanup != Cleanup::Complete
                    {
                        failed = true;
                        if staged.is_none() && receipt.cleanup != Cleanup::Complete {
                            staged = reviewed_stage_path(&attempt.native);
                        }
                    }
                    if receipt.effect == RemovalEffect::Deleted {
                        row.post_commit_failure = Some(crate::receipt::PostCommitFailure {
                            entry: attempt.relative.clone(),
                            reason: REVIEW_POST_COMMIT.to_owned(),
                        });
                    } else if !row
                        .not_removed
                        .iter()
                        .any(|item| item.entry == attempt.relative)
                    {
                        row.not_removed.push(reviewed_not_removed(
                            mark,
                            name,
                            REVIEW_UNCERTAIN,
                            reviewed_stage_path(&attempt.native),
                        ));
                    }
                }
            } else if let Some(index) = stage_failed.get(&(group_index, name.clone())) {
                complete = false;
                failed = true;
                staged = execution
                    .stage_failures
                    .get(*index)
                    .and_then(|f| f.owned.as_ref())
                    .map(|s| {
                        s.diagnostic_path()
                            .strip_prefix(root.canonical_path())
                            .unwrap_or_else(|_| Path::new(""))
                            .to_string_lossy()
                            .into_owned()
                    });
            } else if group.scan.files().iter().any(|f| f.name.as_str() == name) {
                complete = false;
            }
        }
        let plan = if failed {
            Some(PlannedMarkMutation::failure(
                mark,
                Failure {
                    at: now.to_rfc3339_opts(SecondsFormat::Secs, true),
                    reason: REVIEW_UNCERTAIN.to_owned(),
                    staged,
                },
            ))
        } else if complete {
            Some(PlannedMarkMutation::resolve(mark))
        } else {
            None
        };
        if let Some(plan) = plan {
            plans.push(plan.map_err(|e| {
                ReviewedRemovalRefusal::Mutation(
                    crate::mark_mutation::BoundMutationRefusal::Review(
                        crate::review_context::ReviewRefusal::Register(e),
                    ),
                )
            })?);
        }
    }
    checkpoint(ReviewedCheckpoint::BeforeFinalCas, root.canonical_path());
    let final_check = (|| {
        policy.revalidate()?;
        execution
            .carrier
            .as_ref()
            .ok_or(ReviewedRemovalRefusal::Changed)?
            .revalidate()?;
        Ok::<(), ReviewedRemovalRefusal>(())
    })();
    if let Err(error) = final_check {
        reviewed_mark_unsaved_deletions(&mut execution.outcome);
        return Err(error);
    }
    execution.mutation =
        Some(CheckedMarkGuard::acquire(checked).and_then(|guard| guard.publish(&plans)));
    if !matches!(
        execution.mutation.as_ref(),
        Some(Ok(crate::mark_mutation::BoundMutationOutcome::Published(_)
            | crate::mark_mutation::BoundMutationOutcome::Unchanged(_)))
    ) {
        reviewed_mark_unsaved_deletions(&mut execution.outcome);
    }
    if stop {
        execution.outcome.halted = Some(RunHalt {
            reason: REVIEW_HALTED.to_owned(),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn reviewed_not_removed(
    mark: &Mark,
    name: &str,
    reason: &str,
    staged: Option<String>,
) -> NotRemoved {
    NotRemoved {
        entry: crate::layout::content_rel(
            &mark.target.day,
            &mark.target.stream,
            &mark.target.dir,
            name,
        ),
        reason: reason.to_owned(),
        staged,
    }
}

#[cfg(unix)]
fn reviewed_stage_path(removal: &solstone_core_journal_io::media::MediaRemoval) -> Option<String> {
    let stage = removal.stage_observation()?;
    let mut relative = std::path::PathBuf::new();
    for part in &stage.parent_relative {
        relative.push(part);
    }
    relative.push(&stage.directory_name);
    Some(relative.to_string_lossy().into_owned())
}

#[cfg(unix)]
fn reviewed_bookkeeping(
    root: &std::sync::Arc<solstone_core_journal_io::JournalRoot>,
    group: &ReviewedGroup,
    name: &str,
    now: DateTime<Utc>,
    attempt: &mut ReviewedFileRemoval,
) {
    use solstone_core_journal_io::media::{Cleanup, Durability};
    use std::ffi::OsStr;
    let bindings = || -> std::io::Result<()> {
        root.revalidate_canonical_binding()
            .map_err(std::io::Error::other)?;
        group.directory.revalidate()?;
        group.events.revalidate().map_err(std::io::Error::other)?;
        group.health.revalidate().map_err(std::io::Error::other)?;
        let events = solstone_core_journal_io::FlatDirectory::open(
            root,
            Path::new(&crate::layout::segment_rel(
                &group.target.day,
                &group.target.stream,
                &group.target.dir,
            )),
        )
        .map_err(std::io::Error::other)?;
        let health = solstone_core_journal_io::FlatDirectory::open(
            root,
            Path::new(&crate::layout::day_health_rel(&group.target.day)),
        )
        .map_err(std::io::Error::other)?;
        if events.identity() != group.events.identity()
            || health.identity() != group.health.identity()
        {
            return Err(std::io::Error::other("removal bookkeeping binding changed"));
        }
        Ok(())
    };
    if let Err(error) = bindings() {
        attempt.binding = Some(error);
        return;
    }
    let class = match attempt.class {
        RemovalClass::PolicyRawRelease => RawReleaseClass::Policy,
        _ => RawReleaseClass::Offload,
    };
    let mut extra = serde_json::Map::new();
    extra.insert(
        "name".to_owned(),
        serde_json::Value::String(name.to_owned()),
    );
    extra.insert(
        "class".to_owned(),
        serde_json::Value::String(class.tag().to_owned()),
    );
    let event = solstone_core_callosum::CallosumEnvelope {
        tract: "retention".to_owned(),
        event: "original_deleted".to_owned(),
        ts: Some(now.timestamp_millis()),
        extra,
    };
    attempt.append = Some(solstone_core_journal_io::append::append_jsonl_bound(
        &group.events,
        OsStr::new("events.jsonl"),
        &event,
    ));
    attempt.marker = Some(
        solstone_core_journal_io::health_marker::bump_stream_marker_bound(
            &group.health,
            LockOptions {
                timeout: std::time::Duration::ZERO,
                ..LockOptions::default()
            },
        ),
    );
    attempt.binding = bindings().err();
    let native = attempt.native.receipt();
    let native_settled = native.file_durability == Durability::Synced
        && native.source_durability == Durability::Synced
        && native.claim_durability == Durability::Synced
        && native.stage_parent_durability == Durability::Synced
        && native.cleanup == Cleanup::Complete;
    let append_settled = attempt.append.as_ref().is_some_and(|a| {
        a.confirmation() == solstone_core_journal_io::append::BoundAppendConfirmation::Confirmed
            && a.file_durability()
                == solstone_core_journal_io::append::BoundAppendDurability::Synced
            && a.parent_durability()
                == solstone_core_journal_io::append::BoundAppendDurability::Synced
            && a.failures().is_empty()
    });
    let marker_settled = attempt.marker.as_ref().is_some_and(|m| match m {
        Ok(m) => {
            matches!(
                m.publication(),
                solstone_core_journal_io::BoundAtomicOutcome::Published { .. }
            ) && m.binding_error().is_none()
                && m.lock_error().is_none()
        }
        Err(_) => false,
    });
    attempt.settled =
        native_settled && append_settled && marker_settled && attempt.binding.is_none();
}

#[cfg(all(test, unix, feature = "full-tests"))]
#[path = "../tests/support/reviewed_removal.rs"]
mod reviewed_tests;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "test setup and assertions use concise infallible helpers"
)]
mod tests {
    use std::fs;
    use std::path::Path;

    use chrono::TimeZone;

    use super::*;
    use crate::marks::{Proposal, RemovalClass, decline, preflight, reconcile};
    use crate::policy::policy_from_retention;
    use serde_json::json;

    #[test]
    fn a_mark_declined_after_preflight_is_not_unlinked() {
        let journal = tempfile::tempdir().unwrap();
        let target = crate::Target {
            day: "20260701".to_owned(),
            stream: "field.audio".to_owned(),
            dir: "070000_17".to_owned(),
        };
        let segment = journal.path().join(crate::layout::segment_rel(
            &target.day,
            &target.stream,
            &target.dir,
        ));
        fs::create_dir_all(&segment).unwrap();
        let raw = segment.join("audio.flac");
        fs::write(&raw, b"the owner's originals").unwrap();
        let proposal = Proposal {
            bytes: 1,
            reason: "test approval".to_owned(),
            names: vec!["audio.flac".to_owned()],
        };
        let register = reconcile(
            journal.path(),
            RemovalClass::PolicyRawRelease,
            &[(target.clone(), proposal.clone())],
            crate::marks::mark_at("first"),
        )
        .unwrap();
        let id = register.marks.keys().next().unwrap().clone();
        let marks = preflight(journal.path(), std::slice::from_ref(&id)).unwrap();
        decline(journal.path(), &id).unwrap();
        let policy = Policy {
            default_rule: crate::Rule {
                anchor: crate::Anchor::Captured,
                period: Some(crate::Days(1)),
                priority: 0,
            },
            enabled: true,
            ..Policy::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 8, 6, 0, 0, 0).single().unwrap();
        let mut register_errors = Vec::new();
        let outcome = remove_marked(
            journal.path(),
            &marks,
            &policy,
            today,
            now,
            &mut register_errors,
        );

        assert!(raw.exists());
        assert!(register_errors.is_empty());
        assert_eq!(outcome.targets.len(), 1);
        assert_eq!(outcome.targets[0].removed, Vec::new());
        assert_eq!(outcome.targets[0].not_removed.len(), 1);
        assert_eq!(
            outcome.targets[0].not_removed[0].reason,
            NO_LONGER_ON_REMOVAL_LIST
        );
    }

    fn target() -> crate::Target {
        crate::Target {
            day: "20260701".to_owned(),
            stream: "field.audio".to_owned(),
            dir: "070000_17".to_owned(),
        }
    }

    fn keep_journal_product_policy() -> Policy {
        policy_from_retention(json!({"raw_media": "keep"}).as_object().unwrap())
    }

    fn seed_empty_audio(journal: &Path, target: &crate::Target) -> std::path::PathBuf {
        let segment = journal.join(crate::layout::segment_rel(
            &target.day,
            &target.stream,
            &target.dir,
        ));
        fs::create_dir_all(&segment).unwrap();
        let raw = b"raw";
        fs::write(segment.join("audio.flac"), raw).unwrap();
        let header = json!({
            "segment": &target.dir,
            "_solstone_processing": {
                "schema": "solstone.processing.v1",
                "state": "empty",
                "reason_code": "no_decodable_audio",
                "handler": "transcribe",
                "attempted_at": "2026-07-01T00:00:00Z",
                "input_size": raw.len(),
            }
        });
        fs::write(segment.join("audio.jsonl"), format!("{header}\n")).unwrap();
        segment
    }

    fn seed_analyzed_sibling(segment: &Path, name: &str) {
        let raw = b"sibling";
        fs::write(segment.join(name), raw).unwrap();
        let stem = name.rsplit_once('.').unwrap().0;
        let header = json!({
            "segment": "070000_17",
            "_solstone_processing": {
                "schema": "solstone.processing.v1",
                "state": "analyzed",
                "reason_code": "ok",
                "handler": "transcribe",
                "attempted_at": "2026-07-01T00:00:00Z",
                "input_size": raw.len(),
            }
        });
        fs::write(
            segment.join(format!("{stem}.jsonl")),
            format!("{header}\n{{\"start\":0.0,\"text\":\"x\"}}\n"),
        )
        .unwrap();
    }

    fn approve(
        journal: &Path,
        names: Vec<String>,
        policy: &Policy,
    ) -> (crate::Outcome, Vec<String>) {
        let target = target();
        let proposal = Proposal {
            bytes: 1,
            reason: "test approval".to_owned(),
            names,
        };
        let register = reconcile(
            journal,
            RemovalClass::PolicyRawRelease,
            &[(target, proposal)],
            crate::marks::mark_at("first"),
        )
        .unwrap();
        let id = register.marks.keys().next().unwrap().clone();
        let marks = preflight(journal, std::slice::from_ref(&id)).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 8, 6, 0, 0, 0).single().unwrap();
        let mut register_errors = Vec::new();
        let outcome = remove_marked(journal, &marks, policy, today, now, &mut register_errors);
        (outcome, register_errors)
    }

    #[test]
    fn an_unnamed_ordinary_sibling_does_not_block_or_appear_on_an_empty_audio_mark() {
        let journal = tempfile::tempdir().unwrap();
        let target = target();
        let segment = seed_empty_audio(journal.path(), &target);
        seed_analyzed_sibling(&segment, "extra.flac");
        let audio = segment.join("audio.flac");
        let extra = segment.join("extra.flac");
        let policy = keep_journal_product_policy();
        let (outcome, register_errors) =
            approve(journal.path(), vec!["audio.flac".to_owned()], &policy);

        assert!(!audio.exists());
        assert!(extra.exists());
        assert!(register_errors.is_empty());
        assert_eq!(outcome.targets.len(), 1);
        assert_eq!(outcome.targets[0].removed.len(), 1);
        assert!(
            outcome.targets[0].removed[0]
                .as_str()
                .ends_with("audio.flac")
        );
        assert_eq!(outcome.targets[0].not_removed, Vec::new());
    }

    #[test]
    fn a_mark_naming_both_sides_releases_only_the_eligible_file() {
        let journal = tempfile::tempdir().unwrap();
        let target = target();
        let segment = seed_empty_audio(journal.path(), &target);
        seed_analyzed_sibling(&segment, "extra.flac");
        let audio = segment.join("audio.flac");
        let extra = segment.join("extra.flac");
        let policy = keep_journal_product_policy();
        let (outcome, register_errors) = approve(
            journal.path(),
            vec!["audio.flac".to_owned(), "extra.flac".to_owned()],
            &policy,
        );

        assert!(!audio.exists());
        assert!(extra.exists());
        assert!(register_errors.is_empty());
        assert_eq!(outcome.targets[0].removed.len(), 1);
        assert!(
            outcome.targets[0].removed[0]
                .as_str()
                .ends_with("audio.flac")
        );
        assert_eq!(outcome.targets[0].not_removed.len(), 1);
        assert!(
            outcome.targets[0].not_removed[0]
                .entry
                .ends_with("extra.flac")
        );
        assert_eq!(outcome.targets[0].not_removed[0].reason, KEPT_FOREVER);
    }

    #[test]
    fn a_named_file_that_is_no_longer_empty_terminal_is_refused_per_file() {
        let journal = tempfile::tempdir().unwrap();
        let target = target();
        let segment = seed_empty_audio(journal.path(), &target);
        let audio = segment.join("audio.flac");
        seed_analyzed_sibling(&segment, "audio.flac");
        let policy = keep_journal_product_policy();
        let (outcome, register_errors) =
            approve(journal.path(), vec!["audio.flac".to_owned()], &policy);

        assert!(audio.exists());
        assert!(register_errors.is_empty());
        assert_eq!(outcome.targets[0].removed, Vec::new());
        assert_eq!(outcome.targets[0].not_removed.len(), 1);
        assert!(
            outcome.targets[0].not_removed[0]
                .entry
                .ends_with("audio.flac")
        );
        assert_eq!(outcome.targets[0].not_removed[0].reason, KEPT_FOREVER);
    }

    #[test]
    fn ac3_a_policy_approval_writes_policy_record_in_events_jsonl() {
        let journal = tempfile::tempdir().unwrap();
        let target = target();
        let segment = seed_empty_audio(journal.path(), &target);
        let audio = segment.join("audio.flac");
        let policy = keep_journal_product_policy();
        let (outcome, register_errors) =
            approve(journal.path(), vec!["audio.flac".to_owned()], &policy);

        assert!(!audio.exists());
        assert!(register_errors.is_empty());
        assert_eq!(outcome.targets[0].removed.len(), 1);

        let events_path = segment.join("events.jsonl");
        assert!(events_path.is_file());
        let lines: Vec<String> = fs::read_to_string(&events_path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(lines.len(), 1);
        let row: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(row["tract"], "retention");
        assert_eq!(row["event"], "original_deleted");
        assert_eq!(row["name"], "audio.flac");
        assert_eq!(row["class"], "policy_raw_release");
    }

    #[test]
    fn ac3_b_offload_raw_release_records_offload_class_in_events_jsonl() {
        let journal = tempfile::tempdir().unwrap();
        let target = target();
        let segment = seed_empty_audio(journal.path(), &target);
        let now = Utc.with_ymd_and_hms(2026, 8, 6, 0, 0, 0).single().unwrap();
        let register = crate::marks::upsert_offload(
            journal.path(),
            &target,
            vec!["audio.flac".to_owned()],
            100,
            "backup offloaded".to_owned(),
            now,
        )
        .unwrap();

        let id = MarkId::derive(
            RemovalClass::OffloadRawRelease,
            &target,
            &["audio.flac".to_owned()],
        );
        let mark = register.marks.get(&id).unwrap().clone();
        let preflight = PreflightMarks::new_for_test(vec![(id, mark)]);
        let policy = keep_journal_product_policy();
        let today = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let mut register_errors = Vec::new();
        let outcome = remove_marked(
            journal.path(),
            &preflight,
            &policy,
            today,
            now,
            &mut register_errors,
        );

        assert_eq!(outcome.targets.len(), 1);
        assert_eq!(outcome.targets[0].removed.len(), 1);
        assert_eq!(
            outcome.targets[0].removed[0].as_str(),
            "chronicle/20260701/field.audio/070000_17/audio.flac"
        );
        assert!(!segment.join("audio.flac").exists());

        let events_path = segment.join("events.jsonl");
        assert!(events_path.exists());
        let content = fs::read_to_string(&events_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1);
        let row: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(row["tract"], "retention");
        assert_eq!(row["event"], "original_deleted");
        assert_eq!(row["name"], "audio.flac");
        assert_eq!(row["class"], "offload_raw_release");
        assert_eq!(row["ts"], now.timestamp_millis());
    }

    #[test]
    fn ac3_e_non_raw_release_mark_is_refused_with_exact_copy_and_does_not_unlink() {
        let journal = tempfile::tempdir().unwrap();
        let target = target();
        let segment = seed_empty_audio(journal.path(), &target);
        let audio = segment.join("audio.flac");
        let proposal = Proposal {
            bytes: 1,
            reason: "non raw mark".to_owned(),
            names: vec!["audio.flac".to_owned()],
        };
        let id = MarkId::derive(RemovalClass::OwnerRawRelease, &target, &proposal.names);
        let mark = Mark {
            id: id.clone(),
            class: RemovalClass::OwnerRawRelease,
            target: target.clone(),
            marked_at: "2026-08-06T00:00:00Z".to_owned(),
            proposal: proposal.clone(),
            state: crate::marks::MarkState::Marked,
            offload: None,
        };
        let mut reg = crate::marks::Register::empty();
        reg.marks.insert(id.clone(), mark.clone());
        let reg_path = journal.path().join("health/retention-marks.json");
        fs::create_dir_all(reg_path.parent().unwrap()).unwrap();
        fs::write(&reg_path, serde_json::to_string(&reg).unwrap()).unwrap();

        let preflight = PreflightMarks::new_for_test(vec![(id, mark)]);
        let policy = keep_journal_product_policy();
        let today = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 8, 6, 0, 0, 0).single().unwrap();
        let mut register_errors = Vec::new();
        let outcome = remove_marked(
            journal.path(),
            &preflight,
            &policy,
            today,
            now,
            &mut register_errors,
        );

        assert!(audio.exists());
        assert!(register_errors.is_empty());
        assert_eq!(outcome.targets[0].removed, Vec::new());
        assert_eq!(outcome.targets[0].not_removed.len(), 1);
        assert_eq!(
            outcome.targets[0].not_removed[0].reason,
            "this mark is not an original-media deletion that can be approved from the removal list"
        );
        assert!(!segment.join("events.jsonl").exists());
    }
}
