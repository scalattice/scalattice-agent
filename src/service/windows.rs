use crate::config::AgentConfig;
use crate::paths::{agent_log_path, install_dir, lib_dir, resolve_agent_binary};
use crate::service::BackgroundStatus;
use anyhow::{bail, Context, Result};
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const TASK_NAME: &str = "ScalatticeAgent";
const TRAY_TASK_NAME: &str = "ScalatticeAgentTray";
const STARTUP_AGENT_VBS: &str = "ScalatticeAgent.vbs";
const STARTUP_TRAY_VBS: &str = "ScalatticeAgentTray.vbs";

pub fn background_status() -> BackgroundStatus {
    if service_active() {
        return BackgroundStatus::Running;
    }
    if autostart_configured() {
        BackgroundStatus::Stopped
    } else {
        BackgroundStatus::NotInstalled
    }
}

pub fn start_background_from_config(config: &AgentConfig) -> Result<()> {
    ensure_background_task(config, false)
}

pub fn restart_background_from_config(config: &AgentConfig) -> Result<()> {
    force_restart_background(config, true)
}

pub fn restart_after_token_change(config: &AgentConfig) -> Result<()> {
    ensure_background_task(config, false)
}

/// Force-stop then start background + tray using the saved provider token.
/// Used after silent in-place updates so the new binary always comes back up.
pub fn restart_runtime_from_saved_token() -> Result<()> {
    let token = crate::config::read_saved_agent_token()
        .context("no saved provider token; run scalattice-agent set-token first")?;
    let config = AgentConfig::from_env_and_cli(Some(token))?;
    stop_agents_for_update();
    std::thread::sleep(std::time::Duration::from_millis(500));
    force_restart_background(&config, false)
}

fn hidden_powershell(script: &str) -> std::io::Result<std::process::Output> {
    Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
}

fn hidden_powershell_output(script: &str) -> std::io::Result<std::process::Output> {
    Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
}

fn force_restart_background(config: &AgentConfig, skip_tray: bool) -> Result<()> {
    let _ = crate::service::persist_agent_token(&config.token)?;
    let _ = write_background_runner_with_token(&config.token)?;
    sync_launch_scripts()?;

    if !autostart_configured() {
        ensure_agent_autostart_registered()?;
    }

    stop_background_for_token_restart()?;
    spawn_background_detached()?;

    if !skip_tray && !in_tray_process() {
        ensure_tray_autostart_registered()?;
        launch_tray_if_needed()?;
    }

    Ok(())
}

pub fn in_tray_process() -> bool {
    std::env::var("SCALATTICE_TRAY")
        .ok()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

pub fn invoked_by_systemd() -> bool {
    false
}

pub fn invoked_by_background_service() -> bool {
    std::env::var("SCALATTICE_BACKGROUND")
        .ok()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

pub fn background_service_available() -> bool {
    schtasks_available() || startup_dir().map(|d| d.is_dir()).unwrap_or(false)
}

pub fn service_active() -> bool {
    background_agent_running()
}

pub fn follow_service_logs() -> Result<()> {
    if !service_active() {
        bail!(
            "scalattice-agent is not running; save your token with `scalattice-agent set-token` first"
        );
    }

    let log_path = agent_log_path()?;
    if !log_path.is_file() {
        bail!(
            "log file not found at {} (the agent may still be starting)",
            log_path.display()
        );
    }

    let status = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Get-Content -Path '{}' -Wait -Tail 30",
                log_path.display().to_string().replace('\'', "''")
            ),
        ])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .context("failed to run powershell log tail")?;

    match status.code() {
        Some(0) | Some(130) => Ok(()),
        Some(code) => bail!("log tail exited with status {code}"),
        None => bail!("log tail terminated by signal"),
    }
}

pub fn sync_background_env() -> Result<()> {
    if let Some(token) = crate::config::read_saved_agent_token() {
        write_background_runner_with_token(&token).map(|_| ())
    } else {
        write_background_runner_with_token("").map(|_| ())
    }
}

pub fn remove_background_service() -> Result<()> {
    let _ = Command::new("schtasks")
        .args(["/End", "/TN", TASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let _ = Command::new("schtasks")
        .args(["/End", "/TN", TRAY_TASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let _ = Command::new("schtasks")
        .args(["/Delete", "/TN", TASK_NAME, "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let _ = Command::new("schtasks")
        .args(["/Delete", "/TN", TRAY_TASK_NAME, "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    remove_startup_shortcuts();
    stop_background_agent_only();
    stop_tray_agent_only();
    Ok(())
}

pub fn background_runner_path() -> Result<PathBuf> {
    Ok(install_dir()?.join("run-background.cmd"))
}

pub fn systemd_unit_path() -> Result<PathBuf> {
    background_runner_path()
}

pub fn autostart_method_line() -> Option<String> {
    let agent = if startup_agent_shortcut_exists() {
        "agent: Startup folder"
    } else if task_exists() {
        "agent: scheduled task (legacy)"
    } else {
        return None;
    };

    let tray = if startup_tray_shortcut_exists() {
        "tray: Startup folder"
    } else if tray_task_exists() {
        "tray: scheduled task (legacy)"
    } else {
        "tray: not configured"
    };

    Some(format!("{agent}; {tray}"))
}

fn ensure_background_task(config: &AgentConfig, skip_tray: bool) -> Result<()> {
    let token_changed = crate::service::persist_agent_token(&config.token)?;
    let _runner_changed = write_background_runner_with_token(&config.token)?;
    sync_launch_scripts()?;

    // Always (re)register agent + tray autostart so upgrades / partial installs recover
    // after reboot. Single-instance mutexes prevent double-start if both Startup and
    // scheduled tasks fire.
    ensure_agent_autostart_registered()?;

    if token_changed || !background_agent_running() {
        if token_changed && background_agent_running() {
            stop_background_for_token_restart()?;
        }
        start_background_with_retry()?;
    }

    if !skip_tray && !in_tray_process() {
        ensure_tray_autostart_registered()?;
        launch_tray_if_needed()?;
    }

    Ok(())
}

fn start_background_with_retry() -> Result<()> {
    spawn_background_detached()?;
    wait_for_background_start_gentle();
    if !background_agent_running() {
        spawn_background_detached()?;
        wait_for_background_start_gentle();
    }
    Ok(())
}

fn wait_for_background_start_gentle() {
    for delay_ms in [800_u64, 1200, 2000] {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        if background_agent_running() {
            return;
        }
    }
}

fn background_agent_running() -> bool {
    let script = r#"Get-CimInstance Win32_Process -Filter "name='scalattice-agent.exe'" |
  Where-Object { $_.CommandLine -match 'foreground' } |
  Select-Object -First 1 -ExpandProperty ProcessId"#;
    match hidden_powershell_output(script) {
        Ok(output) if output.status.success() => {
            !String::from_utf8_lossy(&output.stdout).trim().is_empty()
        }
        _ => false,
    }
}

fn stop_background_agent_only() {
    let script = r#"Get-CimInstance Win32_Process -Filter "name='scalattice-agent.exe'" |
  Where-Object { $_.CommandLine -match 'foreground' } |
  ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }"#;
    let _ = hidden_powershell(script);
}

fn stop_background_for_token_restart() -> Result<()> {
    for _ in 0..6 {
        stop_background_agent_only();
        if !background_agent_running() {
            std::thread::sleep(std::time::Duration::from_millis(150));
            if !background_agent_running() {
                return Ok(());
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    bail!("could not stop the background agent; close it from Task Manager and try again")
}

fn stop_tray_agent_only() {
    let script = r#"Get-CimInstance Win32_Process -Filter "name='scalattice-agent.exe'" |
  Where-Object { $_.CommandLine -match '\s+tray(\s|$)' } |
  ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }"#;
    let _ = hidden_powershell(script);
}

pub fn stop_agents_for_update() {
    stop_background_agent_only();
    stop_tray_agent_only();
}

fn write_background_runner_with_token(token: &str) -> Result<bool> {
    let install = install_dir()?;
    fs::create_dir_all(&install)?;

    // Keep the token out of the process command line (visible in Task Manager / WMI).
    // Persist to agent.env and let `foreground` load SCALATTICE_AGENT_TOKEN from disk.
    let token_changed = crate::service::persist_agent_token(token.trim())?;

    // Legacy helper used to host the agent under cmd.exe - that left a killable
    // console window. Remove it so nothing can launch it by accident.
    let legacy = install.join("run-background.cmd");
    let removed_legacy = if legacy.is_file() {
        fs::remove_file(&legacy).is_ok()
    } else {
        false
    };

    Ok(token_changed || removed_legacy)
}

fn sync_launch_scripts() -> Result<()> {
    let install = install_dir()?;
    fs::create_dir_all(&install)?;

    let run_cmd = "@echo off\r\n\
setlocal\r\n\
set \"INSTALL=%~dp0\"\r\n\
set \"LIB=%LOCALAPPDATA%\\Scalattice\\lib\"\r\n\
if not exist \"%LIB%\" set \"LIB=%INSTALL%lib\"\r\n\
set \"PATH=%INSTALL%;%LIB%;%PATH%\"\r\n\
cd /d \"%INSTALL%\"\r\n\
if /I \"%~1\"==\"tray\" (\r\n\
  if exist \"%INSTALL%launch-tray.vbs\" (\r\n\
    wscript.exe //nologo \"%INSTALL%launch-tray.vbs\"\r\n\
    exit /b 0\r\n\
  )\r\n\
)\r\n\
if /I \"%~1\"==\"tray-open\" (\r\n\
  if exist \"%INSTALL%launch-tray-interactive.vbs\" (\r\n\
    wscript.exe //nologo \"%INSTALL%launch-tray-interactive.vbs\"\r\n\
    exit /b 0\r\n\
  )\r\n\
)\r\n\
if /I \"%~1\"==\"tray-debug\" (\r\n\
  \"%INSTALL%scalattice-agent.exe\" tray --force\r\n\
  exit /b %ERRORLEVEL%\r\n\
)\r\n\
\"%INSTALL%scalattice-agent.exe\" %*\r\n";

    fs::write(install.join("scalattice-run.cmd"), run_cmd)?;

    for (name, content) in [
        ("launch-tray.vbs", LAUNCH_TRAY_VBS),
        ("launch-tray-interactive.vbs", LAUNCH_TRAY_INTERACTIVE_VBS),
        ("launch-background.vbs", LAUNCH_BACKGROUND_VBS),
    ] {
        fs::write(install.join(name), content)?;
    }

    refresh_startup_shortcuts()?;

    Ok(())
}

const LAUNCH_TRAY_VBS: &str = r#"Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
install = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\bin")
If Not fso.FolderExists(install) Then install = fso.GetParentFolderName(WScript.ScriptFullName)
lib = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\lib")
If Not fso.FolderExists(lib) Then lib = install & "\lib"
Set env = sh.Environment("PROCESS")
env("SCALATTICE_TRAY_HIDDEN") = "1"
env("SCALATTICE_TRAY") = "1"
env("PATH") = install & ";" & lib & ";" & env("PATH")
sh.CurrentDirectory = install
sh.Run """" & install & "\scalattice-agent.exe"" tray", 0, False
"#;

const LAUNCH_TRAY_INTERACTIVE_VBS: &str = r#"Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
install = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\bin")
If Not fso.FolderExists(install) Then install = fso.GetParentFolderName(WScript.ScriptFullName)
lib = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\lib")
If Not fso.FolderExists(lib) Then lib = install & "\lib"
Set env = sh.Environment("PROCESS")
env("SCALATTICE_TRAY_HIDDEN") = "1"
env("SCALATTICE_TRAY") = "1"
env("PATH") = install & ";" & lib & ";" & env("PATH")
sh.CurrentDirectory = install
sh.Run """" & install & "\scalattice-agent.exe"" tray", 0, False
"#;

// Launch the agent exe directly (no cmd.exe host). A blocking .cmd console used to
// survive reboot paths and kill the agent when closed.
const LAUNCH_BACKGROUND_VBS: &str = r#"Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
install = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\bin")
If Not fso.FolderExists(install) Then install = fso.GetParentFolderName(WScript.ScriptFullName)
lib = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\lib")
If Not fso.FolderExists(lib) Then lib = install & "\lib"
Set env = sh.Environment("PROCESS")
env("SCALATTICE_BACKGROUND") = "1"
env("PATH") = install & ";" & lib & ";" & env("PATH")
sh.CurrentDirectory = install
sh.Run """" & install & "\scalattice-agent.exe"" foreground", 0, False
"#;

const STARTUP_AGENT_VBS_CONTENT: &str = r#"Set sh = CreateObject("WScript.Shell")
install = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\bin")
sh.Run "wscript.exe //nologo """ & install & "\launch-background.vbs""", 0, False
"#;

const STARTUP_TRAY_VBS_CONTENT: &str = r#"Set sh = CreateObject("WScript.Shell")
install = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\bin")
sh.Run "wscript.exe //nologo """ & install & "\launch-tray.vbs""", 0, False
"#;

fn schtasks_available() -> bool {
    Command::new("schtasks")
        .arg("/?")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn startup_dir() -> Result<PathBuf> {
    let appdata = std::env::var("APPDATA").context("APPDATA is not set")?;
    Ok(PathBuf::from(appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup"))
}

fn autostart_configured() -> bool {
    startup_agent_shortcut_exists() || task_exists()
}

fn task_exists() -> bool {
    Command::new("schtasks")
        .args(["/Query", "/TN", TASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn tray_task_exists() -> bool {
    Command::new("schtasks")
        .args(["/Query", "/TN", TRAY_TASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn startup_agent_shortcut_exists() -> bool {
    startup_dir()
        .map(|d| d.join(STARTUP_AGENT_VBS).is_file())
        .unwrap_or(false)
}

fn startup_tray_shortcut_exists() -> bool {
    startup_dir()
        .map(|d| d.join(STARTUP_TRAY_VBS).is_file())
        .unwrap_or(false)
}

fn ensure_agent_autostart_registered() -> Result<()> {
    // Prefer the Startup folder only. Dual schtasks + Startup previously caused
    // double launches; the Background mutex stops duplicates but Startup alone is
    // more reliable for interactive Windows sessions after reboot.
    let _ = Command::new("schtasks")
        .args(["/Delete", "/TN", TASK_NAME, "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    install_startup_agent_shortcut()?;
    if startup_agent_shortcut_exists() {
        Ok(())
    } else {
        bail!("failed to register agent Startup shortcut")
    }
}

fn ensure_tray_autostart_registered() -> Result<()> {
    let _ = Command::new("schtasks")
        .args(["/Delete", "/TN", TRAY_TASK_NAME, "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    install_startup_tray_shortcut()?;
    if startup_tray_shortcut_exists() {
        Ok(())
    } else {
        bail!("failed to register tray Startup shortcut")
    }
}

fn try_create_scheduled_task() -> Result<()> {
    let vbs = install_dir()?.join("launch-background.vbs");
    if !vbs.is_file() {
        bail!("failed to write {}", vbs.display());
    }

    let tr = format!("wscript.exe //nologo \"{}\"", vbs.display());
    let output = Command::new("schtasks")
        .args([
            "/Create",
            "/TN",
            TASK_NAME,
            "/TR",
            &tr,
            "/SC",
            "ONLOGON",
            "/RL",
            "LIMITED",
            "/F",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("failed to run schtasks")?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("Access is denied") || stderr.contains("denied") {
        bail!("schtasks access denied");
    }

    bail!("schtasks failed: {}", stderr.trim());
}

fn try_create_tray_task() -> Result<()> {
    let vbs = install_dir()?.join("launch-tray.vbs");
    if !vbs.is_file() {
        bail!("failed to write {}", vbs.display());
    }

    let tr = format!("wscript.exe //nologo \"{}\"", vbs.display());
    let output = Command::new("schtasks")
        .args([
            "/Create",
            "/TN",
            TRAY_TASK_NAME,
            "/TR",
            &tr,
            "/SC",
            "ONLOGON",
            "/RL",
            "LIMITED",
            "/F",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("failed to create tray scheduled task")?;

    if output.status.success() || tray_task_exists() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("Access is denied") || stderr.contains("denied") {
        bail!("schtasks access denied");
    }

    bail!("failed to register tray task: {}", stderr.trim());
}

fn install_startup_agent_shortcut() -> Result<()> {
    write_startup_shortcut(STARTUP_AGENT_VBS, STARTUP_AGENT_VBS_CONTENT)
}

fn install_startup_tray_shortcut() -> Result<()> {
    write_startup_shortcut(STARTUP_TRAY_VBS, STARTUP_TRAY_VBS_CONTENT)
}

fn write_startup_shortcut(name: &str, content: &str) -> Result<()> {
    let startup = startup_dir()?;
    fs::create_dir_all(&startup)?;
    let dest = startup.join(name);
    fs::write(&dest, content).with_context(|| format!("failed to install {}", dest.display()))?;
    Ok(())
}

fn refresh_startup_shortcuts() -> Result<()> {
    if startup_agent_shortcut_exists() {
        install_startup_agent_shortcut()?;
    }
    if startup_tray_shortcut_exists() {
        install_startup_tray_shortcut()?;
    }
    Ok(())
}

fn remove_startup_shortcuts() {
    if let Ok(startup) = startup_dir() {
        let _ = fs::remove_file(startup.join(STARTUP_AGENT_VBS));
        let _ = fs::remove_file(startup.join(STARTUP_TRAY_VBS));
    }
}

fn run_scheduled_task_now() -> Result<()> {
    let output = Command::new("schtasks")
        .args(["/Run", "/TN", TASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("failed to start background task")?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("already running") {
        return Ok(());
    }

    spawn_background_detached()
}

fn run_tray_task_now() -> Result<()> {
    if !tray_task_exists() {
        return launch_tray_if_needed();
    }
    let _ = Command::new("schtasks")
        .args(["/Run", "/TN", TRAY_TASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    launch_tray_if_needed()
}

fn spawn_background_detached() -> Result<()> {
    let vbs = install_dir()?.join("launch-background.vbs");
    if vbs.is_file() {
        Command::new("wscript.exe")
            .args(["//nologo", &vbs.display().to_string()])
            .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start background agent")?;
        return Ok(());
    }

    // Fallback: spawn the exe directly (never host under a visible cmd window).
    let bin = resolve_agent_binary()?;
    let lib = lib_dir().unwrap_or_else(|_| bin.parent().unwrap_or(std::path::Path::new(".")).to_path_buf());
    let install = install_dir().unwrap_or_else(|_| bin.parent().unwrap_or(std::path::Path::new(".")).to_path_buf());
    let path = format!("{};{};{}", install.display(), lib.display(), std::env::var("PATH").unwrap_or_default());
    Command::new(&bin)
        .arg("foreground")
        .env("SCALATTICE_BACKGROUND", "1")
        .env("PATH", path)
        .current_dir(&install)
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start background agent")?;
    Ok(())
}

fn launch_tray_if_needed() -> Result<()> {
    if tray_instance_running() {
        activate_tray_window();
        return Ok(());
    }

    let vbs = install_dir()?.join("launch-tray.vbs");
    Command::new("wscript.exe")
        .args(["//nologo", &vbs.display().to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("failed to launch tray")?;
    Ok(())
}

fn activate_tray_window() {
    let title: Vec<u16> = "Scalattice Agent\0".encode_utf16().collect();
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
        };
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if !hwnd.is_null() {
            ShowWindow(hwnd, SW_RESTORE);
            ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

fn tray_instance_running() -> bool {
    let name: Vec<u16> = "ScalatticeAgentTray\0".encode_utf16().collect();
    unsafe {
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
        use windows_sys::Win32::System::Threading::CreateMutexW;

        let handle = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if handle.is_null() {
            return false;
        }
        let already = GetLastError() == ERROR_ALREADY_EXISTS;
        CloseHandle(handle);
        already
    }
}
