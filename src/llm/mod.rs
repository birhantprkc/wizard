//! LLM wire types matching Ollama's **native** `/api/chat` schema
//! (not the OpenAI-compatible shim). Shared by the agent loop, the tool
//! registry, and the TUI.

pub mod anthropic;
pub mod ollama;
pub mod openai;
pub mod provider;

use std::pin::Pin;

use anyhow::Result;
use futures_util::Stream;
use serde::{Deserialize, Serialize};

/// Boxed stream of [`ChatChunk`]s yielded by every provider's `chat_stream`.
/// Shared across [`ollama`], [`openai`], and [`anthropic`].
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<ChatChunk>> + Send>>;

/// Message role on the Ollama `/api/chat` wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// Tool result fed back to the model.
    Tool,
}

/// A single chat message, serialized exactly as Ollama expects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    /// Tool calls emitted by the model (assistant messages only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Name of the tool that produced this result (`role == Tool`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_name: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_name: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_name: None,
        }
    }

    /// Tool result message answering a prior [`ToolCall`].
    pub fn tool_result(tool_name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_name: Some(tool_name.into()),
        }
    }
}

/// A tool invocation requested by the model.
/// Wire shape: `{ "function": { "name": ..., "arguments": {...} } }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub function: FunctionCall,
}

/// The function half of a [`ToolCall`]. `arguments` is a JSON object
/// (already parsed — Ollama's native endpoint sends structured arguments,
/// not a string).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// A tool advertised to the model in the request's `tools` array.
/// Wire shape: `{ "type": "function", "function": { name, description, parameters } }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionSpec,
}

impl ToolSpec {
    /// Build a `"function"`-typed spec. `parameters` must be a JSON Schema
    /// object describing the arguments.
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            kind: "function".to_string(),
            function: FunctionSpec {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

/// The function half of a [`ToolSpec`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's arguments.
    pub parameters: serde_json::Value,
}

/// Model sampling options forwarded as Ollama's `options` object.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,
}

/// Request body for `POST /api/chat`.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<ChatOptions>,
}

/// One streamed JSON line from `POST /api/chat` (`stream: true`).
/// Text arrives as deltas in `message.content`; tool calls arrive complete
/// in `message.tool_calls`; the final chunk has `done == true`.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub message: Option<ChatMessage>,
    pub done: bool,
    #[serde(default)]
    pub done_reason: Option<String>,
    /// Output token count (final chunk only).
    #[serde(default)]
    pub eval_count: Option<u64>,
    /// Prompt token count (final chunk only).
    #[serde(default)]
    pub prompt_eval_count: Option<u64>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn chat_request_serializes_to_native_ollama_shape() {
        let request = ChatRequest {
            model: "qwen3.6:27b".to_string(),
            messages: vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("hi"),
            ],
            tools: vec![ToolSpec::function(
                "read_file",
                "Read a file.",
                json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }),
            )],
            stream: true,
            options: Some(ChatOptions {
                temperature: Some(0.8),
                num_ctx: None,
            }),
        };

        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["model"], "qwen3.6:27b");
        assert_eq!(value["stream"], true);
        assert_eq!(value["messages"][0]["role"], "system");
        assert_eq!(value["messages"][1]["role"], "user");
        assert_eq!(value["messages"][1]["content"], "hi");
        assert_eq!(value["tools"][0]["type"], "function");
        assert_eq!(value["tools"][0]["function"]["name"], "read_file");
        assert_eq!(
            value["tools"][0]["function"]["parameters"]["required"][0],
            "path"
        );
        let temperature = value["options"]["temperature"]
            .as_f64()
            .expect("temperature is a number");
        assert!(
            (temperature - 0.8).abs() < 1e-6,
            "temperature survives the f32 round-trip: {temperature}"
        );
        assert!(
            value["options"].get("num_ctx").is_none(),
            "unset options are omitted"
        );
    }

    #[test]
    fn empty_tools_and_options_are_omitted_from_the_wire() {
        let request = ChatRequest {
            model: "m".to_string(),
            messages: vec![ChatMessage::user("hi")],
            tools: Vec::new(),
            stream: false,
            options: None,
        };
        let value = serde_json::to_value(&request).unwrap();
        assert!(value.get("tools").is_none(), "empty tools array is omitted");
        assert!(value.get("options").is_none(), "absent options are omitted");
    }

    #[test]
    fn plain_message_omits_tool_fields() {
        let value = serde_json::to_value(ChatMessage::assistant("done")).unwrap();
        assert!(value.get("tool_calls").is_none());
        assert!(value.get("tool_name").is_none());
    }

    #[test]
    fn assistant_tool_call_round_trips() {
        let mut message = ChatMessage::assistant("");
        message.tool_calls.push(ToolCall {
            function: FunctionCall {
                name: "execute".to_string(),
                arguments: json!({ "command": "cargo test" }),
            },
        });

        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["tool_calls"][0]["function"]["name"], "execute");
        assert_eq!(
            value["tool_calls"][0]["function"]["arguments"]["command"],
            "cargo test"
        );

        let back: ChatMessage = serde_json::from_value(value).unwrap();
        assert_eq!(back.tool_calls.len(), 1);
        assert_eq!(back.tool_calls[0].function.name, "execute");
    }

    #[test]
    fn tool_result_message_carries_role_and_name() {
        let value =
            serde_json::to_value(ChatMessage::tool_result("read_file", "contents")).unwrap();
        assert_eq!(value["role"], "tool");
        assert_eq!(value["tool_name"], "read_file");
        assert_eq!(value["content"], "contents");
    }

    #[test]
    fn tool_call_with_missing_arguments_deserializes_to_null() {
        // Ollama may omit `arguments` entirely; the agent normalizes null later.
        let call: ToolCall = serde_json::from_str(r#"{"function":{"name":"git_status"}}"#).unwrap();
        assert_eq!(call.function.name, "git_status");
        assert!(call.function.arguments.is_null());
    }
}
