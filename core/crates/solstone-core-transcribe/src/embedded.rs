// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Explicit in-process transcription for dedicated mobile runtimes.

use std::path::Path;

use serde_json::Value;

use crate::{TranscribeError, TranscribeFile, backend::parakeet_cpp};

/// Provenance of the model actually used by the embedded caller.
pub struct EmbeddedModelInfo {
    /// Registered backend identifier.
    pub backend: String,
    /// Model identity.
    pub model: String,
    /// Compute device.
    pub device: String,
    /// Compute precision.
    pub compute_type: String,
}

/// A callable local recognizer. Input is mono 16 kHz PCM; output uses the normal
/// verbose JSON word contract (`word`, `start`, `end`, optional `conf`).
pub trait EmbeddedAsr {
    /// Transcribe one bounded set of samples, including word timings.
    fn transcribe(&self, samples: &[f32]) -> Result<Value, String>;
    /// Describe the loaded model and compute device.
    fn model_info(&self) -> EmbeddedModelInfo;
}

pub(crate) struct EmbeddedContext<'a> {
    pub(crate) models: &'a Path,
    pub(crate) scratch: &'a Path,
    pub(crate) asr: &'a dyn EmbeddedAsr,
    pub(crate) cancel: &'a dyn Fn() -> bool,
}

impl EmbeddedContext<'_> {
    pub(crate) fn checkpoint(&self) -> Result<(), TranscribeError> {
        if (self.cancel)() {
            Err(TranscribeError::Cancelled)
        } else {
            Ok(())
        }
    }

    pub(crate) fn transcribe(
        &self,
        samples: &[f32],
    ) -> Result<(parakeet_cpp::TranscriptionResponse, parakeet_cpp::ModelInfo), TranscribeError>
    {
        self.checkpoint()?;
        let payload =
            self.asr
                .transcribe(samples)
                .map_err(|detail| TranscribeError::ParakeetCppFailure {
                    reason: "embedded-inference".to_owned(),
                    detail,
                })?;
        self.checkpoint()?;
        let transcript =
            parakeet_cpp::parse_verbose_json(&payload.to_string()).map_err(|error| {
                TranscribeError::ParakeetCppFailure {
                    reason: "embedded-word-contract".to_owned(),
                    detail: format!("{error:?}"),
                }
            })?;
        let info = self.asr.model_info();
        Ok((
            transcript,
            parakeet_cpp::ModelInfo {
                model: info.model,
                device: info.device,
                compute_type: info.compute_type,
            },
        ))
    }
}

/// Run the normal claimed decode/VAD/ASR/speaker/publication pipeline in process.
///
/// The caller opts in explicitly and supplies app-private pinned model and scratch
/// directories. Cancellation is cooperative between stages and immediately before
/// publication. Inference already executing is allowed to return. Errors retain
/// the raw input and do not publish a successful transcript.
pub fn process_embedded(
    audio: &Path,
    journal: &Path,
    models: &Path,
    scratch: &Path,
    asr: &dyn EmbeddedAsr,
    cancel: &dyn Fn() -> bool,
) -> Result<TranscribeFile, TranscribeError> {
    let context = EmbeddedContext {
        models,
        scratch,
        asr,
        cancel,
    };
    context.checkpoint()?;
    std::fs::create_dir_all(scratch).map_err(|error| TranscribeError::VadTemporary {
        detail: format!("could not create embedded scratch: {error}"),
    })?;
    let config = crate::config::read_transcribe_config(journal).map_err(|error| {
        TranscribeError::ProcessingClaim {
            path: audio.to_path_buf(),
            detail: error.to_string(),
        }
    })?;
    let outcome = crate::stage::process_one_with(
        audio,
        journal,
        false,
        None,
        &config,
        &solstone_core_spp_ratls::AttestationStateStore::new(),
        Some(&context),
    )?;
    Ok(crate::record_transcribe_file(outcome, audio))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct CancelDuringAsr(AtomicBool);
    impl EmbeddedAsr for CancelDuringAsr {
        fn transcribe(&self, _: &[f32]) -> Result<Value, String> {
            self.0.store(true, Ordering::Release);
            Ok(
                serde_json::json!({"text":"hello", "words":[{"word":"hello", "start":0.0, "end":0.5}]}),
            )
        }
        fn model_info(&self) -> EmbeddedModelInfo {
            panic!("cancelled inference must not be accepted")
        }
    }

    #[test]
    fn cancellation_during_native_inference_discards_its_result() {
        let asr = CancelDuringAsr(AtomicBool::new(false));
        let cancel = || asr.0.load(Ordering::Acquire);
        let context = EmbeddedContext {
            models: Path::new("unused"),
            scratch: Path::new("unused"),
            asr: &asr,
            cancel: &cancel,
        };
        assert!(matches!(
            context.transcribe(&[0.0; 16000]),
            Err(TranscribeError::Cancelled)
        ));
        assert!(matches!(
            process_embedded(
                Path::new("unused"),
                Path::new("unused"),
                Path::new("unused"),
                Path::new("unused"),
                &asr,
                &cancel
            ),
            Err(TranscribeError::Cancelled)
        ));
    }

    #[test]
    fn duration_budget_retains_original_without_terminal_success() {
        let root = tempfile::tempdir().unwrap();
        let segment = root.path().join("chronicle/20261012/audio/120000_31");
        std::fs::create_dir_all(&segment).unwrap();
        let audio = segment.join("audio.wav");
        let bytes =
            solstone_core_observe_audio::audio_to_wav_bytes(&vec![0.0; 31 * 16000], 16000).unwrap();
        std::fs::write(&audio, &bytes).unwrap();
        let asr = CancelDuringAsr(AtomicBool::new(false));
        let result = process_embedded(
            &audio,
            root.path(),
            Path::new("missing-models"),
            &root.path().join("scratch"),
            &asr,
            &|| false,
        );
        assert!(
            matches!(result, Err(TranscribeError::ParakeetCppDeferred { reason, .. }) if reason == "embedded-duration-budget")
        );
        assert_eq!(std::fs::read(&audio).unwrap(), bytes);
        assert!(!audio.with_extension("jsonl").exists());
        assert!(!audio.with_extension("npz").exists());
    }
}
