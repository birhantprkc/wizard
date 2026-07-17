//! Provider for llama.cpp's `llama-server`.
//!
//! `llama-server` exposes the OpenAI-compatible Chat Completions API under
//! `/v1`, so chat streaming, model listing, and tool support all delegate to
//! an inner [`OpenAiProvider`] bound to `{base_url}/v1`. Only what differs
//! lives here: the health probe hits llama-server's native `GET /health`
//! (which distinguishes "still loading the model" from "down"), and
//! connection failures tell the user how to start the server.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::OnceCell;

use super::openai::OpenAiProvider;
use super::provider::LlmProvider;
use super::{ChatRequest, ChatStream, ProviderError};

/// How long to wait for a TCP connection before declaring llama-server down.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Client bound to one llama-server instance. Cheap to clone.
#[derive(Debug, Clone)]
pub struct LlamaCppProvider {
    http: reqwest::Client,
    /// Server root, e.g. `http://127.0.0.1:11435` (no `/v1` suffix). Trailing
    /// slashes are trimmed.
    base_url: String,
    /// Model tag for [`LlmProvider::label`]; llama-server serves whatever
    /// GGUF it was started with regardless of the requested model.
    model: String,
    /// OpenAI-compatible client bound to `{base_url}/v1`, handling chat
    /// streaming and `/v1/models`. Keyless — llama-server needs no auth.
    inner: OpenAiProvider,
    /// Cached result of the `GET /props` context-window probe (`n_ctx`).
    /// Probed once per provider instance; a failed probe caches `None`.
    ctx_window: Arc<OnceCell<Option<u32>>>,
}

impl LlamaCppProvider {
    /// Build a client for `base_url` (the server root, e.g.
    /// `http://127.0.0.1:11435` — without `/v1`).
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let model = model.into();
        let inner = OpenAiProvider::new(format!("{base_url}/v1"), model.clone(), "");
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            // Builder construction only fails when the TLS backend cannot
            // initialize; fall back to the default client rather than panic.
            .unwrap_or_default();
        Self {
            http,
            base_url,
            model,
            inner,
            ctx_window: Arc::new(OnceCell::new()),
        }
    }

    /// Probe llama-server's `GET /props` for the loaded model's context size
    /// (`default_generation_settings.n_ctx`). Any failure — server down,
    /// older server without the endpoint, unexpected shape — yields `None`.
    async fn fetch_n_ctx(&self) -> Option<u32> {
        let response = self
            .http
            .get(format!("{}/props", self.base_url))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: serde_json::Value = response.json().await.ok()?;
        body.get("default_generation_settings")?
            .get("n_ctx")?
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
    }

    /// Actionable error for a server that cannot be reached at all.
    fn unreachable(&self, source: reqwest::Error) -> anyhow::Error {
        let message = format!(
            "cannot reach llama-server at {} — is the server running? Start it with \
             `llama-server -m <model.gguf> --port 11435` (or check the provider's `base_url` \
             in ~/.wizard/config.toml). Cause: {source}",
            self.base_url
        );
        anyhow::Error::new(source).context(ProviderError::transport(message))
    }

    /// Re-frame errors bubbling out of the inner OpenAI-compatible client:
    /// when the chain bottoms out in a connection failure, prepend the
    /// "start llama-server" hint. Other errors pass through untouched.
    fn reframe(&self, err: anyhow::Error) -> anyhow::Error {
        let is_connect_failure = err.chain().any(|cause| {
            cause
                .downcast_ref::<reqwest::Error>()
                .is_some_and(|e| e.is_connect() || e.is_timeout())
        });
        if is_connect_failure {
            err.context(format!(
                "cannot reach llama-server at {} — is the server running? Start it with \
                 `llama-server -m <model.gguf> --port 11435`",
                self.base_url
            ))
        } else {
            err
        }
    }
}

#[async_trait]
impl LlmProvider for LlamaCppProvider {
    /// Probe llama-server's native `GET /health`: 200 means ready, 503 means
    /// the model is still loading (llama-server answers before the GGUF is
    /// fully in memory).
    async fn health(&self) -> Result<()> {
        let response = self
            .http
            .get(format!("{}/health", self.base_url))
            .send()
            .await
            .map_err(|source| self.unreachable(source))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            return Err(anyhow::Error::new(ProviderError::http(
                503,
                format!(
                    "llama-server at {} is still loading its model (HTTP 503) — try again shortly",
                    self.base_url
                ),
            )));
        }
        let body = response.text().await.unwrap_or_default();
        Err(anyhow::Error::new(ProviderError::http(
            status.as_u16(),
            format!(
                "llama-server at {} returned HTTP {status}: {body}",
                self.base_url
            ),
        )))
    }

    async fn supports_native_tools(&self, model: &str) -> Result<bool> {
        self.inner.supports_native_tools(model).await
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        self.inner
            .list_models()
            .await
            .map_err(|err| self.reframe(err))
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
        self.inner
            .chat_stream(request)
            .await
            .map_err(|err| self.reframe(err))
    }

    /// llama-server serves whatever GGUF it was started with, so the live
    /// `/props` probe beats any static table. Cached after the first call.
    async fn context_window(&self, _model: &str) -> Option<u32> {
        *self.ctx_window.get_or_init(|| self.fetch_n_ctx()).await
    }

    fn label(&self) -> String {
        format!("llama.cpp:{}", self.model)
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::*;

    #[test]
    fn base_url_trailing_slash_is_trimmed() {
        let provider = LlamaCppProvider::new("http://127.0.0.1:8080///", "qwen3-8b");
        assert_eq!(provider.base_url, "http://127.0.0.1:8080");
        assert_eq!(provider.label(), "llama.cpp:qwen3-8b");
    }

    #[test]
    fn inner_client_targets_the_v1_api() {
        let provider = LlamaCppProvider::new("http://10.0.0.5:8080", "m");
        assert_eq!(provider.inner.label(), "openai:m");
        // The unreachable hint names the server root, not the /v1 endpoint.
        let hint = provider.reframe(anyhow!("plain error"));
        assert_eq!(hint.to_string(), "plain error", "non-connect passthrough");
    }

    #[tokio::test]
    async fn context_window_probe_failure_degrades_to_none() {
        // Port 1 on localhost: connection refused immediately, no server
        // needed. The failed probe caches None instead of erroring.
        let provider = LlamaCppProvider::new("http://127.0.0.1:1", "m");
        assert_eq!(provider.context_window("m").await, None);
        assert_eq!(provider.context_window("m").await, None, "cached");
    }

    #[tokio::test]
    async fn unreachable_chat_errors_with_the_start_hint() {
        // The connect failure bubbles out of the inner OpenAI-compatible
        // client; reframe must prepend the "start llama-server" hint.
        let provider = LlamaCppProvider::new("http://127.0.0.1:1", "m");
        let request = ChatRequest {
            model: "m".to_string(),
            messages: vec![crate::llm::ChatMessage::user("hi")],
            tools: Vec::new(),
            stream: true,
            options: None,
        };
        let err = match provider.chat_stream(request).await {
            Ok(_) => panic!("must fail"),
            Err(err) => err,
        };
        let chain = format!("{err:#}");
        assert!(chain.contains("llama-server -m"), "got: {chain}");
        assert!(chain.contains("http://127.0.0.1:1"), "got: {chain}");
    }

    #[tokio::test]
    async fn health_failure_is_actionable() {
        // Port 1 on localhost: connection refused immediately, no server needed.
        let provider = LlamaCppProvider::new("http://127.0.0.1:1", "m");
        let err = provider.health().await.expect_err("must fail");
        let message = err.to_string();
        assert!(message.contains("http://127.0.0.1:1"), "got: {message}");
        assert!(message.contains("llama-server -m"), "got: {message}");
    }
}
