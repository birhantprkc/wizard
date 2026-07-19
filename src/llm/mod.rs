//! LLM wire types matching Ollama's **native** `/api/chat` schema
//! (not the OpenAI-compatible shim). Shared by the agent loop, the tool
//! registry, and the TUI.

pub mod anthropic;
pub mod chatgpt;
pub mod chatgpt_oauth;
pub mod cloudflare;
pub mod compat;
pub mod fusion;
pub mod llamacpp;
pub mod oauth_callback;
pub mod ollama;
pub mod openai;
pub mod openrouter;
pub mod provider;
pub mod xai_oauth;

use std::pin::Pin;

use anyhow::Result;
use futures_util::Stream;
use serde::{Deserialize, Serialize};

/// Boxed stream of [`ChatChunk`]s yielded by every provider's `chat_stream`.
/// Shared across [`llamacpp`], [`ollama`], [`openai`], and [`anthropic`].
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<ChatChunk>> + Send>>;

/// HTTP client builder for the cloud chat backends. A generation can
/// legitimately stream for many minutes, so there is no overall request
/// timeout; instead the client fails fast when it can't connect, errors out
/// of a stream that has gone completely silent (a live SSE stream never goes
/// minutes without a frame — even keep-alive comments count as reads), and
/// keepalive-probes idle connections so a dead peer is noticed. A stream
/// read that times out surfaces as a transient error, which the agent's
/// backoff-retry loop picks up — instead of the turn hanging forever on a
/// stalled connection. Local backends (llama.cpp, Ollama) keep their own
/// clients: a big model can silently prefill for a long time on weak
/// hardware, which this would misread as a stall.
pub(crate) fn cloud_http_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(20))
        .read_timeout(std::time::Duration::from_secs(300))
        .tcp_keepalive(std::time::Duration::from_secs(30))
}

/// Typed error returned by every HTTP-based provider adapter for failed
/// responses and transport failures. Always reachable from the `anyhow`
/// chain via `err.downcast_ref::<ProviderError>()`, so the agent loop can
/// classify retries uniformly across providers.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ProviderError {
    /// HTTP status of the failed response; `None` for transport failures
    /// (connect/timeout/mid-stream drop) where no status was received.
    pub status: Option<u16>,
    /// Human-readable error, including a snippet of the API's response body
    /// so users see the real API message.
    pub message: String,
}

impl ProviderError {
    /// Error for a non-success HTTP response `status`.
    pub fn http(status: u16, message: impl Into<String>) -> Self {
        Self {
            status: Some(status),
            message: message.into(),
        }
    }

    /// Error for a transport failure (no HTTP status was received).
    pub fn transport(message: impl Into<String>) -> Self {
        Self {
            status: None,
            message: message.into(),
        }
    }

    /// Whether a retry after backoff may succeed: transport failures
    /// (`status == None`), timeouts (408), rate limits (429), and server
    /// errors (5xx) are transient; other 4xx (bad request, auth, missing
    /// model) are not.
    pub fn is_transient(&self) -> bool {
        match self.status {
            None => true,
            Some(408) | Some(429) => true,
            Some(status) => status >= 500,
        }
    }
}

/// Message role on the Ollama `/api/chat` wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// Tool result fed back to the model.
    Tool,
}

/// Largest decoded image Wizard carries. Anything bigger is dropped at the
/// seam it arrives on ([`Image::from_bytes`], [`Image::from_path`], the
/// providers' stream decoders, and [`crate::agent::absorb_images`] for
/// anything hand-built) rather than pushed through history, the session file
/// and every surface.
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// Why an image could not be taken in.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("unrecognized image data (not PNG, JPEG, WebP or GIF)")]
    UnknownFormat,
    #[error("image data is not valid base64: {0}")]
    NotBase64(#[from] base64::DecodeError),
    #[error("image is {bytes} bytes, over the {MAX_IMAGE_BYTES} byte cap")]
    TooLarge { bytes: usize },
    #[error("cannot read image {path}: {source}")]
    Unreadable {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// An image travelling through Wizard, in either direction: attached to a
/// [`ChatMessage`] on the way *to* a vision model, or produced *by* a tool
/// ([`crate::tools::ToolOutput::images`]) or by the model itself
/// ([`ChatChunk::images`]).
///
/// `b64` is the base64 of the encoded file bytes with **no** `data:` prefix
/// (providers that want a data URI build one with [`Image::data_uri`]); `mime`
/// is its media type, e.g. `image/png`.
///
/// This diverges from `feat/computer-use`, where images are a bare
/// `Vec<String>` of base64 PNGs: a *generated* image is not always a PNG, so
/// the media type has to ride with the bytes. Reconciling the two branches
/// when that one merges is mechanical — its `Vec<String>` becomes
/// `Image::new(b64, "image/png")` at each construction site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    /// Base64 of the encoded image file (no `data:` prefix).
    pub b64: String,
    /// Media type of the encoded bytes, e.g. `image/png`.
    pub mime: String,
    /// Where the image was written, once the session's image store took it in
    /// ([`crate::agent::absorb_images`]). `None` before then — a tool that has
    /// just produced an image does not know, and does not need to.
    ///
    /// It is recorded in the session file alongside the base64 purely for
    /// *replay*: a surface rebuilding a transcript from disk (the GUI's, the
    /// TUI's on `--resume`) gets the same path the live
    /// [`AgentEvent::Images`](crate::agent::AgentEvent::Images) carried,
    /// instead of re-deriving it. No provider ever sees this field — every
    /// provider translates `images` into its own shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<std::path::PathBuf>,
}

impl Image {
    /// An image whose base64 and media type are already known (a provider
    /// decoding its own wire format, a tool that knows what it encoded).
    pub fn new(b64: impl Into<String>, mime: impl Into<String>) -> Self {
        Self {
            b64: b64.into(),
            mime: mime.into(),
            path: None,
        }
    }

    /// This image, tagged with where the image store wrote it.
    pub fn at_path(mut self, path: std::path::PathBuf) -> Self {
        self.path = Some(path);
        self
    }

    /// Take in raw encoded image bytes: the media type is sniffed from the
    /// magic number and the bytes are base64-encoded. The natural constructor
    /// for a tool that has just produced or read an image file.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ImageError> {
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(ImageError::TooLarge { bytes: bytes.len() });
        }
        let mime = sniff_mime(bytes).ok_or(ImageError::UnknownFormat)?;
        use base64::Engine as _;
        Ok(Self::new(
            base64::engine::general_purpose::STANDARD.encode(bytes),
            mime,
        ))
    }

    /// Take in an image file from disk: a user attaching a screenshot, a
    /// pasted file path. The size cap is enforced against the file's metadata
    /// *before* any bytes are read, so an oversized file is refused without
    /// being pulled into memory, and the media type is sniffed from the bytes
    /// rather than guessed from the extension — a `.png` that is really a
    /// JPEG is tagged as what it is.
    ///
    /// The returned image is tagged with the path it came from, so a surface
    /// replaying the transcript can render the file it already has on disk.
    pub fn from_path(path: &std::path::Path) -> Result<Self, ImageError> {
        let unreadable = |source: std::io::Error| ImageError::Unreadable {
            path: path.display().to_string(),
            source,
        };
        let meta = std::fs::metadata(path).map_err(unreadable)?;
        if !meta.is_file() {
            return Err(unreadable(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a file",
            )));
        }
        if meta.len() > MAX_IMAGE_BYTES as u64 {
            return Err(ImageError::TooLarge {
                bytes: usize::try_from(meta.len()).unwrap_or(usize::MAX),
            });
        }
        let bytes = std::fs::read(path).map_err(unreadable)?;
        Ok(Self::from_bytes(&bytes)?.at_path(path.to_path_buf()))
    }

    /// Take in a `data:` URI (`data:image/png;base64,iVBOR...`), the shape
    /// OpenAI-compatible endpoints use for image content parts. `None` when
    /// the string is not a base64 `data:` URI of an image.
    pub fn from_data_uri(uri: &str) -> Result<Self, ImageError> {
        let rest = uri.strip_prefix("data:").ok_or(ImageError::UnknownFormat)?;
        let (mime, payload) = rest.split_once(',').ok_or(ImageError::UnknownFormat)?;
        let mime = mime
            .strip_suffix(";base64")
            .ok_or(ImageError::UnknownFormat)?;
        if !mime.starts_with("image/") {
            return Err(ImageError::UnknownFormat);
        }
        Self::from_base64(payload, mime)
    }

    /// Take in base64 that arrived with its media type stated separately
    /// (`b64_json` payloads). Validates the base64 and the size cap.
    pub fn from_base64(b64: &str, mime: &str) -> Result<Self, ImageError> {
        let image = Self::new(b64.trim(), mime);
        let bytes = image.decoded_len();
        if bytes > MAX_IMAGE_BYTES {
            return Err(ImageError::TooLarge { bytes });
        }
        // Decode once, up front: a provider must never hand a broken payload
        // to a surface that will try to write it to disk.
        image.decode()?;
        Ok(image)
    }

    /// The encoded file bytes.
    pub fn decode(&self) -> Result<Vec<u8>, base64::DecodeError> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.decode(self.b64.trim())
    }

    /// Size of the decoded image, derived from the base64 length — no decode,
    /// so the size cap can be checked without allocating the payload.
    pub fn decoded_len(&self) -> usize {
        let b64 = self.b64.trim();
        let padding = b64.bytes().rev().take_while(|&byte| byte == b'=').count();
        b64.len().saturating_sub(padding) * 3 / 4
    }

    /// `data:<mime>;base64,<b64>` — how OpenAI-compatible endpoints (and the
    /// GUI's `<img src>`) want an inline image.
    pub fn data_uri(&self) -> String {
        format!("data:{};base64,{}", self.mime, self.b64)
    }

    /// File extension for this media type, for naming the image on disk.
    pub fn extension(&self) -> &'static str {
        match self.mime.as_str() {
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            // PNG is both the common case and the safe default: an image whose
            // type we could not name is still written, just conservatively.
            _ => "png",
        }
    }
}

/// Media type of `bytes` from its magic number. Covers the formats every
/// vision model and image endpoint in use speaks; `None` for anything else,
/// which is refused rather than guessed at.
pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes[8..12] == *b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    None
}

/// A single chat message. Session files and in-memory history use this shape;
/// provider adapters translate it to each backend's wire format (including
/// multimodal content blocks when [`ChatMessage::images`] is non-empty).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    /// Tool calls emitted by the model (assistant messages only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Name of the tool that produced this result (`role == Tool`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Images attached to this message. On a user message they are input for a
    /// vision model (a screenshot, or an image a tool just returned — see
    /// [`ChatMessage::user_with_images`]); on an assistant message they are
    /// what the model itself produced. Every provider translates them into its
    /// own shape in `build_messages` (Ollama's sibling base64 array, OpenAI's
    /// `image_url` parts, Anthropic's base64 `image` blocks).
    ///
    /// Empty is the overwhelming common case and is omitted from the wire, so
    /// text-only traffic is byte-for-byte unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
}

impl ChatMessage {
    /// Rough token estimate for this message (`~4` chars per token, plus a
    /// flat allowance for each attached image). Used for the TUI context
    /// meter when the backend has not yet reported a real prompt size
    /// (fresh session, post-`/clear`, post-compaction).
    pub fn estimated_tokens(&self) -> u64 {
        let mut chars = self.content.len();
        if let Some(name) = &self.tool_name {
            chars = chars.saturating_add(name.len());
        }
        for call in &self.tool_calls {
            chars = chars.saturating_add(call.function.name.len());
            // `arguments` is already a JSON value; its string form is what
            // the wire payload roughly costs.
            chars = chars.saturating_add(call.function.arguments.to_string().len());
        }
        // Vision attachments are model-specific; a flat 1k-token allowance
        // per image is enough for the status-bar meter.
        let image_tokens = (self.images.len() as u64).saturating_mul(1_000);
        estimate_tokens_from_chars(chars).saturating_add(image_tokens)
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_name: None,
            images: Vec::new(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_name: None,
            images: Vec::new(),
        }
    }

    /// User message carrying images alongside its text. This is how a tool's
    /// images reach the model: a `tool`-role message cannot carry image blocks
    /// on OpenAI, but a user message can on every provider, so the tool result
    /// carries the text and the images follow on a user message (see
    /// `Agent::dispatch_call`). A non-vision model simply ignores them.
    pub fn user_with_images(content: impl Into<String>, images: Vec<Image>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_name: None,
            images,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_name: None,
            images: Vec::new(),
        }
    }

    /// Tool result message answering a prior [`ToolCall`].
    pub fn tool_result(tool_name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_name: Some(tool_name.into()),
            images: Vec::new(),
        }
    }
}

/// The text of an assistant message as it goes back on the wire, with any
/// images it produced named in it.
///
/// No chat API accepts image content *inside* an assistant turn — images are
/// user-role input everywhere — so a model's own generated images cannot be
/// replayed as they were produced. They are dropped from the request and named
/// in the text instead: the model still knows what it made (and the user still
/// sees the file, which the surfaces render from
/// [`AgentEvent::Images`](crate::agent::AgentEvent::Images)), and the request
/// stays valid rather than 400-ing on a block the API will not take.
pub(crate) fn assistant_content(message: &ChatMessage) -> String {
    if message.images.is_empty() {
        return message.content.clone();
    }
    let kinds: Vec<&str> = message
        .images
        .iter()
        .map(|image| image.mime.as_str())
        .collect();
    let note = format!(
        "[generated {} image(s) ({}) — delivered to the user]",
        message.images.len(),
        kinds.join(", ")
    );
    if message.content.is_empty() {
        note
    } else {
        format!("{}\n\n{note}", message.content)
    }
}

/// Rough token estimate from a character count (`~4` chars per token). Used
/// only when a backend has not reported real usage; never for billing.
pub fn estimate_tokens_from_chars(chars: usize) -> u64 {
    (chars as u64).div_ceil(4)
}

/// Sum of [`ChatMessage::estimated_tokens`] over a history. The status bar
/// falls back to this after `/clear` or compaction, when the last real
/// prompt size is stale or unknown.
pub fn estimate_history_tokens(messages: &[ChatMessage]) -> u64 {
    messages.iter().map(ChatMessage::estimated_tokens).sum()
}

/// A tool invocation requested by the model.
/// Wire shape: `{ "function": { "name": ..., "arguments": {...} } }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub function: FunctionCall,
}

/// The function half of a [`ToolCall`]. `arguments` is a JSON object
/// (already parsed — Ollama's native endpoint sends structured arguments,
/// not a string).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// A tool advertised to the model in the request's `tools` array.
/// Wire shape: `{ "type": "function", "function": { name, description, parameters } }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionSpec,
}

impl ToolSpec {
    /// Build a `"function"`-typed spec. `parameters` must be a JSON Schema
    /// object describing the arguments.
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            kind: "function".to_string(),
            function: FunctionSpec {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

/// The function half of a [`ToolSpec`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's arguments.
    pub parameters: serde_json::Value,
}

/// Model sampling options forwarded as Ollama's `options` object.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,
    /// Reasoning effort (`"low"`/`"medium"`/`"high"`) for models that accept a
    /// `reasoning_effort` request field. Carried as a string so this module
    /// stays decoupled from [`crate::config::ReasoningEffort`]; the
    /// OpenAI-compatible client forwards it only for supporting models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// Request body for `POST /api/chat`.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<ChatOptions>,
}

/// One streamed JSON line from `POST /api/chat` (`stream: true`).
/// Text arrives as deltas in `message.content`; tool calls arrive complete
/// in `message.tool_calls`; the final chunk has `done == true`.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub message: Option<ChatMessage>,
    /// Images the model produced in this chunk.
    ///
    /// **This is the seam for an image-generating endpoint.** A provider that
    /// receives image content while streaming (an `image_url` part, a
    /// `b64_json` payload — see [`openai::decode_sse`] for the working
    /// example) decodes it into an [`Image`] and emits it here, on the chunk
    /// it arrived in; the chunk may carry images, text, tool calls, or any
    /// combination. The agent loop accumulates them onto the assistant
    /// [`ChatMessage`], writes them to the session's image directory, and
    /// announces them to the surfaces as [`crate::agent::AgentEvent::Images`].
    /// Nothing else is required of the provider.
    #[serde(default)]
    pub images: Vec<Image>,
    /// True when `message.content` is model reasoning ("thinking") rather
    /// than answer text (Anthropic `thinking_delta`, xAI `reasoning_content`).
    /// The UI renders it dimmed; it is never fed back into history.
    #[serde(default)]
    pub thinking: bool,
    pub done: bool,
    #[serde(default)]
    pub done_reason: Option<String>,
    /// Output token count (final chunk only).
    #[serde(default)]
    pub eval_count: Option<u64>,
    /// Prompt token count (final chunk only).
    #[serde(default)]
    pub prompt_eval_count: Option<u64>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn chat_request_serializes_to_native_ollama_shape() {
        let request = ChatRequest {
            model: "qwen3.6:27b".to_string(),
            messages: vec![
                ChatMessage::system("You are Wizard."),
                ChatMessage::user("hi"),
            ],
            tools: vec![ToolSpec::function(
                "read_file",
                "Read a file.",
                json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }),
            )],
            stream: true,
            options: Some(ChatOptions {
                temperature: Some(0.8),
                num_ctx: None,
                reasoning_effort: None,
            }),
        };

        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["model"], "qwen3.6:27b");
        assert_eq!(value["stream"], true);
        assert_eq!(value["messages"][0]["role"], "system");
        assert_eq!(value["messages"][1]["role"], "user");
        assert_eq!(value["messages"][1]["content"], "hi");
        assert_eq!(value["tools"][0]["type"], "function");
        assert_eq!(value["tools"][0]["function"]["name"], "read_file");
        assert_eq!(
            value["tools"][0]["function"]["parameters"]["required"][0],
            "path"
        );
        let temperature = value["options"]["temperature"]
            .as_f64()
            .expect("temperature is a number");
        assert!(
            (temperature - 0.8).abs() < 1e-6,
            "temperature survives the f32 round-trip: {temperature}"
        );
        assert!(
            value["options"].get("num_ctx").is_none(),
            "unset options are omitted"
        );
    }

    #[test]
    fn empty_tools_and_options_are_omitted_from_the_wire() {
        let request = ChatRequest {
            model: "m".to_string(),
            messages: vec![ChatMessage::user("hi")],
            tools: Vec::new(),
            stream: false,
            options: None,
        };
        let value = serde_json::to_value(&request).unwrap();
        assert!(value.get("tools").is_none(), "empty tools array is omitted");
        assert!(value.get("options").is_none(), "absent options are omitted");
    }

    #[test]
    fn plain_message_omits_tool_fields() {
        let value = serde_json::to_value(ChatMessage::assistant("done")).unwrap();
        assert!(value.get("tool_calls").is_none());
        assert!(value.get("tool_name").is_none());
        assert!(
            value.get("images").is_none(),
            "text-only traffic is unchanged on the wire"
        );
    }

    /// Smallest possible files of each format we sniff (header bytes are all
    /// that matters).
    fn png_bytes() -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(b"IHDR-and-the-rest");
        bytes
    }

    #[test]
    fn sniffs_the_media_type_from_magic_numbers() {
        assert_eq!(sniff_mime(&png_bytes()), Some("image/png"));
        assert_eq!(
            sniff_mime(&[0xff, 0xd8, 0xff, 0xe0, 0x00]),
            Some("image/jpeg")
        );
        assert_eq!(sniff_mime(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_mime(b"GIF89a....."), Some("image/gif"));
        assert_eq!(sniff_mime(b"GIF87a....."), Some("image/gif"));
        // Not an image, and truncated headers that merely start right.
        assert_eq!(sniff_mime(b"not an image at all"), None);
        assert_eq!(sniff_mime(b"RIFF\0\0\0\0AVI "), None, "RIFF but not WebP");
        assert_eq!(sniff_mime(&[0x89, b'P']), None);
        assert_eq!(sniff_mime(&[]), None);
    }

    #[test]
    fn from_bytes_sniffs_encodes_and_round_trips() {
        let image = Image::from_bytes(&png_bytes()).expect("a PNG");
        assert_eq!(image.mime, "image/png");
        assert_eq!(image.extension(), "png");
        assert!(!image.b64.starts_with("data:"), "no data: prefix on b64");
        assert_eq!(image.decode().expect("decodes"), png_bytes());
        assert_eq!(image.decoded_len(), png_bytes().len());
        assert_eq!(
            image.data_uri(),
            format!("data:image/png;base64,{}", image.b64)
        );

        let err = Image::from_bytes(b"nonsense").expect_err("unknown format");
        assert!(matches!(err, ImageError::UnknownFormat), "{err}");
    }

    #[test]
    fn decoded_len_matches_the_real_payload_at_every_padding() {
        // 0, 1 and 2 padding chars — the three base64 alignments.
        for raw in [&b"abc"[..], &b"a"[..], &b"ab"[..]] {
            use base64::Engine as _;
            let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
            let image = Image::new(b64, "image/png");
            assert_eq!(
                image.decoded_len(),
                raw.len(),
                "size is derived from the base64 without decoding"
            );
        }
    }

    #[test]
    fn from_data_uri_accepts_images_and_refuses_anything_else() {
        let image = Image::from_data_uri("data:image/webp;base64,UklGRg==").expect("a webp");
        assert_eq!(image.mime, "image/webp");
        assert_eq!(image.b64, "UklGRg==");
        assert_eq!(image.extension(), "webp");

        for bad in [
            "https://example.com/cat.png",
            "data:text/plain;base64,aGk=",
            "data:image/png,notbase64encoded",
            "data:image/png;base64,!!!not base64!!!",
        ] {
            assert!(Image::from_data_uri(bad).is_err(), "must refuse {bad}");
        }
    }

    #[test]
    fn oversized_images_are_refused_at_the_seam() {
        let huge = vec![0u8; MAX_IMAGE_BYTES + 1];
        let err = Image::from_bytes(&huge).expect_err("over the cap");
        assert!(
            matches!(err, ImageError::TooLarge { bytes } if bytes == MAX_IMAGE_BYTES + 1),
            "{err}"
        );

        // The base64 path caps too, without decoding the payload.
        let b64 = "A".repeat(MAX_IMAGE_BYTES / 3 * 4 + 8);
        let err = Image::from_base64(&b64, "image/png").expect_err("over the cap");
        assert!(matches!(err, ImageError::TooLarge { .. }), "{err}");
    }

    #[test]
    fn message_images_round_trip_through_the_session_format() {
        let message =
            ChatMessage::user_with_images("what is this?", vec![Image::new("QUJD", "image/jpeg")]);
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["images"][0]["b64"], "QUJD");
        assert_eq!(value["images"][0]["mime"], "image/jpeg");

        let back: ChatMessage = serde_json::from_value(value).unwrap();
        assert_eq!(back.images, message.images);
    }

    #[test]
    fn chat_chunk_without_images_deserializes_to_none() {
        // Every existing provider's chunks have no `images` field.
        let chunk: ChatChunk =
            serde_json::from_str(r#"{"message":{"role":"assistant","content":"hi"},"done":false}"#)
                .unwrap();
        assert!(chunk.images.is_empty());
    }

    #[test]
    fn from_path_sniffs_the_bytes_and_caps_the_file_size() {
        let dir = tempfile::tempdir().expect("tempdir");

        // The extension lies (a JPEG named .png): the bytes decide.
        let jpeg = dir.path().join("shot.png");
        std::fs::write(&jpeg, [0xff, 0xd8, 0xff, 0xe0, 0x00]).expect("write");
        let image = Image::from_path(&jpeg).expect("a JPEG");
        assert_eq!(image.mime, "image/jpeg");
        assert_eq!(image.path.as_deref(), Some(jpeg.as_path()));

        // Oversized files are refused on their metadata, before being read.
        let huge = dir.path().join("huge.png");
        std::fs::write(&huge, vec![0u8; MAX_IMAGE_BYTES + 1]).expect("write");
        assert!(matches!(
            Image::from_path(&huge).expect_err("over the cap"),
            ImageError::TooLarge { .. }
        ));

        // Not an image, and not a file at all.
        let text = dir.path().join("notes.txt");
        std::fs::write(&text, b"just words").expect("write");
        assert!(matches!(
            Image::from_path(&text).expect_err("not an image"),
            ImageError::UnknownFormat
        ));
        assert!(matches!(
            Image::from_path(dir.path()).expect_err("a directory"),
            ImageError::Unreadable { .. }
        ));
        assert!(matches!(
            Image::from_path(&dir.path().join("gone.png")).expect_err("missing"),
            ImageError::Unreadable { .. }
        ));
    }

    #[test]
    fn old_transcripts_without_images_still_load() {
        let legacy: ChatMessage =
            serde_json::from_str(r#"{"role":"user","content":"hi"}"#).unwrap();
        assert!(legacy.images.is_empty());
    }

    #[test]
    fn estimated_tokens_scales_with_content_and_images() {
        let short = ChatMessage::user("abcd"); // 4 chars → 1 token
        assert_eq!(short.estimated_tokens(), 1);
        let long = ChatMessage::user("a".repeat(400)); // 400 chars → 100 tokens
        assert_eq!(long.estimated_tokens(), 100);
        let with_image =
            ChatMessage::user_with_images("see", vec![Image::new("QUJD", "image/png")]);
        // 3 chars → 1 token + 1000 image allowance
        assert_eq!(with_image.estimated_tokens(), 1_001);
        assert_eq!(
            estimate_history_tokens(&[short, long]),
            101,
            "history sums message estimates"
        );
    }

    #[test]
    fn assistant_tool_call_round_trips() {
        let mut message = ChatMessage::assistant("");
        message.tool_calls.push(ToolCall {
            function: FunctionCall {
                name: "execute".to_string(),
                arguments: json!({ "command": "cargo test" }),
            },
        });

        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["tool_calls"][0]["function"]["name"], "execute");
        assert_eq!(
            value["tool_calls"][0]["function"]["arguments"]["command"],
            "cargo test"
        );

        let back: ChatMessage = serde_json::from_value(value).unwrap();
        assert_eq!(back.tool_calls.len(), 1);
        assert_eq!(back.tool_calls[0].function.name, "execute");
    }

    #[test]
    fn tool_result_message_carries_role_and_name() {
        let value =
            serde_json::to_value(ChatMessage::tool_result("read_file", "contents")).unwrap();
        assert_eq!(value["role"], "tool");
        assert_eq!(value["tool_name"], "read_file");
        assert_eq!(value["content"], "contents");
    }

    #[test]
    fn provider_error_transient_classification() {
        // No status: transport failure (connect refused, timeout, dropped
        // stream) — retryable.
        assert!(ProviderError::transport("connection reset").is_transient());
        // Retryable statuses.
        for status in [408, 429, 500, 502, 503, 529] {
            assert!(
                ProviderError::http(status, "x").is_transient(),
                "HTTP {status} must be transient"
            );
        }
        // Client errors: retrying the same request cannot succeed.
        for status in [400, 401, 403, 404, 409, 413, 422] {
            assert!(
                !ProviderError::http(status, "x").is_transient(),
                "HTTP {status} must not be transient"
            );
        }
    }

    #[test]
    fn provider_error_downcasts_through_anyhow_context() {
        let err = anyhow::Error::new(ProviderError::http(429, "rate limited"))
            .context("chat request failed");
        let provider = err
            .downcast_ref::<ProviderError>()
            .expect("downcast through context");
        assert_eq!(provider.status, Some(429));
        assert!(provider.is_transient());
        assert_eq!(provider.message, "rate limited");
    }

    #[test]
    fn tool_call_with_missing_arguments_deserializes_to_null() {
        // Ollama may omit `arguments` entirely; the agent normalizes null later.
        let call: ToolCall = serde_json::from_str(r#"{"function":{"name":"git_status"}}"#).unwrap();
        assert_eq!(call.function.name, "git_status");
        assert!(call.function.arguments.is_null());
    }
}
