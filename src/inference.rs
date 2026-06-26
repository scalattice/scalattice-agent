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
    pub demo_mode: bool,
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

    pub fn is_ready(&self, demo_mode: bool) -> bool {
        demo_mode || !self.loaded_models().is_empty()
    }

    pub async fn invoke(&self, req: InferenceRequest<'_>) -> Result<InferenceResult> {
        if req.demo_mode {
            return self.invoke_demo(req).await;
        }
        self.invoke_embedded(req).await
    }

    async fn invoke_demo(&self, req: InferenceRequest<'_>) -> Result<InferenceResult> {
        let user = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.as_str())
            .unwrap_or("");

        let pool_label = self.pool.display_name.clone();
        let device_names: Vec<_> = self.pool.devices.iter().map(|d| d.name.as_str()).collect();

        let shard_note = match self.pool.strategy {
            PoolStrategy::TensorParallel => format!(
                "embedded tensor-split {} across {}",
                format_tensor_split(&self.pool.tensor_split),
                device_names.join(" + ")
            ),
            PoolStrategy::GpuWithCpuOffload => format!(
                "embedded GPU layers {} with CPU offload ({})",
                self.pool.gpu_layer_budget,
                device_names.join(" + ")
            ),
            PoolStrategy::CpuOnly => "embedded CPU inference pool".to_string(),
            PoolStrategy::Single => device_names
                .first()
                .unwrap_or(&"device")
                .to_string(),
        };

        let content = format!("[demo · {pool_label} · {shard_note}]\n{user}");
        Ok(InferenceResult {
            prompt_tokens: estimate_tokens(req.messages),
            completion_tokens: estimate_tokens(&[ChatMessage {
                role: "assistant".to_string(),
                content: content.clone(),
            }]),
            content,
        })
    }

    async fn invoke_embedded(&self, req: InferenceRequest<'_>) -> Result<InferenceResult> {
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

fn estimate_tokens(messages: &[ChatMessage]) -> u32 {
    let chars: usize = messages.iter().map(|m| m.content.len()).sum();
    ((chars / 4).max(1)) as u32
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
