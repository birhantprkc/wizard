# Wizard GUI — Wire Protocol

The GUI is served by `wizard gui [--port N] [--no-open]`: an HTTP server on
`127.0.0.1:<port>` (default 4680, fail if taken unless --port given) serving the embedded
static assets from `gui/assets/` (plus `/favicon.ico`, an embedded SVG) and the JSON API
below. All state interaction is JSON. No auth (localhost only, bind 127.0.0.1 strictly),
but every request — the WebSocket upgrade included — passes a drive-by guard: the `Host`
header must be loopback (`127.0.0.1`, `localhost`, or `[::1]`, optional port), and an
`Origin` header, when present, must be a local `http(s)` page; anything else is 403.
This blocks DNS rebinding against the HTTP API and hostile web pages opening the task
WebSocket (WS upgrades are not subject to CORS). Requests without an `Origin` (curl,
same-origin navigations) pass on the Host check alone.

"Task" = a wizard session (one `~/.wizard/sessions/<id>.jsonl` file). The GUI groups tasks
by workspace (the session header `cwd`, displayed as its basename).

## HTTP endpoints

### GET /api/tasks
List tasks for the sidebar. Merge `session::summaries()` with the live registry
(`~/.wizard/running/*.json`) for state.
```json
[{ "id": "2026-07-11T09-12-33", "title": "first user prompt, truncated",
   "cwd": "/home/gradient/projects/ai/wizard", "workspace": "wizard",
   "updated_unix": 1783500000, "state": "working|needs_input|idle|done|failed" }]
```
Sorted by `updated_unix` desc. `title` = first user prompt of the session.

### GET /api/tasks/{id}
Full transcript replay for the center pane, mapped from session JSONL:
```json
{ "id": "...", "cwd": "...", "workspace": "...", "model": "...",
  "items": [
    { "kind": "user", "text": "...", "images": [{ "path": "...", "mime": "image/png", "bytes": 51234 }] },
    { "kind": "turn_marker", "turn": 2, "prompt": "..." },
    { "kind": "text", "text": "assistant narration" },
    { "kind": "tool", "name": "execute", "args": {}, "output": { "ok": true, "summary": "1 search, 1 file" } },
    { "kind": "images", "source": "assistant|tool", "tool": "render", "images": [{ "path": "...", "mime": "image/png", "bytes": 51234 }] },
    { "kind": "notice", "text": "..." }
  ] }
```
Tool output in the replay may be summarized (first line / counts); the GUI renders
single-line tool rows and can expand. There is no `thinking` replay item: thinking is
not persisted to the session JSONL, so it exists only as live `thinking_delta` frames.

An `images` item is the `images` frame of the same turn, rebuilt from disk: the session
file records each image's path on the message that carried it, so a reloaded transcript
shows what the live stream showed, in the same places. The model's own images follow its
`text`; a tool's follow *that tool's* `tool` item — which is not necessarily the last one,
since one assistant message can make several calls. The message a tool's images ride back
to the model on (`role: user`, "Image(s) returned by `x`:") is not a prompt and is not
replayed as one — that label is what tells it apart from a person attaching an image to a
prompt of their own, which can land in exactly the same place after an interrupted turn.

A `user` item's `images` are what the *user* attached (uploaded, or `@shot.png`), echoed on
the message they were attached to; the field is omitted when there were none. Non-image
attachments get no echo of their own: `@file` expansion inlined their contents into `text`,
which is what was actually sent to the model.

`session_start` hook output is persisted as a system note but is **not** replayed: it is
context written for the model, not conversation (the TUI drops it the same way when it
reloads a transcript). The hook still shows as its one-line `hook session_start: appended
context (…)` notice while it fires.

### POST /api/tasks
`{ "cwd": "/abs/path (optional)", "prompt": "... (optional)",
   "model": "provider-or-model-name (optional)" }`
→ `201 { "id": "...", "cwd": "/abs/path", "workspace": "wizard" }`. Creates the
session. Without `cwd` it opens in the directory `wizard gui` was launched from —
this is what the GUI's "New Chat" posts (an empty body). With a `prompt` the first
turn starts immediately, and the client should open the WebSocket right away to catch
the stream (the server buffers events from turn start until the first WS attach, then
replays them); without one the chat opens empty and the first `user_message` frame
starts it.

### GET /api/workspace
The directory the server runs in — where a new chat opens by default.
`{ "cwd": "/abs/path", "name": "wizard" }`

### GET /api/workspaces
Directories a chat can be opened in — the cwds of every known session plus the server's
own — for the topbar's folder chip. Directories that no longer exist are omitted.
`[{ "cwd": "...", "name": "wizard", "task_count": 12, "home": true }]`

### GET /api/models
`{ "active": "anthropic", "providers": [{ "name": "anthropic", "kind": "anthropic",
   "model": "claude-fable-5", "models": ["...", "..."] }] }`
`models` from `LlmProvider::list_models()` where cheap; empty array is fine (picker
then shows just the configured model). Read per request, not cached: a provider added
in Settings shows up without a restart.

### POST /api/tasks/{id}/upload
`multipart/form-data` with one or more `file` parts → the paths a `user_message` may
then attach:
```json
{ "attachments": [
  { "path": "/home/u/.wizard/images/<session>/ab12cd34.png",
    "name": "screenshot.png", "mime": "image/png", "bytes": 51234, "kind": "image" },
  { "path": "/home/u/.wizard/attachments/<session>/spec.pdf",
    "name": "spec.pdf", "mime": "application/octet-stream", "bytes": 8100, "kind": "file" }
] }
```
Task-scoped, not a bare `/api/upload`, because both stores are session-scoped: an image
has to land in *this* session's image directory, since `GET /api/image` serves nothing
from anywhere else.

`kind` is decided by the server from the file's **bytes** (`llm::sniff_mime`) — never
from the client's content-type and never from the extension. An image (png/jpeg/gif/webp)
goes through `Image::from_bytes` (which enforces the 10 MB cap) into the
content-addressed image store; anything else is written to
`~/.wizard/attachments/<session>/` under a sanitized basename (traversal, separators and
whitespace folded to `_` — a space would split the `@/abs/path` token the turn references
it by). A PDF named `x.png` and labelled `image/png` still lands in attachments as
`kind: "file"`. `mime` on a non-image is a cosmetic label for the composer chip; nothing
is decided by it.

### GET /api/commands?cwd=/abs/path
The composer's slash menu. **Derived from `commands::COMMANDS`** — the one table the TUI
completes and dispatches from — plus the custom commands loaded for that workspace. There is
no second list: a built-in the GUI advertises is one the GUI runs, and a built-in the TUI has
is one this menu shows.

```json
{ "commands": [
  { "name": "model",  "detail": "pick or switch the model", "where": "server", "args": "[tag]" },
  { "name": "goal",   "detail": "show or set the standing mission goal", "where": "server", "args": "[text]" },
  { "name": "diff",   "detail": "toggle the git diff sidebar", "where": "client" },
  { "name": "vim",    "detail": "toggle vim-style modal editing of the input line", "where": "unavailable" },
  { "name": "review", "detail": "review the diff", "where": "prompt", "args": "<args>" }
] }
```
(An excerpt — every built-in appears, in the TUI's display order.)

`where` says who executes it:
- `server` → send it as a `command` frame; the server applies it to the Agent and answers
  with `notice` / `context` / `transcript_reset` / `error` frames.
- `client` → the page's own: a panel, an overlay, a list. Nothing to ask the server for, and
  a `command` frame for one comes back as an `error` saying so.
- `unavailable` → terminal-only (`/vim`, `/quit`, `/exit`). Listed so the menu can say what
  it is and why it is not here, rather than pretend it never existed. Render it disabled with
  `detail` as the reason; sending it anyway is answered with an honest `error`.
- `prompt` → a custom command from `.wizard/commands/*.md`. It expands to prompt text, so
  the client sends it as an ordinary `user_message` and the **server** expands it through
  `commands::preprocess`. The client never expands one — that is the same pipeline the TUI
  runs, and having two would mean two behaviors.

Two commands are `client` here though the TUI runs them against the agent, and for the same
reason in both cases — **a GUI task is keyed by its session id**:

- `clear` — `Agent::clear` rotates the session file, so clearing server-side would leave
  `GET /api/tasks/{id}` replaying the session the agent had just stopped writing to, and a
  reload would lose every turn taken after the clear. Where the conversation and the file are
  the same object, clearing one means starting the other.
- `resume` — the task list *is* the session picker.

The rest of the `client` set (`diff`, `todos`, `subagents`, `dashboard`, `settings`,
`provider`, `login`) are windows the page owns; the server has no hand on them.

`/mode` no longer takes `plan`. It never did in the TUI — plan is a posture on top of a mode,
not a mode — and `/plan` and `/omakase` now toggle it here as they do there. A client that
hardcoded `/mode plan` must send `/plan`.

## Settings

Every write re-reads `~/.wizard/config.toml` first and mutates *that* — the TUI and other
GUI servers write the same file, and `Config::save` rewrites it whole, so a stale in-memory
copy must never be what lands on disk.

### GET /api/settings
```json
{ "first_run": false, "config_path": "…/.wizard/config.toml",
  "credentials_path": "…/.wizard/credentials.toml", "active": "anthropic", "max_steps": 100,
  "providers": [{ "name": "anthropic", "kind": "anthropic", "base_url": "…", "model": "…",
                  "key": "stored|env|oauth|not_needed|missing", "active": true }],
  "presets": [{ "name": "anthropic", "label": "Anthropic", "kind": "anthropic", "base_url": "…",
                "model": "…", "needs_key": true, "needs_base_url": false, "hint": "…" }] }
```
`first_run` = no provider configured: the GUI onboards instead of opening a chat.

`max_steps` is the top-level `max_steps` — the one budget every surface runs on. `0` is
the default and means **no limit**: the turn ends when the model stops calling tools. The
GUI used to keep a `[gui] max_steps` of its own; it does not, because it is the same agent
as the TUI and not a reduced one. A config still carrying the old `[gui]` section loads
fine (the section is ignored, and not written back).

### PATCH /api/settings
`{ "max_steps": 0 }` → the same shape as `GET`. `0` (no limit) or 1–1000; anything higher
is a 400.

## Subscription sign-in (OAuth)

An API key is a string the user can paste; a subscription is not. One sign-in may be in
flight at a time — a person signs in to one account at a time, and a second attempt replaces
the first rather than racing it.

### POST /api/login/{provider}
`provider` is `xai` or `chatgpt`. → `{ "authorize_url": "…" }` — start the flow. The browser
opens that URL (from a window opened *synchronously* on the click, or browsers block it).

The redirect never comes back to this server. A provider only sends the browser to the
loopback address registered with its client id — `http://localhost:1455/auth/callback`
(fallback 1457) for OpenAI's Codex client, `http://127.0.0.1:56121/callback` for xAI's — and
ignores any other `redirect_uri`. So each flow binds *its own* listener, waits for the
browser on it in a spawned task, and writes the provider (active) when the exchange lands.
The GUI only watches `GET /api/login`. A `state` that does not match the one minted at the
start is refused before any exchange, and `?error=access_denied` (the user said no) is
reported, not swallowed.

Those ports are the only addresses the providers will redirect to, so a sign-in cannot be
moved elsewhere and an occupied one fails the `POST` rather than hanging. Which makes the
port scarce: a second `POST` **replaces** the sign-in in flight — cancelling it, waiting for
its listener to close, and rebinding — so closing the provider's tab and clicking sign in
again works immediately, rather than colliding with the abandoned flow for five minutes. The
replaced flow is silent; only the sign-in in flight writes `done`/`failed`.

The terminal flows (`wizard --login xai|chatgpt`) bind the same listeners the same way.

### GET /api/login
`{ "state": "idle|pending|done|failed", "provider": "xai", "error": "…" }` — what the
sign-in in flight is doing. The tab the user *started* from polls this, because the tab they
*finish* in is the provider's, which lands on the flow's private callback listener. Every
failure — a denied consent, a timeout, a token exchange that 400s — ends in `failed`, so the
polling tab never waits forever.

### POST /api/providers
`{ "name": "…", "kind": "openai|anthropic|xai|xaioauth|chatgptoauth|openrouter|cloudflare|ollama|llamacpp",
   "base_url": "…", "model": "…", "api_key": "… (optional)", "activate": true }`
→ `{ "settings": {…}, "probe": { "ok": true, "models": ["…"] } }`

Reusing a name is an edit. `api_key` is stored in `~/.wizard/credentials.toml` (0600) under
the provider's name; omit it on an edit to keep the stored key. The provider is saved even
when the probe fails — a bad key should leave an editable row, not vanish.

### POST /api/providers/{name}/test
→ `{ "ok": false, "error": "…", "models": [] }`. Builds the client and lists models.

### POST /api/providers/{name}/active
Switch the active provider → the `GET /api/settings` shape.

### DELETE /api/providers/{name}
Forget the provider and its stored key; removing the active one hands `active` to a
survivor. → the `GET /api/settings` shape.

### GET /api/image?path=/home/u/.wizard/images/&lt;session&gt;/&lt;hash&gt;.png
The bytes of one image the agent wrote — what an `images` frame's `path` names. Returns
the file with the `Content-Type` sniffed from its own magic number (never the extension,
never anything the client said) and `Cache-Control: public, max-age=31536000, immutable`:
the file name is the hash of its bytes, so that URL can never come to mean anything else.

`path` is client input and is resolved against `~/.wizard/images/` and nothing else: the
name must end in `.png`/`.jpg`/`.webp`/`.gif`, and the path must canonicalize — `..`
segments, symlinks and all — to a regular file really inside that directory. A traversal,
an absolute path elsewhere on the disk, a symlink in the store pointing out of it, and a
file that is not an image are all 400. A file that is simply gone is 404, which the GUI
renders as a broken-image tile naming the path rather than an empty box.

### GET /api/git?cwd=/abs/path
```json
{ "branch": "feat/gui", "dirty": true, "additions": 734, "deletions": 7,
  "files": [{ "path": "src/gui/mod.rs", "status": "M|A|D|?", "additions": 10, "deletions": 2 }] }
```
From `git status --porcelain=v1 -b` + `git diff --numstat` (+ staged, + untracked counted
as additions, matching `git_diff_text` semantics; skip `.wizard/` paths).

### GET /api/git/diff?cwd=/abs/path&path=src/gui/mod.rs
```json
{ "path": "src/gui/mod.rs", "status": "M", "additions": 10, "deletions": 2,
  "binary": false, "truncated": false,
  "hunks": [{ "header": "@@ -1,4 +1,6 @@ fn main()",
              "lines": [{ "kind": "ctx|add|del|meta", "text": "+    let x = 1;" }] }] }
```
One changed file as the working tree stands against HEAD — staged *and* unstaged, so the
diff matches the `+N -M` `GET /api/git` reports for it. Untracked files diff against
`/dev/null` (all additions); binary files set `binary` and carry no hunks; a change with no
lines in it (a mode, a rename) is honestly empty. `text` keeps git's leading marker.

`path` is only ever a path `GET /api/git` itself just listed for this workspace — anything
else is a 400, so nothing the client sends becomes a git argument.

### GET /api/git/branches?cwd=/abs/path
`{ "current": "feat/gui", "branches": ["feat/gui", "main", "..."] }` — local branches, most
recently committed first. `current` is null on a detached HEAD.

### POST /api/git/checkout
`{ "cwd": "...", "branch": "main", "create": false, "task": "<id> (optional)" }`
→ `{ "ok": true, "branch": "main" }`. `create` means `git checkout -b`.

Refused (400) while `task` has a turn running — the agent is mid-edit in that working tree.
Git's own refusals (uncommitted changes the switch would overwrite) come back as the error
text: no force-checkout, no stash the user did not ask for.

## WebSocket /api/tasks/{id}/ws

One socket per open task. Server→client frames mirror `AgentEvent`:

```json
{ "type": "text_delta", "text": "..." }
{ "type": "thinking_delta", "text": "..." }
{ "type": "tool_started", "call_id": 7, "name": "read_file", "args": { } }
{ "type": "tool_finished", "call_id": 7, "name": "read_file", "ok": true, "summary": "src/app.rs (120 lines)" }
{ "type": "images", "source": "assistant|tool", "tool": "generate_image", "images": [{ "path": "/home/u/.wizard/images/<session>/c414cd0e204d.png", "mime": "image/png", "bytes": 51234 }] }
{ "type": "todo", "items": [{ "text": "...", "done": true, "active": false }] }
{ "type": "usage", "prompt_tokens": 123, "completion_tokens": 45 }
{ "type": "context", "tokens": 24310, "window": 200000 }
{ "type": "state", "state": "working|needs_input|idle|failed" }
{ "type": "plan_ready", "plan": "markdown" }
{ "type": "interview", "questions": ["..."] }
{ "type": "task_event", "phase": "started|finished", "label": "..." }
{ "type": "subagent", "phase": "started|finished", "name": "...", "task": "..." }
{ "type": "subagent_run_started", "run": 3, "bg": 1, "name": "researcher", "task": "..." }
{ "type": "subagent_run_text", "run": 3, "text": "one of its own messages" }
{ "type": "subagent_run_tool_started", "run": 3, "call_id": 12, "name": "read_file", "args": { } }
{ "type": "subagent_run_tool_finished", "run": 3, "call_id": 12, "name": "read_file", "ok": true, "summary": "src/app.rs (120 lines)" }
{ "type": "subagent_run_images", "run": 3, "source": "tool", "tool": "generate_image", "images": [{ "path": "...", "mime": "image/png", "bytes": 51234 }] }
{ "type": "subagent_run_step", "run": 3, "step": 2 }
{ "type": "subagent_run_done", "run": 3, "completed": true, "output": "its report", "steps_used": 4, "error": null }
{ "type": "notice", "text": "..." }
{ "type": "error", "message": "..." }
{ "type": "retrying", "attempt": 2 }
{ "type": "done", "reason": "completed|cancelled|max_steps|error" }
```

`call_id` is a server-assigned monotonically increasing int per socket, pairing
started/finished frames. `summary` for tool_finished: short human line (file names,
counts, first output line) — GUI shows it muted next to the tool name; full output
not shipped in v1.

### Images

An `images` frame says the turn produced one or more images and where the agent wrote them:
`~/.wizard/images/<session-id>/<content-hash>.<ext>`. `source` is `"assistant"` (the model
generated them itself) or `"tool"`, in which case `tool` names the tool that returned them
and the frame follows that tool's `tool_finished`. `subagent_run_images` is the same frame
scoped to a run, for that run's pane.

The frame carries a *reference*, never the bytes: `path`, `mime`, `bytes`. The base64 stays
in the model's history, where a vision model needs it; a frame that embedded it would put
megabytes into the replay buffer of every turn. The client displays the image by fetching
the file (`GET /api/image?path=…`, which is also what "open full size" opens). The path is
stable — it is the hash of the image's own bytes — and it is also recorded on the message in
the session file, so a transcript replayed from disk shows the same images without
re-deriving anything.

A tool's images carry no `call_id`: they arrive immediately after that tool's
`tool_finished`, and the client puts them on the card it just drew. Their place in the turn
is what identifies them, which is why nothing may be emitted between the two.

### Usage vs context

Two numbers, two frames, and they are not the same number.

`usage` is **session-lifetime**: every model call adds to it, and it only ever grows. It is
what `/cost` bills on.

`context` is what will load into the **next** model call — the number the TUI's status bar
shows. It is emitted on every `Usage` event (carrying that call's `prompt_tokens`, which is
exactly the context the next call inherits) and on `ContextSize`, which fires after the
history *shrank*: compaction, `/clear`. So `context` falls; `usage` cannot.

Conflating the two — showing a lifetime total on a context meter — is the bug the TUI fixed
in 0ed201b. `window` is the active provider's context window for the active model, and it is
**omitted** when the provider does not know one (a local llama.cpp that will not say): a
meter with no ceiling is honest, an invented ceiling is not. It is re-read when `/model`
switches the model.

### Subagent runs

`subagent` is the one-liner a background delegation shows in the chat. The `subagent_run_*`
frames are the run itself: one subagent's own messages and tool calls, streamed live, which
the GUI lists in the context panel and opens as that run's own pane.

Every frame after `subagent_run_started` carries the same `run` — a session-unique id — so
concurrent runs (even two of the same subagent) demux into separate panes instead of
interleaving. `bg` is the background-registry id when the run was detached, and null when
the parent turn is blocked on it. Tool calls pair by `call_id` within their run, and their
`summary` is built exactly like the parent's tool cards'.

`subagent_run_done` distinguishes the three endings: `completed: true` (it reported back),
`completed: false` with `error: null` (it spent its step budget), and `error` set (it died).
`output` is its final report — the step that made no tool call, which therefore never
streamed as a `subagent_run_text`.

A run's lifecycle is not the turn's: a background subagent outlives the turn that spawned
it and keeps streaming after that turn's `done`/`state` frames. So subagent state is not
cleared at turn start, and a run still going when a client attaches is announced to it —
after the replay, and only when the replay does not already carry its `started` frame.

`done` reason `cancelled` means the client asked for the stop (a `cancel` frame); a
turn the agent stopped after emitting an `error` frame — e.g. a provider failure —
reports `error` instead, and the closing `state` frame is `failed` rather than `idle`
(matching the task's `/api/tasks` state until its next turn).

Client→server frames:
```json
{ "type": "user_message", "text": "...", "model": "optional override",
  "images": ["/home/u/.wizard/images/<session>/ab12cd34.png"],
  "files":  ["/home/u/.wizard/attachments/<session>/spec.pdf"] }
{ "type": "cancel" }
{ "type": "plan_verdict", "approve": true, "feedback": "optional" }
{ "type": "interview_answers", "answers": ["..."] }   // or null to skip: { "answers": null }
{ "type": "command", "name": "compact", "args": "" }
```

### user_message

Every `user_message` runs through `commands::preprocess(text, custom, cwd)` server-side —
the one pipeline the TUI and headless runs use. That is what gives the GUI `@file`
references and custom `.wizard/commands/*.md` commands, identically and for free. The
client sends what the user typed; it expands nothing.

`images` (from the upload route) are attached to the user message for the vision path.
`files` are appended to the text as `@/abs/path` tokens, so the `@file` expansion reads
them — there is no second file-reading path in the GUI.

Both lists are **re-verified** server-side against the same canonicalize-and-contain check
`GET /api/image` uses: an image path must resolve inside `~/.wizard/images/`, a file path
inside `~/.wizard/attachments/`. A path the client sends is client input no matter which
route first produced it; taken on trust it is an arbitrary-file read, and — once
`@`-expanded into the prompt — a way to exfiltrate whatever it named. A path outside the
stores is an `error` frame and no turn runs.

### command

A `where: "server"` command from `GET /api/commands`, applied to the live Agent. It takes
the same slot a turn does (both need `&mut Agent`), so one sent mid-turn comes back as
`error` "turn in progress" rather than queuing behind it.

The arguments are parsed by `SlashCommand::parse` — the parser the TUI's prompt uses — so an
argument means here exactly what it means there, and a bad one is rejected in the same words.

The server answers with the frames the protocol already has; there is no command-reply frame:

| command | answers with |
|---|---|
| `compact` | `notice` (what the pass did), `context` (the history is smaller now) |
| `cost` | `notice`: session totals, plus an estimate when the provider carries rates |
| `model <tag>` | `notice`; the context window is re-read for the new model |
| `mode <genie\|sovereign>` | `notice`. Plan mode survives the switch — it is a stance on top of a mode, not one of them |
| `genie`, `sovereign` | `notice` (the `/mode` aliases) |
| `effort <low\|medium\|high\|default>` | `notice` |
| `plan`, `omakase` | `notice`; the next turn investigates read-only and presents a plan through the `plan_ready` gate |
| `goal [text]` | `notice`: the standing mission (`<cwd>/.wizard/mission.toml`), or the one just set |
| `status` | `notice`: model, provider, mode, effort, step budget, session, usage, context, tasks, todos, plan |
| `memory`, `doctor`, `bashes`, `agents` | `notice` |
| `reload` | `notice`: skills, scripted tools, and MCP servers, re-registered against the one shared manager |
| `rewind` | `notice` listing the turns there is something to go back to |
| `rewind <turn>` | **`transcript_reset`** (see below), then `notice` (what was restored) and `context` |
| `fusion` | `notice`; every turn now runs through the panel. `fusion config` is refused: the panel editor is a TUI picker |
| `server [status\|start\|stop]` | `notice`; download and load progress arrive as further notices |
| `evolve [--deep] <desc>`, `publish [branch]` | `notice` at the start and at the end. Both run in the command's own slot, so the task reads `working` until they land |
| `help` | `notice`, derived from the same table this menu is |
| a `client` command | `error`: the page runs it, not the server |
| `vim`, `quit`, `exit` | `error`: what the command is, and why a browser is not where it runs |
| anything unknown | `error` |

### transcript_reset

```json
{ "type": "transcript_reset", "turn": 7 }
```

`/rewind <turn>` truncated the conversation: every turn from `turn` on is gone from the
session file, and the transcript the page has rendered is now a record of turns that no
longer exist. The client must **discard its rendered transcript and re-fetch
`GET /api/tasks/{id}`**, which reads the truncated session back. The session file is the only
copy of the history — this is the frame that says it changed under the client's feet.

It arrives before the `notice` describing what was restored, so a client that re-fetches on
sight of it and then appends the notice ends up with both.

Rules:
- One in-flight turn per task; `user_message` during a turn → `error` frame "turn in progress".
- `PlanReady`/`Interview` gates: forward frame, hold the oneshot, resolve on the matching
  client frame; if socket drops, auto-approve plan / skip interview (gateway behavior).
- On WS attach mid-turn, server first replays buffered frames of the current turn. The
  replay normally opens with the turn's own `state` frame (and carries every later
  transition), in which case no extra snapshot `state` frame follows; otherwise — idle
  attach, or a runaway turn whose buffer dropped its head — the server appends one
  `state` frame with the current state.
- Server keeps `Agent` instances in an in-process manager keyed by task id (LRU keep-warm;
  rebuild via `agent::build_headless_agent(config, cwd, resume_id)` on demand).

## Frontend mapping

api.js `RealApi` implements the seam in `gui-design-spec.md` against this protocol:
- listTasks → GET /api/tasks; getTask → GET /api/tasks/{id}; streamTask → the WS;
  sendMessage → WS user_message; gitStatus → GET /api/git; listModels → GET /api/models;
  newTask → POST /api/tasks; upload → POST /api/tasks/{id}/upload;
  listCommands → GET /api/commands; runCommand → WS command.
- Right-panel Progress card ← `todo` frames; Goal card ← task title + done/total of todos +
  usage tokens; Git card ← /api/git polled after each `tool_finished` that mutates files
  (or simple 3s poll while a turn is live).
- The context meter ← `context` frames, and *not* the Goal card's lifetime token count.
  They are different numbers (see "Usage vs context").
- `MockApi` (`?mock=1`) must cover every one of these: it is the only headless way to drive
  the GUI, so a feature it cannot exercise is a feature nobody can check in a browser.
