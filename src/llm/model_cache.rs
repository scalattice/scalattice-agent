//! In-process GGUF cache - avoids reloading multi-GB weights on every invoke.
//!
//! Small GPUs (≤8 GB) only keep one model resident. Warming or switching without
//! eviction was causing cudaMalloc OOM on context create when a prior 7B stayed in VRAM.
//!
//! Every CUDA / Vulkan pool tries the safest placement first. If weight load or
//! context/KV alloc OOMs *and returns an error*, we walk:
//!   [gpu-full if it fits] → gpu-offload → gpu-offload-reduced → cpu-only
//!
//! `gpu-full` is skipped when on-disk weights + headroom exceed available VRAM —
//! llama.cpp CUDA often abort()s on OOM (kills the agent) instead of returning Err.

use crate::compute_pool::VirtualCard;
use anyhow::{Context, Result};
use llama_cpp_2::model::LlamaModel;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use tracing::{info, warn};

use super::embedded::{
    backend, load_candidate_count, load_candidate_label, load_model_for_pool,
    load_model_for_pool_starting_at,
};

struct CachedModel {
    model: LlamaModel,
    /// Index into [`load_param_candidates`] that produced this resident.
    load_tier: usize,
}

static CACHE: OnceLock<Mutex<HashMap<String, CachedModel>>> = OnceLock::new();

fn cache_key(model_path: &Path, pool: &VirtualCard) -> String {
    format!(
        "{}|{:?}|{:?}|{}",
        model_path.display(),
        pool.strategy,
        pool.cuda_device_ids,
        pool.gpu_layer_budget
    )
}

fn cache() -> &'static Mutex<HashMap<String, CachedModel>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn max_resident_models(pool: &VirtualCard) -> usize {
    // Q4_K_M 7B/8B weights alone are ~4–5 GB; two residents will OOM on context alloc.
    if pool.total_vram_gb <= 8 {
        1
    } else if pool.total_vram_gb <= 16 {
        2
    } else {
        4
    }
}

fn make_room(guard: &mut HashMap<String, CachedModel>, keep_key: &str, pool: &VirtualCard) {
    let max = max_resident_models(pool);
    if max <= 1 {
        let before = guard.len();
        guard.retain(|k, _| k == keep_key);
        if before > guard.len() {
            info!("evicted cached model(s) so only one stays resident on ≤8GB VRAM");
        }
        return;
    }
    let victims: Vec<String> = guard
        .keys()
        .filter(|k| k.as_str() != keep_key)
        .cloned()
        .collect();
    for key in victims {
        if guard.len() < max {
            break;
        }
        // If keep is absent, leave (max-1) others so the upcoming insert fits.
        let room_for_keep = usize::from(!guard.contains_key(keep_key));
        if guard.len() + room_for_keep <= max {
            break;
        }
        info!(
            evicted = %key.split('|').next().unwrap_or(key.as_str()),
            "evicting cached model to free VRAM"
        );
        guard.remove(&key);
    }
}

fn is_vram_pressure(err: &anyhow::Error) -> bool {
    let detail = format!("{err:#}").to_lowercase();
    detail.contains("out of memory")
        || detail.contains("cudamalloc")
        || detail.contains("failed to allocate")
        || detail.contains("create llama context")
        || detail.contains("ggml_backend_cuda")
}

fn insert_loaded(
    guard: &mut HashMap<String, CachedModel>,
    key: String,
    model: LlamaModel,
    load_tier: usize,
) {
    guard.insert(key, CachedModel { model, load_tier });
}

fn ensure_loaded(
    guard: &mut HashMap<String, CachedModel>,
    backend: &llama_cpp_2::llama_backend::LlamaBackend,
    model_path: &Path,
    pool: &VirtualCard,
    key: &str,
    start_at: usize,
) -> Result<(u64, usize)> {
    let load_start = Instant::now();
    let (model, load_tier) = if start_at == 0 {
        match load_model_for_pool(backend, model_path, pool) {
            Ok(loaded) => loaded,
            Err(err) if is_vram_pressure(&err) => {
                info!("model load hit VRAM pressure; clearing cache and retrying");
                guard.clear();
                load_model_for_pool(backend, model_path, pool).with_context(|| {
                    format!("load model {} after VRAM eviction", model_path.display())
                })?
            }
            Err(err) => {
                return Err(err).with_context(|| format!("load model {}", model_path.display()));
            }
        }
    } else {
        load_model_for_pool_starting_at(backend, model_path, pool, start_at).with_context(|| {
            format!(
                "load model {} starting at cascade tier {start_at}",
                model_path.display()
            )
        })?
    };
    let model_load_ms = load_start.elapsed().as_millis() as u64;
    insert_loaded(guard, key.to_string(), model, load_tier);
    Ok((model_load_ms, load_tier))
}

pub fn preload_model(model_path: &Path, pool: &VirtualCard) -> Result<()> {
    with_loaded_model(model_path, pool, |_| Ok(()))
}

pub fn with_loaded_model<R>(
    model_path: &Path,
    pool: &VirtualCard,
    f: impl FnMut(&LlamaModel) -> Result<R>,
) -> Result<R> {
    let (out, _) = with_loaded_model_timed(model_path, pool, f)?;
    Ok(out)
}

/// Like [`with_loaded_model`], but returns model-load wall time in ms (0 on cache hit).
///
/// If the callback fails with VRAM pressure (typical: KV/context alloc after a greedy
/// weight load), evicts the resident and reloads from the next cascade tier, retrying
/// the callback until a tier succeeds or the cascade is exhausted.
pub fn with_loaded_model_timed<R>(
    model_path: &Path,
    pool: &VirtualCard,
    mut f: impl FnMut(&LlamaModel) -> Result<R>,
) -> Result<(R, u64)> {
    let backend = backend()?;
    let key = cache_key(model_path, pool);
    let mut guard = cache()
        .lock()
        .map_err(|_| anyhow::anyhow!("model cache lock poisoned"))?;

    make_room(&mut guard, &key, pool);

    let mut model_load_ms = 0u64;
    if !guard.contains_key(&key) {
        let (ms, _) = ensure_loaded(&mut guard, backend, model_path, pool, &key, 0)?;
        model_load_ms = ms;
    }

    let mut load_tier = guard
        .get(&key)
        .context("model missing immediately after cache insert")?
        .load_tier;

    loop {
        let out = {
            let model = &guard
                .get(&key)
                .context("model missing immediately after cache insert")?
                .model;
            f(model)
        };

        match out {
            Ok(result) => return Ok((result, model_load_ms)),
            Err(err) if is_vram_pressure(&err) => {
                let next_tier = load_tier + 1;
                let tier_count = load_candidate_count(pool, model_path)?;
                if next_tier >= tier_count {
                    return Err(err);
                }
                let label = load_candidate_label(pool, model_path, next_tier)?.unwrap_or("next");
                warn!("context OOM; reloading via '{label}'");
                guard.clear();
                let (ms, new_tier) =
                    ensure_loaded(&mut guard, backend, model_path, pool, &key, next_tier)?;
                model_load_ms = model_load_ms.saturating_add(ms);
                load_tier = new_tier;
            }
            Err(err) => return Err(err),
        }
    }
}

pub fn evict_all() {
    if let Ok(mut guard) = cache().lock() {
        guard.clear();
    }
}

pub fn evict_all_for_path(model_path: &Path) {
    let prefix = model_path.display().to_string();
    if let Ok(mut guard) = cache().lock() {
        guard.retain(|key, _| !key.starts_with(&prefix));
    }
}

#[allow(dead_code)]
pub fn cached_model_paths() -> Vec<PathBuf> {
    let Ok(guard) = cache().lock() else {
        return Vec::new();
    };
    guard
        .keys()
        .filter_map(|key| key.split('|').next().map(PathBuf::from))
        .collect()
}
