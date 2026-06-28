//! Tier 2.5 split inference: lower segment saves KV state, upper segment continues generation.

use crate::compute_pool::VirtualCard;
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;
use std::num::NonZeroU32;
use std::path::PathBuf;

use super::embedded::{backend, decode_token, model_params_for_pool, GenerateOutput};

#[derive(Debug, Clone)]
pub struct SplitLowerConfig {
    pub model_path: PathBuf,
    pub pool: VirtualCard,
    pub prompt_token_ids: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct SplitLowerOutput {
    pub state_b64: String,
    pub prompt_tokens: u32,
}

#[derive(Debug, Clone)]
pub struct SplitUpperConfig {
    pub model_path: PathBuf,
    pub pool: VirtualCard,
    pub state_b64: String,
    pub max_tokens: u32,
}

pub fn split_lower(config: &SplitLowerConfig) -> Result<SplitLowerOutput> {
    let backend = backend()?;
    let model_params = model_params_for_pool(&config.pool)?;
    let ctx_params = LlamaContextParams::default().with_n_ctx(Some(
        NonZeroU32::new(4096).context("invalid default context size")?,
    ));

    let model = LlamaModel::load_from_file(backend, &config.model_path, &model_params)
        .with_context(|| format!("load model {}", config.model_path.display()))?;
    let mut ctx = model
        .new_context(backend, ctx_params)
        .context("create llama context")?;

    let prompt_tokens: Vec<LlamaToken> = config
        .prompt_token_ids
        .iter()
        .map(|id| LlamaToken(*id as i32))
        .collect();
    if prompt_tokens.is_empty() {
        anyhow::bail!("split lower segment requires prompt token ids");
    }

    let mut batch = LlamaBatch::new(prompt_tokens.len().max(1), 1);
    let last = prompt_tokens.len().saturating_sub(1);
    for (pos, token) in prompt_tokens.iter().enumerate() {
        batch
            .add(*token, pos as i32, &[0], pos == last)
            .context("add prompt token to batch")?;
    }
    ctx.decode(&mut batch).context("decode prompt for split lower")?;

    let state_path = std::env::temp_dir().join(format!(
        "scalattice-split-lower-{}.bin",
        std::process::id()
    ));
    ctx.state_save_file(&state_path, &prompt_tokens)
        .with_context(|| format!("save split state to {}", state_path.display()))?;
    let state_bytes = std::fs::read(&state_path).with_context(|| {
        format!(
            "read split state file {}",
            state_path.display()
        )
    })?;
    let _ = std::fs::remove_file(&state_path);

    Ok(SplitLowerOutput {
        state_b64: STANDARD.encode(state_bytes),
        prompt_tokens: prompt_tokens.len() as u32,
    })
}

pub fn split_upper(config: &SplitUpperConfig) -> Result<GenerateOutput> {
    let backend = backend()?;
    let model_params = model_params_for_pool(&config.pool)?;
    let ctx_params = LlamaContextParams::default().with_n_ctx(Some(
        NonZeroU32::new(4096).context("invalid default context size")?,
    ));

    let model = LlamaModel::load_from_file(backend, &config.model_path, &model_params)
        .with_context(|| format!("load model {}", config.model_path.display()))?;
    let mut ctx = model
        .new_context(backend, ctx_params)
        .context("create llama context")?;

    let state_bytes = STANDARD
        .decode(config.state_b64.trim())
        .context("decode split state blob")?;
    let state_path = std::env::temp_dir().join(format!(
        "scalattice-split-upper-{}.bin",
        std::process::id()
    ));
    std::fs::write(&state_path, state_bytes)
        .with_context(|| format!("write split state {}", state_path.display()))?;

    let max_ctx = ctx.n_ctx() as usize;
    let restored = ctx
        .state_load_file(&state_path, max_ctx)
        .with_context(|| format!("load split state from {}", state_path.display()))?;
    let _ = std::fs::remove_file(&state_path);

    let prompt_token_count = restored.len() as u32;
    if restored.is_empty() {
        anyhow::bail!("split upper segment received empty restored context");
    }

    let max_tokens = config.max_tokens.max(1).min(2048);
    let last = *restored.last().context("restored context missing last token")?;
    // After state_load, KV ends at position (len - 1). Replay must start at len (= X + 1).
    let mut position = restored.len() as i32;

    let mut batch = LlamaBatch::new(1, 1);
    batch
        .add(last, position, &[0], true)
        .context("replay last token for split upper")?;
    ctx.decode(&mut batch).context("replay decode for split upper")?;

    let mut sampler = LlamaSampler::chain_simple([
        LlamaSampler::dist(0x5CA1A7CE),
        LlamaSampler::greedy(),
    ]);

    let mut content = String::new();
    let mut generated = 0u32;

    while generated < max_tokens {
        let token = sampler.sample(&ctx, batch.n_tokens().saturating_sub(1).max(0));
        sampler.accept(token);

        if model.is_eog_token(token) {
            break;
        }

        let piece = decode_token(&model, token)?;
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
        content: content.trim().to_string(),
        prompt_tokens: prompt_token_count,
        completion_tokens: generated.max(1),
    })
}
