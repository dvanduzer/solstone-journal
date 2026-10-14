// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#[path = "archive_merge.rs"]
mod archive_merge;
#[cfg(unix)]
#[path = "source_immutability.rs"]
mod source_immutability;
