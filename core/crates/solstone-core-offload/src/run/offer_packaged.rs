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

// Separate caller qualification; the preceding four-control leaf remains frozen.
#[cfg(all(test, feature = "full-tests"))]
mod caller_qualification {
    use super::*;
    use crate::run::{
        OffloadCreationOwner, OffloadCreationRefusal, OffloadLedgerEffect, OffloadLedgerSync,
        OffloadLedgerWrite, create_offload_offers,
    };
    use chrono::Datelike;
    use solstone_core_backup_runtime::{
        BackupServices, NativeJournalMaintenance, SystemToolRunner, ToolOutput, ToolRequest,
        ToolRunner, UreqHttpTransport,
    };
    use solstone_core_retention::archive_proof::OffloadProof;
    use std::cell::RefCell;

    #[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    enum CallerCase {
        #[serde(rename = "caller_first_absent")]
        FirstAbsent,
        #[serde(rename = "caller_orphan_reuse")]
        OrphanReuse,
        #[serde(rename = "caller_writer_uncertain")]
        WriterUncertain,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct CallerInput {
        case: CallerCase,
        journal: PathBuf,
        repository_id: String,
        snapshot_id: String,
        archive_paths: Vec<String>,
        expected_restic_path: PathBuf,
        control: PathBuf,
    }
    struct NativeClock;
    impl Clock for NativeClock {
        fn now_unix(&self) -> i64 {
            Utc::now().timestamp()
        }
        fn iso_week(&self) -> u8 {
            Utc::now().iso_week().week() as u8
        }
    }

    // Observation delegates the same actual request and cancellation, retaining
    // actual output even on failure. It grants no admission or settlement.
    struct ObservedRunner(RefCell<Vec<Value>>);
    impl ObservedRunner {
        fn observe(
            &self,
            request: &ToolRequest<'_>,
            cancellation: Option<&ArchiveCancellation>,
        ) -> io::Result<ToolOutput> {
            use std::os::fd::AsRawFd;
            let argv = request
                .argv
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let sources = argv
                .iter()
                .filter(|arg| arg.starts_with("/proc/self/fd/"))
                .map(|path| Ok(json!({"path":path,"fact":file_fact(Path::new(path))?})))
                .collect::<TestResult<Vec<_>>>()
                .map_err(|error| invalid(error.to_string()))?;
            let descriptors = request
                .pass_fds
                .iter()
                .map(|fd| {
                    let path = format!("/proc/self/fd/{}", fd.as_raw_fd());
                    let metadata = fs::metadata(&path)?;
                    Ok(json!({"fd":fd.as_raw_fd(),"device":metadata.dev(),"inode":metadata.ino()}))
                })
                .collect::<io::Result<Vec<_>>>()?;
            let start = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos();
            let result = match cancellation {
                Some(cancellation) => SystemToolRunner.run_cancellable(request, cancellation),
                None => SystemToolRunner.run(request),
            };
            let end = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos();
            let outcome = match &result {
                Ok(output) => {
                    json!({"returncode":output.returncode,"stdout_bytes":output.stdout,"stderr_bytes":output.stderr})
                }
                Err(error) => {
                    json!({"error":error.to_string(),"kind":format!("{:?}",error.kind())})
                }
            };
            self.0.borrow_mut().push(json!({"program":request.program.to_string_lossy(),"argv":argv,
                "environment_keys":request.env.keys().map(|key|key.to_string_lossy()).collect::<Vec<_>>(),
                "timeout_ms":request.timeout.map(|timeout|timeout.as_millis()),"stdin_present":request.stdin.is_some(),
                "cancellable":cancellation.is_some(),"sources":sources,"descriptors":descriptors,
                "started_unix_ns":start,"ended_unix_ns":end,"outcome":outcome}));
            result
        }
    }
    impl ToolRunner for ObservedRunner {
        fn run(&self, request: &ToolRequest<'_>) -> io::Result<ToolOutput> {
            self.observe(request, None)
        }
        fn run_cancellable(
            &self,
            request: &ToolRequest<'_>,
            cancellation: &ArchiveCancellation,
        ) -> io::Result<ToolOutput> {
            self.observe(request, Some(cancellation))
        }
    }

    #[test]
    #[ignore = "requires copied package and owning actual caller/S3/child fixture"]
    fn packaged_offer_caller_entry() -> TestResult {
        let input: CallerInput = serde_json::from_slice(&fs::read(
            std::env::var_os("OFFLOAD_OFFER_CALLER_PROBE_JSON")
                .ok_or_else(|| invalid("explicit caller input required"))?,
        )?)?;
        assert_eq!(input.archive_paths.len(), 2);
        let root = Arc::new(JournalRoot::open(&input.journal)?);
        let register_path = input.journal.join("health/retention-marks.json");
        let ledger_path = input.journal.join("health/offload/20261009.jsonl");
        assert!(!register_path.try_exists()?);
        assert!(!ledger_path.try_exists()?);
        assert!(stages(&input.journal)?.is_empty());
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
        let raw = selection
            .names
            .iter()
            .map(|name| file_fact(&raw_dir.join(name)))
            .collect::<TestResult<Vec<_>>>()?;
        let headers = selection
            .names
            .iter()
            .map(|name| fs::read(raw_dir.join(name).with_extension("jsonl")))
            .collect::<io::Result<Vec<_>>>()?;
        let configuration = fs::read(input.journal.join("config/journal.json"))?;
        let clock = NativeClock;
        let now = Utc::now();
        let today = NaiveDate::from_ymd_opt(2026, 10, 10).ok_or_else(|| invalid("date"))?;
        let budget = ArchiveBudget::new_cancellable(&clock, ArchiveCancellation::new())?;
        let budget_seconds = budget.remaining(&clock)?.as_secs();
        let operation = PreparedArchiveOperation::prepare(Arc::clone(&root), budget, &clock)
            .map_err(|error| invalid(format!("actual caller preparation: {error:?}")))?;
        let activity = Arc::clone(operation.activity());
        let carrier = Arc::clone(operation.carrier());
        let mut seed = None;
        if input.case == CallerCase::OrphanReuse {
            let observed =
                capture_offload_offers(&operation, std::slice::from_ref(&selection), today, now)
                    .map_err(|error| invalid(format!("actual orphan capture: {error:?}")))?;
            assert!(Arc::ptr_eq(observed.journal_root(), &root));
            assert!(Arc::ptr_eq(observed.activity(), &activity));
            assert_eq!(observed.groups().len(), 1);
            let group = &observed.groups()[0];
            assert!(group.expected_mark().is_none());
            let proof = OffloadProof {
                target: selection.target.clone(),
                layout: selection.layout,
                journal_id: root.identity().observation_id(),
                repository_id: input.repository_id.clone(),
                snapshot_id: input.snapshot_id.clone(),
                manifest_id: "4".repeat(64),
                files: group
                    .files()
                    .iter()
                    .zip(&input.archive_paths)
                    .enumerate()
                    .map(|(index, (file, path))| ArchiveFileProof {
                        witness_id: if index == 0 {
                            "5".repeat(64)
                        } else {
                            "6".repeat(64)
                        },
                        name: file.name().into(),
                        bytes: file.observation().digest.size,
                        sha256: file.observation().digest.sha256.clone(),
                        archive_path: path.clone(),
                        processing: file.processing().clone(),
                    })
                    .collect(),
            };
            proof.validate()?;
            let receipt = append_archive_witness(&root, &proof, u64::try_from(now.timestamp())?)
                .map_err(|error| invalid(format!("actual orphan seed writer: {error:?}")))?;
            assert!(matches!(receipt.effect(),LedgerWriteEffect::Appended {bytes} if bytes>0));
            assert_eq!(receipt.durability(), [LedgerSync::Confirmed; 4]);
            assert_eq!(receipt.confirmation(), LedgerConfirmation::Confirmed);
            assert!(receipt.failures().is_empty());
            let bytes = fs::read(&ledger_path)?;
            let catalog = read_complete_catalog(&root).map_err(invalid)?;
            for file in &proof.files {
                assert!(catalog.active_witness(&proof, file).is_some());
            }
            assert!(!register_path.try_exists()?);
            seed = Some((proof, receipt, bytes, catalog));
        }
        let runner = ObservedRunner(RefCell::new(Vec::new()));
        let http = UreqHttpTransport;
        let maintenance = NativeJournalMaintenance;
        let services = BackupServices {
            runner: &runner,
            http: &http,
            clock: &clock,
            restic_path: Some(&input.expected_restic_path),
            rclone_path: None,
            version: "actual-caller-fixture",
            journal_maintenance: &maintenance,
        };
        let mut control = UnixStream::connect(&input.control)?;
        cut(&mut control, b"prepared")?;
        let fault = if input.case == CallerCase::WriterUncertain {
            Some(unix::FileSyncUncertainty::arm()?)
        } else {
            None
        };
        let creation = create_offload_offers(
            operation,
            &services,
            std::slice::from_ref(&selection),
            today,
            now,
        );
        println!(
            "CALLER_RETURN_FACTS {}",
            json!({"case":input.case,"reason":format!("{:?}",creation.reason()),
            "archives":creation.archives().iter().map(|result|format!("{result:?}")).collect::<Vec<_>>(),
            "requests":runner.0.borrow().clone(),"fault":fault.as_ref().map(unix::FileSyncUncertainty::observations)})
        );
        let (verified, publication) = match (input.case, creation.owner()) {
            (CallerCase::WriterUncertain, OffloadCreationOwner::Verified(verified)) => {
                assert!(matches!(
                    creation.reason(),
                    Some(OffloadCreationRefusal::Ledger)
                ));
                assert_eq!(fault.as_ref().expect("armed").observations(), (1, true));
                assert!(!register_path.try_exists()?);
                (verified, "held_ledger_uncertain")
            }
            (
                CallerCase::FirstAbsent | CallerCase::OrphanReuse,
                OffloadCreationOwner::Published(outcome),
            ) => {
                assert!(creation.reason().is_none());
                let Ok(BoundMutationOutcome::Published(published)) = outcome.publication() else {
                    return Err(invalid(format!(
                        "actual caller publication: {:?}",
                        outcome.publication()
                    ))
                    .into());
                };
                assert!(matches!(published.status, MutationStatus::Applied));
                let current = marks::load(&input.journal)?;
                assert_eq!(current.version, 2);
                assert_eq!(current, published.register);
                assert_eq!(
                    current.marks.values().cloned().collect::<Vec<_>>(),
                    outcome.verified().marks()
                );
                (outcome.verified(), "published_applied")
            }
            _ => {
                return Err(invalid(format!(
                    "actual caller unexpected owning phase: {:?}",
                    creation.reason()
                ))
                .into());
            }
        };
        assert!(Arc::ptr_eq(verified.journal_root(), &root));
        assert!(Arc::ptr_eq(verified.activity(), &activity));
        let observed = creation
            .observed()
            .ok_or_else(|| invalid("retained observed input missing"))?;
        assert!(Arc::ptr_eq(observed.journal_root(), &root));
        assert!(Arc::ptr_eq(observed.activity(), &activity));
        assert!(observed.groups()[0].expected_mark().is_none());
        assert!(creation.partial_copy().is_none());
        assert!(creation.native_error().is_none());
        let copies = creation.source_copies().collect::<Vec<_>>();
        assert_eq!(copies.len(), 2);
        let source_facts = copies
            .iter()
            .zip(&selection.names)
            .enumerate()
            .map(|(index, (copy, name))| {
                assert_eq!(copy.digest().size, raw[index]["bytes"].as_u64().unwrap());
                assert_eq!(copy.digest().sha256, raw[index]["sha256"].as_str().unwrap());
                json!({"name":name,"identity":copy.identity(),"digest":copy.digest()})
            })
            .collect::<Vec<_>>();
        assert_ne!(copies[0].identity(), copies[1].identity());
        let reads = read_facts(verified)?;
        let proof = &verified.marks()[0]
            .offload
            .as_ref()
            .ok_or_else(|| invalid("genuine caller offer missing"))?
            .proof;
        let offer = verified.marks()[0].offload.as_ref().expect("checked offer");
        offer.validate()?;
        assert!(!offer.nonce.as_str().is_empty());
        assert!(!offer.revision.as_str().is_empty());
        let requests = runner.0.borrow();
        let backups = requests
            .iter()
            .filter(|request| request["argv"][0] == "backup")
            .collect::<Vec<_>>();
        if input.case == CallerCase::OrphanReuse {
            assert!(backups.is_empty());
            assert!(creation.archives().is_empty());
            let (seed_proof, _, bytes, catalog) = seed.as_ref().expect("real retained orphan seed");
            assert_eq!(proof, seed_proof);
            assert_eq!(fs::read(&ledger_path)?, *bytes);
            assert_eq!(read_complete_catalog(&root).map_err(invalid)?, *catalog);
        } else {
            assert_eq!(backups.len(), 1);
            assert_eq!(creation.archives().len(), 1);
            let archive = creation.archives()[0]
                .as_ref()
                .map_err(|error| invalid(format!("actual retained archive: {error:?}")))?;
            assert_eq!(archive.repository_id, input.repository_id);
            assert_ne!(archive.snapshot_id, input.snapshot_id);
            assert_eq!(proof.snapshot_id, archive.snapshot_id);
            let args = backups[0]["argv"].as_array().expect("actual argv");
            assert!(args.contains(&json!("--force")));
            assert!(args.contains(&json!("--no-cache")));
            assert!(args.contains(&json!("solstone-archive")));
            assert_eq!(
                backups[0]["sources"]
                    .as_array()
                    .expect("actual sources")
                    .len(),
                2
            );
            assert_eq!(
                backups[0]["descriptors"]
                    .as_array()
                    .expect("actual descriptors")
                    .len(),
                2
            );
            assert_eq!(backups[0]["outcome"]["returncode"], 0);
        }
        let writes = creation.ledger_writes().collect::<Vec<_>>();
        assert_eq!(writes.len(), 1);
        let receipt = match writes[0] {
            OffloadLedgerWrite::Recorded {
                effect,
                durability,
                confirmation,
                failures,
            } => {
                if input.case == CallerCase::OrphanReuse {
                    assert_eq!(effect, OffloadLedgerEffect::AlreadyRecorded);
                } else {
                    assert!(matches!(effect,OffloadLedgerEffect::Appended {bytes} if bytes>0));
                }
                assert_eq!(confirmation, OffloadLedgerSync::Confirmed);
                if input.case == CallerCase::WriterUncertain {
                    assert_eq!(
                        durability,
                        [
                            OffloadLedgerSync::Uncertain,
                            OffloadLedgerSync::Confirmed,
                            OffloadLedgerSync::Confirmed,
                            OffloadLedgerSync::Confirmed
                        ]
                    );
                    assert!(!failures.is_empty());
                    assert!(failures.iter().any(|failure| {
                        failure.contains("sync File: injected owning caller FileSync uncertainty")
                    }));
                } else {
                    assert_eq!(durability, [OffloadLedgerSync::Confirmed; 4]);
                    assert!(failures.is_empty());
                }
                json!({"effect":format!("{effect:?}"),"durability":durability.map(|fact|format!("{fact:?}")),"confirmation":format!("{confirmation:?}"),"failures":failures})
            }
            OffloadLedgerWrite::Refused { reason, detail } => {
                return Err(invalid(format!("actual writer refusal {reason}: {detail:?}")).into());
            }
        };
        let catalog = read_complete_catalog(&root).map_err(invalid)?;
        assert_eq!(catalog.days.len(), 1);
        assert_eq!(catalog.days[0].segments.len(), 1);
        assert_eq!(catalog.days[0].segments[0].history.len(), 1);
        for file in &proof.files {
            assert!(catalog.active_witness(proof, file).is_some());
        }
        let ledger = fs::read(&ledger_path)?;
        carrier.revalidate()?;
        assert_eq!(fs::read_dir(carrier.diagnostic_path())?.count(), 0);
        let namespace = stages(&input.journal)?;
        assert_eq!(namespace.len(), 3);
        for stage in namespace
            .iter()
            .filter(|path| **path != carrier.diagnostic_path())
        {
            let entries = fs::read_dir(stage)?.collect::<io::Result<Vec<_>>>()?;
            assert_eq!(entries.len(), 1);
            let leaf = entries[0].path();
            let name = leaf
                .file_name()
                .ok_or_else(|| invalid("retained copy name missing"))?
                .to_string_lossy();
            let index = selection
                .names
                .iter()
                .position(|selected| *selected == name)
                .ok_or_else(|| invalid("unexpected retained copy"))?;
            let metadata = fs::metadata(&leaf)?;
            assert_eq!(
                json!(copies[index].identity()),
                json!({"platform":"unix","device":metadata.dev(),"inode":metadata.ino()})
            );
            assert_ne!(
                (metadata.dev(), metadata.ino()),
                (
                    raw[index]["device"].as_u64().unwrap(),
                    raw[index]["inode"].as_u64().unwrap()
                )
            );
            assert_eq!(file_fact(&leaf)?["sha256"], raw[index]["sha256"]);
        }
        for (index, name) in selection.names.iter().enumerate() {
            assert_eq!(file_fact(&raw_dir.join(name))?, raw[index]);
            assert_eq!(
                fs::read(raw_dir.join(name).with_extension("jsonl"))?,
                headers[index]
            );
        }
        assert_eq!(
            fs::read(input.journal.join("config/journal.json"))?,
            configuration
        );
        println!(
            "OFFER_CALLER_EVIDENCE {}",
            json!({"case":input.case,"publication":publication,
            "carrier":carrier.diagnostic_path(),"budget_seconds_before_prepare":budget_seconds,"now":now,
            "source_copies":source_facts,"reads":reads,"marks":verified.marks(),"raw":raw,
            "ledger_sha256":format!("{:x}",Sha256::digest(&ledger)),"receipt":receipt,"requests":*requests,
            "seed":seed.as_ref().map(|(proof,receipt,bytes,_)|json!({"label":"SYNTHETIC RECORDED ORPHAN; real pinned archived bytes/live capture; no prior caller completion", "proof":proof,
                "receipt":{"effect":format!("{:?}",receipt.effect()),"durability":receipt.durability().map(|fact|format!("{fact:?}")),
                    "confirmation":format!("{:?}",receipt.confirmation()),"failures":receipt.failures()},
                "ledger_sha256":format!("{:x}",Sha256::digest(bytes))})),"owners_retained":true})
        );
        io::stdout().flush()?;
        cut(&mut control, b"returned")?;
        assert_eq!(read_facts(verified)?, reads);
        assert_eq!(fs::read(&ledger_path)?, ledger);
        assert_eq!(read_complete_catalog(&root).map_err(invalid)?, catalog);
        carrier.revalidate()?;
        Ok(())
    }
}
