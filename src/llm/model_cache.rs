//! In-process GGUF cache - avoids reloading multi-GB weights on every invoke.
//!
//! GPU cache keeps as many models as currently **fit**. Discrete cards use live
//! free VRAM minus occupancy. Metal counts occupancy **plus** each GGUF mmap
//! because those pages share unified RAM with the OS. Idle CUDA/Vulkan models
//! that do not fit are mmap'd on the RAM shelf (never on Metal).
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
use llama_cpp_2::mtmd::MtmdContext;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tracing::{info, warn};

use super::embedded::{
    backend, estimated_n_layer, gguf_weight_gb, load_candidate_label, load_cpu_mmap_model,
    load_model_for_pool, load_model_for_pool_starting_at, load_candidate_labels,
    offload_layers_for_available,
};
use super::vision::init_mtmd_for_model;

const GIB: u64 = 1024 * 1024 * 1024;

struct CachedModel {
    model: LlamaModel,
    mtmd: Option<MtmdContext>,
    /// Index into [`load_param_candidates`] that produced this resident.
    load_tier: usize,
    /// GPU memory this resident actually occupies (nvidia-smi delta, else tier estimate).
    occupancy_gb: f64,
    last_used: Instant,
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
    /// Last measured GPU occupancy by GGUF path (survives RAM-shelf eviction).
    last_gpu_occupancy: HashMap<String, f64>,
}

static CACHE: OnceLock<Mutex<CacheInner>> = OnceLock::new();

fn cache() -> &'static Mutex<CacheInner> {
    CACHE.get_or_init(|| {
        Mutex::new(CacheInner {
            gpu: HashMap::new(),
            ram: HashMap::new(),
            last_gpu_occupancy: HashMap::new(),
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
    crate::specs::system_ram_reserve_gb(ram_gb).round() as u32
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
        let need_gb = ((add_bytes + GIB - 1) / GIB) as u32;
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

fn drop_other_ram_shelves(inner: &mut CacheInner, keep_path: &Path) {
    let keep = path_key(keep_path);
    let dropped: Vec<String> = inner
        .ram
        .keys()
        .filter(|k| k.as_str() != keep)
        .cloned()
        .collect();
    for key in dropped {
        info!(
            evicted = %key,
            "dropping RAM-shelved model before loading a different GGUF"
        );
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

/// True when live/estimated free VRAM can take `need_gb` without evicting.
pub(crate) fn incoming_model_fits(free_gb: f64, need_gb: f64) -> bool {
    free_gb + 0.05 >= need_gb
}

fn incoming_vram_need_gb(inner: &CacheInner, model_path: &Path) -> f64 {
    let weight = gguf_weight_gb(model_path).unwrap_or(0.0);
    let pk = path_key(model_path);
    if let Some(&measured) = inner.last_gpu_occupancy.get(&pk) {
        if measured > 0.05 {
            return measured.max(weight);
        }
    }
    let shape = crate::models::gguf_shape(model_path);
    crate::models::full_host_need_gb(weight, shape, 4096)
}

fn estimated_gpu_free_gb(inner: &CacheInner, pool: &VirtualCard) -> f64 {
    let metal = matches!(pool.strategy, crate::compute_pool::PoolStrategy::Metal);
    let used: f64 = inner
        .gpu
        .iter()
        .map(|(k, e)| resident_accounted_gb(k, e.occupancy_gb, metal))
        .sum();
    (f64::from(pool.total_vram_gb) - used).max(0.0)
}

/// CUDA/Vulkan occupancy is dedicated VRAM. Metal occupancy lives in the same
/// RAM as the GGUF mmap, so each resident costs occupancy + on-disk weight.
fn resident_accounted_gb(key: &str, occupancy_gb: f64, metal: bool) -> f64 {
    let occ = occupancy_gb.max(0.0);
    if !metal {
        return occ;
    }
    occ + gguf_weight_gb(Path::new(path_from_gpu_key(key))).unwrap_or(0.0)
}

fn live_free_vram_gb(pool: &VirtualCard) -> Option<f64> {
    match pool.strategy {
        crate::compute_pool::PoolStrategy::Single
        | crate::compute_pool::PoolStrategy::TensorParallel => {
            crate::specs::live_cuda_free_vram_gb()
        }
        crate::compute_pool::PoolStrategy::Vulkan => {
            let index = pool.devices.iter().find_map(|device| {
                device
                    .id
                    .strip_prefix("amd:")
                    .and_then(|s| s.parse::<usize>().ok())
            });
            crate::specs::live_rocm_free_vram_gb(index)
        }
        crate::compute_pool::PoolStrategy::Metal => crate::specs::live_metal_free_vram_gb(),
        crate::compute_pool::PoolStrategy::CpuOnly => None,
    }
    .filter(|n| n.is_finite() && *n >= 0.0)
}

fn gpu_free_gb(inner: &CacheInner, pool: &VirtualCard) -> f64 {
    // Unified memory: "live free RAM" includes our own Metal buffers, so the
    // nvidia-smi-style probe is wrong. Subtract cache occupancy from advertised.
    if matches!(pool.strategy, crate::compute_pool::PoolStrategy::Metal) {
        return estimated_gpu_free_gb(inner, pool);
    }
    live_free_vram_gb(pool).unwrap_or_else(|| estimated_gpu_free_gb(inner, pool))
}

fn fallback_occupancy_gb(model_path: &Path, pool: &VirtualCard, load_tier: usize) -> f64 {
    let weight = gguf_weight_gb(model_path).unwrap_or(0.0);
    if weight <= 0.05 {
        return 0.0;
    }
    let shape = crate::models::gguf_shape(model_path);
    let full = crate::models::full_host_need_gb(weight, shape, 4096);
    match load_candidate_label(pool, model_path, load_tier)
        .ok()
        .flatten()
    {
        Some("cpu-only") => 0.0,
        Some(label) if label == "gpu-offload" || label == "gpu-offload-reduced" => {
            let n_layer = shape.filter(|s| s.usable()).map(|s| s.n_layer);
            let mut layers =
                offload_layers_for_available(pool.total_vram_gb, Some(weight), n_layer);
            if label == "gpu-offload-reduced" {
                layers = (layers / 2).max(1);
            }
            let denom = n_layer
                .filter(|n| *n > 0)
                .unwrap_or_else(|| estimated_n_layer(weight))
                .max(1);
            full * (f64::from(layers) / f64::from(denom)).clamp(0.0, 1.0)
        }
        _ => full,
    }
}

fn measure_occupancy_gb(before: Option<f64>, after: Option<f64>, fallback: f64) -> f64 {
    match (before, after) {
        (Some(b), Some(a)) if b.is_finite() && a.is_finite() && b > a + 0.05 => b - a,
        _ => fallback,
    }
}

fn make_gpu_room(
    inner: &mut CacheInner,
    backend: &llama_cpp_2::llama_backend::LlamaBackend,
    keep_key: &str,
    model_path: &Path,
    pool: &VirtualCard,
) {
    if let Some(entry) = inner.gpu.get_mut(keep_key) {
        entry.last_used = Instant::now();
        return;
    }

    drop_other_ram_shelves(inner, model_path);

    let metal = matches!(pool.strategy, crate::compute_pool::PoolStrategy::Metal);

    // CPU-pinned residents (0 GPU layers) sit in the GPU map with ~0 occupancy.
    // VRAM-fit checks skip them, so the next GGUF mmap'd beside them and OOMed
    // 16 GB boxes (WebSocket reset → five-minute invoke_timeout).
    let cpu_pinned: Vec<String> = inner
        .gpu
        .iter()
        .filter(|(k, e)| k.as_str() != keep_key && e.occupancy_gb <= 0.05)
        .map(|(k, _)| k.clone())
        .collect();
    for key in cpu_pinned {
        info!(
            evicted = %path_from_gpu_key(&key),
            "dropping CPU-resident weights before loading a different GGUF"
        );
        inner.gpu.remove(&key);
    }

    let need = incoming_vram_need_gb(inner, model_path);
    loop {
        let free = gpu_free_gb(inner, pool);
        if incoming_model_fits(free, need) {
            break;
        }
        let victim = inner
            .gpu
            .iter()
            .filter(|(k, e)| {
                k.as_str() != keep_key
                    && resident_accounted_gb(k, e.occupancy_gb, metal) > 0.05
            })
            .min_by_key(|(_, e)| e.last_used)
            .map(|(k, _)| k.clone());
        let Some(key) = victim else {
            break;
        };
        crate::llm::report_work_progress("evict", 0.0);
        if let Some(entry) = inner.gpu.remove(&key) {
            info!(
                evicted = %path_from_gpu_key(&key),
                occupancy_gb = format!("{:.2}", entry.occupancy_gb),
                free_gb = format!("{:.2}", free),
                need_gb = format!("{:.2}", need),
                "evicting GPU resident — incoming model does not fit beside it"
            );
            // Discrete VRAM: RAM-shelve when live RAM can hold the mmap.
            // Unified memory: shelving re-mmaps the same pages Metal just freed.
            if !metal {
                try_shelve_cpu(inner, backend, Path::new(path_from_gpu_key(&key)));
            }
        }
        crate::llm::report_work_progress("evict", 1.0);
    }

    // CUDA frees asynchronously. nvidia-smi still showed 17.5 GB free after
    // dropping a resident, so GLM 4.7 Flash skipped gpu-full on a 48 GB RTX 8000.
    if matches!(
        pool.strategy,
        crate::compute_pool::PoolStrategy::Single
            | crate::compute_pool::PoolStrategy::TensorParallel
    ) {
        wait_cuda_reclaim(pool, need);
    }
}

fn wait_cuda_reclaim(pool: &VirtualCard, need_gb: f64) {
    let Some(mut free) = live_free_vram_gb(pool) else {
        return;
    };
    if incoming_model_fits(free, need_gb) {
        return;
    }
    // cudaFree is asynchronous; wait in proportion to the GiB still outstanding.
    let gap = (need_gb - free).max(0.0);
    let budget = Duration::from_millis((80.0 + 50.0 * gap).clamp(80.0, 2_000.0) as u64);
    let start = Instant::now();
    while start.elapsed() < budget {
        std::thread::sleep(Duration::from_millis(40));
        free = live_free_vram_gb(pool).unwrap_or(0.0);
        if incoming_model_fits(free, need_gb) {
            return;
        }
    }
}

fn is_vram_pressure(err: &anyhow::Error) -> bool {
    let detail = format!("{err:#}").to_lowercase();
    detail.contains("out of memory")
        || detail.contains("cudamalloc")
        || detail.contains("failed to allocate")
        || detail.contains("create llama context")
        || detail.contains("null reference")
        || detail.contains("ggml_backend_cuda")
}

fn insert_loaded(
    inner: &mut CacheInner,
    key: String,
    model: LlamaModel,
    load_tier: usize,
    occupancy_gb: f64,
) {
    let pk = path_from_gpu_key(&key).to_string();
    if occupancy_gb > 0.05 {
        inner.last_gpu_occupancy.insert(pk, occupancy_gb);
    }
    inner.gpu.insert(
        key,
        CachedModel {
            model,
            mtmd: None,
            load_tier,
            occupancy_gb,
            last_used: Instant::now(),
        },
    );
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
    let before_free = live_free_vram_gb(pool);
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
    let after_free = live_free_vram_gb(pool);
    let occupancy_gb = measure_occupancy_gb(
        before_free,
        after_free,
        fallback_occupancy_gb(model_path, pool, load_tier),
    );
    insert_loaded(inner, key.to_string(), model, load_tier, occupancy_gb);
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
    with_loaded_weights(model_path, pool, false, |model, _mtmd| f(model))
}

/// Same cache lookup as [`with_loaded_model_timed`], optionally initializing mmproj.
pub fn with_loaded_weights<R>(
    model_path: &Path,
    pool: &VirtualCard,
    need_vision: bool,
    mut f: impl FnMut(&LlamaModel, Option<&MtmdContext>) -> Result<R>,
) -> Result<(R, u64)> {
    let backend = backend()?;
    let key = cache_key(model_path, pool);
    let mut guard = cache()
        .lock()
        .map_err(|_| anyhow::anyhow!("model cache lock poisoned"))?;

    make_gpu_room(&mut guard, backend, &key, model_path, pool);

    let mut model_load_ms = 0u64;
    if !guard.gpu.contains_key(&key) {
        let pk = path_key(model_path);
        guard.ram.remove(&pk);
        let (ms, _) = ensure_loaded(&mut guard, backend, model_path, pool, &key, 0)?;
        model_load_ms = ms;
    }

    let mut load_tier = guard
        .gpu
        .get(&key)
        .context("model missing immediately after cache insert")?
        .load_tier;
    let mut labels = load_candidate_labels(pool, model_path).unwrap_or_default();
    let mut oom_hops = 0u8;

    if need_vision {
        ensure_mtmd(&mut guard, model_path, pool, &key)?;
    }

    loop {
        let out = {
            let cached = guard
                .gpu
                .get(&key)
                .context("model missing immediately after cache insert")?;
            f(&cached.model, cached.mtmd.as_ref())
        };

        match out {
            Ok(result) => return Ok((result, model_load_ms)),
            Err(err) if is_vram_pressure(&err) => {
                oom_hops += 1;
                let failed = labels.get(load_tier).copied().unwrap_or("gpu-full");
                warn!(
                    failed_tier = failed,
                    hop = oom_hops,
                    error = %err,
                    "context OOM; dropping GPU residents and continuing cascade"
                );
                guard.gpu.clear();
                if oom_hops > 4 {
                    return Err(err);
                }
                let new_labels = load_candidate_labels(pool, model_path)?;
                let start = next_cascade_index(&new_labels, failed);
                if start >= new_labels.len() {
                    return Err(err);
                }
                let label = new_labels.get(start).copied().unwrap_or("next");
                warn!("context OOM; reloading via '{label}'");
                let (ms, new_tier) =
                    ensure_loaded(&mut guard, backend, model_path, pool, &key, start)?;
                model_load_ms = model_load_ms.saturating_add(ms);
                load_tier = new_tier;
                labels = new_labels;
                if need_vision {
                    ensure_mtmd(&mut guard, model_path, pool, &key)?;
                }
            }
            Err(err) => return Err(err),
        }
    }
}

fn ensure_mtmd(
    inner: &mut CacheInner,
    model_path: &Path,
    pool: &VirtualCard,
    key: &str,
) -> Result<()> {
    if inner
        .gpu
        .get(key)
        .map(|c| c.mtmd.is_some())
        .unwrap_or(false)
    {
        return Ok(());
    }
    let mtmd = {
        let cached = inner
            .gpu
            .get(key)
            .context("model missing while loading mmproj")?;
        init_mtmd_for_model(&cached.model, model_path, pool)?
    };
    if let Some(cached) = inner.gpu.get_mut(key) {
        cached.mtmd = Some(mtmd);
    }
    Ok(())
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

/// After a context OOM, pick the next cascade index on a *fresh* live-VRAM list.
/// Index 0 of the old list may have been `gpu-offload` while the new list starts
/// at `gpu-full` once residents are dropped.
pub(crate) fn next_cascade_index(new_labels: &[&str], failed: &str) -> usize {
    if failed == "gpu-full" {
        return new_labels
            .iter()
            .position(|l| *l != "gpu-full")
            .unwrap_or(new_labels.len());
    }
    if new_labels.first().copied() == Some("gpu-full") {
        return 0;
    }
    new_labels
        .iter()
        .position(|l| *l == failed)
        .map(|i| i + 1)
        .unwrap_or(new_labels.len())
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
    fn ram_shelf_uses_percent_reserve() {
        assert_eq!(ram_reserve_gb(16), 4);
        assert_eq!(ram_budget_bytes(16), 12 * GIB);
        assert_eq!(ram_reserve_gb(64), 8);
        assert_eq!(ram_budget_bytes(64), 56 * GIB);
    }

    #[test]
    fn metal_mmap_plus_buffers_evicts_35b_before_14b() {
        // 64 GB Mac advertised 56 GB. Ornith 35B occupancy + GGUF mmap cannot
        // sit beside Qwen3 14B (need includes KV/compute).
        let advertised = 56.0;
        let ornith35 = 26.0 + 23.7;
        assert!(!incoming_model_fits(advertised - ornith35, 12.0));
    }

    #[test]
    fn metal_two_small_text_models_still_co_reside() {
        let advertised = 56.0;
        let qwen8 = 7.0 + 4.7;
        assert!(incoming_model_fits(advertised - qwen8, 12.0));
    }

    #[test]
    fn evicts_when_free_vram_cannot_cover_incoming() {
        // 3080 with a parked 8B: ~1 GB free, incoming 8B needs ~6.7 GB.
        assert!(!incoming_model_fits(1.03, 6.7));
        // Same 8B on an empty 10 GB card.
        assert!(incoming_model_fits(10.0, 6.7));
        // 24 GB card already holding one 8B (~6.7 used → ~17 free).
        assert!(incoming_model_fits(17.3, 6.7));
        assert!(incoming_model_fits(6.7, 6.7));
    }

    #[test]
    fn cascade_retries_gpu_full_after_offload_oom_once_vram_recovers() {
        let recovered = ["gpu-full", "gpu-offload", "gpu-offload-reduced", "cpu-only"];
        assert_eq!(next_cascade_index(&recovered, "gpu-offload"), 0);
        assert_eq!(next_cascade_index(&recovered, "gpu-full"), 1);
    }

    #[test]
    fn cascade_advances_when_live_vram_still_tight() {
        let tight = ["gpu-offload", "gpu-offload-reduced", "cpu-only"];
        assert_eq!(next_cascade_index(&tight, "gpu-offload"), 1);
        assert_eq!(next_cascade_index(&tight, "gpu-offload-reduced"), 2);
        assert_eq!(next_cascade_index(&tight, "cpu-only"), 3);
    }
}
