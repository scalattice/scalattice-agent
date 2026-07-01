use crate::config::AgentConfig;
use crate::paths::{agent_log_path, install_dir};
use crate::service;
use crate::settings::UserSettings;
use crate::state;
use crate::update::{self, UpdateCheckOutcome};
use anyhow::{Context, Result};
use eframe::egui;
use std::io::{Read, Seek, SeekFrom};
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
    FindWindowW, IsWindowVisible, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
};

const DASHBOARD_URL: &str = "https://scalattice.cloud/providers";
const WINDOW_TITLE: &str = "Scalattice Agent";
const TRAY_MUTEX: &str = "ScalatticeAgentTray";

enum UpdateWorkerCmd {
    Check,
    Install,
}

enum UpdateWorkerMsg {
    Checked(UpdateCheckOutcome),
    CheckFailed(String),
    InstallFailed(String),
}

fn spawn_update_worker() -> (mpsc::Sender<UpdateWorkerCmd>, mpsc::Receiver<UpdateWorkerMsg>) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<UpdateWorkerCmd>();
    let (msg_tx, msg_rx) = mpsc::channel::<UpdateWorkerMsg>();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(err) => {
                let _ = msg_tx.send(UpdateWorkerMsg::CheckFailed(err.to_string()));
                return;
            }
        };
        while let Ok(cmd) = cmd_rx.recv() {
            match cmd {
                UpdateWorkerCmd::Check => match rt.block_on(update::check_for_update()) {
                    Ok(outcome) => {
                        if msg_tx.send(UpdateWorkerMsg::Checked(outcome)).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        if msg_tx
                            .send(UpdateWorkerMsg::CheckFailed(err.to_string()))
                            .is_err()
                        {
                            break;
                        }
                    }
                },
                UpdateWorkerCmd::Install => {
                    if let Err(err) = rt.block_on(update::install_latest_update()) {
                        let _ = msg_tx.send(UpdateWorkerMsg::InstallFailed(err.to_string()));
                    }
                }
            }
        }
    });
    (cmd_tx, msg_rx)
}

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

    let (show_tx, show_rx) = mpsc::channel();
    let show_for_menu = show_tx.clone();

    let (event_tx, event_rx) = mpsc::channel();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = event_tx.send(event);
    }));

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
    let show_for_menu_flag = show_window.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == open_id {
            show_for_menu_flag.store(true, Ordering::SeqCst);
            let _ = show_for_menu.send(());
            activate_tray_window();
        } else if event.id == quit_id {
            write_tray_log("tray quit from menu");
            std::process::exit(0);
        }
    }));

    let _tray = TrayIconBuilder::new()
        .with_tooltip("Scalattice Agent: click to open")
        .with_icon(icon)
        .with_menu(Box::new(tray_menu))
        .build()
        .context("failed to create tray icon")?;

    write_tray_log("tray started");

    std::thread::spawn(|| {
        match service::ensure_background_running_if_configured() {
            Ok(()) => {
                if service::service_active() {
                    write_tray_log("background agent auto-started");
                }
            }
            Err(err) => write_tray_log(&format!("background auto-start failed: {err:#}")),
        }
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([640.0, 680.0])
            .with_min_inner_size([520.0, 560.0])
            .with_title(WINDOW_TITLE)
            .with_visible(interactive)
            .with_icon(viewport_icon),
        ..Default::default()
    };

    let show_for_app = show_window.clone();

    let (status_tx, status_rx) = mpsc::channel();
    let (status_req_tx, status_req_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        while status_req_rx.recv().is_ok() {
            let lines = gather_status_lines();
            if status_tx.send(lines).is_err() {
                break;
            }
        }
    });

    let (update_cmd_tx, update_msg_rx) = spawn_update_worker();

    eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(scalattice_visuals());
            Ok(Box::new(TrayApp::new(
                event_rx,
                show_rx,
                show_for_app,
                status_rx,
                status_req_tx,
                update_cmd_tx,
                update_msg_rx,
                !interactive,
            )))
        }),
    )
    .map_err(|err| anyhow::anyhow!("tray UI exited: {err}"))?;

    clear_tray_pid();
    Ok(())
}

struct TrayApp {
    event_rx: mpsc::Receiver<TrayIconEvent>,
    show_rx: mpsc::Receiver<()>,
    show_window: Arc<AtomicBool>,
    status_rx: mpsc::Receiver<Vec<String>>,
    status_req_tx: mpsc::Sender<()>,
    status_refresh_inflight: bool,
    update_cmd_tx: mpsc::Sender<UpdateWorkerCmd>,
    update_msg_rx: mpsc::Receiver<UpdateWorkerMsg>,
    update_check_inflight: bool,
    update_busy: bool,
    update_available: bool,
    latest_version: Option<String>,
    update_notice: String,
    settings: UserSettings,
    next_update_check: Instant,
    panel_hidden: bool,
    token_input: String,
    token_revealed: bool,
    status_lines: Vec<String>,
    action_message: String,
    logs: String,
    log_path: Option<PathBuf>,
    log_offset: u64,
    next_data_poll: Instant,
    last_outer_rect: Option<egui::Rect>,
}

impl TrayApp {
    fn new(
        event_rx: mpsc::Receiver<TrayIconEvent>,
        show_rx: mpsc::Receiver<()>,
        show_window: Arc<AtomicBool>,
        status_rx: mpsc::Receiver<Vec<String>>,
        status_req_tx: mpsc::Sender<()>,
        update_cmd_tx: mpsc::Sender<UpdateWorkerCmd>,
        update_msg_rx: mpsc::Receiver<UpdateWorkerMsg>,
        panel_hidden: bool,
    ) -> Self {
        let token_input = crate::config::read_saved_agent_token().unwrap_or_default();
        let token_revealed = token_input.is_empty();
        let log_path = agent_log_path().ok();
        let settings = UserSettings::load();
        let should_save_defaults = match crate::paths::settings_path() {
            Ok(path) => !path.is_file(),
            Err(_) => true,
        };
        if should_save_defaults {
            let _ = settings.save();
        }
        let should_check_now = settings.should_check_for_update();
        let next_update_check = if should_check_now {
            Instant::now()
        } else {
            Instant::now() + Duration::from_secs(settings.seconds_until_update_check())
        };
        let mut app = Self {
            event_rx,
            show_rx,
            show_window,
            status_rx,
            status_req_tx,
            status_refresh_inflight: false,
            update_cmd_tx,
            update_msg_rx,
            update_check_inflight: false,
            update_busy: false,
            update_available: false,
            latest_version: None,
            update_notice: String::new(),
            settings,
            next_update_check,
            panel_hidden,
            token_input,
            token_revealed,
            status_lines: Vec::new(),
            action_message: String::new(),
            logs: String::new(),
            log_path,
            log_offset: 0,
            next_data_poll: Instant::now(),
            last_outer_rect: None,
        };
        if should_check_now {
            app.kick_update_check();
        }
        app
    }

    fn poll_tray(&mut self, ctx: &egui::Context) {
        while self.show_rx.try_recv().is_ok() {
            self.reveal_window(ctx);
        }

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

    fn reveal_window(&mut self, ctx: &egui::Context) {
        self.panel_hidden = false;
        self.next_data_poll = Instant::now();
        activate_tray_window();
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        self.kick_status_refresh();
        ctx.request_repaint();
    }

    fn kick_status_refresh(&mut self) {
        if self.status_refresh_inflight {
            return;
        }
        if self.status_req_tx.send(()).is_ok() {
            self.status_refresh_inflight = true;
        }
    }

    fn kick_update_check(&mut self) {
        if self.update_check_inflight || self.update_busy {
            return;
        }
        if self.update_cmd_tx.send(UpdateWorkerCmd::Check).is_ok() {
            self.update_check_inflight = true;
            self.update_notice = "Checking for updates…".to_string();
        }
    }

    fn schedule_next_update_check(&mut self) {
        self.next_update_check =
            Instant::now() + Duration::from_secs(self.settings.seconds_until_update_check());
    }

    fn start_update_install(&mut self) {
        if self.update_busy {
            return;
        }
        self.update_busy = true;
        self.update_notice =
            "Downloading and installing update. This panel will close shortly.".to_string();
        self.action_message.clear();
        let _ = self.update_cmd_tx.send(UpdateWorkerCmd::Install);
    }

    fn poll_update_results(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.update_msg_rx.try_recv() {
            match msg {
                UpdateWorkerMsg::Checked(outcome) => {
                    self.update_check_inflight = false;
                    self.update_available = outcome.info().update_available;
                    self.latest_version = Some(outcome.info().latest_version.clone());
                    self.settings.mark_update_checked();
                    let _ = self.settings.save();
                    self.update_notice = update::format_update_status(&outcome);
                    self.schedule_next_update_check();

                    if self.settings.auto_update && self.update_available {
                        write_tray_log(&format!(
                            "auto-update: installing v{}",
                            outcome.info().latest_version
                        ));
                        self.start_update_install();
                    }
                    ctx.request_repaint();
                }
                UpdateWorkerMsg::CheckFailed(err) => {
                    self.update_check_inflight = false;
                    write_tray_log(&format!("update check failed: {err}"));
                    self.settings.mark_update_checked();
                    let _ = self.settings.save();
                    self.update_notice = format!("Update check failed: {err}");
                    self.schedule_next_update_check();
                }
                UpdateWorkerMsg::InstallFailed(err) => {
                    self.update_busy = false;
                    write_tray_log(&format!("update install failed: {err}"));
                    self.update_notice = format!("Update failed: {err}");
                    self.action_message.clear();
                    ctx.request_repaint();
                }
            }
        }
    }

    fn save_settings_if_needed(&mut self, prev: &UserSettings) {
        if self.settings.auto_update != prev.auto_update {
            let _ = self.settings.save();
            self.schedule_next_update_check();
        }
    }

    fn poll_status_results(&mut self, ctx: &egui::Context) {
        while let Ok(lines) = self.status_rx.try_recv() {
            self.status_refresh_inflight = false;
            if lines != self.status_lines {
                self.status_lines = lines;
                ctx.request_repaint();
            }
        }
    }

    fn window_in_motion(&mut self, ctx: &egui::Context) -> bool {
        let outer = ctx.input(|i| i.viewport().outer_rect);
        let moving = matches!(
            (self.last_outer_rect, outer),
            (Some(prev), Some(cur)) if prev != cur
        );
        self.last_outer_rect = outer;
        moving
    }

    fn refresh_logs(&mut self) -> bool {
        let Some(path) = self.log_path.as_ref() else {
            return false;
        };
        let Ok(meta) = std::fs::metadata(path) else {
            let message = "Log file not created yet. The agent may still be starting.".to_string();
            if self.logs == message {
                return false;
            }
            self.logs = message;
            return true;
        };
        let len = meta.len();
        if len < self.log_offset {
            self.log_offset = 0;
        }
        if len == self.log_offset {
            return false;
        }

        let read_from = if len > self.log_offset {
            self.log_offset
        } else {
            0
        };

        let Ok(mut file) = std::fs::File::open(path) else {
            return false;
        };
        if file.seek(SeekFrom::Start(read_from)).is_err() {
            return false;
        }
        let to_read = (len - read_from) as usize;
        let mut buf = vec![0u8; to_read];
        if file.read_exact(&mut buf).is_err() {
            return false;
        }
        let chunk = strip_ansi_escapes(&String::from_utf8_lossy(&buf));
        self.logs.push_str(&chunk);
        if self.logs.len() > 48_000 {
            let drop_by = self.logs.len().saturating_sub(40_000);
            self.logs.drain(..drop_by);
        }
        self.log_offset = len;
        true
    }

    fn save_token(&mut self) {
        let token = self.token_input.trim().to_string();
        if !token.starts_with("slt_provider_") {
            self.action_message = "Token must start with slt_provider_".to_string();
            return;
        }

        let config = match AgentConfig::from_env_and_cli(Some(token)) {
            Ok(config) => config,
            Err(err) => {
                self.action_message = err.to_string();
                return;
            }
        };

        self.token_revealed = false;
        match service::save_agent_token(&config) {
            Ok(()) => {
                self.action_message = "Token saved. Reconnecting…".to_string();
            }
            Err(err) => {
                self.action_message = format!("Could not save token: {err}");
            }
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
            self.panel_hidden = true;
        }

        self.poll_tray(ctx);
        self.poll_update_results(ctx);

        if Instant::now() >= self.next_update_check
            && !self.update_check_inflight
            && !self.update_busy
        {
            self.kick_update_check();
        }

        let native_visible = native_window_visible();
        if !native_visible {
            self.panel_hidden = true;
            ctx.request_repaint_after(Duration::from_millis(400));
            return;
        }

        if self.panel_hidden {
            self.panel_hidden = false;
            self.next_data_poll = Instant::now();
            self.kick_status_refresh();
        }

        self.poll_status_results(ctx);

        let window_moving = self.window_in_motion(ctx);
        let pointer_busy = ctx.input(|i| i.pointer.any_down());

        if self.status_lines.is_empty() && !self.status_refresh_inflight {
            self.kick_status_refresh();
        }

        if !window_moving && !pointer_busy && Instant::now() >= self.next_data_poll {
            self.next_data_poll = Instant::now() + Duration::from_secs(3);
            self.kick_status_refresh();
            if self.refresh_logs() {
                ctx.request_repaint();
            }
        }

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
                        ui.add_space(8.0);

                        for line in &self.status_lines {
                            render_status_line(ui, line);
                        }

                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(8.0);

                        ui.label(
                            egui::RichText::new("Provider token")
                                .strong()
                                .color(egui::Color32::WHITE),
                        );
                        ui.horizontal(|ui| {
                            let token_empty = self.token_input.is_empty();
                            let mut edit = egui::TextEdit::singleline(&mut self.token_input)
                                .desired_width(ui.available_width() - 72.0)
                                .hint_text("slt_provider_…");
                            if !self.token_revealed && !token_empty {
                                edit = edit.password(true);
                            }
                            ui.add(edit);
                            if !token_empty {
                                let label = if self.token_revealed { "Hide" } else { "Show" };
                                if ui.button(label).clicked() {
                                    self.token_revealed = !self.token_revealed;
                                }
                            }
                        });

                        ui.horizontal(|ui| {
                            if ui.button("Save token").clicked() {
                                self.save_token();
                            }
                            if ui.button("Open dashboard").clicked() {
                                self.open_dashboard();
                            }
                        });

                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(8.0);

                        ui.label(
                            egui::RichText::new("Updates")
                                .strong()
                                .color(egui::Color32::WHITE),
                        );
                        ui.add_space(4.0);

                        let prev_settings = self.settings.clone();
                        ui.checkbox(
                            &mut self.settings.auto_update,
                            "Automatically install updates",
                        );
                        self.save_settings_if_needed(&prev_settings);

                        let update_label = if self.update_busy {
                            "Updating…".to_string()
                        } else if self.update_check_inflight {
                            "Checking…".to_string()
                        } else if self.update_available {
                            format!(
                                "Install v{}",
                                self.latest_version.as_deref().unwrap_or("latest")
                            )
                        } else {
                            "Check for updates".to_string()
                        };
                        ui.horizontal(|ui| {
                            let button =
                                egui::Button::new(update_label).min_size(egui::vec2(140.0, 0.0));
                            if ui
                                .add_enabled(!self.update_busy && !self.update_check_inflight, button)
                                .clicked()
                            {
                                if self.update_available {
                                    self.start_update_install();
                                } else {
                                    self.kick_update_check();
                                }
                            }
                            if self.update_available && !self.update_busy && !self.update_check_inflight {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "v{} available",
                                        self.latest_version.as_deref().unwrap_or("?")
                                    ))
                                    .color(egui::Color32::from_rgb(120, 200, 140)),
                                );
                            }
                        });

                        if !self.update_notice.is_empty() {
                            ui.add_space(6.0);
                            let notice_color = if self.update_notice.contains("failed") {
                                egui::Color32::from_rgb(255, 140, 140)
                            } else if self.update_available {
                                egui::Color32::from_rgb(120, 200, 140)
                            } else {
                                egui::Color32::from_rgb(99, 179, 237)
                            };
                            ui.label(
                                egui::RichText::new(&self.update_notice)
                                    .size(13.0)
                                    .color(notice_color),
                            );
                        }

                        if !self.action_message.is_empty() {
                            ui.add_space(6.0);
                            ui.label(
                                egui::RichText::new(&self.action_message)
                                    .color(egui::Color32::from_rgb(99, 179, 237)),
                            );
                        }
                    });

                ui.add_space(12.0);

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
                            .max_height(280.0)
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

        if !window_moving && !pointer_busy {
            let until_poll = self
                .next_data_poll
                .saturating_duration_since(Instant::now());
            if !until_poll.is_zero() {
                ctx.request_repaint_after(until_poll);
            }
        }
    }
}

fn gather_status_lines() -> Vec<String> {
    let mut lines = vec![format!("Version {}", env!("CARGO_PKG_VERSION"))];
    lines.push(state::cloud_connection_line());

    if crate::config::read_saved_agent_token().is_some() {
        lines.push("Token: set".to_string());
    } else {
        lines.push("Token: not set".to_string());
    }

    let service_line = match service::background_status() {
        service::BackgroundStatus::Running => "Agent: running",
        service::BackgroundStatus::Stopped => "Agent: stopped · starts when you sign in",
        service::BackgroundStatus::NotInstalled => "Agent: not set up yet",
    };
    lines.push(service_line.to_string());

    if let Some(summary) = state::agent_activity_summary() {
        lines.push(format!("Status: {}", summary.status));
        if let Some(node) = summary.node_id {
            lines.push(format!("Node: {node}"));
        }
    }

    lines
}

fn render_status_line(ui: &mut egui::Ui, line: &str) {
    let body = egui::Color32::from_rgb(220, 220, 220);
    let emphasis = egui::Color32::WHITE;

    if let Some(rest) = line.strip_prefix("Scalattice Cloud: ") {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new("Scalattice Cloud:")
                    .strong()
                    .size(13.0)
                    .color(emphasis),
            );
            ui.label(
                egui::RichText::new(rest)
                    .strong()
                    .size(13.0)
                    .color(body),
            );
        });
    } else if let Some(rest) = line.strip_prefix("Status: ") {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new("Status:")
                    .strong()
                    .size(13.0)
                    .color(emphasis),
            );
            ui.label(
                egui::RichText::new(rest)
                    .strong()
                    .size(13.0)
                    .color(body),
            );
        });
    } else if let Some(rest) = line.strip_prefix("Node: ") {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new("Node:")
                    .strong()
                    .size(13.0)
                    .color(emphasis),
            );
            ui.label(
                egui::RichText::new(rest)
                    .strong()
                    .size(13.0)
                    .color(egui::Color32::from_rgb(120, 200, 140)),
            );
        });
    } else if let Some((label, rest)) = line.split_once(": ") {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new(format!("{label}:"))
                    .size(13.0)
                    .color(body),
            );
            ui.label(egui::RichText::new(rest).size(13.0).color(body));
        });
    } else {
        ui.label(egui::RichText::new(line).size(13.0).color(body));
    }
    ui.add_space(5.0);
}

fn native_window_visible() -> bool {
    let title: Vec<u16> = format!("{WINDOW_TITLE}\0").encode_utf16().collect();
    unsafe {
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        !hwnd.is_null() && IsWindowVisible(hwnd) != 0
    }
}

fn strip_ansi_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.next_if_eq(&'[').is_some() {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
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
            write_tray_log("second tray launch: activating existing window");
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
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOW;

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
        let code = rc as isize;
        if code <= 32 {
            anyhow::bail!("ShellExecute failed (code {code})");
        }
    }
    Ok(())
}
