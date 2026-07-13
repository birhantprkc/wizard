//! Streaming HTTP client for Ollama's native `/api/chat` endpoint.
//!
//! Thin `reqwest` wrapper — no `ollama-rs` dependency, keeping the binary
//! small. Provides a startup health probe, a native-tool-support probe, and
//! NDJSON streaming chat.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt, stream};
use serde::Deserialize;

use super::provider::LlmProvider;
use super::{ChatChunk, ChatOptions, ChatRequest, ProviderError};

/// Boxed NDJSON chunk stream returned by [`OllamaClient::chat_stream`].
/// Re-exported from [`crate::llm`] so existing `ollama::ChatStream` paths
/// keep compiling.
pub use super::ChatStream;

/// How long to wait for a TCP/TLS connection before declaring Ollama down.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Overall timeout for small control requests (`/api/tags`, `/api/show`).
/// Chat requests are exempt — generation can legitimately take minutes.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// Request context length when the `/api/show` probe fails. Well above
/// Ollama's silent 4096 default, which truncates agent history server-side.
const DEFAULT_NUM_CTX: u32 = 16_384;
/// Cap on the probe-derived `num_ctx`: a 128k+ token KV cache can exhaust
/// RAM on the machines Ollama typically runs on. An explicit `num_ctx` in
/// the request is passed through untouched.
const MAX_DERIVED_NUM_CTX: u32 = 32_768;

/// Errors specific to talking to Ollama, surfaced so the TUI can render
/// actionable messages (e.g. "is Ollama running?").
#[derive(Debug, thiserror::Error)]
pub enum OllamaError {
    #[error(
        "cannot reach Ollama at {host} — is the server running? Start it with `ollama serve` (or check `ollama_host` in ~/.wizard/config.toml). Cause: {source}"
    )]
    Unreachable {
        host: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("model '{0}' is not installed (try `ollama pull {0}`)")]
    ModelMissing(String),
    #[error("Ollama returned HTTP {status}: {body}")]
    Api {
        status: reqwest::StatusCode,
        body: String,
    },
}

impl OllamaError {
    /// Whether this error is transient — a retry after backoff may succeed.
    /// Connection/timeout failures and server-busy/rate-limit/5xx statuses
    /// are transient; a missing model or a 4xx (other than 429) is not.
    pub fn is_transient(&self) -> bool {
        match self {
            OllamaError::Unreachable { .. } => true,
            OllamaError::ModelMissing(_) => false,
            OllamaError::Api { status, .. } => status.as_u16() == 429 || status.is_server_error(),
        }
    }
}

/// Wrap an [`OllamaError`] so callers can classify it either way:
/// `downcast_ref::<OllamaError>()` (legacy agent retry path) or
/// `downcast_ref::<ProviderError>()` (shared provider retry contract).
/// The two classifications agree: `ModelMissing` maps to its originating
/// 404 and `Unreachable` to a transport failure.
fn typed(err: OllamaError) -> anyhow::Error {
    let status = match &err {
        OllamaError::Unreachable { .. } => None,
        OllamaError::ModelMissing(_) => Some(404),
        OllamaError::Api { status, .. } => Some(status.as_u16()),
    };
    let provider = ProviderError {
        status,
        message: err.to_string(),
    };
    anyhow::Error::new(err).context(provider)
}

/// Client bound to one Ollama host. Cheap to clone.
#[derive(Debug, Clone)]
pub struct OllamaClient {
    http: reqwest::Client,
    host: String,
    /// Per-model derived `num_ctx` (see [`OllamaClient::derived_num_ctx`]);
    /// failed probes cache the fallback so they are not retried per request.
    num_ctx_cache: Arc<Mutex<HashMap<String, u32>>>,
}

impl OllamaClient {
    /// Create a client for `host` (e.g. `http://127.0.0.1:11434`). Trailing
    /// slashes are trimmed.
    pub fn new(host: impl Into<String>) -> Self {
        let host = host.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            // Builder construction only fails when the TLS backend cannot
            // initialize; fall back to the default client rather than panic.
            .unwrap_or_default();
        Self {
            http,
            host,
            num_ctx_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Base URL this client talks to.
    pub fn host(&self) -> &str {
        &self.host
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.host, path)
    }

    /// Map a transport-level `reqwest` failure into an actionable error.
    /// Connection refusals and timeouts become [`OllamaError::Unreachable`],
    /// which tells the user to run `ollama serve`.
    fn transport_error(&self, source: reqwest::Error) -> anyhow::Error {
        if source.is_connect() || source.is_timeout() {
            typed(OllamaError::Unreachable {
                host: self.host.clone(),
                source,
            })
        } else {
            let message = format!("HTTP request to {} failed: {source}", self.host);
            anyhow::Error::new(source).context(ProviderError::transport(message))
        }
    }

    /// Read the body of a non-success response and convert it into
    /// [`OllamaError::ModelMissing`] (404 mentioning the model) or
    /// [`OllamaError::Api`].
    async fn status_error(
        &self,
        response: reqwest::Response,
        model: Option<&str>,
    ) -> anyhow::Error {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if let Some(model) = model
            && status == reqwest::StatusCode::NOT_FOUND
            && body.contains("not found")
        {
            return typed(OllamaError::ModelMissing(model.to_string()));
        }
        typed(OllamaError::Api { status, body })
    }

    /// Startup health probe: `GET /api/tags`. Errors with
    /// [`OllamaError::Unreachable`] when the server is down.
    pub async fn health(&self) -> Result<()> {
        let response = self
            .http
            .get(self.url("/api/tags"))
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if !response.status().is_success() {
            return Err(self.status_error(response, None).await);
        }
        Ok(())
    }

    /// List locally installed model tags (`GET /api/tags`).
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let response = self
            .http
            .get(self.url("/api/tags"))
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if !response.status().is_success() {
            return Err(self.status_error(response, None).await);
        }
        let tags: TagsResponse = response
            .json()
            .await
            .context("failed to parse /api/tags response")?;
        Ok(tags.models.into_iter().map(|m| m.name).collect())
    }

    /// Probe whether `model` supports native tool calling
    /// (`POST /api/show`, inspect `capabilities` for `"tools"`). When this
    /// returns `false` the agent loop falls back to a prompt-based JSON tool
    /// protocol (see `docs/byom.md`).
    pub async fn supports_native_tools(&self, model: &str) -> Result<bool> {
        let response = self
            .http
            .post(self.url("/api/show"))
            .timeout(PROBE_TIMEOUT)
            .json(&serde_json::json!({ "model": model }))
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if !response.status().is_success() {
            return Err(self.status_error(response, Some(model)).await);
        }
        let info: ShowResponse = response
            .json()
            .await
            .context("failed to parse /api/show response")?;
        let supported = info
            .capabilities
            .as_deref()
            .is_some_and(|caps| caps.iter().any(|c| c == "tools"));
        if !supported {
            tracing::debug!(
                model,
                capabilities = ?info.capabilities,
                "model does not advertise native tool support; \
                 the agent loop will use the JSON tool protocol"
            );
        }
        Ok(supported)
    }

    /// Effective `num_ctx` for `model` when the request does not set one:
    /// the model's trained context length from `/api/show` (capped at
    /// [`MAX_DERIVED_NUM_CTX`]), or [`DEFAULT_NUM_CTX`] when the probe
    /// fails. Probed once per model per client.
    async fn derived_num_ctx(&self, model: &str) -> u32 {
        if let Some(&cached) = self.num_ctx_cache.lock().unwrap().get(model) {
            return cached;
        }
        let derived = self
            .model_context_length(model)
            .await
            .map(|n| n.min(MAX_DERIVED_NUM_CTX))
            .unwrap_or(DEFAULT_NUM_CTX);
        self.num_ctx_cache
            .lock()
            .unwrap()
            .insert(model.to_string(), derived);
        derived
    }

    /// `POST /api/show` → the `"<arch>.context_length"` entry of
    /// `model_info`. Any failure yields `None`.
    async fn model_context_length(&self, model: &str) -> Option<u32> {
        let response = self
            .http
            .post(self.url("/api/show"))
            .timeout(PROBE_TIMEOUT)
            .json(&serde_json::json!({ "model": model }))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let info: ShowResponse = response.json().await.ok()?;
        context_length_from_model_info(info.model_info.as_ref()?)
    }

    /// Start a streaming chat completion (`POST /api/chat`, NDJSON).
    /// Yields [`ChatChunk`]s until one with `done == true`; the caller
    /// accumulates `message.content` deltas and collects `tool_calls`.
    pub async fn chat_stream(&self, mut request: ChatRequest) -> Result<ChatStream> {
        let model = request.model.clone();
        // Ollama defaults num_ctx to 4096 and silently truncates the prompt
        // server-side, so always send an explicit value.
        let options = request.options.get_or_insert_with(ChatOptions::default);
        if options.num_ctx.is_none() {
            options.num_ctx = Some(self.derived_num_ctx(&model).await);
        }
        // Translate path-based image attachments into Ollama's native
        // `images: ["base64…"]` arrays on each message. Load failures become
        // text notes so local vision models still get a usable prompt.
        let body = ollama_request_body(&request);
        let response = self
            .http
            .post(self.url("/api/chat"))
            .json(&body)
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if !response.status().is_success() {
            return Err(self.status_error(response, Some(&model)).await);
        }
        let bytes = response
            .bytes_stream()
            .map(|item| match item {
                Ok(chunk) => Ok(chunk.to_vec()),
                Err(e) => Err(anyhow!(e).context(ProviderError::transport(
                    "Ollama response stream was interrupted",
                ))),
            })
            .boxed();
        Ok(decode_ndjson(bytes))
    }
}

#[async_trait]
impl LlmProvider for OllamaClient {
    async fn health(&self) -> Result<()> {
        OllamaClient::health(self).await
    }

    async fn supports_native_tools(&self, model: &str) -> Result<bool> {
        OllamaClient::supports_native_tools(self, model).await
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        OllamaClient::list_models(self).await
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
        OllamaClient::chat_stream(self, request).await
    }

    /// The server truncates the prompt at `num_ctx`, so the effective window
    /// is the value chat requests will carry (probe-derived and capped),
    /// not the model's full trained context length.
    async fn context_window(&self, model: &str) -> Option<u32> {
        Some(self.derived_num_ctx(model).await)
    }

    fn label(&self) -> String {
        self.host().to_string()
    }
}

/// Build the Ollama `/api/chat` JSON body. Path-based image attachments on
/// user messages become Ollama's native `images: ["base64…"]` arrays; load
/// failures append a note to the message text.
fn ollama_request_body(request: &ChatRequest) -> serde_json::Value {
    use serde_json::{Value, json};

    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(|message| {
            let mut content = message.content.clone();
            let mut images_b64: Vec<String> = Vec::new();
            for image in &message.images {
                match super::load_image_base64(image) {
                    Ok((_mime, data)) => images_b64.push(data),
                    Err(err) => {
                        let label = image
                            .path
                            .as_deref()
                            .and_then(|p| std::path::Path::new(p).file_name())
                            .and_then(|n| n.to_str())
                            .unwrap_or("image");
                        let note = format!("[image {label} could not be attached: {err}]");
                        if content.is_empty() {
                            content = note;
                        } else {
                            content = format!("{content}\n{note}");
                        }
                    }
                }
            }
            let mut value = json!({
                "role": message.role,
                "content": content,
            });
            if !message.tool_calls.is_empty() {
                value["tool_calls"] = serde_json::to_value(&message.tool_calls)
                    .unwrap_or_else(|_| Value::Array(Vec::new()));
            }
            if let Some(name) = &message.tool_name {
                value["tool_name"] = json!(name);
            }
            if !images_b64.is_empty() {
                value["images"] = json!(images_b64);
            }
            value
        })
        .collect();

    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "stream": request.stream,
    });
    if !request.tools.is_empty() {
        body["tools"] = serde_json::to_value(&request.tools).unwrap_or(Value::Array(Vec::new()));
    }
    if let Some(options) = &request.options {
        body["options"] = serde_json::to_value(options).unwrap_or(Value::Null);
    }
    body
}

/// `GET /api/tags` response body (subset we care about).
#[derive(Debug, Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<ModelTag>,
}

#[derive(Debug, Deserialize)]
struct ModelTag {
    name: String,
}

/// `POST /api/show` response body (subset we care about). Older Ollama
/// versions omit `capabilities` entirely; we treat that as "no native
/// tools" and use the JSON protocol fallback.
#[derive(Debug, Deserialize)]
struct ShowResponse {
    #[serde(default)]
    capabilities: Option<Vec<String>>,
    #[serde(default)]
    model_info: Option<serde_json::Value>,
}

/// Find the `"<architecture>.context_length"` entry in `/api/show`'s
/// `model_info` map (the key is prefixed by the model architecture,
/// e.g. `"llama.context_length"` or `"qwen3.context_length"`).
fn context_length_from_model_info(model_info: &serde_json::Value) -> Option<u32> {
    model_info.as_object()?.iter().find_map(|(key, value)| {
        if !key.ends_with(".context_length") {
            return None;
        }
        value.as_u64().and_then(|n| u32::try_from(n).ok())
    })
}

/// In-band error line Ollama can emit mid-stream: `{"error": "..."}`.
#[derive(Debug, Deserialize)]
struct ErrorLine {
    error: String,
}

/// Parse one NDJSON line into a [`ChatChunk`], surfacing Ollama's in-band
/// `{"error": ...}` lines as errors.
fn parse_chunk_line(line: &str) -> Result<ChatChunk> {
    match serde_json::from_str::<ChatChunk>(line) {
        Ok(chunk) => Ok(chunk),
        Err(parse_err) => {
            if let Ok(err) = serde_json::from_str::<ErrorLine>(line) {
                bail!("Ollama error: {}", err.error);
            }
            let preview: String = line.chars().take(200).collect();
            Err(anyhow!(parse_err).context(format!("unparseable line from Ollama: {preview}")))
        }
    }
}

/// Decoder state for [`decode_ndjson`].
struct NdjsonState<S> {
    bytes: S,
    buf: Vec<u8>,
    finished: bool,
}

/// Turn a raw byte stream into a [`ChatStream`] by splitting on newlines and
/// parsing each line as a [`ChatChunk`]. The stream ends after the chunk
/// with `done == true` (or on transport EOF / error).
fn decode_ndjson<S>(bytes: S) -> ChatStream
where
    S: Stream<Item = Result<Vec<u8>>> + Send + Unpin + 'static,
{
    let state = NdjsonState {
        bytes,
        buf: Vec::new(),
        finished: false,
    };
    stream::try_unfold(state, |mut state| async move {
        if state.finished {
            return Ok(None);
        }
        loop {
            // Drain any complete lines already buffered.
            while let Some(pos) = state.buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = state.buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let chunk = parse_chunk_line(line)?;
                if chunk.done {
                    state.finished = true;
                }
                return Ok(Some((chunk, state)));
            }
            match state.bytes.next().await {
                Some(Ok(data)) => state.buf.extend_from_slice(&data),
                Some(Err(e)) => return Err(e),
                None => {
                    // EOF: flush a trailing line without a newline (also the
                    // whole body when the caller requested `stream: false`).
                    state.finished = true;
                    let rest = String::from_utf8_lossy(&state.buf);
                    let rest = rest.trim();
                    if rest.is_empty() {
                        return Ok(None);
                    }
                    let chunk = parse_chunk_line(rest)?;
                    return Ok(Some((chunk, state)));
                }
            }
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Role;

    #[test]
    fn host_trailing_slash_is_trimmed() {
        let client = OllamaClient::new("http://127.0.0.1:11434///");
        assert_eq!(client.host(), "http://127.0.0.1:11434");
        assert_eq!(client.url("/api/tags"), "http://127.0.0.1:11434/api/tags");
    }

    #[test]
    fn parses_content_delta_chunk() {
        let chunk = parse_chunk_line(
            r#"{"model":"m","message":{"role":"assistant","content":"hel"},"done":false}"#,
        )
        .expect("valid chunk");
        assert!(!chunk.done);
        let message = chunk.message.expect("message present");
        assert_eq!(message.role, Role::Assistant);
        assert_eq!(message.content, "hel");
    }

    #[test]
    fn parses_tool_call_chunk() {
        let chunk = parse_chunk_line(
            r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"read_file","arguments":{"path":"src/main.rs"}}}]},"done":false}"#,
        )
        .expect("valid chunk");
        let message = chunk.message.expect("message present");
        assert_eq!(message.tool_calls.len(), 1);
        assert_eq!(message.tool_calls[0].function.name, "read_file");
        assert_eq!(
            message.tool_calls[0].function.arguments["path"],
            "src/main.rs"
        );
    }

    #[test]
    fn surfaces_in_band_error_line() {
        let err = parse_chunk_line(r#"{"error":"model 'x' not found"}"#)
            .expect_err("error line must fail");
        assert!(err.to_string().contains("model 'x' not found"));
    }

    #[test]
    fn transient_classification() {
        let status = |code: u16| reqwest::StatusCode::from_u16(code).expect("valid status");
        assert!(
            OllamaError::Api {
                status: status(503),
                body: String::new(),
            }
            .is_transient()
        );
        assert!(
            OllamaError::Api {
                status: status(429),
                body: String::new(),
            }
            .is_transient()
        );
        assert!(
            !OllamaError::Api {
                status: status(400),
                body: String::new(),
            }
            .is_transient()
        );
        assert!(!OllamaError::ModelMissing("m".to_string()).is_transient());
    }

    #[test]
    fn typed_errors_downcast_to_both_error_types() {
        let status = |code: u16| reqwest::StatusCode::from_u16(code).expect("valid status");
        let err = typed(OllamaError::Api {
            status: status(503),
            body: "busy".to_string(),
        });
        let ollama = err.downcast_ref::<OllamaError>().expect("legacy type");
        let provider = err.downcast_ref::<ProviderError>().expect("shared type");
        assert_eq!(provider.status, Some(503));
        assert_eq!(ollama.is_transient(), provider.is_transient());
        assert!(provider.message.contains("busy"), "body surfaces");

        // ModelMissing carries its originating 404 so both classifications
        // agree that it is not retryable.
        let err = typed(OllamaError::ModelMissing("m".to_string()));
        let provider = err.downcast_ref::<ProviderError>().expect("shared type");
        assert_eq!(provider.status, Some(404));
        assert!(!provider.is_transient());
    }

    #[test]
    fn context_length_is_read_from_model_info() {
        let info = serde_json::json!({
            "general.architecture": "qwen3",
            "qwen3.context_length": 40_960,
            "qwen3.embedding_length": 1024,
        });
        assert_eq!(context_length_from_model_info(&info), Some(40_960));
        assert_eq!(
            context_length_from_model_info(&serde_json::json!({"general.architecture": "x"})),
            None
        );
        assert_eq!(
            context_length_from_model_info(&serde_json::Value::Null),
            None
        );
    }

    #[tokio::test]
    async fn derived_num_ctx_falls_back_and_caches_when_probe_fails() {
        // Port 1 on localhost: connection refused immediately, no server needed.
        let client = OllamaClient::new("http://127.0.0.1:1");
        assert_eq!(client.derived_num_ctx("m").await, DEFAULT_NUM_CTX);
        assert_eq!(
            client.num_ctx_cache.lock().unwrap().get("m"),
            Some(&DEFAULT_NUM_CTX),
            "fallback is cached so the probe is not retried per request"
        );
        assert_eq!(client.context_window("m").await, Some(DEFAULT_NUM_CTX));
    }

    #[test]
    fn derived_num_ctx_is_capped() {
        // The cap keeps a huge trained context from allocating an equally
        // huge KV cache by default.
        let derived = context_length_from_model_info(&serde_json::json!({
            "llama.context_length": 1_048_576u64,
        }))
        .map(|n| n.min(MAX_DERIVED_NUM_CTX))
        .unwrap_or(DEFAULT_NUM_CTX);
        assert_eq!(derived, MAX_DERIVED_NUM_CTX);
    }

    #[tokio::test]
    async fn decodes_split_ndjson_lines() {
        // One line split across two network reads, plus a final done line.
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(br#"{"message":{"role":"assistant","#.to_vec()),
            Ok(br#""content":"hi"},"done":false}"#.to_vec()),
            Ok(b"\n".to_vec()),
            Ok(
                br#"{"message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","eval_count":7}"#
                    .to_vec(),
            ),
        ];
        let mut chunks = decode_ndjson(stream::iter(parts));
        let first = chunks
            .next()
            .await
            .expect("first chunk")
            .expect("first chunk ok");
        assert_eq!(first.message.expect("message").content, "hi");
        assert!(!first.done);
        let last = chunks
            .next()
            .await
            .expect("final chunk")
            .expect("final chunk ok");
        assert!(last.done);
        assert_eq!(last.done_reason.as_deref(), Some("stop"));
        assert_eq!(last.eval_count, Some(7));
        assert!(chunks.next().await.is_none(), "stream ends after done");
    }

    #[tokio::test]
    async fn stops_after_done_chunk_even_with_trailing_data() {
        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            b"{\"done\":true}\n{\"message\":{\"role\":\"assistant\",\"content\":\"x\"},\"done\":false}\n".to_vec(),
        )];
        let mut chunks = decode_ndjson(stream::iter(parts));
        let first = chunks.next().await.expect("chunk").expect("ok");
        assert!(first.done);
        assert!(chunks.next().await.is_none());
    }

    #[tokio::test]
    async fn flushes_trailing_line_without_newline_at_eof() {
        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            br#"{"message":{"role":"assistant","content":"all"},"done":true}"#.to_vec(),
        )];
        let mut chunks = decode_ndjson(stream::iter(parts));
        let only = chunks.next().await.expect("chunk").expect("ok");
        assert!(only.done);
        assert_eq!(only.message.expect("message").content, "all");
        assert!(chunks.next().await.is_none());
    }

    #[tokio::test]
    async fn propagates_transport_errors() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(b"{\"done\":false}\n".to_vec()),
            Err(anyhow!("connection reset")),
        ];
        let mut chunks = decode_ndjson(stream::iter(parts));
        assert!(chunks.next().await.expect("chunk").is_ok());
        let err = chunks.next().await.expect("item").expect_err("error");
        assert!(err.to_string().contains("connection reset"));
    }
}
