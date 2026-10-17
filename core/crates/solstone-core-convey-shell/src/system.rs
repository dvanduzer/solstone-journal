// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::path::PathBuf;

use axum::Json;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde_json::json;
use solstone_core_home::HomeContext;
use solstone_core_home::readers::get_capture_health;

#[allow(clippy::needless_return)]
pub async fn status(journal_root: PathBuf) -> Response {
    let context = HomeContext::new(journal_root, Utc::now());
    let capture = get_capture_health(&context);
    #[cfg(target_os = "linux")]
    {
        let current_exe = std::env::current_exe();
        let version_report = tokio::task::spawn_blocking(move || {
            let Some(data_dir) = solstone_core_update::owner_data_dir() else {
                return solstone_core_update::UpdateReport::owner_context_unknown();
            };
            let exe = match current_exe {
                Ok(ref p) => p.as_path(),
                Err(_) => {
                    return solstone_core_update::UpdateReport::not_owned_unknown(
                        solstone_core_update::read_hold_state(&data_dir),
                    );
                }
            };
            let etc_dir = std::path::Path::new("/etc");
            let fetch = solstone_core_update::RealInstallerFetch;
            let runner = solstone_core_update::RealInstallerRunner;
            let probe = solstone_core_update::RealPackageProbe;
            linux_version_status(&runner, &fetch, &probe, exe, &data_dir, etc_dir)
        })
        .await
        .unwrap_or_else(|_| {
            solstone_core_update::UpdateReport::not_owned_unknown(
                solstone_core_update::owner_data_dir()
                    .map(|dir| solstone_core_update::read_hold_state(&dir))
                    .unwrap_or(solstone_core_update::HoldState::Unknown),
            )
        });

        return Json(json!({
            "version": version_report,
            "capture": capture,
            "ok": true,
        }))
        .into_response();
    }

    #[cfg(not(target_os = "linux"))]
    {
        return Json(json!({
            "version": {
                "current": env!("CARGO_PKG_VERSION"),
                "latest": env!("CARGO_PKG_VERSION"),
                "update_available": false,
            },
            "capture": capture,
            "ok": true,
        }))
        .into_response();
    }
}

#[cfg(target_os = "linux")]
fn linux_version_status(
    runner: &dyn solstone_core_update::InstallerRunner,
    fetch: &dyn solstone_core_update::InstallerFetch,
    probe: &dyn solstone_core_update::PackageProbe,
    executable: &std::path::Path,
    data: &std::path::Path,
    etc: &std::path::Path,
) -> solstone_core_update::UpdateReport {
    solstone_core_update::check(runner, fetch, probe, executable, data, etc, true, None)
}

#[cfg(all(test, feature = "full-tests", target_os = "linux"))]
mod update_tests {
    use super::*;
    use solstone_core_update::*;
    use std::path::{Path, PathBuf};
    struct Fetch(PathBuf);
    impl InstallerFetch for Fetch {
        fn fetch(&self, _: InstallerSource) -> std::io::Result<PathBuf> {
            Ok(self.0.clone())
        }
    }
    struct Probe;
    impl PackageProbe for Probe {
        fn owns_journal(&self, _: &Path) -> bool {
            false
        }
        fn payload_matches(&self, _: &Path, _: &str) -> bool {
            true
        }
    }
    #[test]
    fn linux_status_and_cli_check_agree_for_available_and_failed_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("prefix");
        let entry = "2.0.48-0123456789ab";
        let version = prefix.join("versions").join(entry);
        std::fs::create_dir_all(version.join("bin")).unwrap();
        std::fs::write(version.join("bin/solstone"), "fixture").unwrap();
        std::fs::write(
            version.join(".archive-sha256"),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        std::fs::write(version.join(".release"), "version=2.0.48\n").unwrap();
        std::fs::write(
            prefix.join("install-receipt"),
            "role=journal\nsignature_verification=verified\nservice_policy=start\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(format!("versions/{entry}"), prefix.join("current")).unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("install.conf"), format!("[solstone]\nschema_version=1\norigin=https://updates.solstone.app\nplatform_version=2.0.48-r1\nlane=release\nroute=tree\nphase=complete\nprefix={}\n[component:journal]\nroute=tree\nstatus=installed\n", prefix.display())).unwrap();
        let etc = dir.path().join("etc");
        let executable = prefix.join("current/bin/solstone");
        let installer = dir.path().join("installer.sh");
        let fetch = Fetch(installer.clone());
        let args = parse_args(&[std::ffi::OsString::from("--check")]).unwrap();
        for good in [true, false] {
            let script = if good {
                "printf '%s\n' '{\"platform_version\":\"2.0.49-r2\",\"lane\":\"release\",\"components\":{\"journal\":{\"version\":\"2.0.49\"}}}'\n"
            } else {
                "exit 7\n"
            };
            std::fs::write(&installer, script).unwrap();
            let status = linux_version_status(
                &RealInstallerRunner,
                &fetch,
                &Probe,
                &executable,
                &data,
                &etc,
            );
            let cli = perform(
                &RealInstallerRunner,
                &fetch,
                &Probe,
                &executable,
                &data,
                &etc,
                true,
                &args,
            )
            .unwrap_or_else(|(report, _)| report);
            assert_eq!(
                status.availability,
                if good {
                    Availability::Available
                } else {
                    Availability::Unknown
                }
            );
            assert_eq!(status.availability, cli.availability);
            assert_eq!(status.current, cli.current);
            assert_eq!(status.target, cli.target);
            assert_eq!(status.latest_platform_version, cli.latest_platform_version);
        }
    }
}
