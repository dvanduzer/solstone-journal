// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Existing-only restore admission and short recovery inspection. Directory
//! handles survive staging; claims and segment exclusion are acquired afterward.
//! No publication is reachable until the bound durable-intent adapter is added.

use std::sync::Arc;

use solstone_core_journal_io::media::MediaDirectory;
use solstone_core_journal_io::paths::Segment;
use solstone_core_journal_io::{JournalRoot, LockOptions};
use solstone_core_retention::marks;
use solstone_core_segment::output_attempt::OutputClaims;

use crate::ledger;
use crate::restore_intent::{
    DestinationObservation, RecoveryDecision, RepairHold, RestoreIntent, RestoreLocator,
    content_observation, inspect_recovery, selected_name,
};

#[derive(Debug)]
pub(crate) struct PreparedRefusal {
    pub reason: RepairHold,
    pub detail: String,
}

fn refused(reason: RepairHold, error: impl ToString) -> PreparedRefusal {
    PreparedRefusal {
        reason,
        detail: error.to_string(),
    }
}

/// Retained native admission, never reconstructible from intent evidence.
pub(crate) struct PreparedRestore {
    root: Arc<JournalRoot>,
    parent: Arc<MediaDirectory>,
    live: Arc<MediaDirectory>,
    locator: RestoreLocator,
    destination: DestinationObservation,
}

impl PreparedRestore {
    /// Capture the actual existing segment before staging, without allocating
    /// its directory or any chronicle ancestor. No claims are held during reads
    /// from the archive; this core does not launch a helper.
    pub fn admit(
        root: Arc<JournalRoot>,
        day: &str,
        segment: &Segment,
    ) -> Result<Self, PreparedRefusal> {
        root.revalidate_canonical_binding()
            .map_err(|e| refused(RepairHold::DestinationChanged, e))?;
        let locator = RestoreLocator::from_segment(day, segment)
            .map_err(|reason| refused(reason, "invalid restore locator"))?;
        let relative = locator.relative_segment();
        if segment.path() != root.canonical_path().join(&relative) {
            return Err(refused(
                RepairHold::DestinationChanged,
                "discovered segment belongs to another root",
            ));
        }
        let relative_parent = relative
            .parent()
            .ok_or_else(|| refused(RepairHold::InvalidIntent, "segment has no parent"))?;
        let parent = MediaDirectory::open(root.clone(), relative_parent)
            .map_err(|e| refused(RepairHold::DestinationChanged, e))?;
        let live = MediaDirectory::open(root.clone(), &relative)
            .map_err(|e| refused(RepairHold::DestinationChanged, e))?;
        if solstone_core_segment::owner_deleted(live.diagnostic_path())
            .map_err(|e| refused(RepairHold::DestinationChanged, e))?
        {
            return Err(refused(
                RepairHold::DestinationChanged,
                "segment was deleted by its owner",
            ));
        }
        let destination = DestinationObservation {
            root: root.identity().observation_id(),
            parent: parent.identity(),
            segment: live.identity(),
        };
        let prepared = Self {
            root,
            parent,
            live,
            locator,
            destination,
        };
        prepared.revalidate()?;
        Ok(prepared)
    }

    pub fn destination(&self) -> &DestinationObservation {
        &self.destination
    }
    pub fn locator(&self) -> &RestoreLocator {
        &self.locator
    }

    /// The later native adapter creates a stage through this retained parent;
    /// its diagnostic pathname is never sufficient for creation or cleanup.
    pub fn stage_parent(&self) -> Arc<MediaDirectory> {
        self.parent.clone()
    }

    fn revalidate(&self) -> Result<(), PreparedRefusal> {
        self.root
            .revalidate_canonical_binding()
            .map_err(|e| refused(RepairHold::DestinationChanged, e))?;
        self.parent
            .revalidate()
            .map_err(|e| refused(RepairHold::DestinationChanged, e))?;
        self.live
            .revalidate()
            .map_err(|e| refused(RepairHold::DestinationChanged, e))?;
        if self.root.identity().observation_id() != self.destination.root
            || self.parent.identity() != self.destination.parent
            || self.live.identity() != self.destination.segment
        {
            return Err(refused(
                RepairHold::DestinationChanged,
                "retained restore binding changed",
            ));
        }
        Ok(())
    }

    /// Acquire coverage internally, in the owning order, including absent raw
    /// output and every persisted expected proposal name. Results are decisions
    /// only: returning releases exclusion and grants no publication capability.
    pub fn inspect(
        &self,
        intent: &RestoreIntent,
        options: LockOptions,
    ) -> Result<RecoveryDecision, PreparedRefusal> {
        intent
            .validate()
            .map_err(|reason| refused(reason, "invalid restore intent"))?;
        if intent.locator != self.locator || intent.destination != self.destination {
            return Err(refused(
                RepairHold::DestinationChanged,
                "intent does not belong to retained destination",
            ));
        }
        self.revalidate()?;
        let claims =
            OutputClaims::acquire(intent.output_paths(self.root.canonical_path()), options)
                .map_err(|e| refused(RepairHold::ClaimsUnavailable, e))?;
        let _commit = claims
            .commit(self.live.diagnostic_path())
            .map_err(|e| refused(RepairHold::DestinationChanged, e))?;
        self.revalidate()?;
        // Consume the owning fresh complete catalog, including degradation that
        // would otherwise disappear when selection is absent.
        let catalog = ledger::read_complete_catalog(&self.root)
            .map_err(|e| refused(RepairHold::DegradedCatalog, e))?;
        let selected = catalog
            .days
            .iter()
            .flat_map(|day| &day.segments)
            .find(|segment| {
                segment.day == self.locator.target.day
                    && segment.stream == self.locator.target.stream
                    && segment.segment == self.locator.segment
            })
            .ok_or_else(|| {
                refused(
                    RepairHold::CatalogChanged,
                    "exact restore history is absent",
                )
            })?;
        let register = marks::load(self.root.canonical_path())
            .map_err(|e| refused(RepairHold::MarkChanged, e))?;
        let content = content_observation(intent, self.live.observe_file(selected_name(intent)));
        self.revalidate()?;
        inspect_recovery(intent, &self.destination, selected, &register, content)
            .map_err(|reason| refused(reason, "restore recovery is held"))
    }
}

#[cfg(all(test, feature = "full-tests"))]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::restore_intent::tests::{fixture, register};
    use crate::restore_intent::{AttemptEffects, PublicationDecision};
    use sha2::{Digest, Sha256};
    use solstone_core_journal_io::paths::{PathOrDay, iter_segments};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    fn options() -> LockOptions {
        LockOptions {
            timeout: Duration::ZERO,
            poll_interval: Duration::ZERO,
            mode: Some(0o600),
        }
    }

    fn seed_catalog(journal: &Path, intent: &RestoreIntent) -> PathBuf {
        let witness = &intent.witness;
        let event = serde_json::json!({
            "event_kind": "offload_archive_v2", "schema_version": 2, "time": 1,
            "day": intent.locator.target.day, "stream": intent.locator.target.stream,
            "segment": intent.locator.segment, "segment_dir": intent.locator.target.dir,
            "layout": intent.locator.layout, "journal_id": witness.journal_id,
            "manifest_id": witness.manifest_id, "repository_id": witness.repository_id,
            "snapshot_id": witness.snapshot_id,
            "files": [{ "witness_id": witness.witness_id, "name": witness.file.name,
                "bytes": witness.file.bytes, "sha256": witness.file.sha256,
                "archive_path": witness.archive_path, "processing": witness.processing }]
        });
        let path = ledger::ledger_path_for_day(journal, &intent.locator.target.day).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = serde_json::to_vec(&event).unwrap();
        bytes.push(b'\n');
        fs::write(&path, bytes).unwrap();
        path
    }

    fn setup(with_mark: bool) -> (tempfile::TempDir, PathBuf, PreparedRestore, RestoreIntent) {
        let temporary = tempfile::tempdir().unwrap();
        let journal = temporary.path().join("journal");
        let mut intent = fixture(with_mark);
        fs::create_dir_all(journal.join(intent.locator.relative_segment())).unwrap();
        let root = Arc::new(JournalRoot::open(&journal).unwrap());
        let segment = iter_segments(root.canonical_path(), PathOrDay::Day("20261009"))
            .unwrap()
            .remove(0);
        let prepared = PreparedRestore::admit(root, "20261009", &segment).unwrap();
        intent.destination = prepared.destination().clone();
        intent.stage.parent = intent.destination.parent;
        if let solstone_core_journal_io::media::MediaIdentity::Windows { volume, .. } =
            intent.destination.parent
        {
            intent.stage.directory = solstone_core_journal_io::media::MediaIdentity::Windows {
                volume,
                file_id: [3; 16],
            };
            intent.stage.leaf = solstone_core_journal_io::media::MediaIdentity::Windows {
                volume,
                file_id: [6; 16],
            };
        }
        seed_catalog(&journal, &intent);
        if with_mark {
            fs::write(
                journal.join("health/retention-marks.json"),
                serde_json::to_vec(&register(&intent)).unwrap(),
            )
            .unwrap();
        }
        (temporary, journal, prepared, intent)
    }

    #[test]
    fn actual_selected_claim_blocks_absent_raw_while_unrelated_claim_cannot_cover_it() {
        let (_temporary, journal, prepared, intent) = setup(false);
        let segment = journal.join(intent.locator.relative_segment());
        let unrelated =
            OutputClaims::acquire([segment.join("unrelated.jsonl")], options()).unwrap();
        let selected = OutputClaims::acquire([segment.join("a.jsonl")], options()).unwrap();
        assert!(!segment.join("a.flac").exists());
        assert_eq!(
            prepared.inspect(&intent, options()).unwrap_err().reason,
            RepairHold::ClaimsUnavailable
        );
        drop(selected);
        let decision = prepared.inspect(&intent, options()).unwrap();
        assert_eq!(decision.publication, PublicationDecision::Missing);
        assert_eq!(decision.newly_published, AttemptEffects::default());
        assert!(!segment.join("a.flac").exists());
        drop(unrelated);
    }

    #[test]
    fn every_persisted_proposal_output_is_claimed_and_changed_full_mark_refuses() {
        let (_temporary, journal, prepared, intent) = setup(true);
        let other = journal
            .join(intent.locator.relative_segment())
            .join("b.jsonl");
        let busy = OutputClaims::acquire([other], options()).unwrap();
        assert_eq!(
            prepared.inspect(&intent, options()).unwrap_err().reason,
            RepairHold::ClaimsUnavailable
        );
        drop(busy);
        assert_eq!(
            prepared.inspect(&intent, options()).unwrap().publication,
            PublicationDecision::Missing
        );
        let mut newer = register(&intent);
        newer.marks.values_mut().next().unwrap().marked_at = "2026-10-10T01:00:00Z".into();
        let bytes = serde_json::to_vec(&newer).unwrap();
        let path = journal.join("health/retention-marks.json");
        fs::write(&path, &bytes).unwrap();
        assert_eq!(
            prepared.inspect(&intent, options()).unwrap_err().reason,
            RepairHold::MarkChanged
        );
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn retained_segment_tombstone_refuses_without_touching_owner_files() {
        let (_temporary, journal, prepared, intent) = setup(false);
        let segment = journal.join(intent.locator.relative_segment());
        fs::write(segment.join("a.flac"), b"owner bytes").unwrap();
        fs::write(segment.join("tombstone.json"), b"{}").unwrap();
        assert!(prepared.inspect(&intent, options()).is_err());
        assert_eq!(fs::read(segment.join("a.flac")).unwrap(), b"owner bytes");
        assert_eq!(fs::read(segment.join("tombstone.json")).unwrap(), b"{}");
    }

    #[cfg(unix)]
    #[test]
    fn absent_or_replaced_segment_and_parent_are_not_recreated_from_saved_identity() {
        for replace_parent in [false, true] {
            let (_temporary, journal, prepared, intent) = setup(false);
            let segment = journal.join(intent.locator.relative_segment());
            let original = if replace_parent {
                segment.parent().unwrap().to_path_buf()
            } else {
                segment.clone()
            };
            let moved = original.with_extension("moved");
            fs::rename(&original, &moved).unwrap();
            assert_eq!(
                prepared.inspect(&intent, options()).unwrap_err().reason,
                RepairHold::DestinationChanged
            );
            assert!(!original.exists());
            fs::create_dir(&original).unwrap();
            assert_eq!(
                prepared.inspect(&intent, options()).unwrap_err().reason,
                RepairHold::DestinationChanged
            );
            assert!(moved.exists());
            assert!(!segment.join("a.flac").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn retained_root_refuses_copied_root_even_with_exact_catalog_and_intent_bytes() {
        let (temporary, journal, prepared, intent) = setup(false);
        let original = temporary.path().join("original");
        fs::rename(&journal, &original).unwrap();
        fs::create_dir_all(journal.join(intent.locator.relative_segment())).unwrap();
        seed_catalog(&journal, &intent);
        assert_eq!(
            prepared.inspect(&intent, options()).unwrap_err().reason,
            RepairHold::DestinationChanged
        );
        assert!(
            !journal
                .join(intent.locator.relative_segment())
                .join("a.flac")
                .exists()
        );
    }

    #[test]
    fn streamed_matching_distinct_file_and_conflict_preserve_existing_content() {
        let (_temporary, journal, prepared, mut intent) = setup(false);
        intent.witness.file.sha256 = format!("{:x}", Sha256::digest(b"abc"));
        seed_catalog(&journal, &intent);
        let raw = journal
            .join(intent.locator.relative_segment())
            .join("a.flac");
        fs::write(&raw, b"abc").unwrap();
        let decision = prepared.inspect(&intent, options()).unwrap();
        assert_eq!(decision.publication, PublicationDecision::AlreadyPresent);
        assert_eq!(decision.newly_published, AttemptEffects::default());
        assert_ne!(decision.confirmed.unwrap().identity, intent.stage.leaf);
        fs::write(&raw, b"different owner bytes").unwrap();
        assert_eq!(
            prepared.inspect(&intent, options()).unwrap().publication,
            PublicationDecision::Conflict
        );
        assert_eq!(fs::read(&raw).unwrap(), b"different owner bytes");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_targets_remain_held_without_unlink_or_follow() {
        let (_temporary, journal, prepared, intent) = setup(false);
        let raw = journal
            .join(intent.locator.relative_segment())
            .join("a.flac");
        let outside = journal.join("outside");
        fs::write(&outside, b"owner bytes").unwrap();
        std::os::unix::fs::symlink(&outside, &raw).unwrap();
        assert_eq!(
            prepared.inspect(&intent, options()).unwrap_err().reason,
            RepairHold::UncertainContent
        );
        assert!(fs::symlink_metadata(&raw).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&outside).unwrap(), b"owner bytes");
    }

    #[test]
    fn torn_catalog_remains_byte_for_byte_unchanged_and_cannot_be_empty_success() {
        let (_temporary, journal, prepared, intent) = setup(false);
        let path = ledger::ledger_path_for_day(&journal, &intent.locator.target.day).unwrap();
        let bytes = b"{\"event_kind\":\"offload_archive_v2\"";
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            prepared.inspect(&intent, options()).unwrap_err().reason,
            RepairHold::DegradedCatalog
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}
