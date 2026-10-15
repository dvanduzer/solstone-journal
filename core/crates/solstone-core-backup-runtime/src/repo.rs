// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::BTreeMap;
use std::fmt;
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd};
use std::path::Path;
use std::time::Duration;

use solstone_core_backup::archive_admission::ArchiveCancellation;
use solstone_core_backup::{BackupError, Destination, assemble_backend_env};

use crate::destination::validate_destination;
use crate::runner::run_restic_with_stdin_cancellable;
use crate::runner::{PassedHandle, ToolRunner, run_restic, run_restic_with_stdin};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResticKeyError {
    pub returncode: i32,
}
impl fmt::Display for ResticKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "restic key operation failed with returncode {}",
            self.returncode
        )
    }
}
impl std::error::Error for ResticKeyError {}
#[derive(Debug)]
pub enum RepoError {
    Backup(BackupError),
    Key(ResticKeyError),
    Failed,
}
impl From<BackupError> for RepoError {
    fn from(error: BackupError) -> Self {
        Self::Backup(error)
    }
}

fn backend(destination: &Destination) -> Result<BTreeMap<String, Option<String>>, BackupError> {
    Ok(assemble_backend_env(destination)?
        .into_iter()
        .map(|(key, value)| {
            (
                key,
                value
                    .as_str()
                    .map(|value| Some(value.to_owned()))
                    .unwrap_or(None),
            )
        })
        .collect())
}
pub fn init_repository(
    runner: &dyn ToolRunner,
    destination: &Destination,
    daily_key: &str,
    recovery_key: &str,
    restic_path: &Path,
    timeout: Option<Duration>,
) -> Result<(), RepoError> {
    let daily = validate_destination(runner, destination, daily_key, restic_path, timeout)?;
    if daily.reason_code == "repo_missing" {
        run(
            runner,
            &["init".into()],
            destination,
            daily_key,
            restic_path,
            timeout,
            &[],
            None,
        )?;
        add_recovery_key(
            runner,
            destination,
            daily_key,
            recovery_key,
            restic_path,
            timeout,
        )?;
        return verify_recovery_key(runner, destination, recovery_key, restic_path, timeout);
    }
    if daily.reason_code == "repo_exists" {
        let recovery =
            validate_destination(runner, destination, recovery_key, restic_path, timeout)?;
        if recovery.repo_exists && recovery.reason_code == "repo_exists" {
            return Ok(());
        }
        if recovery.reason_code == "auth_failed" {
            add_recovery_key(
                runner,
                destination,
                daily_key,
                recovery_key,
                restic_path,
                timeout,
            )?;
            return verify_recovery_key(runner, destination, recovery_key, restic_path, timeout);
        }
    }
    Err(RepoError::Failed)
}
#[cfg(any(windows, test))]
fn windows_add_key_request_parts(new_password: &str, labels: &[String]) -> (Vec<String>, Vec<u8>) {
    let mut args: Vec<String> = vec!["key".into(), "add".into()];
    args.extend_from_slice(labels);
    (args, format!("{new_password}\n").into_bytes())
}

pub fn add_recovery_key(
    runner: &dyn ToolRunner,
    destination: &Destination,
    daily_key: &str,
    recovery_key: &str,
    restic_path: &Path,
    timeout: Option<Duration>,
) -> Result<(), RepoError> {
    add_key(
        runner,
        destination,
        daily_key,
        recovery_key,
        &[],
        &backend(destination)?,
        restic_path,
        timeout,
    )
}

#[allow(clippy::too_many_arguments)]
fn add_key(
    runner: &dyn ToolRunner,
    destination: &Destination,
    password: &str,
    new_password: &str,
    labels: &[String],
    env: &BTreeMap<String, Option<String>>,
    restic_path: &Path,
    timeout: Option<Duration>,
) -> Result<(), RepoError> {
    #[cfg(unix)]
    {
        let (reader, writer) = nix::unistd::pipe().map_err(|_| RepoError::Failed)?;
        let mut writer = std::fs::File::from(writer);
        use std::io::Write;
        writer
            .write_all(format!("{new_password}\n").as_bytes())
            .map_err(|_| RepoError::Failed)?;
        drop(writer);
        let fd = reader.as_raw_fd();
        let mut args: Vec<String> = vec![
            "key".into(),
            "add".into(),
            "--new-password-file".into(),
            format!("/dev/fd/{fd}"),
        ];
        args.extend_from_slice(labels);
        run_in(
            runner,
            &args,
            destination,
            env,
            password,
            restic_path,
            timeout,
            &[reader.as_fd()],
            None,
        )
    }
    #[cfg(windows)]
    {
        let (args, stdin_bytes) = windows_add_key_request_parts(new_password, labels);
        run_in(
            runner,
            &args,
            destination,
            env,
            password,
            restic_path,
            timeout,
            &[],
            Some(stdin_bytes),
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (
            runner,
            destination,
            password,
            new_password,
            labels,
            env,
            restic_path,
            timeout,
        );
        Err(RepoError::Failed)
    }
}
pub fn capture_current_key_id(
    runner: &dyn ToolRunner,
    destination: &Destination,
    password: &str,
    restic_path: &Path,
    timeout: Option<Duration>,
) -> Result<String, RepoError> {
    current_key(runner, destination, password, None, restic_path, timeout)
}
pub fn remove_key(
    runner: &dyn ToolRunner,
    destination: &Destination,
    password: &str,
    key_id: &str,
    restic_path: &Path,
    timeout: Option<Duration>,
) -> Result<(), RepoError> {
    run(
        runner,
        &["key".into(), "remove".into(), key_id.into()],
        destination,
        password,
        restic_path,
        timeout,
        &[],
        None,
    )
}

pub(crate) fn capture_rotation_key_id(
    runner: &dyn ToolRunner,
    destination: &Destination,
    password: &str,
    restic_path: &Path,
    timeout: Option<Duration>,
) -> Result<String, RepoError> {
    strict_key_list(runner, destination, password, restic_path, timeout)?
        .into_iter()
        .find(|(_, current)| *current)
        .map(|(id, _)| id)
        .ok_or(RepoError::Failed)
}
pub(crate) fn remove_rotation_key(
    runner: &dyn ToolRunner,
    destination: &Destination,
    password: &str,
    key_id: &str,
    restic_path: &Path,
    timeout: Option<Duration>,
    cancellation: &ArchiveCancellation,
) -> Result<(), RepoError> {
    let result = run_restic_with_stdin_cancellable(
        runner,
        &["key".into(), "remove".into(), key_id.into()],
        &destination.repository,
        password,
        restic_path,
        Some(&backend(destination)?),
        false,
        None,
        timeout,
        &[],
        None,
        cancellation,
    )
    .map_err(|_| RepoError::Failed)?;
    if result.returncode == 0 {
        Ok(())
    } else {
        Err(RepoError::Key(ResticKeyError {
            returncode: result.returncode,
        }))
    }
}

pub(crate) fn strict_key_list_absence(
    runner: &dyn ToolRunner,
    destination: &Destination,
    password: &str,
    target_key_id: &str,
    restic_path: &Path,
    timeout: Option<Duration>,
) -> Result<bool, RepoError> {
    Ok(
        strict_key_list(runner, destination, password, restic_path, timeout)?
            .iter()
            .all(|(id, _)| id != target_key_id),
    )
}

fn strict_key_list(
    runner: &dyn ToolRunner,
    destination: &Destination,
    password: &str,
    restic_path: &Path,
    timeout: Option<Duration>,
) -> Result<Vec<(String, bool)>, RepoError> {
    let args: Vec<String> = vec!["key".into(), "list".into()];
    let result = run_restic(
        runner,
        &args,
        &destination.repository,
        password,
        restic_path,
        Some(&backend(destination)?),
        true,
        None,
        timeout,
        &[],
    )
    .map_err(|_| RepoError::Failed)?;
    if result.returncode != 0 {
        return Err(RepoError::Key(ResticKeyError {
            returncode: result.returncode,
        }));
    }
    // Diagnostic scrubbing must never become authority for key identity or
    // absence. Rotating credentials can otherwise change the apparent IDs.
    if result.stdout.contains("[redacted]") {
        return Err(RepoError::Failed);
    }
    #[derive(serde::Deserialize)]
    struct KeyRow {
        id: String,
        current: bool,
    }
    // Typed fields reject duplicates before a JSON map can overwrite them.
    // Other restic metadata fields remain allowed.
    let items: Vec<KeyRow> = serde_json::from_str(&result.stdout).map_err(|_| RepoError::Failed)?;
    let mut seen_ids = std::collections::HashSet::new();
    let mut keys = Vec::with_capacity(items.len());
    let mut current_count = 0;
    for KeyRow { id, current } in items {
        if id.trim().is_empty() || !seen_ids.insert(id.clone()) {
            return Err(RepoError::Failed);
        }
        current_count += usize::from(current);
        keys.push((id, current));
    }
    if current_count != 1 {
        return Err(RepoError::Failed);
    }
    Ok(keys)
}

/// The username and hostname on every key file in an operated repository.
/// Restic keeps both in the clear beside the encrypted key, so storage run for
/// the owner holds this fixed word instead of their account and computer names.
pub const NEUTRAL_KEY_LABEL: &str = "solstone";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyLabelOutcome {
    pub added: usize,
    pub removed: usize,
}

#[derive(Clone, Debug)]
struct KeyRecord {
    id: String,
    current: bool,
    neutral: bool,
}

/// Bring every key file the journal's two secrets open to the neutral label.
///
/// For each secret, a neutral key file is added if none exists, and it must
/// open with that same secret before any labelled key file the secret opens is
/// removed, so the repository is never left without a key for either secret.
/// A key file neither secret opens is not the journal's to judge and stays.
/// Removal needs a credential that can delete.
pub fn neutralize_key_labels(
    runner: &dyn ToolRunner,
    destination: &Destination,
    daily_key: &str,
    recovery_key: &str,
    restic_path: &Path,
    timeout: Option<Duration>,
) -> Result<KeyLabelOutcome, RepoError> {
    let mut outcome = KeyLabelOutcome::default();
    if list_keys(runner, destination, daily_key, None, restic_path, timeout)?
        .iter()
        .all(|key| key.neutral)
    {
        return Ok(outcome);
    }
    for secret in [daily_key, recovery_key] {
        let listed = list_keys(runner, destination, secret, None, restic_path, timeout)?;
        let mut opened = Vec::new();
        for key in &listed {
            if current_key(
                runner,
                destination,
                secret,
                Some(&key.id),
                restic_path,
                timeout,
            )? == key.id
            {
                opened.push(key.clone());
            }
        }
        if opened.iter().all(|key| key.neutral) {
            continue;
        }
        let neutral_id = match opened.iter().find(|key| key.neutral) {
            Some(key) => key.id.clone(),
            None => {
                let id =
                    add_neutral_key(runner, destination, secret, &listed, restic_path, timeout)?;
                outcome.added += 1;
                id
            }
        };
        if current_key(
            runner,
            destination,
            secret,
            Some(&neutral_id),
            restic_path,
            timeout,
        )? != neutral_id
        {
            return Err(RepoError::Failed);
        }
        for key in opened.iter().filter(|key| !key.neutral) {
            run(
                runner,
                &[
                    "key".into(),
                    "remove".into(),
                    key.id.clone(),
                    "--key-hint".into(),
                    neutral_id.clone(),
                ],
                destination,
                secret,
                restic_path,
                timeout,
                &[],
                None,
            )?;
            outcome.removed += 1;
        }
    }
    Ok(outcome)
}

/// Log a key-label pass. Both callers carry on either way, so this is the trace.
pub fn log_key_label_outcome(outcome: Result<KeyLabelOutcome, RepoError>) {
    match outcome {
        Ok(KeyLabelOutcome {
            added: 0,
            removed: 0,
        }) => {}
        Ok(outcome) => log::info!(
            "backup key labels neutralized: added={} removed={}",
            outcome.added,
            outcome.removed
        ),
        Err(error) => log::warn!("backup key labels not neutralized: {error:?}"),
    }
}

fn add_neutral_key(
    runner: &dyn ToolRunner,
    destination: &Destination,
    secret: &str,
    before: &[KeyRecord],
    restic_path: &Path,
    timeout: Option<Duration>,
) -> Result<String, RepoError> {
    let mut env = backend(destination)?;
    // Restic stamps `created` in local time; UTC keeps the owner's offset out.
    env.insert("TZ".into(), Some("UTC".into()));
    add_key(
        runner,
        destination,
        secret,
        secret,
        &[
            "--user".into(),
            NEUTRAL_KEY_LABEL.into(),
            "--host".into(),
            NEUTRAL_KEY_LABEL.into(),
        ],
        &env,
        restic_path,
        timeout,
    )?;
    let mut added = list_keys(runner, destination, secret, None, restic_path, timeout)?
        .into_iter()
        .filter(|key| key.neutral && before.iter().all(|old| old.id != key.id));
    match (added.next(), added.next()) {
        (Some(key), None) => Ok(key.id),
        _ => Err(RepoError::Failed),
    }
}

fn current_key(
    runner: &dyn ToolRunner,
    destination: &Destination,
    password: &str,
    hint: Option<&str>,
    restic_path: &Path,
    timeout: Option<Duration>,
) -> Result<String, RepoError> {
    list_keys(runner, destination, password, hint, restic_path, timeout)?
        .into_iter()
        .find(|key| key.current)
        .map(|key| key.id)
        .ok_or(RepoError::Failed)
}

fn list_keys(
    runner: &dyn ToolRunner,
    destination: &Destination,
    password: &str,
    hint: Option<&str>,
    restic_path: &Path,
    timeout: Option<Duration>,
) -> Result<Vec<KeyRecord>, RepoError> {
    let mut args: Vec<String> = vec!["key".into(), "list".into()];
    if let Some(hint) = hint {
        args.extend(["--key-hint".into(), hint.into()]);
    }
    let result = run_restic(
        runner,
        &args,
        &destination.repository,
        password,
        restic_path,
        Some(&backend(destination)?),
        true,
        None,
        timeout,
        &[],
    )
    .map_err(|_| RepoError::Failed)?;
    if result.returncode != 0 {
        return Err(RepoError::Key(ResticKeyError {
            returncode: result.returncode,
        }));
    }
    result
        .json
        .as_ref()
        .and_then(serde_json::Value::as_array)
        .ok_or(RepoError::Failed)?
        .iter()
        .map(|item| {
            let text = |field: &str| item.get(field).and_then(serde_json::Value::as_str);
            let id = text("id")
                .filter(|id| !id.is_empty())
                .ok_or(RepoError::Failed)?;
            Ok(KeyRecord {
                id: id.to_owned(),
                current: item.get("current") == Some(&serde_json::Value::Bool(true)),
                neutral: text("userName") == Some(NEUTRAL_KEY_LABEL)
                    && text("hostName") == Some(NEUTRAL_KEY_LABEL),
            })
        })
        .collect()
}
fn verify_recovery_key(
    runner: &dyn ToolRunner,
    destination: &Destination,
    recovery_key: &str,
    restic_path: &Path,
    timeout: Option<Duration>,
) -> Result<(), RepoError> {
    let status = validate_destination(runner, destination, recovery_key, restic_path, timeout)?;
    (status.repo_exists && status.reason_code == "repo_exists")
        .then_some(())
        .ok_or(RepoError::Failed)
}
#[allow(clippy::too_many_arguments)]
fn run(
    runner: &dyn ToolRunner,
    args: &[String],
    destination: &Destination,
    password: &str,
    restic_path: &Path,
    timeout: Option<Duration>,
    pass_fds: &[PassedHandle<'_>],
    stdin: Option<Vec<u8>>,
) -> Result<(), RepoError> {
    run_in(
        runner,
        args,
        destination,
        &backend(destination)?,
        password,
        restic_path,
        timeout,
        pass_fds,
        stdin,
    )
}
#[allow(clippy::too_many_arguments)]
fn run_in(
    runner: &dyn ToolRunner,
    args: &[String],
    destination: &Destination,
    env: &BTreeMap<String, Option<String>>,
    password: &str,
    restic_path: &Path,
    timeout: Option<Duration>,
    pass_fds: &[PassedHandle<'_>],
    stdin: Option<Vec<u8>>,
) -> Result<(), RepoError> {
    let result = run_restic_with_stdin(
        runner,
        args,
        &destination.repository,
        password,
        restic_path,
        Some(env),
        false,
        None,
        timeout,
        pass_fds,
        stdin,
    )
    .map_err(|_| RepoError::Failed)?;
    if result.returncode == 0 {
        Ok(())
    } else {
        Err(RepoError::Key(ResticKeyError {
            returncode: result.returncode,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{ToolOutput, ToolRequest};
    use serde_json::json;
    use std::cell::RefCell;
    use std::io;

    struct Script {
        codes: RefCell<Vec<i32>>,
        commands: RefCell<Vec<Vec<String>>>,
    }
    impl ToolRunner for Script {
        fn run(&self, request: &ToolRequest<'_>) -> io::Result<ToolOutput> {
            self.commands.borrow_mut().push(
                request
                    .argv
                    .iter()
                    .map(|arg| arg.to_string_lossy().to_string())
                    .collect(),
            );
            Ok(ToolOutput {
                returncode: self.codes.borrow_mut().remove(0),
                stdout: vec![],
                stderr: vec![],
            })
        }
    }
    fn destination() -> Destination {
        Destination {
            repository: "repo".into(),
            backend: "s3".into(),
            credentials: serde_json::from_value(
                json!({"access_key_id":"ACCESS","secret_access_key":"SECRET"}),
            )
            .unwrap(),
        }
    }

    #[test]
    fn missing_repository_initializes_adds_and_verifies_in_order() {
        let runner = Script {
            codes: RefCell::new(vec![10, 0, 0, 0]),
            commands: RefCell::new(vec![]),
        };
        init_repository(
            &runner,
            &destination(),
            "daily",
            "recovery",
            Path::new("/fixture/bin/restic"),
            None,
        )
        .unwrap();
        let commands = runner.commands.borrow();
        assert_eq!(
            commands
                .iter()
                .map(|args| args[0].as_str())
                .collect::<Vec<_>>(),
            vec!["cat", "init", "key", "cat"]
        );
        assert!(
            commands
                .iter()
                .all(|args| args[0] != "key" || args.get(1) != Some(&"remove".into()))
        );
    }

    #[test]
    fn existing_repository_only_adds_when_recovery_auth_fails() {
        let valid = Script {
            codes: RefCell::new(vec![0, 0]),
            commands: RefCell::new(vec![]),
        };
        init_repository(
            &valid,
            &destination(),
            "daily",
            "recovery",
            Path::new("/fixture/bin/restic"),
            None,
        )
        .unwrap();
        assert!(
            !valid
                .commands
                .borrow()
                .iter()
                .any(|args| args.get(1) == Some(&"add".into()))
        );
        let repair = Script {
            codes: RefCell::new(vec![0, 12, 0, 0]),
            commands: RefCell::new(vec![]),
        };
        init_repository(
            &repair,
            &destination(),
            "daily",
            "recovery",
            Path::new("/fixture/bin/restic"),
            None,
        )
        .unwrap();
        assert!(
            repair
                .commands
                .borrow()
                .iter()
                .any(|args| args.get(1) == Some(&"add".into()))
        );
        let refused = Script {
            codes: RefCell::new(vec![0, 11]),
            commands: RefCell::new(vec![]),
        };
        assert!(
            init_repository(
                &refused,
                &destination(),
                "daily",
                "recovery",
                Path::new("/fixture/bin/restic"),
                None
            )
            .is_err()
        );
        assert!(
            !refused
                .commands
                .borrow()
                .iter()
                .any(|args| args.get(1) == Some(&"add".into()))
        );
    }

    #[test]
    fn failures_keep_prior_remote_phases_and_never_roll_back() {
        let add_failure = Script {
            codes: RefCell::new(vec![10, 0, 12]),
            commands: RefCell::new(vec![]),
        };
        assert!(
            init_repository(
                &add_failure,
                &destination(),
                "daily",
                "recovery",
                Path::new("/fixture/bin/restic"),
                None
            )
            .is_err()
        );
        assert_eq!(
            add_failure
                .commands
                .borrow()
                .iter()
                .map(|args| args[0].as_str())
                .collect::<Vec<_>>(),
            vec!["cat", "init", "key"]
        );
        let verify_failure = Script {
            codes: RefCell::new(vec![10, 0, 0, 12]),
            commands: RefCell::new(vec![]),
        };
        assert!(
            init_repository(
                &verify_failure,
                &destination(),
                "daily",
                "recovery",
                Path::new("/fixture/bin/restic"),
                None
            )
            .is_err()
        );
        assert_eq!(
            verify_failure
                .commands
                .borrow()
                .iter()
                .map(|args| args[0].as_str())
                .collect::<Vec<_>>(),
            vec!["cat", "init", "key", "cat"]
        );
    }

    #[test]
    fn windows_add_key_request_parts_shapes_argv_and_stdin() {
        let (argv, stdin) = windows_add_key_request_parts("test-recovery-key-123", &[]);
        assert_eq!(argv, vec!["key", "add"]);
        assert_eq!(stdin, b"test-recovery-key-123\n");
    }

    /// One key file in the fake repository: who it is labelled as, and the
    /// secret that opens it.
    #[derive(Clone)]
    struct FakeKey {
        id: String,
        user: String,
        host: String,
        secret: String,
    }

    /// Models the restic key behaviour this module relies on: `--key-hint` is
    /// tried first, otherwise keys are tried in id order; `key remove` refuses
    /// the key the run opened with.
    struct FakeRestic {
        keys: RefCell<Vec<FakeKey>>,
        next_id: RefCell<u32>,
        add_returncode: i32,
        add_opens_with: Option<String>,
        log: RefCell<Vec<String>>,
        add_env_tz: RefCell<Vec<Option<String>>>,
    }
    impl FakeRestic {
        fn new(keys: &[(&str, &str, &str)]) -> Self {
            Self {
                keys: RefCell::new(
                    keys.iter()
                        .map(|(id, label, secret)| FakeKey {
                            id: (*id).into(),
                            user: (*label).into(),
                            host: (*label).into(),
                            secret: (*secret).into(),
                        })
                        .collect(),
                ),
                next_id: RefCell::new(0),
                add_returncode: 0,
                add_opens_with: None,
                log: RefCell::new(vec![]),
                add_env_tz: RefCell::new(vec![]),
            }
        }
        fn opened(&self, secret: &str, hint: Option<&str>) -> Option<String> {
            let keys = self.keys.borrow();
            if let Some(hint) = hint
                && keys
                    .iter()
                    .any(|key| key.id == hint && key.secret == secret)
            {
                return Some(hint.to_owned());
            }
            let mut ids: Vec<&FakeKey> = keys.iter().filter(|key| key.secret == secret).collect();
            ids.sort_by(|a, b| a.id.cmp(&b.id));
            ids.first().map(|key| key.id.clone())
        }
        fn labels(&self) -> Vec<(String, String)> {
            let mut keys: Vec<(String, String)> = self
                .keys
                .borrow()
                .iter()
                .map(|key| (key.user.clone(), key.secret.clone()))
                .collect();
            keys.sort();
            keys
        }
    }
    fn output(returncode: i32, stdout: String) -> io::Result<ToolOutput> {
        Ok(ToolOutput {
            returncode,
            stdout: stdout.into_bytes(),
            stderr: vec![],
        })
    }
    impl ToolRunner for FakeRestic {
        fn run(&self, request: &ToolRequest<'_>) -> io::Result<ToolOutput> {
            let argv: Vec<String> = request
                .argv
                .iter()
                .map(|arg| arg.to_string_lossy().to_string())
                .collect();
            let env = |name: &str| {
                request
                    .env
                    .get(std::ffi::OsStr::new(name))
                    .map(|value| value.to_string_lossy().to_string())
            };
            let password = env("RESTIC_PASSWORD").unwrap_or_default();
            let flag = |name: &str| {
                argv.iter()
                    .position(|arg| arg == name)
                    .map(|index| argv[index + 1].clone())
            };
            let Some(opened) = self.opened(&password, flag("--key-hint").as_deref()) else {
                return output(12, String::new());
            };
            match (argv[0].as_str(), argv[1].as_str()) {
                ("key", "list") => {
                    let records: Vec<_> = self
                        .keys
                        .borrow()
                        .iter()
                        .map(|key| json!({"current": key.id == opened, "id": key.id, "userName": key.user, "hostName": key.host}))
                        .collect();
                    output(0, serde_json::to_string(&records).unwrap())
                }
                ("key", "add") => {
                    self.add_env_tz.borrow_mut().push(env("TZ"));
                    if self.add_returncode != 0 {
                        return output(self.add_returncode, String::new());
                    }
                    let id = format!("n{}", self.next_id.borrow());
                    *self.next_id.borrow_mut() += 1;
                    self.log
                        .borrow_mut()
                        .push(format!("add {id} for {password}"));
                    self.keys.borrow_mut().push(FakeKey {
                        id,
                        user: flag("--user").unwrap_or_else(|| "machine".into()),
                        host: flag("--host").unwrap_or_else(|| "machine".into()),
                        secret: self.add_opens_with.clone().unwrap_or(password),
                    });
                    output(0, String::new())
                }
                ("key", "remove") => {
                    let target = argv[2].clone();
                    if target == opened {
                        return output(1, String::new());
                    }
                    let neutral_sibling = self.keys.borrow().iter().any(|key| {
                        key.id != target && key.user == NEUTRAL_KEY_LABEL && key.secret == password
                    });
                    assert!(
                        neutral_sibling,
                        "removed {target} with no neutral key for {password}"
                    );
                    self.log.borrow_mut().push(format!("remove {target}"));
                    self.keys.borrow_mut().retain(|key| key.id != target);
                    output(0, String::new())
                }
                _ => output(1, String::new()),
            }
        }
    }
    fn neutralize(fake: &FakeRestic) -> Result<KeyLabelOutcome, RepoError> {
        neutralize_key_labels(
            fake,
            &destination(),
            "daily",
            "recovery",
            Path::new("/fixture/restic"),
            None,
        )
    }
    fn neutral(secret: &str) -> (String, String) {
        (NEUTRAL_KEY_LABEL.into(), secret.into())
    }

    #[test]
    fn machine_labelled_keys_are_replaced_one_secret_at_a_time() {
        let fake = FakeRestic::new(&[("a", "owner", "daily"), ("b", "owner", "recovery")]);
        assert_eq!(
            neutralize(&fake).unwrap(),
            KeyLabelOutcome {
                added: 2,
                removed: 2
            }
        );
        assert_eq!(fake.labels(), vec![neutral("daily"), neutral("recovery")]);
        assert_eq!(
            *fake.log.borrow(),
            vec![
                "add n0 for daily",
                "remove a",
                "add n1 for recovery",
                "remove b"
            ]
        );
        assert_eq!(
            *fake.add_env_tz.borrow(),
            vec![Some("UTC".to_owned()), Some("UTC".to_owned())]
        );
    }

    #[test]
    fn a_neutral_repository_costs_one_listing() {
        let fake = FakeRestic::new(&[
            ("a", NEUTRAL_KEY_LABEL, "daily"),
            ("b", NEUTRAL_KEY_LABEL, "recovery"),
        ]);
        assert_eq!(neutralize(&fake).unwrap(), KeyLabelOutcome::default());
        assert!(fake.log.borrow().is_empty());
        assert!(fake.add_env_tz.borrow().is_empty());
    }

    #[test]
    fn a_stopped_pass_resumes_with_its_neutral_key_instead_of_adding_another() {
        let fake = FakeRestic::new(&[
            ("a", "owner", "daily"),
            ("b", "owner", "recovery"),
            ("z", NEUTRAL_KEY_LABEL, "daily"),
        ]);
        assert_eq!(
            neutralize(&fake).unwrap(),
            KeyLabelOutcome {
                added: 1,
                removed: 2
            }
        );
        assert_eq!(fake.labels(), vec![neutral("daily"), neutral("recovery")]);
    }

    #[test]
    fn a_key_neither_secret_opens_is_left_in_place() {
        let fake = FakeRestic::new(&[
            ("a", "owner", "daily"),
            ("b", "owner", "recovery"),
            ("c", "owner", "someone-else"),
        ]);
        neutralize(&fake).unwrap();
        assert_eq!(
            fake.labels(),
            vec![
                ("owner".into(), "someone-else".into()),
                neutral("daily"),
                neutral("recovery"),
            ]
        );
    }

    #[test]
    fn a_failed_add_removes_nothing() {
        let mut fake = FakeRestic::new(&[("a", "owner", "daily"), ("b", "owner", "recovery")]);
        fake.add_returncode = 1;
        assert!(matches!(
            neutralize(&fake),
            Err(RepoError::Key(ResticKeyError { returncode: 1 }))
        ));
        assert_eq!(fake.keys.borrow().len(), 2);
        assert!(fake.log.borrow().is_empty());
    }

    #[test]
    fn a_new_key_that_does_not_open_with_its_secret_removes_nothing() {
        let mut fake = FakeRestic::new(&[("a", "owner", "daily"), ("b", "owner", "recovery")]);
        fake.add_opens_with = Some("wrong".into());
        assert!(neutralize(&fake).is_err());
        assert!(fake.keys.borrow().iter().any(|key| key.id == "a"));
        assert!(fake.keys.borrow().iter().any(|key| key.id == "b"));
        assert!(
            !fake
                .log
                .borrow()
                .iter()
                .any(|line| line.starts_with("remove"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn strict_key_list_absence_validates_json_strictly() {
        struct ScriptRunner(ToolOutput);
        impl ToolRunner for ScriptRunner {
            fn run(&self, _: &ToolRequest<'_>) -> io::Result<ToolOutput> {
                Ok(self.0.clone())
            }
        }

        let destination = Destination {
            repository: "repo".into(),
            backend: "s3".into(),
            credentials: serde_json::json!({"access_key_id":"fixture-access", "secret_access_key":"fixture-secret"}).as_object().unwrap().clone(),
        };
        let restic = Path::new("/fixture/bin/restic");

        for malformed in [
            "[]",
            r#"[{"id":"k1","current":false}]"#,
            r#"[{"id":"k1","current":true},{"id":"k2","current":true}]"#,
            r#"[{"id":"k1","current":"true"}]"#,
            r#"[{"id":"k1","current":null}]"#,
            r#"[{"id":"old","id":"daily","current":true}]"#,
            r#"[{"id":"k1","current":false,"current":true}]"#,
            r#"[{"id":"[redacted]abc","current":true}]"#,
            r#"[{"id":"fixture-accessabc","current":true}]"#,
        ] {
            let runner = ScriptRunner(ToolOutput {
                returncode: 0,
                stdout: malformed.as_bytes().to_vec(),
                stderr: vec![],
            });
            assert!(
                strict_key_list_absence(&runner, &destination, "pass", "target", restic, None)
                    .is_err()
            );
            assert!(capture_rotation_key_id(&runner, &destination, "pass", restic, None).is_err());
        }

        // 1. duplicate ids -> Err
        let runner = ScriptRunner(ToolOutput {
            returncode: 0,
            stdout: b"[{\"id\":\"k1\",\"current\":true},{\"id\":\"k1\",\"current\":false}]"
                .to_vec(),
            stderr: vec![],
        });
        assert!(matches!(
            strict_key_list_absence(&runner, &destination, "pass", "target", restic, None),
            Err(RepoError::Failed)
        ));

        // 2. truncated JSON -> Err
        let runner = ScriptRunner(ToolOutput {
            returncode: 0,
            stdout: b"[{\"id\":\"k1\",\"current\":true".to_vec(),
            stderr: vec![],
        });
        assert!(matches!(
            strict_key_list_absence(&runner, &destination, "pass", "target", restic, None),
            Err(RepoError::Failed)
        ));

        // 3. a non-array -> Err
        let runner = ScriptRunner(ToolOutput {
            returncode: 0,
            stdout: b"{\"id\":\"k1\",\"current\":true}".to_vec(),
            stderr: vec![],
        });
        assert!(matches!(
            strict_key_list_absence(&runner, &destination, "pass", "target", restic, None),
            Err(RepoError::Failed)
        ));

        // 4. element missing id -> Err
        let runner = ScriptRunner(ToolOutput {
            returncode: 0,
            stdout: b"[{\"current\":true}]".to_vec(),
            stderr: vec![],
        });
        assert!(matches!(
            strict_key_list_absence(&runner, &destination, "pass", "target", restic, None),
            Err(RepoError::Failed)
        ));

        // 5. element missing boolean current -> Err
        let runner = ScriptRunner(ToolOutput {
            returncode: 0,
            stdout: b"[{\"id\":\"k1\"}]".to_vec(),
            stderr: vec![],
        });
        assert!(matches!(
            strict_key_list_absence(&runner, &destination, "pass", "target", restic, None),
            Err(RepoError::Failed)
        ));

        // 6. empty id -> Err
        let runner = ScriptRunner(ToolOutput {
            returncode: 0,
            stdout: b"[{\"id\":\"\",\"current\":true}]".to_vec(),
            stderr: vec![],
        });
        assert!(matches!(
            strict_key_list_absence(&runner, &destination, "pass", "target", restic, None),
            Err(RepoError::Failed)
        ));

        // 7. NDJSON of two values -> Err
        let runner = ScriptRunner(ToolOutput {
            returncode: 0,
            stdout: b"{\"id\":\"k1\",\"current\":true}\n{\"id\":\"k2\",\"current\":false}".to_vec(),
            stderr: vec![],
        });
        assert!(matches!(
            strict_key_list_absence(&runner, &destination, "pass", "target", restic, None),
            Err(RepoError::Failed)
        ));

        // 8. Unique array that omits target -> Ok(true)
        let runner = ScriptRunner(ToolOutput {
            returncode: 0,
            stdout: b"[{\"id\":\"k1\",\"current\":true},{\"id\":\"k2\",\"current\":false}]"
                .to_vec(),
            stderr: vec![],
        });
        assert!(
            strict_key_list_absence(&runner, &destination, "pass", "target", restic, None).unwrap()
        );

        // 9. Unique array that contains target -> Ok(false)
        let runner = ScriptRunner(ToolOutput {
            returncode: 0,
            stdout: b"[{\"id\":\"k1\",\"current\":true},{\"id\":\"target\",\"current\":false}]"
                .to_vec(),
            stderr: vec![],
        });
        assert!(
            !strict_key_list_absence(&runner, &destination, "pass", "target", restic, None)
                .unwrap()
        );
    }
    #[test]
    fn public_key_helpers_preserve_run_only_desktop_contract() {
        use std::cell::RefCell;
        use std::collections::VecDeque;
        struct RunOnly {
            outputs: RefCell<VecDeque<ToolOutput>>,
            commands: RefCell<Vec<Vec<String>>>,
        }
        impl ToolRunner for RunOnly {
            fn run(&self, request: &ToolRequest<'_>) -> io::Result<ToolOutput> {
                self.commands.borrow_mut().push(
                    request
                        .argv
                        .iter()
                        .map(|arg| arg.to_string_lossy().into_owned())
                        .collect(),
                );
                Ok(self
                    .outputs
                    .borrow_mut()
                    .pop_front()
                    .expect("maintained run only"))
            }
        }
        let output = |code, bytes: &[u8]| ToolOutput {
            returncode: code,
            stdout: bytes.to_vec(),
            stderr: vec![],
        };
        let runner = RunOnly {
            outputs: RefCell::new(VecDeque::from([
                output(0, br#"[{"id":"original","current":true},{"id":"other"}]"#),
                output(0, b""),
                output(12, b""),
            ])),
            commands: RefCell::new(vec![]),
        };
        let destination = Destination {
            repository: "repo".into(),
            backend: "s3".into(),
            credentials: serde_json::json!({"access_key_id":"access","secret_access_key":"secret"})
                .as_object()
                .unwrap()
                .clone(),
        };
        let path = Path::new("/fixture/bin/restic");
        assert_eq!(
            capture_current_key_id(&runner, &destination, "password", path, None).unwrap(),
            "original"
        );
        remove_key(&runner, &destination, "password", "original", path, None).unwrap();
        assert!(matches!(
            remove_key(&runner, &destination, "password", "original", path, None),
            Err(RepoError::Key(ResticKeyError { returncode: 12 }))
        ));
        assert_eq!(
            *runner.commands.borrow(),
            [
                vec!["key", "list", "--json"],
                vec!["key", "remove", "original"],
                vec!["key", "remove", "original"]
            ]
        );
        assert!(runner.outputs.borrow().is_empty());
    }
}
