// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Exact archive bytes in an owned target. These values never authorize release.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

#[cfg(any(unix, test))]
use serde_json::Value;
use solstone_core_journal_io::JournalRoot;
#[cfg(unix)]
use solstone_core_journal_io::media::MediaDigest;
use solstone_core_journal_io::media::{Cleanup, OwnedMediaTarget, PreparedMedia};

use crate::Destination;
#[cfg(unix)]
use crate::archive_admission::ARCHIVE_LS_TIMEOUT_SECONDS;
use crate::archive_admission::{AdmissionError, ArchiveBudget, ArchiveConfiguration, Clock};
#[cfg(unix)]
use crate::bounded_child::{self, CompletedChild, Request};
#[cfg(unix)]
use solstone_core_installed_payload::CurrentInstalledMember;

#[cfg(unix)]
const RESTIC_MEMBER: &str = "lib/solstone-restic/restic";
#[cfg(unix)]
const SUPPORTED_RESTIC_VERSION: &str = "0.19.0";
#[cfg(unix)]
const MAX_QUERY_BYTES: usize = 1024 * 1024;

/// Captured inputs remain unverified until actual admission succeeds.
pub struct ReadAdmission<'a> {
    pub root: Arc<JournalRoot>,
    pub configuration: ArchiveConfiguration,
    pub budget: ArchiveBudget,
    pub clock: &'a dyn Clock,
    pub destination: Destination,
    pub password: String,
    pub repository_id: String,
    pub expected_restic_path: PathBuf,
}

pub struct ExactArchiveFile {
    pub repository_id: String,
    pub snapshot_id: String,
    pub name: String,
    /// Exact snapshot spelling returned by restic; never normalize GUID paths.
    pub archive_path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadbackError {
    Admission(AdmissionError),
    Unsupported,
    NotConfigured,
    ToolUnavailable,
    ToolChanged,
    UnsupportedToolVersion,
    UnsupportedBackend,
    CredentialsUnavailable,
    InvalidSelection,
    RepositoryIdentityChanged,
    InvalidRepositoryIdentity,
    SnapshotNodeUnavailable,
    ToolFailed,
    NativeFailure,
    TargetRootChanged,
}

impl From<AdmissionError> for ReadbackError {
    fn from(value: AdmissionError) -> Self {
        Self::Admission(value)
    }
}

impl fmt::Display for ReadbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Admission(error) => error.fmt(f),
            _ => f.write_str(match self {
                Self::Unsupported => "reading archived files is unavailable on this platform",
                Self::NotConfigured => "reading archived files is not configured",
                Self::ToolUnavailable => "the installed archive tool is unavailable",
                Self::ToolChanged => "the installed archive tool changed; try again",
                Self::UnsupportedToolVersion => "the installed archive tool version is unsupported",
                Self::UnsupportedBackend => {
                    "this backup destination does not support reading archived files"
                }
                Self::CredentialsUnavailable => "archive read credentials are unavailable",
                Self::InvalidSelection => "the selected archive file is invalid",
                Self::RepositoryIdentityChanged => "the backup no longer matches",
                Self::InvalidRepositoryIdentity => "the backup could not be identified",
                Self::SnapshotNodeUnavailable => "the archived file could not be confirmed",
                Self::ToolFailed => "the archive read command failed",
                Self::NativeFailure => "the archived file could not be prepared",
                Self::TargetRootChanged => "the file could not be prepared in your journal",
                Self::Admission(_) => unreachable!(),
            }),
        }
    }
}
impl std::error::Error for ReadbackError {}

pub type ReadbackAdmissionError = ReadbackError;

pub struct ReadOperation<'a> {
    root: Arc<JournalRoot>,
    #[cfg(unix)]
    configuration: ArchiveConfiguration,
    #[cfg(unix)]
    budget: ArchiveBudget,
    #[cfg(unix)]
    clock: &'a dyn Clock,
    #[cfg(not(unix))]
    clock: std::marker::PhantomData<&'a dyn Clock>,
    #[cfg(unix)]
    member: CurrentInstalledMember,
    #[cfg(unix)]
    env: std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    repository_id: String,
}

#[derive(Clone, Copy)]
enum WriterState {
    NotLaunched,
    #[cfg(unix)]
    Unknown,
    #[cfg(unix)]
    QualifiedCompletion,
}

enum OwnedReadback {
    Target(OwnedMediaTarget),
    #[cfg(unix)]
    Prepared(PreparedMedia),
}

pub struct ReadbackFailure {
    reason: ReadbackError,
    writer: WriterState,
    owned: Box<OwnedReadback>,
}

impl fmt::Debug for ReadbackFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadbackFailure")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}
impl fmt::Display for ReadbackFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.reason.fmt(f)
    }
}
impl std::error::Error for ReadbackFailure {}

impl ReadbackFailure {
    pub fn reason(&self) -> ReadbackError {
        self.reason
    }

    /// Unknown writer lifetime never grants cleanup, including on Drop.
    pub fn cleanup(&mut self) -> Cleanup {
        match (self.owned.as_mut(), self.writer) {
            #[cfg(unix)]
            (_, WriterState::Unknown) => Cleanup::Refused,
            (OwnedReadback::Target(target), _) => target.cleanup_after_helper_quiescence(),
            #[cfg(unix)]
            (OwnedReadback::Prepared(prepared), _) => prepared.cleanup(),
        }
    }

    fn target(reason: ReadbackError, writer: WriterState, target: OwnedMediaTarget) -> Self {
        Self {
            reason,
            writer,
            owned: Box::new(OwnedReadback::Target(target)),
        }
    }
}

pub fn full_hex_identity(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn plain_name(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value
            .chars()
            .any(|ch| ch.is_control() || ch == '/' || ch == '\\')
}

/// Validate exact restic spelling without normalization, including its narrow
/// Windows snapshot-volume first component.
pub fn exact_archive_path(value: &str) -> bool {
    let Some(path) = value.strip_prefix('/') else {
        return false;
    };
    let mut components = path.split('/');
    let Some(first) = components.next() else {
        return false;
    };
    if snapshot_volume(first) {
        let Some(next) = components.next() else {
            return false;
        };
        plain_name(next) && components.all(plain_name)
    } else {
        plain_name(first) && components.all(plain_name)
    }
}

fn snapshot_volume(value: &str) -> bool {
    let Some(guid) = value
        .strip_prefix(r"\\?\Volume{")
        .and_then(|guid| guid.strip_suffix('}'))
    else {
        return false;
    };
    guid.len() == 36
        && guid.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

impl ExactArchiveFile {
    pub fn validate(&self) -> Result<(), ReadbackError> {
        if !full_hex_identity(&self.repository_id)
            || !full_hex_identity(&self.snapshot_id)
            || !full_hex_identity(&self.sha256)
            || !plain_name(&self.name)
            || !exact_archive_path(&self.archive_path)
            || self.archive_path.rsplit('/').next() != Some(self.name.as_str())
        {
            return Err(ReadbackError::InvalidSelection);
        }
        Ok(())
    }
}

#[cfg(unix)]
fn direct_environment(
    destination: &Destination,
    password: &str,
) -> Result<std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>, ReadbackError> {
    // restic 0.19.0 cmd_dump.go creates the target in the root process;
    // internal/dump/common.go joins its in-process writers before returning.
    // The direct S3/B2 backends do not launch an external transport. The only
    // password-command execution in internal/global is excluded by this fixed
    // environment and argv. This restriction does not qualify arbitrary tools.
    let scheme = match destination.backend.as_str() {
        "s3" => "s3:",
        "b2" => "b2:",
        _ => return Err(ReadbackError::UnsupportedBackend),
    };
    if !destination.repository.starts_with(scheme)
        || destination.repository[scheme.len()..].trim().is_empty()
        || destination.repository.chars().any(char::is_control)
    {
        return Err(ReadbackError::UnsupportedBackend);
    }
    if password.is_empty() {
        return Err(ReadbackError::CredentialsUnavailable);
    }
    let backend = crate::assemble_backend_env(destination)
        .map_err(|_| ReadbackError::CredentialsUnavailable)?;
    if backend
        .values()
        .any(|value| value.as_str().is_none_or(|s| s.trim().is_empty()))
    {
        // Empty AWS credentials fall through to ambient credential providers.
        return Err(ReadbackError::CredentialsUnavailable);
    }
    let mut env = std::collections::BTreeMap::from([
        (
            "RESTIC_REPOSITORY".into(),
            destination.repository.as_str().into(),
        ),
        ("RESTIC_PASSWORD".into(), password.into()),
    ]);
    for (key, value) in backend {
        env.insert(key.into(), value.as_str().expect("assembled string").into());
    }
    Ok(env)
}

impl<'a> ReadOperation<'a> {
    pub fn admit(input: ReadAdmission<'a>) -> Result<Self, ReadbackAdmissionError> {
        #[cfg(not(unix))]
        {
            let _ = input;
            Err(ReadbackError::Unsupported)
        }
        #[cfg(unix)]
        {
            input.budget.remaining(input.clock)?;
            input.configuration.revalidate(&input.root)?;
            if !input.configuration.enabled() {
                return Err(ReadbackError::NotConfigured);
            }
            let keys = input
                .configuration
                .keys()?
                .ok_or(ReadbackError::NotConfigured)?;
            if input.password != keys.daily_key {
                return Err(ReadbackError::CredentialsUnavailable);
            }
            match input.configuration.mode() {
                Some("byo")
                    if input.configuration.destination().as_ref() == Some(&input.destination) => {}
                Some("operated")
                    if input.configuration.binding().is_some()
                        && input.destination.backend == "s3" => {}
                _ => return Err(ReadbackError::NotConfigured),
            }
            if !full_hex_identity(&input.repository_id) {
                return Err(ReadbackError::InvalidRepositoryIdentity);
            }
            let env = direct_environment(&input.destination, &input.password)?;
            let member = solstone_core_installed_payload::resolve_current_member(RESTIC_MEMBER)
                .map_err(|_| ReadbackError::ToolUnavailable)?;
            if member.path() != input.expected_restic_path {
                return Err(ReadbackError::ToolChanged);
            }
            let admitted = Self {
                root: input.root,
                configuration: input.configuration,
                budget: input.budget,
                clock: input.clock,
                member,
                env,
                repository_id: input.repository_id,
            };
            let version = admitted.query(vec!["version".into()])?;
            let version = std::str::from_utf8(&version.output().stdout)
                .map_err(|_| ReadbackError::UnsupportedToolVersion)?;
            let mut fields = version.split_whitespace();
            if fields.next() != Some("restic") || fields.next() != Some(SUPPORTED_RESTIC_VERSION) {
                return Err(ReadbackError::UnsupportedToolVersion);
            }
            admitted.check_repository()?;
            Ok(admitted)
        }
    }

    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        &self.root
    }
    pub fn repository_id(&self) -> &str {
        &self.repository_id
    }

    #[cfg(unix)]
    fn revalidate_local(&self) -> Result<(), ReadbackError> {
        self.budget.remaining(self.clock)?;
        self.root
            .revalidate()
            .map_err(|_| ReadbackError::Admission(AdmissionError::JournalIdentityChanged))?;
        self.configuration.revalidate(&self.root)?;
        #[cfg(unix)]
        self.member
            .revalidate()
            .map_err(|_| ReadbackError::ToolChanged)?;
        Ok(())
    }

    #[cfg(unix)]
    fn query(&self, argv: Vec<std::ffi::OsString>) -> Result<CompletedChild, ReadbackError> {
        self.revalidate_local()?;
        let timeout = self
            .budget
            .remaining(self.clock)?
            .min(Duration::from_secs(ARCHIVE_LS_TIMEOUT_SECONDS));
        let output = bounded_child::run_owned(
            &Request {
                program: self.member.path().as_os_str().to_owned(),
                argv,
                env: self.env.clone(),
                timeout: Some(timeout),
                pass_fds: vec![],
            },
            Some(MAX_QUERY_BYTES),
        )
        .map_err(|_| ReadbackError::ToolFailed)?;
        self.revalidate_local()?;
        if !output.successful() {
            return Err(ReadbackError::ToolFailed);
        }
        Ok(output)
    }

    #[cfg(unix)]
    fn check_repository(&self) -> Result<(), ReadbackError> {
        let result = self.query(vec!["cat".into(), "config".into(), "--no-cache".into()])?;
        let parsed: Value = serde_json::from_slice(&result.output().stdout)
            .map_err(|_| ReadbackError::InvalidRepositoryIdentity)?;
        let id = parsed
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| full_hex_identity(id))
            .ok_or(ReadbackError::InvalidRepositoryIdentity)?;
        if id != self.repository_id {
            return Err(ReadbackError::RepositoryIdentityChanged);
        }
        Ok(())
    }

    #[cfg(unix)]
    fn check_node(&self, file: &ExactArchiveFile) -> Result<(), ReadbackError> {
        let output = self.query(vec![
            "ls".into(),
            "--json".into(),
            file.snapshot_id.as_str().into(),
            file.archive_path.as_str().into(),
            "--no-cache".into(),
        ])?;
        exact_regular_node(&output.output().stdout, file)
    }

    pub fn read_exact(
        &mut self,
        file: &ExactArchiveFile,
        target: OwnedMediaTarget,
    ) -> Result<PreparedMedia, ReadbackFailure> {
        #[cfg(not(unix))]
        {
            let _ = file;
            Err(ReadbackFailure::target(
                ReadbackError::Unsupported,
                WriterState::NotLaunched,
                target,
            ))
        }
        #[cfg(unix)]
        {
            let mut target = target;
            // Issuance is refusal evidence only: another writer may still hold
            // this target even when this operation never launched a child.
            if target.helper_transport_issued() {
                return Err(ReadbackFailure::target(
                    ReadbackError::NativeFailure,
                    WriterState::Unknown,
                    target,
                ));
            }
            let mut writer = WriterState::NotLaunched;
            let preflight = (|| {
                file.validate()?;
                if file.repository_id != self.repository_id {
                    return Err(ReadbackError::RepositoryIdentityChanged);
                }
                if target.journal_root().identity() != self.root.identity() {
                    return Err(ReadbackError::TargetRootChanged);
                }
                target
                    .journal_root()
                    .revalidate_canonical_binding()
                    .map_err(|_| ReadbackError::TargetRootChanged)?;
                self.check_repository()?;
                self.check_node(file)?;
                Ok(())
            })();
            if let Err(reason) = preflight {
                return Err(ReadbackFailure::target(reason, writer, target));
            }
            let execution = (|| {
                self.revalidate_local()?;
                let timeout = self.budget.remaining(self.clock)?;
                let transport = target
                    .helper_transport()
                    .map_err(|_| ReadbackError::NativeFailure)?;
                let request = Request {
                    program: self.member.path().as_os_str().to_owned(),
                    argv: vec![
                        "dump".into(),
                        "--target".into(),
                        transport.target.into_os_string(),
                        file.snapshot_id.as_str().into(),
                        file.archive_path.as_str().into(),
                        "--no-cache".into(),
                    ],
                    env: self.env.clone(),
                    timeout: Some(timeout),
                    pass_fds: vec![transport.descriptor],
                };
                writer = WriterState::Unknown;
                let completed = bounded_child::run_owned(&request, Some(MAX_QUERY_BYTES))
                    .map_err(|_| ReadbackError::ToolFailed)?;
                if !completed.successful() {
                    return Err(ReadbackError::ToolFailed);
                }
                self.member
                    .revalidate()
                    .map_err(|_| ReadbackError::ToolChanged)?;
                // Qualification is restricted to this admitted direct restic
                // profile; killpg alone does not prove descendant cessation.
                writer = WriterState::QualifiedCompletion;
                self.check_repository()?;
                self.revalidate_local()?;
                Ok(())
            })();
            if let Err(reason) = execution {
                return Err(ReadbackFailure::target(reason, writer, target));
            }
            let expected = MediaDigest {
                size: file.bytes,
                sha256: file.sha256.clone(),
            };
            let prepared =
                target
                    .prepare_after_successful_helper(&expected)
                    .map_err(|failure| {
                        ReadbackFailure::target(ReadbackError::NativeFailure, writer, failure.owned)
                    })?;
            if let Err(reason) = self.revalidate_local() {
                return Err(ReadbackFailure {
                    reason,
                    writer,
                    owned: Box::new(OwnedReadback::Prepared(prepared)),
                });
            }
            Ok(prepared)
        }
    }
}

#[cfg(any(unix, test))]
fn exact_regular_node(bytes: &[u8], file: &ExactArchiveFile) -> Result<(), ReadbackError> {
    let mut snapshots = 0usize;
    let mut nodes = 0usize;
    for value in serde_json::Deserializer::from_slice(bytes).into_iter::<Value>() {
        let value = value.map_err(|_| ReadbackError::SnapshotNodeUnavailable)?;
        match value.get("message_type").and_then(Value::as_str) {
            Some("snapshot") => {
                snapshots += 1;
                if value.get("id").and_then(Value::as_str) != Some(file.snapshot_id.as_str()) {
                    return Err(ReadbackError::SnapshotNodeUnavailable);
                }
            }
            Some("node") => {
                // Equality is over the exact stored snapshot-native spelling.
                if value.get("path").and_then(Value::as_str) == Some(file.archive_path.as_str()) {
                    nodes += 1;
                    if value.get("type").and_then(Value::as_str) != Some("file")
                        || value.get("name").and_then(Value::as_str) != Some(file.name.as_str())
                        || value.get("size").and_then(Value::as_u64) != Some(file.bytes)
                    {
                        return Err(ReadbackError::SnapshotNodeUnavailable);
                    }
                }
            }
            _ => return Err(ReadbackError::SnapshotNodeUnavailable),
        }
    }
    if snapshots != 1 || nodes != 1 {
        return Err(ReadbackError::SnapshotNodeUnavailable);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn selected() -> ExactArchiveFile {
        ExactArchiveFile {
            repository_id: "a".repeat(64),
            snapshot_id: "b".repeat(64),
            name: "audio.flac".into(),
            archive_path: r"/\\?\Volume{08fa948d-3336-40fd-b369-dd44a1c89601}/stage/audio.flac"
                .into(),
            bytes: 7,
            sha256: "c".repeat(64),
        }
    }
    #[test]
    fn exact_node_requires_file_kind_snapshot_size_and_unmodified_path() {
        let file = selected();
        file.validate().unwrap();
        let snapshot = serde_json::json!({"message_type":"snapshot","id":file.snapshot_id});
        let node = serde_json::json!({"message_type":"node","path":file.archive_path,"name":file.name,"type":"file","size":7});
        let encode = |node: &Value| format!("{snapshot}\n{node}\n").into_bytes();
        exact_regular_node(&encode(&node), &file).unwrap();
        for (key, replacement) in [
            ("type", serde_json::json!("dir")),
            ("path", serde_json::json!("/stage/audio.flac")),
            ("size", serde_json::json!(8)),
        ] {
            let mut wrong = node.clone();
            wrong[key] = replacement;
            assert_eq!(
                exact_regular_node(&encode(&wrong), &file),
                Err(ReadbackError::SnapshotNodeUnavailable)
            );
        }
        assert_eq!(
            exact_regular_node(format!("{snapshot}\n{node}\n{node}").as_bytes(), &file),
            Err(ReadbackError::SnapshotNodeUnavailable)
        );
    }
    #[test]
    fn invalid_selector_refuses_instead_of_cleaning_it() {
        for path in [
            "/a/../audio.flac",
            "/a/./audio.flac",
            "relative/audio.flac",
            "/a/other.flac",
            "//a/audio.flac",
            r"/stage\\?\Volume{08fa948d-3336-40fd-b369-dd44a1c89601}/audio.flac",
            r"/\\?\Volume{08fa948d3336-40fd-b369-dd44a1c89601}/audio.flac",
            r"/\\server\share/audio.flac",
        ] {
            let mut file = selected();
            file.archive_path = path.into();
            assert_eq!(file.validate(), Err(ReadbackError::InvalidSelection));
        }
        let mut file = selected();
        file.snapshot_id = "latest".into();
        assert_eq!(file.validate(), Err(ReadbackError::InvalidSelection));
    }
    #[cfg(unix)]
    #[test]
    fn direct_profile_rejects_mismatched_scheme_empty_credentials_and_ambient_keys() {
        let mut destination = Destination {
            backend: "s3".into(),
            repository: "s3:https://example.invalid/bucket".into(),
            credentials: serde_json::from_value(
                serde_json::json!({"access_key_id":"access","secret_access_key":"secret"}),
            )
            .unwrap(),
        };
        let env = direct_environment(&destination, "password").unwrap();
        assert_eq!(env.len(), 4);
        assert!(!env.contains_key(std::ffi::OsStr::new("PATH")));
        for repository in ["sftp:host:path", "rclone:remote:path", "/local"] {
            destination.repository = repository.into();
            assert_eq!(
                direct_environment(&destination, "password"),
                Err(ReadbackError::UnsupportedBackend)
            );
        }
        destination.repository = "s3:https://example.invalid/bucket".into();
        destination
            .credentials
            .insert("access_key_id".into(), Value::String(String::new()));
        assert_eq!(
            direct_environment(&destination, "password"),
            Err(ReadbackError::CredentialsUnavailable)
        );
    }
}
