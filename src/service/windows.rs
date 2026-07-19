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
    // A saved token means setup was requested — treat as Stopped so the tray watchdog
    // restarts the worker even if Startup shortcuts were never registered (common after
    // installer launch-without-set-token, or an early-return save path).
    if autostart_configured() || crate::config::read_saved_agent_token().is_some() {
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
    // Uninstall / wipe: force-kill any remaining agent processes so DLLs and
    // GGUFs unlock (pid-file stops alone can miss a wedged tray).
    force_kill_all_agent_processes();
    Ok(())
}

fn force_kill_all_agent_processes() {
    for _ in 0..8 {
        let _ = Command::new("taskkill")
            .args(["/IM", "scalattice-agent.exe", "/F", "/T"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        std::thread::sleep(std::time::Duration::from_millis(350));
        if !background_agent_running() && !tray_instance_running() {
            break;
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(800));
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
    // Keep this short — callers (tray Save token) must not look hung. The watchdog
    // retries if the mutex is not held yet.
    for delay_ms in [200_u64, 400, 800] {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        if background_agent_running() {
            return;
        }
    }
}

/// True when the background single-instance mutex is held.
/// Prefer this over WMI/`Get-CimInstance` — that path can hang while CUDA/driver init is wedged.
fn background_agent_running() -> bool {
    background_mutex_held() || background_pid_alive()
}

fn background_mutex_held() -> bool {
    let name: Vec<u16> = "Local\\ScalatticeAgentBackground\0".encode_utf16().collect();
    unsafe {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
        let handle = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr());
        if handle.is_null() {
            false
        } else {
            CloseHandle(handle);
            true
        }
    }
}

fn background_pid_path() -> Option<PathBuf> {
    install_dir().ok().map(|d| d.join("background.pid"))
}

fn background_pid_alive() -> bool {
    let Some(path) = background_pid_path() else {
        return false;
    };
    let Ok(raw) = fs::read_to_string(&path) else {
        return false;
    };
    let Ok(pid) = raw.trim().parse::<u32>() else {
        return false;
    };
    process_id_alive(pid)
}

fn process_id_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code) != 0;
        CloseHandle(handle);
        ok && code == STILL_ACTIVE as u32
    }
}

fn stop_background_agent_only() {
    let mut killed = false;
    if let Some(path) = background_pid_path() {
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(pid) = raw.trim().parse::<u32>() {
                killed = taskkill_pid(pid);
            }
        }
        let _ = fs::remove_file(&path);
    }
    // Never taskkill /IM — that would also kill the tray. Prefer the pid file;
    // if it's missing, skip anything that looks like the tray instance.
    if !killed && background_mutex_held() {
        let tray_pid = install_dir()
            .ok()
            .and_then(|d| fs::read_to_string(d.join("tray.pid")).ok())
            .and_then(|raw| raw.trim().parse::<u32>().ok());
        let self_pid = std::process::id();
        for pid in agent_exe_pids() {
            if pid == self_pid || Some(pid) == tray_pid {
                continue;
            }
            let _ = taskkill_pid(pid);
        }
    }
}

fn taskkill_pid(pid: u32) -> bool {
    Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn agent_exe_pids() -> Vec<u32> {
    // Lightweight: tasklist CSV, no WMI command-line lookup (that can hang on bad drivers).
    let output = Command::new("tasklist")
        .args([
            "/FI",
            "IMAGENAME eq scalattice-agent.exe",
            "/FO",
            "CSV",
            "/NH",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            // "scalattice-agent.exe","1234","Session","..."
            let mut parts = line.split(',');
            let _name = parts.next()?;
            let pid = parts.next()?.trim().trim_matches('"').parse().ok()?;
            Some(pid)
        })
        .collect()
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
    if let Ok(path) = install_dir().map(|d| d.join("tray.pid")) {
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(pid) = raw.trim().parse::<u32>() {
                let _ = Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/F"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output();
            }
        }
    }
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

    // Keep in sync with installer/windows/scalattice-run.cmd
    let run_cmd = "@echo off\r\n\
setlocal\r\n\
set \"INSTALL=%~dp0\"\r\n\
set \"LIB=%LOCALAPPDATA%\\Scalattice\\lib\"\r\n\
if not exist \"%LIB%\" set \"LIB=%INSTALL%lib\"\r\n\
set \"PATH=%INSTALL%;%LIB%;%PATH%\"\r\n\
cd /d \"%INSTALL%\"\r\n\
\r\n\
if /I \"%~1\"==\"uninstall\" goto :RunAgent\r\n\
if /I \"%~1\"==\"set-token\" goto :RunAgent\r\n\
\r\n\
call :CheckCudaRuntime\r\n\
if errorlevel 1 (\r\n\
  echo.\r\n\
  echo Scalattice Agent cannot start: CUDA 12 runtime DLLs are missing.\r\n\
  echo Expected under: %LIB%\r\n\
  echo   cudart64_12.dll\r\n\
  echo   cublas64_12.dll\r\n\
  echo   cublasLt64_12.dll\r\n\
  echo.\r\n\
  echo Reinstall Scalattice Agent from https://scalattice.cloud\r\n\
  echo Do not launch scalattice-agent.exe directly without the installer bundle.\r\n\
  call :LogCudaMissing\r\n\
  exit /b 1\r\n\
)\r\n\
\r\n\
call :CheckNvidiaDriver\r\n\
if errorlevel 1 (\r\n\
  echo.\r\n\
  echo WARNING: NVIDIA driver not found (nvcuda.dll).\r\n\
  echo GPU jobs will not run until you install a Game Ready or Studio driver from:\r\n\
  echo   https://www.nvidia.com/Download/index.aspx\r\n\
  echo The agent will still start for CPU-compatible models when this build supports it.\r\n\
  echo.\r\n\
  call :LogNvidiaDriverMissing\r\n\
)\r\n\
\r\n\
:RunAgent\r\n\
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
\"%INSTALL%scalattice-agent.exe\" %*\r\n\
exit /b %ERRORLEVEL%\r\n\
\r\n\
:CheckCudaRuntime\r\n\
if not exist \"%LIB%\\cudart64_12.dll\" if not exist \"%INSTALL%cudart64_12.dll\" exit /b 1\r\n\
if not exist \"%LIB%\\cublas64_12.dll\" if not exist \"%INSTALL%cublas64_12.dll\" exit /b 1\r\n\
if not exist \"%LIB%\\cublasLt64_12.dll\" if not exist \"%INSTALL%cublasLt64_12.dll\" exit /b 1\r\n\
exit /b 0\r\n\
\r\n\
:CheckNvidiaDriver\r\n\
if exist \"%SystemRoot%\\System32\\nvcuda.dll\" exit /b 0\r\n\
if exist \"%SystemRoot%\\SysWOW64\\nvcuda.dll\" exit /b 0\r\n\
exit /b 1\r\n\
\r\n\
:LogCudaMissing\r\n\
set \"LOGDIR=%LOCALAPPDATA%\\Scalattice\\logs\"\r\n\
if not exist \"%LOGDIR%\" mkdir \"%LOGDIR%\" >nul 2>&1\r\n\
>>\"%LOGDIR%\\agent.log\" echo [%DATE% %TIME%] CUDA runtime missing under %LIB% — reinstall Scalattice Agent\r\n\
exit /b 0\r\n\
\r\n\
:LogNvidiaDriverMissing\r\n\
set \"LOGDIR=%LOCALAPPDATA%\\Scalattice\\logs\"\r\n\
if not exist \"%LOGDIR%\" mkdir \"%LOGDIR%\" >nul 2>&1\r\n\
>>\"%LOGDIR%\\agent.log\" echo [%DATE% %TIME%] NVIDIA driver missing (nvcuda.dll) — install Game Ready/Studio driver for GPU jobs\r\n\
exit /b 0\r\n";

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

// Keep in sync with installer/windows/launch-*.vbs
const LAUNCH_TRAY_VBS: &str = r#"Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
install = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\bin")
If Not fso.FolderExists(install) Then install = fso.GetParentFolderName(WScript.ScriptFullName)
lib = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\lib")
If Not fso.FolderExists(lib) Then lib = install & "\lib"
If Not CudaRuntimeOk(fso, lib, install) Then
  LogCudaMissing sh, fso, lib
  MsgBox "Scalattice Agent cannot start because the CUDA 12 runtime is missing." & vbCrLf & vbCrLf & _
    "Expected under:" & vbCrLf & "  " & lib & vbCrLf & vbCrLf & _
    "Reinstall Scalattice Agent from https://scalattice.cloud", _
    vbCritical, "Scalattice Agent"
  WScript.Quit 1
End If
Set env = sh.Environment("PROCESS")
env("SCALATTICE_TRAY_HIDDEN") = "1"
env("SCALATTICE_TRAY") = "1"
env("PATH") = install & ";" & lib & ";" & env("PATH")
sh.CurrentDirectory = install
sh.Run """" & install & "\scalattice-agent.exe"" tray", 0, False

Function CudaRuntimeOk(fso, lib, install)
  Dim names, i, name
  names = Array("cudart64_12.dll", "cublas64_12.dll", "cublasLt64_12.dll")
  For i = 0 To UBound(names)
    name = names(i)
    If Not fso.FileExists(lib & "\" & name) And Not fso.FileExists(install & "\" & name) Then
      CudaRuntimeOk = False
      Exit Function
    End If
  Next
  CudaRuntimeOk = True
End Function

Sub LogCudaMissing(sh, fso, lib)
  Dim logDir, logPath, ts, stream
  On Error Resume Next
  logDir = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\logs")
  If Not fso.FolderExists(logDir) Then fso.CreateFolder logDir
  logPath = logDir & "\agent.log"
  ts = Now
  Set stream = fso.OpenTextFile(logPath, 8, True)
  stream.WriteLine "[" & ts & "] CUDA runtime missing under " & lib & " — reinstall Scalattice Agent"
  stream.Close
  On Error Goto 0
End Sub
"#;

const LAUNCH_TRAY_INTERACTIVE_VBS: &str = LAUNCH_TRAY_VBS;

// Launch the agent exe directly (no cmd.exe host). A blocking .cmd console used to
// survive reboot paths and kill the agent when closed. Crash recovery is handled by
// the tray watchdog (and Linux systemd Restart=always).
const LAUNCH_BACKGROUND_VBS: &str = r#"Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
install = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\bin")
If Not fso.FolderExists(install) Then install = fso.GetParentFolderName(WScript.ScriptFullName)
lib = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\lib")
If Not fso.FolderExists(lib) Then lib = install & "\lib"
If Not CudaRuntimeOk(fso, lib, install) Then
  LogCudaMissing sh, fso, lib
  WScript.Quit 1
End If
Set env = sh.Environment("PROCESS")
env("SCALATTICE_BACKGROUND") = "1"
env("PATH") = install & ";" & lib & ";" & env("PATH")
sh.CurrentDirectory = install
sh.Run """" & install & "\scalattice-agent.exe"" foreground", 0, False

Function CudaRuntimeOk(fso, lib, install)
  Dim names, i, name
  names = Array("cudart64_12.dll", "cublas64_12.dll", "cublasLt64_12.dll")
  For i = 0 To UBound(names)
    name = names(i)
    If Not fso.FileExists(lib & "\" & name) And Not fso.FileExists(install & "\" & name) Then
      CudaRuntimeOk = False
      Exit Function
    End If
  Next
  CudaRuntimeOk = True
End Function

Sub LogCudaMissing(sh, fso, lib)
  Dim logDir, logPath, ts, stream
  On Error Resume Next
  logDir = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\logs")
  If Not fso.FolderExists(logDir) Then fso.CreateFolder logDir
  logPath = logDir & "\agent.log"
  ts = Now
  Set stream = fso.OpenTextFile(logPath, 8, True)
  stream.WriteLine "[" & ts & "] CUDA runtime missing under " & lib & " — reinstall Scalattice Agent"
  stream.Close
  On Error Goto 0
End Sub
"#;

const STARTUP_AGENT_VBS_CONTENT: &str = r#"Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
install = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\bin")
target = install & "\launch-background.vbs"
If Not fso.FileExists(target) Then WScript.Quit 0
sh.Run "wscript.exe //nologo """ & target & """", 0, False
"#;

const STARTUP_TRAY_VBS_CONTENT: &str = r#"Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
install = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Scalattice\bin")
target = install & "\launch-tray.vbs"
If Not fso.FileExists(target) Then WScript.Quit 0
sh.Run "wscript.exe //nologo """ & target & """", 0, False
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
