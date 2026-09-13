//! The OpenAI-compatible family, as a plugin: `https://api.openai.com`'s own
//! Chat Completions endpoint, whatever else a user configures as
//! `kind = "openai"`, and [`openrouter`]. Behind `--features provider-openai`.
//!
//! Almost nothing about talking to OpenAI is peculiar to OpenAI. The request
//! shape, the SSE decoding, the bearer-token seam, the retry classification
//! and the model-family field rules are the *protocol*, shared by every
//! adapter in this family, and they live in [`crate::llm::wire`]. This module is
//! what is left when those are taken out, and it is deliberately small: five
//! other providers used to import their protocol from here, which is what
//! stopped any of them from being lifted out on their own. `wire` stayed in
//! core when this file became a plugin, for exactly that reason: five backends
//! build on it, so it is infrastructure and not this plugin's property.
//!
//! What is left is one request field. OpenAI's Chat Completions API accepts a
//! `prompt_cache_key` that routes a turn to the cache the previous turn
//! warmed, and so does xAI's; the key, and the list of endpoints that take it,
//! live in [`crate::llm::wire`] because two plugins now install them. What is
//! this module's is the decision: `kind = "openai"` is also how vLLM, LM
//! Studio, DeepSeek and every `compat.rs` preset is reached, so [`provider`]
//! attaches the key from the configured URL and not from the kind.

pub mod openrouter;

use crate::kernel::{Capability, Ctx, Plugin, PluginManifest};
use crate::llm::registry::{Credentials, ProviderDescriptor, ProviderKind};
use crate::llm::wire::{OpenAiProvider, endpoint_takes_a_cache_key, prompt_cache_key};

/// Build the client for a provider configured as `kind = "openai"`.
///
/// `base_url` is whatever the user configured, and it is often not OpenAI:
/// the `openai` kind is also how vLLM, LM Studio, DeepSeek and the
/// `compat.rs` presets (Groq, together.ai, Gemini) are reached. That is why
/// the prompt cache key is attached from the URL rather than from the kind —
/// the kind says "speaks this wire shape", not "is OpenAI". An xAI base URL
/// configured this way gets the key too, which is right: it is the endpoint
/// that implements the field.
pub fn provider(
    base_url: impl Into<String>,
    model: impl Into<String>,
    api_key: impl Into<String>,
) -> OpenAiProvider {
    let base_url = base_url.into();
    let keyed = endpoint_takes_a_cache_key(&base_url);
    let provider = OpenAiProvider::new(base_url, model, api_key);
    if keyed {
        provider.with_prompt_cache_key(prompt_cache_key)
    } else {
        provider
    }
}

/// How `kind = "openai"` is registered.
///
/// [`Credentials::ApiKey`] with no default env var, because this kind is also
/// how vLLM, LM Studio, DeepSeek and every `compat.rs` preset is reached:
/// there is no one variable to guess at, so an unconfigured `api_key_env`
/// falls through to the stored credential rather than to `OPENAI_API_KEY`.
/// That is what the old `match` arm did by passing `None`, and guessing here
/// would start sending an OpenAI key to a local vLLM.
pub fn descriptor() -> ProviderDescriptor {
    ProviderDescriptor::new(
        ProviderKind::OPENAI,
        "OpenAI-compatible",
        Credentials::ApiKey { default_env: None },
        |config| {
            let key = config.api_key();
            if key.is_empty() {
                config.warn_missing_key("API key", "an env var");
            }
            Ok(std::sync::Arc::new(provider(
                config.base_url.clone(),
                config.model.clone(),
                key,
            )))
        },
    )
}

/// The OpenAI-compatible family as a kernel plugin.
///
/// Two kinds, one plugin, because they are one endpoint shape with two
/// sets of defaults: `openrouter` is `openai` with a fixed base URL and
/// two attribution headers. Splitting them would give the smaller half a
/// cargo feature whose whole content is a `with_headers` call, and would
/// leave a build that had `openrouter` but not the `openai` kind that
/// vLLM, LM Studio, DeepSeek and every `compat.rs` preset are configured
/// as — a combination nobody wants and everybody would have to test.
///
/// The wire machinery itself is [`crate::llm::wire`] and stays in core:
/// five backends build on it, so it is infrastructure rather than this
/// plugin's property. What is left here is the one field that really is
/// OpenAI's — `prompt_cache_key`, which no other endpoint on this wire
/// implements.
///
/// `network` is declared because that is what this plugin does, even though
/// the capability set only gates the Lua host bridge today. A manifest that
/// under-declares is the failure mode worth avoiding: the grant prompt is
/// generated from it.
pub struct OpenAiPlugin {
    manifest: PluginManifest,
}

impl OpenAiPlugin {
    pub fn new() -> Self {
        Self {
            manifest: PluginManifest {
                name: "openai".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: "OpenAI-compatible Chat Completions, and OpenRouter".to_string(),
                capabilities: vec![Capability::Network],
                optional_deps: Vec::new(),
                profiles: vec![
                    "minimal".to_string(),
                    "server".to_string(),
                    "default".to_string(),
                    "full".to_string(),
                ],
            },
        }
    }
}

impl Default for OpenAiPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for OpenAiPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn apply(&self, ctx: &mut Ctx) -> anyhow::Result<()> {
        ctx.provider(descriptor())?;
        ctx.provider(openrouter::descriptor())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ChatMessage, ChatRequest};

    #[test]
    fn the_prompt_cache_key_rides_only_on_endpoints_that_take_one() {
        let request = ChatRequest {
            model: "gpt-4o".to_string(),
            messages: vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("hi"),
            ],
            tools: Vec::new(),
            stream: true,
            options: None,
        };

        // Both endpoints that document the field, reached through this kind.
        // xAI is here because `kind = "openai"` with xAI's base URL is a real
        // way to configure it, and the field belongs to the endpoint.
        for base_url in [
            "https://api.openai.com/v1",
            "https://api.openai.com/v1/",
            "https://api.x.ai/v1",
        ] {
            let hosted = provider(base_url, "gpt-4o", "sk-test");
            let key = hosted.build_request_body(&request)["prompt_cache_key"]
                .as_str()
                .unwrap_or_else(|| panic!("{base_url} takes a cache key"))
                .to_string();
            assert!(key.starts_with("wz-"), "{key}");
        }

        // Everything else in the family degrades to no key rather than
        // putting a field on the wire that is at best ignored: a local
        // OpenAI-compatible server, Cloudflare's endpoint, OpenRouter.
        for base_url in [
            "http://127.0.0.1:1234/v1",
            "https://api.cloudflare.com/client/v4/accounts/acc/ai/v1",
            "https://openrouter.ai/api/v1",
            // Hosts that merely start with one of the real names are not it.
            "https://api.openai.com.example.net/v1",
            "https://api.x.ai.example.net/v1",
        ] {
            let other = provider(base_url, "gpt-4o", "k");
            assert!(
                other
                    .build_request_body(&request)
                    .get("prompt_cache_key")
                    .is_none(),
                "{base_url} must not receive prompt_cache_key"
            );
        }
    }

    /// The seam itself: a shared client nobody handed a key function to sends
    /// no key, even pointed at OpenAI.
    ///
    /// This is what lets each plugin decide. OpenRouter, Cloudflare and
    /// llama.cpp build the client directly and reach this branch, and each of
    /// them documents that it sends no key. Were the client to go back to
    /// sniffing the URL, they would inherit a field the moment one of them was
    /// pointed at a proxy of an endpoint that takes one, and nothing else
    /// would notice.
    #[test]
    fn the_shared_client_alone_sends_no_key() {
        let request = ChatRequest {
            model: "gpt-4o".to_string(),
            messages: vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("hi"),
            ],
            tools: Vec::new(),
            stream: true,
            options: None,
        };
        let bare = OpenAiProvider::new("https://api.openai.com/v1", "gpt-4o", "sk-test");
        assert!(
            bare.build_request_body(&request)
                .get("prompt_cache_key")
                .is_none()
        );
    }
}
