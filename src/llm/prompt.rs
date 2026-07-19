use crate::protocol::ChatMessage;
use anyhow::{Context, Result};
use llama_cpp_2::model::{LlamaChatMessage, LlamaModel};

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";

pub fn prepare_messages(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = messages
        .iter()
        .filter(|m| !m.content.trim().is_empty())
        .cloned()
        .collect();
    let has_system = out.iter().any(|m| m.role == "system");
    if !has_system {
        out.insert(
            0,
            ChatMessage {
                role: "system".to_string(),
                content: DEFAULT_SYSTEM_PROMPT.to_string(),
            },
        );
    }
    out
}

pub fn build_chat_prompt(model: &LlamaModel, messages: &[ChatMessage]) -> Result<String> {
    let prepared = prepare_messages(messages);
    let llama_messages: Vec<LlamaChatMessage> = prepared
        .iter()
        .map(|m| {
            let role = normalize_role(&m.role);
            LlamaChatMessage::new(role, m.content.clone())
        })
        .collect::<Result<_, _>>()
        .context("build llama chat messages")?;

    match model.chat_template(None) {
        Ok(tmpl) => model
            .apply_chat_template(&tmpl, &llama_messages, true)
            .context("apply model chat template"),
        Err(_) => Ok(messages_to_prompt_fallback(&prepared)),
    }
}

/// Trim only — do not strip model-specific reasoning markers.
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
        out.push_str(&format!("{role}: {}\n", message.content));
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
}
