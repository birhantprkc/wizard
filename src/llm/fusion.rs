//! `FusionProvider` — model fusion as an [`LlmProvider`].
//!
//! Wraps a *panel* of Wizard providers and a *synthesizer*. On each turn the
//! panel members independently answer (and critique each other over N rounds)
//! via the [`fusion_core`] debate engine, then the synthesizer produces the
//! final answer with the panel's drafts injected as guidance. Because this is
//! just another [`LlmProvider`], the agent loop, tools, and TUI are unchanged —
//! `/fusion` simply swaps the active provider to one of these and back.
//!
//! **Tool semantics:** panel members are advisors (text only, no tools); the
//! synthesizer is the sole actor — it receives the request's `tools` and is the
//! only model that may emit `tool_calls`. So fusion works on agentic turns, not
//! just Q&A, with no conflicting tool calls.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures_util::StreamExt;

use fusion_core::config::{AgentConfig, SynthesizerConfig};
use fusion_core::error::ProviderError as FcProviderError;
use fusion_core::provider::{
    ChatProvider as FcChatProvider, ChatRequest as FcRequest, ChatResponse as FcResponse,
    Usage as FcUsage,
};
use fusion_core::{Config as FcConfig, Fusion};

use crate::config::Config;
use crate::llm::provider::LlmProvider;
use crate::llm::{ChatMessage, ChatRequest, ChatStream, Role};

/// One member of the debate panel: a Wizard provider bound to its model, plus a
/// unique routing key (its provider name).
pub struct PanelMember {
    /// Unique routing key — the provider's configured name.
    pub name: String,
    /// The built Wizard provider for this member.
    pub provider: Arc<dyn LlmProvider>,
    /// The model tag to request against `provider`.
    pub model: String,
}

/// Where a routed panel request goes: a provider and the model to ask it for.
struct Route {
    provider: Arc<dyn LlmProvider>,
    model: String,
}

/// Adapts a set of Wizard providers to `fusion_core`'s [`FcChatProvider`] seam.
/// The debate engine targets a panel member by the `model` field of each
/// request (set to the member's routing key); we dispatch that key to the
/// matching Wizard provider and collect its streamed text.
struct RoutingBackend {
    routes: HashMap<String, Route>,
}

#[async_trait]
impl FcChatProvider for RoutingBackend {
    async fn chat(&self, req: &FcRequest) -> std::result::Result<FcResponse, FcProviderError> {
        let route = self.routes.get(&req.model).ok_or_else(|| {
            FcProviderError::Http {
                status: 400,
                body: format!("no fusion route for panel member '{}'", req.model),
            }
        })?;

        // Panel members are advisors: translate the debate messages, attach no
        // tools, and stream the answer back as plain text.
        let messages = req
            .messages
            .iter()
            .map(|m| match m.role.as_str() {
                "system" => ChatMessage::system(m.content.clone()),
                "assistant" => ChatMessage::assistant(m.content.clone()),
                _ => ChatMessage::user(m.content.clone()),
            })
            .collect();
        let wreq = ChatRequest {
            model: route.model.clone(),
            messages,
            tools: Vec::new(),
            stream: true,
            options: None,
        };

        let stream = route
            .provider
            .chat_stream(wreq)
            .await
            // A failed panel member is non-fatal: map to a non-retryable error
            // so the engine records an empty contribution and moves on fast.
            .map_err(|e| FcProviderError::Http {
                status: 400,
                body: e.to_string(),
            })?;
        let content = collect_text(stream).await.map_err(|e| FcProviderError::Http {
            status: 400,
            body: e.to_string(),
        })?;

        Ok(FcResponse {
            content,
            usage: FcUsage::default(),
            raw: serde_json::Value::Null,
        })
    }
}

/// Drain a [`ChatStream`] to the concatenated answer text, skipping `thinking`
/// (reasoning) deltas.
async fn collect_text(mut stream: ChatStream) -> Result<String> {
    let mut out = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if chunk.thinking {
            continue;
        }
        if let Some(message) = chunk.message {
            out.push_str(&message.content);
        }
        if chunk.done {
            break;
        }
    }
    Ok(out)
}

/// Model fusion exposed as a single [`LlmProvider`].
pub struct FusionProvider {
    /// The panel debate engine (phases 1-2). `None` when no panel members were
    /// configured, in which case this degrades to the synthesizer alone.
    panel: Option<Fusion>,
    /// The provider that synthesizes the final, tool-capable, streamed answer.
    synthesizer: Arc<dyn LlmProvider>,
    /// Model tag to request against `synthesizer`.
    synth_model: String,
    /// Status-bar label, e.g. `"fusion: claude+openrouter ×1"`.
    label: String,
}

impl FusionProvider {
    /// Build a fusion provider from a resolved panel and synthesizer.
    ///
    /// `rounds` is the number of critique rounds (typically 1). `label` is shown
    /// in the status bar. An empty `panel` degrades to the synthesizer alone.
    pub fn new(
        panel: Vec<PanelMember>,
        synthesizer: Arc<dyn LlmProvider>,
        synth_model: String,
        rounds: u32,
        label: String,
    ) -> Result<Self> {
        let fusion = if panel.is_empty() {
            None
        } else {
            let mut routes = HashMap::new();
            let mut agents = Vec::new();
            for member in panel {
                agents.push(AgentConfig {
                    name: member.name.clone(),
                    model: member.name.clone(), // routing key
                    role: None,
                    fallback_models: Vec::new(),
                });
                routes.insert(
                    member.name.clone(),
                    Route {
                        provider: member.provider,
                        model: member.model,
                    },
                );
            }
            let cfg = FcConfig {
                api_key: None,
                agents,
                // Unused: Wizard runs the synthesis step itself (run_panel only).
                synthesizer: SynthesizerConfig {
                    name: "synth".to_string(),
                    model: "synth".to_string(),
                    fallback_models: Vec::new(),
                },
                rounds,
                max_tokens: 2048,
                temperature: 0.7,
                seed: None,
                log_file: Config::wizard_dir().ok().map(|d| d.join("fusion-runs.jsonl")),
                extra_headers: Default::default(),
            };
            let backend = Arc::new(RoutingBackend { routes });
            Some(Fusion::from_config(&cfg, backend).map_err(|e| anyhow!("{e}"))?)
        };

        Ok(Self {
            panel: fusion,
            synthesizer,
            synth_model,
            label,
        })
    }

    /// Clone `req` with its model retargeted to the synthesizer.
    fn synth_request(&self, mut req: ChatRequest) -> ChatRequest {
        req.model = self.synth_model.clone();
        req
    }
}

/// Render the conversation into a single query string for the panel members
/// (who do not see the structured message history or tools).
fn render_query(messages: &[ChatMessage]) -> String {
    let mut parts = Vec::new();
    for m in messages {
        match m.role {
            Role::System => {}
            Role::User => parts.push(format!("User: {}", m.content)),
            Role::Assistant if !m.content.is_empty() => {
                parts.push(format!("Assistant: {}", m.content))
            }
            Role::Assistant => {}
            Role::Tool => parts.push(format!(
                "[tool {} result] {}",
                m.tool_name.as_deref().unwrap_or(""),
                m.content
            )),
        }
    }
    parts.join("\n\n")
}

/// Build the synthesizer guidance message that carries the panel's drafts.
fn build_synth_guidance<'a>(drafts: impl IntoIterator<Item = (&'a String, &'a String)>) -> String {
    let mut s = String::from(
        "Several expert models independently drafted answers to the user's latest request. \
Synthesize the single best response: resolve disagreements with reasoning, keep what is \
correct, and discard what is wrong. Use tools as normal if the task requires action. \
Drafts:\n\n",
    );
    for (name, answer) in drafts {
        s.push_str(&format!("[{name}]\n{answer}\n\n"));
    }
    s
}

#[async_trait]
impl LlmProvider for FusionProvider {
    async fn health(&self) -> Result<()> {
        // The synthesizer is the critical path; panel failures degrade
        // gracefully (an unreachable member just contributes nothing).
        self.synthesizer.health().await
    }

    async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
        self.synthesizer
            .supports_native_tools(&self.synth_model)
            .await
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        self.synthesizer.list_models().await
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream> {
        let Some(panel) = &self.panel else {
            return self.synthesizer.chat_stream(self.synth_request(req)).await;
        };

        let query = render_query(&req.messages);
        let drafts = panel.run_panel(&query, false, &mut |_| {}).await;

        let mut synth_req = self.synth_request(req);
        if !drafts.is_empty() {
            synth_req
                .messages
                .push(ChatMessage::system(build_synth_guidance(drafts.iter())));
        }
        self.synthesizer.chat_stream(synth_req).await
    }

    async fn context_window(&self, _model: &str) -> Option<u32> {
        self.synthesizer.context_window(&self.synth_model).await
    }

    fn label(&self) -> String {
        self.label.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ChatChunk;
    use futures_util::stream;
    use std::sync::Mutex;

    /// A stub provider that records the requests it sees and replies with a
    /// fixed, single-chunk answer derived from its tag.
    struct StubProvider {
        tag: String,
        seen: Arc<Mutex<Vec<ChatRequest>>>,
    }

    impl StubProvider {
        fn new(tag: &str) -> (Arc<Self>, Arc<Mutex<Vec<ChatRequest>>>) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    tag: tag.to_string(),
                    seen: seen.clone(),
                }),
                seen,
            )
        }
    }

    #[async_trait]
    impl LlmProvider for StubProvider {
        async fn health(&self) -> Result<()> {
            Ok(())
        }
        async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
            Ok(true)
        }
        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(vec![self.tag.clone()])
        }
        async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream> {
            self.seen.lock().unwrap().push(req.clone());
            let chunk = ChatChunk {
                message: Some(ChatMessage::assistant(format!("answer from {}", self.tag))),
                thinking: false,
                done: true,
                done_reason: Some("stop".to_string()),
                eval_count: None,
                prompt_eval_count: None,
            };
            Ok(Box::pin(stream::iter(vec![Ok(chunk)])))
        }
        fn label(&self) -> String {
            self.tag.clone()
        }
    }

    fn user_req(text: &str, with_tool: bool) -> ChatRequest {
        let tools = if with_tool {
            vec![crate::llm::ToolSpec::function(
                "noop",
                "does nothing",
                serde_json::json!({"type": "object"}),
            )]
        } else {
            Vec::new()
        };
        ChatRequest {
            model: "ignored".to_string(),
            messages: vec![ChatMessage::user(text)],
            tools,
            stream: true,
            options: None,
        }
    }

    #[tokio::test]
    async fn panel_advises_and_only_synthesizer_streams_and_sees_tools() {
        let (a, a_seen) = StubProvider::new("alice");
        let (b, b_seen) = StubProvider::new("bob");
        let (synth, synth_seen) = StubProvider::new("synth");

        let panel = vec![
            PanelMember {
                name: "alice".to_string(),
                provider: a,
                model: "m-alice".to_string(),
            },
            PanelMember {
                name: "bob".to_string(),
                provider: b,
                model: "m-bob".to_string(),
            },
        ];
        let fusion =
            FusionProvider::new(panel, synth, "m-synth".to_string(), 1, "fusion: test".to_string())
                .unwrap();

        let out = collect_text(fusion.chat_stream(user_req("Q", true)).await.unwrap())
            .await
            .unwrap();
        // The final stream is the synthesizer's answer.
        assert_eq!(out, "answer from synth");

        // Panel members were consulted (1 initial + 1 review each), never
        // received tools, and were each routed to their own model.
        assert_eq!(a_seen.lock().unwrap().len(), 2);
        assert_eq!(b_seen.lock().unwrap().len(), 2);
        for req in a_seen.lock().unwrap().iter() {
            assert!(req.tools.is_empty(), "panel members must not get tools");
            assert_eq!(req.model, "m-alice", "alice routed to her model");
        }
        for req in b_seen.lock().unwrap().iter() {
            assert!(req.tools.is_empty(), "panel members must not get tools");
            assert_eq!(req.model, "m-bob", "bob routed to his model");
        }

        // The synthesizer ran once, kept the tools, and got the drafts injected.
        let synth_calls = synth_seen.lock().unwrap();
        assert_eq!(synth_calls.len(), 1);
        let sreq = &synth_calls[0];
        assert_eq!(sreq.model, "m-synth");
        assert_eq!(sreq.tools.len(), 1, "synthesizer is the sole tool-caller");
        let injected = sreq
            .messages
            .iter()
            .any(|m| matches!(m.role, Role::System) && m.content.contains("answer from alice"));
        assert!(injected, "panel drafts injected into the synthesis request");
    }

    #[tokio::test]
    async fn empty_panel_degrades_to_synthesizer_alone() {
        let (synth, synth_seen) = StubProvider::new("synth");
        let fusion =
            FusionProvider::new(Vec::new(), synth, "m-synth".to_string(), 1, "fusion".to_string())
                .unwrap();
        let out = collect_text(fusion.chat_stream(user_req("Q", false)).await.unwrap())
            .await
            .unwrap();
        assert_eq!(out, "answer from synth");
        assert_eq!(synth_seen.lock().unwrap().len(), 1);
        assert_eq!(synth_seen.lock().unwrap()[0].model, "m-synth");
    }
}
