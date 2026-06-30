//! Plan mode's clarifying-questions hatch: the `interview` tool.
//!
//! After exploring the codebase read-only, the model can pause to ask the
//! user a short batch of clarifying questions before committing to a plan —
//! the same "interview" step Claude Code offers in plan mode. The tool is
//! read-only (so the plan-mode gate lets it through), emits an
//! [`AgentEvent::Interview`] carrying the questions, and blocks until the
//! surface answers: the TUI renders an interactive Q&A modal; headless
//! runners, the gateway, and the fleet have no interactive user and decline,
//! which the tool reports back as "proceed with your best judgment" rather
//! than an error. In omakase mode the chef decides for themselves, so the
//! tool refuses to ask at all.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::agent::{AgentEvent, InterviewQuestion};

use super::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput, parse_args};

/// Advertised name of the tool.
pub const INTERVIEW_TOOL_NAME: &str = "interview";

/// Most questions a single interview may carry; keeps the model from dumping
/// an unbounded questionnaire on the user.
const MAX_QUESTIONS: usize = 6;

/// `interview` — ask the user a batch of clarifying questions during plan
/// mode and feed their answers back to the model. Always registered (so the
/// model sees it documented); meaningful only while a surface with an
/// interactive user is attached.
pub struct InterviewTool {
    /// Omakase flag, shared with the agent. When set, the chef makes its own
    /// calls and the tool declines to interview.
    omakase: Arc<AtomicBool>,
}

impl InterviewTool {
    pub fn new(omakase: Arc<AtomicBool>) -> Self {
        Self { omakase }
    }
}

#[derive(Deserialize)]
struct Args {
    questions: Vec<QuestionArg>,
}

#[derive(Deserialize)]
struct QuestionArg {
    question: String,
    #[serde(default)]
    options: Vec<String>,
}

/// Render the questions the model asked, so it can proceed using its own
/// judgment when no answers come back (no surface, or the user declined).
fn unanswered(questions: &[InterviewQuestion], reason: &str) -> ToolOutput {
    let mut out = format!("{reason} Proceed using your best judgment for:\n");
    for (i, q) in questions.iter().enumerate() {
        let _ = write!(out, "\n{}. {}", i + 1, q.question);
    }
    ToolOutput::ok(out)
}

#[async_trait]
impl Tool for InterviewTool {
    fn name(&self) -> &str {
        INTERVIEW_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Ask the user a short batch of clarifying questions during plan mode, \
         before you commit to a plan. Use it only when you have genuine open \
         questions whose answers would change your approach (scope, \
         trade-offs, ambiguous intent) — not for things you can determine by \
         reading the code. Each question may offer suggested options; the user \
         can pick one or write their own. Their answers come back as the tool \
         result. Read-only, so it works mid-plan."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "The clarifying questions to ask, in order \
                                    (at most 6).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {
                                "type": "string",
                                "description": "The question to put to the user."
                            },
                            "options": {
                                "type": "array",
                                "description": "Optional suggested answers the \
                                                user can pick from; omit for a \
                                                free-text question.",
                                "items": { "type": "string" }
                            }
                        },
                        "required": ["question"]
                    }
                }
            },
            "required": ["questions"]
        })
    }

    fn access(&self) -> ToolAccess {
        // Read-only: it gathers information and makes no changes, so the
        // plan-mode gate lets it through.
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Args { questions } = parse_args(INTERVIEW_TOOL_NAME, args)?;

        let questions: Vec<InterviewQuestion> = questions
            .into_iter()
            .map(|q| InterviewQuestion {
                question: q.question.trim().to_string(),
                options: q
                    .options
                    .into_iter()
                    .map(|o| o.trim().to_string())
                    .filter(|o| !o.is_empty())
                    .collect(),
            })
            .filter(|q| !q.question.is_empty())
            .collect();

        if questions.is_empty() {
            return Ok(ToolOutput::error(
                "no questions provided — call interview with at least one question",
            ));
        }
        if questions.len() > MAX_QUESTIONS {
            return Ok(ToolOutput::error(format!(
                "too many questions ({}) — ask at most {MAX_QUESTIONS} at a time",
                questions.len()
            )));
        }

        // Omakase: the chef decides. Don't put questions to the user.
        if self.omakase.load(Ordering::SeqCst) {
            return Ok(unanswered(
                &questions,
                "Omakase mode is on — you have full authority and should not \
                 interview the user.",
            ));
        }

        let Some(events) = ctx.events.clone() else {
            // No surface (subagent / direct execution): nobody to answer.
            return Ok(unanswered(
                &questions,
                "No interactive user is available to answer.",
            ));
        };

        let (respond, answers) = oneshot::channel();
        if events
            .send(AgentEvent::Interview {
                questions: questions.clone(),
                respond,
            })
            .await
            .is_err()
        {
            return Ok(unanswered(
                &questions,
                "The interview could not be presented (no surface).",
            ));
        }

        match answers.await {
            Ok(Some(answers)) => {
                let mut out = String::from("The user answered your questions:\n");
                for (i, q) in questions.iter().enumerate() {
                    let answer = answers.get(i).map(String::as_str).unwrap_or("").trim();
                    let answer = if answer.is_empty() {
                        "(skipped)"
                    } else {
                        answer
                    };
                    let _ = write!(out, "\nQ: {}\nA: {answer}\n", q.question);
                }
                out.push_str("\nIncorporate these answers into your plan, then call exit_plan.");
                Ok(ToolOutput::ok(out))
            }
            Ok(None) => Ok(unanswered(
                &questions,
                "The user dismissed the interview without answering.",
            )),
            Err(_) => Ok(unanswered(
                &questions,
                "The interview ended without answers.",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;

    fn flag(on: bool) -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(on))
    }

    #[tokio::test]
    async fn rejects_an_empty_question_set() {
        let tool = InterviewTool::new(flag(false));
        let out = tool
            .execute(json!({ "questions": [] }), &ToolContext::new("/tmp"))
            .await
            .expect("executes");
        assert!(out.is_error, "{}", out.content);
    }

    #[tokio::test]
    async fn omakase_declines_to_ask() {
        let tool = InterviewTool::new(flag(true));
        let (tx, mut rx) = mpsc::channel(8);
        let ctx = ToolContext::new("/tmp").with_events(tx);
        let out = tool
            .execute(json!({ "questions": [{ "question": "which db?" }] }), &ctx)
            .await
            .expect("executes");
        assert!(!out.is_error);
        assert!(out.content.contains("Omakase"), "{}", out.content);
        assert!(out.content.contains("which db?"), "{}", out.content);
        // No event was emitted: the chef does not interview.
        assert!(rx.try_recv().is_err(), "no interview event in omakase");
    }

    #[tokio::test]
    async fn no_surface_falls_back_to_best_judgment() {
        let tool = InterviewTool::new(flag(false));
        let out = tool
            .execute(
                json!({ "questions": [{ "question": "which db?" }] }),
                &ToolContext::new("/tmp"),
            )
            .await
            .expect("executes");
        assert!(!out.is_error);
        assert!(out.content.contains("best judgment"), "{}", out.content);
    }

    #[tokio::test]
    async fn answers_are_paired_with_questions() {
        let tool = InterviewTool::new(flag(false));
        let (tx, mut rx) = mpsc::channel(8);
        let ctx = ToolContext::new("/tmp").with_events(tx);

        let responder = async {
            let Some(AgentEvent::Interview { questions, respond }) = rx.recv().await else {
                panic!("expected Interview event");
            };
            assert_eq!(questions.len(), 2);
            assert_eq!(questions[0].options, vec!["sqlite", "postgres"]);
            respond
                .send(Some(vec!["postgres".to_string(), String::new()]))
                .expect("answers sent");
        };
        let (out, ()) = tokio::join!(
            tool.execute(
                json!({ "questions": [
                    { "question": "which db?", "options": ["sqlite", "postgres"] },
                    { "question": "any auth?" }
                ] }),
                &ctx
            ),
            responder
        );
        let out = out.expect("executes");
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("A: postgres"), "{}", out.content);
        // The skipped second answer renders explicitly.
        assert!(out.content.contains("A: (skipped)"), "{}", out.content);
    }

    #[tokio::test]
    async fn dismissed_interview_falls_back() {
        let tool = InterviewTool::new(flag(false));
        let (tx, mut rx) = mpsc::channel(8);
        let ctx = ToolContext::new("/tmp").with_events(tx);
        let responder = async {
            let Some(AgentEvent::Interview { respond, .. }) = rx.recv().await else {
                panic!("expected Interview event");
            };
            respond.send(None).expect("decline sent");
        };
        let (out, ()) = tokio::join!(
            tool.execute(json!({ "questions": [{ "question": "x?" }] }), &ctx),
            responder
        );
        let out = out.expect("executes");
        assert!(!out.is_error);
        assert!(out.content.contains("best judgment"), "{}", out.content);
    }
}
