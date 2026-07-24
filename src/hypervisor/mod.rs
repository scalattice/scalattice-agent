//! Local compute hypervisor: one supervisor + per-slot inference workers.

mod demand;
mod ipc;
mod placement;
mod supervisor;
mod worker;

pub use supervisor::{Hypervisor, SlotStatus};
pub use worker::run_worker;
