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
    #[serde(rename = "demoMode")]
    pub demo_mode: bool,
    pub ready: bool,
    #[serde(rename = "jobState")]
    pub job_state: String,
    #[serde(rename = "activeJobId", skip_serializing_if = "Option::is_none")]
    pub active_job_id: Option<String>,
    #[serde(rename = "activeModelId", skip_serializing_if = "Option::is_none")]
    pub active_model_id: Option<String>,
    #[serde(rename = "statusLabel")]
    pub status_label: String,
    #[serde(rename = "loadedModels", skip_serializing_if = "Vec::is_empty")]
    pub loaded_models: Vec<String>,
}

pub fn build_runtime(
    demo_mode: bool,
    job_state: JobState,
    active_job_id: Option<String>,
    active_model_id: Option<String>,
    loaded_models: &[String],
    enabled_compute_devices: usize,
) -> AgentRuntime {
    let ready = enabled_compute_devices > 0 && (demo_mode || !loaded_models.is_empty());
    let status_label = status_label(
        demo_mode,
        ready,
        job_state,
        active_model_id.as_deref(),
        enabled_compute_devices,
    );

    AgentRuntime {
        demo_mode,
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
        loaded_models: loaded_models.to_vec(),
    }
}

fn status_label(
    demo_mode: bool,
    ready: bool,
    job_state: JobState,
    active_model_id: Option<&str>,
    enabled_compute_devices: usize,
) -> String {
    if job_state == JobState::Busy {
        let model = active_model_id.unwrap_or("inference");
        if enabled_compute_devices > 1 {
            return format!("Running job across {enabled_compute_devices} devices · {model}");
        }
        return format!("Running job · {model}");
    }
    if demo_mode {
        if enabled_compute_devices > 1 {
            return format!("Idle · demo mode across {enabled_compute_devices} devices (echo only)");
        }
        return "Idle · demo mode (echo only)".to_string();
    }
    if ready {
        if enabled_compute_devices > 1 {
            return format!("Idle · virtual card ready across {enabled_compute_devices} devices");
        }
        return "Idle · ready for inference".to_string();
    }
    if enabled_compute_devices > 1 {
        return format!(
            "Connected · virtual {enabled_compute_devices}-device card · load model weights or enable demo"
        );
    }
    "Connected · no model runtime loaded".to_string()
}
