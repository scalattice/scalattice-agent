//! Provider-facing logs are quiet by default; `--verbose` / `SCALATTICE_VERBOSE` unlock llama.cpp detail.

use crate::paths::agent_env_path;
use tracing_subscriber::EnvFilter;

/// True when the process should emit full llama.cpp / GGML detail.
pub fn verbose_requested(cli_verbose: bool) -> bool {
    if cli_verbose {
        return true;
    }
    if env_truthy("SCALATTICE_VERBOSE") {
        return true;
    }
    agent_env_truthy("SCALATTICE_VERBOSE")
}

pub fn init_logging(verbose: bool) {
    #[cfg(windows)]
    if crate::service::invoked_by_background_service() {
        // Always keep full detail on disk so the tray Verbose toggle can reveal
        // llama.cpp lines without restarting the agent. Simplified is display-only.
        let filter = build_env_filter(true);
        if let Ok(path) = crate::paths::agent_log_path() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                tracing_subscriber::fmt()
                    .with_ansi(false)
                    .with_env_filter(filter)
                    .with_writer(std::sync::Mutex::new(file))
                    .init();
                return;
            }
        }
    }

    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(build_env_filter(verbose))
        .init();
}

/// Slot workers speak JSON on stdout — keep all logs on stderr so IPC stays clean.
pub fn init_worker_logging(verbose: bool) {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(build_env_filter(verbose))
        .with_writer(std::io::stderr)
        .init();
}

fn build_env_filter(verbose: bool) -> EnvFilter {
    let mut filter = EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into());
    if !verbose {
        // llama-cpp-2 routes GGML / print_info dumps at INFO; keep WARN+ for real failures.
        if let Ok(dir) = "llama-cpp-2=warn".parse() {
            filter = filter.add_directive(dir);
        }
    }
    filter
}

/// Lines that belong only in the Verbose live-log view (and `--verbose` agent output).
pub fn is_verbose_only_log_line(line: &str) -> bool {
    line.contains("llama-cpp-2")
        || line.contains("module=\"llama.cpp")
        || line.contains("module=\"ggml")
        || line.contains(" module=llama.cpp")
        || line.contains(" module=ggml")
}

/// Stream lines from a log source to stdout, optionally Simplified.
pub fn pipe_log_lines(reader: impl std::io::Read, verbose: bool) -> anyhow::Result<()> {
    use std::io::{BufRead, BufReader, Write};
    let reader = BufReader::new(reader);
    let mut out = std::io::stdout().lock();
    for line in reader.lines() {
        let line = line?;
        if !verbose && is_verbose_only_log_line(&line) {
            continue;
        }
        writeln!(out, "{line}")?;
        let _ = out.flush();
    }
    Ok(())
}

/// Filter a multi-line log chunk for the Simplified view (Windows tray Live log).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn simplify_log_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() / 2);
    for line in raw.lines() {
        if is_verbose_only_log_line(line) {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    if raw.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out
}

fn env_truthy(key: &str) -> bool {
    match std::env::var(key) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

fn agent_env_truthy(key: &str) -> bool {
    let Ok(path) = agent_env_path() else {
        return false;
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let assignment = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        let Some((k, value)) = assignment.split_once('=') else {
            continue;
        };
        if k.trim() != key {
            continue;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'');
        return matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        );
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_llama_noise() {
        assert!(is_verbose_only_log_line(
            "INFO llama-cpp-2: BOS token = 1 module=\"llama.cpp::print_info\""
        ));
        assert!(!is_verbose_only_log_line(
            "INFO scalattice_agent::agent: invoke abc · model qwen-3-8b"
        ));
    }

    #[test]
    fn simplify_keeps_agent_lines() {
        let raw = "\
INFO scalattice_agent::agent: connected
INFO llama-cpp-2: n_ctx = 4096 module=\"llama.cpp::llama_context\"
INFO scalattice_agent::llm::model_cache: context OOM; reloading via 'gpu-offload-reduced'
";
        let simple = simplify_log_text(raw);
        assert!(simple.contains("connected"));
        assert!(simple.contains("context OOM"));
        assert!(!simple.contains("n_ctx"));
    }
}
