//! The no-op gateway: stands in when no messaging gateway is configured. Any
//! attempt to use it returns an actionable error.

use anyhow::{Result, bail};
use async_trait::async_trait;

use super::{Gateway, Inbound};

/// A gateway that does nothing but report that none is configured.
pub struct NoneGateway;

#[async_trait]
impl Gateway for NoneGateway {
    fn label(&self) -> &str {
        "none"
    }

    async fn poll(&mut self) -> Result<Vec<Inbound>> {
        bail!(
            "no messaging gateway configured — set [gateway] kind = \"telegram\" in \
             ~/.wizard/config.toml (or re-run `wizard --onboard` and pick Telegram), \
             store the bot token under [keys] telegram in ~/.wizard/credentials.toml, \
             then run `wizard --gateway` in your project"
        )
    }

    async fn send(&self, _chat_id: i64, _text: &str) -> Result<()> {
        bail!(
            "no messaging gateway configured — set [gateway] kind = \"telegram\" in \
             ~/.wizard/config.toml (or re-run `wizard --onboard` and pick Telegram), \
             store the bot token under [keys] telegram in ~/.wizard/credentials.toml, \
             then run `wizard --gateway` in your project"
        )
    }
}
