//! Embedded llama.cpp inference (no external llama-cli binary required).

use crate::compute_pool::{primary_cuda_device, PoolStrategy, VirtualCard};
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
        PoolStrategy::CpuOnly => {
            model_params = model_params.with_n_gpu_layers(0);
        }
        _ => {
            // Vulkan / ROCm / single-device fallback: let compiled backends pick devices.
            model_params = model_params.with_n_gpu_layers(999);
        }
    }

    Ok(model_params)
}

fn offload_params(device: usize, layers: u32) -> Result<LlamaModelParams> {
    Ok(LlamaModelParams::default()
        .with_devices(std::slice::from_ref(&device))
        .context("configure GPU for CPU-offload fallback")?
        .with_use_mmap(true)
        .with_n_gpu_layers(layers))
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
/// next attempt must start at the following cascade step (not `gpu-full` again).
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
                if start_at == 0 && idx > 0 {
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

/// Number of ordered load configurations for this pool.
pub(crate) fn load_candidate_count(pool: &VirtualCard) -> Result<usize> {
    Ok(load_param_candidates(pool)?.len())
}

/// Label for the load candidate at `index`, if any.
pub(crate) fn load_candidate_label(pool: &VirtualCard, index: usize) -> Result<Option<&'static str>> {
    Ok(load_param_candidates(pool)?
        .get(index)
        .map(|(label, _)| *label))
}

/// Ordered load configurations — same ladder for every CUDA size.
///
/// 1. `gpu-full` — all enabled GPUs as one pool (single device or tensor-parallel)
/// 2. `gpu-offload` — largest GPU + CPU RAM for leftover layers (budget from total VRAM)
/// 3. `gpu-offload-reduced` — half that budget
/// 4. `cpu-only` — last resort
///
/// No VRAM-size special cases: tiny cards try full first and fall through quickly on OOM;
/// large cards usually stick the landing on step 1.
fn load_param_candidates(pool: &VirtualCard) -> Result<Vec<(&'static str, LlamaModelParams)>> {
    let mut candidates = Vec::new();

    if matches!(pool.strategy, PoolStrategy::CpuOnly) || pool.cuda_device_ids.is_empty() {
        candidates.push((
            "cpu-only",
            LlamaModelParams::default()
                .with_use_mmap(true)
                .with_n_gpu_layers(0),
        ));
        return Ok(candidates);
    }

    candidates.push(("gpu-full", model_params_for_pool(pool)?));

    if let Some(primary) = primary_cuda_device(pool) {
        let device = primary as usize;
        let budget = pool.gpu_layer_budget.max(1);
        // Skip redundant tiers when budget is already "everything".
        if budget < 999 {
            candidates.push(("gpu-offload", offload_params(device, budget)?));
            let reduced = (budget / 2).max(1);
            if reduced < budget {
                candidates.push(("gpu-offload-reduced", offload_params(device, reduced)?));
            }
        }
    }

    candidates.push((
        "cpu-only",
        LlamaModelParams::default()
            .with_use_mmap(true)
            .with_n_gpu_layers(0),
    ));

    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute_pool::build_virtual_card;
    use crate::specs::ComputeDevice;

    fn gpu_and_cpu(vram_gb: u32) -> VirtualCard {
        build_virtual_card(&[
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "Test GPU".into(),
                vram_gb: Some(vram_gb),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "cpu:0".into(),
                kind: "cpu".into(),
                name: "CPU".into(),
                vram_gb: None,
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap()
    }

    fn dual_gpu(a_gb: u32, b_gb: u32) -> VirtualCard {
        build_virtual_card(&[
            ComputeDevice {
                id: "nvidia:0".into(),
                kind: "discrete".into(),
                name: "GPU A".into(),
                vram_gb: Some(a_gb),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
            ComputeDevice {
                id: "nvidia:1".into(),
                kind: "discrete".into(),
                name: "GPU B".into(),
                vram_gb: Some(b_gb),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap()
    }

    fn cascade_labels(pool: &VirtualCard) -> Vec<&'static str> {
        (0..load_candidate_count(pool).unwrap())
            .map(|i| load_candidate_label(pool, i).unwrap().unwrap())
            .collect()
    }

    #[test]
    fn any_single_gpu_tries_full_then_offload_cascade() {
        for vram in [4u32, 8, 24] {
            let pool = gpu_and_cpu(vram);
            assert_eq!(pool.strategy, PoolStrategy::Single);
            assert_eq!(
                cascade_labels(&pool),
                vec![
                    "gpu-full",
                    "gpu-offload",
                    "gpu-offload-reduced",
                    "cpu-only",
                ]
            );
        }
    }

    #[test]
    fn multi_gpu_tries_tensor_parallel_full_first() {
        let pool = dual_gpu(4, 4);
        assert_eq!(pool.strategy, PoolStrategy::TensorParallel);
        assert_eq!(
            cascade_labels(&pool),
            vec![
                "gpu-full",
                "gpu-offload",
                "gpu-offload-reduced",
                "cpu-only",
            ]
        );
    }
}
