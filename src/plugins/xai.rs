//! xAI (Grok), as a plugin: two kinds — `xai` with a plain API key,
//! `xaioauth` with an account sign-in — behind `--features provider-xai`.
//!
//! # Why this file is forty lines when `xai_oauth.rs` is a thousand
//!
//! Because the transport is not xAI's. Both kinds speak OpenAI-compatible
//! Chat Completions under the `xai` vendor label, so the client is
//! [`crate::llm::wire::OpenAiProvider`] — core infrastructure, shared with
//! five other backends — and all this plugin supplies is which token source
//! to hand it and whether the endpoint takes a `prompt_cache_key`. There is
//! no xAI-shaped protocol to move.
//!
//! # Why the token store and the sign-in stayed in core
//!
//! [`crate::llm::xai_oauth`] holds the OAuth flow, the token file under
//! `~/.wizard`, the refresh-with-a-lock machinery and [`XaiTokenSource`], and
//! it is *core*, not part of this plugin. That reads backwards until you count
//! the callers: five of the six are not this file.
//!
//! * `plugins/web.rs` authenticates xAI's server-side **search** API with that
//!   token, and `web_search` is a core tool that reaches for xAI whatever the
//!   configured chat backend is;
//! * `tools/image.rs` does the same for xAI's **image** API, which is the
//!   default image endpoint even when the active provider is llama.cpp;
//! * `sync.rs` includes the token file in the set it backs up;
//! * onboarding and `app/prompts.rs` ask whether a session exists, to decide
//!   what to offer;
//! * `--login xai`, `/login xai` and the GUI's sign-in sheet drive the flow.
//!
//! So the token store is a credential subsystem that a chat provider happens
//! to be one consumer of, and moving it here would mean a build without
//! `provider-xai` lost web search and image generation — two tools that have
//! nothing to do with which model answers a turn. What is provider-shaped
//! about xAI is exactly what is below: two descriptors saying which credential
//! goes with which kind.
//!
//! A build compiled without this feature still signs in, still searches, still
//! generates images, and answers `kind = "xai"` with the named error.

use std::sync::Arc;

use anyhow::Context;

use crate::kernel::{Capability, Ctx, Plugin, PluginManifest};
use crate::llm::registry::{Credentials, ProviderDescriptor, ProviderKind};
use crate::llm::wire::{OpenAiProvider, StaticToken, endpoint_takes_a_cache_key, prompt_cache_key};
use crate::llm::xai_oauth::{DEFAULT_KEY_ENV, XaiTokenSource};

/// Attach the `prompt_cache_key` field when `base_url` is an endpoint that
/// implements it, which xAI's is.
///
/// xAI's prompt cache is per *server*: the key routes a conversation's turns
/// back to the machine holding its warm prefix, and xAI's own grok-4.6 page
/// says that without it "you often pay full input price on a cache-cold
/// server". At grok-4.6 rates a read is $0.50/Mtok against $2.00 fresh, so on
/// a long agent turn this is most of the input bill.
///
/// Matched on the URL rather than assumed from the kind, because `base_url`
/// is configurable and a `kind = "xai"` pointed at a relay is not xAI. A
/// relay that does not know the field would at best ignore it and at worst
/// reject the request, and losing the routing hint is the cheaper of the two.
fn with_cache_key(provider: OpenAiProvider, base_url: &str) -> OpenAiProvider {
    if endpoint_takes_a_cache_key(base_url) {
        provider.with_prompt_cache_key(prompt_cache_key)
    } else {
        provider
    }
}

/// How `kind = "xai"` is registered — the plain-API-key flavor.
pub fn key_descriptor() -> ProviderDescriptor {
    ProviderDescriptor::new(
        ProviderKind::XAI,
        "xAI",
        Credentials::ApiKey {
            default_env: Some(DEFAULT_KEY_ENV.to_string()),
        },
        |config| {
            Ok(Arc::new(with_cache_key(
                OpenAiProvider::with_token_source(
                    config.base_url.clone(),
                    config.model.clone(),
                    Arc::new(StaticToken::new(config.api_key())),
                    "xai",
                ),
                &config.base_url,
            )))
        },
    )
}

/// How `kind = "xaioauth"` is registered — the account sign-in flavor, whose
/// credential is the token store rather than a key.
pub fn oauth_descriptor() -> ProviderDescriptor {
    ProviderDescriptor::new(
        ProviderKind::XAI_OAUTH,
        "xAI",
        Credentials::Account {
            login: "xai".to_string(),
        },
        |config| {
            let source = XaiTokenSource::new().context("setting up xAI OAuth token storage")?;
            Ok(Arc::new(with_cache_key(
                OpenAiProvider::with_token_source(
                    config.base_url.clone(),
                    config.model.clone(),
                    Arc::new(source),
                    "xai",
                ),
                &config.base_url,
            )))
        },
    )
}

/// xAI as a kernel plugin, registering both of its kinds.
///
/// One plugin rather than two features because the two kinds differ only in
/// where the bearer token comes from: same endpoint, same wire shape, same
/// vendor label, forty lines between them. A `provider-xai-oauth` feature
/// would be a build flag whose entire content is `Credentials::Account`.
pub struct XaiPlugin {
    manifest: PluginManifest,
}

impl XaiPlugin {
    pub fn new() -> Self {
        Self {
            manifest: PluginManifest {
                name: "xai".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: "xAI Grok, by API key or account sign-in".to_string(),
                capabilities: vec![Capability::Network],
                optional_deps: Vec::new(),
                profiles: vec![
                    "server".to_string(),
                    "default".to_string(),
                    "full".to_string(),
                ],
            },
        }
    }
}

impl Default for XaiPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for XaiPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()> {
        ctx.provider(key_descriptor())?;
        ctx.provider(oauth_descriptor())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ChatMessage, ChatRequest};

    /// The whole point of this plugin's one decision: a Grok turn carries the
    /// routing key, and a relay configured as `kind = "xai"` does not.
    #[test]
    fn a_grok_turn_carries_the_cache_key_and_a_relay_does_not() {
        let request = |messages: Vec<ChatMessage>| ChatRequest {
            model: "grok-4.6".to_string(),
            messages,
            tools: Vec::new(),
            stream: true,
            options: None,
        };
        let turn = request(vec![
            ChatMessage::system("You are Wizard."),
            ChatMessage::user("hi"),
        ]);

        let xai = with_cache_key(
            OpenAiProvider::new("https://api.x.ai/v1", "grok-4.6", "k"),
            "https://api.x.ai/v1",
        );
        let key = xai.build_request_body(&turn)["prompt_cache_key"]
            .as_str()
            .expect("xAI's own endpoint gets the key")
            .to_string();
        assert!(key.starts_with("wz-"), "{key}");

        // A second turn of the same conversation routes to the same cache.
        let later = request(vec![
            ChatMessage::system("You are Wizard."),
            ChatMessage::user("hi"),
            ChatMessage::assistant("hello"),
            ChatMessage::user("and now this"),
        ]);
        assert_eq!(xai.build_request_body(&later)["prompt_cache_key"], key);

        for base_url in [
            "https://grok-relay.example.net/v1",
            "http://127.0.0.1:8080/v1",
        ] {
            let relayed = with_cache_key(OpenAiProvider::new(base_url, "grok-4.6", "k"), base_url);
            assert!(
                relayed
                    .build_request_body(&turn)
                    .get("prompt_cache_key")
                    .is_none(),
                "{base_url} is not xAI and must not receive the field"
            );
        }
    }
}
