// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Retained current inputs for archive offers. Capture does not confirm an
//! archive, create consent, publish a Mark or authorize removal.

#[cfg(unix)]
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use solstone_core_backup::archive_activity::PreparedArchiveOperation;
use solstone_core_journal_io::JournalRoot;
use solstone_core_journal_io::activity::ActivityLease;
use solstone_core_journal_io::media::{MediaDirectory, MediaObservation};
use solstone_core_journal_io::paths::SegmentLayout;

use crate::archive_proof::AcceptedProcessingObservation;
#[cfg(unix)]
use crate::marks::{Mark, MarkId, RemovalClass};
use crate::receipt::Target;

/// Requested coordinates only; the owning capture independently reads them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OffloadOfferSelection {
    pub target: Target,
    pub layout: SegmentLayout,
    pub names: Vec<String>,
}

#[derive(Debug)]
pub enum OffloadOfferRefusal {
    Unsupported,
    InvalidSelection,
    Changed,
    Kept(crate::policy::Eligibility),
    Unproven,
    Admission(solstone_core_backup::archive_admission::AdmissionError),
    Io(std::io::Error),
    Review(crate::review_context::ReviewRefusal),
    Policy(Box<crate::remove_marked::ReviewedRemovalRefusal>),
}

impl From<std::io::Error> for OffloadOfferRefusal {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Actual retained bytes and accepted processing observed under writer claims.
pub struct ObservedOffloadFile {
    name: String,
    observation: MediaObservation,
    processing: AcceptedProcessingObservation,
}

impl ObservedOffloadFile {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn observation(&self) -> &MediaObservation {
        &self.observation
    }
    pub fn processing(&self) -> &AcceptedProcessingObservation {
        &self.processing
    }
}

pub struct ObservedOffloadGroup {
    selection: OffloadOfferSelection,
    directory: Arc<MediaDirectory>,
    #[cfg(unix)]
    owner: solstone_core_segment::RetainedOwnerDeletion,
    #[cfg(unix)]
    scan: crate::scan::CheckedSegment,
    files: Vec<ObservedOffloadFile>,
    #[cfg(unix)]
    expected: Option<Mark>,
}

impl ObservedOffloadGroup {
    pub fn selection(&self) -> &OffloadOfferSelection {
        &self.selection
    }
    pub fn directory(&self) -> &Arc<MediaDirectory> {
        &self.directory
    }
    pub fn files(&self) -> &[ObservedOffloadFile] {
        &self.files
    }
}

/// An owning observation, with no public constructor or archive authority.
#[must_use]
pub struct ObservedOffloadOffers {
    root: Arc<JournalRoot>,
    activity: Arc<ActivityLease>,
    groups: Vec<ObservedOffloadGroup>,
    #[cfg(unix)]
    configuration: solstone_core_journal_config::JournalConfigRead,
    #[cfg(unix)]
    config_entry: Option<solstone_core_journal_io::media::MediaEntry>,
}

impl ObservedOffloadOffers {
    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        &self.root
    }
    pub fn activity(&self) -> &Arc<ActivityLease> {
        &self.activity
    }
    pub fn groups(&self) -> &[ObservedOffloadGroup] {
        &self.groups
    }
}

#[cfg(unix)]
fn selected_id(selection: &OffloadOfferSelection) -> MarkId {
    MarkId::derive(
        RemovalClass::OffloadRawRelease,
        &selection.target,
        &selection.names,
    )
}

#[cfg(unix)]
fn selection_path(selection: &OffloadOfferSelection) -> String {
    crate::layout::segment_rel_with_layout(
        &selection.target.day,
        &selection.target.stream,
        &selection.target.dir,
        selection.layout,
    )
}

#[cfg(unix)]
fn validate_selections(selections: &[OffloadOfferSelection]) -> Result<(), OffloadOfferRefusal> {
    use std::collections::BTreeSet;

    use solstone_core_journal_io::paths::DEFAULT_STREAM;

    use crate::content::{ContentName, JournalMedia, MediaClassifier};

    let mut ids = BTreeSet::new();
    let mut files = BTreeSet::new();
    if selections.is_empty() {
        return Err(OffloadOfferRefusal::InvalidSelection);
    }
    for selection in selections {
        let target = &selection.target;
        if target.day.len() != 8
            || !target.day.bytes().all(|byte| byte.is_ascii_digit())
            || NaiveDate::parse_from_str(&target.day, "%Y%m%d").is_err()
            || !crate::archive_catalog::codec::component(&target.stream)
            || !crate::archive_catalog::codec::component(&target.dir)
            || (selection.layout == SegmentLayout::Direct && target.stream != DEFAULT_STREAM)
            || selection.names.is_empty()
            || selection.names.windows(2).any(|names| {
                names
                    .first()
                    .zip(names.get(1))
                    .is_some_and(|(left, right)| left >= right)
            })
            || !ids.insert(selected_id(selection))
        {
            return Err(OffloadOfferRefusal::InvalidSelection);
        }
        for name in &selection.names {
            if !crate::archive_catalog::codec::component(name)
                || !ContentName::new(name).is_some_and(|name| JournalMedia.is_owner_media(&name))
                || !files.insert((selection_path(selection), name))
            {
                return Err(OffloadOfferRefusal::InvalidSelection);
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn scan(directory: &MediaDirectory) -> Result<crate::scan::CheckedSegment, OffloadOfferRefusal> {
    Ok(crate::scan::scan_segment_bound(
        directory,
        &crate::content::ClosedHandlerSet,
        &crate::content::JournalMedia,
        4096,
    )?)
}

#[cfg(unix)]
fn check_group(group: &ObservedOffloadGroup) -> Result<(), OffloadOfferRefusal> {
    group.directory.revalidate()?;
    if group
        .owner
        .state()
        .map_err(|error| OffloadOfferRefusal::Io(std::io::Error::other(error)))?
        != solstone_core_segment::OwnerDeletionState::Live
    {
        return Err(OffloadOfferRefusal::Changed);
    }
    let current = scan(&group.directory)?;
    if current.files().len() != group.scan.files().len()
        || group
            .scan
            .files()
            .iter()
            .zip(current.files())
            .any(|(before, after)| {
                before.name != after.name
                    || before.size != after.size
                    || before.sidecar.record != after.sidecar.record
                    || before.sidecar.has_analysis_row != after.sidecar.has_analysis_row
                    || group.scan.bound_entry(&before.name) != current.bound_entry(&after.name)
            })
    {
        return Err(OffloadOfferRefusal::Changed);
    }
    Ok(())
}

#[cfg(unix)]
struct ClaimedSegment {
    relative: String,
    directory: Arc<MediaDirectory>,
    claims: solstone_core_segment::output_attempt::OutputClaims,
}

#[cfg(unix)]
fn acquire_claims(
    root: &Arc<JournalRoot>,
    selections: &[OffloadOfferSelection],
) -> Result<Vec<ClaimedSegment>, OffloadOfferRefusal> {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use solstone_core_journal_io::LockOptions;
    use solstone_core_segment::output_attempt::OutputClaims;

    let options = LockOptions {
        timeout: Duration::ZERO,
        ..LockOptions::default()
    };
    selections
        .iter()
        .map(selection_path)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|relative| {
            let directory = MediaDirectory::open(Arc::clone(root), Path::new(&relative))?;
            let absolute = root.canonical_path().join(&relative);
            let claims = OutputClaims::discover(&absolute, options)?;
            if claims.segment_identity(&absolute)? != directory.object_identity() {
                return Err(OffloadOfferRefusal::Changed);
            }
            Ok(ClaimedSegment {
                relative,
                directory,
                claims,
            })
        })
        .collect()
}

/// Capture each distinct selected file once, under the existing writer protocol.
/// The returned observation retains inputs and the original activity, not locks.
pub fn capture_offload_offers(
    operation: &PreparedArchiveOperation<'_>,
    selections: &[OffloadOfferSelection],
    today: NaiveDate,
    now: DateTime<Utc>,
) -> Result<ObservedOffloadOffers, OffloadOfferRefusal> {
    #[cfg(not(unix))]
    {
        let _parameters = (operation, selections, today, now);
        Err(OffloadOfferRefusal::Unsupported)
    }
    #[cfg(unix)]
    {
        use crate::grouped::{DecisionPurpose, inspect_grouped};
        use std::ffi::OsStr;

        validate_selections(selections)?;
        operation
            .revalidate()
            .map_err(OffloadOfferRefusal::Admission)?;
        let root = Arc::clone(operation.journal_root());
        let mut selections = selections.to_vec();
        selections.sort_by_key(selection_path);
        let claimed = acquire_claims(&root, &selections)?;
        let mut commits = Vec::new();
        for segment in &claimed {
            let absolute = root.canonical_path().join(&segment.relative);
            let commit = segment.claims.commit(&absolute)?;
            commit.require_outputs(
                &segment.claims,
                &absolute,
                selections
                    .iter()
                    .filter(|selection| selection_path(selection) == segment.relative)
                    .flat_map(|selection| selection.names.iter())
                    .map(|name| absolute.join(name).with_extension("jsonl")),
            )?;
            commits.push(commit);
        }
        let policy = crate::remove_marked::ReviewedPolicy::capture(&root)
            .map_err(|error| OffloadOfferRefusal::Policy(Box::new(error)))?;
        let register =
            crate::review_context::load_at_root(&root).map_err(OffloadOfferRefusal::Review)?;
        let mut groups = Vec::new();
        for selection in selections {
            let directory = Arc::clone(
                &claimed
                    .iter()
                    .find(|segment| segment.relative == selection_path(&selection))
                    .ok_or(OffloadOfferRefusal::Changed)?
                    .directory,
            );
            let parent_relative = crate::layout::stream_rel_with_layout(
                &selection.target.day,
                &selection.target.stream,
                selection.layout,
            );
            let parent = MediaDirectory::open(Arc::clone(&root), Path::new(&parent_relative))?;
            let owner = solstone_core_segment::RetainedOwnerDeletion::open(
                parent,
                OsStr::new(&selection.target.dir),
                Some(&directory),
                4096,
            )
            .map_err(|error| OffloadOfferRefusal::Io(std::io::Error::other(error)))?;
            if owner
                .state()
                .map_err(|error| OffloadOfferRefusal::Io(std::io::Error::other(error)))?
                != solstone_core_segment::OwnerDeletionState::Live
            {
                return Err(OffloadOfferRefusal::Changed);
            }
            let scan = scan(&directory)?;
            let inspection = inspect_grouped(
                &scan,
                &selection.target,
                selection.layout,
                policy.policy(),
                today,
                now,
                DecisionPurpose::OffloadProposal,
            );
            let mut files = Vec::new();
            for name in &selection.names {
                let content_name = crate::content::ContentName::new(name)
                    .ok_or(OffloadOfferRefusal::InvalidSelection)?;
                let entry = scan
                    .bound_entry(&content_name)
                    .ok_or(OffloadOfferRefusal::Changed)?;
                let accepted = inspection
                    .eligible()
                    .find(|file| file.name() == name)
                    .ok_or_else(|| {
                        let group = if inspection.empty().members().contains(name) {
                            inspection.empty()
                        } else {
                            inspection.ordinary()
                        };
                        match group.verdict() {
                            crate::grouped::GroupVerdict::Kept(verdict) => {
                                OffloadOfferRefusal::Kept(*verdict)
                            }
                            _ => OffloadOfferRefusal::Unproven,
                        }
                    })?;
                let before = directory
                    .read_prefix(OsStr::new(name), 0)?
                    .ok_or(OffloadOfferRefusal::Changed)?;
                if before.entry != *entry {
                    return Err(OffloadOfferRefusal::Changed);
                }
                let observation = directory
                    .observe_file(OsStr::new(name))?
                    .ok_or(OffloadOfferRefusal::Changed)?;
                let after = directory
                    .read_prefix(OsStr::new(name), 0)?
                    .ok_or(OffloadOfferRefusal::Changed)?;
                if after.entry != *entry
                    || observation.identity != entry.identity
                    || observation.digest.size != accepted.bytes()
                {
                    return Err(OffloadOfferRefusal::Changed);
                }
                files.push(ObservedOffloadFile {
                    name: name.clone(),
                    observation,
                    processing: accepted.processing().clone(),
                });
            }
            let expected = register.marks.get(&selected_id(&selection)).cloned();
            let group = ObservedOffloadGroup {
                selection,
                directory,
                owner,
                scan,
                files,
                expected,
            };
            check_group(&group)?;
            groups.push(group);
        }
        let (configuration, config_entry) = policy.observation();
        let observed = ObservedOffloadOffers {
            root,
            activity: Arc::clone(operation.activity()),
            groups,
            configuration: configuration.clone(),
            config_entry: config_entry.clone(),
        };
        for commit in &commits {
            commit.revalidate()?;
        }
        check_capture(&observed, operation, &policy)?;
        Ok(observed)
    }
}

#[cfg(unix)]
fn check_capture(
    observed: &ObservedOffloadOffers,
    operation: &PreparedArchiveOperation<'_>,
    policy: &crate::remove_marked::ReviewedPolicy,
) -> Result<(), OffloadOfferRefusal> {
    if !Arc::ptr_eq(&observed.root, operation.journal_root())
        || !Arc::ptr_eq(&observed.activity, operation.activity())
        || policy.observation() != (&observed.configuration, &observed.config_entry)
    {
        return Err(OffloadOfferRefusal::Changed);
    }
    operation
        .revalidate()
        .map_err(OffloadOfferRefusal::Admission)?;
    policy
        .revalidate()
        .map_err(|error| OffloadOfferRefusal::Policy(Box::new(error)))?;
    let current =
        crate::review_context::load_at_root(&observed.root).map_err(OffloadOfferRefusal::Review)?;
    for group in &observed.groups {
        if current.marks.get(&selected_id(&group.selection)) != group.expected.as_ref() {
            return Err(OffloadOfferRefusal::Changed);
        }
        check_group(group)?;
    }
    operation
        .revalidate()
        .map_err(OffloadOfferRefusal::Admission)
}

#[cfg(all(test, unix))]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "retained synthetic input observations exercise the real claim and capture entry"
)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use solstone_core_backup::archive_admission::{ArchiveBudget, Clock};
    use solstone_core_journal_io::{LockOptions, paths::DEFAULT_STREAM};
    use solstone_core_segment::output_attempt::OutputClaims;
    use std::fs;
    use std::time::Duration;

    struct FixedClock;
    impl Clock for FixedClock {
        fn now_unix(&self) -> i64 {
            1_791_590_400
        }
        fn iso_week(&self) -> u8 {
            41
        }
    }

    fn time() -> (NaiveDate, DateTime<Utc>) {
        (
            NaiveDate::from_ymd_opt(2026, 10, 10).unwrap(),
            Utc.with_ymd_and_hms(2026, 10, 10, 0, 0, 0).unwrap(),
        )
    }

    fn fixture() -> (tempfile::TempDir, Arc<JournalRoot>) {
        let journal = tempfile::tempdir().unwrap();
        fs::create_dir(journal.path().join("config")).unwrap();
        fs::write(journal.path().join("config/journal.json"), b"{}").unwrap();
        let root = Arc::new(JournalRoot::open(journal.path()).unwrap());
        (journal, root)
    }

    fn operation(root: &Arc<JournalRoot>) -> PreparedArchiveOperation<'static> {
        PreparedArchiveOperation::prepare(
            Arc::clone(root),
            ArchiveBudget::new(&FixedClock).unwrap(),
            &FixedClock,
        )
        .unwrap()
    }

    fn selection(layout: SegmentLayout, names: &[&str]) -> OffloadOfferSelection {
        OffloadOfferSelection {
            target: Target {
                day: "20261009".into(),
                stream: match layout {
                    SegmentLayout::Direct => DEFAULT_STREAM.into(),
                    SegmentLayout::Named => "camera".into(),
                },
                dir: "120000_60_suffix".into(),
            },
            layout,
            names: names.iter().map(|name| (*name).into()).collect(),
        }
    }

    fn seed(root: &JournalRoot, selection: &OffloadOfferSelection, name: &str, state: &str) {
        let directory = root.canonical_path().join(selection_path(selection));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(name), b"current").unwrap();
        let header = json!({"_solstone_processing": {
            "schema":"solstone.processing.v1", "handler":"transcribe",
            "state":state, "reason_code":if state == "empty" { "no_decodable_audio" } else { "ok" },
            "input_size":7, "attempted_at":"2026-10-09T00:00:00Z"
        }});
        let rows = if state == "analyzed" {
            "{\"start\":0,\"text\":\"words\"}\n"
        } else {
            ""
        };
        fs::write(
            directory.join(name).with_extension("jsonl"),
            format!("{header}\n{rows}"),
        )
        .unwrap();
    }

    fn capture(
        operation: &PreparedArchiveOperation<'_>,
        selections: &[OffloadOfferSelection],
    ) -> Result<ObservedOffloadOffers, OffloadOfferRefusal> {
        let (today, now) = time();
        capture_offload_offers(operation, selections, today, now)
    }

    #[test]
    fn actual_capture_both_layouts_absent_register_and_released_claims() {
        for layout in [SegmentLayout::Direct, SegmentLayout::Named] {
            let (_journal, root) = fixture();
            let selected = selection(layout, &["audio.flac"]);
            seed(&root, &selected, "audio.flac", "analyzed");
            let operation = operation(&root);
            let observed = capture(&operation, std::slice::from_ref(&selected)).unwrap();
            assert!(Arc::ptr_eq(
                observed.journal_root(),
                operation.journal_root()
            ));
            assert!(Arc::ptr_eq(observed.activity(), operation.activity()));
            assert_eq!(observed.groups().len(), 1);
            let group = &observed.groups()[0];
            assert!(group.expected.is_none());
            assert_eq!(group.files()[0].observation().digest.size, 7);
            assert_eq!(
                group.files()[0].observation().digest.sha256,
                format!("{:x}", Sha256::digest(b"current"))
            );
            assert!(group.files()[0].processing().has_analysis_row);
            assert!(
                !root
                    .canonical_path()
                    .join("health/retention-marks.json")
                    .exists()
            );
            let path = root.canonical_path().join(selection_path(&selected));
            let claims = OutputClaims::discover(
                &path,
                LockOptions {
                    timeout: Duration::ZERO,
                    ..LockOptions::default()
                },
            )
            .unwrap();
            let commit = claims.commit(&path).unwrap();
            commit
                .require_outputs(&claims, &path, [path.join("audio.jsonl")])
                .unwrap();
            assert!(crate::remove_marked::ReviewedPolicy::capture(&root).is_ok());
        }
    }

    #[test]
    fn disjoint_rows_share_one_segment_claim_and_keep_exact_locators() {
        let (_journal, root) = fixture();
        let a = selection(SegmentLayout::Named, &["a.flac"]);
        let b = selection(SegmentLayout::Named, &["b.flac"]);
        let direct = selection(SegmentLayout::Direct, &["a.flac"]);
        for selected in [&a, &b, &direct] {
            seed(&root, selected, &selected.names[0], "analyzed");
        }
        let operation = operation(&root);
        let observed = capture(&operation, &[b, direct, a]).unwrap();
        assert_eq!(observed.groups().len(), 3);
        assert_eq!(
            observed
                .groups()
                .iter()
                .flat_map(|group| group.files())
                .count(),
            3
        );
        let paths = observed
            .groups()
            .iter()
            .map(|group| group.directory().object_identity())
            .collect::<Vec<_>>();
        assert_ne!(paths[0], paths[1]);
        assert_eq!(paths[1], paths[2]);
    }

    #[test]
    fn overlap_refuses_before_busy_claim_and_valid_selection_reports_contention() {
        let (_journal, root) = fixture();
        let a = selection(SegmentLayout::Named, &["a.flac"]);
        let both = selection(SegmentLayout::Named, &["a.flac", "b.flac"]);
        seed(&root, &both, "a.flac", "analyzed");
        seed(&root, &both, "b.flac", "analyzed");
        let operation = operation(&root);
        let path = root.canonical_path().join(selection_path(&a));
        let _claims = OutputClaims::discover(
            &path,
            LockOptions {
                timeout: Duration::ZERO,
                ..LockOptions::default()
            },
        )
        .unwrap();
        assert!(matches!(
            capture(&operation, &[a.clone(), both]),
            Err(OffloadOfferRefusal::InvalidSelection)
        ));
        assert!(
            matches!(capture(&operation, &[a]), Err(OffloadOfferRefusal::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
        assert!(
            !root
                .canonical_path()
                .join("health/retention-marks.json")
                .exists()
        );
    }

    #[test]
    fn full_ordinary_sibling_proof_and_independent_empty_group_are_governing() {
        let (_journal, root) = fixture();
        let a = selection(SegmentLayout::Named, &["a.flac"]);
        let empty = selection(SegmentLayout::Named, &["empty.flac"]);
        seed(&root, &a, "a.flac", "analyzed");
        seed(&root, &a, "b.flac", "pending");
        seed(&root, &empty, "empty.flac", "empty");
        let operation = operation(&root);
        assert!(matches!(
            capture(&operation, &[a]),
            Err(OffloadOfferRefusal::Unproven)
        ));
        assert_eq!(
            capture(&operation, &[empty]).unwrap().groups()[0].files()[0].name(),
            "empty.flac"
        );
        assert_eq!(
            fs::read(root.canonical_path().join("config/journal.json")).unwrap(),
            b"{}"
        );
    }

    #[test]
    fn strict_boolean_preserve_all_keeps_only_empty_group() {
        for preserve in [json!(true), json!("true")] {
            let (_journal, root) = fixture();
            fs::write(
                root.canonical_path().join("config/journal.json"),
                json!({"transcribe":{"preserve_all":preserve}}).to_string(),
            )
            .unwrap();
            let empty = selection(SegmentLayout::Named, &["empty.flac"]);
            let ordinary = selection(SegmentLayout::Named, &["ordinary.flac"]);
            seed(&root, &empty, "empty.flac", "empty");
            seed(&root, &ordinary, "ordinary.flac", "analyzed");
            let operation = operation(&root);
            let captured = capture(&operation, &[empty]);
            if preserve == json!(true) {
                assert!(matches!(captured, Err(OffloadOfferRefusal::Kept(_))));
            } else {
                assert!(captured.is_ok());
            }
            assert!(capture(&operation, &[ordinary]).is_ok());
        }
    }

    #[test]
    fn malformed_register_refuses_without_replacing_bytes_or_inputs() {
        let (_journal, root) = fixture();
        let selected = selection(SegmentLayout::Named, &["audio.flac"]);
        seed(&root, &selected, "audio.flac", "analyzed");
        fs::create_dir(root.canonical_path().join("health")).unwrap();
        let path = root.canonical_path().join("health/retention-marks.json");
        fs::write(&path, b"{torn").unwrap();
        let operation = operation(&root);
        assert!(matches!(
            capture(&operation, std::slice::from_ref(&selected)),
            Err(OffloadOfferRefusal::Review(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), b"{torn");
        assert_eq!(
            fs::read(
                root.canonical_path()
                    .join(selection_path(&selected))
                    .join("audio.flac")
            )
            .unwrap(),
            b"current"
        );
    }

    #[test]
    fn raw_and_processing_drift_invalidate_actual_capture() {
        for sidecar in [false, true] {
            let (_journal, root) = fixture();
            let selected = selection(SegmentLayout::Named, &["audio.flac"]);
            seed(&root, &selected, "audio.flac", "analyzed");
            let operation = operation(&root);
            let observed = capture(&operation, std::slice::from_ref(&selected)).unwrap();
            let directory = root.canonical_path().join(selection_path(&selected));
            let path = if sidecar {
                directory.join("audio.jsonl")
            } else {
                directory.join("audio.flac")
            };
            fs::write(
                path,
                if sidecar {
                    b"{}\n".as_slice()
                } else {
                    b"changed".as_slice()
                },
            )
            .unwrap();
            assert!(matches!(
                check_group(&observed.groups()[0]),
                Err(OffloadOfferRefusal::Changed)
            ));
        }
    }

    #[test]
    fn captured_configuration_and_exact_initial_row_are_rechecked() {
        for configuration in [false, true] {
            let (_journal, root) = fixture();
            let selected = selection(SegmentLayout::Named, &["audio.flac"]);
            seed(&root, &selected, "audio.flac", "analyzed");
            let operation = operation(&root);
            let observed = capture(&operation, std::slice::from_ref(&selected)).unwrap();
            if configuration {
                fs::write(
                    root.canonical_path().join("config/journal.json"),
                    b"{\"transcribe\":{\"preserve_all\":true}}",
                )
                .unwrap();
            } else {
                let mark = Mark {
                    id: selected_id(&selected),
                    class: RemovalClass::OffloadRawRelease,
                    target: selected.target.clone(),
                    marked_at: "now".into(),
                    proposal: crate::marks::Proposal {
                        bytes: 7,
                        reason: "archive".into(),
                        names: selected.names.clone(),
                    },
                    state: crate::marks::MarkState::Marked,
                    offload: None,
                };
                let mut register = crate::marks::Register::empty();
                register.marks.insert(mark.id.clone(), mark);
                fs::create_dir(root.canonical_path().join("health")).unwrap();
                fs::write(
                    root.canonical_path().join("health/retention-marks.json"),
                    serde_json::to_vec(&register).unwrap(),
                )
                .unwrap();
            }
            let policy = crate::remove_marked::ReviewedPolicy::capture(&root).unwrap();
            assert!(matches!(
                check_capture(&observed, &operation, &policy),
                Err(OffloadOfferRefusal::Changed)
            ));
        }
    }
}
