// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::BTreeMap;
use std::path::Path;

use solstone_core_segment::{StreamAllocationBase, list_stream_records};
use solstone_core_sol_link::client_description_store::read_descriptions;
use solstone_core_sol_link::ledger::{AuthorizedClientsRead, ClientEntry, read_authorized_clients};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceWord {
    Watch,
    Terminal,
    Phone,
    Tablet,
    Computer,
    Mac,
    Iphone,
    Ipad,
    Browser,
}

impl DeviceWord {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Watch => "watch",
            Self::Terminal => "terminal",
            Self::Phone => "phone",
            Self::Tablet => "tablet",
            Self::Computer => "computer",
            Self::Mac => "mac",
            Self::Iphone => "iphone",
            Self::Ipad => "ipad",
            Self::Browser => "browser",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WhatWord {
    Audio,
    Screen,
    Camera,
    Location,
}

impl WhatWord {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Screen => "screen",
            Self::Camera => "camera",
            Self::Location => "location",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProvenancePart {
    Import {
        source: String,
    },
    Agent,
    Bound {
        device: Option<DeviceWord>,
        what: Option<WhatWord>,
        identity: Option<String>,
    },
    Unbound {
        device: DeviceWord,
        identity: Option<String>,
    },
    Unknown,
}

impl ProvenancePart {
    pub fn device_word(&self) -> Option<DeviceWord> {
        match self {
            Self::Bound { device, .. } => *device,
            Self::Unbound { device, .. } => Some(*device),
            _ => None,
        }
    }

    pub fn identity(&self) -> Option<&str> {
        match self {
            Self::Bound { identity, .. } => identity.as_deref(),
            Self::Unbound { identity, .. } => identity.as_deref(),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LabelTableStatus {
    Failed,
    Partial,
    Complete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamLabelRow {
    pub name: String,
    pub part: ProvenancePart,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamLabelTable {
    pub status: LabelTableStatus,
    pub rows: Vec<StreamLabelRow>,
}

pub fn load_stream_labels(journal: &Path) -> StreamLabelTable {
    let records = match list_stream_records(journal) {
        Ok(records) => records,
        Err(_) => {
            return StreamLabelTable {
                status: LabelTableStatus::Failed,
                rows: Vec::new(),
            };
        }
    };

    let mut partial = false;

    let descriptions = match read_descriptions(journal) {
        Ok(descriptions) => descriptions,
        Err(_) => {
            partial = true;
            BTreeMap::new()
        }
    };

    let ledger_path = journal.join("link/authorized_clients.json");
    let ledger_clients: BTreeMap<String, ClientEntry> = match read_authorized_clients(&ledger_path)
    {
        AuthorizedClientsRead::Present(clients) => clients
            .into_iter()
            .map(|entry| (entry.fingerprint.clone(), entry))
            .collect(),
        AuthorizedClientsRead::Missing => BTreeMap::new(),
        AuthorizedClientsRead::Unreadable
        | AuthorizedClientsRead::Malformed
        | AuthorizedClientsRead::DuplicateCid => {
            partial = true;
            BTreeMap::new()
        }
    };

    let mut rows = Vec::with_capacity(records.len());
    for (stem, record) in records {
        let part = classify_record(&stem, &record, &descriptions, &ledger_clients);
        rows.push(StreamLabelRow { name: stem, part });
    }

    StreamLabelTable {
        status: if partial {
            LabelTableStatus::Partial
        } else {
            LabelTableStatus::Complete
        },
        rows,
    }
}

fn classify_record(
    stem: &str,
    record: &solstone_core_segment::StreamRecord,
    descriptions: &BTreeMap<
        String,
        solstone_core_sol_link::client_description::StoredClientDescription,
    >,
    ledger_clients: &BTreeMap<String, ClientEntry>,
) -> ProvenancePart {
    // 1. Key import.<source> -> Import { source }
    if let Some(source) = stem.strip_prefix("import.") {
        return ProvenancePart::Import {
            source: source.to_owned(),
        };
    }

    // 2. record.kind == "agent_memory" -> Agent
    if record.kind == "agent_memory" {
        return ProvenancePart::Agent;
    }

    // 3. Bound when cid and source are both Some (Some("") is bound)
    if let (Some(cid), Some(source)) = (record.cid.as_deref(), record.source.as_deref()) {
        let description = descriptions.get(cid);
        let ledger_entry = ledger_clients.get(cid);

        let (device, what, identity) =
            classify_bound_source(source, record, description, ledger_entry);
        return ProvenancePart::Bound {
            device,
            what,
            identity,
        };
    }

    // 4. Otherwise: unbound
    if record.kind == "browser" {
        return ProvenancePart::Unbound {
            device: DeviceWord::Browser,
            identity: None,
        };
    }

    if !stem.contains('.') && stem != "_default" {
        let description = record.cid.as_deref().and_then(|cid| descriptions.get(cid));
        let ledger_entry = record
            .cid
            .as_deref()
            .and_then(|cid| ledger_clients.get(cid));
        let platform = resolve_first_hit_platform(record, description, ledger_entry);
        let device = match platform {
            Some("macos") => Some(DeviceWord::Mac),
            Some("linux") | Some("windows") => Some(DeviceWord::Computer),
            _ => None,
        };
        if let Some(device) = device {
            let ident = stem.trim();
            let identity = if ident.is_empty() {
                None
            } else {
                Some(ident.to_owned())
            };
            return ProvenancePart::Unbound { device, identity };
        }
        return ProvenancePart::Unknown;
    }

    let dots = stem.chars().filter(|&c| c == '.').count();
    if dots == 1 {
        let Some((host, qualifier)) = stem.split_once('.') else {
            return ProvenancePart::Unknown;
        };
        let device = match qualifier {
            "tmux" => Some(DeviceWord::Terminal),
            "watch" => Some(DeviceWord::Watch),
            "mobile" | "phone" => Some(DeviceWord::Phone),
            "browser" => Some(DeviceWord::Browser),
            _ => None,
        };
        if let Some(device) = device {
            let host = host.trim();
            let identity = if host.is_empty() {
                None
            } else {
                Some(host.to_owned())
            };
            return ProvenancePart::Unbound { device, identity };
        }
        return ProvenancePart::Unknown;
    }

    // 5. _default and anything else -> Unknown
    ProvenancePart::Unknown
}

fn resolve_first_hit_platform<'a>(
    record: &'a solstone_core_segment::StreamRecord,
    description: Option<&'a solstone_core_sol_link::client_description::StoredClientDescription>,
    ledger_entry: Option<&'a ClientEntry>,
) -> Option<&'a str> {
    if let Some(reported) = description
        .and_then(|d| d.reported.as_ref())
        .and_then(|r| r.platform.as_deref())
        .filter(|s| !s.is_empty())
    {
        return Some(reported);
    }
    if let Some(plat) = ledger_entry.and_then(|e| e.platform) {
        return Some(plat.as_wire());
    }
    if let Some(allocation) = record
        .allocation
        .as_ref()
        .filter(|a| a.base == StreamAllocationBase::Platform)
    {
        return Some(allocation.base_input.as_str());
    }
    record.platform.as_deref().filter(|s| !s.is_empty())
}

fn classify_bound_source(
    source: &str,
    record: &solstone_core_segment::StreamRecord,
    description: Option<&solstone_core_sol_link::client_description::StoredClientDescription>,
    ledger_entry: Option<&ClientEntry>,
) -> (Option<DeviceWord>, Option<WhatWord>, Option<String>) {
    let identity = bound_identity(description, ledger_entry);

    let (device, what) = match source {
        "watch-audio" => (Some(DeviceWord::Watch), Some(WhatWord::Audio)),
        "tmux" => (Some(DeviceWord::Terminal), None),
        source if source.starts_with("browser") => (Some(DeviceWord::Browser), None),
        "audio" | "screen" | "camera" | "location" | "mobile-segment" | "mobile" => {
            let device_word = resolve_platform_device_word(record, description, ledger_entry);
            let what_word = match source {
                "audio" => Some(WhatWord::Audio),
                "screen" => Some(WhatWord::Screen),
                "camera" => Some(WhatWord::Camera),
                "location" => Some(WhatWord::Location),
                _ => None,
            };
            (device_word, what_word)
        }
        _ => (None, None),
    };

    (device, what, identity)
}

fn resolve_platform_device_word(
    record: &solstone_core_segment::StreamRecord,
    description: Option<&solstone_core_sol_link::client_description::StoredClientDescription>,
    ledger_entry: Option<&ClientEntry>,
) -> Option<DeviceWord> {
    let platform = resolve_first_hit_platform(record, description, ledger_entry);

    let device_type = description
        .and_then(|d| d.reported.as_ref())
        .and_then(|r| r.device_type.as_deref())
        .filter(|s| !s.is_empty());

    match platform {
        Some("macos") => Some(DeviceWord::Mac),
        Some("ios") => match device_type {
            Some("tablet") => Some(DeviceWord::Ipad),
            Some("phone") => Some(DeviceWord::Iphone),
            _ => None,
        },
        Some("android") => match device_type {
            Some("tablet") => Some(DeviceWord::Tablet),
            Some("phone") => Some(DeviceWord::Phone),
            _ => None,
        },
        Some("windows") | Some("linux") => match device_type {
            Some("terminal") => Some(DeviceWord::Terminal),
            Some("phone") => Some(DeviceWord::Phone),
            Some("tablet") => Some(DeviceWord::Tablet),
            _ => Some(DeviceWord::Computer),
        },
        _ => match device_type {
            Some("desktop") | Some("laptop") => Some(DeviceWord::Computer),
            Some("phone") => Some(DeviceWord::Phone),
            Some("tablet") => Some(DeviceWord::Tablet),
            Some("terminal") => Some(DeviceWord::Terminal),
            _ => None,
        },
    }
}

fn bound_identity(
    description: Option<&solstone_core_sol_link::client_description::StoredClientDescription>,
    ledger_entry: Option<&ClientEntry>,
) -> Option<String> {
    if let Some(owner) = description
        .and_then(|d| d.owner_label.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(owner.to_owned());
    }

    if let Some(reported_name) = description
        .and_then(|d| d.reported.as_ref())
        .and_then(|r| r.name.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(reported_name.to_owned());
    }

    if let Some(entry) = ledger_entry {
        let base = entry.base_label().trim();
        if !base.is_empty() {
            if entry.label_ordinal != 1 {
                return Some(format!("{base} {}", entry.label_ordinal));
            } else {
                return Some(base.to_owned());
            }
        }
    }

    None
}

pub fn resolve_stream_label(table: &StreamLabelTable, name: &str) -> ProvenancePart {
    if table.status == LabelTableStatus::Failed {
        return ProvenancePart::Unknown;
    }
    table
        .rows
        .iter()
        .find(|row| row.name == name)
        .map(|row| row.part.clone())
        .unwrap_or(ProvenancePart::Unknown)
}

pub fn provenance_label(part: &ProvenancePart, parenthetical: Option<&str>) -> String {
    match part {
        ProvenancePart::Import { source } => {
            let words = match source.as_str() {
                "apple_health" => "Apple Health".to_owned(),
                "oura" => "Oura".to_owned(),
                "strava" => "Strava".to_owned(),
                "chatgpt" => "ChatGPT".to_owned(),
                "claude" => "Claude".to_owned(),
                "gemini" => "Gemini".to_owned(),
                "kindle" => "Kindle".to_owned(),
                "obsidian" => "notes".to_owned(),
                "ics" => "calendar".to_owned(),
                "document" => "documents".to_owned(),
                "image" => "images".to_owned(),
                "audio" => "audio".to_owned(),
                "journal_archive" => "another journal".to_owned(),
                _ => source
                    .split(['_', '-'])
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_lowercase())
                    .collect::<Vec<_>>()
                    .join(" "),
            };
            format!("{words} · imported")
        }
        ProvenancePart::Agent => "your agent".to_owned(),
        ProvenancePart::Bound {
            device: Some(device),
            what,
            ..
        } => {
            let mut label = format!("your {}", device.as_str());
            if let Some(paren) = parenthetical {
                label.push_str(&format!(" ({paren})"));
            }
            if let Some(what) = what {
                label.push_str(&format!(" · {}", what.as_str()));
            }
            label
        }
        ProvenancePart::Bound {
            device: None, what, ..
        } => {
            let mut label = "your device".to_owned();
            if let Some(what) = what {
                label.push_str(&format!(" · {}", what.as_str()));
            }
            label
        }
        ProvenancePart::Unbound { device, .. } => {
            let mut label = format!("your {}", device.as_str());
            if let Some(paren) = parenthetical {
                label.push_str(&format!(" ({paren})"));
            }
            label
        }
        ProvenancePart::Unknown => "source unknown".to_owned(),
    }
}

pub fn rendered_stream_labels(table: &StreamLabelTable) -> Option<BTreeMap<String, String>> {
    if table.status == LabelTableStatus::Failed {
        return None;
    }

    let mut result = BTreeMap::new();
    for row in &table.rows {
        let parenthetical = compute_parenthetical(row, &table.rows);
        let rendered = provenance_label(&row.part, parenthetical);
        result.insert(row.name.clone(), rendered);
    }

    Some(result)
}

fn compute_parenthetical<'a>(
    row: &'a StreamLabelRow,
    all_rows: &'a [StreamLabelRow],
) -> Option<&'a str> {
    let device = row.part.device_word()?;
    let identity = row.part.identity()?;

    if identity.contains('.') {
        return None;
    }

    let has_differentiating_peer = all_rows.iter().any(|other| {
        other.name != row.name
            && other.part.device_word() == Some(device)
            && other
                .part
                .identity()
                .is_some_and(|other_id| other_id != identity && !other_id.contains('.'))
    });

    if has_differentiating_peer {
        Some(identity)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    fn make_journal() -> TempDir {
        TempDir::new().expect("temp dir creates")
    }

    fn write_file(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn write_stream(
        root: &Path,
        name: &str,
        kind: &str,
        cid: Option<&str>,
        source: Option<&str>,
        platform: Option<&str>,
    ) {
        let mut obj = json!({
            "name": name,
            "kind": kind,
            "host": null,
            "platform": platform,
            "created_at": 1,
            "last_day": null,
            "last_segment": null,
            "seq": 1
        });
        if let Some(cid) = cid {
            obj.as_object_mut()
                .unwrap()
                .insert("cid".into(), json!(cid));
        }
        if let Some(source) = source {
            obj.as_object_mut()
                .unwrap()
                .insert("source".into(), json!(source));
        }
        write_file(root, &format!("streams/{name}.json"), &obj.to_string());
    }

    #[test]
    fn missing_streams_directory_is_complete_and_empty() {
        let root = make_journal();
        let table = load_stream_labels(root.path());
        assert_eq!(table.status, LabelTableStatus::Complete);
        assert!(table.rows.is_empty());
    }

    #[test]
    fn one_malformed_record_is_failed_and_empty() {
        let root = make_journal();
        write_file(root.path(), "streams/broken.json", "{not json");
        let table = load_stream_labels(root.path());
        assert_eq!(table.status, LabelTableStatus::Failed);
        assert!(table.rows.is_empty());
        assert_eq!(rendered_stream_labels(&table), None);
    }

    #[test]
    fn description_file_corrupt_is_partial_with_records_classified() {
        let root = make_journal();
        write_stream(
            root.path(),
            "s1",
            "observer",
            Some("sha256:1111"),
            Some("watch-audio"),
            None,
        );
        write_file(root.path(), "link/client-descriptions.json", "{");
        let table = load_stream_labels(root.path());
        assert_eq!(table.status, LabelTableStatus::Partial);
        assert_eq!(table.rows.len(), 1);
        assert_eq!(
            table.rows[0].part,
            ProvenancePart::Bound {
                device: Some(DeviceWord::Watch),
                what: Some(WhatWord::Audio),
                identity: None,
            }
        );
    }

    #[test]
    fn missing_descriptions_and_missing_ledger_is_complete() {
        let root = make_journal();
        write_stream(
            root.path(),
            "s1",
            "observer",
            Some("sha256:1111"),
            Some("tmux"),
            None,
        );
        let table = load_stream_labels(root.path());
        assert_eq!(table.status, LabelTableStatus::Complete);
        assert_eq!(table.rows.len(), 1);
    }

    #[test]
    fn omi_audio_is_bound_none_none() {
        let root = make_journal();
        write_stream(
            root.path(),
            "omi",
            "observer",
            Some("sha256:1111"),
            Some("omi-audio"),
            None,
        );
        let table = load_stream_labels(root.path());
        assert_eq!(table.status, LabelTableStatus::Complete);
        assert_eq!(
            table.rows[0].part,
            ProvenancePart::Bound {
                device: None,
                what: None,
                identity: None,
            }
        );
        let rendered = rendered_stream_labels(&table).unwrap();
        assert_eq!(rendered.get("omi").unwrap(), "your device");
    }

    #[test]
    fn dot_free_fedora_with_no_platform_is_unknown() {
        let root = make_journal();
        write_stream(root.path(), "fedora", "observer", None, None, None);
        let table = load_stream_labels(root.path());
        assert_eq!(table.status, LabelTableStatus::Complete);
        assert_eq!(table.rows[0].part, ProvenancePart::Unknown);
    }

    #[test]
    fn rokid_glasses_is_unknown() {
        let root = make_journal();
        write_stream(
            root.path(),
            "rokid-rg-glasses.glasses",
            "observer",
            None,
            None,
            None,
        );
        let table = load_stream_labels(root.path());
        assert_eq!(table.status, LabelTableStatus::Complete);
        assert_eq!(table.rows[0].part, ProvenancePart::Unknown);
    }

    #[test]
    fn fedora_tmux_is_unbound_terminal_identity_fedora() {
        let root = make_journal();
        write_stream(root.path(), "fedora.tmux", "observer", None, None, None);
        let table = load_stream_labels(root.path());
        assert_eq!(table.status, LabelTableStatus::Complete);
        assert_eq!(
            table.rows[0].part,
            ProvenancePart::Unbound {
                device: DeviceWord::Terminal,
                identity: Some("fedora".to_owned()),
            }
        );
    }

    #[test]
    fn mobile_segment_with_no_platform_and_no_type_has_no_device_word() {
        let root = make_journal();
        write_stream(
            root.path(),
            "s1",
            "observer",
            Some("sha256:1111"),
            Some("mobile-segment"),
            None,
        );
        let table = load_stream_labels(root.path());
        assert_eq!(
            table.rows[0].part,
            ProvenancePart::Bound {
                device: None,
                what: None,
                identity: None,
            }
        );
    }

    #[test]
    fn linux_reported_desktop_screen_is_computer_screen_fedora() {
        let root = make_journal();
        write_stream(
            root.path(),
            "device_2",
            "observer",
            Some("sha256:1111"),
            Some("screen"),
            None,
        );
        let desc = json!({
            "sha256:1111": {
                "protocol_version": 1, "revision": 1, "owner_label": null, "updated_at": null,
                "reported": {
                    "name": "fedora", "platform": "linux", "device_type": "desktop",
                    "app_id": null, "app_version": null
                }
            }
        });
        write_file(
            root.path(),
            "link/client-descriptions.json",
            &desc.to_string(),
        );
        let table = load_stream_labels(root.path());
        assert_eq!(
            table.rows[0].part,
            ProvenancePart::Bound {
                device: Some(DeviceWord::Computer),
                what: Some(WhatWord::Screen),
                identity: Some("fedora".to_owned()),
            }
        );
    }

    #[test]
    fn ios_and_android_permutations() {
        let root = make_journal();
        let make_desc = |_cid: &str, plat: &str, dtype: Option<&str>| {
            json!({
                "protocol_version": 1, "revision": 1, "owner_label": null, "updated_at": null,
                "reported": {
                    "name": "dev", "platform": plat, "device_type": dtype,
                    "app_id": null, "app_version": null
                }
            })
        };
        let desc = json!({
            "sha256:i1": make_desc("sha256:i1", "ios", Some("tablet")),
            "sha256:i2": make_desc("sha256:i2", "ios", Some("phone")),
            "sha256:i3": make_desc("sha256:i3", "ios", None),
            "sha256:a1": make_desc("sha256:a1", "android", Some("tablet")),
            "sha256:a2": make_desc("sha256:a2", "android", Some("phone")),
            "sha256:a3": make_desc("sha256:a3", "android", None),
        });
        write_file(
            root.path(),
            "link/client-descriptions.json",
            &desc.to_string(),
        );
        write_stream(
            root.path(),
            "s_i1",
            "observer",
            Some("sha256:i1"),
            Some("screen"),
            None,
        );
        write_stream(
            root.path(),
            "s_i2",
            "observer",
            Some("sha256:i2"),
            Some("screen"),
            None,
        );
        write_stream(
            root.path(),
            "s_i3",
            "observer",
            Some("sha256:i3"),
            Some("screen"),
            None,
        );
        write_stream(
            root.path(),
            "s_a1",
            "observer",
            Some("sha256:a1"),
            Some("screen"),
            None,
        );
        write_stream(
            root.path(),
            "s_a2",
            "observer",
            Some("sha256:a2"),
            Some("screen"),
            None,
        );
        write_stream(
            root.path(),
            "s_a3",
            "observer",
            Some("sha256:a3"),
            Some("screen"),
            None,
        );

        let table = load_stream_labels(root.path());
        let part_of = |name: &str| resolve_stream_label(&table, name);
        assert_eq!(part_of("s_i1").device_word(), Some(DeviceWord::Ipad));
        assert_eq!(part_of("s_i2").device_word(), Some(DeviceWord::Iphone));
        assert_eq!(part_of("s_i3").device_word(), None);
        assert_eq!(part_of("s_a1").device_word(), Some(DeviceWord::Tablet));
        assert_eq!(part_of("s_a2").device_word(), Some(DeviceWord::Phone));
        assert_eq!(part_of("s_a3").device_word(), None);
    }

    #[test]
    fn windows_and_linux_permutations() {
        let root = make_journal();
        let make_desc = |dtype: Option<&str>| {
            json!({
                "protocol_version": 1, "revision": 1, "owner_label": null, "updated_at": null,
                "reported": {
                    "name": "dev", "platform": "windows", "device_type": dtype,
                    "app_id": null, "app_version": null
                }
            })
        };
        let desc = json!({
            "sha256:w1": make_desc(Some("terminal")),
            "sha256:w2": make_desc(Some("phone")),
            "sha256:w3": make_desc(Some("tablet")),
            "sha256:w4": make_desc(Some("desktop")),
            "sha256:w5": make_desc(None),
        });
        write_file(
            root.path(),
            "link/client-descriptions.json",
            &desc.to_string(),
        );
        write_stream(
            root.path(),
            "s_w1",
            "observer",
            Some("sha256:w1"),
            Some("screen"),
            None,
        );
        write_stream(
            root.path(),
            "s_w2",
            "observer",
            Some("sha256:w2"),
            Some("screen"),
            None,
        );
        write_stream(
            root.path(),
            "s_w3",
            "observer",
            Some("sha256:w3"),
            Some("screen"),
            None,
        );
        write_stream(
            root.path(),
            "s_w4",
            "observer",
            Some("sha256:w4"),
            Some("screen"),
            None,
        );
        write_stream(
            root.path(),
            "s_w5",
            "observer",
            Some("sha256:w5"),
            Some("screen"),
            None,
        );

        let table = load_stream_labels(root.path());
        assert_eq!(
            resolve_stream_label(&table, "s_w1").device_word(),
            Some(DeviceWord::Terminal)
        );
        assert_eq!(
            resolve_stream_label(&table, "s_w2").device_word(),
            Some(DeviceWord::Phone)
        );
        assert_eq!(
            resolve_stream_label(&table, "s_w3").device_word(),
            Some(DeviceWord::Tablet)
        );
        assert_eq!(
            resolve_stream_label(&table, "s_w4").device_word(),
            Some(DeviceWord::Computer)
        );
        assert_eq!(
            resolve_stream_label(&table, "s_w5").device_word(),
            Some(DeviceWord::Computer)
        );
    }

    #[test]
    fn unlisted_platform_blocks_later_facts() {
        let root = make_journal();
        let desc = json!({
            "sha256:1111": {
                "protocol_version": 1, "revision": 1, "owner_label": null, "updated_at": null,
                "reported": {
                    "name": "dev", "platform": "iOS", "device_type": null,
                    "app_id": null, "app_version": null
                }
            }
        });
        write_file(
            root.path(),
            "link/client-descriptions.json",
            &desc.to_string(),
        );
        let ledger = json!([{
            "fingerprint": "sha256:1111",
            "device_label": "Dev",
            "paired_at": "2026-01-01T00:00:00Z",
            "instance_id": "inst",
            "role": "observer",
            "network": null,
            "client_label": "dev",
            "label_ordinal": 1,
            "kind": "cert",
            "platform": "ios"
        }]);
        write_file(
            root.path(),
            "link/authorized_clients.json",
            &ledger.to_string(),
        );
        write_stream(
            root.path(),
            "s1",
            "observer",
            Some("sha256:1111"),
            Some("screen"),
            None,
        );

        let table = load_stream_labels(root.path());
        assert_eq!(table.rows[0].part.device_word(), None);
    }

    #[test]
    fn allocation_platform_used_only_when_reported_and_ledger_missing() {
        let root = make_journal();
        let stream = json!({
            "name": "s1", "kind": "observer", "host": null, "platform": null,
            "created_at": 1, "last_day": null, "last_segment": null, "seq": 1,
            "cid": "sha256:1111", "source": "screen",
            "allocation": {
                "base": "platform",
                "base_input": "macos",
                "source": "screen"
            }
        });
        write_file(root.path(), "streams/s1.json", &stream.to_string());
        let table = load_stream_labels(root.path());
        assert_eq!(table.rows[0].part.device_word(), Some(DeviceWord::Mac));
    }

    #[test]
    fn two_computer_rows_parenthetical_resolution() {
        let root = make_journal();
        let desc = json!({
            "sha256:1": {
                "protocol_version": 1, "revision": 1, "owner_label": null, "updated_at": null,
                "reported": { "name": "fedora", "platform": "linux", "device_type": "desktop", "app_id": null, "app_version": null }
            },
            "sha256:2": {
                "protocol_version": 1, "revision": 1, "owner_label": null, "updated_at": null,
                "reported": { "name": "studio", "platform": "linux", "device_type": "desktop", "app_id": null, "app_version": null }
            }
        });
        write_file(
            root.path(),
            "link/client-descriptions.json",
            &desc.to_string(),
        );
        write_stream(
            root.path(),
            "c1",
            "observer",
            Some("sha256:1"),
            Some("screen"),
            None,
        );
        write_stream(
            root.path(),
            "c2",
            "observer",
            Some("sha256:2"),
            Some("screen"),
            None,
        );

        let table = load_stream_labels(root.path());
        assert_eq!(
            compute_parenthetical(&table.rows[0], &table.rows),
            Some("fedora")
        );
        assert_eq!(
            compute_parenthetical(&table.rows[1], &table.rows),
            Some("studio")
        );

        // If same identity: no parentheses
        let desc_same = json!({
            "sha256:1": {
                "protocol_version": 1, "revision": 1, "owner_label": null, "updated_at": null,
                "reported": { "name": "same", "platform": "linux", "device_type": "desktop", "app_id": null, "app_version": null }
            },
            "sha256:2": {
                "protocol_version": 1, "revision": 1, "owner_label": null, "updated_at": null,
                "reported": { "name": "same", "platform": "linux", "device_type": "desktop", "app_id": null, "app_version": null }
            }
        });
        write_file(
            root.path(),
            "link/client-descriptions.json",
            &desc_same.to_string(),
        );
        let table_same = load_stream_labels(root.path());
        assert_eq!(
            compute_parenthetical(&table_same.rows[0], &table_same.rows),
            None
        );
        assert_eq!(
            compute_parenthetical(&table_same.rows[1], &table_same.rows),
            None
        );
    }

    #[test]
    fn agent_memory_with_cid_and_source_is_agent() {
        let root = make_journal();
        write_stream(
            root.path(),
            "agent-memory-1234",
            "agent_memory",
            Some("sha256:1234"),
            Some("1234"),
            None,
        );
        let table = load_stream_labels(root.path());
        assert_eq!(table.rows[0].part, ProvenancePart::Agent);
    }

    #[test]
    fn import_stream_is_import_part() {
        let root = make_journal();
        write_stream(root.path(), "import.oura", "import", None, None, None);
        let table = load_stream_labels(root.path());
        assert_eq!(
            table.rows[0].part,
            ProvenancePart::Import {
                source: "oura".to_owned()
            }
        );
    }

    #[test]
    fn bound_cid_with_no_description_ledger_or_allocation_is_device_none() {
        let root = make_journal();
        write_stream(
            root.path(),
            "bound_orphaned",
            "observer",
            Some("sha256:deadbeef"),
            Some("audio"),
            None,
        );
        let table = load_stream_labels(root.path());
        assert_eq!(
            table.rows[0].part,
            ProvenancePart::Bound {
                device: None,
                what: Some(WhatWord::Audio),
                identity: None,
            }
        );
    }

    #[test]
    fn resolve_unregistered_name_on_complete_table_is_unknown() {
        let root = make_journal();
        write_stream(root.path(), "registered", "observer", None, None, None);
        let table = load_stream_labels(root.path());
        assert_eq!(
            resolve_stream_label(&table, "not_registered"),
            ProvenancePart::Unknown
        );
    }

    #[test]
    fn multi_dot_stream_name_is_unknown() {
        let root = make_journal();
        write_stream(
            root.path(),
            "device.watch.audio",
            "observer",
            None,
            None,
            None,
        );
        let table = load_stream_labels(root.path());
        assert_eq!(table.rows[0].part, ProvenancePart::Unknown);
    }
}
