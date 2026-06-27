use std::path::{Path, PathBuf};

pub fn models_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SCALATTICE_MODELS_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    std::env::var("HOME")
        .map(|home| PathBuf::from(home).join(".cache/scalattice/models"))
        .unwrap_or_else(|_| PathBuf::from(".cache/scalattice/models"))
}

pub fn runtime_cache_key(runtime_model: &str) -> String {
    runtime_model.replace('/', "__")
}

pub fn model_cache_dir(runtime_model: &str) -> PathBuf {
    models_dir().join(runtime_cache_key(runtime_model))
}

pub fn resolve_model_gguf(runtime_model: &str) -> Option<PathBuf> {
    let dir = model_cache_dir(runtime_model);
    for name in ["model.gguf", "ggml-model.gguf", "model-q4_k_m.gguf"] {
        let path = dir.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return None;
    };
    let mut gguf_files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("gguf"))
        .collect();
    gguf_files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    for path in &gguf_files {
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains("-00001-of-"))
        {
            return Some(path.clone());
        }
    }
    gguf_files.into_iter().next()
}

pub fn list_cached_runtime_models() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(models_dir()) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter(|entry| {
            let dir = entry.path();
            std::fs::read_dir(dir)
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .any(|child| {
                    child
                        .path()
                        .extension()
                        .and_then(|ext| ext.to_str())
                        == Some("gguf")
                })
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
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
