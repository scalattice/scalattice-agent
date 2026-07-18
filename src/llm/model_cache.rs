//! In-process GGUF cache - avoids reloading multi-GB weights on every invoke.

use crate::compute_pool::VirtualCard;
use anyhow::{Context, Result};
use llama_cpp_2::model::LlamaModel;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use super::embedded::{backend, load_model_for_pool};

static CACHE: OnceLock<Mutex<HashMap<String, LlamaModel>>> = OnceLock::new();

fn cache_key(model_path: &Path, pool: &VirtualCard) -> String {
    format!(
        "{}|{:?}|{:?}|{}",
        model_path.display(),
        pool.strategy,
        pool.cuda_device_ids,
        pool.gpu_layer_budget
    )
}

fn cache() -> &'static Mutex<HashMap<String, LlamaModel>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn preload_model(model_path: &Path, pool: &VirtualCard) -> Result<()> {
    with_loaded_model(model_path, pool, |_| Ok(()))
}

pub fn with_loaded_model<R>(
    model_path: &Path,
    pool: &VirtualCard,
    f: impl FnOnce(&LlamaModel) -> Result<R>,
) -> Result<R> {
    let (out, _) = with_loaded_model_timed(model_path, pool, f)?;
    Ok(out)
}

/// Like [`with_loaded_model`], but returns model-load wall time in ms (0 on cache hit).
pub fn with_loaded_model_timed<R>(
    model_path: &Path,
    pool: &VirtualCard,
    f: impl FnOnce(&LlamaModel) -> Result<R>,
) -> Result<(R, u64)> {
    let backend = backend()?;
    let key = cache_key(model_path, pool);
    let mut guard = cache()
        .lock()
        .map_err(|_| anyhow::anyhow!("model cache lock poisoned"))?;

    let mut model_load_ms = 0u64;
    if !guard.contains_key(&key) {
        let load_start = Instant::now();
        let model = load_model_for_pool(backend, model_path, pool)
            .with_context(|| format!("load model {}", model_path.display()))?;
        model_load_ms = load_start.elapsed().as_millis() as u64;
        guard.insert(key.clone(), model);
    }

    let model = guard
        .get(&key)
        .context("model missing immediately after cache insert")?;
    let out = f(model)?;
    Ok((out, model_load_ms))
}

pub fn evict_model(model_path: &Path, pool: &VirtualCard) {
    let key = cache_key(model_path, pool);
    if let Ok(mut guard) = cache().lock() {
        guard.remove(&key);
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
