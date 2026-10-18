// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Handoff of journal setup into the installed Solstone app on Windows.
//!
//! When journal setup completes, the owner can continue in the Solstone desktop
//! app on this PC. Communication happens through an on-disk mailbox and lock
//! protocol defined in the setup handoff contract.

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};

use jsonschema::Draft;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::setup_flow::{ContinueOutcome, admit_mark_render};

const CONTRACT_JSON: &str = include_str!("../contracts/windows-setup-handoff.json");

#[derive(Debug, Clone, Deserialize)]
pub struct HandoffContract {
    pub argv: ArgvContract,
    pub capability_document: CapabilityDocumentContract,
    pub desktop: DesktopContract,
    pub fields: FieldsContract,
    pub files: FilesContract,
    pub identity: IdentityContract,
    pub limits: LimitsContract,
    pub resource: ResourceContract,
    pub schemas: SchemasContract,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArgvContract {
    pub cli_flag: String,
    pub control_verb: String,
    pub installed_executable: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CapabilityDocumentContract {
    pub capabilities: Vec<String>,
    pub schema: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DesktopContract {
    pub canonical_prefix: String,
    pub host: String,
    pub paths: Vec<String>,
    pub scheme: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FieldsContract {
    pub mailbox: FieldGroupContract,
    pub receipt: FieldGroupContract,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FieldGroupContract {
    pub unknown_fields: String,
    pub required: Vec<FieldDefContract>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FieldDefContract {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: String,
    #[serde(default)]
    pub r#const: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FilesContract {
    pub lock: String,
    pub mailbox: String,
    pub receipt: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IdentityContract {
    pub method: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LimitsContract {
    pub mailbox_max_bytes: usize,
    pub receipt_max_bytes: usize,
    pub capability_resource_max_bytes: usize,
    pub lock_acquire_ms: u64,
    pub unpaired_draft_max_age_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResourceContract {
    pub name: String,
    #[serde(rename = "type")]
    pub resource_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SchemasContract {
    pub mailbox: String,
    pub receipt: String,
}

pub fn resource_type_id(resource_type: &str) -> u32 {
    match resource_type {
        "RT_RCDATA" => 10,
        other => panic!("resource type must be RT_RCDATA, got: {other}"),
    }
}

pub fn get_contract() -> HandoffContract {
    let contract: HandoffContract =
        serde_json::from_str(CONTRACT_JSON).expect("valid embedded handoff contract");
    assert_eq!(
        resource_type_id(&contract.resource.resource_type),
        10,
        "resource type must be RT_RCDATA"
    );
    assert_eq!(
        contract.identity.method, "GET",
        "identity method must be GET"
    );
    assert!(
        !contract.argv.control_verb.is_empty(),
        "control_verb must not be empty"
    );
    contract
}

pub fn build_json_schema(group: &FieldGroupContract, schema_const: &str) -> Result<Value, String> {
    if group.unknown_fields != "forbidden" {
        return Err(format!(
            "unsupported unknown_fields: {}",
            group.unknown_fields
        ));
    }
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();

    for field in &group.required {
        required.push(Value::String(field.name.clone()));
        let mut prop = serde_json::Map::new();
        match field.field_type.as_str() {
            "string" => {
                prop.insert("type".to_string(), json!("string"));
                if field.name == "schema" {
                    prop.insert("const".to_string(), json!(schema_const));
                } else if let Some(ref c) = field.r#const {
                    prop.insert("const".to_string(), c.clone());
                } else {
                    prop.insert("minLength".to_string(), json!(1));
                }
            }
            "uuid" => {
                prop.insert("type".to_string(), json!("string"));
                prop.insert(
                    "pattern".to_string(),
                    json!("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"),
                );
            }
            "integer" => {
                prop.insert("type".to_string(), json!("integer"));
                if let Some(ref c) = field.r#const {
                    prop.insert("const".to_string(), c.clone());
                }
            }
            "sha256-hex" => {
                prop.insert("type".to_string(), json!("string"));
                prop.insert("pattern".to_string(), json!("^[0-9a-f]{64}$"));
            }
            "boolean" => {
                prop.insert("type".to_string(), json!("boolean"));
                if let Some(ref c) = field.r#const {
                    prop.insert("const".to_string(), c.clone());
                }
            }
            "unix-milliseconds" => {
                prop.insert("type".to_string(), json!("integer"));
                prop.insert("minimum".to_string(), json!(0));
            }
            "https-pair-link" => {
                prop.insert("type".to_string(), json!("string"));
                prop.insert("minLength".to_string(), json!(1));
            }
            other => return Err(format!("unknown type tag: {other}")),
        }
        properties.insert(field.name.clone(), Value::Object(prop));
    }

    let mut schema = serde_json::Map::new();
    schema.insert("type".to_string(), json!("object"));
    schema.insert("properties".to_string(), Value::Object(properties));
    schema.insert("required".to_string(), Value::Array(required));
    schema.insert("additionalProperties".to_string(), json!(false));
    Ok(Value::Object(schema))
}

pub fn compile_validator(schema_val: &Value) -> Result<jsonschema::Validator, String> {
    jsonschema::options()
        .with_draft(Draft::Draft202012)
        .build(schema_val)
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingContinuation {
    pub handoff_id: String,
}

pub fn is_valid_uuid(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    let b = s.as_bytes();
    if b[8] != b'-' || b[13] != b'-' || b[18] != b'-' || b[23] != b'-' {
        return false;
    }
    for (i, &byte) in b.iter().enumerate() {
        if i == 8 || i == 13 || i == 18 || i == 23 {
            continue;
        }
        if !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase() {
            return false;
        }
    }
    true
}

pub fn generate_uuid() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("getrandom available");
    // Version 4: bits 12..15 of time_hi_and_version set to 0100
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    // Variant RFC 4122: bits 6..7 of clock_seq_hi_and_reserved set to 10
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

pub fn read_pending(state_dir: &Path) -> Result<Option<PendingContinuation>, String> {
    let path = state_dir.join("setup-continuation.json");
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).map_err(|e| e.to_string())?;
    let pending: PendingContinuation = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if !is_valid_uuid(&pending.handoff_id) {
        return Err("invalid handoff_id".to_string());
    }
    Ok(Some(pending))
}

pub fn write_pending(state_dir: &Path, handoff_id: &str) -> Result<(), String> {
    if !is_valid_uuid(handoff_id) {
        return Err("invalid handoff_id".to_string());
    }
    fs::create_dir_all(state_dir).map_err(|e| e.to_string())?;
    let target = state_dir.join("setup-continuation.json");
    let tmp = state_dir.join("setup-continuation.json.tmp");
    let content = serde_json::to_string(&PendingContinuation {
        handoff_id: handoff_id.to_string(),
    })
    .map_err(|e| e.to_string())?;
    fs::write(&tmp, content.as_bytes()).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &target).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn delete_pending(state_dir: &Path) -> Result<(), String> {
    let path = state_dir.join("setup-continuation.json");
    if path.exists() {
        fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn advance_pending(state_dir: &Path) -> Result<String, String> {
    let id = generate_uuid();
    write_pending(state_dir, &id)?;
    Ok(id)
}

#[derive(Debug, Clone)]
pub struct HandoffPaths {
    pub local_appdata: PathBuf,
    pub state_dir: PathBuf,
}

impl HandoffPaths {
    pub fn solstone_dir(&self) -> PathBuf {
        self.local_appdata.join("Solstone")
    }
}

pub fn resolve_installed_executable(template: &str, root: &Path) -> Result<PathBuf, String> {
    if !template.starts_with("%LOCALAPPDATA%\\") && !template.starts_with("%LOCALAPPDATA%/") {
        return Err("installed_executable must start with %LOCALAPPDATA%".to_string());
    }
    let rest = &template["%LOCALAPPDATA%".len()..];
    let trimmed = rest.trim_start_matches('\\').trim_start_matches('/');
    let mut path = root.to_path_buf();
    for segment in trimmed.split(['\\', '/']) {
        if !segment.is_empty() {
            path.push(segment);
        }
    }
    Ok(path)
}

pub trait WaitClock {
    fn now_ms(&self) -> u64;
    fn sleep_ms(&mut self, ms: u64);
}

pub struct WallClock;

impl WaitClock for WallClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn sleep_ms(&mut self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

#[derive(Debug)]
pub enum HandoffError {
    Busy,
    Io(String),
}

impl std::fmt::Display for HandoffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => write!(f, "handoff lock is busy"),
            Self::Io(msg) => write!(f, "handoff io error: {msg}"),
        }
    }
}

impl std::error::Error for HandoffError {}

#[derive(Debug)]
pub struct HandoffLock {
    file: File,
}

impl Drop for HandoffLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl HandoffLock {
    pub fn acquire<C: WaitClock>(
        lock_path: &Path,
        max_wait_ms: u64,
        clock: &mut C,
    ) -> Result<Self, HandoffError> {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(|e| HandoffError::Io(e.to_string()))?;
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|e| HandoffError::Io(e.to_string()))?;

        let start = clock.now_ms();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { file }),
                Err(TryLockError::WouldBlock) => {
                    let elapsed = clock.now_ms().saturating_sub(start);
                    if elapsed >= max_wait_ms {
                        return Err(HandoffError::Busy);
                    }
                    let remaining = max_wait_ms.saturating_sub(elapsed);
                    let step = 50.min(remaining);
                    if step == 0 {
                        return Err(HandoffError::Busy);
                    }
                    clock.sleep_ms(step);
                }
                Err(TryLockError::Error(e)) => return Err(HandoffError::Io(e.to_string())),
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityResult {
    Supported,
    Absent,
    Malformed,
    Oversized,
    Unknown,
    NoResource,
}

pub fn read_capabilities(
    path: &Path,
    contract_doc: &CapabilityDocumentContract,
    max_bytes: usize,
    res_type_id: u32,
    res_name: &str,
) -> CapabilityResult {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return CapabilityResult::Absent,
        Err(_) => return CapabilityResult::Malformed,
    };
    read_capabilities_from_pe(&bytes, contract_doc, max_bytes, res_type_id, res_name)
}

pub fn read_capabilities_from_pe(
    bytes: &[u8],
    contract_doc: &CapabilityDocumentContract,
    max_bytes: usize,
    res_type_id: u32,
    res_name: &str,
) -> CapabilityResult {
    if bytes.len() < 0x40 || &bytes[0..2] != b"MZ" {
        return CapabilityResult::Malformed;
    }
    let e_lfanew = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
    if e_lfanew + 4 > bytes.len() {
        return CapabilityResult::Malformed;
    }
    if &bytes[e_lfanew..e_lfanew + 4] != b"PE\0\0" {
        return CapabilityResult::Malformed;
    }

    let coff = e_lfanew + 4;
    if coff + 20 > bytes.len() {
        return CapabilityResult::Malformed;
    }
    let machine = u16::from_le_bytes(bytes[coff..coff + 2].try_into().unwrap());
    if machine != 0x8664 {
        return CapabilityResult::Malformed;
    }
    let num_sections = u16::from_le_bytes(bytes[coff + 2..coff + 4].try_into().unwrap()) as usize;
    let opt_size = u16::from_le_bytes(bytes[coff + 16..coff + 18].try_into().unwrap()) as usize;

    let opt = coff + 20;
    if opt + opt_size > bytes.len() || opt_size < 136 {
        return CapabilityResult::Malformed;
    }
    let magic = u16::from_le_bytes(bytes[opt..opt + 2].try_into().unwrap());
    if magic != 0x20b {
        return CapabilityResult::Malformed;
    }

    let num_rva_and_sizes = u32::from_le_bytes(bytes[opt + 108..opt + 112].try_into().unwrap());
    if num_rva_and_sizes < 3 {
        return CapabilityResult::NoResource;
    }

    let res_rva = u32::from_le_bytes(bytes[opt + 128..opt + 132].try_into().unwrap());
    let res_size = u32::from_le_bytes(bytes[opt + 132..opt + 136].try_into().unwrap());
    if res_rva == 0 || res_size == 0 {
        return CapabilityResult::NoResource;
    }

    let sections_offset = opt + opt_size;
    let sec_bytes = match num_sections.checked_mul(40) {
        Some(b) => b,
        None => return CapabilityResult::Malformed,
    };
    let sec_end = match sections_offset.checked_add(sec_bytes) {
        Some(end) => end,
        None => return CapabilityResult::Malformed,
    };
    if sec_end > bytes.len() {
        return CapabilityResult::Malformed;
    }

    struct Section {
        va: u32,
        vsize: u32,
        raw_ptr: u32,
        raw_size: u32,
    }
    let mut sections = Vec::with_capacity(num_sections);
    for i in 0..num_sections {
        let s = sections_offset + i * 40;
        let vsize = u32::from_le_bytes(bytes[s + 8..s + 12].try_into().unwrap());
        let va = u32::from_le_bytes(bytes[s + 12..s + 16].try_into().unwrap());
        let raw_size = u32::from_le_bytes(bytes[s + 16..s + 20].try_into().unwrap());
        let raw_ptr = u32::from_le_bytes(bytes[s + 20..s + 24].try_into().unwrap());
        sections.push(Section {
            va,
            vsize,
            raw_ptr,
            raw_size,
        });
    }

    let rva_to_offset = |rva: u32| -> Option<usize> {
        for s in &sections {
            let span = s.vsize.max(s.raw_size);
            if rva >= s.va && rva < s.va + span {
                let off = s.raw_ptr as usize + (rva - s.va) as usize;
                if off < bytes.len() {
                    return Some(off);
                }
            }
        }
        None
    };

    let root_offset = match rva_to_offset(res_rva) {
        Some(o) => o,
        None => return CapabilityResult::Malformed,
    };
    let res_slice = &bytes[root_offset..];
    if res_slice.len() < 16 {
        return CapabilityResult::Malformed;
    }

    let num_named = u16::from_le_bytes(res_slice[12..14].try_into().unwrap()) as usize;
    let num_id = u16::from_le_bytes(res_slice[14..16].try_into().unwrap()) as usize;
    let total_l1 = num_named + num_id;
    if total_l1 > 64 || 16 + total_l1 * 8 > res_slice.len() {
        return CapabilityResult::Malformed;
    }

    let mut l2_offset = None;
    for i in 0..total_l1 {
        let entry = 16 + i * 8;
        let id = u32::from_le_bytes(res_slice[entry..entry + 4].try_into().unwrap());
        let data = u32::from_le_bytes(res_slice[entry + 4..entry + 8].try_into().unwrap());
        if id == res_type_id && (data & 0x8000_0000) != 0 {
            l2_offset = Some((data & 0x7fff_ffff) as usize);
            break;
        }
    }
    let l2_offset = match l2_offset {
        Some(o) => o,
        None => return CapabilityResult::NoResource,
    };

    if l2_offset + 16 > res_slice.len() {
        return CapabilityResult::Malformed;
    }
    let num_named_l2 = u16::from_le_bytes(
        res_slice[l2_offset + 12..l2_offset + 14]
            .try_into()
            .unwrap(),
    ) as usize;
    let num_id_l2 = u16::from_le_bytes(
        res_slice[l2_offset + 14..l2_offset + 16]
            .try_into()
            .unwrap(),
    ) as usize;
    let total_l2 = num_named_l2 + num_id_l2;
    if total_l2 > 64 || l2_offset + 16 + total_l2 * 8 > res_slice.len() {
        return CapabilityResult::Malformed;
    }

    let mut l3_offset = None;
    for i in 0..total_l2 {
        let entry = l2_offset + 16 + i * 8;
        let name_id = u32::from_le_bytes(res_slice[entry..entry + 4].try_into().unwrap());
        let data = u32::from_le_bytes(res_slice[entry + 4..entry + 8].try_into().unwrap());
        if (name_id & 0x8000_0000) != 0 {
            let str_off = (name_id & 0x7fff_ffff) as usize;
            if str_off + 2 <= res_slice.len() {
                let u16_len =
                    u16::from_le_bytes(res_slice[str_off..str_off + 2].try_into().unwrap())
                        as usize;
                if str_off + 2 + u16_len * 2 <= res_slice.len() {
                    let mut u16_chars = Vec::with_capacity(u16_len);
                    for c in 0..u16_len {
                        let pos = str_off + 2 + c * 2;
                        u16_chars.push(u16::from_le_bytes(
                            res_slice[pos..pos + 2].try_into().unwrap(),
                        ));
                    }
                    if let Ok(name_str) = String::from_utf16(&u16_chars) {
                        if name_str == res_name && (data & 0x8000_0000) != 0 {
                            l3_offset = Some((data & 0x7fff_ffff) as usize);
                            break;
                        }
                    }
                }
            }
        }
    }
    let l3_offset = match l3_offset {
        Some(o) => o,
        None => return CapabilityResult::NoResource,
    };

    if l3_offset + 16 > res_slice.len() {
        return CapabilityResult::Malformed;
    }
    let num_named_l3 = u16::from_le_bytes(
        res_slice[l3_offset + 12..l3_offset + 14]
            .try_into()
            .unwrap(),
    ) as usize;
    let num_id_l3 = u16::from_le_bytes(
        res_slice[l3_offset + 14..l3_offset + 16]
            .try_into()
            .unwrap(),
    ) as usize;
    let total_l3 = num_named_l3 + num_id_l3;
    if total_l3 > 64 || total_l3 == 0 || l3_offset + 16 + total_l3 * 8 > res_slice.len() {
        return CapabilityResult::Malformed;
    }

    let entry_l3 = l3_offset + 16;
    let data_entry_off =
        u32::from_le_bytes(res_slice[entry_l3 + 4..entry_l3 + 8].try_into().unwrap()) as usize;
    if (data_entry_off & 0x8000_0000) != 0 {
        return CapabilityResult::Malformed;
    }
    if data_entry_off + 16 > res_slice.len() {
        return CapabilityResult::Malformed;
    }

    let data_rva = u32::from_le_bytes(
        res_slice[data_entry_off..data_entry_off + 4]
            .try_into()
            .unwrap(),
    );
    let data_size = u32::from_le_bytes(
        res_slice[data_entry_off + 4..data_entry_off + 8]
            .try_into()
            .unwrap(),
    ) as usize;

    if data_size > max_bytes {
        return CapabilityResult::Oversized;
    }

    let data_file_offset = match rva_to_offset(data_rva) {
        Some(o) => o,
        None => return CapabilityResult::Malformed,
    };
    if data_file_offset + data_size > bytes.len() {
        return CapabilityResult::Malformed;
    }

    let doc_bytes = &bytes[data_file_offset..data_file_offset + data_size];
    let doc_val: Value = match serde_json::from_slice(doc_bytes) {
        Ok(v) => v,
        Err(_) => return CapabilityResult::Malformed,
    };

    let expected_doc = match serde_json::to_value(contract_doc) {
        Ok(v) => v,
        Err(_) => return CapabilityResult::Unknown,
    };

    if doc_val != expected_doc {
        return CapabilityResult::Unknown;
    }

    CapabilityResult::Supported
}

pub trait HandoffEndpoints {
    fn get(&self, path: &str) -> Result<Value, String>;
    fn post(&self, path: &str, body: &Value) -> Result<Value, String>;
}

#[cfg(windows)]
impl HandoffEndpoints for crate::convey::Convey {
    fn get(&self, path: &str) -> Result<Value, String> {
        self.get(path).map_err(|r| r.message)
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.post(path, Some(body)).map_err(|r| r.message)
    }
}

pub struct HandoffCollaborators<
    'a,
    E: HandoffEndpoints,
    L: FnMut(&Path, &str) -> Result<(), ()>,
    C: WaitClock,
    F: FnMut() -> String,
    S: Fn() -> bool,
> {
    pub paths: HandoffPaths,
    pub endpoints: &'a E,
    pub launch: L,
    pub still_current: S,
    pub clock: C,
    pub new_id: F,
}

pub fn continue_in_solstone<E, L, C, F, S>(
    mut collaborators: HandoffCollaborators<'_, E, L, C, F, S>,
) -> ContinueOutcome
where
    E: HandoffEndpoints,
    L: FnMut(&Path, &str) -> Result<(), ()>,
    C: WaitClock,
    F: FnMut() -> String,
    S: Fn() -> bool,
{
    let contract = get_contract();
    let solstone_dir = collaborators.paths.solstone_dir();

    // 1. check still_current
    if !(collaborators.still_current)() {
        return ContinueOutcome::Superseded;
    }

    // 2. lock and advance pending ID
    let lock_path = solstone_dir.join(&contract.files.lock);
    let handoff_id = {
        let _lock = match HandoffLock::acquire(
            &lock_path,
            contract.limits.lock_acquire_ms,
            &mut collaborators.clock,
        ) {
            Ok(l) => l,
            Err(_) => return ContinueOutcome::Failed,
        };
        let id = (collaborators.new_id)();
        if write_pending(&collaborators.paths.state_dir, &id).is_err() {
            return ContinueOutcome::Failed;
        }
        id
    };

    // 3. check still_current
    if !(collaborators.still_current)() {
        return ContinueOutcome::Superseded;
    }

    // 4. fetch identity
    let identity_val = match collaborators.endpoints.get(&contract.identity.path) {
        Ok(v) => v,
        Err(_) => return ContinueOutcome::Failed,
    };
    if identity_val.get("committed").and_then(Value::as_bool) != Some(true) {
        return ContinueOutcome::Failed;
    }
    let instance_id = match identity_val.get("instance_id").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return ContinueOutcome::Failed,
    };
    let mark = match identity_val.get("mark") {
        Some(m) if !m.is_null() => m,
        _ => return ContinueOutcome::Failed,
    };
    if admit_mark_render(mark).is_err() {
        return ContinueOutcome::Failed;
    }

    // 5. check still_current
    if !(collaborators.still_current)() {
        return ContinueOutcome::Superseded;
    }

    // 6. check receiver capability
    let exe_path = match resolve_installed_executable(
        &contract.argv.installed_executable,
        &collaborators.paths.local_appdata,
    ) {
        Ok(p) => p,
        Err(_) => return ContinueOutcome::Failed,
    };
    let res_type_id = resource_type_id(&contract.resource.resource_type);
    let cap = read_capabilities(
        &exe_path,
        &contract.capability_document,
        contract.limits.capability_resource_max_bytes,
        res_type_id,
        &contract.resource.name,
    );
    if cap != CapabilityResult::Supported {
        return ContinueOutcome::Unsupported;
    }

    // 7. record request start timestamp and POST /app/network/pair-start
    let request_start = collaborators.clock.now_ms();
    let pair_resp = match collaborators
        .endpoints
        .post("/app/network/pair-start", &json!({ "same_machine": true }))
    {
        Ok(v) => v,
        Err(_) => return ContinueOutcome::Failed,
    };

    // 8. validate pair-start response (reads pair_link)
    let pairing_link = match pair_resp.get("pair_link").and_then(Value::as_str) {
        Some(s) if s.starts_with(&contract.desktop.canonical_prefix) => s.to_string(),
        _ => return ContinueOutcome::Failed,
    };
    let parsed_link = match spl_core::pairlink::parse(&pairing_link) {
        Ok(spl_core::pairlink::ParsedPairLink::Direct(d)) => d,
        _ => return ContinueOutcome::Failed,
    };
    if parsed_link.candidates.is_empty()
        || !parsed_link.candidates.iter().all(|c| c.host == "127.0.0.1")
    {
        return ContinueOutcome::Failed;
    }

    let ca_fp = match pair_resp.get("ca_fingerprint").and_then(Value::as_str) {
        Some(s)
            if s.len() == 64
                && s.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) =>
        {
            s.to_string()
        }
        _ => return ContinueOutcome::Failed,
    };
    let prefix_hex: String = parsed_link
        .ca_fp_prefix
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if prefix_hex.len() != 32 || ca_fp[..32] != prefix_hex {
        return ContinueOutcome::Failed;
    }

    let expires_in = match pair_resp.get("expires_in").and_then(Value::as_u64) {
        Some(n) if n >= 1 => n,
        _ => return ContinueOutcome::Failed,
    };
    let expires_ms = match expires_in.checked_mul(1000) {
        Some(ms) => ms,
        None => return ContinueOutcome::Failed,
    };
    let deadline = match request_start.checked_add(expires_ms) {
        Some(d) => d,
        None => return ContinueOutcome::Failed,
    };
    if collaborators.clock.now_ms() >= deadline {
        return ContinueOutcome::Failed;
    }

    // 9. check still_current
    if !(collaborators.still_current)() {
        return ContinueOutcome::Superseded;
    }

    // 10. under lock verify still_current, pending ID, and atomically replace mailbox
    {
        let _lock = match HandoffLock::acquire(
            &lock_path,
            contract.limits.lock_acquire_ms,
            &mut collaborators.clock,
        ) {
            Ok(l) => l,
            Err(_) => return ContinueOutcome::Failed,
        };

        if !(collaborators.still_current)() {
            return ContinueOutcome::Superseded;
        }

        match read_pending(&collaborators.paths.state_dir) {
            Ok(Some(current_pending)) if current_pending.handoff_id == handoff_id => {}
            _ => return ContinueOutcome::Superseded,
        }

        let step_const = match contract
            .fields
            .mailbox
            .required
            .iter()
            .find(|f| f.name == "step")
            .and_then(|f| f.r#const.as_ref())
            .and_then(Value::as_u64)
        {
            Some(c) => c,
            None => return ContinueOutcome::Failed,
        };

        let mailbox_val = json!({
            "schema": contract.schemas.mailbox,
            "handoff_id": handoff_id,
            "step": step_const,
            "expected_instance_id": instance_id,
            "expected_ca_fingerprint": ca_fp,
            "pairing_link": pairing_link,
            "expires_at_unix_ms": deadline,
        });

        let mailbox_schema =
            match build_json_schema(&contract.fields.mailbox, &contract.schemas.mailbox) {
                Ok(s) => s,
                Err(_) => return ContinueOutcome::Failed,
            };
        let validator = match compile_validator(&mailbox_schema) {
            Ok(v) => v,
            Err(_) => return ContinueOutcome::Failed,
        };
        if !validator.is_valid(&mailbox_val) {
            return ContinueOutcome::Failed;
        }

        let mailbox_bytes = match serde_json::to_vec(&mailbox_val) {
            Ok(b) if b.len() <= contract.limits.mailbox_max_bytes => b,
            _ => return ContinueOutcome::Failed,
        };

        let target_mailbox = solstone_dir.join(&contract.files.mailbox);
        let tmp_mailbox = solstone_dir.join(format!("{}.tmp", &contract.files.mailbox));
        if fs::write(&tmp_mailbox, &mailbox_bytes).is_err() {
            return ContinueOutcome::Failed;
        }
        if fs::rename(&tmp_mailbox, &target_mailbox).is_err() {
            let _ = fs::remove_file(&tmp_mailbox);
            return ContinueOutcome::Failed;
        }
    }

    // 11. check still_current
    if !(collaborators.still_current)() {
        return ContinueOutcome::Superseded;
    }

    // 12. launch installed executable with argv.cli_flag
    match (collaborators.launch)(&exe_path, &contract.argv.cli_flag) {
        Ok(()) => ContinueOutcome::Published,
        Err(()) => ContinueOutcome::Failed,
    }
}

pub fn observe_receipt<C: WaitClock>(paths: &HandoffPaths, clock: &mut C) -> Result<bool, String> {
    let contract = get_contract();
    let solstone_dir = paths.solstone_dir();
    let lock_path = solstone_dir.join(&contract.files.lock);

    let _lock = HandoffLock::acquire(&lock_path, contract.limits.lock_acquire_ms, clock)
        .map_err(|e| e.to_string())?;

    let pending = match read_pending(&paths.state_dir)? {
        Some(p) => p,
        None => return Ok(false),
    };

    let receipt_path = solstone_dir.join(&contract.files.receipt);
    if !receipt_path.exists() {
        return Ok(true);
    }

    let receipt_bytes = match fs::read(&receipt_path) {
        Ok(b) => b,
        Err(_) => return Ok(true),
    };

    if receipt_bytes.len() > contract.limits.receipt_max_bytes {
        return Ok(true);
    }

    let receipt_val: Value = match serde_json::from_slice(&receipt_bytes) {
        Ok(v) => v,
        Err(_) => return Ok(true),
    };

    let receipt_schema =
        match build_json_schema(&contract.fields.receipt, &contract.schemas.receipt) {
            Ok(s) => s,
            Err(_) => return Ok(true),
        };
    let validator = match compile_validator(&receipt_schema) {
        Ok(v) => v,
        Err(_) => return Ok(true),
    };
    if !validator.is_valid(&receipt_val) {
        return Ok(true);
    }

    let receipt_id = match receipt_val.get("handoff_id").and_then(Value::as_str) {
        Some(id) => id,
        None => return Ok(true),
    };

    if receipt_id == pending.handoff_id {
        delete_pending(&paths.state_dir)?;
        Ok(false)
    } else {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    fn build_synthetic_pe(
        res_type_id: u32,
        res_name: &str,
        res_data: &[u8],
        zero_res_dir: bool,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; 0x400];
        // DOS header
        buf[0..2].copy_from_slice(b"MZ");
        buf[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes()); // e_lfanew = 0x40

        // PE signature
        buf[0x40..0x44].copy_from_slice(b"PE\0\0");

        // COFF header at 0x44 (20 bytes)
        buf[0x44..0x46].copy_from_slice(&0x8664u16.to_le_bytes()); // machine AMD64
        buf[0x46..0x48].copy_from_slice(&1u16.to_le_bytes()); // 1 section
        buf[0x54..0x56].copy_from_slice(&240u16.to_le_bytes()); // opt header size = 240

        // Optional header at 0x58 (240 bytes)
        buf[0x58..0x5a].copy_from_slice(&0x020bu16.to_le_bytes()); // magic PE32+
        buf[0x58 + 108..0x58 + 112].copy_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes

        if !zero_res_dir {
            // Data Directory entry 2 (Resource): RVA 0x1000, Size 0x200
            buf[0x58 + 128..0x58 + 132].copy_from_slice(&0x1000u32.to_le_bytes());
            buf[0x58 + 132..0x58 + 136].copy_from_slice(&0x200u32.to_le_bytes());
        }

        // Section header at 0x58 + 240 = 0x148 (40 bytes)
        buf[0x148..0x150].copy_from_slice(b".rsrc\0\0\0");
        buf[0x150..0x154].copy_from_slice(&0x1000u32.to_le_bytes()); // VirtualSize
        buf[0x154..0x158].copy_from_slice(&0x1000u32.to_le_bytes()); // VirtualAddress
        buf[0x158..0x15c].copy_from_slice(&0x1000u32.to_le_bytes()); // SizeOfRawData
        buf[0x15c..0x160].copy_from_slice(&0x200u32.to_le_bytes()); // PointerToRawData

        if zero_res_dir {
            return buf;
        }

        // Section data starts at file offset 0x200 (RVA 0x1000)
        let s = 0x200;
        // Level 1 directory at s (16 bytes)
        buf[s + 12..s + 14].copy_from_slice(&0u16.to_le_bytes()); // named = 0
        buf[s + 14..s + 16].copy_from_slice(&1u16.to_le_bytes()); // id = 1
        // L1 entry (8 bytes): type id, offset to L2 (0x80000018 = offset 24)
        buf[s + 16..s + 20].copy_from_slice(&res_type_id.to_le_bytes());
        buf[s + 20..s + 24].copy_from_slice(&0x8000_0018u32.to_le_bytes());

        // Level 2 directory at s + 24 (16 bytes)
        buf[s + 24 + 12..s + 24 + 14].copy_from_slice(&1u16.to_le_bytes()); // named = 1
        buf[s + 24 + 14..s + 24 + 16].copy_from_slice(&0u16.to_le_bytes()); // id = 0
        // L2 entry (8 bytes): name offset (0x80000030 = offset 48), offset to L3
        let name_u16: Vec<u16> = res_name.encode_utf16().collect();
        let name_str_len = 2 + name_u16.len() * 2;
        let l3_off = 48 + name_str_len;
        buf[s + 40..s + 44].copy_from_slice(&0x8000_0030u32.to_le_bytes());
        buf[s + 44..s + 48].copy_from_slice(&(0x8000_0000u32 | (l3_off as u32)).to_le_bytes());

        // String at s + 48
        buf[s + 48..s + 50].copy_from_slice(&(name_u16.len() as u16).to_le_bytes());
        for (i, &ch) in name_u16.iter().enumerate() {
            buf[s + 50 + i * 2..s + 52 + i * 2].copy_from_slice(&ch.to_le_bytes());
        }

        // Level 3 directory at s + l3_off (16 bytes)
        buf[s + l3_off + 12..s + l3_off + 14].copy_from_slice(&0u16.to_le_bytes());
        buf[s + l3_off + 14..s + l3_off + 16].copy_from_slice(&1u16.to_le_bytes());
        // L3 entry: lang 0, offset to Data Entry (high bit 0)
        let data_entry_off = l3_off + 24;
        buf[s + l3_off + 16..s + l3_off + 20].copy_from_slice(&0u32.to_le_bytes());
        buf[s + l3_off + 20..s + l3_off + 24]
            .copy_from_slice(&(data_entry_off as u32).to_le_bytes());

        // Data Entry at s + data_entry_off (16 bytes)
        let raw_data_off = data_entry_off + 16;
        let raw_data_rva = 0x1000 + raw_data_off as u32;
        buf[s + data_entry_off..s + data_entry_off + 4]
            .copy_from_slice(&raw_data_rva.to_le_bytes());
        buf[s + data_entry_off + 4..s + data_entry_off + 8]
            .copy_from_slice(&(res_data.len() as u32).to_le_bytes());

        // Write raw payload
        buf.resize(s + raw_data_off + res_data.len(), 0);
        buf[s + raw_data_off..s + raw_data_off + res_data.len()].copy_from_slice(res_data);

        buf
    }

    #[test]
    fn contract_provenance_and_mailbox_receipt_rules() {
        let contract_bytes = include_bytes!("../contracts/windows-setup-handoff.json");
        let hash = format!("{:x}", Sha256::digest(contract_bytes));
        let prov_str = include_str!("../contracts/windows-setup-handoff.provenance.json");
        let prov_val: Value = serde_json::from_str(prov_str).unwrap();
        assert_eq!(hash, prov_val["sha256"].as_str().unwrap());

        let raw_json: Value = serde_json::from_slice(contract_bytes).unwrap();
        let loaded = get_contract();

        assert_eq!(
            loaded.limits.mailbox_max_bytes,
            raw_json["limits"]["mailbox_max_bytes"].as_u64().unwrap() as usize
        );
        assert_eq!(
            loaded.limits.receipt_max_bytes,
            raw_json["limits"]["receipt_max_bytes"].as_u64().unwrap() as usize
        );
        assert_eq!(
            loaded.limits.capability_resource_max_bytes,
            raw_json["limits"]["capability_resource_max_bytes"]
                .as_u64()
                .unwrap() as usize
        );
        assert_eq!(
            loaded.limits.lock_acquire_ms,
            raw_json["limits"]["lock_acquire_ms"].as_u64().unwrap()
        );
        assert_eq!(
            loaded.limits.unpaired_draft_max_age_secs,
            raw_json["limits"]["unpaired_draft_max_age_secs"]
                .as_u64()
                .unwrap()
        );
        assert_eq!(loaded.schemas.mailbox, raw_json["schemas"]["mailbox"]);
        assert_eq!(loaded.schemas.receipt, raw_json["schemas"]["receipt"]);
        assert_eq!(loaded.argv.cli_flag, raw_json["argv"]["cli_flag"]);
        assert_eq!(loaded.resource.name, raw_json["resource"]["name"]);
        assert_eq!(loaded.files.lock, raw_json["files"]["lock"]);
        assert_eq!(loaded.files.mailbox, raw_json["files"]["mailbox"]);
        assert_eq!(loaded.files.receipt, raw_json["files"]["receipt"]);
        assert_eq!(loaded.identity.path, raw_json["identity"]["path"]);

        // Step const
        let step_field = loaded
            .fields
            .mailbox
            .required
            .iter()
            .find(|f| f.name == "step")
            .unwrap();
        assert_eq!(step_field.r#const, Some(json!(5)));

        // Mailbox validation rules
        let mb_schema = build_json_schema(&loaded.fields.mailbox, &loaded.schemas.mailbox).unwrap();
        let mb_val = compile_validator(&mb_schema).unwrap();

        let valid_mb = json!({
            "schema": "solstone.windows-setup-handoff.v1",
            "handoff_id": "01234567-89ab-4cde-8f01-23456789abcd",
            "step": 5,
            "expected_instance_id": "inst-1",
            "expected_ca_fingerprint": "a".repeat(64),
            "pairing_link": "https://go.solstone.app/p#mock",
            "expires_at_unix_ms": 1234567890u64,
        });
        assert!(mb_val.is_valid(&valid_mb));

        let mut extra_field_mb = valid_mb.clone();
        extra_field_mb["extra"] = json!("forbidden");
        assert!(!mb_val.is_valid(&extra_field_mb));

        let mut wrong_schema_mb = valid_mb.clone();
        wrong_schema_mb["schema"] = json!("wrong.schema");
        assert!(!mb_val.is_valid(&wrong_schema_mb));

        // Receipt validation rules
        let rc_schema = build_json_schema(&loaded.fields.receipt, &loaded.schemas.receipt).unwrap();
        let rc_val = compile_validator(&rc_schema).unwrap();

        let valid_rc = json!({
            "schema": "solstone.windows-setup-receipt.v1",
            "handoff_id": "01234567-89ab-4cde-8f01-23456789abcd",
            "accepted": true,
        });
        assert!(rc_val.is_valid(&valid_rc));

        let mut extra_field_rc = valid_rc.clone();
        extra_field_rc["extra"] = json!("forbidden");
        assert!(!rc_val.is_valid(&extra_field_rc));

        let mut rejected_rc = valid_rc.clone();
        rejected_rc["accepted"] = json!(false);
        assert!(!rc_val.is_valid(&rejected_rc));

        let mut wrong_schema_rc = valid_rc.clone();
        wrong_schema_rc["schema"] = json!("wrong.receipt.schema");
        assert!(!rc_val.is_valid(&wrong_schema_rc));

        // Byte limit assertions
        let large_mb = vec![b'a'; loaded.limits.mailbox_max_bytes + 1];
        assert!(large_mb.len() > loaded.limits.mailbox_max_bytes);
        let large_rc = vec![b'a'; loaded.limits.receipt_max_bytes + 1];
        assert!(large_rc.len() > loaded.limits.receipt_max_bytes);
    }

    #[test]
    fn capability_reader_distinguishes_pe_payloads() {
        let contract = get_contract();
        let dir = tempdir().unwrap();
        let res_type_id = resource_type_id(&contract.resource.resource_type);

        // 1. Supported
        let doc_bytes = serde_json::to_vec(&contract.capability_document).unwrap();
        let pe_supported =
            build_synthetic_pe(res_type_id, &contract.resource.name, &doc_bytes, false);
        let p_supp = dir.path().join("supported.exe");
        fs::write(&p_supp, &pe_supported).unwrap();
        assert_eq!(
            read_capabilities(
                &p_supp,
                &contract.capability_document,
                contract.limits.capability_resource_max_bytes,
                res_type_id,
                &contract.resource.name,
            ),
            CapabilityResult::Supported
        );

        // 2. Absent
        let p_absent = dir.path().join("missing.exe");
        assert_eq!(
            read_capabilities(
                &p_absent,
                &contract.capability_document,
                contract.limits.capability_resource_max_bytes,
                res_type_id,
                &contract.resource.name,
            ),
            CapabilityResult::Absent
        );

        // 3. Malformed ("MZ" only)
        let p_malformed = dir.path().join("malformed.exe");
        fs::write(&p_malformed, b"MZ").unwrap();
        assert_eq!(
            read_capabilities(
                &p_malformed,
                &contract.capability_document,
                contract.limits.capability_resource_max_bytes,
                res_type_id,
                &contract.resource.name,
            ),
            CapabilityResult::Malformed
        );

        // 4. Oversized
        let huge_doc = vec![b' '; 5000];
        let pe_oversized =
            build_synthetic_pe(res_type_id, &contract.resource.name, &huge_doc, false);
        let p_oversized = dir.path().join("oversized.exe");
        fs::write(&p_oversized, &pe_oversized).unwrap();
        assert_eq!(
            read_capabilities(
                &p_oversized,
                &contract.capability_document,
                contract.limits.capability_resource_max_bytes,
                res_type_id,
                &contract.resource.name,
            ),
            CapabilityResult::Oversized
        );

        // 5. Unknown
        let unknown_doc = serde_json::to_vec(&json!({
            "schema": "nope",
            "capabilities": ["other"],
        }))
        .unwrap();
        let pe_unknown =
            build_synthetic_pe(res_type_id, &contract.resource.name, &unknown_doc, false);
        let p_unknown = dir.path().join("unknown.exe");
        fs::write(&p_unknown, &pe_unknown).unwrap();
        assert_eq!(
            read_capabilities(
                &p_unknown,
                &contract.capability_document,
                contract.limits.capability_resource_max_bytes,
                res_type_id,
                &contract.resource.name,
            ),
            CapabilityResult::Unknown
        );

        // 6. NoResource (zeroed resource directory)
        let pe_no_res = build_synthetic_pe(res_type_id, &contract.resource.name, &[], true);
        let p_no_res = dir.path().join("no_res.exe");
        fs::write(&p_no_res, &pe_no_res).unwrap();
        assert_eq!(
            read_capabilities(
                &p_no_res,
                &contract.capability_document,
                contract.limits.capability_resource_max_bytes,
                res_type_id,
                &contract.resource.name,
            ),
            CapabilityResult::NoResource
        );

        // Path resolution tests
        let res =
            resolve_installed_executable(&contract.argv.installed_executable, dir.path()).unwrap();
        assert!(
            res.ends_with(
                Path::new("Solstone")
                    .join("current")
                    .join("solstone-windows-app.exe")
            )
        );

        let bad_template = "%OTHER%\\Solstone\\app.exe";
        assert!(resolve_installed_executable(bad_template, dir.path()).is_err());
    }

    #[derive(Clone, Default)]
    pub struct VirtualClock {
        pub now: std::sync::Arc<std::sync::atomic::AtomicU64>,
        pub sleep_count: std::sync::Arc<std::sync::atomic::AtomicU64>,
        pub total_slept: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }

    impl VirtualClock {
        pub fn new(now: u64) -> Self {
            Self {
                now: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(now)),
                sleep_count: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                total_slept: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            }
        }
    }

    impl WaitClock for VirtualClock {
        fn now_ms(&self) -> u64 {
            self.now.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn sleep_ms(&mut self, ms: u64) {
            self.now.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
            self.sleep_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.total_slept
                .fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
        }
    }

    struct MockEndpoints {
        identity: Result<Value, String>,
        pair_start: Result<Value, String>,
        pair_start_called: std::cell::Cell<bool>,
        on_pair_start: Option<Box<dyn Fn()>>,
    }

    impl HandoffEndpoints for MockEndpoints {
        fn get(&self, _path: &str) -> Result<Value, String> {
            self.identity.clone()
        }

        fn post(&self, _path: &str, _body: &Value) -> Result<Value, String> {
            self.pair_start_called.set(true);
            if let Some(ref cb) = self.on_pair_start {
                cb();
            }
            self.pair_start.clone()
        }
    }

    fn sample_identity() -> Value {
        json!({
            "committed": true,
            "instance_id": "inst-test",
            "mark": {
                "icon1": {
                    "svg": "<path d=\"M0 0h10v10H0z\"/>",
                    "rot": 0,
                    "color": {
                        "name": "blue",
                        "hex": "#0000ff"
                    }
                },
                "icon2": {
                    "svg": "<circle cx=\"5\" cy=\"5\" r=\"5\"/>",
                    "rot": 45,
                    "color": {
                        "name": "red",
                        "hex": "#ff0000"
                    }
                },
                "words": ["north", "stone"]
            }
        })
    }

    fn sample_v04_link(fp_prefix: &[u8; 16]) -> String {
        let mut v04 = vec![0x04, 0x01];
        v04.extend_from_slice(&[127, 0, 0, 1]);
        v04.extend_from_slice(&52443u16.to_be_bytes());
        v04.extend_from_slice(&[0x11; 16]); // nonce
        v04.extend_from_slice(fp_prefix);
        let fragment = spl_core::crockford::encode(&v04);
        format!("https://go.solstone.app/p#{fragment}")
    }

    fn sample_v05_multi_link(fp_prefix: &[u8; 16]) -> String {
        let mut v05 = vec![0x05, 0x01, 0x02]; // count = 2
        v05.extend_from_slice(&52443u16.to_be_bytes());
        v05.extend_from_slice(&[127, 0, 0, 1]);
        v05.extend_from_slice(&[192, 168, 1, 50]);
        v05.extend_from_slice(&[0x11; 16]); // nonce
        v05.extend_from_slice(fp_prefix);
        let fragment = spl_core::crockford::encode(&v05);
        format!("https://go.solstone.app/p#{fragment}")
    }

    #[test]
    fn continue_writes_mailbox_then_launches_or_retries() {
        let contract = get_contract();
        let dir = tempdir().unwrap();
        let local_appdata = dir.path().join("local");
        let state_dir = dir.path().join("state");
        let solstone_dir = local_appdata.join("Solstone");
        fs::create_dir_all(&solstone_dir).unwrap();
        fs::create_dir_all(&state_dir).unwrap();

        let paths = HandoffPaths {
            local_appdata: local_appdata.clone(),
            state_dir: state_dir.clone(),
        };

        // Write supported executable
        let exe_path =
            resolve_installed_executable(&contract.argv.installed_executable, &paths.local_appdata)
                .unwrap();
        fs::create_dir_all(exe_path.parent().unwrap()).unwrap();
        let doc_bytes = serde_json::to_vec(&contract.capability_document).unwrap();
        let res_type_id = resource_type_id(&contract.resource.resource_type);
        let pe_supported =
            build_synthetic_pe(res_type_id, &contract.resource.name, &doc_bytes, false);
        fs::write(&exe_path, &pe_supported).unwrap();

        let fp_prefix = [0xa0u8; 16];
        let link = sample_v04_link(&fp_prefix);
        let full_fp = "a0".repeat(32);

        let pair_resp = json!({
            "pair_link": link,
            "ca_fingerprint": full_fp,
            "expires_in": 60,
        });

        let mut launched = None;
        let endpoints = MockEndpoints {
            identity: Ok(sample_identity()),
            pair_start: Ok(pair_resp.clone()),
            pair_start_called: std::cell::Cell::new(false),
            on_pair_start: None,
        };

        let fixed_id = "01234567-89ab-4cde-8f01-23456789abcd".to_string();
        let fixed_id_copy = fixed_id.clone();

        let collaborators = HandoffCollaborators {
            paths: paths.clone(),
            endpoints: &endpoints,
            launch: |p: &Path, f: &str| {
                launched = Some((p.to_path_buf(), f.to_string()));
                Ok(())
            },
            still_current: || true,
            clock: VirtualClock::new(1000),
            new_id: move || fixed_id_copy.clone(),
        };

        let outcome = continue_in_solstone(collaborators);
        assert_eq!(outcome, ContinueOutcome::Published);
        assert!(endpoints.pair_start_called.get());

        // Verify launch parameters
        let (launch_p, launch_f) = launched.unwrap();
        assert_eq!(launch_p, exe_path);
        assert_eq!(launch_f, contract.argv.cli_flag);

        // Verify mailbox content
        let mb_path = solstone_dir.join(&contract.files.mailbox);
        let mb_bytes = fs::read(&mb_path).unwrap();
        let mb_val: Value = serde_json::from_slice(&mb_bytes).unwrap();
        assert_eq!(mb_val["schema"], contract.schemas.mailbox);
        assert_eq!(mb_val["handoff_id"], fixed_id);
        let step_const = contract
            .fields
            .mailbox
            .required
            .iter()
            .find(|f| f.name == "step")
            .and_then(|f| f.r#const.as_ref())
            .and_then(Value::as_u64)
            .unwrap();
        assert_eq!(mb_val["step"], step_const);
        assert_eq!(mb_val["expected_instance_id"], "inst-test");
        assert_eq!(mb_val["expected_ca_fingerprint"], full_fp);
        assert_eq!(mb_val["pairing_link"], link);
        assert_eq!(mb_val["expires_at_unix_ms"], 1000 + 60 * 1000);

        // Pending file still exists after Published
        assert!(state_dir.join("setup-continuation.json").exists());

        // Multi candidate link with extra non-localhost candidate publishes nothing
        {
            let _ = fs::remove_file(&mb_path);
            let multi_link = sample_v05_multi_link(&fp_prefix);
            let mut launched = false;
            let ep = MockEndpoints {
                identity: Ok(sample_identity()),
                pair_start: Ok(json!({
                    "pair_link": multi_link,
                    "ca_fingerprint": full_fp,
                    "expires_in": 60,
                })),
                pair_start_called: std::cell::Cell::new(false),
                on_pair_start: None,
            };
            let c = HandoffCollaborators {
                paths: paths.clone(),
                endpoints: &ep,
                launch: |_, _| {
                    launched = true;
                    Ok(())
                },
                still_current: || true,
                clock: VirtualClock::new(1000),
                new_id: || fixed_id.clone(),
            };
            assert_eq!(continue_in_solstone(c), ContinueOutcome::Failed);
            assert!(!launched);
            assert!(!mb_path.exists());
        }

        // Test refusal cases where pair-start must NOT be called or mailbox/launch must not happen
        let cases: Vec<(Value, Value)> = vec![
            (
                json!({"committed": false, "instance_id": "inst", "mark": sample_identity()["mark"]}),
                pair_resp.clone(),
            ),
            (
                json!({"committed": true, "instance_id": Value::Null, "mark": sample_identity()["mark"]}),
                pair_resp.clone(),
            ),
            (
                json!({"committed": true, "instance_id": "inst", "mark": Value::Null}),
                pair_resp.clone(),
            ),
            (
                sample_identity(),
                json!({"pair_link": link, "ca_fingerprint": "b0".repeat(32), "expires_in": 60}),
            ),
            (
                sample_identity(),
                json!({"pair_link": link, "ca_fingerprint": full_fp, "expires_in": 0}),
            ),
            (
                sample_identity(),
                json!({"pair_link": link, "ca_fingerprint": full_fp, "expires_in": 1.5}),
            ),
            (
                sample_identity(),
                json!({"pair_link": link, "ca_fingerprint": full_fp, "expires_in": "60"}),
            ),
            (
                sample_identity(),
                json!({"pairing_link": link, "ca_fingerprint": full_fp, "expires_in": 60}), // only pairing_link, no pair_link
            ),
        ];

        for (id_val, p_resp) in cases {
            let _ = fs::remove_file(&mb_path);
            let mut launched = false;
            let ep = MockEndpoints {
                identity: Ok(id_val),
                pair_start: Ok(p_resp),
                pair_start_called: std::cell::Cell::new(false),
                on_pair_start: None,
            };
            let c = HandoffCollaborators {
                paths: paths.clone(),
                endpoints: &ep,
                launch: |_, _| {
                    launched = true;
                    Ok(())
                },
                still_current: || true,
                clock: VirtualClock::new(1000),
                new_id: || fixed_id.clone(),
            };
            assert_eq!(continue_in_solstone(c), ContinueOutcome::Failed);
            assert!(!launched);
            assert!(!mb_path.exists());
        }

        // Deadline already elapsed test
        {
            let clock_now = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1000));
            let clock_now_clone = clock_now.clone();
            let mut launched = false;
            let ep = MockEndpoints {
                identity: Ok(sample_identity()),
                pair_start: Ok(pair_resp.clone()),
                pair_start_called: std::cell::Cell::new(false),
                on_pair_start: Some(Box::new(move || {
                    clock_now_clone.store(1000 + 70 * 1000, std::sync::atomic::Ordering::SeqCst);
                })),
            };
            let c = HandoffCollaborators {
                paths: paths.clone(),
                endpoints: &ep,
                launch: |_, _| {
                    launched = true;
                    Ok(())
                },
                still_current: || true,
                clock: VirtualClock {
                    now: clock_now,
                    sleep_count: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    total_slept: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                },
                new_id: || fixed_id.clone(),
            };
            assert_eq!(continue_in_solstone(c), ContinueOutcome::Failed);
            assert!(!launched);
        }

        // Lock already held test
        {
            let lock_path = solstone_dir.join(&contract.files.lock);
            let held_file = File::create(&lock_path).unwrap();
            held_file.try_lock().unwrap();

            let mut launched = false;
            let ep = MockEndpoints {
                identity: Ok(sample_identity()),
                pair_start: Ok(pair_resp.clone()),
                pair_start_called: std::cell::Cell::new(false),
                on_pair_start: None,
            };
            let c = HandoffCollaborators {
                paths: paths.clone(),
                endpoints: &ep,
                launch: |_, _| {
                    launched = true;
                    Ok(())
                },
                still_current: || true,
                clock: VirtualClock::default(),
                new_id: || fixed_id.clone(),
            };
            assert_eq!(continue_in_solstone(c), ContinueOutcome::Failed);
            assert!(!launched);
            drop(held_file);
        }

        // Mailbox destination replaced by a directory
        {
            let _ = fs::remove_file(&mb_path);
            fs::create_dir_all(&mb_path).unwrap();
            let mut launched = false;
            let ep = MockEndpoints {
                identity: Ok(sample_identity()),
                pair_start: Ok(pair_resp.clone()),
                pair_start_called: std::cell::Cell::new(false),
                on_pair_start: None,
            };
            let c = HandoffCollaborators {
                paths: paths.clone(),
                endpoints: &ep,
                launch: |_, _| {
                    launched = true;
                    Ok(())
                },
                still_current: || true,
                clock: VirtualClock::new(1000),
                new_id: || fixed_id.clone(),
            };
            assert_eq!(continue_in_solstone(c), ContinueOutcome::Failed);
            assert!(!launched);
            let _ = fs::remove_dir(&mb_path);
        }

        // still_current false before work
        {
            let mut launched = false;
            let ep = MockEndpoints {
                identity: Ok(sample_identity()),
                pair_start: Ok(pair_resp.clone()),
                pair_start_called: std::cell::Cell::new(false),
                on_pair_start: None,
            };
            let c = HandoffCollaborators {
                paths: paths.clone(),
                endpoints: &ep,
                launch: |_, _| {
                    launched = true;
                    Ok(())
                },
                still_current: || false,
                clock: VirtualClock::default(),
                new_id: || fixed_id.clone(),
            };
            assert_eq!(continue_in_solstone(c), ContinueOutcome::Superseded);
            assert!(!launched);
            assert!(!ep.pair_start_called.get());
        }

        // pair-start swapping the pending ID publishes nothing
        {
            let _ = fs::remove_file(&mb_path);
            let state_dir_clone = state_dir.clone();
            let ep = MockEndpoints {
                identity: Ok(sample_identity()),
                pair_start: Ok(pair_resp.clone()),
                pair_start_called: std::cell::Cell::new(false),
                on_pair_start: Some(Box::new(move || {
                    let other_id = "99999999-9999-4999-8999-999999999999";
                    write_pending(&state_dir_clone, other_id).unwrap();
                })),
            };
            let mut launched = false;
            let c = HandoffCollaborators {
                paths: paths.clone(),
                endpoints: &ep,
                launch: |_, _| {
                    launched = true;
                    Ok(())
                },
                still_current: || true,
                clock: VirtualClock::new(1000),
                new_id: || fixed_id.clone(),
            };
            assert_eq!(continue_in_solstone(c), ContinueOutcome::Superseded);
            assert!(!launched);
            assert!(!mb_path.exists());
        }

        // Launch Err leaves pending and returns Failed
        {
            write_pending(&state_dir, &fixed_id).unwrap();
            let ep = MockEndpoints {
                identity: Ok(sample_identity()),
                pair_start: Ok(pair_resp.clone()),
                pair_start_called: std::cell::Cell::new(false),
                on_pair_start: None,
            };
            let c = HandoffCollaborators {
                paths: paths.clone(),
                endpoints: &ep,
                launch: |_, _| Err(()),
                still_current: || true,
                clock: VirtualClock::new(1000),
                new_id: || fixed_id.clone(),
            };
            assert_eq!(continue_in_solstone(c), ContinueOutcome::Failed);
            assert!(state_dir.join("setup-continuation.json").exists());
        }

        // Receipt observation tests
        let rc_path = solstone_dir.join(&contract.files.receipt);

        // Missing receipt leaves pending
        let mut clk = VirtualClock::default();
        assert_eq!(observe_receipt(&paths, &mut clk).unwrap(), true);
        assert!(state_dir.join("setup-continuation.json").exists());

        // Receipt ID mismatch (receipt ID A leaves pending B)
        let other_id = "88888888-8888-4888-8888-888888888888";
        let rc_mismatch = json!({
            "schema": contract.schemas.receipt,
            "handoff_id": other_id,
            "accepted": true,
        });
        fs::write(&rc_path, serde_json::to_vec(&rc_mismatch).unwrap()).unwrap();
        assert_eq!(observe_receipt(&paths, &mut clk).unwrap(), true);
        assert!(state_dir.join("setup-continuation.json").exists());

        // Oversized receipt leaves pending
        let large_rc = vec![b' '; 5000];
        fs::write(&rc_path, &large_rc).unwrap();
        assert_eq!(observe_receipt(&paths, &mut clk).unwrap(), true);
        assert!(state_dir.join("setup-continuation.json").exists());

        // Extra field in receipt leaves pending
        let rc_extra = json!({
            "schema": contract.schemas.receipt,
            "handoff_id": fixed_id,
            "accepted": true,
            "extra": "bad",
        });
        fs::write(&rc_path, serde_json::to_vec(&rc_extra).unwrap()).unwrap();
        assert_eq!(observe_receipt(&paths, &mut clk).unwrap(), true);
        assert!(state_dir.join("setup-continuation.json").exists());

        // accepted: false leaves pending
        let rc_declined = json!({
            "schema": contract.schemas.receipt,
            "handoff_id": fixed_id,
            "accepted": false,
        });
        fs::write(&rc_path, serde_json::to_vec(&rc_declined).unwrap()).unwrap();
        assert_eq!(observe_receipt(&paths, &mut clk).unwrap(), true);
        assert!(state_dir.join("setup-continuation.json").exists());

        // Matching receipt deletes ONLY pending; receipt and mailbox remain intact
        let rc_matching = json!({
            "schema": contract.schemas.receipt,
            "handoff_id": fixed_id,
            "accepted": true,
        });
        fs::write(&rc_path, serde_json::to_vec(&rc_matching).unwrap()).unwrap();
        assert_eq!(observe_receipt(&paths, &mut clk).unwrap(), false);
        assert!(!state_dir.join("setup-continuation.json").exists());
        assert!(rc_path.exists());
    }

    #[test]
    fn handoff_lock_is_exclusive_until_drop() {
        let contract = get_contract();
        let dir = tempdir().unwrap();
        let lock_path = dir.path().join(&contract.files.lock);

        let holder_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        holder_file.try_lock().unwrap();

        let mut vclock = VirtualClock::new(0);

        // Bound is contract lock_acquire_ms
        let err = HandoffLock::acquire(&lock_path, contract.limits.lock_acquire_ms, &mut vclock)
            .unwrap_err();
        assert!(matches!(err, HandoffError::Busy));
        assert!(
            vclock.total_slept.load(std::sync::atomic::Ordering::SeqCst)
                >= contract.limits.lock_acquire_ms
        );

        // Bound 0 on held lock returns busy and virtual clock's sleep count stays 0
        let mut zero_clock = VirtualClock::new(0);
        let err0 = HandoffLock::acquire(&lock_path, 0, &mut zero_clock).unwrap_err();
        assert!(matches!(err0, HandoffError::Busy));
        assert_eq!(
            zero_clock
                .sleep_count
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );

        // Drop holder, acquire succeeds
        drop(holder_file);
        let mut acquire_clock = VirtualClock::new(0);
        let lock_ok = HandoffLock::acquire(
            &lock_path,
            contract.limits.lock_acquire_ms,
            &mut acquire_clock,
        );
        assert!(lock_ok.is_ok());
    }
}
