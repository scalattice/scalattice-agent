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
    if in_tray_process() {
        schedule_full_application_restart(config)
    } else {
        // Install / CLI set-token: register autostart and start the agent without killing
        // a tray the installer may launch immediately afterward.
        ensure_background_task(config, false)
    }
}

pub fn schedule_full_application_restart(config: &AgentConfig) -> Result<()> {
    let _ = crate::service::persist_agent_token(&config.token)?;
    let _ = write_background_runner_with_token(&config.token)?;
    spawn_hidden_restart_worker()
}

pub fn run_restart_after_token_worker() -> Result<()> {
    std::thread::sleep(std::time::Duration::from_millis(600));

    let _ = hidden_powershell(
        "[Environment]::SetEnvironmentVariable('SCALATTICE_AGENT_TOKEN', $null, 'User')",
    );

    for task in [TASK_NAME, TRAY_TASK_NAME] {
        let _ = Command::new("schtasks")
            .args(["/End", "/TN", task])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
    }

    let _ = hidden_powershell(
        r#"Get-CimInstance Win32_Process -Filter "name='scalattice-agent.exe'" |
ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }"#,
    );

    std::thread::sleep(std::time::Duration::from_millis(600));

    let _ = sync_launch_scripts();
    let _ = ensure_agent_autostart_registered();
    let _ = ensure_tray_autostart_registered();

    spawn_background_detached()?;
    if !wait_for_background_start(std::time::Duration::from_secs(8)) {
        spawn_background_detached()?;
        let _ = wait_for_background_start(std::time::Duration::from_secs(5));
    }
    std::thread::sleep(std::time::Duration::from_millis(600));
    launch_tray_if_needed()?;
    Ok(())
}

fn spawn_hidden_restart_worker() -> Result<()> {
    let bin = resolve_agent_binary()?;
    Command::new(&bin)
        .arg("restart-after-token")
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("launch restart worker via {}", bin.display()))?;
    Ok(())
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
    let agent = if task_exists() {
        "agent: scheduled task"
    } else if startup_agent_shortcut_exists() {
        "agent: Startup folder"
    } else {
        return None;
    };

    let tray = if tray_task_exists() {
        "tray: scheduled task"
    } else if startup_tray_shortcut_exists() {
        "tray: Startup folder"
    } else {
        "tray: not configured"
    };

    Some(format!("{agent}; {tray}"))
}

fn ensure_background_task(config: &AgentConfig, skip_tray: bool) -> Result<()> {
    let token_changed = crate::service::persist_agent_token(&config.token)?;
    let runner_changed = write_background_runner_with_token(&config.token)?;
    sync_launch_scripts()?;

    let needs_register = !autostart_configured() || token_changed || runner_changed;

    if needs_register {
        ensure_agent_autostart_registered()?;
    }

    if token_changed || !background_agent_running() {
        if token_changed && background_agent_running() {
            stop_background_for_token_restart()?;
        }
        spawn_background_detached()?;
        if !wait_for_background_start(std::time::Duration::from_secs(8)) {
            spawn_background_detached()?;
            let _ = wait_for_background_start(std::time::Duration::from_secs(5));
        }
    }

    if !skip_tray && !in_tray_process() {
        if needs_register {
            ensure_tray_autostart_registered()?;
        }
        launch_tray_if_needed()?;
    }

    Ok(())
}

fn wait_for_background_start(timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if background_agent_running() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    false
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
    let _ = Command::new("schtasks")
        .args(["/End", "/TN", TASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    for _ in 0..10 {
        stop_background_agent_only();
        stop_non_tray_agent_processes();
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

fn stop_non_tray_agent_processes() {
    let script = r#"Get-CimInstance Win32_Process -Filter "name='scalattice-agent.exe'" |
  Where-Object { $_.CommandLine -notmatch '\s+tray(\s|$)' } |
  ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }"#;
    let _ = hidden_powershell(script);
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
    let lib = lib_dir()?;
    let log = agent_log_path()?;
    let bin = resolve_agent_binary()?;
    let runner = install.join("run-background.cmd");
    let token_arg = token.trim().replace('%', "%%").replace('"', "");

    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir_all(&install)?;

    let foreground_cmd = if token_arg.is_empty() {
        format!("\"{}\" foreground", bin.display())
    } else {
        format!(
            "\"{}\" foreground --token \"{}\"",
            bin.display(),
            token_arg
        )
    };

    let script = format!(
        "@echo off\r\n\
setlocal\r\n\
set SCALATTICE_BACKGROUND=1\r\n\
set SCALATTICE_AGENT_TOKEN=\r\n\
set \"PATH={install};{lib};%PATH%\"\r\n\
cd /d \"{install}\"\r\n\
{foreground_cmd} >> \"{log}\" 2>&1\r\n",
        install = install.display(),
        lib = lib.display(),
        log = log.display(),
        foreground_cmd = foreground_cmd,
    );

    let changed = if runner.is_file() {
        fs::read_to_string(&runner).unwrap_or_default() != script
    } else {
        true
    };

    fs::write(&runner, script)?;
    Ok(changed)
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
if /I \"%~1\"==\"tray-debug\" (\r\n\
  \"%INSTALL%scalattice-agent.exe\" tray --force\r\n\
  exit /b %ERRORLEVEL%\r\n\
)\r\n\
\"%INSTALL%scalattice-agent.exe\" %*\r\n";

    fs::write(install.join("scalattice-run.cmd"), run_cmd)?;

    for (name, content) in [
        ("launch-tray.vbs", LAUNCH_TRAY_VBS),
        ("launch-background.vbs", LAUNCH_BACKGROUND_VBS),
    ] {
        fs::write(install.join(name), content)?;
    }

    Ok(())
}

const LAUNCH_TRAY_VBS: &str = r#"Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
install = fso.GetParentFolderName(WScript.ScriptFullName)
lib = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\lib")
If Not fso.FolderExists(lib) Then lib = install & "\lib"
Set env = sh.Environment("PROCESS")
env("SCALATTICE_TRAY_HIDDEN") = "1"
env("PATH") = install & ";" & lib & ";" & env("PATH")
sh.Run """" & install & "\scalattice-agent.exe"" tray", 0, False
"#;

const LAUNCH_BACKGROUND_VBS: &str = r#"Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
install = fso.GetParentFolderName(WScript.ScriptFullName)
lib = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\lib")
If Not fso.FolderExists(lib) Then lib = install & "\lib"
Set env = sh.Environment("PROCESS")
env("PATH") = install & ";" & lib & ";" & env("PATH")
sh.Run """" & install & "\run-background.cmd""", 0, False
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
    task_exists() || startup_agent_shortcut_exists()
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
    if task_exists() {
        return Ok(());
    }
    if try_create_scheduled_task().is_ok() {
        return Ok(());
    }
    install_startup_agent_shortcut()
}

fn ensure_tray_autostart_registered() -> Result<()> {
    if tray_task_exists() {
        return Ok(());
    }
    if try_create_tray_task().is_ok() {
        return Ok(());
    }
    install_startup_tray_shortcut()
}

fn try_create_scheduled_task() -> Result<()> {
    let runner = background_runner_path()?;
    if !runner.is_file() {
        bail!("failed to write {}", runner.display());
    }

    let tr = format!("\"{}\"", runner.display());
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
    let startup = startup_dir()?;
    fs::create_dir_all(&startup)?;
    let src = install_dir()?.join("launch-background.vbs");
    let dest = startup.join(STARTUP_AGENT_VBS);
    fs::copy(&src, &dest).with_context(|| format!("failed to install {}", dest.display()))?;
    Ok(())
}

fn install_startup_tray_shortcut() -> Result<()> {
    let startup = startup_dir()?;
    fs::create_dir_all(&startup)?;
    let src = install_dir()?.join("launch-tray.vbs");
    let dest = startup.join(STARTUP_TRAY_VBS);
    fs::copy(&src, &dest).with_context(|| format!("failed to install {}", dest.display()))?;
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
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .context("failed to start background agent")?;
        return Ok(());
    }

    let runner = background_runner_path()?;
    Command::new("cmd")
        .args(["/C", "start", "", "/MIN", &runner.display().to_string()])
        .creation_flags(CREATE_NO_WINDOW)
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
