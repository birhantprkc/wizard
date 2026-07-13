//! The per-task WebSocket: streams the task's [`Frame`]s to the client and
//! applies client frames (`user_message`, `cancel`, `plan_verdict`,
//! `interview_answers`) to the managed task.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::agent::PlanVerdict;
use crate::gui::server::verify_attachments;
use crate::gui::tasks::{CommandRequest, Frame, TaskManager, TaskShared, TurnRequest};

/// A client→server frame (see `docs/gui-protocol.md`).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientFrame {
    UserMessage {
        text: String,
        #[serde(default)]
        model: Option<String>,
        /// Paths `POST /api/tasks/{id}/upload` returned for images, i.e. files
        /// inside `~/.wizard/images/`. Verified again here: a path the client
        /// sends is client input whatever route first produced it.
        #[serde(default)]
        images: Vec<String>,
        /// The same for non-image attachments, inside `~/.wizard/attachments/`.
        #[serde(default)]
        files: Vec<String>,
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
    /// A server-side slash command (`GET /api/commands`, `where: "server"`).
    Command {
        name: String,
        #[serde(default)]
        args: String,
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
        ClientFrame::UserMessage {
            text,
            model,
            images,
            files,
        } => {
            // An attachment path is a path the *client* chose. Taken on trust
            // it is an arbitrary-file read — and, once `@`-expanded into the
            // prompt, a way to exfiltrate whatever it named.
            let (images, files) = match verify_attachments(&images, &files) {
                Ok(paths) => paths,
                Err(message) => return Some(Frame::Error { message }),
            };
            manager
                .submit_turn(
                    &shared.id,
                    TurnRequest {
                        text,
                        model,
                        images,
                        files,
                    },
                )
                .err()
                .map(|message| Frame::Error { message })
        }
        ClientFrame::Command { name, args } => manager
            .submit_command(&shared.id, CommandRequest { name, args })
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
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::config::Config;
    use crate::gui::settings::ConfigStore;

    /// The error message an inbound frame came back with, or `None` when it was
    /// accepted.
    fn refusal(text: &str) -> Option<String> {
        let shared = TaskShared::new(
            "2026-07-13T00-00-00".to_string(),
            PathBuf::from("/tmp/project"),
            "test-model".to_string(),
        );
        let manager = TaskManager::new(Arc::new(ConfigStore::new(Config::default())));
        match apply(&shared, &manager, text) {
            Some(Frame::Error { message }) => Some(message),
            _ => None,
        }
    }

    #[test]
    fn attachment_paths_outside_the_stores_are_refused_before_the_turn() {
        let images = Config::images_dir().unwrap().join("2026-07-13T00-00-00");
        std::fs::create_dir_all(&images).unwrap();
        let png = images.join("a1b2c3d4.png");
        std::fs::write(&png, [0x89, b'P', b'N', b'G']).unwrap();

        let secret = Config::wizard_dir().unwrap().join("credentials.toml");
        std::fs::write(&secret, "key = 'sk-1'").unwrap();

        // A path inside the store passes the guard. (The task is not live in
        // this bare manager, so the turn is refused *after* the check — which is
        // the point: the check happened first, and it did not fire.)
        let message = refusal(&format!(
            r#"{{ "type": "user_message", "text": "hi", "images": ["{}"] }}"#,
            png.display()
        ))
        .expect("no live task to run the turn");
        assert!(message.contains("not live"), "got: {message}");

        // An absolute path outside the store, a traversal out of it, and a
        // symlink pointing out of it: all refused, and none of them reaches the
        // turn.
        let traversal = images.join("../../credentials.toml");
        let symlink = images.join("escape.png");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, &symlink).ok();
        for path in [&secret, &traversal, &symlink] {
            let message = refusal(&format!(
                r#"{{ "type": "user_message", "text": "read this", "images": ["{}"] }}"#,
                path.display()
            ))
            .expect("refused");
            assert!(
                !message.contains("not live"),
                "the path was taken on trust: {message}"
            );
        }

        // The same for a file attachment: the attachment store is not the image
        // store, and neither is anywhere else on the disk.
        let message = refusal(&format!(
            r#"{{ "type": "user_message", "text": "read this", "files": ["{}"] }}"#,
            secret.display()
        ))
        .expect("refused");
        assert!(!message.contains("not live"), "got: {message}");
    }

    #[test]
    fn client_frames_parse_the_protocol_shapes() {
        let m: ClientFrame =
            serde_json::from_str(r#"{ "type": "user_message", "text": "hi" }"#).unwrap();
        assert!(
            matches!(m, ClientFrame::UserMessage { text, model: None, images, files }
                if text == "hi" && images.is_empty() && files.is_empty())
        );

        let m: ClientFrame =
            serde_json::from_str(r#"{ "type": "user_message", "text": "hi", "model": "claude" }"#)
                .unwrap();
        assert!(
            matches!(m, ClientFrame::UserMessage { model: Some(model), .. } if model == "claude")
        );

        let m: ClientFrame = serde_json::from_str(
            r#"{ "type": "user_message", "text": "look", "images": ["/i/a.png"],
                 "files": ["/f/spec.pdf"] }"#,
        )
        .unwrap();
        assert!(matches!(m, ClientFrame::UserMessage { images, files, .. }
            if images == ["/i/a.png"] && files == ["/f/spec.pdf"]));

        // `args` is optional: an argument-less command carries none.
        let m: ClientFrame =
            serde_json::from_str(r#"{ "type": "command", "name": "compact" }"#).unwrap();
        assert!(
            matches!(m, ClientFrame::Command { name, args } if name == "compact" && args.is_empty())
        );

        let m: ClientFrame =
            serde_json::from_str(r#"{ "type": "command", "name": "model", "args": "claude" }"#)
                .unwrap();
        assert!(
            matches!(m, ClientFrame::Command { name, args } if name == "model" && args == "claude")
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
