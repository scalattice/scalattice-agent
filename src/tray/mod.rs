#[cfg(windows)]
mod notify;
#[cfg(windows)]
mod ui;

#[cfg(windows)]
pub fn open_panel(force: bool, open: bool) -> anyhow::Result<()> {
    if !force && ui::tray_window_exists() {
        // Autostart / second launch: never raise the panel unless explicitly requested.
        if open {
            let _ = ui::activate_existing_panel();
        }
        return Ok(());
    }
    ui::run_tray_ui(force, open)
}
