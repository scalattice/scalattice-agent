use crate::models::storage::{
    catalog_model_weights_ready, ensure_model_dir, is_manifest_weight_file, target_gguf_path,
    weight_filenames,
};
use crate::protocol::{CatalogModel, ModelWeights};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};
use url::Url;

/// Only accept platform model-mirror URLs under Scalattice Cloud API hosts.
fn assert_allowed_mirror_url(raw: &str) -> Result<()> {
    let url = Url::parse(raw.trim()).with_context(|| format!("invalid mirror URL: {raw}"))?;
    if url.scheme() != "https" {
        bail!("mirror URL must use https");
    }
    let host = url
        .host_str()
        .map(|h| h.to_ascii_lowercase())
        .context("mirror URL missing host")?;
    let allowed = matches!(
        host.as_str(),
        "api.scalattice.cloud" | "scalattice.cloud"
    ) || host.ends_with(".scalattice.cloud");
    if !allowed {
        bail!("mirror URL host not allowed: {host}");
    }
    let path = url.path();
    if !path.contains("/v1/operators/agent/models/") {
        bail!("mirror URL path is not a Scalattice model mirror");
    }
    Ok(())
}

async fn stream_url_to_file(url: &str, dest: &std::path::Path, auth_token: Option<&str>) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent("scalattice-agent")
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .context("build HTTP client")?;

    let mut request = client.get(url);
    if let Some(token) = auth_token.filter(|t| !t.trim().is_empty()) {
        request = request.header("Authorization", format!("Bearer {}", token.trim()));
    }

    let response = request
        .send()
        .await
        .with_context(|| format!("request {url}"))?
        .error_for_status()
        .with_context(|| format!("download failed for {url}"))?;

    let expected_len = response.content_length();

    // After redirects, final URL must still be an allowed mirror when this was a mirror fetch.
    // (HF downloads use huggingface.co and skip this helper's mirror check.)

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create {}", parent.display()))?;
    }

    let tmp = dest.with_extension("part");
    if tmp.exists() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }

    let write_result = async {
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("create {}", tmp.display()))?;
        let mut stream = response.bytes_stream();
        let mut written: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("read download chunk")?;
            written = written.saturating_add(chunk.len() as u64);
            file.write_all(&chunk)
                .await
                .context("write download chunk")?;
        }
        file.flush().await.context("flush download")?;
        drop(file);
        Ok::<u64, anyhow::Error>(written)
    }
    .await;

    let written = match write_result {
        Ok(n) => n,
        Err(err) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(err);
        }
    };

    if let Some(expected) = expected_len {
        if written != expected {
            let _ = tokio::fs::remove_file(&tmp).await;
            bail!(
                "truncated download for {}: got {written} of {expected} bytes",
                dest.display()
            );
        }
    }
    if written == 0 {
        let _ = tokio::fs::remove_file(&tmp).await;
        bail!("empty download for {}", dest.display());
    }

    std::fs::rename(&tmp, dest).with_context(|| format!("finalize {}", dest.display()))?;

    // HF/CDN streams often omit Content-Length; reject truncated GGUFs that
    // would otherwise look "complete" (non-empty file) and fail at load time.
    if dest
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gguf"))
    {
        match super::gguf_check::gguf_payload_in_bounds(dest) {
            Ok(true) => {}
            Ok(false) => {
                let size = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
                let _ = tokio::fs::remove_file(dest).await;
                bail!(
                    "truncated download for {}: GGUF tensor payloads exceed file size ({size} bytes on disk)",
                    dest.display()
                );
            }
            Err(err) => {
                let _ = tokio::fs::remove_file(dest).await;
                return Err(err).with_context(|| {
                    format!("validate downloaded GGUF {}", dest.display())
                });
            }
        }
    }

    Ok(())
}

fn mirror_url_for_filename(weights: &ModelWeights, repo_path: &str) -> Option<String> {
    let mirror_url = weights.mirror_url.as_deref()?.trim();
    if mirror_url.is_empty() {
        return None;
    }
    let basename = Path::new(repo_path)
        .file_name()
        .and_then(|name| name.to_str())?;
    let (prefix, _) = mirror_url.rsplit_once('/')?;
    Some(format!("{prefix}/{basename}"))
}

fn prefer_scalattice_mirror(weights: &ModelWeights) -> bool {
    let via = weights
        .download_via
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let has_mirror = weights
        .mirror_url
        .as_deref()
        .is_some_and(|url| !url.trim().is_empty());
    match via.as_str() {
        "huggingface" | "hf" => false,
        "scalattice" | "mirror" | "proxy" => has_mirror,
        _ => has_mirror,
    }
}

fn weights_download_complete(runtime_model: &str, weights: &ModelWeights) -> bool {
    weight_filenames(weights)
        .iter()
        .all(|filename| is_manifest_weight_file(runtime_model, &target_gguf_path(runtime_model, filename)))
}

async fn download_hf_file(
    runtime_model: &str,
    weights: &ModelWeights,
    repo_path: &str,
    token: Option<&str>,
) -> Result<()> {
    let dest = target_gguf_path(runtime_model, repo_path);
    if is_manifest_weight_file(runtime_model, &dest) {
        return Ok(());
    }
    if dest.exists() {
        let _ = tokio::fs::remove_file(&dest).await;
    }

    ensure_model_dir(runtime_model).context("create model cache directory")?;

    let revision = if weights.revision.trim().is_empty() {
        "main"
    } else {
        weights.revision.trim()
    };
    let url = format!(
        "https://huggingface.co/{}/resolve/{}/{}",
        weights.repo.trim(),
        revision,
        repo_path.trim()
    );

    info!("downloading {} -> {}", weights.repo.trim(), dest.display());
    stream_url_to_file(&url, &dest, token).await
}

async fn download_mirror_file(
    runtime_model: &str,
    _weights: &ModelWeights,
    repo_path: &str,
    mirror_url: &str,
    agent_token: &str,
) -> Result<()> {
    assert_allowed_mirror_url(mirror_url)?;
    let dest = target_gguf_path(runtime_model, repo_path);
    if is_manifest_weight_file(runtime_model, &dest) {
        return Ok(());
    }
    if dest.exists() {
        let _ = tokio::fs::remove_file(&dest).await;
    }

    ensure_model_dir(runtime_model).context("create model cache directory")?;
    info!("downloading from Scalattice mirror -> {}", dest.display());
    stream_url_to_file(mirror_url, &dest, Some(agent_token)).await
}

pub async fn download_hf_gguf(
    runtime_model: &str,
    weights: &ModelWeights,
    token: Option<&str>,
) -> Result<()> {
    if weights.source != "huggingface" {
        bail!("unsupported weight source: {}", weights.source);
    }
    if weights.repo.trim().is_empty() || weights.filename.trim().is_empty() {
        bail!("incomplete Hugging Face weights manifest");
    }

    if weights_download_complete(runtime_model, weights) {
        info!("model already cached for {runtime_model}");
        return Ok(());
    }

    for repo_path in weight_filenames(weights) {
        download_hf_file(runtime_model, weights, repo_path, token).await?;
    }

    write_manifest(runtime_model, weights)?;
    crate::models::clear_weight_health(runtime_model);
    info!("downloaded model weights for {runtime_model}");
    Ok(())
}

async fn download_mirror_gguf(
    runtime_model: &str,
    weights: &ModelWeights,
    agent_token: &str,
) -> Result<()> {
    if weights_download_complete(runtime_model, weights) {
        info!("model already cached for {runtime_model}");
        return Ok(());
    }

    for repo_path in weight_filenames(weights) {
        let dest = target_gguf_path(runtime_model, repo_path);
        if is_manifest_weight_file(runtime_model, &dest) {
            continue;
        }
        let mirror_url = mirror_url_for_filename(weights, repo_path)
            .with_context(|| format!("derive mirror URL for {repo_path}"))?;
        download_mirror_file(runtime_model, weights, repo_path, &mirror_url, agent_token).await?;
    }

    write_manifest(runtime_model, weights)?;
    crate::models::clear_weight_health(runtime_model);
    info!("downloaded model weights for {runtime_model}");
    Ok(())
}

fn write_manifest(runtime_model: &str, weights: &ModelWeights) -> Result<()> {
    let dir = ensure_model_dir(runtime_model)?;
    let manifest = serde_json::json!({
        "source": weights.source,
        "repo": weights.repo,
        "filename": weights.filename,
        "companionFilenames": weights.companion_filenames,
        "revision": weights.revision,
        "downloadVia": weights.download_via,
        "mirrorUrl": weights.mirror_url,
    });
    let path = dir.join("manifest.json");
    std::fs::write(path, serde_json::to_vec_pretty(&manifest)?)?;
    Ok(())
}

pub async fn download_catalog_model(
    model: &CatalogModel,
    agent_token: &str,
    hf_token: Option<&str>,
) -> Result<()> {
    let Some(weights) = model.weights.as_ref() else {
        return Ok(());
    };
    if catalog_model_weights_ready(model) {
        info!(
            "model already cached for {}",
            if model.runtime_model.trim().is_empty() {
                model.model_id.as_str()
            } else {
                model.runtime_model.as_str()
            }
        );
        return Ok(());
    }
    if crate::specs::disk_is_full() {
        anyhow::bail!("no space left on device");
    }
    let runtime_model = if model.runtime_model.trim().is_empty() {
        model.model_id.as_str()
    } else {
        model.runtime_model.as_str()
    };

    if prefer_scalattice_mirror(weights) {
        match download_mirror_gguf(runtime_model, weights, agent_token).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                if is_no_space_error(&err) {
                    warn!("Scalattice mirror download failed (disk full); not trying Hugging Face: {err:#}");
                    return Err(err);
                }
                warn!("Scalattice mirror download failed, trying Hugging Face: {err:#}");
            }
        }
    }

    download_hf_gguf(runtime_model, weights, hf_token).await
}

pub fn is_no_space_error(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}").to_ascii_lowercase();
    text.contains("no space left")
        || text.contains("os error 28")
        || text.contains("os error 112")
        || text.contains("there is not enough space")
}
