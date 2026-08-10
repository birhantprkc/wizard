//! Read Claude Code's on-disk session transcripts (`~/.claude/projects/`).
//!
//! [`crate::import_claude`] brings over Claude Code's *configuration* (MCP
//! servers, commands, spinner verbs). This module reads its *conversations*,
//! so a user can pick up a Claude Code session inside Wizard.
//!
//! # The format
//!
//! One session is one file: `~/.claude/projects/<slug>/<session-uuid>.jsonl`,
//! where `<slug>` is the session's working directory with every non
//! alphanumeric character replaced by `-` (see [`project_slug`]). Each line is
//! one JSON object with a `type` discriminant. Two facts about that file drive
//! the whole design:
//!
//! 1. **It is a DAG, not a list.** Message lines carry `uuid` and
//!    `parentUuid`. Editing a prompt or rewinding does not rewrite the file,
//!    it appends a *second* child under the same parent, so reading the file
//!    top to bottom interleaves branches that were never in the same
//!    conversation. The conversation is the parent chain walked back from a
//!    chosen leaf ([`ClaudeSession::resolve_chain`]); the `last-prompt` lines
//!    name the tip Claude Code itself would resume from.
//! 2. **`message` is Anthropic content-block shaped.** `tool_use` and
//!    `tool_result` blocks carry the real provider ids, which is what makes a
//!    faithful hand-off possible at all.
//!
//! # Read-only, structurally
//!
//! Nothing here may write under `~/.claude`: it is another program's live
//! state, and a half-written line would cost the user a conversation. That is
//! enforced rather than promised. Every filesystem call in this module is one
//! of `File::open`, `read_dir`, or an `is_dir`/`exists` probe, and
//! `no_write_api_reachable_from_this_module` fails the build if a write API
//! ever appears in this file.
//!
//! # Intermediate representation
//!
//! [`ClaudeNode`] and [`ClaudeBlock`] are deliberately Claude-Code-shaped, not
//! Wizard-shaped: converting to [`crate::llm::ChatMessage`] is a separate,
//! purely mechanical step so that this parser can be tested against real
//! transcripts without dragging Wizard's own transcript model into it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Line model
// ---------------------------------------------------------------------------

/// The line types that carry `uuid`/`parentUuid` and therefore take part in
/// the conversation DAG. Every other `type` is a sidecar record keyed only by
/// `sessionId` (see [`SessionMeta`]) and is collected separately.
///
/// The list is Claude Code's own: its transcript reader accepts exactly
/// `user`, `assistant`, `attachment`, `system` and `progress` as chain
/// members. Only the first four occur in transcripts observed locally;
/// `progress` is modelled so a session that has one does not lose the nodes
/// hanging off it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    /// A user turn: a typed prompt, a slash-command echo, or the `tool_result`
    /// blocks answering the previous assistant turn.
    User,
    /// An assistant turn: `text`, `thinking`, and `tool_use` blocks.
    Assistant,
    /// Context Claude Code injected around a turn (task reminders, skill
    /// listings, edited-file notices). Carries no `message`; the payload is in
    /// [`ClaudeNode::attachment`].
    Attachment,
    /// A local notice (turn duration, warnings). Carries no `message`; the
    /// text, when there is one, is in [`ClaudeNode::system_content`].
    System,
    /// A long-running-operation progress record.
    Progress,
}

impl NodeKind {
    /// The `type` string this kind is written as, or `None` for a type that
    /// does not take part in the chain.
    fn from_type(kind: &str) -> Option<Self> {
        match kind {
            "user" => Some(NodeKind::User),
            "assistant" => Some(NodeKind::Assistant),
            "attachment" => Some(NodeKind::Attachment),
            "system" => Some(NodeKind::System),
            "progress" => Some(NodeKind::Progress),
            _ => None,
        }
    }

    /// True for the two kinds that carry a model-visible message. The other
    /// three are local UI records: replaying them would put Claude Code's own
    /// chrome into Wizard's transcript.
    pub fn is_message(self) -> bool {
        matches!(self, NodeKind::User | NodeKind::Assistant)
    }
}

/// One Anthropic content block, kept in Claude Code's own vocabulary.
///
/// Blocks this build does not model land in [`ClaudeBlock::Other`] with their
/// JSON intact, so an unrecognised block is never silently dropped.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaudeBlock {
    Text(String),
    /// Extended thinking. `signature` is the provider's opaque attestation and
    /// is only meaningful when replayed to the same provider.
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    /// A tool call. `id` is the real `toolu_…` id that the matching
    /// [`ClaudeBlock::ToolResult`] refers to.
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// A tool result. `content` is left as JSON because Claude Code writes it
    /// both as a plain string and as a block array (text plus images).
    ToolResult {
        tool_use_id: String,
        content: Value,
        is_error: bool,
    },
    /// An image block; `source` is the untouched `{type, media_type, data}` or
    /// `{type, url}` object.
    Image {
        source: Value,
    },
    /// Any block type not modelled above, with its `type` and raw JSON.
    Other {
        kind: String,
        raw: Value,
    },
}

/// One line of a session file that takes part in the conversation DAG.
///
/// Fields are populated per [`NodeKind`]: `role`/`content`/`model` only on
/// [`NodeKind::User`] and [`NodeKind::Assistant`], `attachment` only on
/// [`NodeKind::Attachment`], `system_content`/`subtype` only on
/// [`NodeKind::System`].
#[derive(Debug, Clone)]
pub struct ClaudeNode {
    pub uuid: String,
    /// `None` for a root. A `Some` that names no line in the file is an
    /// *orphan*: normal after a `/clear` or a compaction, and reported as
    /// [`ChainStop::MissingParent`] rather than treated as an error.
    pub parent_uuid: Option<String>,
    pub kind: NodeKind,
    /// Position in the file, 0-based. Retained because file order is the only
    /// tie-breaker available between siblings whose timestamps collide.
    pub index: usize,
    pub timestamp: Option<DateTime<Utc>>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    /// True for lines belonging to a subagent's conversation rather than the
    /// main one. A resume must not replay these into the main transcript.
    pub is_sidechain: bool,
    /// True for lines Claude Code injected on the user's behalf (the caveat
    /// banner around a local command, for instance) rather than typed.
    pub is_meta: bool,
    /// True when the assistant line records an API failure rather than a
    /// model response.
    pub is_api_error: bool,
    /// `message.role`, verbatim.
    pub role: Option<String>,
    /// `message.content`, normalised: a bare string becomes a single
    /// [`ClaudeBlock::Text`].
    pub content: Vec<ClaudeBlock>,
    pub model: Option<String>,
    /// `message.id` (the `msg_…` id).
    pub message_id: Option<String>,
    pub stop_reason: Option<String>,
    /// `message.usage`, raw. Shapes here move faster than Wizard's own usage
    /// model, so it is carried rather than parsed.
    pub usage: Option<Value>,
    /// Claude Code's own structured record of what a tool returned (exit
    /// codes, diffs, interruption flags). Sits *beside* the `tool_result`
    /// block, which holds what the model saw.
    pub tool_use_result: Option<Value>,
    /// Payload of an [`NodeKind::Attachment`] line.
    pub attachment: Option<Value>,
    /// `subtype` of a [`NodeKind::System`] line (`turn_duration`, …).
    pub subtype: Option<String>,
    /// Text of a [`NodeKind::System`] line, when it has one.
    pub system_content: Option<String>,
}

impl ClaudeNode {
    /// True for a node a resume should replay: a real user or assistant
    /// message, in the main conversation, with something in it.
    ///
    /// Excludes sidechains (a subagent's private transcript), meta lines
    /// (Claude Code's own banners), empty messages, Claude Code's failed API
    /// calls, and the synthetic user turns it writes for its own slash
    /// commands, their output and its injected reminders.
    ///
    /// The last two were parsed and then never consulted. `is_api_error` was
    /// read out of `isApiErrorMessage` and used nowhere at all, and
    /// `is_synthetic_prompt` was used only to pick a picker title — so
    /// `wizard resume --claude` replayed `<command-name>/dashboard</command-name>`
    /// as something the user had typed and `API Error: 529 …` as something the
    /// model had said. Both then went into the Wizard session, into the
    /// system's idea of the conversation so far, and back to the provider on
    /// every subsequent turn.
    pub fn is_replayable(&self) -> bool {
        if !self.kind.is_message() || self.is_sidechain || self.is_meta || self.content.is_empty() {
            return false;
        }
        if self.is_api_error {
            return false;
        }
        // Only when the node is *nothing but* text. A user node also carries
        // `tool_result` blocks answering the assistant turn before it, and
        // those have no text at all — dropping them would strand every tool
        // call in the imported conversation with no result.
        if self.kind == NodeKind::User
            && self
                .content
                .iter()
                .all(|block| matches!(block, ClaudeBlock::Text(_)))
            && is_synthetic_prompt(&self.text())
        {
            return false;
        }
        true
    }

    /// Concatenated text of every [`ClaudeBlock::Text`] block, for previews
    /// and titles. Thinking and tool blocks are skipped.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            if let ClaudeBlock::Text(text) = block {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        out
    }
}

/// A `pr-link` sidecar line: a pull request opened during the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrLink {
    pub number: u64,
    pub repository: String,
    pub url: String,
}

/// The sidecar lines, which carry no `uuid` and are keyed only by session.
///
/// Claude Code appends a fresh copy of most of these after every turn rather
/// than rewriting the old one, so the **last** occurrence in file order is the
/// current value. That is exactly how these fields are folded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionMeta {
    /// `ai-title`: the model-generated session title shown in Claude Code's
    /// own resume picker.
    pub ai_title: Option<String>,
    /// `last-prompt.leafUuid`: the tip Claude Code would resume from.
    pub leaf_uuid: Option<String>,
    /// `last-prompt.lastPrompt`: the text of the prompt that produced it.
    pub last_prompt: Option<String>,
    /// `mode`: `normal`, `plan`, …
    pub mode: Option<String>,
    /// `permission-mode`: `default`, `acceptEdits`, …
    pub permission_mode: Option<String>,
    /// `agent-name`: set when the session is running as a named agent.
    pub agent_name: Option<String>,
    /// `pr-link`: every pull request recorded, in file order.
    pub pr_links: Vec<PrLink>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// The union of every field this module reads, across all of Claude Code's
/// line types.
///
/// Everything is optional on purpose: the file mixes a dozen line shapes and
/// gains new ones between Claude Code releases, so a missing field must mean
/// "not this kind of line", never a parse failure that costs the user the rest
/// of the transcript.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    uuid: Option<String>,
    parent_uuid: Option<String>,
    session_id: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    git_branch: Option<String>,
    is_sidechain: Option<bool>,
    is_meta: Option<bool>,
    is_api_error_message: Option<bool>,
    subtype: Option<String>,
    /// `system` lines put their text at the top level, not under `message`.
    content: Option<Value>,
    message: Option<RawMessage>,
    tool_use_result: Option<Value>,
    attachment: Option<Value>,
    // Sidecar lines.
    leaf_uuid: Option<String>,
    last_prompt: Option<String>,
    ai_title: Option<String>,
    mode: Option<String>,
    permission_mode: Option<String>,
    agent_name: Option<String>,
    pr_number: Option<u64>,
    pr_repository: Option<String>,
    pr_url: Option<String>,
}

/// The Anthropic message envelope. Its own fields are snake_case already, so
/// no renaming: only the enclosing [`RawLine`] is camelCase.
#[derive(Debug, Deserialize)]
struct RawMessage {
    id: Option<String>,
    role: Option<String>,
    model: Option<String>,
    stop_reason: Option<String>,
    usage: Option<Value>,
    content: Option<Value>,
}

/// Normalise `message.content` into blocks. A bare string (how Claude Code
/// writes a plain typed prompt) becomes a single text block so downstream code
/// has one shape to handle.
fn parse_blocks(content: &Value) -> Vec<ClaudeBlock> {
    match content {
        Value::String(text) => vec![ClaudeBlock::Text(text.clone())],
        Value::Array(items) => items.iter().map(parse_block).collect(),
        Value::Null => Vec::new(),
        other => vec![ClaudeBlock::Other {
            kind: String::new(),
            raw: other.clone(),
        }],
    }
}

fn parse_block(block: &Value) -> ClaudeBlock {
    let string = |key: &str| {
        block
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    match block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "text" => ClaudeBlock::Text(string("text")),
        "thinking" => ClaudeBlock::Thinking {
            thinking: string("thinking"),
            signature: block
                .get("signature")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        "tool_use" => ClaudeBlock::ToolUse {
            id: string("id"),
            name: string("name"),
            input: block.get("input").cloned().unwrap_or(Value::Null),
        },
        "tool_result" => ClaudeBlock::ToolResult {
            tool_use_id: string("tool_use_id"),
            content: block.get("content").cloned().unwrap_or(Value::Null),
            is_error: block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "image" => ClaudeBlock::Image {
            source: block.get("source").cloned().unwrap_or(Value::Null),
        },
        other => ClaudeBlock::Other {
            kind: other.to_string(),
            raw: block.clone(),
        },
    }
}

/// Accumulates lines into a [`ClaudeSession`]. Split out from
/// [`ClaudeSession::load`] so the streaming reader and the in-memory
/// [`ClaudeSession::parse`] share one ingest path and cannot drift.
#[derive(Default)]
struct Builder {
    nodes: Vec<ClaudeNode>,
    meta: SessionMeta,
    session_id: Option<String>,
    malformed_lines: usize,
    unknown_types: BTreeMap<String, usize>,
}

impl Builder {
    fn ingest(&mut self, index: usize, raw_line: &str) {
        if raw_line.trim().is_empty() {
            return;
        }
        // A session Claude Code is still appending to can end mid-line, and a
        // future release can write a line shape this build cannot decode.
        // Neither is worth discarding the rest of the transcript over: count
        // it and carry on.
        let Ok(line) = serde_json::from_str::<RawLine>(raw_line) else {
            self.malformed_lines += 1;
            return;
        };
        if self.session_id.is_none() {
            self.session_id = line.session_id.clone();
        }
        // A line with no `type` at all is damage, not a line shape from a
        // newer Claude Code: there is nothing to record it under. Counting it
        // as unknown would put an empty-string key in the diagnostic map.
        let Some(kind) = line.kind.clone() else {
            self.malformed_lines += 1;
            return;
        };

        if let Some(node_kind) = NodeKind::from_type(&kind) {
            // A chain-bearing type with no `uuid` cannot be placed in the DAG.
            let Some(uuid) = line.uuid.clone() else {
                self.malformed_lines += 1;
                return;
            };
            self.nodes.push(node(index, uuid, node_kind, line));
            return;
        }

        match kind.as_str() {
            // Sidecars are re-appended every turn; last write wins.
            "last-prompt" => {
                if let Some(leaf) = line.leaf_uuid {
                    self.meta.leaf_uuid = Some(leaf);
                }
                if let Some(prompt) = line.last_prompt {
                    self.meta.last_prompt = Some(prompt);
                }
            }
            "ai-title" => self.meta.ai_title = line.ai_title,
            "mode" => self.meta.mode = line.mode,
            "permission-mode" => self.meta.permission_mode = line.permission_mode,
            "agent-name" => self.meta.agent_name = line.agent_name,
            "pr-link" => {
                if let (Some(number), Some(repository), Some(url)) =
                    (line.pr_number, line.pr_repository, line.pr_url)
                {
                    self.meta.pr_links.push(PrLink {
                        number,
                        repository,
                        url,
                    });
                }
            }
            // Known, deliberately unmodelled: `file-history-snapshot` and
            // `file-history-delta` describe Claude Code's own file-backup
            // store, and `queue-operation` its input queue. Neither has a
            // Wizard counterpart, and both point at paths under `~/.claude`
            // that this module must not touch.
            "file-history-snapshot" | "file-history-delta" | "queue-operation" => {}
            other => *self.unknown_types.entry(other.to_string()).or_default() += 1,
        }
    }

    fn finish(self, path: PathBuf) -> ClaudeSession {
        // The filename is the session uuid, so it is the right fallback when
        // no line carried `sessionId`.
        let session_id = self.session_id.unwrap_or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default()
                .to_string()
        });
        // Duplicate uuids are not expected, but a truncated-then-reappended
        // file could produce them; first occurrence wins so the index always
        // points at the earliest definition.
        let mut index = HashMap::with_capacity(self.nodes.len());
        for (position, node) in self.nodes.iter().enumerate() {
            index.entry(node.uuid.clone()).or_insert(position);
        }
        ClaudeSession {
            path,
            session_id,
            nodes: self.nodes,
            meta: self.meta,
            malformed_lines: self.malformed_lines,
            unknown_types: self.unknown_types,
            index,
        }
    }
}

/// Build one [`ClaudeNode`] from a decoded chain-bearing line.
fn node(index: usize, uuid: String, kind: NodeKind, line: RawLine) -> ClaudeNode {
    let message = line.message;
    let content = message
        .as_ref()
        .and_then(|m| m.content.as_ref())
        .map(parse_blocks)
        .unwrap_or_default();
    ClaudeNode {
        uuid,
        parent_uuid: line.parent_uuid,
        kind,
        index,
        // Timestamps are RFC 3339 with a `Z` offset. An unparseable one is
        // dropped rather than defaulted: a wrong time sorts a picker wrong,
        // which is worse than a blank one.
        timestamp: line
            .timestamp
            .as_deref()
            .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
            .map(|stamped| stamped.with_timezone(&Utc)),
        cwd: line.cwd,
        git_branch: line.git_branch,
        is_sidechain: line.is_sidechain.unwrap_or(false),
        is_meta: line.is_meta.unwrap_or(false),
        is_api_error: line.is_api_error_message.unwrap_or(false),
        role: message.as_ref().and_then(|m| m.role.clone()),
        content,
        model: message.as_ref().and_then(|m| m.model.clone()),
        message_id: message.as_ref().and_then(|m| m.id.clone()),
        stop_reason: message.as_ref().and_then(|m| m.stop_reason.clone()),
        usage: message.and_then(|m| m.usage),
        tool_use_result: line.tool_use_result,
        attachment: line.attachment,
        subtype: line.subtype,
        system_content: match (kind, line.content) {
            (NodeKind::System, Some(Value::String(text))) => Some(text),
            _ => None,
        },
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// One parsed Claude Code session file.
///
/// `nodes` is in **file order**, which is not conversation order: siblings of
/// a branch are interleaved. Use [`ClaudeSession::resume_chain`] (or
/// [`ClaudeSession::resolve_chain`] with a chosen leaf) to get a conversation.
#[derive(Debug, Clone)]
pub struct ClaudeSession {
    pub path: PathBuf,
    /// `sessionId` as recorded, falling back to the file stem.
    pub session_id: String,
    /// Every chain-bearing line, in file order.
    pub nodes: Vec<ClaudeNode>,
    pub meta: SessionMeta,
    /// Lines that were not decodable JSON. A partially written tail line is
    /// normal for a session that is still running.
    pub malformed_lines: usize,
    /// `type` values this build does not model, with counts. Empty is the
    /// expected state; a non-empty map means Claude Code has grown a line
    /// shape worth looking at.
    pub unknown_types: BTreeMap<String, usize>,
    /// uuid → position in `nodes`. Private: it must stay in step with
    /// `nodes`, so a `ClaudeSession` is only ever built by parsing.
    index: HashMap<String, usize>,
}

/// Why a parent-chain walk stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainStop {
    /// Reached a line with `parentUuid: null`. The normal ending.
    Root,
    /// Reached a line whose parent is not in this file. Normal after a
    /// `/clear` or a compaction: the chain is complete as far as this file
    /// goes, and the named uuid is where the rest of it used to be.
    MissingParent(String),
    /// The walk arrived back at a line it had already visited. Cannot happen
    /// in a file Claude Code wrote; can happen in a corrupted or
    /// hand-edited one, and must not hang the reader.
    Cycle(String),
    /// The requested leaf names no line in this file, so the chain is empty.
    UnknownLeaf(String),
}

/// A conversation: one root-to-leaf path through the DAG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    /// Positions in [`ClaudeSession::nodes`], **root first**. Positions rather
    /// than references so a chain can be held alongside its session; resolve
    /// them with [`Chain::nodes`].
    pub positions: Vec<usize>,
    pub stop: ChainStop,
}

impl Chain {
    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    pub fn len(&self) -> usize {
        self.positions.len()
    }

    /// The chain's nodes, root first. Panic-free: positions come from the
    /// session that produced the chain.
    pub fn nodes<'s>(&self, session: &'s ClaudeSession) -> Vec<&'s ClaudeNode> {
        self.positions
            .iter()
            .filter_map(|&position| session.nodes.get(position))
            .collect()
    }

    /// The subset of [`Chain::nodes`] a resume should replay: real user and
    /// assistant messages, main conversation only. This is the conversation
    /// part two converts to [`crate::llm::ChatMessage`].
    pub fn replayable<'s>(&self, session: &'s ClaudeSession) -> Vec<&'s ClaudeNode> {
        self.positions
            .iter()
            .filter_map(|&position| session.nodes.get(position))
            .filter(|node| node.is_replayable())
            .collect()
    }
}

/// A line with more than one child: where an edit or a rewind branched the
/// history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchPoint {
    /// uuid of the shared parent. Not necessarily a line in this file: a
    /// compaction can drop the fork point while keeping both children.
    pub parent: String,
    /// uuids of its children, in file order (oldest branch first).
    pub children: Vec<String>,
}

impl ClaudeSession {
    /// Read and parse one session file.
    ///
    /// Streams the file a line at a time, so peak memory is one line plus the
    /// parsed session; it is `O(file size)` in time, which matters because
    /// real transcripts reach tens of megabytes. A caller listing many
    /// sessions should cache previews against `(path, len, mtime)` rather than
    /// re-reading.
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut reader = BufReader::new(file);
        let mut builder = Builder::default();
        let mut raw = Vec::new();
        let mut index = 0;
        loop {
            raw.clear();
            let read = reader
                .read_until(b'\n', &mut raw)
                .with_context(|| format!("reading {}", path.display()))?;
            if read == 0 {
                break;
            }
            // Decoded lossily rather than through `BufRead::lines`, whose
            // `Err(InvalidData)` on one bad byte would cost the whole file.
            // Everywhere else here a damaged line costs that line and nothing
            // more, and this is the one place that could have broken the rule:
            // the replacement characters make the line undecodable JSON, so it
            // lands in `malformed_lines` like any other unreadable line and the
            // transcript around it survives.
            builder.ingest(index, &String::from_utf8_lossy(&raw));
            index += 1;
        }
        Ok(builder.finish(path.to_path_buf()))
    }

    /// Parse an already-loaded session file. `path` is recorded but not read.
    pub fn parse(path: PathBuf, text: &str) -> Self {
        let mut builder = Builder::default();
        for (index, line) in text.lines().enumerate() {
            builder.ingest(index, line);
        }
        builder.finish(path)
    }

    /// The node with this uuid, if the file has one.
    pub fn node(&self, uuid: &str) -> Option<&ClaudeNode> {
        self.index.get(uuid).and_then(|&at| self.nodes.get(at))
    }

    /// Walk `parentUuid` from `leaf` back to a root and return the path, root
    /// first, together with why the walk stopped.
    ///
    /// Terminating is a hard requirement, not a nicety: this runs against a
    /// file another process is writing, and a cycle would otherwise hang the
    /// UI. Every visited uuid is recorded, so the walk cannot take more steps
    /// than the file has nodes, and a revisit ends it with
    /// [`ChainStop::Cycle`] and the chain built so far.
    pub fn resolve_chain(&self, leaf: &str) -> Chain {
        let Some(&start) = self.index.get(leaf) else {
            return Chain {
                positions: Vec::new(),
                stop: ChainStop::UnknownLeaf(leaf.to_string()),
            };
        };
        let mut seen: HashSet<&str> = HashSet::new();
        let mut walked = Vec::new();
        let mut at = start;
        let stop = loop {
            let node = &self.nodes[at];
            if !seen.insert(node.uuid.as_str()) {
                break ChainStop::Cycle(node.uuid.clone());
            }
            walked.push(at);
            let Some(parent) = node.parent_uuid.as_deref() else {
                break ChainStop::Root;
            };
            let Some(&next) = self.index.get(parent) else {
                break ChainStop::MissingParent(parent.to_string());
            };
            at = next;
        };
        walked.reverse();
        Chain {
            positions: walked,
            stop,
        }
    }

    /// The leaf a resume should start from: the last `last-prompt.leafUuid`
    /// when it names a line in this file, otherwise the last chain-bearing
    /// line in file order.
    ///
    /// The fallback matters. `last-prompt` is written after a turn completes,
    /// so a session killed mid-turn has a `leafUuid` pointing at an older tip,
    /// and a session that never finished a turn has none at all.
    pub fn tip(&self) -> Option<&str> {
        if let Some(leaf) = self.meta.leaf_uuid.as_deref()
            && self.index.contains_key(leaf)
        {
            return Some(leaf);
        }
        self.nodes.last().map(|node| node.uuid.as_str())
    }

    /// [`Self::resolve_chain`] from [`Self::tip`]; empty when the file has no
    /// chain-bearing lines at all.
    pub fn resume_chain(&self) -> Chain {
        match self.tip() {
            Some(leaf) => self.resolve_chain(leaf),
            None => Chain {
                positions: Vec::new(),
                stop: ChainStop::UnknownLeaf(String::new()),
            },
        }
    }

    /// Every line with `parentUuid: null`. More than one is ordinary: Claude
    /// Code starts a fresh root after `/clear` without starting a new file.
    pub fn roots(&self) -> Vec<&ClaudeNode> {
        self.nodes
            .iter()
            .filter(|node| node.parent_uuid.is_none())
            .collect()
    }

    /// Every line whose parent is named but absent from this file.
    pub fn orphans(&self) -> Vec<&ClaudeNode> {
        self.nodes
            .iter()
            .filter(|node| {
                node.parent_uuid
                    .as_deref()
                    .is_some_and(|parent| !self.index.contains_key(parent))
            })
            .collect()
    }

    /// Every uuid with more than one child, in file order of the parent: the
    /// points where an edit or a rewind forked the history.
    ///
    /// A parent that is not itself in the file still counts. Compaction drops
    /// the fork point while keeping both children, and a picker that ignored
    /// those would show a compacted-then-rewound session as unbranched while
    /// [`Self::leaves`] offered two tips.
    pub fn branch_points(&self) -> Vec<BranchPoint> {
        let mut children: HashMap<&str, Vec<String>> = HashMap::new();
        for node in &self.nodes {
            if let Some(parent) = node.parent_uuid.as_deref() {
                children.entry(parent).or_default().push(node.uuid.clone());
            }
        }
        let mut points: Vec<BranchPoint> = children
            .into_iter()
            .filter(|(_, kids)| kids.len() > 1)
            .map(|(parent, kids)| BranchPoint {
                parent: parent.to_string(),
                children: kids,
            })
            .collect();
        // Absent parents all share the `usize::MAX` key, and `children` is a
        // `HashMap`, so the uuid is the tie-breaker that keeps the order total
        // rather than whatever the hasher happened to produce this run.
        points.sort_by(|a, b| {
            let key =
                |point: &BranchPoint| self.index.get(&point.parent).copied().unwrap_or(usize::MAX);
            key(a).cmp(&key(b)).then_with(|| a.parent.cmp(&b.parent))
        });
        points
    }

    /// Every line with no children: the candidate tips a picker can offer when
    /// the user wants a branch other than the one Claude Code last used.
    /// In file order.
    pub fn leaves(&self) -> Vec<&ClaudeNode> {
        let parents: HashSet<&str> = self
            .nodes
            .iter()
            .filter_map(|node| node.parent_uuid.as_deref())
            .collect();
        self.nodes
            .iter()
            .filter(|node| !parents.contains(node.uuid.as_str()))
            .collect()
    }

    /// A picker row for this session.
    pub fn preview(&self) -> SessionPreview {
        let chain = self.resume_chain();
        let mut started = None;
        let mut updated = None;
        for node in &self.nodes {
            let Some(stamp) = node.timestamp else {
                continue;
            };
            // Min/max, not first/last: Claude Code writes a turn's lines in
            // dependency order, and their timestamps are not monotonic within
            // it (a caveat banner is stamped after the command it wraps).
            started = Some(started.map_or(stamp, |seen: DateTime<Utc>| seen.min(stamp)));
            updated = Some(updated.map_or(stamp, |seen: DateTime<Utc>| seen.max(stamp)));
        }
        let (title, title_source) = self.title();
        SessionPreview {
            path: self.path.clone(),
            session_id: self.session_id.clone(),
            title,
            title_source,
            started,
            updated,
            message_count: chain.replayable(self).len(),
            cwd: self.nodes.iter().find_map(|node| node.cwd.clone()),
            git_branch: self.nodes.iter().find_map(|node| node.git_branch.clone()),
            leaf_uuid: self.tip().map(str::to_string),
            branch_points: self.branch_points().len(),
        }
    }

    /// Title for a picker row: the model's own `ai-title` when the session got
    /// one, else the first thing the user actually typed, else the id.
    fn title(&self) -> (String, TitleSource) {
        if let Some(title) = self.meta.ai_title.as_deref()
            && !title.trim().is_empty()
        {
            return (clip(title), TitleSource::AiTitle);
        }
        let first = self.nodes.iter().find(|node| {
            node.kind == NodeKind::User
                && !node.is_meta
                && !node.is_sidechain
                && !is_synthetic_prompt(&node.text())
        });
        match first {
            Some(node) => (clip(&node.text()), TitleSource::FirstPrompt),
            None => (self.session_id.clone(), TitleSource::SessionId),
        }
    }
}

/// Where a [`SessionPreview::title`] came from, so a picker can style a
/// fallback differently from a real title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleSource {
    AiTitle,
    FirstPrompt,
    SessionId,
}

/// One row of a session picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPreview {
    pub path: PathBuf,
    pub session_id: String,
    pub title: String,
    pub title_source: TitleSource,
    /// Earliest and latest timestamp anywhere in the file.
    pub started: Option<DateTime<Utc>>,
    pub updated: Option<DateTime<Utc>>,
    /// Replayable messages on the resume chain, not in the whole file: what
    /// the user would actually get back, which on a branched session is fewer
    /// than the file holds.
    pub message_count: usize,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    /// The tip [`ClaudeSession::resume_chain`] would walk back from.
    pub leaf_uuid: Option<String>,
    /// How many places the history forked. Zero for an unedited session.
    pub branch_points: usize,
}

/// Longest title a picker row keeps, in characters.
const TITLE_CHARS: usize = 80;

/// Collapse whitespace and clip to [`TITLE_CHARS`], so a multi-line prompt
/// still renders as one row.
fn clip(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= TITLE_CHARS {
        return flat;
    }
    let mut out: String = flat.chars().take(TITLE_CHARS - 1).collect();
    out.push('\u{2026}');
    out
}

/// Tag prefixes Claude Code wraps around things the user did *not* type: slash
/// command echoes, their stdout, and injected reminders. A title built from
/// one of these would read as markup, so they are skipped when looking for the
/// first real prompt.
const SYNTHETIC_PROMPT_TAGS: [&str; 4] = [
    "local-command-",
    "command-",
    "system-reminder",
    "user-prompt-submit-hook",
];

/// True when `text` is one of Claude Code's synthetic user turns.
fn is_synthetic_prompt(text: &str) -> bool {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix('<') else {
        return trimmed.is_empty();
    };
    SYNTHETIC_PROMPT_TAGS
        .iter()
        .any(|tag| rest.starts_with(tag))
}

// ---------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------

/// Longest project directory name Claude Code writes before it truncates and
/// appends a hash. Matches the constant in its own path builder.
pub const PROJECT_SLUG_MAX: usize = 200;

/// The directory name Claude Code derives from a working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSlug {
    /// The directory name, or its leading [`PROJECT_SLUG_MAX`] characters when
    /// `exact` is false.
    pub name: String,
    /// True when `name` is the whole directory name.
    ///
    /// False for a working directory long enough that Claude Code truncated
    /// the slug and appended `-<hash>`. That hash is Bun's wyhash over the
    /// original path, which is not reproducible here, so long paths are
    /// matched on the prefix instead (see [`project_dir`]).
    pub exact: bool,
}

/// Slugify a working directory the way Claude Code does: every character that
/// is not an ASCII letter or digit becomes `-`, then the result is truncated
/// to [`PROJECT_SLUG_MAX`].
///
/// The replacement is per **UTF-16 code unit**, not per character, because the
/// original is a JavaScript regex over a JS string. It only shows up outside
/// the Basic Multilingual Plane: an emoji in a path name is two code units and
/// therefore two dashes, where a naive per-`char` pass would emit one.
pub fn project_slug(cwd: &str) -> ProjectSlug {
    let mut slug = String::with_capacity(cwd.len());
    for unit in cwd.encode_utf16() {
        match u8::try_from(unit) {
            Ok(byte) if byte.is_ascii_alphanumeric() => slug.push(char::from(byte)),
            _ => slug.push('-'),
        }
    }
    if slug.len() <= PROJECT_SLUG_MAX {
        ProjectSlug {
            name: slug,
            exact: true,
        }
    } else {
        // Every character in `slug` is ASCII, so a byte truncation is a
        // character truncation.
        slug.truncate(PROJECT_SLUG_MAX);
        ProjectSlug {
            name: slug,
            exact: false,
        }
    }
}

/// The project directory under `projects_root` holding `cwd`'s sessions, or
/// `None` when there is none.
///
/// For a short path this is a single `is_dir` probe. For a long one, where the
/// on-disk name ends in an unreproducible hash, the directory is found by
/// prefix; an ambiguous prefix (two long paths agreeing in their first
/// [`PROJECT_SLUG_MAX`] slug characters) resolves to none, and
/// [`list_sessions`] is the API that still copes, because it can check each
/// session's recorded `cwd`.
pub fn project_dir(projects_root: &Path, cwd: &str) -> Option<PathBuf> {
    let slug = project_slug(cwd);
    let direct = projects_root.join(&slug.name);
    if direct.is_dir() {
        return Some(direct);
    }
    if slug.exact {
        return None;
    }
    let mut matches = prefix_matches(projects_root, &slug.name);
    (matches.len() == 1).then(|| matches.remove(0))
}

/// Directories under `projects_root` whose name is `prefix` followed by
/// Claude Code's `-<hash>` suffix. Sorted, for a deterministic result.
fn prefix_matches(projects_root: &Path, prefix: &str) -> Vec<PathBuf> {
    let hashed = format!("{prefix}-");
    let Ok(entries) = std::fs::read_dir(projects_root) else {
        return Vec::new();
    };
    let mut matches: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&hashed))
        })
        .collect();
    matches.sort();
    matches
}

/// Every session file in one project directory, sorted by name.
///
/// Only `*.jsonl` files directly in the directory: Claude Code also puts
/// per-session subdirectories (`<uuid>/tool-results/`) and a `memory/`
/// directory alongside them.
pub fn session_files(project_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(project_dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        })
        .collect();
    files.sort();
    files
}

/// Picker rows for every Claude Code session recorded against `cwd`, most
/// recently updated first.
///
/// Best-effort throughout: a missing root, an unreadable file or an
/// undecodable one yields fewer rows, never an error. Each file is parsed in
/// full (see [`ClaudeSession::load`]), so this is `O(total bytes)`.
///
/// When the project directory had to be matched by prefix (a long working
/// directory, whose on-disk name ends in a hash this code cannot reproduce),
/// rows whose recorded `cwd` is not `cwd` are dropped: the prefix could
/// belong to a sibling path.
pub fn list_sessions(projects_root: &Path, cwd: &str) -> Vec<SessionPreview> {
    let slug = project_slug(cwd);
    let dirs = if projects_root.join(&slug.name).is_dir() {
        vec![projects_root.join(&slug.name)]
    } else if slug.exact {
        Vec::new()
    } else {
        prefix_matches(projects_root, &slug.name)
    };
    let verify_cwd = dirs.len() != 1 || !slug.exact;

    let mut previews: Vec<SessionPreview> = dirs
        .iter()
        .flat_map(|dir| session_files(dir))
        .filter_map(|path| ClaudeSession::load(&path).ok())
        .map(|session| session.preview())
        .filter(|preview| {
            !verify_cwd
                || preview
                    .cwd
                    .as_deref()
                    .is_none_or(|recorded| recorded == cwd)
        })
        .collect();
    // Most recent first; undated sessions sort last, then by path so the order
    // is total.
    previews.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.path.cmp(&b.path)));
    previews
}

/// Picker rows for the sessions Claude Code recorded against `cwd` on this
/// machine. Empty when Claude Code is not installed.
pub fn list_sessions_for_cwd(cwd: &str) -> Vec<SessionPreview> {
    match crate::import_claude::claude_projects_dir() {
        Some(root) => list_sessions(&root, cwd),
        None => Vec::new(),
    }
}

/// What a read-only guard needs, shared with the guards over this module's
/// *callers*.
///
/// `~/.claude` is read from more than one place now — [`list_sessions`] here,
/// [`crate::session_registry::claude_chats_in`] above it, and
/// [`crate::claude_resume::import`] beside it — and every one of them owes the
/// same proof: the tree it read is byte-for-byte the tree it found. One
/// snapshot function, so the three guards cannot drift into checking different
/// things.
#[cfg(test)]
pub(crate) mod tests_support {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    /// Every file under `dir`, with its bytes and its modification time.
    pub(crate) fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)> {
        let mut out = BTreeMap::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(snapshot(&path));
                continue;
            }
            let meta = std::fs::metadata(&path).expect("metadata");
            out.insert(
                path.clone(),
                (
                    std::fs::read(&path).expect("read"),
                    meta.modified().expect("mtime"),
                ),
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The redacted copies of real Claude Code sessions this module is tested
    /// against. Synthetic transcripts agree with whatever the parser happens
    /// to do; these do not.
    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/claude_sessions")
            .join(name)
    }

    fn branched() -> ClaudeSession {
        ClaudeSession::load(&fixture("branched.jsonl")).expect("load branched fixture")
    }

    fn linear() -> ClaudeSession {
        ClaudeSession::load(&fixture("linear.jsonl")).expect("load linear fixture")
    }

    // -- parsing ---------------------------------------------------------

    #[test]
    fn parses_every_line_of_a_real_session() {
        let session = branched();
        // The fixture is a verbatim structural copy of a 65-line session, so
        // any parse regression shows up as a changed count here.
        assert_eq!(session.nodes.len(), 44);
        assert_eq!(session.malformed_lines, 0);
        assert!(
            session.unknown_types.is_empty(),
            "unmodelled line types: {:?}",
            session.unknown_types
        );
        assert_eq!(
            session.session_id, "00000000-0000-4000-8000-000000000002",
            "sessionId comes off the lines, not the filename"
        );

        let kinds: Vec<NodeKind> = session.nodes.iter().map(|node| node.kind).collect();
        assert!(kinds.contains(&NodeKind::User));
        assert!(kinds.contains(&NodeKind::Assistant));
        assert!(kinds.contains(&NodeKind::Attachment));
        assert!(kinds.contains(&NodeKind::System));
    }

    #[test]
    fn parses_sidecar_lines_last_write_wins() {
        let session = branched();
        assert_eq!(
            session.meta.ai_title.as_deref(),
            Some("Redacted session title")
        );
        assert_eq!(session.meta.mode.as_deref(), Some("normal"));
        assert_eq!(
            session.meta.permission_mode.as_deref(),
            Some("bypassPermissions")
        );
        // The load-bearing one: six `last-prompt` lines, each naming a
        // different leaf as the session advanced. Folding must keep the last,
        // which is the tip the user left the session on. Keeping the first
        // would resume four turns of work earlier.
        assert_eq!(
            session.meta.leaf_uuid.as_deref(),
            Some("00000000-0000-4000-8000-000000000047")
        );
    }

    #[test]
    fn parses_tool_use_and_tool_result_with_real_ids() {
        let session = branched();
        let call = session
            .nodes
            .iter()
            .find_map(|node| {
                node.content.iter().find_map(|block| match block {
                    ClaudeBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
            })
            .expect("the fixture has tool calls");
        assert!(
            call.0.starts_with("toolu_"),
            "tool id preserved: {}",
            call.0
        );
        assert_eq!(call.1, "Bash");
        assert!(call.2.is_object(), "tool input stays JSON");

        // The answering result refers to that same id: the pairing survives.
        let paired = session.nodes.iter().any(|node| {
            node.content.iter().any(|block| {
                matches!(block, ClaudeBlock::ToolResult { tool_use_id, .. } if *tool_use_id == call.0)
            })
        });
        assert!(paired, "no tool_result refers to {}", call.0);
    }

    #[test]
    fn normalises_string_content_into_one_text_block() {
        let session = branched();
        // The opening prompt is written as a bare string, not a block array.
        let first = session
            .nodes
            .iter()
            .find(|node| node.kind == NodeKind::User && !node.is_meta)
            .expect("a user node");
        assert_eq!(first.content.len(), 1);
        assert!(matches!(first.content[0], ClaudeBlock::Text(_)));
    }

    #[test]
    fn keeps_thinking_and_out_of_band_payloads() {
        let session = branched();
        assert!(
            session.nodes.iter().any(|node| node
                .content
                .iter()
                .any(|block| matches!(block, ClaudeBlock::Thinking { .. }))),
            "thinking blocks parse"
        );
        assert!(
            session
                .nodes
                .iter()
                .any(|node| node.tool_use_result.is_some()),
            "toolUseResult is carried beside the tool_result block"
        );
        assert!(
            session
                .nodes
                .iter()
                .any(|node| node.kind == NodeKind::Attachment && node.attachment.is_some()),
            "attachment payloads are carried"
        );
        assert!(
            session
                .nodes
                .iter()
                .any(|node| node.kind == NodeKind::System && node.subtype.is_some()),
            "system subtypes are carried"
        );
    }

    #[test]
    fn tolerates_garbage_and_unknown_types_without_losing_the_file() {
        let text = concat!(
            r#"{"type":"user","uuid":"a","parentUuid":null,"message":{"role":"user","content":"hi"}}"#,
            "\n",
            "not json at all\n",
            "\n",
            r#"{"type":"brand-new-line-shape","sessionId":"s"}"#,
            "\n",
            r#"{"type":"assistant","uuid":"b","parentUuid":"a","message":{"role":"assistant","content":[{"type":"text","text":"yo"}]}}"#,
            "\n",
            // A chain type with no uuid cannot be placed, and counts as
            // malformed rather than silently vanishing.
            r#"{"type":"assistant","parentUuid":"b","message":{"role":"assistant","content":[]}}"#,
            "\n",
            // A line with no `type` is damage, and must not be filed under an
            // empty-string "unknown type" that a diagnostic would then print.
            r#"{"uuid":"c","parentUuid":"b"}"#,
        );
        let session = ClaudeSession::parse(PathBuf::from("s.jsonl"), text);
        assert_eq!(session.nodes.len(), 2);
        assert_eq!(session.malformed_lines, 3);
        assert_eq!(session.unknown_types.len(), 1);
        assert_eq!(session.unknown_types.get("brand-new-line-shape"), Some(&1));
    }

    #[test]
    fn one_undecodable_byte_costs_one_line_not_the_file() {
        // `BufRead::lines` fails the whole read on invalid UTF-8. Every other
        // damaged-input path here costs a single line, and a transcript is not
        // recoverable from anywhere else, so this one must too.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("torn.jsonl");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(
            br#"{"type":"user","uuid":"a","parentUuid":null,"message":{"role":"user","content":"hi"}}"#,
        );
        bytes.push(b'\n');
        // Damage in the structure, not inside a string literal: lossily
        // decoded it becomes U+FFFD where a key was expected, so the line is
        // undecodable JSON rather than a node with a mangled field.
        bytes.extend_from_slice(br#"{"type":"user","uuid":"b","#);
        bytes.push(0xff); // not valid UTF-8 in any position
        bytes.extend_from_slice(br#""parentUuid":"a","message":{"role":"user","content":"x"}}"#);
        bytes.push(b'\n');
        bytes.extend_from_slice(
            br#"{"type":"assistant","uuid":"c","parentUuid":"a","message":{"role":"assistant","content":[{"type":"text","text":"yo"}]}}"#,
        );
        bytes.push(b'\n');
        std::fs::write(&path, &bytes).expect("write");

        let session = ClaudeSession::load(&path).expect("a torn file still loads");
        let uuids: Vec<&str> = session.nodes.iter().map(|n| n.uuid.as_str()).collect();
        assert_eq!(uuids, vec!["a", "c"], "the lines around the damage survive");
        assert_eq!(session.malformed_lines, 1);
    }

    // -- the DAG ---------------------------------------------------------

    #[test]
    fn a_real_branch_yields_two_chains_that_share_a_prefix() {
        let session = branched();
        let points = session.branch_points();
        assert_eq!(points.len(), 1, "the fixture forks in exactly one place");
        let point = &points[0];
        assert_eq!(point.parent, "00000000-0000-4000-8000-000000000016");
        assert_eq!(point.children.len(), 2);

        // Both children resolve to full chains; both contain the shared
        // parent; neither contains the other's tip. That is the property a
        // flat top-to-bottom read gets wrong.
        let left = session.resolve_chain(&point.children[0]);
        let right = session.resolve_chain(&point.children[1]);
        assert_eq!(left.stop, ChainStop::Root);
        assert_eq!(right.stop, ChainStop::Root);
        for chain in [&left, &right] {
            assert!(
                chain
                    .nodes(&session)
                    .iter()
                    .any(|node| node.uuid == point.parent),
                "each branch descends from the fork"
            );
        }
        assert!(
            !left
                .nodes(&session)
                .iter()
                .any(|node| node.uuid == point.children[1])
        );
        assert!(
            !right
                .nodes(&session)
                .iter()
                .any(|node| node.uuid == point.children[0])
        );

        // The abandoned branch is genuinely shorter, which is why resuming
        // from the file's line count would over-count the conversation.
        let resume = session.resume_chain();
        assert!(resume.len() < session.nodes.len());
    }

    #[test]
    fn a_chain_is_root_first_and_parent_linked() {
        let session = branched();
        let chain = session.resume_chain();
        assert_eq!(chain.stop, ChainStop::Root);
        let nodes = chain.nodes(&session);
        assert!(nodes.len() > 10, "the fixture's main chain is long");
        assert!(nodes[0].parent_uuid.is_none(), "root first");
        for pair in nodes.windows(2) {
            assert_eq!(
                pair[1].parent_uuid.as_deref(),
                Some(pair[0].uuid.as_str()),
                "consecutive chain entries are parent → child"
            );
        }
        assert_eq!(nodes.last().map(|node| node.uuid.as_str()), session.tip());

        // `replayable` is the same chain minus Claude Code's own chrome. The
        // fixture's chain ends on a `system` line and opens on an attachment,
        // so this is a strict subset, and it is what part two maps.
        let replayable = chain.replayable(&session);
        assert!(!replayable.is_empty());
        assert!(replayable.len() < nodes.len());
        assert!(
            replayable
                .iter()
                .all(|node| node.kind.is_message() && !node.is_meta && !node.is_sidechain)
        );
    }

    #[test]
    fn tip_prefers_last_prompt_and_falls_back_to_file_order() {
        let session = linear();
        assert_eq!(
            session.tip(),
            Some("00000000-0000-4000-8000-000000000007"),
            "last-prompt names the tip"
        );

        // A session killed mid-turn has a leafUuid pointing at a line that is
        // no longer the tip, or none at all: fall back to the last node.
        let text = concat!(
            r#"{"type":"user","uuid":"a","parentUuid":null,"message":{"role":"user","content":"hi"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"b","parentUuid":"a","message":{"role":"assistant","content":[{"type":"text","text":"yo"}]}}"#,
        );
        let unfinished = ClaudeSession::parse(PathBuf::from("s.jsonl"), text);
        assert_eq!(unfinished.tip(), Some("b"));

        // A leafUuid naming a line this file does not have is ignored, not
        // obeyed: obeying it would resume an empty conversation.
        let stale = ClaudeSession::parse(
            PathBuf::from("s.jsonl"),
            &format!("{text}\n{}", r#"{"type":"last-prompt","leafUuid":"gone"}"#),
        );
        assert_eq!(stale.meta.leaf_uuid.as_deref(), Some("gone"));
        assert_eq!(stale.tip(), Some("b"));
    }

    #[test]
    fn a_cycle_terminates_instead_of_hanging() {
        // Claude Code cannot write this; a truncated-and-recovered file or a
        // hand edit can, and the reader must not spin on it.
        let text = concat!(
            r#"{"type":"user","uuid":"a","parentUuid":"c","message":{"role":"user","content":"1"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"b","parentUuid":"a","message":{"role":"assistant","content":[]}}"#,
            "\n",
            r#"{"type":"user","uuid":"c","parentUuid":"b","message":{"role":"user","content":"2"}}"#,
        );
        let session = ClaudeSession::parse(PathBuf::from("cycle.jsonl"), text);
        let chain = session.resolve_chain("c");
        assert_eq!(chain.stop, ChainStop::Cycle("c".to_string()));
        assert_eq!(chain.len(), 3, "every node once, none twice");
        let mut seen: Vec<&str> = chain
            .nodes(&session)
            .iter()
            .map(|node| node.uuid.as_str())
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, vec!["a", "b", "c"]);

        // The degenerate case: a node that is its own parent.
        let selfish = ClaudeSession::parse(
            PathBuf::from("cycle.jsonl"),
            r#"{"type":"user","uuid":"x","parentUuid":"x","message":{"role":"user","content":"1"}}"#,
        );
        let chain = selfish.resolve_chain("x");
        assert_eq!(chain.stop, ChainStop::Cycle("x".to_string()));
        assert_eq!(chain.len(), 1);

        // And a whole file that is one cycle still terminates through the
        // public entry point.
        assert!(!session.resume_chain().is_empty());
    }

    #[test]
    fn missing_parents_multiple_roots_and_orphans() {
        let text = concat!(
            // Root one, whose parent is simply absent.
            r#"{"type":"user","uuid":"a","parentUuid":null,"message":{"role":"user","content":"1"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"b","parentUuid":"a","message":{"role":"assistant","content":[]}}"#,
            "\n",
            // Root two: a fresh conversation in the same file, as after
            // `/clear`.
            r#"{"type":"user","uuid":"c","parentUuid":null,"message":{"role":"user","content":"2"}}"#,
            "\n",
            // An orphan: names a parent that this file does not contain.
            r#"{"type":"assistant","uuid":"d","parentUuid":"compacted-away","message":{"role":"assistant","content":[]}}"#,
        );
        let session = ClaudeSession::parse(PathBuf::from("s.jsonl"), text);
        let roots: Vec<&str> = session.roots().iter().map(|n| n.uuid.as_str()).collect();
        assert_eq!(roots, vec!["a", "c"]);
        let orphans: Vec<&str> = session.orphans().iter().map(|n| n.uuid.as_str()).collect();
        assert_eq!(orphans, vec!["d"]);

        let chain = session.resolve_chain("d");
        assert_eq!(
            chain.stop,
            ChainStop::MissingParent("compacted-away".to_string()),
            "a dangling parent ends the walk, it does not fail it"
        );
        assert_eq!(chain.len(), 1, "what this file does have is still returned");

        // A leaf that is not in the file at all is distinguishable from one
        // whose ancestry is.
        let unknown = session.resolve_chain("never-existed");
        assert_eq!(unknown.stop, ChainStop::UnknownLeaf("never-existed".into()));
        assert!(unknown.is_empty());

        // Two roots means two leaves plus the orphan.
        let leaves: Vec<&str> = session.leaves().iter().map(|n| n.uuid.as_str()).collect();
        assert_eq!(leaves, vec!["b", "c", "d"]);
    }

    #[test]
    fn a_fork_survives_the_loss_of_its_parent() {
        // Compaction rewrites the history above a point and keeps what hangs
        // below it. A session compacted and *then* rewound has two children of
        // a uuid the file no longer contains: still a fork the picker must
        // offer, even though there is no line to anchor it to.
        let text = concat!(
            r#"{"type":"user","uuid":"a","parentUuid":"compacted-away","message":{"role":"user","content":"1"}}"#,
            "\n",
            r#"{"type":"user","uuid":"b","parentUuid":"compacted-away","message":{"role":"user","content":"2"}}"#,
        );
        let session = ClaudeSession::parse(PathBuf::from("s.jsonl"), text);
        let points = session.branch_points();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].parent, "compacted-away");
        assert_eq!(points[0].children, vec!["a", "b"]);
        assert_eq!(session.preview().branch_points, 1);

        // Ordering is total even when several forks share the "not in this
        // file" sort key, which is otherwise at the mercy of the hasher.
        let text = format!(
            "{text}\n{}\n{}",
            r#"{"type":"user","uuid":"c","parentUuid":"also-gone","message":{"role":"user","content":"3"}}"#,
            r#"{"type":"user","uuid":"d","parentUuid":"also-gone","message":{"role":"user","content":"4"}}"#,
        );
        let session = ClaudeSession::parse(PathBuf::from("s.jsonl"), &text);
        let points = session.branch_points();
        let parents: Vec<&str> = points.iter().map(|point| point.parent.as_str()).collect();
        assert_eq!(parents, vec!["also-gone", "compacted-away"]);
    }

    #[test]
    fn an_empty_or_headerless_file_does_not_panic() {
        let empty = ClaudeSession::parse(PathBuf::from("empty.jsonl"), "");
        assert!(empty.nodes.is_empty());
        assert_eq!(empty.tip(), None);
        assert!(empty.resume_chain().is_empty());
        assert_eq!(
            empty.session_id, "empty",
            "the filename is the session id of last resort"
        );
        assert_eq!(empty.preview().title, "empty");
        assert_eq!(empty.preview().title_source, TitleSource::SessionId);
    }

    // -- previews --------------------------------------------------------

    #[test]
    fn preview_uses_the_ai_title_and_counts_only_the_resume_chain() {
        let session = branched();
        let preview = session.preview();
        assert_eq!(preview.title, "Redacted session title");
        assert_eq!(preview.title_source, TitleSource::AiTitle);
        assert_eq!(preview.branch_points, 1);
        assert_eq!(preview.cwd.as_deref(), Some("/home/user/projects/demo"));
        assert_eq!(preview.git_branch.as_deref(), Some("work"));
        assert!(preview.started <= preview.updated);
        assert!(preview.started.is_some());

        // Fewer than the file's replayable messages, because one branch was
        // abandoned. This is the number the picker must show.
        let all_replayable = session
            .nodes
            .iter()
            .filter(|node| node.is_replayable())
            .count();
        assert!(preview.message_count > 0);
        assert!(
            preview.message_count < all_replayable,
            "{} of {} messages are on the resume chain",
            preview.message_count,
            all_replayable
        );
    }

    #[test]
    fn preview_falls_back_to_the_first_real_prompt() {
        // No `ai-title`, and the first two user turns are Claude Code's own
        // slash-command plumbing: the title must be the third.
        let text = concat!(
            r#"{"type":"user","uuid":"a","parentUuid":null,"isMeta":true,"message":{"role":"user","content":"<local-command-caveat>ignore me</local-command-caveat>"}}"#,
            "\n",
            r#"{"type":"user","uuid":"b","parentUuid":"a","message":{"role":"user","content":"<command-name>/model</command-name>"}}"#,
            "\n",
            r#"{"type":"user","uuid":"c","parentUuid":"b","message":{"role":"user","content":"  explain\n  the parser  "}}"#,
        );
        let session = ClaudeSession::parse(PathBuf::from("s.jsonl"), text);
        let preview = session.preview();
        assert_eq!(preview.title, "explain the parser");
        assert_eq!(preview.title_source, TitleSource::FirstPrompt);
    }

    /// A resume replays the conversation, not Claude Code's own chrome.
    ///
    /// `is_synthetic_prompt` existed and was consulted only when picking a
    /// title; `is_api_error` was parsed off `isApiErrorMessage` and read
    /// nowhere at all. So `wizard resume --claude` imported
    /// `<command-name>/model</command-name>` as a user turn and `API Error:
    /// 529 …` as an assistant turn — then wrote both into the Wizard session,
    /// where they were re-sent to the provider on every later turn as things
    /// the two of them had supposedly said.
    #[test]
    fn slash_command_chrome_and_api_errors_are_not_conversation() {
        let text = concat!(
            r#"{"type":"user","uuid":"a","parentUuid":null,"message":{"role":"user","content":"rename the parser"}}"#,
            "\n",
            r#"{"type":"user","uuid":"b","parentUuid":"a","message":{"role":"user","content":"<command-name>/model</command-name>"}}"#,
            "\n",
            r#"{"type":"user","uuid":"c","parentUuid":"b","message":{"role":"user","content":"<system-reminder>be nice</system-reminder>"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"d","parentUuid":"c","isApiErrorMessage":true,"message":{"role":"assistant","content":[{"type":"text","text":"API Error: 529 overloaded"}]}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"e","parentUuid":"d","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}]}}"#,
            "\n",
            r#"{"type":"user","uuid":"f","parentUuid":"e","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"file body"}]}}"#,
        );
        let session = ClaudeSession::parse(PathBuf::from("s.jsonl"), text);
        let replayed: Vec<&str> = session
            .nodes
            .iter()
            .filter(|node| node.is_replayable())
            .map(|node| node.uuid.as_str())
            .collect();
        assert_eq!(
            replayed,
            vec!["a", "e", "f"],
            "only the real prompt, the real assistant turn, and the tool result \
             that answers it"
        );
    }

    #[test]
    fn long_titles_are_collapsed_and_clipped() {
        let long = "word ".repeat(60);
        let clipped = clip(&long);
        assert_eq!(clipped.chars().count(), TITLE_CHARS);
        assert!(clipped.ends_with('\u{2026}'));
        assert!(!clipped.contains("  "));
        // Multibyte input must not be cut mid-character.
        assert_eq!(clip("é".repeat(200).as_str()).chars().count(), TITLE_CHARS);
    }

    // -- slugs and enumeration -------------------------------------------

    #[test]
    fn project_slug_matches_the_directory_names_on_disk() {
        // Both of these are directory names observed under
        // `~/.claude/projects/`, and are why the rule is "non-alphanumeric
        // becomes a dash", not "path separators become dashes".
        assert_eq!(project_slug("/home/teddy").name, "-home-teddy");
        assert_eq!(
            project_slug("/home/teddy/projects/reactor").name,
            "-home-teddy-projects-reactor"
        );
        assert!(project_slug("/home/teddy").exact);
        // Dots, underscores and spaces are all just "not alphanumeric".
        assert_eq!(
            project_slug("/a/.config/my_app v2").name,
            "-a--config-my-app-v2"
        );
    }

    #[test]
    fn a_long_path_truncates_and_is_matched_by_prefix() {
        let deep = format!("/{}", "segment/".repeat(40));
        let slug = project_slug(&deep);
        assert!(!slug.exact);
        assert_eq!(slug.name.len(), PROJECT_SLUG_MAX);

        let root = tempfile::tempdir().expect("tempdir");
        // Claude Code appends `-<hash>`; only the prefix is reproducible.
        let on_disk = root.path().join(format!("{}-1a2b3c", slug.name));
        std::fs::create_dir_all(&on_disk).expect("mkdir");
        assert_eq!(
            project_dir(root.path(), &deep).as_deref(),
            Some(on_disk.as_path())
        );

        // Two long paths sharing a prefix are ambiguous, and guessing would
        // hand back another project's transcripts.
        std::fs::create_dir_all(root.path().join(format!("{}-9z8y7x", slug.name))).expect("mkdir");
        assert_eq!(project_dir(root.path(), &deep), None);
    }

    #[test]
    fn astral_characters_slugify_per_utf16_unit() {
        // A JS regex replaces per UTF-16 code unit, so an emoji (a surrogate
        // pair) becomes two dashes. Getting this wrong misses the directory.
        assert_eq!(project_slug("/a\u{1F600}b").name, "-a--b");
        // A BMP character is one unit and therefore one dash.
        assert_eq!(project_slug("/aéb").name, "-a-b");
    }

    #[test]
    fn list_sessions_finds_and_orders_the_projects_transcripts() {
        let root = tempfile::tempdir().expect("tempdir");
        let cwd = "/home/user/projects/demo";
        let dir = root.path().join(project_slug(cwd).name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::copy(fixture("branched.jsonl"), dir.join("aaa.jsonl")).expect("copy");
        std::fs::copy(fixture("linear.jsonl"), dir.join("bbb.jsonl")).expect("copy");
        // Files that are not transcripts, and the per-session subdirectory
        // Claude Code keeps beside them, are ignored.
        std::fs::write(dir.join("notes.txt"), b"x").expect("write");
        std::fs::create_dir_all(dir.join("aaa/tool-results")).expect("mkdir");

        let previews = list_sessions(root.path(), cwd);
        assert_eq!(previews.len(), 2);
        // The reactor session ran later than the home one; most recent first.
        assert!(previews[0].updated >= previews[1].updated);
        assert!(previews.iter().any(|p| p.branch_points == 1));

        // A working directory with no sessions yields nothing rather than
        // another project's.
        assert!(list_sessions(root.path(), "/home/user/projects/other").is_empty());
    }

    #[test]
    fn session_files_ignores_directories_and_other_extensions() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("b.jsonl"), b"").expect("write");
        std::fs::write(dir.path().join("a.jsonl"), b"").expect("write");
        std::fs::write(dir.path().join("a.json"), b"").expect("write");
        std::fs::create_dir_all(dir.path().join("memory")).expect("mkdir");
        let names: Vec<String> = session_files(dir.path())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.jsonl", "b.jsonl"]);
    }

    // -- read-only -------------------------------------------------------

    #[test]
    fn no_write_api_reachable_from_this_module() {
        // `~/.claude` is another program's live state. This module reads it
        // while Claude Code may be appending to it, so a write from here would
        // at best race and at worst destroy a conversation the user cannot get
        // back.
        //
        // A runtime assertion cannot see a write that a *future* edit
        // introduces, and the damage is not observable from inside a test
        // process, so the property is enforced against the source: this file
        // may name read-only filesystem APIs and nothing else. The needles are
        // assembled from pieces so that this array is not its own match.
        let source = include_str!("claude_session.rs");
        let forbidden = [
            concat!("File", "::create"),
            concat!("OpenOptions", "::"),
            concat!("File", "::options"),
            concat!("fs", "::write"),
            concat!("fs", "::copy"),
            concat!("fs", "::rename"),
            concat!("fs", "::remove_file"),
            concat!("fs", "::remove_dir"),
            concat!("fs", "::hard_link"),
            concat!("fs", "::soft_link"),
            concat!("fs", "::set_permissions"),
            concat!("write", "_all"),
            concat!("create_dir", "_all"),
            concat!("symlink", "("),
        ];
        // The test module builds throwaway trees in a tempdir; only the
        // module's own code is under audit.
        let module = source
            .split_once("#[cfg(test)]")
            .map_or(source, |(before, _)| before);
        let offenders: Vec<&str> = forbidden
            .iter()
            .copied()
            .filter(|needle| module.contains(needle))
            .collect();
        assert!(
            offenders.is_empty(),
            "claude_session must never write: found {offenders:?}"
        );
        // And the read-only APIs it is allowed to use are actually the ones it
        // uses, so the check above is not vacuously passing over a module that
        // stopped touching the filesystem entirely.
        assert!(module.contains(concat!("File", "::open")));
        assert!(module.contains(concat!("fs", "::read_dir")));
    }

    #[test]
    fn reading_a_claude_tree_leaves_every_byte_of_it_alone() {
        // The companion to the source scan: exercise every public entry point
        // against a `~/.claude/projects`-shaped tree and prove the tree is
        // untouched afterwards.
        //
        // The GUI pickers reach this module through two callers that live
        // outside it and therefore outside the source scan — the shared
        // listing and the import that a picker click runs — so both are driven
        // here too. Otherwise the strongest guarantee in this file would stop
        // one function short of the code that a user actually clicks.
        use crate::claude_session::tests_support::snapshot;

        let root = tempfile::tempdir().expect("tempdir");
        let cwd = "/home/user/projects/demo";
        let dir = root.path().join(project_slug(cwd).name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::copy(fixture("branched.jsonl"), dir.join("one.jsonl")).expect("copy");
        std::fs::copy(fixture("linear.jsonl"), dir.join("two.jsonl")).expect("copy");

        let before = snapshot(root.path());
        assert_eq!(before.len(), 2);

        let previews = list_sessions(root.path(), cwd);
        assert_eq!(previews.len(), 2);
        for preview in &previews {
            let session = ClaudeSession::load(&preview.path).expect("load");
            let _ = session.resume_chain();
            let _ = session.branch_points();
            let _ = session.leaves();
            let _ = session.orphans();
            let _ = session.roots();
            let _ = session.preview();
        }
        let _ = project_dir(root.path(), cwd);
        let _ = session_files(&dir);

        // The picker's own path: list the workspace, then import a row the way
        // clicking it would. The import writes — into Wizard's sessions
        // directory, which under `cfg(test)` is a temp dir of its own.
        let workspace = tempfile::tempdir().expect("workspace");
        let rows = crate::session_registry::claude_chats_in(root.path(), cwd);
        assert_eq!(rows.len(), 2);
        for row in &rows {
            let crate::session_registry::Origin::Claude { path, leaf, .. } = &row.origin else {
                panic!("a Claude row is a Claude row");
            };
            crate::claude_resume::import(path, leaf.as_deref(), workspace.path())
                .expect("import the row");
        }

        assert_eq!(
            snapshot(root.path()),
            before,
            "reading a Claude Code project directory must not change it"
        );
    }

    #[test]
    fn load_and_parse_agree_on_a_real_session() {
        // `parse` is what most tests use; `load` is what the product uses.
        // They must not drift.
        let path = fixture("branched.jsonl");
        let text = std::fs::read_to_string(&path).expect("read fixture");
        let loaded = ClaudeSession::load(&path).expect("load");
        let parsed = ClaudeSession::parse(path, &text);
        assert_eq!(loaded.preview(), parsed.preview());
        assert_eq!(loaded.nodes.len(), parsed.nodes.len());
        assert_eq!(loaded.meta, parsed.meta);
        assert_eq!(loaded.resume_chain(), parsed.resume_chain());
    }
}
