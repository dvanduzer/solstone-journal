// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Atomic installation and verification of the Parakeet CoreML model tree.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use solstone_core_assets::{Artifact, Platform, RuntimeFetchQuery, canonical_host_pair, catalog};
use solstone_core_journal_config::{
    JournalConfigRead,
    parakeet_coreml::{
        ParakeetCoremlSentinel, default_parakeet_coreml_cache_dir, parakeet_coreml_cache_dir,
        parakeet_coreml_model_root, parakeet_coreml_sentinel_path,
        read_valid_parakeet_coreml_sentinel,
    },
};
use thiserror::Error;

use super::archive;

const UNIT: &str = "parakeet-coreml";
const FLUIDAUDIO_VERSION: &str = "0.14.0";
static COUNTER: AtomicU64 = AtomicU64::new(1);

/// Printed by `install-models` before it asks this installer to fetch assets.
pub const PARAKEET_COREML_DOWNLOAD_DISCLOSURE: &str = "parakeet assets: downloading the parakeet-tdt-0.6b-v3 Core ML model (CC-BY-4.0) from updates.solstone.app. see THIRD_PARTY_NOTICES.md.";

#[derive(Debug, Error)]
#[error("{message}")]
pub struct CoremlInstallError {
    pub reason_code: String,
    message: String,
    pub exit_code: u8,
}

impl CoremlInstallError {
    fn new(reason_code: impl Into<String>, message: impl Into<String>, exit_code: u8) -> Self {
        Self {
            reason_code: reason_code.into(),
            message: message.into(),
            exit_code,
        }
    }
}

fn rows() -> Result<Vec<&'static Artifact>, CoremlInstallError> {
    let rows = catalog()
        .iter()
        .filter(|artifact| artifact.unit == UNIT && artifact.platform == Some(Platform::MacosArm64))
        .collect::<Vec<_>>();
    Ok(rows)
}

fn shared_catalog_version(rows: &[&Artifact]) -> Result<&'static str, CoremlInstallError> {
    let first = rows.first().ok_or_else(|| {
        CoremlInstallError::new("catalog_empty", "no parakeet-coreml catalog rows found", 65)
    })?;
    let version = first.version;
    if rows.iter().any(|r| r.version != version) {
        return Err(CoremlInstallError::new(
            "catalog_version_mismatch",
            "parakeet-coreml catalog rows have conflicting versions",
            65,
        ));
    }
    Ok(version)
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn require_coreml_host(os_name: &str, arch: &str) -> Result<(), CoremlInstallError> {
    if os_name == "darwin" && arch == "arm64" {
        Ok(())
    } else {
        Err(CoremlInstallError::new(
            "platform_unsupported",
            format!("parakeet CoreML is only supported on darwin/arm64, not {os_name}/{arch}"),
            69,
        ))
    }
}

fn archive_error_reason_code(error: &archive::ArchiveError) -> &'static str {
    match error {
        archive::ArchiveError::HostRefused { .. } => "download_host_refused",
        archive::ArchiveError::InsecureScheme { .. } => "download_insecure_scheme",
        archive::ArchiveError::UrlUserinfoRefused { .. } => "download_url_userinfo_refused",
        archive::ArchiveError::SizeMismatch { .. } => "download_size_mismatch",
        archive::ArchiveError::DigestMismatch { .. } => "download_digest_mismatch",
        archive::ArchiveError::RedirectHopLimitExceeded { .. } => "download_redirect_hop_limit",
        archive::ArchiveError::OriginUnavailable { .. } => "download_origin_unreachable",
        archive::ArchiveError::Io(_) | archive::ArchiveError::Download(_) => "download_failed",
        archive::ArchiveError::PathEscape(_) => "download_failed",
    }
}

fn archive_error(error: archive::ArchiveError) -> CoremlInstallError {
    CoremlInstallError::new(archive_error_reason_code(&error), error.to_string(), 74)
}

fn write_sentinel(path: &Path, sentinel: &ParakeetCoremlSentinel) -> std::io::Result<()> {
    let parent = path.parent().expect("sentinel has a parent");
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".install-complete.tmp{}",
        uuid::Uuid::new_v4().simple()
    ));
    let bytes = serde_json::to_vec(sentinel).expect("static CoreML sentinel serializes");
    let result = fs::write(&temporary, bytes).and_then(|_| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Install the catalogued CoreML tree using the production origin policy.
pub fn install_parakeet_coreml_model(
    home_dir: &Path,
    config: &JournalConfigRead,
    force: bool,
) -> Result<PathBuf, CoremlInstallError> {
    let (os_name, arch) = canonical_host_pair(std::env::consts::OS, std::env::consts::ARCH);
    let rows = rows()?;
    install_with_rows(home_dir, config, force, (os_name, arch), &rows)
}

/// Refresh an installed CoreML model tree if on darwin/arm64 and catalog pins moved.
pub fn refresh_installed_parakeet_coreml(
    home_dir: &Path,
    config: &JournalConfigRead,
) -> Result<PathBuf, CoremlInstallError> {
    let (os_name, arch) = canonical_host_pair(std::env::consts::OS, std::env::consts::ARCH);
    if !(os_name == "darwin" && arch == "arm64") {
        return Ok(parakeet_coreml_model_root(&parakeet_coreml_cache_dir(
            config, home_dir,
        )));
    }
    let rows = rows()?;
    let mut fetch = |query: &RuntimeFetchQuery<'_>, destination: &Path| {
        fetch_runtime_member(query, destination)
    };
    refresh_with_rows(home_dir, config, (os_name, arch), &rows, &mut fetch)
}

/// Verify the model tree and install sentinel without fetching.
pub fn check_parakeet_coreml_install(
    home_dir: &Path,
    config: &JournalConfigRead,
) -> Result<(), CoremlInstallError> {
    let (os_name, arch) = canonical_host_pair(std::env::consts::OS, std::env::consts::ARCH);
    check_parakeet_coreml_install_with_platform(home_dir, config, os_name, arch)
}

fn fetch_runtime_member(
    query: &solstone_core_assets::RuntimeFetchQuery<'_>,
    destination: &Path,
) -> Result<(), CoremlInstallError> {
    let handle = solstone_core_assets::mint_runtime_fetch(query).map_err(|err| {
        CoremlInstallError::new(
            solstone_core_assets::FetchMintError::REASON_CODE,
            err.to_string(),
            65,
        )
    })?;
    solstone_core_artifact_download::download_runtime_fetch(&handle, destination, |_, _| {})
        .map_err(archive_error)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoremlAdmission {
    Mint,
    #[cfg(feature = "test-hooks")]
    Fixture,
}

fn install_with_rows(
    home_dir: &Path,
    config: &JournalConfigRead,
    force: bool,
    platform: (&str, &str),
    rows: &[&Artifact],
) -> Result<PathBuf, CoremlInstallError> {
    let mut publish = |staging: &Path, target: &Path| fs::rename(staging, target);
    let mut write = write_sentinel;
    install_with_rows_and_seams(
        home_dir,
        config,
        force,
        platform,
        rows,
        CoremlAdmission::Mint,
        &mut publish,
        &mut write,
    )
}

/// Internal fixture seam: run the CoreML installer with injected publish and sentinel writers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn install_with_rows_and_seams(
    home_dir: &Path,
    config: &JournalConfigRead,
    force: bool,
    platform: (&str, &str),
    rows: &[&Artifact],
    admission: CoremlAdmission,
    publish: &mut impl FnMut(&Path, &Path) -> std::io::Result<()>,
    write: &mut impl FnMut(&Path, &ParakeetCoremlSentinel) -> std::io::Result<()>,
) -> Result<PathBuf, CoremlInstallError> {
    let (os_name, arch) = platform;
    require_coreml_host(os_name, arch)?;
    let cache_dir = parakeet_coreml_cache_dir(config, home_dir);
    let target = parakeet_coreml_model_root(&cache_dir);
    if !force && check_with_rows(home_dir, config, os_name, arch, rows).is_ok() {
        return Ok(target);
    }

    let mut fetch = |query: &RuntimeFetchQuery<'_>, destination: &Path| match admission {
        CoremlAdmission::Mint => fetch_runtime_member(query, destination),
        #[cfg(feature = "test-hooks")]
        CoremlAdmission::Fixture => {
            let row = rows
                .iter()
                .find(|r| Some(r.origin_key) == query.origin_key)
                .expect("row exists in fixture rows");
            let handle = solstone_core_assets::runtime_fetch_handle_fixture(
                row.unit,
                row.origin_key,
                row.sha256,
                row.size_bytes,
            );
            solstone_core_artifact_download::download_runtime_fetch(
                &handle,
                destination,
                |_, _| {},
            )
            .map_err(archive_error)?;
            Ok(())
        }
    };

    download_and_commit_coreml(
        home_dir, config, platform, rows, false, &mut fetch, publish, write,
    )
}

fn check_parakeet_coreml_install_with_platform(
    home_dir: &Path,
    config: &JournalConfigRead,
    os_name: &str,
    arch: &str,
) -> Result<(), CoremlInstallError> {
    let rows = rows()?;
    check_with_rows(home_dir, config, os_name, arch, &rows)
}

fn check_with_rows(
    home_dir: &Path,
    config: &JournalConfigRead,
    os_name: &str,
    arch: &str,
    rows: &[&Artifact],
) -> Result<(), CoremlInstallError> {
    require_coreml_host(os_name, arch)?;
    let sentinel =
        read_valid_parakeet_coreml_sentinel(home_dir, os_name, arch).ok_or_else(|| {
            CoremlInstallError::new(
                "sentinel_not_ready",
                format!(
                    "parakeet CoreML sentinel not ready: {}; install it with: solstone journal install-models",
                    parakeet_coreml_sentinel_path(home_dir).display(),
                ),
                65,
            )
        })?;
    let expected_cache_dir = parakeet_coreml_cache_dir(config, home_dir);
    if sentinel.cache_dir() != expected_cache_dir {
        return Err(CoremlInstallError::new(
            "cache_dir_mismatch",
            format!(
                "parakeet CoreML sentinel cache dir ({}) does not match configured ({})",
                sentinel.cache_dir().display(),
                expected_cache_dir.display(),
            ),
            65,
        ));
    }
    check_model_tree_with_rows(sentinel.cache_dir(), rows)?;
    let expected_catalog_version = shared_catalog_version(rows)?;
    if sentinel.catalog_version() != expected_catalog_version {
        return Err(CoremlInstallError::new(
            "catalog_version_mismatch",
            format!(
                "parakeet CoreML catalog version mismatch: found {}, expected {}",
                sentinel.catalog_version(),
                expected_catalog_version,
            ),
            65,
        ));
    }
    Ok(())
}

fn check_model_tree_with_rows(
    cache_dir: &Path,
    rows: &[&Artifact],
) -> Result<(), CoremlInstallError> {
    let model_root = parakeet_coreml_model_root(cache_dir);
    for row in rows {
        let path = model_root.join(row.filename);
        let meta = match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => meta,
            _ => {
                return Err(CoremlInstallError::new(
                    "model_incomplete",
                    format!(
                        "parakeet CoreML asset missing: {}; install it with: solstone journal install-models",
                        row.filename
                    ),
                    65,
                ));
            }
        };
        if meta.len() != row.size_bytes {
            return Err(CoremlInstallError::new(
                "model_incomplete",
                format!(
                    "parakeet CoreML asset size mismatch for {}: expected {}, found {}",
                    row.filename,
                    row.size_bytes,
                    meta.len()
                ),
                65,
            ));
        }
        let actual_sha = sha256_file(&path).map_err(|error| {
            CoremlInstallError::new(
                "model_incomplete",
                format!("failed to read {}: {error}", row.filename),
                65,
            )
        })?;
        if actual_sha != row.sha256 {
            return Err(CoremlInstallError::new(
                "model_incomplete",
                format!(
                    "parakeet CoreML asset digest mismatch for {}: expected {}, found {}",
                    row.filename, row.sha256, actual_sha
                ),
                65,
            ));
        }
    }
    Ok(())
}

/// Shared readiness/doctor check: verifies the sentinel's own `cache_dir` without a config override.
pub fn check_parakeet_coreml_cache_doctor(
    home_dir: &Path,
    os_name: &str,
    arch: &str,
) -> Result<PathBuf, String> {
    require_coreml_host(os_name, arch)
        .map_err(|error| format!("parakeet check failed: {error}"))?;
    let sentinel =
        read_valid_parakeet_coreml_sentinel(home_dir, os_name, arch).ok_or_else(|| {
            format!(
                "parakeet check failed: sentinel not ready at {}",
                default_parakeet_coreml_cache_dir(home_dir)
                    .join(".install-complete")
                    .display()
            )
        })?;
    let rows = rows().map_err(|error| error.to_string())?;
    let expected_catalog_version =
        shared_catalog_version(&rows).map_err(|error| error.to_string())?;
    if sentinel.catalog_version() != expected_catalog_version {
        return Err(format!(
            "parakeet check failed: catalog version mismatch: found {}, expected {}",
            sentinel.catalog_version(),
            expected_catalog_version
        ));
    }
    check_model_tree_with_rows(sentinel.cache_dir(), &rows).map_err(|error| {
        format!(
            "parakeet check failed: cache verification failed at {}: {error}",
            sentinel.cache_dir().display()
        )
    })?;
    Ok(sentinel.cache_dir().to_path_buf())
}

fn files_match_rows(model_root: &Path, rows: &[&Artifact]) -> bool {
    for row in rows {
        let path = model_root.join(row.filename);
        let Ok(meta) = fs::metadata(&path) else {
            return false;
        };
        if !meta.is_file() || meta.len() != row.size_bytes {
            return false;
        }
        let Ok(actual_sha) = sha256_file(&path) else {
            return false;
        };
        if actual_sha != row.sha256 {
            return false;
        }
    }
    true
}

fn rewrite_sentinel_in_place(
    sentinel_path: &Path,
    sentinel: &ParakeetCoremlSentinel,
) -> Result<(), CoremlInstallError> {
    let parent = sentinel_path.parent().expect("sentinel path has parent");
    fs::create_dir_all(parent)
        .map_err(|error| CoremlInstallError::new("sentinel_write_failed", error.to_string(), 74))?;
    let temp_sentinel = parent.join(format!(
        ".install-complete.tmp.{}",
        uuid::Uuid::new_v4().simple()
    ));
    write_sentinel(&temp_sentinel, sentinel)
        .map_err(|error| CoremlInstallError::new("sentinel_write_failed", error.to_string(), 74))?;

    let sentinel_aside = parent.join(format!(
        ".install-complete.previous.{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let had_sentinel = sentinel_path.exists();
    if had_sentinel {
        fs::rename(sentinel_path, &sentinel_aside).map_err(|error| {
            CoremlInstallError::new("sentinel_write_failed", error.to_string(), 74)
        })?;
    }
    if let Err(error) = fs::rename(&temp_sentinel, sentinel_path) {
        if had_sentinel {
            let _ = fs::rename(&sentinel_aside, sentinel_path);
        }
        let _ = fs::remove_file(&temp_sentinel);
        return Err(CoremlInstallError::new(
            "sentinel_write_failed",
            error.to_string(),
            74,
        ));
    }
    if had_sentinel {
        let _ = fs::remove_file(&sentinel_aside);
    }
    Ok(())
}

fn refresh_with_rows(
    home_dir: &Path,
    config: &JournalConfigRead,
    platform: (&str, &str),
    rows: &[&Artifact],
    fetch: &mut impl FnMut(&RuntimeFetchQuery<'_>, &Path) -> Result<(), CoremlInstallError>,
) -> Result<PathBuf, CoremlInstallError> {
    let (os_name, arch) = platform;
    require_coreml_host(os_name, arch)?;
    let cache_dir = parakeet_coreml_cache_dir(config, home_dir);
    let target = parakeet_coreml_model_root(&cache_dir);

    let Some(sentinel) = read_valid_parakeet_coreml_sentinel(home_dir, os_name, arch) else {
        return Ok(target);
    };
    if sentinel.cache_dir() != cache_dir {
        return Ok(target);
    }

    if check_with_rows(home_dir, config, os_name, arch, rows).is_ok() {
        return Ok(target);
    }

    let catalog_version = shared_catalog_version(rows)?;

    if files_match_rows(&target, rows) {
        let updated_sentinel = ParakeetCoremlSentinel::new(
            cache_dir.clone(),
            os_name,
            arch,
            FLUIDAUDIO_VERSION,
            catalog_version,
        );
        let sentinel_path = parakeet_coreml_sentinel_path(home_dir);
        rewrite_sentinel_in_place(&sentinel_path, &updated_sentinel)?;
        return Ok(target);
    }

    let mut publish = |staging: &Path, target: &Path| fs::rename(staging, target);
    let mut write = write_sentinel;
    download_and_commit_coreml(
        home_dir,
        config,
        platform,
        rows,
        true,
        fetch,
        &mut publish,
        &mut write,
    )
}

#[allow(clippy::too_many_arguments)]
fn download_and_commit_coreml(
    home_dir: &Path,
    config: &JournalConfigRead,
    platform: (&str, &str),
    rows: &[&Artifact],
    is_refresh: bool,
    fetch: &mut impl FnMut(&RuntimeFetchQuery<'_>, &Path) -> Result<(), CoremlInstallError>,
    publish: &mut impl FnMut(&Path, &Path) -> std::io::Result<()>,
    write: &mut impl FnMut(&Path, &ParakeetCoremlSentinel) -> std::io::Result<()>,
) -> Result<PathBuf, CoremlInstallError> {
    let (os_name, arch) = platform;
    let cache_dir = parakeet_coreml_cache_dir(config, home_dir);
    let target = parakeet_coreml_model_root(&cache_dir);
    let catalog_version = shared_catalog_version(rows)?;

    let parent = target.parent().expect("CoreML model root has a parent");
    fs::create_dir_all(parent)
        .map_err(|error| CoremlInstallError::new("install_failed", error.to_string(), 74))?;
    let staging = parent.join(format!(
        ".parakeet-tdt-0.6b-v3.stage.{}",
        uuid::Uuid::new_v4().simple()
    ));

    fs::create_dir(&staging)
        .map_err(|error| CoremlInstallError::new("install_failed", error.to_string(), 74))?;

    let download_result = (|| {
        for row in rows {
            let query = RuntimeFetchQuery {
                unit: Some(row.unit),
                origin_key: Some(row.origin_key),
                ..Default::default()
            };
            let destination = staging.join(row.filename);
            if let Some(dest_parent) = destination.parent() {
                fs::create_dir_all(dest_parent).map_err(|error| {
                    CoremlInstallError::new("install_failed", error.to_string(), 74)
                })?;
            }
            fetch(&query, &destination)?;
        }
        Ok(())
    })();

    if let Err(error) = download_result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    commit_coreml_tree(
        home_dir,
        os_name,
        arch,
        &staging,
        &target,
        &cache_dir,
        catalog_version,
        is_refresh,
        publish,
        write,
    )?;

    Ok(target)
}

#[allow(clippy::too_many_arguments)]
fn commit_coreml_tree(
    home_dir: &Path,
    os_name: &str,
    arch: &str,
    staging: &Path,
    target: &Path,
    cache_dir: &Path,
    catalog_version: &str,
    is_refresh: bool,
    publish: &mut impl FnMut(&Path, &Path) -> std::io::Result<()>,
    write: &mut impl FnMut(&Path, &ParakeetCoremlSentinel) -> std::io::Result<()>,
) -> Result<(), CoremlInstallError> {
    if is_refresh && read_valid_parakeet_coreml_sentinel(home_dir, os_name, arch).is_none() {
        let _ = fs::remove_dir_all(staging);
        return Err(CoremlInstallError::new(
            "sentinel_removed",
            "incumbent sentinel was removed before publication",
            74,
        ));
    }

    let sentinel_path = parakeet_coreml_sentinel_path(home_dir);
    let sentinel_parent = sentinel_path.parent().expect("sentinel has parent");
    fs::create_dir_all(sentinel_parent)
        .map_err(|error| CoremlInstallError::new("install_failed", error.to_string(), 74))?;
    let temp_sentinel = sentinel_parent.join(format!(
        ".install-complete.tmp.{}",
        uuid::Uuid::new_v4().simple()
    ));
    let new_record = ParakeetCoremlSentinel::new(
        cache_dir.to_path_buf(),
        os_name,
        arch,
        FLUIDAUDIO_VERSION,
        catalog_version,
    );
    if let Err(error) = write(&temp_sentinel, &new_record) {
        let _ = fs::remove_dir_all(staging);
        return Err(CoremlInstallError::new(
            "sentinel_write_failed",
            error.to_string(),
            74,
        ));
    }

    let target_parent = target.parent().expect("target has parent");
    let tree_aside = target_parent.join(format!(
        ".parakeet-tdt-0.6b-v3.previous.{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let had_target = target.exists();
    if had_target && let Err(error) = fs::rename(target, &tree_aside) {
        let _ = fs::remove_dir_all(staging);
        let _ = fs::remove_file(&temp_sentinel);
        return Err(CoremlInstallError::new(
            "publish_failed",
            error.to_string(),
            74,
        ));
    }

    if let Err(error) = publish(staging, target) {
        if had_target {
            let _ = fs::rename(&tree_aside, target);
            let _ = fs::remove_dir_all(&tree_aside);
        }
        let _ = fs::remove_dir_all(staging);
        let _ = fs::remove_file(&temp_sentinel);
        return Err(CoremlInstallError::new(
            "publish_failed",
            error.to_string(),
            74,
        ));
    }

    let sentinel_aside = sentinel_parent.join(format!(
        ".install-complete.previous.{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let had_sentinel = sentinel_path.exists();
    if had_sentinel && let Err(error) = fs::rename(&sentinel_path, &sentinel_aside) {
        let _ = fs::remove_dir_all(target);
        if had_target {
            let _ = fs::rename(&tree_aside, target);
            let _ = fs::remove_dir_all(&tree_aside);
        }
        let _ = fs::remove_file(&temp_sentinel);
        return Err(CoremlInstallError::new(
            "sentinel_write_failed",
            error.to_string(),
            74,
        ));
    }

    if let Err(error) = fs::rename(&temp_sentinel, &sentinel_path) {
        if had_sentinel {
            let _ = fs::rename(&sentinel_aside, &sentinel_path);
            let _ = fs::remove_file(&sentinel_aside);
        }
        let _ = fs::remove_dir_all(target);
        if had_target {
            let _ = fs::rename(&tree_aside, target);
            let _ = fs::remove_dir_all(&tree_aside);
        }
        let _ = fs::remove_file(&temp_sentinel);
        return Err(CoremlInstallError::new(
            "sentinel_write_failed",
            error.to_string(),
            74,
        ));
    }

    if had_target {
        let _ = fs::remove_dir_all(&tree_aside);
    }
    if had_sentinel {
        let _ = fs::remove_file(&sentinel_aside);
    }

    Ok(())
}

#[cfg(feature = "test-hooks")]
pub(crate) fn install_with_rows_for_test(
    home_dir: &Path,
    config: &JournalConfigRead,
    force: bool,
    rows: &[&Artifact],
) -> Result<PathBuf, CoremlInstallError> {
    let mut publish = |staging: &Path, target: &Path| fs::rename(staging, target);
    let mut write = write_sentinel;
    install_with_rows_and_seams(
        home_dir,
        config,
        force,
        ("darwin", "arm64"),
        rows,
        CoremlAdmission::Fixture,
        &mut publish,
        &mut write,
    )
}

#[cfg(any(test, feature = "test-hooks"))]
pub(crate) fn install_minted_rows_for_test(
    home_dir: &Path,
    config: &JournalConfigRead,
    force: bool,
    rows: &[&Artifact],
) -> Result<PathBuf, CoremlInstallError> {
    let mut publish = |staging: &Path, target: &Path| fs::rename(staging, target);
    let mut write = write_sentinel;
    install_with_rows_and_seams(
        home_dir,
        config,
        force,
        ("darwin", "arm64"),
        rows,
        CoremlAdmission::Mint,
        &mut publish,
        &mut write,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coreml_catalog_rows_match_the_filtered_catalog_set() {
        let rows = rows().unwrap();
        let expected = catalog()
            .iter()
            .filter(|artifact| {
                artifact.unit == UNIT && artifact.platform == Some(Platform::MacosArm64)
            })
            .map(|artifact| artifact.filename)
            .collect::<std::collections::BTreeSet<_>>();
        let actual = rows
            .iter()
            .map(|artifact| artifact.filename)
            .collect::<std::collections::BTreeSet<_>>();
        assert!(!expected.is_empty());
        assert_eq!(actual, expected);
    }

    #[test]
    fn coreml_archive_error_reason_codes_cover_every_archive_error() {
        use archive::ArchiveError;
        assert_eq!(
            archive_error_reason_code(&ArchiveError::HostRefused {
                host: "blocked.test".to_owned(),
            }),
            "download_host_refused"
        );
        assert_eq!(
            archive_error_reason_code(&ArchiveError::InsecureScheme {
                scheme: "http".to_owned(),
                host: "example.test".to_owned(),
            }),
            "download_insecure_scheme"
        );
        assert_eq!(
            archive_error_reason_code(&ArchiveError::UrlUserinfoRefused {
                authority: "user@host".to_owned(),
            }),
            "download_url_userinfo_refused"
        );
        assert_eq!(
            archive_error_reason_code(&ArchiveError::SizeMismatch {
                expected: 1,
                actual: 2,
            }),
            "download_size_mismatch"
        );
        assert_eq!(
            archive_error_reason_code(&ArchiveError::DigestMismatch {
                expected: "a".repeat(64),
                actual: "b".repeat(64),
            }),
            "download_digest_mismatch"
        );
        assert_eq!(
            archive_error_reason_code(&ArchiveError::RedirectHopLimitExceeded { limit: 11 }),
            "download_redirect_hop_limit"
        );
        assert_eq!(
            archive_error_reason_code(&ArchiveError::OriginUnavailable {
                host: "updates.invalid".to_owned(),
                message: "timeout".to_owned(),
            }),
            "download_origin_unreachable"
        );
        assert_eq!(
            archive_error_reason_code(&ArchiveError::Io(std::io::Error::other("failure"))),
            "download_failed"
        );
        assert_eq!(
            archive_error_reason_code(&ArchiveError::Download("network".to_owned())),
            "download_failed"
        );
        assert_eq!(
            archive_error_reason_code(&ArchiveError::PathEscape("..".to_owned())),
            "download_failed"
        );
    }

    fn config(cache_dir: &Path) -> JournalConfigRead {
        let config = serde_json::json!({"transcribe": {"parakeet": {"cache_dir": cache_dir}}})
            .as_object()
            .unwrap()
            .clone();
        JournalConfigRead {
            present: true,
            sha256: None,
            config: Some(config),
        }
    }

    fn row(filename: &'static str, bytes: &[u8]) -> Artifact {
        Artifact {
            unit: UNIT,
            version: "test",
            filename,
            sha256: Box::leak(format!("{:x}", Sha256::digest(bytes)).into_boxed_str()),
            size_bytes: bytes.len() as u64,
            upstream_url: "https://upstream.invalid/test",
            origin_key: Box::leak(format!("test/{filename}").into_boxed_str()),
            artifact_key: None,
            platform: Some(Platform::MacosArm64),
            backend: None,
            extracted_binary_sha256: None,
        }
    }

    #[test]
    fn check_rejects_a_sentinel_for_a_removed_cache_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let missing = temporary.path().join("removed/cache");
        let cfg = config(&missing);
        let record =
            ParakeetCoremlSentinel::new(missing, "darwin", "arm64", FLUIDAUDIO_VERSION, "test");
        let path = parakeet_coreml_sentinel_path(&home);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        write_sentinel(&path, &record).unwrap();

        let error = check_parakeet_coreml_install_with_platform(&home, &cfg, "darwin", "arm64")
            .unwrap_err();
        assert_eq!(error.reason_code, "sentinel_not_ready");
    }

    #[test]
    fn install_refuses_unminted_artifact_without_writing() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let config = config(&temporary.path().join("cache"));
        let artifact = row("model.mil", b"model");
        let rows = [&artifact];
        let error =
            install_with_rows(&home, &config, false, ("darwin", "arm64"), &rows).unwrap_err();
        assert_eq!(error.reason_code, "component_packaged");
        assert!(!parakeet_coreml_model_root(&parakeet_coreml_cache_dir(&config, &home)).exists());
        assert!(!parakeet_coreml_sentinel_path(&home).exists());
    }

    #[test]
    fn check_empty_host_names_the_install_command_without_requests() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let cfg = config(&temporary.path().join("cache"));
        let artifact = row("model.mil", b"model");
        let rows = [&artifact];
        let error = check_with_rows(&home, &cfg, "darwin", "arm64", &rows).unwrap_err();
        assert_eq!(error.reason_code, "sentinel_not_ready");
        assert!(
            error
                .to_string()
                .contains("solstone journal install-models")
        );
    }

    #[test]
    fn the_canonical_coreml_host_is_accepted_by_the_guard() {
        require_coreml_host("darwin", "arm64").unwrap();
    }

    #[test]
    fn owner_output_for_macos_aarch64_is_accepted_by_the_guard() {
        let (os_name, arch) = canonical_host_pair("macos", "aarch64");
        require_coreml_host(os_name, arch).unwrap();
    }

    #[test]
    fn a_genuinely_unsupported_host_is_still_refused() {
        let error =
            require_coreml_host("linux", "x86_64").expect_err("linux/x86_64 is not a CoreML host");
        assert_eq!(error.reason_code, "platform_unsupported");
    }

    #[test]
    fn same_name_and_size_different_digest_fails_check() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let cache = temporary.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        let cfg = config(&cache);
        let artifact = row("model.bin", b"correct-bytes");
        let rows = [&artifact];

        let target = parakeet_coreml_model_root(&cache);
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("model.bin"), b"wrong---bytes").unwrap();

        let sentinel = ParakeetCoremlSentinel::new(
            cache.clone(),
            "darwin",
            "arm64",
            FLUIDAUDIO_VERSION,
            artifact.version,
        );
        let sentinel_path = parakeet_coreml_sentinel_path(&home);
        write_sentinel(&sentinel_path, &sentinel).unwrap();

        let error = check_with_rows(&home, &cfg, "darwin", "arm64", &rows).unwrap_err();
        assert_eq!(error.reason_code, "model_incomplete");
    }

    #[test]
    fn legacy_sentinel_adopts_matching_files_with_zero_fetches() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let cache = temporary.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        let cfg = config(&cache);
        let artifact = row("model.bin", b"matching-bytes");
        let rows = [&artifact];

        let target = parakeet_coreml_model_root(&cache);
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("model.bin"), b"matching-bytes").unwrap();

        let sentinel_path = parakeet_coreml_sentinel_path(&home);
        fs::create_dir_all(sentinel_path.parent().unwrap()).unwrap();
        fs::write(
            &sentinel_path,
            serde_json::json!({
                "schema_version": 1,
                "backend": "parakeet",
                "variant": "coreml",
                "model_version": "v3",
                "quantization": "fp32",
                "fluidaudio_version": "0.14.0",
                "platform": {"os": "darwin", "arch": "arm64"},
                "cache_dir": cache,
            })
            .to_string(),
        )
        .unwrap();

        let mut fetch_count = 0;
        let mut fetch = |_query: &RuntimeFetchQuery<'_>, _dest: &Path| {
            fetch_count += 1;
            Ok(())
        };

        refresh_with_rows(&home, &cfg, ("darwin", "arm64"), &rows, &mut fetch).unwrap();
        assert_eq!(fetch_count, 0);

        let reloaded = read_valid_parakeet_coreml_sentinel(&home, "darwin", "arm64").unwrap();
        assert_eq!(reloaded.catalog_version(), artifact.version);
    }

    #[test]
    fn sentinel_cache_dir_differs_from_configured_skips_fetch() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let cache = temporary.path().join("cache");
        let other_cache = temporary.path().join("other_cache");
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(&other_cache).unwrap();
        let cfg = config(&cache);
        let artifact = row("model.bin", b"bytes");
        let rows = [&artifact];

        let sentinel = ParakeetCoremlSentinel::new(
            other_cache.clone(),
            "darwin",
            "arm64",
            FLUIDAUDIO_VERSION,
            artifact.version,
        );
        let sentinel_path = parakeet_coreml_sentinel_path(&home);
        write_sentinel(&sentinel_path, &sentinel).unwrap();

        let mut fetch_count = 0;
        let mut fetch = |_query: &RuntimeFetchQuery<'_>, _dest: &Path| {
            fetch_count += 1;
            Ok(())
        };

        refresh_with_rows(&home, &cfg, ("darwin", "arm64"), &rows, &mut fetch).unwrap();
        assert_eq!(fetch_count, 0);

        let reloaded = read_valid_parakeet_coreml_sentinel(&home, "darwin", "arm64").unwrap();
        assert_eq!(reloaded.cache_dir(), other_cache);
    }

    #[test]
    fn matching_digest_and_catalog_version_makes_zero_fetches() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let cache = temporary.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        let cfg = config(&cache);
        let artifact = row("model.bin", b"perfect-bytes");
        let rows = [&artifact];

        let target = parakeet_coreml_model_root(&cache);
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("model.bin"), b"perfect-bytes").unwrap();

        let sentinel = ParakeetCoremlSentinel::new(
            cache.clone(),
            "darwin",
            "arm64",
            FLUIDAUDIO_VERSION,
            artifact.version,
        );
        let sentinel_path = parakeet_coreml_sentinel_path(&home);
        write_sentinel(&sentinel_path, &sentinel).unwrap();

        let mut fetch_count = 0;
        let mut fetch = |_query: &RuntimeFetchQuery<'_>, _dest: &Path| {
            fetch_count += 1;
            Ok(())
        };

        refresh_with_rows(&home, &cfg, ("darwin", "arm64"), &rows, &mut fetch).unwrap();
        assert_eq!(fetch_count, 0);
    }

    #[test]
    fn incumbent_exists_sentinel_writer_fails_preserves_tree_and_sentinel() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let cache = temporary.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        let cfg = config(&cache);
        let artifact = row("model.bin", b"new-bytes");
        let rows = [&artifact];

        let target = parakeet_coreml_model_root(&cache);
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("old.bin"), b"old-bytes").unwrap();

        let old_sentinel = ParakeetCoremlSentinel::new(
            cache.clone(),
            "darwin",
            "arm64",
            FLUIDAUDIO_VERSION,
            "old-version",
        );
        let sentinel_path = parakeet_coreml_sentinel_path(&home);
        write_sentinel(&sentinel_path, &old_sentinel).unwrap();

        let mut fetch = |_query: &RuntimeFetchQuery<'_>, dest: &Path| {
            fs::write(dest, b"new-bytes")
                .map_err(|e| CoremlInstallError::new("io", e.to_string(), 74))
        };
        let mut publish = |staging: &Path, target: &Path| fs::rename(staging, target);
        let mut failing_write = |_path: &Path, _sentinel: &ParakeetCoremlSentinel| {
            Err(std::io::Error::other("injected sentinel failure"))
        };

        let result = download_and_commit_coreml(
            &home,
            &cfg,
            ("darwin", "arm64"),
            &rows,
            true,
            &mut fetch,
            &mut publish,
            &mut failing_write,
        );

        assert_eq!(result.unwrap_err().reason_code, "sentinel_write_failed");
        assert_eq!(fs::read(target.join("old.bin")).unwrap(), b"old-bytes");
        assert!(!target.join("model.bin").exists());
        let reloaded = read_valid_parakeet_coreml_sentinel(&home, "darwin", "arm64").unwrap();
        assert_eq!(reloaded.catalog_version(), "old-version");
    }

    #[test]
    fn fetch_fails_on_later_file_preserves_tree_and_sentinel() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let cache = temporary.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        let cfg = config(&cache);
        let artifact1 = row("file1.bin", b"bytes1");
        let artifact2 = row("file2.bin", b"bytes2");
        let rows = [&artifact1, &artifact2];

        let target = parakeet_coreml_model_root(&cache);
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("old.bin"), b"old-content").unwrap();

        let old_sentinel = ParakeetCoremlSentinel::new(
            cache.clone(),
            "darwin",
            "arm64",
            FLUIDAUDIO_VERSION,
            "old-v",
        );
        let sentinel_path = parakeet_coreml_sentinel_path(&home);
        write_sentinel(&sentinel_path, &old_sentinel).unwrap();

        let mut fetch_step = 0;
        let mut fetch = |_query: &RuntimeFetchQuery<'_>, dest: &Path| {
            fetch_step += 1;
            if fetch_step == 1 {
                fs::write(dest, b"bytes1")
                    .map_err(|e| CoremlInstallError::new("io", e.to_string(), 74))
            } else {
                Err(CoremlInstallError::new(
                    "download_failed",
                    "network dropped",
                    74,
                ))
            }
        };

        let error =
            refresh_with_rows(&home, &cfg, ("darwin", "arm64"), &rows, &mut fetch).unwrap_err();
        assert_eq!(error.reason_code, "download_failed");
        assert_eq!(fs::read(target.join("old.bin")).unwrap(), b"old-content");
        assert!(!target.join("file1.bin").exists());
        let reloaded = read_valid_parakeet_coreml_sentinel(&home, "darwin", "arm64").unwrap();
        assert_eq!(reloaded.catalog_version(), "old-v");
    }

    #[test]
    fn fetch_writes_matching_bytes_succeeds_and_second_refresh_is_noop() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let cache = temporary.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        let cfg = config(&cache);
        let artifact = row("model.bin", b"new-payload");
        let rows = [&artifact];

        let target = parakeet_coreml_model_root(&cache);
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("model.bin"), b"stale-payload").unwrap();

        let old_sentinel = ParakeetCoremlSentinel::new(
            cache.clone(),
            "darwin",
            "arm64",
            FLUIDAUDIO_VERSION,
            "old-version",
        );
        let sentinel_path = parakeet_coreml_sentinel_path(&home);
        write_sentinel(&sentinel_path, &old_sentinel).unwrap();

        let mut fetch_count = 0;
        let mut fetch = |_query: &RuntimeFetchQuery<'_>, dest: &Path| {
            fetch_count += 1;
            fs::write(dest, b"new-payload")
                .map_err(|e| CoremlInstallError::new("io", e.to_string(), 74))
        };

        refresh_with_rows(&home, &cfg, ("darwin", "arm64"), &rows, &mut fetch).unwrap();
        assert_eq!(fetch_count, 1);
        check_with_rows(&home, &cfg, "darwin", "arm64", &rows).unwrap();

        let mut second_fetch_count = 0;
        let mut second_fetch = |_query: &RuntimeFetchQuery<'_>, _dest: &Path| {
            second_fetch_count += 1;
            Ok(())
        };
        refresh_with_rows(&home, &cfg, ("darwin", "arm64"), &rows, &mut second_fetch).unwrap();
        assert_eq!(second_fetch_count, 0);
    }
}
