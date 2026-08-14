//! In-process GGUF cache - avoids reloading multi-GB weights on every invoke.
//!
//! Small GPUs (≤8 GB) only keep one model **on the GPU**. Idle models are demoted to a
//! CPU mmap shelf (no CUDA layers) so switching back skips a cold disk read. llama.cpp
//! cannot migrate layers in place; the next invoke still uploads to VRAM, but the GGUF
//! stays mapped in RAM when the machine has room.
//!
//! Every CUDA / Vulkan pool tries the safest placement first. If weight load or
//! context/KV alloc OOMs *and returns an error*, we walk:
//!   [gpu-full if it fits] → gpu-offload → gpu-offload-reduced → cpu-only
//!
//! `gpu-full` is skipped when on-disk weights + headroom exceed available VRAM —
//! llama.cpp CUDA often abort()s on OOM (kills the agent) instead of returning Err.

use crate::compute_pool::VirtualCard;
use crate::specs::{detect_ram_gb, detect_ram_used_gb};
use anyhow::{Context, Result};
use llama_cpp_2::model::LlamaModel;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use tracing::{info, warn};

use super::embedded::{
    backend, load_candidate_count, load_candidate_label, load_cpu_mmap_model, load_model_for_pool,
    load_model_for_pool_starting_at,
};

const GIB: u64 = 1024 * 1024 * 1024;
/// Do not keep a RAM duplicate of a large offload model that is also on the GPU.
const DROP_SHELF_WHEN_PROMOTING_BYTES: u64 = 3 * GIB;

struct CachedModel {
    model: LlamaModel,
    /// Index into [`load_param_candidates`] that produced this resident.
    load_tier: usize,
}

struct RamShelfEntry {
    /// Held so llama.cpp keeps the GGUF mmap'd; not read until the model returns to GPU.
    #[allow(dead_code)]
    model: LlamaModel,
    bytes: u64,
    last_used: Instant,
}

struct CacheInner {
    gpu: HashMap<String, CachedModel>,
    ram: HashMap<String, RamShelfEntry>,
}

static CACHE: OnceLock<Mutex<CacheInner>> = OnceLock::new();

fn cache() -> &'static Mutex<CacheInner> {
    CACHE.get_or_init(|| {
        Mutex::new(CacheInner {
            gpu: HashMap::new(),
            ram: HashMap::new(),
        })
    })
}

fn cache_key(model_path: &Path, pool: &VirtualCard) -> String {
    format!(
        "{}|{:?}|{:?}|{}",
        model_path.display(),
        pool.strategy,
        pool.cuda_device_ids,
        pool.gpu_layer_budget
    )
}

fn path_key(model_path: &Path) -> String {
    model_path.display().to_string()
}

fn path_from_gpu_key(key: &str) -> &str {
    key.split('|').next().unwrap_or(key)
}

fn file_bytes(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn ram_reserve_gb(ram_gb: u32) -> u32 {
    match ram_gb {
        0..=8 => (ram_gb / 2).max(2),
        9..=16 => 8,
        _ => 10,
    }
}

fn ram_budget_bytes(ram_gb: u32) -> u64 {
    u64::from(ram_gb.saturating_sub(ram_reserve_gb(ram_gb))) * GIB
}

fn ram_used_bytes(inner: &CacheInner) -> u64 {
    inner.ram.values().map(|e| e.bytes).sum()
}

fn can_shelve(inner: &CacheInner, add_bytes: u64) -> bool {
    if add_bytes == 0 {
        return false;
    }
    let ram_gb = detect_ram_gb().unwrap_or(16);
    let budget = ram_budget_bytes(ram_gb);
    if ram_used_bytes(inner).saturating_add(add_bytes) > budget {
        return false;
    }
    if let (Some(total), Some(used)) = (detect_ram_gb(), detect_ram_used_gb()) {
        let avail_gb = total.saturating_sub(used);
        let need_gb = ((add_bytes + GIB - 1) / GIB) as u32 + 2;
        if avail_gb < need_gb {
            return false;
        }
    }
    true
}

fn trim_ram_lru(inner: &mut CacheInner, extra_bytes: u64) {
    let ram_gb = detect_ram_gb().unwrap_or(16);
    let budget = ram_budget_bytes(ram_gb);
    while ram_used_bytes(inner).saturating_add(extra_bytes) > budget && !inner.ram.is_empty() {
        let victim = inner
            .ram
            .iter()
            .min_by_key(|(_, e)| e.last_used)
            .map(|(k, _)| k.clone());
        let Some(key) = victim else { break };
        info!(evicted = %key, "evicting RAM-shelved model to stay under RAM budget");
        inner.ram.remove(&key);
    }
}

fn try_shelve_cpu(inner: &mut CacheInner, backend: &llama_cpp_2::llama_backend::LlamaBackend, path: &Path) {
    let key = path_key(path);
    if inner.ram.contains_key(&key) {
        if let Some(entry) = inner.ram.get_mut(&key) {
            entry.last_used = Instant::now();
        }
        return;
    }
    let bytes = file_bytes(path);
    trim_ram_lru(inner, bytes);
    if !can_shelve(inner, bytes) {
        info!(
            path = %path.display(),
            weight_gb = bytes as f64 / GIB as f64,
            "not enough RAM to keep unloaded model mapped"
        );
        return;
    }
    match load_cpu_mmap_model(backend, path) {
        Ok(model) => {
            inner.ram.insert(
                key,
                RamShelfEntry {
                    model,
                    bytes,
                    last_used: Instant::now(),
                },
            );
            info!(
                path = %path.display(),
                weight_gb = format!("{:.1}", bytes as f64 / GIB as f64),
                shelved = inner.ram.len(),
                shelf_gb = format!("{:.1}", ram_used_bytes(inner) as f64 / GIB as f64),
                "kept model mmap'd in RAM for faster GPU reload"
            );
        }
        Err(err) => warn!(
            path = %path.display(),
            error = %err,
            "failed to shelve model in RAM"
        ),
    }
}

/// How many models may occupy VRAM (not the RAM shelf).
fn max_gpu_residents(pool: &VirtualCard) -> usize {
    // Q4_K_M 7B/8B weights alone are ~4–5 GB; two GPU residents will OOM on context alloc.
    if pool.total_vram_gb <= 8 {
        1
    } else if pool.total_vram_gb <= 16 {
        2
    } else {
        4
    }
}

fn make_gpu_room(
    inner: &mut CacheInner,
    backend: &llama_cpp_2::llama_backend::LlamaBackend,
    keep_key: &str,
    pool: &VirtualCard,
) {
    let max = max_gpu_residents(pool);
    let mut victims: Vec<String> = inner
        .gpu
        .keys()
        .filter(|k| k.as_str() != keep_key)
        .cloned()
        .collect();
    if max <= 1 {
        for key in victims {
            if inner.gpu.remove(&key).is_some() {
                info!("evicted GPU resident so only one model stays in VRAM on ≤8GB");
                try_shelve_cpu(inner, backend, Path::new(path_from_gpu_key(&key)));
            }
        }
        return;
    }
    while inner.gpu.len() + usize::from(!inner.gpu.contains_key(keep_key)) > max {
        let Some(key) = victims.pop() else { break };
        if key == keep_key {
            continue;
        }
        if inner.gpu.remove(&key).is_some() {
            info!(
                evicted = %path_from_gpu_key(&key),
                "evicting GPU resident to free VRAM"
            );
            try_shelve_cpu(inner, backend, Path::new(path_from_gpu_key(&key)));
        }
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

fn insert_loaded(inner: &mut CacheInner, key: String, model: LlamaModel, load_tier: usize) {
    inner.gpu.insert(key, CachedModel { model, load_tier });
}

fn ensure_loaded(
    inner: &mut CacheInner,
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
                info!("model load hit VRAM pressure; clearing GPU cache and retrying");
                inner.gpu.clear();
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
    insert_loaded(inner, key.to_string(), model, load_tier);
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
/// weight load), evicts the GPU resident and reloads from the next cascade tier, retrying
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

    make_gpu_room(&mut guard, backend, &key, pool);

    let mut model_load_ms = 0u64;
    if !guard.gpu.contains_key(&key) {
        let pk = path_key(model_path);
        if file_bytes(model_path) >= DROP_SHELF_WHEN_PROMOTING_BYTES {
            guard.ram.remove(&pk);
        }
        let (ms, _) = ensure_loaded(&mut guard, backend, model_path, pool, &key, 0)?;
        model_load_ms = ms;
        if let Some(entry) = guard.ram.get_mut(&pk) {
            entry.last_used = Instant::now();
        }
    }

    let mut load_tier = guard
        .gpu
        .get(&key)
        .context("model missing immediately after cache insert")?
        .load_tier;

    loop {
        let out = {
            let model = &guard
                .gpu
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
                guard.gpu.clear();
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
        guard.gpu.clear();
        guard.ram.clear();
    }
}

pub fn evict_all_for_path(model_path: &Path) {
    let prefix = model_path.display().to_string();
    if let Ok(mut guard) = cache().lock() {
        guard.gpu.retain(|key, _| !key.starts_with(&prefix));
        guard.ram.remove(&prefix);
    }
}

#[allow(dead_code)]
pub fn cached_model_paths() -> Vec<PathBuf> {
    let Ok(guard) = cache().lock() else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = guard
        .gpu
        .keys()
        .filter_map(|key| key.split('|').next().map(PathBuf::from))
        .collect();
    paths.extend(guard.ram.keys().map(PathBuf::from));
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_machines_reserve_half_ram() {
        assert_eq!(ram_reserve_gb(8), 4);
        assert_eq!(ram_budget_bytes(8), 4 * GIB);
    }

    #[test]
    fn typical_laptop_keeps_8gb_free() {
        assert_eq!(ram_reserve_gb(16), 8);
        assert_eq!(ram_budget_bytes(16), 8 * GIB);
    }
}
