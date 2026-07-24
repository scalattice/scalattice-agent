#[cfg(windows)]
mod ui;
#[cfg(windows)]
mod notify;

#[cfg(windows)]
pub fn open_panel(force: bool) -> anyhow::Result<()> {
    if !force && ui::activate_existing_panel() {
        println!("Scalattice tray panel activated.");
        return Ok(());
    }
    ui::run_tray_ui(force)
}
