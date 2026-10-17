// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::path::Path;

use solstone_core_sol_link::http::{
    SetupDownloadError, SetupDownloadTask, SetupDownloads, SetupHost,
};
use solstone_core_thinking_copy::SetupCatalogFacts;

pub(crate) struct ConveySetupHost;

impl SetupHost for ConveySetupHost {
    fn start_local(&self, journal_root: &Path) -> Result<(), String> {
        crate::thinking_install::start(journal_root, "local/qwen3.5-4b").map(|_| ())
    }

    fn cancel_local(&self, journal_root: &Path, attempt_id: &str) -> Result<(), String> {
        crate::thinking_install::cancel(journal_root, attempt_id).map(|_| ())
    }

    fn retry_speech(&self, journal_root: &Path) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            let home_dir = std::env::home_dir().unwrap_or_default();
            let config = solstone_core_journal_config::read_journal_config(journal_root)
                .map_err(|e| e.to_string())?;
            solstone_core_local::install::coreml_install::install_parakeet_coreml_model(
                &home_dir, &config, false,
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        }
        #[cfg(target_os = "windows")]
        {
            let _ = journal_root;
            Ok(())
        }
        #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
        {
            let held = solstone_core_local::install::lease::acquire(journal_root, "parakeet")
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "speech installer lease is busy".to_string())?;
            solstone_core_local::install::install_parakeet_with_lease(
                journal_root,
                std::env::consts::OS,
                std::env::consts::ARCH,
                held,
            )
            .map(|_| ())
            .map_err(|e| {
                e.envelope
                    .error
                    .as_ref()
                    .map(|err| err.message.clone())
                    .unwrap_or_else(|| format!("{e:?}"))
            })
        }
    }

    fn project_downloads(&self, journal_root: &Path) -> SetupDownloads {
        SetupDownloads {
            speech: map_status(journal_root, "parakeet"),
            thinking: map_status(journal_root, "local"),
        }
    }

    fn catalog_facts(&self) -> SetupCatalogFacts {
        solstone_core_thinking_copy::current_setup_facts()
    }
}

fn map_status(journal: &Path, provider: &str) -> SetupDownloadTask {
    let observed = solstone_core_local::install::status::read_observed_status(journal, provider);
    let status = match observed {
        Ok(status) => status,
        Err(_) => {
            return SetupDownloadTask {
                state: "idle".into(),
                attempt_id: None,
                bytes_done: None,
                bytes_total: None,
                error: None,
            };
        }
    };

    match status.install_state.as_str() {
        "resolving" | "downloading" | "verifying" | "installing" => SetupDownloadTask {
            state: "downloading".into(),
            attempt_id: status.attempt_id,
            bytes_done: status.progress_bytes_received,
            bytes_total: status.progress_bytes_total,
            error: None,
        },
        "installed" => SetupDownloadTask {
            state: "complete".into(),
            attempt_id: status.attempt_id,
            bytes_done: status.progress_bytes_received,
            bytes_total: status.progress_bytes_total,
            error: None,
        },
        "failed" if status.error_code.as_deref() == Some("install_cancelled") => {
            SetupDownloadTask {
                state: "cancelled".into(),
                attempt_id: status.attempt_id,
                bytes_done: status.progress_bytes_received,
                bytes_total: status.progress_bytes_total,
                error: None,
            }
        }
        "failed" => SetupDownloadTask {
            state: "error".into(),
            attempt_id: status.attempt_id,
            bytes_done: status.progress_bytes_received,
            bytes_total: status.progress_bytes_total,
            error: Some(SetupDownloadError {
                code: status.error_code.unwrap_or_else(|| "install_failed".into()),
                detail: status.install_error.unwrap_or_default(),
            }),
        },
        _ => SetupDownloadTask {
            state: "idle".into(),
            attempt_id: None,
            bytes_done: status.progress_bytes_received,
            bytes_total: status.progress_bytes_total,
            error: None,
        },
    }
}
