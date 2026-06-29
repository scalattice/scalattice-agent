use crate::config::AgentConfig;
use crate::paths::{agent_binary_name, agent_log_path, install_dir, lib_dir, resolve_agent_binary};
use crate::service::BackgroundStatus;
use anyhow::{bail, Context, Result};
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
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
    ensure_background_task(config)
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
    let output = Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {}", agent_binary_name()), "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout.contains(agent_binary_name())
        }
        _ => false,
    }
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
    write_background_runner().map(|_| ())
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
    let _ = Command::new("taskkill")
        .args(["/F", "/IM", agent_binary_name()])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
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

fn ensure_background_task(config: &AgentConfig) -> Result<()> {
    let token_changed = crate::service::persist_agent_token(&config.token)?;
    let runner_changed = write_background_runner()?;
    sync_launch_scripts()?;

    let needs_register = !autostart_configured() || token_changed || runner_changed;

    if needs_register {
        register_agent_autostart()?;
    } else if !service_active() {
        spawn_background_detached()?;
    }

    register_tray_autostart()?;
    launch_tray_if_needed()?;

    Ok(())
}

fn write_background_runner() -> Result<bool> {
    let install = install_dir()?;
    let lib = lib_dir()?;
    let log = agent_log_path()?;
    let bin = resolve_agent_binary()?;
    let runner = install.join("run-background.cmd");

    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir_all(&install)?;

    let script = format!(
        "@echo off\r\n\
set SCALATTICE_BACKGROUND=1\r\n\
set \"PATH={install};{lib};%PATH%\"\r\n\
cd /d \"{install}\"\r\n\
\"{bin}\" foreground >> \"{log}\" 2>&1\r\n",
        install = install.display(),
        lib = lib.display(),
        bin = bin.display(),
        log = log.display(),
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

fn register_agent_autostart() -> Result<()> {
    if try_create_scheduled_task().is_ok() {
        return run_scheduled_task_now();
    }
    install_startup_agent_shortcut()?;
    spawn_background_detached()
}

fn register_tray_autostart() -> Result<()> {
    if try_create_tray_task().is_ok() {
        return run_tray_task_now();
    }
    install_startup_tray_shortcut()?;
    launch_tray_if_needed()
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
