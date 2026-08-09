use crate::config::{read_saved_agent_token, SCALATTICE_API_BASE};
use std::time::Duration;

/// Best-effort HTTPS notify before local wipe. Never fails uninstall.
pub fn notify_server_uninstall(reason: &str) {
    let Some(token) = read_saved_agent_token() else {
        return;
    };
    let reason = reason.trim();
    let reason = if reason.is_empty() { "uninstall" } else { reason };

    let result = std::panic::catch_unwind(|| {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let _ = rt.block_on(async {
            let client = match reqwest::Client::builder()
                .timeout(Duration::from_secs(8))
                .connect_timeout(Duration::from_secs(4))
                .user_agent(format!("scalattice-agent/{}", env!("CARGO_PKG_VERSION")))
                .build()
            {
                Ok(c) => c,
                Err(_) => return,
            };
            let url = format!("{SCALATTICE_API_BASE}/uninstall");
            let body = serde_json::json!({
                "reason": reason,
                "agentVersion": env!("CARGO_PKG_VERSION"),
                "platform": std::env::consts::OS,
            });
            let _ = client
                .post(url)
                .bearer_auth(token)
                .json(&body)
                .send()
                .await;
        });
    });
    let _ = result;
}
