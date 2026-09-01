use crate::paths::{agent_env_path, config_dir};
use anyhow::{bail, Context, Result};
use std::env;
use std::path::Path;

/// Hard-coded Scalattice Cloud WebSocket endpoint (not configurable).
pub const SCALATTICE_WS_URL: &str = "wss://api.scalattice.cloud/v1/operators/agent/ws";

/// Hard-coded Scalattice Cloud HTTPS API base for agent control calls.
pub const SCALATTICE_API_BASE: &str = "https://api.scalattice.cloud/v1/operators/agent";

/// Loopback-only release HTTP base (`SCALATTICE_UPDATE_BASE`). Production stays
/// on Scalattice Cloud; CI update smoke points this at a local mock.
pub fn loopback_http_override(key: &str) -> Option<String> {
    loopback_override(key, &["http"])
}

/// Loopback-only WebSocket URL (`SCALATTICE_WS_URL`). Rejects anything that is
/// not `ws://127.0.0.1|localhost|::1` so a compromised env cannot redirect the
/// fleet to an attacker.
pub fn agent_ws_url() -> String {
    loopback_override("SCALATTICE_WS_URL", &["ws"])
        .unwrap_or_else(|| SCALATTICE_WS_URL.to_string())
}

/// True when this process is the CI update smoke (isolated HOME, mock Cloud).
pub fn update_smoke_test() -> bool {
    matches!(
        env_or_agent_file("SCALATTICE_UPDATE_SMOKE")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
}

fn loopback_override(key: &str, schemes: &[&str]) -> Option<String> {
    let raw = env_or_agent_file(key)?;
    is_loopback_url(&raw, schemes).then(|| raw.trim().trim_end_matches('/').to_string())
}

fn env_or_agent_file(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| read_key_from_config_files(key))
}

fn is_loopback_url(url: &str, schemes: &[&str]) -> bool {
    let Ok(parsed) = url::Url::parse(url.trim()) else {
        return false;
    };
    if !schemes.iter().any(|s| parsed.scheme().eq_ignore_ascii_case(s)) {
        return false;
    }
    matches!(
        parsed.host_str(),
        Some("127.0.0.1") | Some("localhost") | Some("::1")
    )
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub token: String,
}

fn trim_env_line(line: &str) -> &str {
    line.trim().trim_start_matches('\u{feff}').trim()
}

#[cfg(test)]
fn parse_token_from_env_file(raw: &str) -> Option<String> {
    parse_env_key(raw, "SCALATTICE_AGENT_TOKEN")
}

/// Decode a config/env file that may be UTF-8, UTF-8 BOM, or Windows UTF-16
/// (Notepad "Unicode", PowerShell 5.1 `Set-Content`, some installer writes).
pub fn decode_text_bytes(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return decode_utf16(&bytes[2..], true);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return decode_utf16(&bytes[2..], false);
    }
    let rest = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    if looks_like_utf16_le(rest) {
        return decode_utf16(rest, true);
    }
    String::from_utf8_lossy(rest).into_owned()
}

/// True when on-disk bytes are not canonical UTF-8 (no BOM) of `decoded`.
/// Used to rewrite `agent.env` after reading a UTF-16/BOM file.
pub fn text_file_needs_utf8_rewrite(bytes: &[u8], decoded: &str) -> bool {
    std::str::from_utf8(bytes).ok() != Some(decoded)
}

fn decode_utf16(bytes: &[u8], little: bool) -> String {
    let mut units = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let unit = if little {
            u16::from_le_bytes([chunk[0], chunk[1]])
        } else {
            u16::from_be_bytes([chunk[0], chunk[1]])
        };
        units.push(unit);
    }
    String::from_utf16_lossy(&units)
}

/// ASCII stored as UTF-16 LE is `XX 00 XX 00 …`. Avoid treating normal UTF-8 as UTF-16.
fn looks_like_utf16_le(bytes: &[u8]) -> bool {
    if bytes.len() < 8 || bytes.len() % 2 != 0 {
        return false;
    }
    let pairs = bytes.len() / 2;
    let nul_high = bytes.chunks_exact(2).filter(|c| c[1] == 0).count();
    nul_high * 4 >= pairs * 3
}

pub fn read_text_file_lossy(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(decode_text_bytes(&bytes))
}

pub fn token_snippet(token: &str) -> String {
    let trimmed = token.trim();
    if trimmed.len() <= 20 {
        return trimmed.to_string();
    }
    format!("{}…{}", &trimmed[..20], &trimmed[trimmed.len().saturating_sub(4)..])
}

pub fn read_saved_agent_token() -> Option<String> {
    read_token_from_config_files()
}

/// Token for interactive/CLI use: saved file, then process environment.
pub fn resolve_agent_token(cli: Option<String>) -> Option<String> {
    cli.filter(|t| !t.trim().is_empty())
        .or_else(read_saved_agent_token)
        .or_else(|| {
            env::var("SCALATTICE_AGENT_TOKEN")
                .ok()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
        })
}

fn read_token_from_config_files() -> Option<String> {
    read_key_from_config_files("SCALATTICE_AGENT_TOKEN")
}

fn read_key_from_config_files(key: &str) -> Option<String> {
    let mut paths = Vec::new();
    if let Ok(config) = config_dir() {
        paths.push(config.join("agent.env"));
        paths.push(config.join("agent.systemd.env"));
    }
    if let Ok(path) = agent_env_path() {
        if !paths.iter().any(|p| p == &path) {
            paths.push(path);
        }
    }

    for path in paths {
        let Ok(raw) = read_text_file_lossy(&path) else {
            continue;
        };
        if let Some(value) = parse_env_key(&raw, key) {
            return Some(value);
        }
    }

    None
}

fn parse_env_key(raw: &str, want_key: &str) -> Option<String> {
    for line in raw.lines() {
        let trimmed = trim_env_line(line);
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let assignment = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        let Some((key, value)) = assignment.split_once('=') else {
            continue;
        };
        if trim_env_line(key) != want_key {
            continue;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'');
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

impl AgentConfig {
    pub fn from_env_and_cli(token: Option<String>) -> Result<Self> {
        let token = resolve_agent_token(token)
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .context(
                "Set SCALATTICE_AGENT_TOKEN, pass --token, or run: scalattice-agent set-token --token slt_provider_...",
            )?;

        if !token.starts_with("slt_provider_") {
            bail!("Agent token must start with slt_provider_ (create one on Scalattice Cloud → Providers)");
        }

        Ok(Self { token })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16_le(text: &str, bom: bool) -> Vec<u8> {
        let mut out = Vec::new();
        if bom {
            out.extend_from_slice(&[0xFF, 0xFE]);
        }
        for unit in text.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out
    }

    #[test]
    fn reads_utf8_token() {
        let raw = "SCALATTICE_AGENT_TOKEN=slt_provider_abc123\n";
        assert_eq!(
            parse_token_from_env_file(&decode_text_bytes(raw.as_bytes())).as_deref(),
            Some("slt_provider_abc123")
        );
    }

    #[test]
    fn reads_utf8_bom_token() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"SCALATTICE_AGENT_TOKEN=slt_provider_bom\n");
        assert_eq!(
            parse_token_from_env_file(&decode_text_bytes(&bytes)).as_deref(),
            Some("slt_provider_bom")
        );
    }

    #[test]
    fn reads_utf16_le_bom_token() {
        // Notepad "Unicode" / Windows PowerShell 5.1 Set-Content.
        let bytes = utf16_le("SCALATTICE_AGENT_TOKEN=slt_provider_wide\r\n", true);
        assert_eq!(
            parse_token_from_env_file(&decode_text_bytes(&bytes)).as_deref(),
            Some("slt_provider_wide")
        );
        assert!(text_file_needs_utf8_rewrite(
            &bytes,
            &decode_text_bytes(&bytes)
        ));
    }

    #[test]
    fn reads_utf16_le_without_bom() {
        let bytes = utf16_le("SCALATTICE_AGENT_TOKEN=slt_provider_nobom\n", false);
        assert_eq!(
            parse_token_from_env_file(&decode_text_bytes(&bytes)).as_deref(),
            Some("slt_provider_nobom")
        );
    }

    #[test]
    fn utf8_does_not_look_like_utf16() {
        let raw = b"SCALATTICE_AGENT_TOKEN=slt_provider_plain\n";
        let decoded = decode_text_bytes(raw);
        assert_eq!(decoded, "SCALATTICE_AGENT_TOKEN=slt_provider_plain\n");
        assert!(!text_file_needs_utf8_rewrite(raw, &decoded));
    }

    #[test]
    fn loopback_http_allows_ipv4() {
        assert!(is_loopback_url("http://127.0.0.1:9/api", &["http"]));
        assert!(is_loopback_url("ws://localhost:9/v1", &["ws"]));
        assert!(is_loopback_url("ws://[::1]:9/v1", &["ws"]));
    }

    #[test]
    fn loopback_rejects_non_loopback_and_tls() {
        assert!(!is_loopback_url(
            "https://127.0.0.1/api",
            &["http"]
        ));
        assert!(!is_loopback_url(
            "wss://127.0.0.1/ws",
            &["ws"]
        ));
        assert!(!is_loopback_url(
            "http://scalattice.cloud/api",
            &["http"]
        ));
        assert!(!is_loopback_url(
            "http://127.0.0.1.attacker.example/x",
            &["http"]
        ));
        assert!(!is_loopback_url("not-a-url", &["http"]));
    }
}
