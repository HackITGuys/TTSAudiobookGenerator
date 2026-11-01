//! Minimal blocking client for Mistral's text-to-speech API.
//!
//! - `POST /v1/audio/speech` `{model, input, voice_id, response_format}` returns
//!   `{"audio_data": "<base64>"}`.
//! - `GET /v2/audio/voices` returns `{data: [...], next_page_token}`.
//!
//! The API key is read from `MISTRAL_API_KEY` (the CLI also loads `.env`).

use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};

pub const DEFAULT_BASE_URL: &str = "https://api.mistral.ai";
pub const DEFAULT_MODEL: &str = "voxtral-mini-tts-2603";
pub const API_KEY_ENV: &str = "MISTRAL_API_KEY";
/// Optional override of the API base URL (for proxies and tests).
pub const BASE_URL_ENV: &str = "MISTRAL_API_BASE";

/// Base64 audio for a few minutes of WAV can exceed ureq's 10 MB default.
const MAX_RESPONSE_BYTES: u64 = 200 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct SpeechRequest<'a> {
    pub model: &'a str,
    pub input: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice_id: Option<&'a str>,
    pub response_format: &'a str,
}

#[derive(Deserialize)]
struct SpeechResponse {
    audio_data: String,
}

/// Why a speech request failed.
#[derive(Debug, Clone, PartialEq)]
pub enum SpeechError {
    /// Retrying other segments is pointless (bad API key, no access).
    Fatal(String),
    /// Mistral's moderation guardrail rejected this text; the payload names
    /// the violated categories, e.g. `hate_and_discrimination`.
    Blocked(String),
    /// Only this input failed (validation, retries exhausted).
    Failed(String),
}

impl std::fmt::Display for SpeechError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpeechError::Blocked(c) => write!(f, "blocked by Mistral moderation ({c})"),
            SpeechError::Fatal(m) | SpeechError::Failed(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for SpeechError {}

/// Something that turns text into audio bytes. Implemented by
/// [`MistralClient`]; tests substitute a fake.
pub trait SpeechBackend: Sync {
    fn speech(&self, req: &SpeechRequest) -> Result<Vec<u8>, SpeechError>;
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Voice {
    pub id: String,
    pub name: Option<String>,
    pub slug: Option<String>,
    pub gender: Option<String>,
    pub languages: Vec<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

#[derive(Deserialize)]
struct VoicePage {
    #[serde(default)]
    data: Vec<Voice>,
    next_page_token: Option<String>,
}

pub struct MistralClient {
    agent: ureq::Agent,
    api_key: String,
    base_url: String,
    pub max_retries: u32,
    /// First retry delay; doubles on each attempt (capped at 60 s).
    pub retry_base: Duration,
}

impl MistralClient {
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(300)))
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            agent,
            api_key: api_key.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            max_retries: 5,
            retry_base: Duration::from_secs(2),
        }
    }

    /// Build a client from `MISTRAL_API_KEY` (and optional `MISTRAL_API_BASE`).
    pub fn from_env() -> Result<Self> {
        let key = std::env::var(API_KEY_ENV)
            .ok()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty());
        let Some(key) = key else {
            bail!("{API_KEY_ENV} is not set; export it or add `{API_KEY_ENV}=...` to a .env file");
        };
        let base = std::env::var(BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Ok(Self::new(key, base))
    }

    fn backoff(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        let exp = self.retry_base.saturating_mul(1 << attempt.min(10));
        retry_after.unwrap_or(exp).min(Duration::from_secs(60))
    }

    /// List all voices available to this account.
    pub fn list_voices(&self) -> Result<Vec<Voice>> {
        let mut voices = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = self
                .agent
                .get(format!("{}/v2/audio/voices", self.base_url))
                .header("Authorization", format!("Bearer {}", self.api_key))
                .query("page_size", "100");
            if let Some(t) = &token {
                req = req.query("page_token", t);
            }
            let mut resp = req.call().context("request to Mistral failed")?;
            let status = resp.status().as_u16();
            if status != 200 {
                let body = resp.body_mut().read_to_string().unwrap_or_default();
                bail!("listing voices failed with HTTP {status}: {}", body.trim());
            }
            let page: VoicePage = resp
                .body_mut()
                .read_json()
                .context("unexpected voices response")?;
            voices.extend(page.data);
            match page.next_page_token {
                Some(t) if !t.is_empty() => token = Some(t),
                _ => break,
            }
        }
        Ok(voices)
    }
}

impl SpeechBackend for MistralClient {
    fn speech(&self, req: &SpeechRequest) -> Result<Vec<u8>, SpeechError> {
        let url = format!("{}/v1/audio/speech", self.base_url);
        let mut last_error = String::new();

        for attempt in 0..=self.max_retries {
            let result = self
                .agent
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .send_json(req);

            let mut resp = match result {
                Ok(r) => r,
                Err(e) => {
                    last_error = format!("network error: {e}");
                    if attempt < self.max_retries {
                        thread::sleep(self.backoff(attempt, None));
                    }
                    continue;
                }
            };

            let status = resp.status().as_u16();
            if status == 200 {
                let parsed: SpeechResponse = resp
                    .body_mut()
                    .with_config()
                    .limit(MAX_RESPONSE_BYTES)
                    .read_json()
                    .map_err(|e| SpeechError::Failed(format!("unexpected speech response: {e}")))?;
                return base64::engine::general_purpose::STANDARD
                    .decode(parsed.audio_data.trim())
                    .map_err(|e| SpeechError::Failed(format!("invalid base64 audio: {e}")));
            }

            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<f64>().ok())
                .map(Duration::from_secs_f64);
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            let message = format!("HTTP {status}: {}", body.trim());

            match status {
                401 => {
                    return Err(SpeechError::Fatal(format!(
                        "authentication failed ({message}); check {API_KEY_ENV}"
                    )));
                }
                429 | 500..=599 => {
                    last_error = message;
                    if attempt < self.max_retries {
                        thread::sleep(self.backoff(attempt, retry_after));
                    }
                }
                _ => {
                    if let Some(categories) = guardrail_categories(&body) {
                        return Err(SpeechError::Blocked(categories));
                    }
                    return Err(SpeechError::Failed(message));
                }
            }
        }
        Err(SpeechError::Failed(format!(
            "gave up after {} attempts: {last_error}",
            self.max_retries + 1
        )))
    }
}

/// Violated categories of a `guardrail_violation` error body, e.g.
/// `{"type":"guardrail_violation","guardrails":[{"moderation_llm_v2":
/// {"categories":{"hate_and_discrimination":{"violated":true}}}}]}`.
fn guardrail_categories(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if v["type"] != "guardrail_violation" {
        return None;
    }
    let mut violated = Vec::new();
    for guard in v["guardrails"].as_array().into_iter().flatten() {
        for policy in guard.as_object().into_iter().flat_map(|o| o.values()) {
            for (name, c) in policy["categories"].as_object().into_iter().flatten() {
                if c["violated"] == true {
                    violated.push(name.clone());
                }
            }
        }
    }
    Some(if violated.is_empty() {
        "guardrail policy".into()
    } else {
        violated.join(", ")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// Serve canned HTTP responses, one per connection, and record request bodies.
    fn mock_server(responses: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_thread = seen.clone();
        thread::spawn(move || {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                let mut len = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    head.push_str(&line);
                }
                let mut body_in = vec![0; len];
                reader.read_exact(&mut body_in).unwrap();
                seen_thread
                    .lock()
                    .unwrap()
                    .push(format!("{head}<body>{}", String::from_utf8_lossy(&body_in)));
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nRetry-After: 0\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        (addr, seen)
    }

    fn request() -> SpeechRequest<'static> {
        SpeechRequest {
            model: DEFAULT_MODEL,
            input: "Hello there.",
            voice_id: Some("en_paul_neutral"),
            response_format: "mp3",
        }
    }

    #[test]
    fn decodes_audio_and_sends_expected_request() {
        // "ID3abc" base64-encoded
        let (url, seen) = mock_server(vec![(200, r#"{"audio_data":"SUQzYWJj"}"#)]);
        let client = MistralClient::new("test-key", url);
        let audio = client.speech(&request()).unwrap();
        assert_eq!(audio, b"ID3abc");

        let req = seen.lock().unwrap()[0].clone();
        assert!(req.starts_with("POST /v1/audio/speech"));
        assert!(
            req.to_ascii_lowercase()
                .contains("authorization: bearer test-key")
        );
        let body: serde_json::Value =
            serde_json::from_str(req.split("<body>").last().unwrap()).unwrap();
        assert_eq!(body["model"], "voxtral-mini-tts-2603");
        assert_eq!(body["input"], "Hello there.");
        assert_eq!(body["voice_id"], "en_paul_neutral");
        assert_eq!(body["response_format"], "mp3");
    }

    #[test]
    fn retries_rate_limits_and_server_errors() {
        let (url, seen) = mock_server(vec![
            (429, r#"{"message":"slow down"}"#),
            (503, "unavailable"),
            (200, r#"{"audio_data":"QUJD"}"#),
        ]);
        let mut client = MistralClient::new("k", url);
        client.retry_base = Duration::from_millis(1);
        assert_eq!(client.speech(&request()).unwrap(), b"ABC");
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[test]
    fn classifies_errors() {
        let (url, _) = mock_server(vec![
            (401, "bad key"),
            (403, "moderated"),
            (500, "a"),
            (500, "b"),
        ]);
        let mut client = MistralClient::new("k", url);
        client.retry_base = Duration::from_millis(1);
        assert!(matches!(
            client.speech(&request()),
            Err(SpeechError::Fatal(_))
        ));
        assert!(
            matches!(client.speech(&request()), Err(SpeechError::Failed(m)) if m.contains("403"))
        );
        client.max_retries = 1;
        assert!(
            matches!(client.speech(&request()), Err(SpeechError::Failed(m)) if m.contains("gave up"))
        );
    }

    #[test]
    fn recognises_guardrail_blocks() {
        let body = r#"{"object":"error","message":"Request blocked by guardrail policy","type":"guardrail_violation","param":null,"code":"1920","raw_status_code":403,"guardrails":[{"moderation_llm_v2":{"action":"block","categories":{"sexual":{"violated":false},"hate_and_discrimination":{"violated":true},"selfharm":{"violated":false},"jailbreaking":{"violated":false}}}}]}"#;
        let (url, _) = mock_server(vec![(403, body)]);
        let err = MistralClient::new("k", url).speech(&request()).unwrap_err();
        assert_eq!(err, SpeechError::Blocked("hate_and_discrimination".into()));
        assert_eq!(
            err.to_string(),
            "blocked by Mistral moderation (hate_and_discrimination)"
        );
    }

    #[test]
    fn lists_voices_across_pages() {
        let (url, seen) = mock_server(vec![
            (
                200,
                r#"{"data":[{"id":"v1","name":"Paul","gender":"male","languages":["en"]}],"next_page_token":"p2"}"#,
            ),
            (
                200,
                r#"{"data":[{"id":"v2","name":"Marie","type":"preset"}],"next_page_token":null}"#,
            ),
        ]);
        let voices = MistralClient::new("k", url).list_voices().unwrap();
        assert_eq!(voices.len(), 2);
        assert_eq!(voices[0].gender.as_deref(), Some("male"));
        assert_eq!(voices[1].kind.as_deref(), Some("preset"));
        assert!(seen.lock().unwrap()[1].contains("page_token=p2"));
    }
}
