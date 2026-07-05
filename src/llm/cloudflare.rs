//! Cloudflare Workers AI: serverless inference for open models (GLM, Llama,
//! Qwen, ...) behind an OpenAI-compatible Chat Completions endpoint scoped to a
//! Cloudflare account and authenticated with an API token.
//!
//! Chat uses the OpenAI wire shape, handled by [`super::openai::OpenAiProvider`].
//! This module wraps that client to override the health probe and model listing:
//! Workers AI's OpenAI-compatible surface exposes only `/v1/chat/completions`
//! (there is no `/v1/models`), so reachability/auth and model discovery instead
//! use Cloudflare's native account-scoped catalog (`/ai/models/search`).

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use super::openai::{OpenAiProvider, StaticToken};
use super::provider::LlmProvider;
use super::{ChatRequest, ChatStream, ProviderError};

/// Default model: GLM 5.2 (Z.ai), the most capable text model in the Workers
/// AI catalog.
pub const DEFAULT_MODEL: &str = "@cf/zai-org/glm-5.2";
/// Default env var holding the Cloudflare API token.
pub const DEFAULT_KEY_ENV: &str = "CLOUDFLARE_API_TOKEN";
/// Placeholder in [`BASE_URL_TEMPLATE`] replaced by the account id.
pub const ACCOUNT_ID_PLACEHOLDER: &str = "{account_id}";
/// Base URL template for the OpenAI-compatible endpoint; the placeholder is
/// substituted with the user's Cloudflare account id (see [`base_url`]).
pub const BASE_URL_TEMPLATE: &str =
    "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1";

/// Curated fallback list of Workers AI text-generation models, used only when
/// the live catalog query fails. GLM 5.2 leads (the default).
const FALLBACK_MODELS: &[&str] = &[
    "@cf/zai-org/glm-5.2",
    "@cf/zai-org/glm-4.7-flash",
    "@cf/meta/llama-3.3-70b-instruct-fp8-fast",
    "@cf/qwen/qwen2.5-coder-32b-instruct",
];

/// Build the OpenAI-compatible base URL for `account_id` by substituting it
/// into [`BASE_URL_TEMPLATE`].
pub fn base_url(account_id: &str) -> String {
    BASE_URL_TEMPLATE.replace(ACCOUNT_ID_PLACEHOLDER, account_id.trim())
}

/// Cloudflare Workers AI client. Chat requests are delegated to the wrapped
/// [`OpenAiProvider`]; health and model discovery hit the account-scoped native
/// catalog endpoint instead of the (nonexistent) OpenAI `/v1/models`.
#[derive(Debug, Clone)]
pub struct CloudflareProvider {
    /// Handles the OpenAI-compatible `/chat/completions` wire protocol.
    inner: OpenAiProvider,
    http: reqwest::Client,
    /// Account-scoped catalog base, e.g.
    /// `https://api.cloudflare.com/client/v4/accounts/<id>/ai` (the OpenAI
    /// base URL with the trailing `/v1` stripped).
    account_base: String,
    /// API token; empty means none configured (health will then 401).
    api_key: String,
    /// Model tag, for [`LlmProvider::label`].
    model: String,
}

impl CloudflareProvider {
    /// Build a client for `base_url` (the OpenAI-compatible endpoint, ending in
    /// `/ai/v1`) authenticated with the Cloudflare API token `api_key`.
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let model = model.into();
        let api_key = api_key.into();
        let inner = OpenAiProvider::with_token_source(
            base_url.clone(),
            model.clone(),
            Arc::new(StaticToken::new(api_key.clone())),
            "cloudflare",
        );
        let account_base = base_url
            .strip_suffix("/v1")
            .unwrap_or(&base_url)
            .trim_end_matches('/')
            .to_string();
        Self {
            inner,
            http: reqwest::Client::builder().build().unwrap_or_default(),
            account_base,
            api_key,
            model,
        }
    }

    /// The native model-catalog endpoint for this account.
    fn models_search_url(&self) -> String {
        format!("{}/models/search", self.account_base)
    }

    /// GET the account's text-generation model catalog. Errors (transport,
    /// non-2xx, rejected token) propagate so [`list_models`](Self::list_models)
    /// can fall back to the curated list.
    async fn fetch_models(&self) -> Result<Vec<String>> {
        let mut request = self
            .http
            .get(self.models_search_url())
            .query(&[("task", "Text Generation"), ("per_page", "100")]);
        if !self.api_key.is_empty() {
            request = request.bearer_auth(&self.api_key);
        }
        let response = request.send().await.map_err(|source| {
            anyhow::Error::new(ProviderError::transport(format!(
                "HTTP request to {} failed: {source}",
                self.account_base
            )))
        })?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow::Error::new(ProviderError::http(
                status.as_u16(),
                format!(
                    "{} returned HTTP {status}: {body}",
                    self.models_search_url()
                ),
            )));
        }
        let parsed: ModelSearch = response
            .json()
            .await
            .context("parsing Cloudflare model catalog")?;
        Ok(parsed
            .result
            .into_iter()
            .map(|model| model.name)
            .filter(|name| !name.is_empty())
            .collect())
    }
}

#[async_trait]
impl LlmProvider for CloudflareProvider {
    async fn health(&self) -> Result<()> {
        // Workers AI has no OpenAI-style `/v1/models`, so probe the native
        // account catalog. A 401/403 is a real auth failure (bad or missing
        // token, or wrong Workers-AI permissions); any other response means we
        // reached Cloudflare and the token was not rejected, which is enough to
        // consider the backend healthy without spending inference tokens.
        let mut request = self.http.get(self.models_search_url());
        if !self.api_key.is_empty() {
            request = request.bearer_auth(&self.api_key);
        }
        let response = request.send().await.map_err(|source| {
            anyhow::Error::new(ProviderError::transport(format!(
                "cannot reach {}: {source}",
                self.account_base
            )))
        })?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow::Error::new(ProviderError::http(
                status.as_u16(),
                format!(
                    "Cloudflare rejected the API token (HTTP {status}): {body}; \
                     set {DEFAULT_KEY_ENV} and check the account id in the base URL"
                ),
            )));
        }
        Ok(())
    }

    async fn supports_native_tools(&self, model: &str) -> Result<bool> {
        // OpenAI-compatible endpoint: structured tool calling is supported
        // (GLM 5.2 and the other catalog models call tools natively).
        self.inner.supports_native_tools(model).await
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        match self.fetch_models().await {
            Ok(models) if !models.is_empty() => Ok(models),
            Ok(_) => Ok(fallback_models()),
            Err(err) => {
                tracing::warn!(
                    "Cloudflare model catalog query failed: {err:#}; using the curated fallback list"
                );
                Ok(fallback_models())
            }
        }
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
        self.inner.chat_stream(request).await
    }

    async fn context_window(&self, model: &str) -> Option<u32> {
        context_window(model)
    }

    fn label(&self) -> String {
        format!("cloudflare:{}", self.model)
    }
}

/// The curated fallback model list as owned strings.
fn fallback_models() -> Vec<String> {
    FALLBACK_MODELS.iter().map(|m| (*m).to_string()).collect()
}

/// Context windows for the common Workers AI text models. Deliberately
/// conservative — underestimating only makes history compaction engage a little
/// earlier, whereas overestimating risks overflowing the real window. Unknown
/// tags report `None` so compaction falls back to the byte threshold.
fn context_window(model: &str) -> Option<u32> {
    let model = model.to_ascii_lowercase();
    if model.contains("glm-5") || model.contains("glm-4") {
        return Some(128_000);
    }
    if model.contains("llama-3.3") || model.contains("llama-3.1") {
        return Some(128_000);
    }
    if model.contains("qwen") {
        return Some(32_768);
    }
    None
}

/// Envelope of `GET /ai/models/search` (Cloudflare v4 API — subset).
#[derive(Debug, Deserialize)]
struct ModelSearch {
    #[serde(default)]
    result: Vec<ModelInfo>,
}

/// One entry in the model catalog (subset — only the `@cf/...` name is used).
#[derive(Debug, Deserialize)]
struct ModelInfo {
    #[serde(default)]
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_docs() {
        assert_eq!(DEFAULT_MODEL, "@cf/zai-org/glm-5.2");
        assert_eq!(DEFAULT_KEY_ENV, "CLOUDFLARE_API_TOKEN");
        assert_eq!(
            BASE_URL_TEMPLATE,
            "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1"
        );
    }

    #[test]
    fn base_url_substitutes_the_account_id() {
        assert_eq!(
            base_url("abc123"),
            "https://api.cloudflare.com/client/v4/accounts/abc123/ai/v1"
        );
        // Surrounding whitespace (from a pasted answer) is trimmed.
        assert_eq!(
            base_url("  abc123  "),
            "https://api.cloudflare.com/client/v4/accounts/abc123/ai/v1"
        );
    }

    #[test]
    fn account_base_strips_the_v1_suffix_for_the_catalog_url() {
        let provider = CloudflareProvider::new(base_url("acc"), DEFAULT_MODEL, "token");
        assert_eq!(
            provider.models_search_url(),
            "https://api.cloudflare.com/client/v4/accounts/acc/ai/models/search"
        );
        // Chat still targets the OpenAI-compatible `/v1` endpoint.
        assert_eq!(provider.label(), "cloudflare:@cf/zai-org/glm-5.2");
    }

    #[test]
    fn account_base_tolerates_a_trailing_slash() {
        let provider = CloudflareProvider::new(
            "https://api.cloudflare.com/client/v4/accounts/acc/ai/v1/",
            DEFAULT_MODEL,
            "token",
        );
        assert_eq!(
            provider.models_search_url(),
            "https://api.cloudflare.com/client/v4/accounts/acc/ai/models/search"
        );
    }

    #[test]
    fn context_window_covers_glm_and_unknowns() {
        assert_eq!(context_window("@cf/zai-org/glm-5.2"), Some(128_000));
        assert_eq!(context_window("@cf/zai-org/glm-4.7-flash"), Some(128_000));
        assert_eq!(
            context_window("@cf/meta/llama-3.3-70b-instruct-fp8-fast"),
            Some(128_000)
        );
        assert_eq!(
            context_window("@cf/qwen/qwen2.5-coder-32b-instruct"),
            Some(32_768)
        );
        assert_eq!(context_window("@cf/mistral/mistral-7b-instruct"), None);
    }

    #[test]
    fn model_catalog_response_parses_and_filters_blanks() {
        let raw = r#"{
            "success": true,
            "result": [
                { "name": "@cf/zai-org/glm-5.2", "task": { "name": "Text Generation" } },
                { "name": "" },
                { "name": "@cf/meta/llama-3.3-70b-instruct-fp8-fast" }
            ]
        }"#;
        let parsed: ModelSearch = serde_json::from_str(raw).expect("valid catalog json");
        let names: Vec<String> = parsed
            .result
            .into_iter()
            .map(|m| m.name)
            .filter(|n| !n.is_empty())
            .collect();
        assert_eq!(
            names,
            vec![
                "@cf/zai-org/glm-5.2".to_string(),
                "@cf/meta/llama-3.3-70b-instruct-fp8-fast".to_string()
            ]
        );
    }

    #[test]
    fn fallback_list_leads_with_glm_5_2() {
        assert_eq!(fallback_models()[0], DEFAULT_MODEL);
    }
}
