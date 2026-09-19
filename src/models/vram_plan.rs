//! Memory plan for a llama.cpp load: weights + KV(n_ctx) + compute + overhead.
//! **Fit policy lives on scalattice-server** (`visionBudget.ts`). This module is
//! the load-time GGUF safety net and the fallback when a catalog row predates
//! `kvCacheGb` / `gpuFullVramGb` / `gpuWeightsVramGb`.
//!
//! Image-gen SKUs do not use this plan (no token KV). Chat and VL do.
//! Catalog `max_context_tokens` is the real window; we do not cap at 4096.

use super::gguf_arch::GgufShape;
use crate::protocol::CatalogModel;

/// CUDA context, allocator fragmentation, driver reservation.
pub const CUDA_RUNTIME_OVERHEAD_GB: f64 = 0.40;
/// Qwen3-8B GQA KV at 4k / fp16: catalog fallback scales from this.
const KV_REF_GB_8B_4K: f64 = 0.56;
const WEIGHT_REF_GB: f64 = 4.7;
/// Reference window for the KV scaling formula only — not a live cap.
pub const KV_REF_N_CTX: u32 = 4096;
pub const FALLBACK_TEXT_N_CTX: u32 = 4096;
pub const FALLBACK_VISION_N_CTX: u32 = 8192;
/// Prefill is chunked; scratch tracks batch, not the full window.
const SCRATCH_BATCH_TOKENS: u32 = 2048;

/// Catalog window the agent must allocate. Image-gen has no llama context.
pub fn job_n_ctx(model: &CatalogModel, need_vision: bool) -> u32 {
    if model.is_image_job() {
        return 0;
    }
    if model.max_context_tokens > 0 {
        return model.max_context_tokens;
    }
    if need_vision || model.vision_model {
        FALLBACK_VISION_N_CTX
    } else {
        FALLBACK_TEXT_N_CTX
    }
}

/// fp16 K+V cache GiB from GGUF shape.
pub fn kv_cache_gb(shape: GgufShape, n_ctx: u32) -> f64 {
    if n_ctx == 0 {
        return 0.0;
    }
    let bytes = 2.0
        * f64::from(shape.n_layer)
        * f64::from(n_ctx.max(1))
        * f64::from(shape.n_head_kv.max(1))
        * f64::from(shape.head_dim().max(1))
        * 2.0;
    bytes / (1024.0 * 1024.0 * 1024.0)
}

fn layer_scale(weight_gb: f64) -> f64 {
    (weight_gb / WEIGHT_REF_GB).max(0.25).powf(0.45)
}

/// KV when GGUF shape is unknown: scale Qwen3-8B 4k GQA by weight and n_ctx.
pub fn kv_gb_for_weight(weight_gb: f64, n_ctx: u32) -> f64 {
    if n_ctx == 0 {
        return 0.0;
    }
    KV_REF_GB_8B_4K * layer_scale(weight_gb.max(0.0)) * (f64::from(n_ctx) / f64::from(KV_REF_N_CTX))
}

pub fn kv_gb(weight_gb: f64, shape: Option<GgufShape>, n_ctx: u32) -> f64 {
    match shape.filter(|s| s.usable()) {
        Some(s) => kv_cache_gb(s, n_ctx),
        None => kv_gb_for_weight(weight_gb, n_ctx),
    }
}

/// ggml compute graph / scratch for prefill+decode (batch-sized, not full n_ctx).
pub fn compute_scratch_gb(shape: GgufShape, n_ctx: u32) -> f64 {
    let batch = n_ctx.max(1).min(SCRATCH_BATCH_TOKENS);
    0.25 + f64::from(shape.n_layer) * f64::from(batch) / 180_000.0
}

fn compute_scratch_for_weight(weight_gb: f64, n_ctx: u32) -> f64 {
    let batch = n_ctx.max(1).min(SCRATCH_BATCH_TOKENS);
    0.25 + 36.0 * layer_scale(weight_gb.max(0.0)) * f64::from(batch) / 180_000.0
}

/// Scratch + CUDA overhead, no KV. GPU still needs this when KV lives in RAM.
pub fn extras_without_kv_gb(weight_gb: f64, shape: Option<GgufShape>, n_ctx: u32) -> f64 {
    let scratch = match shape.filter(|s| s.usable()) {
        Some(s) => compute_scratch_gb(s, n_ctx),
        None => compute_scratch_for_weight(weight_gb, n_ctx),
    };
    scratch + CUDA_RUNTIME_OVERHEAD_GB
}

/// When GGUF metadata is missing, scale Qwen3-8B extras by weight and context.
pub fn catalog_runtime_extra_gb(weight_gb: f64, n_ctx: u32) -> f64 {
    kv_gb_for_weight(weight_gb, n_ctx) + extras_without_kv_gb(weight_gb, None, n_ctx)
}

/// Weights + KV + scratch on one device (fast path).
pub fn full_host_need_gb(weight_gb: f64, shape: Option<GgufShape>, n_ctx: u32) -> f64 {
    let w = weight_gb.max(0.0);
    if n_ctx == 0 {
        return w;
    }
    if w <= 0.05 && shape.is_none() {
        return catalog_runtime_extra_gb(0.0, n_ctx);
    }
    w + kv_gb(w, shape, n_ctx) + extras_without_kv_gb(w, shape, n_ctx)
}

pub fn full_host_need_from_weight(weight_gb: f64, n_ctx: u32) -> f64 {
    full_host_need_gb(weight_gb, None, n_ctx)
}

/// Weights + scratch on GPU; KV is elsewhere (system RAM).
pub fn gpu_weights_need_gb(weight_gb: f64, shape: Option<GgufShape>, n_ctx: u32) -> f64 {
    let w = weight_gb.max(0.0);
    w + extras_without_kv_gb(w, shape, n_ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qwen_shape() -> GgufShape {
        GgufShape {
            n_layer: 36,
            n_embd: 4096,
            n_head: 32,
            n_head_kv: 8,
        }
    }

    #[test]
    fn qwen3_8b_kv_is_half_gig_at_4k() {
        let qwen = qwen_shape();
        assert!((kv_cache_gb(qwen, 4096) - 0.5625).abs() < 0.01);
        let need = full_host_need_gb(4.68, Some(qwen), 4096);
        assert!(need > 6.0, "{need}");
        assert!(need < 8.0, "{need}");
        assert!(need > 6.0 + 0.05, "6 GB card must not count as gpu-full");
    }

    #[test]
    fn qwen3_8b_kv_scales_to_about_four_and_a_half_gb_at_32k() {
        let kv = kv_cache_gb(qwen_shape(), 32768);
        assert!((kv - 4.5).abs() < 0.05, "{kv}");
        let gpu_full = full_host_need_gb(4.68, Some(qwen_shape()), 32768);
        assert!(gpu_full > 9.0, "{gpu_full}");
        let weights_only = gpu_weights_need_gb(4.68, Some(qwen_shape()), 32768);
        assert!(weights_only < 7.0, "{weights_only}");
        assert!(gpu_full - weights_only > 4.0);
    }

    #[test]
    fn large_model_extra_exceeds_flat_two_gb_fence() {
        let need = full_host_need_from_weight(40.0, 4096);
        assert!(
            need > 42.0,
            "70B-class must not use a +2 GB constant, got {need}"
        );
    }

    #[test]
    fn eight_gb_card_still_full_hosts_5gb_weights() {
        let need = full_host_need_from_weight(5.0, 4096);
        assert!(need <= 8.05, "{need}");
    }

    #[test]
    fn job_n_ctx_uses_catalog_not_a_4k_cap() {
        let mut m = CatalogModel {
            model_id: "qwen-3-8b".into(),
            display_name: String::new(),
            runtime_model: String::new(),
            job_kind: String::new(),
            chat_template: String::new(),
            thinking: String::new(),
            usd_per_image: 0.0,
            image_max_n: 0,
            max_context_tokens: 32768,
            regions: vec![],
            weight_size_gb: None,
            min_vram_gb: None,
            min_vram_gb_vision: None,
            vision_model: false,
            text_sibling_model_id: None,
            min_ram_gb: None,
            kv_cache_gb: None,
            gpu_full_vram_gb: None,
            gpu_weights_vram_gb: None,
            mmproj_size_gb: None,
            vision_max_images: None,
            vision_max_image_side_px: None,
            vision_max_image_pixels: None,
            weights: None,
        };
        assert_eq!(job_n_ctx(&m, false), 32768);
        m.job_kind = "image".into();
        assert_eq!(job_n_ctx(&m, false), 0);
    }
}
