mod capacity;
mod download;
mod gguf_arch;
mod gguf_check;
mod health;
mod storage;
mod sync;
mod vram_plan;

pub use capacity::{
    can_host_model, can_host_on_machine, can_serve_vision_on_card, can_serve_vision_on_machine,
    gpu_full_host_need_gb_for_job, hosting_min_vram_gb, image_job_min_vram_gb,
    preferred_download_card, vram_can_gpu_full, DEFAULT_CPU_RAM_HEADROOM_GB,
};
pub use gguf_arch::gguf_shape;
pub use gguf_check::gguf_payload_in_bounds;
pub use health::{
    clear_weight_health, handle_weight_load_failure, should_skip_preload,
    spawn_delete_staged_dirs, stage_purge_model_weights, sweep_staged_purge_dirs,
};
pub use storage::{
    list_cached_runtime_models, list_model_disk_status, models_cache_disk_gb, models_dir,
    purge_incomplete_model_weights, resolve_mmproj, resolve_model_gguf, ModelDiskStatus,
};
pub use sync::spawn_catalog_sync;
pub use vram_plan::{full_host_need_from_weight, full_host_need_gb};
