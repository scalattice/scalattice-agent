use crate::config::AgentConfig;
use crate::paths::{agent_log_path, install_dir, lib_dir};
use crate::service;
use crate::state;
use anyhow::{Context, Result};
use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
};

const DASHBOARD_URL: &str = "https://scalattice.cloud/providers";
const WINDOW_TITLE: &str = "Scalattice Agent";
const TRAY_MUTEX: &str = "ScalatticeAgentTray";

pub fn run_tray_ui(force: bool) -> Result<()> {
    write_tray_log(&format!(
        "tray starting pid={} force={force}",
        std::process::id()
    ));
    if let Err(err) = run_tray_ui_inner(force) {
        write_tray_log(&format!("tray exited with error: {err:#}"));
        eprintln!("Scalattice tray error: {err:#}");
        return Err(err);
    }
    write_tray_log("tray exited normally");
    Ok(())
}

/// If the tray app is already running, bring its window forward.
pub fn activate_existing_panel() -> bool {
    activate_tray_window()
}

fn run_tray_ui_inner(force: bool) -> Result<()> {
    std::env::set_var("SCALATTICE_TRAY", "1");
    if force {
        clear_stale_tray_pid()?;
    }
    write_tray_pid()?;

    let interactive = has_attached_console() && !launched_hidden();
    maybe_detach_console();

    if !acquire_tray_instance(force)? {
        if interactive {
            println!("Activated existing Scalattice tray window.");
        }
        clear_tray_pid();
        return Ok(());
    }

    let show_window = Arc::new(AtomicBool::new(interactive));
    let icon = load_tray_icon()?;
    let viewport_icon = load_viewport_icon();

    let (event_tx, event_rx) = mpsc::channel();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = event_tx.send(event);
    }));

    let show_for_menu = show_window.clone();
    let tray_menu = Menu::new();
    let open_item = MenuItem::new("Open panel", true, None);
    let quit_item = MenuItem::new("Quit tray", true, None);
    tray_menu
        .append(&open_item)
        .context("failed to build tray menu")?;
    tray_menu
        .append(&PredefinedMenuItem::separator())
        .context("failed to build tray menu")?;
    tray_menu
        .append(&quit_item)
        .context("failed to build tray menu")?;

    let open_id = open_item.id().clone();
    let quit_id = quit_item.id().clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == open_id {
            show_for_menu.store(true, Ordering::SeqCst);
        } else if event.id == quit_id {
            write_tray_log("tray quit from menu");
            std::process::exit(0);
        }
    }));

    let _tray = TrayIconBuilder::new()
        .with_tooltip("Scalattice Agent — click to open")
        .with_icon(icon)
        .with_menu(Box::new(tray_menu))
        .build()
        .context("failed to create tray icon")?;

    write_tray_log("tray started");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 520.0])
            .with_min_inner_size([640.0, 420.0])
            .with_title(WINDOW_TITLE)
            .with_visible(interactive)
            .with_icon(viewport_icon),
        ..Default::default()
    };

    let show_for_app = show_window.clone();
    eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(scalattice_visuals());
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(200));
                    ctx.request_repaint();
                }
            });
            Ok(Box::new(TrayApp::new(event_rx, show_for_app)))
        }),
    )
    .map_err(|err| anyhow::anyhow!("tray UI exited: {err}"))?;

    clear_tray_pid();
    Ok(())
}

struct TrayApp {
    event_rx: mpsc::Receiver<TrayIconEvent>,
    show_window: Arc<AtomicBool>,
    token_input: String,
    status_lines: Vec<String>,
    action_message: String,
    logs: String,
    log_path: Option<PathBuf>,
    log_offset: u64,
    last_refresh: Instant,
}

impl TrayApp {
    fn new(event_rx: mpsc::Receiver<TrayIconEvent>, show_window: Arc<AtomicBool>) -> Self {
        let token_input = crate::config::read_saved_agent_token().unwrap_or_default();
        let log_path = agent_log_path().ok();
        Self {
            event_rx,
            show_window,
            token_input,
            status_lines: Vec::new(),
            action_message: String::new(),
            logs: String::new(),
            log_path,
            log_offset: 0,
            last_refresh: Instant::now() - Duration::from_secs(1),
        }
    }

    fn poll_tray(&mut self, ctx: &egui::Context) {
        if self.show_window.swap(false, Ordering::SeqCst) {
            self.reveal_window(ctx);
        }

        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                TrayIconEvent::Click {
                    button: tray_icon::MouseButton::Left,
                    ..
                }
                | TrayIconEvent::DoubleClick { .. } => {
                    self.reveal_window(ctx);
                }
                _ => {}
            }
        }
    }

    fn reveal_window(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    }

    fn refresh_status(&mut self) {
        if self.last_refresh.elapsed() < Duration::from_millis(750) {
            return;
        }
        self.last_refresh = Instant::now();

        let mut lines = vec![format!("Version {}", env!("CARGO_PKG_VERSION"))];
        lines.push(state::cloud_connection_line());

        if crate::config::read_saved_agent_token().is_some() {
            lines.push("Token: set".to_string());
        } else {
            lines.push("Token: not set".to_string());
        }

        let service_line = match service::background_status() {
            service::BackgroundStatus::Running => "Agent: running",
            service::BackgroundStatus::Stopped => "Agent: stopped (will start at logon)",
            service::BackgroundStatus::NotInstalled => "Agent: not configured",
        };
        lines.push(service_line.to_string());

        #[cfg(windows)]
        if let Some(method) = service::autostart_method_line() {
            lines.push(format!("Autostart: {method}"));
        }

        if let Ok(bin) = install_dir() {
            lines.push(format!("Bin: {}", bin.display()));
        }
        if let Ok(lib) = lib_dir() {
            lines.push(format!("Lib: {}", lib.display()));
        }
        if let Some(log) = self.log_path.as_ref() {
            lines.push(format!("Log: {}", log.display()));
        }

        if let Some(summary) = state::agent_activity_summary() {
            lines.push(format!("Status: {}", summary.status));
            if let Some(node) = summary.node_id {
                lines.push(format!("Node: {node}"));
            }
        }

        self.status_lines = lines;
    }

    fn refresh_logs(&mut self) {
        let Some(path) = self.log_path.as_ref() else {
            return;
        };
        let Ok(meta) = std::fs::metadata(path) else {
            self.logs = "Log file not created yet. The agent may still be starting.".to_string();
            return;
        };
        let len = meta.len();
        if len < self.log_offset {
            self.log_offset = 0;
        }
        if len == self.log_offset {
            return;
        }

        let read_from = if len > self.log_offset {
            self.log_offset
        } else {
            0
        };

        let Ok(raw) = std::fs::read(path) else {
            return;
        };
        let slice = &raw[read_from as usize..];
        let chunk = String::from_utf8_lossy(slice);
        self.logs.push_str(&chunk);
        if self.logs.len() > 48_000 {
            let drop_by = self.logs.len().saturating_sub(40_000);
            self.logs.drain(..drop_by);
        }
        self.log_offset = len;
    }

    fn save_token(&mut self) {
        let token = self.token_input.trim().to_string();
        if !token.starts_with("slt_provider_") {
            self.action_message = "Token must start with slt_provider_".to_string();
            return;
        }

        match AgentConfig::from_env_and_cli(Some(token.clone())) {
            Ok(config) => {
                if let Err(err) = service::persist_agent_token(&config.token) {
                    self.action_message = format!("Could not save token: {err}");
                    return;
                }
                match service::restart_background_from_config(&config) {
                    Ok(()) => {
                        self.action_message =
                            "Token saved. Background agent restarted.".to_string();
                    }
                    Err(err) => {
                        self.action_message = format!(
                            "Token saved. Background restart issue: {err}"
                        );
                    }
                }
            }
            Err(err) => self.action_message = err.to_string(),
        }
    }

    fn open_dashboard(&mut self) {
        if let Err(err) = open_dashboard_url(DASHBOARD_URL) {
            self.action_message = format!("Could not open dashboard: {err}");
            return;
        }
        self.action_message = "Opened provider dashboard in your browser.".to_string();
    }
}

impl eframe::App for TrayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        self.poll_tray(ctx);
        self.refresh_status();
        self.refresh_logs();

        let panel_fill = egui::Color32::from_rgb(17, 17, 17);
        let border = egui::Color32::from_rgba_premultiplied(255, 255, 255, 31);
        let muted = egui::Color32::from_rgba_premultiplied(255, 255, 255, 140);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(egui::Color32::BLACK)
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ctx, |ui| {
                ui.heading(
                    egui::RichText::new("Scalattice Agent")
                        .color(egui::Color32::WHITE)
                        .size(22.0),
                );
                ui.label(egui::RichText::new("Provider machine control panel").color(muted));
                ui.add_space(10.0);

                ui.columns(2, |columns| {
                    columns[0].set_min_width(300.0);
                    columns[0].set_max_width(320.0);

                    columns[0].group(|ui| {
                        egui::Frame::group(ui.style())
                            .fill(panel_fill)
                            .stroke(egui::Stroke::new(1.0, border))
                            .inner_margin(egui::Margin::same(12))
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new("Status & token")
                                        .strong()
                                        .color(egui::Color32::WHITE),
                                );
                                ui.add_space(6.0);

                                for line in &self.status_lines {
                                    ui.label(
                                        egui::RichText::new(line)
                                            .size(13.0)
                                            .color(egui::Color32::from_rgb(220, 220, 220)),
                                    );
                                }

                                ui.add_space(10.0);
                                ui.separator();
                                ui.add_space(8.0);

                                ui.label(
                                    egui::RichText::new("Provider token")
                                        .strong()
                                        .color(egui::Color32::WHITE),
                                );
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.token_input)
                                        .desired_width(f32::INFINITY)
                                        .hint_text("slt_provider_…"),
                                );

                                ui.horizontal(|ui| {
                                    if ui.button("Save token").clicked() {
                                        self.save_token();
                                    }
                                    if ui.button("Open dashboard").clicked() {
                                        self.open_dashboard();
                                    }
                                });

                                if !self.action_message.is_empty() {
                                    ui.add_space(6.0);
                                    ui.label(
                                        egui::RichText::new(&self.action_message)
                                            .color(egui::Color32::from_rgb(99, 179, 237)),
                                    );
                                }
                            });
                    });

                    columns[1].group(|ui| {
                        egui::Frame::group(ui.style())
                            .fill(panel_fill)
                            .stroke(egui::Stroke::new(1.0, border))
                            .inner_margin(egui::Margin::same(12))
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new("Live log")
                                        .strong()
                                        .color(egui::Color32::WHITE),
                                );
                                ui.add_space(4.0);
                                egui::ScrollArea::vertical()
                                    .stick_to_bottom(true)
                                    .auto_shrink([false; 2])
                                    .max_height(ui.available_height() - 8.0)
                                    .show(ui, |ui| {
                                        ui.set_max_width(ui.available_width());
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(&self.logs)
                                                    .monospace()
                                                    .size(12.0)
                                                    .color(egui::Color32::from_rgb(200, 200, 200)),
                                            )
                                            .wrap(),
                                        );
                                    });
                            });
                    });
                });

                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(
                        "Closing this window hides the panel — the tray icon and background agent keep running.",
                    )
                    .size(12.0)
                    .color(muted),
                );
            });

        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

fn scalattice_visuals() -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = egui::Color32::BLACK;
    visuals.window_fill = egui::Color32::BLACK;
    visuals.extreme_bg_color = egui::Color32::BLACK;
    visuals.faint_bg_color = egui::Color32::from_rgb(17, 17, 17);
    visuals.widgets.noninteractive.fg_stroke.color =
        egui::Color32::from_rgba_premultiplied(255, 255, 255, 180);
    visuals.widgets.inactive.fg_stroke.color = egui::Color32::WHITE;
    visuals.widgets.hovered.fg_stroke.color = egui::Color32::WHITE;
    visuals.widgets.active.fg_stroke.color = egui::Color32::WHITE;
    visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(26, 26, 26);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(38, 38, 38);
    visuals.widgets.active.bg_fill = egui::Color32::from_rgb(48, 48, 48);
    visuals.selection.bg_fill = egui::Color32::from_rgb(99, 179, 237);
    visuals
}

fn load_viewport_icon() -> egui::IconData {
    let rgba = decode_icon_rgba();
    egui::IconData {
        width: rgba.0,
        height: rgba.1,
        rgba: rgba.2,
    }
}

fn decode_icon_rgba() -> (u32, u32, Vec<u8>) {
    let bytes = include_bytes!("../../installer/windows/scalattice.ico");
    let image = image::load_from_memory(bytes).expect("tray icon bytes");
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    (width, height, rgba.into_raw())
}

fn load_tray_icon() -> Result<Icon> {
    let (width, height, rgba) = decode_icon_rgba();
    Icon::from_rgba(rgba, width, height).context("failed to build tray icon")
}

/// Returns `true` when this process should start the tray UI; `false` when an existing instance was activated.
fn acquire_tray_instance(force: bool) -> Result<bool> {
    match try_acquire_tray_mutex() {
        Ok(true) => return Ok(true),
        Ok(false) => {
            write_tray_log("second tray launch — activating existing window");
            if activate_tray_window() {
                return Ok(false);
            }
            if force {
                clear_stale_tray_pid()?;
                if try_acquire_tray_mutex()? {
                    return Ok(true);
                }
            }
            anyhow::bail!(
                "Scalattice tray is already running but its window could not be found. \
                 Stop extra scalattice-agent tray processes or run: scalattice-agent tray --force"
            );
        }
        Err(err) => return Err(err),
    }
}

fn try_acquire_tray_mutex() -> Result<bool> {
    let name: Vec<u16> = format!("{TRAY_MUTEX}\0").encode_utf16().collect();
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if handle.is_null() {
            anyhow::bail!("failed to create tray instance lock");
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(handle);
            return Ok(false);
        }
    }
    Ok(true)
}

fn tray_pid_path() -> Result<PathBuf> {
    Ok(install_dir()?.join("tray.pid"))
}

fn write_tray_pid() -> Result<()> {
    let path = tray_pid_path()?;
    std::fs::write(path, std::process::id().to_string())?;
    Ok(())
}

fn clear_tray_pid() {
    if let Ok(path) = tray_pid_path() {
        let _ = std::fs::remove_file(path);
    }
}

fn clear_stale_tray_pid() -> Result<()> {
    let path = tray_pid_path()?;
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let Ok(pid) = raw.trim().parse::<u32>() else {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    };
    if pid != std::process::id() && process_exists(pid) {
        write_tray_log(&format!("stopping stale tray pid {pid}"));
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .status();
        std::thread::sleep(Duration::from_millis(400));
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

fn process_exists(pid: u32) -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout).contains(&pid.to_string())
        })
        .unwrap_or(false)
}

fn launched_hidden() -> bool {
    std::env::var("SCALATTICE_TRAY_HIDDEN")
        .ok()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

fn has_attached_console() -> bool {
    unsafe {
        !windows_sys::Win32::System::Console::GetConsoleWindow().is_null()
    }
}

fn maybe_detach_console() {
    if launched_hidden() {
        detach_console();
    }
}

fn activate_tray_window() -> bool {
    let title: Vec<u16> = format!("{WINDOW_TITLE}\0").encode_utf16().collect();
    unsafe {
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if hwnd.is_null() {
            return false;
        }
        ShowWindow(hwnd, SW_RESTORE);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        true
    }
}

fn write_tray_log(message: &str) {
    let Ok(agent_log) = agent_log_path() else {
        return;
    };
    let Some(parent) = agent_log.parent() else {
        return;
    };
    let log_path = parent.join("tray.log");
    let timestamp = chrono_lite_timestamp();
    let line = format!("{timestamp} {message}\n");
    let _ = std::fs::create_dir_all(parent);
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .and_then(|mut file| {
            use std::io::Write;
            file.write_all(line.as_bytes())
        });
}

fn chrono_lite_timestamp() -> String {
    use std::time::SystemTime;
    let Ok(duration) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) else {
        return "0".to_string();
    };
    format!("{}", duration.as_secs())
}

fn detach_console() {
    unsafe {
        windows_sys::Win32::System::Console::FreeConsole();
    }
}

fn open_dashboard_url(url: &str) -> Result<()> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::{ShellExecuteW, SW_SHOW};

    let url_wide: Vec<u16> = OsStr::new(url).encode_wide().chain(Some(0)).collect();
    let op: Vec<u16> = "open\0".encode_utf16().collect();
    unsafe {
        let rc = ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            url_wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOW,
        );
        if (rc as isize) <= 32 {
            anyhow::bail!("ShellExecute failed ({rc})");
        }
    }
    Ok(())
}
