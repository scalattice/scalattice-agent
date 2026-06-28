#[cfg(windows)]
mod ui;

#[cfg(windows)]
pub fn run() -> anyhow::Result<()> {
    ui::run_tray_ui()
}
