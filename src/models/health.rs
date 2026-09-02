//! Weight-file health markers and load-failure classification.
//!
//! "complete on disk" previously meant only that a non-empty GGUF existed. Corrupt or
//! truncated files still looked ready and were preloaded forever. Health markers close that gap.

use crate::models::storage::{
    model_cache_dir, models_dir, read_manifest_filenames, resolve_model_gguf, runtime_cache_key,
    target_gguf_path,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const HEALTH_FILE: &str = "health.json";
const PURGING_MARKER: &str = ".__purging__";
const PRELOAD_BACKOFF: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeightLoadKind {
    Corrupt,
    ResourceLimit,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HealthFile {
    state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    at: Option<String>,
}

fn preload_backoff() -> &'static Mutex<HashMap<String, Instant>> {
    static BACKOFF: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    BACKOFF.get_or_init(|| Mutex::new(HashMap::new()))
}

fn is_capacity_or_device_error(detail: &str) -> bool {
    detail.contains("no compute devices enabled")
        || detail.contains("out of memory")
        || detail.contains("cudamalloc")
        || detail.contains("failed to allocate")
        || detail.contains("create llama context")
        || detail.contains("ggml_backend_cuda")
        || detail.contains("cuda error")
        || detail.contains("cuda_error")
        || detail.contains("invalid device")
        || detail.contains("no cuda")
        || detail.contains("cuda driver")
        || detail.contains("device unavailable")
        || detail.contains("failed to initialize")
        || detail.contains("insufficient")
        || detail.contains("won't fit")
        || detail.contains("cannot host")
}

pub fn classify_weight_load_error(err: &anyhow::Error) -> WeightLoadKind {
    let detail = format!("{err:#}").to_lowercase();
    if detail.contains("not within the file bounds")
        || detail.contains("corrupted or incomplete")
        || detail.contains("unexpected eof")
        || detail.contains("unexpected end of file")
        || detail.contains("truncated download")
    {
        return WeightLoadKind::Corrupt;
    }
    if detail.contains("too many open files")
        || detail.contains("emfile")
        || detail.contains("error=24")
    {
        return WeightLoadKind::ResourceLimit;
    }
    // Disabled / undersized compute is not a bad GGUF: never quarantine for capacity.
    if is_capacity_or_device_error(&detail) {
        return WeightLoadKind::Other;
    }
    // Do NOT treat bare "null result from llama" / "load model" as corrupt.
    WeightLoadKind::Other
}

/// Decide whether a failed load means the on-disk GGUF is bad (vs transient EMFILE/OOM).
pub fn classify_load_failure_for_path(
    runtime_model: &str,
    model_path: &std::path::Path,
    err: &anyhow::Error,
) -> WeightLoadKind {
    let from_err = classify_weight_load_error(err);
    if from_err != WeightLoadKind::Other {
        return from_err;
    }
    let detail = format!("{err:#}").to_lowercase();
    // Capacity / device failures must never fall through to the GGUF structural check  - 
    // that path can false-positive and delete healthy weights after a GPU is disabled.
    if is_capacity_or_device_error(&detail) {
        crate::llm::evict_all();
        return WeightLoadKind::Other;
    }
    match crate::models::gguf_check::gguf_payload_in_bounds(model_path) {
        Ok(false) => {
            warn!(
                runtime_model = %runtime_model,
                path = %model_path.display(),
                "GGUF tensor payloads exceed file size; treating weights as corrupt"
            );
            WeightLoadKind::Corrupt
        }
        Ok(true) => {
            // File looks structurally fine - likely EMFILE or transient llama failure.
            WeightLoadKind::ResourceLimit
        }
        Err(io_err) => {
            let msg = io_err.to_string().to_lowercase();
            if msg.contains("too many open files") || msg.contains("os error 24") {
                WeightLoadKind::ResourceLimit
            } else {
                WeightLoadKind::Other
            }
        }
    }
}

fn health_path(runtime_model: &str) -> PathBuf {
    model_cache_dir(runtime_model).join(HEALTH_FILE)
}

pub fn read_weight_health(runtime_model: &str) -> Option<(String, Option<String>)> {
    let raw = std::fs::read_to_string(health_path(runtime_model)).ok()?;
    let parsed: HealthFile = serde_json::from_str(&raw).ok()?;
    let state = parsed.state.trim().to_ascii_lowercase();
    if state.is_empty() {
        return None;
    }
    Some((state, parsed.error))
}

pub fn clear_weight_health(runtime_model: &str) {
    let _ = std::fs::remove_file(health_path(runtime_model));
    clear_preload_backoff(runtime_model);
}

fn write_weight_health(runtime_model: &str, state: &str, error: Option<&str>) {
    let dir = model_cache_dir(runtime_model);
    let _ = std::fs::create_dir_all(&dir);
    let payload = HealthFile {
        state: state.to_string(),
        error: error
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        at: Some(chrono_like_now()),
    };
    if let Ok(bytes) = serde_json::to_vec_pretty(&payload) {
        let _ = std::fs::write(health_path(runtime_model), bytes);
    }
}

fn chrono_like_now() -> String {
    // Avoid pulling chrono just for a marker timestamp.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

/// User-facing, path-free reason for dashboard/email.
pub fn sanitize_weight_error(err: &anyhow::Error) -> String {
    let detail = format!("{err:#}").to_lowercase();
    if detail.contains("not within the file bounds") || detail.contains("corrupted or incomplete") {
        return "Weights file is corrupted or incomplete.".to_string();
    }
    if detail.contains("unexpected eof") || detail.contains("unexpected end of file") {
        return "Weights download looks truncated.".to_string();
    }
    if detail.contains("too many open files") || detail.contains("emfile") {
        return "Agent hit a file-handle limit while loading weights.".to_string();
    }
    if detail.contains("out of memory") || detail.contains("oom") {
        return "Not enough memory to load this model.".to_string();
    }
    if is_capacity_or_device_error(&detail) {
        return "Not enough enabled compute (VRAM/device) to load this model.".to_string();
    }
    "Model weights failed to load.".to_string()
}

fn delete_weight_files(runtime_model: &str) {
    if let Some(path) = resolve_model_gguf(runtime_model) {
        crate::llm::evict_all_for_path(&path);
    } else if let Some(filenames) = read_manifest_filenames(runtime_model) {
        for filename in filenames {
            let path = target_gguf_path(runtime_model, &filename);
            if path.is_file() {
                crate::llm::evict_all_for_path(&path);
            }
        }
    }

    let dir = model_cache_dir(runtime_model);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name == HEALTH_FILE || name == "manifest.json" {
            continue;
        }
        if path.is_file() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Remove broken GGUFs, mark corrupt for the dashboard, and allow a clean redownload.
pub fn quarantine_corrupt_weights(runtime_model: &str, err: &anyhow::Error) {
    let reason = sanitize_weight_error(err);
    warn!(
        runtime_model = %runtime_model,
        reason = %reason,
        "quarantining corrupt or incomplete model weights"
    );
    delete_weight_files(runtime_model);
    write_weight_health(runtime_model, "corrupt", Some(&reason));
    clear_preload_backoff(runtime_model);
}

pub fn note_preload_resource_limit(runtime_model: &str) {
    if let Ok(mut guard) = preload_backoff().lock() {
        guard.insert(runtime_model.to_string(), Instant::now());
    }
    warn!(
        runtime_model = %runtime_model,
        "skipping model preload for a while after resource-limit failure"
    );
}

pub fn should_skip_preload(runtime_model: &str) -> bool {
    if process_preload_paused() {
        return true;
    }
    if matches!(
        read_weight_health(runtime_model)
            .as_ref()
            .map(|(s, _)| s.as_str()),
        Some("corrupt") | Some("removing")
    ) {
        return true;
    }
    let Ok(guard) = preload_backoff().lock() else {
        return false;
    };
    match guard.get(runtime_model) {
        Some(at) if at.elapsed() < PRELOAD_BACKOFF => true,
        _ => false,
    }
}

pub fn clear_preload_backoff(runtime_model: &str) {
    if let Ok(mut guard) = preload_backoff().lock() {
        guard.remove(runtime_model);
    }
}

/// Instantly hide weights from inventory by renaming the cache dir, then delete in the background.
/// Returns the trash path for async `remove_dir_all` when present.
pub fn stage_purge_model_weights(runtime_model: &str) -> Option<PathBuf> {
    if let Some(path) = resolve_model_gguf(runtime_model) {
        crate::llm::evict_all_for_path(&path);
    }
    clear_weight_health(runtime_model);

    let dir = model_cache_dir(runtime_model);
    if !dir.is_dir() {
        return None;
    }

    let trash = models_dir().join(format!(
        "{}{}{}",
        runtime_cache_key(runtime_model),
        PURGING_MARKER,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    match std::fs::rename(&dir, &trash) {
        Ok(()) => {
            info!(
                runtime_model = %runtime_model,
                trash = %trash.display(),
                "staged model weights for background delete"
            );
            Some(trash)
        }
        Err(err) => {
            warn!(
                runtime_model = %runtime_model,
                error = %err,
                "fast rename purge failed; falling back to synchronous delete"
            );
            let _ = std::fs::remove_dir_all(&dir);
            None
        }
    }
}

pub fn is_purging_cache_key(cache_key: &str) -> bool {
    cache_key.contains(PURGING_MARKER)
}

pub fn runtime_from_purging_cache_key(cache_key: &str) -> Option<String> {
    let (prefix, _) = cache_key.split_once(PURGING_MARKER)?;
    if prefix.is_empty() {
        return None;
    }
    Some(prefix.replace("__", "/"))
}

pub fn spawn_delete_staged_dirs(paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for path in paths {
            let path_label = path.display().to_string();
            let result = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&path)).await;
            match result {
                Ok(Ok(())) => info!(path = %path_label, "deleted staged model weights"),
                Ok(Err(err)) => {
                    warn!(path = %path_label, error = %err, "failed deleting staged model weights")
                }
                Err(err) => warn!(path = %path_label, error = %err, "delete task join failed"),
            }
        }
    });
}

/// Best-effort cleanup of leftover `.purging.` dirs from previous agent runs.
pub fn sweep_staged_purge_dirs() {
    let Ok(entries) = std::fs::read_dir(models_dir()) else {
        return;
    };
    let mut trash = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if is_purging_cache_key(name) {
            trash.push(path);
        }
    }
    if !trash.is_empty() {
        spawn_delete_staged_dirs(trash);
    }
}

pub fn handle_weight_load_failure(runtime_model: &str, err: &anyhow::Error) -> WeightLoadKind {
    let path = resolve_model_gguf(runtime_model);
    let kind = match &path {
        Some(model_path) => classify_load_failure_for_path(runtime_model, model_path, err),
        None => classify_weight_load_error(err),
    };
    match kind {
        WeightLoadKind::Corrupt => quarantine_corrupt_weights(runtime_model, err),
        WeightLoadKind::ResourceLimit => {
            note_preload_resource_limit(runtime_model);
            note_process_resource_limit();
        }
        WeightLoadKind::Other => {
            // Soft backoff so we do not hammer a flaky load every heartbeat.
            note_preload_resource_limit(runtime_model);
        }
    }
    kind
}

fn process_resource_limit() -> &'static Mutex<Option<Instant>> {
    static FLAG: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    FLAG.get_or_init(|| Mutex::new(None))
}

fn note_process_resource_limit() {
    if let Ok(mut guard) = process_resource_limit().lock() {
        *guard = Some(Instant::now());
    }
    warn!("pausing all model preloads briefly after a resource-limit failure (restart agent if this persists)");
}

pub fn process_preload_paused() -> bool {
    let Ok(guard) = process_resource_limit().lock() else {
        return false;
    };
    match *guard {
        Some(at) if at.elapsed() < PRELOAD_BACKOFF => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_corrupt_bounds_error() {
        let err = anyhow::anyhow!(
            "tensor 'blk.29.ffn_down.weight' data is not within the file bounds, model is corrupted or incomplete"
        );
        assert_eq!(classify_weight_load_error(&err), WeightLoadKind::Corrupt);
    }

    #[test]
    fn classifies_emfile() {
        let err = anyhow::anyhow!("failed to open GGUF file (Too many open files)");
        assert_eq!(
            classify_weight_load_error(&err),
            WeightLoadKind::ResourceLimit
        );
    }

    #[test]
    fn null_result_is_not_auto_corrupt() {
        let err = anyhow::anyhow!("load model C:\\weights\\model.gguf: null result from llama cpp");
        assert_eq!(classify_weight_load_error(&err), WeightLoadKind::Other);
    }

    #[test]
    fn disabled_compute_is_not_corrupt() {
        let err = anyhow::anyhow!("no compute devices enabled");
        assert_eq!(classify_weight_load_error(&err), WeightLoadKind::Other);
        let err = anyhow::anyhow!("CUDA error: invalid device ordinal");
        assert_eq!(classify_weight_load_error(&err), WeightLoadKind::Other);
    }
}
