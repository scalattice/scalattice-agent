use crate::models::health::{
    is_purging_cache_key, read_weight_health, runtime_from_purging_cache_key,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use tracing::info;

pub fn models_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SCALATTICE_MODELS_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    crate::paths::models_cache_dir()
}

pub fn runtime_cache_key(runtime_model: &str) -> String {
    runtime_model.replace('/', "__")
}

fn find_cache_dirs_case_insensitive(runtime_model: &str) -> Vec<PathBuf> {
    let want = runtime_cache_key(runtime_model).to_ascii_lowercase();
    if want.is_empty() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(models_dir()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.to_ascii_lowercase() == want {
            out.push(path);
        }
    }
    out
}

fn dir_has_complete_manifest_weights(dir: &Path) -> bool {
    let raw = match std::fs::read_to_string(dir.join("manifest.json")) {
        Ok(raw) => raw,
        Err(_) => return false,
    };
    let Some(filenames) = manifest_filenames(&raw) else {
        return false;
    };
    filenames.iter().all(|filename| {
        let basename = Path::new(filename)
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(filename));
        is_download_complete(&dir.join(basename))
    })
}

fn pick_best_cache_dir(candidates: &[PathBuf]) -> Option<PathBuf> {
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        return candidates.first().cloned();
    }
    let mut best: Option<(PathBuf, bool, u64)> = None;
    for candidate in candidates {
        let ready = dir_has_complete_manifest_weights(candidate);
        let bytes = dir_size_bytes(candidate);
        let replace = match &best {
            None => true,
            Some((_, best_ready, best_bytes)) => {
                ready && !*best_ready || (ready == *best_ready && bytes > *best_bytes)
            }
        };
        if replace {
            best = Some((candidate.clone(), ready, bytes));
        }
    }
    best.map(|(path, _, _)| path)
}

/// Cache directory for a runtime model. Reuses an existing on-disk folder even when
/// casing differs (common after reconnect / older agent builds). When both a
/// canonical and a case-variant folder exist, prefer the one with complete weights.
pub fn model_cache_dir(runtime_model: &str) -> PathBuf {
    let canonical = models_dir().join(runtime_cache_key(runtime_model));
    let mut candidates = find_cache_dirs_case_insensitive(runtime_model);
    if canonical.is_dir() && !candidates.iter().any(|path| path == &canonical) {
        candidates.push(canonical.clone());
    }
    if let Some(best) = pick_best_cache_dir(&candidates) {
        return best;
    }
    canonical
}

pub fn model_manifest_path(runtime_model: &str) -> PathBuf {
    model_cache_dir(runtime_model).join("manifest.json")
}

fn manifest_filenames(raw: &str) -> Option<Vec<String>> {
    let parsed: Value = serde_json::from_str(raw).ok()?;
    let primary = parsed.get("filename")?.as_str()?.trim();
    if primary.is_empty() {
        return None;
    }
    let mut files = vec![primary.to_string()];
    if let Some(companions) = parsed.get("companionFilenames").and_then(Value::as_array) {
        for companion in companions {
            let trimmed = companion.as_str()?.trim();
            if trimmed.is_empty() {
                continue;
            }
            if !files.iter().any(|existing| existing == trimmed) {
                files.push(trimmed.to_string());
            }
        }
    }
    Some(files)
}

pub fn read_manifest_filenames(runtime_model: &str) -> Option<Vec<String>> {
    let raw = std::fs::read_to_string(model_manifest_path(runtime_model)).ok()?;
    manifest_filenames(&raw)
}

fn runtime_id_for_catalog(model: &crate::protocol::CatalogModel) -> &str {
    if model.runtime_model.trim().is_empty() {
        model.model_id.as_str()
    } else {
        model.runtime_model.as_str()
    }
}

fn migrate_cache_dir_alias(from_runtime: &str, to_runtime: &str) {
    let from = model_cache_dir(from_runtime);
    let to_key = runtime_cache_key(to_runtime);
    let to = models_dir().join(&to_key);
    if !from.is_dir() || from == to {
        return;
    }
    if to.is_dir() {
        // Prefer keeping the complete install; drop an empty/partial duplicate alias.
        if dir_has_complete_manifest_weights(&to) {
            return;
        }
        if dir_has_complete_manifest_weights(&from) {
            let _ = std::fs::remove_dir_all(&to);
        } else {
            return;
        }
    }
    match std::fs::rename(&from, &to) {
        Ok(()) => tracing::info!(
            from = %from_runtime,
            to = %to_runtime,
            "migrated model cache folder to canonical runtime path"
        ),
        Err(err) => tracing::warn!(
            from = %from_runtime,
            to = %to_runtime,
            error = %err,
            "could not migrate model cache folder; will keep using alias path"
        ),
    }
}

/// Rewrite manifest to the on-disk GGUF when catalog filename drifted but weights are intact.
fn adopt_existing_gguf(runtime_model: &str, weights: &crate::protocol::ModelWeights) -> bool {
    let dir = model_cache_dir(runtime_model);
    if !dir.is_dir() {
        return false;
    }
    let wanted = Path::new(weights.filename.trim())
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(weights.filename.trim());
    if wanted.is_empty() {
        return false;
    }
    let wanted_path = dir.join(wanted);
    if is_download_complete(&wanted_path) {
        return write_adopted_manifest(runtime_model, weights, wanted).is_ok()
            && model_weights_ready(runtime_model);
    }

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    let mut ggufs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().and_then(|ext| ext.to_str()) == Some("gguf")
                && is_download_complete(path)
        })
        .collect();
    if ggufs.len() != 1 {
        return false;
    }
    let existing = ggufs.remove(0);
    let Some(existing_name) = existing.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    tracing::info!(
        runtime_model = %runtime_model,
        existing = %existing_name,
        catalog = %wanted,
        "adopting existing GGUF after catalog filename change"
    );
    write_adopted_manifest(runtime_model, weights, existing_name).is_ok()
        && model_weights_ready(runtime_model)
}

fn write_adopted_manifest(
    runtime_model: &str,
    weights: &crate::protocol::ModelWeights,
    filename: &str,
) -> std::io::Result<()> {
    let dir = ensure_model_dir(runtime_model)?;
    let body = serde_json::json!({
        "source": weights.source,
        "repo": weights.repo,
        "filename": filename,
        // Adopted installs are single-file; companions from a newer catalog may not exist yet.
        "companionFilenames": Vec::<String>::new(),
        "revision": weights.revision,
        "downloadVia": weights.download_via,
        "mirrorUrl": weights.mirror_url,
    });
    std::fs::write(dir.join("manifest.json"), serde_json::to_vec_pretty(&body)?)
}

pub fn model_weights_ready(runtime_model: &str) -> bool {
    if matches!(
        read_weight_health(runtime_model)
            .as_ref()
            .map(|(s, _)| s.as_str()),
        Some("corrupt") | Some("removing")
    ) {
        return false;
    }
    let Some(filenames) = read_manifest_filenames(runtime_model) else {
        return false;
    };
    filenames.iter().all(|filename| {
        is_manifest_weight_file(runtime_model, &target_gguf_path(runtime_model, filename))
    })
}

/// True when weights are ready under the HF runtime id, a legacy modelId cache folder,
/// or an existing GGUF whose catalog filename merely drifted.
pub fn catalog_model_weights_ready(model: &crate::protocol::CatalogModel) -> bool {
    let runtime = runtime_id_for_catalog(model);
    if model_weights_ready(runtime) {
        return true;
    }
    let model_id = model.model_id.trim();
    if !model_id.is_empty() && !model_id.eq_ignore_ascii_case(runtime) {
        if model_weights_ready(model_id) {
            migrate_cache_dir_alias(model_id, runtime);
            if model_weights_ready(runtime) || model_weights_ready(model_id) {
                return true;
            }
        }
    }
    if let Some(weights) = model.weights.as_ref() {
        if adopt_existing_gguf(runtime, weights) {
            return true;
        }
        if !model_id.is_empty() && !model_id.eq_ignore_ascii_case(runtime) {
            if adopt_existing_gguf(model_id, weights) {
                migrate_cache_dir_alias(model_id, runtime);
                return model_weights_ready(runtime) || model_weights_ready(model_id);
            }
        }
    }
    false
}

pub fn resolve_model_gguf(runtime_model: &str) -> Option<PathBuf> {
    if !model_weights_ready(runtime_model) {
        return None;
    }

    let filenames = read_manifest_filenames(runtime_model)?;
    for filename in &filenames {
        if filename.contains("-00001-of-") {
            return Some(target_gguf_path(runtime_model, filename));
        }
    }

    let primary = filenames.first()?;
    Some(target_gguf_path(runtime_model, primary))
}

/// Projector GGUF next to the language weights (`mmproj*.gguf`).
pub fn resolve_mmproj(model_path: &Path) -> Option<PathBuf> {
    let dir = model_path.parent()?;
    let mut found: Vec<PathBuf> = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if name.contains("mmproj") && name.ends_with(".gguf") && path.is_file() {
            found.push(path);
        }
    }
    found.sort_by_key(|path| {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if name.contains("f16") || name.contains("f32") {
            0u8
        } else if name.contains("q8") {
            1
        } else {
            2
        }
    });
    found.into_iter().next()
}

pub fn models_cache_disk_gb() -> u32 {
    dir_size_gb(&models_dir())
}

fn dir_size_gb(path: &Path) -> u32 {
    let bytes = dir_size_bytes(path);
    ((bytes as f64) / 1024.0 / 1024.0 / 1024.0).round() as u32
}

fn dir_size_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Ok(meta) = path.metadata() {
                total = total.saturating_add(meta.len());
            }
        } else if path.is_dir() {
            total = total.saturating_add(dir_size_bytes(&path));
        }
    }
    total
}

#[derive(Debug, Clone)]
pub struct ModelDiskStatus {
    pub bytes: u64,
    pub complete: bool,
    /// `ok` | `incomplete` | `corrupt` | `removing`
    pub state: String,
    pub error: Option<String>,
}

pub fn list_model_disk_status() -> Vec<(String, ModelDiskStatus)> {
    let Ok(entries) = std::fs::read_dir(models_dir()) else {
        return Vec::new();
    };

    let mut out: Vec<(String, ModelDiskStatus)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(cache_key) = entry.file_name().into_string() else {
            continue;
        };

        if is_purging_cache_key(&cache_key) {
            let Some(runtime_model) = runtime_from_purging_cache_key(&cache_key) else {
                continue;
            };
            let bytes = dir_size_bytes(&path).max(1);
            out.push((
                runtime_model,
                ModelDiskStatus {
                    bytes,
                    complete: false,
                    state: "removing".to_string(),
                    error: Some("Removing weights from disk.".to_string()),
                },
            ));
            continue;
        }

        let runtime_model = cache_key.replace("__", "/");
        let bytes = dir_size_bytes(&path);
        let health = read_weight_health(&runtime_model);
        let corrupt = matches!(health.as_ref().map(|(s, _)| s.as_str()), Some("corrupt"));
        if bytes == 0 && !corrupt {
            continue;
        }
        let complete = model_weights_ready(&runtime_model);
        let (state, error) = if corrupt {
            (
                "corrupt".to_string(),
                health.and_then(|(_, e)| e).or_else(|| {
                    Some("Weights failed to load and will be re-downloaded if the model stays enabled.".to_string())
                }),
            )
        } else if complete {
            ("ok".to_string(), None)
        } else {
            ("incomplete".to_string(), None)
        };
        out.push((
            runtime_model,
            ModelDiskStatus {
                bytes: bytes.max(if corrupt { 1 } else { 0 }),
                complete,
                state,
                error,
            },
        ));
    }

    // Collapse case-variant folders (qwen__… vs Qwen__…) into one inventory row.
    let mut deduped: Vec<(String, ModelDiskStatus)> = Vec::new();
    for (runtime, status) in out {
        let key = runtime.to_ascii_lowercase();
        if let Some(slot) = deduped
            .iter_mut()
            .find(|(runtime, _)| runtime.to_ascii_lowercase() == key)
        {
            let replace = status.complete && !slot.1.complete
                || (status.complete == slot.1.complete && status.bytes > slot.1.bytes)
                || (status.state == "corrupt" && slot.1.state != "corrupt" && !slot.1.complete);
            if replace {
                *slot = (runtime, status);
            }
            continue;
        }
        deduped.push((runtime, status));
    }
    deduped
}

pub fn list_cached_runtime_models() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(models_dir()) else {
        return Vec::new();
    };

    let mut out: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Ok(cache_key) = entry.file_name().into_string() else {
            continue;
        };
        let runtime_model = cache_key.replace("__", "/");
        if !model_weights_ready(&runtime_model) {
            continue;
        }
        let key = runtime_model.to_ascii_lowercase();
        if out
            .iter()
            .any(|existing| existing.to_ascii_lowercase() == key)
        {
            continue;
        }
        out.push(runtime_model);
    }
    out
}

pub fn ensure_model_dir(runtime_model: &str) -> std::io::Result<PathBuf> {
    let dir = model_cache_dir(runtime_model);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn target_gguf_path(runtime_model: &str, filename: &str) -> PathBuf {
    let basename = Path::new(filename)
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(filename));
    model_cache_dir(runtime_model).join(basename)
}

pub fn weight_filenames(weights: &crate::protocol::ModelWeights) -> Vec<&str> {
    let mut files = vec![weights.filename.as_str()];
    for companion in &weights.companion_filenames {
        let trimmed = companion.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !files.iter().any(|existing| *existing == trimmed) {
            files.push(trimmed);
        }
    }
    files
}

pub fn is_download_complete(path: &Path) -> bool {
    if !(path.is_file() && path.metadata().map(|m| m.len() > 0).unwrap_or(false)) {
        return false;
    }
    // Non-empty is not enough: interrupted HF/mirror streams can leave a partial
    // GGUF that still has a valid header. Require tensor payloads to fit.
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gguf"))
    {
        return super::gguf_check::gguf_payload_in_bounds(path).unwrap_or(false);
    }
    true
}

/// A weight file counts as cached only when listed in the model manifest.
pub fn is_manifest_weight_file(runtime_model: &str, path: &Path) -> bool {
    if !is_download_complete(path) {
        return false;
    }
    let Some(filenames) = read_manifest_filenames(runtime_model) else {
        return false;
    };
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    filenames.iter().any(|filename| {
        Path::new(filename)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|basename| basename == name)
    })
}

pub fn purge_incomplete_model_weights(runtime_model: &str) {
    let dir = model_cache_dir(runtime_model);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("part") {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("manifest.json") {
            continue;
        }
        if path.is_file() && !is_manifest_weight_file(runtime_model, &path) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// After a failed download, drop the whole cache dir if weights never became ready.
pub fn purge_failed_download(runtime_model: &str) {
    purge_incomplete_model_weights(runtime_model);
    if model_weights_ready(runtime_model) {
        return;
    }
    let dir = model_cache_dir(runtime_model);
    if dir.is_dir() {
        let _ = std::fs::remove_dir_all(&dir);
        info!(
            runtime_model,
            path = %dir.display(),
            "removed incomplete model weights after failed download"
        );
    }
}
