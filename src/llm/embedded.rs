//! Embedded llama.cpp inference (no external llama-cli binary required).

use crate::compute_pool::{PoolStrategy, VirtualCard};
use crate::protocol::ChatMessage;
use anyhow::{Context, Result};
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::LogOptions;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::model::params::{LlamaModelParams, LlamaSplitMode};
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::OnceLock;

use super::prompt::{build_chat_prompt, sanitize_completion};

static BACKEND: OnceLock<Result<LlamaBackend, String>> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct GenerateConfig {
    pub model_path: PathBuf,
    pub pool: VirtualCard,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u32,
    pub model_id: String,
}

#[derive(Debug, Clone)]
pub struct GenerateOutput {
    pub content: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

pub fn init_backend() -> Result<()> {
    llama_cpp_2::send_logs_to_tracing(LogOptions::default());
    backend().map(|_| ())
}

pub(crate) fn decode_token(model: &LlamaModel, token: LlamaToken) -> Result<String> {
    let mut decoder = encoding_rs::UTF_8.new_decoder();
    model
        .token_to_piece(token, &mut decoder, true, None)
        .context("decode generated token")
}

pub(crate) fn backend() -> Result<&'static LlamaBackend> {
    BACKEND
        .get_or_init(|| LlamaBackend::init().map_err(|err| err.to_string()))
        .as_ref()
        .map_err(|err| anyhow::anyhow!(err.clone()))
}

pub fn generate(config: &GenerateConfig) -> Result<GenerateOutput> {
    let backend = backend()?;
    let ctx_params = LlamaContextParams::default().with_n_ctx(Some(
        NonZeroU32::new(4096).context("invalid default context size")?,
    ));

    super::model_cache::with_loaded_model(&config.model_path, &config.pool, |model| {
        let mut ctx = model
            .new_context(backend, ctx_params)
            .context("create llama context")?;

        let prompt = build_chat_prompt(model, &config.messages)?;
        let mut prompt_tokens = model
            .str_to_token(&prompt, AddBos::Never)
            .context("tokenize prompt")?;

        let max_tokens = config.max_tokens.max(1).min(2048) as usize;
        if prompt_tokens.len() + max_tokens > ctx.n_ctx() as usize {
            anyhow::bail!(
                "prompt too long for context window ({} + {} > {})",
                prompt_tokens.len(),
                max_tokens,
                ctx.n_ctx()
            );
        }

        let prompt_token_count = prompt_tokens.len() as u32;
        let mut batch = LlamaBatch::new(prompt_tokens.len().max(1), 1);
        let last = prompt_tokens.len().saturating_sub(1);
        for (pos, token) in prompt_tokens.drain(..).enumerate() {
            batch
                .add(token, pos as i32, &[0], pos == last)
                .context("add prompt token to batch")?;
        }
        ctx.decode(&mut batch).context("decode prompt")?;

        let mut sampler = LlamaSampler::chain_simple([
            LlamaSampler::dist(0x5CA1A7CE),
            LlamaSampler::greedy(),
        ]);

        let mut content = String::new();
        let mut position = last as i32;
        let mut generated = 0u32;

        while generated < max_tokens as u32 {
            let token = sampler.sample(&ctx, batch.n_tokens() - 1);
            sampler.accept(token);

            if model.is_eog_token(token) {
                break;
            }

            let piece = decode_token(model, token)?;
            content.push_str(&piece);

            batch.clear();
            position += 1;
            batch
                .add(token, position, &[0], true)
                .context("add generated token to batch")?;
            ctx.decode(&mut batch).context("decode generated token")?;
            generated += 1;
        }

        Ok(GenerateOutput {
            content: sanitize_completion(&config.model_id, &content),
            prompt_tokens: prompt_token_count,
            completion_tokens: generated.max(1),
        })
    })
}

pub(crate) fn model_params_for_pool(pool: &VirtualCard) -> Result<LlamaModelParams> {
    let ggml_devices: Vec<usize> = pool
        .cuda_device_ids
        .iter()
        .map(|id| *id as usize)
        .collect();

    let mut model_params = LlamaModelParams::default();

    match pool.strategy {
        PoolStrategy::TensorParallel if ggml_devices.len() > 1 => {
            model_params = model_params
                .with_split_mode(LlamaSplitMode::Tensor)
                .with_devices(&ggml_devices)
                .context("configure multi-GPU tensor parallel devices")?
                .with_n_gpu_layers(999);
        }
        PoolStrategy::Single if !ggml_devices.is_empty() => {
            model_params = model_params
                .with_devices(std::slice::from_ref(&ggml_devices[0]))
                .context("configure primary GPU device")?
                .with_n_gpu_layers(999);
        }
        PoolStrategy::GpuWithCpuOffload => {
            if !ggml_devices.is_empty() {
                model_params = model_params
                    .with_devices(std::slice::from_ref(&ggml_devices[0]))
                    .context("configure GPU for CPU offload path")?
                    .with_use_mmap(true);
            }
            model_params = model_params.with_n_gpu_layers(pool.gpu_layer_budget);
        }
        PoolStrategy::CpuOnly => {
            model_params = model_params.with_n_gpu_layers(0);
        }
        _ => {
            // Vulkan / ROCm / fallback: compiled backends are selected automatically.
            model_params = model_params.with_n_gpu_layers(999);
        }
    }

    Ok(model_params)
}
