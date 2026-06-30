use super::{current_version, normalize_version};
use anyhow::{Context, Result};
use std::time::Duration;

pub(crate) const GITHUB_API_LATEST: &str =
    "https://api.github.com/repos/Robottik-Software/Scalattice-Client/releases/latest";

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

pub(crate) async fn download_release_asset(tag: &str, asset_name: &str, dest: &std::path::Path) -> Result<()> {
    use std::io::Write;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).context("create download directory")?;
    }

    let url = release_download_url(tag, asset_name);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .user_agent(format!("scalattice-agent/{}", current_version()))
        .build()
        .context("build HTTP client")?;

    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("download asset from {url}"))?
        .error_for_status()
        .with_context(|| format!("asset download failed for {asset_name}"))?;

    let bytes = response.bytes().await.context("read release asset bytes")?;
    if bytes.len() < 512 * 1024 {
        anyhow::bail!("download for {asset_name} looks too small ({})", bytes.len());
    }

    let mut file = std::fs::File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("write {}", dest.display()))?;
    Ok(())
}
