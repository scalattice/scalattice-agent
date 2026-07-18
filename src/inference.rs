use crate::compute_pool::VirtualCard;
use crate::llm::{
    generate, generate_with_callback, preload_model, split_lower, split_upper, GenerateConfig,
    GenerateTimings, SplitLowerConfig, SplitUpperConfig,
};
use crate::models::{list_cached_runtime_models, models_dir, resolve_model_gguf};
use crate::protocol::{ChatMessage, InvokeTimings};
use crate::specs::ComputeDevice;
use anyhow::{Context, Result};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct InferenceRequest<'a> {
    pub job_id: &'a str,
    pub model_id: &'a str,
    pub runtime_model: &'a str,
    pub messages: &'a [ChatMessage],
}

#[derive(Debug, Clone)]
pub struct InferenceResult {
    pub content: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub timings: InvokeTimings,
}

pub struct InferenceEngine {
    pool: VirtualCard,
}

impl InferenceEngine {
    pub fn new(devices: &[ComputeDevice]) -> Result<Self> {
        let pool = crate::compute_pool::build_virtual_card(devices)?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &VirtualCard {
        &self.pool
    }

    pub fn loaded_models(&self) -> Vec<String> {
        list_cached_runtime_models()
    }

    pub async fn invoke_split_lower(
        &self,
        runtime_model: &str,
        prompt_token_ids: &[u32],
    ) -> Result<crate::llm::SplitLowerOutput> {
        let model_path = resolve_model_gguf(runtime_model).with_context(|| {
            format!(
                "model weights not found for {} in {}",
                runtime_model,
                models_dir().display()
            )
        })?;

        let pool = self.pool.clone();
        let ids = prompt_token_ids.to_vec();

        tokio::task::spawn_blocking(move || {
            split_lower(&SplitLowerConfig {
                model_path,
                pool,
                prompt_token_ids: ids,
            })
        })
        .await
        .context("split lower task failed")?
    }

    pub async fn invoke_split_upper(
        &self,
        runtime_model: &str,
        state_b64: &str,
        max_tokens: u32,
    ) -> Result<InferenceResult> {
        let model_path = resolve_model_gguf(runtime_model).with_context(|| {
            format!(
                "model weights not found for {} in {}",
                runtime_model,
                models_dir().display()
            )
        })?;

        let pool = self.pool.clone();
        let state_b64 = state_b64.to_string();

        let output = tokio::task::spawn_blocking(move || {
            split_upper(&SplitUpperConfig {
                model_path,
                pool,
                state_b64,
                max_tokens,
            })
        })
        .await
        .context("split upper task failed")??;

        Ok(InferenceResult {
            content: output.content,
            prompt_tokens: output.prompt_tokens,
            completion_tokens: output.completion_tokens,
            timings: InvokeTimings::default(),
        })
    }

    pub async fn invoke(&self, req: InferenceRequest<'_>) -> Result<InferenceResult> {
        let model_path = resolve_model_gguf(req.runtime_model)
            .with_context(|| {
                format!(
                    "model weights not found for {} in {} (wait for Scalattice to download them, or check agent logs)",
                    req.runtime_model,
                    models_dir().display()
                )
            })?;

        let pool = self.pool.clone();
        let messages = req.messages.to_vec();
        let model_id = req.model_id.to_string();
        let job_id = req.job_id.to_string();
        let model_id_err = req.model_id.to_string();
        let runtime_err = req.runtime_model.to_string();

        let output = tokio::task::spawn_blocking(move || {
            generate(&GenerateConfig {
                model_path,
                pool,
                messages,
                max_tokens: 512,
                model_id,
            })
        })
        .await
        .context("embedded inference task failed")??;

        if output.content.is_empty() {
            anyhow::bail!(
                "embedded inference returned empty output for {} / {} (job {})",
                model_id_err,
                runtime_err,
                job_id
            );
        }

        Ok(InferenceResult {
            content: output.content,
            prompt_tokens: output.prompt_tokens,
            completion_tokens: output.completion_tokens,
            timings: timings_from_generate(&output.timings),
        })
    }

    /// Stream token pieces on `delta_tx`, then return the final sanitized result.
    pub async fn invoke_streaming(
        &self,
        req: InferenceRequest<'_>,
        delta_tx: mpsc::UnboundedSender<String>,
    ) -> Result<InferenceResult> {
        let model_path = resolve_model_gguf(req.runtime_model)
            .with_context(|| {
                format!(
                    "model weights not found for {} in {} (wait for Scalattice to download them, or check agent logs)",
                    req.runtime_model,
                    models_dir().display()
                )
            })?;

        let pool = self.pool.clone();
        let messages = req.messages.to_vec();
        let model_id = req.model_id.to_string();
        let job_id = req.job_id.to_string();
        let model_id_err = req.model_id.to_string();
        let runtime_err = req.runtime_model.to_string();

        let output = tokio::task::spawn_blocking(move || {
            generate_with_callback(
                &GenerateConfig {
                    model_path,
                    pool,
                    messages,
                    max_tokens: 512,
                    model_id,
                },
                |piece| {
                    let _ = delta_tx.send(piece.to_string());
                },
            )
        })
        .await
        .context("embedded streaming inference task failed")??;

        if output.content.is_empty() {
            anyhow::bail!(
                "embedded inference returned empty output for {} / {} (job {})",
                model_id_err,
                runtime_err,
                job_id
            );
        }

        Ok(InferenceResult {
            content: output.content,
            prompt_tokens: output.prompt_tokens,
            completion_tokens: output.completion_tokens,
            timings: timings_from_generate(&output.timings),
        })
    }
}

fn timings_from_generate(t: &GenerateTimings) -> InvokeTimings {
    InvokeTimings {
        model_load_ms: Some(t.model_load_ms),
        prefill_ms: Some(t.prefill_ms),
        decode_ms: Some(t.decode_ms),
        total_ms: Some(t.total_ms),
    }
}

/// Load enabled model weights into GPU memory so the first invoke skips disk load.
pub async fn warm_cached_models(pool: &VirtualCard, runtime_models: &[String]) -> Result<()> {
    if runtime_models.is_empty() {
        return Ok(());
    }
    let pool = pool.clone();
    let models = runtime_models.to_vec();
    tokio::task::spawn_blocking(move || {
        for runtime_model in models {
            if crate::models::should_skip_preload(&runtime_model) {
                continue;
            }
            let Some(model_path) = resolve_model_gguf(&runtime_model) else {
                continue;
            };
            if let Err(error) = preload_model(&model_path, &pool) {
                tracing::warn!(
                    runtime_model = %runtime_model,
                    error = %error,
                    "model preload failed"
                );
                crate::models::handle_weight_load_failure(&runtime_model, &error);
                if crate::models::process_preload_paused() {
                    break;
                }
            }
        }
        Ok(())
    })
    .await
    .context("model preload task failed")?
}

/// Optional health check: ping each CUDA device in the pool (no-op when unavailable).
pub async fn warm_pool_devices(pool: &VirtualCard) -> Result<()> {
    if pool.cuda_device_ids.is_empty() {
        return Ok(());
    }
    let Some(smi) = crate::specs::resolve_nvidia_smi() else {
        return Ok(());
    };
    for index in &pool.cuda_device_ids {
        let mut cmd = std::process::Command::new(&smi);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let _ = cmd
            .args([
                "-i",
                &index.to_string(),
                "--query-gpu=utilization.gpu",
                "--format=csv,noheader,nounits",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    Ok(())
}
