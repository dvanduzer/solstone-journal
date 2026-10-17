// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::ffi::OsStr;

use solstone_core_journal_io::copy_excludes::{copy_excluded, copy_file_excluded};
use solstone_core_journal_io::operational_log::{
    OplogFormat, OplogNameClassification, classify_oplog_name,
};

/// Portable export includes owner trees. It omits installation identity,
/// service credentials, and the throwaway paths listed here.
/// Backups keep ordinary configuration through a separate copy policy.
const PRUNED_ROOTS: &[&str] = &["mcp-endpoint", "peers"];

const EXTRA_PREFIXES: &[&str] = &[
    "imports/archive-merge-work",
    "awareness/activity_state",
    "awareness/activity_settle",
    "apps/support/portal",
    ".agents/skills/journal",
    ".agents/skills/solstone",
    ".claude/skills/journal",
    ".claude/skills/solstone",
];

const OMITTED_REGULAR_FILES: &[&str] = &[
    "config/journal.json",
    "config/push-vapid.json",
    "config/push-registry.json",
    "backup/hosted/binding.json",
    "awareness/discovery_clusters.json",
    "awareness/activity_state.json",
    "health/heartbeat.log",
    "health/steward.log",
    "health/service.log",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DenyAction {
    Include,
    Skip,
    TreePrune,
}

pub(crate) fn deny_top_level(name: &str) -> DenyAction {
    let original_name = name;
    #[cfg(windows)]
    let name = name.to_ascii_lowercase();
    #[cfg(windows)]
    let name = name.as_str();
    if PRUNED_ROOTS.contains(&name) {
        DenyAction::TreePrune
    } else if copy_excluded(original_name) {
        DenyAction::Skip
    } else {
        DenyAction::Include
    }
}

pub(crate) fn deny_member(member: &str, regular_file: bool) -> bool {
    let original_member = member;
    if copy_excluded(member) || (regular_file && copy_file_excluded(member)) {
        return true;
    }
    #[cfg(windows)]
    if windows_credential_state(member) {
        return true;
    }
    #[cfg(windows)]
    let member = member.to_ascii_lowercase();
    #[cfg(windows)]
    let member = member.as_str();
    #[cfg(windows)]
    if member.starts_with("config/chatgpt-sign-in.json") {
        return true;
    }
    // Device labels are owner settings; all other link state belongs to this installation.
    if (member == "link" && regular_file)
        || (member.starts_with("link/")
            && (member != "link/client-descriptions.json" || !regular_file))
    {
        return true;
    }
    if PRUNED_ROOTS
        .iter()
        .chain(EXTRA_PREFIXES.iter())
        .any(|prefix| {
            member == *prefix
                || member
                    .strip_prefix(prefix)
                    .is_some_and(|tail| tail.starts_with('/'))
        })
    {
        return true;
    }
    if !regular_file {
        return false;
    }
    if OMITTED_REGULAR_FILES.contains(&member) {
        return true;
    }
    if is_root_supervisor_log(member) {
        return true;
    }
    if is_generated_leaf(member) {
        return true;
    }
    if is_canonical_oplog_log(original_member) {
        return true;
    }
    false
}

#[cfg(windows)]
fn windows_credential_state(member: &str) -> bool {
    let Some((root, tail)) = member.split_once('/') else {
        return false;
    };
    let root = root.to_ascii_lowercase();
    // Recognize aliases of credential namespaces without changing shared leaf patterns.
    ["config", "link", "backup"].contains(&root.as_str())
        && copy_excluded(&format!("{root}/{tail}"))
}

fn is_root_supervisor_log(member: &str) -> bool {
    if member == "supervisor.log" {
        return true;
    }
    if let Some(tail) = member.strip_prefix("supervisor.") {
        return !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit());
    }
    false
}

fn is_generated_leaf(member: &str) -> bool {
    if member == "task_log.txt" || member == "stats.json" {
        return true;
    }
    let parts: Vec<&str> = member.split('/').collect();
    if parts.len() == 3
        && parts[0] == "chronicle"
        && is_eight_digit_day(parts[1])
        && (parts[2] == "task_log.txt" || parts[2] == "stats.json")
    {
        return true;
    }
    false
}

fn is_canonical_oplog_log(member: &str) -> bool {
    let parts: Vec<&str> = member.split('/').collect();
    if parts.len() == 4
        && is_eight_digit_day(parts[1])
        && if cfg!(windows) {
            parts[0].eq_ignore_ascii_case("chronicle") && parts[2].eq_ignore_ascii_case("health")
        } else {
            parts[0] == "chronicle" && parts[2] == "health"
        }
    {
        let leaf = parts[3];
        if let OplogNameClassification::Candidate(Ok(name)) = classify_oplog_name(OsStr::new(leaf))
        {
            return name.format() == OplogFormat::Log;
        }
    }
    false
}

fn is_eight_digit_day(day: &str) -> bool {
    day.len() == 8 && day.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use solstone_core_journal_io::operational_log::{
        OplogFormat, OplogNameClassification, classify_oplog_name,
    };

    use super::{DenyAction, deny_member, deny_top_level};

    const LOG_LEAF: &str = "oplog--cortex~1ee11af4ed5d63caf142a30a96ba124b--20260901T164233.381904Z--8f03cabead7e441d83f6c92b2d89a021--daily-think~7df259e6285645a5f9ea769caa484e07.log";
    const JSONL_LEAF: &str = "oplog--cortex~1ee11af4ed5d63caf142a30a96ba124b--20260901T164233.381904Z--8f03cabead7e441d83f6c92b2d89a021--daily-think~7df259e6285645a5f9ea769caa484e07.jsonl";
    const MALFORMED_LEAF: &str = "oplog--not-a-name.log";

    #[test]
    fn portable_tree_prunes_are_root_relative() {
        assert!(matches!(
            classify_oplog_name(OsStr::new(LOG_LEAF)),
            OplogNameClassification::Candidate(Ok(name)) if name.format() == OplogFormat::Log
        ));
        assert!(matches!(
            classify_oplog_name(OsStr::new(JSONL_LEAF)),
            OplogNameClassification::Candidate(Ok(name)) if name.format() == OplogFormat::Jsonl
        ));
        assert!(matches!(
            classify_oplog_name(OsStr::new(MALFORMED_LEAF)),
            OplogNameClassification::Candidate(Err(_))
        ));

        for root in [
            "config",
            "tokens",
            "awareness",
            "apps",
            "backup",
            "solstone",
            "link",
        ] {
            assert_eq!(deny_top_level(root), DenyAction::Include, "{root}");
        }
        for root in ["mcp-endpoint", "peers"] {
            assert_eq!(deny_top_level(root), DenyAction::TreePrune, "{root}");
        }
        assert_eq!(deny_top_level("indexer"), DenyAction::Skip);

        let canonical_log_path = format!("chronicle/20260101/health/{LOG_LEAF}");
        let denied = [
            "config/journal.json",
            "config/push-vapid.json",
            "config/push-registry.json",
            "backup/hosted/binding.json",
            "awareness/discovery_clusters.json",
            "awareness/activity_state.json",
            "health/heartbeat.log",
            "health/steward.log",
            "health/service.log",
            "imports/archive-merge-work/extract/x",
            "peers/remote/client.key",
            "mcp-endpoint/pop.ed25519.pk8",
            "link/client.json",
            "link/service-ca.key",
            "link",
            ".agents/skills/journal",
            "apps/support/portal/index.html",
            "supervisor.log",
            "supervisor.1",
            "supervisor.12",
            "task_log.txt",
            "stats.json",
            "chronicle/20260101/task_log.txt",
            "chronicle/20260101/stats.json",
            &canonical_log_path,
        ];
        for path in denied {
            assert!(deny_member(path, true), "{path}");
        }

        let canonical_jsonl_path = format!("chronicle/20260101/health/{JSONL_LEAF}");
        let malformed_log_path = format!("chronicle/20260101/health/{MALFORMED_LEAF}");
        let nested_log_path = format!("chronicle/20260101/health/nested/{LOG_LEAF}");
        let kept = [
            "imports/batch/config/journal.json",
            "imports/item/config/journal.json",
            "imports/item/peers/original.txt",
            "health/retention.log",
            "health/pruning-runs/x.jsonl",
            "entities/cache/entity.json",
            "link/client-descriptions.json",
            "supervisor.log.1",
            "supervisor.1a",
            "chronicle/20260101/supervisor.log",
            "chronicle/not-a-day/task_log.txt",
            "imports/item/stats.json",
            "chronicle/20260101/health/heartbeat.log",
            &canonical_jsonl_path,
            &malformed_log_path,
            &nested_log_path,
        ];
        for path in kept {
            assert!(!deny_member(path, true), "{path}");
        }

        assert!(!deny_member("stats.json", false));
        assert!(!deny_member("link", false));
        assert!(deny_member("link/client-descriptions.json", false));
        assert!(!deny_member("supervisor.log", false));
        assert!(!deny_member(&canonical_log_path, false));
    }
}
