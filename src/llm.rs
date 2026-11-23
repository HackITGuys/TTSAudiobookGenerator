//! Minimal blocking client for OpenAI-compatible chat completion APIs.
//!
//! `POST {base}/chat/completions` `{model, messages, temperature}` returns
//! `{"choices": [{"message": {"content": "..."}, "finish_reason": "stop"}]}`.
//! Mistral, OpenAI, OpenRouter, Ollama, LM Studio and vLLM all speak it.
//!
//! Defaults to Mistral (`https://api.mistral.ai/v1`) with the key that the
//! TTS stage already uses; `LLM_BASE_URL`, `LLM_API_KEY` and `LLM_MODEL`
//! point it elsewhere.

use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use serde::Serialize;

use crate::mistral::{API_KEY_ENV as MISTRAL_KEY_ENV, backoff_delay, retry_after};

pub const DEFAULT_BASE_URL: &str = "https://api.mistral.ai/v1";
pub const DEFAULT_MODEL: &str = "mistral-medium-latest";
pub const BASE_URL_ENV: &str = "LLM_BASE_URL";
pub const API_KEY_ENV: &str = "LLM_API_KEY";
pub const MODEL_ENV: &str = "LLM_MODEL";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatError {
    /// Retrying other requests is pointless (bad API key, unknown model).
    Fatal(String),
    /// Only this request failed (retries exhausted, truncated answer, ...).
    Failed(String),
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChatError::Fatal(m) | ChatError::Failed(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ChatError {}

/// Something that answers a chat. Implemented by [`ChatClient`]; tests
/// substitute a fake.
pub trait ChatBackend: Sync {
    fn chat(&self, model: &str, messages: &[ChatMessage]) -> Result<String, ChatError>;
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    temperature: f32,
}

pub struct ChatClient {
    agent: ureq::Agent,
    api_key: Option<String>,
    base_url: String,
    pub temperature: f32,
    pub max_retries: u32,
    /// First retry delay; doubles on each attempt (capped at 60 s).
    pub retry_base: Duration,
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl ChatClient {
    pub fn new(api_key: Option<String>, base_url: impl Into<String>) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(600)))
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            agent,
            api_key,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            temperature: 0.2,
            max_retries: 5,
            retry_base: Duration::from_secs(2),
        }
    }

    /// `LLM_BASE_URL` (default Mistral) and `LLM_API_KEY`, falling back to
    /// `MISTRAL_API_KEY`. A key is only required for the Mistral default;
    /// local servers such as Ollama need none.
    pub fn from_env() -> Result<Self> {
        let base = env_var(BASE_URL_ENV).unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let key = env_var(API_KEY_ENV).or_else(|| env_var(MISTRAL_KEY_ENV));
        if key.is_none() && base.trim_end_matches('/') == DEFAULT_BASE_URL {
            bail!(
                "{API_KEY_ENV} or {MISTRAL_KEY_ENV} is not set; export one or add it to a .env file"
            );
        }
        Ok(Self::new(key, base))
    }

    /// `--model`, else `LLM_MODEL`, else the default.
    pub fn model_from_env(cli: Option<String>) -> String {
        cli.or_else(|| env_var(MODEL_ENV))
            .unwrap_or_else(|| DEFAULT_MODEL.to_string())
    }
}

/// The text of `choices[0].message.content`, which is a string or (for some
/// models) a list of `{type: "text", text}` chunks.
fn answer_text(body: &serde_json::Value) -> Result<String, ChatError> {
    let choice = &body["choices"][0];
    if choice["finish_reason"] == "length" {
        return Err(ChatError::Failed(
            "answer was cut off at the model's output limit".into(),
        ));
    }
    let content = &choice["message"]["content"];
    let text = match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text" || p["type"].is_null())
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => {
            return Err(ChatError::Failed(format!(
                "unexpected chat response: {}",
                excerpt(&body.to_string())
            )));
        }
    };
    Ok(text)
}

fn excerpt(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > 300 {
        s.chars().take(300).collect::<String>() + "..."
    } else {
        s.to_string()
    }
}

impl ChatBackend for ChatClient {
    fn chat(&self, model: &str, messages: &[ChatMessage]) -> Result<String, ChatError> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = ChatRequest {
            model,
            messages,
            temperature: self.temperature,
        };
        let mut last_error = String::new();

        for attempt in 0..=self.max_retries {
            let mut req = self.agent.post(&url);
            if let Some(key) = &self.api_key {
                req = req.header("Authorization", format!("Bearer {key}"));
            }
            let mut resp = match req.send_json(&body) {
                Ok(r) => r,
                Err(e) => {
                    last_error = format!("network error: {e}");
                    if attempt < self.max_retries {
                        thread::sleep(backoff_delay(self.retry_base, attempt, None));
                    }
                    continue;
                }
            };

            let status = resp.status().as_u16();
            if status == 200 {
                let value: serde_json::Value = resp
                    .body_mut()
                    .with_config()
                    .limit(50 * 1024 * 1024)
                    .read_json()
                    .map_err(|e| ChatError::Failed(format!("unreadable chat response: {e}")))?;
                return answer_text(&value);
            }

            let wait = retry_after(resp.headers());
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            let message = format!("HTTP {status}: {}", excerpt(&text));
            match status {
                401 | 403 => {
                    return Err(ChatError::Fatal(format!(
                        "authentication failed ({message}); check {API_KEY_ENV} / {MISTRAL_KEY_ENV}"
                    )));
                }
                404 => {
                    return Err(ChatError::Fatal(format!(
                        "{url} or model '{model}' not found ({message})"
                    )));
                }
                408 | 429 | 500..=599 => {
                    last_error = message;
                    if attempt < self.max_retries {
                        thread::sleep(backoff_delay(self.retry_base, attempt, wait));
                    }
                }
                _ => return Err(ChatError::Failed(message)),
            }
        }
        Err(ChatError::Failed(format!(
            "gave up after {} attempts: {last_error}",
            self.max_retries + 1
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mistral::tests::mock_server;

    fn messages() -> Vec<ChatMessage> {
        vec![ChatMessage::system("Be brief."), ChatMessage::user("Hi")]
    }

    #[test]
    fn sends_openai_style_request_and_reads_answer() {
        let (url, seen) = mock_server(vec![(
            200,
            r#"{"choices":[{"message":{"role":"assistant","content":"Hello."},"finish_reason":"stop"}]}"#,
        )]);
        let client = ChatClient::new(Some("k".into()), format!("{url}/v1/"));
        assert_eq!(client.chat("m", &messages()).unwrap(), "Hello.");

        let req = seen.lock().unwrap()[0].clone();
        assert!(req.starts_with("POST /v1/chat/completions"));
        assert!(req.to_ascii_lowercase().contains("authorization: bearer k"));
        let body: serde_json::Value =
            serde_json::from_str(req.split("<body>").last().unwrap()).unwrap();
        assert_eq!(body["model"], "m");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "Hi");
    }

    #[test]
    fn reads_chunked_content_and_works_without_key() {
        let (url, seen) = mock_server(vec![(
            200,
            r#"{"choices":[{"message":{"content":[{"type":"thinking","thinking":[]},{"type":"text","text":"A"},{"type":"text","text":"B"}]},"finish_reason":"stop"}]}"#,
        )]);
        let client = ChatClient::new(None, url);
        assert_eq!(client.chat("m", &messages()).unwrap(), "AB");
        assert!(
            !seen.lock().unwrap()[0]
                .to_ascii_lowercase()
                .contains("authorization")
        );
    }

    #[test]
    fn retries_then_classifies_errors() {
        let (url, seen) = mock_server(vec![
            (429, "slow"),
            (
                200,
                r#"{"choices":[{"message":{"content":"x"},"finish_reason":"length"}]}"#,
            ),
            (401, "bad key"),
        ]);
        let mut client = ChatClient::new(Some("k".into()), url);
        client.retry_base = Duration::from_millis(1);
        assert!(matches!(
            client.chat("m", &messages()),
            Err(ChatError::Failed(m)) if m.contains("cut off")
        ));
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert!(matches!(
            client.chat("m", &messages()),
            Err(ChatError::Fatal(_))
        ));
    }
}
