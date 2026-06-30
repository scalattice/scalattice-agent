use super::{current_version, normalize_version};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

pub(crate) const GITHUB_API_LATEST: &str =
    "https://api.github.com/repos/Robottik-Software/Scalattice-Client/releases/latest";

const MIN_RELEASE_BYTES: u64 = 512 * 1024;
const DOWNLOAD_ATTEMPTS: u32 = 3;

pub(crate) struct LatestRelease {
    pub tag: String,
    pub version: String,
}

pub(crate) async fn fetch_latest_release() -> Result<LatestRelease> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .user_agent(format!("scalattice-agent/{}", current_version()))
        .build()
        .context("build HTTP client")?;

    let response = client
        .get(GITHUB_API_LATEST)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("request GitHub latest release")?
        .error_for_status()
        .context("GitHub latest release request failed")?;

    let payload: serde_json::Value = response.json().await.context("parse GitHub release JSON")?;
    let tag = payload
        .get("tag_name")
        .and_then(|v| v.as_str())
        .context("release missing tag_name")?
        .to_string();
    let version = normalize_version(&tag);
    Ok(LatestRelease { tag, version })
}

pub(crate) fn release_download_url(tag: &str, asset_name: &str) -> String {
    format!(
        "https://github.com/Robottik-Software/Scalattice-Client/releases/download/{tag}/{asset_name}"
    )
}

pub(crate) async fn download_release_asset(
    tag: &str,
    asset_name: &str,
    dest: &Path,
) -> Result<()> {
    let url = release_download_url(tag, asset_name);
    let mut last_err = None;
    for attempt in 1..=DOWNLOAD_ATTEMPTS {
        match download_release_asset_once(&url, asset_name, dest).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                if attempt < DOWNLOAD_ATTEMPTS {
                    eprintln!(
                        "Download failed (attempt {attempt}/{DOWNLOAD_ATTEMPTS}): {err:#}. Retrying…"
                    );
                }
                last_err = Some(err);
                tokio::time::sleep(Duration::from_secs(u64::from(attempt) * 3)).await;
            }
        }
    }
    Err(last_err.unwrap())
}

async fn download_release_asset_once(url: &str, asset_name: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .context("create download directory")?;
    }

    let client = release_download_client()?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("download asset from {url}"))?
        .error_for_status()
        .with_context(|| format!("asset download failed for {asset_name}"))?;

    let total = response.content_length();
    let tmp = dest.with_extension("part");
    if tmp.exists() {
        tokio::fs::remove_file(&tmp)
            .await
            .with_context(|| format!("remove stale {}", tmp.display()))?;
    }

    let mut file = tokio::fs::File::create(&tmp)
        .await
        .with_context(|| format!("create {}", tmp.display()))?;
    let mut stream = response.bytes_stream();
    let mut downloaded = 0u64;
    let mut last_progress_mb = 0u64;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read release asset chunk")?;
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        file.write_all(&chunk)
            .await
            .context("write release asset chunk")?;
        let mb = downloaded / (1024 * 1024);
        if mb >= last_progress_mb + 5 {
            last_progress_mb = mb;
            if let Some(total) = total {
                eprintln!("  … {mb} / {} MB", total / (1024 * 1024));
            } else {
                eprintln!("  … {mb} MB");
            }
        }
    }

    file.flush().await.context("flush release asset download")?;
    drop(file);

    if downloaded < MIN_RELEASE_BYTES {
        tokio::fs::remove_file(&tmp).await.ok();
        anyhow::bail!(
            "download for {asset_name} looks too small ({downloaded} bytes)"
        );
    }

    tokio::fs::rename(&tmp, dest)
        .await
        .with_context(|| format!("finalize {}", dest.display()))?;
    Ok(())
}

fn release_download_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(60))
        .tcp_keepalive(Duration::from_secs(30))
        .user_agent(format!("scalattice-agent/{}", current_version()))
        .build()
        .context("build HTTP client")
}
