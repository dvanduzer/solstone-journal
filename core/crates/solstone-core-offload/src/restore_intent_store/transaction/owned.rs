// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Consuming handoff releases only the immutable operation borrow.

use super::*;
use crate::restore::owned::{
    IntentWriteFacts, LedgerEffect, LedgerFacts, NoEffectCleanupFacts, RetirementFacts,
    StagingFacts, StorePreparationFacts, TransactionFacts,
};

struct DetachedOwner {
    carrier: Arc<RootActivityCarrier>,
    destination: PreparedRestore,
    media: Option<PreparedMedia>,
}

#[must_use = "retain effects and every unresolved native owner"]
pub(crate) struct DetachedTransaction {
    owner: DetachedOwner,
    record: Option<Record>,
    store_failure: Option<StoreAdmissionFailure>,
    retirement: Option<retirement::RetirementFacts>,
    receipts: TransactionReceipts,
}

struct TransactionReceipts {
    preparation: Option<super::super::PreparationReceipt>,
    publication: Option<PublicationReceipt>,
    #[cfg(unix)]
    marker: Option<
        Result<
            solstone_core_journal_io::health_marker::BoundStreamMarkerPublication,
            solstone_core_journal_io::health_marker::BoundStreamMarkerError,
        >,
    >,
    #[cfg(unix)]
    marker_ancestor_sync: Vec<std::io::Result<()>>,
    ledger: Option<Result<LedgerWriteReceipt, LedgerWriteRefusal>>,
    register: Option<Result<BoundMutationOutcome, RestoreRegisterRefusal>>,
    no_effect_cleanup: Option<NoEffectCleanup>,
    settlement_cleanup: Option<NoEffectCleanup>,
    effects: AttemptEffects,
    hold: Option<Hold>,
}

mod settlement;
pub(crate) use settlement::{SettledTransaction, SettlementFailure};

impl TransactionOutcome<'_, '_> {
    pub(crate) fn into_owned(self) -> DetachedTransaction {
        let Self {
            owner,
            mut record,
            store_failure,
            publication,
            #[cfg(unix)]
            marker,
            #[cfg(unix)]
            marker_ancestor_sync,
            ledger,
            register,
            no_effect_cleanup,
            retirement,
            effects,
            hold,
        } = self;
        let mut preparation = None;
        // This is an actual in-process retirement receipt, never a decoded flag.
        if hold.is_none()
            && retirement.as_ref().is_some_and(|facts| facts.retired)
            && let Some(record) = &mut record
            && let Some(mut store) = record.store.take()
        {
            preparation = Some(std::mem::take(&mut store.preparation));
        }
        let Owner {
            carrier,
            destination,
            media,
            admission: _,
        } = owner;
        DetachedTransaction {
            owner: DetachedOwner {
                carrier,
                destination,
                media,
            },
            record,
            store_failure,
            retirement,
            receipts: TransactionReceipts {
                preparation,
                publication,
                #[cfg(unix)]
                marker,
                #[cfg(unix)]
                marker_ancestor_sync,
                ledger,
                register,
                no_effect_cleanup,
                settlement_cleanup: None,
                effects,
                hold,
            },
        }
    }
}

impl DetachedTransaction {
    pub(crate) fn completed(&self) -> bool {
        self.receipts.hold.is_none()
            && self.retirement.as_ref().is_some_and(|facts| facts.retired)
            && self
                .record
                .as_ref()
                .is_some_and(|record| record.store.is_none())
    }

    pub(crate) fn effects(&self) -> AttemptEffects {
        self.receipts.effects
    }

    pub(crate) fn disposition(&self) -> RestoreDisposition {
        self.record
            .as_ref()
            .and_then(|record| record.intent.event.files.first())
            .map_or(RestoreDisposition::Unattempted, |file| file.disposition)
    }

    pub(crate) fn facts(&self) -> TransactionFacts<'_> {
        let retirement = self.retirement.as_ref().map(|facts| RetirementFacts {
            removal: match &facts.native {
                retirement::NativeRetirement::Removal(removal) => Some(removal.receipt()),
                _ => None,
            },
            retained_stage: match &facts.native {
                retirement::NativeRetirement::StageFailure(error) => error.owned.is_some(),
                retirement::NativeRetirement::Unconsumed(_) => true,
                retirement::NativeRetirement::Removal(removal) => {
                    removal.receipt().cleanup != solstone_core_journal_io::media::Cleanup::Complete
                }
                retirement::NativeRetirement::None => false,
            },
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
            .or_else(|| {
                self.record
                    .as_ref()
                    .and_then(|record| record.store.as_ref())
                    .map(|store| &store.preparation)
            })
            .or_else(|| {
                self.store_failure
                    .as_ref()
                    .map(|failure| &failure.owner.preparation)
            })
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
                .map(|record| (&record.intent, record.writes.as_slice())),
            retirement,
            self.record
                .as_ref()
                .is_some_and(|record| record.store.is_some())
                || self.store_failure.is_some(),
            self.owner.media.is_some(),
            preparation,
            self.store_failure
                .as_ref()
                .map_or_else(Vec::new, |failure| {
                    let mut failures = vec![format!("{:?}", failure.problem)];
                    failures.extend(
                        failure
                            .owner
                            .preparation
                            .failures
                            .iter()
                            .map(|error| format!("{error:?}")),
                    );
                    failures
                }),
            self.completed(),
        )
    }
}

impl TransactionReceipts {
    #[allow(clippy::too_many_arguments)]
    fn facts<'a>(
        &'a self,
        record: Option<(&'a RestoreIntent, &'a [StoreWriteReceipt])>,
        retirement: Option<RetirementFacts<'a>>,
        store_exclusion_retained: bool,
        retained_media: bool,
        preparation: Option<StorePreparationFacts<'a>>,
        preparation_failures: Vec<String>,
        completed: bool,
    ) -> TransactionFacts<'a> {
        let ledger = self.ledger.as_ref().map(|result| match result {
            Ok(receipt) => LedgerFacts {
                effect: Some(match receipt.effect() {
                    crate::ledger::writer::LedgerWriteEffect::AlreadyRecorded => {
                        LedgerEffect::AlreadyRecorded
                    }
                    crate::ledger::writer::LedgerWriteEffect::Appended { bytes } => {
                        LedgerEffect::Appended { bytes }
                    }
                    crate::ledger::writer::LedgerWriteEffect::Attempted { written } => {
                        LedgerEffect::Attempted { written }
                    }
                }),
                durability: receipt
                    .durability()
                    .map(|value| value == LedgerSync::Confirmed),
                confirmed: receipt.confirmation() == LedgerConfirmation::Confirmed,
                failures: receipt.failures().to_vec(),
            },
            Err(error) => LedgerFacts {
                effect: None,
                durability: [false; 4],
                confirmed: false,
                failures: vec![format!("{error:?}")],
            },
        });
        TransactionFacts {
            publication: self.publication.as_ref(),
            intent_writes: record.map_or_else(Vec::new, |(_, writes)| {
                writes
                    .iter()
                    .map(|write| IntentWriteFacts {
                        #[cfg(unix)]
                        publication: write.native.as_ref().map(|native| match native {
                            super::super::unix::NativePublication::Create(result) => {
                                crate::restore::owned::IntentPublication::Create(result)
                            }
                            super::super::unix::NativePublication::Update(result) => {
                                crate::restore::owned::IntentPublication::Update(result)
                            }
                        }),
                        named_read_confirmed: write.confirmation.is_some(),
                        #[cfg(not(unix))]
                        lifetime: std::marker::PhantomData,
                        durability: write.durability,
                        failures: write
                            .failures
                            .iter()
                            .map(|error| format!("{error:?}"))
                            .collect(),
                    })
                    .collect()
            }),
            #[cfg(unix)]
            marker: self.marker.as_ref(),
            #[cfg(unix)]
            marker_ancestor_sync: &self.marker_ancestor_sync,
            ledger,
            register: self.register.as_ref(),
            no_effect_cleanup: self
                .no_effect_cleanup
                .as_ref()
                .map(|facts| NoEffectCleanupFacts {
                    cleanup: facts.cleanup,
                    parent_sync: facts.parent_sync.as_ref(),
                }),
            settlement_cleanup: self.settlement_cleanup.as_ref().map(|facts| {
                crate::restore::owned::SettlementCleanupFacts {
                    cleanup: facts.cleanup,
                    parent_sync: facts.parent_sync.as_ref(),
                }
            }),
            retirement,
            operation_id: record.map(|(intent, _)| intent.event.operation_id.as_str()),
            store_exclusion_retained,
            retained_media,
            hold: self.hold.as_ref().map(|hold| format!("{hold:?}")),
            preparation,
            preparation_failures,
            // Borrowing these owners is unnecessary for a diagnostic view; they
            // remain in this value even after confirmed local retirement.
            completed,
        }
    }
}

impl StagingFailure {
    pub(crate) fn disposition(&self) -> RestoreDisposition {
        if matches!(self.evidence, StagingEvidence::Readback(_)) {
            RestoreDisposition::Unverified
        } else {
            RestoreDisposition::Unattempted
        }
    }

    pub(crate) fn facts(&self) -> StagingFacts<'_> {
        StagingFacts {
            hold: format!("{:?}", self.hold),
            retained_stage: match &self.evidence {
                StagingEvidence::None => false,
                StagingEvidence::Stage(error) => error.owned.is_some(),
                StagingEvidence::Target(_)
                | StagingEvidence::Unconsumed(_)
                | StagingEvidence::Readback(_) => true,
            },
            readback: match &self.evidence {
                StagingEvidence::Readback(error) => Some(error),
                _ => None,
            },
        }
    }
}
