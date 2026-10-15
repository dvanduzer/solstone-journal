// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Admitted boot record observation and post-readiness asset refresh.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use solstone_core_journal_io::{AtomicWriteOptions, atomic_replace};

/// Result of observing and updating the admitted boot record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootClass {
    First,
    Changed,
    Same,
}

#[derive(Debug, Serialize, Deserialize)]
struct BootVersionRecord {
    schema: u32,
    product_version: String,
}

/// Observe and update the admitted boot version record under `health/boot-version.json`.
pub fn record_admitted_boot(journal: &Path) -> io::Result<BootClass> {
    let health_dir = journal.join("health");
    let path = health_dir.join("boot-version.json");
    let current_version = env!("CARGO_PKG_VERSION");

    let class = match fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<BootVersionRecord>(&bytes) {
            Ok(record) if record.schema == 1 => {
                if record.product_version == current_version {
                    BootClass::Same
                } else {
                    BootClass::Changed
                }
            }
            Ok(_) => {
                log::error!("health/boot-version.json has unexpected schema");
                BootClass::Changed
            }
            Err(error) => {
                log::error!("health/boot-version.json unreadable: {error}");
                BootClass::Changed
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => BootClass::First,
        Err(error) => {
            log::error!("health/boot-version.json read error: {error}");
            BootClass::Changed
        }
    };

    let payload = serde_json::json!({
        "schema": 1,
        "product_version": current_version,
    });
    let mut bytes = serde_json::to_vec_pretty(&payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    bytes.push(b'\n');

    atomic_replace(&path, &bytes, AtomicWriteOptions::default())
        .map_err(|error| io::Error::other(error.to_string()))?;

    Ok(class)
}

/// Orchestrate readiness signaling, queue readiness, boot version recording,
/// and post-ready asset refresh dispatch.
pub(crate) fn finish_admitted_start<E>(
    signal_ready: impl FnOnce() -> Result<(), E>,
    set_ready: impl FnOnce(),
    record: impl FnOnce() -> io::Result<BootClass>,
    refresh: impl FnOnce() + Send + 'static,
) -> Result<(), E> {
    signal_ready()?;
    set_ready();
    if let Err(error) = record() {
        log::error!("failed to record admitted boot version: {error}");
    }
    let spawned = std::thread::Builder::new()
        .name("asset-refresh".to_string())
        .spawn(refresh);
    if let Err(error) = spawned {
        log::error!("failed to spawn asset-refresh thread: {error}");
    }
    Ok(())
}

/// Background refresh routine run after supervisor readiness is admitted.
pub(crate) fn refresh_installed_assets(journal: &Path) {
    if let Err(error) = solstone_core_location::install::refresh_installed(journal) {
        log::error!("location accuracy refresh failed: {error}");
    }

    let Some(home_var) = std::env::var_os("HOME") else {
        log::warn!("asset refresh: HOME environment variable is not set; skipping CoreML refresh");
        return;
    };
    let home_path = PathBuf::from(home_var);
    if !home_path.is_absolute() {
        log::warn!(
            "asset refresh: HOME environment variable is not absolute; skipping CoreML refresh"
        );
        return;
    }

    let config = match solstone_core_journal_config::read_journal_config(journal) {
        Ok(config) => config,
        Err(error) => {
            log::error!("asset refresh: could not read journal config: {error}");
            return;
        }
    };

    if let Err(error) =
        solstone_core_local::install::coreml_install::refresh_installed_parakeet_coreml(
            &home_path, &config,
        )
    {
        log::error!("parakeet coreml refresh failed: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    #[test]
    fn finish_admitted_start_observes_readiness_before_refresh_unblocks() {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let set_ready_called = Arc::new(AtomicUsize::new(0));
        let set_ready_clone = Arc::clone(&set_ready_called);

        let handle = std::thread::spawn(move || {
            finish_admitted_start(
                || {
                    ready_tx.send(()).unwrap();
                    Ok::<(), &'static str>(())
                },
                || {
                    set_ready_clone.fetch_add(1, Ordering::SeqCst);
                },
                || Ok(BootClass::First),
                move || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                },
            )
            .unwrap();
        });

        ready_rx.recv().unwrap();
        entered_rx.recv().unwrap();
        assert_eq!(set_ready_called.load(Ordering::SeqCst), 1);
        release_tx.send(()).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn finish_admitted_start_fails_fast_when_signal_ready_fails() {
        let set_ready_called = Arc::new(AtomicUsize::new(0));
        let set_ready_clone = Arc::clone(&set_ready_called);
        let record_called = Arc::new(AtomicUsize::new(0));
        let record_clone = Arc::clone(&record_called);
        let refresh_called = Arc::new(AtomicUsize::new(0));
        let refresh_clone = Arc::clone(&refresh_called);

        let result = finish_admitted_start(
            || Err("signal failure"),
            || {
                set_ready_clone.fetch_add(1, Ordering::SeqCst);
            },
            move || {
                record_clone.fetch_add(1, Ordering::SeqCst);
                Ok(BootClass::First)
            },
            move || {
                refresh_clone.fetch_add(1, Ordering::SeqCst);
            },
        );

        assert_eq!(result.unwrap_err(), "signal failure");
        assert_eq!(set_ready_called.load(Ordering::SeqCst), 0);
        assert_eq!(record_called.load(Ordering::SeqCst), 0);
        assert_eq!(refresh_called.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn finish_admitted_start_continues_when_record_fails() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let set_ready_called = Arc::new(AtomicUsize::new(0));
        let set_ready_clone = Arc::clone(&set_ready_called);

        let result = finish_admitted_start(
            || Ok::<(), &'static str>(()),
            || {
                set_ready_clone.fetch_add(1, Ordering::SeqCst);
            },
            || Err(io::Error::other("injected record failure")),
            move || {
                entered_tx.send(()).unwrap();
            },
        );

        assert!(result.is_ok());
        assert_eq!(set_ready_called.load(Ordering::SeqCst), 1);
        entered_rx.recv().unwrap();
    }

    #[test]
    fn record_admitted_boot_lifecycle_and_transitions() {
        let temp = tempfile::tempdir().unwrap();
        let journal = temp.path();

        // 1. Absent file -> First
        let class = record_admitted_boot(journal).unwrap();
        assert_eq!(class, BootClass::First);

        let boot_file = journal.join("health/boot-version.json");
        let bytes = fs::read(&boot_file).unwrap();
        let record: BootVersionRecord = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record.schema, 1);
        assert_eq!(record.product_version, env!("CARGO_PKG_VERSION"));

        // 2. Same version -> Same
        let class = record_admitted_boot(journal).unwrap();
        assert_eq!(class, BootClass::Same);

        // 3. Different version -> Changed
        let modified = serde_json::json!({
            "schema": 1,
            "product_version": "0.0.0-older",
        });
        fs::write(&boot_file, serde_json::to_vec(&modified).unwrap()).unwrap();
        let class = record_admitted_boot(journal).unwrap();
        assert_eq!(class, BootClass::Changed);
        let bytes = fs::read(&boot_file).unwrap();
        let record: BootVersionRecord = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record.product_version, env!("CARGO_PKG_VERSION"));

        // 4. Garbage bytes -> Changed and rewritten
        fs::write(&boot_file, b"corrupted non-json bytes").unwrap();
        let class = record_admitted_boot(journal).unwrap();
        assert_eq!(class, BootClass::Changed);
        let bytes = fs::read(&boot_file).unwrap();
        let record: BootVersionRecord = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record.product_version, env!("CARGO_PKG_VERSION"));
    }
}
