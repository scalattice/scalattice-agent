use crate::models::storage::{ensure_model_dir, is_download_complete, target_gguf_path};
use crate::protocol::{CatalogModel, ModelWeights};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

async fn stream_url_to_file(url: &str, dest: &std::path::Path, auth_token: Option<&str>) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent("scalattice-agent")
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

    let tmp = dest.with_extension("part");
    if tmp.exists() {
        let _ = std::fs::remove_file(&tmp);
    }

    let mut file = tokio::fs::File::create(&tmp)
        .await
        .with_context(|| format!("create {}", tmp.display()))?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read download chunk")?;
        file.write_all(&chunk)
            .await
            .context("write download chunk")?;
    }
    file.flush().await.context("flush download")?;
    drop(file);

    std::fs::rename(tmp, dest).with_context(|| format!("finalize {}", dest.display()))?;
    Ok(())
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

    let dest = target_gguf_path(runtime_model, &weights.filename);
    if is_download_complete(&dest) {
        info!("model already cached: {}", dest.display());
        return Ok(());
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
        weights.filename.trim()
    );

    info!(
        "downloading {} -> {}",
        weights.repo.trim(),
        dest.display()
    );

    stream_url_to_file(&url, &dest, token).await?;
    write_manifest(runtime_model, weights)?;

    info!("downloaded model weights to {}", dest.display());
    Ok(())
}

async fn download_mirror_gguf(
    runtime_model: &str,
    weights: &ModelWeights,
    mirror_url: &str,
    agent_token: &str,
) -> Result<()> {
    let dest = target_gguf_path(runtime_model, &weights.filename);
    if is_download_complete(&dest) {
        info!("model already cached: {}", dest.display());
        return Ok(());
    }

    ensure_model_dir(runtime_model).context("create model cache directory")?;
    info!("downloading from Scalattice mirror -> {}", dest.display());
    stream_url_to_file(mirror_url, &dest, Some(agent_token)).await?;
    write_manifest(runtime_model, weights)?;
    info!("downloaded model weights to {}", dest.display());
    Ok(())
}

fn write_manifest(runtime_model: &str, weights: &ModelWeights) -> Result<()> {
    let dir = ensure_model_dir(runtime_model)?;
    let manifest = serde_json::json!({
        "source": weights.source,
        "repo": weights.repo,
        "filename": weights.filename,
        "revision": weights.revision,
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
    let runtime_model = if model.runtime_model.trim().is_empty() {
        model.model_id.as_str()
    } else {
        model.runtime_model.as_str()
    };

    if let Some(mirror_url) = weights.mirror_url.as_deref().filter(|u| !u.trim().is_empty()) {
        match download_mirror_gguf(runtime_model, weights, mirror_url, agent_token).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                warn!("Scalattice mirror download failed, trying Hugging Face: {err:#}");
            }
        }
    }

    download_hf_gguf(runtime_model, weights, hf_token).await
}
