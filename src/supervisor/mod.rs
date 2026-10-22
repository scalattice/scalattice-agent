//! Local slot supervisor: one process owns per-slot inference workers.
//!
//! Process roles (named hardlinks/symlinks when the install dir is writable):
//! - `scalattice-agent` — supervisor (Cloud WS)
//! - `scalattice-slot-{id} worker` — per-device kid (`cuda-*` / `vulkan-*` / `metal-*` / `cpu`)
//! - `scalattice-tray` — tray / menu bar (Windows + macOS)
//!
//! Orphan prevention is OS-native: Windows Job Object, Linux PDEATHSIG + systemd
//! cgroup kill, macOS tracked PID reap + watchdog sweeps.

mod host;
mod ipc;
mod placement;
mod worker;
pub mod worker_job;

pub use host::{SlotStatus, Supervisor};
pub use worker::run_worker;
