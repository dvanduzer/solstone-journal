// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Borrow restore effects without changing execution or resource ownership.

use std::io;

use serde::{Serialize, Serializer};
use serde_json::{Value, json};
use solstone_core_journal_io::ExistingParentLockError;
#[cfg(unix)]
use solstone_core_journal_io::FlatDirectoryError;
use solstone_core_journal_io::media::{PublicationReceipt, RemovalPhase, RemovalReceipt};
use solstone_core_offload::restore::RestoreResult;
#[cfg(unix)]
use solstone_core_offload::restore::owned::IntentPublication;
use solstone_core_offload::restore::owned::{
    FileEvidence, LedgerEffect, RestoreFileFacts, RestoreRegisterRefusal, RestoreRunOutcome,
    TransactionFacts,
};
use solstone_core_retention::mark_mutation::{
    BoundMutationOutcome, BoundMutationRefusal, RestoredMarkRefusal,
};
use solstone_core_retention::review_context::ReviewRefusal;

/// Borrow the complete owner while serializing its effects and obligations.
///
/// ```
/// use serde::Serialize;
/// use solstone_core_backup_web::restore_report::owned;
/// use solstone_core_offload::restore::owned::RestoreRunOutcome;
/// fn view<'a, 'clock: 'a>(owner: &'a RestoreRunOutcome<'clock>) -> impl Serialize + 'a {
///     owned(owner)
/// }
/// ```
/// ```compile_fail,E0515
/// use serde::Serialize;
/// use solstone_core_backup_web::restore_report::owned;
/// use solstone_core_offload::restore::owned::RestoreRunOutcome;
/// fn escape<'a, 'clock: 'a>(owner: RestoreRunOutcome<'clock>) -> impl Serialize + 'a {
///     owned(&owner)
/// }
/// ```
pub fn owned<'a, 'clock>(owner: &'a RestoreRunOutcome<'clock>) -> impl Serialize + 'a {
    View(owner)
}

struct View<'a, 'clock>(&'a RestoreRunOutcome<'clock>);

impl Serialize for View<'_, '_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Report {
            #[serde(flatten)]
            summary: Value,
            files: Vec<Value>,
        }
        Report {
            summary: summary(self.0.report()),
            files: self.0.files().map(file_facts).collect(),
        }
        .serialize(serializer)
    }
}

pub(crate) fn summary(result: &RestoreResult) -> Value {
    json!({
        "status":result.status, "reason":result.reason, "scope":result.scope, "day":result.day,
        "segments_selected":result.segments_selected, "segments_restored":result.segments_restored,
        "files_expected":result.files_expected, "files_restored":result.files_restored,
        "bytes_expected":result.bytes_expected, "bytes_restored":result.bytes_restored,
        "reason_detail":result.reason_detail,
        "details":result.details.iter().map(|detail| json!({
            "status":detail.status, "reason":detail.reason, "day":detail.day, "stream":detail.stream,
            "segment":detail.segment, "snapshot_id":detail.snapshot_id,
            "files_expected":detail.files_expected, "files_restored":detail.files_restored,
            "bytes_expected":detail.bytes_expected, "bytes_restored":detail.bytes_restored,
        })).collect::<Vec<_>>(),
    })
}

fn file_facts(file: RestoreFileFacts<'_>) -> Value {
    json!({
        "day":file.day, "stream":file.stream, "segment":file.segment,
        "name":file.witness.file.name, "bytes_expected":file.witness.file.bytes,
        "manifest_id":file.witness.manifest_id, "witness_id":file.witness.witness_id,
        "snapshot_id":file.witness.snapshot_id,
        "disposition":file.disposition, "reason":file.reason,
        "files_restored":file.files_restored, "bytes_restored":file.bytes_restored,
        "evidence":match file.evidence {
            FileEvidence::NotAttempted => json!({"state":"not_attempted"}),
            FileEvidence::Staging(facts) => json!({
                "state":"staging", "hold":unavailable_detail(),
                "retained_stage":facts.retained_stage,
                "readback":facts.readback.map(|failure| failure.reason().to_string()),
            }),
            FileEvidence::Transaction(facts) => transaction_facts(&facts),
        },
    })
}

fn unavailable_detail() -> Value {
    json!({"detail_available":false})
}

fn unavailable_details(failures: &[String]) -> Vec<Value> {
    failures.iter().map(|_| unavailable_detail()).collect()
}

fn sync_facts(result: &io::Result<()>) -> Value {
    match result {
        Ok(()) => json!({"state":"ok"}),
        Err(error) => json!({"state":"error", "error":io_kind(error)}),
    }
}

fn transaction_facts(facts: &TransactionFacts<'_>) -> Value {
    #[cfg(unix)]
    let marker = facts.marker.map(marker_facts);
    #[cfg(not(unix))]
    let marker: Option<Value> = None;
    #[cfg(unix)]
    let marker_ancestor_sync = facts
        .marker_ancestor_sync
        .iter()
        .map(sync_facts)
        .collect::<Vec<_>>();
    #[cfg(not(unix))]
    let marker_ancestor_sync: Option<Value> = None;
    json!({
        "state":"transaction", "publication":facts.publication.map(media_publication_facts),
        "intent_writes":facts.intent_writes.iter().map(|write| {
            #[cfg(unix)]
            let publication = write.publication.as_ref().map(intent_publication_facts);
            #[cfg(not(unix))]
            let publication: Option<Value> = None;
            json!({
                "publication":publication, "named_read_confirmed":write.named_read_confirmed,
                "durability":write.durability, "failures":unavailable_details(&write.failures),
            })
        }).collect::<Vec<_>>(),
        "marker":marker, "marker_ancestor_sync":marker_ancestor_sync,
        "ledger":facts.ledger.as_ref().map(|ledger| {
            let effect = ledger.effect.map(|effect| match effect {
                LedgerEffect::AlreadyRecorded => json!({"effect":"already_recorded"}),
                LedgerEffect::Appended {bytes} => json!({"effect":"appended", "bytes":bytes}),
                LedgerEffect::Attempted {written} => json!({"effect":"attempted", "written":written}),
            });
            json!({"effect":effect, "durability":ledger.durability,
                "confirmed":ledger.confirmed, "failures":unavailable_details(&ledger.failures)})
        }),
        "register":facts.register.map(mutation_facts),
        "no_effect_cleanup":facts.no_effect_cleanup.as_ref().map(|cleanup| json!({
            "cleanup":cleanup.cleanup, "parent_sync":cleanup.parent_sync.map(sync_facts),
        })),
        "settlement_cleanup":facts.settlement_cleanup.as_ref().map(|cleanup| json!({
            "cleanup":cleanup.cleanup, "parent_sync":cleanup.parent_sync.map(sync_facts),
        })),
        "retirement":facts.retirement.as_ref().map(|retirement| json!({
            "removal":retirement.removal.map(removal_facts), "retained_stage":retirement.retained_stage,
            "ancestors":retirement.ancestors, "failures":unavailable_details(&retirement.failures),
            "retired":retirement.retired,
        })),
        "preparation":facts.preparation.as_ref().map(|preparation| json!({
            "attempted":preparation.attempted, "durability":preparation.durability,
            "confirmed":preparation.confirmed, "failures":unavailable_details(&preparation.failures),
        })),
        "preparation_failures":unavailable_details(&facts.preparation_failures),
        "hold":facts.hold.as_ref().map(|_| unavailable_detail()),
        "store_exclusion_retained":facts.store_exclusion_retained,
        "retained_media":facts.retained_media, "completed":facts.completed,
    })
}

fn media_publication_facts(receipt: &PublicationReceipt) -> Value {
    json!({
        "effect":receipt.effect, "confirmation":receipt.confirmation,
        "file_durability":receipt.file_durability, "parent_durability":receipt.parent_durability,
        "cleanup":receipt.cleanup, "error":receipt.error.as_ref().map(io_kind),
    })
}

#[cfg(unix)]
fn intent_publication_facts(publication: &IntentPublication<'_>) -> Value {
    use solstone_core_journal_io::atomic::{
        FinalNameConfirmation, MetadataDurability, StageCleanup,
    };
    match publication {
        IntentPublication::Create(result) => match result {
            Err(error) => json!({"kind":"create", "error":write_facts(error)}),
            Ok(publication) => {
                let confirmation = match &publication.final_name {
                    FinalNameConfirmation::Confirmed { .. } => json!({"state":"confirmed"}),
                    FinalNameConfirmation::Unverified { reason, .. } => {
                        json!({"state":"unverified", "error":io_kind(reason)})
                    }
                };
                let durability = match &publication.durability {
                    MetadataDurability::Confirmed => json!({"state":"confirmed"}),
                    MetadataDurability::Unproven { source } => {
                        json!({"state":"unproven", "error":source.as_ref().map(io_kind)})
                    }
                };
                let cleanup = match &publication.cleanup {
                    StageCleanup::Removed => json!({"state":"removed"}),
                    StageCleanup::Retained { source, .. } => {
                        json!({"state":"retained", "error":io_kind(source)})
                    }
                };
                json!({"kind":"create", "effect":"published", "bytes_written":publication.bytes_written,
                    "confirmation":confirmation, "durability":durability, "cleanup":cleanup})
            }
        },
        IntentPublication::Update(result) => match result {
            Ok(publication) => {
                json!({"kind":"update", "publication":publication_facts(publication)})
            }
            Err(error) => json!({"kind":"update", "error":write_facts(error)}),
        },
    }
}

fn io_kind(error: &io::Error) -> String {
    error.kind().to_string()
}

fn removal_facts(receipt: &RemovalReceipt) -> Value {
    json!({
        "effect":receipt.effect, "original_name":receipt.original_name,
        "claim_confirmation":receipt.claim_confirmation,
        "file_durability":receipt.file_durability, "source_durability":receipt.source_durability,
        "claim_durability":receipt.claim_durability, "stage_parent_durability":receipt.stage_parent_durability,
        "cleanup":receipt.cleanup, "restoration_attempted":receipt.restoration_attempted,
        "failures":receipt.failures.iter().map(|failure| json!({
            "phase":match failure.phase {
                RemovalPhase::Mutation => "mutation", RemovalPhase::FileSync => "file_sync",
                RemovalPhase::SourceSync => "source_sync", RemovalPhase::ClaimSync => "claim_sync",
                RemovalPhase::StageParentSync => "stage_parent_sync", RemovalPhase::Cleanup => "cleanup",
                RemovalPhase::Completion => "completion",
            }, "error":io_kind(&failure.error),
        })).collect::<Vec<_>>(),
    })
}

#[cfg(unix)]
fn marker_facts(
    result: &Result<
        solstone_core_journal_io::health_marker::BoundStreamMarkerPublication,
        solstone_core_journal_io::health_marker::BoundStreamMarkerError,
    >,
) -> Value {
    use solstone_core_journal_io::health_marker::BoundStreamMarkerError as Error;
    match result {
        Ok(publication) => json!({
            "publication":publication_facts(publication.publication()), "generation":publication.generation(),
            "binding_error":publication.binding_error().map(directory_facts),
            "lock_error":publication.lock_error().map(lock_facts),
        }),
        Err(error) => match error {
            Error::Parent(error) => json!({"error":"parent", "detail":directory_facts(error)}),
            Error::Lock(error) => json!({"error":"lock", "detail":lock_facts(error)}),
            Error::Read(error) => json!({"error":"read", "detail":directory_facts(error)}),
            Error::Malformed => json!({"error":"malformed"}),
            Error::Overflow => json!({"error":"overflow"}),
            Error::Serialize(_) => json!({"error":"serialize"}),
            Error::Publish(error) => json!({"error":"publish", "detail":write_facts(error)}),
        },
    }
}

#[cfg(unix)]
fn publication_facts(publication: &solstone_core_journal_io::atomic::BoundAtomicOutcome) -> Value {
    use solstone_core_journal_io::atomic::BoundAtomicOutcome as Publication;
    let (durability, observation) = match publication {
        Publication::Published { .. } => (None, None),
        Publication::PublishedDurabilityUncertain { source, .. } => (Some(io_kind(source)), None),
        Publication::PublishedObservationUncertain {
            source,
            durability_source,
            ..
        } => (
            durability_source.as_ref().map(io_kind),
            Some(io_kind(source)),
        ),
    };
    json!({"effect":"published", "durability_error":durability, "observation_error":observation})
}

fn mutation_facts(result: &Result<BoundMutationOutcome, RestoreRegisterRefusal>) -> Value {
    match result {
        Err(error) => {
            let error = match error {
                RestoreRegisterRefusal::Bound(error) => {
                    json!({"kind":"bound", "detail":mutation_error(error)})
                }
                RestoreRegisterRefusal::Restored(error) => {
                    json!({"kind":"restored", "detail":restored_mutation_error(error)})
                }
            };
            json!({"publication":"refused", "error":error})
        }
        Ok(BoundMutationOutcome::Unchanged(result)) => {
            json!({"publication":"unchanged", "status":result.status})
        }
        Ok(BoundMutationOutcome::Published(result)) => {
            json!({"publication":"published", "status":result.status})
        }
        Ok(BoundMutationOutcome::PublishedUncertain {
            candidate,
            uncertainty,
        }) => json!({
            "publication":"published_uncertain", "status":candidate.status,
            "durability_error":uncertainty.durability.as_ref().map(io_kind),
            "observation_error":uncertainty.observation.as_ref().map(io_kind),
            "binding_errors":uncertainty.bindings.iter().map(review_tag).collect::<Vec<_>>(),
            "lock_error":uncertainty.lock.as_ref().map(lock_facts),
        }),
    }
}

fn restored_mutation_error(error: &RestoredMarkRefusal) -> Value {
    match error {
        RestoredMarkRefusal::Unsupported => json!({"kind":"unsupported"}),
        RestoredMarkRefusal::InvalidPlan => json!({"kind":"invalid_plan"}),
        RestoredMarkRefusal::Changed => json!({"kind":"changed"}),
        RestoredMarkRefusal::Catalog(_) => {
            json!({"kind":"catalog", "detail":unavailable_detail()})
        }
        RestoredMarkRefusal::Readback(error) => {
            json!({"kind":"readback", "reason":error.to_string()})
        }
        RestoredMarkRefusal::Io(error) => json!({"kind":"io", "error":io_kind(error)}),
        RestoredMarkRefusal::Segment(_) => {
            json!({"kind":"segment", "detail":unavailable_detail()})
        }
        RestoredMarkRefusal::Bound(error) => {
            json!({"kind":"bound", "detail":mutation_error(error)})
        }
    }
}

fn mutation_error(error: &BoundMutationRefusal) -> Value {
    match error {
        BoundMutationRefusal::Unsupported => json!({"kind":"unsupported"}),
        BoundMutationRefusal::Review(error) => json!({"kind":"review", "reason":review_tag(error)}),
        BoundMutationRefusal::Lock(error) => json!({"kind":"lock", "detail":lock_facts(error)}),
        BoundMutationRefusal::Serialize(_) => json!({"kind":"serialize"}),
        #[cfg(unix)]
        BoundMutationRefusal::Write(error) => json!({"kind":"write", "detail":write_facts(error)}),
        #[cfg(windows)]
        BoundMutationRefusal::Write(error) => match error {
            solstone_core_journal_io::atomic::BoundAtomicPublishError::NamespaceChanged => {
                json!({"kind":"namespace_changed"})
            }
            solstone_core_journal_io::atomic::BoundAtomicPublishError::Atomic(error) => {
                json!({"kind":"write", "detail":write_facts(error)})
            }
        },
    }
}

#[cfg(any(unix, windows))]
fn write_facts(error: &solstone_core_journal_io::atomic::DetailedAtomicError) -> Value {
    json!({"operation":error.operation, "error":io_kind(&error.source), "orphan_stage":error.orphan_stage.is_some(), "cleanup_error":error.cleanup_error.as_ref().map(io_kind)})
}

fn review_tag(error: &ReviewRefusal) -> &'static str {
    match error {
        ReviewRefusal::Empty => "empty",
        ReviewRefusal::Duplicate => "duplicate",
        ReviewRefusal::Missing { .. } => "missing",
        ReviewRefusal::Stale { .. } => "stale",
        ReviewRefusal::Failed { .. } => "failed",
        ReviewRefusal::NotRequired { .. } => "not_required",
        ReviewRefusal::SourceRootChanged { .. } => "source_root_changed",
        ReviewRefusal::RootChanged => "root_changed",
        ReviewRefusal::Root(_) => "root",
        ReviewRefusal::Read(_) => "read",
        ReviewRefusal::Register(_) => "register",
    }
}

fn lock_facts(error: &ExistingParentLockError) -> Value {
    let (kind, source) = match error {
        ExistingParentLockError::InvalidLockPath { .. } => ("invalid_lock_path", None),
        ExistingParentLockError::MissingParent { .. } => ("missing_parent", None),
        ExistingParentLockError::UnsafeParent { .. } => ("unsafe_parent", None),
        ExistingParentLockError::UnsafeLockEntry { .. } => ("unsafe_lock_entry", None),
        ExistingParentLockError::WrongMode { .. } => ("wrong_mode", None),
        ExistingParentLockError::Io { source, .. } => ("io", Some(io_kind(source))),
        ExistingParentLockError::ParentChanged { .. } => ("parent_changed", None),
        ExistingParentLockError::NamespaceChanged { .. } => ("namespace_changed", None),
        ExistingParentLockError::Timeout(_) => ("timeout", None),
    };
    json!({"kind":kind, "error":source})
}

#[cfg(unix)]
fn directory_facts(error: &FlatDirectoryError) -> Value {
    let (kind, source) = match error {
        FlatDirectoryError::InvalidRelativePath { .. } => ("invalid_relative_path", None),
        FlatDirectoryError::InvalidName { .. } => ("invalid_name", None),
        FlatDirectoryError::NotDirectory { .. } => ("not_directory", None),
        FlatDirectoryError::SymlinkRefused { .. } => ("symlink_refused", None),
        FlatDirectoryError::NotRegular { .. } => ("not_regular", None),
        FlatDirectoryError::SizeLimitExceeded { .. } => ("size_limit_exceeded", None),
        FlatDirectoryError::IdentityChanged { .. } => ("identity_changed", None),
        FlatDirectoryError::EnumerationChanged { .. } => ("enumeration_changed", None),
        FlatDirectoryError::Io { source, .. } => ("io", Some(io_kind(source))),
    };
    json!({"kind":kind, "error":source})
}

#[cfg(test)]
mod tests;
