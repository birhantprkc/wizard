//! Telegram bot gateway: long-poll `getUpdates`, dispatch each inbound text
//! message to one agent turn, and reply via `sendMessage`.
//!
//! The bot token comes from `~/.wizard/credentials.toml` (stored under
//! `telegram`) first, then the env var named in
//! [`GatewayConfig::token_env`](crate::config::GatewayConfig::token_env) (or
//! `WIZARD_TELEGRAM_TOKEN` by default) — the same precedence providers use
//! for API keys, so a gateway launched from cron works without an
//! environment. Create a bot with [@BotFather](https://t.me/BotFather) to
//! obtain a token.

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use super::{Gateway, Inbound};
use crate::config::GatewayConfig;

/// Long-poll timeout (seconds) passed to `getUpdates`. The HTTP client's own
/// timeout is set comfortably above this.
const LONG_POLL_SECS: u64 = 30;

/// A connected Telegram bot. Holds the API base URL (with the token embedded)
/// and the update offset cursor so each update is processed once.
pub struct Telegram {
    http: reqwest::Client,
    /// `https://api.telegram.org/bot<token>` — the token is kept here and
    /// never logged.
    api_base: String,
    /// Next `getUpdates` offset: one past the highest update id seen.
    offset: i64,
}

impl Telegram {
    /// Connect using the stored `telegram` credential, falling back to the
    /// env var named in `config` — provider-key precedence. A missing or
    /// empty token is an actionable error naming both sources.
    pub fn connect(config: &GatewayConfig) -> Result<Self> {
        let env_name = config.token_env();
        let token = crate::credentials::get("telegram")
            .filter(|t| !t.trim().is_empty())
            .or_else(|| {
                std::env::var(env_name)
                    .ok()
                    .filter(|t| !t.trim().is_empty())
            });
        let token = token.with_context(|| {
            format!(
                "Telegram bot token not set: export {env_name}=<token> or store it under \
                 'telegram' in ~/.wizard/credentials.toml (create a bot via @BotFather \
                 to obtain one)"
            )
        })?;

        let http = reqwest::Client::builder()
            // Allow the full long-poll window plus slack before timing out.
            .timeout(Duration::from_secs(LONG_POLL_SECS + 30))
            .build()
            // Builder construction only fails when the TLS backend cannot
            // initialize; fall back to the default client rather than panic.
            .unwrap_or_default();

        Ok(Self {
            http,
            api_base: format!("https://api.telegram.org/bot{}", token.trim()),
            offset: 0,
        })
    }

    fn method_url(&self, method: &str) -> String {
        format!("{}/{method}", self.api_base)
    }
}

#[async_trait]
impl Gateway for Telegram {
    fn label(&self) -> &str {
        "telegram"
    }

    async fn poll(&mut self) -> Result<Vec<Inbound>> {
        let url = self.method_url("getUpdates");
        let response = self
            .http
            .get(&url)
            .query(&[
                ("timeout", LONG_POLL_SECS.to_string()),
                ("offset", self.offset.to_string()),
            ])
            .send()
            .await
            .context("requesting Telegram updates")?
            .error_for_status()
            .context("Telegram getUpdates returned an error status")?;

        let body: GetUpdates = response
            .json()
            .await
            .context("decoding Telegram getUpdates response")?;
        if !body.ok {
            anyhow::bail!("Telegram getUpdates returned ok=false");
        }

        let mut inbound = Vec::new();
        for update in body.result {
            // Advance the cursor for every update, even ones we ignore, so
            // they are not redelivered on the next poll.
            if update.update_id >= self.offset {
                self.offset = update.update_id + 1;
            }
            if let Some(message) = update.message
                && let Some(text) = message.text
            {
                inbound.push(Inbound {
                    chat_id: message.chat.id,
                    text,
                });
            }
        }
        Ok(inbound)
    }

    async fn send(&self, chat_id: i64, text: &str) -> Result<()> {
        let url = self.method_url("sendMessage");
        self.http
            .post(&url)
            .json(&serde_json::json!({ "chat_id": chat_id, "text": text }))
            .send()
            .await
            .context("sending Telegram message")?
            .error_for_status()
            .context("Telegram sendMessage returned an error status")?;
        Ok(())
    }
}

/// Top-level `getUpdates` response.
#[derive(Debug, Deserialize)]
struct GetUpdates {
    ok: bool,
    #[serde(default)]
    result: Vec<Update>,
}

/// One update in a `getUpdates` batch.
#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    #[serde(default)]
    message: Option<Message>,
}

/// A Telegram message (only the fields Wizard uses).
#[derive(Debug, Deserialize)]
struct Message {
    chat: Chat,
    #[serde(default)]
    text: Option<String>,
}

/// The chat a message belongs to.
#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_errors_without_token() {
        // Use a uniquely-named env var that is guaranteed unset.
        let config = GatewayConfig {
            kind: crate::config::GatewayKind::Telegram,
            token_env: Some("WIZARD_TEST_TELEGRAM_TOKEN_ABSENT".to_string()),
            allowed_chat_ids: Vec::new(),
        };
        let err = match Telegram::connect(&config) {
            Ok(_) => panic!("missing token must error"),
            Err(err) => err,
        };
        let message = format!("{err:#}");
        assert!(
            message.contains("WIZARD_TEST_TELEGRAM_TOKEN_ABSENT"),
            "error should name the env var: {message}"
        );
    }

    #[test]
    fn parses_get_updates_payload() {
        let raw = r#"{
            "ok": true,
            "result": [
                {"update_id": 10, "message": {"chat": {"id": 555}, "text": "hi"}},
                {"update_id": 11, "message": {"chat": {"id": 555}}},
                {"update_id": 12}
            ]
        }"#;
        let body: GetUpdates = serde_json::from_str(raw).expect("valid payload");
        assert!(body.ok);
        assert_eq!(body.result.len(), 3);
        let texts: Vec<_> = body
            .result
            .into_iter()
            .filter_map(|u| u.message)
            .filter_map(|m| m.text.map(|t| (m.chat.id, t)))
            .collect();
        assert_eq!(texts, vec![(555, "hi".to_string())]);
    }
}
