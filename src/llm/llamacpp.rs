//! Provider for llama.cpp's `llama-server`.
//!
//! `llama-server` exposes the OpenAI-compatible Chat Completions API under
//! `/v1`, so chat streaming, model listing, and tool support all delegate to
//! an inner [`OpenAiProvider`] bound to `{base_url}/v1`. Only what differs
//! lives here: the health probe hits llama-server's native `GET /health`
//! (which distinguishes "still loading the model" from "down"), and
//! connection failures tell the user how to start the server.

use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;

use super::openai::OpenAiProvider;
use super::provider::LlmProvider;
use super::{ChatRequest, ChatStream};

/// How long to wait for a TCP connection before declaring llama-server down.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Client bound to one llama-server instance. Cheap to clone.
#[derive(Debug, Clone)]
pub struct LlamaCppProvider {
    http: reqwest::Client,
    /// Server root, e.g. `http://127.0.0.1:8080` (no `/v1` suffix). Trailing
    /// slashes are trimmed.
    base_url: String,
    /// Model tag for [`LlmProvider::label`]; llama-server serves whatever
    /// GGUF it was started with regardless of the requested model.
    model: String,
    /// OpenAI-compatible client bound to `{base_url}/v1`, handling chat
    /// streaming and `/v1/models`. Keyless — llama-server needs no auth.
    inner: OpenAiProvider,
}

impl LlamaCppProvider {
    /// Build a client for `base_url` (the server root, e.g.
    /// `http://127.0.0.1:8080` — without `/v1`).
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
        }
    }

    /// Actionable error for a server that cannot be reached at all.
    fn unreachable(&self, source: reqwest::Error) -> anyhow::Error {
        anyhow!(
            "cannot reach llama-server at {} — is the server running? Start it with \
             `llama-server -m <model.gguf> --port 8080` (or check the provider's `base_url` \
             in ~/.wizard/config.toml). Cause: {source}",
            self.base_url
        )
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
                 `llama-server -m <model.gguf> --port 8080`",
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
            return Err(anyhow!(
                "llama-server at {} is still loading its model (HTTP 503) — try again shortly",
                self.base_url
            ));
        }
        let body = response.text().await.unwrap_or_default();
        Err(anyhow!(
            "llama-server at {} returned HTTP {status}: {body}",
            self.base_url
        ))
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

    fn label(&self) -> String {
        format!("llama.cpp:{}", self.model)
    }
}

#[cfg(test)]
mod tests {
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
    async fn health_failure_is_actionable() {
        // Port 1 on localhost: connection refused immediately, no server needed.
        let provider = LlamaCppProvider::new("http://127.0.0.1:1", "m");
        let err = provider.health().await.expect_err("must fail");
        let message = err.to_string();
        assert!(message.contains("http://127.0.0.1:1"), "got: {message}");
        assert!(message.contains("llama-server -m"), "got: {message}");
    }
}
