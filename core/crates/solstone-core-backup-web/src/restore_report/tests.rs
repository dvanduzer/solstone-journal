// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "pure public receipt and serialization controls"
)]

use super::*;
use solstone_core_journal_io::media::{Cleanup, Confirmation, Durability, PublicationEffect};
use solstone_core_offload::restore::owned::{
    LedgerFacts, NoEffectCleanupFacts, RetirementFacts, StagingFacts, StorePreparationFacts,
};
use solstone_core_retention::archive_catalog::{ArchiveWitness, OffloadFile, RestoreDisposition};
use solstone_core_retention::mark_mutation::{
    ConditionalResult, MutationStatus, RegisterUncertainty,
};
use solstone_core_retention::marks::Register;

const PRIVATE: &str = "/private/source-path arbitrary IO message";

fn transaction<'a>() -> TransactionFacts<'a> {
    TransactionFacts {
        publication: None,
        intent_writes: Vec::new(),
        #[cfg(unix)]
        marker: None,
        #[cfg(unix)]
        marker_ancestor_sync: &[],
        ledger: None,
        register: None,
        no_effect_cleanup: None,
        retirement: None,
        operation_id: Some(PRIVATE),
        store_exclusion_retained: true,
        retained_media: true,
        preparation: None,
        preparation_failures: Vec::new(),
        hold: None,
        completed: false,
    }
}

fn witness(bytes: u64) -> ArchiveWitness {
    ArchiveWitness {
        manifest_id: "manifest".into(),
        witness_id: "witness".into(),
        snapshot_id: "snapshot".into(),
        repository_id: Some(PRIVATE.into()),
        archive_path: Some(PRIVATE.into()),
        journal_id: Some(PRIVATE.into()),
        layout: None,
        segment_dir: Some(PRIVATE.into()),
        processing: None,
        file: OffloadFile {
            name: "audio.flac".into(),
            bytes,
            sha256: PRIVATE.into(),
        },
    }
}

fn file<'a>(
    witness: &'a ArchiveWitness,
    disposition: RestoreDisposition,
    bytes_restored: u64,
    evidence: FileEvidence<'a>,
) -> RestoreFileFacts<'a> {
    RestoreFileFacts {
        day: "20260101",
        stream: "_default",
        segment: "010000_001",
        witness,
        disposition,
        reason: Some("restore_held"),
        files_restored: u64::from(bytes_restored > 0),
        bytes_restored,
        evidence,
    }
}

#[test]
fn dispositions_order_duplicate_names_and_large_effects_are_preserved() {
    let bytes = (1_u64 << 53) + 19;
    let witness = witness(bytes);
    let publication = PublicationReceipt {
        effect: PublicationEffect::Published,
        confirmation: Confirmation::Unverified,
        file_durability: Durability::Synced,
        parent_durability: Durability::Unproven,
        cleanup: Cleanup::Pending,
        identity: None,
        error: Some(io::Error::other(PRIVATE)),
    };
    let dispositions = [
        RestoreDisposition::Published,
        RestoreDisposition::AlreadyPresent,
        RestoreDisposition::Conflict,
        RestoreDisposition::Unverified,
        RestoreDisposition::Unattempted,
    ];
    let rows = dispositions
        .into_iter()
        .map(|disposition| {
            let mut facts = transaction();
            facts.publication = Some(&publication);
            facts.hold = Some(PRIVATE.into());
            file_facts(file(
                &witness,
                disposition,
                bytes,
                FileEvidence::Transaction(Box::new(facts)),
            ))
        })
        .collect::<Vec<_>>();
    let encoded = serde_json::to_string(&rows).unwrap();
    let decoded: Vec<Value> = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded.len(), 5);
    for (index, disposition) in dispositions.into_iter().enumerate() {
        assert_eq!(
            decoded[index]["disposition"],
            serde_json::to_value(disposition).unwrap()
        );
        assert_eq!(decoded[index]["name"], "audio.flac");
        assert_eq!(decoded[index]["bytes_expected"].as_u64(), Some(bytes));
        assert_eq!(decoded[index]["bytes_restored"].as_u64(), Some(bytes));
        assert_eq!(decoded[index]["files_restored"], 1);
        assert_eq!(
            decoded[index]["evidence"]["publication"]["effect"],
            "published"
        );
        assert_eq!(
            decoded[index]["evidence"]["publication"]["confirmation"],
            "unverified"
        );
        assert_eq!(
            decoded[index]["evidence"]["publication"]["parent_durability"],
            "unproven"
        );
        assert_eq!(
            decoded[index]["evidence"]["publication"]["cleanup"],
            "pending"
        );
        assert!(!decoded[index]["evidence"]["completed"].as_bool().unwrap());
    }
    assert!(!encoded.contains(PRIVATE));
    let not_attempted = file_facts(file(
        &witness,
        RestoreDisposition::Unattempted,
        0,
        FileEvidence::NotAttempted,
    ));
    assert_eq!(not_attempted["evidence"], json!({"state":"not_attempted"}));
    assert_eq!(not_attempted["bytes_restored"], 0);
    let staging = file_facts(file(
        &witness,
        RestoreDisposition::Unverified,
        0,
        FileEvidence::Staging(StagingFacts {
            hold: PRIVATE.into(),
            retained_stage: true,
            readback: None,
        }),
    ));
    assert_eq!(staging["evidence"]["hold"], unavailable_detail());
    assert_eq!(staging["evidence"]["retained_stage"], true);
    assert!(!staging.to_string().contains(PRIVATE));
}

#[test]
fn opaque_diagnostics_keep_presence_without_disclosing_messages() {
    let mut facts = transaction();
    #[cfg(unix)]
    facts
        .intent_writes
        .push(solstone_core_offload::restore::owned::IntentWriteFacts {
            publication: None,
            named_read_confirmed: false,
            durability: [None; 4],
            failures: vec![PRIVATE.into()],
        });
    facts.ledger = Some(LedgerFacts {
        effect: None,
        durability: [false; 4],
        confirmed: false,
        failures: vec![PRIVATE.into(); 2],
    });
    facts.preparation = Some(StorePreparationFacts {
        attempted: &["root", "intents"],
        durability: [Durability::Unproven; 4],
        confirmed: false,
        failures: vec![PRIVATE.into(); 3],
    });
    facts.preparation_failures = vec![PRIVATE.into(); 4];
    facts.retirement = Some(RetirementFacts {
        removal: None,
        retained_stage: true,
        ancestors: [Durability::Unproven; 4],
        failures: vec![PRIVATE.into(); 5],
        retired: false,
    });
    facts.hold = Some(PRIVATE.into());
    let value = transaction_facts(&facts);
    assert_eq!(value["hold"], unavailable_detail());
    #[cfg(unix)]
    assert_eq!(
        value["intent_writes"][0]["failures"],
        json!([unavailable_detail()])
    );
    for (path, length) in [("ledger", 2), ("preparation", 3), ("retirement", 5)] {
        assert_eq!(
            value[path]["failures"],
            json!(vec![unavailable_detail(); length])
        );
    }
    assert_eq!(
        value["preparation_failures"],
        json!(vec![unavailable_detail(); 4])
    );
    assert!(!value.to_string().contains(PRIVATE));
    let empty = transaction_facts(&transaction());
    assert!(empty["hold"].is_null());
    assert_eq!(empty["preparation_failures"], json!([]));
    assert_eq!(empty["intent_writes"], json!([]));
    for field in [
        "publication",
        "ledger",
        "register",
        "preparation",
        "retirement",
        "no_effect_cleanup",
    ] {
        assert!(empty[field].is_null());
    }
}

#[test]
fn no_effect_retirement_sync_ledger_and_register_facts_stay_independent() {
    let ok = Ok(());
    let failed = Err(io::Error::new(io::ErrorKind::PermissionDenied, PRIVATE));
    for (sync, expected) in [
        (None, Value::Null),
        (Some(&ok), json!({"state":"ok"})),
        (
            Some(&failed),
            json!({"state":"error", "error":io::ErrorKind::PermissionDenied.to_string()}),
        ),
    ] {
        let mut facts = transaction();
        facts.no_effect_cleanup = Some(NoEffectCleanupFacts {
            cleanup: Cleanup::Complete,
            parent_sync: sync,
        });
        facts.retirement = Some(RetirementFacts {
            removal: None,
            retained_stage: false,
            ancestors: [Durability::Synced; 4],
            failures: Vec::new(),
            retired: true,
        });
        facts.completed = true;
        let witness = witness(3);
        let row = file_facts(file(
            &witness,
            RestoreDisposition::Conflict,
            0,
            FileEvidence::Transaction(Box::new(facts)),
        ));
        assert_eq!(row["bytes_restored"], 0);
        assert_eq!(row["disposition"], "conflict");
        assert_eq!(row["evidence"]["completed"], true);
        assert_eq!(row["evidence"]["retirement"]["retired"], true);
        assert_eq!(
            row["evidence"]["no_effect_cleanup"]["parent_sync"],
            expected
        );
    }
    let register = Ok(BoundMutationOutcome::PublishedUncertain {
        candidate: ConditionalResult {
            status: MutationStatus::Applied,
            register: Register::empty(),
        },
        uncertainty: RegisterUncertainty {
            durability: Some(io::Error::other(PRIVATE)),
            observation: None,
            bindings: vec![ReviewRefusal::RootChanged],
            lock: None,
        },
    });
    let refusal = Err(RestoreRegisterRefusal::Bound(
        BoundMutationRefusal::Unsupported,
    ));
    let restored_refusal = Err(RestoreRegisterRefusal::Restored(
        RestoredMarkRefusal::Changed,
    ));
    for (effect, expected) in [
        (
            LedgerEffect::AlreadyRecorded,
            json!({"effect":"already_recorded"}),
        ),
        (
            LedgerEffect::Appended { bytes: 7 },
            json!({"effect":"appended", "bytes":7}),
        ),
        (
            LedgerEffect::Attempted { written: Some(3) },
            json!({"effect":"attempted", "written":3}),
        ),
    ] {
        for mutation in [&register, &refusal, &restored_refusal] {
            let mut facts = transaction();
            facts.ledger = Some(LedgerFacts {
                effect: Some(effect),
                durability: [true, false, true, false],
                confirmed: false,
                failures: vec![PRIVATE.into()],
            });
            facts.register = Some(mutation);
            let value = transaction_facts(&facts);
            assert_eq!(value["ledger"]["effect"], expected);
            assert_eq!(
                value["ledger"]["durability"],
                json!([true, false, true, false])
            );
            assert_eq!(value["ledger"]["confirmed"], false);
            assert_eq!(value["register"], mutation_facts(mutation));
            assert!(value["register"].get("register").is_none());
            assert!(!value.to_string().contains(PRIVATE));
        }
    }
}

#[test]
fn typed_register_refusals_preserve_publication_and_hide_private_diagnostics() {
    use solstone_core_backup::archive_readback::ReadbackError;

    for (refusal, kind) in [
        (RestoredMarkRefusal::Unsupported, "unsupported"),
        (RestoredMarkRefusal::InvalidPlan, "invalid_plan"),
        (RestoredMarkRefusal::Changed, "changed"),
        (RestoredMarkRefusal::Catalog(PRIVATE.into()), "catalog"),
        (
            RestoredMarkRefusal::Readback(ReadbackError::TargetRootChanged),
            "readback",
        ),
        (
            RestoredMarkRefusal::Io(io::Error::new(io::ErrorKind::PermissionDenied, PRIVATE)),
            "io",
        ),
        (
            RestoredMarkRefusal::Segment(
                solstone_core_journal_io::ReadError::Io {
                    path: PRIVATE.into(),
                    source: io::Error::other(PRIVATE),
                }
                .into(),
            ),
            "segment",
        ),
        (
            RestoredMarkRefusal::Bound(Box::new(BoundMutationRefusal::Review(
                ReviewRefusal::RootChanged,
            ))),
            "bound",
        ),
    ] {
        let register = Err(RestoreRegisterRefusal::Restored(refusal));
        let publication = PublicationReceipt {
            effect: PublicationEffect::Published,
            confirmation: Confirmation::Unverified,
            file_durability: Durability::Synced,
            parent_durability: Durability::Unproven,
            cleanup: Cleanup::Pending,
            identity: None,
            error: None,
        };
        let mut facts = transaction();
        facts.publication = Some(&publication);
        facts.register = Some(&register);
        let witness = witness(17);
        let value = file_facts(file(
            &witness,
            RestoreDisposition::Unverified,
            17,
            FileEvidence::Transaction(Box::new(facts)),
        ));
        assert_eq!(value["bytes_restored"], 17);
        assert_eq!(value["disposition"], "unverified");
        assert_eq!(value["evidence"]["publication"]["effect"], "published");
        assert_eq!(value["evidence"]["register"]["publication"], "refused");
        assert_eq!(value["evidence"]["register"]["error"]["kind"], "restored");
        assert_eq!(
            value["evidence"]["register"]["error"]["detail"]["kind"],
            kind
        );
        assert!(!value.to_string().contains(PRIVATE));
    }
}

#[cfg(unix)]
#[test]
fn create_publication_keeps_independent_confirmation_durability_and_cleanup() {
    use solstone_core_journal_io::atomic::{
        ExclusivePublication, FinalNameConfirmation, MetadataDurability, StageCleanup,
    };
    let result = Ok(ExclusivePublication {
        bytes_written: 19,
        final_name: FinalNameConfirmation::Unverified {
            destination: PRIVATE.into(),
            reason: io::Error::other(PRIVATE),
        },
        durability: MetadataDurability::Unproven {
            source: Some(io::Error::other(PRIVATE)),
        },
        cleanup: StageCleanup::Retained {
            stage: PRIVATE.into(),
            source: io::Error::other(PRIVATE),
        },
    });
    let value = intent_publication_facts(&IntentPublication::Create(&result));
    assert_eq!(value["effect"], "published");
    assert_eq!(value["bytes_written"], 19);
    assert_eq!(value["confirmation"]["state"], "unverified");
    assert_eq!(value["durability"]["state"], "unproven");
    assert_eq!(value["cleanup"]["state"], "retained");
    assert!(!value.to_string().contains(PRIVATE));
}
