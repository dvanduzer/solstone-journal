// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Pre-clear recovery record for whole-backup teardown.
//!
//! Staged before configuration clear, updated across teardown stages, and removed
//! only after confirmed clear, binding release/own-finish, carrier retirement, and
//! destination secret retirement.

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use solstone_core_journal_config_write::DirectoryFlush;
use solstone_core_journal_io::JournalRoot;
#[cfg(unix)]
use solstone_core_journal_io::{
    BoundAtomicOutcome, ClaimName, ClaimRemovalOutcome, FlatDirectory, FlatDirectoryError,
    atomic_replace_bound, claim_and_remove_observed, create_or_open_flat_directory_bound,
    read_observed_file_bounded,
};

use crate::BackupError;

pub const TEARDOWN_RECOVERY_VERSION: u32 = 1;
pub const TEARDOWN_RECOVERY_FILENAME: &str = "teardown-recovery.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PruneObligation {
    None,
    Admitted,
    Reconciled,
}

mod flush_serde {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(value: &Option<DirectoryFlush>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(DirectoryFlush::Synced) => serializer.serialize_some("synced"),
            Some(DirectoryFlush::Failed) => serializer.serialize_some("failed"),
            Some(DirectoryFlush::Unavailable) => serializer.serialize_some("unavailable"),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<DirectoryFlush>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt: Option<String> = Option::deserialize(deserializer)?;
        match opt.as_deref() {
            Some("synced") => Ok(Some(DirectoryFlush::Synced)),
            Some("failed") => Ok(Some(DirectoryFlush::Failed)),
            Some("unavailable") => Ok(Some(DirectoryFlush::Unavailable)),
            Some(other) => Err(serde::de::Error::custom(format!(
                "unknown directory flush: {other}"
            ))),
            None => Ok(None),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeardownRecoveryRecord {
    pub version: u32,
    pub original_config_sha256: String,
    pub mode: String,

    #[serde(skip_serializing_if = "Option::is_none", default, with = "flush_serde")]
    pub directory_flush: Option<DirectoryFlush>,

    // Captured binding / destination identity (non-secret)
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bucket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub repository_locator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub region: Option<String>,

    // Secret fields, stored only while a later phase needs them
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub daily_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub recovery_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub broker_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub s3_secret: Option<String>,

    pub admitted_snapshot_ids: Vec<String>,
    pub prune_obligation: PruneObligation,
    pub clear_confirmed: bool,
    pub binding_released: bool,
    pub carrier_retired: bool,
    pub destination_secret_retired: bool,
}

impl TeardownRecoveryRecord {
    pub fn new(
        original_config_sha256: String,
        mode: String,
        admitted_snapshot_ids: Vec<String>,
        prune_obligation: PruneObligation,
    ) -> Self {
        Self {
            version: TEARDOWN_RECOVERY_VERSION,
            original_config_sha256,
            mode,
            directory_flush: None,
            account_id: None,
            instance_id: None,
            bucket: None,
            prefix: None,
            repository_locator: None,
            endpoint: None,
            region: None,
            daily_key: None,
            recovery_key: None,
            broker_token: None,
            s3_secret: None,
            admitted_snapshot_ids,
            prune_obligation,
            clear_confirmed: false,
            binding_released: false,
            carrier_retired: false,
            destination_secret_retired: false,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.clear_confirmed
            && self.binding_released
            && self.carrier_retired
            && self.destination_secret_retired
    }
}

impl fmt::Debug for TeardownRecoveryRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TeardownRecoveryRecord")
            .field("version", &self.version)
            .field("original_config_sha256", &self.original_config_sha256)
            .field("mode", &self.mode)
            .field("directory_flush", &self.directory_flush)
            .field("account_id", &self.account_id)
            .field("instance_id", &self.instance_id)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("repository_locator", &self.repository_locator)
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("daily_key", &self.daily_key.as_ref().map(|_| "<redacted>"))
            .field(
                "recovery_key",
                &self.recovery_key.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "broker_token",
                &self.broker_token.as_ref().map(|_| "<redacted>"),
            )
            .field("s3_secret", &self.s3_secret.as_ref().map(|_| "<redacted>"))
            .field("admitted_snapshot_ids", &self.admitted_snapshot_ids)
            .field("prune_obligation", &self.prune_obligation)
            .field("clear_confirmed", &self.clear_confirmed)
            .field("binding_released", &self.binding_released)
            .field("carrier_retired", &self.carrier_retired)
            .field(
                "destination_secret_retired",
                &self.destination_secret_retired,
            )
            .finish()
    }
}

impl fmt::Display for TeardownRecoveryRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "TeardownRecoveryRecord(sha256={}, mode={}, clear={}, binding={}, carrier={}, dest_secret={})",
            self.original_config_sha256,
            self.mode,
            self.clear_confirmed,
            self.binding_released,
            self.carrier_retired,
            self.destination_secret_retired
        )
    }
}

pub fn teardown_recovery_path(journal: &Path) -> PathBuf {
    journal.join("backup").join(TEARDOWN_RECOVERY_FILENAME)
}

#[cfg(unix)]
static CLAIM_COUNTER: AtomicU64 = AtomicU64::new(1);

pub fn load_teardown_recovery_at_root(
    root: &JournalRoot,
) -> Result<Option<TeardownRecoveryRecord>, BackupError> {
    #[cfg(unix)]
    {
        root.revalidate_canonical_binding()
            .map_err(|_| BackupError::TeardownRecoveryRead)?;
        let directory = match FlatDirectory::open(root, Path::new("backup")) {
            Ok(dir) => dir,
            Err(FlatDirectoryError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(None);
            }
            Err(_) => return Err(BackupError::TeardownRecoveryRead),
        };
        let observation = match read_observed_file_bounded(
            &directory,
            OsStr::new(TEARDOWN_RECOVERY_FILENAME),
            1024 * 1024,
        ) {
            Ok(obs) => obs,
            Err(_) => return Err(BackupError::TeardownRecoveryRead),
        };
        let Some(observation) = observation else {
            return Ok(None);
        };
        let record: TeardownRecoveryRecord = serde_json::from_slice(&observation.bytes)
            .map_err(|_| BackupError::TeardownRecoveryCorrupt)?;
        if record.version != TEARDOWN_RECOVERY_VERSION {
            return Err(BackupError::TeardownRecoveryCorrupt);
        }
        Ok(Some(record))
    }
    #[cfg(windows)]
    {
        let _ = root;
        Err(BackupError::TeardownRecoveryRead)
    }
}

pub fn stage_teardown_recovery_at_root(
    root: &JournalRoot,
    record: &TeardownRecoveryRecord,
) -> Result<(), BackupError> {
    #[cfg(unix)]
    {
        root.revalidate_canonical_binding()
            .map_err(|_| BackupError::TeardownRecoveryWrite)?;
        let directory = create_or_open_flat_directory_bound(
            root,
            OsStr::new("backup"),
            0o700,
            root.canonical_path(),
        )
        .map_err(|_| BackupError::TeardownRecoveryWrite)?;
        let bytes = serde_json::to_vec(record).map_err(|_| BackupError::TeardownRecoveryWrite)?;
        let outcome = atomic_replace_bound(
            &directory,
            OsStr::new(TEARDOWN_RECOVERY_FILENAME),
            &bytes,
            0o600,
        )
        .map_err(|_| BackupError::TeardownRecoveryWrite)?;
        match outcome {
            BoundAtomicOutcome::Published { .. } => {}
            BoundAtomicOutcome::PublishedDurabilityUncertain { .. }
            | BoundAtomicOutcome::PublishedObservationUncertain { .. } => {
                return Err(BackupError::TeardownRecoveryWrite);
            }
        }
        directory
            .revalidate()
            .map_err(|_| BackupError::TeardownRecoveryWrite)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = (root, record);
        Err(BackupError::TeardownRecoveryWrite)
    }
}

pub fn update_teardown_recovery_at_root(
    root: &JournalRoot,
    record: &TeardownRecoveryRecord,
) -> Result<(), BackupError> {
    stage_teardown_recovery_at_root(root, record)
}

pub fn delete_teardown_recovery_at_root(root: &JournalRoot) -> Result<(), BackupError> {
    #[cfg(unix)]
    {
        root.revalidate_canonical_binding()
            .map_err(|_| BackupError::TeardownRecoveryDelete)?;
        let directory = match FlatDirectory::open(root, Path::new("backup")) {
            Ok(dir) => dir,
            Err(FlatDirectoryError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(());
            }
            Err(_) => return Err(BackupError::TeardownRecoveryDelete),
        };
        let observation = read_observed_file_bounded(
            &directory,
            OsStr::new(TEARDOWN_RECOVERY_FILENAME),
            1024 * 1024,
        )
        .map_err(|_| BackupError::TeardownRecoveryDelete)?;
        let Some(observation) = observation else {
            return Ok(());
        };
        let counter = CLAIM_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = format!(
            "!solstone-claim-{:08x}-{:016x}",
            std::process::id(),
            counter
        );
        let claim =
            ClaimName::parse(&candidate).map_err(|_| BackupError::TeardownRecoveryDelete)?;
        let outcome = claim_and_remove_observed(
            &directory,
            OsStr::new(TEARDOWN_RECOVERY_FILENAME),
            &observation,
            &claim,
        )
        .map_err(|_| BackupError::TeardownRecoveryDelete)?;
        if outcome != ClaimRemovalOutcome::Removed {
            return Err(BackupError::TeardownRecoveryDelete);
        }
        directory
            .revalidate()
            .map_err(|_| BackupError::TeardownRecoveryDelete)?;
        let second = read_observed_file_bounded(
            &directory,
            OsStr::new(TEARDOWN_RECOVERY_FILENAME),
            1024 * 1024,
        )
        .map_err(|_| BackupError::TeardownRecoveryDelete)?;
        if second.is_some() {
            return Err(BackupError::TeardownRecoveryDelete);
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = root;
        Err(BackupError::TeardownRecoveryDelete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn teardown_recovery_round_trip_at_root() {
        let temp_dir = tempfile::tempdir().unwrap();
        let journal_root = JournalRoot::open(temp_dir.path()).unwrap();

        assert_eq!(load_teardown_recovery_at_root(&journal_root).unwrap(), None);

        let mut record = TeardownRecoveryRecord::new(
            "sha256-original".into(),
            "byo".into(),
            vec!["snap1".into(), "snap2".into()],
            PruneObligation::Admitted,
        );
        record.repository_locator = Some("s3:bucket/prefix".into());
        record.daily_key = Some("secret-daily-key-xyz".into());
        record.directory_flush = Some(DirectoryFlush::Synced);
        assert!(!record.is_complete());

        stage_teardown_recovery_at_root(&journal_root, &record).unwrap();
        let loaded = load_teardown_recovery_at_root(&journal_root)
            .unwrap()
            .unwrap();
        assert_eq!(loaded, record);

        record.clear_confirmed = true;
        record.binding_released = true;
        update_teardown_recovery_at_root(&journal_root, &record).unwrap();
        let loaded = load_teardown_recovery_at_root(&journal_root)
            .unwrap()
            .unwrap();
        assert!(loaded.clear_confirmed);
        assert!(loaded.binding_released);
        assert!(!loaded.is_complete());

        record.carrier_retired = true;
        record.destination_secret_retired = true;
        assert!(record.is_complete());
        update_teardown_recovery_at_root(&journal_root, &record).unwrap();
        let loaded = load_teardown_recovery_at_root(&journal_root)
            .unwrap()
            .unwrap();
        assert!(loaded.is_complete());

        delete_teardown_recovery_at_root(&journal_root).unwrap();
        assert_eq!(load_teardown_recovery_at_root(&journal_root).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn stage_refuses_when_backup_is_symlink_to_foreign_dir() {
        let temp_journal = tempfile::tempdir().unwrap();
        let foreign = tempfile::tempdir().unwrap();
        let journal_root = JournalRoot::open(temp_journal.path()).unwrap();
        std::os::unix::fs::symlink(foreign.path(), temp_journal.path().join("backup")).unwrap();

        let record = TeardownRecoveryRecord::new(
            "sha256-original".into(),
            "byo".into(),
            vec![],
            PruneObligation::None,
        );
        let res = stage_teardown_recovery_at_root(&journal_root, &record);
        assert!(res.is_err());
        assert!(!foreign.path().join(TEARDOWN_RECOVERY_FILENAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn stage_refuses_when_journal_directory_moved_away() {
        let parent = tempfile::tempdir().unwrap();
        let orig_journal = parent.path().join("journal");
        fs::create_dir(&orig_journal).unwrap();
        let journal_root = JournalRoot::open(&orig_journal).unwrap();

        let moved = parent.path().join("journal-moved");
        fs::rename(&orig_journal, &moved).unwrap();
        fs::create_dir(&orig_journal).unwrap();

        let record = TeardownRecoveryRecord::new(
            "sha256-original".into(),
            "byo".into(),
            vec![],
            PruneObligation::None,
        );
        let res = stage_teardown_recovery_at_root(&journal_root, &record);
        assert!(res.is_err());
        assert!(
            !orig_journal
                .join("backup")
                .join(TEARDOWN_RECOVERY_FILENAME)
                .exists()
        );
        assert!(
            !moved
                .join("backup")
                .join(TEARDOWN_RECOVERY_FILENAME)
                .exists()
        );
    }

    #[test]
    fn teardown_recovery_corrupt_file_is_err() {
        let temp_dir = tempfile::tempdir().unwrap();
        let journal_root = JournalRoot::open(temp_dir.path()).unwrap();

        let path = teardown_recovery_path(journal_root.canonical_path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not valid json").unwrap();

        let result = load_teardown_recovery_at_root(&journal_root);
        assert!(result.is_err());
    }

    #[test]
    fn teardown_recovery_unknown_version_is_err() {
        let temp_dir = tempfile::tempdir().unwrap();
        let journal_root = JournalRoot::open(temp_dir.path()).unwrap();

        let path = teardown_recovery_path(journal_root.canonical_path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bad_version = serde_json::json!({
            "version": 999,
            "original_config_sha256": "abc",
            "mode": "byo",
            "admitted_snapshot_ids": [],
            "prune_obligation": "none",
            "clear_confirmed": false,
            "binding_released": false,
            "carrier_retired": false,
            "destination_secret_retired": false
        });
        fs::write(&path, serde_json::to_vec(&bad_version).unwrap()).unwrap();

        let result = load_teardown_recovery_at_root(&journal_root);
        assert!(result.is_err());
    }

    #[test]
    fn teardown_recovery_debug_redacts_secrets() {
        let mut record = TeardownRecoveryRecord::new(
            "sha256-original".into(),
            "operated".into(),
            vec![],
            PruneObligation::None,
        );
        record.daily_key = Some("super-secret-daily-key".into());
        record.recovery_key = Some("super-secret-recovery-key".into());
        record.broker_token = Some("super-secret-broker-token".into());
        record.s3_secret = Some("super-secret-s3-key".into());
        record.bucket = Some("my-bucket".into());

        let debug = format!("{record:?}");
        assert!(!debug.contains("super-secret-daily-key"));
        assert!(!debug.contains("super-secret-recovery-key"));
        assert!(!debug.contains("super-secret-broker-token"));
        assert!(!debug.contains("super-secret-s3-key"));
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("my-bucket"));

        let display = format!("{record}");
        assert!(!display.contains("super-secret"));
    }

    #[test]
    fn clear_pending_revocation_and_config_write_leave_recovery_bytes_unchanged() {
        use crate::{
            HostedBinding, PendingExecution, PendingKeyRevocation, backup_setup_fingerprint,
            candidate_binding, clear_pending_key_revocation_at_root, generate_and_store_keys,
            get_backup_config, save_hosted_binding, set_enabled, set_mode,
            stage_pending_key_revocation_at_root,
        };
        use std::sync::Arc;

        let temp = tempfile::tempdir().unwrap();
        set_mode(temp.path(), "operated").unwrap();
        let keys = generate_and_store_keys(temp.path()).unwrap();
        let original = HostedBinding {
            broker_endpoint: "https://broker.example.invalid".into(),
            account_id: "account".into(),
            instance_id: "instance".into(),
            bucket: "bucket".into(),
            prefix: "prefix".into(),
            broker_token: "token".into(),
        };
        save_hosted_binding(temp.path(), &original).unwrap();
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());

        let record = TeardownRecoveryRecord::new(
            "sha256-original".into(),
            "operated".into(),
            vec!["snap1".into()],
            PruneObligation::Admitted,
        );
        stage_teardown_recovery_at_root(&root, &record).unwrap();
        let recovery_path = teardown_recovery_path(root.canonical_path());
        let initial_bytes = fs::read(&recovery_path).unwrap();

        let candidate = crate::generate_recovery_key().unwrap();
        let setup =
            backup_setup_fingerprint(&get_backup_config(temp.path()).unwrap(), Some(&original))
                .unwrap();
        let pending = PendingKeyRevocation {
            original_key_id: "original-key".into(),
            candidate_binding: candidate_binding(&keys.daily_key, &candidate).unwrap(),
            execution: PendingExecution {
                mode: "operated".into(),
                repository: "s3:https://s3.example.invalid/bucket/prefix".into(),
                backend: "s3".into(),
                credential_binding: setup.clone(),
            },
            setup_fingerprint: Some(setup),
        };
        let sha = solstone_core_journal_config::read_journal_config(temp.path())
            .unwrap()
            .sha256
            .unwrap();
        stage_pending_key_revocation_at_root(&root, &sha, &candidate, &pending, Some(&original))
            .unwrap();

        let after_stage = fs::read(&recovery_path).unwrap();
        assert_eq!(initial_bytes, after_stage);

        let sha = solstone_core_journal_config::read_journal_config(temp.path())
            .unwrap()
            .sha256
            .unwrap();
        clear_pending_key_revocation_at_root(&root, &sha, &pending, Some(&original)).unwrap();

        let after_revocation_clear = fs::read(&recovery_path).unwrap();
        assert_eq!(initial_bytes, after_revocation_clear);

        // Normal backup config write (e.g. set_enabled)
        set_enabled(root.canonical_path(), true).unwrap();
        let after_config_write = fs::read(&recovery_path).unwrap();
        assert_eq!(initial_bytes, after_config_write);
    }

    #[test]
    fn load_and_delete_do_not_create_backup_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let journal_root = JournalRoot::open(temp_dir.path()).unwrap();

        let backup_dir = temp_dir.path().join("backup");
        assert!(!backup_dir.exists());

        assert_eq!(load_teardown_recovery_at_root(&journal_root).unwrap(), None);
        assert!(!backup_dir.exists(), "load must not create backup/");

        let temp_dir2 = tempfile::tempdir().unwrap();
        let journal_root2 = JournalRoot::open(temp_dir2.path()).unwrap();
        let backup_dir2 = temp_dir2.path().join("backup");
        assert!(!backup_dir2.exists());

        assert!(delete_teardown_recovery_at_root(&journal_root2).is_ok());
        assert!(!backup_dir2.exists(), "delete must not create backup/");
    }
}
