use crate::config::{read_saved_agent_token, SCALATTICE_LIFECYCLE_API_BASE};
use std::time::Duration;
use tracing::{info, warn};

/// Best-effort HTTPS notify before local wipe. Never fails uninstall.
pub fn notify_server_uninstall(reason: &str) {
    if crate::config::update_smoke_test() {
        return;
    }
    let Some(token) = read_saved_agent_token() else {
        warn!("uninstall notify skipped: no saved provider token");
        return;
    };
    let reason = reason.trim();
    let reason = if reason.is_empty() {
        "uninstall"
    } else {
        reason
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(err) => {
                warn!(error = %err, "uninstall notify: runtime build failed");
                return;
            }
        };
        rt.block_on(async {
            let client = match reqwest::Client::builder()
                .timeout(Duration::from_secs(8))
                .connect_timeout(Duration::from_secs(4))
                .user_agent(format!("scalattice-agent/{}", crate::update::current_version()))
                .build()
            {
                Ok(c) => c,
                Err(err) => {
                    warn!(error = %err, "uninstall notify: http client build failed");
                    return;
                }
            };
            // Must be cloud providers lifecycle — NOT api.*/operators (inference gateway).
            let url = format!("{SCALATTICE_LIFECYCLE_API_BASE}/uninstall");
            let body = serde_json::json!({
                "reason": reason,
                "agentVersion": crate::update::current_version(),
                "platform": std::env::consts::OS,
            });
            match client.post(&url).bearer_auth(&token).json(&body).send().await {
                Ok(resp) if resp.status().is_success() => {
                    info!(%url, "notified Scalattice Cloud of uninstall");
                }
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    warn!(
                        %url,
                        %status,
                        body = %text.chars().take(240).collect::<String>(),
                        "uninstall notify rejected by Cloud"
                    );
                }
                Err(err) => {
                    warn!(%url, error = %err, "uninstall notify request failed");
                }
            }
        });
    }));
    if result.is_err() {
        warn!("uninstall notify panicked; continuing local uninstall");
    }
}
