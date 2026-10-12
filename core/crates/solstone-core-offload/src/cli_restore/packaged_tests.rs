// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Ignored installed-command control: the copied package's own CLI restores one
//! bound and one historical segment in a single read with its own restic.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solstone_core_journal_io::paths::SegmentLayout;
use solstone_core_retention::archive_catalog::{OffloadFile, codec, read_complete_catalog};
use solstone_core_retention::archive_proof::{
    AcceptedProcessingObservation, ArchiveFileProof, OffloadProof,
};
use solstone_core_retention::{Mark, MarkId, MarkState, Proposal, Register, RemovalClass, Target};

use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    mode: String,
    journal: PathBuf,
    cli: PathBuf,
    repository_id: String,
    snapshot_id: String,
    bound: Fixture,
    historical: Fixture,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    name: String,
    bytes: u64,
    sha256: String,
    archive_path: String,
}

const DAY: &str = "20261009";
const BOUND_DIR: &str = "120000_60_fixture";
const HISTORICAL_DIR: &str = "130000_60_fixture";

fn line(event: &impl serde::Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(event).unwrap();
    bytes.push(b'\n');
    bytes
}

fn seed(input: &Input, root: &Arc<JournalRoot>) {
    let target = |dir: &str| Target {
        day: DAY.into(),
        stream: "_default".into(),
        dir: dir.into(),
    };
    let proof = OffloadProof {
        journal_id: root.identity().observation_id(),
        manifest_id: "cli-restore-manifest".into(),
        target: target(BOUND_DIR),
        layout: SegmentLayout::Direct,
        repository_id: input.repository_id.clone(),
        snapshot_id: input.snapshot_id.clone(),
        files: vec![ArchiveFileProof {
            witness_id: "cli-restore-witness".into(),
            name: input.bound.name.clone(),
            bytes: input.bound.bytes,
            sha256: input.bound.sha256.clone(),
            archive_path: input.bound.archive_path.clone(),
            processing: AcceptedProcessingObservation {
                record: None,
                has_analysis_row: true,
            },
        }],
    };
    proof.validate().unwrap();
    for dir in [BOUND_DIR, HISTORICAL_DIR] {
        fs::create_dir_all(input.journal.join("chronicle").join(DAY).join(dir)).unwrap();
    }
    let ledger = input.journal.join("health/offload");
    fs::create_dir_all(&ledger).unwrap();
    let mut bytes = line(&codec::Event::ArchiveV2 {
        schema_version: 2,
        time: 1,
        day: DAY.into(),
        stream: "_default".into(),
        segment: "120000_60".into(),
        segment_dir: BOUND_DIR.into(),
        layout: proof.layout,
        journal_id: proof.journal_id,
        manifest_id: proof.manifest_id,
        repository_id: proof.repository_id,
        snapshot_id: proof.snapshot_id,
        files: proof.files,
    });
    let historical = OffloadFile {
        name: input.historical.name.clone(),
        bytes: input.historical.bytes,
        sha256: input.historical.sha256.clone(),
    };
    bytes.extend(line(&codec::Event::Archive {
        time: 1,
        day: DAY.into(),
        stream: "_default".into(),
        segment: solstone_core_format::segment::segment_key(HISTORICAL_DIR).unwrap(),
        snapshot_id: input.snapshot_id[..8].to_owned(),
        files: vec![historical.clone()],
    }));
    fs::write(ledger.join(format!("{DAY}.jsonl")), bytes).unwrap();
    // The historical proposal an earlier version left on Home stays as it was.
    let names = vec![historical.name];
    let mark = Mark {
        id: MarkId::derive(
            RemovalClass::OffloadRawRelease,
            &target(HISTORICAL_DIR),
            &names,
        ),
        class: RemovalClass::OffloadRawRelease,
        target: target(HISTORICAL_DIR),
        marked_at: "2026-10-10T00:00:00Z".into(),
        proposal: Proposal {
            bytes: input.historical.bytes,
            reason: "synthetic original legacy mark".into(),
            names,
        },
        state: MarkState::Marked,
        offload: None,
    };
    let mut register = Register::empty();
    register.marks.insert(mark.id.clone(), mark);
    fs::write(
        input.journal.join("health/retention-marks.json"),
        serde_json::to_vec(&register).unwrap(),
    )
    .unwrap();
    let catalog = read_complete_catalog(root).unwrap();
    assert_eq!(catalog.days[0].segments.len(), 2);
}

fn file(path: &Path) -> Value {
    match fs::read(path) {
        Ok(bytes) => json!({"bytes":bytes.len(), "sha256":format!("{:x}", Sha256::digest(&bytes))}),
        Err(_) => Value::Null,
    }
}

fn residue(journal: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if name.starts_with(".tmp-offload-")
                || [
                    "restore-download",
                    "readback-download",
                    "removal-original",
                    "removal-record",
                ]
                .contains(&name.as_str())
            {
                out.push(path.strip_prefix(root).unwrap().display().to_string());
            }
            if path.is_dir() && !path.is_symlink() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(journal, journal, &mut out);
    out
}

#[test]
#[ignore = "requires a copied package carrying this tree's CLI and a synthetic S3 fixture"]
fn packaged_cli_restore_entry() {
    let input: Input = serde_json::from_slice(
        &fs::read(std::env::var_os("OFFLOAD_CLI_RESTORE_JSON").unwrap()).unwrap(),
    )
    .unwrap();
    let root = Arc::new(JournalRoot::open(&input.journal).unwrap());
    seed(&input, &root);
    let bound = input.journal.join("chronicle").join(DAY).join(BOUND_DIR);
    let historical = input
        .journal
        .join("chronicle")
        .join(DAY)
        .join(HISTORICAL_DIR);
    let mut held = None;
    let mut selection = vec![DAY.to_owned()];
    match input.mode.as_str() {
        "cli_day" => {}
        "cli_all" => selection = vec!["--all".into()],
        "cli_conflict" => fs::write(bound.join(&input.bound.name), b"different raw").unwrap(),
        "cli_busy" => held = Some(ActivityLease::try_acquire(Arc::clone(&root)).unwrap()),
        other => panic!("unknown CLI restore mode {other}"),
    }
    let marks = fs::read(input.journal.join("health/retention-marks.json")).unwrap();
    let output = Command::new(&input.cli)
        .args(["backup", "offload", "restore"])
        .args(&selection)
        .arg("--json")
        .env_clear()
        .env("HOME", input.journal.parent().unwrap())
        .env("SOLSTONE_JOURNAL", &input.journal)
        .output()
        .unwrap();
    drop(held);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let report: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("CLI JSON expected\nstdout={stdout}\nstderr={stderr}"));
    let after = json!({
        "bound": file(&bound.join(&input.bound.name)),
        "historical": file(&historical.join(&input.historical.name)),
    });
    let left = residue(&input.journal);
    println!(
        "CLI_RESTORE_RESULT {}",
        json!({"mode":input.mode, "exit":output.status.code(), "report":report,
            "stderr":stderr, "after":after, "residue":left})
    );
    assert!(left.is_empty(), "{left:?}");
    assert_eq!(
        fs::read(input.journal.join("health/retention-marks.json")).unwrap(),
        marks
    );
    // No owner-facing sentence names a code.
    assert!(!stderr.contains('_'), "{stderr}");
    let restored = |fixture: &Fixture| json!({"bytes":fixture.bytes, "sha256":fixture.sha256});
    match input.mode.as_str() {
        "cli_day" | "cli_all" => {
            assert_eq!(output.status.code(), Some(0), "{stdout}{stderr}");
            assert_eq!(report["status"], "ok");
            assert_eq!(report["settlement"], "settled");
            assert_eq!(report["runtime"], "completed");
            assert_eq!(report["files_restored"], 2);
            assert_eq!(after["bound"], restored(&input.bound));
            assert_eq!(after["historical"], restored(&input.historical));
            assert!(stderr.is_empty());
            let catalog = read_complete_catalog(&root).unwrap();
            assert!(
                catalog.days[0]
                    .segments
                    .iter()
                    .all(|segment| segment.active.is_empty())
            );
            // A second run finds nothing left to restore.
            let again = Command::new(&input.cli)
                .args(["backup", "offload", "restore", "--all", "--json"])
                .env_clear()
                .env("HOME", input.journal.parent().unwrap())
                .env("SOLSTONE_JOURNAL", &input.journal)
                .output()
                .unwrap();
            let again: Value = serde_json::from_slice(&again.stdout).unwrap();
            assert_eq!(again["status"], "no_op");
            assert_eq!(again["reason"], "nothing_to_restore");
        }
        "cli_conflict" => {
            assert_eq!(output.status.code(), Some(1), "{stdout}{stderr}");
            assert_ne!(report["status"], "ok");
            assert!(matches!(
                report["settlement"].as_str(),
                Some("settled" | "refused")
            ));
            assert_eq!(report["runtime"], "completed");
            assert_eq!(
                fs::read(bound.join(&input.bound.name)).unwrap(),
                b"different raw"
            );
        }
        "cli_busy" => {
            assert_eq!(output.status.code(), Some(1), "{stdout}{stderr}");
            assert_eq!(report["status"], "refused");
            assert_eq!(report["settlement"], "not_started");
            assert!(report["runtime"].is_null());
            assert_eq!(after["bound"], Value::Null);
            assert_eq!(after["historical"], Value::Null);
        }
        _ => unreachable!(),
    }
}
