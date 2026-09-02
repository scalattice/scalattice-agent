use crate::compute_pool::VirtualCard;
use crate::llm::{preload_model, split_lower, split_upper, SplitLowerConfig, SplitUpperConfig};
use crate::models::{models_dir, resolve_model_gguf};
use crate::specs::ComputeDevice;
use anyhow::{Context, Result};

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

    /// Best-effort load of model weights so a following split upper can skip cold start.
    pub async fn invoke_split_warm(&self, runtime_model: &str) -> Result<()> {
        let model_path = resolve_model_gguf(runtime_model).with_context(|| {
            format!(
                "model weights not found for {} in {}",
                runtime_model,
                models_dir().display()
            )
        })?;
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || preload_model(&model_path, &pool))
            .await
            .context("split warm task failed")?
            .context("split warm preload failed")?;
        Ok(())
    }
}
