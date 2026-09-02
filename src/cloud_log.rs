//! In-memory ring of recent log lines for optional cloud streaming.
//! Only forwarded while the dashboard has an active logs subscription.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Lines retained for dashboard subscribe snapshots + live pending flush.
/// Must stay ahead of a llama.cpp load dump; tiny caps made Verbose useless.
pub const LOCAL_RING_CAP: usize = 2500;
const MAX_LINE_CHARS: usize = 2000;

#[derive(Debug, Clone)]
pub struct CloudLogLine {
    pub ts_ms: u64,
    pub level: String,
    pub msg: String,
}

struct CloudLogState {
    /// Simplified ring (no llama-cpp / ggml noise).
    lines: VecDeque<CloudLogLine>,
    /// Full ring including verbose-only lines (for dashboard verbose mode).
    lines_verbose: VecDeque<CloudLogLine>,
    /// Monotonic sequence for subscribers.
    next_seq: u64,
    /// True while the cloud asked us to stream.
    streaming: bool,
    /// When true, pending + snapshot use the verbose ring.
    streaming_verbose: bool,
    /// Lines queued since last flush (only while streaming).
    pending: VecDeque<CloudLogLine>,
}

static STATE: OnceLock<Mutex<CloudLogState>> = OnceLock::new();

fn state() -> &'static Mutex<CloudLogState> {
    STATE.get_or_init(|| {
        Mutex::new(CloudLogState {
            lines: VecDeque::with_capacity(LOCAL_RING_CAP),
            lines_verbose: VecDeque::with_capacity(LOCAL_RING_CAP),
            next_seq: 1,
            streaming: false,
            streaming_verbose: false,
            pending: VecDeque::new(),
        })
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn guess_level(line: &str) -> &'static str {
    let upper = line.to_ascii_uppercase();
    if upper.contains(" ERROR") || upper.contains("ERROR ") || upper.contains("ERROR:") {
        "error"
    } else if upper.contains(" WARN") || upper.contains("WARN ") || upper.contains("WARN:") {
        "warn"
    } else if upper.contains(" DEBUG") || upper.contains("DEBUG ") {
        "debug"
    } else {
        "info"
    }
}

/// Peel one `tracing` default fmt layer: `2026-…Z  INFO target: rest`.
fn peel_one_tracing_layer(s: &str) -> Option<(&'static str, &str)> {
    let s = s.trim();
    // ISO-8601 timestamp ending in Z (with optional fractional seconds).
    let z = s.find('Z')?;
    if z < 19 || z > 40 {
        return None;
    }
    if !s.as_bytes().get(4).is_some_and(|b| *b == b'-') || !s[..z].contains('T') {
        return None;
    }
    let after = s[z + 1..].trim_start();
    let (level, rest) = if let Some(r) = after.strip_prefix("ERROR ") {
        ("error", r)
    } else if let Some(r) = after.strip_prefix("WARN ") {
        ("warn", r)
    } else if let Some(r) = after.strip_prefix("INFO ") {
        ("info", r)
    } else if let Some(r) = after.strip_prefix("DEBUG ") {
        ("debug", r)
    } else if let Some(r) = after.strip_prefix("TRACE ") {
        ("trace", r)
    } else {
        return None;
    };
    Some((level, rest.trim_start()))
}

/// Drop `crate::module::path: ` so live logs show the message body only.
fn strip_rust_target_prefix(s: &str) -> &str {
    let Some(idx) = s.find(": ") else {
        return s;
    };
    let target = &s[..idx];
    // Keep llama-style prefixes like `load_from_file:` (no `::`).
    if target.contains("::") || target.starts_with("scalattice") {
        return s[idx + 2..].trim_start();
    }
    s
}

/// Collapse nested worker→supervisor tracing wraps into `(level, message)`.
pub fn normalize_tracing_message(raw: &str) -> (String, String) {
    let mut level = guess_level(raw).to_string();
    let mut rest = raw.trim().to_string();
    for _ in 0..6 {
        let Some((lvl, body)) = peel_one_tracing_layer(&rest) else {
            break;
        };
        level = lvl.to_string();
        let next = strip_rust_target_prefix(body).to_string();
        if next == rest {
            break;
        }
        rest = next;
    }
    // Drop a trailing duplicate `slot=…` when the supervisor re-tagged a worker line.
    if let Some((a, b)) = rest.rsplit_once(" slot=") {
        if let Some((_, first_slot)) = a.rsplit_once(" slot=") {
            if first_slot.split_whitespace().next()
                == Some(b.split_whitespace().next().unwrap_or(""))
            {
                rest = a.to_string();
            }
        }
    }
    (level, rest)
}

fn clean_msg(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // Strip ANSI if any slipped through.
    let mut out = String::with_capacity(trimmed.len().min(MAX_LINE_CHARS));
    let mut chars = trimmed.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // skip CSI sequence
            while let Some(n) = chars.next() {
                if ('a'..='z').contains(&n) || ('A'..='Z').contains(&n) {
                    break;
                }
            }
            continue;
        }
        if c == '\r' {
            continue;
        }
        out.push(c);
        if out.len() >= MAX_LINE_CHARS {
            out.push('…');
            break;
        }
    }
    out
}

fn push_ring(ring: &mut VecDeque<CloudLogLine>, line: CloudLogLine) {
    if ring.len() >= LOCAL_RING_CAP {
        ring.pop_front();
    }
    ring.push_back(line);
}

fn forward_to_stream(msg: &str, verbose: bool) -> bool {
    verbose || !crate::logging::is_verbose_only_log_line(msg)
}

/// Called from the logging tee for each written chunk (may contain multiple lines).
pub fn ingest_log_chunk(buf: &[u8]) {
    let Ok(text) = std::str::from_utf8(buf) else {
        return;
    };
    let Ok(mut guard) = state().lock() else {
        return;
    };
    for raw in text.split('\n') {
        let cleaned = clean_msg(raw);
        if cleaned.is_empty() {
            continue;
        }
        let (level, msg) = normalize_tracing_message(&cleaned);
        if msg.is_empty() {
            continue;
        }
        let line = CloudLogLine {
            ts_ms: now_ms(),
            level,
            msg: msg.clone(),
        };
        if forward_to_stream(&msg, false) {
            push_ring(&mut guard.lines, line.clone());
        }
        push_ring(&mut guard.lines_verbose, line.clone());
        guard.next_seq = guard.next_seq.saturating_add(1);
        if guard.streaming && forward_to_stream(&msg, guard.streaming_verbose) {
            if guard.pending.len() >= LOCAL_RING_CAP {
                guard.pending.pop_front();
            }
            guard.pending.push_back(line);
        }
    }
}

pub fn set_streaming(on: bool) {
    if let Ok(mut guard) = state().lock() {
        guard.streaming = on;
        if !on {
            guard.pending.clear();
            guard.streaming_verbose = false;
            crate::logging::set_dashboard_verbose(false);
        }
    }
}

pub fn set_streaming_verbose(verbose: bool) {
    crate::logging::set_dashboard_verbose(verbose);
    if let Ok(mut guard) = state().lock() {
        guard.streaming_verbose = verbose;
        guard.pending.clear();
    }
}

pub fn is_streaming() -> bool {
    state().lock().map(|g| g.streaming).unwrap_or(false)
}

/// Snapshot of the local ring (oldest → newest).
pub fn snapshot(verbose: bool) -> Vec<CloudLogLine> {
    state()
        .lock()
        .map(|g| {
            let ring = if verbose { &g.lines_verbose } else { &g.lines };
            ring.iter().cloned().collect()
        })
        .unwrap_or_default()
}

/// Drain pending lines since last flush (while streaming).
pub fn drain_pending() -> Vec<CloudLogLine> {
    state()
        .lock()
        .map(|mut g| g.pending.drain(..).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbose_snapshot_includes_llama_lines() {
        let sample = b"agent ready\nllama-cpp-2 module=ggml info\n";
        ingest_log_chunk(sample);
        let simple = snapshot(false);
        let verbose = snapshot(true);
        assert_eq!(simple.len(), 1);
        assert!(simple[0].msg.contains("agent ready"));
        assert_eq!(verbose.len(), 2);
        assert!(verbose.iter().any(|l| l.msg.contains("llama-cpp-2")));
    }

    #[test]
    fn normalize_strips_nested_worker_supervisor_wrap() {
        let raw = "2026-08-20T20:26:32.700132Z  INFO scalattice_agent::supervisor::host: 2026-08-20T20:26:32.699996Z  INFO load_from_file: llama-cpp-2: file size = 4.83 GiB slot=cuda-0";
        let (level, msg) = normalize_tracing_message(raw);
        assert_eq!(level, "info");
        assert_eq!(
            msg,
            "load_from_file: llama-cpp-2: file size = 4.83 GiB slot=cuda-0"
        );
    }

    #[test]
    fn normalize_strips_agent_target() {
        let raw = "2026-08-20T20:26:45.868670Z  INFO scalattice_agent::agent: invoke 053e8f25 completed slot=cuda-0";
        let (level, msg) = normalize_tracing_message(raw);
        assert_eq!(level, "info");
        assert_eq!(msg, "invoke 053e8f25 completed slot=cuda-0");
    }
}
