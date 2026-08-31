mod capacity;
mod download;
mod gguf_check;
mod health;
mod storage;
mod sync;

pub use capacity::{
    advertised_vram_can_gpu_full, can_host_model, can_host_on_machine, can_serve_vision_on_card,
    can_serve_vision_on_machine, gpu_full_host_need_gb, hosting_min_vram_gb, image_job_min_vram_gb,
    preferred_download_card, DEFAULT_CPU_RAM_HEADROOM_GB,
};
pub use gguf_check::gguf_payload_in_bounds;
pub use health::{
    clear_weight_health, handle_weight_load_failure, process_preload_paused, should_skip_preload,
    spawn_delete_staged_dirs, stage_purge_model_weights, sweep_staged_purge_dirs,
};
pub use storage::{
    list_cached_runtime_models, list_model_disk_status, models_cache_disk_gb, models_dir,
    purge_incomplete_model_weights, resolve_mmproj, resolve_model_gguf, ModelDiskStatus,
};
pub use sync::spawn_catalog_sync;
