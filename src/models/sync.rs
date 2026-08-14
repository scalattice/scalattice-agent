use crate::compute_pool::VirtualCard;
use crate::models::capacity::can_host_model;
use crate::models::download::{download_catalog_model, is_no_space_error};
use crate::models::storage::{
    catalog_model_weights_ready, purge_failed_download, purge_incomplete_model_weights,
};
use crate::protocol::CatalogModel;
use crate::state;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

pub fn spawn_catalog_sync(
    catalog: Vec<CatalogModel>,
    card: VirtualCard,
    ram_gb: u32,
    cpu_ram_headroom_gb: u32,
    agent_token: String,
    hf_token: Option<String>,
    cancel: Arc<AtomicBool>,
    sync_in_flight: Arc<AtomicBool>,
    enabled_model_ids: std::collections::HashSet<String>,
) {
    if catalog.is_empty() {
        return;
    }

    tokio::spawn(async move {
        let hf_token = hf_token.or_else(|| std::env::var("SCALATTICE_HF_TOKEN").ok());

        for model in catalog {
            if cancel.load(Ordering::Relaxed) {
                state::set_downloading_model(None);
                sync_in_flight.store(false, Ordering::Relaxed);
                return;
            }
            if model.weights.is_none() {
                continue;
            }
            let runtime_model = runtime_model_id(&model);
            if catalog_model_weights_ready(&model) {
                continue;
            }
            if !enabled_model_ids.contains(&model.model_id) {
                let runtime_model = runtime_model_id(&model);
                purge_incomplete_model_weights(runtime_model);
                continue;
            }
            if crate::specs::disk_is_full() {
                warn!("disk full; pausing remaining model downloads");
                crate::state::set_disk_full(true);
                break;
            }
            if !can_host_model(&model, &card, ram_gb, cpu_ram_headroom_gb) {
                info!(
                    "skipping {}: needs {} GB VRAM / {} GB RAM (virtual card has {} GB VRAM, {} GB RAM, headroom {} GB)",
                    model.model_id,
                    model.min_vram_gb.unwrap_or(0.0),
                    model.min_ram_gb.unwrap_or(0.0),
                    card.total_vram_gb,
                    ram_gb,
                    cpu_ram_headroom_gb
                );
                continue;
            }
            state::set_downloading_model(Some(&model.model_id));
            let result =
                download_catalog_model(&model, &agent_token, hf_token.as_deref()).await;
            state::set_downloading_model(None);
            if cancel.load(Ordering::Relaxed) {
                purge_incomplete_model_weights(runtime_model);
                sync_in_flight.store(false, Ordering::Relaxed);
                return;
            }
            if let Err(err) = result {
                let runtime_model = runtime_model_id(&model);
                purge_failed_download(runtime_model);
                warn!(
                    "model download failed for {}: {err:#}",
                    model.model_id
                );
                if is_no_space_error(&err) {
                    crate::state::set_disk_full(true);
                    warn!("disk full; paused remaining model downloads");
                    break;
                }
            }
        }
        state::set_downloading_model(None);
        sync_in_flight.store(false, Ordering::Relaxed);
    });
}

fn runtime_model_id(model: &CatalogModel) -> &str {
    if model.runtime_model.trim().is_empty() {
        model.model_id.as_str()
    } else {
        model.runtime_model.as_str()
    }
}
