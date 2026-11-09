//! Catalog Hugging Face repo → Stable Diffusion family, plus the diffusers
//! files stable-diffusion.cpp loads when the catalog names a repo instead of
//! one checkpoint file.

use std::path::{Path, PathBuf};

/// Classic Stable Diffusion families whose diffusers snapshots we can download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdVersion {
    V1_5,
    V2_1,
    Xl,
    Turbo,
}

const TOKENIZER_CLIP_B32: &str = "openai/clip-vit-base-patch32";
const TOKENIZER_CLIP_L14: &str = "openai/clip-vit-large-patch14";
const TOKENIZER_CLIP_BIGG: &str = "laion/CLIP-ViT-bigG-14-laion2B-39B-b160k";
const SDXL_VAE_FP16_FIX: &str = "madebyollin/sdxl-vae-fp16-fix";

impl SdVersion {
    /// Map a catalog repo id (e.g. `stabilityai/sdxl-turbo`) onto a supported
    /// family. Inpainting, refiner, `-base` 2.1 (epsilon) and everything else
    /// return `None`.
    pub fn from_repo(repo: &str) -> Option<Self> {
        let id = repo.trim().to_ascii_lowercase().replace('_', "-");
        if id.is_empty() {
            return None;
        }
        if id.contains("inpaint") || id.contains("refiner") || id.contains("controlnet") {
            return None;
        }
        if id.contains("sdxl-turbo") || id.contains("sd-turbo") {
            return Some(Self::Turbo);
        }
        if id.contains("stable-diffusion-xl-base") || id.ends_with("/sdxl-base-1.0") {
            return Some(Self::Xl);
        }
        if id.contains("stable-diffusion-2-1") || id.contains("stable-diffusion-v2-1") {
            // `stable-diffusion-2-1-base` is epsilon-prediction at 512; the
            // v2.1 preset is the v-prediction 768 checkpoint.
            if id.contains("base") {
                return None;
            }
            return Some(Self::V2_1);
        }
        if id.contains("stable-diffusion-v1-5") || id.contains("stable-diffusion-1-5") {
            return Some(Self::V1_5);
        }
        None
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::V1_5 => "Stable Diffusion 1.5",
            Self::V2_1 => "Stable Diffusion 2.1",
            Self::Xl => "Stable Diffusion XL",
            Self::Turbo => "SDXL-Turbo",
        }
    }

    pub fn is_xl_family(self) -> bool {
        matches!(self, Self::Xl | Self::Turbo)
    }

    /// Native square size in pixels.
    pub fn default_size(self) -> u32 {
        match self {
            Self::V1_5 | Self::Turbo => 512,
            Self::V2_1 => 768,
            Self::Xl => 1024,
        }
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoFile {
    pub repo: String,
    pub path: String,
}

impl RepoFile {
    fn new(repo: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            repo: repo.into(),
            path: path.into(),
        }
    }
}

/// Everything one SD checkpoint needs on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFiles {
    pub tokenizer: RepoFile,
    pub tokenizer2: Option<RepoFile>,
    pub clip: RepoFile,
    pub clip2: Option<RepoFile>,
    pub unet: RepoFile,
    pub vae: RepoFile,
}

impl ModelFiles {
    pub fn all(&self) -> Vec<&RepoFile> {
        let mut out = vec![&self.tokenizer];
        if let Some(t) = &self.tokenizer2 {
            out.push(t);
        }
        out.push(&self.clip);
        if let Some(c) = &self.clip2 {
            out.push(c);
        }
        out.push(&self.unet);
        out.push(&self.vae);
        out
    }
}

/// Files for `repo` (the catalog Hub id). `fp16` picks the half-precision
/// variants used on CUDA / Metal; CPU builds use full-precision files.
pub fn model_files(repo: &str, version: SdVersion, fp16: bool) -> ModelFiles {
    let repo = repo.trim();
    let var = |base: &str| -> String {
        if fp16 {
            base.replace(".safetensors", ".fp16.safetensors")
        } else {
            base.to_string()
        }
    };
    let tokenizer_repo = if version.is_xl_family() {
        TOKENIZER_CLIP_L14
    } else {
        TOKENIZER_CLIP_B32
    };
    let vae = if version.is_xl_family() && fp16 {
        // The stock SDXL VAE overflows in fp16. Use the fp16-fix VAE.
        RepoFile::new(SDXL_VAE_FP16_FIX, "diffusion_pytorch_model.safetensors")
    } else {
        RepoFile::new(repo, var("vae/diffusion_pytorch_model.safetensors"))
    };
    ModelFiles {
        tokenizer: RepoFile::new(tokenizer_repo, "tokenizer.json"),
        tokenizer2: version
            .is_xl_family()
            .then(|| RepoFile::new(TOKENIZER_CLIP_BIGG, "tokenizer.json")),
        clip: RepoFile::new(repo, var("text_encoder/model.safetensors")),
        clip2: version
            .is_xl_family()
            .then(|| RepoFile::new(repo, var("text_encoder_2/model.safetensors"))),
        unet: RepoFile::new(repo, var("unet/diffusion_pytorch_model.safetensors")),
        vae,
    }
}

/// Auxiliary Hub repos that the catalog never names but installs pull in.
/// Returns the files that make that cache directory "complete".
pub fn aux_repo_files(repo: &str) -> Option<&'static [&'static str]> {
    let repo = repo.trim();
    if [TOKENIZER_CLIP_B32, TOKENIZER_CLIP_L14, TOKENIZER_CLIP_BIGG]
        .iter()
        .any(|r| r.eq_ignore_ascii_case(repo))
    {
        return Some(&["tokenizer.json"]);
    }
    if SDXL_VAE_FP16_FIX.eq_ignore_ascii_case(repo) {
        return Some(&["diffusion_pytorch_model.safetensors"]);
    }
    None
}

fn is_real_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

/// Snapshot directories under a Hub repo cache: `refs/*` targets first, then
/// any other `snapshots/*` directory.
fn snapshot_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let snapshots = root.join("snapshots");
    if let Ok(entries) = std::fs::read_dir(root.join("refs")) {
        for entry in entries.flatten() {
            if let Ok(hash) = std::fs::read_to_string(entry.path()) {
                let snap = snapshots.join(hash.trim());
                if snap.is_dir() && !out.contains(&snap) {
                    out.push(snap);
                }
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(&snapshots) {
        for entry in entries.flatten() {
            let snap = entry.path();
            if snap.is_dir() && !out.contains(&snap) {
                out.push(snap);
            }
        }
    }
    out
}

/// Locate `rel` (e.g. `unet/diffusion_pytorch_model.fp16.safetensors`) in a
/// Hub repo cache directory (`models--org--name`). Follows HF symlinks.
pub fn find_cached_file(repo_root: &Path, rel: &str) -> Option<PathBuf> {
    snapshot_dirs(repo_root)
        .into_iter()
        .map(|snap| snap.join(rel))
        .find(|p| is_real_file(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_repos_map_to_sd_versions() {
        assert_eq!(
            SdVersion::from_repo("stabilityai/stable-diffusion-xl-base-1.0"),
            Some(SdVersion::Xl)
        );
        assert_eq!(
            SdVersion::from_repo("stabilityai/sdxl-turbo"),
            Some(SdVersion::Turbo)
        );
        assert_eq!(
            SdVersion::from_repo("stabilityai/stable-diffusion-2-1"),
            Some(SdVersion::V2_1)
        );
        assert_eq!(
            SdVersion::from_repo("runwayml/stable-diffusion-v1-5"),
            Some(SdVersion::V1_5)
        );
        assert_eq!(
            SdVersion::from_repo("stable-diffusion-v1-5/stable-diffusion-v1-5"),
            Some(SdVersion::V1_5)
        );
    }

    #[test]
    fn unsupported_repos_are_none() {
        for repo in [
            "Qwen/Qwen-Image",
            "black-forest-labs/FLUX.1-dev",
            "stabilityai/stable-diffusion-xl-refiner-1.0",
            "stabilityai/stable-diffusion-2-inpainting",
            "stabilityai/stable-diffusion-2-1-base",
            "",
        ] {
            assert_eq!(SdVersion::from_repo(repo), None, "{repo}");
        }
    }

    #[test]
    fn sdxl_fp16_uses_fixed_vae_and_two_encoders() {
        let f = model_files(
            "stabilityai/stable-diffusion-xl-base-1.0",
            SdVersion::Xl,
            true,
        );
        assert_eq!(f.vae.repo, "madebyollin/sdxl-vae-fp16-fix");
        assert_eq!(f.unet.path, "unet/diffusion_pytorch_model.fp16.safetensors");
        assert_eq!(
            f.clip2.as_ref().unwrap().path,
            "text_encoder_2/model.fp16.safetensors"
        );
        assert_eq!(f.tokenizer.repo, "openai/clip-vit-large-patch14");
        assert_eq!(f.all().len(), 6);
    }

    #[test]
    fn sd15_fp32_is_single_encoder() {
        let f = model_files("runwayml/stable-diffusion-v1-5", SdVersion::V1_5, false);
        assert!(f.clip2.is_none() && f.tokenizer2.is_none());
        assert_eq!(f.vae.path, "vae/diffusion_pytorch_model.safetensors");
        assert_eq!(f.tokenizer.repo, "openai/clip-vit-base-patch32");
        assert_eq!(f.all().len(), 4);
    }

    #[test]
    fn find_cached_file_follows_refs() {
        let root = std::env::temp_dir().join(format!("slt-sd-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let snap = root.join("snapshots").join("abc");
        std::fs::create_dir_all(snap.join("unet")).unwrap();
        std::fs::create_dir_all(root.join("refs")).unwrap();
        std::fs::write(root.join("refs").join("main"), b"abc\n").unwrap();
        assert!(find_cached_file(&root, "unet/x.safetensors").is_none());
        std::fs::write(snap.join("unet").join("x.safetensors"), b"w").unwrap();
        assert_eq!(
            find_cached_file(&root, "unet/x.safetensors").unwrap(),
            snap.join("unet").join("x.safetensors")
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
