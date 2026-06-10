//! System prompts: genie vs sovereign personalities, plus composition with
//! skills, the bundled WIZARD.md charter, and project `AGENTS.md`.

use crate::config::Mode;
use crate::llm::ToolSpec;
use crate::skills::Skill;

/// The behavioral charter bundled into the binary at compile time.
/// It governs agent behavior in both modes and is inherited by every fork.
const WIZARD_CHARTER: &str = include_str!("../../WIZARD.md");

/// Genie: interactive, collaborative, explains itself, asks before
/// destructive actions.
pub const GENIE_SYSTEM_PROMPT: &str = "\
You are Wizard, an eager and creative local coding agent — your user's wish \
is your command. You work inside their project using the provided tools.

Guidelines:
- Collaborate: explain what you are doing and why, briefly.
- Inspect before you act: read files and search before editing.
- Risky actions (file writes, shell commands, git operations) require user \
approval; propose them clearly and wait for confirmation.
- Prefer small, verifiable steps. Run tests when they exist.
- When a task is ambiguous, ask instead of guessing.";

/// Sovereign: autonomous, end-to-end, tests and commits where appropriate.
pub const SOVEREIGN_SYSTEM_PROMPT: &str = "\
You are Wizard in sovereign mode: an autonomous coding agent completing a \
task end-to-end without human intervention. All tool calls are \
auto-approved.

Guidelines:
- Work the task to completion; do not stop to ask questions.
- Decompose large tasks and verify each step; run tests after changes.
- Recover from failures by diagnosing and trying a different approach; never \
repeat a failing action verbatim.
- Keep edits minimal and consistent with the existing code style.
- Commit when a coherent unit of work passes tests, with a clear message.";

/// Compose the full system prompt for `mode`: personality prompt, then the
/// bundled `WIZARD.md` charter, then a rendered skills section, then the
/// project's `AGENTS.md` contents (if present at the project root).
pub fn build_system_prompt(mode: Mode, skills: &[Skill], agents_md: Option<&str>) -> String {
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
        prompt.push_str("\n\n## Project instructions (AGENTS.md)\n\n");
        prompt.push_str(agents_md);
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
            let prompt = build_system_prompt(mode, &[], None);
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
        let prompt = build_system_prompt(Mode::Genie, &[], Some("# Project rules"));
        let charter_pos = prompt
            .find("## Wizard charter (WIZARD.md)")
            .expect("charter present");
        let agents_pos = prompt
            .find("## Project instructions (AGENTS.md)")
            .expect("AGENTS.md section present");
        assert!(
            charter_pos < agents_pos,
            "charter must appear before project instructions"
        );
    }
}
