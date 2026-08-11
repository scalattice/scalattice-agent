//! In-memory ring of recent log lines for optional cloud streaming.
//! Only forwarded while the dashboard has an active logs subscription.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Lines retained locally for the next subscribe snapshot (router keeps ~20).
pub const LOCAL_RING_CAP: usize = 24;
const MAX_LINE_CHARS: usize = 500;

#[derive(Debug, Clone)]
pub struct CloudLogLine {
    pub ts_ms: u64,
    pub level: String,
    pub msg: String,
}

struct CloudLogState {
    lines: VecDeque<CloudLogLine>,
    /// Monotonic sequence for subscribers.
    next_seq: u64,
    /// True while the cloud asked us to stream.
    streaming: bool,
    /// Lines queued since last flush (only while streaming).
    pending: VecDeque<CloudLogLine>,
}

static STATE: OnceLock<Mutex<CloudLogState>> = OnceLock::new();

fn state() -> &'static Mutex<CloudLogState> {
    STATE.get_or_init(|| {
        Mutex::new(CloudLogState {
            lines: VecDeque::with_capacity(LOCAL_RING_CAP),
            next_seq: 1,
            streaming: false,
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

/// Called from the logging tee for each written chunk (may contain multiple lines).
pub fn ingest_log_chunk(buf: &[u8]) {
    let Ok(text) = std::str::from_utf8(buf) else {
        return;
    };
    let Ok(mut guard) = state().lock() else {
        return;
    };
    for raw in text.split('\n') {
        let msg = clean_msg(raw);
        if msg.is_empty() {
            continue;
        }
        // Skip ultra-noisy verbose-only lines from the cloud stream.
        if crate::logging::is_verbose_only_log_line(&msg) {
            continue;
        }
        let line = CloudLogLine {
            ts_ms: now_ms(),
            level: guess_level(&msg).to_string(),
            msg,
        };
        if guard.lines.len() >= LOCAL_RING_CAP {
            guard.lines.pop_front();
        }
        guard.lines.push_back(line.clone());
        guard.next_seq = guard.next_seq.saturating_add(1);
        if guard.streaming {
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
        }
    }
}

pub fn is_streaming() -> bool {
    state()
        .lock()
        .map(|g| g.streaming)
        .unwrap_or(false)
}

/// Snapshot of the local ring (oldest → newest).
pub fn snapshot() -> Vec<CloudLogLine> {
    state()
        .lock()
        .map(|g| g.lines.iter().cloned().collect())
        .unwrap_or_default()
}

/// Drain pending lines since last flush (while streaming).
pub fn drain_pending() -> Vec<CloudLogLine> {
    state()
        .lock()
        .map(|mut g| g.pending.drain(..).collect())
        .unwrap_or_default()
}
