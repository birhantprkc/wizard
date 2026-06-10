//! The provider abstraction: one trait every LLM backend implements so the
//! agent loop, the tool registry, and the TUI are decoupled from any specific
//! API (llama.cpp, Ollama, OpenAI-compatible, Anthropic, ...).
//!
//! Concrete implementations live in sibling modules ([`super::llamacpp`],
//! [`super::ollama`], [`super::openai`], [`super::anthropic`]). A provider is
//! built from a [`crate::config::ProviderConfig`] and handed to the agent as
//! an `Arc<dyn LlmProvider>`.

use async_trait::async_trait;

/// A streaming chat backend. Implementations translate Wizard's native wire
/// types (see [`crate::llm`]) to and from their own API shape, exposing a
/// uniform [`ChatChunk`](crate::llm::ChatChunk) stream the agent loop consumes.
///
/// All methods are `async` and fallible; transport and API errors surface as
/// `anyhow::Error` so the TUI can render actionable messages.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Cheap reachability/auth probe run at startup. Errors when the backend
    /// is unreachable or the credentials are rejected.
    async fn health(&self) -> anyhow::Result<()>;

    /// Whether `model` supports native (structured) tool calling. When this
    /// returns `false` the agent loop falls back to the prompt-based JSON tool
    /// protocol.
    async fn supports_native_tools(&self, model: &str) -> anyhow::Result<bool>;

    /// List the models the backend exposes (for the `/model` picker).
    async fn list_models(&self) -> anyhow::Result<Vec<String>>;

    /// Start a streaming chat completion, yielding
    /// [`ChatChunk`](crate::llm::ChatChunk)s until one with `done == true`.
    async fn chat_stream(
        &self,
        request: crate::llm::ChatRequest,
    ) -> anyhow::Result<crate::llm::ChatStream>;

    /// Short human label for the status bar / errors (e.g. the host or
    /// `"openai:gpt-4o"`).
    fn label(&self) -> String;
}
