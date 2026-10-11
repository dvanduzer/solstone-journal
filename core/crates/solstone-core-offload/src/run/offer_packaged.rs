// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Copied-package qualification only; production orchestration, cooperation,
//! settlement, carrier finish and provider durability remain caller obligations.
#![cfg(all(test, feature = "full-tests"))]
#![cfg(unix)]

use super::*;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solstone_core_backup::archive_activity::PreparedArchiveOperation;
use solstone_core_backup::archive_admission::{
    AdmissionError, ArchiveBudget, ArchiveCancellation, Clock,
};
use solstone_core_backup::archive_readback::{ReadAdmission, ReadOperation, ReadbackError};
use solstone_core_journal_io::media::{Cleanup, RootActivityCarrier};
use solstone_core_retention::archive_catalog::read_complete_catalog;
use solstone_core_retention::archive_offer::{
    OffloadOfferOutcome, OffloadOfferRefusal, OffloadOfferSelection, VerifiedOffloadOffers,
    capture_offload_offers, publish_offload_offers, verify_offload_offers,
};
use solstone_core_retention::archive_proof::ArchiveFileProof;
use solstone_core_retention::mark_mutation::{BoundMutationOutcome, MutationStatus};
use solstone_core_retention::marks;
use solstone_core_retention::remove_marked::ArchiveReadState;
use std::cell::Cell;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Case {
    #[serde(rename = "creation_positive")]
    Positive,
    #[serde(rename = "creation_floor_null")]
    FloorNull,
    #[serde(rename = "creation_stale_processing")]
    StaleProcessing,
    #[serde(rename = "creation_writer_uncertain")]
    WriterUncertain,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    case: Case,
    journal: PathBuf,
    repository_id: String,
    snapshot_id: String,
    archive_paths: Vec<String>,
    expected_restic_path: PathBuf,
    control: PathBuf,
}
struct ProbeClock(Cell<i64>);
impl Clock for ProbeClock {
    fn now_unix(&self) -> i64 {
        self.0.get()
    }
    fn iso_week(&self) -> u8 {
        1
    }
}
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}
fn cut(stream: &mut UnixStream, label: &[u8; 8]) -> io::Result<()> {
    stream.write_all(label)?;
    let mut reply = [0];
    stream.read_exact(&mut reply)?;
    if reply != *b"a" {
        return Err(invalid("owning acknowledgement refused"));
    }
    Ok(())
}
fn stages(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".tmp-offload-")
        {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}
fn check_carrier(carrier: &RootActivityCarrier) -> TestResult {
    carrier.revalidate()?;
    assert_eq!(
        stages(carrier.journal_root().canonical_path())?,
        vec![carrier.diagnostic_path()]
    );
    assert_eq!(fs::read_dir(carrier.diagnostic_path())?.count(), 0);
    Ok(())
}
fn file_fact(path: &Path) -> TestResult<Value> {
    let metadata = fs::metadata(path)?;
    let bytes = fs::read(path)?;
    Ok(
        json!({"device":metadata.dev(),"inode":metadata.ino(),"bytes":metadata.len(),"sha256":format!("{:x}",Sha256::digest(&bytes))}),
    )
}
fn read_facts(verified: &VerifiedOffloadOffers<'_>) -> TestResult<Vec<Value>> {
    let mut facts = Vec::new();
    for resource in verified.archive_reads() {
        let ArchiveReadState::Prepared {
            file,
            target_identity,
            media,
            cleanup,
        } = resource
        else {
            return Err(invalid("successful verification lost original preparation").into());
        };
        assert_eq!(cleanup, Some(Cleanup::Complete));
        assert_eq!(*target_identity, media.identity());
        assert_eq!(file.bytes, media.digest().size);
        assert_eq!(file.sha256, media.digest().sha256);
        facts.push(json!({"name":file.name,"repository":file.repository_id,"snapshot":file.snapshot_id,
            "path":file.archive_path,"identity":target_identity,"cleanup":cleanup,"digest":media.digest()}));
    }
    assert_eq!(facts.len(), 2);
    let proof = &verified.marks()[0]
        .offload
        .as_ref()
        .ok_or_else(|| invalid("genuine offer missing"))?
        .proof;
    for (actual, expected) in facts.iter().zip(&proof.files) {
        assert_eq!(actual["name"], expected.name);
        assert_eq!(actual["repository"], proof.repository_id);
        assert_eq!(actual["snapshot"], proof.snapshot_id);
        assert_eq!(actual["path"], expected.archive_path);
    }
    Ok(facts)
}
fn receipt_facts(receipts: &[Result<LedgerWriteReceipt, LedgerWriteRefusal>]) -> Vec<Value> {
    receipts.iter().map(|result|match result {
        Ok(r)=>json!({"effect":format!("{:?}",r.effect()),"durability":r.durability().map(|fact|format!("{fact:?}")),
            "confirmation":format!("{:?}",r.confirmation()),"failures":r.failures()}),
        Err(e)=>json!({"refusal":format!("{e:?}")}),
    }).collect()
}
enum Dispatch<'clock> {
    Held {
        verified: VerifiedOffloadOffers<'clock>,
        receipts: Vec<Result<LedgerWriteReceipt, LedgerWriteRefusal>>,
    },
    Published {
        outcome: OffloadOfferOutcome<'clock>,
        receipts: Vec<Result<LedgerWriteReceipt, LedgerWriteRefusal>>,
    },
}
// SAME owning gate for every successful verification. No selector or expected
// result reaches this decision; readable history never replaces sync receipts.
fn dispatch<'clock>(
    verified: VerifiedOffloadOffers<'clock>,
    receipts: Vec<Result<LedgerWriteReceipt, LedgerWriteRefusal>>,
    today: NaiveDate,
    now: DateTime<Utc>,
    publication_calls: &mut usize,
) -> Dispatch<'clock> {
    let confirmed = !receipts.is_empty()
        && receipts.len() == verified.marks().len()
        && receipts.iter().all(|result| match result {
            Ok(r) => {
                matches!(
                    r.effect(),
                    LedgerWriteEffect::Appended { .. } | LedgerWriteEffect::AlreadyRecorded
                ) && r.durability() == [LedgerSync::Confirmed; 4]
                    && r.confirmation() == LedgerConfirmation::Confirmed
                    && r.failures().is_empty()
            }
            Err(_) => false,
        });
    if confirmed {
        *publication_calls += 1;
        Dispatch::Published {
            outcome: publish_offload_offers(verified, today, now),
            receipts,
        }
    } else {
        Dispatch::Held { verified, receipts }
    }
}
struct FileSyncFault {
    consumed: bool,
}
impl unix::Hooks for FileSyncFault {
    fn sync(&mut self, file: &impl AsFd, point: unix::SyncPoint) -> io::Result<()> {
        if point == unix::SyncPoint::File {
            assert!(!self.consumed);
            self.consumed = true;
            Err(invalid("injected file-sync uncertainty"))
        } else {
            nix::unistd::fsync(file).map_err(io::Error::from)
        }
    }
}
#[test]
#[ignore = "requires copied package, existing synthetic snapshot and owning process/HTTP fixture"]
fn packaged_offer_creation_entry() -> TestResult {
    let input: Input = serde_json::from_slice(&fs::read(
        std::env::var_os("OFFLOAD_OFFER_CREATION_PROBE_JSON")
            .ok_or_else(|| invalid("explicit input required"))?,
    )?)?;
    assert_eq!(input.archive_paths.len(), 2);
    let root = Arc::new(JournalRoot::open(&input.journal)?);
    let register_path = input.journal.join("health/retention-marks.json");
    let ledger_path = input.journal.join("health/offload/20261009.jsonl");
    assert!(!register_path.try_exists()?);
    assert!(!ledger_path.try_exists()?);
    let selection = OffloadOfferSelection {
        target: Target {
            day: "20261009".into(),
            stream: "_default".into(),
            dir: "120000_60_fixture".into(),
        },
        layout: SegmentLayout::Direct,
        names: vec!["file1.flac".into(), "file2.flac".into()],
    };
    let raw_dir = input
        .journal
        .join(solstone_core_retention::layout::segment_rel(
            &selection.target.day,
            &selection.target.stream,
            &selection.target.dir,
        ));
    let before_raw = selection
        .names
        .iter()
        .map(|name| file_fact(&raw_dir.join(name)))
        .collect::<TestResult<Vec<_>>>()?;
    let before_headers = selection
        .names
        .iter()
        .map(|name| fs::read(raw_dir.join(name).with_extension("jsonl")))
        .collect::<io::Result<Vec<_>>>()?;
    let started = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())?;
    let clock = ProbeClock(Cell::new(started));
    let operation = PreparedArchiveOperation::prepare(
        Arc::clone(&root),
        ArchiveBudget::new_cancellable(&clock, ArchiveCancellation::new())?,
        &clock,
    )
    .map_err(|e| invalid(format!("actual original preparation: {e:?}")))?;
    let carrier = Arc::clone(operation.carrier());
    let activity = Arc::clone(operation.activity());
    let today = NaiveDate::from_ymd_opt(2026, 10, 10).ok_or_else(|| invalid("date"))?;
    let now = DateTime::from_timestamp(started, 0).ok_or_else(|| invalid("time"))?;
    let observed = capture_offload_offers(&operation, &[selection], today, now)
        .map_err(|e| invalid(format!("actual capture: {e:?}")))?;
    assert!(Arc::ptr_eq(observed.journal_root(), &root));
    assert!(Arc::ptr_eq(observed.activity(), &activity));
    assert_eq!(observed.groups().len(), 1);
    let group = &observed.groups()[0];
    assert_eq!(group.expected_mark(), None);
    assert_eq!(group.files().len(), 2);
    let proof = OffloadProof {
        journal_id: root.identity().observation_id(),
        manifest_id: "creation-fixture-manifest".into(),
        target: group.selection().target.clone(),
        layout: group.selection().layout,
        repository_id: input.repository_id.clone(),
        snapshot_id: input.snapshot_id.clone(),
        files: group
            .files()
            .iter()
            .zip(&input.archive_paths)
            .map(|(file, path)| ArchiveFileProof {
                witness_id: format!("creation-fixture-{}", file.name()),
                name: file.name().into(),
                bytes: file.observation().digest.size,
                sha256: file.observation().digest.sha256.clone(),
                archive_path: path.clone(),
                processing: file.processing().clone(),
            })
            .collect(),
    };
    let destination = operation
        .configuration()
        .destination()
        .ok_or_else(|| invalid("destination"))?;
    let password = operation
        .configuration()
        .keys()?
        .ok_or_else(|| invalid("key"))?
        .daily_key;
    let read = ReadOperation::admit(ReadAdmission::bind(
        operation,
        destination,
        password,
        input.repository_id,
        input.expected_restic_path,
    ))
    .map_err(|e| invalid(format!("actual read admission: {e:?}")))?;
    assert!(Arc::ptr_eq(read.carrier(), &carrier));
    let mut control = UnixStream::connect(&input.control)?;
    control.set_read_timeout(Some(Duration::from_secs(30)))?;
    control.set_write_timeout(Some(Duration::from_secs(30)))?;
    cut(&mut control, b"admitted")?;
    let result = verify_offload_offers(read, observed, vec![proof], today, now);
    if input.case == Case::FloorNull {
        let failure = match result {
            Err(f) => f,
            Ok(_) => return Err(invalid("null floor unexpectedly verified").into()),
        };
        assert!(matches!(
            failure.reason(),
            OffloadOfferRefusal::Readback(ReadbackError::Admission(
                AdmissionError::ConfigurationUnavailable
            ))
        ));
        assert!(Arc::ptr_eq(failure.journal_root(), &root));
        assert!(Arc::ptr_eq(failure.activity(), &activity));
        assert_eq!(failure.archive_reads().count(), 0);
        check_carrier(&carrier)?;
        assert!(!register_path.try_exists()?);
        assert!(!ledger_path.try_exists()?);
        for (i, name) in ["file1.flac", "file2.flac"].iter().enumerate() {
            assert_eq!(file_fact(&raw_dir.join(name))?, before_raw[i]);
            assert_eq!(
                fs::read(raw_dir.join(name).with_extension("jsonl"))?,
                before_headers[i]
            );
        }
        println!(
            "OFFER_CREATION_EVIDENCE {}",
            json!({"case":input.case,"refusal":format!("{:?}",failure.reason()),
            "carrier":carrier.diagnostic_path(),"reads":[],"publication_calls":0,"writer_calls":0,"raw":before_raw,
            "root_id":root.identity().observation_id(),"owners_retained":true})
        );
        io::stdout().flush()?;
        cut(&mut control, b"retained")?;
        assert!(Arc::ptr_eq(failure.observed().activity(), &activity));
        return Ok(());
    }
    let verified =
        result.map_err(|f| invalid(format!("actual verification refused: {:?}", f.reason())))?;
    assert!(Arc::ptr_eq(verified.journal_root(), &root));
    assert!(Arc::ptr_eq(verified.activity(), &activity));
    assert_eq!(verified.marks().len(), 1);
    let genuine_marks = verified.marks().to_vec();
    let reads = read_facts(&verified)?;
    check_carrier(&carrier)?;
    let mut fault = FileSyncFault { consumed: false };
    let receipts = verified
        .marks()
        .iter()
        .map(|mark| {
            let proof = &mark
                .offload
                .as_ref()
                .ok_or_else(|| invalid("genuine proof missing"))?
                .proof;
            Ok(if input.case == Case::WriterUncertain {
                unix::append_archive(&root, proof, 1, &mut fault)
            } else {
                append_archive_witness(&root, proof, 1)
            })
        })
        .collect::<TestResult<Vec<_>>>()?;
    assert_eq!(receipts.len(), 1);
    let receipt = receipts[0]
        .as_ref()
        .map_err(|e| invalid(format!("actual writer refused: {e:?}")))?;
    assert!(matches!(receipt.effect(),LedgerWriteEffect::Appended { bytes } if bytes>0));
    assert_eq!(receipt.confirmation(), LedgerConfirmation::Confirmed);
    if input.case == Case::WriterUncertain {
        assert!(fault.consumed);
        assert_eq!(
            receipt.durability(),
            [
                LedgerSync::Uncertain,
                LedgerSync::Confirmed,
                LedgerSync::Confirmed,
                LedgerSync::Confirmed
            ]
        );
        assert!(!receipt.failures().is_empty());
    } else {
        assert!(!fault.consumed);
        assert_eq!(receipt.durability(), [LedgerSync::Confirmed; 4]);
        assert!(receipt.failures().is_empty());
    }
    let ledger_bytes = fs::read(&ledger_path)?;
    let catalog = read_complete_catalog(&root).map_err(invalid)?;
    assert!(!catalog.degraded());
    let proof = &genuine_marks[0]
        .offload
        .as_ref()
        .ok_or_else(|| invalid("genuine proof missing"))?
        .proof;
    for file in &proof.files {
        assert!(catalog.active_witness(proof, file).is_some());
    }
    assert!(!register_path.try_exists()?);
    cut(&mut control, b"written!")?;
    let mut publication_calls = 0;
    let dispatched = dispatch(verified, receipts, today, now, &mut publication_calls);
    let (retained, receipts, publication) = match &dispatched {
        Dispatch::Held { verified, receipts } => {
            assert_eq!(input.case, Case::WriterUncertain);
            assert_eq!(publication_calls, 0);
            assert!(!register_path.try_exists()?);
            (verified, receipts, "held_uncertain_receipt")
        }
        Dispatch::Published { outcome, receipts } => {
            assert_eq!(publication_calls, 1);
            if input.case == Case::StaleProcessing {
                assert!(matches!(
                    outcome.publication(),
                    Err(OffloadOfferRefusal::Changed)
                ));
                assert!(!register_path.try_exists()?);
            } else {
                let Ok(BoundMutationOutcome::Published(result)) = outcome.publication() else {
                    return Err(invalid(format!(
                        "actual first publication: {:?}",
                        outcome.publication()
                    ))
                    .into());
                };
                assert_eq!(result.status, MutationStatus::Applied);
                let current = marks::load(root.canonical_path())?;
                assert_eq!(current.version, 2);
                assert_eq!(current, result.register);
                assert_eq!(
                    current.marks.values().cloned().collect::<Vec<_>>(),
                    genuine_marks
                );
            }
            (
                outcome.verified(),
                receipts,
                if input.case == Case::StaleProcessing {
                    "changed"
                } else {
                    "published_applied"
                },
            )
        }
    };
    assert_eq!(read_facts(retained)?, reads);
    assert!(Arc::ptr_eq(retained.journal_root(), &root));
    assert!(Arc::ptr_eq(retained.activity(), &activity));
    assert_eq!(retained.marks(), genuine_marks);
    assert_eq!(fs::read(&ledger_path)?, ledger_bytes);
    assert_eq!(read_complete_catalog(&root).map_err(invalid)?, catalog);
    let after_headers = ["file1.flac", "file2.flac"]
        .iter()
        .map(|name| fs::read(raw_dir.join(name).with_extension("jsonl")))
        .collect::<io::Result<Vec<_>>>()?;
    if input.case == Case::StaleProcessing {
        assert_eq!(after_headers[0], before_headers[0]);
        let end = before_headers[1]
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| invalid("processing header missing"))?
            + 1;
        assert_eq!(after_headers[1], before_headers[1][..end]);
    } else {
        assert_eq!(after_headers, before_headers);
    }
    for (i, name) in ["file1.flac", "file2.flac"].iter().enumerate() {
        assert_eq!(file_fact(&raw_dir.join(name))?, before_raw[i]);
    }
    check_carrier(&carrier)?;
    println!(
        "OFFER_CREATION_EVIDENCE {}",
        json!({"case":input.case,"carrier":carrier.diagnostic_path(),"reads":reads,
        "publication":publication,"publication_calls":publication_calls,"writer_calls":receipts.len(),"receipts":receipt_facts(receipts),
        "marks":genuine_marks,"raw":before_raw,"ledger_sha256":format!("{:x}",Sha256::digest(&ledger_bytes)),
        "root_id":root.identity().observation_id(),"owners_retained":true})
    );
    io::stdout().flush()?;
    cut(&mut control, b"retained")?;
    assert_eq!(read_facts(retained)?, reads);
    Ok(())
}
