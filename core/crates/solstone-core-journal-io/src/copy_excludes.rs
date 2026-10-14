// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Journal-relative operational paths omitted from backups and portable copies.
//!
//! Raw content may have any plain filename, including writer-shaped temps and
//! locks. Keep ambiguous inert files: a name alone cannot establish ownership.

/// Root-relative patterns. `*` spans one component; `**` spans directories.
/// A matching directory also omits its descendants.
pub const COPY_EXCLUDES: &[&str] = &[
    "indexer",
    "cache",
    ".cache",
    "mcp-endpoint",
    "config/chatgpt-sign-in.json*",
    // Old per-day databases, without pruning a capture source named indexer.
    "chronicle/*/indexer/*.sqlite*",
    "imports/health-dedupe.sqlite*",
    // Segment deletion pivots, in flat and source-qualified day layouts.
    "chronicle/*/.removing_*",
    "chronicle/*/*/.removing_*",
    // These namespaces are application-owned, unlike raw segment filenames.
    "health/sync",
    "health/brain.json",
    "health/brain.log",
    "health/brain-fingerprint.key",
    "health/brain-refresh.lease",
    "health/supervisor.ready",
    "health/supervisor.start_time",
    "health/parakeet-cpp.placement",
    "health/scheduler.json",
    "health/**/*.sock",
    "health/**/*.pid",
    "health/**/*.port",
    "health/**/*.lock",
    "health/**/*.tmp",
    "health/**/.tmp*",
    // Atomic candidates can contain credentials before their final rename.
    "config/**/*.lock",
    "config/**/*.tmp",
    "config/**/.tmp*",
    "link/**/*.lock",
    "link/**/*.tmp",
    "link/**/.tmp*",
    "backup/**/*.lock",
    "backup/**/*.tmp",
    "backup/**/.tmp*",
];

/// Whether a slash-separated journal-relative path is operational copy state.
pub fn copy_excluded(member: &str) -> bool {
    let components: Vec<_> = member.split('/').collect();
    COPY_EXCLUDES.iter().any(|pattern| {
        let pattern: Vec<_> = pattern.split('/').collect();
        prefix_matches(&pattern, &components)
    })
}

fn prefix_matches(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => true,
        Some((&"**", rest)) => (0..=path.len()).any(|skip| prefix_matches(rest, &path[skip..])),
        Some((first, rest)) => path.split_first().is_some_and(|(name, tail)| {
            component_matches(first, name) && prefix_matches(rest, tail)
        }),
    }
}

fn component_matches(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = name.strip_prefix(first) else {
        return false;
    };
    let pieces: Vec<_> = parts.collect();
    let Some((last, middle)) = pieces.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        let Some(index) = rest.find(part) else {
            return false;
        };
        rest = &rest[index + part.len()..];
    }
    rest.ends_with(last)
}

#[cfg(test)]
mod tests {
    use super::copy_excluded;

    #[test]
    fn operational_paths_are_rooted_and_owner_names_survive() {
        for path in [
            "indexer/journal.sqlite-wal",
            "cache/providers/model",
            "chronicle/20260101/indexer/day.sqlite-shm",
            "health/brain-fingerprint.key",
            "health/callosum.sock",
            "health/locks/backup-activity.lock",
            "health/sync/state",
            "mcp-endpoint/pop.ed25519.pk8",
            "config/chatgpt-sign-in.json.unreadable-1",
            "config/.tmp_123_4.tmp",
            "config/_stage_123_4.tmp",
            "link/ca/.tmp_123_4.tmp",
            "backup/hosted/.tmp_123_4.tmp",
            "chronicle/20260101/source/.removing_120000_60/tombstone.json",
        ] {
            assert!(copy_excluded(path), "operational path included: {path}");
        }
        for path in [
            "entities/cache/entity.json",
            "facets/indexer/facet.json",
            "imports/item/source.key",
            "imports/item/cache/a.sqlite",
            "chronicle/20260101/indexer/120000_60/original.key",
            "chronicle/20260101/source/120000_60/brain.json",
            "chronicle/20260101/source/120000_60/audio.jsonl.lock",
            "chronicle/20260101/source/120000_60/.tmp_123_4.tmp",
            "entities/indexer/brain-fingerprint.key",
            "facets/cache/note.tmp",
            "health/retention.log",
            "health/pruning-runs/run.jsonl",
            "chronicle/20260101/health/talent-provenance/run.json",
        ] {
            assert!(!copy_excluded(path), "owner path excluded: {path}");
        }
    }
}
