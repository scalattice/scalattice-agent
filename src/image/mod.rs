//! Image generation. The engine is stable-diffusion.cpp (`scalattice-sd`),
//! spawned on the slot the server assigned, with that slot's GPU visibility.
//! It loads the checkpoint the catalog names (safetensors, ckpt, gguf, or
//! diffusers components): SD 1/2/XL, SD3, FLUX, Qwen-Image, and the other
//! families upstream stable-diffusion.cpp supports. CUDA, Vulkan, and Metal
//! come from that binary, not from a cargo feature on the agent.

pub mod sdcpp;
mod sd_models;

use crate::compute_pool::{PoolStrategy, VirtualCard};
use crate::protocol::{CatalogModel, GeneratedImage};
use anyhow::{anyhow, bail, Context, Result};
use sd_models::{ModelFiles, SdVersion};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::{info, warn};

/// 1x1 PNG returned when `SCALATTICE_QWEN_IMAGE_STUB=1` (tests / CI).
pub(crate) const STUB_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

#[derive(Debug, Clone)]
pub struct ImageJob {
    pub prompt: String,
    pub width: u32,
    pub height: u32,
    pub n: u32,
    pub seed: Option<i64>,
    /// Catalog repo. The runner resolves files from the catalog model, not this copy.
    #[allow(dead_code)]
    pub repo: String,
    /// Revision / token are consumed at install time (`install_image_model`);
    /// generation reads only the local hub cache. Kept so call sites stay stable.
    #[allow(dead_code)]
    pub revision: String,
    #[allow(dead_code)]
    pub hf_token: Option<String>,
    pub input_images: Vec<GeneratedImage>,
}

/// Legacy location of the Diffusers venv / portable CPython. Only used to
/// clean up installs from before the stable-diffusion.cpp runner.
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

pub fn hf_hub_cache_dir() -> PathBuf {
    crate::models::models_dir().join("hub")
}

pub fn hub_repo_dir(repo: &str) -> PathBuf {
    let cache_key = format!("models--{}", repo.trim().replace('/', "--"));
    hf_hub_cache_dir().join(cache_key)
}

/// Map invoke pixels onto the checkpoint. Sides under 64 become the family's
/// native size (SDXL 1024, SD 2.1 768, SD 1.5 / Turbo 512). Zero stays zero
/// when reference images are present so img2img can follow the input.
pub fn resolve_image_job_size(
    repo: &str,
    width: u32,
    height: u32,
    has_input_images: bool,
) -> (u32, u32) {
    if width < 64 && height < 64 && has_input_images {
        return (0, 0);
    }
    let default = SdVersion::from_repo(repo)
        .map(SdVersion::default_size)
        .unwrap_or(1024);
    (
        if width < 64 { default } else { width },
        if height < 64 { default } else { height },
    )
}

pub fn image_repo(model: &CatalogModel) -> Option<&str> {
    model
        .weights
        .as_ref()
        .map(|w| w.repo.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn weights_use_f16() -> bool {
    true
}

fn files_for(repo: &str, version: SdVersion) -> ModelFiles {
    sd_models::model_files(repo, version, weights_use_f16())
}

/// Every required file resolved inside `hub`, or the first missing one.
fn resolve_files_in(hub: &Path, files: &ModelFiles) -> std::result::Result<(), String> {
    let find = |f: &sd_models::RepoFile| -> std::result::Result<(), String> {
        let root = hub.join(format!("models--{}", f.repo.replace('/', "--")));
        sd_models::find_cached_file(&root, &f.path)
            .map(|_| ())
            .ok_or_else(|| format!("{}/{}", f.repo, f.path))
    };
    find(&files.tokenizer)?;
    if let Some(file) = &files.tokenizer2 {
        find(file)?;
    }
    find(&files.clip)?;
    if let Some(file) = &files.clip2 {
        find(file)?;
    }
    find(&files.unet)?;
    find(&files.vae)?;
    Ok(())
}

fn checkpoint_ready_in(hub: &Path, repo: &str) -> bool {
    if let Some(files) = sd_models::aux_repo_files(repo) {
        let root = hub.join(format!("models--{}", repo.trim().replace('/', "--")));
        return files
            .iter()
            .all(|f| sd_models::find_cached_file(&root, f).is_some());
    }
    let Some(version) = SdVersion::from_repo(repo) else {
        return false;
    };
    resolve_files_in(hub, &files_for(repo, version)).is_ok()
}

/// All required weight + tokenizer files for this repo are in the hub cache.
pub fn hf_snapshot_ready(repo: &str) -> bool {
    let repo = repo.trim();
    !repo.is_empty() && checkpoint_ready_in(&hf_hub_cache_dir(), repo)
}

/// Checkpoint bytes are on disk (catalog file, or a diffusers snapshot).
pub fn image_weights_ready(model: &CatalogModel) -> bool {
    if !model.is_image_job() {
        return false;
    }
    if image_stub_enabled() {
        return true;
    }
    sdcpp::weights_on_disk(model)
}

/// Weights are on disk and `scalattice-sd` is installed next to the agent.
pub fn image_install_ready(model: &CatalogModel) -> bool {
    image_weights_ready(model) && (image_stub_enabled() || sdcpp::runtime_available())
}

fn image_repo_on_disk(repo: &str) -> bool {
    hub_repo_dir(repo).is_dir()
}

/// Keep this Hub snapshot when another remaining image SKU still uses the repo.
pub fn keep_image_repo(
    catalog: &[CatalogModel],
    repo: &str,
    skip_model_ids: &[String],
    mut is_enabled: impl FnMut(&str) -> bool,
) -> bool {
    let repo = repo.trim();
    if repo.is_empty() {
        return false;
    }
    catalog.iter().any(|model| {
        if !model.is_image_job() {
            return false;
        }
        if skip_model_ids.iter().any(|id| id == &model.model_id) {
            return false;
        }
        let Some(other) = image_repo(model) else {
            return false;
        };
        if !other.eq_ignore_ascii_case(repo) {
            return false;
        }
        is_enabled(&model.model_id) || image_repo_on_disk(other)
    })
}

/// Image jobs write weights and scratch. Refuse before claiming a GPU when
/// the model cache volume is already full.
pub fn refuse_if_disk_full() -> Result<()> {
    // Always re-measure the models volume so a prior ENOSPC sticky bit clears
    // once free space returns (fleet: OSART33-PC diskFull=true with 192 GB free).
    crate::specs::refresh_disk_full_flag();
    if crate::specs::disk_is_full() {
        bail!("disk_full: this machine has no free disk space for image jobs");
    }
    Ok(())
}

fn io_is_no_space(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(28) | Some(112)) || {
        let text = err.to_string().to_ascii_lowercase();
        text.contains("no space left") || text.contains("not enough space")
    }
}

fn map_image_io_error(err: std::io::Error, path: &Path) -> anyhow::Error {
    if io_is_no_space(&err) {
        crate::state::set_disk_full(true);
        anyhow!("disk_full: this machine has no free disk space for image jobs")
    } else {
        anyhow::Error::new(err).context(format!("write {}", path.display()))
    }
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

/// Any accelerator chat can use. CPU slots are not image slots.
pub fn image_card_eligible(card: &VirtualCard) -> bool {
    !matches!(card.strategy, PoolStrategy::CpuOnly)
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

/// Backend label for logs. stable-diffusion.cpp selects the device from the
/// process environment, not from this string.
pub fn image_backend_for_card(card: &VirtualCard) -> &'static str {
    match card.strategy {
        PoolStrategy::Metal => "metal",
        PoolStrategy::Vulkan => "vulkan",
        PoolStrategy::CpuOnly => "cpu",
        PoolStrategy::TensorParallel | PoolStrategy::Single => "cuda",
    }
}

fn unsupported_repo_error(repo: &str) -> anyhow::Error {
    crate::invoke_code::coded(
        crate::invoke_code::InvokeErrorCode::ImageRuntimeMissing,
        format!(
            "{repo} has no weights.filename. Name the checkpoint file \
             (safetensors, ckpt, or gguf) the way a chat model names its GGUF"
        ),
    )
}

fn download_error(file: &sd_models::RepoFile, err: impl std::fmt::Display) -> anyhow::Error {
    let text = format!("{err}");
    let lower = text.to_ascii_lowercase();
    if lower.contains("no space left")
        || lower.contains("os error 28")
        || lower.contains("os error 112")
        || lower.contains("not enough space")
    {
        crate::state::set_disk_full(true);
        return anyhow!(
            "disk_full: no space left while downloading {}/{}",
            file.repo,
            file.path
        );
    }
    anyhow!(
        "model_load_failed: download {}/{} failed: {text}",
        file.repo,
        file.path
    )
}

/// Download the safetensors + tokenizer files for this SKU into the hub cache.
pub async fn install_image_model(
    model: &CatalogModel,
    hf_token: Option<&str>,
    cancel: &AtomicBool,
) -> Result<()> {
    if image_stub_enabled() {
        return Ok(());
    }
    refuse_if_disk_full()?;
    let repo = image_repo(model)
        .context("image_runtime_missing: catalog image models need a Hugging Face repo")?;
    let version = SdVersion::from_repo(repo).ok_or_else(|| unsupported_repo_error(repo))?;
    if cancel.load(Ordering::Relaxed) {
        bail!("request_canceled");
    }
    if image_install_ready(model) {
        return Ok(());
    }
    let revision = model
        .weights
        .as_ref()
        .map(|w| w.revision.trim())
        .filter(|r| !r.is_empty())
        .unwrap_or("main")
        .to_string();
    let cache_dir = hf_hub_cache_dir();
    std::fs::create_dir_all(&cache_dir).map_err(|e| map_image_io_error(e, &cache_dir))?;

    let mut builder = hf_hub::api::tokio::ApiBuilder::new()
        .with_cache_dir(cache_dir.clone())
        .with_progress(false);
    if let Some(token) = hf_token.map(str::trim).filter(|t| !t.is_empty()) {
        builder = builder.with_token(Some(token.to_string()));
    }
    let api = builder
        .build()
        .map_err(|e| anyhow!("model_load_failed: Hugging Face client: {e}"))?;

    let files = files_for(repo, version);
    let wanted = files.all();
    let total = wanted.len();
    info!(
        repo,
        family = version.label(),
        files = total,
        "installing image checkpoint"
    );
    for (idx, file) in wanted.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            bail!("request_canceled");
        }
        let root = hub_repo_dir(&file.repo);
        if sd_models::find_cached_file(&root, &file.path).is_some() {
            continue;
        }
        let file_revision = if file.repo.eq_ignore_ascii_case(repo) {
            revision.clone()
        } else {
            "main".to_string()
        };
        info!(
            repo = %file.repo,
            path = %file.path,
            step = idx + 1,
            total,
            "downloading image checkpoint file"
        );
        let hub_repo = api.repo(hf_hub::Repo::with_revision(
            file.repo.clone(),
            hf_hub::RepoType::Model,
            file_revision,
        ));
        let fetch = hub_repo.get(&file.path);
        tokio::pin!(fetch);
        let fetched = loop {
            tokio::select! {
                res = &mut fetch => break res,
                _ = tokio::time::sleep(Duration::from_millis(250)) => {
                    if cancel.load(Ordering::Relaxed) {
                        bail!("request_canceled");
                    }
                }
            }
        };
        fetched.map_err(|e| download_error(file, e))?;
    }
    if !hf_snapshot_ready(repo) {
        bail!("model_load_failed: image checkpoint files missing after download ({repo})");
    }
    Ok(())
}

/// Remove old Diffusers venvs and portable CPython (best effort,
/// on a background thread; they can be several GB).
pub fn cleanup_legacy_diffusers_runtime() {
    let dir = runtimes_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let stale: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("diffusers-") || n.starts_with("cpython-"))
        })
        .collect();
    if stale.is_empty() {
        return;
    }
    info!(
        count = stale.len(),
        "removing legacy Diffusers image runtime"
    );
    std::thread::spawn(move || {
        for path in stale {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => info!(path = %path.display(), "removed legacy Diffusers runtime dir"),
                Err(err) => warn!(
                    path = %path.display(),
                    error = %err,
                    "failed to remove legacy Diffusers runtime dir"
                ),
            }
        }
    });
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
            info!(repo, trash = %trash.display(), "staged image checkpoint for delete");
            Some(trash)
        }
        Err(err) => {
            warn!(repo, error = %err, "fast rename of image checkpoint failed; deleting");
            let _ = std::fs::remove_dir_all(&dir);
            None
        }
    }
}

/// Best-effort cleanup of leftover Hub `.purging-*` dirs from a previous run,
/// plus any leftover Diffusers runtime.
pub fn sweep_staged_image_purge_dirs() {
    cleanup_legacy_diffusers_runtime();
    let Ok(entries) = std::fs::read_dir(hf_hub_cache_dir()) else {
        return;
    };
    let mut trash = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.starts_with(".purging-") {
            trash.push(path);
        }
    }
    if !trash.is_empty() {
        crate::models::spawn_delete_staged_dirs(trash);
    }
}

pub fn metal_image_available() -> bool {
    image_stub_enabled() || crate::compute_pool::metal_runtime_supported()
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
    use crate::protocol::{CatalogModel, ModelWeights};

    fn image_sku(id: &str, repo: &str) -> CatalogModel {
        CatalogModel {
            model_id: id.into(),
            job_kind: "image".into(),
            weights: Some(ModelWeights {
                source: "huggingface".into(),
                repo: repo.into(),
                filename: String::new(),
                companion_filenames: vec![],
                revision: "main".into(),
                download_via: None,
                mirror_url: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn sd_repos_use_family_native_size() {
        assert_eq!(
            resolve_image_job_size(
                "stabilityai/stable-diffusion-xl-base-1.0",
                1024,
                1024,
                false
            ),
            (1024, 1024)
        );
        assert_eq!(
            resolve_image_job_size("stabilityai/sdxl-turbo", 0, 0, false),
            (512, 512)
        );
        assert_eq!(
            resolve_image_job_size("stabilityai/stable-diffusion-2-1", 0, 0, false),
            (768, 768)
        );
        assert_eq!(
            resolve_image_job_size("runwayml/stable-diffusion-v1-5", 256, 256, false),
            (256, 256)
        );
        assert_eq!(
            resolve_image_job_size("runwayml/stable-diffusion-v1-5", 0, 640, false),
            (512, 640)
        );
    }

    #[test]
    fn other_repos_keep_requested_size() {
        assert_eq!(
            resolve_image_job_size("black-forest-labs/FLUX.1-dev", 0, 0, false),
            (1024, 1024)
        );
        assert_eq!(
            resolve_image_job_size("some-org/edit-pipe", 0, 0, true),
            (0, 0)
        );
        assert_eq!(
            resolve_image_job_size("Qwen/Qwen-Image", 1792, 1024, false),
            (1792, 1024)
        );
    }

    #[test]
    fn enospc_io_error_maps_to_disk_full() {
        let err = std::io::Error::from_raw_os_error(28);
        let mapped = map_image_io_error(err, Path::new("job.json"));
        assert!(format!("{mapped:#}").starts_with("disk_full:"));
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
    fn vulkan_cards_are_image_eligible() {
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
        assert_eq!(image_backend_for_card(&amd), "vulkan");
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
        assert_eq!(image_backend_for_card(&arc), "vulkan");
        let uhd = card(
            "pci-intel:0",
            "integrated",
            "Intel UHD Graphics 770",
            PoolStrategy::Vulkan,
            true,
        );
        assert!(!is_intel_arc_card(&uhd));
        let cpu = card("cpu:0", "cpu", "CPU", PoolStrategy::CpuOnly, false);
        assert!(!image_card_eligible(&cpu));
    }

    #[test]
    fn nvidia_and_metal_cards_are_image_eligible() {
        let nv = card(
            "nvidia:0",
            "discrete",
            "NVIDIA RTX 4090",
            PoolStrategy::Single,
            false,
        );
        assert!(image_card_eligible(&nv));
        assert_eq!(image_backend_for_card(&nv), "cuda");
        let mac = card(
            "metal:0",
            "integrated",
            "Apple M3 Max",
            PoolStrategy::Metal,
            false,
        );
        assert!(image_card_eligible(&mac));
        assert_eq!(image_backend_for_card(&mac), "metal");
    }

    #[test]
    fn last_image_sku_does_not_keep_repo() {
        let catalog = vec![image_sku("img-a", "org/a")];
        let skip = vec!["img-a".to_string()];
        assert!(!keep_image_repo(&catalog, "org/a", &skip, |_| false));
    }

    #[test]
    fn shared_repo_is_kept_for_sibling_image_sku() {
        let catalog = vec![
            image_sku("img-a", "org/shared"),
            image_sku("img-b", "org/shared"),
        ];
        let skip = vec!["img-a".to_string()];
        assert!(keep_image_repo(&catalog, "org/shared", &skip, |id| id == "img-b"));
    }

    #[test]
    fn keep_repo_while_snapshot_is_on_disk() {
        let models =
            std::env::temp_dir().join(format!("scalattice-image-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&models);
        let prev = std::env::var("SCALATTICE_MODELS_DIR").ok();
        std::env::set_var("SCALATTICE_MODELS_DIR", &models);
        let repo = "org/kept-image";
        std::fs::create_dir_all(hub_repo_dir(repo)).unwrap();
        let catalog = vec![
            image_sku("img-a", "org/purged-image"),
            image_sku("img-b", repo),
        ];
        let skip = vec!["img-a".to_string()];
        assert!(keep_image_repo(&catalog, repo, &skip, |_| false));
        assert!(!keep_image_repo(
            &catalog,
            "org/purged-image",
            &skip,
            |_| false
        ));
        let _ = std::fs::remove_dir_all(&models);
        match prev {
            Some(v) => std::env::set_var("SCALATTICE_MODELS_DIR", v),
            None => std::env::remove_var("SCALATTICE_MODELS_DIR"),
        }
    }

    fn touch(hub: &Path, repo: &str, rel: &str) {
        let snap = hub
            .join(format!("models--{}", repo.replace('/', "--")))
            .join("snapshots")
            .join("abc");
        let path = snap.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"weights").unwrap();
    }

    #[test]
    fn checkpoint_ready_needs_every_file() {
        let hub = std::env::temp_dir().join(format!("slt-sd-ready-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&hub);
        let repo = "runwayml/stable-diffusion-v1-5";
        let files = files_for(repo, SdVersion::V1_5);
        let all = files.all();
        assert!(!checkpoint_ready_in(&hub, repo));
        for f in &all[..all.len() - 1] {
            touch(&hub, &f.repo, &f.path);
        }
        assert!(!checkpoint_ready_in(&hub, repo), "missing the last file");
        let last = all[all.len() - 1];
        touch(&hub, &last.repo, &last.path);
        assert!(checkpoint_ready_in(&hub, repo));
        // Tokenizer repo is complete on its own for storage listings.
        assert!(checkpoint_ready_in(&hub, "openai/clip-vit-base-patch32"));
        let _ = std::fs::remove_dir_all(&hub);
    }

    #[test]
    fn unsupported_repo_is_never_ready() {
        let hub = std::env::temp_dir().join("slt-sd-unsupported");
        assert!(!checkpoint_ready_in(&hub, "Qwen/Qwen-Image"));
    }
}
