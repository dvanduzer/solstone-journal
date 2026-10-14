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
    // Compare as paths, component by component. The Windows and macOS process
    // temporary directories end with a separator that `parent()` never
    // returns, so a raw `OsStr` comparison refused every desktop appraisal.
    if path.parent() != Some(base) {
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
        // Windows separator and drive/stream marker: never a nested,
        // drive-relative or alternate-data-stream name on any platform.
        && !bytes.contains(&b'\\')
        && !bytes.contains(&b':')
        && !bytes.contains(&0)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::staging_child;
    use crate::error::GpuAppraisalReason;

    /// The process temporary directory as Windows (`GetTempPath2W`) and macOS
    /// (`TMPDIR`) report it: with a trailing separator.
    #[cfg(windows)]
    const TRAILING: &str = r"C:\Users\owner\AppData\Local\Temp\";
    #[cfg(not(windows))]
    const TRAILING: &str = "/var/folders/kh/owner/T/";
    #[cfg(windows)]
    const PLAIN: &str = r"C:\Users\owner\AppData\Local\Temp";
    #[cfg(not(windows))]
    const PLAIN: &str = "/tmp";

    const NAME: &str = "solstone-nvattest-evidence-1-2-3.json";

    fn admitted(base: &str) -> PathBuf {
        let base = Path::new(base);
        let path = staging_child(base, NAME).expect("a single file name stages under its base");
        assert_eq!(path.parent(), Some(base));
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some(NAME));
        path
    }

    #[test]
    fn base_with_trailing_separator_stages_its_child() {
        admitted(TRAILING);
    }

    #[test]
    fn base_without_trailing_separator_stages_its_child() {
        admitted(PLAIN);
    }

    #[test]
    fn trailing_separator_does_not_change_the_staged_path() {
        assert_eq!(
            admitted(TRAILING),
            admitted(TRAILING.trim_end_matches(['/', '\\']))
        );
    }

    #[test]
    fn process_temporary_directory_stages_its_child() {
        let base = std::env::temp_dir();
        let path = staging_child(&base, NAME).expect("the process temp directory admits a child");
        assert_eq!(path.parent(), Some(base.as_path()));
    }

    #[test]
    fn names_that_are_not_one_plain_component_are_refused() {
        for name in [
            "", ".", "..", "a/b", "/a", "a\\b", "\\a", "..\\a", "c:a", "C:\\a", "a:b", "a\0b",
        ] {
            assert!(
                matches!(
                    staging_child(Path::new(TRAILING), name),
                    Err(GpuAppraisalReason::NvattestIntegrityFailed)
                ),
                "{name:?} must be refused"
            );
        }
    }
}
