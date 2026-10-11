// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::{BoundMutationOutcome, RestoredMarkRefusal};
use solstone_core_journal_io::archive_recovery::RecoveredArchiveReservation;
use solstone_core_segment::output_attempt::{OutputClaims, SegmentCommit};

/// Borrows an actual recovered reservation and the original whole-Mark claims.
/// Saved intent data alone cannot enter the recovered register phase.
pub struct RecoveredRestoredMarkMutation<'a> {
    pub reservation: &'a RecoveredArchiveReservation,
    pub claims: &'a OutputClaims,
    pub commit: &'a SegmentCommit<'a>,
}

#[derive(Debug)]
pub enum RecoveredRestoredMarkRefusal {
    Mutation(RestoredMarkRefusal),
    PostMutation {
        outcome: Box<BoundMutationOutcome>,
        refusal: RestoredMarkRefusal,
    },
}

/// Retire the saved deterministic remainder without publishing media or retiring
/// recovery resources. A late refusal retains the actual register outcome.
pub fn conditional_retire_recovered_restored(
    input: RecoveredRestoredMarkMutation<'_>,
) -> Result<BoundMutationOutcome, RecoveredRestoredMarkRefusal> {
    #[cfg(not(target_os = "linux"))]
    {
        let _input = input;
        Err(RecoveredRestoredMarkRefusal::Mutation(
            RestoredMarkRefusal::Unsupported,
        ))
    }
    #[cfg(target_os = "linux")]
    {
        use super::{RestoredAdmission, RestoredFacts, mutate_restored};
        use crate::restore_intent::RestoreIntent;

        let reservation = input.reservation;
        reservation
            .revalidate_current_population()
            .map_err(|error| {
                RecoveredRestoredMarkRefusal::Mutation(RestoredMarkRefusal::Recovery(error))
            })?;
        let record = reservation.record();
        if !record.complete {
            return Err(RecoveredRestoredMarkRefusal::Mutation(
                RestoredMarkRefusal::Changed,
            ));
        }
        let intent = RestoreIntent::decode(Ok(&record.bytes)).map_err(|hold| {
            RecoveredRestoredMarkRefusal::Mutation(RestoredMarkRefusal::Intent(hold))
        })?;
        let projected = project(
            &intent,
            reservation.binding(),
            reservation.operation_id(),
            &record.entry.name,
        )
        .map_err(RecoveredRestoredMarkRefusal::Mutation)?;
        let facts = RestoredFacts {
            admission: RestoredAdmission::Recovered {
                reservation,
                destination: &intent.destination,
            },
            operation: &intent.event,
            confirmed: &projected.confirmed,
            plan: projected.plan,
            claims: input.claims,
            commit: input.commit,
        };
        let (outcome, _guard) =
            mutate_restored(&facts).map_err(RecoveredRestoredMarkRefusal::Mutation)?;
        if let Err(refusal) = facts.admission.revalidate() {
            return Err(RecoveredRestoredMarkRefusal::PostMutation {
                outcome: Box::new(outcome),
                refusal,
            });
        }
        Ok(outcome)
    }
}

#[cfg(target_os = "linux")]
struct Projected<'a> {
    plan: &'a super::PlannedMarkMutation,
    confirmed: solstone_core_journal_io::media::MediaObservation,
}

#[cfg(target_os = "linux")]
fn project<'a>(
    intent: &'a crate::restore_intent::RestoreIntent,
    binding: &solstone_core_journal_io::archive_recovery::ArchiveRecoveryBinding,
    operation_id: &str,
    record_name: &std::ffi::OsStr,
) -> Result<Projected<'a>, RestoredMarkRefusal> {
    use crate::archive_catalog::RestoreDisposition;
    use crate::restore_intent::{MarkRetirement, Obligation};
    use solstone_core_journal_io::activity_obligations::intent_filename;
    use solstone_core_journal_io::media::{
        Confirmation, Durability, MediaObservation, PublicationEffect,
    };

    intent.validate().map_err(RestoredMarkRefusal::Intent)?;
    if intent.event.operation_id != operation_id
        || intent_filename(operation_id).as_deref() != Some(record_name)
        || intent.recovery_binding.as_ref() != Some(binding)
        || intent.legacy_binding.is_some()
        || intent.marker != Obligation::Complete
        || intent.event_recorded != Obligation::Complete
        || !matches!(intent.register, Obligation::Pending | Obligation::Complete)
    {
        return Err(RestoredMarkRefusal::Changed);
    }
    let MarkRetirement::Exact { plan } = &intent.retirement else {
        return Err(RestoredMarkRefusal::InvalidPlan);
    };
    let proof = &plan
        .expected()
        .offload
        .as_ref()
        .ok_or(RestoredMarkRefusal::InvalidPlan)?
        .proof;
    if proof.target != intent.locator.target
        || proof.layout != intent.locator.layout
        || proof.repository_id != binding.repository_id()
    {
        return Err(RestoredMarkRefusal::Changed);
    }
    let (Some(outcome), Some(confirmed), [file]) = (
        &intent.outcome,
        &intent.confirmed,
        intent.event.files.as_slice(),
    ) else {
        return Err(RestoredMarkRefusal::InvalidPlan);
    };
    if !matches!(
        (outcome.effect, file.disposition),
        (PublicationEffect::Published, RestoreDisposition::Published)
            | (
                PublicationEffect::AlreadyPresent,
                RestoreDisposition::AlreadyPresent
            )
    ) || outcome.confirmation != Confirmation::Confirmed
        || outcome.identity != Some(confirmed.identity)
        || (outcome.effect == PublicationEffect::Published
            && (outcome.file_durability != Durability::Synced
                || outcome.parent_durability != Durability::Synced))
    {
        return Err(RestoredMarkRefusal::Changed);
    }
    Ok(Projected {
        plan,
        confirmed: MediaObservation {
            identity: confirmed.identity,
            digest: confirmed.digest.clone(),
        },
    })
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::archive_catalog::RestoreDisposition;
    use crate::restore_intent::{Obligation, RestoreIntent};
    use solstone_core_journal_io::activity_obligations::intent_filename;
    use solstone_core_journal_io::archive_recovery::ArchiveRecoveryBinding;
    use solstone_core_journal_io::media::{
        Cleanup, Confirmation, Durability, PublicationEffect, PublicationReceipt,
    };

    // Synthetic strict DATA only; this cannot create a recovered reservation.
    fn binding() -> ArchiveRecoveryBinding {
        serde_json::from_value(serde_json::json!({
            "format_version": 1,
            "origin": {
                "format_version": 1,
                "boot_id": "11111111-1111-1111-1111-111111111111",
                "machine_app_id_sha256": "a".repeat(64),
                "root_identity": "native-root-v1:unix:0000000000010306:0000000000000002",
                "canonical_chain": [
                    {"name": [], "identity": "native-root-v1:unix:0000000000010306:0000000000000001"},
                    {"name": [106], "identity": "native-root-v1:unix:0000000000010306:0000000000000002"}
                ],
                "filesystem": "ext4", "storage_topology": "direct-pcie-nvme",
                "filesystem_device": {"major": 259, "minor": 6},
                "namespace_device": {"major": 259, "minor": 0}, "partition": 1
            },
            "reservation_name": format!(".tmp-offload-{}", "b".repeat(32)),
            "reservation_identity": {"platform": "unix", "device": 66310, "inode": 3},
            "activity_lock_name": "backup-activity.lock",
            "activity_lock": {
                "device": 66310, "inode": 4, "size": 0,
                "modified_seconds": 5, "modified_nanoseconds": 6,
                "changed_seconds": 7, "changed_nanoseconds": 8,
                "mode": 33152, "links": 1
            },
            "repository_id": "c".repeat(64)
        }))
        .unwrap()
    }

    fn completed(with_mark: bool, effect: PublicationEffect) -> RestoreIntent {
        let mut intent = crate::restore_intent::tests::fixture(with_mark);
        intent.recovery_binding = Some(binding());
        intent
            .record_outcome(&PublicationReceipt {
                effect,
                confirmation: Confirmation::Confirmed,
                file_durability: Durability::Synced,
                parent_durability: Durability::Synced,
                cleanup: Cleanup::Complete,
                identity: Some(intent.stage.leaf),
                error: None,
            })
            .unwrap();
        intent.marker = Obligation::Complete;
        intent.event_recorded = Obligation::Complete;
        intent.validate().unwrap();
        intent
    }

    fn projected(intent: &RestoreIntent) -> Result<Projected<'_>, RestoredMarkRefusal> {
        project(
            intent,
            &binding(),
            &intent.event.operation_id,
            &intent_filename(&intent.event.operation_id).unwrap(),
        )
    }

    #[test]
    fn recovered_projection_preserves_native_success_and_exact_remainder() {
        for effect in [
            PublicationEffect::Published,
            PublicationEffect::AlreadyPresent,
        ] {
            let mut intent = completed(true, effect);
            if effect == PublicationEffect::AlreadyPresent {
                let facts = intent.outcome.as_mut().unwrap();
                facts.file_durability = Durability::Unproven;
                facts.parent_durability = Durability::Unproven;
            }
            for state in [Obligation::Pending, Obligation::Complete] {
                intent.register = state;
                let bytes = intent.encode().unwrap();
                let decoded = RestoreIntent::decode(Ok(&bytes)).unwrap();
                let facts = projected(&decoded).unwrap();
                assert_eq!(
                    facts.plan.expected(),
                    decoded.retirement.expected().unwrap()
                );
                assert_eq!(facts.plan.replacement().unwrap().proposal.names, ["b.flac"]);
                assert_eq!(facts.confirmed.digest, decoded.expected_digest());
            }
        }
    }

    #[test]
    fn recovered_projection_refuses_other_binding_or_selected_record() {
        let intent = completed(true, PublicationEffect::Published);
        let mut other = serde_json::to_value(binding()).unwrap();
        other["reservation_name"] = format!(".tmp-offload-{}", "e".repeat(32)).into();
        let other: ArchiveRecoveryBinding = serde_json::from_value(other).unwrap();
        let name = intent_filename(&intent.event.operation_id).unwrap();
        assert!(project(&intent, &other, &intent.event.operation_id, &name).is_err());
        assert!(project(&intent, &binding(), &"e".repeat(32), &name).is_err());
        assert!(
            project(
                &intent,
                &binding(),
                &intent.event.operation_id,
                std::ffi::OsStr::new("other.json")
            )
            .is_err()
        );
        let mut absent = intent.clone();
        absent.recovery_binding = None;
        assert!(projected(&absent).is_err());
    }

    #[test]
    fn recovered_projection_requires_saved_marker_event_and_exact_mark() {
        let intent = completed(true, PublicationEffect::Published);
        for marker in [true, false] {
            let mut pending = intent.clone();
            if marker {
                pending.marker = Obligation::Pending;
            } else {
                pending.event_recorded = Obligation::Pending;
            }
            pending.validate().unwrap();
            assert!(projected(&pending).is_err());
        }
        let no_mark = completed(false, PublicationEffect::AlreadyPresent);
        assert!(projected(&no_mark).is_err());
        let mut wrong_locator = intent.clone();
        wrong_locator.locator.target.dir = "120000_60_other".into();
        assert!(projected(&wrong_locator).is_err());
    }

    #[test]
    fn recovered_projection_refuses_unproven_or_unsuccessful_saved_effects() {
        let intent = completed(true, PublicationEffect::Published);
        for file_sync in [true, false] {
            let mut unproven = intent.clone();
            let facts = unproven.outcome.as_mut().unwrap();
            if file_sync {
                facts.file_durability = Durability::Unproven;
            } else {
                facts.parent_durability = Durability::Unproven;
            }
            unproven.validate().unwrap();
            assert!(projected(&unproven).is_err());
        }
        let mut unverified = intent.clone();
        unverified.outcome.as_mut().unwrap().confirmation = Confirmation::Unverified;
        unverified.event.files[0].disposition = RestoreDisposition::Unverified;
        unverified.confirmed = None;
        unverified.validate().unwrap();
        assert!(projected(&unverified).is_err());
        let mut unsuccessful = intent;
        unsuccessful.event.files[0].disposition = RestoreDisposition::Conflict;
        unsuccessful.outcome.as_mut().unwrap().effect = PublicationEffect::Conflict;
        assert!(projected(&unsuccessful).is_err());

        let mut mismatched = completed(true, PublicationEffect::AlreadyPresent);
        mismatched.outcome.as_mut().unwrap().identity =
            Some(crate::restore_intent::tests::identity(99));
        mismatched.validate().unwrap();
        assert!(projected(&mismatched).is_err());
    }
}
