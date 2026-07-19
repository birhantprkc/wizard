//! Inline composer prompts and paused-turn modals: provider setup, the
//! plan-review modal, and the interview modal.

use crate::agent::{InterviewQuestion, PlanVerdict, ultra};
use crate::config::ProviderKind;

/// A field being collected in the inline provider-setup prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PromptField {
    Name,
    /// Cloudflare account id — substituted into the base-URL template before
    /// the provider is built (Cloudflare setup only).
    AccountId,
    BaseUrl,
    Model,
    ApiKey,
}

/// In-progress provider setup driven by composer prompts. Each queued
/// [`PromptField`] is asked in turn; the answers fill the draft, and the last
/// answer emits a [`SlashCommand::ProviderSetup`](crate::commands::SlashCommand::ProviderSetup).
#[derive(Debug, Clone)]
pub struct ProviderPrompt {
    pub(super) kind: ProviderKind,
    pub(super) name: String,
    pub(super) base_url: String,
    pub(super) model: String,
    pub(super) api_key: Option<String>,
    /// Remaining fields to ask, in order.
    pub(super) queue: std::collections::VecDeque<PromptField>,
}

/// Sentinel value of the final "add provider" row in the level-1 provider
/// picker. The dispatch also keys off the last index, but matching the value
/// keeps it robust if the list grows.
pub(super) const PROVIDER_ADD_ROW: &str = "＋ Add provider…";

/// Value of the final row in the `/ultra config` picker: the judge, which is
/// not a lens and must not land in the roster. It is deliberately the same
/// string as [`crate::agent::ultra::JUDGE_NAME`] — `lens_catalog` excludes that
/// name from the lens rows, so this row cannot collide with one of them.
pub(super) const ULTRA_JUDGE_ROW: &str = ultra::JUDGE_NAME;

/// The level-2 provider-type menu: `(label, detail)` in dispatch order. The
/// Enter handler in [`App::handle_key`] matches on the row index, so this
/// order is the single source of truth for both rendering and dispatch.
/// The OpenAI-compatible presets from [`crate::llm::compat::PRESETS`] are
/// appended after these rows (rendering and dispatch both offset by
/// `PROVIDER_TYPES.len()`).
pub(super) const PROVIDER_TYPES: &[(&str, &str)] = &[
    ("xAI (Grok) — sign in", "OAuth · no API key"),
    (
        "xAI (Grok) — API key",
        "stored in ~/.wizard/credentials.toml",
    ),
    ("OpenRouter — API key", "openrouter.ai"),
    (
        "Cloudflare Workers AI — API token",
        "GLM 5.2 · account id + token",
    ),
    ("OpenAI — API key", "api.openai.com"),
    ("Anthropic (Claude) — API key", "api.anthropic.com"),
    ("OpenAI-compatible — custom", "any base URL + key"),
];

/// The `web_search` backend menu (`/settings`): `(id, label, detail)`. The id
/// is what gets written to `[web] search_backend` and (for keyed backends) the
/// `~/.wizard/credentials.toml` key name; the order is the display order.
pub(super) const WEB_BACKENDS: &[(&str, &str, &str)] = &[
    ("duckduckgo", "DuckDuckGo", "free · no API key"),
    ("brave", "Brave Search", "API key · brave.com/search/api"),
    ("tavily", "Tavily", "API key · tavily.com"),
    ("exa", "Exa", "API key · exa.ai"),
    ("serper", "Serper (Google)", "API key · serper.dev"),
    ("xai", "xAI (Grok)", "sign in with xAI, or API key"),
];

/// Display label for a `web_search` backend id (falls back to the id itself).
pub(super) fn web_backend_label(id: &str) -> &str {
    match id {
        "grok" => "xAI (Grok)",
        other => WEB_BACKENDS
            .iter()
            .find(|(value, _, _)| *value == other)
            .map(|(_, label, _)| *label)
            .unwrap_or(other),
    }
}

/// Whether a keyed `web_search` backend needs a pasted API key (vs DuckDuckGo,
/// which needs none, and xAI, which can use the OAuth session).
pub(super) fn web_backend_needs_key(id: &str) -> bool {
    matches!(id, "brave" | "tavily" | "exa" | "serper")
}

/// Whether an xAI OAuth session already exists on disk (`wizard --login xai`),
/// so web search can reuse it without a fresh sign-in.
pub(super) fn xai_oauth_session_present() -> bool {
    crate::llm::xai_oauth::token_path()
        .map(|path| path.exists())
        .unwrap_or(false)
}

/// Human-readable provider name for a kind, used in inline prompt questions.
pub(super) fn provider_display(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Xai | ProviderKind::XaiOauth => "xAI",
        ProviderKind::ChatgptOauth => "ChatGPT",
        ProviderKind::OpenRouter => "OpenRouter",
        ProviderKind::Openai => "OpenAI-compatible",
        ProviderKind::Anthropic => "Anthropic",
        ProviderKind::Cloudflare => "Cloudflare",
        ProviderKind::LlamaCpp => "llama.cpp",
        ProviderKind::Ollama => "Ollama",
    }
}

/// The question shown when collecting `field` for the in-progress `prompt`.
pub(super) fn prompt_question(field: PromptField, prompt: &ProviderPrompt) -> String {
    match field {
        PromptField::Name => "Provider name (id):".to_string(),
        PromptField::AccountId => {
            "Cloudflare account ID (dash.cloudflare.com → Workers AI → account id):".to_string()
        }
        PromptField::BaseUrl => "Base URL:".to_string(),
        PromptField::Model => "Model:".to_string(),
        PromptField::ApiKey => format!(
            "Paste your {} API key, then Enter (input hidden):",
            provider_display(prompt.kind)
        ),
    }
}

/// In-flight plan review (plan mode): the model called `exit_plan` and the
/// turn is paused inside the tool until a [`PlanVerdict`] is sent back.
#[derive(Debug)]
pub struct PlanReview {
    /// The plan markdown, rendered in the review modal.
    pub plan: String,
    /// Verdict channel back into the paused `exit_plan` call; taken exactly
    /// once when the review finishes.
    pub(super) respond: Option<tokio::sync::oneshot::Sender<PlanVerdict>>,
    /// `Some` while collecting rejection feedback (the text typed so far).
    pub feedback: Option<String>,
    /// Scroll offset from the top of the plan.
    pub scroll: u16,
}

/// In-flight interview (plan mode): the model called `interview` and the turn
/// is paused inside the tool until the user answers every question or
/// dismisses the modal.
#[derive(Debug)]
pub struct Interview {
    /// The questions, in order.
    pub questions: Vec<InterviewQuestion>,
    /// Answers collected so far, one per answered question (parallel to
    /// `questions[..current]`).
    pub answers: Vec<String>,
    /// Index of the question currently being answered.
    pub current: usize,
    /// The answer being typed for the current question.
    pub input: String,
    /// Answer channel back into the paused `interview` call; taken exactly
    /// once when the interview finishes (`Some(answers)`) or is dismissed
    /// (`None`).
    pub(super) respond: Option<tokio::sync::oneshot::Sender<Option<Vec<String>>>>,
}

impl Interview {
    /// The question now being answered, if any remain.
    pub fn current_question(&self) -> Option<&InterviewQuestion> {
        self.questions.get(self.current)
    }
}
