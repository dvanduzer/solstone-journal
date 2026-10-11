// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Explicit continuity of one unchanged metadata record. Every stage and intent
//! remains unresolved; this module grants no replay, retirement or cleanup.

use std::io;
use std::path::Path;
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use solstone_core_journal_io::LockOptions;
use solstone_core_journal_io::activity::ActivityLease;
use solstone_core_journal_io::activity_obligations::{
    IntentRead, NativeObligation, RecoveryObservationError, intent_filename,
};
use solstone_core_journal_io::media::MediaObservation;
#[cfg(unix)]
use solstone_core_journal_io::paths::{PathOrDay, iter_segments};
#[cfg(unix)]
use solstone_core_journal_io::{activity_obligations::observe_obligations, media::MediaRead};
use solstone_core_retention::Register;
use solstone_core_retention::archive_catalog::{JournalCatalog, SegmentCatalog};
#[cfg(unix)]
use solstone_core_retention::{archive_catalog::read_complete_catalog, review_context};

use super::{ObservedIntent, RestoreIntentStore, StoreProblem, StoreWrite};
use crate::restore_intent::{RecoveryDecision, RepairHold, RestoreIntent};
#[cfg(unix)]
use crate::restore_intent::{content_observation, inspect_recovery, selected_name};
use crate::restore_prepared::{PreparedRefusal, PreparedRestore};

#[derive(Debug)]
enum ReviewProblem {
    Selection,
    Changed,
    Nonce,
    Entropy,
    Raw,
    Publication,
    Store(StoreProblem),
    Prepared(PreparedRefusal),
    Repair(RepairHold),
    Namespace(RecoveryObservationError),
    Io(String),
}

#[derive(Default)]
struct Evidence {
    obligations: Option<Vec<NativeObligation>>,
    named: Option<ObservedIntent>,
    final_obligations: Option<Vec<NativeObligation>>,
    final_named: Option<ObservedIntent>,
    catalog: Option<JournalCatalog>,
    register: Option<Register>,
    // Preserve the actual error, even when historical inspection succeeds.
    raw: Option<io::Result<Option<MediaObservation>>>,
    decision: Option<RecoveryDecision>,
}

struct ReviewOwner {
    activity: Arc<ActivityLease>,
    selected: NativeObligation,
    store: Option<RestoreIntentStore>,
    prepared: Option<PreparedRestore>,
    original: Evidence,
    latest: Evidence,
    write: Option<StoreWrite>,
}

#[must_use]
struct RestartReviewRefusal {
    owner: ReviewOwner,
    problem: ReviewProblem,
}

#[must_use]
struct RestartIntentReview {
    owner: ReviewOwner,
    nonce: [u8; 32],
}

/// A private token is supplied only by a confirmed actual replacement. Retaining
/// this result does not settle or hide any of its namespace obligations.
#[must_use]
struct RestartConfirmation {
    review: RestartIntentReview,
    continuity: Option<ObservedIntent>,
    problem: Option<ReviewProblem>,
}

fn selected_operation(selected: &NativeObligation) -> Result<&str, ReviewProblem> {
    let NativeObligation::Intent {
        relative,
        contents: IntentRead::Complete { operation_id, .. },
        ..
    } = selected
    else {
        return Err(ReviewProblem::Selection);
    };
    let name = intent_filename(operation_id).ok_or(ReviewProblem::Selection)?;
    if *relative != Path::new("health/offload/restore-intents").join(name) {
        return Err(ReviewProblem::Selection);
    }
    Ok(operation_id)
}

fn named_obligation(selected: &NativeObligation, read: &ObservedIntent) -> NativeObligation {
    let relative = match selected {
        NativeObligation::Intent { relative, .. } | NativeObligation::Stage { relative, .. } => {
            relative.clone()
        }
    };
    NativeObligation::Intent {
        relative,
        entry: read.read.entry.clone(),
        contents: IntentRead::Complete {
            operation_id: read.intent.event.operation_id.clone(),
            bytes: read.read.bytes.clone(),
        },
    }
}

/// Loading a complete catalog is distinct from enforcing uniqueness across days.
fn selected_catalog<'a>(
    catalog: &'a JournalCatalog,
    intent: &RestoreIntent,
) -> Result<&'a SegmentCatalog, ReviewProblem> {
    if catalog.degraded() {
        return Err(ReviewProblem::Repair(RepairHold::DegradedCatalog));
    }
    let all = || catalog.days.iter().flat_map(|day| &day.segments);
    let mut selected = all().filter(|segment| {
        segment.day == intent.locator.target.day
            && segment.stream == intent.locator.target.stream
            && segment.segment == intent.locator.segment
    });
    let segment = selected
        .next()
        .ok_or(ReviewProblem::Repair(RepairHold::CatalogChanged))?;
    if selected.next().is_some() {
        return Err(ReviewProblem::Repair(RepairHold::CatalogChanged));
    }
    let mut manifests = all()
        .flat_map(|segment| &segment.history)
        .filter(|manifest| manifest.manifest_id == intent.witness.manifest_id);
    let manifest = manifests
        .next()
        .ok_or(ReviewProblem::Repair(RepairHold::CatalogChanged))?;
    if manifests.next().is_some()
        || !segment.history.contains(manifest)
        || !manifest.files.contains(&intent.witness)
    {
        return Err(ReviewProblem::Repair(RepairHold::CatalogChanged));
    }
    let mut witnesses = all()
        .flat_map(|segment| &segment.history)
        .flat_map(|manifest| &manifest.files)
        .filter(|witness| witness.witness_id == intent.witness.witness_id);
    if witnesses.next() != Some(&intent.witness) || witnesses.next().is_some() {
        return Err(ReviewProblem::Repair(RepairHold::CatalogChanged));
    }
    let mut operations = all().flat_map(|segment| {
        segment
            .restores
            .iter()
            .filter(|event| event.operation_id == intent.event.operation_id)
            .map(move |event| (segment, event))
    });
    if operations
        .next()
        .is_some_and(|(location, event)| location != segment || event != &intent.event)
        || operations.next().is_some()
    {
        return Err(ReviewProblem::Repair(RepairHold::OperationCollision));
    }
    if let Some(binding) = &intent.legacy_binding {
        if catalog.restore_binding(&binding.adoption_id) != Some(binding)
            || manifest != &binding.legacy_manifest
        {
            return Err(ReviewProblem::Repair(RepairHold::CatalogChanged));
        }
        for expected in &binding.legacy_manifest.files {
            let mut matches = all()
                .flat_map(|segment| &segment.history)
                .flat_map(|manifest| &manifest.files)
                .filter(|witness| witness.witness_id == expected.witness_id);
            if matches.next() != Some(expected) || matches.next().is_some() {
                return Err(ReviewProblem::Repair(RepairHold::CatalogChanged));
            }
        }
    }
    Ok(segment)
}

impl ReviewOwner {
    fn store(&self) -> Result<&RestoreIntentStore, ReviewProblem> {
        self.write
            .as_ref()
            .map(|write| &write.owner)
            .or(self.store.as_ref())
            .ok_or(ReviewProblem::Changed)
    }

    #[cfg(unix)]
    fn revalidate(&self) -> Result<(), ReviewProblem> {
        self.activity
            .revalidate()
            .map_err(|error| ReviewProblem::Io(error.to_string()))?;
        self.store()?.revalidate().map_err(ReviewProblem::Store)?;
        self.prepared
            .as_ref()
            .ok_or(ReviewProblem::Changed)?
            .revalidate()
            .map_err(ReviewProblem::Prepared)
    }

    #[cfg(unix)]
    fn capture(&self, intent: &RestoreIntent) -> (Evidence, Result<(), ReviewProblem>) {
        let mut evidence = Evidence::default();
        let result = (|| {
            self.revalidate()?;
            evidence.obligations = Some(
                observe_obligations(self.activity.journal_root())
                    .map_err(ReviewProblem::Namespace)?
                    .into_entries(),
            );
            let named = self
                .store()?
                .read(&intent.event.operation_id)
                .map_err(ReviewProblem::Store)?
                .ok_or(ReviewProblem::Changed)?;
            let actual = named_obligation(&self.selected, &named);
            evidence.named = Some(named);
            if evidence
                .obligations
                .as_ref()
                .is_none_or(|entries| entries.iter().filter(|entry| **entry == actual).count() != 1)
            {
                return Err(ReviewProblem::Changed);
            }
            let prepared = self.prepared.as_ref().ok_or(ReviewProblem::Changed)?;
            let decision = prepared
                .inspect(
                    intent,
                    LockOptions {
                        timeout: Duration::ZERO,
                        poll_interval: Duration::ZERO,
                        mode: Some(0o600),
                    },
                )
                .map_err(ReviewProblem::Prepared)?;
            evidence.decision = Some(decision);
            evidence.catalog = Some(
                read_complete_catalog(self.activity.journal_root()).map_err(ReviewProblem::Io)?,
            );
            let catalog = evidence.catalog.as_ref().ok_or(ReviewProblem::Changed)?;
            let segment = selected_catalog(catalog, intent)?;
            evidence.register = Some(
                review_context::load_at_root(self.activity.journal_root())
                    .map_err(|error| ReviewProblem::Io(format!("{error:?}")))?,
            );
            evidence.raw = Some(prepared.live().observe_file(selected_name(intent)));
            let raw = evidence.raw.as_ref().ok_or(ReviewProblem::Changed)?;
            // A recorded HistoricalOnly decision intentionally ignores content.
            // Metadata review independently refuses the actual observation error.
            let raw = raw.as_ref().map_err(|_| ReviewProblem::Raw)?;
            let decision = inspect_recovery(
                intent,
                prepared.destination(),
                segment,
                evidence.register.as_ref().ok_or(ReviewProblem::Changed)?,
                content_observation(intent, Ok(raw.clone())),
            )
            .map_err(ReviewProblem::Repair)?;
            if evidence.decision.as_ref() != Some(&decision) {
                return Err(ReviewProblem::Changed);
            }
            #[cfg(all(test, feature = "full-tests", unix))]
            tests::after_reads();
            // Bracket the slower semantic/content reads with the complete sole
            // walk and named record. A late new obligation stays in the evidence.
            evidence.final_obligations = Some(
                observe_obligations(self.activity.journal_root())
                    .map_err(ReviewProblem::Namespace)?
                    .into_entries(),
            );
            if evidence.final_obligations != evidence.obligations {
                return Err(ReviewProblem::Changed);
            }
            evidence.final_named = self
                .store()?
                .read(&intent.event.operation_id)
                .map_err(ReviewProblem::Store)?;
            if evidence
                .named
                .as_ref()
                .zip(evidence.final_named.as_ref())
                .is_none_or(|(before, after)| {
                    before.read != after.read || before.intent != after.intent
                })
            {
                return Err(ReviewProblem::Changed);
            }
            self.revalidate()?;
            Ok(())
        })();
        (evidence, result)
    }
}

impl RestartIntentReview {
    fn open(
        activity: Arc<ActivityLease>,
        selected: NativeObligation,
    ) -> Result<Self, Box<RestartReviewRefusal>> {
        let mut owner = ReviewOwner {
            activity,
            selected,
            store: None,
            prepared: None,
            original: Evidence::default(),
            latest: Evidence::default(),
            write: None,
        };
        let result = (|| {
            let operation = selected_operation(&owner.selected)?.to_owned();
            match RestoreIntentStore::open_existing(Arc::clone(&owner.activity)) {
                Ok(store) => owner.store = Some(store),
                Err(failure) => {
                    owner.store = Some(*failure.owner);
                    return Err(ReviewProblem::Store(failure.problem));
                }
            }
            #[cfg(unix)]
            {
                let observed = owner
                    .store()?
                    .read(&operation)
                    .map_err(ReviewProblem::Store)?
                    .ok_or(ReviewProblem::Changed)?;
                let actual = named_obligation(&owner.selected, &observed);
                owner.original.named = Some(observed);
                if actual != owner.selected {
                    return Err(ReviewProblem::Changed);
                }
                let intent = &owner
                    .original
                    .named
                    .as_ref()
                    .ok_or(ReviewProblem::Changed)?
                    .intent;
                let root = owner.activity.journal_root();
                let segment = iter_segments(
                    root.canonical_path(),
                    PathOrDay::Day(&intent.locator.target.day),
                )
                .map_err(|error| ReviewProblem::Io(error.to_string()))?
                .into_iter()
                .find(|segment| {
                    segment.path()
                        == root
                            .canonical_path()
                            .join(intent.locator.relative_segment())
                })
                .ok_or(ReviewProblem::Repair(RepairHold::DestinationChanged))?;
                let prepared =
                    PreparedRestore::admit(Arc::clone(root), &intent.locator.target.day, &segment)
                        .map_err(ReviewProblem::Prepared)?;
                if prepared.destination() != &intent.destination {
                    return Err(ReviewProblem::Repair(RepairHold::DestinationChanged));
                }
                owner.prepared = Some(prepared);
                let intent = intent.clone();
                let (evidence, result) = owner.capture(&intent);
                // Keep the independently admitted original read on an early failure.
                if evidence.named.is_some() {
                    owner.original = evidence;
                } else {
                    owner.latest = evidence;
                }
                result?;
                if owner
                    .original
                    .named
                    .as_ref()
                    .is_none_or(|named| named_obligation(&owner.selected, named) != owner.selected)
                {
                    return Err(ReviewProblem::Changed);
                }
                let mut nonce = [0u8; 32];
                getrandom::fill(&mut nonce).map_err(|_| ReviewProblem::Entropy)?;
                Ok(nonce)
            }
            #[cfg(not(unix))]
            {
                let _ = operation;
                Err(ReviewProblem::Store(StoreProblem::Unsupported))
            }
        })();
        match result {
            Ok(nonce) => Ok(Self { owner, nonce }),
            Err(problem) => Err(Box::new(RestartReviewRefusal { owner, problem })),
        }
    }

    fn nonce(&self) -> &[u8; 32] {
        &self.nonce
    }

    #[cfg(unix)]
    fn confirm(self, nonce: &[u8; 32]) -> RestartConfirmation {
        self.confirm_with(nonce, &mut super::unix::NativeHooks)
    }

    #[cfg(unix)]
    fn confirm_with(
        mut self,
        nonce: &[u8; 32],
        hooks: &mut impl super::unix::Hooks,
    ) -> RestartConfirmation {
        let result = (|| {
            if nonce != &self.nonce {
                return Err(ReviewProblem::Nonce);
            }
            let original = self
                .owner
                .original
                .named
                .as_ref()
                .ok_or(ReviewProblem::Changed)?;
            let intent = original.intent.clone();
            let (evidence, result) = self.owner.capture(&intent);
            self.owner.latest = evidence;
            result?;
            self.compare(false)?;
            let store = self.owner.store.take().ok_or(ReviewProblem::Changed)?;
            let observed = self
                .owner
                .original
                .named
                .as_ref()
                .ok_or(ReviewProblem::Changed)?;
            self.owner.write = Some(store.write_bound(intent.clone(), Some(observed), hooks));
            let (evidence, result) = self.owner.capture(&intent);
            self.owner.latest = evidence;
            result?;
            self.compare(true)?;
            let write = self.owner.write.as_ref().ok_or(ReviewProblem::Changed)?;
            if !write.receipt.fully_confirmed() {
                return Err(ReviewProblem::Publication);
            }
            let read = write
                .receipt
                .confirmation
                .as_ref()
                .ok_or(ReviewProblem::Publication)?;
            Ok(ObservedIntent {
                anchor: Arc::clone(&write.owner.anchor),
                read: read.clone(),
                intent,
            })
        })();
        let (continuity, problem) = match result {
            Ok(continuity) => (Some(continuity), None),
            Err(problem) => (None, Some(problem)),
        };
        RestartConfirmation {
            review: self,
            continuity,
            problem,
        }
    }

    #[cfg(unix)]
    fn compare(&self, published: bool) -> Result<(), ReviewProblem> {
        let before = &self.owner.original;
        let after = &self.owner.latest;
        let original = before.named.as_ref().ok_or(ReviewProblem::Changed)?;
        let named = after.named.as_ref().ok_or(ReviewProblem::Changed)?;
        let expected: &MediaRead = if published {
            self.owner
                .write
                .as_ref()
                .and_then(|write| write.receipt.confirmation.as_ref())
                .ok_or(ReviewProblem::Publication)?
        } else {
            &original.read
        };
        if &named.read != expected
            || named.intent != original.intent
            || before.catalog != after.catalog
            || before.register != after.register
            || before.decision != after.decision
            || before.raw.as_ref().and_then(|raw| raw.as_ref().ok())
                != after.raw.as_ref().and_then(|raw| raw.as_ref().ok())
        {
            return Err(ReviewProblem::Changed);
        }
        let mut obligations = before.obligations.clone().ok_or(ReviewProblem::Changed)?;
        if published {
            let selected = obligations
                .iter_mut()
                .find(|entry| **entry == self.owner.selected)
                .ok_or(ReviewProblem::Changed)?;
            *selected = named_obligation(&self.owner.selected, named);
        }
        if after.obligations.as_ref() != Some(&obligations) {
            return Err(ReviewProblem::Changed);
        }
        self.owner.revalidate()
    }
}

#[cfg(all(test, feature = "full-tests", unix))]
mod tests;
