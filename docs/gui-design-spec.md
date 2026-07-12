# Wizard GUI — Design Spec

A dark, three-pane agent workspace: chat list, conversation, git/goal rail. It began as a
copy of a reference screenshot (`~/.claude/image-cache/83725a5a-…/1.png`); what it is now is
described below, and where the two disagree, this wins.

The thing it should feel like is an instrument, not a product page. That means: dense but
breathing, hairlines instead of boxes inside boxes, no colour that is not carrying meaning,
and no sentence of copy that is not load-bearing. A settings screen made of eight cards, each
with a tagline under it and a blue button at the bottom, is the failure mode — it reads as
filler, because it is.

## Global

- Canvas `#0c0c0e`. Surfaces `#141416` / `#191a1d`. Hairlines `#26262a`, and `#1f1f23` for
  separators *inside* a surface (a section divider should be felt, not seen).
- Text: primary `#ececee`, mid `#b6b6bd`, muted `#86868e`, faint `#5c5c64`.
- **No brand hue.** Emphasis is brightness, not colour: the active state is simply lighter,
  and the one primary button per view inverts to light-on-dark (`#ececee` on `#0c0c0e`).
  Colour is reserved for meaning — green `#3fb96a` (additions), red `#e5484d` (deletions,
  errors), amber `#d8a13a` (a state needing attention, e.g. a provider with no key). If a
  pixel is coloured, it is saying something.
- **Type is bundled, not hoped for**: Inter (UI) and JetBrains Mono (literals), variable-weight
  latin subsets under the OFL, embedded in the binary and served from `/fonts/`. The system
  fallback on a plain Linux box is DejaVu Sans, and it shows. Tabular figures throughout, so
  ages, token counts and diffstats do not shift width as they tick.
- **Sans for prose, mono for literals.** A path, model tag, provider kind, base URL, branch
  name, directory or config location is a thing you could paste into a terminal — it is set in
  mono. Everything else is Inter: 13px UI, 14px transcript body.
- Section labels are 10.5px uppercase, letterspaced, faint. Same label in the sidebar
  (`CHATS`), the rail (`GIT TOOLS`), and Settings (`PROVIDERS`) — one idiom, used everywhere.
- Radii 6/10/14px. One filled button per view; every other action is an outline button or a
  plain text action that only gains a background on hover.

## Layout (3 columns)

```
+-------------+--------------------------------------+------------------+
|  Sidebar    |  Conversation (center, fluid)        |  Context panel   |
|  ~240px     |                                      |  ~300px          |
+-------------+--------------------------------------+------------------+
```

Top bar spans center+right: sidebar toggle + chat title (truncated, bold) + repo chip
(`gomoku-ai`, folder icon) + branch chip (`upgrade/v3.0`, branch icon), and the
context-panel toggle on the right.

Both chips are dropdowns, and both act:
- **Repo chip** → the directories wizard knows about (plus a field for any absolute path);
  picking one opens a **new chat** there. A chat's working directory is fixed when its
  session is created — it is written into the session file, and it is where everything the
  chat has already run took effect — so this cannot retroactively move the open chat, and
  does not pretend to.
- **Branch chip** → the workspace's local branches (most recent first) plus a field to create
  one. Picking one is a real `git checkout` in that working tree. It is refused while the
  agent is working (it is mid-edit in those files), and git's own refusal — uncommitted
  changes the switch would overwrite — is shown verbatim rather than forced through.

Every control in the chrome does something. The reference design's decorative bits — macOS
traffic lights (the real window already has them, on macOS), back/forward arrows, notes and
terminal buttons, an attach button, a settings gear — are not drawn: a control that looks
clickable and isn't is worse than no control.

## Left sidebar (~240px)

1. Header: folder icon + the directory `wizard gui` runs in — where a new chat opens —
   with the Settings gear on the right.
2. Action row (icon + label, hover highlight): `New Chat` (plus-in-square icon, `⌘N` /
   `Ctrl-N` shortcut hint right-aligned, matching the platform).
3. `Chats` section header (muted, small caps feel).
4. Chat tree grouped by workspace/repo (folder icon + name, e.g. `gomoku-ai`, `zcode-website`,
   `zcode-desktop`), each with indented rows:
   - single-line truncated title (e.g. "Create an intelligent Go…")
   - right-aligned muted relative age (`2m`, `9m`, `14m`, `27m`, `51m`, `1h`, `2h`, `5h`)
   - selected row: lighter background + a small dot on the left of the title (bright while
     the agent is working there, amber when it needs input, red when it failed)

## Center: conversation

- **Your messages are bubbles**: right-aligned, hugging their own text (max 78% of the
  column), rounded with the corner nearest the composer clipped. What the *agent* says is not
  a bubble — it is long-form prose interleaved with tool rows, and boxing it would fight with
  them. The asymmetry is the point: one side is speech, the other is work.
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
    no mode dropdown) · spacer · `GLM-5.2 ⌄` model picker · the send button (right).
  - **The send button is the stop button.** Idle it is a light-on-dark `↑`; while the agent is
    working it becomes a square with a ring turning around it, and pressing it cancels the
    turn. One control, in the place your hand already is — and it doubles as the "something is
    running" indicator, so no idle spinner sits around reading as "loading forever".

## Settings and onboarding (one sheet, one list shape)

Both are the same surface: a sheet with a hairline-separated stack of blocks. No cards inside
it, no grid of tiles, no tagline under anything.

- The **provider list** is the one list shape, used twice: to show what is configured
  (`xai` · `xaioauth · grok-4.5 · signed in`, active marked by a light rule down its left
  edge, actions as quiet text on the right) and, one step in, to pick what to add (provider
  name, its endpoint host in mono, right-aligned). A provider is a name and where it points;
  that is all a row says.
- **Onboarding** opens instead of a chat when no provider is configured — there is nothing to
  send a message to yet. Pick → one short form (model, API key, base URL where it matters) →
  save, probe, chat. "Skip" is available and honest about the consequence. `wizard login xai`
  (OAuth) is a terminal flow; such a provider simply appears here once it exists.
- **Settings** (gear, sidebar header) manages the same providers afterwards: which is active,
  test, edit, remove, add — plus the GUI's step limit. Each row states where its key comes
  from (stored / from env / signed in / local / none), so "why is it 401ing" is answerable
  from the page. The config path sits in the footer, in mono, because that is where the truth
  lives.
- A provider that fails its probe is still saved: a typo'd key should leave an editable row,
  not vanish.

## Right context rail (~300px)

A rail against the window edge — a hairline and groups of rows, not a card floating in space
with dead air beneath it.

1. **Git tools** group:
   - label `GIT TOOLS`
   - row: `⊞ Changes` … right-aligned `+734` (green) `-7` (red)
   - row: `⎇ feat/gomoku-ai` (current branch, static)
   - row: `-o- Commit ⌄` — expands the commit-message editor
2. **Goal** group:
   - label `GOAL` … right-aligned status `Complete` (muted)
   - `◎` target icon + goal text ("Gomoku vs. AI — implement computer moves with a heuristic algorithm")
   - meta line, muted: `5/5 · 2m · 89K tokens`
3. **Progress** group:
   - label `PROGRESS`
   - checklist: green circled-check icon + item text; completed items are struck through and dimmed.
     5 items in the reference (e.g. "Initialize board, piece rendering, and the 15×15 grid layout").

## Behavior to wire (backend-dependent, confirm against survey)

- Sidebar chats = wizard sessions on disk, grouped by workspace/repo, sorted by recency.
- New Chat: opens an empty session in the directory `wizard gui` runs in and focuses the
  composer; the first message starts the first turn and names the chat. On launch the GUI lands
  in the newest chat of that directory, or a new one when it has none.
- Tool calls stream as structured rows (explore/run/write) rather than raw text where possible.
- Composer sends follow-up user messages to the running session; the model picker reloads
  `/api/models` each time it opens (providers change, local backends come up) and offers
  "Manage providers…" when there is nothing to pick.
- Chats run sovereign (no terminal to prompt at) on `[gui] max_steps`, which Settings edits.
- Git card: live diffstat of the task's workspace, current branch, commit action.
- Goal/Progress: map to wizard's plan/todo state if available (plan.md / todo tool), else hide gracefully.
