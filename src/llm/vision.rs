//! llama.cpp mtmd path: load mmproj, turn inlined images into embeddings, prefill.

use crate::compute_pool::{PoolStrategy, VirtualCard};
use crate::models::resolve_mmproj;
use crate::protocol::{messages_have_images, ChatImage, ChatMessage};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose, Engine};
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::mtmd::{
    mtmd_default_marker, MtmdBitmap, MtmdContext, MtmdContextParams, MtmdInputText,
};
use std::path::Path;

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
    MtmdContext::init_from_file(path_str, model, &params)
        .with_context(|| format!("init mmproj {}", path.display()))
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
    for image in &images {
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

    super::progress::report("prefill", 0.0);
    let n_past = chunks
        .eval_chunks(mtmd, ctx, 0, 0, 64, true)
        .context("mtmd eval image + text chunks")?;
    super::progress::report("prefill", 1.0);

    let next_pos = if n_past > 0 { n_past } else { n_pos };
    Ok((prompt_tokens, next_pos))
}
