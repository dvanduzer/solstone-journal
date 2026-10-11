// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Restores interrupted by a crash or restart. Only a restore that provably
//! never published is undone: its own unpublished stage, then its intent. Marks
//! and the ledger are untouched, so its media stays offloaded and restorable.
//! A published outcome is held: finishing it needs that restore's admitted read.

use std::sync::Arc;

use solstone_core_journal_io::activity::ActivityLease;

use super::RestoreIntent;

/// Why an interrupted restore stays an unresolved obligation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbandonedRestoreHold {
    /// The intent could not be read, decoded or matched to its name.
    Unreadable,
    /// The journal or destination no longer matches what the intent recorded.
    DestinationChanged,
    /// The recorded stage holds something other than its own unpublished copy.
    StageChanged,
    /// A publication outcome was recorded; only its own restore can finish it.
    Recorded,
    /// The restored file is in place and matches the archive. Recording the
    /// restore and retiring its mark need an admitted read of the backup.
    PublishedAwaitingCompletion,
    /// A file with other content is in place. It is never touched.
    PublishedContentMismatch,
    /// Observation or removal failed.
    Native,
}

#[derive(Debug)]
pub struct AbandonedRestore {
    pub operation_id: Option<String>,
    pub outcome: Result<(), AbandonedRestoreHold>,
}

/// Undo every interrupted restore that never published. Call only with a
/// freshly acquired lease, before creating anything: holding it proves no live
/// owner, and only on local storage, so elsewhere nothing is done.
pub fn recover_abandoned_restores(activity: &Arc<ActivityLease>) -> Vec<AbandonedRestore> {
    #[cfg(target_os = "linux")]
    {
        linux::recover(activity)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = activity;
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsStr;
    use std::path::Path;

    use solstone_core_journal_io::activity_obligations::{
        IntentRead, NativeObligation, intent_filename, observe_obligations,
    };
    use solstone_core_journal_io::media::{
        Cleanup, Confirmation, MediaDirectory, MediaEntry, MediaStage, OriginalName, RemovalEffect,
        digest_reader, exclusive_local_activity, retire_recorded_stage,
    };

    use super::super::{Obligation, selected_name};
    use super::*;

    type Hold = AbandonedRestoreHold;

    pub(super) fn recover(activity: &Arc<ActivityLease>) -> Vec<AbandonedRestore> {
        if !matches!(exclusive_local_activity(activity), Ok(true)) {
            return Vec::new();
        }
        let Ok(census) = observe_obligations(activity.journal_root()) else {
            return Vec::new();
        };
        census
            .entries()
            .iter()
            .filter_map(|obligation| match obligation {
                NativeObligation::Intent {
                    entry, contents, ..
                } => Some((entry, contents)),
                NativeObligation::Stage { .. } => None,
            })
            .map(|(entry, contents)| match contents {
                IntentRead::Complete {
                    operation_id,
                    bytes,
                } => AbandonedRestore {
                    operation_id: Some(operation_id.clone()),
                    outcome: recover_one(activity, entry, operation_id, bytes),
                },
                _ => AbandonedRestore {
                    operation_id: None,
                    outcome: Err(Hold::Unreadable),
                },
            })
            .collect()
    }

    fn recover_one(
        activity: &Arc<ActivityLease>,
        entry: &MediaEntry,
        operation_id: &str,
        bytes: &[u8],
    ) -> Result<(), Hold> {
        let intent = RestoreIntent::decode(Ok(bytes)).map_err(|_| Hold::Unreadable)?;
        if intent.event.operation_id != operation_id {
            return Err(Hold::Unreadable);
        }
        let root = activity.journal_root();
        if intent.destination.root != root.identity().observation_id() {
            return Err(Hold::DestinationChanged);
        }
        let segment = intent.locator.relative_segment();
        let (Some(parent), Some(name)) = (segment.parent(), segment.file_name()) else {
            return Err(Hold::DestinationChanged);
        };
        let parent =
            MediaDirectory::open(Arc::clone(root), parent).map_err(|_| Hold::DestinationChanged)?;
        let live = parent
            .open_child(name)
            .map_err(|_| Hold::DestinationChanged)?
            .ok_or(Hold::DestinationChanged)?;
        if parent.identity() != intent.destination.parent
            || parent.identity() != intent.stage.parent
            || live.identity() != intent.destination.segment
        {
            return Err(Hold::DestinationChanged);
        }
        match live.observe_file(selected_name(&intent)) {
            Ok(None) => {}
            Ok(Some(present)) if present.digest == intent.expected_digest() => {
                return Err(Hold::PublishedAwaitingCompletion);
            }
            Ok(Some(_)) => return Err(Hold::PublishedContentMismatch),
            Err(_) => return Err(Hold::Native),
        }
        // Publication records its outcome before anything later; none means it
        // never happened, and the absent name confirms no file was placed.
        if intent.outcome.is_some()
            || intent.confirmed.is_some()
            || intent.event_recorded == Obligation::Complete
        {
            return Err(Hold::Recorded);
        }
        retire_recorded_stage(
            activity,
            &parent,
            OsStr::new(&intent.stage.stage_name),
            intent.stage.directory,
            OsStr::new(&intent.stage.leaf_name),
            intent.stage.leaf,
        )
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::InvalidData => Hold::StageChanged,
            _ => Hold::Native,
        })?;
        remove_intent(activity, entry, operation_id, bytes)
    }

    fn remove_intent(
        activity: &Arc<ActivityLease>,
        entry: &MediaEntry,
        operation_id: &str,
        bytes: &[u8],
    ) -> Result<(), Hold> {
        let name = intent_filename(operation_id).ok_or(Hold::Unreadable)?;
        let health = MediaDirectory::open(
            Arc::clone(activity.journal_root()),
            Path::new("health/offload"),
        )
        .map_err(|_| Hold::Native)?;
        let intents = health
            .open_child(OsStr::new("restore-intents"))
            .map_err(|_| Hold::Native)?
            .ok_or(Hold::Native)?;
        let observed = intents
            .observe_file(&name)
            .map_err(|_| Hold::Native)?
            .ok_or(Hold::Unreadable)?;
        let read = digest_reader(&mut &bytes[..]).map_err(|_| Hold::Unreadable)?;
        if observed.identity != entry.identity || observed.digest != read {
            return Err(Hold::Unreadable);
        }
        let claim = MediaStage::create(Arc::clone(&health), Arc::clone(activity))
            .map_err(|_| Hold::Native)?;
        let removal = intents.remove_observed(&name, &observed, claim);
        let receipt = removal.receipt();
        if receipt.effect == RemovalEffect::Deleted
            && receipt.original_name == OriginalName::Absent
            && receipt.claim_confirmation == Confirmation::Confirmed
            && receipt.cleanup == Cleanup::Complete
            && receipt.failures.is_empty()
        {
            Ok(())
        } else {
            Err(Hold::Native)
        }
    }
}

#[cfg(all(test, feature = "full-tests", target_os = "linux"))]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    reason = "native fixture assertions"
)]
mod tests;
