//! Keep slot-worker processes tied to the supervisor process lifetime.
//!
//! `process::exit` skips Drop/`kill_on_drop`, which previously left orphan
//! inference workers (CUDA / Vulkan / Metal / CPU) that looked like a still-
//! running agent and blocked restart. Platform hooks:
//!
//! - **Windows:** Job Object with `KILL_ON_JOB_CLOSE` + named hardlinks
//! - **Linux:** `PR_SET_PDEATHSIG(SIGKILL)` in the child + tracked PID kill +
//!   systemd `KillMode=control-group`
//! - **macOS:** tracked PID kill + launchd/watchdog sweeps (no pdeathsig)
//!
//! Named images (`scalattice-slot-{id}`, `scalattice-tray`) are hardlinks (or
//! symlinks) of the same bytes so `ps` / Task Manager / Activity Monitor show
//! distinct roles for every GPU/CPU vendor slot id (`cuda-*`, `vulkan-*`,
//! `metal-*`, `cpu`, …).

use std::path::{Path, PathBuf};

fn sanitize_slot_id(slot_id: &str) -> String {
    let mut out = String::with_capacity(slot_id.len());
    for c in slot_id.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("slot");
    }
    out
}

fn slot_link_name(slot_id: &str) -> String {
    let safe = sanitize_slot_id(slot_id);
    if cfg!(windows) {
        format!("scalattice-slot-{safe}.exe")
    } else {
        format!("scalattice-slot-{safe}")
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn tray_link_name() -> &'static str {
    if cfg!(windows) {
        "scalattice-tray.exe"
    } else {
        "scalattice-tray"
    }
}

/// True when `link` is the same bytes as `agent_bin` (hardlink same inode, or
/// symlink that resolves to it). After a binary replace, hardlinks can still
/// point at the **old** inode — those must be deleted and recreated.
fn link_tracks_agent(agent_bin: &Path, link: &Path) -> bool {
    let Ok(link_meta) = std::fs::symlink_metadata(link) else {
        return false;
    };
    if link_meta.file_type().is_symlink() {
        if let (Ok(a), Ok(b)) = (
            std::fs::canonicalize(agent_bin),
            std::fs::canonicalize(link),
        ) {
            return a == b;
        }
        return false;
    }
    let Ok(agent_meta) = std::fs::metadata(agent_bin) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return agent_meta.dev() == link_meta.dev() && agent_meta.ino() == link_meta.ino();
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        match (
            agent_meta.volume_serial_number(),
            link_meta.volume_serial_number(),
            agent_meta.file_index(),
            link_meta.file_index(),
        ) {
            (Some(av), Some(lv), Some(ai), Some(li)) => av == lv && ai == li,
            _ => false,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (agent_meta, link_meta);
        false
    }
}

/// Prefer a same-directory hardlink (same inode → same codesign CDHash on macOS).
/// Fall back to symlink (cross-filesystem installs), then the original binary.
fn ensure_role_image(agent_bin: &Path, link_name: &str) -> PathBuf {
    let Some(dir) = agent_bin.parent() else {
        return agent_bin.to_path_buf();
    };
    let link = dir.join(link_name);
    if link_tracks_agent(agent_bin, &link) {
        return link;
    }
    // Stale hardlink (old inode after update), broken symlink, or wrong target.
    if link.symlink_metadata().is_ok() {
        let _ = std::fs::remove_file(&link);
    }

    match std::fs::hard_link(agent_bin, &link) {
        Ok(()) => {
            tracing::info!(path = %link.display(), "process-role image ready (hardlink)");
            return link;
        }
        Err(err) => {
            tracing::debug!(
                link = %link_name,
                error = %err,
                "hardlink unavailable; trying symlink"
            );
        }
    }

    #[cfg(unix)]
    {
        if std::os::unix::fs::symlink(agent_bin, &link).is_ok() {
            tracing::info!(path = %link.display(), "process-role image ready (symlink)");
            return link;
        }
    }

    tracing::warn!(
        link = %link_name,
        "could not create process-role link; using scalattice-agent"
    );
    agent_bin.to_path_buf()
}

/// Drop role hardlinks/symlinks next to `agent_bin` so the next spawn recreates
/// them against the current inode. Call after self-update / installer replace.
pub fn clear_role_images(agent_bin: &Path) {
    let Some(dir) = agent_bin.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let lower = name.to_ascii_lowercase();
        let is_role = lower == "scalattice-tray"
            || lower == "scalattice-tray.exe"
            || (lower.starts_with("scalattice-slot-")
                && (cfg!(windows) && lower.ends_with(".exe") || !cfg!(windows)));
        if !is_role {
            continue;
        }
        let path = entry.path();
        if link_tracks_agent(agent_bin, &path) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::info!(path = %path.display(), "removed stale process-role image"),
            Err(err) => tracing::debug!(
                path = %path.display(),
                error = %err,
                "could not remove stale process-role image"
            ),
        }
    }
}

/// Prefer `scalattice-slot-{id}` so process lists show a distinct image per GPU/CPU slot.
pub fn worker_image_for_slot(agent_bin: &Path, slot_id: &str) -> PathBuf {
    ensure_role_image(agent_bin, &slot_link_name(slot_id))
}

/// Tray / menu-bar control panel as `scalattice-tray` (same bytes as the agent).
/// Linux has no tray UI — only Windows + macOS create this role image.
#[cfg(any(windows, target_os = "macos"))]
pub fn tray_image(agent_bin: &Path) -> PathBuf {
    ensure_role_image(agent_bin, tray_link_name())
}

#[cfg(windows)]
mod win_job {
    use anyhow::{bail, Result};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    /// Owns the job handle; closing it (Drop or process death) kills assigned kids.
    pub struct WorkerJob {
        handle: HANDLE,
    }

    // SAFETY: HANDLE is pointer-sized; only touched from supervisor + Drop.
    unsafe impl Send for WorkerJob {}
    unsafe impl Sync for WorkerJob {}

    impl WorkerJob {
        pub fn create() -> Result<Self> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                    bail!("CreateJobObjectW failed (err {})", GetLastError());
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of_val(&info) as u32,
                );
                if ok == 0 {
                    let err = GetLastError();
                    CloseHandle(handle);
                    bail!("SetInformationJobObject KILL_ON_JOB_CLOSE failed (err {err})");
                }
                Ok(Self { handle })
            }
        }

        pub fn assign_pid(&self, pid: u32) -> Result<()> {
            if pid == 0 {
                bail!("invalid worker pid");
            }
            unsafe {
                let access =
                    PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION;
                let proc = OpenProcess(access, 0, pid);
                if proc.is_null() {
                    bail!("OpenProcess({pid}) failed (err {})", GetLastError());
                }
                let ok = AssignProcessToJobObject(self.handle, proc);
                let err = GetLastError();
                CloseHandle(proc);
                if ok == 0 {
                    bail!("AssignProcessToJobObject({pid}) failed (err {err})");
                }
            }
            Ok(())
        }
    }

    impl Drop for WorkerJob {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(unix)]
mod unix_job {
    use anyhow::{bail, Result};
    use std::sync::Mutex;

    /// Tracks slot-worker PIDs so Drop / explicit reap can SIGKILL them.
    /// On Linux, kids also install `PR_SET_PDEATHSIG` via [`install_child_guards`].
    pub struct WorkerJob {
        pids: Mutex<Vec<u32>>,
    }

    impl WorkerJob {
        pub fn create() -> Result<Self> {
            Ok(Self {
                pids: Mutex::new(Vec::new()),
            })
        }

        pub fn assign_pid(&self, pid: u32) -> Result<()> {
            if pid == 0 {
                bail!("invalid worker pid");
            }
            if let Ok(mut guard) = self.pids.lock() {
                if !guard.contains(&pid) {
                    guard.push(pid);
                }
            }
            Ok(())
        }

        pub fn reap_all(&self) {
            let pids = self
                .pids
                .lock()
                .map(|mut g| std::mem::take(&mut *g))
                .unwrap_or_default();
            for pid in pids {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }
    }

    impl Drop for WorkerJob {
        fn drop(&mut self) {
            self.reap_all();
        }
    }

    /// Run in the child after fork, before exec (tokio/std `pre_exec`).
    ///
    /// Linux: ask the kernel to SIGKILL us when the supervisor dies — covers
    /// `process::exit`, crashes, and OOM without relying on Drop. macOS has no
    /// equivalent; tracked PIDs + watchdog sweeps cover that path.
    pub fn install_child_guards() -> std::io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            // PR_SET_PDEATHSIG = 1
            const PR_SET_PDEATHSIG: i32 = 1;
            unsafe {
                let rc = libc::prctl(PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong);
                if rc != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Race: parent died between fork and prctl → we would never get the signal.
                if libc::getppid() == 1 {
                    libc::_exit(127);
                }
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
pub use win_job::WorkerJob;

#[cfg(unix)]
pub use unix_job::{install_child_guards, WorkerJob};
