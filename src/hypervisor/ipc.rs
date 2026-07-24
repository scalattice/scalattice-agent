use crate::compute_pool::VirtualCard;
use crate::protocol::{ChatMessage, InvokeTimings};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerBootConfig {
    pub slot_id: String,
    pub card: VirtualCard,
    pub cuda_visible: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WorkerRequest {
    Ping {
        id: String,
    },
    Warm {
        id: String,
        runtime_model: String,
    },
    Invoke {
        id: String,
        job_id: String,
        model_id: String,
        runtime_model: String,
        messages: Vec<ChatMessage>,
        max_tokens: u32,
        stream: bool,
    },
    Evict {
        id: String,
    },
    Health {
        id: String,
    },
    Shutdown {
        id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerResponse {
    Pong {
        id: String,
    },
    Ok {
        id: String,
    },
    Delta {
        id: String,
        text: String,
    },
    Result {
        id: String,
        content: String,
        prompt_tokens: u32,
        completion_tokens: u32,
        timings: InvokeTimings,
        loaded_models: Vec<String>,
    },
    Health {
        id: String,
        ready: bool,
        loaded_models: Vec<String>,
        busy: bool,
    },
    Error {
        id: String,
        error: String,
    },
}
