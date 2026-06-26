use crate::models::download::download_catalog_model;
use crate::protocol::CatalogModel;
use tracing::warn;

pub fn spawn_catalog_sync(
    catalog: Vec<CatalogModel>,
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
            if let Err(err) =
                download_catalog_model(&model, &agent_token, hf_token.as_deref()).await
            {
                warn!(
                    "model download failed for {}: {err:#}",
                    model.model_id
                );
            }
        }
    });
}
