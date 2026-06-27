use crate::compute_pool::{format_tensor_split, PoolStrategy, VirtualCard};
use crate::llm::{generate, GenerateConfig};
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

    pub fn is_ready(&self) -> bool {
        !self.loaded_models().is_empty()
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
        let runtime_model = req.runtime_model.to_string();

        let output = tokio::task::spawn_blocking(move || {
            generate(&GenerateConfig {
                model_path,
                pool,
                messages,
                max_tokens: 512,
            })
        })
        .await
        .context("embedded inference task failed")??;

        if output.content.is_empty() {
            anyhow::bail!("embedded inference returned empty output for {runtime_model}");
        }

        Ok(InferenceResult {
            content: output.content,
            prompt_tokens: output.prompt_tokens,
            completion_tokens: output.completion_tokens,
        })
    }
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
