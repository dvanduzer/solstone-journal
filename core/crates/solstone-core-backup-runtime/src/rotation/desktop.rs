// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Recovery-key rotation. Deliberately non-transactional to match the owner.

use std::path::Path;
use std::time::Duration;

use solstone_core_backup::{
    format_recovery_key_display, generate_recovery_key, get_destination, get_keys,
    set_recovery_key, set_recovery_key_confirmed,
};

use crate::destination::validate_destination;
use crate::engine::BackupServices;
use crate::repo::{add_recovery_key, capture_current_key_id, remove_key};

use super::{ROTATION_TIMEOUT_SECONDS, RotationResult, error, map_error};

/// Rotate the configured recovery key. No remote or local rollback is performed.
pub(super) fn rotate_recovery_key(journal: &Path, services: &BackupServices<'_>) -> RotationResult {
    // Imported Android transactions must retain their original key material.
    if solstone_core_backup::get_backup_config(journal)
        .is_ok_and(|backup| backup.contains_key("pending_key_revocation"))
    {
        return error("failed");
    }
    let (Some(destination), Some(keys)) = (
        get_destination(journal).ok().flatten(),
        get_keys(journal).ok().flatten(),
    ) else {
        return RotationResult {
            status: "skipped".into(),
            reason_code: None,
            recovery_key: None,
            recovery_key_display: None,
        };
    };
    let restic_path = match services.restic_path() {
        Ok(path) => path,
        Err(reason) => return error(reason),
    };
    let timeout = Some(Duration::from_secs(ROTATION_TIMEOUT_SECONDS));
    let old_id = match capture_current_key_id(
        services.runner,
        &destination,
        &keys.recovery_key,
        restic_path,
        timeout,
    ) {
        Ok(id) => id,
        Err(error) => return map_error(error),
    };
    let candidate = match generate_recovery_key() {
        Ok(key) => key,
        Err(_) => return error("failed"),
    };
    if let Err(error) = add_recovery_key(
        services.runner,
        &destination,
        &keys.daily_key,
        &candidate,
        restic_path,
        timeout,
    ) {
        return map_error(error);
    }
    let verified = match validate_destination(
        services.runner,
        &destination,
        &candidate,
        restic_path,
        timeout,
    ) {
        Ok(status) => status,
        Err(_) => return error("failed"),
    };
    if !(verified.repo_exists && verified.reason_code == "repo_exists") {
        return error(verified.reason_code);
    }
    if let Err(error) = remove_key(
        services.runner,
        &destination,
        &keys.daily_key,
        &old_id,
        restic_path,
        timeout,
    ) {
        return map_error(error);
    }
    if set_recovery_key(journal, &candidate).is_err() {
        return error("failed");
    }
    if set_recovery_key_confirmed(journal, false).is_err() {
        return error("failed");
    }
    let display = match format_recovery_key_display(&candidate) {
        Ok(display) => display,
        Err(_) => return error("failed"),
    };
    RotationResult {
        status: "ok".into(),
        reason_code: None,
        recovery_key: Some(candidate),
        recovery_key_display: Some(display),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Clock, JournalMaintenance};
    use crate::hosted_runtime::{HttpError, HttpRequest, HttpResponse, HttpTransport};
    use crate::rotation::rotate_recovery_key;
    use crate::runner::{ToolOutput, ToolRequest, ToolRunner};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    struct Script {
        outputs: RefCell<VecDeque<ToolOutput>>,
        commands: RefCell<Vec<Vec<String>>>,
    }
    impl ToolRunner for Script {
        fn run(&self, request: &ToolRequest<'_>) -> io::Result<ToolOutput> {
            self.commands.borrow_mut().push(
                request
                    .argv
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect(),
            );
            Ok(self.outputs.borrow_mut().pop_front().expect("output"))
        }
    }
    struct Http;
    impl HttpTransport for Http {
        fn execute(&self, _: &HttpRequest) -> Result<HttpResponse, HttpError> {
            panic!("BYO rotation does not use broker")
        }
    }
    struct TestClock;
    impl Clock for TestClock {
        fn now_unix(&self) -> i64 {
            0
        }
        fn iso_week(&self) -> u8 {
            1
        }
    }
    struct Maintenance;
    impl JournalMaintenance for Maintenance {
        fn rebuild_body_history(
            &self,
            _: &Path,
        ) -> Result<(), crate::engine::JournalMaintenanceError> {
            Ok(())
        }
        fn full_scan(&self, _: &Path) -> Result<(), crate::engine::JournalMaintenanceError> {
            Ok(())
        }
    }
    fn output(code: i32, stdout: &str) -> ToolOutput {
        ToolOutput {
            returncode: code,
            stdout: stdout.as_bytes().to_vec(),
            stderr: vec![],
        }
    }
    fn journal() -> tempfile::TempDir {
        let journal = tempfile::tempdir().unwrap();
        let destination = solstone_core_backup::Destination {
            repository: "repo".into(),
            backend: "s3".into(),
            credentials: serde_json::json!({"access_key_id":"access","secret_access_key":"secret"})
                .as_object()
                .unwrap()
                .clone(),
        };
        solstone_core_backup::set_destination(journal.path(), &destination).unwrap();
        solstone_core_backup::generate_and_store_keys(journal.path()).unwrap();
        journal
    }
    fn services<'a>(
        runner: &'a Script,
        http: &'a Http,
        clock: &'a TestClock,
        maintenance: &'a Maintenance,
    ) -> BackupServices<'a> {
        BackupServices {
            runner,
            http,
            clock,
            restic_path: Some(Path::new("/fixture/bin/restic")),
            rclone_path: None,
            version: "test",
            journal_maintenance: maintenance,
        }
    }
    #[test]
    fn debug_redacts_generated_recovery_key() {
        let rendered = format!(
            "{:?}",
            RotationResult {
                status: "ok".into(),
                reason_code: None,
                recovery_key: Some("RECOVERY_SECRET".into()),
                recovery_key_display: Some("DISPLAY_SECRET".into())
            }
        );
        assert!(!rendered.contains("RECOVERY_SECRET"));
        assert!(!rendered.contains("DISPLAY_SECRET"));
    }
    #[test]
    fn rotation_orders_remote_steps_then_publishes_new_key() {
        let journal = journal();
        let prior = solstone_core_backup::get_keys(journal.path())
            .unwrap()
            .unwrap();
        let runner = Script {
            outputs: RefCell::new(VecDeque::from([
                output(0, "[{\"id\":\"old\",\"current\":true}]"),
                output(0, ""),
                output(0, ""),
                output(0, ""),
            ])),
            commands: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock;
        let maintenance = Maintenance;
        let result = rotate_recovery_key(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
        );
        assert_eq!(result.status, "ok");
        let keys = solstone_core_backup::get_keys(journal.path())
            .unwrap()
            .unwrap();
        assert_ne!(keys.recovery_key, prior.recovery_key);
        assert_eq!(
            solstone_core_backup::get_backup_config(journal.path()).unwrap()["confirmed_recovery_key"],
            false
        );
        let commands = runner.commands.borrow();
        assert_eq!(
            commands
                .iter()
                .map(|args| args[..2.min(args.len())].join(" "))
                .collect::<Vec<_>>(),
            vec!["key list", "key add", "cat config", "key remove"]
        );
    }
    #[test]
    fn unconfigured_rotation_skips_without_process_work() {
        let journal = tempfile::tempdir().unwrap();
        let runner = Script {
            outputs: RefCell::new(VecDeque::new()),
            commands: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock;
        let maintenance = Maintenance;
        let mut services = services(&runner, &http, &clock, &maintenance);
        services.restic_path = None;
        assert_eq!(
            rotate_recovery_key(journal.path(), &services).status,
            "skipped"
        );
        assert!(runner.commands.borrow().is_empty());
    }
    #[test]
    fn absent_restic_path_fails_only_after_configuration_check() {
        let journal = journal();
        let runner = Script {
            outputs: RefCell::new(VecDeque::new()),
            commands: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock;
        let maintenance = Maintenance;
        let mut services = services(&runner, &http, &clock, &maintenance);
        services.restic_path = None;

        let result = rotate_recovery_key(journal.path(), &services);

        assert_eq!(result.status, "error");
        assert_eq!(result.reason_code.as_deref(), Some("restic_unavailable"));
        assert!(runner.commands.borrow().is_empty());
    }
    #[test]
    fn capture_failure_leaves_local_config_unchanged() {
        let journal = journal();
        let before = std::fs::read(journal.path().join("config/journal.json")).unwrap();
        let runner = Script {
            outputs: RefCell::new(VecDeque::from([output(12, "")])),
            commands: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock;
        let maintenance = Maintenance;

        assert_eq!(
            rotate_recovery_key(
                journal.path(),
                &services(&runner, &http, &clock, &maintenance)
            )
            .status,
            "error"
        );
        assert_eq!(runner.commands.borrow().len(), 1);
        assert_eq!(
            std::fs::read(journal.path().join("config/journal.json")).unwrap(),
            before
        );
    }
    #[test]
    fn add_failure_leaves_local_config_unchanged() {
        let journal = journal();
        let before = std::fs::read(journal.path().join("config/journal.json")).unwrap();
        let runner = Script {
            outputs: RefCell::new(VecDeque::from([
                output(0, "[{\"id\":\"old\",\"current\":true}]"),
                output(12, ""),
            ])),
            commands: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock;
        let maintenance = Maintenance;

        assert_eq!(
            rotate_recovery_key(
                journal.path(),
                &services(&runner, &http, &clock, &maintenance)
            )
            .status,
            "error"
        );
        assert_eq!(runner.commands.borrow().len(), 2);
        assert_eq!(
            std::fs::read(journal.path().join("config/journal.json")).unwrap(),
            before
        );
    }
    #[test]
    fn verify_failure_keeps_candidate_remote_and_local_config_unchanged() {
        let journal = journal();
        let before = std::fs::read(journal.path().join("config/journal.json")).unwrap();
        let runner = Script {
            outputs: RefCell::new(VecDeque::from([
                output(0, "[{\"id\":\"old\",\"current\":true}]"),
                output(0, ""),
                output(10, ""),
            ])),
            commands: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock;
        let maintenance = Maintenance;

        assert_eq!(
            rotate_recovery_key(
                journal.path(),
                &services(&runner, &http, &clock, &maintenance)
            )
            .status,
            "error"
        );
        assert_eq!(runner.commands.borrow().len(), 3);
        assert_eq!(
            std::fs::read(journal.path().join("config/journal.json")).unwrap(),
            before
        );
    }
    #[test]
    fn remove_failure_keeps_both_remote_keys_and_local_config_unchanged() {
        let journal = journal();
        let before = std::fs::read(journal.path().join("config/journal.json")).unwrap();
        let runner = Script {
            outputs: RefCell::new(VecDeque::from([
                output(0, "[{\"id\":\"old\",\"current\":true}]"),
                output(0, ""),
                output(0, ""),
                output(12, ""),
            ])),
            commands: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock;
        let maintenance = Maintenance;

        assert_eq!(
            rotate_recovery_key(
                journal.path(),
                &services(&runner, &http, &clock, &maintenance)
            )
            .status,
            "error"
        );
        assert_eq!(runner.commands.borrow().len(), 4);
        assert_eq!(
            std::fs::read(journal.path().join("config/journal.json")).unwrap(),
            before
        );
    }
    #[cfg(unix)]
    #[test]
    fn local_key_publication_failure_keeps_remote_change_without_rollback() {
        let journal = journal();
        let config_dir = journal.path().join("config");
        let before = std::fs::read(config_dir.join("journal.json")).unwrap();
        std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let runner = Script {
            outputs: RefCell::new(VecDeque::from([
                output(0, "[{\"id\":\"old\",\"current\":true}]"),
                output(0, ""),
                output(0, ""),
                output(0, ""),
            ])),
            commands: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock;
        let maintenance = Maintenance;
        let result = rotate_recovery_key(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
        );
        std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(result.status, "error");
        // The retained reference does not roll back after old-key removal: the
        // candidate remains remote while local recovery material stays pre-rotation.
        assert_eq!(
            runner
                .commands
                .borrow()
                .iter()
                .map(|args| args[0].as_str())
                .collect::<Vec<_>>(),
            vec!["key", "key", "cat", "key"]
        );
        assert_eq!(
            std::fs::read(config_dir.join("journal.json")).unwrap(),
            before
        );
    }
    #[cfg(feature = "full-tests")]
    #[test]
    fn public_desktop_rotation_preserves_every_present_pending_shape() {
        for pending in [
            serde_json::Value::Null,
            serde_json::json!(false),
            serde_json::json!("unresolved"),
            serde_json::json!({}),
            serde_json::json!({"original_key_id":"original", "future":true}),
        ] {
            let journal = journal();
            let path = journal.path().join("config/journal.json");
            let mut config: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            config["backup"]["pending_key_revocation"] = pending;
            let before = serde_json::to_vec_pretty(&config).unwrap();
            std::fs::write(&path, &before).unwrap();
            let runner = Script {
                outputs: RefCell::new(VecDeque::new()),
                commands: RefCell::new(vec![]),
            };
            let result = rotate_recovery_key(
                journal.path(),
                &services(&runner, &Http, &TestClock, &Maintenance),
            );
            assert_eq!(result.status, "error");
            assert!(result.recovery_key.is_none());
            assert!(runner.commands.borrow().is_empty());
            assert_eq!(std::fs::read(path).unwrap(), before);
        }
    }
}
