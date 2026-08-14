use crate::hypervisor::SlotStatus;
use serde::Serialize;
use std::collections::HashMap;

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
    #[serde(rename = "modelsDiskGb", skip_serializing_if = "Option::is_none")]
    pub models_disk_gb: Option<u32>,
    #[serde(rename = "modelDisk", skip_serializing_if = "HashMap::is_empty")]
    pub model_disk: HashMap<String, SerializedModelDiskStatus>,
    #[serde(rename = "diskFull")]
    pub disk_full: bool,
    /// Agent can emit invoke_delta tokens for true SSE streaming.
    #[serde(rename = "supportsStream")]
    pub supports_stream: bool,
    /// Dashboard can request live logs / remote restart / update (1.1.14+).
    #[serde(rename = "supportsRemoteControl")]
    pub supports_remote_control: bool,
    /// Independent compute slots (per GPU / Vulkan / CPU).
    #[serde(rename = "slots", skip_serializing_if = "Vec::is_empty")]
    pub slots: Vec<SlotStatus>,
    /// Max concurrent invokes this machine can accept.
    #[serde(rename = "maxConcurrentJobs", skip_serializing_if = "Option::is_none")]
    pub max_concurrent_jobs: Option<u32>,
    /// Idle healthy slots available for new work.
    #[serde(rename = "idleSlots", skip_serializing_if = "Option::is_none")]
    pub idle_slots: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SerializedModelDiskStatus {
    #[serde(rename = "diskGb")]
    pub disk_gb: f64,
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn build_runtime(
    job_state: JobState,
    active_job_id: Option<String>,
    active_model_id: Option<String>,
    loaded_models: &[String],
    enabled_compute_devices: usize,
    downloading_model: Option<&str>,
    blocked_enabled_models: usize,
    models_disk_gb: u32,
    model_disk: HashMap<String, SerializedModelDiskStatus>,
    slots: Vec<SlotStatus>,
    max_concurrent_jobs: u32,
    idle_slots: u32,
) -> AgentRuntime {
    let ready = enabled_compute_devices > 0 && !loaded_models.is_empty();
    // Report real busyness for dashboards. Routing uses claim slots / idleSlots,
    // not this flag — forcing Idle whenever any slot (often cpu-0) was free hid
    // active GPU jobs as "in the inference pool".
    let reported_state = if job_state == JobState::Busy || (idle_slots == 0 && max_concurrent_jobs > 0)
    {
        JobState::Busy
    } else {
        JobState::Idle
    };
    let mut status_label = status_label(
        ready,
        reported_state,
        active_model_id.as_deref(),
        enabled_compute_devices,
        downloading_model,
        blocked_enabled_models,
        idle_slots,
        slots.len() as u32,
    );
    let disk_full = crate::state::disk_full();
    if disk_full && downloading_model.is_none() && reported_state != JobState::Busy {
        status_label = "Disk full — paused model downloads".to_string();
    }

    AgentRuntime {
        ready,
        job_state: reported_state.as_str().to_string(),
        active_job_id: if reported_state == JobState::Busy || job_state == JobState::Busy {
            active_job_id
        } else {
            None
        },
        active_model_id: if job_state == JobState::Busy || reported_state == JobState::Busy {
            active_model_id
        } else {
            None
        },
        status_label,
        downloading_model: downloading_model.map(str::to_string),
        loaded_models: loaded_models.to_vec(),
        models_disk_gb: if models_disk_gb > 0 {
            Some(models_disk_gb)
        } else {
            None
        },
        model_disk,
        disk_full,
        supports_stream: true,
        supports_remote_control: true,
        slots,
        max_concurrent_jobs: Some(max_concurrent_jobs.max(1)),
        idle_slots: Some(idle_slots),
    }
}

pub fn serialize_model_disk(
    entries: &[(String, crate::models::ModelDiskStatus)],
) -> HashMap<String, SerializedModelDiskStatus> {
    entries
        .iter()
        .map(|(runtime_model, status)| {
            let disk_gb = (status.bytes as f64) / 1024.0 / 1024.0 / 1024.0;
            let disk_gb = ((disk_gb * 10.0).round() / 10.0).max(0.1);
            (
                runtime_model.clone(),
                SerializedModelDiskStatus {
                    disk_gb,
                    complete: status.complete,
                    state: Some(status.state.clone()),
                    error: status.error.clone(),
                },
            )
        })
        .collect()
}

fn status_label(
    ready: bool,
    job_state: JobState,
    active_model_id: Option<&str>,
    enabled_compute_devices: usize,
    downloading_model: Option<&str>,
    blocked_enabled_models: usize,
    idle_slots: u32,
    total_slots: u32,
) -> String {
    if job_state == JobState::Busy {
        let model = active_model_id.unwrap_or("inference");
        if total_slots > 1 {
            if idle_slots == 0 {
                return format!("Running {model} · all {total_slots} slots busy");
            }
            if idle_slots < total_slots {
                return format!("Running {model} · {idle_slots}/{total_slots} slots free");
            }
            // Slot cache still shows all idle — the job has started; don't say Ready.
            return format!("Running {model}");
        }
        return format!("Running {model}");
    }
    if idle_slots > 0 && idle_slots < total_slots && total_slots > 1 {
        let model = active_model_id.unwrap_or("inference");
        return format!("Running {model} · {idle_slots}/{total_slots} slots free");
    }
    if let Some(model) = downloading_model {
        return format!("Downloading {model}");
    }
    if ready {
        if total_slots > 1 {
            return format!("Ready · {total_slots} compute slots");
        }
        return "Ready for inference".to_string();
    }
    if enabled_compute_devices == 0 {
        return "No compute enabled".to_string();
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
