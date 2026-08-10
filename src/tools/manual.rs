//! Native `manual` tool: the on-demand half of the system prompt.
//!
//! The always-on prompt carries the charter's *index* (the topic ids, the
//! capability ladder's rung names, and the three rules that govern every
//! reply); the sections themselves live here, one call away. See
//! [`crate::agent::prompts`] for the split and why the depth is not resident.
//!
//! This tool is the other half of that split, and it only works as a pair: a
//! prompt that advertises a topic no page serves is a dangling pointer the
//! model cannot route around, so
//! `the_manual_resolves_every_topic_the_system_prompt_names` in this file's
//! tests fails the build rather than shipping one.
//!
//! Everything served here is a compiled-in `&'static str` ([`WIZARD.md`] is
//! `include_str!`d by `prompts`), so a call costs a string copy and no I/O.
//! That is deliberate: a model that has to weigh whether a lookup is worth
//! paying for will guess instead, which is exactly the failure §4 of the
//! charter exists to prevent. The description says so, in those words.
//!
//! [`WIZARD.md`]: https://github.com/teddytennant/wizard/blob/main/WIZARD.md

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::prompts::{ManualPage, manual_page, manual_pages};

use super::{
    MAX_OUTPUT_BYTES, Tool, ToolAccess, ToolContext, ToolError, ToolOutput, parse_args,
    truncate_output,
};

/// Advertised name of the tool.
///
/// The system prompt names it in three places (the charter digest's lead, the
/// ladder summary, and the memory section). Nothing but a test binds those
/// literals to this constant; that test is
/// `the_manual_is_registered_under_the_name_the_prompt_calls`.
pub const MANUAL_TOOL_NAME: &str = "manual";

/// Arguments for [`ManualTool`].
#[derive(Debug, Default, Deserialize)]
pub struct ManualArgs {
    /// Which page to read: an advertised id, a section number, an id prefix,
    /// or a word from the title. Absent (or empty) lists every topic.
    ///
    /// The aliases are not documented in the schema; they are here because a
    /// model that has just read "topic ids" in its prompt reaches for
    /// `section` or `id` often enough that failing the call over the key name
    /// would waste a round trip on a lookup that is supposed to be free.
    #[serde(
        default,
        alias = "section",
        alias = "id",
        alias = "name",
        alias = "page"
    )]
    pub topic: Option<String>,
}

/// `manual`: read one section of the operating charter in full.
pub struct ManualTool;

#[async_trait]
impl Tool for ManualTool {
    fn name(&self) -> &str {
        MANUAL_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Read one section of your operating charter (WIZARD.md) in full: the capability \
         ladder and what each rung costs, the browser-use recipe, how to delegate to \
         subagents, the grounding rules, the publish flow, and the guardrails. Your system \
         prompt carries only the index of these; this is where the text lives. Pass 'topic' \
         as an advertised id ('recipe-browser-use'), a section number ('2'), or a word from \
         the title; omit it to list every topic. Everything it returns is compiled into this \
         binary, so a call is instant and costs nothing but the text itself. Look a section \
         up before you act on its subject, and look one up speculatively rather than \
         guessing what it says."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "topic": {
                    "type": "string",
                    "description": "Topic id as advertised in the system prompt (e.g. 'recipe-browser-use'), a charter section number ('2'), or any word from a section title. Omit to list every available topic."
                }
            },
            "required": []
        })
    }

    /// Reading a compiled-in constant observes nothing and changes nothing, so
    /// the manual stays available in plan mode. That matters more than it
    /// looks: plan mode is exactly when the model should be reading §4 before
    /// committing to an approach.
    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ManualArgs = parse_args(self.name(), args)?;
        let pages = manual_pages();
        let topic = args.topic.unwrap_or_default();
        let topic = topic.trim();

        // No topic is a legitimate "what is in here" call, not a miss: answer
        // it with the index and no error flag.
        if topic.is_empty() {
            return Ok(ToolOutput::ok(index(&pages)));
        }

        match manual_page(topic) {
            Some(page) => Ok(ToolOutput::ok(truncate_output(
                render(&page),
                MAX_OUTPUT_BYTES,
            ))),
            // A miss is reported as one (the model asked for something that is
            // not there) but never as a dead end: the index rides along so the
            // retry is the next call rather than the next guess.
            None => Ok(ToolOutput::error(format!(
                "No manual topic matches '{topic}'.\n\n{}",
                index(&pages)
            ))),
        }
    }
}

/// One page, with its charter title restored as a heading. The body is
/// verbatim: the whole point of the lookup is that the model reads what the
/// charter says rather than a paraphrase of it.
fn render(page: &ManualPage) -> String {
    format!("# {}\n\n{}\n", page.title, page.body)
}

/// The topic list: every id the model may pass back, with the title that says
/// what it holds. Ids first, because that is the argument.
fn index(pages: &[ManualPage]) -> String {
    let mut out = String::from(
        "Manual topics. Call `manual` again with one of these ids (or a section number, \
         or a word from a title):\n",
    );
    for page in pages {
        out.push_str(&format!("- `{}`: {}\n", page.id, page.title));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::agent::prompts::{
        CONTEXT_PROMPT, PromptSection, TODO_PROMPT, system_prompt_sections,
    };
    use crate::config::Mode;
    use crate::tools::registry::ToolRegistry;

    /// The charter as it sits on disk, read independently of
    /// `crate::agent::prompts` so this file's round-trip test compares the tool
    /// against the source of truth rather than against the same copy the tool
    /// is built from.
    const CHARTER_ON_DISK: &str = include_str!("../../WIZARD.md");

    fn ctx() -> ToolContext {
        ToolContext::new(std::env::temp_dir())
    }

    async fn read(topic: &str) -> ToolOutput {
        ManualTool
            .execute(json!({ "topic": topic }), &ctx())
            .await
            .expect("a manual lookup never fails the call")
    }

    /// The prompt sections whose text is a compiled constant of this build.
    ///
    /// `personality` is excluded on purpose: `~/.wizard/system_prompt.md` may
    /// replace it on the machine running the tests, and a developer's override
    /// must not be able to fail (or to pass) an assertion about what *this*
    /// tree promises the model.
    fn compiled_prompt_sections(mode: Mode) -> Vec<PromptSection> {
        let mut sections: Vec<PromptSection> = system_prompt_sections(mode, &[], None, None)
            .into_iter()
            .filter(|section| section.name != "personality")
            .collect();
        // Appended by the agent loop on every ordinary run, so they are part of
        // what the model is told even though they are not prompt *sections*.
        for (name, text) in [("todo", TODO_PROMPT), ("context", CONTEXT_PROMPT)] {
            sections.push(PromptSection {
                name,
                text: text.to_string(),
            });
        }
        sections
    }

    /// Every ``  `manual` topic `x`  `` reference in `text`, in order.
    fn referenced_topics(text: &str) -> Vec<String> {
        let lead = format!("`{MANUAL_TOOL_NAME}` topic `");
        let mut found = Vec::new();
        let mut rest = text;
        while let Some(start) = rest.find(&lead) {
            rest = &rest[start + lead.len()..];
            let Some(end) = rest.find('`') else { break };
            found.push(rest[..end].to_string());
            rest = &rest[end..];
        }
        found
    }

    /// The ids advertised on the digest's `Topics: ...` line.
    fn advertised_topics(text: &str) -> Vec<String> {
        let Some(line) = text.lines().find(|line| line.starts_with("Topics: ")) else {
            return Vec::new();
        };
        line.split('`')
            // `a` (A); `b` (B): the ids are the odd-indexed splits.
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect()
    }

    /// Every ``  `name` tool  `` the prompt tells the model to call.
    fn tools_named_in(text: &str) -> Vec<String> {
        const TRAILER: &str = "` tool";
        let mut found = Vec::new();
        let mut rest = text;
        while let Some(at) = rest.find(TRAILER) {
            let head = &rest[..at];
            if let Some(start) = head.rfind('`') {
                found.push(head[start + 1..].to_string());
            }
            rest = &rest[at + TRAILER.len()..];
        }
        found
    }

    /// THE acceptance test for the prompt/manual split: every topic the
    /// always-on prompt names must be a topic this tool actually resolves.
    ///
    /// Nothing else connects the two halves. When the charter body moved behind
    /// this lookup, the prompt kept advertising ids while the tool that served
    /// them did not exist, and the suite stayed green through the whole
    /// release, because no test crossed the seam. This one does.
    #[tokio::test]
    async fn the_manual_resolves_every_topic_the_system_prompt_names() {
        for mode in [Mode::Genie, Mode::Sovereign] {
            let mut named: Vec<String> = Vec::new();
            for section in compiled_prompt_sections(mode) {
                named.extend(advertised_topics(&section.text));
                named.extend(referenced_topics(&section.text));
            }
            assert!(
                named.len() >= manual_pages().len(),
                "the {mode} prompt advertises {} topics for {} pages; the digest's index \
                 stopped listing them",
                named.len(),
                manual_pages().len()
            );
            assert!(
                named.iter().any(|topic| topic == "memory"),
                "the memory section must still point at the rules it no longer carries"
            );
            for topic in named {
                let out = read(&topic).await;
                assert!(
                    !out.is_error,
                    "the {mode} system prompt tells the model to read `manual` topic \
                     {topic:?}, and the tool does not serve it:\n{}",
                    out.content
                );
                assert!(
                    out.content.lines().count() > 1,
                    "topic {topic:?} resolved to an empty page"
                );
            }
        }
    }

    /// The other half of the same seam: the prompt must not name a *tool* that
    /// is missing from the registry either. This is the general form of the bug
    /// above, and it costs one assertion.
    #[tokio::test]
    async fn the_manual_is_registered_under_the_name_the_prompt_calls() {
        let registry = ToolRegistry::with_native_tools();
        let tool = registry.get(MANUAL_TOOL_NAME).unwrap_or_else(|| {
            panic!(
                "`{MANUAL_TOOL_NAME}` is not registered, and the system prompt tells the \
                 model to call it"
            )
        });
        assert_eq!(tool.name(), MANUAL_TOOL_NAME);
        assert_eq!(
            tool.access(),
            ToolAccess::ReadOnly,
            "reading a compiled constant must stay legal in plan mode"
        );
        assert!(
            tool.description().contains("costs nothing"),
            "the description has to say the lookup is cheap, or the model rations it"
        );

        for mode in [Mode::Genie, Mode::Sovereign] {
            let mut named = Vec::new();
            for section in compiled_prompt_sections(mode) {
                named.extend(tools_named_in(&section.text));
            }
            assert!(
                named.iter().any(|name| name == MANUAL_TOOL_NAME),
                "the {mode} prompt must tell the model the manual exists: {named:?}"
            );
            for name in named {
                assert!(
                    registry.get(&name).is_some(),
                    "the {mode} system prompt tells the model to call the `{name}` tool, \
                     which is registered nowhere"
                );
            }
        }
    }

    /// The depth is not just reachable, it is complete: every content line of
    /// `WIZARD.md` comes back through an actual tool call.
    ///
    /// `prompts::tests::the_manual_serves_the_whole_charter` proves the same of
    /// the page list; this proves it of the path the model uses, which is the
    /// one that was broken.
    #[tokio::test]
    async fn every_line_of_the_charter_comes_back_through_the_tool() {
        let mut served = String::new();
        for page in manual_pages() {
            let out = read(&page.id).await;
            assert!(!out.is_error, "advertised id {:?} did not resolve", page.id);
            served.push_str(&out.content);
            served.push('\n');
        }
        let served = normalized(&served);

        for line in CHARTER_ON_DISK.lines() {
            let line = line.trim();
            // Headings survive as page titles; the `---` rules and the `# `
            // title carry no content.
            if line.is_empty() || line.starts_with('#') || line == "---" {
                continue;
            }
            assert!(
                served.contains(&normalized(line)),
                "charter line {line:?} does not come back from any `manual` call"
            );
        }
    }

    /// The five-rung ladder, verbatim, including the prose the digest
    /// deliberately leaves out. These are the exact strings
    /// `always_on_prompt_has_the_rung_names_but_not_the_ladder` asserts are
    /// *absent* from the prompt, so the pair of tests says the whole thing:
    /// not resident, and not lost.
    #[tokio::test]
    async fn the_ladder_page_carries_all_five_rungs_and_their_prose() {
        let out = read("1").await;
        assert!(!out.is_error, "{}", out.content);
        for rung in [
            "1. **Skill**",
            "2. **MCP server**",
            "3. **Scripted tool**",
            "4. **Subagent**",
            "5. **Deep evolve",
        ] {
            assert!(
                out.content.contains(rung),
                "rung {rung:?} missing from the ladder page:\n{}",
                out.content
            );
        }
        for prose in [
            "knowledge or procedure, not new code",
            "Pick the lowest rung that solves it",
            "Don't deep-evolve what a skill covers",
        ] {
            assert!(
                out.content.contains(prose),
                "the ladder page must carry {prose:?}, which the prompt no longer does"
            );
        }

        // The recipe the digest points at separately, because a capability gap
        // is the most common reason to open the manual at all.
        let out = read("recipe-browser-use").await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("npx -y @playwright/mcp@latest"));
    }

    /// The memory rules, under exactly the id the memory section advertises.
    #[tokio::test]
    async fn the_memory_topic_serves_the_rules_the_prompt_dropped() {
        let out = read("memory").await;
        assert!(!out.is_error, "{}", out.content);
        for rule in [
            "Never save what the repo already records",
            "update it (save over its name)",
            "Delete a memory that turns out to be wrong",
        ] {
            assert!(out.content.contains(rule), "memory rules missing {rule:?}");
        }
    }

    /// A call with no topic, or with one nothing matches, always answers with
    /// the list of what is there. A lookup tool that can only say "no" teaches
    /// the model to stop asking.
    #[tokio::test]
    async fn no_topic_or_an_unknown_one_lists_what_is_there() {
        let pages = manual_pages();

        for empty in [json!({}), Value::Null, json!({ "topic": "   " })] {
            let out = ManualTool
                .execute(empty.clone(), &ctx())
                .await
                .unwrap_or_else(|err| panic!("{empty} should list topics, not fail: {err}"));
            assert!(!out.is_error, "listing topics is not an error");
            for page in &pages {
                assert!(
                    out.content.contains(&format!("`{}`", page.id)),
                    "topic {:?} missing from the index",
                    page.id
                );
            }
        }

        let out = read("how do i fly").await;
        assert!(out.is_error, "an unknown topic is a miss");
        assert!(out.content.contains("No manual topic matches"));
        for page in &pages {
            assert!(
                out.content.contains(&format!("`{}`", page.id)),
                "a miss must still show the way: {:?} missing",
                page.id
            );
        }
    }

    /// Whatever the model types has to land: the advertised id, a section
    /// number, a `§`-prefixed one, a prefix, or a word from the title.
    #[tokio::test]
    async fn a_lookup_accepts_the_spellings_a_model_reaches_for() {
        for spelling in ["6", "§6", "guardrails", "GUARD", "Guardrails"] {
            let out = read(spelling).await;
            assert!(
                !out.is_error,
                "{spelling:?} should reach the guardrails page"
            );
            assert!(
                out.content.starts_with("# 6. Guardrails"),
                "{spelling:?} reached the wrong page:\n{}",
                out.content
            );
        }

        // An argument under a name the model borrowed from the prompt's prose
        // still resolves, rather than silently listing the index.
        let out = ManualTool
            .execute(json!({ "section": "6" }), &ctx())
            .await
            .unwrap();
        assert!(
            out.content.starts_with("# 6. Guardrails"),
            "{}",
            out.content
        );
    }

    /// The tool answers from compiled-in text, so it must not care where it is
    /// run: no project, no store, no network.
    #[tokio::test]
    async fn a_lookup_needs_nothing_from_the_working_directory() {
        let ctx = ToolContext::new(PathBuf::from("/nonexistent/wizard-manual-test"));
        let out = ManualTool
            .execute(json!({ "topic": "guardrails" }), &ctx)
            .await
            .expect("the manual does not touch the filesystem");
        assert!(!out.is_error);
        assert!(out.content.contains("No em dashes"));
    }

    /// Whitespace-insensitive comparison, so a wrapped charter line still
    /// matches the page that serves it.
    fn normalized(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}
