use crate::protocol::ChatMessage;
use anyhow::{Context, Result};
use llama_cpp_2::model::{LlamaChatMessage, LlamaModel};

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";

/// Completion must cover this many slices of `n_ctx` or thinking eats the probe.
/// 48 tokens at 4096 ctx is a debug/health completion, not a reasoned reply.
const THINK_COMPLETION_CTX_SLICES: u32 = 32;
const NO_THINK_TAG: &str = "/no_think";
const THINK_TAG: &str = "/think";

pub fn prepare_messages(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = messages
        .iter()
        .filter(|m| !m.content.trim().is_empty() || m.has_images())
        .cloned()
        .collect();
    let has_system = out.iter().any(|m| m.role == "system");
    if !has_system {
        out.insert(
            0,
            ChatMessage {
                role: "system".to_string(),
                content: DEFAULT_SYSTEM_PROMPT.to_string(),
                images: Vec::new(),
            },
        );
    }
    out
}

pub fn build_chat_prompt(
    model: &LlamaModel,
    messages: &[ChatMessage],
    max_tokens: u32,
    n_ctx: u32,
) -> Result<String> {
    let mut prepared = prepare_messages(messages);
    if let Ok(tmpl) = model.chat_template(None) {
        if let Ok(s) = tmpl.to_str() {
            suppress_short_completion_thinking(s, &mut prepared, max_tokens, n_ctx);
        }
    }
    let llama_messages: Vec<LlamaChatMessage> = prepared
        .iter()
        .map(|m| {
            let role = normalize_role(&m.role);
            let content = super::vision::content_with_media_markers(m);
            LlamaChatMessage::new(role, content)
        })
        .collect::<Result<_, _>>()
        .context("build llama chat messages")?;

    match model.chat_template(None) {
        Ok(tmpl) => match model.apply_chat_template(&tmpl, &llama_messages, true) {
            Ok(prompt) => Ok(prompt),
            Err(err) => {
                tracing::warn!(error = %err, "chat template apply failed; using plaintext fallback");
                Ok(messages_to_prompt_fallback(&prepared))
            }
        },
        Err(_) => Ok(messages_to_prompt_fallback(&prepared)),
    }
}

pub(crate) fn template_has_thinking_switch(template: &str) -> bool {
    template.contains("enable_thinking")
        || template.contains("no_think")
        || template.contains("/think")
}

/// True when `max_tokens` is a large enough slice of the context window to
/// hold chain-of-thought and still emit an answer.
pub(crate) fn completion_can_afford_thinking(max_tokens: u32, n_ctx: u32) -> bool {
    max_tokens.saturating_mul(THINK_COMPLETION_CTX_SLICES) >= n_ctx.max(1)
}

/// Qwen3-family Jinja looks for `/no_think` on the last user turn when
/// `enable_thinking` is not passed through llama-cpp-2's apply_chat_template.
pub(crate) fn suppress_short_completion_thinking(
    template: &str,
    messages: &mut [ChatMessage],
    max_tokens: u32,
    n_ctx: u32,
) {
    if completion_can_afford_thinking(max_tokens, n_ctx) {
        return;
    }
    if !template_has_thinking_switch(template) {
        return;
    }
    let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == "user") else {
        return;
    };
    if last_user.content.contains(NO_THINK_TAG) || last_user.content.contains(THINK_TAG) {
        return;
    }
    if last_user.content.is_empty() {
        last_user.content = NO_THINK_TAG.to_string();
    } else {
        last_user.content.push('\n');
        last_user.content.push_str(NO_THINK_TAG);
    }
}

/// Trim only: do not strip model-specific reasoning markers.
/// Open R1-class hosts leave `<think>…</think>` in `content` for the client.
pub fn sanitize_completion(_model_id: &str, content: &str) -> String {
    content.trim().to_string()
}

fn normalize_role(role: &str) -> String {
    match role.trim().to_lowercase().as_str() {
        "system" => "system".to_string(),
        "assistant" => "assistant".to_string(),
        _ => "user".to_string(),
    }
}

fn messages_to_prompt_fallback(messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    for message in messages {
        let role = match message.role.as_str() {
            "system" => "System",
            "assistant" => "Assistant",
            _ => "User",
        };
        out.push_str(&format!(
            "{role}: {}\n",
            super::vision::content_with_media_markers(message)
        ));
    }
    out.push_str("Assistant: ");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_default_system_message() {
        let prepared = prepare_messages(&[ChatMessage {
            role: "user".into(),
            content: "Hi".into(),
            images: Vec::new(),
        }]);
        assert_eq!(prepared[0].role, "system");
        assert_eq!(prepared[1].content, "Hi");
    }

    #[test]
    fn passes_through_reasoning_markers() {
        const OPEN: &str = concat!("<", "think", ">");
        const CLOSE: &str = concat!("</", "think", ">");
        let raw = format!("{OPEN}reason{CLOSE}\nHello there.");
        assert_eq!(
            sanitize_completion("deepseek-r1-7b", &raw),
            format!("{OPEN}reason{CLOSE}\nHello there.")
        );
    }

    #[test]
    fn keeps_image_only_user_message() {
        let prepared = prepare_messages(&[ChatMessage {
            role: "user".into(),
            content: String::new(),
            images: vec![crate::protocol::ChatImage {
                mime: "image/png".into(),
                data: "aaaa".into(),
            }],
        }]);
        assert_eq!(prepared[1].role, "user");
        assert!(prepared[1].has_images());
        assert!(
            crate::llm::vision::content_with_media_markers(&prepared[1]).contains("<__media__>")
        );
    }

    #[test]
    fn short_debug_probe_appends_no_think() {
        let tmpl = "{%- if enable_thinking is defined %}x{% endif %}";
        let mut msgs = prepare_messages(&[ChatMessage {
            role: "user".into(),
            content: "Say ok.".into(),
            images: Vec::new(),
        }]);
        suppress_short_completion_thinking(tmpl, &mut msgs, 48, 4096);
        assert!(msgs
            .iter()
            .any(|m| m.role == "user" && m.content.contains("/no_think")));
    }

    #[test]
    fn long_completions_keep_default_thinking() {
        let tmpl = "{%- if enable_thinking is defined %}x{% endif %}";
        let mut msgs = prepare_messages(&[ChatMessage {
            role: "user".into(),
            content: "Write an essay.".into(),
            images: Vec::new(),
        }]);
        suppress_short_completion_thinking(tmpl, &mut msgs, 1024, 4096);
        assert!(!msgs.iter().any(|m| m.content.contains("/no_think")));
    }

    #[test]
    fn thinking_cutoff_scales_with_context_window() {
        assert!(!completion_can_afford_thinking(48, 4096));
        assert!(!completion_can_afford_thinking(48, 8192));
        assert!(completion_can_afford_thinking(128, 4096));
        assert!(completion_can_afford_thinking(1024, 8192));
    }

    #[test]
    fn does_not_override_explicit_think_tag() {
        let tmpl = "enable_thinking";
        let mut msgs = prepare_messages(&[ChatMessage {
            role: "user".into(),
            content: "Plan this /think".into(),
            images: Vec::new(),
        }]);
        suppress_short_completion_thinking(tmpl, &mut msgs, 48, 4096);
        assert!(!msgs[1].content.contains("/no_think"));
    }
}
