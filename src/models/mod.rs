mod capacity;
mod download;
mod gguf_check;
mod health;
mod storage;
mod sync;

pub use capacity::{can_host_model, DEFAULT_CPU_RAM_HEADROOM_GB};
pub use health::{
    clear_weight_health, handle_weight_load_failure, process_preload_paused, should_skip_preload,
    spawn_delete_staged_dirs, stage_purge_model_weights, sweep_staged_purge_dirs,
};
pub use storage::{
    list_cached_runtime_models, list_model_disk_status, model_weights_ready, models_cache_disk_gb,
    models_dir, purge_incomplete_model_weights, resolve_model_gguf, ModelDiskStatus,
};
pub use sync::spawn_catalog_sync;
