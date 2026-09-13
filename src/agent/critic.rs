//! Independent verification of a claim of done.
//!
//! An agent that trusts its own "done" is grading its own homework: the model
//! that just decided the work is finished is the last one who should rule on
//! it. Both verifiers here answer that by spawning a *fresh* subagent, one
//! that never saw the builder's reasoning or claims, and acting on its verdict
//! rather than on the say-so.
//!
//! Two of them, because they answer different questions.
//!
//! # The goal critic
//!
//! Mission scope, for the continuous goal loop: *is the standing goal met?*
//! Binary on purpose. A score out of ten drifts up over rounds until every
//! artifact is a nine; `OURS` / `BAR` / `PLATEAU` cannot. `BAR` carries the one
//! gap worth another round, and two `PLATEAU`s in a row mean the critic cannot
//! name a gap another round would close, which is the loop's cue to stop rather
//! than churn. [`crate::agent::Agent::critique_goal`] runs it.
//!
//! # The completion review
//!
//! Turn scope, for every autonomous run: *is the request you just called done
//! actually satisfied?* One pass, never a loop of them, and one round of
//! feedback when it fails. It shares the goal critic's machinery (the fresh
//! subagent, the first-line verdict, the gap carried back) and differs in what
//! it is told to do: restate the request, check that everything the request
//! named exists and is not empty, re-run the acceptance path the request states,
//! and ask whether the answer survives a change of input. See [`review_config`].
//!
//! Both verdict parsers and both decision functions are pure and tested here.

use crate::agent::subagent::SubagentConfig;
use crate::config::StepBudget;
use std::path::Path;

/// How many steps the critic may take: enough to read the files and run a quick
/// look, not enough to wander. It is read-only, so it cannot do harm with them.
const CRITIC_MAX_STEPS: u32 = 20;

/// Longest gap text carried back to the builder. A critic that writes an essay
/// still hands the loop one actionable line, not a wall.
const MAX_GAP_CHARS: usize = 600;

/// A critic's binary judgement of whether the goal is met.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalVerdict {
    /// The artifact meets the goal (beats the bar, if one was fetched).
    Ours,
    /// It does not yet. The single biggest gap to close before judging again.
    Bar(String),
    /// The critic cannot name a gap another round would close.
    Plateau,
}

impl GoalVerdict {
    /// A one-line description for a notice or a mission note.
    pub fn summary(&self) -> String {
        match self {
            Self::Ours => "OURS — the critic signed off on the current artifact".to_string(),
            Self::Bar(gap) => format!("BAR — {gap}"),
            Self::Plateau => {
                "PLATEAU — the critic can name no gap another round would close".to_string()
            }
        }
    }
}

/// The critic's system prompt. Independence is the load-bearing part: it is
/// spawned with a fresh context and no history, so it physically cannot see the
/// builder's turn, and this prompt tells it not to reconstruct one.
const CRITIC_SYSTEM_PROMPT: &str = "\
You are an independent critic for a goal-driven agent. You have never seen the \
builder and you get none of its reasoning, reports, or claims — only the goal \
and the project on disk. Do not imagine what the builder intended; judge what \
is actually there.\n\
\n\
Inspect the real files, not a summary of them. If a quality bar has been \
fetched under `.gauntlet/bar/`, compare the artifact against it; otherwise \
judge the artifact against the goal's own success criteria. If the goal is \
code, its tests passing is part of meeting it.\n\
\n\
Return your verdict as the FIRST line of your reply, exactly one of:\n\
  OURS      — the artifact meets the goal (beats the bar, if there is one)\n\
  BAR       — it does not; on the next line, name the SINGLE biggest gap\n\
  PLATEAU   — you cannot name a gap another round would close\n\
\n\
No scores out of ten. No praise. No advice beyond the one gap. If you are \
unsure whether a round could close the distance, that is PLATEAU, not OURS.";

/// The critic subagent's definition. Read-only is enforced by the spawn
/// options, not here, so this stays a plain description; `tool_scope` is left
/// open so the read-only registry keeps every inspection tool the install has.
pub fn critic_config() -> SubagentConfig {
    SubagentConfig {
        name: "goal-critic".to_string(),
        description: "Independent critic that judges whether the standing goal is met and returns a binary OURS/BAR/PLATEAU verdict.".to_string(),
        system_prompt: CRITIC_SYSTEM_PROMPT.to_string(),
        tool_scope: None,
        max_steps: StepBudget::new(CRITIC_MAX_STEPS),
    }
}

/// The task handed to the critic: the goal and where to look. Deliberately
/// spare — the critic forms its own view from the files, not from a briefing.
pub fn critic_task(goal: &str, project_root: &Path) -> String {
    format!(
        "The standing goal is:\n\n{goal}\n\nThe project is at {root}. Inspect the real \
         artifact there and judge whether the goal is met. If `.gauntlet/bar/` exists, that \
         is the quality bar to beat. Return your verdict now.",
        root = project_root.display()
    )
}

/// Does `upper` begin with `token` as a whole word (end of string or a
/// non-alphabetic char after it)? Guards against `BARELY` reading as `BAR`.
fn starts_with_token(upper: &str, token: &str) -> bool {
    upper
        .strip_prefix(token)
        .is_some_and(|rest| rest.chars().next().is_none_or(|c| !c.is_ascii_alphabetic()))
}

/// Parse a critic's reply into a verdict. `None` when it named none — the
/// caller decides what an unclear critic means (the loop treats it as "judge
/// again", never as a pass).
pub fn parse_verdict(output: &str) -> Option<GoalVerdict> {
    let lines: Vec<&str> = output.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        // Ignore leading markdown / bullet punctuation the model may prepend.
        let stripped = line
            .trim()
            .trim_start_matches(|c: char| !c.is_ascii_alphabetic());
        let upper = stripped.to_ascii_uppercase();
        if starts_with_token(&upper, "OURS") {
            return Some(GoalVerdict::Ours);
        }
        if starts_with_token(&upper, "PLATEAU") {
            return Some(GoalVerdict::Plateau);
        }
        if starts_with_token(&upper, "BAR") {
            return Some(GoalVerdict::Bar(gap_after(
                &lines,
                idx,
                stripped,
                "BAR".len(),
                "the critic returned BAR without naming a gap; identify the biggest gap and \
                 close it",
            )));
        }
    }
    None
}

/// The gap text after a verdict token: whatever follows it on its own line,
/// then the following lines, joined and trimmed to one carryable line.
/// `token_len` is the length of the token just matched, which is ASCII.
fn gap_after(
    lines: &[&str],
    idx: usize,
    stripped_line: &str,
    token_len: usize,
    fallback: &str,
) -> String {
    let mut gap = String::new();
    // Remainder of the verdict line itself, after the token and any separator.
    let same_line = stripped_line[token_len..]
        .trim_start_matches([':', '-', '.', ' ', '\t'])
        .trim();
    if !same_line.is_empty() {
        gap.push_str(same_line);
    }
    for line in &lines[idx + 1..] {
        let piece = line.trim();
        if piece.is_empty() {
            if gap.is_empty() {
                continue;
            }
            break; // first blank line after some gap text ends the paragraph
        }
        if !gap.is_empty() {
            gap.push(' ');
        }
        gap.push_str(piece);
    }
    if gap.is_empty() {
        gap.push_str(fallback);
    }
    brief(&gap, MAX_GAP_CHARS)
}

/// Trim to `max` chars on a char boundary, so a multi-byte gap cannot panic.
fn brief(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// What the loop should do next, given a verdict and how many `PLATEAU`s have
/// come in a row. Pure so the loop's control flow can be tested without a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CriticAction {
    /// The goal is met: let the cycle land.
    Accept,
    /// Not yet: send this prompt back to the builder for another round.
    Rework(String),
    /// Plateaued twice: stop the loop and report `reason`.
    Stop(String),
}

/// Plateaus in a row that mean "stop", not "try once more".
pub const PLATEAU_LIMIT: u32 = 2;

/// Decide the loop's next move from a verdict. `plateau_streak` is the count
/// *including* this verdict when it is `Plateau` (the caller bumps it first).
pub fn plan_after_verdict(verdict: &GoalVerdict, plateau_streak: u32) -> CriticAction {
    match verdict {
        GoalVerdict::Ours => CriticAction::Accept,
        GoalVerdict::Bar(gap) => CriticAction::Rework(format!(
            "An independent critic judged the goal NOT yet met and named one gap to close:\n\n\
             {gap}\n\n\
             Close exactly this gap, then stop and let the critic judge again. Do not declare \
             the goal done yourself — the critic decides."
        )),
        GoalVerdict::Plateau if plateau_streak >= PLATEAU_LIMIT => CriticAction::Stop(
            "the critic returned PLATEAU twice: it can name no gap another round would close"
                .to_string(),
        ),
        GoalVerdict::Plateau => CriticAction::Rework(
            "An independent critic returned PLATEAU: it could not name a gap another round \
             would close. Take a genuinely different approach — a different angle, tool, or \
             decomposition — then let the critic judge again. If there is truly nothing more \
             to try, say so plainly."
                .to_string(),
        ),
    }
}

// ---------------------------------------------------------------------------
// The completion review
// ---------------------------------------------------------------------------

/// Steps the review may take: enough to list a directory, read the deliverables
/// and run the acceptance command a couple of times, not enough to start
/// building. A review that needs forty steps is doing the work again.
const REVIEW_MAX_STEPS: u32 = 24;

/// Rounds of review feedback one claim of done may cost.
///
/// One. The review catches the run that wrote nothing and said it was finished,
/// and one round of feedback is what fixes that. Past one it stops being a
/// check and becomes an argument: the same two models trading "it is done" and
/// "no it is not" at a model call each, on a run with nobody watching. Whatever
/// is still wrong after the rework is wrong in a way another round of the same
/// prompt was not going to find.
pub const REVIEW_MAX_ROUNDS: u32 = 1;

/// Seconds of the run's own deadline that must be left for a review to start.
///
/// The review runs a model call and then some commands, and a review that is
/// killed halfway has cost the run its last minute and told it nothing. Under
/// this, the claim is accepted as it stands.
pub const REVIEW_MIN_SECS: u64 = 60;

/// The reviewer's system prompt.
///
/// The four numbered steps are the four ways a confident "done" has been wrong
/// in practice: the deliverable was never written, the check was a variation of
/// the one the user asked for, the check ran against the agent's own example,
/// and the thing that made the check pass was torn down afterwards.
const REVIEW_SYSTEM_PROMPT: &str = "\
You are reviewing a finished autonomous run. You never saw the agent work and \
you get none of its reasoning, its summary, or its claims: only the request it \
was given and the machine it left behind. Answer one question: is the request \
satisfied right now, on this machine?\n\
\n\
Work through these in order.\n\
1. Restate the request in your own words, and list every deliverable it names.\n\
2. Check every one of them on the machine: a file, a directory, a path, an \
endpoint, a service. Missing, zero bytes, or still a placeholder is a failure \
on its own, and say which one it was.\n\
3. If the request states how the work will be checked (a command to run, a \
scenario to reproduce, an input with an expected output), run it literally, as \
written, from where the request says to run it. Do not substitute a variation \
you believe is equivalent.\n\
4. Ask whether the result still holds when the inputs change. If the work could \
have been fitted to one example, construct a second case the request also \
covers and run that too. If anything was reset, restarted, moved, or cleaned up \
after the agent's own check, redo the check now.\n\
\n\
Judge, do not build. Do not write, edit, or delete anything, and do not finish \
the work yourself. Your job is to report what is there, not to repair it.\n\
\n\
The FIRST line of your reply is your verdict, exactly one of:\n\
  PASS   the request is satisfied, and you ran something that shows it\n\
  FAIL   it is not; on the next line, say what is wrong and quote the command \
output that shows it, in one or two sentences\n\
\n\
No praise, no summary of the agent's work, no advice beyond what is wrong. \
Absence of evidence is FAIL: if you could not check a deliverable at all, that \
is FAIL, and say what stopped you.";

/// The review subagent's definition.
///
/// `tool_scope` is left open and the narrowing happens at spawn time
/// ([`crate::agent::subagent::RunScope::Inspect`]), so the reviewer keeps every
/// inspection tool this install has while having none that writes a file.
pub fn review_config() -> SubagentConfig {
    SubagentConfig {
        name: "completion-review".to_string(),
        description: "Independent review of a finished autonomous run: checks the request was \
                      actually satisfied and returns PASS or FAIL."
            .to_string(),
        tool_scope: None,
        system_prompt: REVIEW_SYSTEM_PROMPT.to_string(),
        max_steps: StepBudget::new(REVIEW_MAX_STEPS),
    }
}

/// The task handed to the reviewer: the request, verbatim, and where it ran.
///
/// Verbatim matters. A summary of the request is the agent's own reading of it,
/// and the agent's reading is the thing under review.
pub fn review_task(request: &str, project_root: &Path) -> String {
    format!(
        "This is the request the agent was given, verbatim between the markers. The agent has \
         reported it complete.\n\n<request>\n{request}\n</request>\n\nIt ran in {root}. Check \
         whether the request is satisfied there and return your verdict now.",
        root = project_root.display()
    )
}

/// A reviewer's verdict on one claim of done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewVerdict {
    /// The request is satisfied and the reviewer saw evidence of it.
    Pass,
    /// It is not, and this is what is wrong.
    Fail(String),
}

impl ReviewVerdict {
    /// A one-line description for a notice or a mission note.
    pub fn summary(&self) -> String {
        match self {
            Self::Pass => "PASS — the review found the request satisfied".to_string(),
            Self::Fail(why) => format!("FAIL — {why}"),
        }
    }
}

/// Parse a reviewer's reply. `None` when it named no verdict. The caller
/// decides what an unclear reviewer means, and no caller reads it as a pass.
pub fn parse_review(output: &str) -> Option<ReviewVerdict> {
    let lines: Vec<&str> = output.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        let stripped = line
            .trim()
            .trim_start_matches(|c: char| !c.is_ascii_alphabetic());
        let upper = stripped.to_ascii_uppercase();
        if starts_with_token(&upper, "PASS") {
            return Some(ReviewVerdict::Pass);
        }
        if starts_with_token(&upper, "FAIL") {
            return Some(ReviewVerdict::Fail(gap_after(
                &lines,
                idx,
                stripped,
                "FAIL".len(),
                "the review returned FAIL without saying what is wrong; check the request's \
                 deliverables yourself before reporting done again",
            )));
        }
    }
    None
}

/// What the run loop should do after a review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewAction {
    /// Let the run finish.
    Accept,
    /// Hand this back to the agent for one more turn.
    Rework(String),
}

/// Decide the loop's next move from a review verdict. `rounds_used` counts the
/// reviews already spent on this claim, including this one.
pub fn plan_after_review(verdict: &ReviewVerdict, rounds_used: u32) -> ReviewAction {
    match verdict {
        ReviewVerdict::Pass => ReviewAction::Accept,
        // Out of rounds: the run ends with the failure reported rather than
        // argued. The caller is expected to say so on its way out.
        ReviewVerdict::Fail(_) if rounds_used > REVIEW_MAX_ROUNDS => ReviewAction::Accept,
        ReviewVerdict::Fail(why) => ReviewAction::Rework(format!(
            "You reported this done. An independent review checked the machine and it is not:\n\n\
             {why}\n\n\
             Fix exactly that, then report done again. Run the request's own check yourself \
             before you do, and leave whatever made it pass in place. The next thing to \
             look at it will not be you."
        )),
    }
}

/// Why a completion review did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewSkip {
    /// Turned off for this surface (see [`review_enabled`]).
    Disabled,
    /// The turn wrote no file and ran no command, so there is no work to check
    /// and a review would be a model call spent reading an unchanged tree.
    NothingHappened,
    /// This claim has already had its review and its round of feedback.
    RoundsSpent,
    /// Less than [`REVIEW_MIN_SECS`] of the run's deadline is left.
    NoTimeLeft,
}

impl ReviewSkip {
    /// A one-line reason, for a log or a `--output-format text` notice.
    pub fn summary(self) -> &'static str {
        match self {
            Self::Disabled => "completion review is off",
            Self::NothingHappened => "nothing was written or run this turn",
            Self::RoundsSpent => "the completion review has already had its round",
            Self::NoTimeLeft => "the run's time limit is too close",
        }
    }
}

/// Whether to review this claim of done, or why not.
///
/// Pure, and the whole decision: every caller asks this one function so the
/// skip rules cannot drift between surfaces.
pub fn plan_review(
    enabled: bool,
    effects: u64,
    rounds_used: u32,
    secs_left: Option<u64>,
) -> Result<(), ReviewSkip> {
    if !enabled {
        return Err(ReviewSkip::Disabled);
    }
    if rounds_used >= REVIEW_MAX_ROUNDS {
        return Err(ReviewSkip::RoundsSpent);
    }
    if effects == 0 {
        return Err(ReviewSkip::NothingHappened);
    }
    if secs_left.is_some_and(|left| left < REVIEW_MIN_SECS) {
        return Err(ReviewSkip::NoTimeLeft);
    }
    Ok(())
}

/// Is the completion review on for this surface?
///
/// `configured` is the `completion_review` config key, unset by default so the
/// answer can depend on who is watching. Nobody is watching an autonomous run,
/// so it reviews; in the TUI the user is reading every line as it arrives and
/// an extra model call per turn is their time and their money, so it does not.
/// Setting the key either way wins on both.
pub fn review_enabled(configured: Option<bool>, interactive: bool) -> bool {
    configured.unwrap_or(!interactive)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_bare_verdict() {
        assert_eq!(parse_verdict("OURS"), Some(GoalVerdict::Ours));
        assert_eq!(parse_verdict("PLATEAU"), Some(GoalVerdict::Plateau));
    }

    #[test]
    fn parses_ours_with_trailing_prose() {
        assert_eq!(
            parse_verdict("OURS\nThe tests pass and it matches the bar."),
            Some(GoalVerdict::Ours)
        );
    }

    #[test]
    fn bar_captures_the_gap_on_the_next_line() {
        assert_eq!(
            parse_verdict("BAR\nThe retry path has no backoff, so a flapping host is hammered."),
            Some(GoalVerdict::Bar(
                "The retry path has no backoff, so a flapping host is hammered.".to_string()
            ))
        );
    }

    #[test]
    fn bar_captures_the_gap_on_the_same_line() {
        assert_eq!(
            parse_verdict("BAR: missing the error path for a closed socket"),
            Some(GoalVerdict::Bar(
                "missing the error path for a closed socket".to_string()
            ))
        );
    }

    #[test]
    fn ignores_leading_markdown() {
        assert_eq!(parse_verdict("**OURS**"), Some(GoalVerdict::Ours));
        assert_eq!(parse_verdict("- PLATEAU"), Some(GoalVerdict::Plateau));
    }

    #[test]
    fn does_not_read_barely_as_bar() {
        // No verdict token present at all.
        assert_eq!(parse_verdict("Barely acceptable, but fine."), None);
    }

    #[test]
    fn unclear_reply_has_no_verdict() {
        assert_eq!(parse_verdict("I think it looks pretty good overall."), None);
    }

    #[test]
    fn bar_without_a_gap_still_names_one() {
        let GoalVerdict::Bar(gap) = parse_verdict("BAR").unwrap() else {
            panic!("expected BAR");
        };
        assert!(!gap.is_empty());
    }

    #[test]
    fn a_long_gap_is_trimmed() {
        let huge = format!("BAR\n{}", "x".repeat(MAX_GAP_CHARS + 200));
        let GoalVerdict::Bar(gap) = parse_verdict(&huge).unwrap() else {
            panic!("expected BAR");
        };
        assert!(gap.chars().count() <= MAX_GAP_CHARS + 1); // +1 for the ellipsis
    }

    #[test]
    fn ours_accepts() {
        assert_eq!(
            plan_after_verdict(&GoalVerdict::Ours, 0),
            CriticAction::Accept
        );
    }

    #[test]
    fn bar_reworks_with_the_gap_in_the_prompt() {
        let CriticAction::Rework(prompt) =
            plan_after_verdict(&GoalVerdict::Bar("no backoff".into()), 0)
        else {
            panic!("expected rework");
        };
        assert!(prompt.contains("no backoff"));
        assert!(prompt.contains("critic decides"));
    }

    #[test]
    fn first_plateau_reworks_second_stops() {
        assert!(matches!(
            plan_after_verdict(&GoalVerdict::Plateau, 1),
            CriticAction::Rework(_)
        ));
        assert!(matches!(
            plan_after_verdict(&GoalVerdict::Plateau, PLATEAU_LIMIT),
            CriticAction::Stop(_)
        ));
    }

    // --- the completion review ---------------------------------------------

    #[test]
    fn parses_a_review_verdict() {
        assert_eq!(parse_review("PASS"), Some(ReviewVerdict::Pass));
        assert_eq!(
            parse_review("**PASS** — server.py answers on :8080"),
            Some(ReviewVerdict::Pass)
        );
        assert_eq!(
            parse_review("FAIL\n/app/out.txt does not exist."),
            Some(ReviewVerdict::Fail(
                "/app/out.txt does not exist.".to_string()
            ))
        );
        assert_eq!(
            parse_review("FAIL: `curl -sf localhost:8080` returned 404"),
            Some(ReviewVerdict::Fail(
                "`curl -sf localhost:8080` returned 404".to_string()
            ))
        );
    }

    #[test]
    fn a_review_that_names_no_verdict_is_unclear() {
        // The caller turns this into a FAIL; a mumbling reviewer must never be
        // the reason a run reports success.
        assert_eq!(parse_review("Looks good to me, mostly."), None);
        assert_eq!(parse_review("FAILURE-mode analysis follows"), None);
    }

    #[test]
    fn a_review_that_fails_without_saying_why_still_says_something() {
        let Some(ReviewVerdict::Fail(why)) = parse_review("FAIL") else {
            panic!("expected FAIL");
        };
        assert!(!why.is_empty());
    }

    #[test]
    fn a_passing_review_lets_the_run_finish() {
        assert_eq!(
            plan_after_review(&ReviewVerdict::Pass, 1),
            ReviewAction::Accept
        );
    }

    #[test]
    fn a_failing_review_buys_exactly_one_rework() {
        let verdict = ReviewVerdict::Fail("/app/out.txt is missing".to_string());
        let ReviewAction::Rework(prompt) = plan_after_review(&verdict, REVIEW_MAX_ROUNDS) else {
            panic!("the first failed review sends it back");
        };
        assert!(prompt.contains("/app/out.txt is missing"));
        assert!(prompt.contains("report done again"));
        // The round after that is an argument, not a check.
        assert_eq!(
            plan_after_review(&verdict, REVIEW_MAX_ROUNDS + 1),
            ReviewAction::Accept
        );
    }

    #[test]
    fn a_turn_that_wrote_nothing_and_ran_nothing_is_not_reviewed() {
        assert_eq!(
            plan_review(true, 0, 0, None),
            Err(ReviewSkip::NothingHappened)
        );
        assert_eq!(plan_review(true, 1, 0, None), Ok(()));
    }

    #[test]
    fn a_claim_is_reviewed_once() {
        assert_eq!(plan_review(true, 5, 0, None), Ok(()));
        assert_eq!(
            plan_review(true, 5, REVIEW_MAX_ROUNDS, None),
            Err(ReviewSkip::RoundsSpent)
        );
    }

    #[test]
    fn the_review_does_not_start_on_the_last_seconds_of_a_run() {
        assert_eq!(
            plan_review(true, 5, 0, Some(REVIEW_MIN_SECS - 1)),
            Err(ReviewSkip::NoTimeLeft)
        );
        assert_eq!(plan_review(true, 5, 0, Some(REVIEW_MIN_SECS)), Ok(()));
        // A run with no `--max-hours` has no last seconds.
        assert_eq!(plan_review(true, 5, 0, None), Ok(()));
    }

    #[test]
    fn off_is_off_whatever_else_is_true() {
        assert_eq!(plan_review(false, 99, 0, None), Err(ReviewSkip::Disabled));
    }

    #[test]
    fn the_review_is_on_where_nobody_is_watching_and_off_where_someone_is() {
        assert!(review_enabled(None, false), "headless reviews by default");
        assert!(
            !review_enabled(None, true),
            "the TUI does not: the user is reading the claim themselves"
        );
        // An explicit key wins on both surfaces.
        assert!(review_enabled(Some(true), true));
        assert!(!review_enabled(Some(false), false));
    }
}
