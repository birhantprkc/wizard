//! Telegram bot gateway: long-poll `getUpdates`, dispatch each inbound text
//! (or caption / photo / image document) message to one agent turn, and reply
//! via `sendMessage`.
//!
//! The bot token comes from `~/.wizard/credentials.toml` (stored under
//! `telegram`) first, then the env var named in
//! [`GatewayConfig::token_env`](crate::config::GatewayConfig::token_env) (or
//! `WIZARD_TELEGRAM_TOKEN` by default) — the same precedence providers use
//! for API keys, so a gateway launched from cron/systemd works without an
//! environment. Create a bot with [@BotFather](https://t.me/BotFather) to
//! obtain a token. Onboarding stores the token via `credentials::store`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use super::{Gateway, Inbound};
use crate::config::GatewayConfig;

/// Long-poll timeout (seconds) passed to `getUpdates`. The HTTP client's own
/// timeout is set comfortably above this.
const LONG_POLL_SECS: u64 = 30;

/// Placeholder text for a photo/document with no caption so the agent still
/// runs a turn and can open the attached file.
const PHOTO_ONLY_PROMPT: &str = "Please look at the attached image.";

/// Reply sent for message types we do not handle (stickers, voice, etc.).
const UNSUPPORTED_REPLY: &str =
    "unsupported message type — send text, a photo, or an image document";

/// A connected Telegram bot. Holds the API base URL (with the token embedded)
/// and the update offset cursor so each update is processed once.
pub struct Telegram {
    http: reqwest::Client,
    /// `https://api.telegram.org/bot<token>` — the token is kept here and
    /// never logged.
    api_base: String,
    /// Token alone, used to build the file-download URL
    /// (`/file/bot<token>/<path>`). Never logged.
    token: String,
    /// Next `getUpdates` offset: one past the highest update id seen.
    offset: i64,
    /// Directory under which downloaded attachments land
    /// (`~/.wizard/gateway-attachments` or a temp fallback).
    attachments_dir: PathBuf,
}

impl Telegram {
    /// Connect using the stored `telegram` credential, falling back to the
    /// env var named in `config` — provider-key precedence. A missing or
    /// empty token is an actionable error naming both sources and onboarding.
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
                "Telegram bot token not set. Paste it during `wizard --onboard` (Telegram), \
                 store it under [keys] telegram = \"...\" in ~/.wizard/credentials.toml \
                 (mode 0600), or export {env_name}=<token> (create a bot via @BotFather)"
            )
        })?;
        let token = token.trim().to_string();

        let http = reqwest::Client::builder()
            // Allow the full long-poll window plus slack before timing out.
            .timeout(Duration::from_secs(LONG_POLL_SECS + 30))
            .build()
            // Builder construction only fails when the TLS backend cannot
            // initialize; fall back to the default client rather than panic.
            .unwrap_or_default();

        let attachments_dir = attachments_dir();
        if let Err(err) = std::fs::create_dir_all(&attachments_dir) {
            tracing::warn!(
                "could not create attachments dir {}: {err}",
                attachments_dir.display()
            );
        }

        Ok(Self {
            http,
            api_base: format!("https://api.telegram.org/bot{token}"),
            token,
            offset: 0,
            attachments_dir,
        })
    }

    fn method_url(&self, method: &str) -> String {
        format!("{}/{method}", self.api_base)
    }

    /// Download a Telegram file by `file_id` into `attachments_dir` and return
    /// the local path. Uses `getFile` then fetches
    /// `https://api.telegram.org/file/bot<token>/<file_path>`.
    async fn download_file(&self, file_id: &str) -> Result<PathBuf> {
        let url = self.method_url("getFile");
        let response = self
            .http
            .get(&url)
            .query(&[("file_id", file_id)])
            .send()
            .await
            .context("requesting Telegram getFile")?
            .error_for_status()
            .context("Telegram getFile returned an error status")?;
        let body: GetFile = response
            .json()
            .await
            .context("decoding Telegram getFile response")?;
        if !body.ok {
            anyhow::bail!("Telegram getFile returned ok=false");
        }
        let file_path = body
            .result
            .and_then(|r| r.file_path)
            .context("Telegram getFile response missing file_path")?;

        let download_url = format!(
            "https://api.telegram.org/file/bot{}/{}",
            self.token, file_path
        );
        let bytes = self
            .http
            .get(&download_url)
            .send()
            .await
            .context("downloading Telegram file")?
            .error_for_status()
            .context("Telegram file download returned an error status")?
            .bytes()
            .await
            .context("reading Telegram file body")?;

        std::fs::create_dir_all(&self.attachments_dir).with_context(|| {
            format!(
                "creating attachments dir {}",
                self.attachments_dir.display()
            )
        })?;

        // Prefer the remote basename; fall back to a unique id-based name.
        let name = Path::new(&file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .map(|n| n.to_string())
            .unwrap_or_else(|| format!("tg-{file_id}"));
        // Disambiguate concurrent downloads of the same remote name.
        let local = self.attachments_dir.join(format!(
            "{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            sanitize_filename(&name)
        ));
        std::fs::write(&local, &bytes)
            .with_context(|| format!("writing attachment {}", local.display()))?;
        Ok(local)
    }

    /// Convert a Telegram message into zero or one [`Inbound`]. Unsupported
    /// types yield a short rejection reply (best-effort) and no inbound.
    async fn message_to_inbound(&self, message: Message) -> Option<Inbound> {
        let chat_id = message.chat.id;
        let caption = message.caption.filter(|c| !c.trim().is_empty());
        let text = message.text.filter(|t| !t.trim().is_empty());

        // Pure text.
        if let Some(text) = text {
            return Some(Inbound {
                chat_id,
                text,
                attachments: Vec::new(),
            });
        }

        // Photo: Telegram sends several sizes; take the largest (last).
        if let Some(photos) = message.photo.as_ref().filter(|p| !p.is_empty()) {
            let largest = &photos[photos.len() - 1];
            match self.download_file(&largest.file_id).await {
                Ok(path) => {
                    let text = caption.unwrap_or_else(|| PHOTO_ONLY_PROMPT.to_string());
                    return Some(Inbound {
                        chat_id,
                        text,
                        attachments: vec![path],
                    });
                }
                Err(err) => {
                    tracing::warn!("failed to download Telegram photo: {err:#}");
                    // Still deliver caption-only so the agent can respond.
                    if let Some(text) = caption {
                        return Some(Inbound {
                            chat_id,
                            text,
                            attachments: Vec::new(),
                        });
                    }
                    return None;
                }
            }
        }

        // Image document (or any document with a caption / image mime).
        if let Some(doc) = message.document {
            let is_image = doc
                .mime_type
                .as_deref()
                .is_some_and(|m| m.starts_with("image/"))
                || doc
                    .file_name
                    .as_deref()
                    .is_some_and(|n| is_image_filename(n));
            if is_image || caption.is_some() {
                match self.download_file(&doc.file_id).await {
                    Ok(path) => {
                        let text = caption.unwrap_or_else(|| {
                            if is_image {
                                PHOTO_ONLY_PROMPT.to_string()
                            } else {
                                format!(
                                    "Please look at the attached file ({}).",
                                    doc.file_name.as_deref().unwrap_or("document")
                                )
                            }
                        });
                        return Some(Inbound {
                            chat_id,
                            text,
                            attachments: vec![path],
                        });
                    }
                    Err(err) => {
                        tracing::warn!("failed to download Telegram document: {err:#}");
                        if let Some(text) = caption {
                            return Some(Inbound {
                                chat_id,
                                text,
                                attachments: Vec::new(),
                            });
                        }
                        return None;
                    }
                }
            }
        }

        // Caption-only (no media we recognized) — still useful.
        if let Some(text) = caption {
            return Some(Inbound {
                chat_id,
                text,
                attachments: Vec::new(),
            });
        }

        // Stickers, voice, video notes, etc.: acknowledge rather than silence.
        tracing::info!("unsupported Telegram message type from chat {chat_id}");
        if let Err(err) = self.send(chat_id, UNSUPPORTED_REPLY).await {
            tracing::warn!("failed to send unsupported-type reply: {err:#}");
        }
        None
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
                // Only message updates; drops callback_query / channel_post noise.
                ("allowed_updates", r#"["message"]"#.to_string()),
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
                && let Some(item) = self.message_to_inbound(message).await
            {
                inbound.push(item);
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

    async fn typing(&self, chat_id: i64) -> Result<()> {
        let url = self.method_url("sendChatAction");
        self.http
            .post(&url)
            .json(&serde_json::json!({ "chat_id": chat_id, "action": "typing" }))
            .send()
            .await
            .context("sending Telegram chat action")?
            .error_for_status()
            .context("Telegram sendChatAction returned an error status")?;
        Ok(())
    }
}

/// Directory for downloaded gateway attachments. Prefers
/// `~/.wizard/gateway-attachments`; falls back to the system temp dir.
fn attachments_dir() -> PathBuf {
    crate::config::Config::wizard_dir()
        .map(|d| d.join("gateway-attachments"))
        .unwrap_or_else(|_| std::env::temp_dir().join("wizard-gateway-attachments"))
}

/// Keep only a safe basename so a hostile `file_path` cannot escape the
/// attachments directory.
fn sanitize_filename(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}

fn is_image_filename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".png")
        || lower.ends_with(".gif")
        || lower.ends_with(".webp")
        || lower.ends_with(".bmp")
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
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    photo: Option<Vec<PhotoSize>>,
    #[serde(default)]
    document: Option<Document>,
}

/// One size of a photo array (Telegram sends several; we take the last).
#[derive(Debug, Deserialize)]
struct PhotoSize {
    file_id: String,
}

/// A document attachment.
#[derive(Debug, Deserialize)]
struct Document {
    file_id: String,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
}

/// The chat a message belongs to.
#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
}

/// `getFile` response.
#[derive(Debug, Deserialize)]
struct GetFile {
    ok: bool,
    #[serde(default)]
    result: Option<FileInfo>,
}

#[derive(Debug, Deserialize)]
struct FileInfo {
    #[serde(default)]
    file_path: Option<String>,
}

/// Pure helpers for unit tests: turn a parsed [`Message`] into an inbound
/// without network I/O. Photo/document messages are represented as text +
/// empty attachments here; the live path downloads files.
#[cfg(test)]
fn inbound_from_message_fields(
    chat_id: i64,
    text: Option<String>,
    caption: Option<String>,
    has_photo: bool,
    has_document: bool,
) -> Option<Inbound> {
    let caption = caption.filter(|c| !c.trim().is_empty());
    let text = text.filter(|t| !t.trim().is_empty());
    if let Some(text) = text {
        return Some(Inbound {
            chat_id,
            text,
            attachments: Vec::new(),
        });
    }
    if has_photo {
        return Some(Inbound {
            chat_id,
            text: caption.unwrap_or_else(|| PHOTO_ONLY_PROMPT.to_string()),
            attachments: Vec::new(), // live path fills this after download
        });
    }
    if has_document {
        return Some(Inbound {
            chat_id,
            text: caption.unwrap_or_else(|| PHOTO_ONLY_PROMPT.to_string()),
            attachments: Vec::new(),
        });
    }
    if let Some(text) = caption {
        return Some(Inbound {
            chat_id,
            text,
            attachments: Vec::new(),
        });
    }
    None
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
        assert!(
            message.contains("credentials.toml") || message.contains("onboard"),
            "error should mention credentials or onboarding: {message}"
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

    #[test]
    fn parses_caption_only_message() {
        let raw = r#"{
            "ok": true,
            "result": [
                {
                    "update_id": 20,
                    "message": {
                        "chat": {"id": 42},
                        "caption": "describe this",
                        "photo": [
                            {"file_id": "small"},
                            {"file_id": "large"}
                        ]
                    }
                }
            ]
        }"#;
        let body: GetUpdates = serde_json::from_str(raw).expect("valid payload");
        let msg = body.result[0].message.as_ref().expect("message");
        assert_eq!(msg.chat.id, 42);
        assert_eq!(msg.caption.as_deref(), Some("describe this"));
        assert!(msg.text.is_none());
        assert_eq!(msg.photo.as_ref().map(|p| p.len()), Some(2));

        let inbound = inbound_from_message_fields(
            msg.chat.id,
            msg.text.clone(),
            msg.caption.clone(),
            msg.photo.as_ref().is_some_and(|p| !p.is_empty()),
            msg.document.is_some(),
        )
        .expect("caption+photo becomes inbound");
        assert_eq!(inbound.chat_id, 42);
        assert_eq!(inbound.text, "describe this");
    }

    #[test]
    fn parses_photo_without_caption() {
        let raw = r#"{
            "ok": true,
            "result": [
                {
                    "update_id": 21,
                    "message": {
                        "chat": {"id": 7},
                        "photo": [{"file_id": "only"}]
                    }
                }
            ]
        }"#;
        let body: GetUpdates = serde_json::from_str(raw).expect("valid payload");
        let msg = body.result[0].message.as_ref().expect("message");
        let inbound = inbound_from_message_fields(
            msg.chat.id,
            msg.text.clone(),
            msg.caption.clone(),
            true,
            false,
        )
        .expect("photo-only becomes inbound");
        assert_eq!(inbound.text, PHOTO_ONLY_PROMPT);
    }

    #[test]
    fn parses_image_document_with_caption() {
        let raw = r#"{
            "ok": true,
            "result": [
                {
                    "update_id": 22,
                    "message": {
                        "chat": {"id": 9},
                        "caption": "scan this",
                        "document": {
                            "file_id": "doc1",
                            "file_name": "shot.png",
                            "mime_type": "image/png"
                        }
                    }
                }
            ]
        }"#;
        let body: GetUpdates = serde_json::from_str(raw).expect("valid payload");
        let msg = body.result[0].message.as_ref().expect("message");
        assert_eq!(
            msg.document.as_ref().unwrap().mime_type.as_deref(),
            Some("image/png")
        );
        let inbound = inbound_from_message_fields(
            msg.chat.id,
            msg.text.clone(),
            msg.caption.clone(),
            false,
            true,
        )
        .expect("document becomes inbound");
        assert_eq!(inbound.text, "scan this");
    }

    #[test]
    fn sanitize_filename_strips_path_components() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("ok-file_1.jpg"), "ok-file_1.jpg");
        assert_eq!(sanitize_filename("weird name!.png"), "weird_name_.png");
    }

    #[test]
    fn is_image_filename_detects_common_extensions() {
        assert!(is_image_filename("x.PNG"));
        assert!(is_image_filename("a.jpeg"));
        assert!(!is_image_filename("notes.txt"));
    }
}
