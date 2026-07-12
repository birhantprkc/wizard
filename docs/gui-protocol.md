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
    { "kind": "user", "text": "..." },
    { "kind": "turn_marker", "turn": 2, "prompt": "..." },
    { "kind": "text", "text": "assistant narration" },
    { "kind": "tool", "name": "execute", "args": {}, "output": { "ok": true, "summary": "1 search, 1 file" } },
    { "kind": "notice", "text": "..." }
  ] }
```
Tool output in the replay may be summarized (first line / counts); the GUI renders
single-line tool rows and can expand. There is no `thinking` replay item: thinking is
not persisted to the session JSONL, so it exists only as live `thinking_delta` frames.

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
`max_steps` is `[gui] max_steps` — the GUI's own step budget, not the TUI's top-level one.

### PATCH /api/settings
`{ "max_steps": 100 }` → the same shape as `GET`.

### POST /api/providers
`{ "name": "…", "kind": "openai|anthropic|xai|openrouter|cloudflare|ollama|llamacpp",
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

### GET /api/git?cwd=/abs/path
```json
{ "branch": "feat/gui", "dirty": true, "additions": 734, "deletions": 7,
  "files": [{ "path": "src/gui/mod.rs", "status": "M|A|D|?", "additions": 10, "deletions": 2 }] }
```
From `git status --porcelain=v1 -b` + `git diff --numstat` (+ staged, + untracked counted
as additions, matching `git_diff_text` semantics; skip `.wizard/` paths).

### POST /api/git/commit
`{ "cwd": "...", "message": "..." }` → `{ "ok": true, "sha": "..." }`. Runs `git add -A && git commit`.

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
{ "type": "todo", "items": [{ "text": "...", "done": true, "active": false }] }
{ "type": "usage", "prompt_tokens": 123, "completion_tokens": 45 }
{ "type": "state", "state": "working|needs_input|idle|failed" }
{ "type": "plan_ready", "plan": "markdown" }
{ "type": "interview", "questions": ["..."] }
{ "type": "task_event", "phase": "started|finished", "label": "..." }
{ "type": "subagent", "phase": "started|finished", "name": "...", "task": "..." }
{ "type": "notice", "text": "..." }
{ "type": "error", "message": "..." }
{ "type": "retrying", "attempt": 2 }
{ "type": "done", "reason": "completed|cancelled|max_steps|error" }
```

`call_id` is a server-assigned monotonically increasing int per socket, pairing
started/finished frames. `summary` for tool_finished: short human line (file names,
counts, first output line) — GUI shows it muted next to the tool name; full output
not shipped in v1.

`done` reason `cancelled` means the client asked for the stop (a `cancel` frame); a
turn the agent stopped after emitting an `error` frame — e.g. a provider failure —
reports `error` instead, and the closing `state` frame is `failed` rather than `idle`
(matching the task's `/api/tasks` state until its next turn).

Client→server frames:
```json
{ "type": "user_message", "text": "...", "model": "optional override" }
{ "type": "cancel" }
{ "type": "plan_verdict", "approve": true, "feedback": "optional" }
{ "type": "interview_answers", "answers": ["..."] }   // or null to skip: { "answers": null }
```

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
  newTask → POST /api/tasks.
- Right-panel Progress card ← `todo` frames; Goal card ← task title + done/total of todos +
  usage tokens; Git card ← /api/git polled after each `tool_finished` that mutates files
  (or simple 3s poll while a turn is live).
