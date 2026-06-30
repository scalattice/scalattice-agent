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

pub fn sanitize_completion(model_id: &str, content: &str) -> String {
    let mut out = content.trim().to_string();
    if is_reasoning_model(model_id) {
        out = strip_thinking_blocks(&out);
    }
    out.trim().to_string()
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

fn is_reasoning_model(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    id.contains("deepseek-r1") || id.contains("r1-7b")
}

fn strip_thinking_blocks(content: &str) -> String {
    const OPEN: &str = concat!("<", "think", ">");
    const CLOSE: &str = concat!("</", "think", ">");
    let mut out = content.trim().to_string();
    loop {
        if let Some(close_idx) = out.find(CLOSE) {
            out = out[close_idx + CLOSE.len()..].trim().to_string();
            continue;
        }
        if let Some(open_idx) = out.find(OPEN) {
            out = out[..open_idx].trim().to_string();
        }
        break;
    }
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
    fn strips_reasoning_suffix() {
        const CLOSE: &str = concat!("</", "think", ">");
        let raw = format!("Let me think about this.{CLOSE}Hello there.");
        assert_eq!(sanitize_completion("deepseek-r1-7b", &raw), "Hello there.");
    }
}
