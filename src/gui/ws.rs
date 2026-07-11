//! The per-task WebSocket: streams the task's [`Frame`]s to the client and
//! applies client frames (`user_message`, `cancel`, `plan_verdict`,
//! `interview_answers`) to the managed task.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::agent::PlanVerdict;
use crate::gui::tasks::{Frame, TaskManager, TaskShared, TurnRequest};

/// A client→server frame (see `docs/gui-protocol.md`).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientFrame {
    UserMessage {
        text: String,
        #[serde(default)]
        model: Option<String>,
    },
    Cancel,
    PlanVerdict {
        approve: bool,
        #[serde(default)]
        feedback: Option<String>,
    },
    InterviewAnswers {
        answers: Option<Vec<String>>,
    },
}

/// Drive one attached socket until it closes: replay/attach through
/// [`TaskShared::attach`], forward buffered and live frames out, and apply
/// inbound client frames. On disconnect the subscription is detached, which
/// auto-approves a held plan and skips a held interview (gateway behavior —
/// a dropped reviewer must never hang the turn).
pub async fn serve(socket: WebSocket, shared: Arc<TaskShared>, state: Arc<super::GuiState>) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut frames) = mpsc::unbounded_channel::<String>();
    let generation = shared.attach(tx);

    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Some(text) => {
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                // Replaced by a newer socket for this task.
                None => break,
            },
            inbound = stream.next() => match inbound {
                Some(Ok(Message::Text(text))) => {
                    if let Some(reply) = apply(&shared, &state.manager, &text) {
                        let reply = serde_json::to_string(&reply).expect("frames serialize");
                        if sink.send(Message::Text(reply.into())).await.is_err() {
                            break;
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                // Pings are answered by the protocol layer; binary is not
                // part of the protocol.
                Some(Ok(_)) => {}
            },
        }
    }

    shared.detach(generation);
}

/// Apply one inbound client frame. The returned frame, if any, is a direct
/// reply to this socket only (protocol errors like "turn in progress" are
/// not part of the task's replayable stream).
fn apply(shared: &TaskShared, manager: &TaskManager, text: &str) -> Option<Frame> {
    let frame: ClientFrame = match serde_json::from_str(text) {
        Ok(frame) => frame,
        Err(err) => {
            return Some(Frame::Error {
                message: format!("unrecognized frame: {err}"),
            });
        }
    };
    match frame {
        ClientFrame::UserMessage { text, model } => manager
            .submit_turn(&shared.id, TurnRequest { text, model })
            .err()
            .map(|message| Frame::Error { message }),
        ClientFrame::Cancel => {
            shared.cancel_turn();
            None
        }
        ClientFrame::PlanVerdict { approve, feedback } => {
            let verdict = if approve {
                PlanVerdict::approve()
            } else {
                PlanVerdict::reject(feedback.unwrap_or_default())
            };
            (!shared.resolve_plan(verdict)).then(|| Frame::Error {
                message: "no plan awaiting a verdict".to_string(),
            })
        }
        ClientFrame::InterviewAnswers { answers } => {
            (!shared.resolve_interview(answers)).then(|| Frame::Error {
                message: "no interview awaiting answers".to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_frames_parse_the_protocol_shapes() {
        let m: ClientFrame =
            serde_json::from_str(r#"{ "type": "user_message", "text": "hi" }"#).unwrap();
        assert!(matches!(m, ClientFrame::UserMessage { text, model: None } if text == "hi"));

        let m: ClientFrame =
            serde_json::from_str(r#"{ "type": "user_message", "text": "hi", "model": "claude" }"#)
                .unwrap();
        assert!(
            matches!(m, ClientFrame::UserMessage { model: Some(model), .. } if model == "claude")
        );

        let m: ClientFrame = serde_json::from_str(r#"{ "type": "cancel" }"#).unwrap();
        assert!(matches!(m, ClientFrame::Cancel));

        let m: ClientFrame = serde_json::from_str(
            r#"{ "type": "plan_verdict", "approve": false, "feedback": "no" }"#,
        )
        .unwrap();
        assert!(
            matches!(m, ClientFrame::PlanVerdict { approve: false, feedback: Some(f) } if f == "no")
        );

        // `answers: null` skips the interview.
        let m: ClientFrame =
            serde_json::from_str(r#"{ "type": "interview_answers", "answers": null }"#).unwrap();
        assert!(matches!(m, ClientFrame::InterviewAnswers { answers: None }));

        let m: ClientFrame =
            serde_json::from_str(r#"{ "type": "interview_answers", "answers": ["a", "b"] }"#)
                .unwrap();
        assert!(matches!(m, ClientFrame::InterviewAnswers { answers: Some(a) } if a.len() == 2));
    }
}
