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
use crate::server::{ByteProgress, Progress};

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

    /// Make sure `model` is available locally: a no-op when `/api/tags`
    /// already lists it (a bare `name` counts as `name:latest`), otherwise
    /// pull it with [`OllamaClient::pull_model`]. The setup paths call this
    /// so a freshly onboarded tag materializes on first run, mirroring how a
    /// missing GGUF is downloaded for llama.cpp.
    pub async fn ensure_model(&self, model: &str, progress: &dyn Progress) -> Result<()> {
        let installed = self.list_models().await?;
        if model_installed(model, &installed) {
            return Ok(());
        }
        self.pull_model(model, progress).await
    }

    /// Pull `model` through Ollama's native streaming API (`POST /api/pull`,
    /// NDJSON progress lines), rendering layer downloads as byte-counted
    /// bars. Fails on transport errors, non-success statuses, and in-band
    /// `{"error": ...}` lines (e.g. an unknown tag). Interrupted pulls
    /// resume server-side on the next attempt.
    pub async fn pull_model(&self, model: &str, progress: &dyn Progress) -> Result<()> {
        progress.status(&format!(
            "model '{model}' is not pulled yet — pulling it now (one-time)…"
        ));
        let response = self
            .http
            .post(self.url("/api/pull"))
            .json(&serde_json::json!({ "model": model }))
            .send()
            .await
            .map_err(|e| self.transport_error(e))?;
        if !response.status().is_success() {
            return Err(self.status_error(response, Some(model)).await);
        }

        let mut render = PullRender::new(progress, model);
        let mut done = false;
        let mut buf: Vec<u8> = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                anyhow!(e).context(format!(
                    "the pull of '{model}' was interrupted — re-run to resume"
                ))
            })?;
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                done |= apply_pull_line(&mut render, model, &String::from_utf8_lossy(&line))?;
            }
        }
        // Flush a trailing line without a newline at EOF.
        done |= apply_pull_line(&mut render, model, &String::from_utf8_lossy(&buf))?;
        render.close();

        // No explicit success line: trust the server's model list over the
        // transcript before declaring failure.
        if !done && !model_installed(model, &self.list_models().await.unwrap_or_default()) {
            bail!("the pull of '{model}' ended without success — re-run to resume");
        }
        progress.status(&format!("pulled {model}"));
        Ok(())
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
        let body = build_request_body(&request)?;
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

/// Translate a native [`ChatRequest`] into Ollama's `/api/chat` body.
///
/// Identical to the request's own serde shape but for `images`: Ollama's native
/// endpoint takes a sibling array of bare base64 strings on the message (it
/// sniffs the media type itself), whereas Wizard carries typed
/// [`Image`](crate::llm::Image)s. Images on an *assistant* message — ones the
/// model generated — are named in its text instead: an assistant turn is not
/// image input, and a vision model handed its own output back as input would
/// only be confused by it.
fn build_request_body(request: &ChatRequest) -> Result<serde_json::Value> {
    use crate::llm::Role;
    use serde_json::Value;

    let mut body = serde_json::to_value(request).context("serializing chat request")?;
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return Ok(body);
    };
    for (wire, message) in messages.iter_mut().zip(&request.messages) {
        if message.images.is_empty() {
            continue;
        }
        if message.role == Role::Assistant {
            wire["content"] = Value::String(crate::llm::assistant_content(message));
            if let Some(object) = wire.as_object_mut() {
                object.remove("images");
            }
            continue;
        }
        wire["images"] = Value::Array(
            message
                .images
                .iter()
                .map(|image| Value::String(image.b64.clone()))
                .collect(),
        );
    }
    Ok(body)
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

/// Whether `tag` is present in `installed` (the `/api/tags` names). A tag
/// without an explicit version means `:latest` — matching how Ollama
/// resolves bare names — so `"llama3"` matches an installed
/// `"llama3:latest"` and vice versa, while `"qwen3.5"` does *not* match
/// `"qwen3.5:9b"`.
pub fn model_installed(tag: &str, installed: &[String]) -> bool {
    fn canonical(tag: &str) -> std::borrow::Cow<'_, str> {
        // The version separator is the colon after the last `/`, so a
        // registry port (`host:port/name`) is not mistaken for a version.
        let name = tag.rsplit('/').next().unwrap_or(tag);
        if name.contains(':') {
            std::borrow::Cow::Borrowed(tag)
        } else {
            std::borrow::Cow::Owned(format!("{tag}:latest"))
        }
    }
    let want = canonical(tag);
    installed.iter().any(|have| canonical(have) == want)
}

/// One NDJSON progress line from `POST /api/pull`. Layer downloads carry
/// `digest`/`total`/`completed`; milestones ("pulling manifest", "verifying
/// sha256 digest", "success") carry only `status`; failures carry `error`.
#[derive(Debug, Default, PartialEq, Deserialize)]
struct PullLine {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    completed: Option<u64>,
    #[serde(default)]
    error: Option<String>,
}

/// Parse one `/api/pull` NDJSON line.
fn parse_pull_line(line: &str) -> Result<PullLine> {
    serde_json::from_str(line).with_context(|| {
        let preview: String = line.chars().take(200).collect();
        format!("unparseable line from Ollama pull: {preview}")
    })
}

/// Feed one raw pull line into `render`. Returns whether it was the final
/// `"success"` line; blank lines are skipped; in-band errors bail.
fn apply_pull_line(render: &mut PullRender, model: &str, raw: &str) -> Result<bool> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(false);
    }
    let line = parse_pull_line(raw)?;
    if let Some(error) = line.error {
        bail!("Ollama could not pull '{model}': {error}");
    }
    let success = line.status.as_deref() == Some("success");
    if !success {
        render.apply(&line);
    }
    Ok(success)
}

/// Renders pull progress onto a [`Progress`] sink: one byte-counted bar per
/// layer digest (Ollama reports each blob separately), plain status lines
/// for the milestones in between.
struct PullRender<'a> {
    sink: &'a dyn Progress,
    model: &'a str,
    bar: Option<PullBar>,
    last_status: String,
}

/// The open byte bar for one layer digest.
struct PullBar {
    digest: String,
    guard: Box<dyn ByteProgress>,
    completed: u64,
}

impl<'a> PullRender<'a> {
    fn new(sink: &'a dyn Progress, model: &'a str) -> Self {
        Self {
            sink,
            model,
            bar: None,
            last_status: String::new(),
        }
    }

    /// Advance the display for one progress line.
    fn apply(&mut self, line: &PullLine) {
        match (line.digest.as_deref(), line.total) {
            (Some(digest), Some(total)) => {
                if self.bar.as_ref().is_none_or(|bar| bar.digest != digest) {
                    self.close();
                    let label = format!("pulling {} ({})", self.model, short_digest(digest));
                    self.bar = Some(PullBar {
                        digest: digest.to_string(),
                        guard: self.sink.bytes(&label, Some(total)),
                        completed: 0,
                    });
                }
                let bar = self.bar.as_mut().expect("bar was just ensured");
                let completed = line.completed.unwrap_or(0).min(total);
                if completed > bar.completed {
                    bar.guard.inc(completed - bar.completed);
                    bar.completed = completed;
                }
            }
            // A layer line before its size is known — wait for totals.
            (Some(_), None) => {}
            (None, _) => {
                if let Some(status) = line.status.as_deref()
                    && status != self.last_status
                {
                    self.close();
                    self.last_status = status.to_string();
                    self.sink.status(status);
                }
            }
        }
    }

    /// Finish the open byte bar, if any.
    fn close(&mut self) {
        if let Some(bar) = self.bar.take() {
            bar.guard.finish("");
        }
    }
}

/// Compact display form of a layer digest: `sha256:ab12cd34…`.
fn short_digest(digest: &str) -> String {
    match digest.split_once(':') {
        Some((algo, hex)) if hex.len() > 8 => format!("{algo}:{}…", &hex[..8]),
        _ => digest.to_string(),
    }
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

    fn request(messages: Vec<crate::llm::ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "qwen3-vl".to_string(),
            messages,
            tools: Vec::new(),
            stream: true,
            options: None,
        }
    }

    #[test]
    fn user_images_flatten_to_the_native_base64_array() {
        let body = build_request_body(&request(vec![
            crate::llm::ChatMessage::user("plain"),
            crate::llm::ChatMessage::user_with_images(
                "what is this?",
                vec![
                    crate::llm::Image::new("QUJD", "image/png"),
                    crate::llm::Image::new("REVG", "image/webp"),
                ],
            ),
        ]))
        .expect("body");

        assert!(
            body["messages"][0].get("images").is_none(),
            "text-only messages are untouched"
        );
        // Ollama's native shape: bare base64 strings, no media type (it sniffs).
        assert_eq!(body["messages"][1]["content"], "what is this?");
        assert_eq!(body["messages"][1]["images"][0], "QUJD");
        assert_eq!(body["messages"][1]["images"][1], "REVG");
    }

    #[test]
    fn the_on_disk_path_never_reaches_the_wire() {
        // `Image::path` is bookkeeping for replaying a transcript, not content:
        // no provider sees it, including the one whose body is serde-derived.
        let image = crate::llm::Image::new("QUJD", "image/png")
            .at_path(std::path::PathBuf::from("/home/u/.wizard/images/s/abc.png"));
        let body = build_request_body(&request(vec![crate::llm::ChatMessage::user_with_images(
            "look",
            vec![image],
        )]))
        .expect("body");
        assert!(
            !body.to_string().contains(".wizard/images"),
            "no local path on the wire: {body}"
        );
    }

    #[test]
    fn assistant_images_are_named_in_the_text_not_sent_back_as_input() {
        let mut assistant = crate::llm::ChatMessage::assistant("here it is");
        assistant
            .images
            .push(crate::llm::Image::new("QUJD", "image/png"));
        let body = build_request_body(&request(vec![assistant])).expect("body");
        let content = body["messages"][0]["content"].as_str().expect("content");
        assert!(content.contains("here it is"));
        assert!(
            content.contains("generated 1 image(s) (image/png)"),
            "{content}"
        );
        assert!(body["messages"][0].get("images").is_none());
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
    fn model_installed_treats_a_bare_name_as_latest() {
        let installed = vec![
            "llama3:latest".to_string(),
            "qwen3.5:9b".to_string(),
            "myuser/coder".to_string(),
        ];
        assert!(model_installed("llama3", &installed), "bare = :latest");
        assert!(model_installed("llama3:latest", &installed));
        assert!(model_installed("qwen3.5:9b", &installed), "exact tag");
        assert!(
            model_installed("myuser/coder:latest", &installed),
            "installed side normalizes too"
        );
        assert!(
            !model_installed("qwen3.5", &installed),
            "a bare name never matches a versioned tag"
        );
        assert!(!model_installed("qwen3.6:27b", &installed));
        assert!(!model_installed("llama3", &[]));
    }

    #[test]
    fn pull_lines_parse_layers_milestones_and_errors() {
        let layer = parse_pull_line(
            r#"{"status":"pulling ab12","digest":"sha256:ab12","total":100,"completed":25}"#,
        )
        .expect("layer line");
        assert_eq!(layer.digest.as_deref(), Some("sha256:ab12"));
        assert_eq!(layer.total, Some(100));
        assert_eq!(layer.completed, Some(25));

        let milestone = parse_pull_line(r#"{"status":"verifying sha256 digest"}"#).expect("status");
        assert_eq!(milestone.status.as_deref(), Some("verifying sha256 digest"));
        assert_eq!(milestone.digest, None);

        let error = parse_pull_line(r#"{"error":"pull model manifest: file does not exist"}"#)
            .expect("error line");
        assert_eq!(
            error.error.as_deref(),
            Some("pull model manifest: file does not exist")
        );

        assert!(parse_pull_line("not json").is_err());
    }

    /// [`Progress`] sink that records every call as a plain string.
    #[derive(Default)]
    struct Recording(Arc<Mutex<Vec<String>>>);

    impl Progress for Recording {
        fn status(&self, line: &str) {
            self.0.lock().unwrap().push(format!("status:{line}"));
        }
        fn bytes(&self, label: &str, total: Option<u64>) -> Box<dyn crate::server::ByteProgress> {
            self.0
                .lock()
                .unwrap()
                .push(format!("bar:{label}:{}", total.unwrap_or(0)));
            Box::new(RecordingBar(Arc::clone(&self.0)))
        }
    }

    struct RecordingBar(Arc<Mutex<Vec<String>>>);

    impl ByteProgress for RecordingBar {
        fn inc(&self, n: u64) {
            self.0.lock().unwrap().push(format!("inc:{n}"));
        }
        fn finish(self: Box<Self>, _msg: &str) {
            self.0.lock().unwrap().push("finish".to_string());
        }
    }

    #[test]
    fn pull_render_opens_one_bar_per_layer_and_ticks_deltas() {
        let sink = Recording::default();
        let mut render = PullRender::new(&sink, "my-model");
        let lines = [
            r#"{"status":"pulling manifest"}"#,
            // First layer: two progress lines — one bar, delta-ticked.
            r#"{"status":"pulling ab","digest":"sha256:abcdef012345","total":100,"completed":40}"#,
            r#"{"status":"pulling ab","digest":"sha256:abcdef012345","total":100,"completed":100}"#,
            // Second layer: a new bar.
            r#"{"status":"pulling cd","digest":"sha256:cd","total":10,"completed":10}"#,
            r#"{"status":"verifying sha256 digest"}"#,
            r#"{"status":"writing manifest"}"#,
        ];
        let mut done = false;
        for line in lines {
            done |= apply_pull_line(&mut render, "my-model", line).expect("line applies");
        }
        assert!(!done, "no success line yet");
        done = apply_pull_line(&mut render, "my-model", r#"{"status":"success"}"#).expect("ok");
        assert!(done, "success line reported");
        render.close();

        let events = sink.0.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                "status:pulling manifest",
                "bar:pulling my-model (sha256:abcdef01…):100",
                "inc:40",
                "inc:60",
                "finish", // first layer bar closed by the second layer
                "bar:pulling my-model (sha256:cd):10",
                "inc:10",
                "finish", // second layer bar closed by the milestone
                "status:verifying sha256 digest",
                "status:writing manifest",
            ]
        );
    }

    #[test]
    fn pull_render_skips_blank_lines_and_regressions() {
        let sink = Recording::default();
        let mut render = PullRender::new(&sink, "m");
        assert!(!apply_pull_line(&mut render, "m", "  \n").expect("blank ok"));
        // completed going backwards (Ollama re-verifying) never underflows.
        for line in [
            r#"{"digest":"sha256:ab","total":100,"completed":50}"#,
            r#"{"digest":"sha256:ab","total":100,"completed":30}"#,
        ] {
            apply_pull_line(&mut render, "m", line).expect("applies");
        }
        render.close();
        let events = sink.0.lock().unwrap().clone();
        assert_eq!(
            events,
            vec!["bar:pulling m (sha256:ab):100", "inc:50", "finish"]
        );
    }

    #[test]
    fn in_band_pull_errors_bail_with_the_model_name() {
        let sink = Recording::default();
        let mut render = PullRender::new(&sink, "bogus:tag");
        let err = apply_pull_line(
            &mut render,
            "bogus:tag",
            r#"{"error":"pull model manifest: file does not exist"}"#,
        )
        .expect_err("error line must fail");
        assert!(err.to_string().contains("bogus:tag"));
        assert!(err.to_string().contains("file does not exist"));
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
