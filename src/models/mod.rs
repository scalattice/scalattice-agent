mod capacity;
mod download;
mod storage;
mod sync;

pub use capacity::can_host_model;
pub use storage::{list_cached_runtime_models, models_dir, resolve_model_gguf};
pub use sync::spawn_catalog_sync;
