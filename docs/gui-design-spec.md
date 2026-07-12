# Wizard GUI — Design Spec

Target look: reference screenshot at
`/home/gradient/.claude/image-cache/83725a5a-d06f-43f8-8deb-b25461061e9a/1.png`
(read it with the Read tool — it renders as an image).

A dark, rounded, three-pane desktop-style agent workspace. Overall feel: Claude-Code-style
task manager + conversation + git/plan side panel. Everything lives on a near-black canvas
with slightly lighter raised cards and 10–14px corner radii.

## Global

- Canvas: `#0d0d0f` (near-black). Cards/panels: `#161619` to `#1c1c20`. Hairline borders `#2a2a2e`.
- Text: primary `#e8e8ea`, secondary/muted `#8a8a90`, faint `#5a5a60`.
- Accent blue `#3b82f6` (send button, selected-task dot), green `#22c55e` (+ diffstat, check icons),
  red `#ef4444` (- diffstat), amber/strikethrough gray for completed items.
- Font: system UI sans (Inter-like), 13–14px base; monospace for commands/code.
- Rounded cards everywhere; no hard 90° panels. Subtle 1px borders, no drop shadows except composer.

## Layout (3 columns)

```
+-------------+--------------------------------------+------------------+
|  Sidebar    |  Conversation (center, fluid)        |  Context panel   |
|  ~240px     |                                      |  ~300px          |
+-------------+--------------------------------------+------------------+
```

Top bar spans center+right: sidebar toggle + chat title (truncated, bold) + repo chip
(`gomoku-ai` with folder icon) + branch chip (`upgrade/v3.0` with branch icon), and the
context-panel toggle on the right.

Every control in the chrome does something. The reference design's decorative bits — macOS
traffic lights (the real window already has them, on macOS), back/forward arrows, notes and
terminal buttons, an attach button, a settings gear — are not drawn: a control that looks
clickable and isn't is worse than no control.

## Left sidebar (~240px)

1. Header: folder icon + the directory `wizard gui` runs in — where a new chat opens.
2. Action row (icon + label, hover highlight): `New Chat` (plus-in-square icon, `⌘N` /
   `Ctrl-N` shortcut hint right-aligned, matching the platform).
3. `Chats` section header (muted, small caps feel).
4. Chat tree grouped by workspace/repo (folder icon + name, e.g. `gomoku-ai`, `zcode-website`,
   `zcode-desktop`), each with indented rows:
   - single-line truncated title (e.g. "Create an intelligent Go…")
   - right-aligned muted relative age (`2m`, `9m`, `14m`, `27m`, `51m`, `1h`, `2h`, `5h`)
   - selected row: lighter pill background + small blue dot on the left of the title

## Center: conversation

- The user's prompt renders as a full-width rounded quote card at top (lighter bg `#232327`).
- `Worked for 3m 1s ⌄` collapsible section header (muted) with hairline rule.
- Agent narration: plain paragraphs of body text.
- Tool-call rows, inline with icons, muted single-line summaries:
  - `⌕ Explored  1 search, 1 file  Failed` (label bold-ish, args muted, status in gray strikethrough-ish)
  - `Ran  git status --short  Failed` (command in monospace, muted)
  - `✎ Wrote  index.html  app.js  styles.css  +733` (file chips with filetype icons, green diffstat)
- Streaming text continues below; content area scrolls, fading under the composer.
- Composer (bottom, floating rounded-2xl card with border):
  - placeholder `Ask wizard to change something`
  - bottom row: `✦ Sovereign` mode chip (static — wizard has no permission gating, so there is
    no mode dropdown) · spacer · spinner / stop button · `GLM-5.2 ⌄` model picker ·
    circular blue send button `↑` (right).

## Right context panel (~300px), stack of rounded cards

1. **Git tools** card:
   - header `Git tools` (muted small)
   - row: `⊞ Changes` … right-aligned `+734` (green) `-7` (red)
   - row: `⎇ feat/gomoku-ai` (current branch, static)
   - row: `-o- Commit ⌄` — expands the commit-message editor
2. **Goal** card:
   - header row: `Goal` … right-aligned status `Complete` (muted)
   - `◎` target icon + goal text ("Gomoku vs. AI — implement computer moves with a heuristic algorithm")
   - meta line, muted: `5/5 · 2m · 89K tokens`
3. **Progress** card:
   - header `Progress`
   - checklist: green circled-check icon + item text; completed items are struck through and dimmed.
     5 items in the reference (e.g. "Initialize board, piece rendering, and the 15×15 grid layout").

## Behavior to wire (backend-dependent, confirm against survey)

- Sidebar chats = wizard sessions on disk, grouped by workspace/repo, sorted by recency.
- New Chat: opens an empty session in the directory `wizard gui` runs in and focuses the
  composer; the first message starts the first turn and names the chat. On launch the GUI lands
  in the newest chat of that directory, or a new one when it has none.
- Tool calls stream as structured rows (explore/run/write) rather than raw text where possible.
- Composer sends follow-up user messages to the running session; model picker lists configured providers/models.
- Git card: live diffstat of the task's workspace, current branch, commit action.
- Goal/Progress: map to wizard's plan/todo state if available (plan.md / todo tool), else hide gracefully.
