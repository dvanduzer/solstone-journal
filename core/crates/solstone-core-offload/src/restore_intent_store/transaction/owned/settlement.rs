// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Private qualification and receipt-only transfer after actual lower settlement.

use super::*;
use solstone_core_journal_io::media::{MediaRead, RemovalReceipt};
#[cfg(any(unix, windows))]
use solstone_core_journal_io::media::{OriginalName, RemovalEffect, RemovalPhase};
#[cfg(any(unix, windows))]
use solstone_core_retention::restore_intent::{MarkRetirement, PublicationFacts};

pub(crate) struct SettlementFailure {
    pub(crate) reason: &'static str,
    pub(crate) source: Option<std::io::Error>,
    pub(crate) destination: Option<crate::restore_prepared::PreparedRefusal>,
}

impl SettlementFailure {
    pub(crate) fn refused(reason: &'static str) -> Self {
        Self {
            reason,
            source: None,
            destination: None,
        }
    }
}

// These fields are original evidence only; none retain a native handle or lease.
pub(crate) struct SettledTransaction {
    receipts: TransactionReceipts,
    record: Option<(RestoreIntent, Vec<StoreWriteReceipt>)>,
    retirement: Option<SettledRetirement>,
}

struct SettledRetirement {
    _observed: MediaRead,
    removal: Option<RemovalReceipt>,
    ancestors: [Durability; 4],
    failures: Vec<StoreProblem>,
    retired: bool,
}

impl DetachedTransaction {
    pub(crate) fn qualify_settlement(
        &self,
        carrier: &Arc<RootActivityCarrier>,
        witness: &ArchiveWitness,
        disposition: RestoreDisposition,
        effects: AttemptEffects,
    ) -> Result<(), SettlementFailure> {
        let refusal = || SettlementFailure::refused("unsettled_transaction");
        let record = self.record.as_ref().ok_or_else(refusal)?;
        let intent = &record.intent;
        let receipts = &self.receipts;
        if !Arc::ptr_eq(&self.owner.carrier, carrier)
            || !Arc::ptr_eq(
                carrier.journal_root(),
                self.owner.destination.journal_root(),
            )
            || self.owner.destination.destination() != &intent.destination
            || self.owner.destination.locator() != &intent.locator
            || intent.witness != *witness
            || self.disposition() != disposition
            || receipts.effects != effects
            || self.owner.media.is_none()
            || receipts.settlement_cleanup.is_some()
            || record.store.is_some()
            || self.store_failure.is_some()
            || receipts.hold.is_some()
            || record.persisted.as_ref() != Some(intent)
            || intent.validate().is_err()
            || record.writes.is_empty()
        {
            return Err(refusal());
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(SettlementFailure::refused("unsupported_settlement"))
        }
        #[cfg(any(unix, windows))]
        {
            if !record.writes.iter().all(StoreWriteReceipt::fully_confirmed) {
                return Err(refusal());
            }
            let preparation = receipts.preparation.as_ref().ok_or_else(refusal)?;
            let preparation_valid = if cfg!(windows) {
                preparation.confirmed
                    && preparation.durability == [Durability::Unproven; 4]
                    && preparation.failures.is_empty()
            } else {
                preparation.confirmed
                    && preparation.durability == [Durability::Synced; 4]
                    && preparation.failures.is_empty()
            };
            if !preparation_valid {
                return Err(refusal());
            }
            let retired = self.retirement.as_ref().ok_or_else(refusal)?;
            let confirmed = record
                .writes
                .last()
                .and_then(|write| write.confirmation.as_ref());
            let retirement::NativeRetirement::Removal(removal) = &retired.native else {
                return Err(refusal());
            };
            let removed = removal.receipt();
            let required_durabilities = if cfg!(windows) {
                (retired.ancestors == [Durability::Synced; 4]
                    || retired.ancestors == [Durability::Unproven; 4])
                    && (removed.source_durability == Durability::Synced
                        || removed.source_durability == Durability::Unproven)
                    && (removed.claim_durability == Durability::Synced
                        || removed.claim_durability == Durability::Unproven)
            } else {
                retired.ancestors == [Durability::Synced; 4]
                    && removed.source_durability == Durability::Synced
                    && removed.claim_durability == Durability::Synced
            };
            if !retired.retired
                || confirmed != Some(&retired.observed)
                || !required_durabilities
                || !retired.failures.is_empty()
                || removed.effect != RemovalEffect::Deleted
                || removed.original_name != OriginalName::Absent
                || removed.claim_confirmation != Confirmation::Confirmed
                || removed.cleanup != Cleanup::Complete
                || removed
                    .failures
                    .iter()
                    .any(|f| f.phase != RemovalPhase::Cleanup)
                || removed.restoration_attempted
            {
                return Err(refusal());
            }
            let publication = receipts.publication.as_ref().ok_or_else(refusal)?;
            let publication_facts = PublicationFacts::from(publication);
            if !intent
                .outcome
                .as_ref()
                .is_some_and(|facts| facts.matches_ignoring_cleanup(&publication_facts))
                || intent.event_recorded != Obligation::Complete
                || intent.cleanup != Obligation::Complete
            {
                return Err(refusal());
            }
            let ledger = receipts
                .ledger
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .ok_or_else(refusal)?;
            if !ledger.settles() {
                return Err(refusal());
            }
            match publication.effect {
                PublicationEffect::Published | PublicationEffect::AlreadyPresent => {
                    let expected_effects = if publication.effect == PublicationEffect::Published {
                        AttemptEffects {
                            files: 1,
                            bytes: witness.file.bytes,
                        }
                    } else {
                        AttemptEffects::default()
                    };
                    let durability_invalid = if cfg!(windows) {
                        publication.effect == PublicationEffect::Published
                            && publication.file_durability != Durability::Synced
                    } else {
                        publication.effect == PublicationEffect::Published
                            && (publication.file_durability != Durability::Synced
                                || publication.parent_durability != Durability::Synced)
                    };
                    if effects != expected_effects
                        || publication.error.is_some()
                        || publication.confirmation != Confirmation::Confirmed
                        || publication.identity.is_none()
                        || publication.cleanup != Cleanup::Complete
                        || intent.marker != Obligation::Complete
                        || durability_invalid
                    {
                        return Err(refusal());
                    }
                    #[cfg(unix)]
                    {
                        let marker = receipts
                            .marker
                            .as_ref()
                            .and_then(|result| result.as_ref().ok())
                            .ok_or_else(refusal)?;
                        let marker_ancestor_sync_invalid = receipts.marker_ancestor_sync.len() != 3
                            || !receipts.marker_ancestor_sync.iter().all(Result::is_ok);
                        if !matches!(
                            marker.publication(),
                            solstone_core_journal_io::atomic::BoundAtomicOutcome::Published { .. }
                        ) || marker.binding_error().is_some()
                            || marker.lock_error().is_some()
                            || marker_ancestor_sync_invalid
                        {
                            return Err(refusal());
                        }
                    }
                    #[cfg(windows)]
                    {
                        if !receipts.marker_published {
                            return Err(refusal());
                        }
                    }
                    match &intent.retirement {
                        MarkRetirement::Exact { .. } => {
                            if intent.register != Obligation::Complete
                                || !matches!(receipts.register.as_ref(), Some(Ok(
                                    BoundMutationOutcome::Unchanged(result) | BoundMutationOutcome::Published(result)
                                )) if matches!(result.status, MutationStatus::Applied | MutationStatus::AlreadyApplied))
                            {
                                return Err(refusal());
                            }
                        }
                        MarkRetirement::NoMark | MarkRetirement::PreserveLegacy { .. } => {
                            if intent.register != Obligation::NotRequired
                                || receipts.register.is_some()
                            {
                                return Err(refusal());
                            }
                        }
                    }
                }
                PublicationEffect::Conflict | PublicationEffect::NotPublished => {
                    #[cfg(unix)]
                    let marker_no_effect =
                        receipts.marker.is_none() && receipts.marker_ancestor_sync.is_empty();
                    #[cfg(windows)]
                    let marker_no_effect = !receipts.marker_published;
                    #[cfg(not(any(unix, windows)))]
                    let marker_no_effect = true;
                    if !intent.no_effect_outcome()
                        || effects != AttemptEffects::default()
                        || publication.identity.is_some()
                        || !receipts
                            .no_effect_cleanup
                            .as_ref()
                            .is_some_and(NoEffectCleanup::confirmed)
                        || intent.marker != Obligation::NotRequired
                        || intent.register != Obligation::NotRequired
                        || !marker_no_effect
                        || receipts.register.is_some()
                    {
                        return Err(refusal());
                    }
                }
            }
            Ok(())
        }
    }

    pub(crate) fn revalidate_settlement(
        &self,
        carrier: &Arc<RootActivityCarrier>,
    ) -> Result<(), SettlementFailure> {
        if !Arc::ptr_eq(&self.owner.carrier, carrier)
            || !Arc::ptr_eq(
                carrier.journal_root(),
                self.owner.destination.journal_root(),
            )
        {
            return Err(SettlementFailure::refused("settlement_owner_changed"));
        }
        carrier.revalidate().map_err(|source| SettlementFailure {
            reason: "settlement_owner_changed",
            source: Some(source),
            destination: None,
        })?;
        self.owner
            .destination
            .revalidate()
            .map_err(|destination| SettlementFailure {
                reason: "settlement_destination_changed",
                source: None,
                destination: Some(destination),
            })
    }

    pub(crate) fn cleanup_settlement(&mut self) -> Result<(), SettlementFailure> {
        // Never retries a failed settlement cleanup using an old success report.
        if self.receipts.settlement_cleanup.is_some() {
            return Err(SettlementFailure::refused("settlement_already_attempted"));
        }
        let Some(media) = &mut self.owner.media else {
            return Err(SettlementFailure::refused("settlement_media_missing"));
        };
        let cleanup = media.cleanup();
        let parent_sync = if cleanup == Cleanup::Complete {
            Some(self.owner.destination.stage_parent().sync())
        } else {
            None
        };
        self.receipts.settlement_cleanup = Some(NoEffectCleanup {
            cleanup,
            parent_sync,
        });
        if !self
            .receipts
            .settlement_cleanup
            .as_ref()
            .is_some_and(NoEffectCleanup::confirmed)
        {
            return Err(SettlementFailure::refused("settlement_cleanup_unconfirmed"));
        }
        Ok(())
    }

    /// Called only after every Native file qualifies and the original lower read settles.
    pub(crate) fn into_settled(self) -> SettledTransaction {
        let Self {
            owner,
            record,
            store_failure,
            retirement,
            receipts,
        } = self;
        let record = record.map(|record| (record.intent, record.writes));
        let retirement = retirement.map(|facts| SettledRetirement {
            _observed: facts.observed,
            removal: match facts.native {
                retirement::NativeRetirement::Removal(removal) => Some((*removal).into_receipt()),
                // Qualification excludes these branches; no inferred cleanup occurs here.
                _ => None,
            },
            ancestors: facts.ancestors,
            failures: facts.failures,
            retired: facts.retired,
        });
        drop((owner, store_failure));
        SettledTransaction {
            receipts,
            record,
            retirement,
        }
    }
}

impl SettledTransaction {
    pub(crate) fn facts(&self) -> TransactionFacts<'_> {
        let retirement = self.retirement.as_ref().map(|facts| RetirementFacts {
            removal: facts.removal.as_ref(),
            retained_stage: false,
            ancestors: facts.ancestors,
            failures: facts
                .failures
                .iter()
                .map(|error| format!("{error:?}"))
                .collect(),
            retired: facts.retired,
        });
        let preparation = self
            .receipts
            .preparation
            .as_ref()
            .map(|facts| StorePreparationFacts {
                attempted: &facts.attempted,
                durability: facts.durability,
                confirmed: facts.confirmed,
                failures: facts
                    .failures
                    .iter()
                    .map(|error| format!("{error:?}"))
                    .collect(),
            });
        self.receipts.facts(
            self.record
                .as_ref()
                .map(|(intent, writes)| (intent, writes.as_slice())),
            retirement,
            false,
            false,
            preparation,
            Vec::new(),
            true,
        )
    }
}
