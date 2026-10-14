// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Appraisal input directory shared by the production temporary-file writers.

use std::path::{Path, PathBuf};

use crate::error::GpuAppraisalReason;

use super::binary::{android_authority_required, map_android_authority};

pub(super) struct AppraisalStaging {
    pub(super) directory: PathBuf,
    /// Unix evidence files in this directory are created mode `0600`.
    pub(super) private_files: bool,
}

/// Directory for one appraisal's private inputs.
///
/// Android, and any process that has admitted Android service tools, uses the
/// admitted resource root. The installed authority is revalidated before that
/// directory is returned. Desktop and iOS processes with no Android authority
/// keep the process temporary directory.
pub(super) fn appraisal_staging() -> Result<AppraisalStaging, GpuAppraisalReason> {
    let private_files = android_authority_required();
    let directory = if private_files {
        crate::android_service_tools::create_android_appraisal_staging()
            .map_err(map_android_authority)?
    } else {
        std::env::temp_dir()
    };
    Ok(AppraisalStaging {
        directory,
        private_files,
    })
}

pub(super) fn staging_child(base: &Path, name: &str) -> Result<PathBuf, GpuAppraisalReason> {
    if !single_file_component(name) {
        return Err(GpuAppraisalReason::NvattestIntegrityFailed);
    }
    let path = base.join(name);
    if path.parent().map(Path::as_os_str) != Some(base.as_os_str()) {
        return Err(GpuAppraisalReason::NvattestIntegrityFailed);
    }
    Ok(path)
}

fn single_file_component(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes != b"."
        && bytes != b".."
        && !bytes.contains(&b'/')
        && !bytes.contains(&0)
}
