use crate::compute_pool::VirtualCard;
use crate::models::capacity::can_host_model;
use crate::models::download::download_catalog_model;
use crate::protocol::CatalogModel;
use crate::state;
use tracing::{info, warn};

pub fn spawn_catalog_sync(
    catalog: Vec<CatalogModel>,
    card: VirtualCard,
    ram_gb: u32,
    agent_token: String,
    hf_token: Option<String>,
) {
    if catalog.is_empty() {
        return;
    }

    tokio::spawn(async move {
        let hf_token = hf_token.or_else(|| std::env::var("SCALATTICE_HF_TOKEN").ok());

        for model in catalog {
            if model.weights.is_none() {
                continue;
            }
            if !can_host_model(&model, &card, ram_gb) {
                info!(
                    "skipping {} — needs {} GB VRAM / {} GB RAM (virtual card has {} GB VRAM, {} GB RAM)",
                    model.model_id,
                    model.min_vram_gb.unwrap_or(0),
                    model.min_ram_gb.unwrap_or(0),
                    card.total_vram_gb,
                    ram_gb
                );
                continue;
            }
            state::set_downloading_model(Some(&model.model_id));
            let result =
                download_catalog_model(&model, &agent_token, hf_token.as_deref()).await;
            state::set_downloading_model(None);
            if let Err(err) = result {
                warn!(
                    "model download failed for {}: {err:#}",
                    model.model_id
                );
            }
        }
        state::set_downloading_model(None);
    });
}
