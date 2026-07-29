//! Windows toast notifications for the tray control panel.

use crate::paths::install_dir;
use std::path::PathBuf;
use tauri_winrt_notification::{Duration, IconCrop, Toast};

/// Must match the Inno Setup `AppUserModelID` on the Start Menu shortcut.
pub const APP_USER_MODEL_ID: &str = "RobottikSoftware.Scalattice.Agent";

/// Tell Windows this process owns the Scalattice AUMID so toasts show the right app name/icon.
pub fn set_process_app_id() {
    let wide: Vec<u16> = APP_USER_MODEL_ID
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
        let _ = SetCurrentProcessExplicitAppUserModelID(wide.as_ptr());
    }
}

/// Best-effort desktop toast. Failures are ignored (notifications are optional UX).
pub fn show(title: &str, body: &str) {
    let mut toast = Toast::new(APP_USER_MODEL_ID)
        .title(title)
        .text1(body)
        .duration(Duration::Short);
    if let Some(icon) = toast_icon_path() {
        toast = toast.icon(&icon, IconCrop::Square, "Scalattice");
    }
    let _ = toast.show();
}

fn toast_icon_path() -> Option<PathBuf> {
    let dir = install_dir().ok()?;
    for name in ["scalattice.ico", "scalattice-agent.ico", "app.ico", "icon.ico"] {
        let path = dir.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}
