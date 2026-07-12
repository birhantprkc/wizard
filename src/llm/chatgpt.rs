//! Streaming client for a ChatGPT **subscription** — the Responses API at
//! `chatgpt.com/backend-api/codex`, reached with OAuth tokens from
//! [`super::chatgpt_oauth`] rather than an API key.
//!
//! This is not the OpenAI Chat Completions API: the request is the Responses
//! shape (`instructions` + `input` items), the stream is Responses SSE
//! (`response.output_text.delta`, `response.output_item.done`, …), and every
//! call carries the ChatGPT account id and the Codex client identity that the
//! endpoint requires. Wizard's native [`ChatRequest`] is translated in and the
//! SSE is decoded back into Wizard's [`ChatChunk`] stream, so the agent core
//! sees the same interface as every other provider.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::chatgpt_oauth::{self, StoredTokens};
use super::provider::LlmProvider;
use super::{
    ChatChunk, ChatMessage, ChatRequest, ChatStream, FunctionCall, ProviderError, Role, ToolCall,
};

/// Static fallback model list; the live list comes from `GET /models`.
const FALLBACK_MODELS: &[&str] = &["gpt-5.2", "gpt-5.5", "gpt-5.6-luna"];

/// Manages the stored OAuth tokens: hands out the bearer + account id, and
/// refreshes proactively (near expiry) or after a 401.
#[derive(Debug)]
pub struct ChatgptTokens {
    path: PathBuf,
    cache: Mutex<Option<StoredTokens>>,
}

impl ChatgptTokens {
    pub fn new() -> Result<Self> {
        Ok(Self {
            path: chatgpt_oauth::token_path()?,
            cache: Mutex::new(None),
        })
    }

    async fn load(&self) -> Result<StoredTokens> {
        let mut cache = self.cache.lock().await;
        if cache.is_none() {
            *cache = chatgpt_oauth::load_tokens(&self.path)?;
        }
        cache
            .clone()
            .ok_or_else(|| anyhow!("not signed in to ChatGPT; run `wizard --login chatgpt` first"))
    }

    /// The `(access_token, account_id)` to authorize a request, refreshing the
    /// access token first if it is close to expiry.
    async fn credentials(&self) -> Result<(String, Option<String>)> {
        let tokens = self.load().await?;
        if chatgpt_oauth::expires_soon(&tokens.access_token)
            && let Some(refreshed) = self.refresh().await?
        {
            return Ok((refreshed.access_token, refreshed.account_id));
        }
        Ok((tokens.access_token, tokens.account_id))
    }

    /// Force a refresh after a 401. `true` when a fresh token was obtained.
    async fn refresh_after_unauthorized(&self) -> Result<bool> {
        Ok(self.refresh().await?.is_some())
    }

    /// Exchange the refresh token for new tokens, persist, and cache them.
    /// `None` when there is no refresh token to use.
    async fn refresh(&self) -> Result<Option<StoredTokens>> {
        let current = self.load().await?;
        let Some(refresh_token) = current.refresh_token.clone() else {
            return Ok(None);
        };
        let response = chatgpt_oauth::refresh(&refresh_token).await?;
        // A refresh may omit the refresh token or id_token; keep what we had.
        let id_token = response.id_token.or(current.id_token);
        let account_id = id_token
            .as_deref()
            .and_then(chatgpt_oauth::account_id_from_id_token)
            .or(current.account_id);
        let merged = StoredTokens {
            access_token: response.access_token,
            refresh_token: response.refresh_token.or(current.refresh_token),
            id_token,
            account_id,
        };
        chatgpt_oauth::save_tokens(&self.path, &merged)?;
        *self.cache.lock().await = Some(merged.clone());
        Ok(Some(merged))
    }
}

/// Client for one ChatGPT-subscription account.
#[derive(Debug)]
pub struct ChatgptProvider {
    http: reqwest::Client,
    base_url: String,
    model: String,
    tokens: Arc<ChatgptTokens>,
    /// A stable id for this process's requests (the endpoint expects one).
    session_id: String,
}

impl ChatgptProvider {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        Ok(Self {
            http: crate::llm::cloud_http_builder().build().unwrap_or_default(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            tokens: Arc::new(ChatgptTokens::new()?),
            session_id: session_id(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Attach the auth + Codex-client headers a subscription request needs.
    async fn authed(&self, builder: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        let (access, account) = self.tokens.credentials().await?;
        let mut builder = builder
            .header("Authorization", format!("Bearer {access}"))
            .header("originator", chatgpt_oauth::API_ORIGINATOR)
            .header("User-Agent", user_agent())
            .header("session-id", &self.session_id)
            .header("OpenAI-Beta", "responses=experimental");
        if let Some(account) = account {
            builder = builder.header("ChatGPT-Account-ID", account);
        }
        Ok(builder)
    }

    fn build_request_body(&self, request: &ChatRequest) -> Value {
        let (instructions, input) = build_input(&request.messages);
        let mut body = json!({
            "model": request.model,
            "input": input,
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            // The subscription endpoint is stateless per call for this client.
            "store": false,
            "stream": true,
            "include": ["reasoning.encrypted_content"],
        });
        if !instructions.is_empty() {
            body["instructions"] = Value::String(instructions);
        }
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|spec| {
                json!({
                    "type": "function",
                    "name": spec.function.name,
                    "description": spec.function.description,
                    "parameters": spec.function.parameters,
                })
            })
            .collect();
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if let Some(options) = &request.options
            && let Some(effort) = &options.reasoning_effort
        {
            body["reasoning"] = json!({ "effort": effort, "summary": "auto" });
        }
        body
    }

    async fn post_responses(&self, request: &ChatRequest) -> Result<reqwest::Response> {
        let body = self.build_request_body(request);
        let send = || async {
            self.authed(self.http.post(self.url("/responses")).json(&body))
                .await?
                .header("Accept", "text/event-stream")
                .send()
                .await
                .with_context(|| format!("chat request to {} failed", self.base_url))
        };
        let mut response = send().await?;
        // One refresh-and-retry on 401, exactly as the keyed OpenAI client does.
        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            && self
                .tokens
                .refresh_after_unauthorized()
                .await
                .unwrap_or(false)
        {
            response = send().await?;
        }
        Ok(response)
    }

    fn http_failure(&self, status: reqwest::StatusCode, body: String) -> anyhow::Error {
        let hint = if status == reqwest::StatusCode::UNAUTHORIZED {
            " — sign in again from Settings"
        } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            " — your ChatGPT plan's usage limit was reached"
        } else {
            ""
        };
        anyhow::Error::new(ProviderError::http(
            status.as_u16(),
            format!("ChatGPT returned HTTP {status}{hint}: {body}"),
        ))
    }
}

#[async_trait]
impl LlmProvider for ChatgptProvider {
    async fn health(&self) -> Result<()> {
        let response = self
            .authed(self.http.get(self.url("/models")))
            .await?
            .send()
            .await
            .with_context(|| format!("cannot reach {}", self.base_url))?;
        if response.status().is_success() {
            return Ok(());
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(self.http_failure(status, body))
    }

    async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
        Ok(true)
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let response = match self.authed(self.http.get(self.url("/models"))).await {
            Ok(builder) => builder.send().await,
            Err(_) => return Ok(fallback_models()),
        };
        let Ok(response) = response else {
            return Ok(fallback_models());
        };
        if !response.status().is_success() {
            return Ok(fallback_models());
        }
        match response.json::<ModelsResponse>().await {
            Ok(models) if !models.data.is_empty() => {
                Ok(models.data.into_iter().map(|m| m.id).collect())
            }
            _ => Ok(fallback_models()),
        }
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
        let response = self.post_responses(&request).await?;
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
                    "ChatGPT response stream was interrupted",
                ))),
            })
            .boxed();
        Ok(decode_sse(bytes))
    }

    async fn context_window(&self, model: &str) -> Option<u32> {
        super::openai::context_window(model)
    }

    fn label(&self) -> String {
        format!("chatgpt:{}", self.model)
    }
}

fn fallback_models() -> Vec<String> {
    FALLBACK_MODELS.iter().map(|m| m.to_string()).collect()
}

/// A per-process request id. `getrandom` avoids the `Date`/`rand` bans in some
/// build contexts and is already a dependency.
fn session_id() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn user_agent() -> String {
    format!("codex_cli_rs/{} (wizard)", env!("CARGO_PKG_VERSION"))
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
}

/// Translate native messages into `(instructions, input)`: system messages join
/// into `instructions`; user/assistant/tool turns become Responses `input`
/// items. Assistant tool calls become `function_call` items with synthetic
/// `call_id`s, and `tool`-role results become `function_call_output` items
/// correlated back to those ids by tool name and order.
fn build_input(messages: &[ChatMessage]) -> (String, Vec<Value>) {
    use std::collections::BTreeMap;

    let mut instructions: Vec<String> = Vec::new();
    let mut pending: BTreeMap<String, VecDeque<String>> = BTreeMap::new();
    let mut seq: u64 = 0;
    let mut input: Vec<Value> = Vec::new();

    for message in messages {
        match message.role {
            Role::System => {
                if !message.content.is_empty() {
                    instructions.push(message.content.clone());
                }
            }
            Role::User => input.push(json!({
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": message.content }],
            })),
            Role::Assistant => {
                if !message.content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": message.content }],
                    }));
                }
                for call in &message.tool_calls {
                    seq += 1;
                    let call_id = format!("call_{seq}");
                    pending
                        .entry(call.function.name.clone())
                        .or_default()
                        .push_back(call_id.clone());
                    // Responses wants arguments as a JSON string, not an object.
                    let arguments = match &call.function.arguments {
                        Value::Null => "{}".to_string(),
                        other => other.to_string(),
                    };
                    input.push(json!({
                        "type": "function_call",
                        "name": call.function.name,
                        "arguments": arguments,
                        "call_id": call_id,
                    }));
                }
            }
            Role::Tool => {
                let name = message.tool_name.clone().unwrap_or_default();
                let call_id = pending
                    .get_mut(&name)
                    .and_then(|queue| queue.pop_front())
                    .unwrap_or_else(|| format!("call_{name}"));
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": message.content,
                }));
            }
        }
    }
    (instructions.join("\n\n"), input)
}

/* --- SSE decoding --------------------------------------------------------- */

/// One decoded Responses SSE event (the subset Wizard acts on).
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Event {
    #[serde(rename = "response.output_text.delta")]
    TextDelta { delta: String },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningDelta { delta: String },
    #[serde(rename = "response.output_item.done")]
    ItemDone { item: OutputItem },
    #[serde(rename = "response.completed")]
    Completed { response: CompletedResponse },
    #[serde(rename = "response.failed")]
    Failed { response: FailedResponse },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum OutputItem {
    #[serde(rename = "function_call")]
    FunctionCall {
        name: String,
        /// Arguments as a JSON string, per the Responses wire format.
        #[serde(default)]
        arguments: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct CompletedResponse {
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct FailedResponse {
    #[serde(default)]
    error: Option<FailedError>,
}

#[derive(Debug, Deserialize)]
struct FailedError {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

struct SseState<S> {
    bytes: S,
    buf: Vec<u8>,
    tool_calls: Vec<ToolCall>,
    prompt_tokens: Option<u64>,
    output_tokens: Option<u64>,
    done: bool,
    emitted_final: bool,
    failure: Option<String>,
}

/// Decode a Responses SSE byte stream into a [`ChatStream`]: text and reasoning
/// deltas are emitted live; completed `function_call` items are accumulated and
/// flushed in one synthesized `done: true` chunk at the end.
pub(crate) fn decode_sse<S>(bytes: S) -> ChatStream
where
    S: Stream<Item = Result<Vec<u8>>> + Send + Unpin + 'static,
{
    let state = SseState {
        bytes,
        buf: Vec::new(),
        tool_calls: Vec::new(),
        prompt_tokens: None,
        output_tokens: None,
        done: false,
        emitted_final: false,
        failure: None,
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
                if payload == "[DONE]" {
                    state.done = true;
                    continue;
                }
                let event: Event = match serde_json::from_str(payload) {
                    Ok(event) => event,
                    Err(_) => continue,
                };
                match event {
                    Event::TextDelta { delta } if !delta.is_empty() => {
                        return Ok(Some((text_chunk(delta, false), state)));
                    }
                    Event::ReasoningDelta { delta } if !delta.is_empty() => {
                        return Ok(Some((text_chunk(delta, true), state)));
                    }
                    Event::ItemDone {
                        item: OutputItem::FunctionCall { name, arguments },
                    } => {
                        let arguments = serde_json::from_str(&arguments).unwrap_or(Value::Null);
                        state.tool_calls.push(ToolCall {
                            function: FunctionCall { name, arguments },
                        });
                    }
                    Event::Completed { response } => {
                        if let Some(usage) = response.usage {
                            state.prompt_tokens = usage.input_tokens;
                            state.output_tokens = usage.output_tokens;
                        }
                        state.done = true;
                    }
                    Event::Failed { response } => {
                        let error = response.error;
                        let message = error
                            .as_ref()
                            .and_then(|e| e.message.clone())
                            .or_else(|| error.as_ref().and_then(|e| e.code.clone()))
                            .unwrap_or_else(|| "the response failed".to_string());
                        state.failure = Some(message);
                        state.done = true;
                    }
                    _ => {}
                }
            }
            if state.done {
                if let Some(message) = state.failure.take() {
                    return Err(anyhow!(ProviderError::http(502, message)));
                }
                state.emitted_final = true;
                return Ok(Some((build_final(&mut state), state)));
            }
            match state.bytes.next().await {
                Some(Ok(data)) => state.buf.extend_from_slice(&data),
                Some(Err(e)) => return Err(e),
                None => {
                    if !state.buf.is_empty() && state.buf.last() != Some(&b'\n') {
                        state.buf.push(b'\n');
                    }
                    state.done = true;
                }
            }
        }
    })
    .boxed()
}

fn text_chunk(text: String, thinking: bool) -> ChatChunk {
    ChatChunk {
        message: Some(ChatMessage {
            role: Role::Assistant,
            content: text,
            tool_calls: Vec::new(),
            tool_name: None,
        }),
        thinking,
        done: false,
        done_reason: None,
        eval_count: None,
        prompt_eval_count: None,
    }
}

fn build_final<S>(state: &mut SseState<S>) -> ChatChunk {
    let tool_calls = std::mem::take(&mut state.tool_calls);
    ChatChunk {
        message: Some(ChatMessage {
            role: Role::Assistant,
            content: String::new(),
            tool_calls,
            tool_name: None,
        }),
        thinking: false,
        done: true,
        done_reason: Some("stop".to_string()),
        eval_count: state.output_tokens,
        prompt_eval_count: state.prompt_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolSpec;

    fn request(messages: Vec<ChatMessage>, tools: Vec<ToolSpec>) -> ChatRequest {
        ChatRequest {
            model: "gpt-5.2".to_string(),
            messages,
            tools,
            stream: true,
            options: None,
        }
    }

    fn provider() -> ChatgptProvider {
        // No tokens needed to test request translation.
        ChatgptProvider {
            http: reqwest::Client::new(),
            base_url: chatgpt_oauth::BASE_URL.to_string(),
            model: "gpt-5.2".to_string(),
            tokens: Arc::new(ChatgptTokens {
                path: PathBuf::from("/nonexistent"),
                cache: Mutex::new(None),
            }),
            session_id: "test".to_string(),
        }
    }

    #[test]
    fn translates_messages_to_responses_input() {
        let mut assistant = ChatMessage::assistant("Reading it.");
        assistant.tool_calls.push(ToolCall {
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: json!({ "path": "src/main.rs" }),
            },
        });
        let body = provider().build_request_body(&request(
            vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("read it"),
                assistant,
                ChatMessage::tool_result("read_file", "fn main() {}"),
            ],
            vec![ToolSpec::function(
                "read_file",
                "Read a file.",
                json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
            )],
        ));

        assert_eq!(body["model"], "gpt-5.2");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["instructions"], "You are Wizard.");

        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");

        // assistant text, then its function_call
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["name"], "read_file");
        let call_id = input[2]["call_id"].as_str().unwrap();
        // arguments are a JSON *string*
        assert_eq!(input[2]["arguments"], "{\"path\":\"src/main.rs\"}");

        // the tool result, correlated back to the call by id
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], call_id);
        assert_eq!(input[3]["output"], "fn main() {}");

        // tools use the flat Responses shape
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["parameters"]["type"], "object");
    }

    #[tokio::test]
    async fn decodes_text_and_a_tool_call() {
        let parts: Vec<Result<Vec<u8>>> = vec![
            Ok(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello \"}\n\n".to_vec()),
            Ok(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"world\"}\n\n".to_vec()),
            Ok(b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\",\"call_id\":\"call_1\"}}\n\n".to_vec()),
            Ok(b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":3}}}\n\n".to_vec()),
        ];
        let bytes = stream::iter(parts).boxed();
        let mut out = decode_sse(bytes);

        let mut text = String::new();
        let mut final_chunk = None;
        while let Some(chunk) = out.next().await {
            let chunk = chunk.unwrap();
            if chunk.done {
                final_chunk = Some(chunk);
            } else if let Some(msg) = &chunk.message {
                text.push_str(&msg.content);
            }
        }
        assert_eq!(text, "Hello world");
        let final_chunk = final_chunk.expect("a final chunk");
        assert_eq!(final_chunk.prompt_eval_count, Some(11));
        assert_eq!(final_chunk.eval_count, Some(3));
        let calls = &final_chunk.message.unwrap().tool_calls;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments["path"], "a.rs");
    }

    #[tokio::test]
    async fn a_failed_response_becomes_an_error() {
        let parts: Vec<Result<Vec<u8>>> = vec![Ok(
            b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"slow down\"}}}\n\n".to_vec(),
        )];
        let bytes = stream::iter(parts).boxed();
        let mut out = decode_sse(bytes);
        let mut saw_error = false;
        while let Some(chunk) = out.next().await {
            if let Err(err) = chunk {
                saw_error = true;
                assert!(format!("{err:#}").contains("slow down"));
            }
        }
        assert!(saw_error);
    }
}
