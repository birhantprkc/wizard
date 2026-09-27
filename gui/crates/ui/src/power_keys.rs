//! Power user mode: the vim-style navigation keymap.
//!
//! [`BINDINGS`] is the one table behind both the gpui keymap
//! ([`key_bindings`]) and the `?` cheat sheet ([`sheet`]), so the sheet can
//! never list a key that does nothing. Every binding is scoped to the
//! [`CONTEXT`] key context, which only the shell's navigation focus target
//! carries. Text fields (the message box, palette searches, the terminal, file
//! editors) never have it on their context stack, so a bare `j` typed into
//! one of them is text, not navigation.

use gpui::{Action, KeyBinding, KeyContext};

/// Key context the navigation focus target carries while power user mode is
/// on. Its `area` value says which pane has the navigation cursor.
pub const CONTEXT: &str = "PowerNav";

gpui::actions!(
    power_nav,
    [
        Down,
        Up,
        Top,
        Bottom,
        HalfPageDown,
        HalfPageUp,
        Open,
        Yank,
        Archive,
        Delete,
        Filter,
        FocusLeft,
        FocusRight,
        FocusUp,
        FocusDown,
        FocusNext,
        FocusComposer,
        NextTab,
        PrevTab,
        NewChat,
        PickAgent,
        PickModel,
        PickEffort,
        PickDevice,
        Settings,
        Palette,
        CheatSheet,
        NextSession,
        PrevSession,
        TogglePanelSidebar,
        TogglePanelRight,
        TogglePanelTerminal,
        Back,
    ]
);

/// Select the right panel's tab at this zero-based position.
#[derive(Clone, PartialEq, gpui::Action)]
#[action(namespace = power_nav, no_json)]
pub struct SelectTab(pub usize);

/// Where the navigation cursor lives. `Global` bindings work in every area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Area {
    Global,
    Sidebar,
    Transcript,
    RightPanel,
}

impl Area {
    /// The `area` value in the key context.
    pub fn key(self) -> &'static str {
        match self {
            Area::Global => "global",
            Area::Sidebar => "sidebar",
            Area::Transcript => "transcript",
            Area::RightPanel => "right",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Area::Global => "Anywhere",
            Area::Sidebar => "Session list",
            Area::Transcript => "Transcript",
            Area::RightPanel => "Right panel",
        }
    }
}

/// What a binding does. One variant per action, so the table stays data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Down,
    Up,
    Top,
    Bottom,
    HalfPageDown,
    HalfPageUp,
    Open,
    Yank,
    Archive,
    Delete,
    Filter,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    FocusNext,
    FocusComposer,
    NextTab,
    PrevTab,
    SelectTab(usize),
    NewChat,
    PickAgent,
    PickModel,
    PickEffort,
    PickDevice,
    Settings,
    Palette,
    CheatSheet,
    NextSession,
    PrevSession,
    ToggleSidebar,
    ToggleRight,
    ToggleTerminal,
    Back,
}

impl Command {
    pub fn action(self) -> Box<dyn Action> {
        match self {
            Command::Down => Box::new(Down),
            Command::Up => Box::new(Up),
            Command::Top => Box::new(Top),
            Command::Bottom => Box::new(Bottom),
            Command::HalfPageDown => Box::new(HalfPageDown),
            Command::HalfPageUp => Box::new(HalfPageUp),
            Command::Open => Box::new(Open),
            Command::Yank => Box::new(Yank),
            Command::Archive => Box::new(Archive),
            Command::Delete => Box::new(Delete),
            Command::Filter => Box::new(Filter),
            Command::FocusLeft => Box::new(FocusLeft),
            Command::FocusRight => Box::new(FocusRight),
            Command::FocusUp => Box::new(FocusUp),
            Command::FocusDown => Box::new(FocusDown),
            Command::FocusNext => Box::new(FocusNext),
            Command::FocusComposer => Box::new(FocusComposer),
            Command::NextTab => Box::new(NextTab),
            Command::PrevTab => Box::new(PrevTab),
            Command::SelectTab(ix) => Box::new(SelectTab(ix)),
            Command::NewChat => Box::new(NewChat),
            Command::PickAgent => Box::new(PickAgent),
            Command::PickModel => Box::new(PickModel),
            Command::PickEffort => Box::new(PickEffort),
            Command::PickDevice => Box::new(PickDevice),
            Command::Settings => Box::new(Settings),
            Command::Palette => Box::new(Palette),
            Command::CheatSheet => Box::new(CheatSheet),
            Command::NextSession => Box::new(NextSession),
            Command::PrevSession => Box::new(PrevSession),
            Command::ToggleSidebar => Box::new(TogglePanelSidebar),
            Command::ToggleRight => Box::new(TogglePanelRight),
            Command::ToggleTerminal => Box::new(TogglePanelTerminal),
            Command::Back => Box::new(Back),
        }
    }
}

/// One key sequence in gpui keymap syntax (`"g g"`, `"ctrl-w h"`,
/// `"shift-g"`), the area it works in, and the cheat sheet's line for it.
#[derive(Debug, Clone, Copy)]
pub struct Binding {
    pub keys: &'static str,
    pub area: Area,
    pub command: Command,
    pub description: &'static str,
}

const fn b(keys: &'static str, area: Area, command: Command, description: &'static str) -> Binding {
    Binding {
        keys,
        area,
        command,
        description,
    }
}

use Area::{Global, RightPanel, Sidebar, Transcript};

pub const BINDINGS: &[Binding] = &[
    // Anywhere
    b("?", Global, Command::CheatSheet, "Show or hide this sheet"),
    b(":", Global, Command::Palette, "Command palette"),
    b(
        "i",
        Global,
        Command::FocusComposer,
        "Type in the message box",
    ),
    b(
        "h",
        Global,
        Command::FocusLeft,
        "Focus the pane to the left",
    ),
    b(
        "l",
        Global,
        Command::FocusRight,
        "Focus the pane to the right",
    ),
    b(
        "ctrl-w h",
        Global,
        Command::FocusLeft,
        "Focus the pane to the left",
    ),
    b(
        "ctrl-w l",
        Global,
        Command::FocusRight,
        "Focus the pane to the right",
    ),
    b("ctrl-w k", Global, Command::FocusUp, "Focus the transcript"),
    b(
        "ctrl-w j",
        Global,
        Command::FocusDown,
        "Focus the message box",
    ),
    b(
        "ctrl-w w",
        Global,
        Command::FocusNext,
        "Cycle through panes",
    ),
    b("c", Global, Command::NewChat, "New chat"),
    b("space n", Global, Command::NewChat, "New chat"),
    b("space a", Global, Command::PickAgent, "Switch agent"),
    b("space m", Global, Command::PickModel, "Switch model"),
    b(
        "space e",
        Global,
        Command::PickEffort,
        "Switch reasoning effort",
    ),
    b(
        "space d",
        Global,
        Command::PickDevice,
        "Switch device (new chat)",
    ),
    b("space ,", Global, Command::Settings, "Settings"),
    b(
        "space b",
        Global,
        Command::ToggleSidebar,
        "Show or hide the sidebar",
    ),
    b(
        "space r",
        Global,
        Command::ToggleRight,
        "Show or hide the right panel",
    ),
    b(
        "space t",
        Global,
        Command::ToggleTerminal,
        "Show or hide the terminal",
    ),
    b("shift-j", Global, Command::NextSession, "Next session"),
    b("shift-k", Global, Command::PrevSession, "Previous session"),
    b("g t", Global, Command::NextTab, "Next right panel tab"),
    b(
        "g shift-t",
        Global,
        Command::PrevTab,
        "Previous right panel tab",
    ),
    b(
        "q",
        Global,
        Command::Back,
        "Close settings, back to the chat",
    ),
    // Session list
    b("j", Sidebar, Command::Down, "Next session"),
    b("down", Sidebar, Command::Down, "Next session"),
    b("k", Sidebar, Command::Up, "Previous session"),
    b("up", Sidebar, Command::Up, "Previous session"),
    b("g g", Sidebar, Command::Top, "First session"),
    b("shift-g", Sidebar, Command::Bottom, "Last session"),
    b("enter", Sidebar, Command::Open, "Open session"),
    b("o", Sidebar, Command::Open, "Open session"),
    b(
        "x",
        Sidebar,
        Command::Archive,
        "Archive session (asks first)",
    ),
    b(
        "d d",
        Sidebar,
        Command::Archive,
        "Archive session (asks first)",
    ),
    b(
        "shift-d",
        Sidebar,
        Command::Delete,
        "Delete session (asks first)",
    ),
    b("/", Sidebar, Command::Filter, "Search sessions"),
    // Transcript
    b("j", Transcript, Command::Down, "Next message"),
    b("down", Transcript, Command::Down, "Next message"),
    b("k", Transcript, Command::Up, "Previous message"),
    b("up", Transcript, Command::Up, "Previous message"),
    b(
        "ctrl-d",
        Transcript,
        Command::HalfPageDown,
        "Half page down",
    ),
    b("ctrl-u", Transcript, Command::HalfPageUp, "Half page up"),
    b("g g", Transcript, Command::Top, "First message"),
    b("shift-g", Transcript, Command::Bottom, "Last message"),
    b(
        "o",
        Transcript,
        Command::Open,
        "Expand or collapse tool card",
    ),
    b(
        "enter",
        Transcript,
        Command::Open,
        "Expand or collapse tool card",
    ),
    b("y", Transcript, Command::Yank, "Copy message or code block"),
    b("/", Transcript, Command::Filter, "Search sessions"),
    // Right panel
    b("enter", RightPanel, Command::Open, "Focus the open tab"),
    b("1", RightPanel, Command::SelectTab(0), "Go to tab 1 to 9"),
    b("2", RightPanel, Command::SelectTab(1), "Go to tab 1 to 9"),
    b("3", RightPanel, Command::SelectTab(2), "Go to tab 1 to 9"),
    b("4", RightPanel, Command::SelectTab(3), "Go to tab 1 to 9"),
    b("5", RightPanel, Command::SelectTab(4), "Go to tab 1 to 9"),
    b("6", RightPanel, Command::SelectTab(5), "Go to tab 1 to 9"),
    b("7", RightPanel, Command::SelectTab(6), "Go to tab 1 to 9"),
    b("8", RightPanel, Command::SelectTab(7), "Go to tab 1 to 9"),
    b("9", RightPanel, Command::SelectTab(8), "Go to tab 1 to 9"),
    b("j", RightPanel, Command::NextTab, "Next tab"),
    b("k", RightPanel, Command::PrevTab, "Previous tab"),
];

/// Composer keys the power mode adds, listed on the sheet only (they are
/// handled by the message box itself, not the navigation keymap).
pub const COMPOSER_KEYS: &[(&str, &str)] = &[("Esc", "Leave the message box for navigation")];

/// Vim keys in the message box, for the sheet when vim editing is on.
pub const VIM_KEYS: &[(&str, &str)] = &[
    ("Esc", "Normal mode"),
    (
        "i a I A o O",
        "Insert before, after, line start, line end, new line",
    ),
    (
        "h j k l  w b e  0 ^ $",
        "Move by char, line, word, line edge",
    ),
    ("gg G  f t F T  ; ,", "Top, bottom, find char, repeat find"),
    (
        "x D C s S r J ~",
        "Delete char, to end, change, replace, join",
    ),
    ("d c y > <  + motion", "Operators; dd cc yy >> << for lines"),
    ("iw aw i\" i( i[ i{ ...", "Text objects: ciw, da(, yi\""),
    ("p P  u Ctrl-r  .", "Paste, undo, redo, repeat last change"),
    ("v V", "Visual and visual line; then y d c > <"),
    ("3w 2dd", "Counts"),
    ("Enter", "Send (in normal mode)"),
];

/// The gpui predicate for an area's bindings.
pub fn predicate(area: Area) -> String {
    match area {
        Area::Global => CONTEXT.to_string(),
        area => format!("{CONTEXT} && area == {}", area.key()),
    }
}

/// Key context for the navigation focus target.
pub fn key_context(area: Area) -> KeyContext {
    let mut context = KeyContext::default();
    context.add(CONTEXT);
    context.set("area", area.key());
    context
}

/// Every binding in [`BINDINGS`] as a gpui key binding.
pub fn key_bindings() -> Vec<KeyBinding> {
    BINDINGS
        .iter()
        .map(|binding| {
            let predicate = predicate(binding.area);
            KeyBinding::load(
                binding.keys,
                binding.command.action(),
                Some(
                    gpui::KeyBindingContextPredicate::parse(&predicate)
                        .expect("static predicate parses")
                        .into(),
                ),
                false,
                None,
                &gpui::DummyKeyboardMapper,
            )
            .expect("static binding parses")
        })
        .collect()
}

/// How the sheet writes a key sequence: `g g` as `gg`, `shift-g` as `G`,
/// `ctrl-w h` as `Ctrl-w h`, `space n` as `Space n`.
pub fn display_keys(keys: &str) -> String {
    let parts: Vec<String> = keys.split(' ').map(display_stroke).collect();
    // Plain letter runs read as one word (gg, dd, gt), the way vim docs write them.
    if parts.len() > 1 && parts.iter().all(|p| p.chars().count() == 1) {
        parts.concat()
    } else {
        parts.join(" ")
    }
}

fn display_stroke(stroke: &str) -> String {
    if let Some(rest) = stroke.strip_prefix("shift-")
        && rest.chars().count() == 1
    {
        return rest.to_uppercase();
    }
    if let Some(rest) = stroke.strip_prefix("ctrl-") {
        return format!("Ctrl-{rest}");
    }
    match stroke {
        "space" => "Space".into(),
        "enter" => "Enter".into(),
        "escape" => "Esc".into(),
        "down" => "\u{2193}".into(),
        "up" => "\u{2191}".into(),
        other => other.into(),
    }
}

/// One cheat-sheet group: a title and (keys, description) lines. Bindings
/// that share an area and description are merged onto one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SheetGroup {
    pub title: &'static str,
    pub rows: Vec<(String, &'static str)>,
}

pub fn sheet(vim: bool) -> Vec<SheetGroup> {
    let mut groups = Vec::new();
    for area in [Global, Sidebar, Transcript, RightPanel] {
        let mut rows: Vec<(String, &'static str)> = Vec::new();
        for binding in BINDINGS.iter().filter(|b| b.area == area) {
            let keys = display_keys(binding.keys);
            if let Some(row) = rows.iter_mut().find(|(_, d)| *d == binding.description) {
                row.0 = format!("{}  {}", row.0, keys);
            } else {
                rows.push((keys, binding.description));
            }
        }
        // A run of digits reads as a range: `1-9`, not nine keys.
        for row in &mut rows {
            let parts: Vec<&str> = row.0.split("  ").collect();
            if parts.len() > 2
                && parts
                    .iter()
                    .all(|p| p.len() == 1 && p.as_bytes()[0].is_ascii_digit())
            {
                row.0 = format!("{}-{}", parts[0], parts[parts.len() - 1]);
            }
        }
        groups.push(SheetGroup {
            title: area.title(),
            rows,
        });
    }
    groups.push(SheetGroup {
        title: "Message box",
        rows: COMPOSER_KEYS
            .iter()
            .map(|(k, d)| ((*k).to_string(), *d))
            .collect(),
    });
    if vim {
        groups.push(SheetGroup {
            title: "Vim editing",
            rows: VIM_KEYS
                .iter()
                .map(|(k, d)| ((*k).to_string(), *d))
                .collect(),
        });
    }
    groups
}

/// Bindings that can follow a pending prefix in `area`, as (remaining keys,
/// description), for the which-key hint that appears after `Space`, `g`,
/// `d` or `Ctrl-w`.
pub fn continuations(prefix: &[String], area: Area) -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = Vec::new();
    for binding in BINDINGS
        .iter()
        .filter(|b| b.area == Area::Global || b.area == area)
    {
        let strokes: Vec<&str> = binding.keys.split(' ').collect();
        if strokes.len() <= prefix.len()
            || !strokes
                .iter()
                .zip(prefix)
                .all(|(a, b)| stroke_eq(a, b.as_str()))
        {
            continue;
        }
        let rest = display_keys(&strokes[prefix.len()..].join(" "));
        if !out
            .iter()
            .any(|(k, d)| *k == rest && *d == binding.description)
        {
            out.push((rest, binding.description));
        }
    }
    out
}

fn stroke_eq(binding: &str, typed: &str) -> bool {
    binding == typed
}

/// The panes navigation moves between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Sidebar,
    Transcript,
    Composer,
    RightPanel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
    Next,
}

/// Where a focus move lands. The layout is sidebar | transcript over
/// composer | right panel; hidden panes are skipped, and a move with nowhere
/// to go stays put.
pub fn move_focus(from: Pane, dir: Dir, sidebar_open: bool, right_open: bool) -> Pane {
    use Pane::*;
    match (from, dir) {
        (Transcript | Composer, Dir::Left) if sidebar_open => Sidebar,
        (RightPanel, Dir::Left) => Transcript,
        (Sidebar, Dir::Right) => Transcript,
        (Transcript | Composer, Dir::Right) if right_open => RightPanel,
        (Transcript, Dir::Down) => Composer,
        (Composer, Dir::Up) => Transcript,
        (Sidebar | RightPanel, Dir::Down) => Composer,
        (Sidebar | RightPanel, Dir::Up) => Transcript,
        (from, Dir::Next) => {
            let order = [Sidebar, Transcript, Composer, RightPanel];
            let at = order.iter().position(|p| *p == from).unwrap_or(0);
            (1..=order.len())
                .map(|step| order[(at + step) % order.len()])
                .find(|p| match p {
                    Sidebar => sidebar_open,
                    RightPanel => right_open,
                    _ => true,
                })
                .unwrap_or(from)
        }
        (from, _) => from,
    }
}

impl Pane {
    /// The keymap area navigation keys use while this pane has the cursor.
    /// The composer has no area: it is a text field.
    pub fn area(self) -> Option<Area> {
        match self {
            Pane::Sidebar => Some(Area::Sidebar),
            Pane::Transcript => Some(Area::Transcript),
            Pane::RightPanel => Some(Area::RightPanel),
            Pane::Composer => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Keymap, Keystroke};

    fn strokes(keys: &str) -> Vec<Keystroke> {
        keys.split(' ')
            .map(|k| Keystroke::parse(k).expect("stroke parses"))
            .collect()
    }

    fn overlaps(a: Area, b: Area) -> bool {
        a == b || a == Area::Global || b == Area::Global
    }

    #[test]
    fn no_two_bindings_collide_in_one_area() {
        for (i, a) in BINDINGS.iter().enumerate() {
            for b in &BINDINGS[i + 1..] {
                if !overlaps(a.area, b.area) {
                    continue;
                }
                let (sa, sb) = (strokes(a.keys), strokes(b.keys));
                let n = sa.len().min(sb.len());
                assert_ne!(
                    &sa[..n],
                    &sb[..n],
                    "{:?} ({}) and {:?} ({}) are the same keys or one is a prefix of the other",
                    a.keys,
                    a.area.key(),
                    b.keys,
                    b.area.key()
                );
            }
        }
    }

    #[test]
    fn every_binding_parses_and_loads() {
        assert_eq!(key_bindings().len(), BINDINGS.len());
        for binding in BINDINGS {
            assert!(!binding.description.is_empty());
        }
    }

    fn stack(contexts: &[&str]) -> Vec<KeyContext> {
        contexts
            .iter()
            .map(|c| KeyContext::parse(c).expect("context parses"))
            .collect()
    }

    fn matched(keymap: &Keymap, keys: &str, contexts: &[KeyContext]) -> (Vec<String>, bool) {
        let (bindings, pending) = keymap.bindings_for_input(&strokes(keys), contexts);
        (
            bindings
                .iter()
                .map(|b| b.action().name().to_string())
                .collect(),
            pending,
        )
    }

    #[test]
    fn navigation_keys_fire_on_the_navigation_target() {
        let keymap = Keymap::new(key_bindings());
        let nav = |area: Area| {
            let mut s = stack(&["Root"]);
            s.push(key_context(area));
            s
        };
        assert_eq!(
            matched(&keymap, "j", &nav(Area::Sidebar)).0,
            vec!["power_nav::Down"]
        );
        assert_eq!(
            matched(&keymap, "j", &nav(Area::RightPanel)).0,
            vec!["power_nav::NextTab"]
        );
        assert_eq!(
            matched(&keymap, "?", &nav(Area::Transcript)).0,
            vec!["power_nav::CheatSheet"]
        );
        // A prefix waits for the next key instead of firing.
        assert_eq!(matched(&keymap, "g", &nav(Area::Sidebar)), (vec![], true));
        assert_eq!(
            matched(&keymap, "g g", &nav(Area::Sidebar)).0,
            vec!["power_nav::Top"]
        );
        assert_eq!(
            matched(&keymap, "space n", &nav(Area::Transcript)).0,
            vec!["power_nav::NewChat"]
        );
        // Area-scoped keys stay in their area.
        assert_eq!(
            matched(&keymap, "x", &nav(Area::Transcript)),
            (vec![], false)
        );
    }

    #[test]
    fn navigation_keys_are_ignored_while_a_text_field_has_focus() {
        let keymap = Keymap::new(key_bindings());
        // Every text-entry context in the app: the message box, generic
        // inputs, palette searches, the terminal, the browser, file editors.
        for field in [
            "MessageComposer",
            "Composer",
            "PaletteSearch",
            "Terminal",
            "Browser",
            "Input",
        ] {
            let contexts = stack(&["Root", field]);
            for binding in BINDINGS {
                let (hits, pending) = matched(&keymap, binding.keys, &contexts);
                assert!(hits.is_empty(), "{:?} fired inside {field}", binding.keys);
                let first = binding.keys.split(' ').next().unwrap();
                assert_eq!(
                    matched(&keymap, first, &contexts),
                    (vec![], false),
                    "{first:?} started a sequence inside {field}"
                );
                let _ = pending;
            }
        }
    }

    #[test]
    fn every_command_has_a_binding_and_a_sheet_line() {
        let commands = [
            Command::Down,
            Command::Up,
            Command::Top,
            Command::Bottom,
            Command::HalfPageDown,
            Command::HalfPageUp,
            Command::Open,
            Command::Yank,
            Command::Archive,
            Command::Delete,
            Command::Filter,
            Command::FocusLeft,
            Command::FocusRight,
            Command::FocusUp,
            Command::FocusDown,
            Command::FocusNext,
            Command::FocusComposer,
            Command::NextTab,
            Command::PrevTab,
            Command::SelectTab(0),
            Command::NewChat,
            Command::PickAgent,
            Command::PickModel,
            Command::PickEffort,
            Command::PickDevice,
            Command::Settings,
            Command::Palette,
            Command::CheatSheet,
            Command::NextSession,
            Command::PrevSession,
            Command::ToggleSidebar,
            Command::ToggleRight,
            Command::ToggleTerminal,
            Command::Back,
        ];
        let sheet = sheet(false);
        for command in commands {
            let binding = BINDINGS
                .iter()
                .find(|b| b.command == command)
                .unwrap_or_else(|| panic!("{command:?} has no binding"));
            assert!(
                sheet
                    .iter()
                    .flat_map(|g| &g.rows)
                    .any(|(_, d)| *d == binding.description),
                "{command:?} is missing from the sheet"
            );
        }
    }

    #[test]
    fn sheet_merges_aliases_and_adds_vim_only_when_on() {
        let groups = sheet(false);
        let sidebar = groups.iter().find(|g| g.title == "Session list").unwrap();
        assert!(
            sidebar
                .rows
                .contains(&("j  \u{2193}".into(), "Next session"))
        );
        assert!(sidebar.rows.contains(&("gg".into(), "First session")));
        assert!(sidebar.rows.contains(&("G".into(), "Last session")));
        assert!(
            sidebar
                .rows
                .contains(&("x  dd".into(), "Archive session (asks first)"))
        );
        assert!(!groups.iter().any(|g| g.title == "Vim editing"));
        let right = groups.iter().find(|g| g.title == "Right panel").unwrap();
        assert!(right.rows.contains(&("1-9".into(), "Go to tab 1 to 9")));
        assert!(sheet(true).iter().any(|g| g.title == "Vim editing"));
    }

    #[test]
    fn display_keys_reads_like_vim_docs() {
        assert_eq!(display_keys("g g"), "gg");
        assert_eq!(display_keys("g shift-t"), "gT");
        assert_eq!(display_keys("shift-g"), "G");
        assert_eq!(display_keys("ctrl-w h"), "Ctrl-w h");
        assert_eq!(display_keys("space n"), "Space n");
        assert_eq!(display_keys("space ,"), "Space ,");
        assert_eq!(display_keys("ctrl-d"), "Ctrl-d");
        assert_eq!(display_keys("?"), "?");
    }

    #[test]
    fn continuations_list_what_can_follow_a_prefix() {
        let space = continuations(&["space".into()], Area::Sidebar);
        assert!(space.contains(&("n".into(), "New chat")));
        assert!(space.contains(&(",".into(), "Settings")));
        let g = continuations(&["g".into()], Area::Sidebar);
        assert!(g.contains(&("g".into(), "First session")));
        assert!(g.contains(&("t".into(), "Next right panel tab")));
        assert!(g.contains(&("T".into(), "Previous right panel tab")));
        // `dd` only exists in the session list.
        assert!(continuations(&["d".into()], Area::Transcript).is_empty());
        assert_eq!(
            continuations(&["d".into()], Area::Sidebar),
            vec![("d".into(), "Archive session (asks first)")]
        );
    }

    #[test]
    fn focus_moves_follow_the_layout() {
        use Dir::*;
        use Pane::*;
        let cases = [
            // (from, dir, sidebar open, right open, expected)
            (Transcript, Left, true, true, Sidebar),
            (Transcript, Left, false, true, Transcript),
            (Transcript, Right, true, true, RightPanel),
            (Transcript, Right, true, false, Transcript),
            (Sidebar, Right, true, false, Transcript),
            (RightPanel, Left, true, true, Transcript),
            (Transcript, Down, true, true, Composer),
            (Composer, Up, true, true, Transcript),
            (Composer, Left, true, true, Sidebar),
            (Composer, Right, true, true, RightPanel),
            (Sidebar, Down, true, true, Composer),
            (RightPanel, Up, true, true, Transcript),
            (Sidebar, Left, true, true, Sidebar),
            (RightPanel, Right, true, true, RightPanel),
            (Sidebar, Next, true, true, Transcript),
            (Transcript, Next, true, true, Composer),
            (Composer, Next, true, true, RightPanel),
            (RightPanel, Next, true, true, Sidebar),
            (Composer, Next, false, false, Transcript),
            (Transcript, Next, false, false, Composer),
        ];
        for (from, dir, sidebar, right, expected) in cases {
            assert_eq!(
                move_focus(from, dir, sidebar, right),
                expected,
                "{from:?} {dir:?} sidebar={sidebar} right={right}"
            );
        }
    }
}
