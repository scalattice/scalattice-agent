//! Embedded llama.cpp inference (no external llama-cli binary required).

use crate::compute_pool::{
    primary_cuda_device, primary_cuda_vram_gb, cuda_device_vram_gb, PoolStrategy, VirtualCard,
};
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
use tracing::{info, warn};

use super::ggml_devices::vulkan_ggml_device_indices;
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
        PoolStrategy::Vulkan => {
            model_params = vulkan_full_params()?.with_n_gpu_layers(999);
        }
        PoolStrategy::CpuOnly => {
            model_params = model_params.with_n_gpu_layers(0);
        }
        _ => {
            model_params = model_params.with_n_gpu_layers(999);
        }
    }

    Ok(model_params)
}

/// Full Vulkan placement: pin ggml Vulkan GPU/iGPU indices when the backend exposes them.
///
/// If enumeration is empty (no ICD / wrong build), still request GPU layers and let
/// llama.cpp choose — cascade falls to CPU on failure.
fn vulkan_full_params() -> Result<LlamaModelParams> {
    let indices = vulkan_ggml_device_indices();
    let mut params = LlamaModelParams::default().with_use_mmap(true);
    if !indices.is_empty() {
        info!(
            devices = ?indices,
            "Vulkan pool: pinning ggml backend device(s)"
        );
        params = params
            .with_devices(&indices)
            .context("configure Vulkan ggml devices")?;
    } else {
        warn!(
            "Vulkan pool: no ggml Vulkan devices enumerated yet; \
             loading with n_gpu_layers only (needs a Vulkan ICD on the host)"
        );
    }
    Ok(params)
}

fn vulkan_offload_params(layers: u32) -> Result<LlamaModelParams> {
    let indices = vulkan_ggml_device_indices();
    let mut params = LlamaModelParams::default()
        .with_use_mmap(true)
        .with_n_gpu_layers(layers);
    if let Some(primary) = indices.first().copied() {
        params = params
            .with_devices(std::slice::from_ref(&primary))
            .context("configure primary Vulkan device for offload")?;
    }
    Ok(params)
}

fn cuda_offload_params(device: usize, layers: u32) -> Result<LlamaModelParams> {
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
    let candidates = load_param_candidates(pool, Some(model_path))?;
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

/// Number of ordered load configurations for this pool / model.
pub(crate) fn load_candidate_count(pool: &VirtualCard, model_path: &Path) -> Result<usize> {
    Ok(load_param_candidates(pool, Some(model_path))?.len())
}

/// Label for the load candidate at `index`, if any.
pub(crate) fn load_candidate_label(
    pool: &VirtualCard,
    model_path: &Path,
    index: usize,
) -> Result<Option<&'static str>> {
    Ok(load_param_candidates(pool, Some(model_path))?
        .get(index)
        .map(|(label, _)| *label))
}

/// GGUF on-disk size in GiB (approx weights). Used to skip suicidal gpu-full attempts.
pub(crate) fn gguf_weight_gb(path: &Path) -> Option<f64> {
    std::fs::metadata(path)
        .ok()
        .map(|m| m.len() as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn full_placement_vram_gb(pool: &VirtualCard) -> u32 {
    match pool.strategy {
        // Single-GPU full fit uses the primary card only.
        PoolStrategy::Single => primary_cuda_vram_gb(pool).max(pool.total_vram_gb),
        // TP full fit uses pooled VRAM (only when should_attempt_tensor_parallel).
        PoolStrategy::TensorParallel => pool.total_vram_gb,
        PoolStrategy::Vulkan => pool.total_vram_gb,
        PoolStrategy::CpuOnly => 0,
    }
}

/// Tensor-parallel gpu-full is safe for *this* model when each GPU can hold its
/// weight slice plus local KV/compute overhead (derived from on-disk size — no
/// fixed VRAM tier cutoff).
pub(crate) fn should_attempt_tensor_parallel(
    pool: &VirtualCard,
    weight_gb: Option<f64>,
) -> bool {
    if pool.cuda_device_ids.len() < 2 {
        return false;
    }
    let Some(w) = weight_gb.filter(|w| *w > 0.05) else {
        // Unknown size → do not risk a CUDA abort() on TP.
        return false;
    };
    let vrams = cuda_device_vram_gb(pool);
    if vrams.len() != pool.cuda_device_ids.len() || vrams.is_empty() {
        return false;
    }
    let n = vrams.len() as f64;
    let min_v = f64::from(*vrams.iter().min().unwrap_or(&0));
    let total = f64::from(vrams.iter().copied().sum::<u32>());

    // llama.cpp TP is not a perfect weight/n split — reserve overhead per device
    // and for the pool so we never ask CUDA for an allocation that abort()s.
    const PER_GPU_OVERHEAD_GB: f64 = 2.0;
    const TOTAL_OVERHEAD_GB: f64 = 2.0;
    let per_need = w / n + PER_GPU_OVERHEAD_GB;
    let tot_need = w + TOTAL_OVERHEAD_GB;
    min_v + 0.05 >= per_need && total + 0.05 >= tot_need
}

/// Whether a greedy all-layers load on the *primary* GPU is safe to attempt.
pub(crate) fn should_attempt_single_gpu_full(
    pool: &VirtualCard,
    weight_gb: Option<f64>,
) -> bool {
    let available = f64::from(primary_cuda_vram_gb(pool).max(1));
    const HEADROOM_GB: f64 = 2.0;
    match weight_gb {
        Some(w) if w > 0.05 => available + 0.05 >= w + HEADROOM_GB,
        _ => false,
    }
}

/// Whether a greedy all-layers GPU load is safe to *attempt* for this pool/model.
///
/// llama.cpp's CUDA backend often `abort()`s on OOM instead of returning an error,
/// which kills the agent process and looks like a disconnect.
pub(crate) fn should_attempt_gpu_full(pool: &VirtualCard, weight_gb: Option<f64>) -> bool {
    match pool.strategy {
        PoolStrategy::TensorParallel => should_attempt_tensor_parallel(pool, weight_gb),
        PoolStrategy::Single => should_attempt_single_gpu_full(pool, weight_gb),
        PoolStrategy::Vulkan => {
            let available = f64::from(full_placement_vram_gb(pool));
            const HEADROOM_GB: f64 = 2.0;
            match weight_gb {
                Some(w) if w > 0.05 => available + 0.05 >= w + HEADROOM_GB,
                _ => false,
            }
        }
        PoolStrategy::CpuOnly => false,
    }
}

fn single_gpu_full_params(device: usize) -> Result<LlamaModelParams> {
    Ok(LlamaModelParams::default()
        .with_devices(std::slice::from_ref(&device))
        .context("configure primary GPU for single-device full load")?
        .with_use_mmap(true)
        .with_n_gpu_layers(999))
}

/// Ordered load configurations — CUDA and Vulkan share the same ladder.
///
/// For multi-GPU pools, TP gpu-full is only queued when the model fits per-GPU;
/// otherwise we try largest-GPU full, then offload — never a blind TP that abort()s.
fn load_param_candidates(
    pool: &VirtualCard,
    model_path: Option<&Path>,
) -> Result<Vec<(&'static str, LlamaModelParams)>> {
    load_param_candidates_with_weight(pool, model_path.and_then(gguf_weight_gb))
}

fn load_param_candidates_with_weight(
    pool: &VirtualCard,
    weight_gb: Option<f64>,
) -> Result<Vec<(&'static str, LlamaModelParams)>> {
    let mut candidates = Vec::new();

    if matches!(pool.strategy, PoolStrategy::CpuOnly) {
        candidates.push((
            "cpu-only",
            LlamaModelParams::default()
                .with_use_mmap(true)
                .with_n_gpu_layers(0),
        ));
        return Ok(candidates);
    }

    match pool.strategy {
        PoolStrategy::TensorParallel => {
            if should_attempt_tensor_parallel(pool, weight_gb) {
                candidates.push(("gpu-full", model_params_for_pool(pool)?));
            } else if should_attempt_single_gpu_full(pool, weight_gb) {
                if let Some(primary) = primary_cuda_device(pool) {
                    info!(
                        weight_gb = weight_gb.unwrap_or(-1.0),
                        primary_vram_gb = primary_cuda_vram_gb(pool),
                        gpus = pool.cuda_device_ids.len(),
                        "tensor-parallel unsafe for this model size; trying largest GPU full"
                    );
                    candidates.push(("gpu-full", single_gpu_full_params(primary as usize)?));
                }
            } else {
                info!(
                    weight_gb = weight_gb.unwrap_or(-1.0),
                    min_vram_gb = cuda_device_vram_gb(pool)
                        .into_iter()
                        .min()
                        .unwrap_or(0),
                    primary_vram_gb = primary_cuda_vram_gb(pool),
                    "skipping gpu-full (model does not fit GPUs safely); starting at offload"
                );
            }
        }
        PoolStrategy::Single | PoolStrategy::Vulkan => {
            if should_attempt_gpu_full(pool, weight_gb) {
                candidates.push(("gpu-full", model_params_for_pool(pool)?));
            } else {
                info!(
                    available_vram_gb = full_placement_vram_gb(pool),
                    weight_gb = weight_gb.unwrap_or(-1.0),
                    "skipping gpu-full placement (would not fit / risk CUDA abort); starting at offload"
                );
            }
        }
        PoolStrategy::CpuOnly => {}
    }

    let budget = pool.gpu_layer_budget.max(1);
    if budget < 999 {
        match pool.strategy {
            PoolStrategy::Vulkan => {
                candidates.push(("gpu-offload", vulkan_offload_params(budget)?));
                let reduced = (budget / 2).max(1);
                if reduced < budget {
                    candidates.push(("gpu-offload-reduced", vulkan_offload_params(reduced)?));
                }
            }
            PoolStrategy::Single | PoolStrategy::TensorParallel => {
                if let Some(primary) = primary_cuda_device(pool) {
                    let device = primary as usize;
                    candidates.push(("gpu-offload", cuda_offload_params(device, budget)?));
                    let reduced = (budget / 2).max(1);
                    if reduced < budget {
                        candidates.push((
                            "gpu-offload-reduced",
                            cuda_offload_params(device, reduced)?,
                        ));
                    }
                }
            }
            PoolStrategy::CpuOnly => {}
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
    use crate::compute_pool::{build_virtual_card, vulkan_runtime_supported};
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

    fn cascade_labels(pool: &VirtualCard, weight_gb: Option<f64>) -> Vec<&'static str> {
        load_param_candidates_with_weight(pool, weight_gb)
            .unwrap()
            .into_iter()
            .map(|(label, _)| label)
            .collect()
    }

    #[test]
    fn roomy_single_gpu_tries_full_then_offload_cascade() {
        for vram in [12u32, 24] {
            let pool = gpu_and_cpu(vram);
            assert_eq!(pool.strategy, PoolStrategy::Single);
            assert_eq!(
                cascade_labels(&pool, Some(4.0)),
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
    fn small_vram_skips_gpu_full_when_weights_do_not_fit() {
        let pool = gpu_and_cpu(4);
        assert_eq!(pool.strategy, PoolStrategy::Single);
        // ~5GB Q4 8B on 4GB card — must not attempt gpu-full (CUDA abort risk).
        assert_eq!(
            cascade_labels(&pool, Some(4.7)),
            vec!["gpu-offload", "gpu-offload-reduced", "cpu-only"]
        );
        assert!(!should_attempt_gpu_full(&pool, Some(4.7)));
    }

    #[test]
    fn eight_gb_fits_qwen8b_full() {
        let pool = gpu_and_cpu(8);
        assert!(should_attempt_gpu_full(&pool, Some(4.68)));
        assert_eq!(
            cascade_labels(&pool, Some(4.68)),
            vec![
                "gpu-full",
                "gpu-offload",
                "gpu-offload-reduced",
                "cpu-only",
            ]
        );
    }

    #[test]
    fn dual_small_gpus_skip_unsafe_full_for_large_model() {
        let pool = dual_gpu(4, 4);
        assert_eq!(pool.strategy, PoolStrategy::TensorParallel);
        assert_eq!(pool.cuda_device_ids, vec![0, 1]);
        // ~4.7GB weights: per-GPU need ≈ 2.35+2 > 4GB → no TP; single also too small.
        assert!(!should_attempt_tensor_parallel(&pool, Some(4.7)));
        assert!(!should_attempt_single_gpu_full(&pool, Some(4.7)));
        assert_eq!(
            cascade_labels(&pool, Some(4.7)),
            vec!["gpu-offload", "gpu-offload-reduced", "cpu-only"]
        );
    }

    #[test]
    fn dual_gpus_use_tp_when_model_fits_per_device() {
        let pool = dual_gpu(24, 24);
        assert!(should_attempt_tensor_parallel(&pool, Some(20.0)));
        assert_eq!(
            cascade_labels(&pool, Some(20.0)),
            vec![
                "gpu-full",
                "gpu-offload",
                "gpu-offload-reduced",
                "cpu-only",
            ]
        );
    }

    #[test]
    fn dual_gpus_fall_back_to_largest_full_when_tp_slice_too_big() {
        // 14GB model on 2×8GB: TP slice ≈7+2 > 8 → skip TP; primary 8 < 14+2 → offload.
        let pool = dual_gpu(8, 8);
        assert!(!should_attempt_tensor_parallel(&pool, Some(14.0)));
        assert!(!should_attempt_single_gpu_full(&pool, Some(14.0)));
        assert_eq!(
            cascade_labels(&pool, Some(14.0)),
            vec!["gpu-offload", "gpu-offload-reduced", "cpu-only"]
        );
    }

    #[test]
    fn roomy_multi_gpu_tries_tensor_parallel_full_first() {
        let pool = dual_gpu(24, 24);
        assert_eq!(pool.strategy, PoolStrategy::TensorParallel);
        assert_eq!(
            cascade_labels(&pool, Some(4.0)),
            vec![
                "gpu-full",
                "gpu-offload",
                "gpu-offload-reduced",
                "cpu-only",
            ]
        );
    }

    #[test]
    fn amd_vulkan_pool_uses_same_cascade_ladder() {
        if !vulkan_runtime_supported() {
            return;
        }
        let pool = build_virtual_card(&[
            ComputeDevice {
                id: "amd:0".into(),
                kind: "discrete".into(),
                name: "AMD Radeon RX 6800".into(),
                vram_gb: Some(16),
                vram_used_gb: None,
                util_pct: None,
                enabled: true,
            },
        ])
        .unwrap();
        assert_eq!(pool.strategy, PoolStrategy::Vulkan);
        assert_eq!(
            cascade_labels(&pool, Some(4.0)),
            vec![
                "gpu-full",
                "gpu-offload",
                "gpu-offload-reduced",
                "cpu-only",
            ]
        );
    }
}
