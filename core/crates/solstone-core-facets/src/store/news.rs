// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::path::Path;

use solstone_core_journal_io::{path_lexists, read_text};

use super::error::FacetStoreError;
use super::paths::{FacetContentKind, content_file_path};

/// Read a news markdown file without interpretation.
pub fn read_news_file(
    journal_root: &Path,
    facet_dir: &str,
    relative_path: &str,
) -> Result<Option<String>, FacetStoreError> {
    let path = content_file_path(
        journal_root,
        facet_dir,
        FacetContentKind::News,
        relative_path,
    )?;
    if !path_lexists(&path)? {
        return Ok(None);
    }
    read_text(&path, String::new())
        .map(Some)
        .map_err(Into::into)
}
