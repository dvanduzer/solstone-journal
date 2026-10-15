// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Preview and start share a source snapshot and normalized options. Planning
//! runs in the native importer process, away from the web executor.

use crate::{
    AppState,
    http::json as json_response,
    lifecycle::{command, contained_import_file, detect_inprocess_source, source_hint},
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

struct Input {
    id: String,
    source: PathBuf,
    metadata: ImportMetadata,
    options: Value,
}

fn input(root: &Path, data: &Value) -> Result<Input, String> {
    let path = data
        .get("path")
        .and_then(Value::as_str)
        .ok_or("choose a source first")?;
    let timestamp = data
        .get("timestamp")
        .and_then(Value::as_str)
        .ok_or("choose an import date first")?;
    solstone_core_import::validate_timestamp(timestamp).map_err(|error| error.to_string())?;
    let id = data
        .get("import_id")
        .and_then(Value::as_str)
        .or_else(|| Path::new(path).parent()?.file_name()?.to_str())
        .ok_or("missing staged import")?;
    solstone_core_import::validate_timestamp(id).map_err(|error| error.to_string())?;
    contained_import_file(root, id, "import.json").map_err(|error| error.to_string())?;
    let metadata = read_import_metadata(root, id).map_err(|error| error.to_string())?;
    if metadata.get("file_path").and_then(Value::as_str) != Some(path) {
        return Err("the staged source changed; choose it again".to_owned());
    }
    if matches!(
        solstone_core_import::project_import_result(root, id).status,
        solstone_core_import::ProjectionStatus::Running
    ) {
        return Err("this import is already running".to_owned());
    }
    let options = json!({
        "timestamp": timestamp,
        "force": data.get("force").and_then(Value::as_bool).unwrap_or(false),
        "source": source_hint(&metadata),
        "setting": metadata.get("setting"),
        "zone": solstone_core_journal_config::owner_zone(root).name(),
    });
    Ok(Input {
        id: id.to_owned(),
        source: PathBuf::from(path),
        metadata,
        options,
    })
}

/// The native importer beside this executable, which plans every preview.
pub(crate) fn planner() -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    Ok(executable
        .parent()
        .ok_or("cannot locate the importer")?
        .join(if cfg!(windows) {
            "solstone-core.exe"
        } else {
            "solstone-core"
        }))
}

async fn plan(
    program: &Path,
    path: &Path,
    options: &Value,
    metadata: &ImportMetadata,
) -> Result<Value, String> {
    plan_using(program, path, options, metadata, Duration::from_secs(600)).await
}

async fn plan_using(
    program: &Path,
    path: &Path,
    options: &Value,
    metadata: &ImportMetadata,
    bound: Duration,
) -> Result<Value, String> {
    let mut metadata = metadata.clone();
    metadata.remove("preview");
    let path = path.to_str().ok_or("the source path is not UTF-8")?;
    // Start imports an image or document by the kind it was saved as, with or without a source
    // hint. The planner only recognizes those kinds when told, so it is told the same kind.
    if source_hint(&metadata).is_none()
        && let Some(source) = detect_inprocess_source(&metadata, path)
    {
        metadata.insert("source_hint".to_owned(), json!(source.name()));
    }
    let args = command(
        path,
        options["timestamp"].as_str().ok_or("missing timestamp")?,
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
            options["zone"].as_str().ok_or("missing time zone")?,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(bound, child.output())
        .await
        .map_err(|_| "the import preview timed out; try a smaller source".to_owned())?
        .map_err(|error| format!("could not prepare the import preview: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let size: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| "the importer did not return a size preview")?;
    if size.get("schema").and_then(Value::as_str) != Some("solstone.import.size.v1") {
        return Err("this source does not provide a recording preview".to_owned());
    }
    Ok(size)
}

pub(crate) async fn preview(State(state): State<AppState>, Json(data): Json<Value>) -> Response {
    let previewed = match planner() {
        Ok(program) => create_preview(state.root, data, &program).await,
        Err(error) => Err(error),
    };
    match previewed {
        Ok(preview) => json_response(StatusCode::OK, preview),
        Err(error) => json_response(
            StatusCode::BAD_REQUEST,
            json!({"error": error, "reason_code": "import_preview_failed"}),
        ),
    }
}

async fn create_preview(root: PathBuf, data: Value, program: &Path) -> Result<Value, String> {
    let snapshot_root = root.clone();
    let (input, snapshot) = tokio::task::spawn_blocking(move || {
        let input = input(&snapshot_root, &data)?;
        let hash =
            solstone_core_import::hash_source(&input.source).map_err(|error| error.to_string())?;
        if input.metadata.get("source_hash").and_then(Value::as_str) != Some(hash.as_str()) {
            return Err("the staged source changed; choose it again before previewing".to_owned());
        }
        let snapshot =
            solstone_core_import::snapshot::create(&snapshot_root, &input.id, &input.source)?;
        Ok::<_, String>((input, snapshot))
    })
    .await
    .map_err(|error| error.to_string())??;
    let source = contained_import_file(
        &root,
        &input.id,
        snapshot.relative.to_str().ok_or("invalid snapshot path")?,
    )
    .map_err(|error| error.to_string())?;
    let size = plan(program, &source, &input.options, &input.metadata).await?;
    let record = json!({"source": input.source, "relative": snapshot.relative, "digest": snapshot.digest, "options": input.options, "size": size});
    let token = format!("{:x}", Sha256::digest(record.to_string().as_bytes()));
    let mut record = record;
    record["token"] = json!(token);
    let response = json!({"preview_token": token, "import_id": input.id, "size": size});
    tokio::task::spawn_blocking(move || {
        let mut metadata =
            read_import_metadata(&root, &input.id).map_err(|error| error.to_string())?;
        // A settings edit during planning must not bless the earlier options.
        if metadata.get("setting") != input.metadata.get("setting")
            || source_hint(&metadata) != source_hint(&input.metadata)
        {
            return Err("the import settings changed; preview again".to_owned());
        }
        metadata.insert("preview".to_owned(), record);
        write_import_metadata(&root, &input.id, &metadata).map_err(|error| error.to_string())?;
        snapshot.persist();
        Ok::<_, String>(())
    })
    .await
    .map_err(|error| error.to_string())??;
    Ok(response)
}

pub(crate) async fn validate_start(
    root: &Path,
    mut data: Value,
    program: &Path,
) -> Result<Value, String> {
    // No client can supply the private, already-validated start fields.
    if let Some(object) = data.as_object_mut() {
        object.remove("_preview_source");
    }
    // A start without a token is the owner's start instruction from a client with no size
    // step, such as a share from the phone. It is previewed here, under the same snapshot,
    // settings and size binding as a preview the owner saw.
    let previewed_here = data.get("preview_token").and_then(Value::as_str).is_none();
    if previewed_here {
        let preview = create_preview(root.to_path_buf(), data.clone(), program).await?;
        data["preview_token"] = preview["preview_token"].clone();
    }
    let checked_root = root.to_path_buf();
    let checked_data = data.clone();
    let (input, record, snapshot) = tokio::task::spawn_blocking(move || {
        let input = input(&checked_root, &checked_data)?;
        let token = checked_data
            .get("preview_token")
            .and_then(Value::as_str)
            .ok_or("preview the import before starting")?;
        let record = input
            .metadata
            .get("preview")
            .cloned()
            .ok_or("preview the import before starting")?;
        if record.get("token").and_then(Value::as_str) != Some(token)
            || record["options"] != input.options
            || record["source"] != json!(input.source)
        {
            return Err("the source or settings changed; preview again before starting".to_owned());
        }
        let digest = record["digest"]
            .as_str()
            .ok_or("the saved preview is unreadable")?;
        solstone_core_import::snapshot::verify(&input.source, digest)?;
        let relative = record["relative"]
            .as_str()
            .ok_or("the saved preview is unreadable")?;
        let snapshot = contained_import_file(&checked_root, &input.id, relative)
            .map_err(|error| error.to_string())?;
        solstone_core_import::snapshot::verify(&snapshot, digest)?;
        Ok::<_, String>((input, record, snapshot))
    })
    .await
    .map_err(|error| error.to_string())??;
    let current = if previewed_here {
        record["size"].clone()
    } else {
        plan(program, &snapshot, &input.options, &input.metadata).await?
    };
    if current != record["size"] {
        return Err("the import size changed as your journal changed; preview it again".to_owned());
    }
    if current["recordings"].as_u64() == Some(0) && current["other_units"].is_null() {
        return Err("there are no recordings to import from this source".to_owned());
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
        write_import_metadata(root.path(), ID, json!({"file_path":source, "setting":"office", "source_hash":solstone_core_import::hash_source(&source).unwrap().as_str()}).as_object().unwrap()).unwrap();
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
        let error = validate_start(root.path(), data, &root.path().join("no-importer"))
            .await
            .unwrap_err();
        assert!(
            error.contains("could not prepare the import preview"),
            "{error}"
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
        metadata.insert("preview".into(), json!({"token":"token", "source":input.source,"relative":snapshot.relative,"digest":snapshot.digest,"options":input.options,"size":{"recordings":1,"days":1}}));
        write_import_metadata(root.path(), ID, &metadata).unwrap();
        snapshot.persist();
        data["preview_token"] = json!("token");
        let mut changed = data.clone();
        changed["timestamp"] = json!("20261012_120000");
        assert!(
            validate_start(root.path(), changed, Path::new("unused"))
                .await
                .unwrap_err()
                .contains("settings changed")
        );
        metadata.insert("setting".into(), json!("home"));
        write_import_metadata(root.path(), ID, &metadata).unwrap();
        assert!(
            validate_start(root.path(), data.clone(), Path::new("unused"))
                .await
                .unwrap_err()
                .contains("settings changed")
        );
        metadata.insert("setting".into(), json!("office"));
        write_import_metadata(root.path(), ID, &metadata).unwrap();
        fs::write(input.source, "a changed note").unwrap();
        assert!(
            validate_start(root.path(), data, Path::new("unused"))
                .await
                .unwrap_err()
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
        assert!(preview.await.unwrap_err().contains("timed out"));
    }
}
