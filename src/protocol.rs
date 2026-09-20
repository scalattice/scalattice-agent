use crate::runtime::AgentRuntime;
use crate::specs::MachineSpecs;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// Software ceiling for max_tokens. Live generate is still prompt + completion ≤ n_ctx.
pub const ABSOLUTE_MAX_COMPLETION_TOKENS: u32 = 131_072;

fn null_as_empty_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

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
    #[serde(rename = "downloadVia", default)]
    pub download_via: Option<String>,
    #[serde(rename = "mirrorUrl", default)]
    pub mirror_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CatalogModel {
    #[serde(rename = "modelId")]
    pub model_id: String,
    #[serde(rename = "displayName", default)]
    pub display_name: String,
    #[serde(rename = "runtimeModel", default)]
    pub runtime_model: String,
    #[serde(rename = "jobKind", default)]
    pub job_kind: String,
    /// Catalog: `gguf` (default) or `chatml`. Empty deserializes as gguf.
    #[serde(rename = "chatTemplate", default)]
    pub chat_template: String,
    /// Catalog: `auto` (default), `none`, or `always`. Empty deserializes as auto.
    #[serde(rename = "thinking", default)]
    pub thinking: String,
    #[serde(rename = "usdPerImage", default)]
    pub usd_per_image: f64,
    #[serde(rename = "imageMaxN", default)]
    pub image_max_n: u32,
    #[serde(rename = "maxContextTokens", default)]
    pub max_context_tokens: u32,
    #[serde(default)]
    pub regions: Vec<String>,
    #[serde(rename = "weightSizeGb", default)]
    pub weight_size_gb: Option<f64>,
    #[serde(rename = "minVramGb", default)]
    pub min_vram_gb: Option<f64>,
    #[serde(rename = "minVramGbVision", default)]
    pub min_vram_gb_vision: Option<f64>,
    #[serde(rename = "visionModel", default)]
    pub vision_model: bool,
    /// Non-VL catalog id this vision SKU may serve for text-only jobs.
    #[serde(rename = "textSiblingModelId", default)]
    pub text_sibling_model_id: Option<String>,
    #[serde(rename = "mmprojSizeGb", default)]
    pub mmproj_size_gb: Option<f64>,
    #[serde(rename = "visionMaxImages", default)]
    pub vision_max_images: Option<u32>,
    #[serde(rename = "visionMaxImageSidePx", default)]
    pub vision_max_image_side_px: Option<u32>,
    #[serde(rename = "visionMaxImagePixels", default)]
    pub vision_max_image_pixels: Option<u32>,
    #[serde(rename = "minRamGb", default)]
    pub min_ram_gb: Option<f64>,
    /// Server-computed KV at catalog n_ctx. Agent must not re-derive this.
    #[serde(rename = "kvCacheGb", default)]
    pub kv_cache_gb: Option<f64>,
    /// Server-computed GPU-full (weights + KV + scratch).
    #[serde(rename = "gpuFullVramGb", default)]
    pub gpu_full_vram_gb: Option<f64>,
    /// Server-computed GPU weights+scratch (KV elsewhere).
    #[serde(rename = "gpuWeightsVramGb", default)]
    pub gpu_weights_vram_gb: Option<f64>,
    #[serde(default)]
    pub weights: Option<ModelWeights>,
}

impl CatalogModel {
    pub fn is_image_job(&self) -> bool {
        matches!(
            self.job_kind.trim().to_ascii_lowercase().as_str(),
            "image"
                | "images"
                | "image_generation"
                | "image_edit"
                | "image-edit"
                | "image_edits"
        )
    }

    /// Image generation and chat/VL are separate runtimes. Both sides must agree.
    pub fn invoke_job_kind_error(
        &self,
        invoke_job_kind: &str,
    ) -> Option<(&'static str, &'static str)> {
        invoke_job_kind_error(self.is_image_job(), invoke_job_kind)
    }

    pub fn image_max_n(&self) -> u32 {
        let n = self.image_max_n;
        if n == 0 {
            1
        } else {
            n.min(4)
        }
    }

    pub fn catalog_kv_cache_gb(&self) -> Option<f64> {
        self.kv_cache_gb.filter(|v| *v > 0.0)
    }

    /// Catalog `chatTemplate=chatml`: skip GGUF jinja and render ChatML.
    pub fn uses_chatml(&self) -> bool {
        self.chat_template.trim().eq_ignore_ascii_case("chatml")
    }

    /// Catalog `thinking=none`: instruct-only, never CoT.
    pub fn thinking_none(&self) -> bool {
        self.thinking.trim().eq_ignore_ascii_case("none")
    }

    /// Catalog `thinking=always`: cannot disable CoT (R1 / Ornith).
    pub fn thinking_always(&self) -> bool {
        self.thinking.trim().eq_ignore_ascii_case("always")
    }

    pub fn catalog_gpu_full_vram_gb(&self) -> Option<f64> {
        self.gpu_full_vram_gb.filter(|v| *v > 0.0)
    }

    pub fn catalog_gpu_weights_vram_gb(&self) -> Option<f64> {
        self.gpu_weights_vram_gb.filter(|v| *v > 0.0)
    }
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

#[derive(Debug, Clone, Deserialize)]
pub struct AgentSchedule {
    #[serde(rename = "acceptingJobs", default = "default_true")]
    pub accepting_jobs: bool,
    #[serde(rename = "minutesUntilEarning", default)]
    pub minutes_until_earning: Option<u32>,
}

fn default_true() -> bool {
    true
}

impl Default for AgentSchedule {
    fn default() -> Self {
        Self {
            accepting_jobs: false,
            minutes_until_earning: None,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ReadyMessage {
    #[serde(rename = "nodeId")]
    pub node_id: String,
    #[serde(default, deserialize_with = "null_as_empty_vec")]
    pub catalog: Vec<CatalogModel>,
    #[serde(rename = "computeDevices", default, deserialize_with = "null_as_empty_vec")]
    pub compute_devices: Vec<ComputeDevicePolicy>,
    #[serde(rename = "enabledModels", default, deserialize_with = "null_as_empty_vec")]
    pub enabled_models: Vec<ModelPolicyEntry>,
    #[serde(rename = "maxCompletionTokens", default)]
    pub max_completion_tokens: u32,
    /// Extra system RAM (GB) beyond weight size for CPU / offload fit checks.
    /// Server always sends this; default matches platform settings.
    #[serde(rename = "cpuRamHeadroomGb", default = "default_cpu_ram_headroom_gb")]
    pub cpu_ram_headroom_gb: u32,
    #[serde(rename = "huggingFaceToken", default)]
    pub hugging_face_token: Option<String>,
    /// Runtime the Go hypervisor wants preloaded. Empty / omitted = stay empty.
    #[serde(rename = "warmRuntimeModel", default)]
    pub warm_runtime_model: Option<String>,
    #[serde(default)]
    pub schedule: AgentSchedule,
}

fn default_cpu_ram_headroom_gb() -> u32 {
    crate::models::DEFAULT_CPU_RAM_HEADROOM_GB
}

#[derive(Debug, Deserialize)]
pub struct PongMessage {
    #[serde(rename = "computeDevices", default, deserialize_with = "null_as_empty_vec")]
    pub compute_devices: Vec<ComputeDevicePolicy>,
    #[serde(rename = "enabledModels", default, deserialize_with = "null_as_empty_vec")]
    pub enabled_models: Vec<ModelPolicyEntry>,
    #[serde(rename = "huggingFaceToken", default)]
    pub hugging_face_token: Option<String>,
    #[serde(rename = "purgeModels", default, deserialize_with = "null_as_empty_vec")]
    pub purge_models: Vec<String>,
    #[serde(rename = "maxCompletionTokens", default)]
    pub max_completion_tokens: u32,
    /// Live catalog refresh (omit on ordinary heartbeats). Same shape as `ready.catalog`.
    #[serde(default)]
    pub catalog: Option<Vec<CatalogModel>>,
    #[serde(rename = "cpuRamHeadroomGb", default)]
    pub cpu_ram_headroom_gb: Option<u32>,
    /// Runtime the Go hypervisor wants preloaded. Omitted on older routers.
    #[serde(rename = "warmRuntimeModel", default)]
    pub warm_runtime_model: Option<String>,
    #[serde(default)]
    pub schedule: AgentSchedule,
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
    #[serde(default, deserialize_with = "null_as_empty_vec")]
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
    #[serde(rename = "promptTokenIds", default, deserialize_with = "null_as_empty_vec")]
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
    #[serde(default, deserialize_with = "null_as_empty_vec")]
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, rename = "maxTokens")]
    pub max_tokens: u32,
    #[serde(default, rename = "jobKind")]
    pub job_kind: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub n: u32,
    #[serde(default)]
    pub seed: Option<i64>,
    #[serde(default, rename = "inputImages", deserialize_with = "null_as_empty_vec")]
    pub input_images: Vec<ChatImage>,
}

#[derive(Debug, Deserialize)]
pub struct InvokeCancelMessage {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InvokeTimings {
    #[serde(rename = "modelLoadMs", skip_serializing_if = "Option::is_none")]
    pub model_load_ms: Option<u64>,
    #[serde(rename = "prefillMs", skip_serializing_if = "Option::is_none")]
    pub prefill_ms: Option<u64>,
    #[serde(rename = "decodeMs", skip_serializing_if = "Option::is_none")]
    pub decode_ms: Option<u64>,
    #[serde(rename = "totalMs", skip_serializing_if = "Option::is_none")]
    pub total_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct InvokeDeltaMessage {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: String,
    pub delta: String,
}

#[derive(Debug, Serialize)]
pub struct InvokeProgressMessage {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: String,
    pub phase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pct: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatImage {
    #[serde(default)]
    pub mime: String,
    /// Raw base64 (no `data:` prefix). Router inlines http(s) URLs before invoke.
    #[serde(default)]
    pub data: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ChatImage>,
}

#[derive(Deserialize)]
struct ChatMessageWire {
    role: String,
    #[serde(default)]
    content: serde_json::Value,
    #[serde(default, deserialize_with = "null_as_empty_vec")]
    images: Vec<ChatImage>,
}

impl From<ChatMessageWire> for ChatMessage {
    fn from(wire: ChatMessageWire) -> Self {
        let mut images = wire.images;
        let content = flatten_message_content(wire.content, &mut images);
        Self {
            role: wire.role,
            content,
            images,
        }
    }
}

impl<'de> Deserialize<'de> for ChatMessage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        ChatMessageWire::deserialize(deserializer).map(Self::from)
    }
}

fn flatten_message_content(value: serde_json::Value, images: &mut Vec<ChatImage>) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(text) => text,
        serde_json::Value::Array(parts) => {
            let mut texts = Vec::new();
            for part in parts {
                match part.get("type").and_then(|v| v.as_str()).unwrap_or("text") {
                    "image_url" => {
                        if let Some(url) = image_url_from_part(&part) {
                            if let Some(image) = chat_image_from_data_url(&url) {
                                images.push(image);
                            }
                        }
                    }
                    _ => {
                        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                texts.push(text.to_string());
                            }
                        }
                    }
                }
            }
            texts.join("\n")
        }
        other => other.as_str().unwrap_or("").to_string(),
    }
}

fn image_url_from_part(part: &serde_json::Value) -> Option<String> {
    let url = part.get("image_url")?;
    if let Some(s) = url.as_str() {
        return Some(s.to_string());
    }
    url.get("url")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

pub fn chat_image_from_data_url(url: &str) -> Option<ChatImage> {
    let trimmed = url.trim();
    let rest = trimmed.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    if !meta.to_ascii_lowercase().contains("base64") {
        return None;
    }
    let mime = meta
        .split(';')
        .next()
        .unwrap_or("image/png")
        .trim()
        .to_string();
    Some(ChatImage {
        mime: if mime.is_empty() {
            "image/png".into()
        } else {
            mime
        },
        data: payload.trim().to_string(),
    })
}

impl ChatMessage {
    pub fn has_images(&self) -> bool {
        self.images.iter().any(|img| !img.data.trim().is_empty())
    }
}

pub fn messages_have_images(messages: &[ChatMessage]) -> bool {
    messages.iter().any(ChatMessage::has_images)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedImage {
    #[serde(default)]
    pub mime: String,
    #[serde(default)]
    pub data: String,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<GeneratedImage>,
    #[serde(rename = "imageCount", skip_serializing_if = "is_zero_u32")]
    pub image_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timings: Option<InvokeTimings>,
    /// Compute slot that ran the job (`cuda-0`, `tp:cuda-0+cuda-1`, …).
    #[serde(rename = "slotId", skip_serializing_if = "Option::is_none")]
    pub slot_id: Option<String>,
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

#[derive(Debug, Serialize)]
pub struct InvokeErrorMessage {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: String,
    /// Stable code for routing / damage policy (agent_busy, model_load_failed, …).
    pub error: String,
    /// Truncated human detail for Scalattice admin / ops (not shown to API customers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ControlMessage {
    pub action: String,
}

#[derive(Debug, Serialize)]
pub struct ControlAckMessage {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub action: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LogsSubscribeMessage {
    /// subscribe | unsubscribe
    pub action: String,
    /// When true, include llama.cpp / ggml noise in cloud batches.
    #[serde(default)]
    pub verbose: bool,
}

#[derive(Debug, Serialize)]
pub struct LogsBatchMessage {
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// snapshot | live
    pub mode: &'static str,
    pub lines: Vec<LogsLinePayload>,
}

#[derive(Debug, Serialize)]
pub struct LogsLinePayload {
    #[serde(rename = "tsMs")]
    pub ts_ms: u64,
    pub level: String,
    pub msg: String,
}

/// Short operator-facing detail for cloud admin tooling. Avoid dumping megabyte traces.
pub fn cloud_invoke_error_detail(err: &anyhow::Error) -> String {
    let mut s = format!("{err:#}");
    // Soft-redact home directories so provider usernames are less exposed in admin UI.
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            s = s.replace(&home, "~");
        }
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        if !profile.is_empty() {
            s = s.replace(&profile, "%USERPROFILE%");
        }
    }
    const MAX: usize = 400;
    if s.len() > MAX {
        s.truncate(MAX);
        s.push('…');
    }
    s
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

/// Best-effort id so a malformed invoke can still be nacked without dropping the socket.
pub fn peek_invoke_id(data: &[u8]) -> String {
    #[derive(Deserialize)]
    struct IdOnly {
        id: Option<String>,
    }
    serde_json::from_slice::<IdOnly>(data)
        .ok()
        .and_then(|row| row.id)
        .unwrap_or_default()
}

pub fn parse_invoke_cancel(data: &[u8]) -> anyhow::Result<InvokeCancelMessage> {
    Ok(serde_json::from_slice(data)?)
}

pub fn parse_error(data: &[u8]) -> anyhow::Result<Value> {
    Ok(serde_json::from_slice(data)?)
}

/// Catalog `jobKind: image` only runs when the invoke also says `jobKind: image`.
/// Chat/VL probes (missing or `chat` jobKind) must never start Diffusers.
pub fn invoke_job_kind_error(
    catalog_image: bool,
    invoke_job_kind: &str,
) -> Option<(&'static str, &'static str)> {
    let invoke_image = invoke_job_kind.trim().eq_ignore_ascii_case("image");
    match (catalog_image, invoke_image) {
        (true, false) => Some((
            "image_model_chat_unsupported",
            "This catalog model generates images, not chat. Use jobKind image.",
        )),
        (false, true) => Some((
            "chat_model_image_unsupported",
            "This catalog model does not generate images.",
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_string_content() {
        let msg: ChatMessage =
            serde_json::from_str(r#"{"role":"user","content":"hello"}"#).unwrap();
        assert_eq!(msg.content, "hello");
        assert!(msg.images.is_empty());
    }

    #[test]
    fn deserializes_openai_image_parts() {
        let raw = r#"{
            "role":"user",
            "content":[
                {"type":"text","text":"what is this?"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,aaaa"}}
            ]
        }"#;
        let msg: ChatMessage = serde_json::from_str(raw).unwrap();
        assert_eq!(msg.content, "what is this?");
        assert_eq!(msg.images.len(), 1);
        assert_eq!(msg.images[0].data, "aaaa");
        assert_eq!(msg.images[0].mime, "image/png");
    }

    #[test]
    fn deserializes_inlined_images_field() {
        let raw =
            r#"{"role":"user","content":"look","images":[{"mime":"image/jpeg","data":"bbbb"}]}"#;
        let msg: ChatMessage = serde_json::from_str(raw).unwrap();
        assert_eq!(msg.content, "look");
        assert_eq!(msg.images[0].data, "bbbb");
    }

    #[test]
    fn image_catalog_rejects_chat_invoke() {
        let (code, _) = invoke_job_kind_error(true, "").unwrap();
        assert_eq!(code, "image_model_chat_unsupported");
        let (code, _) = invoke_job_kind_error(true, "chat").unwrap();
        assert_eq!(code, "image_model_chat_unsupported");
        assert!(invoke_job_kind_error(true, "image").is_none());
    }

    #[test]
    fn catalog_chat_serving_comes_from_fields_not_model_id() {
        let mut m = CatalogModel {
            model_id: "anything".into(),
            ..CatalogModel::default()
        };
        assert!(!m.uses_chatml());
        assert!(!m.thinking_none());
        m.chat_template = "chatml".into();
        m.thinking = "none".into();
        assert!(m.uses_chatml());
        assert!(m.thinking_none());
        assert!(!m.thinking_always());
        m.thinking = "always".into();
        assert!(m.thinking_always());
        assert!(!m.thinking_none());
        let wired: CatalogModel = serde_json::from_str(
            r#"{"modelId":"x","chatTemplate":"chatml","thinking":"none"}"#,
        )
        .unwrap();
        assert!(wired.uses_chatml());
        assert!(wired.thinking_none());
    }

    #[test]
    fn pong_treats_null_arrays_as_empty() {
        let msg: PongMessage = serde_json::from_str(
            r#"{"type":"pong","computeDevices":null,"enabledModels":null,"purgeModels":null}"#,
        )
        .unwrap();
        assert!(msg.compute_devices.is_empty());
        assert!(msg.enabled_models.is_empty());
        assert!(msg.purge_models.is_empty());
    }

    #[test]
    fn ready_treats_null_catalog_as_empty() {
        let msg: ReadyMessage = serde_json::from_str(
            r#"{"nodeId":"agent-1","catalog":null,"computeDevices":null,"enabledModels":null}"#,
        )
        .unwrap();
        assert_eq!(msg.node_id, "agent-1");
        assert!(msg.catalog.is_empty());
        assert!(msg.compute_devices.is_empty());
        assert!(msg.enabled_models.is_empty());
    }

    #[test]
    fn image_invoke_treats_null_messages_as_empty() {
        // Go encodes a nil slice as JSON null. Image jobs leave Messages unset,
        // which used to kill the WebSocket (`invalid type: null, expected a sequence`).
        let raw = r#"{"type":"invoke","id":"8d9cdc04-748f-4a08-b80d-470d562098fe","modelId":"qwen-image-2512","runtimeModel":"Qwen/Qwen-Image-2512","messages":null,"jobKind":"image","prompt":"a lantern","inputImages":null}"#;
        let msg: InvokeMessage = serde_json::from_str(raw).unwrap();
        assert!(msg.messages.is_empty());
        assert!(msg.input_images.is_empty());
        assert_eq!(msg.job_kind, "image");
        assert_eq!(msg.prompt, "a lantern");
        assert_eq!(
            peek_invoke_id(raw.as_bytes()),
            "8d9cdc04-748f-4a08-b80d-470d562098fe"
        );
    }
}
