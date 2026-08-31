//! Memory plan for a llama.cpp GPU-full load: weights + KV(n_ctx) + compute +
//! CUDA overhead. Keep the catalog fallback in lockstep with
//! `visionBudget.ts` `runtimeFullHostVramGb`.

use super::gguf_arch::GgufShape;
use crate::protocol::CatalogModel;

/// CUDA context, allocator fragmentation, driver reservation.
pub const CUDA_RUNTIME_OVERHEAD_GB: f64 = 0.40;
/// Qwen3-8B GQA KV at 4k / fp16 — catalog fallback scales from this.
const KV_REF_GB_8B_4K: f64 = 0.56;
const WEIGHT_REF_GB: f64 = 4.7;
const TEXT_N_CTX: u32 = 4096;
const VISION_N_CTX: u32 = 8192;

pub fn job_n_ctx(model: &CatalogModel, need_vision: bool) -> u32 {
    let default = if need_vision { VISION_N_CTX } else { TEXT_N_CTX };
    let cap = model.max_context_tokens;
    if cap == 0 {
        default
    } else {
        default.min(cap)
    }
}

/// fp16 K+V cache GiB from GGUF shape.
pub fn kv_cache_gb(shape: GgufShape, n_ctx: u32) -> f64 {
    let bytes = 2.0
        * f64::from(shape.n_layer)
        * f64::from(n_ctx.max(1))
        * f64::from(shape.n_head_kv.max(1))
        * f64::from(shape.head_dim().max(1))
        * 2.0;
    bytes / (1024.0 * 1024.0 * 1024.0)
}

/// Worst-case ggml CUDA compute graph / scratch for prefill+decode.
pub fn compute_scratch_gb(shape: GgufShape, n_ctx: u32) -> f64 {
    0.25 + f64::from(shape.n_layer) * f64::from(n_ctx.max(1)) / 180_000.0
}

/// When GGUF metadata is missing, scale Qwen3-8B extras by weight and context.
pub fn catalog_runtime_extra_gb(weight_gb: f64, n_ctx: u32) -> f64 {
    let layer_scale = (weight_gb / WEIGHT_REF_GB).max(0.25).powf(0.45);
    let kv = KV_REF_GB_8B_4K * layer_scale * (f64::from(n_ctx.max(1)) / f64::from(TEXT_N_CTX));
    let compute = 0.25 + 36.0 * layer_scale * f64::from(n_ctx.max(1)) / 180_000.0;
    kv + compute + CUDA_RUNTIME_OVERHEAD_GB
}

pub fn full_host_need_gb(weight_gb: f64, shape: Option<GgufShape>, n_ctx: u32) -> f64 {
    let w = weight_gb.max(0.0);
    if w <= 0.05 && shape.is_none() {
        return catalog_runtime_extra_gb(0.0, n_ctx);
    }
    let extra = match shape.filter(|s| s.usable()) {
        Some(s) => kv_cache_gb(s, n_ctx) + compute_scratch_gb(s, n_ctx) + CUDA_RUNTIME_OVERHEAD_GB,
        None => catalog_runtime_extra_gb(w, n_ctx),
    };
    w + extra
}

pub fn full_host_need_from_weight(weight_gb: f64, n_ctx: u32) -> f64 {
    full_host_need_gb(weight_gb, None, n_ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwen3_8b_kv_is_half_gig_at_4k() {
        let qwen = GgufShape {
            n_layer: 36,
            n_embd: 4096,
            n_head: 32,
            n_head_kv: 8,
        };
        assert!((kv_cache_gb(qwen, 4096) - 0.5625).abs() < 0.01);
        let need = full_host_need_gb(4.68, Some(qwen), 4096);
        assert!(need > 6.0, "{need}");
        assert!(need < 8.0, "{need}");
        assert!(need > 6.0 + 0.05, "6 GB card must not count as gpu-full");
    }

    #[test]
    fn large_model_extra_exceeds_flat_two_gb_fence() {
        let need = full_host_need_from_weight(40.0, 4096);
        assert!(need > 42.0, "70B-class must not use a +2 GB constant, got {need}");
    }

    #[test]
    fn eight_gb_card_still_full_hosts_5gb_weights() {
        let need = full_host_need_from_weight(5.0, 4096);
        assert!(need <= 8.05, "{need}");
    }
}
