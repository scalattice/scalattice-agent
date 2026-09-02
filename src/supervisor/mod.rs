//! Local slot supervisor: one process owns per-slot inference workers.

mod host;
mod ipc;
mod placement;
mod worker;

pub use host::{SlotStatus, Supervisor};
pub use worker::run_worker;
