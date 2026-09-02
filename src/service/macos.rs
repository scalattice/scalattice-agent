use crate::config::AgentConfig;
use crate::paths::resolve_agent_binary;
use crate::service::BackgroundStatus;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const LABEL: &str = "com.scalattice.agent";
const UPDATE_LABEL: &str = "com.scalattice.agent.update";
const TRAY_LABEL: &str = "com.scalattice.agent.tray";

pub fn background_status() -> BackgroundStatus {
    if !launch_agent_plist_path_home()
        .map(|p| p.is_file())
        .unwrap_or(false)
    {
        return BackgroundStatus::NotInstalled;
    }
    if service_active() {
        BackgroundStatus::Running
    } else {
        BackgroundStatus::Stopped
    }
}

pub fn start_background_from_config(config: &AgentConfig) -> Result<()> {
    ensure_service_running(config)
}

pub fn restart_background_from_config(config: &AgentConfig) -> Result<()> {
    let _ = crate::service::persist_agent_token(&config.token)?;
    write_launch_agent()?;
    reload_launch_agent()?;
    verify_service_active()
}

pub fn invoked_by_systemd() -> bool {
    invoked_by_background_service()
}

pub fn invoked_by_background_service() -> bool {
    std::env::var("SCALATTICE_LAUNCHD").ok().as_deref() == Some("1")
        || std::env::var("XPC_SERVICE_NAME")
            .map(|v| v.contains(LABEL))
            .unwrap_or(false)
}

pub fn background_service_available() -> bool {
    Path::new("/bin/launchctl").is_file() || which("launchctl")
}

fn which(name: &str) -> bool {
    Command::new(name)
        .arg("--help")
        .output()
        .map(|o| o.status.success() || o.status.code().is_some())
        .unwrap_or(false)
}

pub fn service_active() -> bool {
    launchctl_print_running(LABEL)
}

pub fn follow_service_logs(verbose: bool) -> Result<()> {
    if !service_active() {
        bail!(
            "scalattice-agent is not running; save your token with `scalattice-agent set-token` first"
        );
    }
    let log_path = crate::paths::agent_log_path()?;
    if !log_path.is_file() {
        bail!("log file not found at {}", log_path.display());
    }
    let mut child = Command::new("tail")
        .args(["-n", "30", "-F"])
        .arg(&log_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to tail {}", log_path.display()))?;
    let stdout = child.stdout.take().context("tail stdout missing")?;
    crate::logging::pipe_log_lines(stdout, verbose)?;
    match child.wait().context("wait for log tail")?.code() {
        Some(0) | Some(130) => Ok(()),
        Some(code) => bail!("log tail exited with status {code}"),
        None => bail!("log tail terminated by signal"),
    }
}

pub fn sync_background_env() -> Result<()> {
    if launch_agent_plist_path()?.is_file() {
        write_launch_agent()?;
    }
    Ok(())
}

pub fn remove_background_service() -> Result<()> {
    let uid = user_id();
    bootout(&format!("gui/{uid}/{LABEL}"));
    bootout(&format!("gui/{uid}/{UPDATE_LABEL}"));
    bootout(&format!("gui/{uid}/{TRAY_LABEL}"));
    let plist = launch_agent_plist_path()?;
    if plist.is_file() {
        fs::remove_file(&plist)?;
        println!("Removed {}", plist.display());
    }
    let update_plist = update_plist_path()?;
    if update_plist.is_file() {
        fs::remove_file(&update_plist)?;
    }
    let tray_plist = tray_plist_path()?;
    if tray_plist.is_file() {
        fs::remove_file(&tray_plist)?;
    }
    Ok(())
}

pub fn stop_background_for_update() -> Result<()> {
    if invoked_by_background_service() {
        // bootout of this job unloads it; the live updater would never come back.
        return Ok(());
    }
    if !service_active() {
        return Ok(());
    }
    let uid = user_id();
    let domain = format!("gui/{uid}/{LABEL}");
    let _ = run_launchctl(&["bootout", &domain]);
    Ok(())
}

pub fn restart_background_after_update() -> Result<()> {
    if !launch_agent_plist_path()?.is_file() {
        return Ok(());
    }
    reload_launch_agent()
}

pub fn systemd_unit_path() -> Result<PathBuf> {
    launch_agent_plist_path()
}

pub fn in_tray_process() -> bool {
    std::env::var("SCALATTICE_TRAY").ok().as_deref() == Some("1")
}

pub fn ensure_tray_login_item() -> Result<()> {
    write_tray_plist()?;
    let uid = user_id();
    let domain = format!("gui/{uid}/{TRAY_LABEL}");
    let plist = tray_plist_path()?;
    bootout(&domain);
    run_launchctl(&["bootstrap", &format!("gui/{uid}"), &plist.to_string_lossy()])?;
    Ok(())
}

pub fn sync_update_launch_agent(enable: bool) -> Result<()> {
    let uid = user_id();
    let domain = format!("gui/{uid}/{UPDATE_LABEL}");
    if enable {
        write_update_plist()?;
        let plist = update_plist_path()?;
        bootout(&domain);
        run_launchctl(&["bootstrap", &format!("gui/{uid}"), &plist.to_string_lossy()])?;
        println!("Automatic daily updates enabled (LaunchAgent).");
    } else {
        bootout(&domain);
        let plist = update_plist_path()?;
        if plist.is_file() {
            fs::remove_file(&plist)?;
        }
        println!("Automatic daily updates disabled.");
    }
    Ok(())
}

fn ensure_service_running(config: &AgentConfig) -> Result<()> {
    if !background_service_available() {
        bail!("launchctl is required for background mode - use: scalattice-agent foreground");
    }
    let _ = crate::service::persist_agent_token(&config.token)?;
    write_launch_agent()?;
    reload_launch_agent()?;
    verify_service_active()
}

fn write_launch_agent() -> Result<()> {
    // ProcessType=Background lets App Nap / jetsam freeze or kill the agent during
    // Metal loads. The next KeepAlive spawn then steals the WebSocket and in-flight
    // jobs fail as `agent disconnected: superseded`. Standard keeps the WS loop alive.
    let plist_path = launch_agent_plist_path()?;
    let bin = resolve_agent_binary()?;
    let env_pairs = parse_agent_env()?;
    let mut env_xml =
        String::from("        <key>SCALATTICE_LAUNCHD</key>\n        <string>1</string>\n");
    for (k, v) in env_pairs {
        env_xml.push_str(&format!(
            "        <key>{}</key>\n        <string>{}</string>\n",
            xml_escape(&k),
            xml_escape(&v)
        ));
    }
    let path_prefix = format!(
        "{}:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin",
        bin.parent()
            .unwrap_or(Path::new("/usr/local/bin"))
            .display()
    );
    env_xml.push_str(&format!(
        "        <key>PATH</key>\n        <string>{}</string>\n",
        xml_escape(&path_prefix)
    ));

    let log_dir = crate::paths::agent_log_path()?
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| {
            crate::paths::home_dir()
                .unwrap_or_default()
                .join("Library/Logs")
        });
    fs::create_dir_all(&log_dir).ok();
    let stdout_log = log_dir.join("launchd.out.log");
    let stderr_log = log_dir.join("launchd.err.log");

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>foreground</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
{env}    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Standard</string>
    <key>StandardOutPath</key>
    <string>{stdout}</string>
    <key>StandardErrorPath</key>
    <string>{stderr}</string>
</dict>
</plist>
"#,
        label = LABEL,
        bin = xml_escape(&bin.display().to_string()),
        env = env_xml,
        stdout = xml_escape(&stdout_log.display().to_string()),
        stderr = xml_escape(&stderr_log.display().to_string()),
    );

    fs::create_dir_all(plist_path.parent().context("LaunchAgents parent")?)?;
    fs::write(&plist_path, plist)?;
    Ok(())
}

fn write_tray_plist() -> Result<()> {
    let bin = resolve_agent_binary().unwrap_or_else(|_| crate::paths::bundled_macos_agent_binary());
    let plist_path = tray_plist_path()?;
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>tray</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>SCALATTICE_TRAY</key>
        <string>1</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
</dict>
</plist>
"#,
        label = TRAY_LABEL,
        bin = xml_escape(&bin.display().to_string()),
    );
    fs::create_dir_all(plist_path.parent().context("LaunchAgents parent")?)?;
    fs::write(&plist_path, plist)?;
    Ok(())
}

fn write_update_plist() -> Result<()> {
    let bin = resolve_agent_binary().unwrap_or_else(|_| {
        crate::paths::install_dir()
            .map(|d| d.join("scalattice-agent"))
            .unwrap_or_else(|_| PathBuf::from("scalattice-agent"))
    });
    let plist_path = update_plist_path()?;
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>update</string>
    </array>
    <key>StartInterval</key>
    <integer>86400</integer>
    <key>RunAtLoad</key>
    <false/>
</dict>
</plist>
"#,
        label = UPDATE_LABEL,
        bin = xml_escape(&bin.display().to_string()),
    );
    fs::create_dir_all(plist_path.parent().context("LaunchAgents parent")?)?;
    fs::write(&plist_path, plist)?;
    Ok(())
}

fn reload_launch_agent() -> Result<()> {
    let uid = user_id();
    let domain = format!("gui/{uid}/{LABEL}");
    if invoked_by_background_service() {
        // NEVER bootout our own LaunchAgent. bootout unloads the job, launchd
        // kills this process, and bootstrap/kickstart never run. KeepAlive does
        // not apply to a booted-out job, so the agent stays dead until the next
        // GUI login. Linux `systemctl restart` is one atomic call; this is not.
        //
        // kickstart -k is a single launchd operation: replace this instance
        // while leaving the job loaded. If that fails, the caller still exits
        // and KeepAlive re-execs the (already loaded) ProgramArguments.
        let _ = run_launchctl(&["kickstart", "-k", &domain]);
        return Ok(());
    }
    let plist = launch_agent_plist_path()?;
    bootout(&domain);
    run_launchctl(&["bootstrap", &format!("gui/{uid}"), &plist.to_string_lossy()])?;
    let _ = run_launchctl(&["kickstart", "-k", &domain]);
    Ok(())
}

fn verify_service_active() -> Result<()> {
    for _ in 0..10 {
        if service_active() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    bail!("scalattice-agent is not running - try: scalattice-agent set-token --token …");
}

fn launchctl_print_running(label: &str) -> bool {
    let uid = user_id();
    let domain = format!("gui/{uid}/{label}");
    let output = Command::new("launchctl")
        .args(["print", &domain])
        .output()
        .ok();
    let Some(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.contains("state = running") || text.contains("pid = ")
}

fn bootout(domain: &str) {
    let _ = run_launchctl(&["bootout", domain]);
}

fn run_launchctl(args: &[&str]) -> Result<()> {
    let output = Command::new("launchctl")
        .args(args)
        .output()
        .context("failed to run launchctl")?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(if stderr.is_empty() {
            format!("launchctl {} failed", args.join(" "))
        } else {
            stderr
        })
    }
}

fn parse_agent_env() -> Result<Vec<(String, String)>> {
    let env_file = crate::paths::agent_env_path()?;
    if !env_file.is_file() {
        return Ok(Vec::new());
    }
    let raw = crate::config::read_text_file_lossy(&env_file)
        .with_context(|| format!("read {}", env_file.display()))?;
    let mut pairs = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let assignment = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        if let Some((k, v)) = assignment.split_once('=') {
            let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
            pairs.push((k.trim().to_string(), v));
        }
    }
    Ok(pairs)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn user_id() -> u32 {
    unsafe { libc::getuid() }
}

fn launch_agent_plist_path_home() -> Result<PathBuf> {
    Ok(crate::paths::home_dir()?
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn launch_agent_plist_path() -> Result<PathBuf> {
    launch_agent_plist_path_home()
}

fn update_plist_path() -> Result<PathBuf> {
    Ok(crate::paths::home_dir()?
        .join("Library/LaunchAgents")
        .join(format!("{UPDATE_LABEL}.plist")))
}

fn tray_plist_path() -> Result<PathBuf> {
    Ok(crate::paths::home_dir()?
        .join("Library/LaunchAgents")
        .join(format!("{TRAY_LABEL}.plist")))
}
