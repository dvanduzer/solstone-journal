// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Android recovery-key publication and resumable original-key revocation.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use solstone_core_backup::archive_admission::ArchiveCancellation;
use solstone_core_backup::{
    PendingExecution, PendingKeyRevocation, backup_setup_fingerprint, candidate_binding,
    clear_pending_key_revocation_at_root, credential_binding, destination_from_config,
    format_recovery_key_display, generate_recovery_key, load_hosted_binding,
    stage_pending_key_revocation_at_root,
};
use solstone_core_journal_config_write::BoundConfigOutcome;
use solstone_core_journal_config_write::DirectoryFlush;
use solstone_core_journal_io::JournalRoot;

use crate::destination::validate_destination;
use crate::engine::BackupServices;
use crate::repo::{
    add_recovery_key, capture_rotation_key_id, remove_rotation_key, strict_key_list_absence,
};

#[cfg(feature = "test-hooks")]
use super::pause_after_remote_removal;
use super::{ROTATION_TIMEOUT_SECONDS, RotationResult, error, map_error};

/// A fetched capability expires within this attempt; later retries fetch fresh
/// credentials while keeping the same durable key-revocation intent.
struct RotationRunner<'a> {
    inner: &'a dyn crate::runner::ToolRunner,
    credential_deadline: Option<Instant>,
}

impl RotationRunner<'_> {
    fn bounded<'a>(
        &self,
        request: &crate::runner::ToolRequest<'a>,
    ) -> std::io::Result<crate::runner::ToolRequest<'a>> {
        let mut bounded = request.clone();
        if let Some(deadline) = self.credential_deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "rotation credential lifetime expired",
                ));
            }
            bounded.timeout = Some(
                bounded
                    .timeout
                    .map_or(remaining, |timeout| timeout.min(remaining)),
            );
        }
        Ok(bounded)
    }
}

impl crate::runner::ToolRunner for RotationRunner<'_> {
    fn run(
        &self,
        request: &crate::runner::ToolRequest<'_>,
    ) -> std::io::Result<crate::runner::ToolOutput> {
        self.inner.run(&self.bounded(request)?)
    }

    fn run_cancellable(
        &self,
        request: &crate::runner::ToolRequest<'_>,
        cancellation: &ArchiveCancellation,
    ) -> std::io::Result<crate::runner::ToolOutput> {
        self.inner
            .run_cancellable(&self.bounded(request)?, cancellation)
    }

    fn cancellation(&self) -> Option<&ArchiveCancellation> {
        self.inner.cancellation()
    }
}

fn is_confirmed_publication(
    pub_receipt: &solstone_core_journal_config_write::BoundConfigPublication,
) -> bool {
    let flush_ok = pub_receipt.directory_flush == DirectoryFlush::Synced;

    flush_ok
        && pub_receipt.durability_uncertainty.is_none()
        && pub_receipt.observation_uncertainty.is_none()
        && pub_receipt.confirmation_refusal.is_none()
}

fn clear_intent(
    root: &Arc<JournalRoot>,
    visible_sha: &str,
    candidate: &str,
    pending: &PendingKeyRevocation,
    binding: Option<&solstone_core_backup::HostedBinding>,
) -> RotationResult {
    let clear_outcome =
        match clear_pending_key_revocation_at_root(root, visible_sha, pending, binding) {
            Ok(outcome) => outcome,
            Err(_) => return error("failed"),
        };
    match clear_outcome {
        BoundConfigOutcome::Published(ref p) if is_confirmed_publication(p) => {}
        _ => return error("failed"),
    }
    match format_recovery_key_display(candidate) {
        Ok(display) => RotationResult {
            status: "ok".into(),
            reason_code: None,
            recovery_key: Some(candidate.to_owned()),
            recovery_key_display: Some(display),
        },
        Err(_) => error("failed"),
    }
}

/// Rotate the configured recovery key. No remote or local rollback is performed.
pub(super) fn rotate_recovery_key(journal: &Path, services: &BackupServices<'_>) -> RotationResult {
    let read = match solstone_core_journal_config::read_journal_config(journal) {
        Ok(read) => read,
        Err(_) => return error("failed"),
    };
    let (Some(config), Some(mut visible_sha)) = (read.config, read.sha256) else {
        return RotationResult {
            status: "skipped".into(),
            reason_code: None,
            recovery_key: None,
            recovery_key_display: None,
        };
    };

    let Some(backup) = config.get("backup").and_then(Value::as_object) else {
        return RotationResult {
            status: "skipped".into(),
            reason_code: None,
            recovery_key: None,
            recovery_key_display: None,
        };
    };

    let pending_intent = match backup.get("pending_key_revocation") {
        None => None,
        Some(value) => match PendingKeyRevocation::parse(value) {
            Some(pending) => Some(pending),
            None => return error("failed"),
        },
    };
    let normalized_backup = solstone_core_backup::merge_backup_config(&config);

    let mode = backup.get("mode").and_then(Value::as_str).unwrap_or("byo");
    if !matches!(mode, "byo" | "operated") {
        return error("failed");
    }

    let (Some(daily_key), Some(recovery_key)) = (
        backup.get("daily_key").and_then(Value::as_str),
        backup.get("recovery_key").and_then(Value::as_str),
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
    let mut credential_deadline = None;

    let (destination, hosted_binding) = match mode {
        "operated" => {
            let Some(binding) = load_hosted_binding(journal) else {
                return RotationResult {
                    status: "skipped".into(),
                    reason_code: None,
                    recovery_key: None,
                    recovery_key_display: None,
                };
            };
            let current_setup = backup_setup_fingerprint(&normalized_backup, Some(&binding));
            if current_setup.is_none()
                || pending_intent
                    .as_ref()
                    .is_some_and(|pending| pending.setup_fingerprint != current_setup)
            {
                return error("failed");
            }
            let credentials = match crate::hosted_runtime::fetch_hosted_credentials(
                services.http,
                &binding,
                "maintenance",
                services.version,
            ) {
                Ok(creds) => creds,
                Err(err) => return error(err.reason_code),
            };
            let expires = match chrono::DateTime::parse_from_rfc3339(&credentials.expires_at) {
                Ok(expires) => expires.timestamp(),
                Err(_) => return error("failed"),
            };
            let remaining = expires.saturating_sub(services.clock.now_unix());
            if remaining <= 0 {
                return error("failed");
            }
            credential_deadline = Instant::now().checked_add(Duration::from_secs(remaining as u64));
            if credential_deadline.is_none() {
                return error("failed");
            }
            let dest = crate::hosted_runtime::operated_destination(&binding, &credentials);
            (dest, Some(binding))
        }
        _ => {
            let Some(dest) = destination_from_config(backup) else {
                return RotationResult {
                    status: "skipped".into(),
                    reason_code: None,
                    recovery_key: None,
                    recovery_key_display: None,
                };
            };
            (dest, None)
        }
    };

    let setup_fingerprint =
        match backup_setup_fingerprint(&normalized_backup, hosted_binding.as_ref()) {
            Some(fingerprint) => fingerprint,
            None => return error("failed"),
        };
    let execution_credential_binding = if mode == "operated" {
        // The keyed setup identity covers the durable broker credential and
        // binding; temporary access/secret/session-token bytes stay ephemeral.
        setup_fingerprint.clone()
    } else {
        match credential_binding(daily_key, &destination.credentials) {
            Some(binding) => binding,
            None => return error("failed"),
        }
    };
    let runner = RotationRunner {
        inner: services.runner,
        credential_deadline,
    };

    let root = match JournalRoot::open(journal) {
        Ok(root) => Arc::new(root),
        Err(_) => return error("failed"),
    };

    let (candidate, pending) = if let Some(parsed) = pending_intent {
        let candidate = recovery_key.to_owned();

        let expected_candidate_binding = candidate_binding(daily_key, &candidate);

        let drifted = expected_candidate_binding.as_deref() != Some(&parsed.candidate_binding)
            || mode != parsed.execution.mode
            || destination.repository != parsed.execution.repository
            || destination.backend != parsed.execution.backend
            || execution_credential_binding != parsed.execution.credential_binding
            || parsed.setup_fingerprint.as_deref() != Some(setup_fingerprint.as_str());

        if drifted {
            return error("failed");
        }

        let verified =
            match validate_destination(&runner, &destination, &candidate, restic_path, timeout) {
                Ok(status) => status,
                Err(_) => return error("failed"),
            };
        if !(verified.repo_exists && verified.reason_code == "repo_exists") {
            return error(verified.reason_code);
        }

        match strict_key_list_absence(
            &runner,
            &destination,
            daily_key,
            &parsed.original_key_id,
            restic_path,
            timeout,
        ) {
            Ok(true) => {
                return clear_intent(
                    &root,
                    &visible_sha,
                    &candidate,
                    &parsed,
                    hosted_binding.as_ref(),
                );
            }
            Ok(false) => {}
            Err(err) => return map_error(err),
        }

        let stage_outcome = match stage_pending_key_revocation_at_root(
            &root,
            &visible_sha,
            &candidate,
            &parsed,
            hosted_binding.as_ref(),
        ) {
            Ok(outcome) => outcome,
            Err(_) => return error("failed"),
        };
        let pub_receipt = match stage_outcome {
            BoundConfigOutcome::Published(p) if is_confirmed_publication(&p) => p,
            _ => return error("failed"),
        };
        visible_sha = pub_receipt.candidate_sha256;

        (candidate, parsed)
    } else {
        let old_id = match capture_rotation_key_id(
            &runner,
            &destination,
            recovery_key,
            restic_path,
            timeout,
        ) {
            Ok(id) => id,
            Err(err) => return map_error(err),
        };

        let candidate = match generate_recovery_key() {
            Ok(key) => key,
            Err(_) => return error("failed"),
        };

        if let Err(err) = add_recovery_key(
            &runner,
            &destination,
            daily_key,
            &candidate,
            restic_path,
            timeout,
        ) {
            return map_error(err);
        }

        let verified =
            match validate_destination(&runner, &destination, &candidate, restic_path, timeout) {
                Ok(status) => status,
                Err(_) => return error("failed"),
            };
        if !(verified.repo_exists && verified.reason_code == "repo_exists") {
            return error(verified.reason_code);
        }

        let candidate_binding = match candidate_binding(daily_key, &candidate) {
            Some(b) => b,
            None => return error("failed"),
        };

        let pending = PendingKeyRevocation {
            original_key_id: old_id,
            candidate_binding,
            execution: PendingExecution {
                mode: mode.to_owned(),
                repository: destination.repository.clone(),
                backend: destination.backend.clone(),
                credential_binding: execution_credential_binding,
            },
            setup_fingerprint: Some(setup_fingerprint),
        };

        let stage_outcome = match stage_pending_key_revocation_at_root(
            &root,
            &visible_sha,
            &candidate,
            &pending,
            hosted_binding.as_ref(),
        ) {
            Ok(outcome) => outcome,
            Err(_) => return error("failed"),
        };
        let pub_receipt = match stage_outcome {
            BoundConfigOutcome::Published(p) if is_confirmed_publication(&p) => p,
            _ => return error("failed"),
        };
        visible_sha = pub_receipt.candidate_sha256;

        (candidate, pending)
    };

    let default_cancel = ArchiveCancellation::new();
    let cancellation = services.runner.cancellation().unwrap_or(&default_cancel);

    let binding_exclusion = match hosted_binding.as_ref() {
        Some(binding) => {
            match solstone_core_backup::hold_rotation_binding_at_root(&root, binding) {
                Ok(held) => Some(held),
                Err(_) => return error("failed"),
            }
        }
        None => None,
    };

    let removal = remove_rotation_key(
        &runner,
        &destination,
        daily_key,
        &pending.original_key_id,
        restic_path,
        timeout,
        cancellation,
    );

    #[cfg(feature = "test-hooks")]
    pause_after_remote_removal();

    if binding_exclusion
        .as_ref()
        .is_some_and(|held| held.revalidate().is_err())
    {
        return error("failed");
    }
    // Release binding exclusion before clear reacquires config→binding.
    drop(binding_exclusion);

    if let Err(err) = removal {
        return map_error(err);
    }

    clear_intent(
        &root,
        &visible_sha,
        &candidate,
        &pending,
        hosted_binding.as_ref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Clock, JournalMaintenance};
    use crate::hosted_runtime::{HttpError, HttpRequest, HttpResponse, HttpTransport};
    use crate::runner::{ToolOutput, ToolRequest, ToolRunner};
    use solstone_core_backup::Destination;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io;

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

        fn run_cancellable(
            &self,
            request: &ToolRequest<'_>,
            _cancellation: &ArchiveCancellation,
        ) -> io::Result<ToolOutput> {
            self.run(request)
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
        let destination = Destination {
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
        runner: &'a dyn ToolRunner,
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
        let config = solstone_core_backup::get_backup_config(journal.path()).unwrap();
        assert_eq!(config["confirmed_recovery_key"], false);
        assert!(config.get("pending_key_revocation").is_none());
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
    fn remove_failure_keeps_candidate_and_intent_staged_in_local_config() {
        let journal = journal();
        let before_keys = solstone_core_backup::get_keys(journal.path())
            .unwrap()
            .unwrap();
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

        let result = rotate_recovery_key(
            journal.path(),
            &services(&runner, &http, &clock, &maintenance),
        );
        assert_eq!(result.status, "error");
        assert!(result.recovery_key.is_none());
        assert_eq!(runner.commands.borrow().len(), 4);

        // Local config has candidate staged with pending revocation
        let after_keys = solstone_core_backup::get_keys(journal.path())
            .unwrap()
            .unwrap();
        assert_ne!(after_keys.recovery_key, before_keys.recovery_key);
        let config = solstone_core_backup::get_backup_config(journal.path()).unwrap();
        assert_eq!(config["confirmed_recovery_key"], false);
        assert!(config.get("pending_key_revocation").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn stage_publication_failure_records_no_remove_key() {
        use solstone_core_journal_io::{
            BoundPublicationPrimitive, run_with_bound_publication_fault,
        };

        let journal = journal();
        let before_keys = solstone_core_backup::get_keys(journal.path())
            .unwrap()
            .unwrap();
        let runner = Script {
            outputs: RefCell::new(VecDeque::from([
                output(0, "[{\"id\":\"old\",\"current\":true}]"),
                output(0, ""),
                output(0, ""),
            ])),
            commands: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock;
        let maintenance = Maintenance;

        let (result, fired) =
            run_with_bound_publication_fault(BoundPublicationPrimitive::ParentSync, 1, 5, || {
                rotate_recovery_key(
                    journal.path(),
                    &services(&runner, &http, &clock, &maintenance),
                )
            });
        assert!(fired);
        assert_eq!(result.status, "error");

        // Verify remove_key was NEVER called
        let commands = runner.commands.borrow();
        assert_eq!(
            commands
                .iter()
                .map(|args| args[..2.min(args.len())].join(" "))
                .collect::<Vec<_>>(),
            vec!["key list", "key add", "cat config"]
        );
        drop(commands);

        // Retry without fault successfully stages, removes key, and completes
        let runner_retry = Script {
            outputs: RefCell::new(VecDeque::from([
                output(0, ""), // cat config (validation of staged candidate)
                output(
                    0,
                    "[{\"id\":\"old\",\"current\":false},{\"id\":\"n0\",\"current\":true}]",
                ), // key list (still present remotely)
                output(0, ""), // key remove
            ])),
            commands: RefCell::new(vec![]),
        };
        let retry_result = rotate_recovery_key(
            journal.path(),
            &services(&runner_retry, &http, &clock, &maintenance),
        );
        assert_eq!(retry_result.status, "ok");
        let retry_commands = runner_retry.commands.borrow();
        assert_eq!(
            retry_commands
                .iter()
                .map(|args| args[..2.min(args.len())].join(" "))
                .collect::<Vec<_>>(),
            vec!["cat config", "key list", "key remove"]
        );
        let after_keys = solstone_core_backup::get_keys(journal.path())
            .unwrap()
            .unwrap();
        assert_ne!(after_keys.recovery_key, before_keys.recovery_key);
        let config = solstone_core_backup::get_backup_config(journal.path()).unwrap();
        assert!(config.get("pending_key_revocation").is_none());
    }

    #[cfg(all(test, unix, feature = "full-tests"))]
    #[test]
    fn clear_publication_uncertainty_reports_error_without_inventing_pending_retention() {
        use solstone_core_journal_io::{
            BoundPublicationPrimitive as Primitive, run_with_bound_publication_fault,
            run_with_bound_publication_faults_and_barrier,
        };
        for readback in [false, true] {
            let journal = journal();
            let old = solstone_core_backup::get_keys(journal.path())
                .unwrap()
                .unwrap()
                .recovery_key;
            let runner = Script {
                outputs: RefCell::new(VecDeque::from([
                    output(0, r#"[{"id":"old","current":true}]"#),
                    output(0, ""),
                    output(0, ""),
                    output(0, ""),
                ])),
                commands: RefCell::new(vec![]),
            };
            let http = Http;
            let clock = TestClock;
            let maintenance = Maintenance;
            let operation = || {
                rotate_recovery_key(
                    journal.path(),
                    &services(&runner, &http, &clock, &maintenance),
                )
            };
            let result = if readback {
                let path = journal.path().join("config/journal.json");
                let (result, consumed, fired) = run_with_bound_publication_faults_and_barrier(
                    [],
                    Primitive::ParentSync,
                    2,
                    move || {
                        // Clear already renamed. Preserve its bytes but replace its
                        // identity before native observation/owner confirmation.
                        let substitute = path.with_file_name("identity-substitute.json");
                        std::fs::copy(&path, &substitute).unwrap();
                        std::fs::rename(substitute, path).unwrap();
                    },
                    operation,
                );
                assert!(fired);
                assert_eq!(consumed, 0);
                result
            } else {
                let (result, fired) =
                    run_with_bound_publication_fault(Primitive::ParentSync, 2, 5, operation);
                assert!(fired);
                result
            };
            assert_eq!(result.status, "error");
            assert!(runner.outputs.borrow().is_empty());
            assert_eq!(
                runner
                    .commands
                    .borrow()
                    .iter()
                    .filter(|command| command.windows(2).any(|words| words == ["key", "remove"]))
                    .count(),
                1
            );
            let visible = solstone_core_backup::get_backup_config(journal.path()).unwrap();
            assert_ne!(visible["recovery_key"].as_str(), Some(old.as_str()));
            assert_eq!(visible["confirmed_recovery_key"], false);
            assert!(
                !visible.contains_key("pending_key_revocation"),
                "clear actually became visible before uncertainty"
            );
            assert!(
                result.recovery_key.is_none(),
                "error result does not invent an owner completion"
            );
        }
    }

    #[cfg(all(test, feature = "full-tests"))]
    #[test]
    fn expired_hosted_credentials_refuse_before_any_native_work() {
        struct ExpiredHttp;
        impl HttpTransport for ExpiredHttp {
            fn execute(&self, _: &HttpRequest) -> Result<HttpResponse, HttpError> {
                Ok(HttpResponse {status:200,headers:vec![],body:serde_json::to_vec(&serde_json::json!({
                    "access_key_id":"ACCESS","secret_access_key":"SECRET","session_token":"SESSION",
                    "endpoint":"https://s3.example.invalid","expires_at":"1970-01-01T00:00:00Z"
                })).unwrap()})
            }
        }
        let journal = tempfile::tempdir().unwrap();
        solstone_core_backup::set_mode(journal.path(), "operated").unwrap();
        solstone_core_backup::generate_and_store_keys(journal.path()).unwrap();
        solstone_core_backup::save_hosted_binding(
            journal.path(),
            &solstone_core_backup::HostedBinding {
                broker_endpoint: "https://broker.example.invalid".into(),
                account_id: "account".into(),
                instance_id: "instance".into(),
                bucket: "bucket".into(),
                prefix: "prefix".into(),
                broker_token: "durable-broker-token".into(),
            },
        )
        .unwrap();
        let before = std::fs::read(journal.path().join("config/journal.json")).unwrap();
        let runner = Script {
            outputs: RefCell::new(VecDeque::new()),
            commands: RefCell::new(vec![]),
        };
        let http = ExpiredHttp;
        let clock = TestClock;
        let maintenance = Maintenance;
        let result = rotate_recovery_key(
            journal.path(),
            &BackupServices {
                runner: &runner,
                http: &http,
                clock: &clock,
                restic_path: Some(Path::new("/fixture/bin/restic")),
                rclone_path: None,
                version: "test",
                journal_maintenance: &maintenance,
            },
        );
        assert_eq!(result.status, "error");
        assert!(runner.commands.borrow().is_empty());
        assert_eq!(
            std::fs::read(journal.path().join("config/journal.json")).unwrap(),
            before
        );
    }

    #[test]
    fn restart_with_valid_pending_and_absent_remote_key_clears_intent() {
        let journal = journal();
        let runner_first = Script {
            outputs: RefCell::new(VecDeque::from([
                output(0, "[{\"id\":\"old\",\"current\":true}]"),
                output(0, ""),
                output(0, ""),
                output(12, ""), // remove fails
            ])),
            commands: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock;
        let maintenance = Maintenance;
        let first_result = rotate_recovery_key(
            journal.path(),
            &services(&runner_first, &http, &clock, &maintenance),
        );
        assert_eq!(first_result.status, "error");
        let staged_config = solstone_core_backup::get_backup_config(journal.path()).unwrap();
        assert!(staged_config.get("pending_key_revocation").is_some());

        // On restart, strict_key_list_absence sees old key is already gone
        let runner_second = Script {
            outputs: RefCell::new(VecDeque::from([
                output(0, ""), // cat config (validate destination)
                output(0, "[{\"id\":\"new_id\",\"current\":true}]"), // key list (old id absent!)
            ])),
            commands: RefCell::new(vec![]),
        };
        let second_result = rotate_recovery_key(
            journal.path(),
            &services(&runner_second, &http, &clock, &maintenance),
        );
        assert_eq!(second_result.status, "ok");
        let commands = runner_second.commands.borrow();
        assert_eq!(
            commands
                .iter()
                .map(|args| args[..2.min(args.len())].join(" "))
                .collect::<Vec<_>>(),
            vec!["cat config", "key list"]
        );
        let cleared_config = solstone_core_backup::get_backup_config(journal.path()).unwrap();
        assert!(cleared_config.get("pending_key_revocation").is_none());
    }

    #[test]
    fn drift_refuses_removal_and_keeps_intent() {
        let journal = journal();
        let runner_first = Script {
            outputs: RefCell::new(VecDeque::from([
                output(0, "[{\"id\":\"old\",\"current\":true}]"),
                output(0, ""),
                output(0, ""),
                output(12, ""), // remove fails
            ])),
            commands: RefCell::new(vec![]),
        };
        let http = Http;
        let clock = TestClock;
        let maintenance = Maintenance;
        let first_result = rotate_recovery_key(
            journal.path(),
            &services(&runner_first, &http, &clock, &maintenance),
        );
        assert_eq!(first_result.status, "error");

        // Mutate destination repository to simulate drift
        let new_dest = Destination {
            repository: "drifted_repo".into(),
            backend: "s3".into(),
            credentials: serde_json::json!({"access_key_id":"access","secret_access_key":"secret"})
                .as_object()
                .unwrap()
                .clone(),
        };
        solstone_core_backup::set_destination(journal.path(), &new_dest).unwrap();

        let runner_second = Script {
            outputs: RefCell::new(VecDeque::from([
                output(0, ""), // cat config
            ])),
            commands: RefCell::new(vec![]),
        };
        let second_result = rotate_recovery_key(
            journal.path(),
            &services(&runner_second, &http, &clock, &maintenance),
        );
        assert_eq!(second_result.status, "error");
        let config = solstone_core_backup::get_backup_config(journal.path()).unwrap();
        assert!(config.get("pending_key_revocation").is_some());
        let commands = runner_second.commands.borrow();
        assert!(!commands.iter().any(|c| c.contains(&"remove".to_string())));
    }

    #[test]
    fn malformed_pending_intent_refuses() {
        for intent in [
            Value::Null,
            Value::Bool(false),
            serde_json::json!([]),
            serde_json::json!("pending"),
            serde_json::json!({"original_key_id": ""}),
        ] {
            let journal = journal();
            let config_path = journal.path().join("config/journal.json");
            let mut config_val: Value =
                serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
            config_val["backup"]["pending_key_revocation"] = intent;
            let before = serde_json::to_vec(&config_val).unwrap();
            std::fs::write(&config_path, &before).unwrap();
            let runner = Script {
                outputs: RefCell::new(VecDeque::new()),
                commands: RefCell::new(vec![]),
            };
            let result = rotate_recovery_key(
                journal.path(),
                &services(&runner, &Http, &TestClock, &Maintenance),
            );
            assert_eq!(result.status, "error");
            assert!(runner.commands.borrow().is_empty());
            assert_eq!(std::fs::read(&config_path).unwrap(), before);
        }
    }

    #[cfg(all(unix, feature = "full-tests"))]
    #[test]
    fn operated_binding_writer_is_excluded_through_remote_removal() {
        use solstone_core_backup::{HostedBinding, load_hosted_binding, save_hosted_binding};
        use solstone_core_journal_io::{LockOptions, hold_lock};
        use std::sync::{Mutex, mpsc};
        struct Broker;
        impl HttpTransport for Broker {
            fn execute(&self, _: &HttpRequest) -> Result<HttpResponse, HttpError> {
                Ok(HttpResponse { status: 200, headers: vec![], body: serde_json::to_vec(&serde_json::json!({
                    "access_key_id":"ACCESS", "secret_access_key":"SECRET", "session_token":"SESSION",
                    "endpoint":"https://s3.example.invalid", "expires_at":"1970-01-01T00:01:00Z"
                })).unwrap() })
            }
        }
        struct CheckedRemoval {
            script: Script,
            journal: std::path::PathBuf,
            original: HostedBinding,
            changed: HostedBinding,
            writer: Mutex<Option<std::thread::JoinHandle<()>>>,
            release: Mutex<Option<mpsc::Sender<()>>>,
        }
        impl ToolRunner for CheckedRemoval {
            fn run(&self, request: &ToolRequest<'_>) -> io::Result<ToolOutput> {
                if request
                    .argv
                    .windows(2)
                    .any(|words| words == ["key", "remove"])
                {
                    let journal = self.journal.clone();
                    let changed = self.changed.clone();
                    let (checked_tx, checked_rx) = mpsc::channel();
                    let (release_tx, release_rx) = mpsc::channel();
                    let writer = std::thread::spawn(move || {
                        // This is the same persistent lock used by the production
                        // save/delete writer; a second open must fail immediately.
                        let attempt = hold_lock(
                            journal.join("backup/hosted/binding.json"),
                            LockOptions {
                                timeout: Duration::ZERO,
                                ..LockOptions::default()
                            },
                        );
                        assert!(
                            attempt.is_err(),
                            "remote removal must retain binding exclusion"
                        );
                        checked_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                        save_hosted_binding(&journal, &changed).unwrap();
                        assert_eq!(load_hosted_binding(&journal), Some(changed));
                    });
                    checked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    assert_eq!(
                        load_hosted_binding(&self.journal),
                        Some(self.original.clone())
                    );
                    *self.writer.lock().unwrap() = Some(writer);
                    *self.release.lock().unwrap() = Some(release_tx);
                }
                self.script.run(request)
            }
            fn run_cancellable(
                &self,
                request: &ToolRequest<'_>,
                _: &ArchiveCancellation,
            ) -> io::Result<ToolOutput> {
                self.run(request)
            }
        }
        let journal = tempfile::tempdir().unwrap();
        solstone_core_backup::set_mode(journal.path(), "operated").unwrap();
        solstone_core_backup::generate_and_store_keys(journal.path()).unwrap();
        let original = HostedBinding {
            broker_endpoint: "https://broker.example.invalid".into(),
            account_id: "account".into(),
            instance_id: "instance".into(),
            bucket: "bucket".into(),
            prefix: "prefix".into(),
            broker_token: "original-token".into(),
        };
        save_hosted_binding(journal.path(), &original).unwrap();
        let runner = CheckedRemoval {
            script: Script {
                outputs: RefCell::new(VecDeque::from([
                    output(0, r#"[{"id":"old","current":true}]"#),
                    output(0, ""),
                    output(0, ""),
                    output(0, ""),
                ])),
                commands: RefCell::new(vec![]),
            },
            journal: journal.path().to_path_buf(),
            changed: HostedBinding {
                broker_token: "new-token".into(),
                ..original.clone()
            },
            original,
            writer: Mutex::new(None),
            release: Mutex::new(None),
        };
        let services = BackupServices {
            runner: &runner,
            http: &Broker,
            clock: &TestClock,
            restic_path: Some(Path::new("/fixture/bin/restic")),
            rclone_path: None,
            version: "test",
            journal_maintenance: &Maintenance,
        };
        let result = rotate_recovery_key(journal.path(), &services);
        // Release even on failure so that the test owns and joins its worker.
        runner
            .release
            .lock()
            .unwrap()
            .take()
            .expect("remote removal entered")
            .send(())
            .unwrap();
        runner
            .writer
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(result.status, "ok");
        assert!(runner.script.outputs.borrow().is_empty());
        assert!(
            !solstone_core_backup::get_backup_config(journal.path())
                .unwrap()
                .contains_key("pending_key_revocation")
        );
    }

    #[cfg(all(test, feature = "full-tests"))]
    #[test]
    fn duplicate_key_object_fields_cannot_clear_or_execute_pending_revocation() {
        for listing in [
            r#"[{"id":"old","id":"daily","current":true}]"#,
            r#"[{"id":"old","current":false,"current":true}]"#,
        ] {
            let journal = journal();
            let first = Script {
                outputs: RefCell::new(VecDeque::from([
                    output(0, r#"[{"id":"old","current":true}]"#),
                    output(0, ""),
                    output(0, ""),
                    output(124, ""),
                ])),
                commands: RefCell::new(vec![]),
            };
            let http = Http;
            let clock = TestClock;
            let maintenance = Maintenance;
            let result = rotate_recovery_key(
                journal.path(),
                &services(&first, &http, &clock, &maintenance),
            );
            assert_eq!(result.reason_code.as_deref(), Some("timeout"));
            let before = std::fs::read(journal.path().join("config/journal.json")).unwrap();
            let second = Script {
                outputs: RefCell::new(VecDeque::from([output(0, ""), output(0, listing)])),
                commands: RefCell::new(vec![]),
            };
            assert_eq!(
                rotate_recovery_key(
                    journal.path(),
                    &services(&second, &http, &clock, &maintenance)
                )
                .status,
                "error"
            );
            assert!(second.outputs.borrow().is_empty());
            assert_eq!(
                std::fs::read(journal.path().join("config/journal.json")).unwrap(),
                before
            );
            assert!(
                !second
                    .commands
                    .borrow()
                    .iter()
                    .any(|command| command.iter().any(|word| word == "add" || word == "remove"))
            );
        }
    }

    #[test]
    fn timeout_124_returns_error_and_keeps_candidate_and_intent() {
        let journal = journal();
        let runner = Script {
            outputs: RefCell::new(VecDeque::from([
                output(0, "[{\"id\":\"old\",\"current\":true}]"),
                output(0, ""),
                output(0, ""),
                output(124, ""),
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
        assert_eq!(result.status, "error");
        assert_eq!(result.reason_code.as_deref(), Some("timeout"));
        let config = solstone_core_backup::get_backup_config(journal.path()).unwrap();
        assert!(config.get("pending_key_revocation").is_some());
    }

    #[test]
    fn operated_destination_drift_refuses_removal() {
        struct HostedHttp;
        impl HttpTransport for HostedHttp {
            fn execute(&self, _: &HttpRequest) -> Result<HttpResponse, HttpError> {
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![],
                    body: serde_json::to_vec(&serde_json::json!({
                        "access_key_id": "key",
                        "secret_access_key": "secret",
                        "session_token": "token",
                        "endpoint": "https://new-broker.example.com",
                        "expires_at": "2026-10-14T00:00:00Z"
                    }))
                    .unwrap(),
                })
            }
        }

        let journal = tempfile::tempdir().unwrap();
        let binding = solstone_core_backup::HostedBinding {
            broker_endpoint: "https://broker.example.com".into(),
            account_id: "acct".into(),
            instance_id: "inst".into(),
            bucket: "mybucket".into(),
            prefix: "myprefix".into(),
            broker_token: "token".into(),
        };
        solstone_core_backup::save_hosted_binding(journal.path(), &binding).unwrap();
        solstone_core_backup::set_mode(journal.path(), "operated").unwrap();
        solstone_core_backup::generate_and_store_keys(journal.path()).unwrap();
        let keys = solstone_core_backup::get_keys(journal.path())
            .unwrap()
            .unwrap();

        // Staged intent holds old repository
        let old_repo = "s3:https://old-broker.example.com/mybucket/myprefix".to_string();
        let candidate_bind =
            solstone_core_backup::candidate_binding(&keys.daily_key, &keys.recovery_key).unwrap();
        let setup = solstone_core_backup::backup_setup_fingerprint(
            &solstone_core_backup::get_backup_config(journal.path()).unwrap(),
            Some(&binding),
        )
        .unwrap();
        let pending = PendingKeyRevocation {
            original_key_id: "old_key".into(),
            candidate_binding: candidate_bind,
            execution: PendingExecution {
                mode: "operated".into(),
                repository: old_repo,
                backend: "s3".into(),
                credential_binding: setup.clone(),
            },
            setup_fingerprint: Some(setup),
        };

        let config_path = journal.path().join("config/journal.json");
        let mut config_val: Value =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        config_val["backup"]["pending_key_revocation"] = serde_json::to_value(&pending).unwrap();
        std::fs::write(&config_path, serde_json::to_vec(&config_val).unwrap()).unwrap();

        let runner = Script {
            outputs: RefCell::new(VecDeque::from([
                output(0, ""), // cat config (validation succeeds)
            ])),
            commands: RefCell::new(vec![]),
        };
        let http = HostedHttp;
        let clock = TestClock;
        let maintenance = Maintenance;

        let result = rotate_recovery_key(
            journal.path(),
            &BackupServices {
                runner: &runner,
                http: &http,
                clock: &clock,
                restic_path: Some(Path::new("/fixture/bin/restic")),
                rclone_path: None,
                version: "test",
                journal_maintenance: &maintenance,
            },
        );

        assert_eq!(result.status, "error");
        let after_config = solstone_core_backup::get_backup_config(journal.path()).unwrap();
        assert!(after_config.get("pending_key_revocation").is_some());
        let commands = runner.commands.borrow();
        assert!(
            commands.is_empty(),
            "destination drift must refuse before any native work"
        );
    }

    #[cfg(all(test, feature = "full-tests"))]
    #[test]
    fn operated_retry_refreshes_ephemeral_credentials_without_adding_another_key() {
        use std::cell::Cell;
        struct RotatingHttp(Cell<usize>);
        impl HttpTransport for RotatingHttp {
            fn execute(&self, _: &HttpRequest) -> Result<HttpResponse, HttpError> {
                let n = self.0.get() + 1;
                self.0.set(n);
                Ok(HttpResponse {status:200,headers:vec![],body:serde_json::to_vec(&serde_json::json!({
                    "access_key_id":format!("ACCESS{n}"),"secret_access_key":format!("SECRET{n}"),
                    "session_token":format!("SESSION{n}"),"endpoint":"https://s3.example.invalid",
                    "expires_at":"1970-01-01T00:00:40Z"
                })).unwrap()})
            }
        }
        struct CheckedRunner {
            script: Script,
            token: &'static str,
        }
        impl ToolRunner for CheckedRunner {
            fn run(&self, request: &ToolRequest<'_>) -> io::Result<ToolOutput> {
                assert_eq!(
                    request
                        .env
                        .get(std::ffi::OsStr::new("AWS_SESSION_TOKEN"))
                        .unwrap(),
                    self.token
                );
                assert!(request.timeout.is_some_and(
                    |timeout| !timeout.is_zero() && timeout <= Duration::from_secs(40)
                ));
                self.script.run(request)
            }

            fn run_cancellable(
                &self,
                request: &ToolRequest<'_>,
                cancellation: &ArchiveCancellation,
            ) -> io::Result<ToolOutput> {
                assert!(!cancellation.is_cancelled());
                self.run(request)
            }
        }
        let journal = tempfile::tempdir().unwrap();
        solstone_core_backup::set_mode(journal.path(), "operated").unwrap();
        solstone_core_backup::generate_and_store_keys(journal.path()).unwrap();
        solstone_core_backup::save_hosted_binding(
            journal.path(),
            &solstone_core_backup::HostedBinding {
                broker_endpoint: "https://broker.example.invalid".into(),
                account_id: "account".into(),
                instance_id: "instance".into(),
                bucket: "bucket".into(),
                prefix: "prefix".into(),
                broker_token: "durable-broker-token".into(),
            },
        )
        .unwrap();
        let first = CheckedRunner {
            token: "SESSION1",
            script: Script {
                outputs: RefCell::new(VecDeque::from([
                    output(0, r#"[{"id":"old","current":true}]"#),
                    output(0, ""),
                    output(0, ""),
                    output(124, ""),
                ])),
                commands: RefCell::new(vec![]),
            },
        };
        let http = RotatingHttp(Cell::new(0));
        let clock = TestClock;
        let maintenance = Maintenance;
        let services = |runner| BackupServices {
            runner,
            http: &http,
            clock: &clock,
            restic_path: Some(Path::new("/fixture/bin/restic")),
            rclone_path: None,
            version: "test",
            journal_maintenance: &maintenance,
        };
        let first_result = rotate_recovery_key(journal.path(), &services(&first));
        assert_eq!(first_result.status, "error");
        assert_eq!(first_result.reason_code.as_deref(), Some("timeout"));
        assert!(first.script.outputs.borrow().is_empty());
        let staged = solstone_core_backup::get_backup_config(journal.path()).unwrap();
        assert!(PendingKeyRevocation::parse(&staged["pending_key_revocation"]).is_some());
        let candidate = staged["recovery_key"].clone();
        solstone_core_backup::set_recovery_key_confirmed(journal.path(), true).unwrap();
        let second = CheckedRunner {
            token: "SESSION2",
            script: Script {
                outputs: RefCell::new(VecDeque::from([
                    output(0, ""),
                    output(
                        0,
                        r#"[{"id":"old","current":false},{"id":"daily","current":true}]"#,
                    ),
                    output(0, ""),
                ])),
                commands: RefCell::new(vec![]),
            },
        };
        assert_eq!(
            rotate_recovery_key(journal.path(), &services(&second)).status,
            "ok"
        );
        assert_eq!(http.0.get(), 2);
        let final_config = solstone_core_backup::get_backup_config(journal.path()).unwrap();
        assert_eq!(final_config["recovery_key"], candidate);
        assert_eq!(final_config["confirmed_recovery_key"], true);
        assert!(!final_config.contains_key("pending_key_revocation"));
        assert!(
            !second
                .script
                .commands
                .borrow()
                .iter()
                .any(|command| command.windows(2).any(|words| words == ["key", "add"]))
        );
        let serialized = serde_json::to_string(&staged).unwrap();
        for temporary in ["ACCESS1", "SECRET1", "SESSION1"] {
            assert!(!serialized.contains(temporary));
        }
    }
}
