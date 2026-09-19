use crate::protocol::ChatMessage;
use anyhow::{Context, Result};
use llama_cpp_2::model::{LlamaChatMessage, LlamaModel};

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";

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
    model_id: &str,
) -> Result<String> {
    let mut prepared = prepare_messages(messages);
    // Official Qwen3-Coder GGUF jinja uses {% macro %}, tools, tojson — llama.cpp minja
    // often fails or emits a non-ChatML string. Plaintext "User:" fallback then
    // collapses decode to vocab token 0 (`!`). Instruct Coder is ChatML-only.
    if model_forces_no_think(model_id) {
        strip_user_think_tags(&mut prepared);
        return Ok(chatml_prompt(&prepared, true));
    }
    let baked = match model.chat_template(None) {
        Ok(tmpl) => tmpl.to_str().ok().map(|s| s.to_string()),
        Err(_) => None,
    };
    if let Some(s) = baked.as_deref() {
        suppress_short_completion_thinking(s, &mut prepared, max_tokens, n_ctx, model_id);
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
            Ok(prompt) => {
                if baked.as_deref().is_some_and(template_is_chatml) && !prompt_is_chatml(&prompt) {
                    tracing::warn!(
                        model_id,
                        "chat template apply omitted ChatML markers; using ChatML"
                    );
                    return Ok(chatml_prompt(&prepared, true));
                }
                Ok(prompt)
            }
            Err(err) => {
                tracing::warn!(error = %err, model_id, "chat template apply failed");
                Ok(fallback_prompt(model_id, baked.as_deref(), &prepared))
            }
        },
        Err(_) => Ok(fallback_prompt(model_id, baked.as_deref(), &prepared)),
    }
}

pub(crate) fn template_has_thinking_switch(template: &str) -> bool {
    template.contains("enable_thinking")
        || template.contains("no_think")
        || template.contains("/think")
}

/// True when `max_tokens` can hold chain-of-thought and still emit an answer.
/// Debug 48-token probes stay off. Bracket's default 1024 is enough on 4k/8k
/// windows, but at catalog 32k it is a tiny slice — Qwen3 spends the budget
/// inside `<think>` or collapses to vocab token 0 (`!`).
pub(crate) fn completion_can_afford_thinking(max_tokens: u32, n_ctx: u32) -> bool {
    max_tokens >= 256 && max_tokens.saturating_mul(8) >= n_ctx.max(1)
}

/// Qwen3-Coder is instruct-only. `/think` on those SKUs emits `!` (token 0).
pub(crate) fn model_forces_no_think(model_id: &str) -> bool {
    model_id
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|part| part.eq_ignore_ascii_case("coder"))
}

fn template_is_chatml(template: &str) -> bool {
    template.contains("<|im_start|>") || template.contains("im_start")
}

fn prompt_is_chatml(prompt: &str) -> bool {
    prompt.contains("<|im_start|>")
}

fn strip_user_think_tags(messages: &mut [ChatMessage]) {
    for m in messages.iter_mut() {
        if m.role != "user" {
            continue;
        }
        let stripped = m
            .content
            .replace(NO_THINK_TAG, "")
            .replace(THINK_TAG, "")
            .trim()
            .to_string();
        if !stripped.is_empty() {
            m.content = stripped;
        }
    }
}

pub(crate) fn chatml_prompt(messages: &[ChatMessage], add_generation: bool) -> String {
    let mut out = String::new();
    for message in messages {
        let role = normalize_role(&message.role);
        out.push_str("<|im_start|>");
        out.push_str(&role);
        out.push('\n');
        out.push_str(&super::vision::content_with_media_markers(message));
        out.push_str("<|im_end|>\n");
    }
    if add_generation {
        out.push_str("<|im_start|>assistant\n");
    }
    out
}

fn fallback_prompt(
    model_id: &str,
    baked_template: Option<&str>,
    messages: &[ChatMessage],
) -> String {
    if model_forces_no_think(model_id)
        || baked_template.is_some_and(template_is_chatml)
        || model_id_looks_like_qwen(model_id)
    {
        chatml_prompt(messages, true)
    } else {
        messages_to_prompt_fallback(messages)
    }
}

fn model_id_looks_like_qwen(model_id: &str) -> bool {
    model_id
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|part| part.eq_ignore_ascii_case("qwen"))
}

fn set_last_user_think_tag(messages: &mut [ChatMessage], tag: &str) {
    let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == "user") else {
        return;
    };
    let stripped = last_user
        .content
        .replace(NO_THINK_TAG, "")
        .replace(THINK_TAG, "")
        .trim_end()
        .to_string();
    last_user.content = if stripped.is_empty() {
        tag.to_string()
    } else {
        format!("{stripped}\n{tag}")
    };
}

/// Qwen3-family Jinja looks for `/no_think` on the last user turn when
/// `enable_thinking` is not passed through llama-cpp-2's apply_chat_template.
/// Client `/think` cannot override a completion that cannot afford CoT.
pub(crate) fn suppress_short_completion_thinking(
    template: &str,
    messages: &mut [ChatMessage],
    max_tokens: u32,
    n_ctx: u32,
    model_id: &str,
) {
    let force_off =
        model_forces_no_think(model_id) || !completion_can_afford_thinking(max_tokens, n_ctx);
    if !force_off {
        return;
    }
    if !template_has_thinking_switch(template) {
        return;
    }
    set_last_user_think_tag(messages, NO_THINK_TAG);
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
        suppress_short_completion_thinking(tmpl, &mut msgs, 48, 4096, "qwen-3-8b");
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
        suppress_short_completion_thinking(tmpl, &mut msgs, 1024, 4096, "qwen-3-8b");
        assert!(!msgs.iter().any(|m| m.content.contains("/no_think")));
    }

    #[test]
    fn thinking_cutoff_scales_with_context_window() {
        assert!(!completion_can_afford_thinking(48, 4096));
        assert!(!completion_can_afford_thinking(48, 8192));
        assert!(!completion_can_afford_thinking(128, 4096));
        assert!(completion_can_afford_thinking(1024, 4096));
        assert!(completion_can_afford_thinking(1024, 8192));
        assert!(!completion_can_afford_thinking(1024, 32768));
        assert!(completion_can_afford_thinking(4096, 32768));
    }

    #[test]
    fn overrides_think_when_completion_cannot_afford_cot() {
        let tmpl = "enable_thinking";
        let mut msgs = prepare_messages(&[ChatMessage {
            role: "user".into(),
            content: "Plan this /think".into(),
            images: Vec::new(),
        }]);
        suppress_short_completion_thinking(tmpl, &mut msgs, 48, 4096, "qwen-3-8b");
        assert!(msgs[1].content.contains("/no_think"));
        assert!(!msgs[1].content.contains("/think\n") && !msgs[1].content.ends_with("/think"));
    }

    #[test]
    fn coder_models_force_no_think_even_on_long_completions() {
        assert!(model_forces_no_think("qwen-3-coder-30b-a3b"));
        assert!(!model_forces_no_think("qwen-3-8b"));
        let tmpl = "enable_thinking";
        let mut msgs = prepare_messages(&[ChatMessage {
            role: "user".into(),
            content: "test /think".into(),
            images: Vec::new(),
        }]);
        suppress_short_completion_thinking(tmpl, &mut msgs, 4096, 32768, "qwen-3-coder-30b-a3b");
        assert!(msgs[1].content.contains("/no_think"));
    }

    #[test]
    fn keeps_think_when_completion_fits_the_window() {
        let tmpl = "enable_thinking";
        let mut msgs = prepare_messages(&[ChatMessage {
            role: "user".into(),
            content: "Plan this /think".into(),
            images: Vec::new(),
        }]);
        suppress_short_completion_thinking(tmpl, &mut msgs, 1024, 4096, "qwen-3-8b");
        assert!(msgs[1].content.contains("/think"));
        assert!(!msgs[1].content.contains("/no_think"));
    }

    #[test]
    fn coder_prompt_is_chatml_and_drops_think_tags() {
        let mut msgs = prepare_messages(&[ChatMessage {
            role: "user".into(),
            content: "test\n/think".into(),
            images: Vec::new(),
        }]);
        strip_user_think_tags(&mut msgs);
        let prompt = chatml_prompt(&msgs, true);
        assert!(prompt.contains("<|im_start|>user\ntest<|im_end|>"));
        assert!(!prompt.contains("/think"));
        assert!(prompt.ends_with("<|im_start|>assistant\n"));
        assert!(!prompt.contains("User:"));
    }

    #[test]
    fn chatml_template_apply_failure_does_not_use_plaintext_roles() {
        let msgs = prepare_messages(&[ChatMessage {
            role: "user".into(),
            content: "hi".into(),
            images: Vec::new(),
        }]);
        // Non-coder id: ChatML fallback must come from the baked template, not the coder SKU check.
        let prompt = fallback_prompt("any-chat-model", Some("<|im_start|>{{ content }}"), &msgs);
        assert!(prompt.contains("<|im_start|>user\nhi<|im_end|>"));
        assert!(!prompt.contains("User:"));
    }

    #[test]
    fn qwen38_fallback_is_chatml_not_plaintext_user() {
        let msgs = prepare_messages(&[ChatMessage {
            role: "user".into(),
            content: "hi".into(),
            images: Vec::new(),
        }]);
        let prompt = fallback_prompt("qwen-3.8-27b", None, &msgs);
        assert!(prompt.contains("<|im_start|>user\nhi<|im_end|>"));
        assert!(!prompt.contains("User:"));
    }
}
