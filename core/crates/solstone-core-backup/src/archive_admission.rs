// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Retained archive configuration and one operation's nonrenewable budget.

use crate::HostedBinding;
use serde_json::{Map, Value};
use solstone_core_journal_io::JournalRoot;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const ARCHIVE_BACKUP_TIMEOUT_SECONDS: u64 = 6 * 60 * 60;
pub const ARCHIVE_LS_TIMEOUT_SECONDS: u64 = 30 * 60;

pub trait Clock {
    fn now_unix(&self) -> i64;
    fn iso_week(&self) -> u8;
}

/// A shared refusal signal. Cancellation is irreversible and grants no authority.
#[derive(Clone, Default)]
pub struct ArchiveCancellation {
    cancelled: Arc<AtomicBool>,
}

impl ArchiveCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionError {
    ConfigurationUnavailable,
    ConfigurationChanged,
    JournalIdentityChanged,
    DeadlineExceeded,
    ClockChanged,
    CredentialsExpired,
    Cancelled,
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ConfigurationUnavailable => "archive configuration could not be read",
            Self::ConfigurationChanged => "archive configuration changed",
            Self::JournalIdentityChanged => "your journal could not be confirmed",
            Self::DeadlineExceeded => "the archive operation deadline was reached",
            Self::ClockChanged => "the archive clock moved backward",
            Self::CredentialsExpired => "archive credentials expired",
            Self::Cancelled => "the archive operation was cancelled",
        })
    }
}
impl std::error::Error for AdmissionError {}

#[derive(PartialEq)]
pub struct ArchiveConfiguration {
    settings: Map<String, Value>,
    binding: Option<HostedBinding>,
}

const MAX_ARCHIVE_CONFIGURATION_BYTES: usize = 1024 * 1024;

impl ArchiveConfiguration {
    pub fn enabled(&self) -> bool {
        self.settings.get("enabled") == Some(&Value::Bool(true))
    }

    pub fn mode(&self) -> Option<&str> {
        self.settings.get("mode").and_then(Value::as_str)
    }

    pub fn keys(&self) -> Result<Option<crate::BackupKeys>, AdmissionError> {
        match (
            self.settings.get("daily_key"),
            self.settings.get("recovery_key"),
        ) {
            (Some(Value::Null) | None, _) | (_, Some(Value::Null) | None) => Ok(None),
            (Some(Value::String(daily_key)), Some(Value::String(recovery_key))) => {
                crate::format_recovery_key_display(recovery_key)
                    .map_err(|_| AdmissionError::ConfigurationUnavailable)?;
                Ok(Some(crate::BackupKeys {
                    daily_key: daily_key.clone(),
                    recovery_key: recovery_key.clone(),
                }))
            }
            _ => Err(AdmissionError::ConfigurationUnavailable),
        }
    }

    pub fn destination(&self) -> Option<crate::Destination> {
        let destination = self.settings.get("destination")?.as_object()?;
        let credentials = match destination.get("credentials") {
            None => Map::new(),
            Some(Value::Object(credentials)) => credentials.clone(),
            Some(_) => return None,
        };
        Some(crate::Destination {
            repository: destination.get("repository")?.as_str()?.to_owned(),
            backend: destination.get("backend")?.as_str()?.to_owned(),
            credentials,
        })
    }

    pub fn binding(&self) -> Option<&HostedBinding> {
        self.binding.as_ref()
    }

    pub fn revalidate(&self, root: &JournalRoot) -> Result<(), AdmissionError> {
        if Self::capture(root)? != *self {
            return Err(AdmissionError::ConfigurationChanged);
        }
        Ok(())
    }

    pub fn capture(root: &JournalRoot) -> Result<Self, AdmissionError> {
        root.revalidate_canonical_binding()
            .map_err(|_| AdmissionError::JournalIdentityChanged)?;
        let raw = Self::read_object(root, Path::new("config/journal.json"))?;
        let config = crate::merge_backup_config(&raw);
        let binding = if config.get("mode") == Some(&Value::String("operated".into())) {
            let raw = Self::read_object(root, Path::new("backup/hosted/binding.json"))?;
            let nonblank = |key| {
                raw.get(key)
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_owned)
                    .ok_or(AdmissionError::ConfigurationUnavailable)
            };
            Some(HostedBinding {
                broker_endpoint: nonblank("broker_endpoint")?,
                account_id: nonblank("account_id")?,
                instance_id: nonblank("instance_id")?,
                bucket: nonblank("bucket")?,
                prefix: nonblank("prefix")?,
                broker_token: nonblank("broker_token")?,
            })
        } else {
            None
        };
        root.revalidate_canonical_binding()
            .map_err(|_| AdmissionError::JournalIdentityChanged)?;
        // Status, schedules and accounting are not transport identity.
        let settings = [
            "enabled",
            "mode",
            "destination",
            "daily_key",
            "recovery_key",
        ]
        .into_iter()
        .filter_map(|key| {
            config
                .get(key)
                .cloned()
                .map(|value| (key.to_owned(), value))
        })
        .collect();
        Ok(Self { settings, binding })
    }

    fn read_object(
        root: &JournalRoot,
        relative: &Path,
    ) -> Result<Map<String, Value>, AdmissionError> {
        let observed = solstone_core_journal_io::read_relative_file_bounded(
            root,
            relative,
            MAX_ARCHIVE_CONFIGURATION_BYTES,
        )
        .map_err(|_| AdmissionError::ConfigurationUnavailable)?
        .ok_or(AdmissionError::ConfigurationUnavailable)?;
        serde_json::from_slice(&observed.bytes)
            .map_err(|_| AdmissionError::ConfigurationUnavailable)
    }
}

pub struct ArchiveBudget {
    deadline: i64,
    last_now: std::cell::Cell<Option<i64>>,
    credential_expiry: Option<i64>,
    cancellation: Option<ArchiveCancellation>,
}

impl ArchiveBudget {
    /// A later cap can shorten credentials, never extend their lifetime.
    pub fn cap_credential_expiry(&mut self, expiry: i64) {
        self.credential_expiry = Some(self.credential_expiry.map_or(expiry, |old| old.min(expiry)));
    }

    pub fn new(clock: &dyn Clock) -> Result<Self, AdmissionError> {
        Self::start(clock, None)
    }

    pub fn new_cancellable(
        clock: &dyn Clock,
        cancellation: ArchiveCancellation,
    ) -> Result<Self, AdmissionError> {
        Self::start(clock, Some(cancellation))
    }

    pub fn cancellation(&self) -> Option<&ArchiveCancellation> {
        self.cancellation.as_ref()
    }

    fn start(
        clock: &dyn Clock,
        cancellation: Option<ArchiveCancellation>,
    ) -> Result<Self, AdmissionError> {
        let now = clock.now_unix();
        let deadline = now
            .checked_add(ARCHIVE_BACKUP_TIMEOUT_SECONDS as i64)
            .ok_or(AdmissionError::DeadlineExceeded)?;
        Ok(Self {
            deadline,
            last_now: std::cell::Cell::new(Some(now)),
            credential_expiry: None,
            cancellation,
        })
    }

    pub fn remaining(&self, clock: &dyn Clock) -> Result<Duration, AdmissionError> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(ArchiveCancellation::is_cancelled)
        {
            return Err(AdmissionError::Cancelled);
        }
        let now = clock.now_unix();
        let last_now = self.last_now.get().ok_or(AdmissionError::ClockChanged)?;
        if now < last_now {
            self.last_now.set(None);
            return Err(AdmissionError::ClockChanged);
        }
        self.last_now.set(Some(now));
        let deadline = self
            .deadline
            .checked_sub(now)
            .filter(|remaining| *remaining > 0)
            .ok_or(AdmissionError::DeadlineExceeded)?;
        let remaining = match self.credential_expiry {
            Some(expiry) => deadline.min(
                expiry
                    .checked_sub(now)
                    .filter(|remaining| *remaining > 0)
                    .ok_or(AdmissionError::CredentialsExpired)?,
            ),
            None => deadline,
        };
        Ok(Duration::from_secs(remaining as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    struct TestClock(Cell<i64>);
    impl Clock for TestClock {
        fn now_unix(&self) -> i64 {
            self.0.get()
        }
        fn iso_week(&self) -> u8 {
            1
        }
    }
    #[test]
    fn cancellation_and_original_lifetime_survive_worker_budget_transfer() {
        fn worker_entry(budget: ArchiveBudget, now: i64) -> ArchiveBudget {
            let local_clock = TestClock(Cell::new(now));
            assert_eq!(
                budget.remaining(&local_clock).unwrap(),
                Duration::from_secs(1)
            );
            budget
        }
        let admission_clock = TestClock(Cell::new(100));
        let cancellation = ArchiveCancellation::new();
        let mut budget =
            ArchiveBudget::new_cancellable(&admission_clock, cancellation.clone()).unwrap();
        budget.cap_credential_expiry(100 + ARCHIVE_BACKUP_TIMEOUT_SECONDS as i64);
        budget.cap_credential_expiry(100 + 2 * ARCHIVE_BACKUP_TIMEOUT_SECONDS as i64);
        let near_deadline = 100 + ARCHIVE_BACKUP_TIMEOUT_SECONDS as i64 - 1;
        let transferred = worker_entry(budget, near_deadline);
        let worker_clock = TestClock(Cell::new(near_deadline + 1));
        assert_eq!(
            transferred.remaining(&worker_clock),
            Err(AdmissionError::DeadlineExceeded)
        );
        cancellation.cancel();
        assert!(transferred.cancellation().unwrap().is_cancelled());
        assert!(!ArchiveCancellation::new().is_cancelled());
        assert_eq!(
            transferred.remaining(&worker_clock),
            Err(AdmissionError::Cancelled)
        );
        let moved_again = transferred;
        assert_eq!(
            moved_again.remaining(&worker_clock),
            Err(AdmissionError::Cancelled)
        );
    }
    #[test]
    fn moved_budget_keeps_deadline_and_sticky_clock_refusal() {
        let clock = TestClock(Cell::new(100));
        let original = ArchiveBudget::new(&clock).unwrap();
        clock.0.set(100 + ARCHIVE_BACKUP_TIMEOUT_SECONDS as i64 - 1);
        assert_eq!(original.remaining(&clock).unwrap(), Duration::from_secs(1));
        let transferred = original;
        clock.0.set(100 + ARCHIVE_BACKUP_TIMEOUT_SECONDS as i64);
        assert_eq!(
            transferred.remaining(&clock),
            Err(AdmissionError::DeadlineExceeded)
        );
        let poisoned = ArchiveBudget::new(&clock).unwrap();
        clock.0.set(clock.0.get() - 1);
        assert_eq!(
            poisoned.remaining(&clock),
            Err(AdmissionError::ClockChanged)
        );
        let transferred = poisoned;
        clock.0.set(clock.0.get() + 2);
        assert_eq!(
            transferred.remaining(&clock),
            Err(AdmissionError::ClockChanged)
        );
    }
    #[test]
    fn expiry_cap_cannot_renew_a_session() {
        let clock = TestClock(Cell::new(10));
        let mut budget = ArchiveBudget::new(&clock).unwrap();
        budget.cap_credential_expiry(20);
        budget.cap_credential_expiry(30);
        assert_eq!(budget.remaining(&clock).unwrap(), Duration::from_secs(10));
        clock.0.set(20);
        assert_eq!(
            budget.remaining(&clock),
            Err(AdmissionError::CredentialsExpired)
        );
    }

    #[test]
    fn captured_transport_ignores_status_but_refuses_configuration_change() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("config")).unwrap();
        let path = directory.path().join("config/journal.json");
        let original = serde_json::json!({"backup": {
            "enabled": true, "mode": "byo", "daily_key": "key", "recovery_key": "0".repeat(64),
            "destination": {"backend":"s3", "repository":"s3:https://example.invalid/bucket", "credentials": {"access_key_id":"access", "secret_access_key":"secret"}}
        }});
        std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
        let root = JournalRoot::open(directory.path()).unwrap();
        let captured = ArchiveConfiguration::capture(&root).unwrap();
        let mut changed = original;
        changed["backup"]["last_backup"] = serde_json::json!({"status":"ok"});
        std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
        captured.revalidate(&root).unwrap();
        changed["backup"]["enabled"] = Value::Bool(false);
        std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert_eq!(
            captured.revalidate(&root),
            Err(AdmissionError::ConfigurationChanged)
        );
        std::fs::write(&path, vec![b' '; MAX_ARCHIVE_CONFIGURATION_BYTES + 1]).unwrap();
        assert_eq!(
            captured.revalidate(&root),
            Err(AdmissionError::ConfigurationUnavailable)
        );
    }

    #[cfg(unix)]
    #[test]
    fn bound_configuration_refuses_symlink_and_replaced_root() {
        let directory = tempfile::tempdir().unwrap();
        let journal = directory.path().join("journal");
        std::fs::create_dir_all(journal.join("config")).unwrap();
        let outside = directory.path().join("outside.json");
        std::fs::write(&outside, b"{}").unwrap();
        std::os::unix::fs::symlink(&outside, journal.join("config/journal.json")).unwrap();
        let root = JournalRoot::open(&journal).unwrap();
        assert!(matches!(
            ArchiveConfiguration::capture(&root),
            Err(AdmissionError::ConfigurationUnavailable)
        ));
        std::fs::remove_file(journal.join("config/journal.json")).unwrap();
        std::fs::write(journal.join("config/journal.json"), b"{}").unwrap();
        let captured = ArchiveConfiguration::capture(&root).unwrap();
        std::fs::rename(&journal, directory.path().join("retained")).unwrap();
        std::fs::create_dir(&journal).unwrap();
        assert_eq!(
            captured.revalidate(&root),
            Err(AdmissionError::JournalIdentityChanged)
        );
    }
}
