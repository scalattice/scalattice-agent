mod capacity;
mod download;
mod gguf_check;
mod health;
mod storage;
mod sync;

pub use capacity::{
    can_host_model, can_host_on_machine, preferred_download_card, DEFAULT_CPU_RAM_HEADROOM_GB,
};
pub use gguf_check::gguf_payload_in_bounds;
pub use health::{
    clear_weight_health, handle_weight_load_failure, process_preload_paused, should_skip_preload,
    spawn_delete_staged_dirs, stage_purge_model_weights, sweep_staged_purge_dirs,
};
pub use storage::{
    catalog_model_weights_ready, list_cached_runtime_models, list_model_disk_status,
    models_cache_disk_gb, models_dir, purge_incomplete_model_weights, resolve_model_gguf,
    ModelDiskStatus,
};
pub use sync::spawn_catalog_sync;
