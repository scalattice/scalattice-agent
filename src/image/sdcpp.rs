//! stable-diffusion.cpp runner (`scalattice-sd`).
//!
//! This is the image equivalent of the chat worker's llama.cpp: one process,
//! the slot's GPU visibility, and the checkpoint the catalog names
//! (safetensors, ckpt, gguf, or the diffusers components). The binary and its
//! ggml live beside the agent (`sd/scalattice-sd`, `sd/lib`) so they are not
//! linked into the agent and do not pull libcuda into the main binary.

use super::sd_models::{self, SdVersion};
use super::{ImageJob, STUB_PNG_B64};
use crate::compute_pool::{cuda_visible_devices_for_slot, PoolStrategy, VirtualCard};
use crate::protocol::{CatalogModel, GeneratedImage};
use anyhow::{anyhow, Context, Result};
use base64::Engine;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Notify;

const WALL: Duration = Duration::from_secs(45 * 60);

#[derive(Debug, Clone, Default)]
pub struct SdWeights {
    pub model: Option<PathBuf>,
    pub diffusion: Option<PathBuf>,
    pub vae: Option<PathBuf>,
    pub clip_l: Option<PathBuf>,
    pub clip_g: Option<PathBuf>,
    pub t5: Option<PathBuf>,
    pub llm: Option<PathBuf>,
}

impl SdWeights {
    fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.diffusion.is_none()
            && self.vae.is_none()
            && self.clip_l.is_none()
            && self.clip_g.is_none()
            && self.t5.is_none()
            && self.llm.is_none()
    }
}

pub fn sd_binary() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("SCALATTICE_SD_BIN") {
        let path = PathBuf::from(raw.trim());
        if path.is_file() {
            return Some(path);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let names = ["scalattice-sd", "scalattice-sd.exe"];
    for name in names {
        let beside = dir.join(name);
        if beside.is_file() {
            return Some(beside);
        }
        let nested = dir.join("sd").join(name);
        if nested.is_file() {
            return Some(nested);
        }
    }
    None
}

pub fn runtime_available() -> bool {
    sd_binary().is_some()
}

/// Files the catalog named, or a diffusers snapshot already on disk.
pub fn resolve_weights(model: &CatalogModel) -> Result<SdWeights> {
    let mut weights = SdWeights::default();
    for path in named_weight_paths(model) {
        assign_path(&mut weights, &path);
    }
    if weights.is_empty() {
        if let Some(repo) = super::image_repo(model) {
            if let Some(from_hub) = diffusers_weights(repo) {
                weights = from_hub;
            }
        }
    }
    if weights.is_empty() {
        return Err(crate::invoke_code::coded(
            crate::invoke_code::InvokeErrorCode::ModelNotInstalled,
            "image checkpoint is not on disk".to_string(),
        ));
    }
    Ok(weights)
}

pub fn weights_on_disk(model: &CatalogModel) -> bool {
    resolve_weights(model).is_ok()
}

fn runtime_id(model: &CatalogModel) -> &str {
    let runtime = model.runtime_model.trim();
    if runtime.is_empty() {
        model.model_id.as_str()
    } else {
        runtime
    }
}

fn named_weight_paths(model: &CatalogModel) -> Vec<PathBuf> {
    let Some(spec) = model.weights.as_ref() else {
        return Vec::new();
    };
    if spec.filename.trim().is_empty() {
        return Vec::new();
    }
    let runtime = runtime_id(model);
    crate::models::weight_filenames(spec)
        .into_iter()
        .map(|name| crate::models::target_gguf_path(runtime, name))
        .filter(|path| path.is_file())
        .collect()
}

fn assign_path(weights: &mut SdWeights, path: &Path) {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let role = weight_role(&name);
    match role {
        Role::Vae => weights.vae = Some(path.to_path_buf()),
        Role::ClipL => weights.clip_l = Some(path.to_path_buf()),
        Role::ClipG => weights.clip_g = Some(path.to_path_buf()),
        Role::T5 => weights.t5 = Some(path.to_path_buf()),
        Role::Llm => weights.llm = Some(path.to_path_buf()),
        Role::Diffusion => weights.diffusion = Some(path.to_path_buf()),
        Role::Model => {
            if weights.model.is_none() {
                weights.model = Some(path.to_path_buf());
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Role {
    Model,
    Diffusion,
    Vae,
    ClipL,
    ClipG,
    T5,
    Llm,
}

fn weight_role(name: &str) -> Role {
    if name.contains("vae") || name.starts_with("ae.") || name.contains("ae.safetensors") {
        Role::Vae
    } else if name.contains("clip_g") || name.contains("clip-g") || name.contains("text_encoder_2")
    {
        Role::ClipG
    } else if name.contains("clip_l")
        || name.contains("clip-l")
        || name.contains("text_encoder")
        || name.contains("clip-vit")
    {
        Role::ClipL
    } else if name.contains("t5") {
        Role::T5
    } else if name.contains("llm") || (name.contains("qwen") && name.contains("encoder")) {
        Role::Llm
    } else if name.contains("diffusion") || name.contains("unet") {
        Role::Diffusion
    } else {
        Role::Model
    }
}

fn diffusers_weights(repo: &str) -> Option<SdWeights> {
    let version = SdVersion::from_repo(repo)?;
    for fp16 in [false, true] {
        let files = sd_models::model_files(repo, version, fp16);
        let take = |file: &sd_models::RepoFile| -> Option<PathBuf> {
            sd_models::find_cached_file(&super::hub_repo_dir(&file.repo), &file.path)
        };
        let weights = SdWeights {
            diffusion: take(&files.unet),
            vae: take(&files.vae),
            clip_l: take(&files.clip),
            clip_g: files.clip2.as_ref().and_then(take),
            ..SdWeights::default()
        };
        if weights.diffusion.is_some() {
            return Some(weights);
        }
    }
    None
}

/// Run one generation. `split` is the tensor-parallel group: the process sees
/// every device in `cuda_visible` and stable-diffusion.cpp splits layers.
pub async fn generate(
    job: &ImageJob,
    weights: &SdWeights,
    card: &VirtualCard,
    cuda_visible: &[u32],
    split: bool,
    cancel: &Notify,
) -> Result<Vec<GeneratedImage>> {
    if super::image_stub_enabled() {
        return Ok(stub_images(job.n));
    }
    let binary = sd_binary().ok_or_else(|| {
        crate::invoke_code::coded(
            crate::invoke_code::InvokeErrorCode::ImageRuntimeMissing,
            "stable-diffusion.cpp runner is not installed (scalattice-sd)".to_string(),
        )
    })?;
    let dir = tempfile_dir()?;
    let output = dir.join("out");
    let mut cmd = Command::new(&binary);
    cmd.arg("--output").arg(&output);
    cmd.arg("--prompt").arg(&job.prompt);
    cmd.arg("--width").arg(job.width.to_string());
    cmd.arg("--height").arg(job.height.to_string());
    cmd.arg("--batch").arg(job.n.clamp(1, 4).to_string());
    cmd.arg("--seed")
        .arg(job.seed.unwrap_or(-1).to_string());
    if split {
        cmd.arg("--split-mode").arg("layer");
    }
    push_opt(&mut cmd, "--model", weights.model.as_deref());
    push_opt(&mut cmd, "--diffusion-model", weights.diffusion.as_deref());
    push_opt(&mut cmd, "--vae", weights.vae.as_deref());
    push_opt(&mut cmd, "--clip-l", weights.clip_l.as_deref());
    push_opt(&mut cmd, "--clip-g", weights.clip_g.as_deref());
    push_opt(&mut cmd, "--t5xxl", weights.t5.as_deref());
    push_opt(&mut cmd, "--llm", weights.llm.as_deref());
    if let Some(init) = job.input_images.first() {
        let init_path = dir.join("init.rgb");
        write_init_rgb(&init.data, &init_path)?;
        cmd.arg("--init-rgb").arg(&init_path);
    }
    apply_slot_env(&mut cmd, card.strategy, cuda_visible);
    apply_lib_path(&mut cmd, &binary);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().with_context(|| {
        format!(
            "image_runtime_missing: failed to start {}",
            binary.display()
        )
    })?;
    let mut stderr = child.stderr.take();
    let log_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(pipe) = stderr.as_mut() {
            let _ = pipe.read_to_end(&mut buf).await;
        }
        buf
    });
    let status = tokio::select! {
        biased;
        _ = cancel.notified() => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = log_task.await;
            let _ = std::fs::remove_dir_all(&dir);
            return Err(anyhow!("request_canceled"));
        }
        status = tokio::time::timeout(WALL, child.wait()) => status,
    };
    let logs = String::from_utf8_lossy(&log_task.await.unwrap_or_default()).to_string();
    let status = status.context("image generation timed out")??;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        let tail = log_tail(&logs);
        return Err(crate::invoke_code::coded(
            crate::invoke_code::InvokeErrorCode::InferenceFailed,
            format!("stable-diffusion.cpp failed: {tail}"),
        ));
    }
    let mut images = Vec::new();
    for i in 0..job.n.clamp(1, 4) {
        let path = dir.join(format!("out.{i}"));
        if !path.is_file() {
            break;
        }
        let raw = std::fs::read(&path)
            .with_context(|| format!("read {}", path.display()))?;
        images.push(raw_to_png(&raw)?);
    }
    let _ = std::fs::remove_dir_all(&dir);
    if images.is_empty() {
        return Err(crate::invoke_code::coded(
            crate::invoke_code::InvokeErrorCode::InferenceFailed,
            "stable-diffusion.cpp wrote no image".to_string(),
        ));
    }
    Ok(images)
}

fn push_opt(cmd: &mut Command, flag: &str, path: Option<&Path>) {
    if let Some(path) = path {
        cmd.arg(flag).arg(path);
    }
}

pub fn apply_slot_env(cmd: &mut Command, strategy: PoolStrategy, cuda_visible: &[u32]) {
    cmd.env(
        "CUDA_VISIBLE_DEVICES",
        cuda_visible_devices_for_slot(strategy, cuda_visible),
    );
    match strategy {
        PoolStrategy::Vulkan => {
            if cuda_visible.is_empty() {
                cmd.env_remove("GGML_VK_VISIBLE_DEVICES");
            } else {
                let list = cuda_visible
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                cmd.env("GGML_VK_VISIBLE_DEVICES", list);
            }
        }
        PoolStrategy::Metal | PoolStrategy::CpuOnly => {
            cmd.env("GGML_VK_VISIBLE_DEVICES", "");
        }
        PoolStrategy::Single | PoolStrategy::TensorParallel => {
            cmd.env("GGML_VK_VISIBLE_DEVICES", "");
        }
    }
}

fn apply_lib_path(cmd: &mut Command, binary: &Path) {
    let Some(lib) = binary.parent().map(|dir| dir.join("lib")) else {
        return;
    };
    if !lib.is_dir() {
        return;
    }
    let lib = lib.to_string_lossy().to_string();
    prepend_env(cmd, "LD_LIBRARY_PATH", &lib);
    prepend_env(cmd, "DYLD_LIBRARY_PATH", &lib);
}

fn prepend_env(cmd: &mut Command, key: &str, prefix: &str) {
    let prev = std::env::var(key).unwrap_or_default();
    if prev.is_empty() {
        cmd.env(key, prefix);
    } else {
        cmd.env(key, format!("{prefix}:{prev}"));
    }
}

fn tempfile_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "scalattice-sd-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn write_init_rgb(b64: &str, path: &Path) -> Result<()> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(b64.trim()))
        .context("input image is not base64")?;
    let img = image::load_from_memory(&bytes).context("input image could not be decoded")?;
    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();
    let mut raw = Vec::with_capacity(12 + rgb.len());
    raw.extend_from_slice(&w.to_le_bytes());
    raw.extend_from_slice(&h.to_le_bytes());
    raw.extend_from_slice(&3u32.to_le_bytes());
    raw.extend_from_slice(rgb.as_raw());
    std::fs::write(path, raw)?;
    Ok(())
}

fn raw_to_png(bytes: &[u8]) -> Result<GeneratedImage> {
    if bytes.len() < 12 {
        anyhow::bail!("inference_failed: short image from stable-diffusion.cpp");
    }
    let w = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let h = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let c = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let pixels = &bytes[12..];
    let expected = (w as usize)
        .saturating_mul(h as usize)
        .saturating_mul(c as usize);
    if pixels.len() < expected || w == 0 || h == 0 {
        anyhow::bail!("inference_failed: truncated image from stable-diffusion.cpp");
    }
    let dynamic = match c {
        1 => image::DynamicImage::ImageLuma8(
            image::GrayImage::from_raw(w, h, pixels[..expected].to_vec())
                .context("luma image")?,
        ),
        3 => image::DynamicImage::ImageRgb8(
            image::RgbImage::from_raw(w, h, pixels[..expected].to_vec()).context("rgb image")?,
        ),
        4 => image::DynamicImage::ImageRgba8(
            image::RgbaImage::from_raw(w, h, pixels[..expected].to_vec()).context("rgba image")?,
        ),
        other => anyhow::bail!("inference_failed: unsupported channel count {other}"),
    };
    let mut png = Vec::new();
    dynamic
        .write_to(
            &mut std::io::Cursor::new(&mut png),
            image::ImageFormat::Png,
        )
        .context("encode png")?;
    Ok(GeneratedImage {
        mime: "image/png".to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(png),
    })
}

fn stub_images(n: u32) -> Vec<GeneratedImage> {
    (0..n.clamp(1, 4))
        .map(|_| GeneratedImage {
            mime: "image/png".to_string(),
            data: STUB_PNG_B64.to_string(),
        })
        .collect()
}

fn log_tail(logs: &str) -> String {
    let trimmed = logs.trim();
    if trimmed.is_empty() {
        return "no log".to_string();
    }
    let start = trimmed.len().saturating_sub(1500);
    trimmed[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::weight_role;

    #[test]
    fn classifies_component_filenames() {
        assert!(matches!(weight_role("ae.safetensors"), super::Role::Vae));
        assert!(matches!(
            weight_role("diffusion_pytorch_model.safetensors"),
            super::Role::Diffusion
        ));
        assert!(matches!(
            weight_role("flux1-dev-q4.gguf"),
            super::Role::Model
        ));
        assert!(matches!(
            weight_role("clip_l.safetensors"),
            super::Role::ClipL
        ));
        assert!(matches!(weight_role("t5xxl_fp16.safetensors"), super::Role::T5));
    }
}
