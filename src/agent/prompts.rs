//! System prompts: genie vs sovereign personalities, plus composition with
//! skills, the bundled WIZARD.md charter, and project `AGENTS.md`.

use std::path::{Path, PathBuf};

use crate::config::{Config, Mode};
use crate::llm::ToolSpec;
use crate::skills::Skill;

/// The behavioral charter bundled into the binary at compile time.
/// It governs agent behavior in both modes and is inherited by every fork.
const WIZARD_CHARTER: &str = include_str!("../../WIZARD.md");

/// Genie: interactive, bypass-permissions agent — acts directly without
/// asking permission for file writes, shell, or git operations.
pub const GENIE_SYSTEM_PROMPT: &str = "\
You are Wizard, an eager and creative local agent — your user's wish \
is your command. You work inside their project using the provided tools.

Guidelines:
- Collaborate: explain what you are doing and why, briefly.
- Inspect before you act: read files and search before editing.
- Act directly: file writes, shell commands, and git operations run without \
asking permission — just do the work and narrate briefly as you go.
- Prefer small, verifiable steps. Run tests when they exist.
- When the TASK itself is genuinely ambiguous, ask instead of guessing \
(that is about intent, not permission).";

/// Sovereign: autonomous, end-to-end, tests and commits where appropriate.
pub const SOVEREIGN_SYSTEM_PROMPT: &str = include_str!("sovereign_prompt.md");

/// Appended to the system prompt while plan mode is active (the agent
/// re-composes the prompt whenever the flag flips, so this block disappears
/// once a plan is approved).
pub const PLAN_MODE_PROMPT: &str = "\
## Plan mode (active)

You are in PLAN MODE. Investigate using read-only tools only (reading, \
listing, and searching files; inspecting git state); every other tool is \
blocked until your plan is approved. Do not attempt to make changes yet. \
Once you have explored enough to understand the shape of the task but still \
have genuine open questions whose answers would change the plan (scope, \
trade-offs, ambiguous intent, where something should live), call the \
`interview` tool to ask the user a short batch of clarifying questions \
before you commit to an approach — prefer one well-aimed interview over \
guessing. Skip it when the task is already unambiguous. \
Once you understand the task, present your implementation plan by calling \
the `exit_plan` tool with the complete plan as markdown. If the plan is \
approved, plan mode ends and you carry it out; if it is rejected, refine \
the plan using the feedback you receive and call `exit_plan` again.";

/// Appended after [`PLAN_MODE_PROMPT`] when omakase mode is active: the agent
/// has full authority over the approach. It still explores read-only first,
/// but it does not interview the user and its plan is auto-approved — it
/// decides and proceeds. Like plan mode, this block disappears once omakase
/// is turned off (the prompt is recomposed on every flag flip).
pub const OMAKASE_PROMPT: &str = "\
## Omakase mode (chef's choice)

Omakase is on: this is the chef's-choice flavor of plan mode — you have full \
authority over the approach and the user has handed you the wheel. After \
exploring read-only, do NOT call `interview`; resolve every open question \
yourself by making the most reasonable assumption a senior engineer would, \
and choose the approach you judge best. Your plan is auto-approved — there \
is no human review gate — so when you call `exit_plan`, make the plan \
self-justifying: state the approach you picked, the alternatives you \
weighed, the assumptions you made, and why. Then execute it end to end, \
verify your work, and deliver a polished result. Be decisive and tasteful; \
surprise them with quality, not with questions.";

/// Appended to the system prompt when the `todo` tool is registered: keep a
/// working todo list for multi-step tasks so every surface can mirror
/// progress.
pub const TODO_PROMPT: &str = "\
## Working todo list

For multi-step work, maintain a todo list with the `todo` tool: write the \
full list up front (action \"write\" replaces the entire list), keep exactly \
one item in_progress while you work on it, and mark items completed as soon \
as they are done. Skip the list for trivial single-step tasks.";

/// Always appended: how the agent should steward its own context window.
/// Wizard already auto-compacts, persists every session as JSONL, and exposes
/// `/compact` via `run_command` — this block teaches the model *when* to lean
/// on those, so long or multi-topic sessions do not drown in pollution.
pub const CONTEXT_PROMPT: &str = "\
## Context management (you own your window)

Your conversation history is finite. Wizard already persists every turn to \
`~/.wizard/sessions/<id>.jsonl` and auto-compacts older history when the \
byte or token threshold is hit — treat that as a safety net, not a plan. \
Steer the window yourself:

1. **Stay lean every step.** Prefer short tool output (pipe through \
`head`/`tail`/`wc`, write bulky data to a file and summarize). Delegate \
noisy multi-step work to `spawn_subagent` so intermediate steps never enter \
your context — only the final report does.
2. **Compact when the thread is still useful but bloated.** After a long \
investigation, a finished sub-goal, or when older tool dumps are drowning \
the current task, call `run_command` with `/compact`. That summarizes older \
history into a progress note (goal, decisions, files touched, open next \
steps, todo state) and keeps the recent tail verbatim. Prefer this over \
asking the user to clear.
3. **When the task changes, do not drag the old thread along.** If the user \
pivots to an unrelated task, or you are done with a self-contained unit of \
work and the next one needs a clean slate:
   - Save any durable facts with the `memory` tool (preferences, project \
constraints, standing decisions) — only what should survive sessions.
   - Rewrite the todo list for the new task (or clear it to empty).
   - Call `/compact` so the prior conversation collapses into one progress \
summary. The full prior transcript remains on disk as the session JSONL; \
nothing is lost.
   - Only if the new task must not see the old work at all, tell the user \
that `/clear` rotates to a fresh session file (the previous JSONL is kept \
under `~/.wizard/sessions/`). You cannot run `/clear` yourself — it is the \
user's call — so compact + memory is your default reset.
4. **Do not re-read what compaction already summarized** unless you need a \
specific detail; open the relevant file or the session JSONL instead of \
replaying the whole investigation.
5. **Check pressure.** `/status` (via `run_command` on interactive surfaces) \
reports the current context size. Compact proactively before the automatic \
threshold if the next steps need headroom.

Headless / gateway / continuous runs still auto-compact; `run_command` is \
only available on interactive surfaces, so there lean harder on lean tool \
output and subagents.";

/// Memory guidance injected when the project has saved memories; the rules
/// and then the index (MEMORY.md) follow it.
const MEMORY_PROMPT_WITH_INDEX: &str = "\
You have persistent project memory. The index below lists every saved memory \
(one markdown file each) with its type and a one-line description. Use the \
`memory` tool with action \"read\" to recall a memory in full, \"save\" to \
record a new durable fact or update an existing one, and \"delete\" to drop \
one that turned out to be wrong.";

/// Memory guidance injected when no memories exist yet, so memory
/// bootstraps on first use.
const MEMORY_PROMPT_EMPTY: &str = "\
You have persistent project memory via the `memory` tool, but nothing is \
saved for this project yet. When you learn a durable fact, record it with \
action \"save\" — it appears in your system prompt next session, so the \
memory you write now is the one you read then.";

/// The rules that make memory worth having, injected whether or not anything
/// is saved yet: what the types mean, how memories link to each other, and
/// what must never be written down.
const MEMORY_RULES: &str = "\
Every memory has a type:
- `user` — who the user is: their role, expertise, and standing preferences.
- `feedback` — how you should work: corrections *and* confirmed approaches. \
Include the why, not just the what.
- `project` — ongoing work, goals, and constraints that are not derivable \
from the code or the git history. Convert relative dates (\"next week\") to \
absolute ones.
- `reference` — a pointer to an external resource: a URL, a dashboard, a \
ticket.

Link related memories from a memory's body by name, `[[wiki-style]]`. A link \
to a memory that does not exist yet is fine — it marks something worth \
writing later, not an error. A `read` tells you which links resolve.

A memory has to earn its place:
- Never save what the repo already records: code structure, past fixes, \
anything in the git history. Never save what only matters to the current \
conversation.
- Before saving, look for a memory that already covers the same ground and \
update it (save over its name) instead of creating a near-duplicate.
- Delete a memory that turns out to be wrong. Names are kebab-case, \
descriptions are one line.";

/// Resolve the base personality prompt for `mode`. An external override —
/// `$WIZARD_SYSTEM_PROMPT` if set, otherwise `~/.wizard/system_prompt.md` —
/// replaces the compiled default when it exists and is non-empty. This is the
/// single file external harness-evolution tools (e.g. AHE) mutate; with no
/// override present, the result is byte-identical to the baked prompt. The
/// charter, skills, instructions, and memory sections are always appended on
/// top by [`build_system_prompt`], so the charter cannot be evolved away here.
fn base_system_prompt(mode: Mode) -> String {
    let default = match mode {
        Mode::Genie => GENIE_SYSTEM_PROMPT,
        Mode::Sovereign => SOVEREIGN_SYSTEM_PROMPT,
    };
    override_path()
        .as_deref()
        .and_then(read_prompt_override)
        .unwrap_or_else(|| default.to_string())
}

/// The path an override would live at, if any: the harness bundle's
/// `system_prompt.md` wins when it exists (so a bundle missing the file
/// degrades to the next candidate), then `$WIZARD_SYSTEM_PROMPT`, then the
/// well-known `~/.wizard/system_prompt.md`.
fn override_path() -> Option<PathBuf> {
    if let Some(dir) = Config::harness_dir() {
        let bundled = dir.join("system_prompt.md");
        if bundled.exists() {
            return Some(bundled);
        }
    }
    if let Some(p) = std::env::var_os("WIZARD_SYSTEM_PROMPT") {
        return Some(PathBuf::from(p));
    }
    Config::system_prompt_path().ok()
}

/// Read an override file, returning its trimmed contents only when the file
/// exists and is non-empty. A missing or empty file yields `None` so the
/// caller falls back to the baked default.
fn read_prompt_override(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Compose the full system prompt for `mode`: personality prompt, then the
/// bundled `WIZARD.md` charter, then a rendered skills section, then the
/// project's instruction hierarchy (`agents_md`, assembled by
/// [`crate::instructions`] from WIZARD.md/AGENTS.md/CLAUDE.md files), then
/// the persistent memory section (`memory_index` is the project's
/// MEMORY.md, when any memories are saved).
pub fn build_system_prompt(
    mode: Mode,
    skills: &[Skill],
    agents_md: Option<&str>,
    memory_index: Option<&str>,
) -> String {
    let mut prompt = base_system_prompt(mode);

    // Inject the bundled WIZARD.md charter so every session — genie and
    // sovereign alike — operates under it, and so forks inherit it.
    prompt.push_str("\n\n## Wizard charter (WIZARD.md)\n\n");
    prompt.push_str(WIZARD_CHARTER);

    let skills_section = crate::skills::render_for_prompt(skills);
    if !skills_section.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&skills_section);
    }

    if let Some(agents_md) = agents_md {
        prompt.push_str("\n\n## Project instructions\n\n");
        prompt.push_str(agents_md);
    }

    prompt.push_str("\n\n## Memory\n\n");
    prompt.push_str(match memory_index {
        Some(_) => MEMORY_PROMPT_WITH_INDEX,
        None => MEMORY_PROMPT_EMPTY,
    });
    prompt.push_str("\n\n");
    prompt.push_str(MEMORY_RULES);
    if let Some(index) = memory_index {
        prompt.push_str("\n\n### Memory index (MEMORY.md)\n\n");
        prompt.push_str(index);
    }

    prompt
}

/// Instructions appended to the system prompt when the model lacks native
/// tool calling: defines the prompt-based JSON tool protocol the parser in
/// the agent loop understands (see `docs/byom.md`).
pub const JSON_TOOL_PROTOCOL_PROMPT: &str = "\
You do not have native function calling. To call a tool, reply with ONLY a \
single JSON object on its own line, no other text:

{\"tool\": \"<tool_name>\", \"arguments\": { ... }}

You will receive the tool result in the next message. When you are finished \
with tools, reply with your final answer as plain text.";

/// Render the JSON tool protocol plus the available tool roster, appended to
/// the system prompt for models without native tool calling.
pub fn render_tool_protocol(specs: &[ToolSpec]) -> String {
    let mut section = String::from(JSON_TOOL_PROTOCOL_PROMPT);
    if specs.is_empty() {
        return section;
    }
    section.push_str("\n\n## Available tools\n");
    for spec in specs {
        let schema =
            serde_json::to_string(&spec.function.parameters).unwrap_or_else(|_| "{}".to_string());
        section.push_str(&format!(
            "\n- `{}`: {}\n  arguments schema: {}",
            spec.function.name, spec.function.description, schema
        ));
    }
    section
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled WIZARD.md charter must appear in the composed system prompt
    /// for both modes. This verifies the `include_str!` path is correct and
    /// the charter is actually injected.
    #[test]
    fn system_prompt_contains_wizard_charter() {
        for mode in [Mode::Genie, Mode::Sovereign] {
            let prompt = build_system_prompt(mode, &[], None, None);
            assert!(
                prompt.contains("## Wizard charter (WIZARD.md)"),
                "charter header missing in {mode} prompt"
            );
            // Marker from WIZARD.md §1 ("build the capability" prime directive).
            assert!(
                prompt.contains("build the capability"),
                "charter body missing in {mode} prompt"
            );
        }
    }

    /// Skills and AGENTS.md appear after the charter.
    #[test]
    fn charter_comes_before_agents_md() {
        let prompt = build_system_prompt(Mode::Genie, &[], Some("# Project rules"), None);
        let charter_pos = prompt
            .find("## Wizard charter (WIZARD.md)")
            .expect("charter present");
        let agents_pos = prompt
            .find("## Project instructions")
            .expect("project instructions section present");
        assert!(
            charter_pos < agents_pos,
            "charter must appear before project instructions"
        );
    }

    /// `read_prompt_override` returns trimmed contents for a non-empty file,
    /// and `None` for an empty or missing one (so the baked default is used).
    #[test]
    fn prompt_override_reads_nonempty_file_only() {
        let dir = std::env::temp_dir();
        let pid = std::process::id();

        let present = dir.join(format!("wizard_prompt_override_{pid}.md"));
        std::fs::write(&present, "  CUSTOM EVOLVED PROMPT\n").expect("write temp prompt");
        assert_eq!(
            read_prompt_override(&present).as_deref(),
            Some("CUSTOM EVOLVED PROMPT"),
            "non-empty override should be read and trimmed"
        );
        std::fs::remove_file(&present).ok();

        let empty = dir.join(format!("wizard_prompt_override_empty_{pid}.md"));
        std::fs::write(&empty, "   \n\t").expect("write empty temp prompt");
        assert_eq!(
            read_prompt_override(&empty),
            None,
            "whitespace-only override should fall back to default"
        );
        std::fs::remove_file(&empty).ok();

        let missing = dir.join(format!("wizard_prompt_override_missing_{pid}.md"));
        assert_eq!(
            read_prompt_override(&missing),
            None,
            "missing override → None"
        );
    }

    /// The memory index appears verbatim under its own section when saved
    /// memories exist; without one, the bootstrap guidance still mentions
    /// the `memory` tool.
    #[test]
    fn memory_index_is_injected_when_present() {
        let index = "- [build-system](build-system.md) [project] — uses cargo with lto\n";
        let prompt = build_system_prompt(Mode::Genie, &[], None, Some(index));
        assert!(prompt.contains("## Memory"));
        assert!(prompt.contains("### Memory index (MEMORY.md)"));
        assert!(prompt.contains(index));

        let prompt = build_system_prompt(Mode::Genie, &[], None, None);
        assert!(prompt.contains("## Memory"));
        assert!(prompt.contains("`memory` tool"));
        assert!(
            !prompt.contains("### Memory index (MEMORY.md)"),
            "no index section without saved memories"
        );
    }

    /// The rules — the four types, the `[[link]]` convention, and what must
    /// not be written down — are taught whether or not anything is saved yet.
    /// Without them the model has a store and no idea what belongs in it.
    #[test]
    fn memory_rules_are_taught_with_and_without_an_index() {
        for index in [None, Some("- [x](x.md) [user] — y\n")] {
            let prompt = build_system_prompt(Mode::Genie, &[], None, index);
            for kind in crate::memory::MemoryType::ALL {
                assert!(
                    prompt.contains(&format!("`{kind}`")),
                    "the {kind} type is explained (index: {index:?})"
                );
            }
            assert!(prompt.contains("[[wiki-style]]"));
            assert!(prompt.contains("Never save what the repo already records"));
            assert!(prompt.contains("update it (save over its name)"));
        }
    }

    /// The context-management block is a free-standing constant the agent loop
    /// appends after the composed base prompt. Sanity-check the guidance that
    /// models actually need is present, so a rewrite cannot silently drop it.
    #[test]
    fn context_prompt_teaches_compact_and_task_change_hygiene() {
        let text = CONTEXT_PROMPT;
        assert!(text.contains("## Context management"));
        assert!(text.contains("/compact"));
        assert!(text.contains("~/.wizard/sessions/"));
        assert!(text.contains("When the task changes"));
        assert!(text.contains("spawn_subagent"));
        assert!(text.contains("memory"));
    }

    /// Models on the JSON protocol only know the tools this section names —
    /// it must carry the roster with each tool's argument schema, and stay
    /// bare when no tools are registered.
    #[test]
    fn tool_protocol_renders_the_roster_with_schemas() {
        let specs = vec![ToolSpec::function(
            "read_file",
            "Read a file.",
            serde_json::json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        )];
        let section = render_tool_protocol(&specs);
        assert!(section.contains("You do not have native function calling"));
        assert!(section.contains("## Available tools"));
        assert!(section.contains("`read_file`: Read a file."));
        assert!(
            section.contains("\"path\""),
            "the argument schema is inlined"
        );

        let bare = render_tool_protocol(&[]);
        assert!(bare.contains("You do not have native function calling"));
        assert!(
            !bare.contains("## Available tools"),
            "no roster section without tools"
        );
    }
}
