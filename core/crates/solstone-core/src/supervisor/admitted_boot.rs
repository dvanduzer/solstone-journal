// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Admitted boot record observation and post-readiness asset refresh.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use solstone_core_journal_io::{AtomicWriteOptions, atomic_replace};
use solstone_core_local::install::coreml_install::CoremlHeal;

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
    let (os_name, arch) =
        solstone_core_assets::canonical_host_pair(std::env::consts::OS, std::env::consts::ARCH);
    refresh_installed_assets_with_platform(journal, (os_name, arch), || {
        let home_var = match std::env::var_os("HOME") {
            Some(var) => var,
            None => {
                log::warn!(
                    "asset refresh: HOME environment variable is not set; skipping CoreML refresh"
                );
                return None;
            }
        };
        let home_path = PathBuf::from(home_var);
        if !home_path.is_absolute() {
            log::warn!(
                "asset refresh: HOME environment variable is not absolute; skipping CoreML refresh"
            );
            return None;
        }

        let config = match solstone_core_journal_config::read_journal_config(journal) {
            Ok(config) => config,
            Err(error) => {
                log::error!("asset refresh: could not read journal config: {error}");
                return None;
            }
        };
        Some((home_path, config))
    });
}

pub(crate) fn refresh_installed_assets_with_platform(
    journal: &Path,
    platform: (&str, &str),
    resolve_home_and_config: impl FnOnce() -> Option<(
        PathBuf,
        solstone_core_journal_config::JournalConfigRead,
    )>,
) {
    if let Err(error) = solstone_core_location::install::refresh_installed(journal) {
        log::error!("location accuracy refresh failed: {error}");
    }

    let (os_name, arch) = platform;
    if !(os_name == "darwin" && arch == "arm64") {
        return;
    }

    let Some((home_path, config)) = resolve_home_and_config() else {
        return;
    };

    if let Err(error) =
        solstone_core_local::install::coreml_install::refresh_installed_parakeet_coreml(
            &home_path, &config,
        )
    {
        log::error!("parakeet coreml refresh failed: {error}");
    }

    heal_parakeet_coreml_until_settled(
        || {
            solstone_core_local::install::coreml_install::heal_missing_parakeet_coreml(
                journal, &home_path, &config,
            )
        },
        std::thread::sleep,
    );
}

/// Waits between self-heal passes, the same steps as the linux provider follow.
const COREML_HEAL_WAITS: [Duration; 4] = [
    Duration::from_secs(30 * 60),
    Duration::from_secs(2 * 60 * 60),
    Duration::from_secs(8 * 60 * 60),
    Duration::from_secs(24 * 60 * 60),
];

/// Parakeet is the default speech recognizer on macOS too, so a missing CoreML
/// model is installed rather than left for the owner to notice. Transcription
/// defers its audio until then. Retries until installed or the owner cancels.
fn heal_parakeet_coreml_until_settled(
    mut heal: impl FnMut() -> Result<CoremlHeal, String>,
    mut sleep: impl FnMut(Duration),
) {
    for attempt in 0.. {
        match heal() {
            Ok(CoremlHeal::AlreadyInstalled) => return,
            Ok(CoremlHeal::Installed) => {
                log::warn!("parakeet coreml model was not installed; installed it");
                return;
            }
            Ok(CoremlHeal::OwnerCancelled) => {
                log::warn!(
                    "parakeet coreml model is not installed; the owner cancelled its install"
                );
                return;
            }
            Ok(CoremlHeal::CoolingDown | CoremlHeal::InstallRunning) => {}
            Ok(CoremlHeal::Failed(error)) => {
                log::warn!("parakeet coreml model is not installed; its install failed: {error}");
            }
            Err(error) => log::warn!("parakeet coreml self-heal could not run: {error}"),
        }
        sleep(COREML_HEAL_WAITS[attempt.min(COREML_HEAL_WAITS.len() - 1)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    #[test]
    fn finish_admitted_start_returns_while_refresh_is_still_held() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (returned_tx, returned_rx) = mpsc::channel();
        let order = Arc::new(AtomicUsize::new(0));
        let caller_order = Arc::clone(&order);
        let caller = std::thread::spawn(move || {
            let ready_order = Arc::clone(&caller_order);
            let record_order = Arc::clone(&caller_order);
            let refresh_order = Arc::clone(&caller_order);
            let result = finish_admitted_start(
                || {
                    assert_eq!(caller_order.fetch_add(1, Ordering::SeqCst), 0);
                    Ok::<(), &'static str>(())
                },
                || {
                    assert_eq!(ready_order.fetch_add(1, Ordering::SeqCst), 1);
                },
                || {
                    assert_eq!(record_order.fetch_add(1, Ordering::SeqCst), 2);
                    Ok(BootClass::First)
                },
                move || {
                    assert_eq!(refresh_order.load(Ordering::SeqCst), 3);
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                },
            );
            returned_tx.send(result).unwrap();
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        // Release even a broken spawn-and-join implementation before asserting,
        // so the negative control fails instead of stranding the test process.
        let returned_while_held = returned_rx.recv_timeout(std::time::Duration::from_secs(5));
        release_tx.send(()).unwrap();
        caller.join().unwrap();
        assert_eq!(returned_while_held.unwrap(), Ok(()));
        assert_eq!(order.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn refresh_installed_assets_skips_home_and_config_on_non_darwin_arm64() {
        let temp = tempfile::tempdir().unwrap();
        let journal = temp.path();

        refresh_installed_assets_with_platform(journal, ("linux", "x86_64"), || {
            panic!("home and config closure must not be called on linux");
        });

        refresh_installed_assets_with_platform(journal, ("windows", "x86_64"), || {
            panic!("home and config closure must not be called on windows");
        });

        let called = Arc::new(AtomicUsize::new(0));
        let called_clone = Arc::clone(&called);
        refresh_installed_assets_with_platform(journal, ("darwin", "arm64"), move || {
            called_clone.fetch_add(1, Ordering::SeqCst);
            None
        });
        assert_eq!(called.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn coreml_heal_retries_on_the_follow_steps_until_settled() {
        let mut outcomes = vec![
            Ok(CoremlHeal::Installed),
            Ok(CoremlHeal::CoolingDown),
            Err("journal unreadable".to_owned()),
            Ok(CoremlHeal::InstallRunning),
            Ok(CoremlHeal::Failed("network".to_owned())),
            Ok(CoremlHeal::Failed("network".to_owned())),
        ];
        let mut waits = Vec::new();
        heal_parakeet_coreml_until_settled(|| outcomes.pop().unwrap(), |wait| waits.push(wait));
        assert!(outcomes.is_empty());
        assert_eq!(
            waits,
            [
                COREML_HEAL_WAITS[0],
                COREML_HEAL_WAITS[1],
                COREML_HEAL_WAITS[2],
                COREML_HEAL_WAITS[3],
                COREML_HEAL_WAITS[3],
            ]
        );

        for settled in [CoremlHeal::AlreadyInstalled, CoremlHeal::OwnerCancelled] {
            let mut slept = false;
            heal_parakeet_coreml_until_settled(|| Ok(settled.clone()), |_| slept = true);
            assert!(!slept);
        }
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
