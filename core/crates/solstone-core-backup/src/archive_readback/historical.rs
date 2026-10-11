// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#[cfg(unix)]
use super::DirectInvocation;
use super::{ExactArchiveFile, ReadOperation, ReadbackError, full_hex_identity, plain_name};
#[cfg(any(unix, test))]
use super::{Value, exact_archive_path};
#[cfg(any(unix, test))]
use std::collections::BTreeMap;
use std::collections::BTreeSet;

pub struct HistoricalArchiveFile<'s> {
    pub name: &'s str,
    pub bytes: u64,
    pub sha256: &'s str,
}

pub struct HistoricalSnapshotFiles {
    pub snapshot_id: String,
    pub archive_parent: String,
    pub files: Vec<ExactArchiveFile>,
}

fn valid_selection(selector: &str, files: &[HistoricalArchiveFile<'_>]) -> bool {
    let mut names = BTreeSet::new();
    !selector.is_empty()
        && selector.len() <= 64
        && selector
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && !files.is_empty()
        && files.iter().all(|file| {
            plain_name(file.name) && full_hex_identity(file.sha256) && names.insert(file.name)
        })
}

impl ReadOperation<'_> {
    pub fn discover_historical_snapshot(
        &mut self,
        snapshot_selector: &str,
        files: &[HistoricalArchiveFile<'_>],
    ) -> Result<HistoricalSnapshotFiles, ReadbackError> {
        self.discover_historical_snapshot_inner(snapshot_selector, files)
            .inspect_err(|reason| self.execution.borrow_mut().fail(*reason))
    }

    fn discover_historical_snapshot_inner(
        &mut self,
        snapshot_selector: &str,
        files: &[HistoricalArchiveFile<'_>],
    ) -> Result<HistoricalSnapshotFiles, ReadbackError> {
        if !valid_selection(snapshot_selector, files) {
            return Err(ReadbackError::InvalidSelection);
        }
        #[cfg(not(unix))]
        {
            Err(ReadbackError::Unsupported)
        }
        #[cfg(unix)]
        {
            self.revalidate_local()?;
            self.check_repository()?;
            let child = self.query(vec![
                "ls".into(),
                "--json".into(),
                snapshot_selector.into(),
                "--no-cache".into(),
            ])?;
            let observed = historical_snapshot_files(
                &child.output().stdout,
                self.repository_id(),
                snapshot_selector,
                files,
            )?;
            self.check_repository()?;
            self.revalidate_local()?;
            self.execution
                .borrow_mut()
                .accept(DirectInvocation::List, child);
            Ok(observed)
        }
    }
}

#[cfg(any(unix, test))]
struct ParentFiles {
    paths: BTreeMap<usize, String>,
    invalid: bool,
}

#[cfg(any(unix, test))]
fn historical_snapshot_files(
    bytes: &[u8],
    repository_id: &str,
    selector: &str,
    files: &[HistoricalArchiveFile<'_>],
) -> Result<HistoricalSnapshotFiles, ReadbackError> {
    if !valid_selection(selector, files) {
        return Err(ReadbackError::InvalidSelection);
    }
    let unavailable = ReadbackError::SnapshotNodeUnavailable;
    let mut values = serde_json::Deserializer::from_slice(bytes).into_iter::<Value>();
    let header = values.next().ok_or(unavailable)?.map_err(|_| unavailable)?;
    let snapshot_id = header
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| full_hex_identity(id) && id.starts_with(selector))
        .ok_or(unavailable)?
        .to_owned();
    if header.get("message_type").and_then(Value::as_str) != Some("snapshot") {
        return Err(unavailable);
    }
    let names: BTreeMap<_, _> = files
        .iter()
        .enumerate()
        .map(|(index, file)| (file.name, index))
        .collect();
    let mut parents = BTreeMap::<String, ParentFiles>::new();
    for value in values {
        let value = value.map_err(|_| unavailable)?;
        if value.get("message_type").and_then(Value::as_str) != Some("node") {
            return Err(unavailable);
        }
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .ok_or(unavailable)?;
        let Some(&index) = names.get(name) else {
            continue;
        };
        let path = value
            .get("path")
            .and_then(Value::as_str)
            .ok_or(unavailable)?;
        if !exact_archive_path(path) || path.rsplit('/').next() != Some(name) {
            return Err(unavailable);
        }
        let (parent, _) = path.rsplit_once('/').ok_or(unavailable)?;
        let parent = if parent.is_empty() { "/" } else { parent };
        let group = parents
            .entry(parent.to_owned())
            .or_insert_with(|| ParentFiles {
                paths: BTreeMap::new(),
                invalid: false,
            });
        if group.paths.contains_key(&index)
            || value.get("type").and_then(Value::as_str) != Some("file")
            || value.get("size").and_then(Value::as_u64) != Some(files[index].bytes)
        {
            group.invalid = true;
        }
        group.paths.insert(index, path.to_owned());
    }
    let mut complete = parents
        .into_iter()
        .filter(|(_, group)| !group.invalid && group.paths.len() == files.len());
    let (archive_parent, mut group) = complete.next().ok_or(unavailable)?;
    if complete.next().is_some() {
        return Err(unavailable);
    }
    let observed = files
        .iter()
        .enumerate()
        .map(|(index, expected)| ExactArchiveFile {
            repository_id: repository_id.to_owned(),
            snapshot_id: snapshot_id.clone(),
            name: expected.name.to_owned(),
            archive_path: group.paths.remove(&index).expect("complete parent group"),
            bytes: expected.bytes,
            sha256: expected.sha256.to_owned(),
        })
        .collect();
    Ok(HistoricalSnapshotFiles {
        snapshot_id,
        archive_parent,
        files: observed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: &str = "61cc73a4bf7d520b6c28f68ab7f5402df357899c4613a4a6ea6dd9ea00b1804c";
    const SHA: &str = "a111111111111111111111111111111111111111111111111111111111111111";
    const SCREEN_SHA: &str = "b222222222222222222222222222222222222222222222222222222222222222";
    fn manifest() -> [HistoricalArchiveFile<'static>; 2] {
        [
            HistoricalArchiveFile {
                name: "audio.flac",
                bytes: 37,
                sha256: SHA,
            },
            HistoricalArchiveFile {
                name: "screen.webp",
                bytes: 19,
                sha256: SCREEN_SHA,
            },
        ]
    }
    fn header() -> Value {
        json!({"message_type":"snapshot", "id":ID, "short_id":"61cc73a4"})
    }
    fn node(parent: &str, name: &str, bytes: u64) -> Value {
        json!({"message_type":"node", "name":name, "path":format!("{parent}/{name}"), "type":"file", "size":bytes})
    }
    fn encode(values: &[Value]) -> Vec<u8> {
        values
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes()
    }
    fn parse(values: &[Value]) -> Result<HistoricalSnapshotFiles, ReadbackError> {
        historical_snapshot_files(&encode(values), &"b".repeat(64), "61cc", &manifest())
    }
    #[test]
    fn complete_parent_preserves_order_and_ignores_unrelated_same_basenames() {
        let values = [
            header(),
            node("/other", "audio.flac", 37),
            node("/chosen", "screen.webp", 19),
            node("/wrong-size", "audio.flac", 40),
            node("/wrong-size", "screen.webp", 19),
            node("/chosen", "audio.flac", 37),
        ];
        let observed = parse(&values).unwrap();
        assert_eq!(observed.snapshot_id, ID);
        assert_eq!(observed.archive_parent, "/chosen");
        assert_eq!(
            observed
                .files
                .iter()
                .map(|file| file.name.as_str())
                .collect::<Vec<_>>(),
            ["audio.flac", "screen.webp"]
        );
        assert_eq!(observed.files[0].archive_path, "/chosen/audio.flac");
        assert_eq!((observed.files[0].bytes, observed.files[1].bytes), (37, 19));
        assert_eq!(observed.files[0].sha256, SHA);
        assert_eq!(observed.files[1].sha256, SCREEN_SHA);
        assert!(
            observed
                .files
                .iter()
                .all(|file| file.repository_id == "b".repeat(64))
        );
    }
    #[test]
    fn exact_full_id_and_snapshot_volume_spelling_are_retained() {
        let parent = r"/\\?\Volume{12345678-1234-1234-1234-123456789abc}/Users/owner/segment";
        let values = [
            header(),
            node(parent, "audio.flac", 37),
            node(parent, "screen.webp", 19),
        ];
        let observed =
            historical_snapshot_files(&encode(&values), &"b".repeat(64), ID, &manifest()).unwrap();
        assert_eq!(observed.archive_parent, parent);
        assert_eq!(
            observed.files[1].archive_path,
            format!("{parent}/screen.webp")
        );
        for selector in ["f", &"f".repeat(64)] {
            assert!(
                historical_snapshot_files(&encode(&values), &"b".repeat(64), selector, &manifest())
                    .is_err()
            );
        }
    }
    #[test]
    fn incomplete_ambiguous_and_invalid_parent_groups_refuse() {
        let first = node("/first", "audio.flac", 37);
        let second = node("/first", "screen.webp", 19);
        let mut cases = vec![
            vec![header(), first.clone()],
            vec![
                header(),
                first.clone(),
                node("/elsewhere", "screen.webp", 19),
            ],
            vec![
                header(),
                first.clone(),
                second.clone(),
                node("/second", "audio.flac", 37),
                node("/second", "screen.webp", 19),
            ],
            vec![header(), first.clone(), second.clone(), first.clone()],
            vec![header(), first.clone(), node("/first", "screen.webp", 20)],
        ];
        let mut nonfile = second.clone();
        nonfile["type"] = json!("symlink");
        cases.push(vec![header(), first.clone(), nonfile.clone()]);
        for values in cases {
            assert!(matches!(
                parse(&values),
                Err(ReadbackError::SnapshotNodeUnavailable)
            ));
        }
        let valid = [
            header(),
            first,
            nonfile,
            node("/valid", "audio.flac", 37),
            node("/valid", "screen.webp", 19),
        ];
        assert_eq!(parse(&valid).unwrap().archive_parent, "/valid");
    }
    #[test]
    fn whole_stream_and_input_validation_reject_hidden_or_malformed_evidence() {
        let good = [
            header(),
            node("/chosen", "audio.flac", 37),
            node("/chosen", "screen.webp", 19),
        ];
        for selector in [
            "",
            "latest",
            "61CC",
            "61cc:sub",
            " 61cc",
            "--61cc",
            &"1".repeat(65),
        ] {
            assert!(matches!(
                historical_snapshot_files(&encode(&good), "unused", selector, &manifest()),
                Err(ReadbackError::InvalidSelection)
            ));
        }
        let duplicate = [
            HistoricalArchiveFile {
                name: "audio.flac",
                bytes: 37,
                sha256: SHA,
            },
            HistoricalArchiveFile {
                name: "audio.flac",
                bytes: 19,
                sha256: SHA,
            },
        ];
        assert!(matches!(
            historical_snapshot_files(&encode(&good), "unused", "61cc", &duplicate),
            Err(ReadbackError::InvalidSelection)
        ));
        let mut trailing = encode(&good);
        trailing.extend_from_slice(b"\n{bad");
        assert!(historical_snapshot_files(&trailing, "unused", "61cc", &manifest()).is_err());
        let missing_name = json!({"message_type":"node","path":"/other/hidden", "type":"file"});
        let invalid_path = json!({"message_type":"node","name":"audio.flac","path":"/chosen/../audio.flac","type":"file","size":37});
        for tail in [
            header(),
            missing_name,
            json!({"message_type":"unknown"}),
            invalid_path,
        ] {
            let mut values = good.to_vec();
            values.push(tail);
            assert!(parse(&values).is_err());
        }
        assert!(parse(&good[1..]).is_err());
    }
}
