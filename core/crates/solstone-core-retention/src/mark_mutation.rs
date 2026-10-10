// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Exact-observation updates for removal proposals and durable recovery plans.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use solstone_core_journal_io::{JsonWriteOptions, hold_lock, write_json};

use crate::archive_proof::OffloadOffer;
use crate::marks::{
    Failure, Mark, MarkState, Register, StoreError, load_path, register_lock_options,
    register_path, validate,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationStatus {
    Applied,
    AlreadyApplied,
    Missing,
    Stale,
    Collision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConditionalResult {
    pub status: MutationStatus,
    pub register: Register,
}

/// A deterministic update that can be recorded before publishing restored media.
/// Deserialization supplies data only; execution validates the entire plan again.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedMarkMutation {
    expected: Mark,
    replacement: Option<Mark>,
}

fn invalid(reason: &'static str) -> StoreError {
    StoreError::Integrity { reason }
}

fn validate_one(mark: &Mark) -> Result<(), StoreError> {
    let mut register = Register::empty();
    if mark.offload.is_some() {
        register.version = 2;
    }
    register.marks.insert(mark.id.clone(), mark.clone());
    validate(&register)
}

impl PlannedMarkMutation {
    pub fn resolve(expected: &Mark) -> Result<Self, StoreError> {
        validate_one(expected)?;
        Ok(Self {
            expected: expected.clone(),
            replacement: None,
        })
    }

    pub fn failure(expected: &Mark, failure: Failure) -> Result<Self, StoreError> {
        validate_one(expected)?;
        let mut replacement = expected.clone();
        replacement.state = MarkState::Failed(failure);
        Ok(Self {
            expected: expected.clone(),
            replacement: Some(replacement),
        })
    }

    /// Keep only a sorted subset of the offered files, preserving the nonce.
    /// Legacy proposals have no per-file bytes or witness and cannot be shrunk.
    pub fn remainder(expected: &Mark, names: &[String]) -> Result<Self, StoreError> {
        validate_one(expected)?;
        if names.is_empty() {
            return Self::resolve(expected);
        }
        let names_set: BTreeSet<_> = names.iter().map(String::as_str).collect();
        if names_set.len() != names.len()
            || names
                .iter()
                .map(String::as_str)
                .ne(names_set.iter().copied())
            || names
                .iter()
                .any(|name| expected.proposal.names.binary_search(name).is_err())
        {
            return Err(invalid(
                "a remainder must be a sorted subset of the proposal",
            ));
        }
        let offer = expected
            .offload
            .as_ref()
            .ok_or_else(|| invalid("an unbound proposal has no exact remainder"))?;
        let mut proof = offer.proof.clone();
        proof
            .files
            .retain(|file| names_set.contains(file.name.as_str()));
        let bytes = proof
            .bytes()
            .map_err(|_| invalid("remainder bytes overflow"))?;
        let mut replacement = expected.clone();
        replacement.proposal.names = names.to_vec();
        replacement.proposal.bytes = bytes;
        replacement.id = crate::marks::MarkId::derive(expected.class, &expected.target, names);
        replacement.offload = Some(
            OffloadOffer::with_nonce(proof, offer.nonce.clone())
                .map_err(|_| invalid("invalid remainder archive evidence"))?,
        );
        validate_one(&replacement)?;
        Ok(Self {
            expected: expected.clone(),
            replacement: Some(replacement),
        })
    }

    pub fn expected(&self) -> &Mark {
        &self.expected
    }
    pub fn replacement(&self) -> Option<&Mark> {
        self.replacement.as_ref()
    }

    pub fn validate(&self) -> Result<(), StoreError> {
        validate_one(&self.expected)?;
        let Some(replacement) = &self.replacement else {
            return Ok(());
        };
        validate_one(replacement)?;
        if replacement == &self.expected {
            return Ok(());
        }
        if let MarkState::Failed(failure) = &replacement.state
            && Self::failure(&self.expected, failure.clone())?
                .replacement
                .as_ref()
                == Some(replacement)
        {
            return Ok(());
        }
        if Self::remainder(&self.expected, &replacement.proposal.names)?
            .replacement
            .as_ref()
            == Some(replacement)
        {
            return Ok(());
        }
        Err(invalid(
            "a planned mutation is not an exact failure or remainder",
        ))
    }
}

fn apply(
    register: &mut Register,
    plan: &PlannedMarkMutation,
) -> Result<MutationStatus, StoreError> {
    validate(register)?;
    plan.validate()?;
    match register.marks.get(&plan.expected.id) {
        None => {
            return Ok(
                if plan.replacement.as_ref().is_some_and(|replacement| {
                    register.marks.get(&replacement.id) == Some(replacement)
                }) {
                    MutationStatus::AlreadyApplied
                } else {
                    MutationStatus::Missing
                },
            );
        }
        Some(current) if current != &plan.expected => {
            return Ok(if plan.replacement.as_ref() == Some(current) {
                MutationStatus::AlreadyApplied
            } else {
                MutationStatus::Stale
            });
        }
        Some(_) => {}
    }
    if let Some(replacement) = &plan.replacement {
        if replacement == &plan.expected {
            return Ok(MutationStatus::AlreadyApplied);
        }
        if replacement.id != plan.expected.id && register.marks.contains_key(&replacement.id) {
            return Ok(MutationStatus::Collision);
        }
    }
    register.marks.remove(&plan.expected.id);
    if let Some(replacement) = &plan.replacement {
        register
            .marks
            .insert(replacement.id.clone(), replacement.clone());
    }
    validate(register)?;
    Ok(MutationStatus::Applied)
}

/// Apply only to the complete expected Mark under the register lock.
/// Existing v2 registers remain read-only until their writer is enabled.
pub fn conditional_mutate(
    journal: &Path,
    plan: &PlannedMarkMutation,
) -> Result<ConditionalResult, StoreError> {
    plan.validate()?;
    let path = register_path(journal);
    let _lock = hold_lock(&path, register_lock_options()).map_err(StoreError::Lock)?;
    let mut register = load_path(&path)?;
    if register.version != 1 || plan.expected.offload.is_some() {
        return Err(StoreError::WriterHeld);
    }
    let status = apply(&mut register, plan)?;
    if status == MutationStatus::Applied {
        write_json(
            &path,
            &register,
            JsonWriteOptions {
                mode: Some(0o600),
                ..JsonWriteOptions::default()
            },
        )
        .map_err(StoreError::Write)?;
    }
    Ok(ConditionalResult { status, register })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::marks::{MarkId, Proposal, RemovalClass};
    use crate::receipt::Target;

    fn mark() -> Mark {
        let target = Target {
            day: "20261009".into(),
            stream: "camera".into(),
            dir: "120000_60_suffix".into(),
        };
        let proposal = Proposal {
            bytes: 3,
            reason: "archive".into(),
            names: vec!["a.flac".into(), "b.flac".into()],
        };
        Mark {
            id: MarkId::derive(RemovalClass::PolicyRawRelease, &target, &proposal.names),
            class: RemovalClass::PolicyRawRelease,
            target,
            marked_at: "2026-10-10T00:00:00Z".into(),
            proposal,
            state: MarkState::Marked,
            offload: None,
        }
    }

    fn bound() -> Mark {
        let mut mark = mark();
        mark.class = RemovalClass::OffloadRawRelease;
        mark.id = MarkId::derive(mark.class, &mark.target, &mark.proposal.names);
        let files = mark
            .proposal
            .names
            .iter()
            .enumerate()
            .map(|(index, name)| crate::archive_proof::ArchiveFileProof {
                witness_id: name.clone(),
                name: name.clone(),
                bytes: if index == 0 { 1 } else { 2 },
                sha256: "a".repeat(64),
                archive_path: format!("/stage/{name}"),
                processing: crate::archive_proof::AcceptedProcessingObservation {
                    record: None,
                    has_analysis_row: true,
                },
            })
            .collect();
        mark.offload = Some(
            OffloadOffer::new(crate::archive_proof::OffloadProof {
                journal_id: "source".into(),
                manifest_id: "manifest".into(),
                target: mark.target.clone(),
                layout: solstone_core_journal_io::paths::SegmentLayout::Named,
                repository_id: "b".repeat(64),
                snapshot_id: "c".repeat(64),
                files,
            })
            .unwrap(),
        );
        mark
    }

    #[test]
    fn changed_full_marks_survive_old_completion_and_failure() {
        let journal = tempfile::tempdir().unwrap();
        let expected = mark();
        let path = register_path(journal.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut current = expected.clone();
        current.marked_at = "2026-10-10T00:00:01Z".into();
        let mut register = Register::empty();
        register.marks.insert(current.id.clone(), current.clone());
        let bytes = serde_json::to_vec(&register).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        for plan in [
            PlannedMarkMutation::resolve(&expected).unwrap(),
            PlannedMarkMutation::failure(
                &expected,
                Failure {
                    at: "now".into(),
                    reason: "failure".into(),
                    staged: None,
                },
            )
            .unwrap(),
        ] {
            assert_eq!(
                conditional_mutate(journal.path(), &plan).unwrap().status,
                MutationStatus::Stale
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        let failure = PlannedMarkMutation::failure(
            &current,
            Failure {
                at: "now".into(),
                reason: "failure".into(),
                staged: None,
            },
        )
        .unwrap();
        assert_eq!(
            conditional_mutate(journal.path(), &failure).unwrap().status,
            MutationStatus::Applied
        );
        assert_eq!(
            conditional_mutate(journal.path(), &failure).unwrap().status,
            MutationStatus::AlreadyApplied
        );
        assert_eq!(
            conditional_mutate(
                journal.path(),
                &PlannedMarkMutation::resolve(&current).unwrap()
            )
            .unwrap()
            .status,
            MutationStatus::Stale
        );
        let failed = failure.replacement().unwrap();
        assert_eq!(
            conditional_mutate(
                journal.path(),
                &PlannedMarkMutation::resolve(failed).unwrap()
            )
            .unwrap()
            .status,
            MutationStatus::Applied
        );
        assert_eq!(
            conditional_mutate(
                journal.path(),
                &PlannedMarkMutation::resolve(&current).unwrap()
            )
            .unwrap()
            .status,
            MutationStatus::Missing
        );
    }

    #[test]
    fn exact_remainder_is_durable_idempotent_and_collision_safe() {
        let expected = bound();
        let plan = PlannedMarkMutation::remainder(&expected, &["b.flac".into()]).unwrap();
        let plan: PlannedMarkMutation =
            serde_json::from_slice(&serde_json::to_vec(&plan).unwrap()).unwrap();
        let replacement = plan.replacement().unwrap();
        assert_eq!(replacement.proposal.bytes, 2);
        assert_eq!(
            replacement.offload.as_ref().unwrap().nonce,
            expected.offload.as_ref().unwrap().nonce
        );
        assert_ne!(
            replacement.offload.as_ref().unwrap().revision,
            expected.offload.as_ref().unwrap().revision
        );
        assert!(PlannedMarkMutation::remainder(&expected, &["z.flac".into()]).is_err());
        let mut register = Register::empty();
        register.version = 2;
        register.marks.insert(expected.id.clone(), expected.clone());
        register
            .marks
            .insert(replacement.id.clone(), replacement.clone());
        let before = register.clone();
        assert_eq!(
            apply(&mut register, &plan).unwrap(),
            MutationStatus::Collision
        );
        assert_eq!(register, before);
        register.marks.remove(&replacement.id);
        assert_eq!(
            apply(&mut register, &plan).unwrap(),
            MutationStatus::Applied
        );
        assert_eq!(
            apply(&mut register, &plan).unwrap(),
            MutationStatus::AlreadyApplied
        );
        register.marks.insert(expected.id.clone(), expected.clone());
        register
            .marks
            .get_mut(&expected.id)
            .unwrap()
            .proposal
            .reason = "newer".into();
        assert_eq!(apply(&mut register, &plan).unwrap(), MutationStatus::Stale);
    }

    #[test]
    fn bound_register_is_strict_and_all_legacy_writers_refuse() {
        use crate::marks::*;
        let journal = tempfile::tempdir().unwrap();
        let mark = bound();
        let mut register = Register::empty();
        register.version = 2;
        register.marks.insert(mark.id.clone(), mark.clone());
        let path = register_path(journal.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes = serde_json::to_vec(&register).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(load(journal.path()).unwrap(), register);
        assert!(matches!(
            preflight(journal.path(), std::slice::from_ref(&mark.id)),
            Err(PreflightRefusal::ArchiveBound { .. })
        ));
        assert!(matches!(
            decline(journal.path(), &mark.id),
            Err(PreflightRefusal::Register(StoreError::WriterHeld))
        ));
        let now = chrono::Utc::now();
        assert!(matches!(
            upsert(
                journal.path(),
                mark.class,
                &mark.target,
                mark.proposal.clone(),
                now
            ),
            Err(StoreError::WriterHeld)
        ));
        assert!(matches!(
            reconcile(journal.path(), mark.class, &[], now),
            Err(StoreError::WriterHeld)
        ));
        assert!(matches!(
            resolve(journal.path(), &mark.id),
            Err(StoreError::WriterHeld)
        ));
        assert!(matches!(
            reconcile_recovered(journal.path()),
            Err(StoreError::WriterHeld)
        ));
        assert!(matches!(
            record_failure(
                journal.path(),
                mark.class,
                &mark.target,
                &mark.proposal.names,
                Failure {
                    at: String::new(),
                    reason: "failure".into(),
                    staged: None
                },
                now
            ),
            Err(StoreError::WriterHeld)
        ));
        assert!(matches!(
            conditional_mutate(
                journal.path(),
                &PlannedMarkMutation::resolve(&mark).unwrap()
            ),
            Err(StoreError::WriterHeld)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let mut invalid = register.clone();
        invalid.version = 1;
        assert!(validate(&invalid).is_err());
        let mut invalid = register.clone();
        invalid.marks.get_mut(&mark.id).unwrap().offload = None;
        assert!(validate(&invalid).is_err());
        let mut invalid = register.clone();
        invalid.marks.get_mut(&mark.id).unwrap().proposal.bytes = 999;
        assert!(validate(&invalid).is_err());
        let mut invalid = serde_json::to_value(&register).unwrap();
        invalid["marks"][mark.id.as_str()]["offload"]["proof"]["target"]["unknown"] = true.into();
        assert!(serde_json::from_value::<Register>(invalid).is_err());
        let mut invalid = serde_json::to_value(
            PlannedMarkMutation::remainder(&mark, &["b.flac".into()]).unwrap(),
        )
        .unwrap();
        invalid["replacement"]["proposal"]["reason"] = "changed".into();
        assert!(
            serde_json::from_value::<PlannedMarkMutation>(invalid)
                .unwrap()
                .validate()
                .is_err()
        );
    }
}
