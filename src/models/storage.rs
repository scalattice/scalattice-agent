use serde_json::Value;
use std::path::{Path, PathBuf};

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

pub fn model_cache_dir(runtime_model: &str) -> PathBuf {
    models_dir().join(runtime_cache_key(runtime_model))
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

pub fn model_weights_ready(runtime_model: &str) -> bool {
    let Some(filenames) = read_manifest_filenames(runtime_model) else {
        return false;
    };
    filenames
        .iter()
        .all(|filename| is_manifest_weight_file(runtime_model, &target_gguf_path(runtime_model, filename)))
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
}

pub fn list_model_disk_status() -> Vec<(String, ModelDiskStatus)> {
    let Ok(entries) = std::fs::read_dir(models_dir()) else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let cache_key = entry.file_name().into_string().ok()?;
            let runtime_model = cache_key.replace("__", "/");
            let bytes = dir_size_bytes(&entry.path());
            if bytes == 0 {
                return None;
            }
            Some((
                runtime_model.clone(),
                ModelDiskStatus {
                    bytes,
                    complete: model_weights_ready(&runtime_model),
                },
            ))
        })
        .collect()
}

pub fn list_cached_runtime_models() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(models_dir()) else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let cache_key = entry.file_name().into_string().ok()?;
            let runtime_model = cache_key.replace("__", "/");
            if model_weights_ready(&runtime_model) {
                Some(runtime_model)
            } else {
                None
            }
        })
        .collect()
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
    path.is_file() && path.metadata().map(|m| m.len() > 0).unwrap_or(false)
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

pub fn purge_model_weights(runtime_model: &str) {
    if let Some(path) = resolve_model_gguf(runtime_model) {
        crate::llm::evict_all_for_path(&path);
    }
    let dir = model_cache_dir(runtime_model);
    if dir.is_dir() {
        let _ = std::fs::remove_dir_all(&dir);
    }
}
