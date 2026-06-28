use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Idle,
    Busy,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Idle => "idle",
            JobState::Busy => "busy",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentRuntime {
    pub ready: bool,
    #[serde(rename = "jobState")]
    pub job_state: String,
    #[serde(rename = "activeJobId", skip_serializing_if = "Option::is_none")]
    pub active_job_id: Option<String>,
    #[serde(rename = "activeModelId", skip_serializing_if = "Option::is_none")]
    pub active_model_id: Option<String>,
    #[serde(rename = "statusLabel")]
    pub status_label: String,
    #[serde(rename = "downloadingModel", skip_serializing_if = "Option::is_none")]
    pub downloading_model: Option<String>,
    #[serde(rename = "loadedModels", skip_serializing_if = "Vec::is_empty")]
    pub loaded_models: Vec<String>,
}

pub fn build_runtime(
    job_state: JobState,
    active_job_id: Option<String>,
    active_model_id: Option<String>,
    loaded_models: &[String],
    enabled_compute_devices: usize,
    downloading_model: Option<&str>,
    blocked_enabled_models: usize,
) -> AgentRuntime {
    let ready = enabled_compute_devices > 0 && !loaded_models.is_empty();
    let status_label = status_label(
        ready,
        job_state,
        active_model_id.as_deref(),
        enabled_compute_devices,
        downloading_model,
        blocked_enabled_models,
    );

    AgentRuntime {
        ready,
        job_state: job_state.as_str().to_string(),
        active_job_id: if job_state == JobState::Busy {
            active_job_id
        } else {
            None
        },
        active_model_id: if job_state == JobState::Busy {
            active_model_id
        } else {
            None
        },
        status_label,
        downloading_model: downloading_model.map(str::to_string),
        loaded_models: loaded_models.to_vec(),
    }
}

fn status_label(
    ready: bool,
    job_state: JobState,
    active_model_id: Option<&str>,
    enabled_compute_devices: usize,
    downloading_model: Option<&str>,
    blocked_enabled_models: usize,
) -> String {
    if job_state == JobState::Busy {
        let model = active_model_id.unwrap_or("inference");
        if enabled_compute_devices > 1 {
            return format!("Running {model} across {enabled_compute_devices} devices");
        }
        return format!("Running {model}");
    }
    if let Some(model) = downloading_model {
        return format!("Downloading {model}");
    }
    if ready {
        return "Ready for inference".to_string();
    }
    if blocked_enabled_models > 0 {
        if blocked_enabled_models == 1 {
            return "Enabled model won't fit this machine".to_string();
        }
        return format!("{blocked_enabled_models} enabled models won't fit this machine");
    }
    if enabled_compute_devices > 1 {
        return format!("Waiting for model weights ({enabled_compute_devices} devices)");
    }
    "Waiting for model weights".to_string()
}
