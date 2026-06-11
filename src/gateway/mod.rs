//! Messaging gateway: expose Wizard over a chat platform so inbound messages
//! drive one autonomous agent turn each and the reply is sent back.
//!
//! The gateway runs as a long-lived headless process (`wizard --gateway`). It
//! builds a single [`Agent`](crate::agent::Agent) in sovereign / auto-approve
//! posture and keeps it for the whole session, so the conversation continues
//! across messages. The transport is abstracted behind the [`Gateway`] trait;
//! [`telegram::Telegram`] is the first concrete backend, and [`none`] is a
//! no-op that errors with an actionable message.

pub mod none;
pub mod telegram;

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::agent::{Agent, AgentEvent, build_headless_agent};
use crate::cli::Cli;
use crate::config::{Config, GatewayKind, Mode};

/// Telegram's hard cap is 4096 UTF-16 code units; stay well under it.
const MAX_MESSAGE_CHARS: usize = 4000;

/// Cap on a single reply before it is split into messages, so a runaway turn
/// cannot flood a chat.
const MAX_REPLY_CHARS: usize = 24_000;

/// An inbound message from a messaging gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    /// Platform chat identifier the message came from (and replies go to).
    pub chat_id: i64,
    /// The message text.
    pub text: String,
}

/// A chat transport: long-poll for inbound messages and send replies. The
/// agent loop and reply formatting are transport-agnostic and live in
/// [`serve`].
#[async_trait]
pub trait Gateway: Send {
    /// Short human label for status output (e.g. `"telegram"`).
    fn label(&self) -> &str;

    /// Block until the next batch of inbound messages arrives. A transient
    /// network error returns `Err`; [`serve`] retries with backoff. The
    /// implementation tracks its own cursor so messages are not reprocessed.
    async fn poll(&mut self) -> Result<Vec<Inbound>>;

    /// Send `text` to `chat_id`. Callers pre-split long replies via
    /// [`split_message`].
    async fn send(&self, chat_id: i64, text: &str) -> Result<()>;
}

/// Whether `chat_id` is allowed: an empty allow-list permits everyone,
/// otherwise the id must be listed explicitly.
pub fn is_authorized(chat_id: i64, allowed: &[i64]) -> bool {
    allowed.is_empty() || allowed.contains(&chat_id)
}

/// Split `text` into chunks of at most `max` characters, preferring to break
/// on line boundaries. A single line longer than `max` is hard-split. The
/// concatenation of the chunks equals `text`; an empty input yields one empty
/// chunk.
pub fn split_message(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();

    for line in text.split_inclusive('\n') {
        // A line that cannot fit on its own is hard-split by characters.
        if line.chars().count() > max {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            for ch in line.chars() {
                if current.chars().count() == max {
                    chunks.push(std::mem::take(&mut current));
                }
                current.push(ch);
            }
            continue;
        }
        if current.chars().count() + line.chars().count() > max {
            chunks.push(std::mem::take(&mut current));
        }
        current.push_str(line);
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

/// Entry point for `wizard --gateway`: dispatch on the configured gateway
/// kind. [`GatewayKind::None`] is an actionable error; otherwise the matching
/// transport is constructed and driven by [`serve`].
pub async fn run(config: Config, _cli: Cli) -> Result<()> {
    let project_root = std::env::current_dir().context("determining project root")?;
    match config.gateway.kind {
        GatewayKind::None => none::NoneGateway.poll().await.map(|_| ()),
        GatewayKind::Telegram => {
            let gateway = telegram::Telegram::connect(&config.gateway)?;
            serve(Box::new(gateway), config, &project_root).await
        }
    }
}

/// Drive a gateway: build one sovereign agent, then loop — poll for inbound
/// messages (retrying network errors with backoff), enforce the allow-list,
/// run one agent turn per message, and send the reply (split into
/// platform-sized chunks). Runs until Ctrl-C.
async fn serve(mut gateway: Box<dyn Gateway>, config: Config, project_root: &Path) -> Result<()> {
    // The gateway is fully autonomous: there is no terminal, so run in
    // sovereign posture.
    let mut agent_config = config.clone();
    agent_config.mode = Mode::Sovereign;
    if agent_config.max_steps < Mode::Sovereign.default_max_steps() {
        agent_config.max_steps = Mode::Sovereign.default_max_steps();
    }

    let mut agent = build_headless_agent(&agent_config, project_root, false)
        .await
        .context("building gateway agent")?;

    // session_start hooks fire once for the whole gateway session.
    fire_session_hooks(&mut agent, true).await;

    let allowed = config.gateway.allowed_chat_ids.clone();
    println!(
        "wizard gateway ({}) — listening for messages (Ctrl-C to stop)",
        gateway.label()
    );

    let mut attempt: u32 = 0;
    loop {
        let inbound = tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                println!("\n[gateway stopped]");
                break;
            }
            result = gateway.poll() => match result {
                Ok(messages) => {
                    attempt = 0;
                    messages
                }
                Err(err) => {
                    let secs = config.retry_max_secs.min(
                        config
                            .retry_base_secs
                            .saturating_mul(2u64.saturating_pow(attempt)),
                    );
                    eprintln!("gateway poll failed ({err:#}); retrying in {secs}s");
                    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                    attempt = attempt.saturating_add(1);
                    continue;
                }
            },
        };

        for message in inbound {
            if !is_authorized(message.chat_id, &allowed) {
                tracing::warn!("rejecting unauthorized chat {}", message.chat_id);
                if let Err(err) = gateway
                    .send(message.chat_id, "unauthorized: this chat is not allowed")
                    .await
                {
                    eprintln!("failed to send rejection: {err:#}");
                }
                continue;
            }

            println!("← [{}] {}", message.chat_id, first_line(&message.text));
            let reply = run_one_turn(&mut agent, &message.text).await;
            for chunk in split_message(&reply, MAX_MESSAGE_CHARS) {
                if let Err(err) = gateway.send(message.chat_id, &chunk).await {
                    eprintln!("failed to send reply to {}: {err:#}", message.chat_id);
                    break;
                }
            }
        }
    }

    fire_session_hooks(&mut agent, false).await;
    Ok(())
}

/// Fire the `session_start` (`start = true`) or `session_end` hooks and
/// print their activity — the gateway has no long-lived event channel.
async fn fire_session_hooks(agent: &mut Agent, start: bool) {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    if start {
        agent.fire_session_start(&tx).await;
    } else {
        agent.fire_session_end(Some(&tx)).await;
    }
    drop(tx);
    while let Some(event) = rx.recv().await {
        if let AgentEvent::HookFired {
            event,
            command,
            outcome,
        } = event
        {
            println!("hook {event}: {outcome} ({command})");
        }
    }
}

/// Run exactly one agent turn against `text` and collect the reply: stream the
/// turn while draining its [`AgentEvent`] channel, concatenating text deltas
/// and noting tool activity. The reply is capped at [`MAX_REPLY_CHARS`].
async fn run_one_turn(agent: &mut Agent, text: &str) -> String {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);

    // Drain events concurrently with the turn: the turn borrows the agent
    // mutably and owns the sender (dropped on completion, which ends the
    // collector); the collector owns the receiver — disjoint borrows.
    let collector = async move {
        let mut reply = String::new();
        let mut tools: Vec<String> = Vec::new();
        let mut error: Option<String> = None;
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::TextDelta(delta) => reply.push_str(&delta),
                AgentEvent::ToolStarted { name, .. } => tools.push(name),
                AgentEvent::Error(message) => error = Some(message),
                _ => {}
            }
        }
        (reply, tools, error)
    };

    let (_done, (mut reply, tools, error)) = tokio::join!(agent.run_turn(text, tx), collector);

    let reply_trimmed = reply.trim();
    if reply_trimmed.is_empty() {
        reply = match (error, tools.is_empty()) {
            (Some(message), _) => format!("(no reply — {message})"),
            (None, false) => format!("(done; ran tools: {})", tools.join(", ")),
            (None, true) => "(done, no reply)".to_string(),
        };
    } else {
        reply = reply_trimmed.to_string();
    }

    if reply.chars().count() > MAX_REPLY_CHARS {
        let truncated: String = reply.chars().take(MAX_REPLY_CHARS).collect();
        reply = format!("{truncated}\n… (reply truncated)");
    }
    reply
}

/// First line of `text`, for terse console logging of inbound messages.
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_allows_all_when_list_empty() {
        assert!(is_authorized(123, &[]));
        assert!(is_authorized(-100, &[]));
    }

    #[test]
    fn authorization_enforces_membership_when_list_set() {
        let allowed = [42, -100123];
        assert!(is_authorized(42, &allowed));
        assert!(is_authorized(-100123, &allowed));
        assert!(!is_authorized(7, &allowed));
    }

    #[test]
    fn split_short_message_is_one_chunk() {
        let chunks = split_message("hello world", 4000);
        assert_eq!(chunks, vec!["hello world".to_string()]);
    }

    #[test]
    fn split_empty_message_yields_one_empty_chunk() {
        assert_eq!(split_message("", 4000), vec![String::new()]);
    }

    #[test]
    fn split_respects_max_and_preserves_content() {
        let text = "line one\nline two\nline three\nline four\n";
        let chunks = split_message(text, 18);
        assert!(chunks.iter().all(|c| c.chars().count() <= 18), "{chunks:?}");
        assert_eq!(chunks.concat(), text, "round-trips losslessly");
    }

    #[test]
    fn split_hard_splits_an_overlong_line() {
        let line = "x".repeat(25);
        let chunks = split_message(&line, 10);
        assert_eq!(chunks, vec!["xxxxxxxxxx", "xxxxxxxxxx", "xxxxx"]);
        assert_eq!(chunks.concat(), line);
    }
}
