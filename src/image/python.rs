//! Portable CPython for the Diffusers image worker.
//!
//! Isolated under `~/.cache/scalattice/runtimes/cpython-…`. Never uses or
//! mutates the machine's PATH / conda / pyenv Python.

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use futures_util::StreamExt;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tar::Archive;
use tracing::info;

fn canceled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|flag| flag.load(Ordering::Relaxed))
}

const PYTHON_TAG: &str = "20260901";
const PYTHON_VERSION: &str = "3.12.14";
const PYTHON_BASE: &str =
    "https://github.com/astral-sh/python-build-standalone/releases/download/20260901";

pub struct StandaloneArtifact {
    pub filename: &'static str,
}

pub fn standalone_artifact() -> Option<StandaloneArtifact> {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let triple = match (os, arch) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        _ => return None,
    };
    let filename = match triple {
        "x86_64-unknown-linux-gnu" => {
            "cpython-3.12.14+20260901-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz"
        }
        "aarch64-unknown-linux-gnu" => {
            "cpython-3.12.14+20260901-aarch64-unknown-linux-gnu-install_only_stripped.tar.gz"
        }
        "x86_64-apple-darwin" => {
            "cpython-3.12.14+20260901-x86_64-apple-darwin-install_only_stripped.tar.gz"
        }
        "aarch64-apple-darwin" => {
            "cpython-3.12.14+20260901-aarch64-apple-darwin-install_only_stripped.tar.gz"
        }
        "x86_64-pc-windows-msvc" => {
            "cpython-3.12.14+20260901-x86_64-pc-windows-msvc-install_only_stripped.tar.gz"
        }
        "aarch64-pc-windows-msvc" => {
            "cpython-3.12.14+20260901-aarch64-pc-windows-msvc-install_only_stripped.tar.gz"
        }
        _ => return None,
    };
    Some(StandaloneArtifact { filename })
}

pub fn install_root() -> PathBuf {
    super::runtimes_dir().join(format!("cpython-{PYTHON_VERSION}+{PYTHON_TAG}"))
}

fn python_bin_in(root: &Path) -> PathBuf {
    if cfg!(windows) {
        root.join("python").join("python.exe")
    } else {
        root.join("python").join("bin").join("python3")
    }
}

pub fn portable_python_bin() -> PathBuf {
    python_bin_in(&install_root())
}

fn python_runs(bin: &Path) -> bool {
    if !bin.is_file() {
        return false;
    }
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("-c")
        .arg("import sys; assert sys.version_info[:2] >= (3, 10)");
    super::apply_isolated_python_env(&mut cmd);
    apply_no_window(&mut cmd);
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

fn apply_no_window(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd;
}

pub fn portable_python_ready() -> bool {
    python_runs(&portable_python_bin())
}

/// Portable CPython if already extracted, otherwise download it.
/// Never falls back to a PATH / conda / pyenv interpreter.
pub async fn ensure_image_python(
    mut on_progress: impl FnMut(&str, Option<f32>),
    cancel: Option<&AtomicBool>,
) -> Result<PathBuf> {
    if canceled(cancel) {
        bail!("request_canceled");
    }
    let bin = portable_python_bin();
    if python_runs(&bin) {
        return Ok(bin);
    }

    on_progress("python", Some(2.0));
    install_portable_python(&mut on_progress, cancel)
        .await
        .context("image_runtime_missing: could not install isolated Python from GitHub")
}

pub fn teardown_portable_python() {
    let root = install_root();
    if root.is_dir() {
        match fs::remove_dir_all(&root) {
            Ok(()) => info!(path = %root.display(), "removed isolated image CPython"),
            Err(err) => tracing::warn!(
                path = %root.display(),
                error = %err,
                "failed to remove isolated image CPython"
            ),
        }
    }
}

async fn install_portable_python(
    on_progress: &mut impl FnMut(&str, Option<f32>),
    cancel: Option<&AtomicBool>,
) -> Result<PathBuf> {
    if canceled(cancel) {
        bail!("request_canceled");
    }
    let artifact = standalone_artifact().context(
        "image_runtime_missing: no portable CPython build for this OS/arch",
    )?;
    let root = install_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;

    let url = format!("{PYTHON_BASE}/{}", artifact.filename);
    let archive = root.join(artifact.filename);
    info!(url = %url, dest = %archive.display(), "downloading isolated CPython");
    download_file(&url, &archive, on_progress, cancel).await?;
    if canceled(cancel) {
        let _ = fs::remove_dir_all(&root);
        bail!("request_canceled");
    }
    on_progress("python", Some(85.0));

    let archive_clone = archive.clone();
    let root_clone = root.clone();
    tokio::task::spawn_blocking(move || extract_tarball(&archive_clone, &root_clone))
        .await
        .context("join python extract")??;
    if canceled(cancel) {
        let _ = fs::remove_dir_all(&root);
        bail!("request_canceled");
    }
    let _ = fs::remove_file(&archive);

    let bin = python_bin_in(&root);
    if !python_runs(&bin) {
        bail!(
            "image_runtime_missing: extracted CPython does not run ({})",
            bin.display()
        );
    }
    on_progress("python", Some(95.0));
    info!(path = %bin.display(), "isolated CPython ready");
    Ok(bin)
}

async fn download_file(
    url: &str,
    dest: &Path,
    on_progress: &mut impl FnMut(&str, Option<f32>),
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent("scalattice-agent")
        .redirect(reqwest::redirect::Policy::limited(8))
        .build()
        .context("build HTTP client")?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("download {url}"))?;
    let total = response.content_length().unwrap_or(0);
    let tmp = dest.with_extension("part");
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).ok();
    }
    let mut file = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    let mut stream = response.bytes_stream();
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        if canceled(cancel) {
            drop(file);
            let _ = fs::remove_file(&tmp);
            bail!("request_canceled");
        }
        let chunk = chunk.context("read python download")?;
        file.write_all(&chunk)
            .with_context(|| format!("write {}", tmp.display()))?;
        written += chunk.len() as u64;
        if total > 0 {
            let pct = 5.0 + (written as f32 / total as f32) * 75.0;
            on_progress("python", Some(pct.min(80.0)));
        }
    }
    file.flush().ok();
    drop(file);
    if canceled(cancel) {
        let _ = fs::remove_file(&tmp);
        bail!("request_canceled");
    }
    fs::rename(&tmp, dest).with_context(|| format!("rename {}", dest.display()))?;
    Ok(())
}

fn extract_tarball(archive: &Path, dest: &Path) -> Result<()> {
    let file = File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let mut tarball = Archive::new(GzDecoder::new(file));
    tarball
        .unpack(dest)
        .with_context(|| format!("unpack {} into {}", archive.display(), dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_artifact_covers_desktop_targets() {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        if matches!(os, "linux" | "macos" | "windows")
            && matches!(arch, "x86_64" | "aarch64")
        {
            let art = standalone_artifact().expect("portable CPython triple");
            assert!(art.filename.contains(PYTHON_VERSION));
            assert!(art.filename.contains(PYTHON_TAG));
            assert!(art.filename.contains("install_only_stripped"));
        }
    }

    #[test]
    fn portable_python_lives_under_runtimes_not_path() {
        let root = install_root();
        let s = root.to_string_lossy();
        assert!(s.contains("cpython-"));
        assert!(!s.contains("/usr/bin"));
        assert!(portable_python_bin().starts_with(&root));
    }
}
