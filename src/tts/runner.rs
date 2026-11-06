//! Speech on the slot the server assigned.
//!
//! GGUF voices (Qwen3-TTS, Pocket TTS, Soprano, LFM3-Audio, Chatterbox, Drama)
//! go to `llama-tts` from llama.cpp. That program is not linked into the agent.

use super::{TtsJob, TtsOutput, MAX_INPUT_CHARS};
use crate::compute_pool::{PoolStrategy, VirtualCard};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const WALL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// llama.cpp `llama-tts`. Uses the slot GPU.
    Llama,
}

pub fn occupies_gpu(model: &crate::protocol::CatalogModel) -> bool {
    engine_for(model) == Some(Engine::Llama)
}

pub fn runtime_available(model: &crate::protocol::CatalogModel) -> bool {
    if super::stub_enabled() {
        return true;
    }
    match engine_for(model) {
        Some(Engine::Llama) => llama_tts_binary().is_some(),
        None => false,
    }
}

pub fn engine_for(model: &crate::protocol::CatalogModel) -> Option<Engine> {
    let name = primary_filename(model)?;
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".gguf") {
        return Some(Engine::Llama);
    }
    None
}

fn primary_filename(model: &crate::protocol::CatalogModel) -> Option<String> {
    model
        .weights
        .as_ref()
        .map(|w| w.filename.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn llama_tts_binary() -> Option<PathBuf> {
    find_binary("SCALATTICE_LLAMA_TTS_BIN", &["llama-tts", "llama-tts.exe"])
}

fn find_binary(env_key: &str, names: &[&str]) -> Option<PathBuf> {
    if let Ok(raw) = std::env::var(env_key) {
        let path = PathBuf::from(raw.trim());
        if path.is_file() {
            return Some(path);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    for name in names {
        for candidate in [
            dir.join(name),
            dir.join("tts").join(name),
            dir.join("sd").join(name),
        ] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Synthesize. `card` / `cuda_visible` are the claimed slot. llama-tts uses them.
/// `on_chunk` receives WAV bytes as each window finishes (header, then PCM).
pub async fn synthesize(
    job: TtsJob,
    card: &VirtualCard,
    cuda_visible: &[u32],
    mut on_chunk: Option<&mut (dyn FnMut(&[u8]) + Send)>,
) -> Result<TtsOutput> {
    let characters = job.text.chars().count();
    if job.text.trim().is_empty() {
        anyhow::bail!("prompt_too_long: tts input is empty");
    }
    if characters > MAX_INPUT_CHARS {
        return Err(crate::invoke_code::coded(
            crate::invoke_code::InvokeErrorCode::PromptTooLong,
            format!("tts input is {characters} characters (max {MAX_INPUT_CHARS})"),
        ));
    }
    if super::stub_enabled() {
        let wav = super::wav::encode_pcm16_mono(&vec![0.0f32; 2205], 22_050);
        if let Some(cb) = on_chunk.as_mut() {
            cb(&wav);
        }
        return Ok(TtsOutput {
            wav,
            seconds: 0.1,
            characters: characters as u32,
            sample_rate: 22_050,
        });
    }
    match engine_for(&job.model) {
        Some(Engine::Llama) => llama_tts(&job, card, cuda_visible, characters, on_chunk).await,
        None => Err(crate::invoke_code::coded(
            crate::invoke_code::InvokeErrorCode::TtsRuntimeMissing,
            "speech models are GGUF and run through llama-tts".to_string(),
        )),
    }
}

async fn llama_tts(
    job: &TtsJob,
    card: &VirtualCard,
    cuda_visible: &[u32],
    characters: usize,
    on_chunk: Option<&mut (dyn FnMut(&[u8]) + Send)>,
) -> Result<TtsOutput> {
    let binary = llama_tts_binary().ok_or_else(|| {
        crate::invoke_code::coded(
            crate::invoke_code::InvokeErrorCode::TtsRuntimeMissing,
            "llama.cpp TTS runner is not installed (llama-tts)".to_string(),
        )
    })?;
    let model = gguf_model(&job.model).ok_or_else(|| {
        crate::invoke_code::coded(
            crate::invoke_code::InvokeErrorCode::ModelNotInstalled,
            "tts gguf is not on disk".to_string(),
        )
    })?;
    let dir = tempfile_dir("scalattice-tts")?;
    let output = dir.join("out.wav");
    let mut cmd = Command::new(&binary);
    cmd.arg("-m").arg(&model);
    cmd.arg("-p").arg(&job.text);
    if on_chunk.is_some() {
        cmd.arg("--stream");
    }
    cmd.arg("--output").arg(&output);
    if let Some(mmproj) = mmproj_beside(&model, &job.model) {
        cmd.arg("-mm").arg(mmproj);
    }
    if !job.voice.trim().is_empty() {
        cmd.arg("--tts-voice").arg(job.voice.trim());
    }
    if let Some(speaker) = speaker_file(&job.model) {
        cmd.arg("--tts-speaker-file").arg(speaker);
    }
    crate::image::sdcpp::apply_slot_env(&mut cmd, card.strategy, cuda_visible);
    // A CPU pin still has to hide the GPUs, matching the chat worker.
    if matches!(card.strategy, PoolStrategy::CpuOnly) {
        cmd.arg("-ngl").arg("0");
    }
    apply_lib_path(&mut cmd, &binary);
    let wav = if on_chunk.is_some() {
        run_stream(&mut cmd, &dir, "llama-tts", on_chunk).await?
    } else {
        run_and_read(&mut cmd, &output, &dir, "llama-tts").await?
    };
    let seconds = wav_seconds(&wav).unwrap_or(0.0);
    Ok(TtsOutput {
        wav,
        seconds,
        characters: characters as u32,
        sample_rate: 0,
    })
}

fn gguf_model(model: &crate::protocol::CatalogModel) -> Option<PathBuf> {
    let weights = model.weights.as_ref()?;
    let runtime = {
        let runtime = model.runtime_model.trim();
        if runtime.is_empty() {
            model.model_id.as_str()
        } else {
            runtime
        }
    };
    let path = crate::models::target_gguf_path(runtime, &weights.filename);
    path.is_file().then_some(path)
}

fn mmproj_beside(model_path: &Path, model: &crate::protocol::CatalogModel) -> Option<PathBuf> {
    if let Some(weights) = model.weights.as_ref() {
        let runtime = {
            let runtime = model.runtime_model.trim();
            if runtime.is_empty() {
                model.model_id.as_str()
            } else {
                runtime
            }
        };
        for extra in &weights.companion_filenames {
            let lower = extra.to_ascii_lowercase();
            if lower.contains("mmproj") {
                let path = crate::models::target_gguf_path(runtime, extra);
                if path.is_file() {
                    return Some(path);
                }
            }
        }
    }
    crate::models::resolve_mmproj(model_path)
}

fn speaker_file(model: &crate::protocol::CatalogModel) -> Option<PathBuf> {
    companion_ending(model, &[".wav", ".mp3", ".flac"])
}

fn locate_file(model: &crate::protocol::CatalogModel, rel: &str) -> Option<PathBuf> {
    let flat = crate::models::target_gguf_path(runtime_of(model), rel);
    flat.is_file().then_some(flat)
}

fn companion_ending(model: &crate::protocol::CatalogModel, suffixes: &[&str]) -> Option<PathBuf> {
    let weights = model.weights.as_ref()?;
    for extra in &weights.companion_filenames {
        let lower = extra.to_ascii_lowercase();
        if suffixes.iter().any(|suffix| lower.ends_with(suffix)) {
            if let Some(path) = locate_file(model, extra) {
                return Some(path);
            }
        }
    }
    None
}

fn runtime_of(model: &crate::protocol::CatalogModel) -> &str {
    let runtime = model.runtime_model.trim();
    if runtime.is_empty() {
        model.model_id.as_str()
    } else {
        runtime
    }
}

async fn run_stream(
    cmd: &mut Command,
    dir: &Path,
    what: &str,
    mut on_chunk: Option<&mut (dyn FnMut(&[u8]) + Send)>,
) -> Result<Vec<u8>> {
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("tts_runtime_missing: failed to start {what}"))?;
    let mut stdout = child.stdout.take().context(format!("{what} has no stdout"))?;
    let mut stderr = child.stderr.take();
    let log_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(pipe) = stderr.as_mut() {
            let _ = pipe.read_to_end(&mut buf).await;
        }
        buf
    });
    let deadline = tokio::time::Instant::now() + WALL;
    let mut wav = Vec::new();
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        let n = tokio::time::timeout_at(deadline, stdout.read(&mut buf))
            .await
            .context(format!("{what} timed out"))?
            .context(format!("{what} stdout"))?;
        if n == 0 {
            break;
        }
        if let Some(cb) = on_chunk.as_mut() {
            cb(&buf[..n]);
        }
        wav.extend_from_slice(&buf[..n]);
    }
    let status = child
        .wait()
        .await
        .with_context(|| format!("{what} wait"))?;
    let logs = String::from_utf8_lossy(&log_task.await.unwrap_or_default()).to_string();
    if !status.success() || wav.len() < 44 {
        let _ = std::fs::remove_dir_all(dir);
        let tail: String = logs.chars().rev().take(1500).collect::<String>().chars().rev().collect();
        return Err(crate::invoke_code::coded(
            crate::invoke_code::InvokeErrorCode::InferenceFailed,
            format!("{what} failed: {tail}"),
        ));
    }
    let _ = std::fs::remove_dir_all(dir);
    Ok(wav)
}

async fn run_and_read(
    cmd: &mut Command,
    output: &Path,
    dir: &Path,
    what: &str,
) -> Result<Vec<u8>> {
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("tts_runtime_missing: failed to start {what}"))?;
    let mut stderr = child.stderr.take();
    let log_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(pipe) = stderr.as_mut() {
            let _ = pipe.read_to_end(&mut buf).await;
        }
        buf
    });
    let status = tokio::time::timeout(WALL, child.wait())
        .await
        .context(format!("{what} timed out"))??;
    let logs = String::from_utf8_lossy(&log_task.await.unwrap_or_default()).to_string();
    if !status.success() || !output.is_file() {
        let _ = std::fs::remove_dir_all(dir);
        let tail: String = logs.chars().rev().take(1500).collect::<String>().chars().rev().collect();
        return Err(crate::invoke_code::coded(
            crate::invoke_code::InvokeErrorCode::InferenceFailed,
            format!("{what} failed: {tail}"),
        ));
    }
    let wav = std::fs::read(output).with_context(|| format!("read {}", output.display()))?;
    let _ = std::fs::remove_dir_all(dir);
    Ok(wav)
}

fn apply_lib_path(cmd: &mut Command, binary: &Path) {
    let Some(lib) = binary.parent().map(|dir| dir.join("lib")) else {
        return;
    };
    if !lib.is_dir() {
        return;
    }
    let lib = lib.to_string_lossy().to_string();
    for key in ["LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH"] {
        let prev = std::env::var(key).unwrap_or_default();
        if prev.is_empty() {
            cmd.env(key, &lib);
        } else {
            cmd.env(key, format!("{lib}:{prev}"));
        }
    }
}

fn tempfile_dir(prefix: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn wav_seconds(wav: &[u8]) -> Option<f64> {
    if wav.len() < 44 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return None;
    }
    let mut offset = 12usize;
    let mut rate = 0u32;
    let mut channels = 0u16;
    let mut bits = 0u16;
    let mut data_bytes = 0u32;
    while offset + 8 <= wav.len() {
        let id = &wav[offset..offset + 4];
        let size = u32::from_le_bytes(wav[offset + 4..offset + 8].try_into().ok()?) as usize;
        let body = offset + 8;
        if id == b"fmt " && body + 16 <= wav.len() {
            channels = u16::from_le_bytes(wav[body + 2..body + 4].try_into().ok()?);
            rate = u32::from_le_bytes(wav[body + 4..body + 8].try_into().ok()?);
            bits = u16::from_le_bytes(wav[body + 14..body + 16].try_into().ok()?);
        } else if id == b"data" {
            data_bytes = size as u32;
            break;
        }
        offset = body + size + (size % 2);
    }
    if rate == 0 || channels == 0 || bits == 0 {
        return None;
    }
    let frame = channels as u64 * (bits as u64 / 8);
    if frame == 0 {
        return None;
    }
    Some(data_bytes as f64 / frame as f64 / rate as f64)
}
