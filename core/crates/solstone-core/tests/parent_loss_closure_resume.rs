// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#![cfg(unix)]

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use solstone_core_system::lifecycle::{
    AdmissionFinding, AdmissionIdentity, AdmissionIntent, AdmissionResult, AdmissionResultState,
    HostedServiceKind, ParentLossGenerationRecord, ParentLossLedger,
    write_parent_loss_admission_intent, write_parent_loss_admission_result,
    write_parent_loss_admission_spawn_identity,
};
use solstone_core_system::process::{
    InspectResult, ProcessBirth, ProcessInstance, ProcessInstanceSource,
    SystemProcessInstanceSource,
};

static NEXT_JOURNAL_ID: AtomicU64 = AtomicU64::new(0);

struct TempJournal {
    root: PathBuf,
}

impl TempJournal {
    fn new(name: &str) -> Self {
        let seq = NEXT_JOURNAL_ID.fetch_add(1, Ordering::Relaxed);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("solstone-closure-resume-{name}-{seq}-{stamp}"));
        fs::create_dir_all(root.join("config")).expect("config dir");
        fs::create_dir_all(root.join("health")).expect("health dir");
        fs::write(
            root.join("config/journal.json"),
            br#"{"setup":{"completed_at":1}}"#,
        )
        .expect("journal config");
        Self { root }
    }
}

impl Drop for TempJournal {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn synth_instance(pid: u32, birth: u64) -> ProcessInstance {
    ProcessInstance {
        pid,
        birth: ProcessBirth::linux(birth, 1, 100),
    }
}

fn supervisor_command(journal: &TempJournal) -> Command {
    let core_bin = env!("CARGO_BIN_EXE_solstone-core");
    let test_child = env!("CARGO_BIN_EXE_solstone-core-system-test-child");
    let mut cmd = Command::new(core_bin);
    cmd.args(["supervisor", "--journal"])
        .arg(&journal.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env("SOLSTONE_LOCAL_BINARY", test_child)
        .env("SOLSTONE_SUPERVISOR_LOCAL_FIXTURE", "1")
        .env("SOLSTONE_SUPERVISOR_APP_FIXTURE", "1")
        .env("SOLSTONE_SUPERVISOR_APP_FIXTURE_FAST_TIMING", "1")
        .env("SOLSTONE_SUPERVISOR_APP_BINARY", test_child)
        .env("SOLSTONE_SPEAKERS_ANALYZE_BINARY", test_child)
        .env_remove("SOLSTONE_JOURNAL");
    cmd
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpectedAdmission {
    launch_id: String,
    service: Option<HostedServiceKind>,
    uid: Option<u32>,
    parent_launch_id: Option<String>,
    finding: AdmissionFinding,
}

#[test]
fn large_history_recovery_across_repeated_restarts() {
    let journal = TempJournal::new("large-history");
    let ledger = ParentLossLedger::open(&journal.root).expect("open ledger");

    let dead_sup = synth_instance(99980, 100);
    let dead_coord = synth_instance(99981, 101);

    let active = ledger
        .reserve_generation(dead_sup, [HostedServiceKind::Sense])
        .expect("reserve");
    ledger.initialize_record(&active).expect("init record");
    let active = ledger
        .persist_coordinator_identity(active.generation, dead_coord)
        .expect("persist coord");
    let active = ledger
        .mark_admitting(active.generation, dead_coord)
        .expect("mark admitting");

    let mut expected: Vec<ExpectedAdmission> = Vec::new();
    let own_uid = nix::unistd::getuid().as_raw();

    // 520 admissions to cross two 256 batches
    for i in 0..520 {
        let launch_id = format!("admit-{i:05}");
        let inst = synth_instance(10000 + i as u32, 1000 + i as u64);
        write_parent_loss_admission_intent(
            &journal.root,
            &AdmissionIntent::new(
                active.generation,
                &launch_id,
                Some(HostedServiceKind::Sense),
                None,
            ),
        )
        .expect("write intent");

        let identity = AdmissionIdentity {
            generation: active.generation,
            launch_id: launch_id.clone(),
            instance: inst,
            uid: own_uid,
            parent_launch_id: None,
        };
        write_parent_loss_admission_spawn_identity(&journal.root, &identity).expect("spawn id");
        write_parent_loss_admission_result(
            &journal.root,
            active.generation,
            &launch_id,
            &AdmissionResult {
                schema: 1,
                identity: Some(identity),
                state: AdmissionResultState::Admitted,
            },
        )
        .expect("result");

        expected.push(ExpectedAdmission {
            launch_id,
            service: Some(HostedServiceKind::Sense),
            uid: Some(own_uid),
            parent_launch_id: None,
            finding: AdmissionFinding::Exited,
        });
    }

    let ready_path = journal.root.join("health/supervisor.ready");

    // First supervisor start with slow timing (40ms per admission) so it cannot finish within 18s ready window
    let mut cmd1 = supervisor_command(&journal);
    cmd1.env("SOLSTONE_PARENT_LOSS_CLOSURE_SLOW_MS", "40");
    let mut sup1 = ChildGuard(cmd1.spawn().expect("start first"));

    let ids_dir = ledger.closure_recovery_ids_dir(active.generation);
    let wait_deadline = Instant::now() + Duration::from_secs(18);
    let mut has_ids = false;
    while Instant::now() < wait_deadline {
        if !has_ids {
            if let Ok(entries) = fs::read_dir(&ids_dir) {
                if entries
                    .flatten()
                    .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
                {
                    has_ids = true;
                }
            }
        }
        if sup1.0.try_wait().expect("try_wait sup1").is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !ready_path.exists(),
        "health/supervisor.ready must be absent"
    );
    assert!(
        has_ids,
        "at least one closure-recovery/ids/*.json file must exist"
    );

    // Only then kill the process if it is still running.
    if sup1.0.try_wait().expect("try_wait sup1").is_none() {
        let _ = sup1.0.kill();
        let _ = sup1.0.wait();
    }

    // Start a second supervisor with the slow env removed.
    let mut cmd2 = supervisor_command(&journal);
    cmd2.env_remove("SOLSTONE_PARENT_LOSS_CLOSURE_SLOW_MS");
    let mut sup2 = ChildGuard(cmd2.spawn().expect("start second"));

    let wait_second = Instant::now() + Duration::from_secs(30);
    let mut ready = false;
    while Instant::now() < wait_second {
        if ready_path.exists() {
            ready = true;
            break;
        }
        if let Ok(Some(st)) = sup2.0.try_wait() {
            panic!("second supervisor exited prematurely with {st:?}");
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(ready, "health/supervisor.ready must appear");
    assert!(
        sup2.0.try_wait().expect("try_wait").is_none(),
        "second supervisor must still be running when supervisor.ready appears"
    );

    let record_path = ledger.record_path(active.generation);
    assert!(record_path.is_file(), "predecessor record.json must exist");
    let record_bytes = fs::read(&record_path).expect("read record");
    let record: ParentLossGenerationRecord =
        serde_json::from_slice(&record_bytes).expect("parse record");
    let closure = record.closure.expect("closure must exist");

    assert_eq!(closure.admissions.len(), expected.len());
    let mut actual_ids: HashSet<String> = HashSet::new();
    for (act, exp) in closure.admissions.iter().zip(expected.iter()) {
        assert_eq!(&act.launch_id, &exp.launch_id);
        assert_eq!(act.service, exp.service);
        assert_eq!(act.uid, exp.uid);
        assert_eq!(exp.parent_launch_id, None);
        assert_eq!(act.finding, exp.finding);
        actual_ids.insert(act.launch_id.clone());
    }
    assert_eq!(actual_ids.len(), expected.len(), "no duplicate launch ids");
    for exp in &expected {
        assert!(actual_ids.contains(&exp.launch_id), "no missing launch id");
    }

    let ledger_path = ledger.sealed_ledger_path(active.generation);
    assert!(ledger_path.is_file(), "ledger.json must exist");
}

#[test]
fn kill_at_after_batch_seam_and_resume_with_new_closer() {
    let journal = TempJournal::new("kill-after-batch");
    let ledger = ParentLossLedger::open(&journal.root).expect("open ledger");

    let dead_sup = synth_instance(99970, 100);
    let dead_coord = synth_instance(99971, 101);

    let active = ledger
        .reserve_generation(dead_sup, [HostedServiceKind::Sense])
        .expect("reserve");
    ledger.initialize_record(&active).expect("init record");
    let active = ledger
        .persist_coordinator_identity(active.generation, dead_coord)
        .expect("persist coord");
    let active = ledger
        .mark_admitting(active.generation, dead_coord)
        .expect("mark admitting");

    let own_uid = nix::unistd::getuid().as_raw();

    for i in 0..300 {
        let launch_id = format!("batch-admit-{i:04}");
        let inst = synth_instance(20000 + i as u32, 2000 + i as u64);
        write_parent_loss_admission_intent(
            &journal.root,
            &AdmissionIntent::new(
                active.generation,
                &launch_id,
                Some(HostedServiceKind::Sense),
                None,
            ),
        )
        .expect("write intent");

        let identity = AdmissionIdentity {
            generation: active.generation,
            launch_id: launch_id.clone(),
            instance: inst,
            uid: own_uid,
            parent_launch_id: None,
        };
        write_parent_loss_admission_spawn_identity(&journal.root, &identity).expect("spawn id");
        write_parent_loss_admission_result(
            &journal.root,
            active.generation,
            &launch_id,
            &AdmissionResult {
                schema: 1,
                identity: Some(identity),
                state: AdmissionResultState::Admitted,
            },
        )
        .expect("result");
    }

    let ids_path = ledger.closure_recovery_ids_path(active.generation, 1);
    let ready_path = journal.root.join("health/supervisor.ready");

    let mut cmd1 = supervisor_command(&journal);
    cmd1.env("SOLSTONE_PARENT_LOSS_CLOSURE_SEAM", "after-batch");
    let mut sup1 = ChildGuard(cmd1.spawn().expect("start first"));

    let wait_ids = Instant::now() + Duration::from_secs(15);
    while Instant::now() < wait_ids {
        if ids_path.is_file() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(ids_path.is_file(), "ids/000001.json must exist");

    let _ = sup1.0.kill();
    let _ = sup1.0.wait();

    let active_cur = ledger.active_generation().expect("read").expect("active");
    assert_eq!(
        active_cur.generation, active.generation,
        "pointer still names abandoned generation"
    );
    assert!(
        !ready_path.exists(),
        "supervisor.ready absent while audit incomplete"
    );

    let mut cmd2 = supervisor_command(&journal);
    let _sup2 = ChildGuard(cmd2.spawn().expect("start second"));
    let source = SystemProcessInstanceSource;
    let sup2_instance = match source.inspect(_sup2.0.id()) {
        InspectResult::Present { instance, .. } => instance,
        other => panic!("sup2 inspectable: {other:?}"),
    };

    let wait_ready = Instant::now() + Duration::from_secs(30);
    while Instant::now() < wait_ready {
        if ready_path.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        ready_path.exists(),
        "second supervisor must finish and be ready"
    );

    let record = ledger
        .record(active.generation)
        .expect("read")
        .expect("record exists");
    let closure = record.closure.expect("closure exists");
    assert_eq!(
        closure.closed_by, sup2_instance,
        "closer must be the second supervisor"
    );
}

#[test]
fn kill_at_during_inventory_and_after_ledger_seams() {
    // 1. during-inventory seam
    {
        let journal = TempJournal::new("kill-during-inv");
        let ledger = ParentLossLedger::open(&journal.root).expect("open ledger");

        let dead_sup = synth_instance(99960, 100);
        let dead_coord = synth_instance(99961, 101);

        let active = ledger
            .reserve_generation(dead_sup, [HostedServiceKind::Sense])
            .expect("reserve");
        ledger.initialize_record(&active).expect("init record");
        let active = ledger
            .persist_coordinator_identity(active.generation, dead_coord)
            .expect("persist coord");
        let active = ledger
            .mark_admitting(active.generation, dead_coord)
            .expect("mark admitting");

        let launch_id = "inv-test-id";
        let inst = synth_instance(30001, 300);
        write_parent_loss_admission_intent(
            &journal.root,
            &AdmissionIntent::new(
                active.generation,
                launch_id,
                Some(HostedServiceKind::Sense),
                None,
            ),
        )
        .expect("intent");
        let own_uid = nix::unistd::getuid().as_raw();
        let identity = AdmissionIdentity {
            generation: active.generation,
            launch_id: launch_id.to_string(),
            instance: inst,
            uid: own_uid,
            parent_launch_id: None,
        };
        write_parent_loss_admission_spawn_identity(&journal.root, &identity).expect("spawn id");
        write_parent_loss_admission_result(
            &journal.root,
            active.generation,
            launch_id,
            &AdmissionResult {
                schema: 1,
                identity: Some(identity),
                state: AdmissionResultState::Admitted,
            },
        )
        .expect("result");

        let mut cmd_inv = supervisor_command(&journal);
        cmd_inv.env("SOLSTONE_PARENT_LOSS_CLOSURE_SEAM", "during-inventory");
        let mut sup_inv = ChildGuard(cmd_inv.spawn().expect("start during-inventory"));

        thread::sleep(Duration::from_millis(500));
        assert!(
            sup_inv.0.try_wait().expect("try_wait").is_none(),
            "process is still alive"
        );

        let _ = sup_inv.0.kill();
        let _ = sup_inv.0.wait();

        let ready_path = journal.root.join("health/supervisor.ready");
        assert!(
            !ready_path.exists(),
            "supervisor.ready absent while during-inventory"
        );

        let active_cur = ledger.active_generation().expect("read").expect("active");
        assert_eq!(
            active_cur.generation, active.generation,
            "active generation pointer is unchanged"
        );

        // Resume without the seam and require supervisor.ready
        let mut cmd_resume = supervisor_command(&journal);
        let _sup_resume = ChildGuard(cmd_resume.spawn().expect("resume"));
        let wait_resume = Instant::now() + Duration::from_secs(30);
        while Instant::now() < wait_resume {
            if ready_path.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(ready_path.exists(), "supervisor.ready required on resume");
    }

    // 2. after-ledger seam
    {
        let journal = TempJournal::new("kill-after-ledger");
        let ledger = ParentLossLedger::open(&journal.root).expect("open ledger");

        let dead_sup = synth_instance(99962, 100);
        let dead_coord = synth_instance(99963, 101);

        let active = ledger
            .reserve_generation(dead_sup, [HostedServiceKind::Sense])
            .expect("reserve");
        ledger.initialize_record(&active).expect("init record");
        let active = ledger
            .persist_coordinator_identity(active.generation, dead_coord)
            .expect("persist coord");
        let active = ledger
            .mark_admitting(active.generation, dead_coord)
            .expect("mark admitting");

        let launch_id = "after-ledger-test-id";
        let inst = synth_instance(30002, 301);
        write_parent_loss_admission_intent(
            &journal.root,
            &AdmissionIntent::new(
                active.generation,
                launch_id,
                Some(HostedServiceKind::Sense),
                None,
            ),
        )
        .expect("intent");
        let own_uid = nix::unistd::getuid().as_raw();
        let identity = AdmissionIdentity {
            generation: active.generation,
            launch_id: launch_id.to_string(),
            instance: inst,
            uid: own_uid,
            parent_launch_id: None,
        };
        write_parent_loss_admission_spawn_identity(&journal.root, &identity).expect("spawn id");
        write_parent_loss_admission_result(
            &journal.root,
            active.generation,
            launch_id,
            &AdmissionResult {
                schema: 1,
                identity: Some(identity),
                state: AdmissionResultState::Admitted,
            },
        )
        .expect("result");

        let mut cmd_ledger = supervisor_command(&journal);
        cmd_ledger.env("SOLSTONE_PARENT_LOSS_CLOSURE_SEAM", "after-ledger");
        let mut sup_ledger = ChildGuard(cmd_ledger.spawn().expect("start after-ledger"));

        let ledger_path = ledger.sealed_ledger_path(active.generation);
        let wait_ledger = Instant::now() + Duration::from_secs(15);
        while Instant::now() < wait_ledger {
            if ledger_path.is_file() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            ledger_path.is_file(),
            "ledger.json exists before record write"
        );

        let _ = sup_ledger.0.kill();
        let _ = sup_ledger.0.wait();

        let record_path = ledger.record_path(active.generation);
        let record_before = ledger
            .record(active.generation)
            .expect("read")
            .expect("exists");
        assert!(
            record_before.closure.is_none(),
            "predecessor closure is still absent"
        );

        let ledger_bytes_before = fs::read(&ledger_path).expect("read before");

        // Next supervisor start finishes closure and reuses existing ledger
        let mut cmd_finish = supervisor_command(&journal);
        let _sup_finish = ChildGuard(cmd_finish.spawn().expect("start finish"));

        let ready_path = journal.root.join("health/supervisor.ready");
        let wait_finish = Instant::now() + Duration::from_secs(30);
        while Instant::now() < wait_finish {
            if ready_path.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(ready_path.exists(), "supervisor finished");

        let record_after = ledger
            .record(active.generation)
            .expect("read")
            .expect("exists");
        assert!(record_after.closure.is_some(), "predecessor closure exists");
        assert!(record_path.is_file());
        assert!(ledger_path.is_file(), "the same ledger file is still there");
        let ledger_bytes_after = fs::read(&ledger_path).expect("read after");
        assert_eq!(
            ledger_bytes_before, ledger_bytes_after,
            "the same ledger file contents are preserved"
        );
    }
}

#[test]
fn real_coordinator_shutdown_on_multi_admission_history_recovered_through_abandoned_closure() {
    let journal = TempJournal::new("coord-shutdown");
    let ledger = ParentLossLedger::open(&journal.root).expect("open ledger");

    let ready_path = journal.root.join("health/supervisor.ready");
    let mut cmd1 = supervisor_command(&journal);
    let mut sup1 = ChildGuard(cmd1.spawn().expect("start live supervisor"));

    let wait_ready = Instant::now() + Duration::from_secs(30);
    while Instant::now() < wait_ready {
        if ready_path.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        ready_path.exists(),
        "first supervisor reached supervisor.ready"
    );

    let active_gen = ledger
        .active_generation()
        .expect("read active")
        .expect("active generation exists");

    // SIGKILL it
    let _ = sup1.0.kill();
    let _ = sup1.0.wait();

    let record_before = ledger
        .record(active_gen.generation)
        .expect("read record")
        .expect("record exists");
    assert!(
        record_before.closure.is_none(),
        "active generation has no closure yet"
    );

    let _ = fs::remove_file(&ready_path);

    // Start a second supervisor
    let mut cmd2 = supervisor_command(&journal);
    let _sup2 = ChildGuard(cmd2.spawn().expect("start second supervisor"));

    let wait_second = Instant::now() + Duration::from_secs(30);
    while Instant::now() < wait_second {
        if ready_path.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        ready_path.exists(),
        "second supervisor reached supervisor.ready"
    );

    let record_after = ledger
        .record(active_gen.generation)
        .expect("read record")
        .expect("record exists");
    assert!(
        record_after.closure.is_some(),
        "predecessor record.json closure must exist"
    );

    let admissions_dir = ledger
        .generation_path(active_gen.generation)
        .join("admissions");
    assert!(
        admissions_dir.is_dir(),
        "admission drop files must still be on disk"
    );
    let admission_entries: Vec<_> = fs::read_dir(&admissions_dir)
        .expect("read admissions")
        .flatten()
        .collect();
    assert!(
        !admission_entries.is_empty(),
        "admission drop files must still be on disk"
    );
}
