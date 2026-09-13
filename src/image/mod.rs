//! Second runtime: Hugging Face Diffusers via an isolated CPython venv.
//! NVIDIA CUDA, AMD (ROCm / DirectML), Intel Arc (XPU), or Apple Silicon MPS.
//! Catalog `jobKind: image` rows supply the repo. Setup runs when an image
//! SKU is installed; teardown when the last image SKU is removed.

mod python;

use crate::compute_pool::{PoolStrategy, VirtualCard};
use crate::protocol::{CatalogModel, GeneratedImage};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Notify;
use tracing::{info, warn};

const WORKER_PY: &str = include_str!("worker.py");
const IMAGE_WALL_CLOCK: Duration = Duration::from_secs(45 * 60);
const IMAGE_SILENCE: Duration = Duration::from_secs(90);
const DEPS_MARKER: &str = ".deps_ok_v3";
const HOST_PYTHON_UNSET: &[&str] = &[
    "PYTHONHOME",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "PYTHONUSERBASE",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "CONDA_DEFAULT_ENV",
    "CONDA_PYTHON_EXE",
    "CONDA_SHLVL",
    "PIP_USER",
    "PIP_REQUIRE_VIRTUALENV",
    "UV_SYSTEM_PYTHON",
    "PYENV_VERSION",
    "PYENV_VIRTUAL_ENV",
];

#[derive(Debug, Clone)]
pub struct ImageJob {
    pub prompt: String,
    pub width: u32,
    pub height: u32,
    pub n: u32,
    pub seed: Option<i64>,
    pub repo: String,
    pub revision: String,
    pub hf_token: Option<String>,
    pub input_images: Vec<GeneratedImage>,
}

#[derive(Debug, Deserialize)]
struct WorkerLine {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    phase: String,
    #[serde(default)]
    pct: Option<f32>,
    #[serde(default)]
    images: Vec<GeneratedImage>,
    #[serde(default)]
    error: String,
    #[serde(default)]
    detail: String,
}

pub fn runtimes_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SCALATTICE_RUNTIMES_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir.trim());
        }
    }
    crate::paths::home_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".cache")
        .join("scalattice")
        .join("runtimes")
}

pub fn diffusers_venv_dir(device: &str) -> PathBuf {
    let name = match device {
        "mps" => "diffusers-mps",
        "rocm" => "diffusers-rocm",
        "dml" => "diffusers-dml",
        "xpu" => "diffusers-xpu",
        _ => "diffusers-cuda",
    };
    runtimes_dir().join(name)
}

fn all_diffusers_venv_dirs() -> Vec<PathBuf> {
    ["cuda", "mps", "rocm", "dml", "xpu"]
        .into_iter()
        .map(diffusers_venv_dir)
        .collect()
}

pub fn hf_hub_cache_dir() -> PathBuf {
    crate::models::models_dir().join("hub")
}

pub fn hub_repo_dir(repo: &str) -> PathBuf {
    let cache_key = format!("models--{}", repo.trim().replace('/', "--"));
    hf_hub_cache_dir().join(cache_key)
}

pub fn is_qwen_image_family(repo: &str) -> bool {
    repo.trim()
        .to_ascii_lowercase()
        .replace('_', "-")
        .contains("qwen-image")
}

/// Map invoke pixels onto the checkpoint. Qwen-Image uses 1328-class natives;
/// other Diffusers repos keep the requested size (default 1024). Zero stays
/// zero when reference images are present so edits can follow the input.
pub fn resolve_image_job_size(
    repo: &str,
    width: u32,
    height: u32,
    has_input_images: bool,
) -> (u32, u32) {
    if width < 64 && height < 64 && has_input_images {
        return (0, 0);
    }
    if is_qwen_image_family(repo) {
        return match (width, height) {
            (0, 0) | (256, 256) | (512, 512) | (1024, 1024) => (1328, 1328),
            (1792, 1024) => (1664, 928),
            (1024, 1792) => (928, 1664),
            (w, h) if w < 64 || h < 64 => (1328, 1328),
            other => other,
        };
    }
    (
        if width < 64 { 1024 } else { width },
        if height < 64 { 1024 } else { height },
    )
}

fn dir_has_weight_file(dir: &Path, depth: u32) -> bool {
    if depth > 6 {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        if child.is_dir() {
            if dir_has_weight_file(&child, depth + 1) {
                return true;
            }
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if name.ends_with(".incomplete") {
            continue;
        }
        if !(name.ends_with(".safetensors")
            || name.ends_with(".bin")
            || name.ends_with(".pt")
            || name.ends_with(".ckpt")
            || name.ends_with(".gguf"))
        {
            continue;
        }
        if child.is_file() {
            return true;
        }
    }
    false
}

const WEIGHT_COMPONENTS: &[&str] = &[
    "transformer",
    "unet",
    "vae",
    "text_encoder",
    "text_encoder_2",
    "text_encoder_3",
    "image_encoder",
    "visual",
];

fn snapshot_components_ready(snap: &Path) -> bool {
    let index_path = snap.join("model_index.json");
    let Ok(raw) = std::fs::read_to_string(&index_path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    let Some(obj) = value.as_object() else {
        return false;
    };
    let mut saw_weight_component = false;
    for (key, val) in obj {
        if key.starts_with('_') || !val.is_array() {
            continue;
        }
        if !WEIGHT_COMPONENTS.iter().any(|name| *name == key.as_str()) {
            continue;
        }
        saw_weight_component = true;
        let folder = snap.join(key);
        if !folder.is_dir() || !dir_has_weight_file(&folder, 0) {
            return false;
        }
    }
    if saw_weight_component {
        return true;
    }
    dir_has_weight_file(snap, 0)
}

/// Complete Diffusers snapshot — `model_index.json` alone is not enough;
/// Hugging Face writes that file first, then the multi-GB weight shards.
/// Leftover `*.incomplete` blobs from an earlier attempt do not block a
/// snapshot that already has its weight components.
pub fn hf_snapshot_dir_ready(root: &Path) -> bool {
    let snapshots = root.join("snapshots");
    let Ok(entries) = std::fs::read_dir(&snapshots) else {
        return false;
    };
    for entry in entries.flatten() {
        let snap = entry.path();
        if snap.is_dir() && snapshot_components_ready(&snap) {
            return true;
        }
    }
    false
}

pub fn hf_snapshot_ready(repo: &str) -> bool {
    let repo = repo.trim();
    if repo.is_empty() {
        return false;
    }
    hf_snapshot_dir_ready(&hub_repo_dir(repo))
}

pub fn image_repo(model: &CatalogModel) -> Option<&str> {
    model
        .weights
        .as_ref()
        .map(|w| w.repo.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub fn image_runtime_ready() -> bool {
    if image_stub_enabled() {
        return true;
    }
    if !python::portable_python_ready() {
        return false;
    }
    all_diffusers_venv_dirs()
        .iter()
        .any(|dir| dir.join(DEPS_MARKER).is_file())
}

pub fn image_install_ready(model: &CatalogModel) -> bool {
    if !model.is_image_job() {
        return false;
    }
    if image_stub_enabled() {
        return true;
    }
    let Some(repo) = image_repo(model) else {
        return false;
    };
    image_runtime_ready() && hf_snapshot_ready(repo)
}

fn ensure_worker_script(venv_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(venv_dir).with_context(|| format!("create {}", venv_dir.display()))?;
    let dest = venv_dir.join("worker.py");
    let existing = std::fs::read_to_string(&dest).unwrap_or_default();
    if existing != WORKER_PY {
        std::fs::write(&dest, WORKER_PY).with_context(|| format!("write {}", dest.display()))?;
    }
    Ok(dest)
}

fn which_bin(name: &str) -> Result<PathBuf> {
    let output = if cfg!(windows) {
        std::process::Command::new("where").arg(name).output()?
    } else {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {name}"))
            .output()?
    };
    if !output.status.success() {
        bail!("{name} not found");
    }
    let path = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if path.is_empty() {
        bail!("{name} not found");
    }
    Ok(PathBuf::from(path))
}

pub(crate) fn apply_isolated_python_env(cmd: &mut std::process::Command) {
    for key in HOST_PYTHON_UNSET {
        cmd.env_remove(*key);
    }
    cmd.env("PYTHONNOUSERSITE", "1");
    cmd.env("PIP_USER", "0");
}

fn apply_isolated_python_env_tokio(cmd: &mut Command) {
    for key in HOST_PYTHON_UNSET {
        cmd.env_remove(*key);
    }
    cmd.env("PYTHONNOUSERSITE", "1");
    cmd.env("PIP_USER", "0");
}

fn looks_like_amd(id: &str, name: &str, kind: &str) -> bool {
    if kind != "discrete" {
        return false;
    }
    let id = id.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    id.starts_with("amd:")
        || id.starts_with("pci-amd:")
        || name.contains("radeon")
        || (name.contains("amd") && !name.contains("intel") && !name.contains("nvidia"))
}

fn looks_like_intel_arc(id: &str, name: &str, kind: &str) -> bool {
    let id = id.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    if name.contains("uhd")
        || name.contains("iris")
        || (name.contains("hd graphics") && !name.contains("arc"))
    {
        return false;
    }
    name.contains("arc") || (id.starts_with("pci-intel:") && kind == "discrete")
}

pub fn is_amd_discrete_card(card: &VirtualCard) -> bool {
    card.devices
        .iter()
        .any(|d| looks_like_amd(&d.id, &d.name, &d.kind))
}

pub fn is_intel_arc_card(card: &VirtualCard) -> bool {
    card.devices
        .iter()
        .any(|d| looks_like_intel_arc(&d.id, &d.name, &d.kind))
}

pub fn image_card_eligible(card: &VirtualCard) -> bool {
    if matches!(card.strategy, PoolStrategy::CpuOnly) {
        return false;
    }
    if card
        .devices
        .iter()
        .any(|d| d.kind == "integrated" && !looks_like_intel_arc(&d.id, &d.name, &d.kind))
        && !is_amd_discrete_card(card)
        && !is_intel_arc_card(card)
    {
        return false;
    }
    let nvidia = matches!(card.strategy, PoolStrategy::Single)
        && !card.uses_vulkan
        && !card.cuda_device_ids.is_empty();
    let metal = matches!(card.strategy, PoolStrategy::Metal);
    let vulkan_ok = matches!(card.strategy, PoolStrategy::Vulkan)
        && (is_amd_discrete_card(card) || is_intel_arc_card(card));
    nvidia || metal || vulkan_ok
}

pub fn amd_visible_index(card: &VirtualCard) -> Vec<u32> {
    vendor_visible_index(card, "amd:")
}

pub fn intel_visible_index(card: &VirtualCard) -> Vec<u32> {
    vendor_visible_index(card, "pci-intel:")
}

fn vendor_visible_index(card: &VirtualCard, prefix: &str) -> Vec<u32> {
    for d in &card.devices {
        if let Some(rest) = d.id.strip_prefix(prefix) {
            if let Ok(i) = rest.parse::<u32>() {
                return vec![i];
            }
        }
    }
    vec![0]
}

pub fn image_backend_for_card(card: &VirtualCard) -> &'static str {
    match card.strategy {
        PoolStrategy::Metal => "mps",
        PoolStrategy::Vulkan if is_intel_arc_card(card) => "xpu",
        PoolStrategy::Vulkan => {
            if cfg!(windows) {
                "dml"
            } else {
                "rocm"
            }
        }
        _ => "cuda",
    }
}

pub fn host_image_backend() -> &'static str {
    if cfg!(target_os = "macos") {
        return "mps";
    }
    if nvidia_cuda_available() {
        return "cuda";
    }
    let devices = crate::specs::detect_all_compute_devices();
    let amd = devices
        .iter()
        .any(|d| d.enabled && looks_like_amd(&d.id, &d.name, &d.kind));
    let arc = devices
        .iter()
        .any(|d| d.enabled && looks_like_intel_arc(&d.id, &d.name, &d.kind));
    if amd {
        return if cfg!(windows) { "dml" } else { "rocm" };
    }
    if arc {
        return "xpu";
    }
    if cfg!(windows) {
        "dml"
    } else {
        "rocm"
    }
}

fn torch_index_for(device: &str) -> &'static str {
    match device {
        "cuda" => "https://download.pytorch.org/whl/cu124",
        "rocm" => "https://download.pytorch.org/whl/rocm6.3",
        "xpu" => "https://download.pytorch.org/whl/xpu",
        _ => "",
    }
}

fn extra_pip_for(device: &str) -> Vec<&'static str> {
    if device == "dml" {
        vec!["torch-directml"]
    } else {
        Vec::new()
    }
}

fn worker_payload(
    device: &str,
    venv_dir: &Path,
    cache_dir: &Path,
    repo: &str,
    revision: &str,
    hf_token: Option<&str>,
    mode: &str,
    job: Option<&ImageJob>,
) -> serde_json::Value {
    let extra_pip = extra_pip_for(device);
    let mut payload = serde_json::json!({
        "mode": mode,
        "repo": repo,
        "revision": revision,
        "hf_token": hf_token,
        "cache_dir": cache_dir.display().to_string(),
        "venv_dir": venv_dir.display().to_string(),
        "device": device,
        "torch_index": torch_index_for(device),
        "extra_pip": extra_pip,
    });
    if let Some(job) = job {
        payload["prompt"] = serde_json::json!(job.prompt);
        payload["width"] = serde_json::json!(job.width);
        payload["height"] = serde_json::json!(job.height);
        payload["n"] = serde_json::json!(job.n.clamp(1, 4));
        payload["seed"] = serde_json::json!(job.seed);
        payload["images"] = serde_json::json!(job.input_images);
    }
    payload
}

async fn run_image_worker(
    python: &Path,
    script: &Path,
    job_path: &Path,
    device: &str,
    gpu_visible: &[u32],
    setup: bool,
    cancel: Option<&Notify>,
    cancel_flag: Option<&AtomicBool>,
    mut on_progress: impl FnMut(&str, Option<f32>),
) -> Result<Vec<GeneratedImage>> {
    let cache_dir = hf_hub_cache_dir();
    let cvd = gpu_visible
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let mut cmd = Command::new(python);
    cmd.arg(script);
    if setup {
        cmd.arg("--setup");
    }
    cmd.arg(job_path)
        .env("PYTHONUNBUFFERED", "1")
        .env("HF_HUB_CACHE", &cache_dir)
        .env("HUGGINGFACE_HUB_CACHE", &cache_dir)
        .env("PYTORCH_ENABLE_MPS_FALLBACK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_isolated_python_env_tokio(&mut cmd);
    cmd.env_remove("CUDA_VISIBLE_DEVICES");
    cmd.env_remove("HIP_VISIBLE_DEVICES");
    cmd.env_remove("ROCR_VISIBLE_DEVICES");
    cmd.env_remove("ZE_AFFINITY_MASK");
    if !cvd.is_empty() && device != "mps" && device != "dml" {
        if device == "xpu" {
            cmd.env("ZE_AFFINITY_MASK", &cvd);
        } else {
            cmd.env("CUDA_VISIBLE_DEVICES", &cvd);
            if device == "rocm" {
                cmd.env("HIP_VISIBLE_DEVICES", &cvd);
                cmd.env("ROCR_VISIBLE_DEVICES", &cvd);
            }
        }
    }
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd
        .spawn()
        .context("spawn Diffusers image python worker")?;

    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let t = line.trim();
                        if t.is_empty() {
                            continue;
                        }
                        if t.contains("unauthenticated requests to the HF Hub") {
                            continue;
                        }
                        info!(target: "qwen_image", "{t}");
                    }
                    Err(_) => break,
                }
            }
        });
    }

    let stdout = child.stdout.take().context("worker stdout")?;
    let mut reader = BufReader::new(stdout);
    let mut buf = String::new();
    let started = Instant::now();
    let mut last_progress = Instant::now();
    let mut images: Option<Vec<GeneratedImage>> = None;
    let mut setup_done = false;

    loop {
        if cancel_flag.is_some_and(|f| f.load(Ordering::Relaxed)) {
            let _ = child.kill().await;
            bail!("request_canceled");
        }
        if started.elapsed() >= IMAGE_WALL_CLOCK {
            let _ = child.kill().await;
            bail!("invoke_timeout: image job exceeded wall-clock limit");
        }
        let silence_left = IMAGE_SILENCE
            .checked_sub(last_progress.elapsed())
            .unwrap_or(Duration::ZERO);
        let wall_left = IMAGE_WALL_CLOCK
            .checked_sub(started.elapsed())
            .unwrap_or(Duration::from_millis(1));
        let wait = silence_left.min(wall_left).max(Duration::from_millis(50));
        tokio::select! {
            biased;
            _ = async {
                if let Some(n) = cancel {
                    n.notified().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                let _ = child.kill().await;
                bail!("request_canceled");
            }
            n = reader.read_line(&mut buf) => {
                let n = n.context("read image worker stdout")?;
                if n == 0 {
                    break;
                }
                last_progress = Instant::now();
                let line = buf.trim().to_string();
                buf.clear();
                if line.is_empty() {
                    continue;
                }
                let parsed: WorkerLine = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => {
                        warn!(line = %line, "ignoring non-json image worker stdout");
                        continue;
                    }
                };
                match parsed.kind.as_str() {
                    "progress" => {
                        let phase = if parsed.phase.is_empty() {
                            "working"
                        } else {
                            parsed.phase.as_str()
                        };
                        on_progress(phase, parsed.pct);
                    }
                    "result" => {
                        setup_done = true;
                        images = Some(parsed.images);
                    }
                    "error" => {
                        let _ = child.kill().await;
                        let code = if parsed.error.is_empty() {
                            "inference_failed".to_string()
                        } else {
                            parsed.error
                        };
                        if parsed.detail.is_empty() {
                            bail!("{code}");
                        }
                        bail!("{code}: {}", parsed.detail);
                    }
                    _ => {}
                }
            }
            _ = tokio::time::sleep(wait) => {
                if last_progress.elapsed() >= IMAGE_SILENCE {
                    let _ = child.kill().await;
                    bail!("agent invoke timeout");
                }
            }
        }
    }

    let status = child.wait().await.context("wait image worker")?;
    if setup {
        if !status.success() && !setup_done {
            bail!("image_runtime_missing: image setup worker exited {status}");
        }
        return Ok(images.unwrap_or_default());
    }
    let images = images.ok_or_else(|| {
        if status.success() {
            anyhow!("inference_failed: image worker returned no images")
        } else {
            anyhow!("inference_failed: image worker exited {}", status)
        }
    })?;
    if images.is_empty() {
        bail!("inference_failed: image worker produced no images");
    }
    Ok(images)
}

/// Install isolated CPython, Diffusers venv, and the HF snapshot for this SKU.
pub async fn install_image_model(
    model: &CatalogModel,
    hf_token: Option<&str>,
    cancel: &AtomicBool,
) -> Result<()> {
    if image_stub_enabled() {
        return Ok(());
    }
    let repo = image_repo(model).context(
        "image_runtime_missing: catalog image models need a Hugging Face Diffusers repo",
    )?;
    if image_install_ready(model) {
        return Ok(());
    }
    let device = host_image_backend();
    let venv_dir = diffusers_venv_dir(device);
    let script = ensure_worker_script(&venv_dir)?;
    let mut on_progress = |phase: &str, pct: Option<f32>| {
        info!(
            phase,
            pct = pct.unwrap_or(-1.0),
            repo,
            "image runtime setup"
        );
    };
    let python = python::ensure_image_python(&mut on_progress).await?;
    let cache_dir = hf_hub_cache_dir();
    let _ = std::fs::create_dir_all(&cache_dir);
    let revision = model
        .weights
        .as_ref()
        .map(|w| w.revision.as_str())
        .unwrap_or("main");
    let payload = worker_payload(
        device,
        &venv_dir,
        &cache_dir,
        repo,
        revision,
        hf_token,
        "setup",
        None,
    );
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let job_path = venv_dir.join(format!("setup-{}-{nonce}.json", std::process::id()));
    std::fs::write(&job_path, serde_json::to_vec_pretty(&payload)?)
        .with_context(|| format!("write {}", job_path.display()))?;
    struct JobFileGuard(PathBuf);
    impl Drop for JobFileGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _job_guard = JobFileGuard(job_path.clone());
    info!(repo, device, "setting up isolated Diffusers runtime");
    run_image_worker(
        &python,
        &script,
        &job_path,
        device,
        &[],
        true,
        None,
        Some(cancel),
        on_progress,
    )
    .await?;
    if !hf_snapshot_ready(repo) {
        bail!("model_load_failed: Diffusers snapshot missing after setup ({repo})");
    }
    Ok(())
}

pub fn teardown_image_runtime() {
    python::teardown_portable_python();
    for dir in all_diffusers_venv_dirs() {
        if dir.is_dir() {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => info!(path = %dir.display(), "removed Diffusers image venv"),
                Err(err) => warn!(
                    path = %dir.display(),
                    error = %err,
                    "failed to remove Diffusers image venv"
                ),
            }
        }
    }
}

pub fn maybe_teardown_image_runtime(any_enabled_image: bool) {
    if any_enabled_image {
        return;
    }
    if !python::portable_python_ready()
        && all_diffusers_venv_dirs().iter().all(|d| !d.exists())
    {
        return;
    }
    info!("no image models remain enabled; removing isolated Diffusers runtime");
    teardown_image_runtime();
}

pub fn stage_purge_image_snapshot(repo: &str) -> Option<PathBuf> {
    let repo = repo.trim();
    if repo.is_empty() {
        return None;
    }
    let dir = hub_repo_dir(repo);
    if !dir.is_dir() {
        return None;
    }
    let trash = hf_hub_cache_dir().join(format!(
        ".purging-{}-{}",
        repo.replace('/', "--"),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    match std::fs::rename(&dir, &trash) {
        Ok(()) => {
            info!(repo, trash = %trash.display(), "staged Diffusers snapshot for delete");
            Some(trash)
        }
        Err(err) => {
            warn!(repo, error = %err, "fast rename of Diffusers snapshot failed; deleting");
            let _ = std::fs::remove_dir_all(&dir);
            None
        }
    }
}

pub async fn run_qwen_image(
    job: &ImageJob,
    cuda_visible: &[u32],
    device: &str,
    cancel: &Notify,
    mut on_progress: impl FnMut(&str, Option<f32>),
) -> Result<Vec<GeneratedImage>> {
    let venv_dir = diffusers_venv_dir(device);
    let script = ensure_worker_script(&venv_dir)?;
    let python = python::ensure_image_python(&mut on_progress).await?;
    let cache_dir = hf_hub_cache_dir();
    let _ = std::fs::create_dir_all(&cache_dir);

    let payload = worker_payload(
        device,
        &venv_dir,
        &cache_dir,
        &job.repo,
        &job.revision,
        job.hf_token.as_deref(),
        "generate",
        Some(job),
    );

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let job_path = venv_dir.join(format!("job-{}-{nonce}.json", std::process::id()));
    std::fs::write(&job_path, serde_json::to_vec_pretty(&payload)?)
        .with_context(|| format!("write {}", job_path.display()))?;
    struct JobFileGuard(PathBuf);
    impl Drop for JobFileGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _job_guard = JobFileGuard(job_path.clone());

    info!(
        repo = %job.repo,
        width = job.width,
        height = job.height,
        n = job.n,
        device = %device,
        "starting Diffusers image worker"
    );

    run_image_worker(
        &python,
        &script,
        &job_path,
        device,
        cuda_visible,
        false,
        Some(cancel),
        None,
        on_progress,
    )
    .await
}

pub fn nvidia_cuda_available() -> bool {
    if image_stub_enabled() {
        return true;
    }
    if which_bin("nvidia-smi").is_ok() {
        return true;
    }
    crate::specs::detect_all_compute_devices().iter().any(|d| {
        d.enabled
            && d.kind == "discrete"
            && (d.id.starts_with("nvidia:")
                || d.name.to_ascii_lowercase().contains("nvidia")
                || d.name.to_ascii_lowercase().contains("geforce"))
    })
}

pub fn metal_image_available() -> bool {
    if image_stub_enabled() {
        return true;
    }
    cfg!(target_os = "macos") && crate::compute_pool::metal_runtime_supported()
}

fn image_stub_enabled() -> bool {
    std::env::var("SCALATTICE_QWEN_IMAGE_STUB")
        .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute_pool::{PoolDevice, VirtualCard};

    #[test]
    fn worker_script_embeds() {
        assert!(WORKER_PY.contains("DiffusionPipeline"));
        assert!(WORKER_PY.contains("SCALATTICE_QWEN_IMAGE_STUB"));
        assert!(WORKER_PY.contains("os.execv"));
        assert!(WORKER_PY.contains("image_edit_unsupported"));
        assert!(WORKER_PY.contains("is_qwen_image_family"));
        assert!(WORKER_PY.contains("pick_torch_device"));
        assert!(WORKER_PY.contains("torch_index"));
        assert!(WORKER_PY.contains("--setup"));
        assert!(WORKER_PY.contains("torch_directml"));
        assert!(WORKER_PY.contains("isolate_from_host_python"));
        assert!(WORKER_PY.contains("snapshot_download"));
        assert!(WORKER_PY.contains("image_accelerator_required"));
        assert!(WORKER_PY.contains(r#"want == "xpu""#));
        assert!(WORKER_PY.contains("torch.xpu"));
    }

    #[test]
    fn qwen_repo_maps_openai_sizes() {
        assert_eq!(
            resolve_image_job_size("Qwen/Qwen-Image", 1024, 1024, false),
            (1328, 1328)
        );
        assert_eq!(
            resolve_image_job_size("Qwen/Qwen-Image", 1792, 1024, false),
            (1664, 928)
        );
        assert_eq!(
            resolve_image_job_size("Qwen/Qwen-Image-Edit-2509", 0, 0, true),
            (0, 0)
        );
    }

    #[test]
    fn other_diffusers_repos_keep_requested_size() {
        assert_eq!(
            resolve_image_job_size("stabilityai/stable-diffusion-xl-base-1.0", 1024, 1024, false),
            (1024, 1024)
        );
        assert_eq!(
            resolve_image_job_size("black-forest-labs/FLUX.1-dev", 0, 0, false),
            (1024, 1024)
        );
        assert_eq!(
            resolve_image_job_size("some-org/edit-pipe", 0, 0, true),
            (0, 0)
        );
    }

    fn card(id: &str, kind: &str, name: &str, strategy: PoolStrategy, vulkan: bool) -> VirtualCard {
        VirtualCard {
            devices: vec![PoolDevice {
                id: id.into(),
                kind: kind.into(),
                name: name.into(),
                vram_gb: 16,
                cuda_index: None,
            }],
            strategy,
            display_name: name.into(),
            total_vram_gb: 16,
            tensor_split: vec![],
            cuda_device_ids: if vulkan { vec![] } else { vec![0] },
            uses_vulkan: vulkan,
            gpu_layer_budget: 0,
        }
    }

    #[test]
    fn amd_discrete_card_is_image_eligible() {
        let amd = card(
            "amd:0",
            "discrete",
            "AMD Radeon RX 7900 XTX",
            PoolStrategy::Vulkan,
            true,
        );
        assert!(is_amd_discrete_card(&amd));
        assert!(image_card_eligible(&amd));
        assert_eq!(amd_visible_index(&amd), vec![0]);
        assert_eq!(
            image_backend_for_card(&amd),
            if cfg!(windows) { "dml" } else { "rocm" }
        );
    }

    #[test]
    fn intel_arc_card_is_image_eligible() {
        let arc = card(
            "pci-intel:0",
            "discrete",
            "Intel Arc A770",
            PoolStrategy::Vulkan,
            true,
        );
        assert!(!is_amd_discrete_card(&arc));
        assert!(is_intel_arc_card(&arc));
        assert!(image_card_eligible(&arc));
        assert_eq!(intel_visible_index(&arc), vec![0]);
        assert_eq!(image_backend_for_card(&arc), "xpu");
        let uhd = card(
            "pci-intel:0",
            "integrated",
            "Intel UHD Graphics 770",
            PoolStrategy::Vulkan,
            true,
        );
        assert!(!is_intel_arc_card(&uhd));
        assert!(!image_card_eligible(&uhd));
        let cpu = card("cpu:0", "cpu", "CPU", PoolStrategy::CpuOnly, false);
        assert!(!image_card_eligible(&cpu));
    }

    #[test]
    fn teardown_removes_cpython_and_venvs() {
        let dir = std::env::temp_dir().join(format!(
            "scalattice-image-teardown-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("SCALATTICE_RUNTIMES_DIR").ok();
        std::env::set_var("SCALATTICE_RUNTIMES_DIR", &dir);
        let py = python::install_root();
        std::fs::create_dir_all(&py).unwrap();
        std::fs::write(py.join("marker"), b"x").unwrap();
        for name in [
            "diffusers-cuda",
            "diffusers-rocm",
            "diffusers-dml",
            "diffusers-mps",
            "diffusers-xpu",
        ]
        {
            let v = dir.join(name);
            std::fs::create_dir_all(&v).unwrap();
            std::fs::write(v.join(".deps_ok_v3"), b"ok").unwrap();
        }
        teardown_image_runtime();
        assert!(!py.exists());
        for name in [
            "diffusers-cuda",
            "diffusers-rocm",
            "diffusers-dml",
            "diffusers-mps",
            "diffusers-xpu",
        ]
        {
            assert!(!dir.join(name).exists(), "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
        match prev {
            Some(v) => std::env::set_var("SCALATTICE_RUNTIMES_DIR", v),
            None => std::env::remove_var("SCALATTICE_RUNTIMES_DIR"),
        }
    }

    #[test]
    fn snapshot_index_alone_is_not_ready() {
        let root = std::env::temp_dir().join(format!("slt-hf-index-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let snap = root.join("snapshots").join("abc");
        std::fs::create_dir_all(snap.join("transformer")).unwrap();
        std::fs::write(
            snap.join("model_index.json"),
            r#"{"_class_name":"X","transformer":["diffusers","T"]}"#,
        )
        .unwrap();
        assert!(
            !hf_snapshot_dir_ready(&root),
            "model_index.json without weights must not advertise"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn snapshot_ready_when_transformer_weights_exist() {
        let root = std::env::temp_dir().join(format!("slt-hf-ready-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let snap = root.join("snapshots").join("abc");
        std::fs::create_dir_all(snap.join("transformer")).unwrap();
        std::fs::write(
            snap.join("model_index.json"),
            r#"{"_class_name":"X","transformer":["diffusers","T"]}"#,
        )
        .unwrap();
        std::fs::write(snap.join("transformer").join("model.safetensors"), b"weights").unwrap();
        assert!(hf_snapshot_dir_ready(&root));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn leftover_incomplete_blob_does_not_block_ready_snapshot() {
        let root = std::env::temp_dir().join(format!("slt-hf-inc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let snap = root.join("snapshots").join("abc");
        std::fs::create_dir_all(snap.join("transformer")).unwrap();
        std::fs::create_dir_all(root.join("blobs")).unwrap();
        std::fs::write(
            snap.join("model_index.json"),
            r#"{"_class_name":"X","transformer":["diffusers","T"]}"#,
        )
        .unwrap();
        std::fs::write(snap.join("transformer").join("model.safetensors"), b"weights").unwrap();
        std::fs::write(root.join("blobs").join("shard.incomplete"), b"partial").unwrap();
        assert!(
            hf_snapshot_dir_ready(&root),
            "complete weight components must win over leftover .incomplete blobs"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
