// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::error::Error;
use std::io;

use solstone_core_journal_io::paths::{DEFAULT_STREAM, SegmentLayout};

use super::*;
use crate::archive_proof::{
    AcceptedProcessingObservation, ArchiveFileProof, OffloadOffer, OffloadProof,
};
use crate::marks::{Failure, MarkId, Proposal};
use crate::receipt::Target;

const SOURCE: &str = "native-root-v1:unix:0000000000000001:0000000000000002";

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}

fn mark(dir: &str, name: &str) -> TestResult<Mark> {
    let proof = OffloadProof {
        journal_id: SOURCE.into(),
        manifest_id: "manifest".into(),
        target: Target {
            day: "20261009".into(),
            stream: DEFAULT_STREAM.into(),
            dir: dir.into(),
        },
        layout: SegmentLayout::Direct,
        repository_id: "a".repeat(64),
        snapshot_id: "b".repeat(64),
        files: vec![ArchiveFileProof {
            witness_id: "witness".into(),
            name: name.into(),
            bytes: 7,
            sha256: "c".repeat(64),
            archive_path: format!("/private-stage/{name}"),
            processing: AcceptedProcessingObservation {
                record: None,
                has_analysis_row: true,
            },
        }],
    };
    let offer = OffloadOffer::new(proof)?;
    let names = vec![name.into()];
    Ok(Mark {
        id: MarkId::derive(RemovalClass::OffloadRawRelease, &offer.proof.target, &names),
        class: RemovalClass::OffloadRawRelease,
        target: offer.proof.target.clone(),
        marked_at: "2026-10-09T12:00:00Z".into(),
        proposal: Proposal {
            bytes: offer.proof.bytes()?,
            reason: "fixture".into(),
            names,
        },
        state: MarkState::Marked,
        offload: Some(offer),
    })
}

fn offer_mut(mark: &mut Mark) -> TestResult<&mut OffloadOffer> {
    Ok(mark
        .offload
        .as_mut()
        .ok_or_else(|| invalid("fixture offer missing"))?)
}

fn file_mut(mark: &mut Mark) -> TestResult<&mut ArchiveFileProof> {
    Ok(offer_mut(mark)?
        .proof
        .files
        .first_mut()
        .ok_or_else(|| invalid("fixture file missing"))?)
}

fn invalid_selection(marks: &[Mark]) {
    assert!(matches!(
        selected_files(marks, SOURCE, &"a".repeat(64)),
        Err(ArchiveApprovalError::InvalidSelection)
    ));
}

#[test]
fn complete_selection_preserves_input_order_and_exact_tuple() -> TestResult {
    let mut first = mark("120000_60_first", "audio.flac")?;
    let mut second = mark("130000_60_second", "other.flac")?;
    let proof = &mut offer_mut(&mut first)?.proof;
    let file = proof
        .files
        .first_mut()
        .ok_or_else(|| invalid("file missing"))?;
    file.archive_path = "/C:/private-stage/audio.flac".into();
    let old = first
        .offload
        .take()
        .ok_or_else(|| invalid("offer missing"))?;
    first.offload = Some(OffloadOffer::with_nonce(old.proof, old.nonce)?);
    let old = second
        .offload
        .take()
        .ok_or_else(|| invalid("offer missing"))?;
    second.offload = Some(OffloadOffer::with_nonce(old.proof, old.nonce)?);
    let selected = vec![second.clone(), first.clone()];
    let files = selected_files(&selected, SOURCE, &"a".repeat(64))
        .map_err(|reason| invalid(&format!("valid selection refused: {reason:?}")))?;
    assert_eq!(files.len(), 2);
    let tuples: Vec<_> = files
        .iter()
        .map(|file| {
            (
                file.repository_id.as_str(),
                file.snapshot_id.as_str(),
                file.name.as_str(),
                file.archive_path.as_str(),
                file.bytes,
                file.sha256.as_str(),
            )
        })
        .collect();
    assert_eq!(
        tuples,
        vec![
            (
                "a".repeat(64).as_str(),
                "b".repeat(64).as_str(),
                "other.flac",
                "/private-stage/other.flac",
                7,
                "c".repeat(64).as_str()
            ),
            (
                "a".repeat(64).as_str(),
                "b".repeat(64).as_str(),
                "audio.flac",
                "/C:/private-stage/audio.flac",
                7,
                "c".repeat(64).as_str()
            ),
        ]
    );
    assert_eq!(selected, vec![second, first]);
    Ok(())
}

#[test]
fn empty_duplicate_ordinary_failed_and_unbound_refuse() -> TestResult {
    invalid_selection(&[]);
    let valid = mark("120000_60_first", "audio.flac")?;
    invalid_selection(&[valid.clone(), valid.clone()]);
    let mut ordinary = valid.clone();
    ordinary.class = RemovalClass::OwnerRawRelease;
    ordinary.offload = None;
    ordinary.id = MarkId::derive(ordinary.class, &ordinary.target, &ordinary.proposal.names);
    invalid_selection(&[ordinary]);
    let mut failed = valid.clone();
    failed.state = MarkState::Failed(Failure {
        at: "2026-10-09T12:00:00Z".into(),
        reason: "fixture".into(),
        staged: None,
    });
    invalid_selection(&[failed]);
    let mut unbound = valid;
    unbound.offload = None;
    invalid_selection(&[unbound]);
    Ok(())
}

#[test]
fn owning_register_refuses_complete_mark_mismatches() -> TestResult {
    let valid = mark("120000_60_first", "audio.flac")?;
    let mut bytes = valid.clone();
    bytes.proposal.bytes = 8;
    invalid_selection(&[bytes]);
    let mut names = valid.clone();
    names.proposal.names = vec!["other.flac".into()];
    names.id = MarkId::derive(names.class, &names.target, &names.proposal.names);
    invalid_selection(&[names]);
    let mut target = valid.clone();
    target.target.dir = "130000_60_changed".into();
    target.id = MarkId::derive(target.class, &target.target, &target.proposal.names);
    invalid_selection(&[target]);
    let mut id = valid;
    id.id = mark("130000_60_other", "audio.flac")?.id;
    invalid_selection(&[id]);
    Ok(())
}

#[test]
fn owning_offer_refuses_nonce_revision_and_proof_tampering() -> TestResult {
    let valid = mark("120000_60_first", "audio.flac")?;
    let mut nonce = valid.clone();
    let mut encoded = serde_json::to_value(nonce.offload.as_ref())?;
    let object = encoded
        .as_object_mut()
        .ok_or_else(|| invalid("offer encoding missing"))?;
    object.insert("nonce".into(), serde_json::Value::String("d".repeat(64)));
    nonce.offload = Some(serde_json::from_value(encoded)?);
    invalid_selection(&[nonce]);
    let mut revision = valid.clone();
    let other = mark("130000_60_other", "other.flac")?;
    offer_mut(&mut revision)?.revision = other
        .offload
        .ok_or_else(|| invalid("offer missing"))?
        .revision;
    invalid_selection(&[revision]);
    for change in [
        |mark: &mut Mark| -> TestResult {
            file_mut(mark)?.archive_path = "/x/../audio.flac".into();
            Ok(())
        },
        |mark: &mut Mark| -> TestResult {
            offer_mut(mark)?.proof.snapshot_id = "latest".into();
            Ok(())
        },
        |mark: &mut Mark| -> TestResult {
            file_mut(mark)?.processing.has_analysis_row = false;
            Ok(())
        },
        |mark: &mut Mark| -> TestResult {
            offer_mut(mark)?.proof.target.day = "20260230".into();
            Ok(())
        },
        |mark: &mut Mark| -> TestResult {
            file_mut(mark)?.sha256 = "partial".into();
            Ok(())
        },
    ] {
        let mut changed = valid.clone();
        change(&mut changed)?;
        assert!(offer_mut(&mut changed)?.proof.validate().is_err());
        invalid_selection(&[changed]);
    }
    Ok(())
}

#[test]
fn valid_offers_require_exact_source_repository_for_every_row() -> TestResult {
    let valid = mark("120000_60_first", "audio.flac")?;
    assert!(matches!(
        selected_files(
            std::slice::from_ref(&valid),
            "native-root-v1:unix:0000000000000001:0000000000000003",
            &"a".repeat(64)
        ),
        Err(ArchiveApprovalError::SourceRootChanged)
    ));
    assert!(matches!(
        selected_files(std::slice::from_ref(&valid), SOURCE, &"d".repeat(64)),
        Err(ArchiveApprovalError::RepositoryChanged)
    ));
    let mut later = mark("130000_60_second", "other.flac")?;
    let old = later
        .offload
        .take()
        .ok_or_else(|| invalid("offer missing"))?;
    let mut proof = old.proof;
    proof.repository_id = "d".repeat(64);
    later.offload = Some(OffloadOffer::with_nonce(proof, old.nonce)?);
    assert!(matches!(
        selected_files(&[valid, later], SOURCE, &"a".repeat(64)),
        Err(ArchiveApprovalError::RepositoryChanged)
    ));
    Ok(())
}

#[cfg(all(unix, feature = "full-tests"))]
mod packaged;
