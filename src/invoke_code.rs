//! Invoke outcomes are a closed set. Callers branch on [`InvokeErrorCode`].
//! A human sentence is never the state.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvokeErrorCode {
    /// A fitting slot exists and is busy. Waiting can succeed.
    AgentBusy,
    /// Nothing idle can run this model. Waiting will not help.
    NoIdleSlot,
    InsufficientVram,
    RequestCanceled,
    InvokeTimeout,
    ModelLoadFailed,
    ModelNotInstalled,
    ModelOutOfMemory,
    PromptTooLong,
    InvalidImage,
    ImageAcceleratorRequired,
    ImageRuntimeMissing,
    ImageModelChatUnsupported,
    ChatModelImageUnsupported,
    ImageStreamUnsupported,
    ImageEditUnsupported,
    ImageEditRequired,
    ImageTooMany,
    DiskFull,
    /// Worker process died. The supervisor may try another slot.
    WorkerLost,
    VocabZeroCollapse,
    InferenceFailed,
}

impl InvokeErrorCode {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::AgentBusy => "agent_busy",
            Self::NoIdleSlot => "no_idle_slot",
            Self::InsufficientVram => "insufficient_vram",
            Self::RequestCanceled => "request_canceled",
            Self::InvokeTimeout => "invoke_timeout",
            Self::ModelLoadFailed => "model_load_failed",
            Self::ModelNotInstalled => "model_not_installed",
            Self::ModelOutOfMemory => "model_out_of_memory",
            Self::PromptTooLong => "prompt_too_long",
            Self::InvalidImage => "invalid_image",
            Self::ImageAcceleratorRequired => "image_accelerator_required",
            Self::ImageRuntimeMissing => "image_runtime_missing",
            Self::ImageModelChatUnsupported => "image_model_chat_unsupported",
            Self::ChatModelImageUnsupported => "chat_model_image_unsupported",
            Self::ImageStreamUnsupported => "image_stream_unsupported",
            Self::ImageEditUnsupported => "image_edit_unsupported",
            Self::ImageEditRequired => "image_edit_required",
            Self::ImageTooMany => "image_too_many",
            Self::DiskFull => "disk_full",
            // The socket still reports a failed invoke. Retry is an internal decision.
            Self::WorkerLost => "inference_failed",
            Self::VocabZeroCollapse => "vocab_zero_collapse",
            Self::InferenceFailed => "inference_failed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "agent_busy" => Some(Self::AgentBusy),
            "no_idle_slot" => Some(Self::NoIdleSlot),
            "insufficient_vram" | "no_vision_capacity" => Some(Self::InsufficientVram),
            "request_canceled" | "request_cancelled" => Some(Self::RequestCanceled),
            "invoke_timeout" | "provider_timeout" | "operator_timeout" => Some(Self::InvokeTimeout),
            "model_load_failed" => Some(Self::ModelLoadFailed),
            "model_not_installed" => Some(Self::ModelNotInstalled),
            "model_out_of_memory" | "out_of_memory" | "provider_out_of_memory" | "operator_out_of_memory" => {
                Some(Self::ModelOutOfMemory)
            }
            "prompt_too_long" => Some(Self::PromptTooLong),
            "invalid_image" | "image_unsupported" | "image_too_large" => Some(Self::InvalidImage),
            "image_accelerator_required" | "image_cuda_required" => {
                Some(Self::ImageAcceleratorRequired)
            }
            "image_runtime_missing" => Some(Self::ImageRuntimeMissing),
            "image_model_chat_unsupported" => Some(Self::ImageModelChatUnsupported),
            "chat_model_image_unsupported" => Some(Self::ChatModelImageUnsupported),
            "image_stream_unsupported" => Some(Self::ImageStreamUnsupported),
            "image_edit_unsupported" => Some(Self::ImageEditUnsupported),
            "image_edit_required" => Some(Self::ImageEditRequired),
            "image_too_many" => Some(Self::ImageTooMany),
            "disk_full" => Some(Self::DiskFull),
            "worker_lost" => Some(Self::WorkerLost),
            "vocab_zero_collapse" => Some(Self::VocabZeroCollapse),
            "inference_failed" => Some(Self::InferenceFailed),
            _ => None,
        }
    }

    /// Not evidence the machine is broken.
    pub fn is_benign(self) -> bool {
        matches!(
            self,
            Self::AgentBusy
                | Self::NoIdleSlot
                | Self::InsufficientVram
                | Self::RequestCanceled
                | Self::DiskFull
                | Self::PromptTooLong
                | Self::InvalidImage
                | Self::ModelNotInstalled
        )
    }
}

#[derive(Debug)]
pub struct CodedError {
    pub code: InvokeErrorCode,
    pub detail: String,
}

impl CodedError {
    pub fn new(code: InvokeErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for CodedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.detail.is_empty() {
            write!(f, "{}", self.code.as_wire())
        } else {
            write!(f, "{}: {}", self.code.as_wire(), self.detail)
        }
    }
}

impl std::error::Error for CodedError {}

pub fn coded(code: InvokeErrorCode, detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::from(CodedError::new(code, detail))
}

/// Wire code for an invoke failure. A coded error wins. A `code: detail` prefix
/// we emitted wins. Only then do we classify a foreign library or OS message.
pub fn code_of(err: &anyhow::Error) -> InvokeErrorCode {
    for cause in err.chain() {
        if let Some(coded) = cause.downcast_ref::<CodedError>() {
            return coded.code;
        }
    }
    let text = format!("{err:#}");
    if let Some(code) = leading_code(&text) {
        return code;
    }
    InvokeErrorCode::parse(classify_foreign(err)).unwrap_or(InvokeErrorCode::InferenceFailed)
}

pub fn wire_code(err: &anyhow::Error) -> &'static str {
    code_of(err).as_wire()
}

pub fn crash_retryable(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(coded) = cause.downcast_ref::<CodedError>() {
            return matches!(
                coded.code,
                InvokeErrorCode::WorkerLost | InvokeErrorCode::ModelOutOfMemory
            );
        }
    }
    false
}

/// Parse a worker or wire string that is either a bare code or `code: detail`.
pub fn error_from_wire(raw: &str) -> anyhow::Error {
    let raw = raw.trim();
    if let Some(code) = leading_code(raw) {
        let detail = raw
            .split_once(':')
            .map(|(_, rest)| rest.trim().to_string())
            .filter(|rest| !rest.is_empty() && InvokeErrorCode::parse(raw).is_none())
            .unwrap_or_default();
        // Bare code has no detail. `code: rest` keeps rest.
        let detail = if InvokeErrorCode::parse(raw).is_some() {
            String::new()
        } else {
            detail
        };
        return coded(code, detail);
    }
    anyhow::anyhow!("{raw}")
}

fn leading_code(raw: &str) -> Option<InvokeErrorCode> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let token = raw.split_once(':').map(|(head, _)| head).unwrap_or(raw).trim();
    if token.is_empty() || token.contains(char::is_whitespace) {
        return None;
    }
    InvokeErrorCode::parse(token)
}

/// Last resort for llama.cpp, CUDA, and OS text that arrived with no code.
/// Our own placement and worker failures must use [`coded`] instead of landing here.
fn classify_foreign(err: &anyhow::Error) -> &'static str {
    let detail = format!("{err:#}").to_lowercase();
    if detail.contains("request_canceled")
        || detail.contains("request_cancelled")
        || detail.contains("invoke_timeout")
    {
        "request_canceled"
    } else if detail.contains("insufficient_vram")
        || detail.contains("no placeable offload")
        || detail.contains("no_vision_capacity")
        || (detail.contains("need") && detail.contains("vision job") && detail.contains("gb"))
        || detail.contains("create llama context")
        || detail.contains("mps backend out of memory")
        || detail.contains("pytorch_mps")
        || detail.contains("high_watermark")
        || detail.contains("sigkill")
        || detail.contains("signal: 9")
        || (detail.contains("image worker") && detail.contains("ran out of memory"))
        // Opaque llama-cpp-2 null after a GPU cascade is almost always alloc/OOM,
        // not a corrupt GGUF. Real corrupt loads attach "corrupted or incomplete".
        || (detail.contains("null result")
            && !detail.contains("corrupted")
            && !detail.contains("incomplete gguf")
            && !detail.contains("unknown architecture"))
    {
        "insufficient_vram"
    } else if detail.contains("model_not_installed")
        || detail.contains("snapshot is incomplete")
        || detail.contains("snapshot is not on disk")
        || detail.contains("missing a weight shard")
        || detail.contains("missing a weight file")
        || (detail.contains("no such file") && detail.contains("safetensors"))
    {
        "model_not_installed"
    } else if detail.contains("load model")
        || detail.contains("load_from_file")
        || detail.contains("weights not found")
        || detail.contains("model weights not found")
        || detail.contains("unknown model architecture")
        || detail.contains("unknown architecture")
        || (detail.contains("gguf") && detail.contains("not found"))
    {
        "model_load_failed"
    } else if detail.contains("agent_busy")
        || detail.contains("no idle compute slot")
        || detail.contains("not available")
        || (detail.contains("sibling slot") && detail.contains("busy"))
        || detail.contains("erroroutdevicememory")
        || detail.contains("out of device memory")
    {
        "agent_busy"
    } else if detail.contains("out of memory")
        || detail.contains("oom")
        || detail.contains("cudamalloc")
        || detail.contains("failed to allocate")
        || detail.contains("no compute devices")
        || detail.contains("cuda error")
        || detail.contains("invalid device")
        || detail.contains("ggml_backend_cuda")
    {
        "model_out_of_memory"
    } else if detail.contains("context window") || detail.contains("too long") {
        "prompt_too_long"
    } else if detail.contains("invalid_image")
        || detail.contains("decode image for mmproj")
        || detail.contains("bitmap creation returned null")
    {
        "invalid_image"
    } else if detail.contains("image_accelerator_required") || detail.contains("image_cuda_required")
    {
        "image_accelerator_required"
    } else if detail.contains("image_runtime_missing") {
        "image_runtime_missing"
    } else if detail.contains("image_model_chat_unsupported") {
        "image_model_chat_unsupported"
    } else if detail.contains("chat_model_image_unsupported") {
        "chat_model_image_unsupported"
    } else if detail.contains("image_stream_unsupported") {
        "image_stream_unsupported"
    } else if crate::models::is_no_space_error(err) || detail.contains("disk_full") {
        "disk_full"
    } else if detail.contains("diffusers load failed")
        || detail.contains("qwen-image load failed")
        || detail.contains("model_load_failed")
    {
        "model_load_failed"
    } else if detail.contains("vocab_zero_collapse") {
        "vocab_zero_collapse"
    } else {
        "inference_failed"
    }
}
