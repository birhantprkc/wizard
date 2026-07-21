//! The agent turn loop, top to bottom: append the user's prompt, then loop —
//! stream a completion, emit deltas, execute tool calls, feed the results
//! back — until the model stops calling tools (or a step cap, the time
//! limit, the circuit breaker, or an interrupt ends the turn).
//!
//! Only the loop and its immediate drivers live here. The machinery the loop
//! calls — prompt assembly, compaction, the tool registry, usage recording,
//! background-task drains — stays in [`super`].

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::config::Mode;
use crate::hooks::PromptSubmit;
use crate::llm::{ChatMessage, ChatOptions, ChatRequest, Image, Role, ToolCall};
use crate::tools::ToolOutput;

use super::{
    Agent, AgentEvent, DoneReason, EMPTY_COMPLETION_NUDGE, ImageSource, LoopControl, absorb_images,
    breaker, clear_loop_control, completion_is_empty, emit, error_is_transient,
    parse_json_tool_call, read_loop_control, ultra,
};

/// One streamed completion, or the turn's cancellation observed mid-stream.
pub(super) enum Completion {
    Done {
        content: String,
        tool_calls: Vec<ToolCall>,
        /// Images the model produced inline in this reply, in arrival order.
        images: Vec<Image>,
    },
    Cancelled,
}

impl Agent {
    /// Run one user turn: append `input`, then loop
    /// (stream completion → emit deltas → execute tool calls → feed results
    /// back) until the model stops calling tools — or, when `max_steps` is
    /// capped, until the budget runs out.
    /// Always finishes with [`AgentEvent::Done`]. Each message is appended
    /// to the session file as it lands.
    pub async fn run_turn(
        &mut self,
        input: &str,
        events: mpsc::Sender<AgentEvent>,
    ) -> Result<DoneReason> {
        self.run_turn_with_images(input, Vec::new(), events).await
    }

    /// Like [`Self::run_turn`], but attach filesystem image paths to the user
    /// message for vision-capable models.
    pub async fn run_turn_with_images(
        &mut self,
        input: &str,
        images: Vec<std::path::PathBuf>,
        events: mpsc::Sender<AgentEvent>,
    ) -> Result<DoneReason> {
        if let Some(warning) = self.load_warning.take() {
            let _ = emit(&events, AgentEvent::Error(warning)).await;
        }
        // Arm cancellation for this turn; a stale request from a previous
        // turn must not kill this one. Same for the background-promote gate
        // so a leftover Ctrl-B does not instantly background the first
        // command of the next turn.
        self.cancel.clear();
        self.background.clear();
        self.usage.begin_turn();
        let result = match self.turn_inner(input, &images, &events).await {
            Ok(reason) => {
                let _ = emit(&events, AgentEvent::Done { reason }).await;
                Ok(reason)
            }
            Err(err) => {
                let _ = emit(&events, AgentEvent::Error(format!("{err:#}"))).await;
                let _ = emit(
                    &events,
                    AgentEvent::Done {
                        reason: DoneReason::Stopped,
                    },
                )
                .await;
                Err(err)
            }
        };
        // However the turn ended, ultra's guidance goes with it.
        self.drop_ultra_guidance();
        // turn_end hooks: observational, fired however the turn ended.
        self.hooks.turn_end(self.mode, Some(&events)).await;
        self.record_turn_usage();
        result
    }

    async fn turn_inner(
        &mut self,
        input: &str,
        images: &[std::path::PathBuf],
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<DoneReason> {
        // user_prompt_submit hooks: may veto the turn before the model sees
        // the prompt (the message is never pushed to history), or append
        // extra context to it.
        let input = match self
            .hooks
            .user_prompt_submit_with_prompt(input, self.mode, Some(events))
            .await
        {
            PromptSubmit::Block(reason) => {
                let _ = emit(
                    events,
                    AgentEvent::Error(format!(
                        "prompt blocked by user_prompt_submit hook: {reason}"
                    )),
                )
                .await;
                return Ok(DoneReason::Stopped);
            }
            PromptSubmit::Continue(Some(extra)) => {
                format!("{input}\n\n[user_prompt_submit hook]\n{extra}")
            }
            PromptSubmit::Continue(None) => input.to_string(),
        };
        // Turn boundary: a fresh checkpoint turn for the dispatcher's
        // snapshots, anchored in the session file so /rewind can truncate
        // here. Best-effort — a marker failure never blocks the turn.
        let turn = self.checkpoints.begin_turn();
        if let Err(err) = self.session.append_marker(turn, &input) {
            tracing::warn!("could not append turn marker: {err}");
        }
        // Images the user attached (pasted into the TUI, passed on the command
        // line). They are read off disk here — size-capped and media-typed from
        // their bytes — and ride the user message as `Image`s, the same shape a
        // tool's or the model's own images travel in. One that cannot be read
        // is reported and skipped: the rest of the turn still runs.
        let mut attachments: Vec<crate::llm::Image> = Vec::with_capacity(images.len());
        for path in images {
            match crate::llm::Image::from_path(path) {
                Ok(image) => attachments.push(image),
                Err(err) => {
                    let notice = format!("could not attach {}: {err}", path.display());
                    tracing::warn!("{notice}");
                    emit(events, AgentEvent::Notice(notice)).await;
                }
            }
        }
        if attachments.is_empty() {
            self.push(ChatMessage::user(input.clone()));
        } else {
            self.push(ChatMessage::user_with_images(input.clone(), attachments));
        }

        // Ultra: the mixture-of-agents pre-phase. Candidates propose and judges
        // compare *before* the main loop starts, so their conclusions enter the
        // turn as one system note and the loop below — the only thing in this
        // session that may write — proceeds unchanged.
        //
        // Position is load-bearing. After the user push, so a cancellation here
        // leaves history exactly as a cancelled model stream does. Before
        // `compact_if_needed`, so a large guidance block is *accounted for* by
        // the compactor instead of overflowing the window behind it. `run`
        // borrows `self` immutably and hands back an owned outcome, so those
        // borrows are over by the time history needs `&mut self`.
        if let Some(engine) = self.ultra.clone() {
            let outcome = ultra::run(
                &engine,
                &input,
                // The history as it stood *before* this request: it is already
                // pushed, and a candidate must not read its own brief twice.
                &self.history[..self.history.len() - 1],
                &self.client,
                &self.model,
                self.dispatcher.registry(),
                &self.hooks,
                // Bare: `ultra::run` wires this turn's event channel into the
                // context itself, since that is what its candidates' panes hang
                // off (see its doc comment).
                &self.ctx,
                &self.cancel,
                events,
            )
            .await;
            match outcome {
                ultra::UltraOutcome::Guidance(guidance) => {
                    // The drafts and the verdict, verbatim, for the surface to
                    // keep: the candidates' panes retire off the rail long
                    // before this turn ends, and the guidance itself is never
                    // rendered, so without this the work the user just paid N×
                    // for would be unreadable everywhere.
                    let _ = emit(
                        events,
                        AgentEvent::UltraGuidance {
                            label: engine.label(),
                            guidance: guidance.clone(),
                        },
                    )
                    .await;
                    // History only, never the session: this is advice about the
                    // *one* request below it, so it is dropped again at the end
                    // of the turn (`drop_ultra_guidance`) and must not come back
                    // on `/resume` either. `push` would persist it as a system
                    // note, which is exactly what we do not want.
                    self.history.push(ChatMessage::system(guidance));
                }
                ultra::UltraOutcome::Skipped(reason) => {
                    let _ = emit(events, AgentEvent::Notice(format!("ultra: {reason}"))).await;
                }
                ultra::UltraOutcome::Cancelled => return Ok(DoneReason::Stopped),
            }
        }

        self.compact_if_needed(events).await;
        // Unlimited by default: the turn runs until the model stops calling
        // tools. An interrupt, the time limit, the circuit breaker and the
        // sovereign loop-control file all still end it; only a configured
        // `max_steps` ends it in `DoneReason::MaxSteps`.
        let max_steps = self.config.max_steps.last_step();

        for step in 1..=max_steps {
            // Surface background tasks that finished since the last step.
            self.drain_background_tasks(events).await;
            self.drain_background_subagents(events).await;
            if let Some(deadline) = self.deadline
                && Instant::now() >= deadline
            {
                return Ok(DoneReason::TimeLimit);
            }
            if self.mode == Mode::Sovereign
                && let Some(reason) = self.honor_loop_control().await
            {
                return Ok(reason);
            }
            // Plan mode can flip mid-turn (exit_plan approval): keep the
            // system prompt's plan-mode block in step with the flag.
            self.sync_plan_prompt();

            let (mut content, mut tool_calls, mut images) =
                match self.stream_completion_with_retry(events).await {
                    // Cancelled mid-stream: the partial completion is discarded
                    // (never entered history), so nothing dangles.
                    Ok(Completion::Cancelled) => return Ok(DoneReason::Stopped),
                    Ok(Completion::Done {
                        content,
                        tool_calls,
                        images,
                    }) => (content, tool_calls, images),
                    // Endpoint breaker open: end the turn as a circuit breaker
                    // (rolled back and clean in sovereign) rather than a hard
                    // error.
                    Err(err) if err.is::<breaker::LlmBreakerOpen>() => {
                        return Ok(DoneReason::CircuitBreaker);
                    }
                    Err(err) => return Err(err),
                };

            // Some reasoning models (xAI grok-4.3 after tool results) emit
            // only reasoning and stop, leaving the visible message empty.
            // Nudge once; if it stays empty, surface a notice instead of
            // ending the turn silently. A reply that produced an image but no
            // text is not empty — it said what it had to say in pixels.
            if images.is_empty() && completion_is_empty(&content, &tool_calls) {
                // In-memory only (not `push`): the nudge must not pollute the
                // persisted session history.
                self.history.push(ChatMessage::user(EMPTY_COMPLETION_NUDGE));
                let retried = self.stream_completion_with_retry(events).await;
                self.history.pop();
                let (retry_content, retry_calls, retry_images) = match retried {
                    Ok(Completion::Cancelled) => return Ok(DoneReason::Stopped),
                    Ok(Completion::Done {
                        content,
                        tool_calls,
                        images,
                    }) => (content, tool_calls, images),
                    Err(err) if err.is::<breaker::LlmBreakerOpen>() => {
                        return Ok(DoneReason::CircuitBreaker);
                    }
                    Err(err) => return Err(err),
                };
                if retry_images.is_empty() && completion_is_empty(&retry_content, &retry_calls) {
                    let _ = emit(
                        events,
                        AgentEvent::Error("model returned an empty response".to_string()),
                    )
                    .await;
                    return Ok(DoneReason::Completed);
                }
                content = retry_content;
                tool_calls = retry_calls;
                images = retry_images;
            }

            // Images the model generated: persisted and announced before the
            // assistant message lands, so what history carries is exactly what
            // the surfaces were told about.
            let images = absorb_images(images, self.ctx.images.as_ref(), Some(events), |images| {
                AgentEvent::Images {
                    source: ImageSource::Assistant,
                    images,
                }
            })
            .await;

            let assistant = ChatMessage {
                role: Role::Assistant,
                content: content.clone(),
                tool_calls: tool_calls.clone(),
                tool_name: None,
                images,
            };
            self.push(assistant);

            if !self.native_tools
                && tool_calls.is_empty()
                && let Some(call) = parse_json_tool_call(&content)
            {
                tool_calls.push(call);
            }

            if tool_calls.is_empty() {
                return Ok(DoneReason::Completed);
            }

            for (index, call) in tool_calls.iter().enumerate() {
                // Cancellation is honored between tool calls: pending calls
                // are answered so the persisted assistant message never
                // carries dangling tool_use.
                if self.cancel.is_cancelled() {
                    self.answer_skipped_calls(
                        &tool_calls[index..],
                        "(not executed — interrupted by user)",
                    );
                    return Ok(DoneReason::Stopped);
                }
                if let Some(reason) = self.dispatch_call(call, events).await {
                    // dispatch_call answered `call` itself; the rest of the
                    // batch never ran.
                    self.answer_skipped_calls(
                        &tool_calls[index + 1..],
                        "(not executed — turn ended early)",
                    );
                    return Ok(reason);
                }
            }

            // Compact between steps too, so a long tool loop cannot outgrow
            // the context window mid-turn. The compactor always keeps the
            // most recent messages verbatim, so the in-flight turn's tail —
            // the tool calls and results the model is reasoning about —
            // stays intact.
            self.compact_if_needed(events).await;

            if !emit(events, AgentEvent::StepCompleted { step }).await {
                return Ok(DoneReason::Stopped);
            }
        }

        Ok(DoneReason::MaxSteps)
    }

    /// Stream one completion, forwarding text deltas and collecting tool
    /// calls. Observes the turn's cancel handle so an interrupt lands at the
    /// next chunk (or immediately when the stream is idle).
    async fn stream_completion(&self, events: &mpsc::Sender<AgentEvent>) -> Result<Completion> {
        if self.cancel.is_cancelled() {
            return Ok(Completion::Cancelled);
        }
        let request = ChatRequest {
            model: self.model.clone(),
            messages: self.history.clone(),
            tools: if self.native_tools {
                self.dispatcher.registry().specs()
            } else {
                Vec::new()
            },
            stream: true,
            options: Some(ChatOptions {
                temperature: Some(self.mode.temperature()),
                num_ctx: None,
                reasoning_effort: self
                    .config
                    .reasoning_effort
                    .map(|effort| effort.as_str().to_string()),
            }),
        };

        let mut stream = self
            .client
            .chat_stream(request)
            .await
            .context("starting chat completion")?;

        let mut content = String::new();
        let mut tool_calls = Vec::new();
        let mut images = Vec::new();
        let mut prompt_tokens = None;
        let mut completion_tokens = None;
        loop {
            let chunk = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Ok(Completion::Cancelled),
                chunk = stream.next() => match chunk {
                    Some(chunk) => chunk,
                    None => break,
                },
            };
            let chunk = chunk.context("reading chat stream")?;
            // Images the model generated (see `ChatChunk::images`). They are
            // collected here and taken in by `absorb_images` once the reply is
            // complete, so a cancelled or retried stream leaves nothing behind.
            images.extend(chunk.images);
            if let Some(message) = chunk.message {
                if !message.content.is_empty() {
                    if chunk.thinking {
                        // Reasoning is surfaced to the UI but never becomes
                        // part of the assistant message.
                        let _ = emit(events, AgentEvent::ThinkingDelta(message.content)).await;
                    } else {
                        content.push_str(&message.content);
                        let _ = emit(events, AgentEvent::TextDelta(message.content)).await;
                    }
                }
                images.extend(message.images);
                tool_calls.extend(message.tool_calls);
            }
            if chunk.prompt_eval_count.is_some() {
                prompt_tokens = chunk.prompt_eval_count;
            }
            if chunk.eval_count.is_some() {
                completion_tokens = chunk.eval_count;
            }
            if chunk.done {
                break;
            }
        }
        if prompt_tokens.is_some() || completion_tokens.is_some() {
            self.usage.record(prompt_tokens, completion_tokens);
            let _ = emit(
                events,
                AgentEvent::Usage {
                    prompt_tokens: prompt_tokens.unwrap_or(0),
                    completion_tokens: completion_tokens.unwrap_or(0),
                },
            )
            .await;
        }
        Ok(Completion::Done {
            content,
            tool_calls,
            images,
        })
    }

    /// [`stream_completion`] with sleep-and-wake exponential backoff so a
    /// transient LLM outage (server down, rate-limited, mid-stream drop)
    /// pauses and retries instead of aborting the run. In continuous mode it
    /// retries indefinitely; otherwise it gives up after ~6 attempts. A
    /// non-transient error (auth, bad request, missing model) fails the turn
    /// immediately with the provider's message — in continuous mode too.
    pub(super) async fn stream_completion_with_retry(
        &self,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<Completion> {
        let mut attempt: u32 = 0;
        loop {
            // Fail fast when the endpoint breaker is open (tripped this turn or
            // a prior one): don't dial a provider that is down. Past the
            // cooldown, `check` admits a single recovery probe.
            if let Err(open) = self.llm_breaker.check() {
                let _ = emit(
                    events,
                    AgentEvent::Error(format!(
                        "LLM circuit breaker open; provider still unavailable (retry in {}s)",
                        open.retry_after.as_secs()
                    )),
                )
                .await;
                return Err(breaker::LlmBreakerOpen {
                    retry_after: open.retry_after,
                }
                .into());
            }
            match self.stream_completion(events).await {
                Ok(result) => {
                    // A cancelled completion is a user interrupt, not a
                    // provider outcome — it must not count toward the breaker.
                    if !matches!(result, Completion::Cancelled) {
                        self.llm_breaker.record(breaker::Outcome::Success);
                    }
                    return Ok(result);
                }
                Err(err) => {
                    if !error_is_transient(&err) {
                        // Permanent (auth, bad request, missing model): fails
                        // the turn now and never feeds the breaker.
                        return Err(err);
                    }
                    self.llm_breaker.record(breaker::Outcome::Failure);
                    if !self.config.continuous && attempt >= 6 {
                        return Err(err);
                    }
                    // If that failure just tripped the breaker, end now (mapped
                    // to DoneReason::CircuitBreaker) rather than sleeping a full
                    // backoff before the next `check` would catch it — this is
                    // what bounds continuous mode's otherwise-infinite retry.
                    if self.llm_breaker.is_open() {
                        let _ = emit(
                            events,
                            AgentEvent::Error(
                                "LLM circuit breaker tripped after repeated failures; ending"
                                    .to_string(),
                            ),
                        )
                        .await;
                        return Err(breaker::LlmBreakerOpen {
                            retry_after: self.llm_breaker.cooldown(),
                        }
                        .into());
                    }
                    let secs = self.config.retry_max_secs.min(
                        self.config
                            .retry_base_secs
                            .saturating_mul(2u64.saturating_pow(attempt)),
                    );
                    let n = attempt + 1;
                    // The retry re-generates the response from the top; tell
                    // consumers to drop any partial text this attempt streamed.
                    let _ = emit(events, AgentEvent::StreamRetrying).await;
                    let _ = emit(
                        events,
                        AgentEvent::Error(format!(
                            "LLM unavailable ({err:#}); sleeping {secs}s then retrying (attempt {n})"
                        )),
                    )
                    .await;
                    // The backoff sleep is also cancellable.
                    tokio::select! {
                        biased;
                        _ = self.cancel.cancelled() => return Ok(Completion::Cancelled),
                        _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
                    }
                    attempt += 1;
                }
            }
        }
    }

    /// Run one tool call through the dispatcher and feed its results back to
    /// the model. Returns `Some(reason)` when the turn must end early (UI
    /// gone, circuit breaker).
    async fn dispatch_call(
        &mut self,
        call: &ToolCall,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Option<DoneReason> {
        let name = call.function.name.clone();
        let outcome = self.dispatcher.dispatch(call, &self.ctx, events).await;
        let images = match &outcome.output {
            Some(output) => {
                self.push(self.tool_feedback(&name, output));
                output.images.clone()
            }
            // No result (event receiver gone mid-batch): answer the call
            // anyway so the persisted history carries no dangling tool_use.
            None => {
                self.push(
                    self.tool_feedback(&name, &ToolOutput::ok("(not executed — turn ended early)")),
                );
                Vec::new()
            }
        };

        // Images a tool returned ride back to the model on a follow-up user
        // message: user messages carry images uniformly across every provider,
        // whereas a `tool` result cannot on OpenAI. A non-vision model simply
        // ignores the attachment. They are persisted and announced first, so
        // the surfaces see them attached to the tool card that produced them.
        if !images.is_empty() {
            let tool = name.clone();
            let images = absorb_images(images, self.ctx.images.as_ref(), Some(events), |images| {
                AgentEvent::Images {
                    source: ImageSource::Tool(tool),
                    images,
                }
            })
            .await;
            if !images.is_empty() {
                self.push(ChatMessage::user_with_images(
                    format!("Image(s) returned by `{name}`:"),
                    images,
                ));
            }
        }

        if let Some(nudge) = outcome.nudge {
            self.push(ChatMessage::system(nudge));
        }
        outcome.done
    }

    /// Answer tool calls that will never run (turn ended early, user
    /// interrupt) with a synthesized `note`, so the already-persisted
    /// assistant message carries no dangling tool_use.
    fn answer_skipped_calls(&mut self, calls: &[ToolCall], note: &str) {
        for call in calls {
            self.push(self.tool_feedback(&call.function.name, &ToolOutput::ok(note)));
        }
    }

    /// Build the message that feeds a tool result back to the model.
    fn tool_feedback(&self, name: &str, output: &ToolOutput) -> ChatMessage {
        let body = if output.is_error {
            format!("Error: {}", output.content)
        } else {
            output.content.clone()
        };
        if self.native_tools {
            ChatMessage::tool_result(name, body)
        } else {
            ChatMessage::user(format!("Tool result for `{name}`:\n{body}"))
        }
    }

    /// Honor `.wizard/loop-control` between steps: `stop` ends the turn,
    /// `pause` blocks until released, `skip` injects an instruction to move
    /// on. Returns `Some(reason)` when the turn must end.
    async fn honor_loop_control(&mut self) -> Option<DoneReason> {
        loop {
            match read_loop_control(&self.ctx.cwd) {
                Some(LoopControl::Stop) => {
                    clear_loop_control(&self.ctx.cwd);
                    return Some(DoneReason::Stopped);
                }
                Some(LoopControl::Pause) => {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Some(LoopControl::Skip) => {
                    clear_loop_control(&self.ctx.cwd);
                    self.push(ChatMessage::user(
                        "Operator control: skip the current sub-task and move on to the next \
                         part of the task.",
                    ));
                    return None;
                }
                None => return None,
            }
        }
    }
}
