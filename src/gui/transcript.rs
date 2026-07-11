//! Session JSONL → GUI transcript replay (`GET /api/tasks/{id}`), plus the
//! one-line tool summaries shared with the live `tool_finished` frames.

use serde::Serialize;
use serde_json::Value;

use crate::agent::session::SessionEntry;
use crate::llm::Role;

/// Cap on a tool summary line — the GUI renders it muted next to the tool
/// name, so it must stay short.
const SUMMARY_CHARS: usize = 100;

/// One transcript item, in the protocol's shape (`kind`-tagged).
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    User {
        text: String,
    },
    TurnMarker {
        turn: u64,
        prompt: String,
    },
    Text {
        text: String,
    },
    Tool {
        name: String,
        args: Value,
        /// `None` for a call whose result never landed (interrupted run) —
        /// the field is omitted and the GUI renders the call as pending.
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<ItemOutput>,
    },
    Notice {
        text: String,
    },
}

/// The summarized result attached to a [`Item::Tool`] row.
#[derive(Debug, Serialize)]
pub struct ItemOutput {
    pub ok: bool,
    pub summary: String,
}

/// Map session entries (file order) to transcript items: user prompts, turn
/// markers, assistant narration, tool rows, and system-note notices. Tool
/// calls pair positionally with the `Tool`-role results that follow their
/// assistant message (system notes may interleave), mirroring
/// [`crate::agent::session::repair_dangling_tool_calls`].
pub fn replay(entries: &[SessionEntry]) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    // Indices into `items` of tool rows still awaiting their result.
    let mut pending: std::collections::VecDeque<usize> = std::collections::VecDeque::new();

    for entry in entries {
        let record = match entry {
            SessionEntry::Header(_) => continue,
            SessionEntry::Marker(marker) => {
                items.push(Item::TurnMarker {
                    turn: marker.turn,
                    prompt: marker.prompt.clone(),
                });
                continue;
            }
            SessionEntry::Message(record) => record,
        };
        let message = &record.message;
        match message.role {
            Role::User => {
                pending.clear();
                items.push(Item::User {
                    text: message.content.clone(),
                });
            }
            Role::System => {
                // Only flagged system notes are persisted mid-conversation
                // (stale system prompts from old files render the same way).
                items.push(Item::Notice {
                    text: message.content.clone(),
                });
            }
            Role::Assistant => {
                pending.clear();
                if !message.content.trim().is_empty() {
                    items.push(Item::Text {
                        text: message.content.clone(),
                    });
                }
                for call in &message.tool_calls {
                    pending.push_back(items.len());
                    items.push(Item::Tool {
                        name: call.function.name.clone(),
                        args: call.function.arguments.clone(),
                        output: None,
                    });
                }
            }
            Role::Tool => {
                let output = ItemOutput {
                    ok: replay_ok(&message.content),
                    summary: String::new(), // filled below with args in hand
                };
                match pending.pop_front() {
                    Some(index) => {
                        if let Item::Tool {
                            name,
                            args,
                            output: slot,
                        } = &mut items[index]
                        {
                            let summary = summarize_tool(name, args, &message.content);
                            *slot = Some(ItemOutput { summary, ..output });
                        }
                    }
                    None => {
                        // Orphan result (old/truncated file): render it as a
                        // bare tool row without arguments.
                        let name = message
                            .tool_name
                            .clone()
                            .unwrap_or_else(|| "tool".to_string());
                        let summary = summarize_tool(&name, &Value::Null, &message.content);
                        items.push(Item::Tool {
                            name,
                            args: Value::Object(serde_json::Map::new()),
                            output: Some(ItemOutput { summary, ..output }),
                        });
                    }
                }
            }
        }
    }
    items
}

/// Whether a replayed tool result looks successful. The session file does
/// not persist [`ToolOutput::is_error`](crate::tools::ToolOutput), so this
/// falls back to recognizing the dispatcher's failure phrasings; anything
/// else replays as ok.
fn replay_ok(content: &str) -> bool {
    let head = content.trim_start();
    !(head.starts_with("error")
        || head.starts_with("Error")
        || head.starts_with("unknown tool:")
        || head.starts_with("invalid arguments")
        || head.starts_with("blocked by")
        || head.starts_with("(not executed"))
}

/// One short human line describing a finished tool call: file paths and
/// counts where the tool has an obvious subject, otherwise the first line
/// of its output. Shared by the live `tool_finished` frames and the
/// transcript replay.
pub fn summarize_tool(name: &str, args: &Value, output: &str) -> String {
    let arg = |key: &str| args.get(key).and_then(Value::as_str).map(str::trim);
    let first_line = output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let lines = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();

    let summary = match name {
        "read_file" => {
            arg("path").map(|path| format!("{path} ({})", count(lines, "line", "lines")))
        }
        "write_file" | "edit_file" => arg("path").map(str::to_string),
        "list_files" | "search_files" => {
            let counted = if name == "list_files" {
                count(lines, "file", "files")
            } else {
                count(lines, "match", "matches")
            };
            Some(match arg("pattern").or_else(|| arg("path")) {
                Some(subject) => format!("{subject}: {counted}"),
                None => counted,
            })
        }
        "execute" => arg("command").map(|command| first_of(command).to_string()),
        "web_fetch" => arg("url").map(str::to_string),
        "web_search" => {
            arg("query").map(|query| format!("{query}: {}", count(lines, "result", "results")))
        }
        "spawn_subagent" => arg("name")
            .or_else(|| arg("task"))
            .map(|subject| first_of(subject).to_string()),
        _ => None,
    };

    let summary = summary.unwrap_or_else(|| {
        if first_line.is_empty() {
            "(no output)".to_string()
        } else {
            first_line.to_string()
        }
    });
    truncate_chars(&summary, SUMMARY_CHARS)
}

/// `3 lines`, `1 file`, ... — a count with the right noun form.
fn count(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {plural}")
    }
}

/// First line of `text`, trimmed.
fn first_of(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

/// Clip to `max` characters with an ellipsis.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(max.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent::session::{SessionHeader, SessionRecord, TurnMarker};
    use crate::llm::{ChatMessage, FunctionCall, ToolCall};

    fn message_entry(message: ChatMessage) -> SessionEntry {
        SessionEntry::Message(SessionRecord {
            timestamp: chrono::Utc::now(),
            message,
            system_note: false,
        })
    }

    fn assistant_with_calls(content: &str, calls: &[(&str, Value)]) -> ChatMessage {
        let mut message = ChatMessage::assistant(content);
        for (name, args) in calls {
            message.tool_calls.push(ToolCall {
                function: FunctionCall {
                    name: name.to_string(),
                    arguments: args.clone(),
                },
            });
        }
        message
    }

    #[test]
    fn replay_maps_kinds_and_pairs_tool_results() {
        let entries = vec![
            SessionEntry::Header(SessionHeader {
                timestamp: chrono::Utc::now(),
                cwd: "/tmp/project".to_string(),
            }),
            SessionEntry::Marker(TurnMarker {
                timestamp: chrono::Utc::now(),
                turn: 1,
                prompt: "read main".to_string(),
            }),
            message_entry(ChatMessage::user("read main")),
            message_entry(assistant_with_calls(
                "I'll read it.",
                &[("read_file", json!({ "path": "src/main.rs" }))],
            )),
            message_entry(ChatMessage::tool_result("read_file", "fn main() {}\n")),
            message_entry(ChatMessage::assistant("It's a stub.")),
        ];

        let items = replay(&entries);
        assert_eq!(items.len(), 5, "the header maps to no item: {items:?}");
        assert!(matches!(&items[0], Item::TurnMarker { turn: 1, prompt } if prompt == "read main"));
        assert!(matches!(&items[1], Item::User { text } if text == "read main"));
        assert!(matches!(&items[2], Item::Text { text } if text == "I'll read it."));
        match &items[3] {
            Item::Tool { name, args, output } => {
                assert_eq!(name, "read_file");
                assert_eq!(args["path"], "src/main.rs");
                let output = output.as_ref().expect("result paired");
                assert!(output.ok);
                assert_eq!(output.summary, "src/main.rs (1 line)");
            }
            other => panic!("expected a tool item, got {other:?}"),
        }
        assert!(matches!(&items[4], Item::Text { text } if text == "It's a stub."));
    }

    #[test]
    fn replay_pairs_multiple_calls_in_order_and_leaves_dangles_pending() {
        let entries = vec![
            message_entry(ChatMessage::user("go")),
            message_entry(assistant_with_calls(
                "",
                &[
                    ("read_file", json!({ "path": "a.rs" })),
                    ("execute", json!({ "command": "cargo check" })),
                ],
            )),
            // Only the first call got its result (interrupted run); a system
            // note interleaves like background-task reports do.
            message_entry(ChatMessage::system("[note] something finished")),
            message_entry(ChatMessage::tool_result("read_file", "contents")),
        ];

        let items = replay(&entries);
        // [0] user, [1] tool read_file, [2] tool execute, [3] the note.
        assert_eq!(items.len(), 4);
        assert!(matches!(&items[3], Item::Notice { text } if text.starts_with("[note]")));
        match (&items[1], &items[2]) {
            (
                Item::Tool { output: first, .. },
                Item::Tool {
                    name,
                    output: second,
                    ..
                },
            ) => {
                assert!(first.is_some(), "answered call carries its result");
                assert_eq!(name, "execute");
                assert!(second.is_none(), "dangling call replays without output");
            }
            other => panic!("expected two tool items, got {other:?}"),
        }
    }

    #[test]
    fn replay_flags_dispatcher_failures() {
        let entries = vec![
            message_entry(assistant_with_calls("", &[("execute", json!({}))])),
            message_entry(ChatMessage::tool_result(
                "execute",
                "invalid arguments for 'execute': missing field `command`",
            )),
        ];
        let items = replay(&entries);
        match &items[0] {
            Item::Tool {
                output: Some(output),
                ..
            } => assert!(!output.ok),
            other => panic!("expected a failed tool item, got {other:?}"),
        }
    }

    #[test]
    fn summaries_name_the_subject_and_count_output() {
        assert_eq!(
            summarize_tool("read_file", &json!({ "path": "src/app.rs" }), "a\nb\nc"),
            "src/app.rs (3 lines)"
        );
        assert_eq!(
            summarize_tool(
                "write_file",
                &json!({ "path": "src/gui/mod.rs" }),
                "wrote it"
            ),
            "src/gui/mod.rs"
        );
        assert_eq!(
            summarize_tool(
                "execute",
                &json!({ "command": "git status --short\n# extra" }),
                "clean"
            ),
            "git status --short"
        );
        assert_eq!(
            summarize_tool(
                "search_files",
                &json!({ "pattern": "TODO" }),
                "a.rs:1\nb.rs:9"
            ),
            "TODO: 2 matches"
        );
        assert_eq!(
            summarize_tool(
                "git_status",
                &json!({}),
                "On branch main\nnothing to commit"
            ),
            "On branch main"
        );
        assert_eq!(summarize_tool("todo", &json!({}), ""), "(no output)");
    }

    #[test]
    fn summaries_are_clipped() {
        let long = "x".repeat(400);
        let summary = summarize_tool("execute", &json!({ "command": long }), "");
        assert_eq!(summary.chars().count(), 100);
        assert!(summary.ends_with('…'));
    }
}
