use super::{current_version, normalize_version};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// Scalattice Cloud release channel (repo name is resolved server-side only).
pub(crate) const CLOUD_RELEASE_BASE: &str = "https://scalattice.cloud/api/v1/health/agent-release";

const MIN_RELEASE_BYTES: u64 = 512 * 1024;
const DOWNLOAD_ATTEMPTS: u32 = 3;

pub(crate) struct LatestRelease {
    pub tag: String,
    pub version: String,
    pub checksums: HashMap<String, String>,
}

pub(crate) async fn fetch_latest_release() -> Result<LatestRelease> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .user_agent(format!("scalattice-agent/{}", current_version()))
        .build()
        .context("build HTTP client")?;

    let response = client
        .get(format!("{CLOUD_RELEASE_BASE}/latest"))
        .header("Accept", "application/json")
        .send()
        .await
        .context("request Scalattice Cloud latest release")?
        .error_for_status()
        .context("Scalattice Cloud latest release request failed")?;

    let payload: serde_json::Value = response
        .json()
        .await
        .context("parse Scalattice Cloud release JSON")?;
    let tag = payload
        .get("tag")
        .and_then(|v| v.as_str())
        .context("release missing tag")?
        .to_string();
    let version = payload
        .get("version")
        .and_then(|v| v.as_str())
        .map(normalize_version)
        .unwrap_or_else(|| normalize_version(&tag));
    let mut checksums = HashMap::new();
    if let Some(obj) = payload.get("checksums").and_then(|v| v.as_object()) {
        for (name, value) in obj {
            if let Some(digest) = value.as_str() {
                let hex = digest
                    .trim()
                    .strip_prefix("sha256:")
                    .unwrap_or(digest.trim())
                    .to_ascii_lowercase();
                if !hex.is_empty() {
                    checksums.insert(name.clone(), hex);
                }
            }
        }
    }
    Ok(LatestRelease {
        tag,
        version,
        checksums,
    })
}

pub(crate) fn release_download_url(tag: &str, asset_name: &str) -> String {
    format!(
        "{CLOUD_RELEASE_BASE}/download/{}/{}",
        urlencoding_path(tag),
        urlencoding_path(asset_name)
    )
}

fn urlencoding_path(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}

pub(crate) async fn download_release_asset(
    tag: &str,
    asset_name: &str,
    dest: &Path,
    expected_sha256: &str,
) -> Result<()> {
    let expected = expected_sha256.trim().to_ascii_lowercase();
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("refusing to install {asset_name}: missing or invalid published SHA-256 checksum");
    }

    let url = release_download_url(tag, asset_name);
    let mut last_err = None;
    for attempt in 1..=DOWNLOAD_ATTEMPTS {
        match download_release_asset_once(&url, asset_name, dest, &expected).await {
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

async fn download_release_asset_once(
    url: &str,
    asset_name: &str,
    dest: &Path,
    expected_sha256: &str,
) -> Result<()> {
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
    let mut hasher = Sha256::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read release asset chunk")?;
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        hasher.update(&chunk);
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
        bail!("download for {asset_name} looks too small ({downloaded} bytes)");
    }

    let actual = format!("{:x}", hasher.finalize());
    if actual != expected_sha256 {
        tokio::fs::remove_file(&tmp).await.ok();
        bail!(
            "checksum mismatch for {asset_name}: expected {expected_sha256}, got {actual}"
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
