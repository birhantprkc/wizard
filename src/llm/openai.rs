//! Streaming HTTP client for the OpenAI **Chat Completions** API
//! (`POST {base_url}/chat/completions`).
//!
//! Compatible with OpenAI, OpenRouter, Groq, together.ai, vLLM, LM Studio, and
//! any other endpoint that speaks the same shape. Thin `reqwest` wrapper with
//! manual SSE parsing — no extra dependencies. Wizard's native [`ChatRequest`]
//! is translated to the OpenAI request shape on the way out, and the SSE
//! response is decoded back into Wizard's [`ChatChunk`] stream.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};

use super::provider::LlmProvider;
use super::{
    ChatChunk, ChatMessage, ChatRequest, ChatStream, FunctionCall, ProviderError, Role, ToolCall,
};

/// Supplies the `Authorization: Bearer` token for each request. The plain
/// API-key case is [`StaticToken`]; OAuth-backed providers (xAI sign-in)
/// plug in a source that refreshes the access token between calls.
#[async_trait]
pub trait TokenSource: Send + Sync + std::fmt::Debug {
    /// The current bearer token, or `None` when the endpoint needs no auth.
    /// May refresh an expiring token before returning it.
    async fn bearer(&self) -> Result<Option<String>>;

    /// Called once after an HTTP 401 from the API. Returns `true` when a
    /// fresh token was obtained and the request should be retried.
    async fn refresh_after_unauthorized(&self) -> Result<bool> {
        Ok(false)
    }

    /// What the user should do about a persistent HTTP 401.
    fn unauthorized_hint(&self) -> &str {
        "check the configured API key env var"
    }

    /// Extra context appended to HTTP 403 errors (e.g. plan-gating hints).
    fn forbidden_hint(&self) -> Option<&str> {
        None
    }
}

/// Fixed API key. An empty key means no `Authorization` header is sent
/// (keyless local servers like vLLM or LM Studio).
#[derive(Debug)]
pub struct StaticToken(Option<String>);

impl StaticToken {
    pub fn new(api_key: impl Into<String>) -> Self {
        let key = api_key.into();
        Self((!key.is_empty()).then_some(key))
    }
}

#[async_trait]
impl TokenSource for StaticToken {
    async fn bearer(&self) -> Result<Option<String>> {
        Ok(self.0.clone())
    }
}

/// Client bound to one OpenAI-compatible endpoint.
#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    http: reqwest::Client,
    /// Base URL including the API version segment, e.g.
    /// `https://api.openai.com/v1`. Trailing slashes are trimmed.
    base_url: String,
    /// Default model tag (used only for [`LlmProvider::label`]; requests carry
    /// their own model).
    model: String,
    /// Bearer token supplier (static key or refreshing OAuth source).
    auth: Arc<dyn TokenSource>,
    /// Vendor prefix for [`LlmProvider::label`] (`openai`, `xai`, ...).
    vendor: &'static str,
}

impl OpenAiProvider {
    /// Build a client for `base_url` (which must already include `/v1`).
    /// `api_key` may be empty for keyless local servers (vLLM, LM Studio).
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self::with_token_source(
            base_url,
            model,
            Arc::new(StaticToken::new(api_key)),
            "openai",
        )
    }

    /// Build a client whose bearer token comes from `auth` on every request.
    /// `vendor` is the label prefix shown in the UI (e.g. `xai`).
    pub fn with_token_source(
        base_url: impl Into<String>,
        model: impl Into<String>,
        auth: Arc<dyn TokenSource>,
        vendor: &'static str,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let http = crate::llm::cloud_http_builder().build().unwrap_or_default();
        Self {
            http,
            base_url,
            model: model.into(),
            auth,
            vendor,
        }
    }

    /// Rebuild the inner HTTP client with `headers` sent on every request
    /// (e.g. OpenRouter's attribution headers). Invalid header names or
    /// values are skipped.
    pub fn with_headers(mut self, headers: &[(&str, &str)]) -> Self {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let mut map = HeaderMap::new();
        for &(name, value) in headers {
            if let (Ok(name), Ok(value)) =
                (HeaderName::try_from(name), HeaderValue::try_from(value))
            {
                map.insert(name, value);
            }
        }
        self.http = crate::llm::cloud_http_builder()
            .default_headers(map)
            .build()
            .unwrap_or_default();
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Send a request with the current bearer token attached. On a 401 the
    /// token source gets one chance to refresh, after which the request is
    /// rebuilt (via `build`) and retried exactly once.
    async fn send_authed<F>(&self, build: F) -> Result<reqwest::Response>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let mut retried = false;
        loop {
            let mut request = build();
            if let Some(token) = self.auth.bearer().await? {
                request = request.bearer_auth(token);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(source) => {
                    let message = format!("HTTP request to {} failed: {source}", self.base_url);
                    // Root reqwest error kept on the chain (llama.cpp reframes
                    // connect failures); ProviderError carries the retry class.
                    return Err(
                        anyhow::Error::new(source).context(ProviderError::transport(message))
                    );
                }
            };
            if response.status() == reqwest::StatusCode::UNAUTHORIZED
                && !retried
                && self.auth.refresh_after_unauthorized().await?
            {
                retried = true;
                continue;
            }
            return Ok(response);
        }
    }

    /// Error for a non-success HTTP status, with the token source's hint
    /// appended on 403 (e.g. OAuth plan gating).
    fn http_failure(&self, status: reqwest::StatusCode, body: String) -> anyhow::Error {
        let hint = if status == reqwest::StatusCode::FORBIDDEN {
            self.auth
                .forbidden_hint()
                .map(|hint| format!(" ({hint})"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        anyhow::Error::new(ProviderError::http(
            status.as_u16(),
            format!("{} returned HTTP {status}: {body}{hint}", self.base_url),
        ))
    }

    /// Translate a native [`ChatRequest`] into the OpenAI Chat Completions
    /// request body. Always sets `stream: true`.
    fn build_request_body(&self, request: &ChatRequest) -> Value {
        let messages = build_messages(&request.messages);
        let mut body = json!({
            "model": request.model,
            "messages": messages,
            "stream": true,
            // Without this OpenAI omits `usage` from the SSE stream and
            // token-aware compaction never engages. Compatible servers
            // (llama.cpp, vLLM, OpenRouter, Groq, Ollama's /v1 shim) accept
            // or ignore it.
            "stream_options": { "include_usage": true },
        });
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|spec| {
                json!({
                    "type": "function",
                    "function": {
                        "name": spec.function.name,
                        "description": spec.function.description,
                        "parameters": spec.function.parameters,
                    }
                })
            })
            .collect();
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if let Some(options) = &request.options
            && let Some(temperature) = options.temperature
            && !rejects_temperature(&request.model)
        {
            body["temperature"] = json!(temperature);
        }
        if let Some(options) = &request.options
            && let Some(effort) = &options.reasoning_effort
            && supports_reasoning_effort(&request.model)
        {
            body["reasoning_effort"] = json!(effort);
        }
        body
    }
}

/// Models that accept a `reasoning_effort` request field: xAI Grok 4.x and
/// OpenAI's reasoning families (o-series, gpt-5). Anything else 400s on it, so
/// it is sent only for these. Mirrors the families in [`context_window`].
fn supports_reasoning_effort(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("grok-4")
        || model.starts_with("gpt-5")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
}

/// OpenAI reasoning models (o-series, gpt-5 family) reject any non-default
/// `temperature` with HTTP 400, so it is omitted for them. Mirrors the model
/// families in [`context_window`].
fn rejects_temperature(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("gpt-5")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
}

/// Translate native messages into the OpenAI `messages` array. Tool calls are
/// assigned synthetic ids (`call_N`) as they appear on assistant messages;
/// `tool`-role results are correlated back to those ids by tool name (the
/// earliest unmatched call of the same name), since Wizard's wire format does
/// not carry call ids.
///
/// User messages with [`ChatMessage::images`] become multimodal content arrays
/// (`text` + `image_url` data-URLs). Paths that fail to load fall back to a
/// text note so the turn still proceeds.
fn build_messages(messages: &[ChatMessage]) -> Vec<Value> {
    use std::collections::VecDeque;

    let mut pending: BTreeMap<String, VecDeque<String>> = BTreeMap::new();
    let mut seq: u64 = 0;
    let mut out = Vec::with_capacity(messages.len());

    for message in messages {
        match message.role {
            Role::System => out.push(json!({ "role": "system", "content": message.content })),
            Role::User => out.push(openai_user_message(message)),
            Role::Assistant => {
                let mut value = json!({ "role": "assistant" });
                // OpenAI requires `content: null` (not "") when only tool calls
                // are present.
                value["content"] = if message.content.is_empty() && !message.tool_calls.is_empty() {
                    Value::Null
                } else {
                    json!(message.content)
                };
                if !message.tool_calls.is_empty() {
                    let calls: Vec<Value> = message
                        .tool_calls
                        .iter()
                        .map(|call| {
                            seq += 1;
                            let id = format!("call_{seq}");
                            pending
                                .entry(call.function.name.clone())
                                .or_default()
                                .push_back(id.clone());
                            let arguments = match &call.function.arguments {
                                Value::String(raw) => raw.clone(),
                                other => other.to_string(),
                            };
                            json!({
                                "id": id,
                                "type": "function",
                                "function": { "name": call.function.name, "arguments": arguments },
                            })
                        })
                        .collect();
                    value["tool_calls"] = Value::Array(calls);
                }
                out.push(value);
            }
            Role::Tool => {
                let name = message.tool_name.clone().unwrap_or_default();
                let id = pending
                    .get_mut(&name)
                    .and_then(|queue| queue.pop_front())
                    .unwrap_or_else(|| format!("call_{name}"));
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": message.content,
                }));
            }
        }
    }
    out
}

/// OpenAI user content: plain string when there are no images, otherwise a
/// multimodal content array.
fn openai_user_message(message: &ChatMessage) -> Value {
    if message.images.is_empty() {
        return json!({ "role": "user", "content": message.content });
    }
    let mut content: Vec<Value> = Vec::new();
    let mut text = message.content.clone();
    for image in &message.images {
        match super::load_image_base64(image) {
            Ok((mime, data)) => {
                content.push(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{mime};base64,{data}")
                    }
                }));
            }
            Err(err) => {
                let label = image
                    .path
                    .as_deref()
                    .and_then(|p| std::path::Path::new(p).file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("image");
                let note = format!("[image {label} could not be attached: {err}]");
                if text.is_empty() {
                    text = note;
                } else {
                    text = format!("{text}\n{note}");
                }
            }
        }
    }
    // Text block first (OpenAI convention); include even if empty when images
    // loaded so the model still gets an explicit text part.
    let mut parts = vec![json!({ "type": "text", "text": text })];
    parts.append(&mut content);
    json!({ "role": "user", "content": parts })
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn health(&self) -> Result<()> {
        let response = self
            .send_authed(|| self.http.get(self.url("/models")))
            .await
            .with_context(|| format!("cannot reach {}", self.base_url))?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(anyhow::Error::new(ProviderError::http(
                401,
                format!(
                    "{} rejected the credentials (HTTP 401): {}",
                    self.base_url,
                    self.auth.unauthorized_hint()
                ),
            )));
        }
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(self.http_failure(status, body));
        }
        Ok(())
    }

    async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
        // OpenAI-compatible endpoints support structured tool calling; the
        // agent loop's JSON fallback is not needed.
        Ok(true)
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let response = self
            .send_authed(|| self.http.get(self.url("/models")))
            .await
            .with_context(|| format!("listing models from {}", self.base_url))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(self.http_failure(status, body));
        }
        let models: ModelsResponse = response
            .json()
            .await
            .context("failed to parse /models response")?;
        Ok(models.data.into_iter().map(|m| m.id).collect())
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
        let body = self.build_request_body(&request);
        let response = self
            .send_authed(|| self.http.post(self.url("/chat/completions")).json(&body))
            .await
            .with_context(|| format!("chat request to {} failed", self.base_url))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(self.http_failure(status, body));
        }
        let bytes = response
            .bytes_stream()
            .map(|item| match item {
                Ok(chunk) => Ok(chunk.to_vec()),
                Err(e) => Err(anyhow!(e).context(ProviderError::transport(
                    "OpenAI response stream was interrupted",
                ))),
            })
            .boxed();
        Ok(decode_sse(bytes))
    }

    async fn context_window(&self, model: &str) -> Option<u32> {
        context_window(model)
    }

    fn label(&self) -> String {
        format!("{}:{}", self.vendor, self.model)
    }
}

/// Context-window table for OpenAI-compatible endpoints (OpenAI and xAI
/// model families; llama.cpp overrides this with a live `/props` probe).
/// Unknown tags report `None` so compaction falls back to the byte
/// threshold.
pub(crate) fn context_window(model: &str) -> Option<u32> {
    let model = model.to_ascii_lowercase();
    // xAI Grok (served through this provider with vendor "xai").
    if model.starts_with("grok-4.5") {
        return Some(500_000);
    }
    if model.starts_with("grok-4") {
        return Some(256_000);
    }
    if model.starts_with("grok") {
        return Some(131_072);
    }
    // OpenAI.
    if model.starts_with("gpt-5") {
        return Some(400_000);
    }
    if model.starts_with("gpt-4.1") {
        return Some(1_047_576);
    }
    if model.starts_with("gpt-4o") || model.starts_with("gpt-4-turbo") {
        return Some(128_000);
    }
    if model.starts_with("o1") || model.starts_with("o3") || model.starts_with("o4") {
        return Some(200_000);
    }
    None
}

/// `GET /models` response (subset).
#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
}

/// One streamed `data: {...}` chunk from Chat Completions (subset).
#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    /// Reasoning ("thinking") fragments streamed before the visible text by
    /// reasoning models (xAI grok-4.3, DeepSeek R1, ...).
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    index: u64,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
}

/// Per-index accumulator for a streamed tool call.
#[derive(Debug, Default)]
struct ToolAccum {
    name: String,
    arguments: String,
}

/// Decoder state for [`decode_sse`].
struct SseState<S> {
    bytes: S,
    buf: Vec<u8>,
    /// Second chunk queued when one delta carries both reasoning and content.
    pending: Option<ChatChunk>,
    tool_calls: BTreeMap<u64, ToolAccum>,
    prompt_eval_count: Option<u64>,
    eval_count: Option<u64>,
    /// Last `finish_reason` seen ("stop", "length", "tool_calls", ...).
    done_reason: Option<String>,
    /// Saw `data: [DONE]` or EOF — drain the buffer, then emit the final chunk.
    saw_done: bool,
    /// The synthesized `done: true` chunk has been emitted.
    emitted_final: bool,
}

/// Build the final `done: true` chunk from accumulated tool-call fragments.
fn build_final<S>(state: &SseState<S>) -> ChatChunk {
    let tool_calls: Vec<ToolCall> = state
        .tool_calls
        .values()
        .filter(|accum| !accum.name.is_empty())
        .map(|accum| {
            let arguments = if accum.arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str::<Value>(&accum.arguments)
                    .unwrap_or_else(|_| Value::String(accum.arguments.clone()))
            };
            ToolCall {
                function: FunctionCall {
                    name: accum.name.clone(),
                    arguments,
                },
            }
        })
        .collect();
    let message = if tool_calls.is_empty() {
        None
    } else {
        let mut message = ChatMessage::assistant("");
        message.tool_calls = tool_calls;
        Some(message)
    };
    ChatChunk {
        message,
        thinking: false,
        done: true,
        done_reason: state.done_reason.clone(),
        eval_count: state.eval_count,
        prompt_eval_count: state.prompt_eval_count,
    }
}

/// A live `done: false` text chunk; `thinking` marks reasoning deltas.
fn text_chunk(content: String, thinking: bool) -> ChatChunk {
    ChatChunk {
        message: Some(ChatMessage::assistant(content)),
        thinking,
        done: false,
        done_reason: None,
        eval_count: None,
        prompt_eval_count: None,
    }
}

/// Decode an OpenAI SSE byte stream into a [`ChatStream`]: text and reasoning
/// deltas are emitted live as `done: false` chunks; tool-call fragments are
/// accumulated per index and emitted in a single synthesized `done: true`
/// chunk at the end.
pub(crate) fn decode_sse<S>(bytes: S) -> ChatStream
where
    S: Stream<Item = Result<Vec<u8>>> + Send + Unpin + 'static,
{
    let state = SseState {
        bytes,
        buf: Vec::new(),
        pending: None,
        tool_calls: BTreeMap::new(),
        prompt_eval_count: None,
        eval_count: None,
        done_reason: None,
        saw_done: false,
        emitted_final: false,
    };
    stream::try_unfold(state, |mut state| async move {
        loop {
            if state.emitted_final {
                return Ok(None);
            }
            if let Some(queued) = state.pending.take() {
                return Ok(Some((queued, state)));
            }
            // Drain complete lines, returning the first content delta we find.
            while let Some(pos) = state.buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = state.buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim();
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload == "[DONE]" {
                    state.saw_done = true;
                    continue;
                }
                let chunk: StreamChunk = match serde_json::from_str(payload) {
                    Ok(chunk) => chunk,
                    // Ignore keep-alives and anything we cannot parse.
                    Err(_) => continue,
                };
                if let Some(usage) = chunk.usage {
                    if let Some(prompt) = usage.prompt_tokens {
                        state.prompt_eval_count = Some(prompt);
                    }
                    if let Some(completion) = usage.completion_tokens {
                        state.eval_count = Some(completion);
                    }
                }
                if let Some(choice) = chunk.choices.into_iter().next() {
                    if let Some(reason) = choice.finish_reason {
                        state.done_reason = Some(reason);
                    }
                    for delta in choice.delta.tool_calls {
                        let accum = state.tool_calls.entry(delta.index).or_default();
                        if let Some(function) = delta.function {
                            if let Some(name) = function.name {
                                accum.name.push_str(&name);
                            }
                            if let Some(arguments) = function.arguments {
                                accum.arguments.push_str(&arguments);
                            }
                        }
                    }
                    let reasoning = choice
                        .delta
                        .reasoning_content
                        .filter(|text| !text.is_empty());
                    let content = choice.delta.content.filter(|text| !text.is_empty());
                    if let Some(reasoning) = reasoning {
                        // Both in one delta: queue the content behind the
                        // reasoning so neither fragment is lost.
                        state.pending = content.map(|content| text_chunk(content, false));
                        return Ok(Some((text_chunk(reasoning, true), state)));
                    }
                    if let Some(content) = content {
                        return Ok(Some((text_chunk(content, false), state)));
                    }
                }
            }
            if state.saw_done {
                state.emitted_final = true;
                let final_chunk = build_final(&state);
                return Ok(Some((final_chunk, state)));
            }
            match state.bytes.next().await {
                Some(Ok(data)) => state.buf.extend_from_slice(&data),
                Some(Err(e)) => return Err(e),
                None => {
                    // EOF: flush a trailing line without a newline, then emit
                    // the final chunk on the next pass.
                    if !state.buf.is_empty() && state.buf.last() != Some(&b'\n') {
                        state.buf.push(b'\n');
                    }
                    state.saw_done = true;
                }
            }
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ChatOptions, ToolSpec};

    fn provider() -> OpenAiProvider {
        OpenAiProvider::new("https://api.openai.com/v1/", "gpt-4o", "sk-test")
    }

    #[test]
    fn base_url_trailing_slash_is_trimmed() {
        let provider = OpenAiProvider::new("http://localhost:1234/v1///", "m", "");
        assert_eq!(
            provider.url("/chat/completions"),
            "http://localhost:1234/v1/chat/completions"
        );
        assert_eq!(provider.label(), "openai:m");
    }

    #[test]
    fn vendor_prefix_shows_in_the_label() {
        let provider = OpenAiProvider::with_token_source(
            "https://api.x.ai/v1",
            "grok-4.3",
            Arc::new(StaticToken::new("k")),
            "xai",
        );
        assert_eq!(provider.label(), "xai:grok-4.3");
    }

    #[test]
    fn with_headers_keeps_url_and_label() {
        let provider = provider().with_headers(&[
            ("HTTP-Referer", "https://example.com"),
            ("X-Title", "Wizard"),
        ]);
        assert_eq!(
            provider.url("/chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(provider.label(), "openai:gpt-4o");
    }

    #[test]
    fn with_headers_skips_invalid_headers() {
        // An invalid name/value must not panic or break the client.
        let provider = provider().with_headers(&[("bad header", "x"), ("X-Ok", "bad\nvalue")]);
        assert_eq!(provider.label(), "openai:gpt-4o");
    }

    #[tokio::test]
    async fn static_token_skips_the_header_when_empty() {
        assert_eq!(
            StaticToken::new("sk-test").bearer().await.expect("ok"),
            Some("sk-test".to_string())
        );
        assert_eq!(StaticToken::new("").bearer().await.expect("ok"), None);
    }

    #[test]
    fn translates_native_request_to_openai_shape() {
        let mut assistant = ChatMessage::assistant("");
        assistant.tool_calls.push(ToolCall {
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: json!({ "path": "src/main.rs" }),
            },
        });
        let request = ChatRequest {
            model: "gpt-4o".to_string(),
            messages: vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("read it"),
                assistant,
                ChatMessage::tool_result("read_file", "fn main() {}"),
            ],
            tools: vec![ToolSpec::function(
                "read_file",
                "Read a file.",
                json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
            )],
            stream: true,
            options: Some(ChatOptions {
                temperature: Some(0.7),
                num_ctx: None,
                reasoning_effort: None,
            }),
        };

        let body = provider().build_request_body(&request);
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["stream"], true);
        assert_eq!(
            body["stream_options"]["include_usage"], true,
            "usage must be requested on SSE streams"
        );
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");

        let assistant = &body["messages"][2];
        assert_eq!(assistant["role"], "assistant");
        assert!(assistant["content"].is_null(), "tool-only content is null");
        let call = &assistant["tool_calls"][0];
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "read_file");
        // arguments are serialized to a JSON *string* for OpenAI.
        let id = call["id"].as_str().expect("call id");
        let args = call["function"]["arguments"]
            .as_str()
            .expect("arguments serialized to a string");
        assert!(args.contains("src/main.rs"));

        let tool_msg = &body["messages"][3];
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(
            tool_msg["tool_call_id"], id,
            "result correlates to the call"
        );
        assert_eq!(tool_msg["content"], "fn main() {}");

        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        let temperature = body["temperature"].as_f64().expect("temperature number");
        assert!((temperature - 0.7).abs() < 1e-6);
    }

    #[test]
    fn reasoning_models_omit_temperature() {
        let options = Some(ChatOptions {
            temperature: Some(0.2),
            num_ctx: None,
            reasoning_effort: None,
        });
        for model in ["gpt-5", "gpt-5-mini", "o1", "o3-mini", "o4-mini", "O3"] {
            let request = ChatRequest {
                model: model.to_string(),
                messages: vec![ChatMessage::user("hi")],
                tools: Vec::new(),
                stream: true,
                options: options.clone(),
            };
            let body = provider().build_request_body(&request);
            assert!(
                body.get("temperature").is_none(),
                "{model} must not receive temperature"
            );
        }
        // Non-reasoning models keep it.
        let request = ChatRequest {
            model: "gpt-4o".to_string(),
            messages: vec![ChatMessage::user("hi")],
            tools: Vec::new(),
            stream: true,
            options,
        };
        let body = provider().build_request_body(&request);
        assert!(body.get("temperature").is_some());
    }

    #[test]
    fn reasoning_effort_is_sent_only_for_supporting_models() {
        let options = Some(ChatOptions {
            temperature: Some(0.7),
            num_ctx: None,
            reasoning_effort: Some("high".to_string()),
        });
        // Forwarded for xAI Grok 4.x and OpenAI reasoning families.
        for model in ["grok-4.5", "grok-4.3", "gpt-5", "o3-mini", "o4-mini"] {
            let request = ChatRequest {
                model: model.to_string(),
                messages: vec![ChatMessage::user("hi")],
                tools: Vec::new(),
                stream: true,
                options: options.clone(),
            };
            let body = provider().build_request_body(&request);
            assert_eq!(
                body["reasoning_effort"], "high",
                "{model} must receive reasoning_effort"
            );
        }
        // Omitted for models that would 400 on it.
        for model in ["gpt-4o", "grok-code-fast-1", "grok-3", "qwen3-8b"] {
            let request = ChatRequest {
                model: model.to_string(),
                messages: vec![ChatMessage::user("hi")],
                tools: Vec::new(),
                stream: true,
                options: options.clone(),
            };
            let body = provider().build_request_body(&request);
            assert!(
                body.get("reasoning_effort").is_none(),
                "{model} must not receive reasoning_effort"
            );
        }
        // Absent when unset, even on a supporting model.
        let request = ChatRequest {
            model: "grok-4.5".to_string(),
            messages: vec![ChatMessage::user("hi")],
            tools: Vec::new(),
            stream: true,
            options: Some(ChatOptions {
                temperature: Some(0.7),
                num_ctx: None,
                reasoning_effort: None,
            }),
        };
        let body = provider().build_request_body(&request);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn http_failures_downcast_to_provider_error() {
        let err = provider().http_failure(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "slow down".to_string(),
        );
        let provider_err = err
            .downcast_ref::<ProviderError>()
            .expect("typed provider error");
        assert_eq!(provider_err.status, Some(429));
        assert!(provider_err.is_transient());
        assert!(provider_err.message.contains("slow down"), "body surfaces");

        let err = provider().http_failure(reqwest::StatusCode::BAD_REQUEST, "bad".to_string());
        let provider_err = err.downcast_ref::<ProviderError>().expect("typed");
        assert_eq!(provider_err.status, Some(400));
        assert!(!provider_err.is_transient());
    }

    #[tokio::test]
    async fn decodes_sse_with_split_tool_call() {
        // A content delta, then a tool call whose arguments span two fragments.
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(b"data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n".to_vec()),
            Ok(
                b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",\"function\":{\"name\":\"execute\",\"arguments\":\"{\\\"command\\\":\"}}]}}]}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"ls\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\n"
                    .to_vec(),
            ),
            Ok(b"data: [DONE]\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let first = chunks.next().await.expect("content").expect("ok");
        assert!(!first.done);
        assert_eq!(first.message.expect("message").content, "Hi");

        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        assert_eq!(last.done_reason.as_deref(), Some("tool_calls"));
        assert_eq!(last.eval_count, Some(4));
        assert_eq!(last.prompt_eval_count, Some(11));
        let message = last.message.expect("tool call message");
        assert_eq!(message.tool_calls.len(), 1);
        assert_eq!(message.tool_calls[0].function.name, "execute");
        assert_eq!(message.tool_calls[0].function.arguments["command"], "ls");

        assert!(chunks.next().await.is_none(), "stream ends after done");
    }

    #[tokio::test]
    async fn decodes_xai_reasoning_content_as_thinking() {
        // Real xAI grok-4.3 stream shape: `delta.reasoning_content` fragments
        // first, then the visible `delta.content`.
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"grok-4.3\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"Weighing the \"},\"finish_reason\":null}]}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"grok-4.3\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"options.\"},\"finish_reason\":null}]}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"grok-4.3\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Done.\"},\"finish_reason\":\"stop\"}]}\n\n"
                    .to_vec(),
            ),
            Ok(b"data: [DONE]\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let first = chunks.next().await.expect("reasoning").expect("ok");
        assert!(first.thinking, "reasoning delta is flagged");
        assert_eq!(first.message.expect("message").content, "Weighing the ");

        let second = chunks.next().await.expect("reasoning").expect("ok");
        assert!(second.thinking);
        assert_eq!(second.message.expect("message").content, "options.");

        let third = chunks.next().await.expect("content").expect("ok");
        assert!(!third.thinking, "visible text is not flagged");
        assert_eq!(third.message.expect("message").content, "Done.");

        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        assert_eq!(last.done_reason.as_deref(), Some("stop"));
        assert!(chunks.next().await.is_none());
    }

    #[tokio::test]
    async fn reasoning_only_completion_yields_empty_final_chunk() {
        // grok-4.3 sometimes thinks and then just stops (no text, no tools).
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"hmm\"},\"finish_reason\":\"stop\"}]}\n\n"
                    .to_vec(),
            ),
            Ok(b"data: [DONE]\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let first = chunks.next().await.expect("reasoning").expect("ok");
        assert!(first.thinking);

        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        assert!(last.message.is_none(), "no visible message was produced");
        assert!(chunks.next().await.is_none());
    }

    #[tokio::test]
    async fn reasoning_and_content_in_one_delta_keeps_both() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"why\",\"content\":\"Hi\"}}]}\n\n"
                    .to_vec(),
            ),
            Ok(b"data: [DONE]\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let first = chunks.next().await.expect("reasoning").expect("ok");
        assert!(first.thinking);
        assert_eq!(first.message.expect("message").content, "why");

        let second = chunks.next().await.expect("content").expect("ok");
        assert!(!second.thinking);
        assert_eq!(second.message.expect("message").content, "Hi");

        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        assert!(chunks.next().await.is_none());
    }

    #[test]
    fn context_window_table_covers_openai_xai_and_unknowns() {
        assert_eq!(context_window("gpt-4o"), Some(128_000));
        assert_eq!(context_window("gpt-4o-mini"), Some(128_000));
        assert_eq!(context_window("gpt-4.1"), Some(1_047_576));
        assert_eq!(context_window("gpt-5"), Some(400_000));
        assert_eq!(context_window("o3-mini"), Some(200_000));
        assert_eq!(context_window("grok-3"), Some(131_072));
        assert_eq!(context_window("grok-4.3"), Some(256_000));
        assert_eq!(context_window("grok-4.5"), Some(500_000));
        assert_eq!(context_window("qwen3-8b"), None, "local tags stay unknown");
        assert_eq!(context_window(""), None);
    }
}
