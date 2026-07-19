//! Embedded llama.cpp inference (no external llama-cli binary required).

use crate::compute_pool::{PoolStrategy, VirtualCard};
use crate::protocol::ChatMessage;
use anyhow::{anyhow, Context, Result};
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::LogOptions;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::model::params::{LlamaModelParams, LlamaSplitMode};
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;
use tracing::warn;

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

#[derive(Debug, Clone, Default)]
pub struct GenerateTimings {
    pub model_load_ms: u64,
    pub prefill_ms: u64,
    pub decode_ms: u64,
    pub total_ms: u64,
}

#[derive(Debug, Clone)]
pub struct GenerateOutput {
    pub content: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub timings: GenerateTimings,
}

pub fn init_backend() -> Result<()> {
    llama_cpp_2::send_logs_to_tracing(LogOptions::default());
    if BACKEND.get().is_some() {
        return backend().map(|_| ());
    }

    // CUDA driver mismatches can stall inside ggml_cuda_init. Bound the wait so the
    // agent can still connect and run CPU-compatible work (or report no compute).
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("llama-backend-init".into())
        .spawn(move || {
            let result = LlamaBackend::init().map_err(|err| err.to_string());
            let _ = tx.send(result);
        })
        .context("spawn llama.cpp backend init thread")?;

    match rx.recv_timeout(std::time::Duration::from_secs(12)) {
        Ok(result) => {
            let _ = BACKEND.set(result);
        }
        Err(_) => {
            warn!(
                "llama.cpp backend init timed out after 12s (often an outdated NVIDIA driver); continuing without GPU backend"
            );
            let _ = BACKEND.set(Err(
                "timed out initializing llama.cpp — update the NVIDIA driver or use CPU-only models"
                    .into(),
            ));
        }
    }
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
    generate_with_callback(config, |_| {})
}

pub fn generate_with_callback(
    config: &GenerateConfig,
    mut on_token: impl FnMut(&str),
) -> Result<GenerateOutput> {
    let total_start = Instant::now();
    let backend = backend()?;

    let (output, model_load_ms) =
        super::model_cache::with_loaded_model_timed(&config.model_path, &config.pool, |model| {
            let ctx_params = LlamaContextParams::default().with_n_ctx(Some(
                NonZeroU32::new(4096).context("invalid default context size")?,
            ));
            let prefill_start = Instant::now();
            let mut ctx = model
                .new_context(backend, ctx_params)
                .context("create llama context")?;

            let prompt = build_chat_prompt(model, &config.messages)?;
            let mut prompt_tokens = model
                .str_to_token(&prompt, AddBos::Never)
                .context("tokenize prompt")?;

            let max_tokens = config.max_tokens.max(1).min(8192) as usize;
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
            let mut prefill_ms = 0u64;
            let mut decode_ms = 0u64;
            let mut first_token = true;

            while generated < max_tokens as u32 {
                let token = sampler.sample(&ctx, batch.n_tokens() - 1);
                sampler.accept(token);

                if model.is_eog_token(token) {
                    break;
                }

                let piece = decode_token(model, token)?;
                if first_token {
                    prefill_ms = prefill_start.elapsed().as_millis() as u64;
                    first_token = false;
                }
                let decode_piece_start = Instant::now();
                content.push_str(&piece);
                on_token(&piece);

                batch.clear();
                position += 1;
                batch
                    .add(token, position, &[0], true)
                    .context("add generated token to batch")?;
                ctx.decode(&mut batch).context("decode generated token")?;
                decode_ms += decode_piece_start.elapsed().as_millis() as u64;
                generated += 1;
            }

            if first_token {
                prefill_ms = prefill_start.elapsed().as_millis() as u64;
            }

            Ok(GenerateOutput {
                content: sanitize_completion(&config.model_id, &content),
                prompt_tokens: prompt_token_count,
                completion_tokens: generated.max(1),
                timings: GenerateTimings {
                    model_load_ms: 0, // filled by caller
                    prefill_ms,
                    decode_ms,
                    total_ms: 0,
                },
            })
        })?;

    Ok(GenerateOutput {
        content: output.content,
        prompt_tokens: output.prompt_tokens,
        completion_tokens: output.completion_tokens,
        timings: GenerateTimings {
            model_load_ms,
            prefill_ms: output.timings.prefill_ms,
            decode_ms: output.timings.decode_ms,
            total_ms: total_start.elapsed().as_millis() as u64,
        },
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

/// Load the model for this pool, degrading gracefully instead of hard-failing.
///
/// Small / memory-constrained pools frequently OOM inside llama.cpp during load,
/// which surfaces as a null model pointer. Rather than fail the job, retry with
/// progressively less GPU offload and finally a CPU-only floor that loads whenever
/// system RAM allows. Detailed failures are logged locally only; the caller must
/// keep provider-specific detail (paths, device names) out of anything sent upstream.
///
/// Returns `(model, candidate_index)` so the cache can skip already-failed tiers
/// when context/KV allocation OOMs after a successful weight load.
pub(crate) fn load_model_for_pool(
    backend: &LlamaBackend,
    model_path: &Path,
    pool: &VirtualCard,
) -> Result<(LlamaModel, usize)> {
    load_model_for_pool_starting_at(backend, model_path, pool, 0)
}

/// Like [`load_model_for_pool`], but skips candidates before `start_at`.
///
/// Used after context create OOM: weights already loaded at a greedy tier, so the
/// next attempt must start at the following cascade step (not `configured` again).
pub(crate) fn load_model_for_pool_starting_at(
    backend: &LlamaBackend,
    model_path: &Path,
    pool: &VirtualCard,
    start_at: usize,
) -> Result<(LlamaModel, usize)> {
    let candidates = load_param_candidates(pool)?;
    let mut last_err: Option<anyhow::Error> = None;

    for (idx, (label, params)) in candidates.into_iter().enumerate().skip(start_at) {
        match LlamaModel::load_from_file(backend, model_path, &params) {
            Ok(model) => {
                if start_at == 0 && label != "configured" {
                    warn!("model loaded via '{label}' fallback after primary load failed");
                }
                return Ok((model, idx));
            }
            Err(err) => {
                warn!("model load attempt '{label}' failed: {err}");
                last_err = Some(anyhow!(err));
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("model failed to load")))
}

/// Number of ordered load configurations for this pool (configured → reduced → cpu).
pub(crate) fn load_candidate_count(pool: &VirtualCard) -> Result<usize> {
    Ok(load_param_candidates(pool)?.len())
}

/// Label for the load candidate at `index`, if any.
pub(crate) fn load_candidate_label(pool: &VirtualCard, index: usize) -> Result<Option<&'static str>> {
    Ok(load_param_candidates(pool)?
        .get(index)
        .map(|(label, _)| *label))
}

/// Ordered load configurations to try: the pool's configured strategy first,
/// then reduced GPU offload, then a CPU-only floor.
fn load_param_candidates(pool: &VirtualCard) -> Result<Vec<(&'static str, LlamaModelParams)>> {
    let mut candidates = Vec::new();
    candidates.push(("configured", model_params_for_pool(pool)?));

    if matches!(pool.strategy, PoolStrategy::GpuWithCpuOffload) && pool.gpu_layer_budget > 1 {
        if let Some(primary) = pool.cuda_device_ids.first() {
            let device = *primary as usize;
            let reduced = (pool.gpu_layer_budget / 2).max(1);
            let params = LlamaModelParams::default()
                .with_devices(std::slice::from_ref(&device))
                .context("configure GPU for reduced offload fallback")?
                .with_use_mmap(true)
                .with_n_gpu_layers(reduced);
            candidates.push(("gpu-offload-reduced", params));
        }
    }

    if !matches!(pool.strategy, PoolStrategy::CpuOnly) {
        candidates.push((
            "cpu-only",
            LlamaModelParams::default()
                .with_use_mmap(true)
                .with_n_gpu_layers(0),
        ));
    }

    Ok(candidates)
}
