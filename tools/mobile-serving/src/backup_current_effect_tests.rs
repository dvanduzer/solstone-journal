// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Host caller-composition controls, not native-worker or remote-storage proof.

use super::*;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

struct CurrentRunner {
    program: PathBuf,
    failed_operation: Option<&'static str>,
    commands: Mutex<Vec<Vec<String>>>,
}

impl ToolRunner for CurrentRunner {
    fn run(&self, _: &ToolRequest<'_>) -> std::io::Result<ToolOutput> {
        panic!("the embedded wrapper must forward cancellable execution")
    }

    fn run_cancellable(
        &self,
        request: &ToolRequest<'_>,
        cancellation: &ArchiveCancellation,
    ) -> std::io::Result<ToolOutput> {
        assert!(!cancellation.is_cancelled());
        let program = Path::new(&request.program);
        assert!(
            program == self.program
                || program
                    == self
                        .program
                        .parent()
                        .unwrap()
                        .parent()
                        .unwrap()
                        .join("solstone-rclone/rclone")
        );
        let argv: Vec<String> = request
            .argv
            .iter()
            .map(|v| v.to_string_lossy().into_owned())
            .collect();
        let operation = ["version", "unlock", "backup", "check", "forget"]
            .into_iter()
            .find(|name| argv.iter().any(|v| v == name))
            .expect("only expected resolution or work commands are admitted");
        if program.file_name().unwrap() == "rclone" {
            assert_eq!(operation, "version");
        }
        self.commands.lock().unwrap().push(argv);
        let stdout = if operation == "version" {
            if program.file_name().unwrap() == "rclone" {
                b"rclone v0.19.0\n".to_vec()
            } else {
                b"restic 0.19.0\n".to_vec()
            }
        } else if operation == "backup" {
            b"{\"message_type\":\"summary\",\"snapshot_id\":\"current-embedded-snapshot\"}\n"
                .to_vec()
        } else {
            Vec::new()
        };
        Ok(ToolOutput {
            returncode: i32::from(self.failed_operation == Some(operation)),
            stdout,
            stderr: Vec::new(),
        })
    }
}

struct CurrentHttp {
    operated: bool,
    fail: bool,
    calls: AtomicU64,
}

impl HttpTransport for CurrentHttp {
    fn execute(&self, _: &HttpRequest) -> Result<HttpResponse, HttpError> {
        panic!("the embedded wrapper must forward cancellable HTTP")
    }

    fn execute_cancellable(
        &self,
        request: &HttpRequest,
        cancellation: &ArchiveCancellation,
    ) -> Result<HttpResponse, HttpError> {
        assert!(self.operated, "BYO must not request broker credentials");
        assert!(!cancellation.is_cancelled());
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.url,
            "https://broker.example.invalid/backup/credentials"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap(),
            json!({"scope":"operated"})
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(HttpResponse {
            status: if self.fail { 503 } else { 200 },
            headers: Vec::new(),
            body: serde_json::to_vec(&json!({
                "access_key_id":"access", "secret_access_key":"secret", "session_token":"token",
                "endpoint":"https://example.invalid", "expires_at":"2099-01-01T00:00:00Z"
            }))
            .unwrap(),
        })
    }
}

fn write(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn package(root: &Path) -> PathBuf {
    use solstone_core_installed_payload::{
        COMPILED_VERSION, INSTALLED_PAYLOAD_MANIFEST, PRODUCT, compiled_target,
        render_installed_payload,
    };
    write(root, "bin/solstone", b"launcher");
    write(root, "lib/solstone-restic/restic", b"restic");
    write(root, "lib/solstone-rclone/rclone", b"rclone");
    let manifest = render_installed_payload(
        root,
        PRODUCT,
        COMPILED_VERSION,
        compiled_target(),
        "fixture-commit",
    )
    .unwrap();
    write(root, INSTALLED_PAYLOAD_MANIFEST, &manifest);
    root.join("bin/solstone")
}

fn configured(journal: &Path, operated: bool) {
    if operated {
        solstone_core_backup::set_mode(journal, "operated").unwrap();
        solstone_core_backup::save_hosted_binding(
            journal,
            &solstone_core_backup::HostedBinding {
                broker_endpoint: "https://broker.example.invalid".into(),
                account_id: "account".into(),
                instance_id: "instance".into(),
                bucket: "bucket".into(),
                prefix: "prefix".into(),
                broker_token: "token".into(),
            },
        )
        .unwrap();
    } else {
        solstone_core_backup::set_destination(
            journal,
            &solstone_core_backup::Destination {
                repository: "s3:bucket/prefix".into(),
                backend: "s3".into(),
                credentials: json!({"access_key_id":"access","secret_access_key":"secret"})
                    .as_object()
                    .unwrap()
                    .clone(),
            },
        )
        .unwrap();
    }
    solstone_core_backup::generate_and_store_keys(journal).unwrap();
    solstone_core_backup::set_enabled(journal, true).unwrap();
    solstone_core_backup::record_backup_result(
        journal,
        "ok",
        json!(1),
        json!("old-snapshot"),
        Value::Null,
        None,
        None,
    )
    .unwrap();
    solstone_core_backup::record_prune_result(journal, "ok", json!(1), Value::Null).unwrap();
    solstone_core_backup::record_verification_result(
        journal,
        "ok",
        json!(1),
        Value::Null,
        json!("old-subset"),
    )
    .unwrap();
}

fn current_effect(routine: EmbeddedBackupRoutine, operated: bool, fail: bool) {
    let directory = tempfile::tempdir().unwrap();
    let payload = tempfile::tempdir().unwrap();
    configured(directory.path(), operated);
    let root = JournalRoot::open(directory.path()).unwrap();
    let operation = match routine {
        EmbeddedBackupRoutine::Run => "backup",
        EmbeddedBackupRoutine::Verify => "check",
        EmbeddedBackupRoutine::Prune => "forget",
    };
    let runner = Arc::new(CurrentRunner {
        program: payload.path().join("lib/solstone-restic/restic"),
        failed_operation: if fail && !operated {
            Some(operation)
        } else {
            None
        },
        commands: Mutex::new(Vec::new()),
    });
    let http = Arc::new(CurrentHttp {
        operated,
        fail: fail && operated,
        calls: AtomicU64::new(0),
    });
    let mut deps = BackupWebDeps::new(root.canonical_path().to_owned());
    deps.executable = package(payload.path());
    deps.runner = runner.clone();
    deps.http = http.clone();
    let checkpoints = Arc::new(AtomicU64::new(0));
    let counted = checkpoints.clone();
    let task: Arc<dyn Fn() -> Result<(), String> + Send + Sync> = Arc::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    let reference = format!("current-{operation}-{operated}-{fail}");
    let outcome = settle_embedded_backup(&root, routine, &deps, &reference, &task);
    assert_eq!(
        outcome,
        BackupRequestOutcome::Settled(BackupSettlement {
            reference,
            routine,
            disposition: if fail {
                BackupDisposition::Failure
            } else {
                BackupDisposition::Success
            },
            exit_code: i32::from(fail),
        })
    );
    assert!(checkpoints.load(Ordering::SeqCst) > 0);
    assert_eq!(http.calls.load(Ordering::SeqCst), u64::from(operated));
    let commands = runner.commands.lock().unwrap();
    let work: Vec<_> = commands
        .iter()
        .filter(|argv| argv.iter().any(|v| v == operation))
        .collect();
    assert_eq!(work.len(), usize::from(!(operated && fail)));
    let saved = solstone_core_backup::get_backup_config(directory.path()).unwrap();
    let record = &saved[match routine {
        EmbeddedBackupRoutine::Run => "last_backup",
        EmbeddedBackupRoutine::Verify => "last_verification",
        EmbeddedBackupRoutine::Prune => "last_prune",
    }];
    assert_eq!(record["status"], if fail { "error" } else { "ok" });
    assert!(
        record["time"].as_i64().unwrap() > 1,
        "current operation must replace the old result"
    );
    match routine {
        EmbeddedBackupRoutine::Run => {
            assert_eq!(
                record["snapshot_id"],
                if fail {
                    Value::Null
                } else {
                    json!("current-embedded-snapshot")
                }
            );
            assert_eq!(
                record["error_reason"],
                if fail {
                    json!(if operated { "broker_error" } else { "failed" })
                } else {
                    Value::Null
                }
            );
        }
        EmbeddedBackupRoutine::Verify => {
            let argv = work[0];
            let i = argv.iter().position(|v| v == "--read-data-subset").unwrap();
            assert_ne!(argv[i + 1], "old-subset");
            assert_eq!(
                record["checked_subset"],
                if fail {
                    Value::Null
                } else {
                    json!(argv[i + 1])
                }
            );
            assert_eq!(
                record["reason"],
                if fail {
                    json!("integrity_failed")
                } else {
                    Value::Null
                }
            );
            assert_eq!(
                record["last_ok_time"],
                if fail {
                    json!(1)
                } else {
                    record["time"].clone()
                }
            );
        }
        EmbeddedBackupRoutine::Prune => {
            assert!(work[0].iter().any(|v| v == "--prune"));
            assert_eq!(
                record["error_reason"],
                if fail { json!("failed") } else { Value::Null }
            );
        }
    }
}

#[test]
fn enabled_embedded_backup_persists_current_success_through_the_real_wrappers() {
    for routine in [
        EmbeddedBackupRoutine::Run,
        EmbeddedBackupRoutine::Verify,
        EmbeddedBackupRoutine::Prune,
    ] {
        current_effect(routine, false, false);
    }
    current_effect(EmbeddedBackupRoutine::Run, true, false);
}

#[test]
fn enabled_embedded_backup_current_failures_cannot_reuse_saved_success() {
    for routine in [
        EmbeddedBackupRoutine::Run,
        EmbeddedBackupRoutine::Verify,
        EmbeddedBackupRoutine::Prune,
    ] {
        current_effect(routine, false, true);
    }
    current_effect(EmbeddedBackupRoutine::Run, true, true);
}
