use crate::config::AgentConfig;
use crate::paths::{agent_log_path, install_dir, lib_dir};
use crate::service;
use crate::state;
use anyhow::{Context, Result};
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
use windows_sys::Win32::System::Threading::CreateMutexW;

const DASHBOARD_URL: &str = "https://scalattice.cloud/providers";

pub fn run_tray_ui() -> Result<()> {
    detach_console();
    ensure_single_tray_instance()?;

    let icon = load_tray_icon()?;
    let (event_tx, event_rx) = mpsc::channel();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = event_tx.send(event);
    }));

    let _tray = TrayIconBuilder::new()
        .with_tooltip("Scalattice Agent — click to open")
        .with_icon(icon)
        .build()
        .context("failed to create tray icon")?;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 520.0])
            .with_min_inner_size([560.0, 400.0])
            .with_title("Scalattice Agent")
            .with_visible(false),
        ..Default::default()
    };

    eframe::run_native(
        "Scalattice Agent",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::light());
            Ok(Box::new(TrayApp::new(event_rx)))
        }),
    )
    .map_err(|err| anyhow::anyhow!("tray UI exited: {err}"))
}

struct TrayApp {
    event_rx: mpsc::Receiver<TrayIconEvent>,
    token_input: String,
    status_lines: Vec<String>,
    action_message: String,
    logs: String,
    show_window: bool,
    log_path: Option<PathBuf>,
    log_offset: u64,
    last_refresh: Instant,
}

impl TrayApp {
    fn new(event_rx: mpsc::Receiver<TrayIconEvent>) -> Self {
        let token_input = crate::config::read_saved_agent_token().unwrap_or_default();
        let log_path = agent_log_path().ok();
        Self {
            event_rx,
            token_input,
            status_lines: Vec::new(),
            action_message: String::new(),
            logs: String::new(),
            show_window: false,
            log_path,
            log_offset: 0,
            last_refresh: Instant::now() - Duration::from_secs(1),
        }
    }

    fn poll_tray(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.event_rx.try_recv() {
            if matches!(
                event,
                TrayIconEvent::Click { .. } | TrayIconEvent::DoubleClick { .. }
            ) {
                self.show_window = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }
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
                match service::start_background_from_config(&config) {
                    Ok(()) => {
                        self.action_message =
                            "Token saved. Background agent started.".to_string();
                        self.log_offset = 0;
                        self.logs.clear();
                    }
                    Err(err) => {
                        self.action_message = format!(
                            "Token saved. Start issue (agent may still run at logon): {err}"
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
        self.poll_tray(ctx);
        self.refresh_status();
        self.refresh_logs();

        if !self.show_window {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        let panel_fill = egui::Color32::from_rgb(250, 250, 252);
        let border = egui::Color32::from_rgb(220, 222, 228);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(egui::Color32::WHITE)
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ctx, |ui| {
                ui.heading("Scalattice Agent");
                ui.label(
                    egui::RichText::new("Provider machine control panel")
                        .color(egui::Color32::from_rgb(90, 90, 95)),
                );
                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    let left_width = ui.available_width() * 0.42;
                    let right_width = ui.available_width();

                    ui.allocate_ui_with_layout(
                        egui::vec2(left_width, ui.available_height()),
                        egui::Layout::top_down(egui::Align::LEFT),
                        |ui| {
                            egui::Frame::group(ui.style())
                                .fill(panel_fill)
                                .stroke(egui::Stroke::new(1.0, border))
                                .inner_margin(egui::Margin::same(12))
                                .show(ui, |ui| {
                                    ui.label(egui::RichText::new("Status & token").strong());
                                    ui.add_space(6.0);

                                    for line in &self.status_lines {
                                        ui.label(
                                            egui::RichText::new(line)
                                                .size(13.0)
                                                .family(egui::FontFamily::Proportional),
                                        );
                                    }

                                    ui.add_space(10.0);
                                    ui.separator();
                                    ui.add_space(8.0);

                                    ui.label(egui::RichText::new("Provider token").strong());
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
                                                .color(egui::Color32::from_rgb(40, 90, 160)),
                                        );
                                    }
                                });
                        },
                    );

                    ui.add_space(8.0);

                    ui.allocate_ui_with_layout(
                        egui::vec2(right_width, ui.available_height()),
                        egui::Layout::top_down(egui::Align::LEFT),
                        |ui| {
                            egui::Frame::group(ui.style())
                                .fill(panel_fill)
                                .stroke(egui::Stroke::new(1.0, border))
                                .inner_margin(egui::Margin::same(12))
                                .show(ui, |ui| {
                                    ui.label(egui::RichText::new("Live log").strong());
                                    ui.add_space(4.0);
                                    egui::ScrollArea::vertical()
                                        .stick_to_bottom(true)
                                        .auto_shrink([false; 2])
                                        .show(ui, |ui| {
                                            ui.add(
                                                egui::TextEdit::multiline(&mut self.logs)
                                                    .desired_width(f32::INFINITY)
                                                    .desired_rows(18)
                                                    .interactive(false)
                                                    .font(egui::TextStyle::Monospace),
                                            );
                                        });
                                });
                        },
                    );
                });

                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(
                        "Click the notification area icon to hide this window. The agent keeps running in the background.",
                    )
                    .size(12.0)
                    .color(egui::Color32::from_rgb(110, 110, 115)),
                );
            });

        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

fn load_tray_icon() -> Result<Icon> {
    let bytes = include_bytes!("../../installer/windows/scalattice.ico");
    let image = image::load_from_memory(bytes).context("failed to decode tray icon")?;
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    Icon::from_rgba(rgba.into_raw(), width, height).context("failed to build tray icon")
}

fn ensure_single_tray_instance() -> Result<()> {
    let name: Vec<u16> = "ScalatticeAgentTray\0".encode_utf16().collect();
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if handle.is_null() {
            anyhow::bail!("failed to create tray instance lock");
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(handle);
            anyhow::bail!("Scalattice tray is already running");
        }
    }
    Ok(())
}

fn detach_console() {
    unsafe {
        windows_sys::Win32::System::Console::FreeConsole();
    }
}

fn open_dashboard_url(url: &str) -> Result<()> {
    std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .context("failed to open browser")?;
    Ok(())
}
