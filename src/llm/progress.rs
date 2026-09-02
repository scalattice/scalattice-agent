//! Work-tied progress for GGUF load / prefill / decode.
//!
//! The supervisor treats silence on this channel as a dead worker. Reports must
//! come from the thread that is actually loading or generating: never a sidecar
//! timer, which would hide a CUDA hang.

use std::cell::RefCell;
use std::time::{Duration, Instant};

thread_local! {
    static SINK: RefCell<Option<Box<dyn FnMut(&str, f32)>>> = RefCell::new(None);
    static LAST: RefCell<Option<(String, Instant, f32)>> = RefCell::new(None);
}

/// Install a progress sink for the current thread while `f` runs.
pub fn with_sink<R>(sink: impl FnMut(&str, f32) + 'static, f: impl FnOnce() -> R) -> R {
    SINK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(sink));
    });
    LAST.with(|slot| *slot.borrow_mut() = None);
    let out = f();
    SINK.with(|slot| {
        *slot.borrow_mut() = None;
    });
    out
}

/// llama.cpp load callback: progress in `0.0..=1.0`. Returning true continues the load.
pub fn llama_load_callback(progress: f32) -> bool {
    report("load", progress);
    true
}

pub fn report(phase: &str, pct: f32) {
    let pct = pct.clamp(0.0, 1.0);
    let now = Instant::now();
    let emit = LAST.with(|slot| {
        let mut guard = slot.borrow_mut();
        match guard.as_mut() {
            None => {
                *guard = Some((phase.to_string(), now, pct));
                true
            }
            Some((last_phase, last_at, last_pct)) => {
                let phase_changed = last_phase != phase;
                let moved = (pct - *last_pct).abs() >= 0.02;
                let heartbeat = now.duration_since(*last_at) >= Duration::from_millis(400);
                let edge = pct <= 0.001 || pct >= 0.999;
                if phase_changed || moved || heartbeat || edge {
                    *last_phase = phase.to_string();
                    *last_at = now;
                    *last_pct = pct;
                    true
                } else {
                    false
                }
            }
        }
    });
    if !emit {
        return;
    }
    SINK.with(|slot| {
        if let Some(cb) = slot.borrow_mut().as_mut() {
            cb(phase, pct);
        }
    });
}

pub fn attach_llama_progress(
    params: llama_cpp_2::model::params::LlamaModelParams,
) -> llama_cpp_2::model::params::LlamaModelParams {
    params.with_progress_callback(llama_load_callback)
}
