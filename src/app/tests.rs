use super::*;

use crate::agent::InterviewQuestion;
use crate::commands::{FusionAction, ServerAction};
use crate::images::ImageRef;

use super::command::{git_diff_text, is_wizard_state_path};

fn app() -> App {
    App::new(Config::default())
}

fn press(app: &mut App, code: KeyCode) -> Option<AppAction> {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
        .expect("key handled")
}

fn press_ctrl(app: &mut App, c: char) -> Option<AppAction> {
    app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
        .expect("key handled")
}

/// An app with `n` subagent runs on the rail, all still running.
fn app_with_panes(n: u64) -> App {
    let mut app = app();
    for i in 0..n {
        app.handle_agent_event(AgentEvent::SubagentRunStarted {
            run: i,
            bg: Some(i as u32),
            name: format!("agent{i}"),
            task: format!("task {i}"),
        });
    }
    app
}

fn press_mod(app: &mut App, code: KeyCode, mods: KeyModifiers) -> Option<AppAction> {
    app.handle_key(KeyEvent::new(code, mods))
        .expect("key handled")
}

fn type_str(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

/// Untracked (new) files are invisible to plain `git diff`, so `/diff`
/// must surface them itself — otherwise a tree whose only change is a new
/// file reads as "(working tree clean)".
#[tokio::test]
async fn diff_text_includes_untracked_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git");
    };
    run(&["init"]);
    run(&["config", "user.email", "t@example.com"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(root.join("brand_new.txt"), "fresh content\n").expect("write");

    let text = git_diff_text(root).await.expect("diff text");
    assert!(
        text.contains("brand_new.txt") && text.contains("fresh content"),
        "untracked file missing from /diff output:\n{text}"
    );
    assert!(text.contains("# --- untracked ---"));
}

/// Wizard's own `.wizard/` session state (checkpoints, snapshots) is an
/// implementation detail — it must never show up in `/diff`, or the
/// sidebar fills with internal noise and looks broken.
#[tokio::test]
async fn diff_text_omits_wizard_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git");
    };
    run(&["init"]);
    run(&["config", "user.email", "t@example.com"]);
    run(&["config", "user.name", "t"]);
    std::fs::create_dir_all(root.join(".wizard/checkpoints/1")).expect("mkdir");
    std::fs::write(root.join(".wizard/checkpoints/1/0.snap"), "internal\n").expect("write");
    std::fs::write(root.join("real_change.txt"), "user content\n").expect("write");

    let text = git_diff_text(root).await.expect("diff text");
    assert!(
        text.contains("real_change.txt"),
        "real untracked change missing:\n{text}"
    );
    assert!(
        !text.contains(".wizard/checkpoints"),
        "wizard internal state leaked into /diff:\n{text}"
    );
}

#[test]
fn is_wizard_state_path_matches_state_dir_only() {
    assert!(is_wizard_state_path(".wizard/checkpoints/1/0.snap"));
    assert!(is_wizard_state_path("sub/.wizard/x"));
    assert!(is_wizard_state_path(".wizard"));
    assert!(!is_wizard_state_path("src/wizard.rs"));
    assert!(!is_wizard_state_path(".wizardrc"));
}

/// The diff sidebar paginates with PgUp/PgDn (offset from the top) and Esc
/// closes it — without this a diff taller than the pane is unreadable.
#[test]
fn diff_sidebar_pages_and_closes() {
    let mut app = app();
    app.show_diff = true;
    assert_eq!(app.diff_scroll, 0);

    press(&mut app, KeyCode::PageDown);
    assert_eq!(app.diff_scroll, 10, "PgDn scrolls the diff down");
    press(&mut app, KeyCode::PageUp);
    assert_eq!(app.diff_scroll, 0, "PgUp scrolls back up");
    // PgUp at the top stays clamped (no underflow).
    press(&mut app, KeyCode::PageUp);
    assert_eq!(app.diff_scroll, 0);

    // While the diff owns paging, the transcript scroll is untouched.
    assert_eq!(app.scroll, 0);

    app.diff_scroll = 30;
    press(&mut app, KeyCode::Esc);
    assert!(!app.show_diff, "Esc closes the diff sidebar");
    assert_eq!(app.diff_scroll, 0, "closing resets the diff scroll");
}

#[test]
fn welcome_stays_up_for_empty_and_notice_only_transcripts() {
    let mut app = app();
    // Fresh launch: nothing typed, welcome screen.
    assert!(!app.has_conversation());

    // Early system notices (provider health, partial MCP failure) land
    // before the first message; they alone must not dismiss the welcome.
    app.notice("error: 1 of 2 MCP servers failed to connect (see logs)");
    app.notice("just a status line");
    assert!(
        !app.has_conversation(),
        "notices alone should not count as conversation"
    );
}

#[test]
fn slash_command_dismisses_the_welcome_screen() {
    let mut app = app();
    assert!(app.welcome_visible());

    // Startup notices land before anything is submitted; they alone must
    // leave the welcome screen up.
    app.notice("error: 1 of 2 MCP servers failed to connect (see logs)");
    assert!(app.welcome_visible());

    // A slash command dispatches without adding transcript entries, but
    // it still begins the session.
    type_str(&mut app, "/effort high");
    press(&mut app, KeyCode::Enter);
    assert!(
        !app.welcome_visible(),
        "a slash command dismisses the welcome screen"
    );
}

#[test]
fn welcome_dismisses_once_real_entries_appear() {
    for entry in [
        TranscriptEntry::User("hi".to_string()),
        TranscriptEntry::Assistant("hello".to_string()),
        TranscriptEntry::ToolCard {
            name: "read".to_string(),
            args: serde_json::json!({}),
            output: None,
            is_error: false,
            collapsed: false,
        },
    ] {
        let mut app = app();
        app.transcript.push(entry);
        assert!(
            app.has_conversation(),
            "a User/Assistant/ToolCard entry begins the conversation"
        );
    }
}

#[test]
fn spinner_verb_starts_from_the_default_list() {
    let app = app();
    assert!(crate::config::UiConfig::DEFAULT_SPINNER_VERBS.contains(&app.spinner_verb.as_str()));
}

#[test]
fn spinner_verb_is_deterministic_and_stable_within_a_busy_period() {
    let config = Config {
        ui: crate::config::UiConfig {
            spinner_verbs: vec![
                "Pondering".to_string(),
                "Musing".to_string(),
                "Noodling".to_string(),
            ],
            vim: false,
        },
        ..Config::default()
    };
    let mut a = App::new(config.clone());
    let mut b = App::new(config);
    a.tick = 17;
    b.tick = 17;
    a.roll_spinner_verb();
    b.roll_spinner_verb();
    // Same tick and roll count -> same verb.
    assert_eq!(a.spinner_verb, b.spinner_verb);
    // Ticks advancing mid-turn must not change the verb until a re-roll.
    let during = a.spinner_verb.clone();
    a.tick += 5;
    assert_eq!(a.spinner_verb, during);
}

#[test]
fn spinner_verb_rerolls_across_busy_periods() {
    let mut app = app();
    let mut seen = std::collections::HashSet::new();
    for turn in 0..40u64 {
        app.tick = turn * 13;
        app.roll_spinner_verb();
        seen.insert(app.spinner_verb.clone());
    }
    assert!(seen.len() > 1, "verb never varied across busy periods");
}

#[test]
fn slash_filters_suggestions_by_prefix() {
    let mut app = app();
    type_str(&mut app, "/mo");
    let names: Vec<&str> = app.suggestions.iter().map(|s| s.name.as_str()).collect();
    // Prefix matches first, then substring matches ("me*mo*ry").
    assert_eq!(names, ["model", "mode", "memory"]);
    assert_eq!(app.input_mode, InputMode::Command);
}

#[test]
fn suggestions_hide_once_args_are_typed() {
    let mut app = app();
    type_str(&mut app, "/evolve add");
    assert!(app.suggestions.is_empty());
}

#[test]
fn arrow_keys_cycle_suggestions_with_wraparound() {
    let mut app = app();
    type_str(&mut app, "/mo");
    assert_eq!(app.suggestion_index, 0);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.suggestion_index, 1);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.suggestion_index, 2);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.suggestion_index, 0);
    press(&mut app, KeyCode::Up);
    assert_eq!(app.suggestion_index, 2);
}

#[test]
fn tab_completes_the_selected_suggestion() {
    let mut app = app();
    // "/re" would be ambiguous between /rewind and /reload.
    type_str(&mut app, "/rel");
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.input, "/reload");
    assert_eq!(app.cursor, "/reload".chars().count());
}

#[test]
fn tab_completion_appends_space_for_commands_taking_args() {
    let mut app = app();
    type_str(&mut app, "/ev");
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.input, "/evolve ");
}

#[test]
fn enter_completes_and_runs_argless_commands() {
    let mut app = app();
    type_str(&mut app, "/d");
    let action = press(&mut app, KeyCode::Enter);
    assert!(matches!(
        action,
        Some(AppAction::Command(SlashCommand::Diff))
    ));
    assert!(app.input.is_empty());
}

#[test]
fn enter_on_partial_arg_command_completes_and_waits() {
    let mut app = app();
    type_str(&mut app, "/ev");
    let action = press(&mut app, KeyCode::Enter);
    assert!(action.is_none());
    assert_eq!(app.input, "/evolve ");
}

#[test]
fn exactly_typed_command_wins_over_longer_completion() {
    // "model" prefix-matches the typed "mode"; Enter must still run
    // /mode itself, not complete to /model.
    let mut app = app();
    type_str(&mut app, "/mode");
    assert_eq!(app.suggestions[0].name, "mode");
    let action = press(&mut app, KeyCode::Enter);
    assert!(matches!(
        action,
        Some(AppAction::Command(SlashCommand::Mode(None)))
    ));
}

fn custom(name: &str, template: &str, description: Option<&str>) -> CustomCommand {
    CustomCommand {
        name: name.to_string(),
        description: description.map(str::to_string),
        template: template.to_string(),
        path: PathBuf::new(),
    }
}

#[test]
fn custom_commands_appear_in_suggestions_after_builtins() {
    let mut app = app();
    app.custom_commands = vec![custom(
        "models-report",
        "Report on $ARGUMENTS",
        Some("report"),
    )];
    type_str(&mut app, "/mo");
    let names: Vec<&str> = app.suggestions.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["model", "mode", "models-report", "memory"]);
    let spec = &app.suggestions[2];
    assert_eq!(spec.description, "report");
    assert!(spec.takes_args);
}

#[test]
fn typed_custom_command_submits_the_expanded_prompt() {
    let mut app = app();
    app.custom_commands = vec![custom("review", "Review $1 with care.", None)];
    type_str(&mut app, "/review src/app.rs");
    let action = press(&mut app, KeyCode::Enter);
    let Some(AppAction::Submit(prepared)) = action else {
        panic!("expected a submit, got {action:?}");
    };
    assert_eq!(prepared.text, "Review src/app.rs with care.");
    // The transcript shows what the user actually typed.
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptEntry::User(text)) if text == "/review src/app.rs"
    ));
}

#[test]
fn unknown_slash_command_passes_through_as_a_prompt() {
    let mut app = app();
    type_str(&mut app, "/frobnicate the build");
    let action = press(&mut app, KeyCode::Enter);
    assert!(matches!(
        action,
        Some(AppAction::Submit(prepared)) if prepared.text == "/frobnicate the build"
    ));
}

#[test]
fn builtin_command_with_bad_args_keeps_its_usage_notice() {
    let mut app = app();
    type_str(&mut app, "/mode warlock");
    let action = press(&mut app, KeyCode::Enter);
    assert!(action.is_none());
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptEntry::Notice(text)) if text.contains("unknown mode")
    ));
}

#[test]
fn submit_expands_at_file_references() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("ctx.txt"), "the context\n").unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();
    type_str(&mut app, "use @ctx.txt here");
    let action = press(&mut app, KeyCode::Enter);
    let Some(AppAction::Submit(prepared)) = action else {
        panic!("expected a submit, got {action:?}");
    };
    assert!(
        prepared.text.contains("the context"),
        "got: {}",
        prepared.text
    );
    // The transcript keeps the compact form.
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptEntry::User(text)) if text == "use @ctx.txt here"
    ));
}

#[test]
fn submit_attaches_image_at_refs() {
    let tmp = tempfile::tempdir().unwrap();
    // Minimal 1x1 PNG.
    let png = [
        0x89u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8,
        0xcf, 0xc0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x05, 0xfe, 0xd4, 0xef, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    std::fs::write(tmp.path().join("shot.png"), png).unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();
    type_str(&mut app, "look at @shot.png");
    let action = press(&mut app, KeyCode::Enter);
    let Some(AppAction::Submit(prepared)) = action else {
        panic!("expected a submit, got {action:?}");
    };
    assert!(
        prepared.text.contains("[image: shot.png]"),
        "got: {}",
        prepared.text
    );
    assert_eq!(prepared.images.len(), 1);
    assert!(prepared.images[0].ends_with("shot.png"));
}

/// A 1x1 PNG for tests that only need a real image file on disk.
const MINI_PNG: [u8; 70] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00,
    0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x05, 0xfe, 0xd4, 0xef, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[test]
fn pasting_image_paths_shows_numbered_indicators() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.png"), MINI_PNG).unwrap();
    std::fs::write(tmp.path().join("b.png"), MINI_PNG).unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();

    app.handle_paste(&tmp.path().join("a.png").display().to_string());
    app.handle_paste(&tmp.path().join("b.png").display().to_string());

    assert!(app.input.contains("[Image #1]"), "input: {}", app.input);
    assert!(app.input.contains("[Image #2]"), "input: {}", app.input);
    assert_eq!(app.pending_images.len(), 2);
}

#[test]
fn pasting_the_same_image_twice_stages_it_once() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.png"), MINI_PNG).unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();

    let token = tmp.path().join("a.png").display().to_string();
    app.handle_paste(&token);
    app.handle_paste(&token);

    assert_eq!(app.pending_images.len(), 1);
    assert!(!app.input.contains("[Image #2]"), "input: {}", app.input);
}

#[test]
fn clearing_the_composer_drops_staged_images() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.png"), MINI_PNG).unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();

    app.handle_paste(&tmp.path().join("a.png").display().to_string());
    assert_eq!(app.pending_images.len(), 1);

    app.clear_input();
    assert!(app.pending_images.is_empty());
    assert!(app.input.is_empty());
}

#[test]
fn backspace_deletes_a_pasted_image_token_in_one_press() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.png"), MINI_PNG).unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();

    app.handle_paste(&tmp.path().join("a.png").display().to_string());
    assert_eq!(app.input, "[Image #1]");
    assert_eq!(app.pending_images.len(), 1);
    assert_eq!(app.cursor, app.input.chars().count());

    // One Backspace at the end of the token removes the whole attachment.
    press(&mut app, KeyCode::Backspace);
    assert!(app.input.is_empty(), "input: {}", app.input);
    assert!(app.pending_images.is_empty());
    assert_eq!(app.cursor, 0);
}

#[test]
fn delete_removes_image_token_under_the_cursor() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.png"), MINI_PNG).unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();

    type_str(&mut app, "see ");
    app.handle_paste(&tmp.path().join("a.png").display().to_string());
    assert!(app.input.ends_with("[Image #1]"), "input: {}", app.input);

    // Park the cursor on the '[' of the token, then Delete.
    app.cursor = "see ".chars().count();
    press(&mut app, KeyCode::Delete);
    assert_eq!(app.input, "see ");
    assert!(app.pending_images.is_empty());
    assert_eq!(app.cursor, "see ".chars().count());
}

#[test]
fn deleting_one_image_renumbers_the_rest() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.png"), MINI_PNG).unwrap();
    std::fs::write(tmp.path().join("b.png"), MINI_PNG).unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();

    app.handle_paste(&tmp.path().join("a.png").display().to_string());
    app.handle_paste(&tmp.path().join("b.png").display().to_string());
    assert_eq!(app.input, "[Image #1] [Image #2]");
    let first = app.pending_images[0].clone();
    let second = app.pending_images[1].clone();

    // Cursor after token #1; Backspace drops #1 and renumbers #2 → #1.
    app.cursor = "[Image #1]".chars().count();
    press(&mut app, KeyCode::Backspace);
    assert_eq!(app.input, " [Image #1]");
    assert_eq!(app.pending_images, vec![second]);
    assert!(!app.pending_images.contains(&first));
}

#[test]
fn sniff_identifies_supported_image_formats() {
    assert_eq!(sniff_image_ext(&MINI_PNG), Some("png"));
    assert_eq!(sniff_image_ext(&[0xff, 0xd8, 0xff, 0xe0]), Some("jpg"));
    assert_eq!(sniff_image_ext(b"GIF89a\x01\x00"), Some("gif"));
    let mut webp = b"RIFF".to_vec();
    webp.extend_from_slice(&[0x1a, 0x00, 0x00, 0x00]);
    webp.extend_from_slice(b"WEBPVP8 ");
    assert_eq!(sniff_image_ext(&webp), Some("webp"));
    assert_eq!(sniff_image_ext(b"not an image at all"), None);
    assert_eq!(sniff_image_ext(&[]), None);
}

#[test]
fn tab_completes_at_paths_from_the_directory_listing() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("readme.md"), "x").unwrap();
    std::fs::create_dir(tmp.path().join("reach")).unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();

    // Common prefix of readme.md / reach.
    type_str(&mut app, "see @re");
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.input, "see @rea");

    // Unique file completes fully.
    type_str(&mut app, "d");
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.input, "see @readme.md");
}

#[test]
fn tab_completes_unique_directory_with_a_trailing_slash() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("sources")).unwrap();
    std::fs::write(tmp.path().join("sources").join("inner.rs"), "x").unwrap();
    let mut app = app();
    app.project_root = tmp.path().to_path_buf();
    type_str(&mut app, "@so");
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.input, "@sources/");
    type_str(&mut app, "in");
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.input, "@sources/inner.rs");
}

#[test]
fn genie_and_sovereign_parse_as_mode_switches() {
    assert_eq!(
        SlashCommand::parse("/genie"),
        Some(Ok(SlashCommand::Mode(Some(Mode::Genie))))
    );
    assert_eq!(
        SlashCommand::parse("/sovereign"),
        Some(Ok(SlashCommand::Mode(Some(Mode::Sovereign))))
    );
}

#[test]
fn effort_parses_levels_default_and_bare() {
    assert_eq!(
        SlashCommand::parse("/effort"),
        Some(Ok(SlashCommand::Effort(None))),
        "bare /effort opens the picker"
    );
    assert_eq!(
        SlashCommand::parse("/effort low"),
        Some(Ok(SlashCommand::Effort(Some(Some(ReasoningEffort::Low)))))
    );
    assert_eq!(
        SlashCommand::parse("/effort HIGH"),
        Some(Ok(SlashCommand::Effort(Some(Some(ReasoningEffort::High))))),
        "level is case-insensitive"
    );
    assert_eq!(
        SlashCommand::parse("/effort default"),
        Some(Ok(SlashCommand::Effort(Some(None)))),
        "default clears back to the provider default"
    );
    assert!(
        matches!(SlashCommand::parse("/effort turbo"), Some(Err(_))),
        "unknown level is an error"
    );
}

#[test]
fn goal_parses_show_and_set() {
    assert_eq!(
        SlashCommand::parse("/goal"),
        Some(Ok(SlashCommand::Goal(None)))
    );
    assert_eq!(
        SlashCommand::parse("/goal ship the thing"),
        Some(Ok(SlashCommand::Goal(Some("ship the thing".into()))))
    );
}

#[test]
fn server_subcommands_parse() {
    assert_eq!(
        SlashCommand::parse("/server"),
        Some(Ok(SlashCommand::Server(ServerAction::Status)))
    );
    assert_eq!(
        SlashCommand::parse("/server status"),
        Some(Ok(SlashCommand::Server(ServerAction::Status)))
    );
    assert_eq!(
        SlashCommand::parse("/server start"),
        Some(Ok(SlashCommand::Server(ServerAction::Start)))
    );
    assert_eq!(
        SlashCommand::parse("/server stop"),
        Some(Ok(SlashCommand::Server(ServerAction::Stop)))
    );
    let parsed = SlashCommand::parse("/server restart").expect("is a slash command");
    let message = parsed.expect_err("unknown subcommand");
    assert!(message.contains("status|start|stop"), "got: {message}");
}

#[test]
fn provider_add_accepts_xai_kinds() {
    let parsed =
        SlashCommand::parse("/provider add xai xai https://api.x.ai/v1 grok-4.3 XAI_API_KEY")
            .expect("is a slash command")
            .expect("parses");
    assert_eq!(
        parsed,
        SlashCommand::Provider(ProviderAction::Add {
            name: "xai".to_string(),
            kind: ProviderKind::Xai,
            base_url: "https://api.x.ai/v1".to_string(),
            model: "grok-4.3".to_string(),
            api_key_env: Some("XAI_API_KEY".to_string()),
        })
    );

    let parsed = SlashCommand::parse("/provider add grok xaioauth https://api.x.ai/v1 grok-4.3")
        .expect("is a slash command")
        .expect("parses");
    assert_eq!(
        parsed,
        SlashCommand::Provider(ProviderAction::Add {
            name: "grok".to_string(),
            kind: ProviderKind::XaiOauth,
            base_url: "https://api.x.ai/v1".to_string(),
            model: "grok-4.3".to_string(),
            api_key_env: None,
        })
    );

    // The error for an unknown kind names the xai kinds too.
    let parsed =
        SlashCommand::parse("/provider add x bogus https://e.com m").expect("is a slash command");
    let message = parsed.expect_err("unknown kind");
    assert!(message.contains("xai|xaioauth"), "got: {message}");
}

#[test]
fn provider_add_accepts_openrouter_kind() {
    let parsed = SlashCommand::parse(
            "/provider add openrouter openrouter https://openrouter.ai/api/v1 openrouter/auto OPENROUTER_API_KEY",
        )
        .expect("is a slash command")
        .expect("parses");
    assert_eq!(
        parsed,
        SlashCommand::Provider(ProviderAction::Add {
            name: "openrouter".to_string(),
            kind: ProviderKind::OpenRouter,
            base_url: "https://openrouter.ai/api/v1".to_string(),
            model: "openrouter/auto".to_string(),
            api_key_env: Some("OPENROUTER_API_KEY".to_string()),
        })
    );

    // The error for an unknown kind names openrouter too.
    let parsed =
        SlashCommand::parse("/provider add x bogus https://e.com m").expect("is a slash command");
    let message = parsed.expect_err("unknown kind");
    assert!(message.contains("openrouter"), "got: {message}");
}

#[test]
fn provider_add_accepts_cloudflare_kind() {
    let parsed = SlashCommand::parse(
            "/provider add cf cloudflare https://api.cloudflare.com/client/v4/accounts/acc/ai/v1 @cf/zai-org/glm-5.2 CLOUDFLARE_API_TOKEN",
        )
        .expect("is a slash command")
        .expect("parses");
    assert_eq!(
        parsed,
        SlashCommand::Provider(ProviderAction::Add {
            name: "cf".to_string(),
            kind: ProviderKind::Cloudflare,
            base_url: "https://api.cloudflare.com/client/v4/accounts/acc/ai/v1".to_string(),
            model: "@cf/zai-org/glm-5.2".to_string(),
            api_key_env: Some("CLOUDFLARE_API_TOKEN".to_string()),
        })
    );
}

#[test]
fn provider_no_args_opens_the_menu_and_list_still_lists() {
    // Bare `/provider` opens the interactive picker; `/provider list` keeps
    // the scripting/text behavior.
    assert_eq!(
        SlashCommand::parse("/provider"),
        Some(Ok(SlashCommand::Provider(ProviderAction::Menu)))
    );
    assert_eq!(
        SlashCommand::parse("/provider list"),
        Some(Ok(SlashCommand::Provider(ProviderAction::List)))
    );
}

#[test]
fn login_parses_with_a_provider_argument() {
    assert_eq!(
        SlashCommand::parse("/login xai"),
        Some(Ok(SlashCommand::Login("xai".to_string())))
    );
    let parsed = SlashCommand::parse("/login").expect("is a slash command");
    let message = parsed.expect_err("missing provider");
    assert!(message.contains("/login xai"), "got: {message}");
}

#[test]
fn fusion_parses_toggle_config_and_rejects_unknown() {
    assert_eq!(
        SlashCommand::parse("/fusion"),
        Some(Ok(SlashCommand::Fusion(FusionAction::Toggle)))
    );
    assert_eq!(
        SlashCommand::parse("/fusion config"),
        Some(Ok(SlashCommand::Fusion(FusionAction::Config)))
    );
    assert!(matches!(SlashCommand::parse("/fusion bogus"), Some(Err(_))));
}

#[test]
fn ultra_parses_toggle_config_and_rejects_unknown() {
    assert_eq!(
        SlashCommand::parse("/ultra"),
        Some(Ok(SlashCommand::Ultra(UltraAction::Toggle)))
    );
    assert_eq!(
        SlashCommand::parse("/ultra config"),
        Some(Ok(SlashCommand::Ultra(UltraAction::Config)))
    );
    assert!(matches!(SlashCommand::parse("/ultra bogus"), Some(Err(_))));
}

/// The `/ultra config` picker offers every lens in the catalog plus a trailing
/// judge row, pre-toggled to the configured roster, and Enter turns exactly the
/// toggled rows into the roster to save. The lens rows are compared as a set:
/// a user `~/.wizard/subagents/` entry that shadows a built-in moves it to the
/// end of the catalog, which is a legitimate reordering, not a failure.
#[test]
fn ultra_picker_saves_the_toggled_lenses_and_the_judge_row() {
    let mut app = app();
    app.open_ultra_picker();
    let picker = app.picker.as_ref().expect("the ultra picker is open");
    assert_eq!(picker.kind, PickerKind::UltraLenses);
    let (judge, lenses) = picker.items.split_last().expect("rows");
    assert_eq!(judge.value, ULTRA_JUDGE_ROW);
    assert!(judge.current, "the default roster runs one judge");
    for name in ultra::DEFAULT_LENSES {
        let row = lenses
            .iter()
            .find(|item| item.value == *name)
            .unwrap_or_else(|| panic!("{name} has a row"));
        assert!(row.current, "{name} is in the default roster");
    }

    let action = press(&mut app, KeyCode::Enter);
    let Some(AppAction::Command(SlashCommand::Ultra(UltraAction::Apply(saved)))) = action else {
        panic!("Enter saves the roster, got {action:?}");
    };
    let mut got = saved.lenses;
    got.sort();
    let mut want = UltraConfig::default().lenses;
    want.sort();
    assert_eq!(got, want);
    assert_eq!(saved.judges, 1, "the judge row was left on");
    assert!(app.picker.is_none(), "the picker closed");
}

/// Untoggling the judge row is how the compare phase is turned off — it must
/// reach `[ultra]` as `judges = 0`, not be silently floored back to one.
#[test]
fn ultra_picker_untoggling_the_judge_row_drops_the_compare_phase() {
    let mut app = app();
    app.open_ultra_picker();
    let picker = app.picker.as_mut().expect("the ultra picker is open");
    picker.selected = picker.items.len() - 1;
    press(&mut app, KeyCode::Char(' '));

    let action = press(&mut app, KeyCode::Enter);
    let Some(AppAction::Command(SlashCommand::Ultra(UltraAction::Apply(saved)))) = action else {
        panic!("Enter saves the roster, got {action:?}");
    };
    assert_eq!(saved.judges, 0);
    assert!(!saved.lenses.is_empty(), "the lens roster is untouched");
}

/// An empty roster has nothing to fan out over, so Enter refuses it rather
/// than persisting an `[ultra]` that `UltraEngine::build` would then reject
/// at the next toggle.
#[test]
fn ultra_picker_refuses_an_empty_roster() {
    let mut app = app();
    app.open_ultra_picker();
    let picker = app.picker.as_mut().expect("the ultra picker is open");
    for item in &mut picker.items {
        item.current = false;
    }

    assert!(press(&mut app, KeyCode::Enter).is_none(), "nothing to save");
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptEntry::Notice(text)) if text.contains("at least one lens")
    ));
}

#[test]
fn rewind_parses_with_and_without_a_turn() {
    assert_eq!(
        SlashCommand::parse("/rewind"),
        Some(Ok(SlashCommand::Rewind(None)))
    );
    assert_eq!(
        SlashCommand::parse("/rewind 7"),
        Some(Ok(SlashCommand::Rewind(Some(7))))
    );
    let parsed = SlashCommand::parse("/rewind soon").expect("is a slash command");
    let message = parsed.expect_err("non-numeric turn");
    assert!(message.contains("/rewind [turn]"), "got: {message}");
}

#[test]
fn resume_parses_with_and_without_an_id() {
    assert_eq!(
        SlashCommand::parse("/resume"),
        Some(Ok(SlashCommand::Resume(None)))
    );
    assert_eq!(
        SlashCommand::parse("/resume 2026-06-09T09-30-00"),
        Some(Ok(SlashCommand::Resume(Some(
            "2026-06-09T09-30-00".to_string()
        ))))
    );
}

#[test]
fn resume_picker_selection_becomes_a_resume_command() {
    let mut app = app();
    app.picker = Some(Picker {
        kind: PickerKind::Resume,
        title: " resume session ".to_string(),
        items: vec![PickerItem {
            value: "2026-06-09T09-30-00".to_string(),
            detail: "add resume · 4 msgs".to_string(),
            current: false,
        }],
        selected: 0,
    });
    let action = press(&mut app, KeyCode::Enter);
    assert!(matches!(
        action,
        Some(AppAction::Command(SlashCommand::Resume(Some(id)))) if id == "2026-06-09T09-30-00"
    ));
    assert!(app.picker.is_none(), "the picker closed");
}

#[test]
fn load_transcript_replays_messages_and_pairs_tool_results() {
    use crate::llm::{ChatMessage, FunctionCall, ToolCall};
    let mut app = app();
    let mut assistant = ChatMessage::assistant("reading it");
    assistant.tool_calls.push(ToolCall {
        function: FunctionCall {
            name: "read_file".to_string(),
            arguments: serde_json::json!({ "path": "x.rs" }),
        },
    });
    app.load_transcript(vec![
        ChatMessage::system("ignored system prompt"),
        ChatMessage::user("read x.rs"),
        assistant,
        ChatMessage::tool_result("read_file", "fn main() {}"),
    ]);
    // System dropped; user + assistant + one filled tool card remain.
    assert!(matches!(
        app.transcript.first(),
        Some(TranscriptEntry::User(text)) if text == "read x.rs"
    ));
    assert!(matches!(
        app.transcript.get(1),
        Some(TranscriptEntry::Assistant(text)) if text == "reading it"
    ));
    assert!(matches!(
        app.transcript.get(2),
        Some(TranscriptEntry::ToolCard { name, output: Some(out), .. })
            if name == "read_file" && out == "fn main() {}"
    ));
    assert_eq!(app.transcript.len(), 3);
}

#[test]
fn rewind_picker_selection_becomes_a_rewind_command() {
    let mut app = app();
    app.picker = Some(Picker {
        kind: PickerKind::Rewind,
        title: " rewind to before turn ".to_string(),
        items: vec![
            PickerItem {
                value: "9".to_string(),
                detail: "fix tests · notes.txt".to_string(),
                current: false,
            },
            PickerItem {
                value: "8".to_string(),
                detail: String::new(),
                current: false,
            },
        ],
        selected: 0,
    });
    press(&mut app, KeyCode::Down);
    let action = press(&mut app, KeyCode::Enter);
    assert!(matches!(
        action,
        Some(AppAction::Command(SlashCommand::Rewind(Some(8))))
    ));
    assert!(app.picker.is_none(), "the picker closed");
}

#[test]
fn rewind_picker_esc_cancels() {
    let mut app = app();
    app.picker = Some(Picker {
        kind: PickerKind::Rewind,
        title: " rewind to before turn ".to_string(),
        items: vec![PickerItem {
            value: "3".to_string(),
            detail: String::new(),
            current: false,
        }],
        selected: 0,
    });
    let action = press(&mut app, KeyCode::Esc);
    assert!(action.is_none());
    assert!(app.picker.is_none(), "Esc closed the picker");
}

#[test]
fn agents_parses_to_the_roster_picker() {
    // /agents opens the roster picker. Live runs are watched on the
    // subagent rail below the composer — no separate slash command.
    assert!(matches!(
        SlashCommand::parse("/agents"),
        Some(Ok(SlashCommand::Agents))
    ));
    assert!(
        matches!(
            SlashCommand::parse("/subagents"),
            Some(Err(message)) if message.contains("unknown command")
        ),
        "/subagents was removed; the rail is always on screen"
    );
}

#[test]
fn subagent_picker_selection_prefills_a_delegation_request() {
    let mut app = app();
    app.picker = Some(Picker {
        kind: PickerKind::Subagent,
        title: " delegate to subagent ".to_string(),
        items: vec![
            PickerItem {
                value: "worker".to_string(),
                detail: "general-purpose".to_string(),
                current: false,
            },
            PickerItem {
                value: "reviewer".to_string(),
                detail: "code review".to_string(),
                current: false,
            },
        ],
        selected: 0,
    });
    press(&mut app, KeyCode::Down);
    let action = press(&mut app, KeyCode::Enter);
    // Subagents are model-invoked, so Enter pre-fills input instead of
    // emitting a command.
    assert!(action.is_none());
    assert!(app.picker.is_none(), "the picker closed");
    assert_eq!(app.input, "Use the reviewer subagent to ");
    assert_eq!(app.cursor, app.input.chars().count());
}

#[test]
fn ctrl_c_idle_arms_then_exits() {
    let mut app = app();
    assert!(press_ctrl(&mut app, 'c').is_none());
    assert!(app.ctrl_c_armed);
    assert!(!app.should_quit, "first press only arms");
    assert!(press_ctrl(&mut app, 'c').is_none());
    assert!(app.should_quit, "second press exits");
}

#[test]
fn ctrl_c_busy_interrupts_then_exits() {
    let mut app = app();
    app.status.busy = true;
    // First press while busy interrupts the turn, doesn't quit.
    assert!(matches!(
        press_ctrl(&mut app, 'c'),
        Some(AppAction::Interrupt)
    ));
    assert!(!app.should_quit);
    // Armed now: a second press exits even while busy.
    assert!(press_ctrl(&mut app, 'c').is_none());
    assert!(app.should_quit);
}

#[test]
fn any_other_key_disarms_ctrl_c() {
    let mut app = app();
    press_ctrl(&mut app, 'c');
    assert!(app.ctrl_c_armed);
    press(&mut app, KeyCode::Char('x'));
    assert!(!app.ctrl_c_armed);
    // So the next Ctrl-C re-arms rather than quitting.
    assert!(press_ctrl(&mut app, 'c').is_none());
    assert!(!app.should_quit);
}

#[test]
fn dashboard_navigates_and_esc_closes() {
    use crate::session_registry::{SessionRecord, SessionState};
    let mut app = app();
    let make = |id: &str, state: SessionState| SessionRecord {
        id: id.to_string(),
        name: id.to_string(),
        cwd: "/tmp".to_string(),
        model: "m".to_string(),
        mode: "genie".to_string(),
        state,
        activity: String::new(),
        pid: 1,
        started_unix: 0,
        updated_unix: 0,
    };
    app.sessions = vec![
        make("a", SessionState::Working),
        make("b", SessionState::Idle),
    ];
    app.show_dashboard = true;

    // ↓ moves the selection and wraps; ↑ wraps back.
    press(&mut app, KeyCode::Down);
    assert_eq!(app.dashboard_selected, 1);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.dashboard_selected, 0, "wraps to the top");
    press(&mut app, KeyCode::Up);
    assert_eq!(app.dashboard_selected, 1, "wraps to the bottom");

    // Esc closes the modal.
    press(&mut app, KeyCode::Esc);
    assert!(!app.show_dashboard);
}

#[test]
fn dashboard_input_composes_and_esc_clears_then_closes() {
    let mut app = app();
    app.show_dashboard = true;
    press(&mut app, KeyCode::Char('h'));
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(app.dashboard_input, "hi");
    press(&mut app, KeyCode::Backspace);
    assert_eq!(app.dashboard_input, "h");
    // Esc with text clears it but keeps the modal open.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.dashboard_input, "");
    assert!(app.show_dashboard);
    // Esc again, now empty, closes the modal.
    press(&mut app, KeyCode::Esc);
    assert!(!app.show_dashboard);
}

#[test]
fn session_record_reflects_state() {
    let mut app = app();
    app.session_id = "sess-1".to_string();
    app.session_name = "fix bug".to_string();
    assert_eq!(app.session_record().state, SessionState::Idle);
    app.status.busy = true;
    assert_eq!(app.session_record().state, SessionState::Working);
}

#[test]
fn todo_update_mirrors_the_list_and_auto_shows_the_overlay_once() {
    use crate::tools::todo::{TodoItem, TodoStatus};
    let mut app = app();
    assert!(!app.show_todos);

    let items = vec![TodoItem {
        content: "first".to_string(),
        status: TodoStatus::InProgress,
    }];
    app.handle_agent_event(AgentEvent::TodoUpdated(items.clone()));
    assert_eq!(app.todos, items);
    assert!(app.show_todos, "first update auto-shows the overlay");

    // The user hides it; later updates respect that.
    app.show_todos = false;
    app.handle_agent_event(AgentEvent::TodoUpdated(items.clone()));
    assert!(!app.show_todos, "auto-show happens only once");
    assert_eq!(app.todos, items, "the list still updates");
}

#[test]
fn esc_dismisses_the_todo_overlay_after_the_diff_sidebar() {
    let mut app = app();
    app.show_todos = true;
    press(&mut app, KeyCode::Esc);
    assert!(!app.show_todos, "Esc dismisses the todo band");

    // Diff sidebar and todo band are independent: Esc closes the
    // diff first, then the overlay, then falls through to the input.
    app.show_todos = true;
    app.show_diff = true;
    app.input = "draft".to_string();
    press(&mut app, KeyCode::Esc);
    assert!(!app.show_diff, "diff closes first");
    assert!(app.show_todos, "todos stay open until the next Esc");
    press(&mut app, KeyCode::Esc);
    assert!(!app.show_todos);
    assert_eq!(app.input, "draft", "input untouched while panels close");
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.input, "", "Esc finally clears the input");

    // Vim Normal mode keeps the same escape hatch.
    let mut app = vim_app();
    press(&mut app, KeyCode::Esc); // insert -> normal
    app.show_todos = true;
    press(&mut app, KeyCode::Esc);
    assert!(!app.show_todos, "Normal-mode Esc dismisses the todo band");
}

#[test]
fn usage_events_drive_session_totals_and_the_context_meter() {
    let mut app = app();
    app.handle_agent_event(AgentEvent::Usage {
        prompt_tokens: 100,
        completion_tokens: 20,
    });
    app.handle_agent_event(AgentEvent::Usage {
        prompt_tokens: 50,
        completion_tokens: 5,
    });
    // Session lifetime still accumulates for /cost.
    assert_eq!(app.status.prompt_tokens, 150);
    assert_eq!(app.status.completion_tokens, 25);
    // Context meter tracks the most recent prompt size, not the sum.
    assert_eq!(app.status.context_tokens, 50);

    // Auto-compaction replaces the meter without touching lifetime totals.
    app.handle_agent_event(AgentEvent::ContextSize { tokens: 12 });
    assert_eq!(app.status.context_tokens, 12);
    assert_eq!(app.status.prompt_tokens, 150);
    assert_eq!(app.status.completion_tokens, 25);
}

#[test]
fn background_task_events_drive_the_live_status_bar_counter() {
    let mut app = app();
    assert_eq!(app.status.background_tasks, 0);

    app.handle_agent_event(AgentEvent::TaskStarted {
        id: 1,
        command: "sleep 5".to_string(),
    });
    assert_eq!(
        app.status.background_tasks, 1,
        "marker appears while running"
    );

    app.handle_agent_event(AgentEvent::TaskStarted {
        id: 2,
        command: "ping -c 1 example.com".to_string(),
    });
    assert_eq!(app.status.background_tasks, 2);

    app.handle_agent_event(AgentEvent::TaskFinished {
        id: 1,
        command: "sleep 5".to_string(),
        status: crate::tools::tasks::TaskStatus::Done(0),
    });
    assert_eq!(
        app.status.background_tasks, 1,
        "counter drops back down as tasks finish"
    );
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptEntry::Notice(text))
            if text.contains("background task #1 finished")
    ));

    app.handle_agent_event(AgentEvent::TaskFinished {
        id: 2,
        command: "ping -c 1 example.com".to_string(),
        status: crate::tools::tasks::TaskStatus::Done(0),
    });
    assert_eq!(
        app.status.background_tasks, 0,
        "marker clears once all finish"
    );
}

#[test]
fn failed_tool_cards_start_collapsed() {
    let mut app = app();
    app.handle_agent_event(AgentEvent::ToolStarted {
        name: "web_fetch".to_string(),
        args: serde_json::json!({"url": "https://example.com"}),
    });
    app.handle_agent_event(AgentEvent::ToolFinished {
        name: "web_fetch".to_string(),
        output: crate::tools::ToolOutput::error("HTTP 403 Forbidden\n<!DOCTYPE html>\n..."),
    });
    assert!(
        matches!(
            app.transcript.last(),
            Some(TranscriptEntry::ToolCard {
                is_error: true,
                collapsed: true,
                ..
            })
        ),
        "errors show only the ✗ card line until expanded via Ctrl-T"
    );

    // Short successful outputs still arrive expanded.
    app.handle_agent_event(AgentEvent::ToolStarted {
        name: "read_file".to_string(),
        args: serde_json::json!({"path": "a.txt"}),
    });
    app.handle_agent_event(AgentEvent::ToolFinished {
        name: "read_file".to_string(),
        output: crate::tools::ToolOutput::ok("one line"),
    });
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptEntry::ToolCard {
            is_error: false,
            collapsed: false,
            ..
        })
    ));
}

#[test]
fn stream_retry_discards_the_partial_streamed_text() {
    let mut app = app();
    app.handle_agent_event(AgentEvent::TextDelta("half an ans".to_string()));
    app.handle_agent_event(AgentEvent::StreamRetrying);
    app.handle_agent_event(AgentEvent::Error(
        "LLM unavailable (stream stalled); sleeping 5s then retrying (attempt 1)".to_string(),
    ));
    assert!(
        app.streaming.is_empty(),
        "the doomed attempt's partial text is dropped, not flushed"
    );
    assert!(
            !app
                .transcript
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::Assistant(text) if text.contains("half an ans"))),
            "no assistant entry made of the discarded partial"
        );

    // The retry streams the full answer; only that lands.
    app.handle_agent_event(AgentEvent::TextDelta("the full answer".to_string()));
    assert_eq!(app.streaming, "the full answer");
}

#[test]
fn long_outputs_start_collapsed_by_lines_or_length() {
    assert!(!collapse_long("short output"));
    assert!(!collapse_long(&"line\n".repeat(6)));
    assert!(collapse_long(&"line\n".repeat(7)), "more than six lines");
    assert!(
        collapse_long(&"x".repeat(601)),
        "a giant single line wraps to fill the screen just the same"
    );
}

fn click(app: &mut App, column: u16, row: u16) {
    use crossterm::event::MouseEvent;
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        let _ = app.handle_event(Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }));
    }
}

#[test]
fn clicking_a_tool_card_header_toggles_its_output() {
    let mut app = app();
    app.handle_agent_event(AgentEvent::ToolStarted {
        name: "execute".to_string(),
        args: serde_json::json!({"command": "ls"}),
    });
    app.handle_agent_event(AgentEvent::ToolFinished {
        name: "execute".to_string(),
        output: crate::tools::ToolOutput::ok("a\nb\nc\nd\ne\nf\ng\nh"),
    });
    let index = app.transcript.len() - 1;
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptEntry::ToolCard {
            collapsed: true,
            ..
        })
    ));

    // Render a frame so the click hit map is populated.
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| crate::ui::draw(frame, &app)).unwrap();
    let (row, hit_index) = *app
        .card_hits
        .borrow()
        .first()
        .expect("the card header should be clickable");
    assert_eq!(hit_index, index);

    // A plain click on the header expands the card...
    click(&mut app, 2, row);
    assert!(matches!(
        app.transcript.get(index),
        Some(TranscriptEntry::ToolCard {
            collapsed: false,
            ..
        })
    ));

    // ...and a second click (at its possibly-shifted row) collapses it.
    terminal.draw(|frame| crate::ui::draw(frame, &app)).unwrap();
    let row = app.card_hits.borrow().first().map(|(y, _)| *y).unwrap();
    click(&mut app, 2, row);
    assert!(matches!(
        app.transcript.get(index),
        Some(TranscriptEntry::ToolCard {
            collapsed: true,
            ..
        })
    ));
}

/// A real PNG on disk, as the image store would have left it: a solid red
/// square, so any cell that drew it is unmistakable.
fn red_png(dir: &Path) -> ImageRef {
    let path = dir.join("red.png");
    image::RgbaImage::from_pixel(48, 48, image::Rgba([255, 0, 0, 255]))
        .save(&path)
        .expect("wrote the png");
    ImageRef {
        path,
        mime: "image/png".to_string(),
        bytes: std::fs::metadata(dir.join("red.png")).unwrap().len() as usize,
    }
}

/// Every cell of a drawn frame, row by row: what is on screen.
fn screen(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
    terminal.backend().buffer().clone()
}

/// Rows holding image pixels. The UI is deliberately monochrome — white,
/// grays, and no hues anywhere (see [`crate::ui`]) — so a cell painted in
/// 24-bit colour is an image cell and nothing else. That makes this both the
/// "it drew" check and the "it left nothing behind" check.
fn pixel_rows(buf: &ratatui::buffer::Buffer) -> Vec<u16> {
    use ratatui::style::Color;
    (0..buf.area.height)
        .filter(|&y| {
            (0..buf.area.width).any(|x| {
                let cell = buf.cell((x, y)).unwrap();
                matches!(cell.fg, Color::Rgb(..)) || matches!(cell.bg, Color::Rgb(..))
            })
        })
        .collect()
}

fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf.cell((x, y)).unwrap().symbol())
        .collect()
}

#[test]
fn an_image_from_the_model_and_one_from_a_tool_both_render_with_their_file() {
    let dir = tempfile::tempdir().unwrap();
    let image = red_png(dir.path());
    let mut app = app();
    app.welcome_dismissed = true;

    app.handle_agent_event(AgentEvent::TextDelta("here it is".to_string()));
    app.handle_agent_event(AgentEvent::Images {
        source: ImageSource::Assistant,
        images: vec![image.clone()],
    });
    app.handle_agent_event(AgentEvent::ToolStarted {
        name: "render".to_string(),
        args: serde_json::json!({}),
    });
    app.handle_agent_event(AgentEvent::ToolFinished {
        name: "render".to_string(),
        output: crate::tools::ToolOutput::ok("drawn"),
    });
    app.handle_agent_event(AgentEvent::Images {
        source: ImageSource::Tool("render".to_string()),
        images: vec![image.clone()],
    });
    assert_eq!(
        app.transcript
            .iter()
            .filter(|entry| matches!(entry, TranscriptEntry::Image { .. }))
            .count(),
        2,
    );

    let buf = screen(&app, 80, 40);
    let text: String = (0..buf.area.height)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");

    // Both images were drawn, in pixels.
    assert_eq!(
        pixel_rows(&buf).len(),
        6,
        "two three-row image blocks, drawn in pixels:\n{text}"
    );
    // Each is named by what made it, and each names its file — untruncated,
    // on a line of its own, so it can be copied out and opened.
    assert!(text.contains("image · image/png"), "{text}");
    assert!(text.contains("image from `render` · image/png"), "{text}");
    let path = image.path.display().to_string();
    assert_eq!(
        (0..buf.area.height)
            .filter(|&y| row_text(&buf, y).trim() == path)
            .count(),
        2,
        "each image's path stands alone on its own row:\n{text}"
    );
}

#[test]
fn a_scrolled_image_is_clipped_to_the_transcript_and_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let image = red_png(dir.path());
    let mut app = app();
    app.welcome_dismissed = true;
    app.handle_agent_event(AgentEvent::Images {
        source: ImageSource::Assistant,
        images: vec![image],
    });
    // Enough text after it to push the image off the top of a short screen.
    for line in 0..12 {
        app.handle_agent_event(AgentEvent::Notice(format!("line {line}")));
    }

    // Pinned to the bottom, the image is above the viewport: no pixels.
    let (width, height) = (60u16, 12u16);
    let buf = screen(&app, width, height);
    assert!(pixel_rows(&buf).is_empty(), "the image is scrolled away");

    // Scroll it back into view a row at a time. However the block straddles
    // the edge of the viewport, its pixels stay inside the transcript body —
    // never in the composer, the rail or the status bar below it.
    let body = crate::ui::regions(&app, ratatui::layout::Rect::new(0, 0, width, height))[0];
    let mut ever_drawn = false;
    for _ in 0..20 {
        app.scroll_transcript(1);
        let buf = screen(&app, width, height);
        let rows = pixel_rows(&buf);
        ever_drawn |= !rows.is_empty();
        for row in rows {
            assert!(
                row < body.bottom(),
                "row {row} has pixels below the transcript body (which ends at {})",
                body.bottom()
            );
        }
    }
    assert!(
        ever_drawn,
        "scrolling back never brought the image into view"
    );

    // And back at the bottom, the screen is exactly what it was before the
    // scroll — no pixels left over anywhere.
    app.scroll_to_bottom();
    assert!(pixel_rows(&screen(&app, width, height)).is_empty());
}

#[test]
fn a_subagents_image_renders_inside_that_runs_pane() {
    let dir = tempfile::tempdir().unwrap();
    let image = red_png(dir.path());
    let mut app = app();
    app.welcome_dismissed = true;
    app.handle_agent_event(AgentEvent::SubagentRunStarted {
        run: 1,
        bg: None,
        name: "researcher".to_string(),
        task: "look".to_string(),
    });
    app.handle_agent_event(AgentEvent::SubagentRunImages {
        run: 1,
        source: ImageSource::Tool("screenshot".to_string()),
        images: vec![image],
    });

    // The run's image is its own: the main chat, which the subagent has said
    // nothing to yet, shows no pixels.
    assert!(pixel_rows(&screen(&app, 80, 40)).is_empty());

    // Open the pane and it is there, on the tool that took it.
    app.attached = Some(0);
    let buf = screen(&app, 80, 40);
    assert!(!pixel_rows(&buf).is_empty(), "the pane draws the image");
    let text: String = (0..buf.area.height)
        .map(|y| row_text(&buf, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("image from `screenshot`"), "{text}");
}

#[test]
fn a_resumed_session_replays_the_images_it_left_on_disk() {
    use crate::llm::{ChatMessage, Image};
    let png = Image::new("iVBOR", "image/png").at_path(PathBuf::from("/img/a.png"));

    let mut app = app();
    let mut assistant = ChatMessage::assistant("done");
    assistant.images.push(png.clone());
    app.load_transcript(vec![
        ChatMessage::user("draw"),
        assistant,
        ChatMessage::tool_result("render", "ok"),
        // The images `render` returned, riding back to the model. Not a
        // prompt — the agent wrote it, not the user.
        ChatMessage::user_with_images("Image(s) returned by `render`:", vec![png]),
    ]);

    let images: Vec<&TranscriptEntry> = app
        .transcript
        .iter()
        .filter(|entry| matches!(entry, TranscriptEntry::Image { .. }))
        .collect();
    assert!(
        matches!(
            images.as_slice(),
            [
                TranscriptEntry::Image {
                    source: ImageSource::Assistant,
                    ..
                },
                TranscriptEntry::Image {
                    source: ImageSource::Tool(tool),
                    image,
                },
            ] if tool == "render" && image.path == Path::new("/img/a.png")
        ),
        "both directions came back, attributed: {images:?}"
    );
    assert!(
            !app.transcript
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::User(text) if text.contains("Image(s) returned"))),
            "the carrier message is not replayed as something the user said"
        );
}

#[test]
fn backtab_toggles_plan_mode() {
    let mut app = app();
    let action = press(&mut app, KeyCode::BackTab);
    assert!(matches!(
        action,
        Some(AppAction::Command(SlashCommand::Plan))
    ));
}

#[test]
fn backtab_in_a_picker_still_navigates() {
    let mut app = app();
    app.picker = Some(Picker {
        kind: PickerKind::Mode,
        title: " select mode ".to_string(),
        items: vec![
            PickerItem {
                value: "genie".to_string(),
                detail: String::new(),
                current: true,
            },
            PickerItem {
                value: "sovereign".to_string(),
                detail: String::new(),
                current: false,
            },
        ],
        selected: 0,
    });
    let action = press(&mut app, KeyCode::BackTab);
    assert!(action.is_none(), "the picker captured the key");
    assert_eq!(app.picker.as_ref().expect("open").selected, 1);
}

/// Open a plan review via the agent event, returning the verdict
/// receiver.
fn open_review(app: &mut App, plan: &str) -> tokio::sync::oneshot::Receiver<PlanVerdict> {
    let (respond, rx) = tokio::sync::oneshot::channel();
    app.handle_agent_event(AgentEvent::PlanReady {
        plan: plan.to_string(),
        respond,
    });
    rx
}

#[test]
fn plan_ready_opens_a_review_and_y_approves() {
    let mut app = app();
    let mut rx = open_review(&mut app, "# the plan");
    let review = app.plan_review.as_ref().expect("review open");
    assert_eq!(review.plan, "# the plan");
    assert!(app.plan_mode, "a pending plan implies plan mode");

    // Review keys never leak into the input line.
    press(&mut app, KeyCode::Char('y'));
    assert!(app.input.is_empty());
    assert!(app.plan_review.is_none(), "review closed");
    assert!(!app.plan_mode, "approval clears the plan-mode mirror");
    assert_eq!(rx.try_recv(), Ok(PlanVerdict::approve()));
}

#[test]
fn plan_review_enter_also_approves() {
    let mut app = app();
    let mut rx = open_review(&mut app, "# p");
    let action = press(&mut app, KeyCode::Enter);
    assert!(action.is_none());
    assert_eq!(rx.try_recv(), Ok(PlanVerdict::approve()));
}

#[test]
fn plan_review_rejection_collects_feedback() {
    let mut app = app();
    let mut rx = open_review(&mut app, "# p");

    press(&mut app, KeyCode::Char('n'));
    let review = app.plan_review.as_ref().expect("still open");
    assert_eq!(review.feedback.as_deref(), Some(""));

    type_str(&mut app, "add testz");
    press(&mut app, KeyCode::Backspace);
    type_str(&mut app, "s first");
    assert!(app.input.is_empty(), "feedback typing never hits the input");
    press(&mut app, KeyCode::Enter);

    assert!(app.plan_review.is_none(), "review closed");
    assert!(app.plan_mode, "rejection keeps plan mode on");
    assert_eq!(rx.try_recv(), Ok(PlanVerdict::reject("add tests first")));
}

#[test]
fn plan_review_esc_leaves_feedback_entry() {
    let mut app = app();
    let mut rx = open_review(&mut app, "# p");
    press(&mut app, KeyCode::Char('n'));
    type_str(&mut app, "half a thought");
    press(&mut app, KeyCode::Esc);
    let review = app.plan_review.as_ref().expect("still open");
    assert!(review.feedback.is_none(), "back to the review state");
    assert!(rx.try_recv().is_err(), "no verdict sent yet");
    // 'n' again starts fresh feedback.
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(
        app.plan_review.as_ref().expect("open").feedback.as_deref(),
        Some("")
    );
}

/// Open an interview via the agent event, returning the answers receiver.
fn open_interview(
    app: &mut App,
    questions: Vec<InterviewQuestion>,
) -> tokio::sync::oneshot::Receiver<Option<Vec<String>>> {
    let (respond, rx) = tokio::sync::oneshot::channel();
    app.handle_agent_event(AgentEvent::Interview { questions, respond });
    rx
}

fn question(q: &str, options: &[&str]) -> InterviewQuestion {
    InterviewQuestion {
        question: q.to_string(),
        options: options.iter().map(|s| s.to_string()).collect(),
    }
}

/// Parse `input` and return the agent-runnable verdict, asserting it is a
/// well-formed command first.
fn runnable(input: &str) -> Result<(), String> {
    match SlashCommand::parse(input) {
        Some(Ok(command)) => command.agent_runnable(),
        other => panic!("{input} did not parse to a command: {other:?}"),
    }
}

#[test]
fn agent_runnable_allows_self_config_and_info_commands() {
    for input in [
        "/effort high",
        "/model claude-sonnet-5",
        "/mode sovereign",
        "/goal ship it",
        "/goal",
        "/status",
        "/diff",
        "/compact",
        "/reload",
        "/settings",
        "/fusion",
        "/ultra",
    ] {
        assert!(runnable(input).is_ok(), "{input} should be runnable");
    }
}

#[test]
fn command_requested_event_queues_for_post_turn_dispatch() {
    let mut app = app();
    assert!(app.pending_agent_commands.is_empty());
    app.handle_agent_event(AgentEvent::CommandRequested("/effort high".into()));
    assert_eq!(app.pending_agent_commands, vec!["/effort high".to_string()]);
    // A second request accumulates rather than replacing.
    app.handle_agent_event(AgentEvent::CommandRequested("/compact".into()));
    assert_eq!(
        app.pending_agent_commands,
        vec!["/effort high".to_string(), "/compact".to_string()]
    );
}

#[test]
fn agent_runnable_refuses_pickers_and_dangerous_commands() {
    for input in [
        "/effort",   // interactive picker without an argument
        "/model",    // interactive picker without an argument
        "/mode",     // interactive picker without an argument
        "/quit",     // ends the session
        "/clear",    // wipes the conversation
        "/rewind 2", // restores checkpoints
        "/resume",   // switches sessions
        "/login xai",
        "/provider list",
        "/publish",
        "/agents",
        "/fusion config",
        "/ultra config",
    ] {
        assert!(runnable(input).is_err(), "{input} should be refused");
    }
}

#[test]
fn interview_collects_answers_and_advances() {
    let mut app = app();
    let mut rx = open_interview(
        &mut app,
        vec![
            question("which db?", &["sqlite", "postgres"]),
            question("any auth?", &[]),
        ],
    );
    assert!(app.interview.is_some(), "interview modal open");

    // Pick option 2 for the first question, then accept it with Enter.
    press(&mut app, KeyCode::Char('2'));
    assert_eq!(
        app.interview.as_ref().expect("open").input,
        "postgres",
        "digit fills the matching option"
    );
    assert!(
        app.input.is_empty(),
        "interview keys never hit the input line"
    );
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.interview.as_ref().expect("still open").current, 1);

    // Free-text the second answer.
    type_str(&mut app, "yes, oauth");
    press(&mut app, KeyCode::Enter);

    assert!(
        app.interview.is_none(),
        "interview closed after the last answer"
    );
    assert_eq!(
        rx.try_recv(),
        Ok(Some(vec!["postgres".to_string(), "yes, oauth".to_string()]))
    );
}

#[test]
fn interview_esc_dismisses_with_no_answers() {
    let mut app = app();
    let mut rx = open_interview(&mut app, vec![question("which db?", &[])]);
    press(&mut app, KeyCode::Esc);
    assert!(app.interview.is_none(), "dismissed");
    assert_eq!(rx.try_recv(), Ok(None), "decline sent to the tool");
}

#[test]
fn empty_interview_declines_immediately() {
    let mut app = app();
    let mut rx = open_interview(&mut app, vec![]);
    assert!(app.interview.is_none(), "nothing to ask");
    assert_eq!(rx.try_recv(), Ok(None));
}

#[test]
fn omakase_proceeding_clears_flags_and_shows_the_plan() {
    let mut app = app();
    app.plan_mode = true;
    app.omakase = true;
    app.handle_agent_event(AgentEvent::OmakaseProceeding {
        plan: "# chef plan".to_string(),
    });
    assert!(!app.plan_mode, "chef's choice leaves plan mode");
    assert!(!app.omakase, "omakase cleared once proceeding");
    let shown = app.transcript.iter().any(|e| {
        matches!(
            e,
            TranscriptEntry::ToolCard { output: Some(p), .. } if p == "# chef plan"
        )
    });
    assert!(shown, "the chosen plan is surfaced in the transcript");
}

#[test]
fn cursor_editing_inserts_mid_line() {
    let mut app = app();
    type_str(&mut app, "helo");
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Char('l'));
    assert_eq!(app.input, "hello");
    press(&mut app, KeyCode::Home);
    press(&mut app, KeyCode::Delete);
    assert_eq!(app.input, "ello");
    press(&mut app, KeyCode::End);
    press(&mut app, KeyCode::Backspace);
    assert_eq!(app.input, "ell");
}

#[test]
fn history_recall_restores_draft() {
    let mut app = app();
    type_str(&mut app, "first message");
    press(&mut app, KeyCode::Enter);
    type_str(&mut app, "second message");
    press(&mut app, KeyCode::Enter);

    type_str(&mut app, "draft");
    press(&mut app, KeyCode::Up);
    assert_eq!(app.input, "second message");
    press(&mut app, KeyCode::Up);
    assert_eq!(app.input, "first message");
    press(&mut app, KeyCode::Down);
    assert_eq!(app.input, "second message");
    press(&mut app, KeyCode::Down);
    assert_eq!(app.input, "draft");
}

#[test]
fn picker_navigation_wraps_and_enter_selects() {
    let mut app = app();
    app.picker = Some(Picker {
        kind: PickerKind::Model,
        title: " select model ".to_string(),
        items: vec![
            PickerItem {
                value: "qwen3.6:27b".to_string(),
                detail: String::new(),
                current: true,
            },
            PickerItem {
                value: "llama4:8b".to_string(),
                detail: String::new(),
                current: false,
            },
        ],
        selected: 0,
    });

    press(&mut app, KeyCode::Up);
    assert_eq!(app.picker.as_ref().expect("open").selected, 1);
    let action = press(&mut app, KeyCode::Enter);
    match action {
        Some(AppAction::Command(SlashCommand::Model(Some(tag)))) => {
            assert_eq!(tag, "llama4:8b");
        }
        other => panic!("expected model switch, got {other:?}"),
    }
    assert!(app.picker.is_none());
}

#[test]
fn picker_escape_cancels() {
    let mut app = app();
    app.picker = Some(Picker {
        kind: PickerKind::Mode,
        title: " select mode ".to_string(),
        items: vec![PickerItem {
            value: "genie".to_string(),
            detail: String::new(),
            current: true,
        }],
        selected: 0,
    });
    press(&mut app, KeyCode::Esc);
    assert!(app.picker.is_none());
}

#[test]
fn ctrl_w_kills_previous_word() {
    let mut app = app();
    type_str(&mut app, "fix the parser bug");
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL))
        .expect("key handled");
    assert_eq!(app.input, "fix the parser ");
}

#[test]
fn history_recall_of_slash_command_keeps_browsing_history() {
    let mut app = app();
    type_str(&mut app, "older message");
    press(&mut app, KeyCode::Enter);
    type_str(&mut app, "/model");
    press(&mut app, KeyCode::Enter);

    press(&mut app, KeyCode::Up);
    assert_eq!(app.input, "/model");
    // The recalled slash command repopulates suggestions; ↑ must keep
    // walking history instead of cycling them.
    press(&mut app, KeyCode::Up);
    assert_eq!(app.input, "older message");
}

#[test]
fn unbound_ctrl_chords_do_not_insert_literal_chars() {
    let mut app = app();
    type_str(&mut app, "abc");
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL))
        .expect("key handled");
    assert_eq!(app.input, "abc");
}

#[test]
fn ctrl_g_requests_external_prompt_edit() {
    let mut app = app();
    type_str(&mut app, "draft in progress");
    let action = press_ctrl(&mut app, 'g');
    assert!(action.is_none());
    assert!(app.pending_edit_prompt);
    // The buffer is only replaced after the editor exits cleanly.
    assert_eq!(app.input, "draft in progress");
}

#[test]
fn ctrl_g_is_inert_during_masked_key_entry() {
    // An API key being typed must never be staged into a temp file.
    let mut app = app();
    app.web_key_backend = Some("brave".to_string());
    type_str(&mut app, "sk-secret");
    press_ctrl(&mut app, 'g');
    assert!(!app.pending_edit_prompt);
    assert_eq!(app.input, "sk-secret", "chord must not insert a literal g");
}

#[test]
fn editor_text_replaces_input_with_cursor_at_end() {
    let mut app = app();
    type_str(&mut app, "old draft");
    app.set_input_from_editor("hello\nworld\n".to_string());
    // Exactly one trailing newline (the editor's) is trimmed.
    assert_eq!(app.input, "hello\nworld");
    assert_eq!(app.cursor, app.input.chars().count());
}

#[test]
fn editor_text_trims_at_most_one_line_ending() {
    let mut app = app();
    app.set_input_from_editor("two\n\n".to_string());
    assert_eq!(app.input, "two\n");
    app.set_input_from_editor("crlf\r\n".to_string());
    assert_eq!(app.input, "crlf");
}

#[test]
fn busy_submit_queues_the_message() {
    let mut app = app();
    app.status.busy = true;
    type_str(&mut app, "queued message");
    let action = press(&mut app, KeyCode::Enter);
    assert!(action.is_none(), "queued submit is not an AppAction");
    assert_eq!(app.history, vec!["queued message".to_string()]);
    assert_eq!(app.message_queue.len(), 1);
    assert_eq!(app.message_queue[0].text, "queued message");
    assert!(app.input.is_empty(), "composer clears on queue");
    assert!(
        matches!(
            app.transcript.iter().find(|e| matches!(e, TranscriptEntry::User(_))),
            Some(TranscriptEntry::User(t)) if t == "queued message"
        ),
        "queued text lands in the transcript"
    );
    assert!(
        matches!(
            app.transcript.last(),
            Some(TranscriptEntry::Notice(n)) if n.contains("queued")
        ),
        "a notice announces the queue position"
    );
}

#[test]
fn busy_submit_respects_the_queue_cap() {
    let mut app = app();
    app.status.busy = true;
    for i in 0..MESSAGE_QUEUE_CAP {
        type_str(&mut app, &format!("msg {i}"));
        let action = press(&mut app, KeyCode::Enter);
        assert!(action.is_none());
    }
    assert_eq!(app.message_queue.len(), MESSAGE_QUEUE_CAP);
    type_str(&mut app, "one too many");
    let action = press(&mut app, KeyCode::Enter);
    assert!(action.is_none());
    assert_eq!(app.message_queue.len(), MESSAGE_QUEUE_CAP);
    assert_eq!(app.input, "one too many", "overflow keeps the composer");
    assert!(
        matches!(
            app.transcript.last(),
            Some(TranscriptEntry::Notice(n)) if n.contains("full")
        ),
        "overflow surfaces a full-queue notice"
    );
}

#[test]
fn goal_kickoff_queues_a_working_turn() {
    let mut app = app();
    app.queue_goal_kickoff("rewrite spore in assembly");
    assert_eq!(app.message_queue.len(), 1);
    assert!(
        app.message_queue[0]
            .text
            .contains("rewrite spore in assembly")
    );
    assert!(
        matches!(
            app.transcript.last(),
            Some(TranscriptEntry::User(t)) if t.contains("rewrite spore in assembly")
        ),
        "the kickoff prompt lands in the transcript"
    );
}

#[test]
fn goal_kickoff_respects_the_queue_cap() {
    let mut app = app();
    app.status.busy = true;
    for i in 0..MESSAGE_QUEUE_CAP {
        type_str(&mut app, &format!("msg {i}"));
        press(&mut app, KeyCode::Enter);
    }
    app.queue_goal_kickoff("rewrite spore in assembly");
    assert_eq!(app.message_queue.len(), MESSAGE_QUEUE_CAP);
    assert!(
        matches!(
            app.transcript.last(),
            Some(TranscriptEntry::Notice(n)) if n.contains("full")
        ),
        "a full queue surfaces a notice instead of auto-starting"
    );
}

#[test]
fn pop_queued_message_is_fifo() {
    let mut app = app();
    app.status.busy = true;
    type_str(&mut app, "first");
    press(&mut app, KeyCode::Enter);
    type_str(&mut app, "second");
    press(&mut app, KeyCode::Enter);
    let a = app.pop_queued_message().expect("first");
    let b = app.pop_queued_message().expect("second");
    assert_eq!(a.text, "first");
    assert_eq!(b.text, "second");
    assert!(app.pop_queued_message().is_none());
}

#[test]
fn ctrl_u_kills_to_line_start_keeping_tail() {
    let mut app = app();
    type_str(&mut app, "hello world");
    for _ in 0..6 {
        press(&mut app, KeyCode::Left);
    }
    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
        .expect("key handled");
    assert_eq!(app.input, " world");
    assert_eq!(app.cursor, 0);
}

#[test]
fn submit_rejected_while_agent_rebuilds() {
    let mut app = app();
    app.rebuilding = Some("switching to qwen3:0.6b".to_string());
    type_str(&mut app, "hello");
    let action = press(&mut app, KeyCode::Enter);
    assert!(action.is_none());
    assert!(app.history.is_empty());
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptEntry::Notice(_))
    ));
}

#[test]
fn ctrl_p_is_a_noop_while_busy() {
    let mut app = app();
    app.status.busy = true;
    let action = app
        .handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .expect("key handled");
    assert!(action.is_none());
    assert!(app.picker.is_none());
}

// --- vim modal editing ---

fn vim_app() -> App {
    let mut app = app();
    app.toggle_vim();
    assert!(app.vim.enabled);
    assert_eq!(app.vim.mode, VimMode::Insert);
    app
}

#[test]
fn esc_enters_normal_x_deletes_and_i_returns_to_insert() {
    let mut app = vim_app();
    type_str(&mut app, "hello");
    assert_eq!(app.vim.mode, VimMode::Insert);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.vim.mode, VimMode::Normal);
    // Leaving insert nudges the cursor left onto the last char ('o').
    assert_eq!(app.cursor, 4);
    // In normal mode 'x' deletes the char under the cursor, not insert 'x'.
    press(&mut app, KeyCode::Char('x'));
    assert_eq!(app.input, "hell");
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(app.vim.mode, VimMode::Insert);
}

#[test]
fn word_motions_and_dw_in_normal_mode() {
    let mut app = vim_app();
    type_str(&mut app, "foo bar baz");
    press(&mut app, KeyCode::Esc); // normal, cursor on last 'z'
    press(&mut app, KeyCode::Char('0')); // start of line
    assert_eq!(app.cursor, 0);
    press(&mut app, KeyCode::Char('w')); // -> "bar"
    assert_eq!(app.cursor, 4);
    // dw deletes the word + trailing space.
    press(&mut app, KeyCode::Char('d'));
    press(&mut app, KeyCode::Char('w'));
    assert_eq!(app.input, "foo baz");
}

#[test]
fn insert_transitions_append() {
    let mut app = vim_app();
    type_str(&mut app, "ab");
    press(&mut app, KeyCode::Esc); // normal, cursor on 'b' (index 1)
    press(&mut app, KeyCode::Char('0')); // index 0 ('a')
    press(&mut app, KeyCode::Char('a')); // insert after 'a'
    assert_eq!(app.vim.mode, VimMode::Insert);
    press(&mut app, KeyCode::Char('X'));
    assert_eq!(app.input, "aXb");
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('A')); // append at end
    type_str(&mut app, "Z");
    assert_eq!(app.input, "aXbZ");
}

#[test]
fn dd_clears_line_and_u_undoes() {
    let mut app = vim_app();
    type_str(&mut app, "scratch");
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('d'));
    press(&mut app, KeyCode::Char('d'));
    assert_eq!(app.input, "");
    press(&mut app, KeyCode::Char('u'));
    assert_eq!(app.input, "scratch");
}

#[test]
fn count_prefix_repeats_motion() {
    let mut app = vim_app();
    type_str(&mut app, "abcdef");
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('0'));
    press(&mut app, KeyCode::Char('3'));
    press(&mut app, KeyCode::Char('l')); // 3 right -> index 3
    assert_eq!(app.cursor, 3);
    press(&mut app, KeyCode::Char('2'));
    press(&mut app, KeyCode::Char('x')); // delete 2 chars
    assert_eq!(app.input, "abcf");
}

#[test]
fn delete_then_paste_register() {
    let mut app = vim_app();
    type_str(&mut app, "ab");
    press(&mut app, KeyCode::Esc); // cursor on 'b' (index 1)
    press(&mut app, KeyCode::Char('0')); // index 0
    press(&mut app, KeyCode::Char('x')); // delete 'a' -> register "a", input "b"
    assert_eq!(app.input, "b");
    press(&mut app, KeyCode::Char('p')); // paste after 'b'
    assert_eq!(app.input, "ba");
}

#[test]
fn enter_submits_in_normal_mode() {
    let mut app = vim_app();
    type_str(&mut app, "/help");
    press(&mut app, KeyCode::Esc);
    let action = press(&mut app, KeyCode::Enter);
    assert!(matches!(
        action,
        Some(AppAction::Command(SlashCommand::Help))
    ));
    assert_eq!(app.input, "");
}

#[test]
fn disabled_vim_inserts_hjkl_literally() {
    let mut app = app(); // vim off
    type_str(&mut app, "hjkl");
    press(&mut app, KeyCode::Esc); // plain clear, not a mode switch
    assert_eq!(app.input, "");
}

// --- Shift/Alt+Enter newline ---

#[test]
fn shift_enter_inserts_newline_without_submitting() {
    let mut app = app();
    type_str(&mut app, "line one");
    let action = press_mod(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
    assert!(action.is_none());
    type_str(&mut app, "line two");
    assert_eq!(app.input, "line one\nline two");
    // Nothing was submitted.
    assert!(!app.has_conversation());
}

#[test]
fn alt_enter_also_inserts_newline() {
    let mut app = app();
    type_str(&mut app, "a");
    press_mod(&mut app, KeyCode::Enter, KeyModifiers::ALT);
    type_str(&mut app, "b");
    assert_eq!(app.input, "a\nb");
}

#[test]
fn plain_enter_submits_multiline_input() {
    let mut app = app();
    type_str(&mut app, "first");
    press_mod(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
    type_str(&mut app, "second");
    let action = press(&mut app, KeyCode::Enter);
    match action {
        Some(AppAction::Submit(prepared)) => {
            assert!(
                prepared.text.contains("first")
                    && prepared.text.contains('\n')
                    && prepared.text.contains("second")
            );
        }
        other => panic!("expected a submit action, got {other:?}"),
    }
    assert_eq!(app.input, "");
}

#[test]
fn shift_enter_inserts_newline_in_vim_normal_mode() {
    let mut app = vim_app();
    type_str(&mut app, "xy");
    press(&mut app, KeyCode::Esc); // NORMAL, cursor on the last char
    let action = press_mod(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
    // A break is inserted (never submits); the cursor sits on a char, so it
    // lands before it rather than at the very end.
    assert!(action.is_none());
    assert!(app.input.contains('\n'));
    assert_eq!(app.input.chars().filter(|c| !c.is_whitespace()).count(), 2);
}

// ---- Subagent rail ---------------------------------------------------

#[test]
fn subagent_run_events_build_a_pane() {
    let mut app = app_with_panes(1);
    assert_eq!(app.panes.len(), 1);
    assert_eq!(app.panes[0].name, "agent0");
    assert_eq!(app.panes[0].status, PaneStatus::Running);

    app.handle_agent_event(AgentEvent::SubagentRunToolStarted {
        run: 0,
        name: "read_file".to_string(),
        args: serde_json::json!({"path": "src/app.rs"}),
    });
    app.handle_agent_event(AgentEvent::SubagentRunText {
        run: 0,
        text: "found it".to_string(),
    });

    // The subagent's work lands in *its* pane, not the main transcript.
    assert_eq!(app.panes[0].transcript.len(), 2);
    assert!(app.transcript.is_empty());
    // …and it is flagged as unread, since the user is not watching it.
    assert_eq!(app.panes[0].unread, 2);
}

#[test]
fn concurrent_runs_of_one_subagent_stay_in_separate_panes() {
    let mut app = app();
    for run in [7, 9] {
        app.handle_agent_event(AgentEvent::SubagentRunStarted {
            run,
            bg: None,
            name: "worker".to_string(),
            task: format!("task {run}"),
        });
    }
    app.handle_agent_event(AgentEvent::SubagentRunText {
        run: 9,
        text: "from the second".to_string(),
    });

    assert_eq!(app.panes.len(), 2);
    assert!(app.panes[0].transcript.is_empty());
    assert_eq!(app.panes[1].transcript.len(), 1);
}

#[test]
fn tool_output_lands_on_the_panes_open_card() {
    let mut app = app_with_panes(1);
    app.handle_agent_event(AgentEvent::SubagentRunToolStarted {
        run: 0,
        name: "read_file".to_string(),
        args: Value::Null,
    });
    app.handle_agent_event(AgentEvent::SubagentRunToolFinished {
        run: 0,
        name: "read_file".to_string(),
        output: crate::tools::ToolOutput::ok("contents"),
    });

    assert_eq!(app.panes[0].transcript.len(), 1);
    let TranscriptEntry::ToolCard { output, .. } = &app.panes[0].transcript[0] else {
        panic!("expected a tool card");
    };
    assert_eq!(output.as_deref(), Some("contents"));
}

#[test]
fn down_from_the_composer_focuses_the_rail_then_enter_attaches() {
    let mut app = app_with_panes(2);
    assert_eq!(app.rail_focus, None);

    press(&mut app, KeyCode::Down);
    assert_eq!(app.rail_focus, Some(0));

    press(&mut app, KeyCode::Down);
    assert_eq!(app.rail_focus, Some(1));
    // Clamped at the bottom rather than wrapping — you cannot fall off.
    press(&mut app, KeyCode::Down);
    assert_eq!(app.rail_focus, Some(1));

    press(&mut app, KeyCode::Enter);
    assert_eq!(app.attached, Some(1));
    assert_eq!(
        app.attached_pane().map(|pane| pane.name.as_str()),
        Some("agent1")
    );

    // Esc backs out to the main chat, all the way to the composer.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.attached, None);
    assert_eq!(app.rail_focus, None);
}

#[test]
fn up_off_the_top_of_the_rail_returns_to_the_composer() {
    let mut app = app_with_panes(2);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.rail_focus, Some(0));

    press(&mut app, KeyCode::Up);
    assert_eq!(app.rail_focus, None);

    // Focus really is back in the composer: typing goes to the input.
    press(&mut app, KeyCode::Char('h'));
    assert_eq!(app.input, "h");
}

#[test]
fn down_still_walks_history_when_there_are_no_subagents() {
    let mut app = app();
    app.history.push("earlier".to_string());
    press(&mut app, KeyCode::Up);
    assert_eq!(app.input, "earlier");
    press(&mut app, KeyCode::Down);
    assert_eq!(app.rail_focus, None);
    assert!(app.input.is_empty());
}

#[test]
fn typing_on_the_rail_hands_focus_back_to_the_composer() {
    let mut app = app_with_panes(1);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.rail_focus, Some(0));

    // The keystroke must not be swallowed by the rail.
    press(&mut app, KeyCode::Char('x'));
    assert_eq!(app.rail_focus, None);
    assert_eq!(app.input, "x");
}

#[test]
fn vim_normal_j_and_k_drive_the_rail_like_arrows() {
    let mut app = app_with_panes(2);
    app.toggle_vim();
    press(&mut app, KeyCode::Esc);
    assert!(app.vim.is_normal());

    // j from the composer drops into the rail, then walks it down,
    // clamping at the bottom just like ↓.
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.rail_focus, Some(0));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.rail_focus, Some(1));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.rail_focus, Some(1));

    // k walks back up; off the top it returns to the composer.
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.rail_focus, Some(0));
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.rail_focus, None);

    // Insert mode is still text: on the rail, j hands focus back and
    // types.
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.rail_focus, Some(0));
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(app.rail_focus, None);
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.input, "j");
}

#[test]
fn vim_normal_j_finishes_history_before_dropping_into_the_rail() {
    let mut app = app_with_panes(1);
    app.toggle_vim();
    app.history.push("earlier".to_string());
    press(&mut app, KeyCode::Esc);

    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.input, "earlier");
    // Mid-history j walks forward (back to the empty draft) rather than
    // jumping to the rail.
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.rail_focus, None);
    assert!(app.input.is_empty());
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.rail_focus, Some(0));
}

#[test]
fn vim_normal_j_keeps_walking_subagents_while_attached() {
    let mut app = app_with_panes(2);
    app.toggle_vim();
    press(&mut app, KeyCode::Esc);
    app.attach_pane(0);

    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.attached, Some(1));
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.attached, Some(0));

    // In insert mode the composer under the pane is live again: j types.
    press(&mut app, KeyCode::Char('i'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.attached, Some(0));
    assert_eq!(app.input, "j");
}

#[test]
fn attaching_clears_the_unread_badge_and_live_entries_stay_read() {
    let mut app = app_with_panes(1);
    app.handle_agent_event(AgentEvent::SubagentRunText {
        run: 0,
        text: "one".to_string(),
    });
    assert_eq!(app.panes[0].unread, 1);

    app.attach_pane(0);
    assert_eq!(app.panes[0].unread, 0);

    // While you are watching, new work is not "unread".
    app.handle_agent_event(AgentEvent::SubagentRunText {
        run: 0,
        text: "two".to_string(),
    });
    assert_eq!(app.panes[0].unread, 0);
}

#[test]
fn run_done_retires_the_pane() {
    let mut app = app_with_panes(1);
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: true,
        output: "report".to_string(),
        steps_used: 3,
        error: None,
    });
    assert_eq!(app.panes[0].status, PaneStatus::Done);
    assert!(app.panes[0].finished.is_some());
    assert_eq!(app.running_panes(), 0);
}

#[test]
fn the_final_report_lands_in_the_pane() {
    let mut app = app_with_panes(1);
    // The report is the step that made no tool call, so the sub-loop ends
    // on it and never streams it as text — it only arrives on the Done
    // event. The pane must still show it.
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: true,
        output: "the auth flow starts in login.rs".to_string(),
        steps_used: 2,
        error: None,
    });
    let TranscriptEntry::Assistant(text) = app.panes[0].transcript.last().unwrap() else {
        panic!("expected the report as an assistant message");
    };
    assert_eq!(text, "the auth flow starts in login.rs");
    assert_eq!(app.panes[0].activity(), "the auth flow starts in login.rs");
}

#[test]
fn the_report_is_not_duplicated_when_it_also_streamed() {
    let mut app = app_with_panes(1);
    app.handle_agent_event(AgentEvent::SubagentRunText {
        run: 0,
        text: "all done".to_string(),
    });
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: true,
        output: "all done".to_string(),
        steps_used: 1,
        error: None,
    });
    assert_eq!(app.panes[0].transcript.len(), 1);
}

#[test]
fn a_failed_run_shows_its_error_in_the_pane() {
    let mut app = app_with_panes(1);
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: false,
        output: String::new(),
        steps_used: 1,
        error: Some("provider is down".to_string()),
    });
    assert_eq!(app.panes[0].status, PaneStatus::Failed);
    let TranscriptEntry::Notice(text) = &app.panes[0].transcript[0] else {
        panic!("expected a notice");
    };
    assert!(text.contains("provider is down"));
}

#[test]
fn focus_rail_prefers_a_running_pane_over_a_finished_one() {
    let mut app = app_with_panes(2);
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: true,
        output: "done".to_string(),
        steps_used: 1,
        error: None,
    });
    // agent0 has finished; ↓ should land on the one still working.
    press(&mut app, KeyCode::Down);
    assert_eq!(app.rail_focus, Some(1));
}

#[test]
fn arrows_walk_from_one_pane_straight_into_the_next() {
    let mut app = app_with_panes(3);
    app.attach_pane(0);

    press(&mut app, KeyCode::Down);
    assert_eq!(app.attached, Some(1));
    press(&mut app, KeyCode::Down);
    assert_eq!(app.attached, Some(2));
    // Wraps rather than dead-ending at the last run.
    press(&mut app, KeyCode::Down);
    assert_eq!(app.attached, Some(0));

    press(&mut app, KeyCode::Up);
    assert_eq!(app.attached, Some(2));
    // Browsing runs never scrolls the one you passed through.
    assert!(app.panes.iter().all(|pane| pane.scroll == 0));
}

#[test]
fn shift_arrows_scroll_the_pane_you_are_reading() {
    let mut app = app_with_panes(3);
    app.attach_pane(1);
    // Pretend the last frame had room to scroll (renderer fills this).
    app.panes[1].max_scroll.set(100);

    press_mod(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
    press_mod(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
    assert_eq!(app.attached, Some(1), "shift+↑ must not change pane");
    assert!(!app.panes[1].scroll_follow, "scrolling up leaves the tail");
    assert_eq!(
        app.panes[1].scroll, 98,
        "top-anchored: two lines up from max"
    );

    press_mod(&mut app, KeyCode::Down, KeyModifiers::SHIFT);
    assert_eq!(app.panes[1].scroll, 99);
    assert!(!app.panes[1].scroll_follow);
}

#[test]
fn arrows_in_a_pane_scroll_it_instead_of_recalling_history() {
    let mut app = app_with_panes(1);
    app.history.push("an earlier prompt".to_string());
    app.attach_pane(0);
    app.panes[0].max_scroll.set(100);

    // The bug: ↑/↓ fell through to the composer and walked the main chat's
    // history while the user was plainly looking at a subagent.
    press(&mut app, KeyCode::Up);
    assert!(app.input.is_empty(), "↑ must not recall history in a pane");
    assert!(!app.panes[0].scroll_follow);
    assert_eq!(app.panes[0].scroll, 99);

    press(&mut app, KeyCode::Up);
    assert_eq!(app.panes[0].scroll, 98);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.panes[0].scroll, 99);
    // Pinned at the live tail; it cannot scroll past the bottom.
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    assert!(app.panes[0].scroll_follow, "reaching the bottom re-follows");
    assert_eq!(app.panes[0].scroll, 0);
    assert!(app.input.is_empty());
    assert_eq!(app.attached, Some(0));
}

#[test]
fn scroll_step_clamps_and_tracks_follow() {
    // Following the tail: scrolling up unsticks and moves off the bottom.
    assert_eq!(scroll_step(true, 0, 10, 3), (7, false));
    // Scrolling further up clamps at the oldest content.
    assert_eq!(scroll_step(false, 2, 10, 5), (0, false));
    // Scrolling down past the bottom clamps and re-enables follow.
    assert_eq!(scroll_step(false, 8, 10, -5), (0, true));
    // While following, scrolling down stays stuck to the bottom.
    assert_eq!(scroll_step(true, 0, 10, -1), (0, true));
}

#[test]
fn transcript_stays_put_while_streaming_after_scroll_up() {
    let mut app = app();
    // Viewport is full and we are following the live tail.
    app.transcript_max_scroll.set(50);
    assert!(app.scroll_follow);

    // User scrolls up to re-read earlier output.
    app.scroll_transcript(10);
    assert!(!app.scroll_follow);
    assert_eq!(app.scroll, 40);

    // Content grows (renderer would bump max_scroll); the top-anchored
    // offset must not change — that is the whole stick-to-bottom contract.
    app.transcript_max_scroll.set(80);
    assert_eq!(app.scroll, 40, "scroll offset holds while content grows");
    assert!(!app.scroll_follow);

    // Scrolling down to the (new) bottom re-enables follow.
    app.scroll_transcript(-100);
    assert!(app.scroll_follow);
    assert_eq!(app.scroll, 0);

    // Ctrl-End is the explicit jump-to-tail chord.
    app.scroll_transcript(5);
    assert!(!app.scroll_follow);
    app.scroll_to_bottom();
    assert!(app.scroll_follow);
    assert_eq!(app.scroll, 0);
}

#[test]
fn wheel_and_page_keys_drive_stick_to_bottom() {
    let mut app = app();
    app.transcript_max_scroll.set(30);

    press(&mut app, KeyCode::PageUp);
    assert!(!app.scroll_follow);
    assert_eq!(app.scroll, 20);

    // One PgDn of 10 lands exactly on the bottom and re-enables follow.
    press(&mut app, KeyCode::PageDown);
    assert!(
        app.scroll_follow,
        "PgDn onto the bottom should re-enable follow"
    );
    assert_eq!(app.scroll, 0);

    // Esc while scrolled away jumps to the tail (instead of clearing input).
    app.scroll_transcript(5);
    assert!(!app.scroll_follow);
    press(&mut app, KeyCode::Esc);
    assert!(app.scroll_follow);

    // Ctrl-End does the same.
    app.scroll_transcript(5);
    press_mod(&mut app, KeyCode::End, KeyModifiers::CONTROL);
    assert!(app.scroll_follow);
}

#[test]
fn esc_from_a_pane_lands_in_the_composer_in_one_press() {
    let mut app = app_with_panes(2);
    app.attach_pane(1);

    press(&mut app, KeyCode::Esc);
    assert_eq!(app.attached, None);
    // Focus is all the way back in the composer, not parked on the rail —
    // one Esc, and you are typing again.
    assert_eq!(app.rail_focus, None);
    press(&mut app, KeyCode::Char('h'));
    assert_eq!(app.input, "h");
}

#[test]
fn an_aborted_turn_closes_out_the_panes_it_left_running() {
    let mut app = app_with_panes(3);
    // The first run finished before the interrupt; the other two were still
    // streaming when the task was killed, so their loops were dropped
    // mid-poll and no SubagentRunDone is ever coming for them.
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: true,
        output: "report".to_string(),
        steps_used: 1,
        error: None,
    });

    app.fail_running_panes("interrupted");

    assert_eq!(
        app.running_panes(),
        0,
        "nothing is left pulsing on the rail"
    );
    assert_eq!(
        app.panes[0].status,
        PaneStatus::Done,
        "the finished one is untouched"
    );
    for pane in &app.panes[1..] {
        assert_eq!(pane.status, PaneStatus::Failed);
        assert!(pane.finished.is_some(), "so its linger clock can start");
    }

    // And they retire like any other finished run, instead of sitting on the
    // rail with a live clock for the rest of the session.
    for pane in &mut app.panes {
        pane.finished = Some(Instant::now() - PANE_LINGER - Duration::from_secs(1));
    }
    app.retire_finished_panes();
    assert!(app.panes.is_empty());
}

#[test]
fn the_ultra_pre_phase_leaves_its_drafts_in_the_transcript() {
    let mut app = app();
    app.handle_agent_event(AgentEvent::UltraGuidance {
        label: "ultra ×2 · implementer+skeptic · 1 judge".to_string(),
        guidance: "[Ultra] 2 agent(s)…\n\ndraft from the implementer".to_string(),
    });

    // The candidates' panes retire seconds after they finish, while the main
    // agent works on for minutes — so the card is the only place the drafts
    // the user paid 3× for can still be read.
    let card = app
        .transcript
        .iter()
        .find_map(|entry| match entry {
            TranscriptEntry::ToolCard {
                name,
                output,
                collapsed,
                ..
            } => Some((name, output, collapsed)),
            _ => None,
        })
        .expect("the guidance card");
    assert_eq!(card.0, "ultra ×2 · implementer+skeptic · 1 judge");
    assert!(
        card.1
            .as_deref()
            .is_some_and(|body| body.contains("draft from the implementer"))
    );
    assert!(*card.2, "folded: the answer is the point of the turn");
}

#[test]
fn a_finished_run_retires_off_the_rail() {
    let mut app = app_with_panes(2);
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: true,
        output: "report".to_string(),
        steps_used: 1,
        error: None,
    });
    // It lingers first, so you actually see it land.
    app.retire_finished_panes();
    assert_eq!(app.panes.len(), 2);

    // Once its linger is up it drops off, leaving the rail showing live work.
    app.panes[0].finished = Some(Instant::now() - PANE_LINGER - Duration::from_secs(1));
    app.retire_finished_panes();
    assert_eq!(app.panes.len(), 1);
    assert_eq!(app.panes[0].name, "agent1");
    assert_eq!(app.running_panes(), 1);
}

#[test]
fn the_pane_you_are_watching_never_retires_under_you() {
    let mut app = app_with_panes(1);
    app.attach_pane(0);
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: true,
        output: "report".to_string(),
        steps_used: 1,
        error: None,
    });
    app.panes[0].finished = Some(Instant::now() - PANE_LINGER - Duration::from_secs(1));

    // Long past its linger, but you are reading it.
    app.retire_finished_panes();
    assert_eq!(app.panes.len(), 1);
    assert_eq!(app.attached, Some(0));

    // Esc lets it go, and lands you back in the composer.
    press(&mut app, KeyCode::Esc);
    assert!(app.panes.is_empty());
    assert_eq!(app.attached, None);
    assert_eq!(app.rail_focus, None);
}

#[test]
fn retiring_keeps_the_selection_on_the_run_it_pointed_at() {
    let mut app = app_with_panes(3);
    // Focus the third run, then retire the first.
    app.rail_focus = Some(2);
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: true,
        output: "done".to_string(),
        steps_used: 1,
        error: None,
    });
    app.panes[0].finished = Some(Instant::now() - PANE_LINGER - Duration::from_secs(1));
    app.retire_finished_panes();

    // Indices shifted, but the selection still points at the same run.
    assert_eq!(app.panes.len(), 2);
    assert_eq!(app.rail_focus, Some(1));
    assert_eq!(app.panes[1].name, "agent2");
}

#[test]
fn a_background_report_survives_its_pane_retiring() {
    let mut app = app_with_panes(1);
    // The card the model got back when it delegated: a placeholder.
    app.transcript.push(TranscriptEntry::ToolCard {
        name: "spawn_subagent".to_string(),
        args: serde_json::json!({"subagent": "agent0", "task": "task 0"}),
        output: Some("Delegated to subagent 'agent0' (#0)".to_string()),
        is_error: false,
        collapsed: false,
    });
    app.handle_agent_event(AgentEvent::SubagentRunDone {
        run: 0,
        completed: true,
        output: "the auth flow starts in login.rs".to_string(),
        steps_used: 4,
        error: None,
    });
    app.panes[0].finished = Some(Instant::now() - PANE_LINGER - Duration::from_secs(1));
    app.retire_finished_panes();
    assert!(app.panes.is_empty());

    // The pane is gone, but the run is still readable in the main chat.
    let TranscriptEntry::ToolCard { output, .. } = &app.transcript[0] else {
        panic!("expected the spawn card");
    };
    assert_eq!(output.as_deref(), Some("the auth flow starts in login.rs"));
}

#[test]
fn the_composer_stays_live_while_attached() {
    let mut app = app_with_panes(1);
    app.attach_pane(0);
    // Better than a modal: you can keep driving the main conversation
    // while you watch a subagent work.
    press(&mut app, KeyCode::Char('h'));
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(app.input, "hi");
    assert_eq!(app.attached, Some(0));
}

#[test]
fn activity_reports_the_tool_in_flight_then_the_last_message() {
    let mut app = app_with_panes(1);
    // Nothing yet: fall back to the task.
    assert_eq!(app.panes[0].activity(), "task 0");

    app.handle_agent_event(AgentEvent::SubagentRunToolStarted {
        run: 0,
        name: "grep".to_string(),
        args: Value::Null,
    });
    assert_eq!(app.panes[0].activity(), "grep");

    app.handle_agent_event(AgentEvent::SubagentRunToolFinished {
        run: 0,
        name: "grep".to_string(),
        output: crate::tools::ToolOutput::ok("hit"),
    });
    app.handle_agent_event(AgentEvent::SubagentRunText {
        run: 0,
        text: "narrowing it down".to_string(),
    });
    assert_eq!(app.panes[0].activity(), "narrowing it down");
}

#[test]
fn bare_commands_parse_to_their_variants() {
    for (input, expected) in [
        ("/plan", SlashCommand::Plan),
        ("/todos", SlashCommand::Todos),
        ("/cost", SlashCommand::Cost),
        ("/compact", SlashCommand::Compact),
        ("/dashboard", SlashCommand::Dashboard),
        ("/omakase", SlashCommand::Omakase),
    ] {
        assert_eq!(SlashCommand::parse(input), Some(Ok(expected)), "{input}");
    }
}

#[test]
fn welcome_hides_while_a_turn_is_in_flight_and_returns_after() {
    let mut app = app();
    assert!(app.welcome_visible());

    app.status.busy = true;
    assert!(
        !app.welcome_visible(),
        "a running turn replaces the welcome"
    );
    app.status.busy = false;
    assert!(app.welcome_visible(), "an aborted turn brings it back");

    app.streaming = "partial".to_string();
    assert!(!app.welcome_visible(), "streamed text replaces the welcome");
    app.streaming.clear();
    assert!(app.welcome_visible());
}

#[test]
fn builtin_with_bad_args_still_dismisses_the_welcome_screen() {
    let mut app = app();
    assert!(app.welcome_visible());
    type_str(&mut app, "/mode warlock");
    press(&mut app, KeyCode::Enter);
    assert!(
        !app.welcome_visible(),
        "a mistyped builtin still begins the session"
    );
}
