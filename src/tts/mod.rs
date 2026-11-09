//! Text-to-speech. GGUF voices run through llama.cpp `llama-tts` on the slot
//! the server assigned. See [`runner`].

pub mod runner;
pub mod wav;

use crate::protocol::CatalogModel;

/// Hard ceiling on input characters per request (matches OpenAI `audio/speech`).
pub const MAX_INPUT_CHARS: usize = 4096;

#[derive(Debug, Clone)]
pub struct TtsJob {
    pub text: String,
    /// Speaker name. Empty = the model's default voice.
    pub voice: String,
    /// Speaking-rate multiplier. 0 / non-finite = voice default.
    #[allow(dead_code)]
    pub speed: f32,
    pub model: CatalogModel,
}

#[derive(Debug, Clone)]
pub struct TtsOutput {
    pub wav: Vec<u8>,
    pub seconds: f64,
    pub characters: u32,
    #[allow(dead_code)]
    pub sample_rate: u32,
}

pub(crate) fn stub_enabled() -> bool {
    std::env::var("SCALATTICE_TTS_STUB")
        .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes"))
        .unwrap_or(false)
}

/// Voice weights are on disk. Does not require the speech binary, so a download
/// can finish before `scripts/build-tts.sh` has been packaged next to the agent.
pub fn tts_weights_ready(model: &CatalogModel) -> bool {
    if !model.is_tts_job() {
        return false;
    }
    if stub_enabled() {
        return true;
    }
    let Some(weights) = model.weights.as_ref() else {
        return false;
    };
    let name = weights.filename.trim();
    if !name.to_ascii_lowercase().ends_with(".gguf") {
        return false;
    }
    let runtime = if model.runtime_model.trim().is_empty() {
        model.model_id.as_str()
    } else {
        model.runtime_model.trim()
    };
    crate::models::model_weights_ready(runtime)
}

/// Weights are present and llama-tts is installed.
pub fn tts_install_ready(model: &CatalogModel) -> bool {
    tts_weights_ready(model) && runner::runtime_available(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ModelWeights;

    fn row(filename: &str) -> CatalogModel {
        CatalogModel {
            model_id: "qwen3-tts".into(),
            job_kind: "tts".into(),
            weights: Some(ModelWeights {
                source: "huggingface".into(),
                repo: "example/qwen3-tts".into(),
                filename: filename.into(),
                companion_filenames: Vec::new(),
                revision: String::new(),
                download_via: None,
                mirror_url: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn kind_mismatch_codes() {
        let m = row("qwen3-tts.gguf");
        assert!(m.invoke_job_kind_error("tts").is_none());
        assert_eq!(m.invoke_job_kind_error("").unwrap().0, "tts_model_chat_unsupported");
        let chat = CatalogModel::default();
        assert_eq!(
            chat.invoke_job_kind_error("tts").unwrap().0,
            "chat_model_tts_unsupported"
        );
    }

    #[test]
    fn onnx_filename_is_not_a_voice() {
        assert!(!tts_weights_ready(&row("voice.onnx")));
    }
}
