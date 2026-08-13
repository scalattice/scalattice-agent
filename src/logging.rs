//! Provider-facing console logs are quiet by default; `--verbose` / `SCALATTICE_VERBOSE`
//! unlock llama.cpp detail on stderr. The on-disk `agent.log` is always full detail and
//! size-capped (active file + one rotated sibling).

use crate::paths::agent_env_path;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing_subscriber::EnvFilter;

/// Soft cap for the active `agent.log`. A single `agent.log.1` backup keeps roughly 2× this.
pub const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
const ROTATE_CHECK_EVERY: u64 = 64 * 1024;

/// True when the process should emit full llama.cpp / GGML detail on the console.
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
    let file_only = crate::service::invoked_by_background_service();
    #[cfg(not(windows))]
    let file_only = false;

    if let Some(file) = open_agent_log_writer() {
        let file = Arc::new(Mutex::new(file));
        // Disk always gets full detail; stderr may be simplified via the tee.
        let filter = build_env_filter(true);
        let writer = TeeLogWriter {
            file: Some(file),
            also_stderr: !file_only,
            stderr_verbose: verbose || file_only,
        };
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter(filter)
            .with_writer(Mutex::new(writer))
            .init();
        return;
    }

    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(build_env_filter(verbose))
        .with_writer(std::io::stderr)
        .init();
}

/// Slot workers speak JSON on stdout — keep tracing off stdout.
/// Disk `agent.log` always gets full llama.cpp detail (same contract as the supervisor);
/// stderr stays Simplified unless `--verbose` / `SCALATTICE_VERBOSE`.
pub fn init_worker_logging(verbose: bool) {
    // Always capture llama-cpp-2 INFO on disk; do not inherit a parent `RUST_LOG=warn`.
    let filter = build_env_filter(true);
    if let Some(file) = open_agent_log_writer() {
        let file = Arc::new(Mutex::new(file));
        let writer = TeeLogWriter {
            file: Some(file),
            also_stderr: true,
            stderr_verbose: verbose,
        };
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter(filter)
            .with_writer(Mutex::new(writer))
            .init();
        return;
    }

    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

fn open_agent_log_writer() -> Option<CappedLogWriter> {
    let path = crate::paths::agent_log_path().ok()?;
    CappedLogWriter::open(path).ok()
}

/// Writes every event to the capped log file; optionally mirrors to stderr with Simplified filter.
struct TeeLogWriter {
    file: Option<Arc<Mutex<CappedLogWriter>>>,
    also_stderr: bool,
    stderr_verbose: bool,
}

impl Write for TeeLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        crate::cloud_log::ingest_log_chunk(buf);

        if let Some(file) = &self.file {
            if let Ok(mut guard) = file.lock() {
                let _ = guard.write_all(buf);
            }
        }

        if self.also_stderr {
            let text = std::str::from_utf8(buf).unwrap_or("");
            let skip = !self.stderr_verbose
                && text
                    .lines()
                    .any(|line| !line.is_empty() && is_verbose_only_log_line(line));
            if !skip {
                let _ = io::stderr().write_all(buf);
            }
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = &self.file {
            if let Ok(mut guard) = file.lock() {
                let _ = guard.flush();
            }
        }
        if self.also_stderr {
            let _ = io::stderr().flush();
        }
        Ok(())
    }
}

/// Append-only writer that rotates `agent.log` → `agent.log.1` when over [`MAX_LOG_BYTES`].
struct CappedLogWriter {
    path: PathBuf,
    file: Option<File>,
    bytes_since_check: u64,
}

impl CappedLogWriter {
    fn open(path: PathBuf) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let mut writer = Self {
            path,
            file: Some(file),
            // Force a size check on the first write so a leftover oversized file rotates.
            bytes_since_check: ROTATE_CHECK_EVERY,
        };
        writer.rotate_if_needed()?;
        Ok(writer)
    }

    fn rotate_if_needed(&mut self) -> io::Result<()> {
        let len = self
            .file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map(|m| m.len())
            .unwrap_or(0);
        self.bytes_since_check = 0;
        if len < MAX_LOG_BYTES {
            return Ok(());
        }
        if let Some(mut file) = self.file.take() {
            let _ = file.flush();
            drop(file);
        }
        let backup = rotated_log_path(&self.path);
        rotate_file(&self.path, &backup)?;
        self.file = Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?,
        );
        Ok(())
    }

    fn file_mut(&mut self) -> io::Result<&mut File> {
        self.file.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "agent log file handle is closed")
        })
    }
}

fn rotate_file(path: &Path, backup: &Path) -> io::Result<()> {
    let _ = std::fs::remove_file(backup);
    match std::fs::rename(path, backup) {
        Ok(()) => Ok(()),
        Err(err) => match OpenOptions::new().write(true).truncate(true).open(path) {
            Ok(_) => Ok(()),
            Err(_) => Err(err),
        },
    }
}

fn rotated_log_path(path: &Path) -> PathBuf {
    let mut backup = path.as_os_str().to_owned();
    backup.push(".1");
    PathBuf::from(backup)
}

impl Write for CappedLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes_since_check = self
            .bytes_since_check
            .saturating_add(buf.len() as u64);
        if self.bytes_since_check >= ROTATE_CHECK_EVERY {
            self.rotate_if_needed()?;
        }
        self.file_mut()?.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file_mut()?.flush()
    }
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
    use std::io::{BufRead, BufReader};
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

    #[test]
    fn rotated_path_appends_dot_one() {
        let p = PathBuf::from("/tmp/scalattice/agent.log");
        assert_eq!(
            rotated_log_path(&p),
            PathBuf::from("/tmp/scalattice/agent.log.1")
        );
    }

    #[test]
    fn capped_writer_rotates_when_over_limit() {
        let dir = std::env::temp_dir().join(format!(
            "scalattice-log-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.log");

        {
            let mut f = File::create(&path).unwrap();
            let chunk = vec![b'x'; 1024];
            let target = MAX_LOG_BYTES + 1024;
            let mut written = 0u64;
            while written < target {
                f.write_all(&chunk).unwrap();
                written += chunk.len() as u64;
            }
            f.flush().unwrap();
        }

        let mut writer = CappedLogWriter::open(path.clone()).unwrap();
        writer.write_all(b"fresh\n").unwrap();
        writer.flush().unwrap();

        assert!(rotated_log_path(&path).is_file());
        let active = std::fs::read_to_string(&path).unwrap();
        assert!(active.contains("fresh"));
        assert!(std::fs::metadata(&path).unwrap().len() < MAX_LOG_BYTES);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
