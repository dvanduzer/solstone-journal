// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Preview and start share a source snapshot and normalized options. Planning
//! runs in the native importer process, away from the web executor.

use crate::{
    AppState,
    http::json as json_response,
    lifecycle::{command, contained_import_file, detect_inprocess_source, source_hint},
    mobile_import_contract::ImportErrorKind,
};
use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::Response,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solstone_core_import::{ImportMetadata, read_import_metadata, write_import_metadata};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreviewError {
    AlreadyRunning,
    NothingToImport,
    SourceChanged(String),
    InvalidRequest(String),
    NotFound(String),
    PlanFailed(String),
}

impl PreviewError {
    pub const fn kind(&self) -> ImportErrorKind {
        match self {
            Self::AlreadyRunning => ImportErrorKind::AlreadyRunning,
            Self::NothingToImport => ImportErrorKind::NothingToImport,
            Self::SourceChanged(_) => ImportErrorKind::SourceChanged,
            Self::InvalidRequest(_) => ImportErrorKind::InvalidRequest,
            Self::NotFound(_) => ImportErrorKind::NotFound,
            Self::PlanFailed(_) => ImportErrorKind::PlanFailed,
        }
    }

    pub const fn owner_sentence(&self) -> &'static str {
        match self {
            Self::AlreadyRunning => "this import is already running.",
            Self::NothingToImport => "there is nothing in this source to import.",
            Self::SourceChanged(_) => {
                "the source or settings changed. preview again before starting."
            }
            Self::InvalidRequest(_) => "choose a source and an import date first.",
            Self::NotFound(_) => "that staged import is not there.",
            Self::PlanFailed(_) => "the import could not be prepared. try again.",
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            Self::AlreadyRunning => "this import is already running",
            Self::NothingToImport => "there are no recordings to import from this source",
            Self::SourceChanged(s)
            | Self::InvalidRequest(s)
            | Self::NotFound(s)
            | Self::PlanFailed(s) => s.as_str(),
        }
    }
}

pub(crate) fn preview_error_body(reason_code: &str, error: &PreviewError) -> Value {
    json!({
        "error": error.owner_sentence(),
        "reason_code": reason_code,
        "detail": error.detail(),
        "kind": error.kind().as_wire(),
    })
}

pub(crate) fn preview_error_response(
    status: StatusCode,
    reason_code: &str,
    error: &PreviewError,
) -> Response {
    json_response(status, preview_error_body(reason_code, error))
}

pub(crate) type PreviewPlanStub = dyn Fn(
        &Path,
        &Value,
        &ImportMetadata,
    )
        -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, PreviewError>> + Send>>
    + Send
    + Sync;

#[derive(Clone)]
pub(crate) enum PreviewPlan<'a> {
    Program(&'a Path),
    #[cfg_attr(not(test), allow(dead_code))]
    Stub(std::sync::Arc<PreviewPlanStub>),
}

struct Input {
    id: String,
    source: PathBuf,
    metadata: ImportMetadata,
    options: Value,
}

fn input(root: &Path, data: &Value) -> Result<Input, PreviewError> {
    let path = data
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| PreviewError::InvalidRequest("choose a source first".to_owned()))?;
    let timestamp = data
        .get("timestamp")
        .and_then(Value::as_str)
        .ok_or_else(|| PreviewError::InvalidRequest("choose an import date first".to_owned()))?;
    solstone_core_import::validate_timestamp(timestamp)
        .map_err(|error| PreviewError::InvalidRequest(error.to_string()))?;
    let id = crate::lifecycle::resolve_import_id(root, data)
        .ok_or_else(|| PreviewError::NotFound("missing staged import".to_owned()))?;
    solstone_core_import::validate_timestamp(&id)
        .map_err(|error| PreviewError::InvalidRequest(error.to_string()))?;
    contained_import_file(root, &id, "import.json")
        .map_err(|error| PreviewError::NotFound(error.to_string()))?;
    let metadata = read_import_metadata(root, &id)
        .map_err(|error| PreviewError::NotFound(error.to_string()))?;
    if metadata.get("file_path").and_then(Value::as_str) != Some(path) {
        return Err(PreviewError::SourceChanged(
            "the staged source changed; choose it again".to_owned(),
        ));
    }
    if matches!(
        solstone_core_import::project_import_result(root, &id).status,
        solstone_core_import::ProjectionStatus::Running
    ) {
        return Err(PreviewError::AlreadyRunning);
    }
    let options = json!({
        "timestamp": timestamp,
        "timestamp_override": crate::lifecycle::owner_timestamp_choice(data, &metadata).is_some(),
        "resolved_timestamp": data.get("resolved_timestamp").and_then(Value::as_str),
        "force": data.get("force").and_then(Value::as_bool).unwrap_or(false),
        "source": source_hint(&metadata),
        "setting": metadata.get("setting"),
        "zone": solstone_core_journal_config::owner_zone(root).name(),
    });
    Ok(Input {
        id,
        source: PathBuf::from(path),
        metadata,
        options,
    })
}

/// The native importer beside this executable, which plans every preview.
pub(crate) fn planner() -> Result<PathBuf, PreviewError> {
    let executable =
        std::env::current_exe().map_err(|error| PreviewError::PlanFailed(error.to_string()))?;
    Ok(executable
        .parent()
        .ok_or_else(|| PreviewError::PlanFailed("cannot locate the importer".to_owned()))?
        .join(if cfg!(windows) {
            "solstone-core.exe"
        } else {
            "solstone-core"
        }))
}

async fn plan(
    program: PreviewPlan<'_>,
    path: &Path,
    options: &Value,
    metadata: &ImportMetadata,
) -> Result<Value, PreviewError> {
    match program {
        PreviewPlan::Program(prog) => {
            plan_using(prog, path, options, metadata, Duration::from_secs(600)).await
        }
        PreviewPlan::Stub(stub) => stub(path, options, metadata).await,
    }
}

async fn plan_using(
    program: &Path,
    path: &Path,
    options: &Value,
    metadata: &ImportMetadata,
    bound: Duration,
) -> Result<Value, PreviewError> {
    let mut metadata = metadata.clone();
    metadata.remove("preview");
    if options["timestamp_override"].as_bool().unwrap_or(false) {
        metadata.insert("owner_timestamp".to_owned(), options["timestamp"].clone());
    }
    let path = path
        .to_str()
        .ok_or_else(|| PreviewError::InvalidRequest("the source path is not UTF-8".to_owned()))?;
    // Start imports an image or document by the kind it was saved as, with or without a source
    // hint. The planner only recognizes those kinds when told, so it is told the same kind.
    if source_hint(&metadata).is_none()
        && let Some(source) = detect_inprocess_source(&metadata, path)
    {
        metadata.insert("source_hint".to_owned(), json!(source.name()));
    }
    let args = command(
        path,
        options["timestamp"]
            .as_str()
            .ok_or_else(|| PreviewError::InvalidRequest("missing timestamp".to_owned()))?,
        &metadata,
        options["force"].as_bool().unwrap_or(false),
    );
    let mut child = tokio::process::Command::new(program);
    child
        .args(&args[2..])
        .args([
            "--dry-run",
            "--json",
            "--preview-zone",
            options["zone"]
                .as_str()
                .ok_or_else(|| PreviewError::PlanFailed("missing time zone".to_owned()))?,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(bound, child.output())
        .await
        .map_err(|_| {
            PreviewError::PlanFailed(
                "the import preview timed out; try a smaller source".to_owned(),
            )
        })?
        .map_err(|error| {
            PreviewError::PlanFailed(format!("could not prepare the import preview: {error}"))
        })?;
    if !output.status.success() {
        return Err(PreviewError::PlanFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let size: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        PreviewError::PlanFailed("the importer did not return a size preview".to_owned())
    })?;
    if size.get("schema").and_then(Value::as_str) != Some("solstone.import.size.v1") {
        return Err(PreviewError::PlanFailed(
            "this source does not provide a recording preview".to_owned(),
        ));
    }
    Ok(size)
}

pub(crate) async fn preview(State(state): State<AppState>, Json(data): Json<Value>) -> Response {
    let resolved_id = crate::lifecycle::resolve_import_id(&state.root, &data);
    let _guard = if let Some(id) = resolved_id.as_deref() {
        Some(crate::start_lock::acquire(&state.root, id).await)
    } else {
        None
    };
    let previewed = match planner() {
        Ok(program) => create_preview(state.root, data, PreviewPlan::Program(&program)).await,
        Err(error) => Err(error),
    };
    match previewed {
        Ok(preview) => json_response(StatusCode::OK, preview),
        Err(error) => {
            preview_error_response(StatusCode::BAD_REQUEST, "import_preview_failed", &error)
        }
    }
}

async fn create_preview(
    root: PathBuf,
    data: Value,
    program: PreviewPlan<'_>,
) -> Result<Value, PreviewError> {
    let snapshot_root = root.clone();
    let (input, snapshot) = tokio::task::spawn_blocking(move || {
        let input = input(&snapshot_root, &data)?;
        let hash = solstone_core_import::hash_source(&input.source)
            .map_err(|error| PreviewError::NotFound(error.to_string()))?;
        if input.metadata.get("source_hash").and_then(Value::as_str) != Some(hash.as_str()) {
            return Err(PreviewError::SourceChanged(
                "the staged source changed; choose it again before previewing".to_owned(),
            ));
        }
        let snapshot = match solstone_core_import::snapshot::create(
            &snapshot_root,
            &input.id,
            &input.source,
        ) {
            Ok(s) => s,
            Err(e) => {
                return Err(match e.as_str() {
                    "choose a source outside its import staging folder" => {
                        PreviewError::InvalidRequest(e)
                    }
                    "source has no filename" => PreviewError::InvalidRequest(e),
                    "the source changed while preparing its preview; try again" => {
                        PreviewError::SourceChanged(e)
                    }
                    "invalid snapshot path" => PreviewError::PlanFailed(e),
                    _ => PreviewError::PlanFailed(e),
                });
            }
        };
        Ok::<_, PreviewError>((input, snapshot))
    })
    .await
    .map_err(|error| PreviewError::PlanFailed(error.to_string()))??;
    let relative_str = snapshot
        .relative
        .to_str()
        .ok_or_else(|| PreviewError::PlanFailed("invalid snapshot path".to_owned()))?;
    let source = contained_import_file(&root, &input.id, relative_str)
        .map_err(|error| PreviewError::NotFound(error.to_string()))?;
    let size = plan(program, &source, &input.options, &input.metadata).await?;
    let record = json!({"source": input.source, "relative": snapshot.relative, "digest": snapshot.digest, "options": input.options, "size": size});
    let token = format!("{:x}", Sha256::digest(record.to_string().as_bytes()));
    let mut record = record;
    record["token"] = json!(token);
    let notice = if input.options["force"].as_bool() == Some(true) {
        let previous = input
            .metadata
            .get("source_timestamp")
            .and_then(Value::as_str)
            .unwrap_or(&input.id);
        let planned = input.options["timestamp"]
            .as_str()
            .ok_or_else(|| PreviewError::InvalidRequest("missing timestamp".to_owned()))?;
        let fresh = input.options["resolved_timestamp"]
            .as_str()
            .unwrap_or(planned);
        match (
            solstone_core_import::validate_timestamp(fresh),
            solstone_core_import::validate_timestamp(previous),
            solstone_core_import::validate_timestamp(planned),
        ) {
            (Ok(fresh), Ok(previous), Ok(planned)) => {
                solstone_core_import::force_day_notice(&fresh, &previous, &planned)
            }
            _ => None,
        }
    } else {
        None
    };
    let response = json!({"preview_token": token, "import_id": input.id, "size": size, "force_notice": notice});
    tokio::task::spawn_blocking(move || {
        let mut metadata = read_import_metadata(&root, &input.id)
            .map_err(|error| PreviewError::NotFound(error.to_string()))?;
        // A settings edit during planning must not bless the earlier options.
        if metadata.get("setting") != input.metadata.get("setting")
            || source_hint(&metadata) != source_hint(&input.metadata)
        {
            return Err(PreviewError::SourceChanged(
                "the import settings changed; preview again".to_owned(),
            ));
        }
        metadata.insert("preview".to_owned(), record);
        write_import_metadata(&root, &input.id, &metadata)
            .map_err(|error| PreviewError::PlanFailed(error.to_string()))?;
        snapshot.persist();
        Ok::<_, PreviewError>(())
    })
    .await
    .map_err(|error| PreviewError::PlanFailed(error.to_string()))??;
    Ok(response)
}

pub(crate) async fn validate_start(
    root: &Path,
    mut data: Value,
    program: PreviewPlan<'_>,
) -> Result<Value, PreviewError> {
    // No client can supply the private, already-validated start fields.
    if let Some(object) = data.as_object_mut() {
        object.remove("_preview_source");
    }
    // A start without a token is the owner's start instruction from a client with no size
    // step, such as a share from the phone. It is previewed here, under the same snapshot,
    // settings and size binding as a preview the owner saw.
    let previewed_here = data.get("preview_token").and_then(Value::as_str).is_none();
    if previewed_here {
        let preview = create_preview(root.to_path_buf(), data.clone(), program.clone()).await?;
        data["preview_token"] = preview["preview_token"].clone();
    }
    let checked_root = root.to_path_buf();
    let checked_data = data.clone();
    let (input, record, snapshot) = tokio::task::spawn_blocking(move || {
        let input = input(&checked_root, &checked_data)?;
        let token = checked_data
            .get("preview_token")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PreviewError::InvalidRequest("preview the import before starting".to_owned())
            })?;
        let record = input.metadata.get("preview").cloned().ok_or_else(|| {
            PreviewError::InvalidRequest("preview the import before starting".to_owned())
        })?;
        if record.get("token").and_then(Value::as_str) != Some(token)
            || record["options"] != input.options
            || record["source"] != json!(input.source)
        {
            return Err(PreviewError::SourceChanged(
                "the source or settings changed; preview again before starting".to_owned(),
            ));
        }
        let digest = record["digest"].as_str().ok_or_else(|| {
            PreviewError::PlanFailed("the saved preview is unreadable".to_owned())
        })?;
        solstone_core_import::snapshot::verify(&input.source, digest)
            .map_err(PreviewError::SourceChanged)?;
        let relative = record["relative"].as_str().ok_or_else(|| {
            PreviewError::PlanFailed("the saved preview is unreadable".to_owned())
        })?;
        let snapshot = contained_import_file(&checked_root, &input.id, relative)
            .map_err(|error| PreviewError::NotFound(error.to_string()))?;
        solstone_core_import::snapshot::verify(&snapshot, digest)
            .map_err(PreviewError::SourceChanged)?;
        Ok::<_, PreviewError>((input, record, snapshot))
    })
    .await
    .map_err(|error| PreviewError::PlanFailed(error.to_string()))??;
    let current = if previewed_here {
        record["size"].clone()
    } else {
        plan(program, &snapshot, &input.options, &input.metadata).await?
    };
    if current != record["size"] {
        return Err(PreviewError::SourceChanged(
            "the import size changed as your journal changed; preview it again".to_owned(),
        ));
    }
    if current["recordings"].as_u64() == Some(0) && current["other_units"].is_null() {
        return Err(PreviewError::NothingToImport);
    }
    data["import_id"] = json!(input.id);
    data["_preview_source"] = record["relative"].clone();
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    const ID: &str = "20261011_120000";
    fn fixture() -> (tempfile::TempDir, Value) {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("imports").join(ID);
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("notes.txt");
        fs::write(&source, "a note").unwrap();
        write_import_metadata(
            root.path(),
            ID,
            json!({"file_path":source, "setting":"office", "source_hash":solstone_core_import::hash_source(&source).unwrap().as_str()})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        (root, json!({"path":source, "timestamp":ID, "import_id":ID}))
    }
    #[tokio::test]
    async fn settings_for_a_local_pointer_use_its_import_id_and_check_the_source() {
        let (root, _) = fixture();
        let source = root.path().join("owner-notes.txt");
        fs::write(&source, "owner material").unwrap();
        let mut metadata = read_import_metadata(root.path(), ID).unwrap();
        metadata.insert("file_path".into(), json!(source));
        write_import_metadata(root.path(), ID, &metadata).unwrap();
        let state = AppState {
            root: root.path().to_path_buf(),
        };
        let response = crate::lifecycle::meta(
            State(state.clone()),
            Json(json!({"path":source,"import_id":ID,"setting":"home"})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            read_import_metadata(root.path(), ID).unwrap()["setting"],
            "home"
        );
        let response = crate::lifecycle::meta(
            State(state),
            Json(
                json!({"path":root.path().join("different.txt"),"import_id":ID,"setting":"wrong"}),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            read_import_metadata(root.path(), ID).unwrap()["setting"],
            "home"
        );
        assert_eq!(fs::read_to_string(source).unwrap(), "owner material");
    }
    #[tokio::test]
    async fn a_start_that_cannot_be_previewed_is_refused_without_admission_or_outputs() {
        let (root, data) = fixture();
        let error = validate_start(
            root.path(),
            data,
            PreviewPlan::Program(&root.path().join("no-importer")),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .detail()
                .contains("could not prepare the import preview"),
            "{:?}",
            error
        );
        assert!(!root.path().join("chronicle").exists());
        assert!(
            read_import_metadata(root.path(), ID)
                .unwrap()
                .get("preview")
                .is_none()
        );
        assert_eq!(
            solstone_core_import::project_import_result(root.path(), ID).status,
            solstone_core_import::ProjectionStatus::Pending
        );
    }
    #[tokio::test]
    async fn changed_source_settings_and_date_refuse_before_planning() {
        let (root, mut data) = fixture();
        let input = input(root.path(), &data).unwrap();
        let snapshot =
            solstone_core_import::snapshot::create(root.path(), ID, &input.source).unwrap();
        let mut metadata = input.metadata.clone();
        metadata.insert(
            "preview".into(),
            json!({"token":"token", "source":input.source,"relative":snapshot.relative,"digest":snapshot.digest,"options":input.options,"size":{"recordings":1,"days":1}}),
        );
        write_import_metadata(root.path(), ID, &metadata).unwrap();
        snapshot.persist();
        data["preview_token"] = json!("token");
        let mut changed = data.clone();
        changed["timestamp"] = json!("20261012_120000");
        assert!(
            validate_start(
                root.path(),
                changed,
                PreviewPlan::Program(Path::new("unused")),
            )
            .await
            .unwrap_err()
            .detail()
            .contains("settings changed")
        );
        metadata.insert("setting".into(), json!("home"));
        write_import_metadata(root.path(), ID, &metadata).unwrap();
        assert!(
            validate_start(
                root.path(),
                data.clone(),
                PreviewPlan::Program(Path::new("unused")),
            )
            .await
            .unwrap_err()
            .detail()
            .contains("settings changed")
        );
        metadata.insert("setting".into(), json!("office"));
        write_import_metadata(root.path(), ID, &metadata).unwrap();
        fs::write(input.source, "a changed note").unwrap();
        assert!(
            validate_start(root.path(), data, PreviewPlan::Program(Path::new("unused")),)
                .await
                .unwrap_err()
                .detail()
                .contains("source changed")
        );
        assert!(!root.path().join("chronicle").exists());
    }
    #[cfg(all(unix, feature = "full-tests"))]
    #[tokio::test]
    async fn a_blocked_preview_keeps_the_executor_responsive_and_times_out() {
        use std::os::unix::fs::PermissionsExt;
        let (root, data) = fixture();
        let input = input(root.path(), &data).unwrap();
        let program = root.path().join("blocked-preview");
        fs::write(&program, "#!/bin/sh\nexec sleep 60\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let preview = plan_using(
            &program,
            &input.source,
            &input.options,
            &input.metadata,
            Duration::from_millis(100),
        );
        tokio::pin!(preview);
        tokio::select! {
            result = &mut preview => panic!("the preview should still be blocked: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(10)) => {},
        }
        assert!(preview.await.unwrap_err().detail().contains("timed out"));
    }
}
