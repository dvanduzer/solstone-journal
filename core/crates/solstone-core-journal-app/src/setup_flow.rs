// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Functional reducer and helpers for the Windows journal window setup flow.
//!
//! Owns setup argv construction, route and origin validation, action serialization,
//! copy string formatting, and the state machine across welcome, sign-in, folder selection,
//! setup progress, mark confirmation, thinking configuration, and landing home.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const APP_HOME_PATH: &str = "/app/home";
pub const APP_ORIGIN: &str = "https://journal.localhost";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Welcome,
    SignIn,
    Folder,
    Opening,
    Setup,
    Mark,
    Thinking,
    Home,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserEffect {
    None,
    OpenHome,
    Rejected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupAction {
    RegenerateMark,
    CommitMark { proposal_id: String },
    SaveThinking { choice: String },
    StartThinkingDownload,
    CancelThinkingDownload { attempt_id: String },
    RetrySpeechDownload,
    Finalize { timezone: Option<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    FetchCopy,
    RunSetup {
        journal: PathBuf,
        task_installed: bool,
        start_at_sign_in: bool,
    },
    SetSignInLaunch(bool),
    FetchState,
    PostAction(SetupAction),
    CheckModels {
        generation: u64,
    },
    InstallModels {
        generation: u64,
    },
    Browser(BrowserEffect),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActionErrorPayload {
    pub reason_code: Option<String>,
    pub message: String,
    pub is_transport: bool,
}

#[derive(Clone, Debug)]
pub enum Event {
    Boot {
        task_installed: bool,
    },
    AcknowledgeWelcome,
    ChooseSignIn(bool),
    ContinueSignIn,
    ChooseFolder(PathBuf),
    StartSetup,
    RetrySetup,
    SetupFinished {
        generation: u64,
        result: Result<Option<String>, String>,
    },
    CopyLoaded {
        generation: u64,
        result: Result<Value, String>,
    },
    StateLoaded {
        generation: u64,
        result: Result<Value, String>,
    },
    ActionCompleted {
        generation: u64,
        result: Result<Value, ActionErrorPayload>,
    },
    RegenerateMarkClicked,
    CommitMarkClicked,
    ChooseThinking(String),
    StartThinkingDownloadClicked,
    CancelThinkingDownloadClicked,
    RetrySpeechDownloadClicked,
    FinalizeClicked {
        timezone: Option<String>,
    },
    OpenJournalClicked,
    ModelsCheckFinished {
        generation: u64,
        result: Result<(), String>,
    },
    ModelsInstallFinished {
        generation: u64,
        result: Result<(), String>,
    },
    RetryModelsInstallClicked,
    RetryState,
    Refresh,
    DismissError,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub generation: u64,
    pub inflight: Option<u64>,
    pub models_generation: u64,
    pub phase: Phase,
    pub task_installed: bool,
    pub start_at_sign_in: bool,
    pub journal_path: PathBuf,
    pub copy: Option<Value>,
    pub snapshot: Option<Value>,
    pub models_notice: Option<String>,
    pub stored_redirect: Option<String>,
    pub busy: bool,
    pub setup_failed: bool,
    pub setup_running: bool,
    pub error: Option<String>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            generation: 0,
            inflight: None,
            models_generation: 0,
            phase: Phase::Welcome,
            task_installed: false,
            start_at_sign_in: true,
            journal_path: PathBuf::new(),
            copy: None,
            snapshot: None,
            models_notice: None,
            stored_redirect: None,
            busy: false,
            setup_failed: false,
            setup_running: false,
            error: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub session: Session,
    pub effects: Vec<Effect>,
}

impl Step {
    pub fn browser_effect(&self) -> BrowserEffect {
        for effect in &self.effects {
            if let Effect::Browser(b) = effect {
                return *b;
            }
        }
        BrowserEffect::None
    }
}

pub fn is_app_origin(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://").or_else(|| {
        if url.len() >= 8 && url[..8].eq_ignore_ascii_case("https://") {
            Some(&url[8..])
        } else {
            None
        }
    }) else {
        return false;
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    if !host.eq_ignore_ascii_case("journal.localhost") {
        return false;
    }
    matches!(port, None | Some("443"))
}

pub fn app_asset_path(url: &str) -> Option<String> {
    if !is_app_origin(url) {
        return None;
    }
    let rest = url.strip_prefix("https://").or_else(|| {
        if url.len() >= 8 && url[..8].eq_ignore_ascii_case("https://") {
            Some(&url[8..])
        } else {
            None
        }
    })?;
    let path_start = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let path_part = &rest[path_start..];
    let path_end = path_part.find(['?', '#']).unwrap_or(path_part.len());
    let path = &path_part[..path_end];
    if path.is_empty() {
        Some("/".to_string())
    } else {
        Some(path.to_string())
    }
}

pub fn convey_route_allowed(method: &str, path: &str) -> bool {
    if path.contains(['?', '#']) || path.contains("..") {
        return false;
    }
    matches!(
        (method, path),
        ("GET" | "PUT", "/app/settings/api/config")
            | ("GET", "/app/link/api/identity")
            | ("GET", "/app/network/api/devices")
            | ("POST", "/app/network/pair-start")
            | ("POST", "/app/network/unpair")
            | ("GET", "/app/network/api/local-network")
            | ("POST", "/app/network/local-network/open")
            | ("POST", "/app/network/local-network/close")
            | ("GET", "/init/api/state")
            | ("POST", "/init/api/action")
    )
}

pub fn setup_argv(journal: &Path, task_installed: bool, start_at_sign_in: bool) -> Vec<String> {
    let mut args = vec![
        "setup".to_owned(),
        "--jsonl".to_owned(),
        "--yes".to_owned(),
        "--accept-existing-journal".to_owned(),
        "--skip-models".to_owned(),
        "--skip-brain".to_owned(),
    ];
    if !task_installed {
        args.push("--initial-start-at-sign-in".to_owned());
        args.push(if start_at_sign_in { "on" } else { "off" }.to_owned());
    }
    args.push("--journal".to_owned());
    args.push(journal.display().to_string());
    args
}

fn format_bytes(bytes: u64) -> String {
    let units = ["bytes", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < units.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} bytes")
    } else if value < 10.0 {
        format!("{:.1} {}", value, units[unit])
    } else {
        format!("{:.0} {}", value, units[unit])
    }
}

pub fn filled_default_line(
    template: &str,
    download_bytes: Option<u64>,
    size_unknown: &str,
) -> String {
    let size_str = match download_bytes {
        Some(bytes) => format_bytes(bytes),
        None => size_unknown.to_string(),
    };
    template.replacen("{size}", &size_str, 1)
}

pub fn action_body(action: &SetupAction) -> Value {
    match action {
        SetupAction::RegenerateMark => json!({
            "action": "regenerate_mark"
        }),
        SetupAction::CommitMark { proposal_id } => json!({
            "action": "commit_mark",
            "proposal_id": proposal_id
        }),
        SetupAction::SaveThinking { choice } => json!({
            "action": "save_thinking",
            "choice": choice
        }),
        SetupAction::StartThinkingDownload => json!({
            "action": "start_thinking_download"
        }),
        SetupAction::CancelThinkingDownload { attempt_id } => json!({
            "action": "cancel_thinking_download",
            "attempt_id": attempt_id
        }),
        SetupAction::RetrySpeechDownload => json!({
            "action": "retry_speech_download"
        }),
        SetupAction::Finalize { timezone } => {
            let mut map = serde_json::Map::new();
            map.insert("action".into(), json!("finalize"));
            if let Some(tz) = timezone.as_ref().filter(|tz| !tz.trim().is_empty()) {
                map.insert("timezone".into(), json!(tz));
            }
            Value::Object(map)
        }
    }
}

pub fn speech_retry(snapshot: Option<&Value>) -> bool {
    let Some(s) = snapshot else { return false };
    let state = s.pointer("/downloads/speech/state").and_then(Value::as_str);
    let error_code = s
        .pointer("/downloads/speech/error/code")
        .and_then(Value::as_str);
    state == Some("error") && error_code != Some("speech_package_unavailable")
}

pub fn thinking_retry(snapshot: Option<&Value>) -> bool {
    let Some(s) = snapshot else { return false };
    let saved = s
        .pointer("/thinking/saved")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let choice = s.pointer("/thinking/choice").and_then(Value::as_str);
    let state = s
        .pointer("/downloads/thinking/state")
        .and_then(Value::as_str);
    saved
        && choice == Some("local")
        && matches!(state, Some("error") | Some("cancelled") | Some("idle"))
}

pub fn view(session: &Session) -> Value {
    let mark_render = session
        .snapshot
        .as_ref()
        .and_then(|s| s.pointer("/mark/render"))
        .cloned();
    let mark_state = session
        .snapshot
        .as_ref()
        .and_then(|s| s.pointer("/mark/state"))
        .and_then(Value::as_str);
    let mark_proposal = session
        .snapshot
        .as_ref()
        .and_then(|s| s.pointer("/mark/proposal_id"))
        .and_then(Value::as_str);

    let local_offered = session
        .snapshot
        .as_ref()
        .and_then(|s| s.pointer("/thinking/capability"))
        .and_then(Value::as_str)
        == Some("ok");

    let default_line = match (
        session
            .copy
            .as_ref()
            .and_then(|c| c.pointer("/thinking/default_line"))
            .and_then(Value::as_str),
        session
            .copy
            .as_ref()
            .and_then(|c| c.pointer("/thinking/size_unknown"))
            .and_then(Value::as_str),
    ) {
        (Some(template), Some(size_unknown)) => {
            let download_bytes = session
                .snapshot
                .as_ref()
                .and_then(|s| s.pointer("/thinking/download_bytes"))
                .and_then(Value::as_u64);
            json!(filled_default_line(template, download_bytes, size_unknown))
        }
        _ => Value::Null,
    };

    let downloads = session
        .snapshot
        .as_ref()
        .and_then(|s| s.get("downloads"))
        .cloned()
        .unwrap_or(Value::Null);

    let speech_state = session
        .snapshot
        .as_ref()
        .and_then(|s| s.pointer("/downloads/speech/state"))
        .and_then(Value::as_str);
    let speech_retry_val = speech_retry(session.snapshot.as_ref());

    let thinking_saved = session
        .snapshot
        .as_ref()
        .and_then(|s| s.pointer("/thinking/saved"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let thinking_choice = session
        .snapshot
        .as_ref()
        .and_then(|s| s.pointer("/thinking/choice"))
        .and_then(Value::as_str);
    let thinking_state = session
        .snapshot
        .as_ref()
        .and_then(|s| s.pointer("/downloads/thinking/state"))
        .and_then(Value::as_str);
    let thinking_attempt_id = session
        .snapshot
        .as_ref()
        .and_then(|s| s.pointer("/downloads/thinking/attempt_id"))
        .and_then(Value::as_str);

    let thinking_retry_val = thinking_retry(session.snapshot.as_ref());
    let thinking_cancel = thinking_state == Some("downloading") && thinking_attempt_id.is_some();

    let mark_locked = mark_state == Some("locked");
    let remaining_empty = session
        .snapshot
        .as_ref()
        .and_then(|s| s.get("remaining_required_steps"))
        .and_then(Value::as_array)
        .is_some_and(|arr| arr.is_empty());
    let can_finalize = mark_locked && thinking_saved && remaining_empty;

    let watch_downloads =
        speech_state == Some("downloading") || thinking_state == Some("downloading");

    let destination_rejected = session.stored_redirect.as_deref().is_some()
        && session.stored_redirect.as_deref() != Some(APP_HOME_PATH);

    json!({
        "phase": session.phase,
        "busy": session.busy,
        "error": session.error,
        "sign_in": session.start_at_sign_in,
        "copy": session.copy,
        "mark": {
            "state": mark_state,
            "proposal_id": mark_proposal,
            "render": mark_render,
        },
        "local_offered": local_offered,
        "default_line": default_line,
        "downloads": downloads,
        "speech_retry": speech_retry_val,
        "thinking_retry": thinking_retry_val,
        "thinking_cancel": thinking_cancel,
        "thinking_saved": thinking_saved,
        "thinking_choice": thinking_choice,
        "setup_running": session.phase == Phase::Setup && session.setup_running,
        "setup_failed": session.setup_failed,
        "models": {
            "showing": session.models_notice.is_some(),
            "detail": session.models_notice,
        },
        "can_finalize": can_finalize,
        "watch_downloads": watch_downloads,
        "destination_rejected": destination_rejected,
    })
}

pub fn reduce(session: &Session, event: &Event) -> Step {
    let mut next = session.clone();
    let mut effects = Vec::new();

    match event {
        Event::Boot { task_installed } => {
            next.task_installed = *task_installed;
            next.start_at_sign_in = true;
            next.busy = true;
            next.error = None;
            next.generation += 1;
            next.inflight = Some(next.generation);
            if *task_installed {
                next.phase = Phase::Opening;
                effects.push(Effect::FetchState);
            } else {
                next.phase = Phase::Welcome;
                effects.push(Effect::FetchCopy);
            }
        }
        Event::AcknowledgeWelcome => {
            if next.inflight.is_some() || next.phase != Phase::Welcome {
                return Step {
                    session: next,
                    effects,
                };
            }
            next.phase = Phase::SignIn;
        }
        Event::ChooseSignIn(on) => {
            if next.phase != Phase::SignIn {
                return Step {
                    session: next,
                    effects,
                };
            }
            next.start_at_sign_in = *on;
        }
        Event::ContinueSignIn => {
            if next.phase != Phase::SignIn {
                return Step {
                    session: next,
                    effects,
                };
            }
            next.phase = Phase::Folder;
        }
        Event::ChooseFolder(path) => {
            next.journal_path = path.clone();
        }
        Event::StartSetup => {
            if next.inflight.is_some() || next.phase != Phase::Folder {
                return Step {
                    session: next,
                    effects,
                };
            }
            if next.journal_path.as_os_str().is_empty()
                || next.journal_path.to_string_lossy().trim().is_empty()
            {
                next.error = Some("choose where your journal lives".into());
                return Step {
                    session: next,
                    effects,
                };
            }
            next.setup_failed = false;
            next.setup_running = true;
            next.phase = Phase::Setup;
            next.generation += 1;
            next.inflight = Some(next.generation);
            next.busy = true;
            next.error = None;
            effects.push(Effect::RunSetup {
                journal: next.journal_path.clone(),
                task_installed: next.task_installed,
                start_at_sign_in: next.start_at_sign_in,
            });
        }
        Event::RetrySetup => {
            if next.inflight.is_some() || next.journal_path.as_os_str().is_empty() {
                return Step {
                    session: next,
                    effects,
                };
            }
            next.setup_failed = false;
            next.setup_running = true;
            next.phase = Phase::Setup;
            next.generation += 1;
            next.inflight = Some(next.generation);
            next.busy = true;
            next.error = None;
            effects.push(Effect::RunSetup {
                journal: next.journal_path.clone(),
                task_installed: next.task_installed,
                start_at_sign_in: next.start_at_sign_in,
            });
        }
        Event::SetupFinished { generation, result } => {
            if Some(*generation) != next.inflight {
                return Step {
                    session: next,
                    effects,
                };
            }
            next.setup_running = false;
            next.inflight = None;
            next.busy = false;
            match result {
                Ok(models_err) => {
                    next.setup_failed = false;
                    if !next.task_installed {
                        effects.push(Effect::SetSignInLaunch(next.start_at_sign_in));
                    }
                    if let Some(text) = models_err {
                        next.models_notice = Some(text.clone());
                    }
                    next.models_generation += 1;
                    effects.push(Effect::CheckModels {
                        generation: next.models_generation,
                    });
                    next.generation += 1;
                    next.inflight = Some(next.generation);
                    next.busy = true;
                    effects.push(Effect::FetchState);
                }
                Err(err) => {
                    next.phase = Phase::Setup;
                    next.setup_failed = true;
                    next.error = Some(err.clone());
                }
            }
        }
        Event::CopyLoaded { generation, result } => {
            if Some(*generation) != next.inflight {
                return Step {
                    session: next,
                    effects,
                };
            }
            next.inflight = None;
            next.busy = false;
            match result {
                Ok(val) => {
                    next.copy = Some(val.clone());
                    next.error = None;
                }
                Err(err) => {
                    next.error = Some(err.clone());
                }
            }
        }
        Event::StateLoaded { generation, result } => {
            if Some(*generation) != next.inflight {
                return Step {
                    session: next,
                    effects,
                };
            }
            next.inflight = None;
            next.busy = false;
            match result {
                Ok(state) => {
                    apply_snapshot(&mut next, &mut effects, state);
                }
                Err(err) => {
                    next.error = Some(err.clone());
                }
            }
        }
        Event::ActionCompleted { generation, result } => {
            if Some(*generation) != next.inflight {
                return Step {
                    session: next,
                    effects,
                };
            }
            next.inflight = None;
            next.busy = false;
            match result {
                Ok(state) => {
                    apply_snapshot(&mut next, &mut effects, state);
                }
                Err(payload) => {
                    if payload.is_transport {
                        next.generation += 1;
                        next.inflight = Some(next.generation);
                        next.busy = true;
                        effects.push(Effect::FetchState);
                    } else if payload.reason_code.as_deref() == Some("proposal_conflict") {
                        next.error = Some(payload.message.clone());
                        next.generation += 1;
                        next.inflight = Some(next.generation);
                        next.busy = true;
                        effects.push(Effect::FetchState);
                    } else {
                        next.error = Some(payload.message.clone());
                    }
                }
            }
        }
        Event::RegenerateMarkClicked => {
            if next.inflight.is_some() || next.phase != Phase::Mark {
                return Step {
                    session: next,
                    effects,
                };
            }
            let mark_locked = next
                .snapshot
                .as_ref()
                .and_then(|s| s.pointer("/mark/state"))
                .and_then(Value::as_str)
                == Some("locked");
            if mark_locked {
                return Step {
                    session: next,
                    effects,
                };
            }
            next.generation += 1;
            next.inflight = Some(next.generation);
            next.busy = true;
            next.error = None;
            effects.push(Effect::PostAction(SetupAction::RegenerateMark));
        }
        Event::CommitMarkClicked => {
            if next.inflight.is_some() || next.phase != Phase::Mark {
                return Step {
                    session: next,
                    effects,
                };
            }
            let proposal_id = next
                .snapshot
                .as_ref()
                .and_then(|s| s.pointer("/mark/proposal_id"))
                .and_then(Value::as_str);
            if let Some(pid) = proposal_id.filter(|pid| !pid.trim().is_empty()) {
                next.generation += 1;
                next.inflight = Some(next.generation);
                next.busy = true;
                next.error = None;
                effects.push(Effect::PostAction(SetupAction::CommitMark {
                    proposal_id: pid.to_string(),
                }));
            }
        }
        Event::ChooseThinking(choice) => {
            if next.inflight.is_some() || next.phase != Phase::Thinking {
                return Step {
                    session: next,
                    effects,
                };
            }
            if !matches!(choice.as_str(), "local" | "byo" | "spp" | "later") {
                next.error = Some("unknown thinking choice".into());
                return Step {
                    session: next,
                    effects,
                };
            }
            if choice == "local" {
                let capability = next
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.pointer("/thinking/capability"))
                    .and_then(Value::as_str);
                if capability != Some("ok") {
                    next.error = Some("local thinking is unavailable on this device".into());
                    return Step {
                        session: next,
                        effects,
                    };
                }
            }
            next.generation += 1;
            next.inflight = Some(next.generation);
            next.busy = true;
            next.error = None;
            effects.push(Effect::PostAction(SetupAction::SaveThinking {
                choice: choice.clone(),
            }));
        }
        Event::StartThinkingDownloadClicked => {
            if next.inflight.is_some() || !thinking_retry(next.snapshot.as_ref()) {
                return Step {
                    session: next,
                    effects,
                };
            }
            next.generation += 1;
            next.inflight = Some(next.generation);
            next.busy = true;
            next.error = None;
            effects.push(Effect::PostAction(SetupAction::StartThinkingDownload));
        }
        Event::CancelThinkingDownloadClicked => {
            if next.inflight.is_some() {
                return Step {
                    session: next,
                    effects,
                };
            }
            let attempt_id = next
                .snapshot
                .as_ref()
                .and_then(|s| s.pointer("/downloads/thinking/attempt_id"))
                .and_then(Value::as_str);
            if let Some(aid) = attempt_id.filter(|aid| !aid.trim().is_empty()) {
                next.generation += 1;
                next.inflight = Some(next.generation);
                next.busy = true;
                next.error = None;
                effects.push(Effect::PostAction(SetupAction::CancelThinkingDownload {
                    attempt_id: aid.to_string(),
                }));
            }
        }
        Event::RetrySpeechDownloadClicked => {
            if next.inflight.is_some() || !speech_retry(next.snapshot.as_ref()) {
                return Step {
                    session: next,
                    effects,
                };
            }
            next.generation += 1;
            next.inflight = Some(next.generation);
            next.busy = true;
            next.error = None;
            effects.push(Effect::PostAction(SetupAction::RetrySpeechDownload));
        }
        Event::FinalizeClicked { timezone } => {
            if next.inflight.is_some() || next.phase != Phase::Thinking {
                return Step {
                    session: next,
                    effects,
                };
            }
            let mark_locked = next
                .snapshot
                .as_ref()
                .and_then(|s| s.pointer("/mark/state"))
                .and_then(Value::as_str)
                == Some("locked");
            let thinking_saved = next
                .snapshot
                .as_ref()
                .and_then(|s| s.pointer("/thinking/saved"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let remaining_empty = next
                .snapshot
                .as_ref()
                .and_then(|s| s.get("remaining_required_steps"))
                .and_then(Value::as_array)
                .is_some_and(|arr| arr.is_empty());

            if !mark_locked || !thinking_saved || !remaining_empty {
                return Step {
                    session: next,
                    effects,
                };
            }
            next.generation += 1;
            next.inflight = Some(next.generation);
            next.busy = true;
            next.error = None;
            effects.push(Effect::PostAction(SetupAction::Finalize {
                timezone: timezone.clone(),
            }));
        }
        Event::OpenJournalClicked => {
            if next.stored_redirect.as_deref() == Some(APP_HOME_PATH) {
                effects.push(Effect::Browser(BrowserEffect::OpenHome));
            } else {
                next.error = Some("unexpected redirect destination".into());
                effects.push(Effect::Browser(BrowserEffect::Rejected));
            }
        }
        Event::ModelsCheckFinished { generation, result } => {
            if *generation != next.models_generation {
                return Step {
                    session: next,
                    effects,
                };
            }
            match result {
                Ok(()) => {
                    next.models_notice = None;
                }
                Err(text) => {
                    next.models_notice = Some(text.clone());
                }
            }
        }
        Event::ModelsInstallFinished { generation, result } => {
            if *generation != next.models_generation {
                return Step {
                    session: next,
                    effects,
                };
            }
            match result {
                Ok(()) => {
                    next.models_generation += 1;
                    effects.push(Effect::CheckModels {
                        generation: next.models_generation,
                    });
                }
                Err(text) => {
                    next.models_notice = Some(text.clone());
                }
            }
        }
        Event::RetryModelsInstallClicked => {
            next.models_generation += 1;
            effects.push(Effect::InstallModels {
                generation: next.models_generation,
            });
        }
        Event::RetryState => {
            if next.inflight.is_some()
                || (next.phase == Phase::Setup && !next.setup_failed && next.setup_running)
            {
                return Step {
                    session: next,
                    effects,
                };
            }
            next.generation += 1;
            next.inflight = Some(next.generation);
            next.busy = true;
            next.error = None;
            effects.push(Effect::FetchState);
        }
        Event::Refresh => {
            if next.inflight.is_some() {
                return Step {
                    session: next,
                    effects,
                };
            }
            let speech_downloading = next
                .snapshot
                .as_ref()
                .and_then(|s| s.pointer("/downloads/speech/state"))
                .and_then(Value::as_str)
                == Some("downloading");
            let thinking_downloading = next
                .snapshot
                .as_ref()
                .and_then(|s| s.pointer("/downloads/thinking/state"))
                .and_then(Value::as_str)
                == Some("downloading");
            if speech_downloading || thinking_downloading {
                next.generation += 1;
                next.inflight = Some(next.generation);
                next.busy = true;
                effects.push(Effect::FetchState);
            }
        }
        Event::DismissError => {
            next.error = None;
        }
    }

    Step {
        session: next,
        effects,
    }
}

fn apply_snapshot(session: &mut Session, effects: &mut Vec<Effect>, state: &Value) {
    session.snapshot = Some(state.clone());
    if let Some(c) = state.get("copy").filter(|c| !c.is_null()) {
        session.copy = Some(c.clone());
    }
    let redirect = state
        .get("redirect")
        .and_then(Value::as_str)
        .map(str::to_owned);
    session.stored_redirect = redirect;

    let done = state.get("done").and_then(Value::as_bool).unwrap_or(false);
    if done {
        session.inflight = None;
        session.busy = false;
        if session.stored_redirect.as_deref() == Some(APP_HOME_PATH) {
            session.phase = Phase::Home;
            session.models_generation += 1;
            effects.push(Effect::CheckModels {
                generation: session.models_generation,
            });
        } else {
            session.error = Some("unexpected redirect destination".into());
        }
        return;
    }

    let mark_state = state.pointer("/mark/state").and_then(Value::as_str);
    let proposal_id = state.pointer("/mark/proposal_id").and_then(Value::as_str);
    let mark_locked = mark_state == Some("locked");

    if !mark_locked {
        session.phase = Phase::Mark;
        if proposal_id.is_none() || proposal_id.unwrap().trim().is_empty() {
            session.generation += 1;
            session.inflight = Some(session.generation);
            session.busy = true;
            effects.push(Effect::PostAction(SetupAction::RegenerateMark));
        } else {
            session.inflight = None;
            session.busy = false;
        }
        return;
    }

    let thinking_saved = state
        .pointer("/thinking/saved")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !thinking_saved {
        session.phase = Phase::Thinking;
        session.inflight = None;
        session.busy = false;
        return;
    }

    session.phase = Phase::Thinking;
    session.inflight = None;
    session.busy = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unseeded_snapshot_walk_through_regenerate_commit_thinking_and_finalize() {
        let mut session = Session::default();
        let step = reduce(
            &session,
            &Event::Boot {
                task_installed: false,
            },
        );
        session = step.session;
        assert_eq!(session.phase, Phase::Welcome);
        assert_eq!(step.effects, vec![Effect::FetchCopy]);

        let step = reduce(
            &session,
            &Event::CopyLoaded {
                generation: session.inflight.unwrap(),
                result: Ok(json!({ "welcome": { "title": "Welcome" } })),
            },
        );
        session = step.session;
        assert_eq!(session.phase, Phase::Welcome);
        assert!(session.inflight.is_none());

        let step = reduce(&session, &Event::AcknowledgeWelcome);
        session = step.session;
        assert_eq!(session.phase, Phase::SignIn);

        let step = reduce(&session, &Event::ChooseSignIn(false));
        session = step.session;
        assert!(!session.start_at_sign_in);
        assert_eq!(session.phase, Phase::SignIn);

        let step = reduce(&session, &Event::ContinueSignIn);
        session = step.session;
        assert_eq!(session.phase, Phase::Folder);

        let step = reduce(
            &session,
            &Event::ChooseFolder(PathBuf::from(r"C:\Users\owner\journal")),
        );
        session = step.session;
        assert_eq!(
            session.journal_path,
            PathBuf::from(r"C:\Users\owner\journal")
        );

        let step = reduce(&session, &Event::StartSetup);
        session = step.session;
        assert_eq!(session.phase, Phase::Setup);
        assert_eq!(
            step.effects,
            vec![Effect::RunSetup {
                journal: PathBuf::from(r"C:\Users\owner\journal"),
                task_installed: false,
                start_at_sign_in: false,
            }]
        );

        let current_gen = session.inflight.unwrap();
        // Stale SetupFinished does not apply
        let stale = reduce(
            &session,
            &Event::SetupFinished {
                generation: current_gen - 1,
                result: Ok(None),
            },
        );
        assert_eq!(stale.session, session);
        assert!(stale.effects.is_empty());

        let step = reduce(
            &session,
            &Event::SetupFinished {
                generation: current_gen,
                result: Ok(None),
            },
        );
        session = step.session;
        assert_eq!(
            step.effects,
            vec![
                Effect::SetSignInLaunch(false),
                Effect::CheckModels { generation: 1 },
                Effect::FetchState,
            ]
        );

        // State arrives with unlocked mark and no proposal_id
        let unseeded_state = json!({
            "done": false,
            "redirect": "/app/home",
            "mark": {
                "state": "proposed",
                "proposal_id": null
            },
            "thinking": {
                "saved": false,
                "capability": "ok"
            },
            "remaining_required_steps": ["mark", "thinking"]
        });
        let step = reduce(
            &session,
            &Event::StateLoaded {
                generation: session.inflight.unwrap(),
                result: Ok(unseeded_state),
            },
        );
        session = step.session;
        assert_eq!(session.phase, Phase::Mark);
        assert_eq!(
            step.effects,
            vec![Effect::PostAction(SetupAction::RegenerateMark)]
        );

        // Action completed with generated proposal_id
        let state_with_prop = json!({
            "done": false,
            "redirect": "/app/home",
            "mark": {
                "state": "proposed",
                "proposal_id": "prop-42"
            },
            "thinking": {
                "saved": false,
                "capability": "ok"
            },
            "remaining_required_steps": ["mark", "thinking"]
        });
        let step = reduce(
            &session,
            &Event::ActionCompleted {
                generation: session.inflight.unwrap(),
                result: Ok(state_with_prop),
            },
        );
        session = step.session;
        assert_eq!(session.phase, Phase::Mark);
        assert!(session.inflight.is_none());

        // Commit mark sends the displayed id
        let step = reduce(&session, &Event::CommitMarkClicked);
        session = step.session;
        assert_eq!(
            step.effects,
            vec![Effect::PostAction(SetupAction::CommitMark {
                proposal_id: "prop-42".into(),
            })]
        );

        // Mark locked, thinking unsaved -> moves to Thinking
        let state_mark_locked = json!({
            "done": false,
            "redirect": "/app/home",
            "mark": {
                "state": "locked",
                "proposal_id": "prop-42"
            },
            "thinking": {
                "saved": false,
                "capability": "ok"
            },
            "remaining_required_steps": ["thinking"]
        });
        let step = reduce(
            &session,
            &Event::ActionCompleted {
                generation: session.inflight.unwrap(),
                result: Ok(state_mark_locked),
            },
        );
        session = step.session;
        assert_eq!(session.phase, Phase::Thinking);

        // Choosing local with capability ok posts only SaveThinking, does not start download
        let step = reduce(&session, &Event::ChooseThinking("local".into()));
        session = step.session;
        assert_eq!(
            step.effects,
            vec![Effect::PostAction(SetupAction::SaveThinking {
                choice: "local".into(),
            })]
        );

        // Thinking saved, remaining empty -> can finalize
        let state_thinking_saved = json!({
            "done": false,
            "redirect": "/app/home",
            "mark": {
                "state": "locked",
                "proposal_id": "prop-42"
            },
            "thinking": {
                "saved": true,
                "choice": "local",
                "capability": "ok"
            },
            "remaining_required_steps": []
        });
        let step = reduce(
            &session,
            &Event::ActionCompleted {
                generation: session.inflight.unwrap(),
                result: Ok(state_thinking_saved),
            },
        );
        session = step.session;
        assert_eq!(session.phase, Phase::Thinking);
        let v = view(&session);
        assert_eq!(v["can_finalize"], true);

        // Finalize clicked
        let step = reduce(
            &session,
            &Event::FinalizeClicked {
                timezone: Some("America/Denver".into()),
            },
        );
        session = step.session;
        assert_eq!(
            step.effects,
            vec![Effect::PostAction(SetupAction::Finalize {
                timezone: Some("America/Denver".into()),
            })]
        );

        // Failed finalize stays off Home
        let failed_step = reduce(
            &session,
            &Event::ActionCompleted {
                generation: session.inflight.unwrap(),
                result: Err(ActionErrorPayload {
                    reason_code: Some("failed".into()),
                    message: "finalize refused".into(),
                    is_transport: false,
                }),
            },
        );
        assert_eq!(failed_step.session.phase, Phase::Thinking);
        assert_eq!(
            failed_step.session.error.as_deref(),
            Some("finalize refused")
        );

        // Successful finalize with redirect /app/home reaches Home and emits CheckModels
        let state_done = json!({
            "done": true,
            "redirect": "/app/home",
            "mark": {
                "state": "locked",
                "proposal_id": "prop-42"
            },
            "thinking": {
                "saved": true,
                "choice": "local",
                "capability": "ok"
            },
            "remaining_required_steps": []
        });
        let step = reduce(
            &session,
            &Event::ActionCompleted {
                generation: session.inflight.unwrap(),
                result: Ok(state_done),
            },
        );
        assert_eq!(step.browser_effect(), BrowserEffect::None);
        assert_eq!(step.effects, vec![Effect::CheckModels { generation: 2 }]);
        session = step.session;
        assert_eq!(session.phase, Phase::Home);
    }

    #[test]
    fn argv_fresh_on_fresh_off_resumed_no_skip_service() {
        let p = Path::new(r"C:\journal");
        let on = setup_argv(p, false, true);
        assert_eq!(
            on,
            vec![
                "setup",
                "--jsonl",
                "--yes",
                "--accept-existing-journal",
                "--skip-models",
                "--skip-brain",
                "--initial-start-at-sign-in",
                "on",
                "--journal",
                r"C:\journal",
            ]
        );
        assert!(!on.iter().any(|arg| arg == "--skip-service"));

        let off = setup_argv(p, false, false);
        assert_eq!(
            off,
            vec![
                "setup",
                "--jsonl",
                "--yes",
                "--accept-existing-journal",
                "--skip-models",
                "--skip-brain",
                "--initial-start-at-sign-in",
                "off",
                "--journal",
                r"C:\journal",
            ]
        );
        assert!(!off.iter().any(|arg| arg == "--skip-service"));

        let resumed = setup_argv(p, true, false);
        assert_eq!(
            resumed,
            vec![
                "setup",
                "--jsonl",
                "--yes",
                "--accept-existing-journal",
                "--skip-models",
                "--skip-brain",
                "--journal",
                r"C:\journal",
            ]
        );
        assert!(!resumed.iter().any(|arg| arg == "--skip-service"));
    }

    #[test]
    fn stale_generation_duplicate_click_conflict_and_cancel_handling() {
        let mut session = Session {
            generation: 10,
            inflight: Some(10),
            phase: Phase::Mark,
            snapshot: Some(json!({
                "mark": { "state": "proposed", "proposal_id": "click-id" },
                "downloads": { "thinking": { "state": "downloading", "attempt_id": "att-99" } }
            })),
            ..Default::default()
        };

        // Second click while inflight is dropped
        let step = reduce(&session, &Event::CommitMarkClicked);
        assert_eq!(step.session, session);
        assert!(step.effects.is_empty());

        // Worker result from older generation is dropped
        let step = reduce(
            &session,
            &Event::ActionCompleted {
                generation: 9,
                result: Ok(json!({"done": true})),
            },
        );
        assert_eq!(step.session, session);
        assert!(step.effects.is_empty());

        // Clearing inflight and clicking commit captures displayed id
        session.inflight = None;
        let step = reduce(&session, &Event::CommitMarkClicked);
        session = step.session;
        assert_eq!(session.inflight, Some(11));
        assert_eq!(
            step.effects,
            vec![Effect::PostAction(SetupAction::CommitMark {
                proposal_id: "click-id".into(),
            })]
        );

        // Conflict response emits FetchState and does not commit
        let conflict_step = reduce(
            &session,
            &Event::ActionCompleted {
                generation: 11,
                result: Err(ActionErrorPayload {
                    reason_code: Some("proposal_conflict".into()),
                    message: "identity proposal conflict".into(),
                    is_transport: false,
                }),
            },
        );
        assert_eq!(
            conflict_step.session.error.as_deref(),
            Some("identity proposal conflict")
        );
        assert_eq!(conflict_step.effects, vec![Effect::FetchState]);
        assert_eq!(conflict_step.session.inflight, Some(12));

        // Cancel body captures attempt_id
        session.inflight = None;
        let cancel_step = reduce(&session, &Event::CancelThinkingDownloadClicked);
        assert_eq!(
            cancel_step.effects,
            vec![Effect::PostAction(SetupAction::CancelThinkingDownload {
                attempt_id: "att-99".into(),
            })]
        );
    }

    #[test]
    fn saved_local_survives_failures_retry_and_finalize_with_downloads() {
        let mut session = Session {
            phase: Phase::Thinking,
            snapshot: Some(json!({
                "done": false,
                "redirect": "/app/home",
                "mark": { "state": "locked", "proposal_id": "locked-1" },
                "thinking": { "saved": true, "choice": "local", "capability": "ok" },
                "downloads": {
                    "speech": { "state": "complete" },
                    "thinking": { "state": "error", "error": { "code": "dl_failed", "detail": "net" } }
                },
                "remaining_required_steps": []
            })),
            ..Default::default()
        };

        let v = view(&session);
        assert_eq!(v["thinking_retry"], true);
        assert_eq!(v["can_finalize"], true);

        // Missing remaining_required_steps denies finalize
        let missing_steps_session = Session {
            phase: Phase::Thinking,
            snapshot: Some(json!({
                "done": false,
                "redirect": "/app/home",
                "mark": { "state": "locked", "proposal_id": "locked-1" },
                "thinking": { "saved": true, "choice": "local", "capability": "ok" },
                "downloads": {
                    "speech": { "state": "complete" },
                    "thinking": { "state": "error" }
                }
            })),
            ..Default::default()
        };
        assert_eq!(view(&missing_steps_session)["can_finalize"], false);
        let denied_step = reduce(
            &missing_steps_session,
            &Event::FinalizeClicked { timezone: None },
        );
        assert!(denied_step.effects.is_empty());

        // Retry posts start_thinking_download
        let step = reduce(&session, &Event::StartThinkingDownloadClicked);
        assert_eq!(
            step.effects,
            vec![Effect::PostAction(SetupAction::StartThinkingDownload)]
        );

        // Saved byo, and saved local with no thinking state, -> thinking_retry false and click emits nothing
        let byo_session = Session {
            phase: Phase::Thinking,
            snapshot: Some(json!({
                "thinking": { "saved": true, "choice": "byo", "capability": "ok" },
                "downloads": { "thinking": { "state": "error" } }
            })),
            ..Default::default()
        };
        assert_eq!(view(&byo_session)["thinking_retry"], false);
        let byo_step = reduce(&byo_session, &Event::StartThinkingDownloadClicked);
        assert!(byo_step.effects.is_empty());

        let no_state_session = Session {
            phase: Phase::Thinking,
            snapshot: Some(json!({
                "thinking": { "saved": true, "choice": "local", "capability": "ok" },
                "downloads": {}
            })),
            ..Default::default()
        };
        assert_eq!(view(&no_state_session)["thinking_retry"], false);
        let no_state_step = reduce(&no_state_session, &Event::StartThinkingDownloadClicked);
        assert!(no_state_step.effects.is_empty());

        // Finalize during downloading, error, or cancelled succeeds to Home
        session.inflight = None;
        let step = reduce(&session, &Event::FinalizeClicked { timezone: None });
        session = step.session;
        assert_eq!(
            step.effects,
            vec![Effect::PostAction(SetupAction::Finalize { timezone: None })]
        );

        session.models_notice = Some("bundled asset verification failed: wespeaker".into());
        let state_done_with_downloads = json!({
            "done": true,
            "redirect": "/app/home",
            "mark": { "state": "locked", "proposal_id": "locked-1" },
            "thinking": { "saved": true, "choice": "local", "capability": "ok" },
            "downloads": {
                "speech": { "state": "complete" },
                "thinking": { "state": "downloading", "attempt_id": "att-1" }
            },
            "remaining_required_steps": []
        });
        let step = reduce(
            &session,
            &Event::ActionCompleted {
                generation: session.inflight.unwrap(),
                result: Ok(state_done_with_downloads),
            },
        );
        session = step.session;
        assert_eq!(session.phase, Phase::Home);
        // Speech complete on that home does not clear models_notice
        assert_eq!(
            session.models_notice.as_deref(),
            Some("bundled asset verification failed: wespeaker")
        );
        let v = view(&session);
        assert_eq!(v["downloads"]["speech"]["state"], "complete");
        assert_eq!(v["downloads"]["thinking"]["state"], "downloading");
    }

    #[test]
    fn resume_behavior_not_installed_is_welcome_locked_mark_no_regen_done_is_home() {
        // Not installed -> Welcome, FetchCopy
        let step = reduce(
            &Session::default(),
            &Event::Boot {
                task_installed: false,
            },
        );
        assert_eq!(step.session.phase, Phase::Welcome);
        assert_eq!(step.effects, vec![Effect::FetchCopy]);

        // Installed -> Opening, FetchState, browser None
        let step = reduce(
            &Session::default(),
            &Event::Boot {
                task_installed: true,
            },
        );
        assert_eq!(step.session.phase, Phase::Opening);
        assert_eq!(step.effects, vec![Effect::FetchState]);
        assert_eq!(step.browser_effect(), BrowserEffect::None);

        // Locked proposal in state does not emit regenerate
        let state_locked_no_regen = json!({
            "done": false,
            "redirect": "/app/home",
            "mark": { "state": "locked", "proposal_id": "existing-locked" },
            "thinking": { "saved": false },
            "remaining_required_steps": ["thinking"]
        });
        let step = reduce(
            &step.session,
            &Event::StateLoaded {
                generation: step.session.inflight.unwrap(),
                result: Ok(state_locked_no_regen),
            },
        );
        assert_eq!(step.session.phase, Phase::Thinking);
        assert!(step.effects.is_empty());
        assert!(step.session.inflight.is_none());

        // Done state goes directly Home with no regenerate and no save
        let state_done = json!({
            "done": true,
            "redirect": "/app/home",
            "mark": { "state": "locked", "proposal_id": "existing-locked" },
            "thinking": { "saved": true },
            "remaining_required_steps": []
        });
        let step = reduce(
            &Session {
                inflight: Some(5),
                ..Default::default()
            },
            &Event::StateLoaded {
                generation: 5,
                result: Ok(state_done),
            },
        );
        assert_eq!(step.session.phase, Phase::Home);
        assert_eq!(step.browser_effect(), BrowserEffect::None);
        assert!(
            !step
                .effects
                .iter()
                .any(|e| matches!(e, Effect::PostAction(_)))
        );

        // Done with unexpected redirect stores redirect, phase is not Home, browser effect is None
        let evil_done = json!({
            "done": true,
            "redirect": "https://evil.example/app/home",
            "remaining_required_steps": []
        });
        let evil_step = reduce(
            &Session {
                inflight: Some(6),
                phase: Phase::Opening,
                ..Default::default()
            },
            &Event::StateLoaded {
                generation: 6,
                result: Ok(evil_done),
            },
        );
        assert_ne!(evil_step.session.phase, Phase::Home);
        assert_eq!(
            evil_step.session.stored_redirect.as_deref(),
            Some("https://evil.example/app/home")
        );
        assert_eq!(evil_step.browser_effect(), BrowserEffect::None);
        assert_eq!(
            evil_step.session.error.as_deref(),
            Some("unexpected redirect destination")
        );
    }

    #[test]
    fn browser_effects_spy_enforcement() {
        let step_boot = reduce(
            &Session::default(),
            &Event::Boot {
                task_installed: false,
            },
        );
        assert_eq!(step_boot.browser_effect(), BrowserEffect::None);

        let mut session = Session {
            phase: Phase::Home,
            stored_redirect: Some("/app/home".into()),
            ..Default::default()
        };

        // Open home when stored redirect is /app/home
        let step_open = reduce(&session, &Event::OpenJournalClicked);
        assert_eq!(step_open.browser_effect(), BrowserEffect::OpenHome);
        assert_eq!(
            step_open.effects,
            vec![Effect::Browser(BrowserEffect::OpenHome)]
        );

        // Open home when stored redirect is unexpected
        session.stored_redirect = Some("https://evil.example/app/home".into());
        let step_rejected = reduce(&session, &Event::OpenJournalClicked);
        assert_eq!(step_rejected.browser_effect(), BrowserEffect::Rejected);
        assert_eq!(
            step_rejected.effects,
            vec![Effect::Browser(BrowserEffect::Rejected)]
        );
        assert_eq!(
            step_rejected.session.error.as_deref(),
            Some("unexpected redirect destination")
        );
    }

    #[test]
    fn speech_diagnostics_isolation_and_package_repair() {
        let session = Session {
            phase: Phase::Mark,
            snapshot: Some(json!({
                "mark": { "state": "proposed", "proposal_id": "p-1" },
                "downloads": {
                    "speech": {
                        "state": "error",
                        "error": { "code": "speech_package_unavailable", "detail": "repair or reinstall" }
                    }
                }
            })),
            ..Default::default()
        };

        let v = view(&session);
        assert_eq!(v["speech_retry"], false);

        // Retry clicked does nothing for speech_package_unavailable
        let step = reduce(&session, &Event::RetrySpeechDownloadClicked);
        assert!(step.effects.is_empty());

        // Mark commit still allowed despite speech failure
        let step = reduce(&session, &Event::CommitMarkClicked);
        assert_eq!(
            step.effects,
            vec![Effect::PostAction(SetupAction::CommitMark {
                proposal_id: "p-1".into()
            })]
        );

        // Speech complete click emits nothing
        let complete_session = Session {
            phase: Phase::Mark,
            snapshot: Some(json!({
                "mark": { "state": "proposed", "proposal_id": "p-1" },
                "downloads": { "speech": { "state": "complete" } }
            })),
            ..Default::default()
        };
        assert_eq!(view(&complete_session)["speech_retry"], false);
        let step_complete = reduce(&complete_session, &Event::RetrySpeechDownloadClicked);
        assert!(step_complete.effects.is_empty());

        // Another speech error enables retry
        let session_other_err = Session {
            phase: Phase::Mark,
            snapshot: Some(json!({
                "mark": { "state": "proposed", "proposal_id": "p-1" },
                "downloads": {
                    "speech": {
                        "state": "error",
                        "error": { "code": "network_down", "detail": "timeout" }
                    }
                }
            })),
            ..Default::default()
        };
        let v2 = view(&session_other_err);
        assert_eq!(v2["speech_retry"], true);
        let step_retry = reduce(&session_other_err, &Event::RetrySpeechDownloadClicked);
        assert_eq!(
            step_retry.effects,
            vec![Effect::PostAction(SetupAction::RetrySpeechDownload)]
        );
    }

    #[test]
    fn models_diagnostics_lifecycle_and_independence() {
        let mut session = Session {
            models_generation: 1,
            ..Default::default()
        };

        // Check error displays string unchanged
        let step = reduce(
            &session,
            &Event::ModelsCheckFinished {
                generation: 1,
                result: Err("bundled asset verification failed: wespeaker".into()),
            },
        );
        session = step.session;
        assert_eq!(
            session.models_notice.as_deref(),
            Some("bundled asset verification failed: wespeaker")
        );
        assert_eq!(view(&session)["models"]["showing"], true);

        // Speech complete state load whose generation matches inflight leaves notice in place
        session.inflight = Some(10);
        let step = reduce(
            &session,
            &Event::StateLoaded {
                generation: 10,
                result: Ok(json!({
                    "done": false,
                    "mark": { "state": "proposed", "proposal_id": "p1" },
                    "downloads": { "speech": { "state": "complete" } }
                })),
            },
        );
        assert_eq!(
            step.session.models_notice.as_deref(),
            Some("bundled asset verification failed: wespeaker")
        );

        // Install error replaces the string and stays showing
        session.models_generation = 2;
        let step = reduce(
            &session,
            &Event::ModelsInstallFinished {
                generation: 2,
                result: Err(
                    "object detection is unavailable. repair or reinstall your journal.".into(),
                ),
            },
        );
        session = step.session;
        assert_eq!(
            session.models_notice.as_deref(),
            Some("object detection is unavailable. repair or reinstall your journal.")
        );
        assert_eq!(view(&session)["models"]["showing"], true);

        // Install ok does NOT clear and emits CheckModels at the new generation
        let step = reduce(
            &session,
            &Event::ModelsInstallFinished {
                generation: 2,
                result: Ok(()),
            },
        );
        session = step.session;
        assert_eq!(session.models_generation, 3);
        assert_eq!(step.effects, vec![Effect::CheckModels { generation: 3 }]);
        assert!(session.models_notice.is_some());

        // Stale check result does not clear it
        let stale_step = reduce(
            &session,
            &Event::ModelsCheckFinished {
                generation: 2,
                result: Ok(()),
            },
        );
        assert_eq!(
            stale_step.session.models_notice.as_deref(),
            session.models_notice.as_deref()
        );

        // Later check success clears it
        let step = reduce(
            &session,
            &Event::ModelsCheckFinished {
                generation: 3,
                result: Ok(()),
            },
        );
        session = step.session;
        assert_eq!(session.models_notice, None);
        assert_eq!(view(&session)["models"]["showing"], false);
    }

    #[test]
    fn allowlist_origin_parser_and_hostile_error_handling() {
        // conveys allowed
        assert!(convey_route_allowed("GET", "/init/api/state"));
        assert!(convey_route_allowed("POST", "/init/api/action"));
        assert!(convey_route_allowed("GET", "/app/settings/api/config"));
        assert!(convey_route_allowed("PUT", "/app/settings/api/config"));
        assert!(convey_route_allowed("POST", "/app/network/unpair"));

        // conveys rejected
        assert!(!convey_route_allowed("POST", "/init/api/state"));
        assert!(!convey_route_allowed("GET", "/init/api/action"));
        assert!(!convey_route_allowed("GET", "/init/mark"));
        assert!(!convey_route_allowed("POST", "/init/mark/regenerate"));
        assert!(!convey_route_allowed("POST", "/init/mark/lock"));
        assert!(!convey_route_allowed("POST", "/init/finalize"));
        assert!(!convey_route_allowed("GET", "/init/api/state?q=1"));
        assert!(!convey_route_allowed("GET", "/init/api/state#sec"));
        assert!(!convey_route_allowed(
            "GET",
            "/app/settings/api/config/../../etc"
        ));

        // is_app_origin allowed
        assert!(is_app_origin("https://journal.localhost"));
        assert!(is_app_origin("https://journal.localhost/"));
        assert!(is_app_origin("https://journal.localhost:443"));
        assert!(is_app_origin("https://journal.localhost:443/"));
        assert!(is_app_origin("https://journal.localhost:443/index.html"));
        assert!(is_app_origin("https://journal.localhost/app.js?v=1#anchor"));
        assert!(is_app_origin("HTTPS://JOURNAL.LOCALHOST/INDEX.HTML"));

        // is_app_origin rejected
        assert!(!is_app_origin("https://journal.localhost:444"));
        assert!(!is_app_origin("https://journal.localhost:4433"));
        assert!(!is_app_origin("https://journal.localhost.evil"));
        assert!(!is_app_origin("https://journal.localhost.evil/index.html"));
        assert!(!is_app_origin("https://user@journal.localhost"));
        assert!(!is_app_origin("http://journal.localhost"));
        assert!(!is_app_origin("http://journal.localhost:443"));
        assert!(!is_app_origin(""));
        assert!(!is_app_origin("not-a-url"));

        // app_asset_path cases
        assert_eq!(
            app_asset_path("https://journal.localhost:443/app.js").as_deref(),
            Some("/app.js")
        );
        assert_eq!(
            app_asset_path("https://journal.localhost/app.js?v=1#a").as_deref(),
            Some("/app.js")
        );
        assert_eq!(
            app_asset_path("https://journal.localhost.evil/app.js"),
            None
        );
        assert_eq!(
            app_asset_path("https://user@journal.localhost/app.js"),
            None
        );
        assert_eq!(
            app_asset_path("https://journal.localhost").as_deref(),
            Some("/")
        );
        assert_eq!(
            app_asset_path("https://journal.localhost:443").as_deref(),
            Some("/")
        );
        assert_eq!(
            app_asset_path("https://journal.localhost/").as_deref(),
            Some("/")
        );

        // Hostile error string
        let session = Session {
            inflight: Some(1),
            ..Default::default()
        };
        let step = reduce(
            &session,
            &Event::ActionCompleted {
                generation: 1,
                result: Err(ActionErrorPayload {
                    reason_code: None,
                    message: "<script>alert('xss')</script>".into(),
                    is_transport: false,
                }),
            },
        );
        assert_eq!(
            step.session.error.as_deref(),
            Some("<script>alert('xss')</script>")
        );
        assert_ne!(step.session.phase, Phase::Home);
    }

    #[test]
    fn choose_thinking_local_capability_gate() {
        let blocked_session = Session {
            phase: Phase::Thinking,
            snapshot: Some(json!({
                "thinking": { "capability": "blocked" }
            })),
            ..Default::default()
        };
        let step = reduce(&blocked_session, &Event::ChooseThinking("local".into()));
        assert!(step.effects.is_empty());
        assert!(step.session.error.is_some());

        let ok_session = Session {
            phase: Phase::Thinking,
            snapshot: Some(json!({
                "thinking": { "capability": "ok" }
            })),
            ..Default::default()
        };
        let step = reduce(&ok_session, &Event::ChooseThinking("local".into()));
        assert_eq!(
            step.effects,
            vec![Effect::PostAction(SetupAction::SaveThinking {
                choice: "local".into()
            })]
        );
    }

    #[test]
    fn retry_state_policy() {
        // While inflight is set, ignored
        let inflight_session = Session {
            inflight: Some(1),
            ..Default::default()
        };
        let step = reduce(&inflight_session, &Event::RetryState);
        assert_eq!(step.session, inflight_session);
        assert!(step.effects.is_empty());

        // While phase is Setup, !setup_failed, and setup_running, ignored
        let running_setup_session = Session {
            phase: Phase::Setup,
            setup_failed: false,
            setup_running: true,
            ..Default::default()
        };
        let step = reduce(&running_setup_session, &Event::RetryState);
        assert_eq!(step.session, running_setup_session);
        assert!(step.effects.is_empty());

        // When setup failed, or state read failed, emits FetchState
        let failed_setup_session = Session {
            phase: Phase::Setup,
            setup_failed: true,
            setup_running: false,
            ..Default::default()
        };
        let step = reduce(&failed_setup_session, &Event::RetryState);
        assert_eq!(step.effects, vec![Effect::FetchState]);
        assert_eq!(step.session.inflight, Some(1));

        let opening_session = Session {
            phase: Phase::Opening,
            ..Default::default()
        };
        let step = reduce(&opening_session, &Event::RetryState);
        assert_eq!(step.effects, vec![Effect::FetchState]);
        assert_eq!(step.session.inflight, Some(1));
    }
}
