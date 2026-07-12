//! Sign in with a ChatGPT subscription (Plus/Pro/Team), rather than a
//! pay-as-you-go API key.
//!
//! A subscription is reached exactly the way OpenAI's own Codex CLI reaches it:
//! OAuth 2.0 Authorization Code + PKCE against `auth.openai.com` using Codex's
//! public client id, and then the **Responses** API at
//! `chatgpt.com/backend-api/codex` — not the Chat Completions API, and not
//! `api.openai.com`. That endpoint only answers to the Codex client, so the
//! requests present as it (`originator: codex_cli_rs`, the Codex client id);
//! [`super::chatgpt`] speaks the protocol, this module supplies the credentials.
//!
//! Tokens live in `~/.wizard/chatgpt_oauth.json` (file 0600), never in
//! `config.toml`. The account id needed on every API call is a claim inside the
//! `id_token` and is stored alongside the tokens.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::xai_oauth::{generate_pkce, jwt_exp};
use crate::config::Config;

/// OAuth authorize endpoint.
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
/// OAuth token + refresh endpoint.
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Codex CLI's public OAuth client id (no secret). The subscription endpoint
/// only issues tokens to this client, so a third-party sign-in must use it.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Scopes: identity plus a refresh token.
const SCOPE: &str = "openid profile email offline_access";
/// The redirect the client is registered with. Fixed — unlike a floating
/// loopback port, this must match what OpenAI has on file, so both the
/// preferred and the fallback port are registered ones and the path is exact.
const CALLBACK_PORT: u16 = 1455;
const FALLBACK_PORT: u16 = 1457;
const CALLBACK_PATH: &str = "/auth/callback";
/// Identifies the client to both the authorize flow and the API.
const ORIGINATOR: &str = "codex_cli_rs";
/// Refresh the access token when its JWT `exp` is within this many seconds.
const EXPIRY_LEEWAY_SECS: i64 = 300;
/// How long the localhost listener waits for the browser to come back.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// The subscription API base (Responses API lives under it).
pub const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// A reasonable default model; the real list comes from `GET {BASE_URL}/models`.
pub const DEFAULT_MODEL: &str = "gpt-5.2";
/// Client identifier sent on every API request.
pub const API_ORIGINATOR: &str = ORIGINATOR;

// ---------------------------------------------------------------------------
// Token storage (~/.wizard/chatgpt_oauth.json)
// ---------------------------------------------------------------------------

/// Persisted OAuth state. Its own 0600 file, never config.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// The identity token; its claims carry the account id and plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    /// `chatgpt_account_id` from the id_token — sent on every API call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

/// `~/.wizard/chatgpt_oauth.json`
pub fn token_path() -> Result<PathBuf> {
    Ok(Config::wizard_dir()?.join("chatgpt_oauth.json"))
}

/// Write tokens atomically at 0600 (temp file in the same dir, then rename).
pub fn save_tokens(path: &Path, tokens: &StoredTokens) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        harden_dir(parent);
    }
    let json = serde_json::to_string_pretty(tokens).context("serializing ChatGPT tokens")?;
    let tmp = path.with_file_name(".chatgpt_oauth.json.tmp");
    write_private(&tmp, json.as_bytes())?;
    std::fs::rename(&tmp, path).with_context(|| format!("saving {}", path.display()))?;
    Ok(())
}

/// Read the stored tokens; `Ok(None)` when the file is absent.
pub fn load_tokens(path: &Path) -> Result<Option<StoredTokens>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Forget the stored tokens (a missing file is not an error).
pub fn clear_tokens(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

#[cfg(unix)]
fn harden_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn harden_dir(_dir: &Path) {}

// ---------------------------------------------------------------------------
// The id_token's account-id claim
// ---------------------------------------------------------------------------

/// Extract `chatgpt_account_id` from the id_token's `https://api.openai.com/auth`
/// claim. `None` when the token is not a parseable JWT or lacks the claim.
pub fn account_id_from_id_token(id_token: &str) -> Option<String> {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let payload = id_token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// The sign-in flow
// ---------------------------------------------------------------------------

/// A sign-in in flight: the URL to send the user to, plus everything needed to
/// finish once the browser comes back. Holds its own bound listener, since the
/// redirect is a fixed address rather than the caller's server.
pub struct PendingLogin {
    pub authorize_url: String,
    state: String,
    redirect_uri: String,
    verifier: String,
    listener: TcpListener,
}

/// Bind the (registered) callback port and build the authorize URL. The
/// listener is held in the returned [`PendingLogin`]; [`wait_and_complete`]
/// consumes it.
pub fn begin_login() -> Result<PendingLogin> {
    let (listener, port) = bind_callback_listener()?;
    let redirect_uri = format!("http://localhost:{port}{CALLBACK_PATH}");

    let pkce = generate_pkce()?;
    let state = random_state()?;

    let mut url = reqwest::Url::parse(AUTHORIZE_URL).context("parsing the authorize URL")?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", &state)
        .append_pair("originator", ORIGINATOR);

    Ok(PendingLogin {
        authorize_url: url.to_string(),
        state,
        redirect_uri,
        verifier: pkce.verifier,
        listener,
    })
}

/// Wait for the browser to hit the callback, exchange the code, and persist the
/// tokens. Consumes the pending login (and its listener).
pub async fn wait_and_complete(pending: PendingLogin) -> Result<StoredTokens> {
    let PendingLogin {
        state,
        redirect_uri,
        verifier,
        listener,
        ..
    } = pending;
    let expected = state.clone();
    let code = tokio::task::spawn_blocking(move || wait_for_callback(listener, &expected))
        .await
        .context("callback listener task panicked")??;

    let token = exchange_code(&code, &redirect_uri, &verifier).await?;
    let account_id = token.id_token.as_deref().and_then(account_id_from_id_token);
    let stored = StoredTokens {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        id_token: token.id_token,
        account_id,
    };
    save_tokens(&token_path()?, &stored)?;
    Ok(stored)
}

/// The provider entry a completed sign-in earns.
pub fn provider_config() -> crate::config::ProviderConfig {
    crate::config::ProviderConfig {
        name: "chatgpt".to_string(),
        kind: crate::config::ProviderKind::ChatgptOauth,
        base_url: BASE_URL.to_string(),
        model: DEFAULT_MODEL.to_string(),
        api_key_env: None,
        gguf_path: None,
        usd_per_mtok_in: None,
        usd_per_mtok_out: None,
    }
}

/// The self-contained terminal flow (`wizard --login chatgpt`): open the
/// browser, wait, exchange. `report` receives progress lines.
pub async fn login<F>(report: F) -> Result<()>
where
    F: Fn(&str) + Send + Sync,
{
    let pending = begin_login()?;
    report(&format!(
        "open this URL to sign in with your ChatGPT account:\n{}",
        pending.authorize_url
    ));
    open_browser(&pending.authorize_url);
    report("waiting for the browser callback (5 minute timeout)...");
    wait_and_complete(pending).await?;
    report(&format!(
        "signed in to ChatGPT; tokens saved to {}",
        token_path()?.display()
    ));
    Ok(())
}

/// The token endpoint's response (from both the code exchange and refresh).
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
}

/// Exchange the authorization `code` for tokens (form-encoded, per OAuth).
async fn exchange_code(code: &str, redirect_uri: &str, verifier: &str) -> Result<TokenResponse> {
    let http = reqwest::Client::new();
    let response = http
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .context("exchanging the authorization code")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("ChatGPT token exchange failed (HTTP {status}): {body}");
    }
    response
        .json()
        .await
        .context("parsing the ChatGPT token response")
}

/// Refresh an access token (JSON body, per Codex). Returns the tokens to
/// persist; the caller merges them (a refresh may omit the refresh token).
pub async fn refresh(refresh_token: &str) -> Result<TokenResponse> {
    let http = reqwest::Client::new();
    let response = http
        .post(TOKEN_URL)
        .json(&json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .context("refreshing the ChatGPT token")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("ChatGPT token refresh failed (HTTP {status}): {body}");
    }
    response
        .json()
        .await
        .context("parsing the ChatGPT refresh response")
}

/// True when `access_token` expires within [`EXPIRY_LEEWAY_SECS`]. A token with
/// no readable `exp` is treated as live; the API's 401 path forces a refresh.
pub fn expires_soon(access_token: &str) -> bool {
    match jwt_exp(access_token) {
        Some(exp) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            exp <= now + EXPIRY_LEEWAY_SECS
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Localhost callback listener
// ---------------------------------------------------------------------------

fn random_state() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| anyhow::anyhow!("gathering randomness: {err}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Bind the preferred registered port, then the registered fallback. Both are
/// addresses OpenAI has on file for this client, so either produces a
/// redirect_uri the authorize endpoint will accept.
fn bind_callback_listener() -> Result<(TcpListener, u16)> {
    for port in [CALLBACK_PORT, FALLBACK_PORT] {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) {
            return Ok((listener, port));
        }
    }
    bail!(
        "could not bind the sign-in callback port ({CALLBACK_PORT} or {FALLBACK_PORT}); \
         is another Codex/wizard sign-in already running?"
    )
}

fn open_browser(url: &str) {
    for opener in ["xdg-open", "open"] {
        if std::process::Command::new(opener)
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
        {
            return;
        }
    }
}

enum Callback {
    Code(String),
    Failed(String),
    Ignored,
}

/// Classify a request target (`/auth/callback?code=…&state=…`).
fn parse_callback(target: &str, expected_state: &str) -> Callback {
    let Ok(url) = reqwest::Url::parse(&format!("http://127.0.0.1{target}")) else {
        return Callback::Ignored;
    };
    if url.path() != CALLBACK_PATH {
        return Callback::Ignored;
    }
    let (mut code, mut state, mut error, mut error_desc) = (None, None, None, None);
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => error_desc = Some(value.into_owned()),
            _ => {}
        }
    }
    if let Some(error) = error {
        let detail = error_desc.unwrap_or(error);
        // OpenAI surfaces a missing Codex entitlement here rather than at the API.
        if detail.contains("missing_codex_entitlement") {
            return Callback::Failed(
                "this ChatGPT plan does not include Codex/API access".to_string(),
            );
        }
        return Callback::Failed(format!("OpenAI returned an error: {detail}"));
    }
    if state.as_deref() != Some(expected_state) {
        return Callback::Failed("the sign-in state did not match; aborting".to_string());
    }
    match code {
        Some(code) => Callback::Code(code),
        None => Callback::Failed("the callback carried no authorization code".to_string()),
    }
}

fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<String> {
    listener
        .set_nonblocking(true)
        .context("configuring the callback listener")?;
    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(outcome) = handle_connection(stream, expected_state) {
                    return outcome;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for the browser sign-in (5 minutes)");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => return Err(err).context("accepting the callback connection"),
        }
    }
}

/// Serve one connection: `Some(result)` ends the wait, `None` keeps waiting.
fn handle_connection(mut stream: TcpStream, expected_state: &str) -> Option<Result<String>> {
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).ok()?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let target = request.split_whitespace().nth(1).unwrap_or("/");
    match parse_callback(target, expected_state) {
        Callback::Code(code) => {
            respond(&mut stream, "Signed in to Wizard. You can close this tab.");
            Some(Ok(code))
        }
        Callback::Failed(message) => {
            respond(&mut stream, &format!("Sign-in failed: {message}"));
            Some(Err(anyhow::anyhow!(message)))
        }
        Callback::Ignored => {
            respond(&mut stream, "Waiting for the sign-in…");
            None
        }
    }
}

fn respond(stream: &mut TcpStream, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Wizard</title>\
         <body style=\"background:#0c0c0e;color:#ececee;font:14px system-ui;\
         display:flex;align-items:center;justify-content:center;height:100vh;margin:0\">\
         <p>{message}</p>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn id_token_with(auth_claim: serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let payload = URL_SAFE_NO_PAD.encode(
            json!({ "https://api.openai.com/auth": auth_claim })
                .to_string()
                .as_bytes(),
        );
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn account_id_comes_from_the_auth_claim() {
        let token =
            id_token_with(json!({ "chatgpt_account_id": "acct-123", "chatgpt_plan_type": "pro" }));
        assert_eq!(
            account_id_from_id_token(&token).as_deref(),
            Some("acct-123")
        );
    }

    #[test]
    fn account_id_is_none_without_the_claim() {
        assert_eq!(account_id_from_id_token("not.a.jwt"), None);
        let token = id_token_with(json!({ "chatgpt_plan_type": "pro" }));
        assert_eq!(account_id_from_id_token(&token), None);
    }

    #[test]
    fn callback_requires_matching_state_and_a_code() {
        assert!(matches!(
            parse_callback("/auth/callback?code=c&state=s", "s"),
            Callback::Code(c) if c == "c"
        ));
        assert!(matches!(
            parse_callback("/auth/callback?code=c&state=wrong", "s"),
            Callback::Failed(_)
        ));
        assert!(matches!(
            parse_callback("/auth/callback?state=s", "s"),
            Callback::Failed(_)
        ));
        assert!(matches!(
            parse_callback("/favicon.ico", "s"),
            Callback::Ignored
        ));
    }

    #[test]
    fn a_denied_callback_reports_the_error() {
        assert!(matches!(
            parse_callback("/auth/callback?error=access_denied&error_description=nope", "s"),
            Callback::Failed(m) if m.contains("nope")
        ));
        assert!(matches!(
            parse_callback("/auth/callback?error=x&error_description=missing_codex_entitlement", "s"),
            Callback::Failed(m) if m.contains("Codex")
        ));
    }
}
