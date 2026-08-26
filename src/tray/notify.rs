//! Desktop notifications for the tray control panel.

#[cfg(windows)]
mod windows_impl {
    use crate::paths::install_dir;
    use std::path::PathBuf;
    use tauri_winrt_notification::{Duration, IconCrop, Toast};

    /// Must match the Inno Setup `AppUserModelID` on the Start Menu shortcut.
    pub const APP_USER_MODEL_ID: &str = "RobottikSoftware.Scalattice.Agent";

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
}

#[cfg(windows)]
pub use windows_impl::{set_process_app_id, show};

#[cfg(target_os = "macos")]
pub fn set_process_app_id() {}

/// macOS notifications go through `osascript`. That must never run on the UI
/// thread: Apple Events / TCC can block `osascript` indefinitely, which froze
/// the tray as soon as an update was detected.
#[cfg(target_os = "macos")]
pub fn show(title: &str, body: &str) {
    let title = title.to_string();
    let body = body.to_string();
    let _ = std::thread::Builder::new()
        .name("scalattice-notify".into())
        .spawn(move || show_macos_notification(&title, &body));
}

#[cfg(target_os = "macos")]
fn show_macos_notification(title: &str, body: &str) {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let title = applescript_escape(title);
    let body = applescript_escape(body);
    let script = format!("display notification \"{body}\" with title \"{title}\"");
    let mut child = match Command::new("osascript")
        .args(["-e", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return,
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return,
        }
    }
}

#[cfg(any(test, target_os = "macos"))]
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::applescript_escape;

    #[test]
    fn applescript_escape_quotes_and_backslashes() {
        assert_eq!(applescript_escape(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(applescript_escape(r"a\b"), r"a\\b");
    }
}
