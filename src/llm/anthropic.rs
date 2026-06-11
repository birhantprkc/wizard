//! Streaming HTTP client for the Anthropic **Messages** API
//! (`POST {base_url}/v1/messages`).
//!
//! Thin `reqwest` wrapper with manual SSE parsing. Wizard's native
//! [`ChatRequest`] is translated to the Messages request shape (top-level
//! `system`, content-block `messages`, `tool_use` / `tool_result` blocks) and
//! the SSE event stream is decoded back into Wizard's [`ChatChunk`] stream.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};

use super::provider::LlmProvider;
use super::{ChatChunk, ChatMessage, ChatRequest, ChatStream, FunctionCall, Role, ToolCall};

/// Anthropic API version pinned in the `anthropic-version` header.
const API_VERSION: &str = "2023-06-01";
/// Required `max_tokens` for the Messages API (Anthropic has no implicit cap).
const MAX_TOKENS: u32 = 8192;
/// Static fallback model list when `GET /v1/models` is unavailable.
const FALLBACK_MODELS: &[&str] = &["claude-fable-5"];

/// Client bound to one Anthropic-compatible endpoint.
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    http: reqwest::Client,
    /// Base URL without the `/v1` suffix, e.g. `https://api.anthropic.com`.
    /// Trailing slashes are trimmed.
    base_url: String,
    /// Default model tag (used for [`LlmProvider::label`]).
    model: String,
    /// API key sent in the `x-api-key` header; empty surfaces a 401 at runtime.
    api_key: String,
}

impl AnthropicProvider {
    /// Build a client for `base_url` (defaults to `https://api.anthropic.com`).
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder().build().unwrap_or_default();
        Self {
            http,
            base_url,
            model: model.into(),
            api_key: api_key.into(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Attach the Anthropic auth + version headers.
    fn headers(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
    }

    /// Translate a native [`ChatRequest`] into a Messages API request body.
    fn build_request_body(&self, request: &ChatRequest) -> Value {
        let (system, messages) = build_messages(&request.messages);
        let mut body = json!({
            "model": request.model,
            "max_tokens": MAX_TOKENS,
            "messages": messages,
            "stream": true,
        });
        if !system.is_empty() {
            body["system"] = Value::String(system);
        }
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|spec| {
                json!({
                    "name": spec.function.name,
                    "description": spec.function.description,
                    "input_schema": spec.function.parameters,
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

/// Translate native messages into `(system, messages)`: all system messages are
/// joined into the top-level `system` string; user/assistant turns become
/// content-block messages. Assistant tool calls become `tool_use` blocks (with
/// synthetic ids) and `tool`-role results become a user message holding a
/// `tool_result` block correlated back to the matching `tool_use` id by name.
fn build_messages(messages: &[ChatMessage]) -> (String, Vec<Value>) {
    use std::collections::VecDeque;

    let mut system_parts: Vec<String> = Vec::new();
    let mut pending: BTreeMap<String, VecDeque<String>> = BTreeMap::new();
    let mut seq: u64 = 0;
    let mut out: Vec<Value> = Vec::new();

    for message in messages {
        match message.role {
            Role::System => {
                if !message.content.is_empty() {
                    system_parts.push(message.content.clone());
                }
            }
            Role::User => out.push(json!({
                "role": "user",
                "content": [{ "type": "text", "text": message.content }],
            })),
            Role::Assistant => {
                let mut blocks: Vec<Value> = Vec::new();
                if !message.content.is_empty() {
                    blocks.push(json!({ "type": "text", "text": message.content }));
                }
                for call in &message.tool_calls {
                    seq += 1;
                    let id = format!("toolu_{seq}");
                    pending
                        .entry(call.function.name.clone())
                        .or_default()
                        .push_back(id.clone());
                    let input = match &call.function.arguments {
                        Value::Null => json!({}),
                        other => other.clone(),
                    };
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": id,
                        "name": call.function.name,
                        "input": input,
                    }));
                }
                if blocks.is_empty() {
                    blocks.push(json!({ "type": "text", "text": "" }));
                }
                out.push(json!({ "role": "assistant", "content": blocks }));
            }
            Role::Tool => {
                let name = message.tool_name.clone().unwrap_or_default();
                let id = pending
                    .get_mut(&name)
                    .and_then(|queue| queue.pop_front())
                    .unwrap_or_else(|| format!("toolu_{name}"));
                out.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": message.content,
                    }],
                }));
            }
        }
    }
    (system_parts.join("\n\n"), out)
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn health(&self) -> Result<()> {
        let response = self
            .headers(self.http.get(self.url("/v1/models")))
            .send()
            .await
            .with_context(|| format!("cannot reach {}", self.base_url))?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(anyhow!(
                "{} rejected the API key (HTTP 401) — check the configured API key env var",
                self.base_url
            ));
        }
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("{} returned HTTP {status}: {body}", self.base_url));
        }
        Ok(())
    }

    async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
        Ok(true)
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let fallback = || FALLBACK_MODELS.iter().map(|m| m.to_string()).collect();
        let response = match self
            .headers(self.http.get(self.url("/v1/models")))
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!("listing Anthropic models failed: {err}; using fallback list");
                return Ok(fallback());
            }
        };
        if !response.status().is_success() {
            tracing::warn!(
                "Anthropic /v1/models returned {}; using fallback list",
                response.status()
            );
            return Ok(fallback());
        }
        match response.json::<ModelsResponse>().await {
            Ok(models) => Ok(models.data.into_iter().map(|m| m.id).collect()),
            Err(err) => {
                tracing::warn!("parsing Anthropic models failed: {err}; using fallback list");
                Ok(fallback())
            }
        }
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
        let body = self.build_request_body(&request);
        let response = self
            .headers(self.http.post(self.url("/v1/messages")))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("chat request to {} failed", self.base_url))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("{} returned HTTP {status}: {body}", self.base_url));
        }
        let bytes = response
            .bytes_stream()
            .map(|item| match item {
                Ok(chunk) => Ok(chunk.to_vec()),
                Err(e) => Err(anyhow!(e).context("Anthropic response stream was interrupted")),
            })
            .boxed();
        Ok(decode_sse(bytes))
    }

    fn label(&self) -> String {
        format!("anthropic:{}", self.model)
    }
}

/// `GET /v1/models` response (subset).
#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
}

/// One SSE `data: {...}` event from the Messages stream (subset). The JSON's
/// own `type` field selects the variant.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Event {
    #[serde(rename = "message_start")]
    MessageStart { message: MessageStartBody },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: u64,
        content_block: BlockStart,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: u64, delta: BlockDelta },
    #[serde(rename = "message_delta")]
    MessageDelta {
        #[serde(default)]
        usage: Option<UsageDelta>,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct MessageStartBody {
    #[serde(default)]
    usage: Option<UsageStart>,
}

#[derive(Debug, Deserialize)]
struct UsageStart {
    #[serde(default)]
    input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum BlockStart {
    #[serde(rename = "tool_use")]
    ToolUse { name: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum BlockDelta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    /// Extended-thinking reasoning fragment.
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct UsageDelta {
    #[serde(default)]
    output_tokens: Option<u64>,
}

/// Per-index accumulator for a streamed `tool_use` block.
#[derive(Debug, Default)]
struct ToolAccum {
    name: String,
    input: String,
}

/// Decoder state for [`decode_sse`].
struct SseState<S> {
    bytes: S,
    buf: Vec<u8>,
    tool_calls: BTreeMap<u64, ToolAccum>,
    prompt_eval_count: Option<u64>,
    eval_count: Option<u64>,
    /// Saw `message_stop` or EOF — drain, then emit the final chunk.
    saw_stop: bool,
    emitted_final: bool,
}

/// Build the final `done: true` chunk from accumulated `tool_use` blocks.
fn build_final<S>(state: &SseState<S>) -> ChatChunk {
    let tool_calls: Vec<ToolCall> = state
        .tool_calls
        .values()
        .filter(|accum| !accum.name.is_empty())
        .map(|accum| {
            let arguments = if accum.input.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str::<Value>(&accum.input)
                    .unwrap_or_else(|_| Value::String(accum.input.clone()))
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
        done_reason: None,
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

/// Decode an Anthropic Messages SSE byte stream into a [`ChatStream`]: text
/// and thinking deltas are emitted live; `tool_use` blocks are accumulated and
/// emitted in a single synthesized `done: true` chunk at the end.
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
        saw_stop: false,
        emitted_final: false,
    };
    stream::try_unfold(state, |mut state| async move {
        loop {
            if state.emitted_final {
                return Ok(None);
            }
            while let Some(pos) = state.buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = state.buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim();
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                let event: Event = match serde_json::from_str(payload) {
                    Ok(event) => event,
                    Err(_) => continue,
                };
                match event {
                    Event::MessageStart { message } => {
                        if let Some(usage) = message.usage
                            && let Some(input) = usage.input_tokens
                        {
                            state.prompt_eval_count = Some(input);
                        }
                    }
                    Event::ContentBlockStart {
                        index,
                        content_block,
                    } => {
                        if let BlockStart::ToolUse { name } = content_block {
                            state.tool_calls.entry(index).or_default().name = name;
                        }
                    }
                    Event::ContentBlockDelta { index, delta } => match delta {
                        BlockDelta::Text { text } => {
                            if !text.is_empty() {
                                return Ok(Some((text_chunk(text, false), state)));
                            }
                        }
                        BlockDelta::Thinking { thinking } => {
                            if !thinking.is_empty() {
                                return Ok(Some((text_chunk(thinking, true), state)));
                            }
                        }
                        BlockDelta::InputJson { partial_json } => {
                            state
                                .tool_calls
                                .entry(index)
                                .or_default()
                                .input
                                .push_str(&partial_json);
                        }
                        BlockDelta::Other => {}
                    },
                    Event::MessageDelta { usage } => {
                        if let Some(usage) = usage
                            && let Some(output) = usage.output_tokens
                        {
                            state.eval_count = Some(output);
                        }
                    }
                    Event::MessageStop => state.saw_stop = true,
                    Event::Other => {}
                }
            }
            if state.saw_stop {
                state.emitted_final = true;
                let final_chunk = build_final(&state);
                return Ok(Some((final_chunk, state)));
            }
            match state.bytes.next().await {
                Some(Ok(data)) => state.buf.extend_from_slice(&data),
                Some(Err(e)) => return Err(e),
                None => {
                    if !state.buf.is_empty() && state.buf.last() != Some(&b'\n') {
                        state.buf.push(b'\n');
                    }
                    state.saw_stop = true;
                }
            }
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolSpec;

    fn provider() -> AnthropicProvider {
        AnthropicProvider::new("https://api.anthropic.com/", "claude-fable-5", "key")
    }

    #[test]
    fn translates_native_request_to_messages_shape() {
        let mut assistant = ChatMessage::assistant("Let me read it.");
        assistant.tool_calls.push(ToolCall {
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: json!({ "path": "src/main.rs" }),
            },
        });
        let request = ChatRequest {
            model: "claude-fable-5".to_string(),
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
            options: Some(crate::llm::ChatOptions {
                temperature: Some(0.5),
                num_ctx: None,
            }),
        };

        let body = provider().build_request_body(&request);
        assert_eq!(body["model"], "claude-fable-5");
        assert_eq!(body["max_tokens"], MAX_TOKENS);
        assert_eq!(body["stream"], true);
        assert_eq!(body["system"], "You are Wizard.");

        // messages[0]: user text block; messages[1]: assistant text + tool_use.
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");

        let assistant = &body["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"][0]["type"], "text");
        let tool_use = &assistant["content"][1];
        assert_eq!(tool_use["type"], "tool_use");
        assert_eq!(tool_use["name"], "read_file");
        assert_eq!(tool_use["input"]["path"], "src/main.rs");
        let id = tool_use["id"].as_str().expect("tool_use id");

        // messages[2]: user message carrying the tool_result, correlated by id.
        let result = &body["messages"][2];
        assert_eq!(result["role"], "user");
        assert_eq!(result["content"][0]["type"], "tool_result");
        assert_eq!(result["content"][0]["tool_use_id"], id);
        assert_eq!(result["content"][0]["content"], "fn main() {}");

        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    }

    #[tokio::test]
    async fn decodes_sse_text_and_tool_use() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9}}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"execute\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"ls\\\"}\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":6}}\n\n"
                    .to_vec(),
            ),
            Ok(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let first = chunks.next().await.expect("text").expect("ok");
        assert!(!first.done);
        assert_eq!(first.message.expect("message").content, "Hi");

        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        assert_eq!(last.prompt_eval_count, Some(9));
        assert_eq!(last.eval_count, Some(6));
        let message = last.message.expect("tool call message");
        assert_eq!(message.tool_calls.len(), 1);
        assert_eq!(message.tool_calls[0].function.name, "execute");
        assert_eq!(message.tool_calls[0].function.arguments["command"], "ls");

        assert!(chunks.next().await.is_none());
    }

    #[tokio::test]
    async fn decodes_thinking_deltas_as_thinking() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(
                b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Considering...\"}}\n\n"
                    .to_vec(),
            ),
            Ok(
                b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Answer.\"}}\n\n"
                    .to_vec(),
            ),
            Ok(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_vec()),
        ];
        let mut chunks = decode_sse(stream::iter(parts));

        let first = chunks.next().await.expect("thinking").expect("ok");
        assert!(first.thinking, "thinking delta is flagged");
        assert_eq!(first.message.expect("message").content, "Considering...");

        let second = chunks.next().await.expect("text").expect("ok");
        assert!(!second.thinking, "visible text is not flagged");
        assert_eq!(second.message.expect("message").content, "Answer.");

        let last = chunks.next().await.expect("final").expect("ok");
        assert!(last.done);
        assert!(chunks.next().await.is_none());
    }
}
