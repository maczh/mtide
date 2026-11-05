use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{config::AiSettings, error::AppError};

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub language: String,
    pub file_path: String,
    pub line: usize,
    pub column: usize,
    pub prefix: String,
    pub text: String,
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [ChatMessage<'a>; 2],
    temperature: f32,
    max_tokens: u32,
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessageResponse,
}

#[derive(Debug, Deserialize)]
struct ChatMessageResponse {
    content: String,
}

pub fn complete(settings: &AiSettings, request: &CompletionRequest) -> Result<String, AppError> {
    let api_key = std::env::var(&settings.api_key_env).map_err(|_| {
        AppError::Workspace(format!(
            "AI API key environment variable {:?} is not set",
            settings.api_key_env
        ))
    })?;
    if api_key.is_empty() {
        return Err(AppError::Workspace(format!(
            "AI API key environment variable {:?} is empty",
            settings.api_key_env
        )));
    }
    let prompt = format!(
        "Language: {}\nFile: {}\nCursor: line {}, column {}\nPrefix: {:?}\n\nCode:\n{}",
        request.language,
        request.file_path,
        request.line,
        request.column,
        request.prefix,
        request.text
    );
    let body = serde_json::to_vec(&ChatRequest {
        model: &settings.model,
        messages: [
            ChatMessage {
                role: "system",
                content: "You are a code completion engine. Return only the short code to insert after the cursor. Do not use markdown fences, explanations, or quotes.".to_owned(),
            },
            ChatMessage {
                role: "user",
                content: prompt,
            },
        ],
        temperature: 0.2,
        max_tokens: 128,
    })
    .map_err(|error| AppError::Workspace(format!("encode AI completion request: {error}")))?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(
            settings.timeout_ms.clamp(500, 60_000),
        ))
        .build()
        .map_err(|error| AppError::Workspace(format!("create AI client: {error}")))?;
    let response = client
        .post(&settings.endpoint)
        .bearer_auth(api_key)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .map_err(|error| AppError::Workspace(format!("send AI completion request: {error}")))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .map_err(|error| AppError::Workspace(format!("read AI completion response: {error}")))?;
    if !status.is_success() {
        let detail = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| {
                value
                    .pointer("/error/message")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_default();
        return Err(AppError::Workspace(format!(
            "AI completion failed with {status}{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        )));
    }
    let decoded: ChatResponse = serde_json::from_slice(&bytes)
        .map_err(|error| AppError::Workspace(format!("decode AI completion response: {error}")))?;
    Ok(clean_completion(
        decoded
            .choices
            .first()
            .map_or("", |choice| choice.message.content.as_str()),
    ))
}

fn clean_completion(value: &str) -> String {
    let value = value.trim();
    if value.starts_with("```") && value.ends_with("```") {
        let mut lines = value.lines();
        let _ = lines.next();
        let body: Vec<_> = lines.collect();
        if let Some((last, rest)) = body.split_last() {
            if last.trim() == "```" {
                return rest.join("\n").trim().to_owned();
            }
        }
    }
    value.to_owned()
}

#[cfg(test)]
mod tests {
    use super::clean_completion;

    #[test]
    fn strips_markdown_fences_from_completion() {
        assert_eq!(clean_completion("```rust\nlet x = 1;\n```"), "let x = 1;");
        assert_eq!(clean_completion("return value"), "return value");
    }
}
