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
use super::{ChatChunk, ChatMessage, ChatRequest, ChatStream, FunctionCall, Role, ToolCall};

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
        let http = reqwest::Client::builder().build().unwrap_or_default();
        Self {
            http,
            base_url,
            model: model.into(),
            auth,
            vendor,
        }
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
            let response = request.send().await?;
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
        anyhow!("{} returned HTTP {status}: {body}{hint}", self.base_url)
    }

    /// Translate a native [`ChatRequest`] into the OpenAI Chat Completions
    /// request body. Always sets `stream: true`.
    fn build_request_body(&self, request: &ChatRequest) -> Value {
        let messages = build_messages(&request.messages);
        let mut body = json!({
            "model": request.model,
            "messages": messages,
            "stream": true,
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
        {
            body["temperature"] = json!(temperature);
        }
        body
    }
}

/// Translate native messages into the OpenAI `messages` array. Tool calls are
/// assigned synthetic ids (`call_N`) as they appear on assistant messages;
/// `tool`-role results are correlated back to those ids by tool name (the
/// earliest unmatched call of the same name), since Wizard's wire format does
/// not carry call ids.
fn build_messages(messages: &[ChatMessage]) -> Vec<Value> {
    use std::collections::VecDeque;

    let mut pending: BTreeMap<String, VecDeque<String>> = BTreeMap::new();
    let mut seq: u64 = 0;
    let mut out = Vec::with_capacity(messages.len());

    for message in messages {
        match message.role {
            Role::System => out.push(json!({ "role": "system", "content": message.content })),
            Role::User => out.push(json!({ "role": "user", "content": message.content })),
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

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn health(&self) -> Result<()> {
        let response = self
            .send_authed(|| self.http.get(self.url("/models")))
            .await
            .with_context(|| format!("cannot reach {}", self.base_url))?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(anyhow!(
                "{} rejected the credentials (HTTP 401): {}",
                self.base_url,
                self.auth.unauthorized_hint()
            ));
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
                Err(e) => Err(anyhow!(e).context("OpenAI response stream was interrupted")),
            })
            .boxed();
        Ok(decode_sse(bytes))
    }

    fn label(&self) -> String {
        format!("{}:{}", self.vendor, self.model)
    }
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
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
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
    tool_calls: BTreeMap<u64, ToolAccum>,
    prompt_eval_count: Option<u64>,
    eval_count: Option<u64>,
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
        done: true,
        done_reason: None,
        eval_count: state.eval_count,
        prompt_eval_count: state.prompt_eval_count,
    }
}

/// Decode an OpenAI SSE byte stream into a [`ChatStream`]: text deltas are
/// emitted live as `done: false` chunks; tool-call fragments are accumulated
/// per index and emitted in a single synthesized `done: true` chunk at the end.
pub(crate) fn decode_sse<S>(bytes: S) -> ChatStream
where
    S: Stream<Item = Result<Vec<u8>>> + Send + Unpin + 'static,
{
    let state = SseState {
        bytes,
        buf: Vec::new(),
        tool_calls: BTreeMap::new(),
        prompt_eval_count: None,
        eval_count: None,
        saw_done: false,
        emitted_final: false,
    };
    stream::try_unfold(state, |mut state| async move {
        loop {
            if state.emitted_final {
                return Ok(None);
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
                    if let Some(content) = choice.delta.content
                        && !content.is_empty()
                    {
                        let out = ChatChunk {
                            message: Some(ChatMessage::assistant(content)),
                            done: false,
                            done_reason: None,
                            eval_count: None,
                            prompt_eval_count: None,
                        };
                        return Ok(Some((out, state)));
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
            }),
        };

        let body = provider().build_request_body(&request);
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["stream"], true);
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
                b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"ls\\\"}\"}}]}}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\n"
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
        assert_eq!(last.eval_count, Some(4));
        assert_eq!(last.prompt_eval_count, Some(11));
        let message = last.message.expect("tool call message");
        assert_eq!(message.tool_calls.len(), 1);
        assert_eq!(message.tool_calls[0].function.name, "execute");
        assert_eq!(message.tool_calls[0].function.arguments["command"], "ls");

        assert!(chunks.next().await.is_none(), "stream ends after done");
    }
}
