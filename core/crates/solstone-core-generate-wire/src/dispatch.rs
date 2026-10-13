// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Shared provider dispatch for process and app-owned Generate calls.

use serde_json::{Value, json};
use solstone_core_journal_config::read_journal_config;
use solstone_core_local::LocalInferenceAuthority;
use std::path::Path;

/// Resolve the journal's configured lane and run the production Generate policy.
/// The local authority factory is called only for bundled local inference.
///
/// # Errors
/// Returns configuration, response encoding or local-runtime errors. Provider
/// and attestation refusals remain typed Generate responses.
pub fn respond(
    request: &solstone_core_generate::GenerateRequest,
    endpoint_runtime: &crate::EndpointRuntime,
    journal: &Path,
    acquire_local_inference_authority: impl FnOnce(&Path) -> Option<LocalInferenceAuthority>,
) -> Result<solstone_core_generate::GenerateResponse, String> {
    let config = match read_journal_config(journal) {
        Ok(read) => read.config.unwrap_or_default(),
        Err(error) => return Err(format!("could not read journal config: {error}")),
    };
    respond_config(
        request,
        endpoint_runtime,
        journal,
        &config,
        acquire_local_inference_authority,
        true,
    )
}

/// Run the same dispatch against an explicit owner-selected configuration.
/// Candidate credential/model probes use this without mutating journal or
/// process configuration. The caller owns admission of the configuration.
///
/// # Errors
/// Returns response encoding or local-runtime errors; provider refusals stay typed.
pub fn respond_with_config(
    request: &solstone_core_generate::GenerateRequest,
    endpoint_runtime: &crate::EndpointRuntime,
    journal: &Path,
    config: &serde_json::Map<String, Value>,
    acquire_local_inference_authority: impl FnOnce(&Path) -> Option<LocalInferenceAuthority>,
) -> Result<solstone_core_generate::GenerateResponse, String> {
    respond_config(
        request,
        endpoint_runtime,
        journal,
        config,
        acquire_local_inference_authority,
        false,
    )
}

fn respond_config(
    request: &solstone_core_generate::GenerateRequest,
    endpoint_runtime: &crate::EndpointRuntime,
    journal: &Path,
    config: &serde_json::Map<String, Value>,
    acquire_local_inference_authority: impl FnOnce(&Path) -> Option<LocalInferenceAuthority>,
    process_overrides: bool,
) -> Result<solstone_core_generate::GenerateResponse, String> {
    let request_id = request.id.clone();
    let (provider, outcome) = if process_overrides {
        crate::resolve_lane(config)
    } else {
        crate::lane::resolve_lane_with(config, |_| None)
    };
    if !matches!(outcome, crate::LaneOutcome::ConfidentialEndpoint(_)) {
        endpoint_runtime.drain_idle_pool();
    }
    let response = match outcome {
        crate::LaneOutcome::BundledLocal => {
            let mut authority = acquire_local_inference_authority(journal);
            match crate::bundled_generate_with_authority(request, journal, authority.as_mut()) {
                Ok(solstone_core_local::GenerateResult::Success(mut success)) => {
                    let usage = success
                        .usage
                        .as_ref()
                        .map(serde_json::to_value)
                        .transpose()
                        .map_err(|error| error.to_string())?
                        .unwrap_or_else(|| json!({}));
                    let assessment = crate::assess_provider_result(crate::ProviderResultView {
                        journal_path: journal,
                        context: &request.context,
                        model: &success.model,
                        text: &success.text,
                        finish_reason: &success.finish_reason,
                        usage: &usage,
                        json_output: request.json_output,
                        raw_response_snippet: None,
                        thinking_seen: false,
                    });
                    if let Some(error) = assessment.token_log_error {
                        eprintln!("generate token usage log failed: {error}");
                    }
                    if let Some(failure) = assessment.failure {
                        solstone_core_generate::GenerateResponse::Refused(crate::refusal_for(
                            &crate::LaneOutcome::ValidationFailure(failure),
                            &provider,
                            request_id.clone(),
                        ))
                    } else {
                        let schema_validation = apply_schema_validation(
                            &mut success.text,
                            request.json_schema.as_ref(),
                        );
                        let response = generated_response(request, success, schema_validation)?;
                        solstone_core_generate::GenerateResponse::Generated(Box::new(response))
                    }
                }
                Ok(solstone_core_local::GenerateResult::Failure(failure)) => {
                    solstone_core_generate::GenerateResponse::Refused(crate::refusal_for(
                        &crate::LaneOutcome::BundledFailure(Box::new(failure)),
                        &provider,
                        request_id.clone(),
                    ))
                }
                Err(error) => {
                    return Err(format!("bundled local generate is unavailable: {error:?}"));
                }
            }
        }
        crate::LaneOutcome::ByoEndpoint(endpoint) => endpoint_result_response(
            crate::endpoint_generate(request, journal, &endpoint, config, endpoint_runtime),
            request,
            journal,
            &provider,
            request_id.clone(),
        )?,
        crate::LaneOutcome::ConfidentialEndpoint(endpoint) => {
            match crate::confidential_generate(
                request,
                journal,
                &endpoint,
                config,
                endpoint_runtime,
            ) {
                crate::ConfidentialResult::Generated(success) => endpoint_result_response(
                    crate::EndpointResult::Generated(success),
                    request,
                    journal,
                    &provider,
                    request_id.clone(),
                )?,
                crate::ConfidentialResult::Failed(failure) => endpoint_result_response(
                    crate::EndpointResult::Failed(failure),
                    request,
                    journal,
                    &provider,
                    request_id.clone(),
                )?,
                crate::ConfidentialResult::AttestationNotVerified(detail) => {
                    solstone_core_generate::GenerateResponse::Refused(crate::refusal_for(
                        &crate::LaneOutcome::AttestationNotVerified(detail),
                        &provider,
                        request_id.clone(),
                    ))
                }
                crate::ConfidentialResult::AttestationFailed(detail) => {
                    solstone_core_generate::GenerateResponse::Refused(crate::refusal_for(
                        &crate::LaneOutcome::AttestationFailed(detail),
                        &provider,
                        request_id.clone(),
                    ))
                }
            }
        }
        crate::LaneOutcome::Anthropic => match if process_overrides {
            crate::anthropic_generate(request, config)
        } else {
            crate::anthropic::anthropic_generate_from_config(request, config)
        } {
            crate::AnthropicResult::Generated(mut success) => {
                let assessment = crate::assess_provider_result(crate::ProviderResultView {
                    journal_path: journal,
                    context: &request.context,
                    model: &success.model,
                    text: &success.text,
                    finish_reason: &success.finish_reason,
                    usage: &success.usage,
                    json_output: request.json_output,
                    raw_response_snippet: success.raw_response_snippet.as_deref(),
                    thinking_seen: success.thinking.is_some(),
                });
                if let Some(error) = assessment.token_log_error {
                    eprintln!("generate token usage log failed: {error}");
                }
                if let Some(failure) = assessment.failure {
                    solstone_core_generate::GenerateResponse::Refused(crate::refusal_for(
                        &crate::LaneOutcome::ValidationFailure(failure),
                        &provider,
                        request_id.clone(),
                    ))
                } else {
                    let schema_validation =
                        apply_schema_validation(&mut success.text, request.json_schema.as_ref());
                    let response =
                        anthropic_generated_response(request, success, schema_validation)?;
                    solstone_core_generate::GenerateResponse::Generated(Box::new(response))
                }
            }
            crate::AnthropicResult::Failed(failure) => {
                solstone_core_generate::GenerateResponse::Refused(crate::refusal_for(
                    &crate::LaneOutcome::AnthropicFailure(failure),
                    &provider,
                    request_id.clone(),
                ))
            }
        },
        crate::LaneOutcome::OpenAi => match if process_overrides {
            crate::openai_generate(request, config)
        } else {
            crate::openai::openai_generate_from_config(request, config)
        } {
            crate::OpenAiResult::Generated(mut success) => {
                let assessment = crate::assess_provider_result(crate::ProviderResultView {
                    journal_path: journal,
                    context: &request.context,
                    model: &success.model,
                    text: &success.text,
                    finish_reason: &success.finish_reason,
                    usage: &success.usage,
                    json_output: request.json_output,
                    raw_response_snippet: success.raw_response_snippet.as_deref(),
                    thinking_seen: false,
                });
                if let Some(error) = assessment.token_log_error {
                    eprintln!("generate token usage log failed: {error}");
                }
                if let Some(failure) = assessment.failure {
                    solstone_core_generate::GenerateResponse::Refused(crate::refusal_for(
                        &crate::LaneOutcome::ValidationFailure(failure),
                        &provider,
                        request_id.clone(),
                    ))
                } else {
                    let schema_validation =
                        apply_schema_validation(&mut success.text, request.json_schema.as_ref());
                    let response = openai_generated_response(request, success, schema_validation)?;
                    solstone_core_generate::GenerateResponse::Generated(Box::new(response))
                }
            }
            crate::OpenAiResult::Failed(failure) => {
                solstone_core_generate::GenerateResponse::Refused(crate::refusal_for(
                    &crate::LaneOutcome::OpenAiFailure(failure),
                    &provider,
                    request_id.clone(),
                ))
            }
        },
        crate::LaneOutcome::Google => match if process_overrides {
            crate::google_generate(request, config)
        } else {
            crate::google::google_generate_from_config(request, config)
        } {
            crate::GoogleResult::Generated(mut success) => {
                let assessment = crate::assess_provider_result(crate::ProviderResultView {
                    journal_path: journal,
                    context: &request.context,
                    model: &success.model,
                    text: &success.text,
                    finish_reason: &success.finish_reason,
                    usage: &success.usage,
                    json_output: request.json_output,
                    raw_response_snippet: success.raw_response_snippet.as_deref(),
                    thinking_seen: false,
                });
                if let Some(error) = assessment.token_log_error {
                    eprintln!("generate token usage log failed: {error}");
                }
                if let Some(failure) = assessment.failure {
                    solstone_core_generate::GenerateResponse::Refused(crate::refusal_for(
                        &crate::LaneOutcome::ValidationFailure(failure),
                        &provider,
                        request_id.clone(),
                    ))
                } else {
                    let schema_validation =
                        apply_schema_validation(&mut success.text, request.json_schema.as_ref());
                    let response = google_generated_response(request, success, schema_validation)?;
                    solstone_core_generate::GenerateResponse::Generated(Box::new(response))
                }
            }
            crate::GoogleResult::Failed(failure) => {
                solstone_core_generate::GenerateResponse::Refused(crate::refusal_for(
                    &crate::LaneOutcome::GoogleFailure(failure),
                    &provider,
                    request_id.clone(),
                ))
            }
        },
        crate::LaneOutcome::ChatGpt => match if process_overrides {
            crate::chatgpt_generate(request, config, journal)
        } else {
            crate::chatgpt::chatgpt_generate_from_config(request, config, journal)
        } {
            crate::ChatGptResult::Generated(mut success) => {
                let assessment = crate::assess_provider_result(crate::ProviderResultView {
                    journal_path: journal,
                    context: &request.context,
                    model: &success.model,
                    text: &success.text,
                    finish_reason: &success.finish_reason,
                    usage: &success.usage,
                    json_output: request.json_output,
                    raw_response_snippet: success.raw_response_snippet.as_deref(),
                    thinking_seen: false,
                });
                if let Some(error) = assessment.token_log_error {
                    eprintln!("generate token usage log failed: {error}");
                }
                if let Some(failure) = assessment.failure {
                    solstone_core_generate::GenerateResponse::Refused(crate::refusal_for(
                        &crate::LaneOutcome::ValidationFailure(failure),
                        &provider,
                        request_id.clone(),
                    ))
                } else {
                    let schema_validation =
                        apply_schema_validation(&mut success.text, request.json_schema.as_ref());
                    let response = openai_generated_response(request, success, schema_validation)?;
                    solstone_core_generate::GenerateResponse::Generated(Box::new(response))
                }
            }
            crate::ChatGptResult::Failed(failure) => {
                solstone_core_generate::GenerateResponse::Refused(crate::refusal_for(
                    &crate::LaneOutcome::ChatGptFailure(failure),
                    &provider,
                    request_id.clone(),
                ))
            }
        },
        crate::LaneOutcome::NoEngine
        | crate::LaneOutcome::AttestationNotVerified(_)
        | crate::LaneOutcome::AttestationFailed(_)
        | crate::LaneOutcome::AttestationStale
        | crate::LaneOutcome::UnimplementedLane => {
            solstone_core_generate::GenerateResponse::Refused(crate::refusal_for(
                &outcome,
                &provider,
                request_id.clone(),
            ))
        }
        crate::LaneOutcome::BundledFailure(_)
        | crate::LaneOutcome::EndpointFailure(_)
        | crate::LaneOutcome::AnthropicFailure(_)
        | crate::LaneOutcome::OpenAiFailure(_)
        | crate::LaneOutcome::GoogleFailure(_)
        | crate::LaneOutcome::ChatGptFailure(_)
        | crate::LaneOutcome::ValidationFailure(_) => {
            unreachable!("lane resolution cannot return an arm failure")
        }
    };
    Ok(response)
}

fn endpoint_result_response(
    result: crate::EndpointResult,
    request: &solstone_core_generate::GenerateRequest,
    journal: &std::path::Path,
    provider: &str,
    request_id: Option<String>,
) -> Result<solstone_core_generate::GenerateResponse, String> {
    match result {
        crate::EndpointResult::Generated(mut success) => {
            let usage = success
                .usage
                .as_ref()
                .map(serde_json::to_value)
                .transpose()
                .map_err(|error| error.to_string())?
                .unwrap_or_else(|| json!({}));
            let assessment = crate::assess_provider_result(crate::ProviderResultView {
                journal_path: journal,
                context: &request.context,
                model: &success.model,
                text: &success.text,
                finish_reason: &success.finish_reason,
                usage: &usage,
                json_output: request.json_output,
                raw_response_snippet: None,
                thinking_seen: false,
            });
            if let Some(error) = assessment.token_log_error {
                eprintln!("generate token usage log failed: {error}");
            }
            if let Some(failure) = assessment.failure {
                Ok(solstone_core_generate::GenerateResponse::Refused(
                    crate::refusal_for(
                        &crate::LaneOutcome::ValidationFailure(failure),
                        provider,
                        request_id,
                    ),
                ))
            } else {
                let schema_validation =
                    apply_schema_validation(&mut success.text, request.json_schema.as_ref());
                let response = endpoint_generated_response(request, success, schema_validation)?;
                Ok(solstone_core_generate::GenerateResponse::Generated(
                    Box::new(response),
                ))
            }
        }
        crate::EndpointResult::Failed(failure) => Ok(
            solstone_core_generate::GenerateResponse::Refused(crate::refusal_for(
                &crate::LaneOutcome::EndpointFailure(failure),
                provider,
                request_id,
            )),
        ),
    }
}

fn generated_response(
    request: &solstone_core_generate::GenerateRequest,
    success: solstone_core_local::GenerateSuccess,
    schema_validation: Option<Value>,
) -> Result<solstone_core_generate::GeneratedResponse, String> {
    let usage = success
        .usage
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_else(|| json!({}));
    let input_budget = success
        .input_budget
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| error.to_string())?;
    let request_budget = serde_json::to_value(&success.request_budget)
        .map(Some)
        .map_err(|error| error.to_string())?;
    let inference = serde_json::to_value(&success.inference)
        .map(Some)
        .map_err(|error| error.to_string())?;
    let hints_applied = applied_hints(request, true, true);
    Ok(solstone_core_generate::GeneratedResponse {
        id: request.id.clone(),
        text: success.text,
        model: success.model,
        usage,
        finish_reason: success.finish_reason,
        thinking: None,
        schema_validation,
        input_budget,
        request_budget,
        inference,
        hints_applied,
    })
}

/// Which request hints the boundary actually applied.
///
/// 🔴 A hint is reported only by a lane that HONOURED it, and the lanes differ:
/// measured, `bundled` reads `exclusive_admission` and the attempt index,
/// `endpoint` reads `exclusive_admission` to size its admission slot, and the
/// four cloud lanes read neither — their only mention of it is a test fixture.
///
/// ⚠ Two wrong rules were tried before this one, and both passed a real test
/// suite. The original reported `exclusive_admission` from every lane, so the
/// cloud lanes claimed an effect they never had. The correction after that keyed
/// off the response carrying an inference block, which is how the Python
/// reference decided it — but that gates on the *shape of the result*, not on
/// what was honoured, and it silenced the endpoint lane, which genuinely does
/// acquire the slot. ⛔ Do not re-derive this from the reference; it
/// under-reported.
fn applied_hints(
    request: &solstone_core_generate::GenerateRequest,
    honours_attempt_index: bool,
    honours_exclusive_admission: bool,
) -> Vec<String> {
    let mut hints = Vec::new();
    if honours_attempt_index && request.attempt_index != 0 {
        hints.push("attempt_index".to_owned());
    }
    if honours_exclusive_admission && request.exclusive_admission {
        hints.push("exclusive_admission".to_owned());
    }
    hints
}

fn endpoint_generated_response(
    request: &solstone_core_generate::GenerateRequest,
    success: crate::EndpointGenerated,
    schema_validation: Option<Value>,
) -> Result<solstone_core_generate::GeneratedResponse, String> {
    let usage = success
        .usage
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_else(|| json!({}));
    let input_budget = success
        .input_budget
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| error.to_string())?;
    let request_budget = success
        .request_budget
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| error.to_string())?;
    let hints_applied = applied_hints(request, false, true);
    Ok(solstone_core_generate::GeneratedResponse {
        id: request.id.clone(),
        text: success.text,
        model: success.model,
        usage,
        finish_reason: success.finish_reason,
        thinking: None,
        schema_validation,
        input_budget,
        request_budget,
        inference: None,
        hints_applied,
    })
}

fn anthropic_generated_response(
    request: &solstone_core_generate::GenerateRequest,
    success: crate::AnthropicGenerated,
    schema_validation: Option<Value>,
) -> Result<solstone_core_generate::GeneratedResponse, String> {
    let hints_applied = applied_hints(request, false, false);
    Ok(solstone_core_generate::GeneratedResponse {
        id: request.id.clone(),
        text: success.text,
        model: success.model,
        usage: success.usage,
        finish_reason: success.finish_reason,
        thinking: success.thinking,
        schema_validation,
        input_budget: None,
        request_budget: None,
        inference: None,
        hints_applied,
    })
}

fn openai_generated_response(
    request: &solstone_core_generate::GenerateRequest,
    success: crate::OpenAiGenerated,
    schema_validation: Option<Value>,
) -> Result<solstone_core_generate::GeneratedResponse, String> {
    let hints_applied = applied_hints(request, false, false);
    Ok(solstone_core_generate::GeneratedResponse {
        id: request.id.clone(),
        text: success.text,
        model: success.model,
        usage: success.usage,
        finish_reason: success.finish_reason,
        thinking: success.thinking,
        schema_validation,
        input_budget: None,
        request_budget: None,
        inference: None,
        hints_applied,
    })
}

fn google_generated_response(
    request: &solstone_core_generate::GenerateRequest,
    success: crate::GoogleGenerated,
    schema_validation: Option<Value>,
) -> Result<solstone_core_generate::GeneratedResponse, String> {
    let hints_applied = applied_hints(request, false, false);
    Ok(solstone_core_generate::GeneratedResponse {
        id: request.id.clone(),
        text: success.text,
        model: success.model,
        usage: success.usage,
        finish_reason: success.finish_reason,
        thinking: success.thinking,
        schema_validation,
        input_budget: None,
        request_budget: None,
        inference: None,
        hints_applied,
    })
}

fn apply_schema_validation(text: &mut String, schema: Option<&Value>) -> Option<Value> {
    schema.map(|schema| {
        let result = crate::validate_schema_with_annotations(text, schema);
        *text = result.text;
        log_schema_validation(&result.validation);
        result.validation
    })
}

/// Schema validation is advisory, so a failure never changes the outcome and is
/// visible only in the response record — which reaches the caller and nothing
/// else.
///
/// ⚠ The reference emitted one operator-visible line per error and the port
/// dropped it. Nothing failed: the record still carried the block, so the
/// contract-level assertions passed and only the diagnostic disappeared. ⛔ On
/// stderr, never stdout — a line on stdout would corrupt the response stream,
/// which is what the test guarding this is named for.
fn log_schema_validation(validation: &Value) {
    let Some(errors) = validation.get("errors").and_then(Value::as_array) else {
        return;
    };
    for error in errors {
        let path = error.get("path").and_then(Value::as_str).unwrap_or("");
        let constraint = error
            .get("constraint")
            .and_then(Value::as_str)
            .unwrap_or("");
        let message = error.get("message").and_then(Value::as_str).unwrap_or("");
        eprintln!("schema_validation: {path}: {constraint}: {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn anthropic_generated_response_preserves_thinking_blocks() {
        let request = solstone_core_generate::GenerateRequest {
            id: Some("request".into()),
            context: "context".into(),
            contents: Vec::new(),
            system_instruction: None,
            temperature: 0.0,
            max_output_tokens: 1,
            timeout_s: None,
            json_output: false,
            json_schema: None,
            attempt_index: 0,
            exclusive_admission: false,
            transport_retries: None,
        };
        let response = anthropic_generated_response(
            &request,
            crate::AnthropicGenerated {
                text: "text".into(),
                model: "model".into(),
                usage: json!({"input_tokens": 1}),
                finish_reason: "stop".into(),
                thinking: Some(json!({"type": "thinking", "thinking": "work"})),
                raw_response_snippet: None,
            },
            None,
        )
        .unwrap();
        assert_eq!(
            response.thinking,
            Some(json!({"type": "thinking", "thinking": "work"}))
        );
    }
}
