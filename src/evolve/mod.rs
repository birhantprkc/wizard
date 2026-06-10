//! Tiered self-extension (`/evolve`) and fork-and-distribute (`/publish`).
//! See `docs/evolve.md` and `docs/market.md`.
//!
//! Tier 1 (runtime, default): write a skill, MCP server entry, scripted
//! tool, or subagent under `~/.wizard/` and activate via `/reload`.
//! Tier 2 (`--deep`): propose a diff over Wizard's own source, build, and
//! `exec`-replace the running binary. Falls back to Tier 1 when no
//! toolchain/source can be provisioned.

pub mod publish;
pub use publish::{PublishOutcome, PublishRequest, publish};

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::subagent::SubagentConfig;
use crate::cli::Cli;
use crate::config::Config;
use crate::llm::ollama::OllamaClient;
use crate::llm::{ChatMessage, ChatOptions, ChatRequest};
use crate::mcp::{McpConfig, McpServerConfig, McpTransport};
use crate::tools::scripted::ScriptManifest;

/// Where deep evolve clones Wizard's source from on first use. Overridable
/// with the `WIZARD_SOURCE_REPO` environment variable (forks, mirrors,
/// air-gapped file:// remotes).
const DEFAULT_REPO_URL: &str = "https://github.com/teddytennant/wizard";

/// How many times we re-prompt the model when its reply cannot be parsed.
const PROPOSAL_ATTEMPTS: usize = 2;

/// Cap on the repository file listing included in the deep-evolve prompt.
const MAX_LISTED_FILES: usize = 400;

/// Self-extension tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvolveTier {
    /// Tier 1: runtime extension via data/config; no recompile.
    Runtime,
    /// Tier 2: rebuild Wizard's own source (`--deep`).
    Deep,
}

/// Tier-1 channel chosen by the agent for a runtime extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolveChannel {
    /// Markdown skill injected into the system prompt.
    Skill,
    /// External MCP server registered in `~/.wizard/mcp.toml`.
    McpServer,
    /// Agent-authored script under `~/.wizard/tools/`.
    ScriptedTool,
    /// Named subagent definition under `~/.wizard/subagents/`.
    Subagent,
}

impl EvolveChannel {
    /// Human-readable label for status messages.
    fn label(self) -> &'static str {
        match self {
            EvolveChannel::Skill => "skill",
            EvolveChannel::McpServer => "MCP server",
            EvolveChannel::ScriptedTool => "scripted tool",
            EvolveChannel::Subagent => "subagent",
        }
    }
}

/// What the user asked `/evolve` to do.
#[derive(Debug, Clone)]
pub struct EvolveRequest {
    /// Natural-language description of the capability to add.
    pub description: String,
    pub tier: EvolveTier,
    /// Skip the approval step (sovereign mode / `--auto`).
    pub auto_approve: bool,
}

/// What an evolution produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvolveOutcome {
    SkillAdded {
        name: String,
        path: PathBuf,
    },
    McpServerRegistered {
        name: String,
    },
    ScriptedToolAdded {
        name: String,
        path: PathBuf,
    },
    SubagentAdded {
        name: String,
    },
    /// Deep evolve built and staged a new binary; the process will
    /// `exec`-replace itself next.
    DeepRebuilt {
        binary: PathBuf,
    },
    /// Deep evolve could not proceed (no toolchain/source) and ran a Tier-1
    /// evolution instead.
    FellBackToRuntime {
        reason: String,
        outcome: Box<EvolveOutcome>,
    },
    /// The user denied the proposed change.
    Denied,
}

/// One line of `~/.wizard/evolution.jsonl` — every evolution, both tiers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionEvent {
    pub timestamp: DateTime<Utc>,
    pub tier: EvolveTier,
    /// The user's request.
    pub description: String,
    pub outcome: EvolveOutcome,
    /// Unified diff over Wizard's source (deep evolve only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// Whether `cargo build --release` succeeded (deep evolve only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_ok: Option<bool>,
}

/// Append one [`EvolutionEvent`] as a JSONL line to `path`, creating the
/// parent directory if needed. Backs [`Evolver::log`].
fn append_event(path: &Path, event: &EvolutionEvent) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let line = serde_json::to_string(event).context("serializing evolution event")?;
    writeln!(file, "{line}").with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// A Tier-1 extension proposed by the model: one channel plus the artifact
/// to write. Parsed from the single JSON object the planning prompt demands.
#[derive(Debug, Deserialize)]
#[serde(tag = "channel", rename_all = "snake_case")]
enum ChannelProposal {
    Skill(SkillProposal),
    McpServer(McpServerConfig),
    ScriptedTool(ScriptedToolProposal),
    Subagent(SubagentProposal),
}

impl ChannelProposal {
    fn channel(&self) -> EvolveChannel {
        match self {
            ChannelProposal::Skill(_) => EvolveChannel::Skill,
            ChannelProposal::McpServer(_) => EvolveChannel::McpServer,
            ChannelProposal::ScriptedTool(_) => EvolveChannel::ScriptedTool,
            ChannelProposal::Subagent(_) => EvolveChannel::Subagent,
        }
    }
}

#[derive(Debug, Deserialize)]
struct SkillProposal {
    name: String,
    #[serde(default)]
    description: Option<String>,
    body: String,
}

#[derive(Debug, Deserialize)]
struct ScriptedToolProposal {
    name: String,
    description: String,
    #[serde(default)]
    interpreter: Option<String>,
    /// Script file name (sanitized; derived from `name` when omitted).
    #[serde(default)]
    script_name: Option<String>,
    script_content: String,
    /// JSON Schema for the tool's arguments object.
    #[serde(default)]
    parameters: Option<Value>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct SubagentProposal {
    name: String,
    description: String,
    system_prompt: String,
    #[serde(default)]
    tool_scope: Option<Vec<String>>,
    #[serde(default)]
    max_steps: Option<u32>,
}

/// System prompt for the Tier-1 planning turn: pick one channel, emit one
/// JSON object describing the artifact.
const TIER1_SYSTEM_PROMPT: &str = r##"You are Wizard's self-extension planner. Wizard is a local coding agent that can extend itself at runtime through exactly four channels. Given the user's request, choose the single best channel and respond with ONLY one JSON object — no prose, no markdown fences, no comments.

Channels and their exact JSON shapes:

1. "skill" — knowledge, guidelines, or a workflow injected into the system prompt as markdown:
{"channel":"skill","name":"kebab-case-name","description":"one-line summary","body":"full markdown content of the skill"}

2. "mcp_server" — register an external Model Context Protocol tool server (computer use, browsers, databases, search, ...):
{"channel":"mcp_server","name":"server-name","transport":"stdio","command":"uvx","args":["mcp-package-name"],"env":{}}
or, for a remote server:
{"channel":"mcp_server","name":"server-name","transport":"http","url":"https://example.com/mcp"}

3. "scripted_tool" — a small executable script exposed as a tool. The tool's arguments arrive as a single JSON string in argv[1]; the script prints its result to stdout:
{"channel":"scripted_tool","name":"snake_case_name","description":"what it does","interpreter":"bash","script_name":"snake_case_name.sh","script_content":"#!/usr/bin/env bash\n...","parameters":{"type":"object","properties":{"arg":{"type":"string","description":"..."}},"required":["arg"]},"timeout_secs":120}

4. "subagent" — a named, reusable sub-worker with its own prompt, tool scope, and step budget:
{"channel":"subagent","name":"reviewer","description":"what it is for","system_prompt":"You are ...","tool_scope":["read_file","search_files","git_diff"],"max_steps":15}

Native tool names available for "tool_scope": read_file, write_file, edit_file, list_files, search_files, execute, git_status, git_diff. Omit "tool_scope" (or use null) to grant the full set.

Picking a channel: use a skill for knowledge or process, an mcp_server for capabilities that live outside Wizard, a scripted_tool for small executable glue, and a subagent for a specialized, reusable sub-worker. Keep names short and filesystem-safe. Make the artifact complete and immediately usable."##;

/// System prompt for the deep-evolve (Tier 2) diff-authoring turn.
const DEEP_SYSTEM_PROMPT: &str = r#"You are Wizard's deep-evolve engineer. Wizard is a single-binary Rust 2024 coding agent (Ratatui TUI + Ollama-backed agent loop) and you are modifying its own source checkout. Produce ONE unified diff that implements the requested change.

Rules:
- Output ONLY the diff, inside a single ```diff fenced code block. No other text.
- Use standard unified diff format with `--- a/<path>` and `+++ b/<path>` headers (use /dev/null for created or deleted files) and correct `@@` hunk headers.
- Paths are relative to the repository root.
- Include at least 3 unchanged context lines around each hunk so `git apply` can locate it.
- Keep the change minimal, correct, and consistent with the existing code style. Proper error handling; no todo!() or unwrap() on fallible paths."#;

/// Drives the evolve pipeline. Holds config and paths; the model
/// interaction itself runs through a dedicated agent turn.
pub struct Evolver {
    config: Config,
    /// Print progress and stream model output to stdout (CLI runs).
    verbose: bool,
}

impl Evolver {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            verbose: false,
        }
    }

    /// Enable progress printing to stdout (used by the CLI entry point).
    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Run one evolution end to end: have the agent pick a Tier-1 channel
    /// (or drive the deep pipeline), apply it, log it, and return the
    /// outcome. Tier-1 results become live after `/reload`.
    pub async fn run(&mut self, request: EvolveRequest) -> Result<EvolveOutcome> {
        if request.description.trim().is_empty() {
            bail!("evolution request has an empty description");
        }
        match request.tier {
            EvolveTier::Runtime => {
                let outcome = self.run_runtime(&request).await?;
                self.log_event(&request, EvolveTier::Runtime, &outcome, None, None)?;
                Ok(outcome)
            }
            EvolveTier::Deep => self.run_deep(&request).await,
        }
    }

    /// Append an event to `~/.wizard/evolution.jsonl`.
    pub fn log(&self, event: &EvolutionEvent) -> Result<()> {
        append_event(&Config::evolution_log_path()?, event)
    }

    fn log_event(
        &self,
        request: &EvolveRequest,
        tier: EvolveTier,
        outcome: &EvolveOutcome,
        diff: Option<String>,
        build_ok: Option<bool>,
    ) -> Result<()> {
        self.log(&EvolutionEvent {
            timestamp: Utc::now(),
            tier,
            description: request.description.clone(),
            outcome: outcome.clone(),
            diff,
            build_ok,
        })
    }

    // ---- Tier 1: runtime extension ----

    /// Plan and apply one runtime extension. Does not log; callers do, so
    /// the deep-evolve fallback can wrap the outcome first.
    async fn run_runtime(&self, request: &EvolveRequest) -> Result<EvolveOutcome> {
        self.status(&format!(
            "Planning a runtime extension for: {}",
            request.description
        ));
        let proposal = self.propose_channel(&request.description).await?;
        self.status(&format!(
            "\nProposed {} extension:\n{}\n",
            proposal.channel().label(),
            proposal_summary(&proposal)
        ));

        if !request.auto_approve && !confirm("Apply this change?")? {
            return Ok(EvolveOutcome::Denied);
        }

        let outcome = self.apply_proposal(proposal)?;
        self.status(
            "Change written under ~/.wizard — run /reload (or restart Wizard) to activate it.",
        );
        Ok(outcome)
    }

    /// One dedicated model turn (with one retry) producing a parsed Tier-1
    /// channel proposal.
    async fn propose_channel(&self, description: &str) -> Result<ChannelProposal> {
        let messages = vec![
            ChatMessage::system(TIER1_SYSTEM_PROMPT),
            ChatMessage::user(description),
        ];
        self.propose(
            messages,
            parse_proposal,
            "Reply with ONLY the JSON object for one channel, exactly matching the documented shape — no prose, no code fences.",
        )
        .await
    }

    /// Write the proposed artifact under `~/.wizard/`.
    fn apply_proposal(&self, proposal: ChannelProposal) -> Result<EvolveOutcome> {
        match proposal {
            ChannelProposal::Skill(skill) => self.add_skill(skill),
            ChannelProposal::McpServer(server) => self.register_mcp_server(server),
            ChannelProposal::ScriptedTool(tool) => self.add_scripted_tool(tool),
            ChannelProposal::Subagent(subagent) => self.add_subagent(subagent),
        }
    }

    /// Write `~/.wizard/skills/<name>/SKILL.md` with frontmatter the skills
    /// loader understands.
    fn add_skill(&self, proposal: SkillProposal) -> Result<EvolveOutcome> {
        if proposal.body.trim().is_empty() {
            bail!("the proposed skill has an empty body");
        }
        let name = slugify(&proposal.name, '-')?;
        let dir = Config::skills_dir()?.join(&name);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("SKILL.md");

        let mut doc = format!("---\nname: {name}\n");
        if let Some(description) = proposal.description.as_deref() {
            let description = description.replace(['\n', '\r'], " ");
            let description = description.trim();
            if !description.is_empty() {
                doc.push_str(&format!("description: {description}\n"));
            }
        }
        doc.push_str("---\n\n");
        doc.push_str(proposal.body.trim());
        doc.push('\n');

        std::fs::write(&path, doc).with_context(|| format!("writing {}", path.display()))?;
        Ok(EvolveOutcome::SkillAdded { name, path })
    }

    /// Upsert a `[[server]]` entry in `~/.wizard/mcp.toml`.
    fn register_mcp_server(&self, mut server: McpServerConfig) -> Result<EvolveOutcome> {
        server.name = slugify(&server.name, '-')?;
        match server.transport {
            McpTransport::Stdio
                if server
                    .command
                    .as_deref()
                    .is_none_or(|c| c.trim().is_empty()) =>
            {
                bail!("stdio MCP server '{}' is missing a command", server.name)
            }
            McpTransport::Http if server.url.as_deref().is_none_or(|u| u.trim().is_empty()) => {
                bail!("http MCP server '{}' is missing a url", server.name)
            }
            _ => {}
        }

        let path = Config::mcp_config_path()?;
        let mut mcp = McpConfig::load(&path)?;
        let name = server.name.clone();
        let replaced = mcp.servers.iter().any(|s| s.name == name);
        mcp.servers.retain(|s| s.name != name);
        mcp.servers.push(server);
        mcp.save(&path)?;
        if replaced {
            self.status(&format!("Replaced existing MCP server entry '{name}'."));
        }
        Ok(EvolveOutcome::McpServerRegistered { name })
    }

    /// Write the script plus its `<name>.toml` manifest under
    /// `~/.wizard/tools/`.
    fn add_scripted_tool(&self, proposal: ScriptedToolProposal) -> Result<EvolveOutcome> {
        if proposal.script_content.trim().is_empty() {
            bail!("the proposed scripted tool has an empty script");
        }
        let tool_name = slugify(&proposal.name, '_')?;
        let dir = Config::scripted_tools_dir()?;
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

        let interpreter = proposal
            .interpreter
            .as_deref()
            .map(str::trim)
            .filter(|i| !i.is_empty())
            .map(str::to_string);
        let script_file = match proposal.script_name.as_deref().map(str::trim) {
            Some(name) if !name.is_empty() => sanitize_file_name(name)?,
            _ => format!(
                "{}.{}",
                slugify(&proposal.name, '-')?,
                script_extension(interpreter.as_deref())
            ),
        };
        let script_path = dir.join(&script_file);

        let mut content = proposal.script_content;
        if !content.ends_with('\n') {
            content.push('\n');
        }
        // A script with neither a shebang nor an interpreter cannot run;
        // default the interpreter to `sh` rather than write a dud tool.
        let interpreter =
            interpreter.or_else(|| (!content.starts_with("#!")).then(|| "sh".to_string()));

        std::fs::write(&script_path, content)
            .with_context(|| format!("writing {}", script_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .with_context(|| format!("chmod {}", script_path.display()))?;
        }

        let parameters = match proposal.parameters {
            Some(value) if value.is_object() => value,
            _ => serde_json::json!({ "type": "object", "properties": {} }),
        };
        let manifest = ScriptManifest {
            name: tool_name.clone(),
            description: proposal.description,
            script: script_file,
            interpreter,
            parameters,
            timeout_secs: proposal.timeout_secs,
        };
        let manifest_path = dir.join(format!("{tool_name}.toml"));
        let raw =
            toml::to_string_pretty(&manifest).context("serializing scripted tool manifest")?;
        std::fs::write(&manifest_path, raw)
            .with_context(|| format!("writing {}", manifest_path.display()))?;

        Ok(EvolveOutcome::ScriptedToolAdded {
            name: tool_name,
            path: manifest_path,
        })
    }

    /// Write a subagent definition to `~/.wizard/subagents/<name>.toml`.
    fn add_subagent(&self, proposal: SubagentProposal) -> Result<EvolveOutcome> {
        if proposal.system_prompt.trim().is_empty() {
            bail!("the proposed subagent has an empty system prompt");
        }
        let name = slugify(&proposal.name, '-')?;
        let config = SubagentConfig {
            name: name.clone(),
            description: proposal.description,
            system_prompt: proposal.system_prompt,
            tool_scope: proposal.tool_scope,
            max_steps: proposal.max_steps.unwrap_or(15).max(1),
        };
        let dir = Config::wizard_dir()?.join("subagents");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{name}.toml"));
        let raw = toml::to_string_pretty(&config).context("serializing subagent config")?;
        std::fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(EvolveOutcome::SubagentAdded { name })
    }

    // ---- Deep-evolve pipeline (tier 2) ----

    /// The full Tier-2 pipeline: source + toolchain, diff proposal, approval,
    /// build, install. Falls back to Tier 1 when source/toolchain cannot be
    /// provisioned. Logs its own events (it needs the diff and build result).
    async fn run_deep(&mut self, request: &EvolveRequest) -> Result<EvolveOutcome> {
        let prepared = self
            .ensure_source()
            .and_then(|dir| self.ensure_toolchain().map(|()| dir));
        let source_dir = match prepared {
            Ok(dir) => dir,
            Err(err) => {
                let reason = format!("{err:#}");
                self.status(&format!(
                    "Deep evolve unavailable ({reason}); falling back to a runtime (Tier 1) evolution."
                ));
                let inner = self.run_runtime(request).await?;
                let outcome = EvolveOutcome::FellBackToRuntime {
                    reason,
                    outcome: Box::new(inner),
                };
                // Log the tier that actually ran, not the one requested.
                self.log_event(request, EvolveTier::Runtime, &outcome, None, None)?;
                return Ok(outcome);
            }
        };

        self.status("Proposing a change to Wizard's own source…");
        let diff = self.propose_diff(&request.description, &source_dir).await?;
        if self.verbose {
            println!("\n{diff}");
        }

        if !request.auto_approve && !confirm("Apply this diff and rebuild Wizard?")? {
            let outcome = EvolveOutcome::Denied;
            self.log_event(request, EvolveTier::Deep, &outcome, Some(diff), None)?;
            return Ok(outcome);
        }

        self.apply_diff(&source_dir, &diff)?;
        self.status("Building (cargo build --release)… this can take a while.");
        let built = match self.build(&source_dir).await {
            Ok(binary) => binary,
            Err(err) => {
                self.revert_diff(&source_dir);
                return Err(err.context("deep evolve build failed; the diff was reverted"));
            }
        };
        if let Err(err) = smoke_test(&built) {
            self.revert_diff(&source_dir);
            return Err(err.context(
                "deep evolve smoke test failed; the current binary was kept and the diff reverted",
            ));
        }
        self.commit_source(&source_dir, &request.description);

        let binary = self.install_binary(&built);
        let outcome = EvolveOutcome::DeepRebuilt {
            binary: binary.clone(),
        };
        self.log_event(request, EvolveTier::Deep, &outcome, Some(diff), Some(true))?;
        self.status(&format!("Rebuilt Wizard: {}", binary.display()));
        Ok(outcome)
    }

    /// One dedicated model turn (with one retry) producing a unified diff.
    async fn propose_diff(&self, description: &str, source_dir: &Path) -> Result<String> {
        let listing = source_file_listing(source_dir);
        let user = format!(
            "Requested change to Wizard:\n{description}\n\n\
             Files in the repository (relative to its root):\n{listing}\n\n\
             Reply with one unified diff implementing the change."
        );
        let messages = vec![
            ChatMessage::system(DEEP_SYSTEM_PROMPT),
            ChatMessage::user(user),
        ];
        self.propose(
            messages,
            |reply| extract_diff(reply).context("no unified diff found in the reply"),
            "Reply with ONLY a unified diff inside a single ```diff fenced block.",
        )
        .await
    }

    /// Ensure `~/.wizard/src` holds a source checkout, cloning the repo on
    /// first use. Errors when offline with no existing checkout.
    pub fn ensure_source(&self) -> Result<PathBuf> {
        let dir = Config::source_dir()?;
        if dir.join("Cargo.toml").is_file() {
            return Ok(dir);
        }
        let non_empty = dir.exists()
            && std::fs::read_dir(&dir)
                .map(|mut entries| entries.next().is_some())
                .unwrap_or(false);
        if non_empty {
            bail!(
                "{} exists but does not look like a Wizard checkout; remove it and retry",
                dir.display()
            );
        }
        if !command_exists("git") {
            bail!("`git` is required to clone Wizard's source");
        }
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let url = repo_url();
        self.status(&format!("Cloning {url} into {}…", dir.display()));
        let output = Command::new("git")
            .args(["clone", "--depth", "1"])
            .arg(&url)
            .arg(&dir)
            .output()
            .context("running git clone")?;
        if !output.status.success() {
            bail!(
                "git clone of {url} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        if !dir.join("Cargo.toml").is_file() {
            bail!("cloned {url} but no Cargo.toml found in {}", dir.display());
        }
        Ok(dir)
    }

    /// Ensure a Rust toolchain is available, installing just-in-time via
    /// `rustup --profile minimal` when `cargo` is absent. Errors when it
    /// cannot be provisioned (the caller then falls back to Tier 1).
    pub fn ensure_toolchain(&self) -> Result<()> {
        if find_cargo().is_some() {
            return Ok(());
        }
        self.status("No Rust toolchain found; installing one via rustup (--profile minimal)…");
        let status = if command_exists("rustup") {
            Command::new("rustup")
                .args(["toolchain", "install", "stable", "--profile", "minimal"])
                .status()
                .context("running rustup toolchain install")?
        } else if command_exists("curl") {
            Command::new("sh")
                .arg("-c")
                .arg(
                    "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
                     | sh -s -- -y --profile minimal --default-toolchain stable",
                )
                .status()
                .context("running the rustup installer")?
        } else {
            bail!("no Rust toolchain, and neither `rustup` nor `curl` is available to install one");
        };
        if !status.success() {
            bail!("rustup install exited with {status}");
        }
        if find_cargo().is_none() {
            bail!("rustup ran but `cargo` is still not available");
        }
        Ok(())
    }

    /// `cargo build --release` in `source_dir`; returns the built binary
    /// path. The previous binary is kept beside it for rollback.
    pub async fn build(&self, source_dir: &std::path::Path) -> Result<PathBuf> {
        let cargo = find_cargo().context("cargo is not available (no Rust toolchain installed)")?;
        let mut cmd = tokio::process::Command::new(&cargo);
        cmd.args(["build", "--release"])
            .current_dir(source_dir)
            .env("PATH", augmented_path());

        if self.verbose {
            // Let the user watch the compile.
            let status = cmd
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .await
                .context("running cargo build --release")?;
            if !status.success() {
                bail!("cargo build --release failed (see output above)");
            }
        } else {
            let output = cmd
                .output()
                .await
                .context("running cargo build --release")?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("cargo build --release failed:\n{}", tail_lines(&stderr, 40));
            }
        }

        let binary = source_dir.join("target").join("release").join("wizard");
        if !binary.is_file() {
            bail!(
                "build succeeded but the binary is missing at {}",
                binary.display()
            );
        }
        Ok(binary)
    }

    /// Replace the running process with `binary` (Unix `exec`). On success
    /// this never returns.
    pub fn exec_replace(binary: &std::path::Path) -> Result<std::convert::Infallible> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let err = std::process::Command::new(binary).exec();
            Err(anyhow::Error::new(err)
                .context(format!("failed to exec-replace with {}", binary.display())))
        }
        #[cfg(not(unix))]
        {
            bail!(
                "exec-replace is only supported on Unix; the new binary is staged at {}",
                binary.display()
            )
        }
    }

    /// Pipe `diff` to `git apply` in `source_dir` (a `--check` pass first,
    /// then for real).
    fn apply_diff(&self, source_dir: &Path, diff: &str) -> Result<()> {
        for check in [true, false] {
            let mut cmd = Command::new("git");
            cmd.arg("-C")
                .arg(source_dir)
                .args(["apply", "--whitespace=nowarn"]);
            if check {
                cmd.arg("--check");
            }
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn().context("spawning git apply")?;
            child
                .stdin
                .take()
                .context("opening git apply stdin")?
                .write_all(diff.as_bytes())
                .context("writing diff to git apply")?;
            let output = child.wait_with_output().context("waiting for git apply")?;
            if !output.status.success() {
                bail!(
                    "git apply{} failed:\n{}",
                    if check { " --check" } else { "" },
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
        Ok(())
    }

    /// Best-effort revert of an applied-but-unbuildable diff so the next
    /// deep evolve starts from a clean tree.
    fn revert_diff(&self, source_dir: &Path) {
        let checkout = Command::new("git")
            .arg("-C")
            .arg(source_dir)
            .args(["checkout", "--", "."])
            .status();
        let clean = Command::new("git")
            .arg("-C")
            .arg(source_dir)
            .args(["clean", "-fdq"])
            .status();
        let ok = checkout.map(|s| s.success()).unwrap_or(false)
            && clean.map(|s| s.success()).unwrap_or(false);
        if ok {
            self.status("Reverted the applied diff in the source checkout.");
        } else {
            tracing::warn!(
                source_dir = %source_dir.display(),
                "failed to revert the applied diff; the checkout may be dirty"
            );
        }
    }

    /// Best-effort commit of a successful deep evolve so the checkout stays
    /// clean for the next one and the change is recoverable from history.
    fn commit_source(&self, source_dir: &Path, description: &str) {
        let added = Command::new("git")
            .arg("-C")
            .arg(source_dir)
            .args(["add", "-A"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !added {
            tracing::warn!("git add failed in the source checkout; skipping commit");
            return;
        }
        let subject = description.lines().next().unwrap_or(description);
        let message = format!("evolve(deep): {subject}");
        let committed = Command::new("git")
            .arg("-C")
            .arg(source_dir)
            .args([
                "-c",
                "user.name=Wizard",
                "-c",
                "user.email=wizard@localhost",
                "commit",
                "-m",
            ])
            .arg(&message)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !committed {
            tracing::warn!("git commit failed in the source checkout");
        }
    }

    /// Install `built` over the currently running executable, keeping the
    /// prior binary beside it as `<name>.prev` for rollback. When the
    /// install location is not writable (or unknown), returns the built
    /// binary's own path so the caller can exec it in place.
    fn install_binary(&self, built: &Path) -> PathBuf {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(err) => {
                tracing::warn!(%err, "could not determine the current executable; running the built binary in place");
                return built.to_path_buf();
            }
        };
        // Already running from the build output (e.g. after a prior
        // in-place deep evolve) — nothing to install.
        if std::fs::canonicalize(&exe).ok() == std::fs::canonicalize(built).ok() {
            return built.to_path_buf();
        }
        match swap_in(built, &exe) {
            Ok(backup) => {
                self.status(&format!(
                    "Installed the new binary over {}. To roll back: mv {} {}",
                    exe.display(),
                    backup.display(),
                    exe.display()
                ));
                exe
            }
            Err(err) => {
                self.status(&format!(
                    "Could not install over {} ({err:#}); the rebuilt binary will run from the source tree instead.",
                    exe.display()
                ));
                built.to_path_buf()
            }
        }
    }

    // ---- Model interaction ----

    /// Run a dedicated model turn and parse the reply, re-prompting once
    /// with `retry_hint` when parsing fails.
    async fn propose<T>(
        &self,
        mut messages: Vec<ChatMessage>,
        parse: impl Fn(&str) -> Result<T>,
        retry_hint: &str,
    ) -> Result<T> {
        let mut last_err: Option<anyhow::Error> = None;
        for _ in 0..PROPOSAL_ATTEMPTS {
            let reply = self.complete(&messages).await?;
            match parse(&reply) {
                Ok(value) => return Ok(value),
                Err(err) => {
                    messages.push(ChatMessage::assistant(reply));
                    messages.push(ChatMessage::user(format!(
                        "That response could not be used ({err:#}). {retry_hint}"
                    )));
                    last_err = Some(err);
                }
            }
        }
        Err(last_err
            .unwrap_or_else(|| anyhow!("the model produced no reply"))
            .context("the model did not produce a usable evolution proposal"))
    }

    /// Stream one completion from Ollama and return the accumulated text.
    async fn complete(&self, messages: &[ChatMessage]) -> Result<String> {
        let client = OllamaClient::new(self.config.ollama_host.clone());
        let request = ChatRequest {
            model: self.config.model.clone(),
            messages: messages.to_vec(),
            tools: Vec::new(),
            stream: true,
            options: Some(ChatOptions {
                // Low temperature: we want a parseable artifact, not prose.
                temperature: Some(0.3),
                num_ctx: None,
            }),
        };
        let mut stream = client.chat_stream(request).await?;
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if let Some(message) = chunk.message
                && !message.content.is_empty()
            {
                if self.verbose {
                    print!("{}", message.content);
                    let _ = std::io::stdout().flush();
                }
                text.push_str(&message.content);
            }
            if chunk.done {
                break;
            }
        }
        if self.verbose && !text.is_empty() {
            println!();
        }
        Ok(text)
    }

    /// Print a progress line when verbose (CLI); always trace it.
    fn status(&self, message: &str) {
        tracing::info!(target: "wizard::evolve", "{message}");
        if self.verbose {
            println!("{message}");
        }
    }
}

/// CLI entry point for `wizard --publish`: forks Wizard to the user's GitHub
/// and prints the fork URL and one-line installer to stdout.
pub async fn run_publish_cli(config: Config, cli: Cli) -> Result<()> {
    use publish::PublishRequest;

    let branch = cli.prompt.clone().and_then(|p| {
        let p = p.trim().to_string();
        (!p.is_empty()).then_some(p)
    });

    let req = PublishRequest {
        branch,
        auto_approve: config.auto_approve || cli.auto,
    };

    let outcome = publish::publish(&config, req, true).await?;
    println!("Fork:    {}", outcome.fork_url);
    println!("Branch:  {}", outcome.branch);
    if let Some(sha) = &outcome.commit {
        println!("Commit:  {sha}");
    }
    println!("\nInstall one-liner:\n{}", outcome.install_one_liner);
    Ok(())
}

/// CLI entry point for `wizard --evolve [-p "..."] [--deep]`: runs one
/// evolution without the full TUI, printing progress to stdout.
pub async fn run_cli(config: Config, cli: Cli) -> Result<()> {
    let description = match cli.prompt.as_deref().map(str::trim) {
        Some(prompt) if !prompt.is_empty() => prompt.to_string(),
        _ => prompt_for_description()?,
    };
    let tier = if cli.deep {
        EvolveTier::Deep
    } else {
        EvolveTier::Runtime
    };
    let request = EvolveRequest {
        description,
        tier,
        // `Config::apply_cli` already folded `--auto`/sovereign in, but the
        // CLI flag is honored here too in case the caller skipped that.
        auto_approve: config.auto_approve || cli.auto,
    };

    let mut evolver = Evolver::new(config).with_verbose(true);
    let outcome = evolver.run(request).await?;
    print_outcome(&outcome);

    if let EvolveOutcome::DeepRebuilt { binary } = &outcome {
        println!("Restarting Wizard with the new binary…");
        Evolver::exec_replace(binary)?; // never returns on success
    }
    Ok(())
}

/// Ask for a description interactively when `-p` was not given.
fn prompt_for_description() -> Result<String> {
    if !std::io::stdin().is_terminal() {
        bail!("no evolution description provided; pass one with -p \"...\"");
    }
    print!("What capability should Wizard add? ");
    std::io::stdout().flush().context("flushing stdout")?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading the description")?;
    let description = line.trim().to_string();
    if description.is_empty() {
        bail!("no evolution description provided");
    }
    Ok(description)
}

/// Print a user-facing summary of an outcome (recurses into fallbacks).
fn print_outcome(outcome: &EvolveOutcome) {
    match outcome {
        EvolveOutcome::SkillAdded { name, path } => println!(
            "Skill '{name}' added at {} — /reload (or restart) to activate.",
            path.display()
        ),
        EvolveOutcome::McpServerRegistered { name } => {
            println!("MCP server '{name}' registered in ~/.wizard/mcp.toml — /reload to connect.")
        }
        EvolveOutcome::ScriptedToolAdded { name, path } => println!(
            "Scripted tool '{name}' added ({}) — /reload to activate.",
            path.display()
        ),
        EvolveOutcome::SubagentAdded { name } => {
            println!("Subagent '{name}' configured — /reload to activate.")
        }
        EvolveOutcome::DeepRebuilt { binary } => {
            println!("Deep evolve complete: {}", binary.display())
        }
        EvolveOutcome::FellBackToRuntime { reason, outcome } => {
            println!("Deep evolve fell back to a runtime extension: {reason}");
            print_outcome(outcome);
        }
        EvolveOutcome::Denied => println!("Evolution denied; no changes were applied."),
    }
}

/// Blocking y/N confirmation on the terminal. Refuses (rather than hangs)
/// when stdin is not a TTY — pass `--auto` for unattended runs.
fn confirm(question: &str) -> Result<bool> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        bail!(
            "approval required but stdin is not a terminal; re-run with --auto to skip confirmation"
        );
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush().context("flushing stdout")?;
    let mut line = String::new();
    stdin.read_line(&mut line).context("reading confirmation")?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// Human-readable proposal preview for the approval gate.
fn proposal_summary(proposal: &ChannelProposal) -> String {
    match proposal {
        ChannelProposal::Skill(skill) => format!(
            "skill '{}' — {}\n\n{}",
            skill.name,
            skill.description.as_deref().unwrap_or("(no description)"),
            truncate(&skill.body, 2000)
        ),
        ChannelProposal::McpServer(server) => format!(
            "MCP server '{}':\n{}",
            server.name,
            toml::to_string_pretty(server).unwrap_or_else(|_| format!("{server:?}"))
        ),
        ChannelProposal::ScriptedTool(tool) => format!(
            "scripted tool '{}' — {}\n\n{}",
            tool.name,
            tool.description,
            truncate(&tool.script_content, 2000)
        ),
        ChannelProposal::Subagent(subagent) => format!(
            "subagent '{}' — {}\n\nsystem prompt:\n{}",
            subagent.name,
            subagent.description,
            truncate(&subagent.system_prompt, 2000)
        ),
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let head: String = text.chars().take(max_chars).collect();
        format!("{head}…")
    }
}

/// Parse the model's reply into a Tier-1 channel proposal.
fn parse_proposal(reply: &str) -> Result<ChannelProposal> {
    let value = extract_json_object(reply)?;
    serde_json::from_value(value)
        .map_err(|err| anyhow!("the proposal JSON did not match any channel shape: {err}"))
}

/// Find the first JSON object in a model reply, tolerating prose,
/// `<think>` blocks, and code fences around it.
fn extract_json_object(text: &str) -> Result<Value> {
    let text = strip_thinking(text);
    for block in fenced_blocks(&text) {
        if let Some(value) = first_json_object(&block) {
            return Ok(value);
        }
    }
    first_json_object(&text).context("no JSON object found in the model's reply")
}

/// Scan for `{` and try to parse one JSON object starting there (trailing
/// text after the object is fine).
fn first_json_object(text: &str) -> Option<Value> {
    let mut start = 0;
    let mut attempts = 0;
    while let Some(offset) = text[start..].find('{') {
        let index = start + offset;
        let mut iter = serde_json::Deserializer::from_str(&text[index..]).into_iter::<Value>();
        if let Some(Ok(value)) = iter.next()
            && value.is_object()
        {
            return Some(value);
        }
        start = index + 1;
        attempts += 1;
        if attempts >= 50 {
            break;
        }
    }
    None
}

/// Contents of ``` fenced blocks, with a short language tag line stripped.
fn fenced_blocks(text: &str) -> Vec<String> {
    text.split("```")
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, block)| match block.split_once('\n') {
            Some((first, rest)) if first.trim().len() <= 20 && !first.contains('{') => {
                rest.to_string()
            }
            _ => block.to_string(),
        })
        .collect()
}

/// Remove `<think>`/`<thinking>` blocks some models emit inline.
fn strip_thinking(text: &str) -> String {
    let mut out = text.to_string();
    for tag in ["think", "thinking"] {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        while let Some(start) = out.find(&open) {
            match out[start..].find(&close) {
                Some(end) => out.replace_range(start..start + end + close.len(), ""),
                None => {
                    out.truncate(start);
                    break;
                }
            }
        }
    }
    out
}

/// Extract a unified diff from a model reply: a ```diff fenced block, or a
/// bare diff starting at the first `diff --git` / `--- ` line.
fn extract_diff(text: &str) -> Option<String> {
    let text = strip_thinking(text);
    for block in fenced_blocks(&text) {
        let block = block.trim();
        if looks_like_diff(block) {
            return Some(format!("{block}\n"));
        }
    }

    let mut index = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end();
        if trimmed.starts_with("diff --git") || trimmed.starts_with("--- ") {
            let candidate = text[index..].trim_end().trim_end_matches("```").trim_end();
            if looks_like_diff(candidate) {
                return Some(format!("{candidate}\n"));
            }
        }
        index += line.len();
    }
    None
}

fn looks_like_diff(text: &str) -> bool {
    let mut has_header = false;
    let mut has_hunk = false;
    for line in text.lines() {
        if line.starts_with("--- ") || line.starts_with("diff --git") {
            has_header = true;
        }
        if line.starts_with("@@") {
            has_hunk = true;
        }
    }
    has_header && has_hunk
}

/// Reduce a free-form name to a lowercase filesystem-safe slug joined by
/// `sep`. Errors when nothing usable remains (defends against path
/// traversal and junk names from the model).
fn slugify(raw: &str, sep: char) -> Result<String> {
    let mut out = String::new();
    let mut prev_sep = true;
    for c in raw.trim().chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_sep = false;
        } else if !prev_sep {
            out.push(sep);
            prev_sep = true;
        }
    }
    let out = out.trim_matches(sep).to_string();
    if out.is_empty() {
        bail!("'{raw}' does not reduce to a usable name");
    }
    Ok(out)
}

/// Reduce a proposed script file name to a safe basename (no directories,
/// no traversal, conservative character set).
fn sanitize_file_name(raw: &str) -> Result<String> {
    let name: String = Path::new(raw)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            n.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                        c
                    } else {
                        '-'
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let name = name.trim_matches(['.', '-']).to_string();
    if name.is_empty() {
        bail!("'{raw}' is not a usable file name");
    }
    Ok(name)
}

/// Pick a script extension from the interpreter name.
fn script_extension(interpreter: Option<&str>) -> &'static str {
    match interpreter {
        Some(i) if i.contains("python") => "py",
        Some(i) if i.contains("node") || i.contains("deno") || i.contains("bun") => "js",
        _ => "sh",
    }
}

fn repo_url() -> String {
    std::env::var("WIZARD_SOURCE_REPO").unwrap_or_else(|_| DEFAULT_REPO_URL.to_string())
}

/// `true` when `cmd --version` runs successfully.
fn command_exists(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Locate `cargo`: on `PATH`, or in `~/.cargo/bin` (a just-in-time rustup
/// install may not be on `PATH` yet in this process).
fn find_cargo() -> Option<PathBuf> {
    if command_exists("cargo") {
        return Some(PathBuf::from("cargo"));
    }
    let candidate = dirs::home_dir()?.join(".cargo").join("bin").join("cargo");
    candidate.is_file().then_some(candidate)
}

/// `PATH` with `~/.cargo/bin` appended, so a freshly rustup-installed
/// toolchain resolves `rustc` during the build.
fn augmented_path() -> std::ffi::OsString {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut paths: Vec<PathBuf> = std::env::split_paths(&current).collect();
    if let Some(home) = dirs::home_dir() {
        let cargo_bin = home.join(".cargo").join("bin");
        if !paths.contains(&cargo_bin) {
            paths.push(cargo_bin);
        }
    }
    std::env::join_paths(paths).unwrap_or(current)
}

fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Relative paths of the source files (skipping `.git` and `target`),
/// sorted and capped, for the deep-evolve prompt.
fn source_file_listing(root: &Path) -> String {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name();
                if name == ".git" || name == "target" {
                    continue;
                }
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(root) {
                files.push(rel.display().to_string());
            }
        }
    }
    files.sort();
    files.truncate(MAX_LISTED_FILES);
    files.join("\n")
}

/// Run `binary --version` and check it exits 0 printing a `wizard …`
/// version line, before trusting it to replace the running executable.
fn smoke_test(binary: &Path) -> Result<()> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .with_context(|| format!("running {} --version", binary.display()))?;
    if !output.status.success() {
        bail!(
            "{} --version exited with {}",
            binary.display(),
            output.status
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim_start().starts_with("wizard") {
        bail!(
            "{} --version printed {:?} instead of a wizard version",
            binary.display(),
            stdout.trim()
        );
    }
    Ok(())
}

/// Move the running executable aside as `<name>.prev` (the rollback copy)
/// and copy the new binary into its place.
fn swap_in(built: &Path, exe: &Path) -> Result<PathBuf> {
    let file_name = exe
        .file_name()
        .and_then(|n| n.to_str())
        .context("the current executable has no file name")?;
    let backup = exe.with_file_name(format!("{file_name}.prev"));
    let _ = std::fs::remove_file(&backup);
    // Renaming a running executable is fine on Unix (the inode lives on),
    // and the renamed file doubles as the rollback binary.
    std::fs::rename(exe, &backup)
        .with_context(|| format!("moving {} aside for rollback", exe.display()))?;
    if let Err(err) = std::fs::copy(built, exe) {
        let _ = std::fs::rename(&backup, exe); // restore on failure
        return Err(anyhow!(err).context(format!("copying the new binary to {}", exe.display())));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(exe, std::fs::Permissions::from_mode(0o755));
    }
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_normalizes_names() {
        assert_eq!(
            slugify("Conventional Commits!", '-').unwrap(),
            "conventional-commits"
        );
        assert_eq!(slugify("  ../../Evil Name  ", '-').unwrap(), "evil-name");
        assert_eq!(slugify("mermaid PNG", '_').unwrap(), "mermaid_png");
        assert!(slugify("///", '-').is_err());
    }

    #[test]
    fn sanitize_file_name_strips_directories() {
        assert_eq!(sanitize_file_name("../../evil.sh").unwrap(), "evil.sh");
        assert_eq!(sanitize_file_name("tool name.sh").unwrap(), "tool-name.sh");
        assert!(sanitize_file_name("..").is_err());
    }

    #[test]
    fn strips_thinking_blocks() {
        let text = "<think>secret plan</think>{\"a\":1}";
        assert_eq!(strip_thinking(text), "{\"a\":1}");
        let unterminated = "<think>never closed";
        assert_eq!(strip_thinking(unterminated), "");
    }

    #[test]
    fn extracts_json_from_fenced_reply_with_prose() {
        let reply = "Here you go:\n```json\n{\"channel\":\"skill\",\"name\":\"x\",\"body\":\"b\"}\n```\nDone.";
        let value = extract_json_object(reply).unwrap();
        assert_eq!(value["channel"], "skill");
    }

    #[test]
    fn extracts_bare_json_with_trailing_text() {
        let reply = "{\"channel\":\"subagent\",\"name\":\"r\",\"description\":\"d\",\"system_prompt\":\"p\"} hope that helps";
        let value = extract_json_object(reply).unwrap();
        assert_eq!(value["channel"], "subagent");
    }

    #[test]
    fn parses_each_channel_proposal() {
        let skill: ChannelProposal = serde_json::from_str(
            r#"{"channel":"skill","name":"commits","description":"d","body":"b"}"#,
        )
        .unwrap();
        assert_eq!(skill.channel(), EvolveChannel::Skill);

        let mcp: ChannelProposal = serde_json::from_str(
            r#"{"channel":"mcp_server","name":"computer-use","transport":"stdio","command":"uvx","args":["mcp-computer-use"]}"#,
        )
        .unwrap();
        assert_eq!(mcp.channel(), EvolveChannel::McpServer);

        let tool: ChannelProposal = serde_json::from_str(
            r##"{"channel":"scripted_tool","name":"mermaid_png","description":"d","script_content":"#!/bin/sh\necho hi","parameters":{"type":"object"}}"##,
        )
        .unwrap();
        assert_eq!(tool.channel(), EvolveChannel::ScriptedTool);

        let sub: ChannelProposal = serde_json::from_str(
            r#"{"channel":"subagent","name":"reviewer","description":"d","system_prompt":"p","tool_scope":["read_file"],"max_steps":10}"#,
        )
        .unwrap();
        assert_eq!(sub.channel(), EvolveChannel::Subagent);
    }

    #[test]
    fn extracts_diff_from_fenced_block() {
        let reply = "Sure:\n```diff\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n```\n";
        let diff = extract_diff(reply).unwrap();
        assert!(diff.starts_with("--- a/src/main.rs"));
        assert!(diff.ends_with('\n'));
        assert!(diff.contains("@@ -1,2 +1,2 @@"));
    }

    #[test]
    fn extracts_bare_diff() {
        let reply = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n";
        let diff = extract_diff(reply).unwrap();
        assert!(diff.starts_with("diff --git"));
    }

    #[test]
    fn rejects_non_diff_text() {
        assert!(extract_diff("no patch here, sorry").is_none());
    }

    #[test]
    fn script_extension_matches_interpreter() {
        assert_eq!(script_extension(Some("python3")), "py");
        assert_eq!(script_extension(Some("node")), "js");
        assert_eq!(script_extension(Some("bash")), "sh");
        assert_eq!(script_extension(None), "sh");
    }

    #[test]
    fn tail_lines_keeps_the_end() {
        let text = "1\n2\n3\n4";
        assert_eq!(tail_lines(text, 2), "3\n4");
        assert_eq!(tail_lines(text, 10), text);
    }

    /// Temp dir removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sample_event(description: &str, outcome: EvolveOutcome) -> EvolutionEvent {
        EvolutionEvent {
            timestamp: Utc::now(),
            tier: EvolveTier::Runtime,
            description: description.to_string(),
            outcome,
            diff: None,
            build_ok: None,
        }
    }

    #[test]
    fn evolution_event_round_trips_through_jsonl() {
        let event = EvolutionEvent {
            timestamp: Utc::now(),
            tier: EvolveTier::Deep,
            description: "add a status panel".to_string(),
            outcome: EvolveOutcome::DeepRebuilt {
                binary: PathBuf::from("/tmp/wizard-new"),
            },
            diff: Some("--- a/x\n+++ b/x\n".to_string()),
            build_ok: Some(true),
        };
        let line = serde_json::to_string(&event).unwrap();
        let parsed: EvolutionEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed.tier, EvolveTier::Deep);
        assert_eq!(parsed.description, "add a status panel");
        assert_eq!(parsed.diff.as_deref(), Some("--- a/x\n+++ b/x\n"));
        assert_eq!(parsed.build_ok, Some(true));
        match parsed.outcome {
            EvolveOutcome::DeepRebuilt { binary } => {
                assert_eq!(binary, PathBuf::from("/tmp/wizard-new"));
            }
            other => panic!("wrong outcome variant: {other:?}"),
        }
    }

    #[test]
    fn runtime_event_omits_deep_only_fields() {
        let line = serde_json::to_string(&sample_event(
            "learn conventional commits",
            EvolveOutcome::SkillAdded {
                name: "commits".to_string(),
                path: PathBuf::from("/tmp/skills/commits/SKILL.md"),
            },
        ))
        .unwrap();
        assert!(!line.contains("\"diff\""), "absent diff is not serialized");
        assert!(
            !line.contains("\"build_ok\""),
            "absent build_ok is not serialized"
        );
        assert!(
            line.contains("\"kind\":\"skill_added\""),
            "outcome is kind-tagged"
        );
    }

    #[test]
    fn append_event_creates_parents_and_appends_lines() {
        let tmp = TempDir::new();
        let log = tmp.0.join("nested").join("evolution.jsonl");

        append_event(
            &log,
            &sample_event(
                "first",
                EvolveOutcome::SubagentAdded {
                    name: "reviewer".to_string(),
                },
            ),
        )
        .unwrap();
        append_event(&log, &sample_event("second", EvolveOutcome::Denied)).unwrap();

        let raw = std::fs::read_to_string(&log).unwrap();
        let events: Vec<EvolutionEvent> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].description, "first");
        assert_eq!(events[1].description, "second");
        assert!(matches!(events[1].outcome, EvolveOutcome::Denied));
    }
}
