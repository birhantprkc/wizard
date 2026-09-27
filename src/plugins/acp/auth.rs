//! `authenticate`: signing in from the client's side of the pipe.
//!
//! An editor on a desktop never needs this, because the user ran `wizard
//! --login` or pasted a key into the first-run screen. An app that runs
//! `wizard acp` on a phone has neither a terminal nor `xdg-open`, so the
//! sign-in has to be something the client can start and see through.
//!
//! Three methods, each advertised only when this build can use what it
//! produces:
//!
//! - `xai-oauth` and `chatgpt-oauth` bind the provider's loopback callback,
//!   send the authorize URL to the client as a [`AUTH_URL`] notification and
//!   to `$BROWSER`, and answer once the browser comes back (at most five
//!   minutes). An xAI session already on disk is reused unless the request
//!   sets `_meta.force`.
//! - `api-key` takes `_meta.provider` (a first-run key-list name such as
//!   `xai`, `claude`, `openai`, `openrouter` or `gemini`) and `_meta.apiKey`,
//!   with optional `_meta.model` and `_meta.baseUrl`. The key goes to
//!   `credentials.toml`, never `config.toml`.
//!
//! Either way the provider becomes the active one in `config.toml`, so the
//! next `session/new` runs on it.

use agent_client_protocol::schema::v1::{AuthMethod, AuthMethodAgent, AuthenticateRequest};
use agent_client_protocol::{self as acp, Client, ConnectionTo, UntypedMessage};
use serde_json::Value;

use crate::config::{Config, ProviderConfig};
use crate::llm::oauth_callback::{self, Cancel};

use super::internal;

/// Sign in with the xAI account behind SuperGrok or X Premium.
pub const XAI_OAUTH: &str = "xai-oauth";
/// Sign in with a ChatGPT subscription.
pub const CHATGPT_OAUTH: &str = "chatgpt-oauth";
/// A provider API key handed over in `_meta`.
pub const API_KEY: &str = "api-key";

/// The extension notification that carries a sign-in URL to the client:
/// `{"methodId": ..., "url": ...}`. Sent before the wait, since the wait only
/// ends once somebody opens it.
pub const AUTH_URL: &str = "_wizard/auth_url";

/// The methods `initialize` advertises.
pub fn methods() -> Vec<AuthMethod> {
    let mut methods = Vec::new();
    if crate::llm::registry::installed(&crate::config::ProviderKind::XAI_OAUTH).is_some() {
        methods.push(method(
            XAI_OAUTH,
            "Sign in with xAI",
            "Opens the xAI sign-in page ($BROWSER, and a _wizard/auth_url notification) \
             and waits for its redirect to 127.0.0.1.",
        ));
    }
    #[cfg(feature = "provider-chatgpt")]
    methods.push(method(
        CHATGPT_OAUTH,
        "Sign in with ChatGPT",
        "Opens the ChatGPT sign-in page ($BROWSER, and a _wizard/auth_url notification) \
         and waits for its redirect to localhost.",
    ));
    methods.push(method(
        API_KEY,
        "API key",
        "Pass _meta.provider (xai, claude, openai, openrouter, gemini, ...) and \
         _meta.apiKey; _meta.model and _meta.baseUrl are optional.",
    ));
    methods
}

fn method(id: &'static str, name: &str, description: &str) -> AuthMethod {
    AuthMethod::Agent(AuthMethodAgent::new(id, name).description(description.to_string()))
}

/// Run the sign-in `args` asks for and return `config` with its provider
/// added and made active, already saved.
///
/// `cancel` ends an OAuth wait early; the server fires it when a newer
/// `authenticate` replaces this one, so a retry is not locked out of the
/// callback port for five minutes.
pub async fn authenticate(
    args: &AuthenticateRequest,
    connection: &ConnectionTo<Client>,
    config: &Config,
    cancel: Cancel,
) -> Result<Config, acp::Error> {
    let meta = args.meta.clone().unwrap_or_default();
    let provider = match args.method_id.0.as_ref() {
        XAI_OAUTH => xai(&meta, connection, cancel).await?,
        #[cfg(feature = "provider-chatgpt")]
        CHATGPT_OAUTH => chatgpt(connection, cancel).await?,
        API_KEY => api_key(&meta)?,
        other => {
            return Err(acp::Error::invalid_params().data(format!("unknown auth method {other}")));
        }
    };
    let mut config = config.clone();
    adopt(&mut config, provider);
    config.save().map_err(internal)?;
    Ok(config)
}

async fn xai(
    meta: &serde_json::Map<String, Value>,
    connection: &ConnectionTo<Client>,
    cancel: Cancel,
) -> Result<ProviderConfig, acp::Error> {
    use crate::llm::xai_oauth;
    let force = meta.get("force").and_then(Value::as_bool).unwrap_or(false);
    if force || !xai_oauth::signed_in() {
        let pending = xai_oauth::begin_browser_login().await.map_err(internal)?;
        announce(connection, XAI_OAUTH, &pending.authorize_url)?;
        xai_oauth::wait_and_complete(pending, cancel)
            .await
            .map_err(internal)?;
    }
    Ok(xai_oauth::provider_config())
}

#[cfg(feature = "provider-chatgpt")]
async fn chatgpt(
    connection: &ConnectionTo<Client>,
    cancel: Cancel,
) -> Result<ProviderConfig, acp::Error> {
    use crate::plugins::chatgpt::oauth;
    let pending = oauth::begin_login().map_err(internal)?;
    announce(connection, CHATGPT_OAUTH, &pending.authorize_url)?;
    oauth::wait_and_complete(pending, cancel)
        .await
        .map_err(internal)?;
    Ok(oauth::provider_config())
}

/// Hand the sign-in URL to the client and to `$BROWSER`.
fn announce(connection: &ConnectionTo<Client>, method: &str, url: &str) -> Result<(), acp::Error> {
    let params = serde_json::json!({ "methodId": method, "url": url });
    connection.send_notification(UntypedMessage::new(AUTH_URL, params)?)?;
    oauth_callback::open_browser(url);
    Ok(())
}

/// The `api-key` method: find the provider, store the key under its name.
fn api_key(meta: &serde_json::Map<String, Value>) -> Result<ProviderConfig, acp::Error> {
    let provider = api_key_provider(meta)?;
    let key = meta_str(meta, "apiKey").unwrap_or_default();
    crate::credentials::store(&provider.name, key).map_err(internal)?;
    Ok(provider)
}

/// The provider entry an `api-key` request describes, without storing
/// anything.
fn api_key_provider(meta: &serde_json::Map<String, Value>) -> Result<ProviderConfig, acp::Error> {
    let name = meta_str(meta, "provider")
        .ok_or_else(|| acp::Error::invalid_params().data("api-key needs _meta.provider"))?;
    if meta_str(meta, "apiKey").is_none() {
        return Err(acp::Error::invalid_params().data("api-key needs _meta.apiKey"));
    }
    let mut provider = crate::onboarding::keyed_provider(name).ok_or_else(|| {
        acp::Error::invalid_params().data(format!("no API-key provider named {name} in this build"))
    })?;
    if let Some(model) = meta_str(meta, "model") {
        provider.model = model.to_string();
    }
    if let Some(base_url) = meta_str(meta, "baseUrl") {
        provider.base_url = base_url.to_string();
    }
    Ok(provider)
}

fn meta_str<'a>(meta: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    meta.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Make `provider` the active one, replacing a same-named entry.
fn adopt(config: &mut Config, provider: ProviderConfig) {
    config.providers.retain(|p| p.name != provider.name);
    config.active_provider = Some(provider.name.clone());
    config.providers.push(provider);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(value: Value) -> serde_json::Map<String, Value> {
        value.as_object().expect("an object").clone()
    }

    #[test]
    fn every_build_offers_an_api_key() {
        let ids: Vec<String> = methods().iter().map(|m| m.id().to_string()).collect();
        assert!(ids.contains(&API_KEY.to_string()), "{ids:?}");
    }

    #[test]
    fn an_api_key_request_needs_a_provider_and_a_key() {
        let missing_key = api_key_provider(&meta(serde_json::json!({ "provider": "openai" })));
        assert!(missing_key.is_err());
        let missing_provider = api_key_provider(&meta(serde_json::json!({ "apiKey": "k" })));
        assert!(missing_provider.is_err());
        let unknown = api_key_provider(&meta(
            serde_json::json!({ "provider": "nope", "apiKey": "k" }),
        ));
        assert!(unknown.is_err());
    }

    #[cfg(feature = "provider-openai")]
    #[test]
    fn an_api_key_request_can_point_a_known_provider_elsewhere() {
        let provider = api_key_provider(&meta(serde_json::json!({
            "provider": "openai",
            "apiKey": "k",
            "model": "grok-4.6",
            "baseUrl": "http://10.0.2.2:8790/v1",
        })))
        .expect("openai is a keyed provider");
        assert_eq!(provider.name, "openai");
        assert_eq!(provider.model, "grok-4.6");
        assert_eq!(provider.base_url, "http://10.0.2.2:8790/v1");
        assert_eq!(provider.api_key_env.as_deref(), Some(""));
    }

    #[test]
    fn adopting_a_provider_replaces_its_namesake_and_activates_it() {
        let mut config = Config::default();
        let mut old = crate::llm::xai_oauth::provider_config();
        old.model = "old".to_string();
        config.providers.push(old);
        adopt(&mut config, crate::llm::xai_oauth::provider_config());
        assert_eq!(config.providers.len(), 1);
        assert_ne!(config.providers[0].model, "old");
        assert_eq!(config.active().name, config.providers[0].name);
    }
}
