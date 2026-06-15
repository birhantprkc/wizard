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
pub const SOVEREIGN_SYSTEM_PROMPT: &str = "\
You are Wizard in sovereign mode: an autonomous agent completing a \
task end-to-end without human intervention. All tool calls are \
auto-approved.

Guidelines:
- Work the task to completion; do not stop to ask questions.
- Decompose large tasks and verify each step; run tests after changes.
- Recover from failures by diagnosing and trying a different approach; never \
repeat a failing action verbatim.
- Keep edits minimal and consistent with the existing code style.
- Commit when a coherent unit of work passes tests, with a clear message.";

/// Appended to the system prompt while plan mode is active (the agent
/// re-composes the prompt whenever the flag flips, so this block disappears
/// once a plan is approved).
pub const PLAN_MODE_PROMPT: &str = "\
## Plan mode (active)

You are in PLAN MODE. Investigate using read-only tools only (reading, \
listing, and searching files; inspecting git state); every other tool is \
blocked until your plan is approved. Do not attempt to make changes yet. \
Once you understand the task, present your implementation plan by calling \
the `exit_plan` tool with the complete plan as markdown. If the plan is \
approved, plan mode ends and you carry it out; if it is rejected, refine \
the plan using the feedback you receive and call `exit_plan` again.";

/// Appended to the system prompt when the `todo` tool is registered: keep a
/// working todo list for multi-step tasks so every surface can mirror
/// progress.
pub const TODO_PROMPT: &str = "\
## Working todo list

For multi-step work, maintain a todo list with the `todo` tool: write the \
full list up front (action \"write\" replaces the entire list), keep exactly \
one item in_progress while you work on it, and mark items completed as soon \
as they are done. Skip the list for trivial single-step tasks.";

/// Memory guidance injected when the project has saved memories; the index
/// (MEMORY.md) follows it.
const MEMORY_PROMPT_WITH_INDEX: &str = "\
You have persistent project memory. The index below lists saved memories \
(one per file). Use the `memory` tool with action \"read\" to recall \
details, \"save\" to record new durable facts (user preferences, project \
conventions, decisions not derivable from the code), and \"delete\" for \
stale ones. Keep names kebab-case and descriptions one line. Don't save \
things the repo already records.";

/// Memory guidance injected when no memories exist yet, so memory
/// bootstraps on first use.
const MEMORY_PROMPT_EMPTY: &str = "\
You have persistent project memory via the `memory` tool, but nothing is \
saved for this project yet. When you learn a durable fact (user \
preferences, project conventions, decisions not derivable from the code), \
record it with action \"save\" (kebab-case name, one-line description); it \
will appear in your system prompt next session. Don't save things the repo \
already records.";

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

/// The path an override would live at, if any: `$WIZARD_SYSTEM_PROMPT` wins,
/// else the well-known `~/.wizard/system_prompt.md`.
fn override_path() -> Option<PathBuf> {
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
    match memory_index {
        Some(index) => {
            prompt.push_str(MEMORY_PROMPT_WITH_INDEX);
            prompt.push_str("\n\n### Memory index (MEMORY.md)\n\n");
            prompt.push_str(index);
        }
        None => prompt.push_str(MEMORY_PROMPT_EMPTY),
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
        let index = "- [build-system](build-system.md) — uses cargo with lto\n";
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
}
