use crate::compute_pool::VirtualCard;
use crate::llm::{
    generate, preload_model, split_lower, split_upper, GenerateConfig, SplitLowerConfig,
    SplitUpperConfig,
};
use crate::models::{list_cached_runtime_models, models_dir, resolve_model_gguf};
use crate::protocol::ChatMessage;
use crate::specs::ComputeDevice;
use anyhow::{Context, Result};

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
                req.model_id,
                req.runtime_model,
                req.job_id
            );
        }

        Ok(InferenceResult {
            content: output.content,
            prompt_tokens: output.prompt_tokens,
            completion_tokens: output.completion_tokens,
        })
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
            let Some(model_path) = resolve_model_gguf(&runtime_model) else {
                continue;
            };
            if let Err(error) = preload_model(&model_path, &pool) {
                tracing::warn!(
                    runtime_model = %runtime_model,
                    error = %error,
                    "model preload failed"
                );
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
    for index in &pool.cuda_device_ids {
        let _ = std::process::Command::new("nvidia-smi")
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
