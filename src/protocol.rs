use crate::runtime::AgentRuntime;
use crate::specs::MachineSpecs;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct Envelope {
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelWeights {
    pub source: String,
    pub repo: String,
    pub filename: String,
    #[serde(default, rename = "companionFilenames")]
    pub companion_filenames: Vec<String>,
    #[serde(default)]
    pub revision: String,
    #[serde(rename = "mirrorUrl", default)]
    pub mirror_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CatalogModel {
    #[serde(rename = "modelId")]
    pub model_id: String,
    #[serde(rename = "displayName", default)]
    pub display_name: String,
    #[serde(rename = "runtimeModel", default)]
    pub runtime_model: String,
    #[serde(rename = "maxContextTokens", default)]
    pub max_context_tokens: u32,
    #[serde(default)]
    pub regions: Vec<String>,
    #[serde(rename = "weightSizeGb", default)]
    pub weight_size_gb: Option<u32>,
    #[serde(rename = "minVramGb", default)]
    pub min_vram_gb: Option<u32>,
    #[serde(rename = "minRamGb", default)]
    pub min_ram_gb: Option<u32>,
    #[serde(default)]
    pub weights: Option<ModelWeights>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelPolicyEntry {
    #[serde(rename = "modelId")]
    pub model_id: String,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ComputeDevicePolicy {
    pub id: String,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct ReadyMessage {
    #[serde(rename = "nodeId")]
    pub node_id: String,
    pub catalog: Vec<CatalogModel>,
    #[serde(rename = "computeDevices", default)]
    pub compute_devices: Vec<ComputeDevicePolicy>,
    #[serde(rename = "enabledModels", default)]
    pub enabled_models: Vec<ModelPolicyEntry>,
    #[serde(rename = "huggingFaceToken", default)]
    pub hugging_face_token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PongMessage {
    #[serde(rename = "computeDevices", default)]
    pub compute_devices: Vec<ComputeDevicePolicy>,
    #[serde(rename = "enabledModels", default)]
    pub enabled_models: Vec<ModelPolicyEntry>,
    #[serde(rename = "huggingFaceToken", default)]
    pub hugging_face_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RegisterMessage {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub models: Vec<String>,
    #[serde(rename = "gpuName", skip_serializing_if = "Option::is_none")]
    pub gpu_name: Option<String>,
    #[serde(rename = "vramGb", skip_serializing_if = "Option::is_none")]
    pub vram_gb: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub specs: Option<MachineSpecs>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<AgentRuntime>,
}

#[derive(Debug, Deserialize)]
pub struct RegisteredMessage {
    #[serde(rename = "nodeId")]
    pub node_id: String,
    pub models: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct InvokeSplitMessage {
    pub id: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
    #[serde(rename = "runtimeModel")]
    pub runtime_model: String,
    pub segment: String,
    #[serde(rename = "promptTokenIds", default)]
    pub prompt_token_ids: Vec<u32>,
    #[serde(rename = "stateB64", default)]
    pub state_b64: String,
    #[serde(rename = "maxTokens", default)]
    pub max_tokens: u32,
}

#[derive(Debug, Serialize)]
pub struct InvokeSplitResultMessage {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: String,
    #[serde(rename = "stateB64", skip_serializing_if = "String::is_empty")]
    pub state_b64: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub content: String,
    #[serde(rename = "promptTokens")]
    pub prompt_tokens: u32,
    #[serde(rename = "completionTokens")]
    pub completion_tokens: u32,
}

#[derive(Debug, Deserialize)]
pub struct InvokeMessage {
    pub id: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
    #[serde(rename = "runtimeModel")]
    pub runtime_model: String,
    pub messages: Vec<ChatMessage>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct InvokeResultMessage {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: String,
    pub content: String,
    #[serde(rename = "promptTokens")]
    pub prompt_tokens: u32,
    #[serde(rename = "completionTokens")]
    pub completion_tokens: u32,
}

#[derive(Debug, Serialize)]
pub struct InvokeErrorMessage {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: String,
    pub error: String,
}

#[derive(Debug, Serialize)]
pub struct HeartbeatMessage {
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub specs: Option<MachineSpecs>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<AgentRuntime>,
}

pub fn parse_envelope(data: &[u8]) -> anyhow::Result<Envelope> {
    Ok(serde_json::from_slice(data)?)
}

pub fn parse_ready(data: &[u8]) -> anyhow::Result<ReadyMessage> {
    Ok(serde_json::from_slice(data)?)
}

pub fn parse_pong(data: &[u8]) -> anyhow::Result<PongMessage> {
    Ok(serde_json::from_slice(data)?)
}

pub fn parse_registered(data: &[u8]) -> anyhow::Result<RegisteredMessage> {
    Ok(serde_json::from_slice(data)?)
}

pub fn parse_invoke_split(data: &[u8]) -> anyhow::Result<InvokeSplitMessage> {
    Ok(serde_json::from_slice(data)?)
}

pub fn parse_invoke(data: &[u8]) -> anyhow::Result<InvokeMessage> {
    Ok(serde_json::from_slice(data)?)
}

pub fn parse_error(data: &[u8]) -> anyhow::Result<Value> {
    Ok(serde_json::from_slice(data)?)
}
