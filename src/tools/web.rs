//! Native web tools: `web_fetch` (URL → markdown/text) and `web_search`
//! (pluggable search backends).
//!
//! Both are [`ToolAccess::ReadOnly`], so they stay available in plan mode.
//! Settings live in `[web]` in `config.toml` (see
//! [`WebConfig`](crate::config::WebConfig)), carried into the tools via
//! [`ToolContext::web`](super::ToolContext). Fetches are SSRF-guarded:
//! requests to localhost and private/link-local ranges are rejected unless
//! `allow_local = true`. Search API keys are read from the environment at
//! call time and never stored.

use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    MAX_OUTPUT_BYTES, Tool, ToolAccess, ToolContext, ToolError, ToolOutput, parse_args,
    truncate_output,
};
use crate::llm::openai::TokenSource;
use crate::llm::xai_oauth::{self, XaiTokenSource};

/// Whole-request timeout for fetches and searches.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Desktop browser user agent (some sites block obvious bots outright).
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                          Chrome/124.0.0.0 Safari/537.36";

/// Default number of search results.
const DEFAULT_SEARCH_COUNT: usize = 5;

/// Hard cap on requested search results.
const MAX_SEARCH_COUNT: usize = 10;

// ---------------------------------------------------------------------------
// SSRF guard
// ---------------------------------------------------------------------------

/// Whether an address is local/private: loopback (127.0.0.0/8, ::1), RFC1918
/// (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16), link-local (169.254.0.0/16,
/// fe80::/10), unique-local (fc00::/7), or unspecified.
fn ip_is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return ip_is_local(IpAddr::V4(mapped));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link local fe80::/10
        }
    }
}

/// Whether a hostname is a local name: `localhost` or `*.local` (mDNS).
fn host_is_local_name(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    host.eq_ignore_ascii_case("localhost")
        || (host.len() >= 6 && host[host.len() - 6..].eq_ignore_ascii_case(".local"))
}

/// Synchronous URL checks: scheme, literal IPs, and local hostnames. Used
/// before the request and inside the redirect policy (which cannot resolve
/// DNS asynchronously).
fn check_url_sync(url: &reqwest::Url, allow_local: bool) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "unsupported URL scheme '{other}' (only http and https are allowed)"
            ));
        }
    }
    let Some(host) = url.host_str() else {
        return Err("URL has no host".to_string());
    };
    if allow_local {
        return Ok(());
    }
    let blocked = format!(
        "fetching local/private address '{host}' is blocked \
         (set [web] allow_local = true in config.toml to permit)"
    );
    // Literal IPs (IPv6 literals come bracketed in URLs).
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        if ip_is_local(ip) {
            return Err(blocked);
        }
        return Ok(());
    }
    if host_is_local_name(host) {
        return Err(blocked);
    }
    Ok(())
}

/// Full SSRF check: the synchronous checks plus DNS resolution of domain
/// hosts, rejecting any URL whose host resolves to a local/private address.
async fn check_url(url: &reqwest::Url, allow_local: bool) -> Result<(), String> {
    check_url_sync(url, allow_local)?;
    if allow_local {
        return Ok(());
    }
    let Some(host) = url.host_str() else {
        return Err("URL has no host".to_string());
    };
    // Literal IPs were already checked synchronously.
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|err| format!("could not resolve host '{host}': {err}"))?;
    for addr in addrs {
        if ip_is_local(addr.ip()) {
            return Err(format!(
                "host '{host}' resolves to local/private address {} — blocked \
                 (set [web] allow_local = true in config.toml to permit)",
                addr.ip()
            ));
        }
    }
    Ok(())
}

/// HTTP client for the web tools: desktop UA, 30s timeout, and a redirect
/// policy that re-applies the synchronous SSRF checks on every hop.
fn web_client(allow_local: bool) -> Result<reqwest::Client, reqwest::Error> {
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() > 10 {
            return attempt.error("too many redirects");
        }
        match check_url_sync(attempt.url(), allow_local) {
            Ok(()) => attempt.follow(),
            Err(reason) => attempt.error(reason),
        }
    });
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .redirect(policy)
        .timeout(FETCH_TIMEOUT)
        .connect_timeout(Duration::from_secs(10))
        .build()
}

// ---------------------------------------------------------------------------
// web_fetch
// ---------------------------------------------------------------------------

/// Arguments for [`WebFetchTool`].
#[derive(Debug, Deserialize)]
struct FetchArgs {
    url: String,
    /// Response byte cap; clamped to `[web] fetch_max_bytes`.
    #[serde(default)]
    max_bytes: Option<usize>,
}

/// `web_fetch` — fetch a URL and return its content, HTML as markdown.
pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL over HTTP(S) and return its content. HTML pages are converted to \
         markdown; other text content is returned as-is. Responses are size-capped."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The http(s) URL to fetch" },
                "max_bytes": { "type": "integer", "description": "Cap on response bytes read (default and ceiling from config)" }
            },
            "required": ["url"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: FetchArgs = parse_args(self.name(), args)?;
        let url = reqwest::Url::parse(args.url.trim()).map_err(|err| ToolError::InvalidArgs {
            tool: self.name().to_string(),
            message: format!("invalid url '{}': {err}", args.url),
        })?;

        let allow_local = ctx.web.allow_local;
        if let Err(reason) = check_url(&url, allow_local).await {
            return Ok(ToolOutput::error(reason));
        }

        let client = web_client(allow_local).map_err(|err| ToolError::Execution {
            tool: self.name().to_string(),
            source: anyhow::Error::new(err).context("building HTTP client"),
        })?;

        let response = match client.get(url.clone()).send().await {
            Ok(response) => response,
            Err(err) => return Ok(ToolOutput::error(format!("fetch failed: {err}"))),
        };

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();

        let cap = args
            .max_bytes
            .unwrap_or(ctx.web.fetch_max_bytes)
            .min(ctx.web.fetch_max_bytes)
            .max(1);
        let (body, capped) = match read_capped(response, cap).await {
            Ok(read) => read,
            Err(err) => return Ok(ToolOutput::error(format!("reading response failed: {err}"))),
        };

        if !status.is_success() {
            let snippet = truncate_output(String::from_utf8_lossy(&body).into_owned(), 1_000);
            return Ok(ToolOutput::error(format!(
                "fetch of {url} returned HTTP {status}\n{snippet}"
            )));
        }

        let text = String::from_utf8_lossy(&body).into_owned();
        let mut content = if content_type.contains("html") {
            // HTML → markdown; fall back to the raw HTML if conversion fails.
            htmd::convert(&text).unwrap_or(text)
        } else if is_texty(&content_type) {
            text
        } else {
            return Ok(ToolOutput::ok(format!(
                "(binary content type '{content_type}', {} bytes — not shown)",
                body.len()
            )));
        };

        if capped {
            content.push_str(&format!("\n... [response capped at {cap} bytes]"));
        }
        Ok(ToolOutput::ok(truncate_output(content, MAX_OUTPUT_BYTES)))
    }
}

/// Whether a content type is textual enough to return verbatim. An absent
/// content type is treated as text.
fn is_texty(content_type: &str) -> bool {
    content_type.is_empty()
        || content_type.starts_with("text/")
        || content_type.contains("json")
        || content_type.contains("xml")
        || content_type.contains("javascript")
        || content_type.contains("yaml")
        || content_type.contains("toml")
        || content_type.contains("x-www-form-urlencoded")
}

/// Stream a response body, stopping after `cap` bytes. Returns the (possibly
/// capped) body and whether the cap cut anything off.
async fn read_capped(
    response: reqwest::Response,
    cap: usize,
) -> Result<(Vec<u8>, bool), reqwest::Error> {
    let mut body: Vec<u8> = Vec::new();
    let mut capped = false;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len() + chunk.len() > cap {
            let room = cap - body.len();
            body.extend_from_slice(&chunk[..room]);
            capped = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, capped))
}

// ---------------------------------------------------------------------------
// web_search
// ---------------------------------------------------------------------------

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// A pluggable `web_search` backend.
#[async_trait]
pub trait SearchBackend: Send + Sync {
    async fn search(&self, query: &str, count: usize) -> anyhow::Result<Vec<SearchResult>>;
}

/// Default backend: scrape the DuckDuckGo HTML endpoint. No API key.
pub struct DuckDuckGoHtml {
    base_url: String,
}

impl DuckDuckGoHtml {
    pub fn new() -> Self {
        Self::with_base_url("https://html.duckduckgo.com/html/")
    }

    /// Point the backend at a different endpoint (tests use a local server).
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

impl Default for DuckDuckGoHtml {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SearchBackend for DuckDuckGoHtml {
    async fn search(&self, query: &str, count: usize) -> anyhow::Result<Vec<SearchResult>> {
        let client = web_client(true)?;
        let response = client
            .get(&self.base_url)
            .query(&[("q", query)])
            .send()
            .await?
            .error_for_status()?;
        let html = response.text().await?;
        Ok(parse_duckduckgo_html(&html, count))
    }
}

/// Parse DuckDuckGo HTML-endpoint results. Kept synchronous and self-
/// contained (scraper's DOM is not `Send`, so it must not live across an
/// await point) and separate for fixture-based unit tests.
fn parse_duckduckgo_html(html: &str, count: usize) -> Vec<SearchResult> {
    let document = scraper::Html::parse_document(html);
    let result_sel = scraper::Selector::parse("div.result").expect("valid selector");
    let title_sel = scraper::Selector::parse("a.result__a").expect("valid selector");
    let snippet_sel = scraper::Selector::parse(".result__snippet").expect("valid selector");

    let mut results = Vec::new();
    for result in document.select(&result_sel) {
        if results.len() >= count {
            break;
        }
        let Some(link) = result.select(&title_sel).next() else {
            continue;
        };
        let title = link.text().collect::<String>().trim().to_string();
        let url = decode_ddg_href(link.value().attr("href").unwrap_or(""));
        if title.is_empty() || url.is_empty() {
            continue;
        }
        let snippet = result
            .select(&snippet_sel)
            .next()
            .map(|node| node.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        results.push(SearchResult {
            title,
            url,
            snippet,
        });
    }
    results
}

/// Unwrap a DuckDuckGo redirect href
/// (`//duckduckgo.com/l/?uddg=<encoded target>&rut=...`) to the real target.
/// Non-redirect hrefs are returned as-is.
fn decode_ddg_href(href: &str) -> String {
    let absolute = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_string()
    };
    if let Ok(url) = reqwest::Url::parse(&absolute) {
        if let Some((_, target)) = url.query_pairs().find(|(key, _)| key == "uddg") {
            return target.into_owned();
        }
        return absolute;
    }
    href.to_string()
}

/// Brave Search API backend (`X-Subscription-Token` key).
pub struct BraveSearch {
    base_url: String,
    api_key: String,
}

impl BraveSearch {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url("https://api.search.brave.com", api_key)
    }

    pub fn with_base_url(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }
}

#[async_trait]
impl SearchBackend for BraveSearch {
    async fn search(&self, query: &str, count: usize) -> anyhow::Result<Vec<SearchResult>> {
        let client = web_client(true)?;
        let response = client
            .get(format!("{}/res/v1/web/search", self.base_url))
            .header("X-Subscription-Token", &self.api_key)
            .header("Accept", "application/json")
            .query(&[("q", query), ("count", &count.to_string())])
            .send()
            .await?
            .error_for_status()?;
        let body: Value = response.json().await?;
        let results = body["web"]["results"]
            .as_array()
            .map(|results| {
                results
                    .iter()
                    .take(count)
                    .filter_map(|hit| {
                        let title = hit["title"].as_str()?.to_string();
                        let url = hit["url"].as_str()?.to_string();
                        let snippet = hit["description"].as_str().unwrap_or("").to_string();
                        Some(SearchResult {
                            title,
                            url,
                            snippet,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(results)
    }
}

/// Tavily Search API backend (key in the JSON request body).
pub struct TavilySearch {
    base_url: String,
    api_key: String,
}

impl TavilySearch {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url("https://api.tavily.com", api_key)
    }

    pub fn with_base_url(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }
}

#[async_trait]
impl SearchBackend for TavilySearch {
    async fn search(&self, query: &str, count: usize) -> anyhow::Result<Vec<SearchResult>> {
        let client = web_client(true)?;
        let response = client
            .post(format!("{}/search", self.base_url))
            .json(&json!({
                "api_key": self.api_key,
                "query": query,
                "max_results": count,
            }))
            .send()
            .await?
            .error_for_status()?;
        let body: Value = response.json().await?;
        let results = body["results"]
            .as_array()
            .map(|results| {
                results
                    .iter()
                    .take(count)
                    .filter_map(|hit| {
                        let title = hit["title"].as_str()?.to_string();
                        let url = hit["url"].as_str()?.to_string();
                        let snippet = hit["content"].as_str().unwrap_or("").to_string();
                        Some(SearchResult {
                            title,
                            url,
                            snippet,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(results)
    }
}

/// How `XaiSearch` authenticates to xAI: the browser OAuth session
/// (`wizard --login xai`) or a plain `XAI_API_KEY`. OAuth is preferred.
enum XaiAuth {
    Oauth(XaiTokenSource),
    ApiKey(String),
}

/// xAI Grok web search via the Responses API server-side `web_search` tool.
///
/// Unlike the scraper/keyed backends this is an agentic call: Grok runs its
/// own search-and-browse loop server-side and returns the synthesized hits.
/// We ask for a strict JSON envelope and fall back to the response's
/// `url_citation` annotations / top-level `citations` when the model adds
/// prose anyway.
pub struct XaiSearch {
    base_url: String,
    model: String,
    auth: XaiAuth,
}

/// Whole-request timeout for an xAI search. Generous: the server-side
/// search-and-browse loop is much slower than a single scrape.
const XAI_SEARCH_TIMEOUT: Duration = Duration::from_secs(120);

/// Whether an xAI OAuth session exists on disk (`wizard --login xai`).
fn xai_signed_in() -> bool {
    xai_oauth::token_path()
        .map(|path| path.exists())
        .unwrap_or(false)
}

impl XaiSearch {
    /// Search using the stored xAI OAuth session.
    fn oauth(source: XaiTokenSource) -> Self {
        Self {
            base_url: xai_oauth::DEFAULT_BASE_URL.to_string(),
            model: xai_oauth::DEFAULT_MODEL.to_string(),
            auth: XaiAuth::Oauth(source),
        }
    }

    /// Search using a plain API key.
    fn api_key(key: impl Into<String>) -> Self {
        Self {
            base_url: xai_oauth::DEFAULT_BASE_URL.to_string(),
            model: xai_oauth::DEFAULT_MODEL.to_string(),
            auth: XaiAuth::ApiKey(key.into()),
        }
    }

    /// Point the backend at a different endpoint (tests use a local server).
    #[cfg(test)]
    fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Resolve the current bearer token (refreshing the OAuth access token
    /// near expiry happens inside the token source).
    async fn bearer(&self) -> anyhow::Result<String> {
        match &self.auth {
            XaiAuth::ApiKey(key) => Ok(key.clone()),
            XaiAuth::Oauth(source) => source.bearer().await?.ok_or_else(|| {
                anyhow::anyhow!("no xAI OAuth token available; run `wizard --login xai`")
            }),
        }
    }

    /// The Responses API request body: a single user turn that hands Grok the
    /// `web_search` tool and constrains it to a JSON-only reply.
    fn request_body(&self, query: &str, count: usize) -> Value {
        json!({
            "model": self.model,
            "input": [{ "role": "user", "content": xai_search_prompt(query, count) }],
            "tools": [{ "type": "web_search" }],
            "include": ["no_inline_citations"],
        })
    }
}

#[async_trait]
impl SearchBackend for XaiSearch {
    async fn search(&self, query: &str, count: usize) -> anyhow::Result<Vec<SearchResult>> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(XAI_SEARCH_TIMEOUT)
            .build()?;
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let body = self.request_body(query, count);

        let mut retried = false;
        let response = loop {
            let token = self.bearer().await?;
            let response = client
                .post(&url)
                .bearer_auth(token)
                .json(&body)
                .send()
                .await?;
            // One forced refresh after a 401, mirroring the chat provider.
            if response.status() == reqwest::StatusCode::UNAUTHORIZED
                && !retried
                && let XaiAuth::Oauth(source) = &self.auth
                && source.refresh_after_unauthorized().await.unwrap_or(false)
            {
                retried = true;
                continue;
            }
            break response.error_for_status()?;
        };

        let payload: Value = response.json().await?;
        // Some errors arrive as HTTP 200 with an `error` envelope.
        if let Some(message) = payload["error"]["message"].as_str() {
            anyhow::bail!("xAI web search error: {message}");
        }
        Ok(parse_xai_results(&payload, count))
    }
}

/// Prompt that pins Grok to a JSON-only result envelope.
fn xai_search_prompt(query: &str, count: usize) -> String {
    format!(
        "Use the web_search tool to find current information for the query below, then \
         respond with ONLY a single JSON object — no prose, no markdown fences, no inline \
         citation links — matching this exact schema:\n\n\
         {{\"results\": [{{\"title\": \"string\", \"url\": \"string\", \"description\": \
         \"1-2 sentence summary\"}}]}}\n\n\
         Return at most {count} results, ordered by relevance, with absolute https:// URLs. \
         If no usable results exist, return {{\"results\": []}}.\n\n\
         Query: {query}"
    )
}

/// Parse an xAI Responses API payload into search hits, in three tiers:
/// 1. the JSON `{"results": [...]}` envelope the model was asked to emit,
/// 2. `url_citation` annotations on the output text, then
/// 3. a top-level `citations` array of bare URLs.
fn parse_xai_results(payload: &Value, count: usize) -> Vec<SearchResult> {
    let mut text = String::new();
    let mut citations: Vec<SearchResult> = Vec::new();
    if let Some(output) = payload["output"].as_array() {
        for item in output {
            let Some(content) = item["content"].as_array() else {
                continue;
            };
            for part in content {
                if let Some(chunk) = part["text"].as_str() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(chunk);
                }
                if let Some(annotations) = part["annotations"].as_array() {
                    for annotation in annotations {
                        if annotation["type"].as_str() != Some("url_citation") {
                            continue;
                        }
                        if let Some(url) = annotation["url"].as_str() {
                            citations.push(SearchResult {
                                title: annotation["title"].as_str().unwrap_or(url).to_string(),
                                url: url.to_string(),
                                snippet: String::new(),
                            });
                        }
                    }
                }
            }
        }
    }

    if let Some(results) = extract_results_json(&text)
        && !results.is_empty()
    {
        return results.into_iter().take(count).collect();
    }
    if !citations.is_empty() {
        return citations.into_iter().take(count).collect();
    }
    if let Some(urls) = payload["citations"].as_array() {
        return urls
            .iter()
            .filter_map(|url| url.as_str())
            .map(|url| SearchResult {
                title: url.to_string(),
                url: url.to_string(),
                snippet: String::new(),
            })
            .take(count)
            .collect();
    }
    Vec::new()
}

/// Pull a `{"results": [...]}` envelope out of model text. Tries the whole
/// string first, then the widest `{...}` span (which transparently strips any
/// surrounding prose or ```json fences).
fn extract_results_json(text: &str) -> Option<Vec<SearchResult>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok().or_else(|| {
        let start = trimmed.find('{')?;
        let end = trimmed.rfind('}')?;
        if end <= start {
            return None;
        }
        serde_json::from_str(&trimmed[start..=end]).ok()
    })?;
    let results = value["results"].as_array()?;
    Some(
        results
            .iter()
            .filter_map(|hit| {
                let title = hit["title"].as_str()?.to_string();
                let url = hit["url"].as_str()?.to_string();
                let snippet = hit["description"].as_str().unwrap_or("").to_string();
                Some(SearchResult {
                    title,
                    url,
                    snippet,
                })
            })
            .collect(),
    )
}

/// Render results as the numbered markdown list fed back to the model.
fn render_results(results: &[SearchResult]) -> String {
    results
        .iter()
        .enumerate()
        .map(|(index, result)| {
            let mut line = format!("{}. [{}]({})", index + 1, result.title, result.url);
            if !result.snippet.is_empty() {
                line.push_str("\n   ");
                line.push_str(&result.snippet);
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Arguments for [`WebSearchTool`].
#[derive(Debug, Deserialize)]
struct SearchArgs {
    query: String,
    /// Number of results (default 5, max 10).
    #[serde(default)]
    count: Option<usize>,
}

/// `web_search` — query the configured search backend.
pub struct WebSearchTool;

impl WebSearchTool {
    /// Build the configured backend, reading any API key from the
    /// environment at call time (keys are never stored).
    fn backend(ctx: &ToolContext) -> Result<Box<dyn SearchBackend>, String> {
        let name = ctx.web.search_backend.trim().to_ascii_lowercase();
        match name.as_str() {
            "" | "duckduckgo" => Ok(Box::new(DuckDuckGoHtml::new())),
            "brave" => Ok(Box::new(BraveSearch::new(Self::api_key(ctx, "brave")?))),
            "tavily" => Ok(Box::new(TavilySearch::new(Self::api_key(ctx, "tavily")?))),
            "xai" | "grok" => Ok(Box::new(Self::xai_backend(ctx)?)),
            // Prefer the xAI session when signed in; otherwise DuckDuckGo.
            "auto" => {
                if xai_signed_in() {
                    Ok(Box::new(Self::xai_backend(ctx)?))
                } else {
                    Ok(Box::new(DuckDuckGoHtml::new()))
                }
            }
            other => Err(format!(
                "unknown [web] search_backend '{other}' \
                 (expected duckduckgo, brave, tavily, xai, or auto)"
            )),
        }
    }

    /// Build the xAI Grok backend: the OAuth session if the user has signed
    /// in, else a plain `XAI_API_KEY` (or the configured key env var).
    fn xai_backend(ctx: &ToolContext) -> Result<XaiSearch, String> {
        if xai_signed_in() {
            let source = XaiTokenSource::new()
                .map_err(|err| format!("opening the xAI OAuth token store: {err:#}"))?;
            return Ok(XaiSearch::oauth(source));
        }
        let env_name = ctx
            .web
            .search_api_key_env
            .as_deref()
            .unwrap_or(xai_oauth::DEFAULT_KEY_ENV);
        match std::env::var(env_name) {
            Ok(key) if !key.trim().is_empty() => Ok(XaiSearch::api_key(key)),
            _ => Err(format!(
                "xAI web search needs auth: run `wizard --login xai` to sign in, \
                 or set ${env_name} to an xAI API key"
            )),
        }
    }

    fn api_key(ctx: &ToolContext, backend: &str) -> Result<String, String> {
        let Some(env_name) = ctx.web.search_api_key_env.as_deref() else {
            return Err(format!(
                "search backend '{backend}' needs an API key: set [web] search_api_key_env \
                 in config.toml to the name of the env var holding it"
            ));
        };
        match std::env::var(env_name) {
            Ok(key) if !key.trim().is_empty() => Ok(key),
            _ => Err(format!(
                "search backend '{backend}' needs an API key, but ${env_name} is unset or empty"
            )),
        }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web and return a numbered list of results (title, url, snippet)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "count": { "type": "integer", "description": "Number of results (default 5, max 10)" }
            },
            "required": ["query"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: SearchArgs = parse_args(self.name(), args)?;
        if args.query.trim().is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "query must not be empty".to_string(),
            });
        }
        let count = args
            .count
            .unwrap_or(DEFAULT_SEARCH_COUNT)
            .clamp(1, MAX_SEARCH_COUNT);

        let backend = match Self::backend(ctx) {
            Ok(backend) => backend,
            Err(reason) => return Ok(ToolOutput::error(reason)),
        };
        let results = match backend.search(args.query.trim(), count).await {
            Ok(results) => results,
            Err(err) => return Ok(ToolOutput::error(format!("search failed: {err:#}"))),
        };
        if results.is_empty() {
            return Ok(ToolOutput::ok("(no results)"));
        }
        Ok(ToolOutput::ok(truncate_output(
            render_results(&results),
            MAX_OUTPUT_BYTES,
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::config::WebConfig;

    // -- SSRF guard -----------------------------------------------------------

    #[test]
    fn local_ips_are_detected() {
        for ip in [
            "127.0.0.1",
            "127.8.8.8",
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "192.168.255.255",
            "169.254.0.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(ip_is_local(ip.parse().unwrap()), "{ip} is local");
        }
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "172.32.0.1",
            "172.15.0.1",
            "2606:4700::1111",
        ] {
            assert!(!ip_is_local(ip.parse().unwrap()), "{ip} is public");
        }
    }

    #[test]
    fn local_hostnames_are_detected() {
        assert!(host_is_local_name("localhost"));
        assert!(host_is_local_name("LOCALHOST"));
        assert!(host_is_local_name("localhost."));
        assert!(host_is_local_name("printer.local"));
        assert!(host_is_local_name("nas.Local"));
        assert!(!host_is_local_name("example.com"));
        assert!(!host_is_local_name("local"));
        assert!(!host_is_local_name("notlocal.com"));
    }

    #[tokio::test]
    async fn check_url_rejects_private_ranges_and_local_names() {
        for url in [
            "http://127.0.0.1/",
            "http://127.0.0.1:8080/path",
            "http://10.1.2.3/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "http://localhost/",
            "http://localhost:3000/",
            "http://printer.local/",
        ] {
            let parsed = reqwest::Url::parse(url).unwrap();
            let err = check_url(&parsed, false).await.expect_err(url);
            assert!(err.contains("blocked"), "{url}: {err}");
        }
    }

    #[tokio::test]
    async fn check_url_allows_local_when_configured() {
        for url in [
            "http://127.0.0.1:8080/",
            "http://localhost/",
            "http://10.0.0.1/",
        ] {
            let parsed = reqwest::Url::parse(url).unwrap();
            check_url(&parsed, true).await.expect(url);
        }
    }

    #[tokio::test]
    async fn check_url_rejects_non_http_schemes_even_when_local_is_allowed() {
        for url in ["ftp://example.com/", "file:///etc/passwd", "gopher://x/"] {
            let parsed = reqwest::Url::parse(url).unwrap();
            for allow_local in [false, true] {
                let err = check_url(&parsed, allow_local)
                    .await
                    .expect_err("non-http scheme rejected");
                assert!(err.contains("unsupported URL scheme"), "{url}: {err}");
            }
        }
    }

    #[tokio::test]
    async fn check_url_allows_public_ip_literals_without_dns() {
        // A public IP literal needs no DNS resolution to pass.
        let parsed = reqwest::Url::parse("http://8.8.8.8/").unwrap();
        check_url(&parsed, false).await.expect("public IP allowed");
    }

    // -- local fixture server -------------------------------------------------

    /// Serve a fixed raw HTTP response on a loopback listener; returns the
    /// bound address. Every connection gets the same response.
    async fn serve(response: String) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let response = response.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = socket.read(&mut buf).await;
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        addr
    }

    fn http_response(content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Context whose `[web]` settings allow loopback fetches.
    fn local_ctx() -> ToolContext {
        ToolContext::new(std::env::temp_dir()).with_web(WebConfig {
            allow_local: true,
            ..WebConfig::default()
        })
    }

    // -- web_fetch ------------------------------------------------------------

    #[tokio::test]
    async fn fetch_converts_html_to_markdown() {
        let addr = serve(http_response(
            "text/html; charset=utf-8",
            "<html><body><h1>Spellbook</h1><p>Read the <a href=\"https://example.com/docs\">docs</a>.</p></body></html>",
        ))
        .await;
        let out = WebFetchTool
            .execute(json!({ "url": format!("http://{addr}/") }), &local_ctx())
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("# Spellbook"), "{}", out.content);
        assert!(
            out.content.contains("[docs](https://example.com/docs)"),
            "{}",
            out.content
        );
        assert!(
            !out.content.contains("<h1>"),
            "no raw html: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn fetch_returns_plain_text_as_is() {
        let addr = serve(http_response("text/plain", "plain payload, not markdown")).await;
        let out = WebFetchTool
            .execute(json!({ "url": format!("http://{addr}/") }), &local_ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, "plain payload, not markdown");
    }

    #[tokio::test]
    async fn fetch_notes_binary_content_instead_of_dumping_it() {
        let addr = serve(http_response("application/octet-stream", "\u{1}\u{2}\u{3}")).await;
        let out = WebFetchTool
            .execute(json!({ "url": format!("http://{addr}/") }), &local_ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            out.content
                .contains("binary content type 'application/octet-stream'"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn fetch_caps_the_response_at_the_configured_bytes() {
        let body = "a".repeat(5_000);
        let addr = serve(http_response("text/plain", &body)).await;
        let ctx = ToolContext::new(std::env::temp_dir()).with_web(WebConfig {
            allow_local: true,
            fetch_max_bytes: 100,
            ..WebConfig::default()
        });
        let out = WebFetchTool
            .execute(json!({ "url": format!("http://{addr}/") }), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            out.content.contains("[response capped at 100 bytes]"),
            "{}",
            out.content
        );
        assert!(out.content.len() < 200, "content stayed small");
    }

    #[tokio::test]
    async fn fetch_max_bytes_arg_is_clamped_to_the_config_cap() {
        let body = "b".repeat(5_000);
        let addr = serve(http_response("text/plain", &body)).await;
        let ctx = ToolContext::new(std::env::temp_dir()).with_web(WebConfig {
            allow_local: true,
            fetch_max_bytes: 100,
            ..WebConfig::default()
        });
        // Asking for more than the config cap still stops at the cap.
        let out = WebFetchTool
            .execute(
                json!({ "url": format!("http://{addr}/"), "max_bytes": 50_000 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            out.content.contains("capped at 100 bytes"),
            "{}",
            out.content
        );

        // Asking for less reads less.
        let out = WebFetchTool
            .execute(
                json!({ "url": format!("http://{addr}/"), "max_bytes": 10 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            out.content.contains("capped at 10 bytes"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn fetch_blocks_loopback_by_default() {
        // No request is made: the guard rejects before connecting, so an
        // unbound port is fine.
        let ctx = ToolContext::new(std::env::temp_dir());
        assert!(!ctx.web.allow_local, "guard on by default");
        let out = WebFetchTool
            .execute(json!({ "url": "http://127.0.0.1:1/" }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("blocked"), "{}", out.content);
    }

    #[tokio::test]
    async fn fetch_reports_http_errors_as_tool_errors() {
        let addr = serve(
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found".to_string(),
        )
        .await;
        let out = WebFetchTool
            .execute(
                json!({ "url": format!("http://{addr}/missing") }),
                &local_ctx(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("HTTP 404"), "{}", out.content);
        assert!(out.content.contains("not found"), "{}", out.content);
    }

    #[tokio::test]
    async fn fetch_rejects_invalid_urls_as_invalid_args() {
        let err = WebFetchTool
            .execute(json!({ "url": "not a url" }), &local_ctx())
            .await
            .expect_err("invalid url");
        assert!(matches!(err, ToolError::InvalidArgs { tool, .. } if tool == "web_fetch"));
    }

    // -- web_search -----------------------------------------------------------

    /// DuckDuckGo-shaped HTML fixture: two results, one with a wrapped
    /// redirect href and one with a plain absolute href.
    const DDG_FIXTURE: &str = r#"<!DOCTYPE html><html><body>
      <div class="serp__results">
        <div class="result results_links results_links_deep web-result">
          <div class="links_main links_deep result__body">
            <h2 class="result__title">
              <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust%2Dlang.org%2F&amp;rut=abc123">Rust Programming Language</a>
            </h2>
            <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust%2Dlang.org%2F&amp;rut=abc123">A language empowering everyone to build reliable software.</a>
          </div>
        </div>
        <div class="result results_links results_links_deep web-result">
          <div class="links_main links_deep result__body">
            <h2 class="result__title">
              <a rel="nofollow" class="result__a" href="https://doc.rust-lang.org/book/">The Rust Book</a>
            </h2>
            <a class="result__snippet" href="https://doc.rust-lang.org/book/">An introductory book about Rust.</a>
          </div>
        </div>
      </div>
    </body></html>"#;

    #[test]
    fn duckduckgo_parser_extracts_results_from_fixture() {
        let results = parse_duckduckgo_html(DDG_FIXTURE, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(
            results[0].url, "https://www.rust-lang.org/",
            "uddg redirect href is unwrapped"
        );
        assert!(results[0].snippet.contains("reliable software"));
        assert_eq!(results[1].title, "The Rust Book");
        assert_eq!(results[1].url, "https://doc.rust-lang.org/book/");
    }

    #[test]
    fn duckduckgo_parser_honors_count_and_empty_input() {
        assert_eq!(parse_duckduckgo_html(DDG_FIXTURE, 1).len(), 1);
        assert!(parse_duckduckgo_html("<html><body>no results</body></html>", 5).is_empty());
    }

    #[test]
    fn ddg_href_decoding_handles_plain_and_wrapped_links() {
        assert_eq!(
            decode_ddg_href("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%20b&rut=x"),
            "https://example.com/a b"
        );
        assert_eq!(
            decode_ddg_href("https://example.com/direct"),
            "https://example.com/direct"
        );
        assert_eq!(decode_ddg_href(""), "");
    }

    #[tokio::test]
    async fn duckduckgo_backend_searches_a_local_fixture_server() {
        let addr = serve(http_response("text/html", DDG_FIXTURE)).await;
        let backend = DuckDuckGoHtml::with_base_url(format!("http://{addr}/html/"));
        let results = backend.search("rust", 5).await.expect("search ok");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
    }

    #[tokio::test]
    async fn brave_backend_parses_the_api_shape() {
        let body = json!({
            "web": { "results": [
                { "title": "Result One", "url": "https://one.example/", "description": "first" },
                { "title": "Result Two", "url": "https://two.example/", "description": "second" }
            ]}
        })
        .to_string();
        let addr = serve(http_response("application/json", &body)).await;
        let backend = BraveSearch::with_base_url(format!("http://{addr}"), "test-key");
        let results = backend.search("anything", 5).await.expect("search ok");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Result One");
        assert_eq!(results[1].snippet, "second");
    }

    #[tokio::test]
    async fn tavily_backend_parses_the_api_shape() {
        let body = json!({
            "results": [
                { "title": "Tavily Hit", "url": "https://hit.example/", "content": "summary text" }
            ]
        })
        .to_string();
        let addr = serve(http_response("application/json", &body)).await;
        let backend = TavilySearch::with_base_url(format!("http://{addr}"), "test-key");
        let results = backend.search("anything", 5).await.expect("search ok");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].snippet, "summary text");
    }

    #[tokio::test]
    async fn xai_backend_extracts_the_json_envelope() {
        // Grok replies with the JSON envelope we asked for inside an
        // output_text part.
        let envelope =
            r#"{"results":[{"title":"Grok 4.3","url":"https://x.ai/","description":"flagship"}]}"#;
        let body = json!({
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": envelope, "annotations": [] }]
            }]
        })
        .to_string();
        let addr = serve(http_response("application/json", &body)).await;
        let backend = XaiSearch::api_key("test-key").with_base_url(format!("http://{addr}"));
        let results = backend.search("grok", 5).await.expect("search ok");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Grok 4.3");
        assert_eq!(results[0].url, "https://x.ai/");
        assert_eq!(results[0].snippet, "flagship");
    }

    #[test]
    fn xai_parser_strips_prose_and_fences_around_the_envelope() {
        let payload = json!({
            "output": [{
                "content": [{
                    "type": "output_text",
                    "text": "Here you go:\n```json\n{\"results\":[{\"title\":\"A\",\"url\":\"https://a.example/\",\"description\":\"d\"}]}\n```",
                    "annotations": []
                }]
            }]
        });
        let results = parse_xai_results(&payload, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://a.example/");
    }

    #[test]
    fn xai_parser_falls_back_to_url_citation_annotations() {
        // No JSON envelope — recover from annotations on the text part.
        let payload = json!({
            "output": [{
                "content": [{
                    "type": "output_text",
                    "text": "Grok rambled without emitting JSON.",
                    "annotations": [
                        { "type": "url_citation", "title": "Cited One", "url": "https://one.example/" },
                        { "type": "url_citation", "url": "https://two.example/" }
                    ]
                }]
            }]
        });
        let results = parse_xai_results(&payload, 5);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Cited One");
        assert_eq!(
            results[1].title, "https://two.example/",
            "url stands in for a missing title"
        );
    }

    #[test]
    fn xai_parser_falls_back_to_top_level_citations() {
        let payload = json!({
            "output": [{ "content": [{ "type": "output_text", "text": "no json, no annotations" }] }],
            "citations": ["https://cite.example/a", "https://cite.example/b"]
        });
        let results = parse_xai_results(&payload, 5);
        assert_eq!(results.len(), 2);
        assert_eq!(results[1].url, "https://cite.example/b");
    }

    #[test]
    fn xai_parser_honors_count_and_empty_results() {
        let payload = json!({
            "output": [{ "content": [{ "type": "output_text", "text": "{\"results\":[]}" }] }]
        });
        assert!(parse_xai_results(&payload, 5).is_empty());
        assert!(parse_xai_results(&json!({}), 5).is_empty());
    }

    #[test]
    fn results_render_as_a_numbered_markdown_list() {
        let rendered = render_results(&[
            SearchResult {
                title: "One".to_string(),
                url: "https://one.example/".to_string(),
                snippet: "first snippet".to_string(),
            },
            SearchResult {
                title: "Two".to_string(),
                url: "https://two.example/".to_string(),
                snippet: String::new(),
            },
        ]);
        assert_eq!(
            rendered,
            "1. [One](https://one.example/)\n   first snippet\n2. [Two](https://two.example/)"
        );
    }

    #[tokio::test]
    async fn search_rejects_empty_queries() {
        let err = WebSearchTool
            .execute(json!({ "query": "  " }), &local_ctx())
            .await
            .expect_err("empty query");
        assert!(matches!(err, ToolError::InvalidArgs { tool, .. } if tool == "web_search"));
    }

    #[tokio::test]
    async fn search_unknown_backend_is_a_tool_error_without_network() {
        let ctx = ToolContext::new(std::env::temp_dir()).with_web(WebConfig {
            search_backend: "askjeeves".to_string(),
            ..WebConfig::default()
        });
        let out = WebSearchTool
            .execute(json!({ "query": "rust" }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content
                .contains("unknown [web] search_backend 'askjeeves'"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn search_keyed_backends_require_a_key_env() {
        // No env var configured at all.
        let ctx = ToolContext::new(std::env::temp_dir()).with_web(WebConfig {
            search_backend: "brave".to_string(),
            search_api_key_env: None,
            ..WebConfig::default()
        });
        let out = WebSearchTool
            .execute(json!({ "query": "rust" }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("search_api_key_env"),
            "{}",
            out.content
        );

        // Env var configured but unset in the environment.
        let ctx = ToolContext::new(std::env::temp_dir()).with_web(WebConfig {
            search_backend: "tavily".to_string(),
            search_api_key_env: Some("WIZARD_TEST_KEY_THAT_DOES_NOT_EXIST".to_string()),
            ..WebConfig::default()
        });
        let out = WebSearchTool
            .execute(json!({ "query": "rust" }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("WIZARD_TEST_KEY_THAT_DOES_NOT_EXIST"),
            "{}",
            out.content
        );
    }
}
