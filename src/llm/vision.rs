//! llama.cpp mtmd path: load mmproj, turn inlined images into embeddings, prefill.

use crate::compute_pool::{PoolStrategy, VirtualCard};
use crate::models::resolve_mmproj;
use crate::protocol::{messages_have_images, ChatImage, ChatMessage};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose, Engine};
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::mtmd::{
    mtmd_default_marker, MtmdBitmap, MtmdContext, MtmdContextParams, MtmdInputChunk, MtmdInputText,
};
use std::path::Path;

/// llama-cpp-2 0.1.154 stores the C pointer as the first field of these wrappers.
unsafe fn c_ptr<T>(wrapper: &T) -> *mut std::ffi::c_void {
    *(wrapper as *const T as *const *mut std::ffi::c_void)
}

unsafe extern "C" {
    fn mtmd_helper_eval_chunk_single(
        ctx: *mut std::ffi::c_void,
        lctx: *mut std::ffi::c_void,
        chunk: *const std::ffi::c_void,
        n_past: i32,
        seq_id: i32,
        n_batch: i32,
        logits_last: bool,
        new_n_past: *mut i32,
    ) -> i32;
}

fn eval_one_chunk(
    mtmd: &MtmdContext,
    llama_ctx: &LlamaContext<'_>,
    chunk: &MtmdInputChunk,
    n_past: i32,
    logits_last: bool,
) -> Result<i32> {
    let mut new_n_past = n_past;
    let rc = unsafe {
        mtmd_helper_eval_chunk_single(
            c_ptr(mtmd),
            c_ptr(llama_ctx),
            c_ptr(chunk),
            n_past,
            0,
            64,
            logits_last,
            &mut new_n_past,
        )
    };
    if rc != 0 {
        anyhow::bail!("mtmd eval image/text chunk failed ({rc})");
    }
    Ok(new_n_past)
}

pub fn collect_images(messages: &[ChatMessage]) -> Vec<&ChatImage> {
    messages
        .iter()
        .flat_map(|m| m.images.iter())
        .filter(|img| !img.data.trim().is_empty())
        .collect()
}

pub fn prompt_needs_vision(messages: &[ChatMessage]) -> bool {
    messages_have_images(messages)
}

pub fn content_with_media_markers(message: &ChatMessage) -> String {
    let marker = mtmd_default_marker();
    let mut text = message.content.clone();
    let existing = text.matches(marker).count();
    let need = message
        .images
        .iter()
        .filter(|img| !img.data.trim().is_empty())
        .count();
    for _ in existing..need {
        if !text.is_empty() && !text.ends_with(char::is_whitespace) {
            text.push(' ');
        }
        text.push_str(marker);
    }
    text
}

pub fn init_mtmd_for_model(
    model: &LlamaModel,
    model_path: &Path,
    pool: &VirtualCard,
) -> Result<MtmdContext> {
    let path = resolve_mmproj(model_path).ok_or_else(|| {
        anyhow!(
            "image input needs an mmproj companion next to {}: enable the catalog companion and wait for download",
            model_path.display()
        )
    })?;
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("mmproj path is not valid UTF-8: {}", path.display()))?;
    let mut params = MtmdContextParams::default();
    params.use_gpu = !matches!(pool.strategy, PoolStrategy::CpuOnly);
    params.print_timings = false;
    super::progress::report("load", 0.0);
    let ctx = MtmdContext::init_from_file(path_str, model, &params)
        .with_context(|| format!("init mmproj {}", path.display()))?;
    super::progress::report("load", 1.0);
    Ok(ctx)
}

fn decode_image_bytes(image: &ChatImage) -> Result<Vec<u8>> {
    let data = image.data.trim();
    if data.starts_with("http://") || data.starts_with("https://") {
        anyhow::bail!("image URLs must be inlined as base64 before they reach the agent");
    }
    let payload = if let Some(img) = crate::protocol::chat_image_from_data_url(data) {
        img.data
    } else {
        data.to_string()
    };
    general_purpose::STANDARD
        .decode(payload.as_bytes())
        .or_else(|_| general_purpose::STANDARD_NO_PAD.decode(payload.as_bytes()))
        .context("decode image base64")
}

pub fn prefill_vision(
    _model: &LlamaModel,
    mtmd: &MtmdContext,
    ctx: &LlamaContext,
    prompt: &str,
    messages: &[ChatMessage],
    add_special: bool,
) -> Result<(u32, i32)> {
    if !mtmd.support_vision() {
        anyhow::bail!("loaded mmproj does not advertise vision support");
    }

    let images = collect_images(messages);
    let mut bitmaps = Vec::with_capacity(images.len());
    for (i, image) in images.iter().enumerate() {
        super::progress::report(
            "prefill",
            0.05 * (i as f32 / images.len().max(1) as f32),
        );
        let bytes = decode_image_bytes(image)?;
        if bytes.len() > 12 * 1024 * 1024 {
            anyhow::bail!("image exceeds 12 MB decoded");
        }
        let bitmap = MtmdBitmap::from_buffer(mtmd, &bytes, false).map_err(|err| {
            // Stable client-input code: router/backend must not treat as operator fault.
            anyhow!("invalid_image: {err}")
        })?;
        bitmaps.push(bitmap);
    }
    let bitmap_refs: Vec<&MtmdBitmap> = bitmaps.iter().collect();

    super::progress::report("prefill", 0.1);
    let chunks = mtmd
        .tokenize(
            MtmdInputText {
                text: prompt.to_string(),
                add_special,
                parse_special: true,
            },
            &bitmap_refs,
        )
        .context("mtmd tokenize (media markers must match image count)")?;

    let prompt_tokens = chunks.total_tokens() as u32;
    let n_pos = chunks.total_positions();
    if prompt_tokens == 0 {
        anyhow::bail!("mtmd produced an empty prompt");
    }
    if prompt_tokens as usize + 1 > ctx.n_ctx() as usize {
        anyhow::bail!(
            "vision prompt too long for context window ({} tokens > {})",
            prompt_tokens,
            ctx.n_ctx()
        );
    }

    let n_chunks = chunks.len();
    if n_chunks == 0 {
        anyhow::bail!("mtmd produced no chunks");
    }
    // One llama.cpp call per chunk (text decode or image encode). Ping between
    // them so a Mac chewing photos is not silent for the whole prefill.
    let mut n_past = 0i32;
    for i in 0..n_chunks {
        let chunk = chunks.get(i).context("mtmd chunk missing")?;
        super::progress::report("prefill", 0.1 + 0.8 * (i as f32 / n_chunks as f32));
        n_past = eval_one_chunk(mtmd, ctx, &chunk, n_past, i + 1 == n_chunks)?;
    }
    super::progress::report("prefill", 1.0);

    let next_pos = if n_past > 0 { n_past } else { n_pos };
    Ok((prompt_tokens, next_pos))
}
