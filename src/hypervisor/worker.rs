use super::ipc::{WorkerBootConfig, WorkerRequest, WorkerResponse};
use crate::compute_pool::apply_slot_backend_visibility;
use crate::llm::{
    evict_all, generate_with_callback, init_backend, preload_model, GenerateConfig,
};
use crate::models::{list_cached_runtime_models, resolve_model_gguf};
use crate::protocol::InvokeTimings;
use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};

/// Entry point for `scalattice-agent worker` — owns one slot's llama backend.
pub fn run_worker(config_json: &str) -> Result<()> {
    let boot: WorkerBootConfig =
        serde_json::from_str(config_json).context("parse worker boot config")?;
    apply_slot_backend_visibility(boot.card.strategy, &boot.cuda_visible);
    info!(
        slot = %boot.slot_id,
        strategy = ?boot.card.strategy,
        cuda_visible = ?boot.cuda_visible,
        "compute worker starting"
    );

    if let Err(err) = init_backend() {
        warn!(slot = %boot.slot_id, error = %err, "worker llama backend init failed");
    }

    let busy = Arc::new(AtomicBool::new(false));
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = std::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).context("worker stdin read")?;
        if n == 0 {
            break;
        }
        let req: WorkerRequest = match serde_json::from_str(line.trim()) {
            Ok(r) => r,
            Err(err) => {
                let resp = WorkerResponse::Error {
                    id: "unknown".into(),
                    error: format!("bad request: {err}"),
                };
                write_response(&mut stdout, &resp)?;
                continue;
            }
        };
        let req_id = request_id(&req);
        match handle_request(&boot, req, &busy, &mut stdout) {
            Ok(()) => {}
            Err(err) => {
                let resp = WorkerResponse::Error {
                    id: req_id,
                    error: format!("{err:#}"),
                };
                let _ = write_response(&mut stdout, &resp);
            }
        }
    }
    Ok(())
}

fn request_id(req: &WorkerRequest) -> String {
    match req {
        WorkerRequest::Ping { id }
        | WorkerRequest::Warm { id, .. }
        | WorkerRequest::Invoke { id, .. }
        | WorkerRequest::Evict { id }
        | WorkerRequest::Health { id }
        | WorkerRequest::Shutdown { id } => id.clone(),
    }
}

fn write_response(out: &mut impl Write, resp: &WorkerResponse) -> Result<()> {
    serde_json::to_writer(&mut *out, resp)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn handle_request(
    boot: &WorkerBootConfig,
    req: WorkerRequest,
    busy: &AtomicBool,
    stdout: &mut impl Write,
) -> Result<()> {
    match req {
        WorkerRequest::Ping { id } => write_response(stdout, &WorkerResponse::Pong { id }),
        WorkerRequest::Shutdown { id } => {
            evict_all();
            write_response(stdout, &WorkerResponse::Ok { id })
        }
        WorkerRequest::Evict { id } => {
            evict_all();
            write_response(stdout, &WorkerResponse::Ok { id })
        }
        WorkerRequest::Health { id } => write_response(
            stdout,
            &WorkerResponse::Health {
                id,
                ready: true,
                loaded_models: list_cached_runtime_models(),
                busy: busy.load(Ordering::Relaxed),
            },
        ),
        WorkerRequest::Warm { id, runtime_model } => {
            let Some(path) = resolve_model_gguf(&runtime_model) else {
                return write_response(
                    stdout,
                    &WorkerResponse::Error {
                        id,
                        error: format!("weights not found for {runtime_model}"),
                    },
                );
            };
            match preload_model(&path, &boot.card) {
                Ok(()) => write_response(stdout, &WorkerResponse::Ok { id }),
                Err(err) => write_response(
                    stdout,
                    &WorkerResponse::Error {
                        id,
                        error: format!("{err:#}"),
                    },
                ),
            }
        }
        WorkerRequest::Invoke {
            id,
            job_id: _,
            model_id,
            runtime_model,
            messages,
            max_tokens,
            stream,
        } => {
            busy.store(true, Ordering::Relaxed);
            let result = run_invoke(
                boot,
                &id,
                &model_id,
                &runtime_model,
                messages,
                max_tokens,
                stream,
                stdout,
            );
            busy.store(false, Ordering::Relaxed);
            result
        }
    }
}

fn run_invoke(
    boot: &WorkerBootConfig,
    id: &str,
    model_id: &str,
    runtime_model: &str,
    messages: Vec<crate::protocol::ChatMessage>,
    max_tokens: u32,
    stream: bool,
    stdout: &mut impl Write,
) -> Result<()> {
    let job_id = id.to_string();
    let output = crate::llm::with_work_progress(
        {
            let job_id = job_id.clone();
            move |phase, pct| {
                let mut out = std::io::stdout();
                let _ = write_response(
                    &mut out,
                    &WorkerResponse::Progress {
                        id: job_id.clone(),
                        phase: phase.to_string(),
                        pct: Some(pct),
                    },
                );
            }
        },
        || {
            crate::llm::report_work_progress("resolve", 0.0);
            let started = Instant::now();
            info!(
                slot = %boot.slot_id,
                model = %runtime_model,
                "worker invoke accepted"
            );
            let model_path = resolve_model_gguf(runtime_model).with_context(|| {
                format!("model weights not found for {runtime_model}")
            })?;
            info!(
                slot = %boot.slot_id,
                elapsed_ms = started.elapsed().as_millis() as u64,
                path = %model_path.display(),
                "worker resolved gguf"
            );
            crate::llm::report_work_progress("resolve", 1.0);
            let config = GenerateConfig {
                model_path,
                pool: boot.card.clone(),
                messages,
                max_tokens: max_tokens.max(1).min(8192),
                model_id: model_id.to_string(),
            };
            crate::llm::report_work_progress("start", 0.0);
            generate_with_callback(&config, |piece| {
                if piece.is_empty() {
                    return;
                }
                if stream {
                    let _ = write_response(
                        stdout,
                        &WorkerResponse::Delta {
                            id: job_id.clone(),
                            text: piece.to_string(),
                        },
                    );
                }
            })
        },
    );

    match output {
        Ok(out) => write_response(
            stdout,
            &WorkerResponse::Result {
                id: id.to_string(),
                content: out.content,
                prompt_tokens: out.prompt_tokens,
                completion_tokens: out.completion_tokens,
                timings: InvokeTimings {
                    model_load_ms: Some(out.timings.model_load_ms),
                    prefill_ms: Some(out.timings.prefill_ms),
                    decode_ms: Some(out.timings.decode_ms),
                    total_ms: Some(out.timings.total_ms),
                },
                loaded_models: list_cached_runtime_models(),
            },
        ),
        Err(err) => write_response(
            stdout,
            &WorkerResponse::Error {
                id: id.to_string(),
                error: format!("{err:#}"),
            },
        ),
    }
}
