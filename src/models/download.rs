use crate::models::storage::{
    catalog_model_weights_ready, ensure_model_dir, is_manifest_weight_file, target_gguf_path,
    weight_filenames,
};
use crate::protocol::{CatalogModel, ModelWeights};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::StatusCode;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};
use url::Url;

const DOWNLOAD_RETRIES: u32 = 8;

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

fn content_range_total(value: Option<&reqwest::header::HeaderValue>) -> Option<u64> {
    let raw = value?.to_str().ok()?.trim();
    let total = raw.split('/').nth(1)?.trim();
    if total == "*" {
        return None;
    }
    total.parse().ok()
}

pub fn is_retryable_transfer_error(err: &anyhow::Error) -> bool {
    if is_no_space_error(err) {
        return false;
    }
    let text = format!("{err:#}").to_ascii_lowercase();
    if text.contains("404") || text.contains("401") || text.contains("403") || text.contains("410")
    {
        return false;
    }
    text.contains("end of file")
        || text.contains("error decoding response body")
        || text.contains("error reading a body")
        || text.contains("error sending request")
        || text.contains("connection")
        || text.contains("timed out")
        || text.contains("timeout")
        || text.contains("reset")
        || text.contains("broken pipe")
        || text.contains("unexpected eof")
        || text.contains("temporarily")
        || text.contains("network is unreachable")
        || text.contains("502")
        || text.contains("503")
        || text.contains("504")
        || text.contains("429")
        || text.contains("truncated download")
}

async fn part_len(path: &Path) -> u64 {
    tokio::fs::metadata(path)
        .await
        .ok()
        .map(|m| m.len())
        .unwrap_or(0)
}

async fn finalize_download(tmp: &Path, dest: &Path) -> Result<()> {
    let written = part_len(tmp).await;
    if written == 0 {
        let _ = tokio::fs::remove_file(tmp).await;
        bail!("empty download for {}", dest.display());
    }
    tokio::fs::rename(tmp, dest)
        .await
        .with_context(|| format!("finalize {}", dest.display()))?;

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

async fn stream_url_once(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    tmp: &Path,
    auth_token: Option<&str>,
) -> Result<()> {
    let existing = part_len(tmp).await;
    let mut request = client.get(url);
    if let Some(token) = auth_token.filter(|t| !t.trim().is_empty()) {
        request = request.header("Authorization", format!("Bearer {}", token.trim()));
    }
    if existing > 0 {
        request = request.header(RANGE, format!("bytes={existing}-"));
        info!("resuming {} from {existing} bytes", dest.display());
    }

    let response = request
        .send()
        .await
        .with_context(|| format!("request {url}"))?;
    let status = response.status();

    if status == StatusCode::RANGE_NOT_SATISFIABLE && existing > 0 {
        return finalize_download(tmp, dest).await;
    }
    if !status.is_success() {
        let code = status.as_u16();
        let body = response.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(180).collect();
        bail!("download failed for {url} (HTTP {code}) {snippet}");
    }

    let resume = existing > 0 && status == StatusCode::PARTIAL_CONTENT;
    if existing > 0 && !resume {
        warn!(
            "server ignored resume for {}; restarting from byte 0",
            dest.display()
        );
        let _ = tokio::fs::remove_file(tmp).await;
    }
    let start_at = if resume { existing } else { 0 };
    let expected_total = if resume {
        content_range_total(response.headers().get(CONTENT_RANGE))
            .or_else(|| response.content_length().map(|n| start_at.saturating_add(n)))
    } else {
        response.content_length()
    };

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(resume)
        .truncate(!resume)
        .open(tmp)
        .await
        .with_context(|| format!("open {}", tmp.display()))?;
    let mut stream = response.bytes_stream();
    let mut written = start_at;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read download chunk")?;
        written = written.saturating_add(chunk.len() as u64);
        file.write_all(&chunk)
            .await
            .context("write download chunk")?;
    }
    file.flush().await.context("flush download")?;
    drop(file);

    if let Some(expected) = expected_total {
        if written < expected {
            bail!(
                "truncated download for {}: got {written} of {expected} bytes",
                dest.display()
            );
        }
        if written > expected {
            let _ = tokio::fs::remove_file(tmp).await;
            bail!(
                "download larger than expected for {}: got {written} of {expected} bytes",
                dest.display()
            );
        }
    }
    finalize_download(tmp, dest).await
}

async fn stream_url_to_file(url: &str, dest: &Path, auth_token: Option<&str>) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent("scalattice-agent")
        .redirect(reqwest::redirect::Policy::limited(8))
        .tcp_keepalive(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .context("build HTTP client")?;

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create {}", parent.display()))?;
    }

    let tmp = dest.with_extension("part");
    let mut last_err = None;
    for attempt in 1..=DOWNLOAD_RETRIES {
        match stream_url_once(&client, url, dest, &tmp, auth_token).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                if !is_retryable_transfer_error(&err) || attempt == DOWNLOAD_RETRIES {
                    return Err(err);
                }
                let have = part_len(&tmp).await;
                let wait = Duration::from_secs(2u64.saturating_pow(attempt.min(4)));
                warn!(
                    "download interrupted at {have} bytes (attempt {attempt}/{DOWNLOAD_RETRIES}): {err:#}; retrying in {}s",
                    wait.as_secs()
                );
                last_err = Some(err);
                tokio::time::sleep(wait).await;
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("download failed")))
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
                for repo_path in weight_filenames(weights) {
                    let dest = target_gguf_path(runtime_model, repo_path);
                    let tmp = dest.with_extension("part");
                    let _ = tokio::fs::remove_file(&tmp).await;
                }
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
