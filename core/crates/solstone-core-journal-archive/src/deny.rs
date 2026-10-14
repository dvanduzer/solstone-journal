// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use solstone_core_journal_io::copy_excludes::{copy_excluded, copy_file_excluded};

/// Portable copies omit installation, service credentials and merge scratch.
/// Backups keep ordinary configuration and durable operational history.
const PORTABLE_TREES: &[&str] = &[
    "config",
    "link",
    "mcp-endpoint",
    "tokens",
    "awareness",
    "apps",
    "backup",
    "solstone",
    "imports/archive-merge-work",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DenyAction {
    Include,
    Skip,
    TreePrune,
}

pub(crate) fn deny_top_level(name: &str) -> DenyAction {
    if PORTABLE_TREES.contains(&name) {
        DenyAction::TreePrune
    } else if copy_excluded(name) {
        DenyAction::Skip
    } else {
        DenyAction::Include
    }
}

pub(crate) fn deny_member(member: &str, regular_file: bool) -> bool {
    copy_excluded(member)
        || (regular_file && copy_file_excluded(member))
        || PORTABLE_TREES.iter().any(|prefix| {
            member == *prefix
                || member
                    .strip_prefix(prefix)
                    .is_some_and(|tail| tail.starts_with('/'))
        })
}

#[cfg(test)]
mod tests {
    use super::{DenyAction, deny_member, deny_top_level};

    #[test]
    fn portable_tree_prunes_are_root_relative() {
        assert_eq!(deny_top_level("config"), DenyAction::TreePrune);
        assert_eq!(deny_top_level("indexer"), DenyAction::Skip);
        for path in [
            "config/journal.json",
            "mcp-endpoint/pop.ed25519.pk8",
            "backup/hosted/binding.json",
            "imports/archive-merge-work/extract/x",
        ] {
            assert!(deny_member(path, true), "{path}");
        }
        for path in [
            "entities/cache/entity.json",
            "facets/indexer/facet.json",
            "imports/batch/original.key",
            "imports/batch/config/journal.json",
            "chronicle/20260101/indexer/120000_60/note.tmp",
            "health/retention.log",
            "health/pruning-runs/x.jsonl",
        ] {
            assert!(!deny_member(path, true), "{path}");
        }
    }
}
