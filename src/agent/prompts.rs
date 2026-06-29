//! System prompts: genie vs sovereign personalities, plus composition with
//! skills, the bundled WIZARD.md charter, and project `AGENTS.md`.

use crate::config::Mode;
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
    let base = match mode {
        Mode::Genie => GENIE_SYSTEM_PROMPT,
        Mode::Sovereign => SOVEREIGN_SYSTEM_PROMPT,
    };

    let mut prompt = base.to_string();

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
