// Wizard GUI — data seam.
//
// Two implementations of one interface:
//   `RealApi` — HTTP + one WebSocket per open task against the `wizard gui`
//               JSON API (docs/gui-protocol.md).
//   `MockApi` — wizard-flavored fixture data so the shell can be developed
//               and reviewed without a backend. Select with `?mock=1`.
// app.js only talks to this surface; `createApi()` picks the implementation.
//
// Streaming model: `streamTask(id, callbacks)` attaches a listener set for a
// task. All later server-pushed events for that task — including responses to
// `sendMessage` — are delivered through those callbacks, exactly like a single
// WebSocket subscription per task. `close()` unsubscribes. The real backend
// replays the buffered frames of an in-flight turn on every (re)attach; the
// first `working` state frame after `onOpen` marks the start of that replay.

/**
 * @typedef {Object} TaskSummary
 * @property {string} id            Stable task/session id.
 * @property {string} title         One-line task title (may be truncated by the UI).
 * @property {number} updatedAt     Epoch ms of last activity (UI renders relative age).
 * @property {'working'|'needs_input'|'complete'|'idle'|'failed'} status
 */

/**
 * @typedef {Object} Workspace
 * @property {string} name          Repo/workspace display name (directory basename).
 * @property {string} path          Absolute path of the workspace on disk.
 * @property {TaskSummary[]} tasks  Tasks in the workspace, most recent first.
 */

/**
 * @typedef {Object} WorkspaceRef
 * @property {string} cwd   Absolute path of the directory the server runs in.
 * @property {string} name  Its basename, for display.
 */

/**
 * @typedef {Object} FileRef
 * @property {string} name          File name shown on the chip.
 * @property {string} [path]        Workspace-relative path (tooltip / open action).
 */

/**
 * One tool row, either replayed (`type:'tool'` transcript item) or streamed
 * (`onToolCall`). `tool` is the display bucket; `name` the wire tool name.
 * @typedef {Object} ToolCall
 * @property {string} id            Correlates with a later ToolResult.callId.
 * @property {string} [name]        Raw wire tool name (`read_file`, ...).
 * @property {'explore'|'run'|'write'|'search'|'delegate'|'other'|string} tool
 * @property {string} title         Row label ("Explored", "Ran", "Wrote", ...).
 * @property {string} [noun]        Aggregation noun ("file", "search", ...).
 * @property {string} [detail]      Muted summary ("1 search, 1 file").
 * @property {string} [command]     Monospace command text (tool === 'run').
 * @property {FileRef[]} [files]    File chips (tool === 'write').
 * @property {{additions?: number, deletions?: number}} [diffstat]
 * @property {'pending'|'ok'|'failed'} [status]
 */

/**
 * @typedef {Object} ToolResult
 * @property {string} callId
 * @property {'ok'|'failed'} status
 * @property {string} [summary]
 */

/**
 * @typedef {Object} TaskStatus
 * @property {'connecting'|'working'|'needs_input'|'complete'|'idle'|'failed'} state
 * @property {string} [elapsedLabel]  Human label, e.g. "3m 1s".
 */

/**
 * One image the agent wrote to `~/.wizard/images/<session>/`, as an `images`
 * frame (or a replayed `images` item) names it: a reference, never the bytes.
 * The page fetches the file itself from `imageUrl()`.
 * @typedef {Object} ImageRef
 * @property {string} path   Absolute path of the file the agent wrote.
 * @property {string} mime   Media type, e.g. `image/png`.
 * @property {number} bytes  Size of the file on disk.
 */

/**
 * Images one turn produced, and where they came from.
 * @typedef {Object} ImageBatch
 * @property {'assistant'|'tool'} source  The model produced them, or a tool returned them.
 * @property {string|null} tool           The tool that returned them (`source: 'tool'`).
 * @property {ImageRef[]} images
 */

/**
 * One file the user attached, as `POST /api/tasks/{id}/upload` saved it. The
 * server decides `kind` by sniffing the bytes — never the client, and never the
 * file name — because that is what decides where the file was written, and only
 * `~/.wizard/images/` is servable back to the page.
 * @typedef {Object} Attachment
 * @property {string} path   Absolute path the server wrote it to.
 * @property {string} name   Original file name.
 * @property {string} mime   Sniffed media type.
 * @property {number} bytes
 * @property {'image'|'file'} kind
 */

/**
 * One entry of `GET /api/commands`, derived from the one table the TUI runs on
 * (`commands::COMMANDS`). `where` says who runs it:
 *  - `server` — a `command` frame; the Agent acts on it.
 *  - `client` — app.js handles it (a pane, a panel, an overlay, the chat list).
 *  - `prompt` — a custom command from `.wizard/commands/*.md`. It goes out as an
 *    ordinary `user_message`: the SERVER expands it, through the same
 *    `commands::preprocess` that gives the composer its `@file` refs. The client
 *    never expands one, so a custom command means the same thing in both UIs.
 *  - `unavailable` — terminal-only (`/vim`, `/quit`, `/exit`): a browser has no
 *    modal editor to toggle and no process to exit. Listed, not hidden — someone
 *    who knows it from the TUI should read why it is not here rather than wonder
 *    whether they mistyped it — and never sent, in either direction.
 * @typedef {Object} SlashCommand
 * @property {string} name
 * @property {string} detail          One line, for the palette.
 * @property {'server'|'client'|'prompt'|'unavailable'} where
 * @property {string} [args]          Argument hint ("<name>"), when it takes any.
 */

/**
 * The `context` frame: what the NEXT model call will carry. Distinct from
 * `usage`, which is the session's lifetime spend — conflating the two is what
 * the TUI got wrong before main's 0ed201b, so the two readouts stay apart here.
 * @typedef {Object} ContextSize
 * @property {number} tokens
 * @property {number|null} window  The model's context window; null when unknown.
 */

/**
 * One subagent run, announced by a `subagent_run_started` frame. Every later
 * `subagent_run_*` frame carries the same `run`, so concurrent runs demux
 * into their own panes.
 * @typedef {Object} SubagentRun
 * @property {number} run           Session-unique run id.
 * @property {number|null} bg       Background-registry id; null when the parent waits on it.
 * @property {string} name          Subagent name ("researcher", "reviewer", ...).
 * @property {string} task          The task it was handed.
 */

/**
 * How a subagent run ended.
 * @typedef {Object} SubagentResult
 * @property {boolean} completed    False when it hit its step budget.
 * @property {string} output        Its final report (the step that made no tool call).
 * @property {number} stepsUsed
 * @property {string|null} error    Set when it died on a hard error.
 */

/**
 * One item of a task transcript, discriminated by `type`:
 *  - {type:'user',     text, attachments}       user prompt quote card
 *  - {type:'worked',   label}                   collapsible "Worked ..." divider
 *  - {type:'text',     text}                    agent narration paragraph
 *  - {type:'thinking', text}                    collapsed reasoning block
 *  - {type:'tool',     ...ToolCall}             tool row
 *  - {type:'images',   ...ImageBatch}           images, inline or on a tool's card
 *  - {type:'notice',   text}                    muted system row
 * @typedef {Object} TranscriptItem
 * @property {'user'|'worked'|'text'|'thinking'|'tool'|'images'|'notice'} type
 * @property {string} [text]
 * @property {string} [label]
 */

/**
 * @typedef {Object} GitInfo
 * @property {string} branch
 * @property {boolean} [dirty]
 * @property {number} additions
 * @property {number} deletions
 * @property {Array<{path: string, status?: string, additions: number, deletions: number}>} [files]
 */

/**
 * One changed file's diff against HEAD — staged and unstaged changes together,
 * which is what the same file's `additions`/`deletions` in `GitInfo` count.
 * Already parsed into hunks: the client colors lines, it does not parse diffs.
 * @typedef {Object} FileDiff
 * @property {string} path
 * @property {string} status         `M`, `A`, `D` or `?`, as in `GitInfo.files`.
 * @property {number} additions
 * @property {number} deletions
 * @property {boolean} binary        No line diff exists: an image, an artifact.
 * @property {boolean} truncated     Too long to ship whole; `hunks` is what fits.
 * @property {Array<{header: string, lines: Array<{kind: 'add'|'del'|'ctx'|'meta', text: string}>}>} hunks
 */

/**
 * @typedef {Object} TaskDetail
 * @property {string} id
 * @property {string} title
 * @property {string} workspace     Workspace display name (topbar repo chip).
 * @property {string} path          Workspace absolute path.
 * @property {string} [model]       Model the task runs on.
 * @property {'working'|'needs_input'|'complete'|'idle'|'failed'} status
 * @property {string} [workedFor]   Elapsed label for the last worked divider (mock only).
 * @property {Array<{text: string, done: boolean, active?: boolean}>} [progress]
 * @property {GitInfo} [git]        Mock only; the real git card polls /api/git.
 * @property {TranscriptItem[]} transcript
 */

/**
 * Callbacks invoked as server events arrive for a streamed task. All are
 * optional; a WebSocket frame maps 1:1 onto one callback.
 * @typedef {Object} StreamCallbacks
 * @property {() => void} [onOpen]                 Socket (re)attached.
 * @property {() => void} [onClose]                Socket dropped (not via close()).
 * @property {(delta: string) => void} [onText]
 * @property {(delta: string) => void} [onThinking]
 * @property {(call: ToolCall) => void} [onToolCall]
 * @property {(result: ToolResult) => void} [onToolResult]
 * @property {(batch: ImageBatch) => void} [onImages]                          The turn produced images.
 * @property {(status: TaskStatus) => void} [onStatus]
 * @property {(items: Array<{text:string,done:boolean,active:boolean}>) => void} [onTodo]
 * @property {(usage: {promptTokens:number, completionTokens:number}) => void} [onUsage]
 * @property {(context: ContextSize) => void} [onContext]                       Next turn's context size.
 * @property {(plan: string) => void} [onPlan]
 * @property {(questions: string[]) => void} [onInterview]
 * @property {(text: string) => void} [onNotice]
 * @property {(turn: number|null) => void} [onTranscriptReset]                 `/rewind` truncated the session on disk.
 * @property {(message: string) => void} [onError]
 * @property {(attempt: number) => void} [onRetrying]
 * @property {(reason: string) => void} [onDone]
 * @property {(run: SubagentRun) => void} [onSubagentRun]                      A run started.
 * @property {(run: number, text: string) => void} [onSubagentText]            One of its messages.
 * @property {(run: number, call: ToolCall) => void} [onSubagentToolCall]
 * @property {(run: number, result: ToolResult) => void} [onSubagentToolResult]
 * @property {(run: number, batch: ImageBatch) => void} [onSubagentImages]     Images from inside a run.
 * @property {(run: number, step: number) => void} [onSubagentStep]
 * @property {(run: number, result: SubagentResult) => void} [onSubagentDone]
 */

/**
 * @typedef {Object} StreamHandle
 * @property {() => void} close  Unsubscribe; suppresses all later callbacks.
 */

/**
 * @typedef {Object} ModelInfo
 * @property {string|null} value    Sent as the `model` override; null = task default.
 * @property {string} label
 * @property {string} provider
 * @property {boolean} [isDefault]
 */

/** Title of a chat with no messages yet, in the sidebar and the topbar. */
export const NEW_CHAT_TITLE = 'New chat';

/* ------------------------------------------------------------------------ */
/* Tool taxonomy: wire tool names → display buckets                          */
/* ------------------------------------------------------------------------ */

/** Wire-name → display bucket + aggregation noun. */
const TOOL_BUCKETS = {
  read_file: { tool: 'explore', noun: 'file' },
  list_files: { tool: 'explore', noun: 'listing' },
  search_files: { tool: 'explore', noun: 'search' },
  git_status: { tool: 'explore', noun: 'git check' },
  git_diff: { tool: 'explore', noun: 'git check' },
  execute: { tool: 'run' },
  run_command: { tool: 'run' },
  write_file: { tool: 'write' },
  edit_file: { tool: 'write' },
  web_search: { tool: 'search' },
  web_fetch: { tool: 'search' },
  spawn_subagent: { tool: 'delegate' },
};

const BUCKET_TITLES = {
  explore: 'Explored',
  run: 'Ran',
  write: 'Wrote',
  search: 'Searched',
  delegate: 'Delegated',
};

/** Tools whose calls render as dedicated cards (plan review, interview),
 *  so their raw tool rows are suppressed. */
const HIDDEN_TOOLS = new Set(['exit_plan', 'interview']);

const firstLine = (text) => String(text ?? '').split('\n', 1)[0].trim();
const pathBasename = (path) => String(path ?? '').replace(/\/+$/, '').split('/').pop() || String(path ?? '');

/** Short human line for a call's arguments, shown while the call is pending. */
function argsDetail(name, args) {
  const a = args && typeof args === 'object' ? args : {};
  switch (name) {
    case 'read_file': return a.path || '';
    case 'list_files': return a.path || '.';
    case 'search_files': return a.pattern || '';
    case 'git_status': return 'status';
    case 'git_diff': return 'diff';
    case 'web_search': return a.query || '';
    case 'web_fetch': return a.url || '';
    case 'spawn_subagent':
      return [a.name, firstLine(a.task)].filter(Boolean).join(' — ');
    default: {
      const json = JSON.stringify(a);
      if (!json || json === '{}') return '';
      return json.length > 80 ? `${json.slice(0, 79)}…` : json;
    }
  }
}

/**
 * Classify a wire tool call into its display shape (bucket, title, chips,
 * command, pending detail). Shared by the transcript replay and the live
 * `tool_started` frames.
 * @param {string} name
 * @param {Object} args
 * @returns {ToolCall} without `id`/`status` (caller fills those)
 */
export function classifyTool(name, args) {
  const bucket = TOOL_BUCKETS[name];
  const tool = bucket ? bucket.tool : 'other';
  /** @type {ToolCall} */
  const call = {
    name,
    tool,
    title: BUCKET_TITLES[tool] || name,
    hidden: HIDDEN_TOOLS.has(name),
  };
  if (bucket && bucket.noun) call.noun = bucket.noun;
  if (tool === 'run') {
    call.command = firstLine(args && args.command);
  } else if (tool === 'write') {
    const path = args && typeof args.path === 'string' ? args.path : '';
    call.files = path ? [{ name: pathBasename(path), path }] : [];
  } else {
    call.detail = argsDetail(name, args);
  }
  return call;
}

/* ------------------------------------------------------------------------ */
/* Images                                                                    */
/* ------------------------------------------------------------------------ */

/**
 * Images the browser already holds the bytes of — the file the user just
 * attached — keyed by the path the upload came back with. Nothing here is ever
 * revoked: the same blob backs the composer's chip, then the thumbnail on the
 * prompt card the message became, for as long as the page lives.
 * @type {Map<string, string>}
 */
const localImages = new Map();

/** Serve `path` from these bytes rather than fetching it back from the server. */
export function rememberImage(path, objectUrl) {
  if (path && objectUrl) localImages.set(path, objectUrl);
}

/**
 * Where the page fetches one image's bytes: `GET /api/image`, by the path the
 * frame carried. The frames deliberately carry no base64 — a turn's replay
 * buffer would be megabytes — so the file is a fetch, and the server serves it
 * only if it really is one of the images wizard saved. An image this page
 * uploaded itself skips the round trip entirely.
 * @param {ImageRef} image
 * @returns {string}
 */
export function imageUrl(image) {
  return localImages.get(image.path) || `/api/image?path=${encodeURIComponent(image.path)}`;
}

/**
 * Normalize an `images` / `subagent_run_images` frame (or its replayed item)
 * into an {@link ImageBatch}, dropping entries with no file to fetch.
 * @returns {ImageBatch}
 */
function imageBatch(frame) {
  const images = (Array.isArray(frame.images) ? frame.images : [])
    .filter((image) => image && image.path)
    .map((image) => ({ path: image.path, mime: image.mime || 'image/png', bytes: image.bytes || 0 }));
  return { source: frame.source === 'tool' ? 'tool' : 'assistant', tool: frame.tool || null, images };
}

/**
 * The attachment refs a recorded user message carries, so a replayed session
 * shows what was sent up with the prompt rather than only the prose. Accepts
 * either the upload response's own shape (an `attachments` list, each entry
 * carrying the server's `kind`) or separate `images` / `files` lists, and a
 * bare path string in place of an object.
 * @returns {Attachment[]}
 */
function userAttachments(item) {
  /** @type {Attachment[]} */
  const out = [];
  const add = (raw, kind) => {
    const ref = typeof raw === 'string' ? { path: raw } : raw;
    if (!ref || !ref.path) return;
    out.push({
      path: ref.path,
      name: ref.name || pathBasename(ref.path),
      mime: ref.mime || (kind === 'image' ? 'image/png' : ''),
      bytes: ref.bytes || 0,
      kind,
    });
  };
  for (const raw of Array.isArray(item.attachments) ? item.attachments : []) {
    add(raw, raw && raw.kind === 'image' ? 'image' : 'file');
  }
  for (const raw of Array.isArray(item.images) ? item.images : []) add(raw, 'image');
  for (const raw of Array.isArray(item.files) ? item.files : []) add(raw, 'file');
  return out;
}

/** Fold a finished call's server summary into its display shape. */
export function applyToolSummary(call, summary) {
  const text = String(summary ?? '').trim();
  if (!text) return;
  if (call.tool === 'run') {
    if (!call.command) call.command = text;
  } else if (call.tool === 'write') {
    if (!call.files || !call.files.length) {
      call.files = [{ name: pathBasename(text), path: text }];
    }
    const add = /(?:^|\s)\+(\d+)\b/.exec(text);
    const del = /(?:^|\s)[−-](\d+)\b/.exec(text);
    if (add || del) {
      call.diffstat = {};
      if (add) call.diffstat.additions = Number(add[1]);
      if (del) call.diffstat.deletions = Number(del[1]);
    }
  } else {
    call.detail = text;
  }
}

/* ------------------------------------------------------------------------ */
/* RealApi — HTTP + WebSocket against `wizard gui`                           */
/* ------------------------------------------------------------------------ */

/** Map a wire task state (`/api/tasks`) to the seam's status vocabulary. */
function mapTaskState(state) {
  switch (state) {
    case 'working': return 'working';
    case 'needs_input': return 'needs_input';
    case 'idle': return 'idle';
    case 'failed': return 'failed';
    case 'done':
    default: return 'complete';
  }
}

const WHERE = new Set(['server', 'client', 'prompt', 'unavailable']);

/**
 * The command list, with every entry pinned to a `where` the client knows how
 * to act on. An unrecognized one falls back to `server`: the list came from the
 * server, so the server is the half that knows what to do with it.
 * @returns {SlashCommand[]}
 */
function normalizeCommands(rows) {
  return (Array.isArray(rows) ? rows : [])
    .filter((row) => row && row.name)
    .map((row) => ({
      name: String(row.name),
      detail: row.detail || '',
      where: WHERE.has(row.where) ? row.where : 'server',
      args: row.args || '',
    }));
}

/** Dispatch one parsed server frame onto the callback surface. */
function dispatchFrame(frame, cb) {
  switch (frame.type) {
    case 'text_delta':
      cb.onText && cb.onText(frame.text || '');
      break;
    case 'thinking_delta':
      cb.onThinking && cb.onThinking(frame.text || '');
      break;
    case 'tool_started': {
      const call = classifyTool(frame.name || 'tool', frame.args || {});
      call.id = String(frame.call_id);
      call.status = 'pending';
      if (!call.hidden) cb.onToolCall && cb.onToolCall(call);
      break;
    }
    case 'tool_finished':
      cb.onToolResult && cb.onToolResult({
        callId: String(frame.call_id),
        name: frame.name,
        status: frame.ok ? 'ok' : 'failed',
        summary: frame.summary || '',
      });
      break;
    // A tool's images follow its `tool_finished`, and the model's own follow
    // its text: the batch lands where the thing that made it already is.
    case 'images': {
      const batch = imageBatch(frame);
      if (batch.images.length) cb.onImages && cb.onImages(batch);
      break;
    }
    case 'todo':
      cb.onTodo && cb.onTodo(Array.isArray(frame.items) ? frame.items : []);
      break;
    case 'usage':
      cb.onUsage && cb.onUsage({
        promptTokens: frame.prompt_tokens || 0,
        completionTokens: frame.completion_tokens || 0,
      });
      break;
    // What the next model call will carry — not the session's lifetime spend,
    // which is `usage` above and which the Goal card already reports.
    case 'context':
      cb.onContext && cb.onContext({
        tokens: frame.tokens || 0,
        window: typeof frame.window === 'number' && frame.window > 0 ? frame.window : null,
      });
      break;
    case 'state':
      cb.onStatus && cb.onStatus({ state: mapTaskState(frame.state) });
      break;
    case 'plan_ready':
      cb.onPlan && cb.onPlan(frame.plan || '');
      break;
    case 'interview':
      cb.onInterview && cb.onInterview(Array.isArray(frame.questions) ? frame.questions : []);
      break;
    case 'task_event':
      cb.onNotice && cb.onNotice(`background task ${frame.phase}: ${frame.label || ''}`.trim());
      break;
    case 'subagent':
      cb.onNotice && cb.onNotice(
        `subagent ${frame.name || ''} ${frame.phase}${frame.task ? `: ${firstLine(frame.task)}` : ''}`,
      );
      break;
    // The run-scoped stream: one subagent's own messages and tool calls, which
    // the panel lists as a row and the pane renders as its own chat.
    case 'subagent_run_started':
      cb.onSubagentRun && cb.onSubagentRun({
        run: frame.run,
        bg: frame.bg == null ? null : frame.bg,
        name: frame.name || 'subagent',
        task: frame.task || '',
      });
      break;
    case 'subagent_run_text':
      cb.onSubagentText && cb.onSubagentText(frame.run, frame.text || '');
      break;
    case 'subagent_run_tool_started': {
      const call = classifyTool(frame.name || 'tool', frame.args || {});
      call.id = String(frame.call_id);
      call.status = 'pending';
      if (!call.hidden) cb.onSubagentToolCall && cb.onSubagentToolCall(frame.run, call);
      break;
    }
    case 'subagent_run_tool_finished':
      cb.onSubagentToolResult && cb.onSubagentToolResult(frame.run, {
        callId: String(frame.call_id),
        name: frame.name,
        status: frame.ok ? 'ok' : 'failed',
        summary: frame.summary || '',
      });
      break;
    case 'subagent_run_images': {
      const batch = imageBatch(frame);
      if (batch.images.length) cb.onSubagentImages && cb.onSubagentImages(frame.run, batch);
      break;
    }
    case 'subagent_run_step':
      cb.onSubagentStep && cb.onSubagentStep(frame.run, frame.step || 0);
      break;
    case 'subagent_run_done':
      cb.onSubagentDone && cb.onSubagentDone(frame.run, {
        completed: !!frame.completed,
        output: frame.output || '',
        stepsUsed: frame.steps_used || 0,
        error: frame.error || null,
      });
      break;
    case 'notice':
      cb.onNotice && cb.onNotice(frame.text || '');
      break;
    // `/rewind` truncated the session file: the transcript on screen is a record
    // of turns that no longer exist. The client re-reads the session rather than
    // editing what it drew — the file is the history, and this says it changed.
    case 'transcript_reset':
      cb.onTranscriptReset && cb.onTranscriptReset(typeof frame.turn === 'number' ? frame.turn : null);
      break;
    case 'error':
      cb.onError && cb.onError(frame.message || 'unknown error');
      break;
    case 'retrying':
      cb.onRetrying && cb.onRetrying(frame.attempt || 2);
      break;
    case 'done':
      cb.onDone && cb.onDone(frame.reason || 'completed');
      break;
    default:
      break; // unknown frame types are ignored, forward-compatible
  }
}

export class RealApi {
  /** @param {string} [base] Origin override for tests; '' = same origin. */
  constructor(base = '') {
    this._base = base.replace(/\/$/, '');
    /** @type {Map<string, WebSocket>} */
    this._sockets = new Map();
  }

  async _json(path, options) {
    const res = await fetch(this._base + path, options);
    let body = null;
    try {
      body = await res.json();
    } catch {
      body = null;
    }
    if (!res.ok) {
      const message = body && body.error ? body.error : `${res.status} ${res.statusText}`;
      throw new Error(message);
    }
    return body;
  }

  _post(path, payload) {
    return this._json(path, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(payload),
    });
  }

  /** GET /api/tasks, grouped by workspace (cwd), most recent first. */
  async listTasks() {
    const rows = (await this._json('/api/tasks')) || [];
    /** @type {Map<string, Workspace>} */
    const groups = new Map();
    for (const row of rows) {
      const path = row.cwd || '';
      let ws = groups.get(path);
      if (!ws) {
        ws = { name: row.workspace || pathBasename(path) || '(unknown)', path, tasks: [] };
        groups.set(path, ws);
      }
      ws.tasks.push({
        id: row.id,
        title: row.title || '(untitled)',
        updatedAt: (row.updated_unix || 0) * 1000,
        status: mapTaskState(row.state),
      });
    }
    return Array.from(groups.values());
  }

  /** GET /api/workspace: the directory the server runs in. */
  async home() {
    const row = (await this._json('/api/workspace')) || {};
    return { cwd: row.cwd || '', name: row.name || pathBasename(row.cwd || '') };
  }

  /** GET /api/tasks/{id}: replay items normalized to transcript items. */
  async getTask(id) {
    const raw = await this._json(`/api/tasks/${encodeURIComponent(id)}`);
    /** @type {TranscriptItem[]} */
    const transcript = [];
    let title = '';
    let sawUser = false;
    let inWorked = false;
    let counter = 0;
    const ensureWorked = () => {
      if (sawUser && !inWorked) {
        transcript.push({ type: 'worked', label: 'Worked' });
        inWorked = true;
      }
    };
    for (const item of raw.items || []) {
      switch (item.kind) {
        case 'user':
          transcript.push({ type: 'user', text: item.text || '', attachments: userAttachments(item) });
          sawUser = true;
          inWorked = false;
          if (!title) title = firstLine(item.text) || String(item.text || '').trim();
          break;
        case 'turn_marker':
          break; // the user message itself follows; the marker adds nothing visual
        case 'text':
          ensureWorked();
          transcript.push({ type: 'text', text: item.text || '' });
          break;
        case 'thinking':
          ensureWorked();
          transcript.push({ type: 'thinking', text: item.text || '' });
          break;
        case 'tool': {
          const call = classifyTool(item.name || 'tool', item.args || {});
          if (call.hidden) break;
          ensureWorked();
          call.id = `replay-${++counter}`;
          if (item.output) {
            call.status = item.output.ok ? 'ok' : 'failed';
            applyToolSummary(call, item.output.summary);
          } else {
            call.status = 'pending'; // interrupted run: the result never landed
          }
          transcript.push({ type: 'tool', ...call });
          break;
        }
        case 'images': {
          // The session file records where each image was written, so a
          // transcript reloaded from disk shows the same images the live
          // stream did — the item sits next to the tool card, or the text,
          // it belongs to.
          const batch = imageBatch(item);
          if (!batch.images.length) break;
          ensureWorked();
          transcript.push({ type: 'images', ...batch });
          break;
        }
        case 'notice':
          ensureWorked();
          transcript.push({ type: 'notice', text: item.text || '' });
          break;
        default:
          break;
      }
    }
    if (title.length > 90) title = `${title.slice(0, 89)}…`;
    return {
      id: raw.id,
      title: title || NEW_CHAT_TITLE,
      workspace: raw.workspace || pathBasename(raw.cwd || ''),
      path: raw.cwd || '',
      model: raw.model || '',
      status: 'idle',
      transcript,
    };
  }

  /**
   * Open the task's WebSocket. The server replays the current turn's
   * buffered frames on attach, then streams live. `onClose` fires on any
   * drop not caused by `close()` — the app schedules the reconnect.
   * @param {string} id
   * @param {StreamCallbacks} callbacks
   * @returns {StreamHandle}
   */
  streamTask(id, callbacks) {
    const path = `/api/tasks/${encodeURIComponent(id)}/ws`;
    const url = this._base
      ? this._base.replace(/^http/, 'ws') + path
      : `${window.location.protocol === 'https:' ? 'wss://' : 'ws://'}${window.location.host}${path}`;
    let closed = false;
    const ws = new WebSocket(url);
    this._sockets.set(id, ws);
    ws.addEventListener('open', () => {
      if (!closed && callbacks.onOpen) callbacks.onOpen();
    });
    ws.addEventListener('message', (event) => {
      if (closed) return;
      let frame;
      try {
        frame = JSON.parse(event.data);
      } catch {
        return;
      }
      dispatchFrame(frame, callbacks);
    });
    ws.addEventListener('close', () => {
      if (this._sockets.get(id) === ws) this._sockets.delete(id);
      if (!closed) {
        closed = true;
        if (callbacks.onClose) callbacks.onClose();
      }
    });
    return {
      close: () => {
        closed = true;
        if (this._sockets.get(id) === ws) this._sockets.delete(id);
        try {
          ws.close();
        } catch {
          /* already closed */
        }
      },
    };
  }

  _send(id, frame) {
    const ws = this._sockets.get(id);
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      throw new Error('not connected to the task stream');
    }
    ws.send(JSON.stringify(frame));
  }

  /**
   * `user_message` over the task's socket; the reply streams back. Attachment
   * paths go up as the paths the server itself handed back from `upload` — it
   * re-verifies each one against the directory it wrote them to, so a path this
   * client invented would be refused rather than read.
   * @param {{model?: string, images?: string[], files?: string[]}} [opts]
   */
  async sendMessage(id, text, opts = {}) {
    const frame = { type: 'user_message', text };
    if (opts.model) frame.model = opts.model;
    if (opts.images && opts.images.length) frame.images = opts.images;
    if (opts.files && opts.files.length) frame.files = opts.files;
    this._send(id, frame);
    return { ok: true };
  }

  /**
   * POST /api/tasks/{id}/upload: the files, as multipart. One request per file,
   * so a chip in the composer maps to exactly one result — the endpoint takes
   * several parts, but nothing here needs to guess which came back as which.
   * The content-type is left to the browser: it owns the multipart boundary.
   * @param {string} id
   * @param {File[]} files
   * @returns {Promise<Attachment[]>}
   */
  async upload(id, files) {
    const body = new FormData();
    for (const file of files) body.append('file', file, file.name);
    const out = await this._json(`/api/tasks/${encodeURIComponent(id)}/upload`, { method: 'POST', body });
    return (out && out.attachments) || [];
  }

  /**
   * GET /api/commands: what `/` offers in this workspace — the built-ins plus
   * the custom commands of `.wizard/commands/`, which is why it is scoped to a
   * directory rather than global.
   * @param {string} cwd
   * @returns {Promise<SlashCommand[]>}
   */
  async commands(cwd) {
    const out = await this._json(`/api/commands?cwd=${encodeURIComponent(cwd || '')}`);
    return normalizeCommands(out && out.commands);
  }

  /** `command`: a server-side slash command. It answers with the frame kinds it
   *  already has — `notice`, `state`, `context`, or `error`. */
  sendCommand(id, name, args) {
    this._send(id, { type: 'command', name, args: args || '' });
  }

  /** `plan_verdict`: approve or reject (with optional feedback) a held plan. */
  planVerdict(id, approve, feedback) {
    const frame = { type: 'plan_verdict', approve };
    if (feedback) frame.feedback = feedback;
    this._send(id, frame);
  }

  /** `interview_answers`; pass null to skip the interview. */
  interviewAnswers(id, answers) {
    this._send(id, { type: 'interview_answers', answers });
  }

  /** `cancel`: interrupt the running turn cooperatively. */
  cancel(id) {
    this._send(id, { type: 'cancel' });
  }

  /** GET /api/git for the task's workspace. */
  async gitStatus(task) {
    return this._json(`/api/git?cwd=${encodeURIComponent(task.path)}`);
  }

  /**
   * GET /api/git/diff: one changed file's diff, hunks already parsed. The
   * backend takes only paths `gitStatus` itself just reported, so a path it
   * does not recognize comes back as an error rather than a diff.
   * @param {{path: string}} task
   * @param {string} path  workspace-relative, as listed in `GitInfo.files`
   * @returns {Promise<FileDiff>}
   */
  async fileDiff(task, path) {
    const query = `cwd=${encodeURIComponent(task.path)}&path=${encodeURIComponent(path)}`;
    return this._json(`/api/git/diff?${query}`);
  }

  /**
   * GET /api/models flattened for the picker. The active provider lists its
   * configured model (the no-override default) plus its listed models; every
   * other provider gets one entry whose value is the provider name — the
   * backend resolves provider names by switching the active provider when
   * the agent is (re)built.
   */
  async listModels() {
    const data = await this._json('/api/models');
    /** @type {ModelInfo[]} */
    const models = [];
    for (const provider of data.providers || []) {
      const isActive = provider.name === data.active;
      if (isActive) {
        models.push({ value: null, label: provider.model, provider: provider.name, isDefault: true });
        for (const model of provider.models || []) {
          if (model !== provider.model) {
            models.push({ value: model, label: model, provider: provider.name });
          }
        }
      } else {
        models.push({ value: provider.name, label: provider.model, provider: provider.name });
      }
    }
    return models;
  }

  /**
   * POST /api/tasks with no prompt: an empty chat, in `cwd` or (without one)
   * the directory the server runs in. The first `user_message` starts its
   * first turn.
   * @param {string} [cwd] absolute path of the workspace
   */
  async newChat(cwd) {
    const out = await this._post('/api/tasks', cwd ? { cwd } : {});
    return { id: out.id, cwd: out.cwd || '', workspace: out.workspace || pathBasename(out.cwd || '') };
  }

  /** GET /api/workspaces: directories a chat can be opened in. */
  async workspaces() {
    const rows = (await this._json('/api/workspaces')) || [];
    return rows.map((row) => ({
      cwd: row.cwd,
      name: row.name || pathBasename(row.cwd),
      taskCount: row.task_count || 0,
      home: !!row.home,
    }));
  }

  /** GET /api/git/branches: local branches of the chat's workspace. */
  async branches(task) {
    const out = await this._json(`/api/git/branches?cwd=${encodeURIComponent(task.path)}`);
    return { current: out.current || null, branches: out.branches || [] };
  }

  /**
   * POST /api/git/checkout: `git checkout [-b] <branch>` in the workspace.
   * Git's refusals (an uncommitted change the switch would overwrite) surface
   * as the error text.
   * @returns {Promise<string>} the branch now checked out
   */
  async checkout(task, branch, create = false) {
    const out = await this._post('/api/git/checkout', {
      cwd: task.path, branch, create, task: task.id,
    });
    return out.branch;
  }

  /** GET /api/settings: providers, key sources, presets, first-run flag. */
  async settings() {
    return this._json('/api/settings');
  }

  /**
   * POST /api/login/{provider}: begin a subscription sign-in.
   * @returns {Promise<string>} the URL to send the user to
   */
  async beginSignIn(provider) {
    const out = await this._post(`/api/login/${encodeURIComponent(provider)}`, {});
    return out.authorize_url;
  }

  /** GET /api/login: `{state: idle|pending|done|failed, provider?, error?}`. */
  async signInStatus() {
    return this._json('/api/login');
  }

  /** PATCH /api/settings: `{mode?, max_steps?}` → the new settings. */
  async saveSettings(patch) {
    return this._json('/api/settings', {
      method: 'PATCH',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(patch),
    });
  }

  /**
   * POST /api/providers: add or edit a provider (same name = edit), storing
   * `apiKey` in ~/.wizard/credentials.toml when given.
   * @returns {Promise<{settings: Object, probe: {ok: boolean, error?: string, models: string[]}}>}
   */
  async saveProvider({ name, kind, baseUrl, model, apiKey, activate = true }) {
    const payload = { name, kind, base_url: baseUrl, model, activate };
    if (apiKey) payload.api_key = apiKey;
    return this._post('/api/providers', payload);
  }

  /** POST /api/providers/{name}/test: does this provider answer? */
  async testProvider(name) {
    return this._post(`/api/providers/${encodeURIComponent(name)}/test`, {});
  }

  /** POST /api/providers/{name}/active: switch the active provider. */
  async activateProvider(name) {
    return this._post(`/api/providers/${encodeURIComponent(name)}/active`, {});
  }

  /** DELETE /api/providers/{name}: forget the provider and its stored key. */
  async removeProvider(name) {
    return this._json(`/api/providers/${encodeURIComponent(name)}`, { method: 'DELETE' });
  }
}

/* ------------------------------------------------------------------------ */
/* Mock data                                                                 */
/* ------------------------------------------------------------------------ */

const GUI_TASK_ID = 'wiz-gui-shell';

function buildMockData() {
  const now = Date.now();
  const ago = (min) => now - Math.round(min * 60000);

  /** @type {Workspace[]} */
  const workspaces = [
    {
      name: 'wizard',
      path: '/home/gradient/projects/ai/wizard',
      tasks: [
        { id: GUI_TASK_ID, title: 'Build a static three-pane GUI shell for the wizard binary', updatedAt: ago(2), status: 'complete' },
        { id: 'wiz-provider', title: 'Refine provider picker copy and key entry', updatedAt: ago(9), status: 'complete' },
        { id: 'wiz-toolrows', title: 'Wire tool-call stream rows into the TUI', updatedAt: ago(14), status: 'complete' },
        { id: 'wiz-softwrap', title: 'Adapt composer soft-wrap for narrow panes', updatedAt: ago(27), status: 'complete' },
        { id: 'wiz-update', title: 'Add rollback tests for self-update', updatedAt: ago(51), status: 'complete' },
      ],
    },
    {
      name: 'wizard-site',
      path: '/home/gradient/projects/frontend/wizard-site',
      tasks: [
        { id: 'site-pinning', title: 'Fix bottom pinning when the demo gif loads', updatedAt: ago(8), status: 'complete' },
        { id: 'site-hero', title: 'Refresh hero visual wording and spacing', updatedAt: ago(3), status: 'complete' },
        { id: 'site-bench', title: 'Tighten homepage bench copy', updatedAt: ago(42), status: 'complete' },
        { id: 'site-breakpoints', title: 'Tune hero breakpoints for tablet widths', updatedAt: ago(60), status: 'complete' },
        { id: 'site-faq', title: 'Add pricing FAQ content and anchors', updatedAt: ago(120), status: 'complete' },
        { id: 'site-docs-search', title: 'Improve docs search highlighting', updatedAt: ago(300), status: 'complete' },
      ],
    },
    {
      name: 'apollos-brain',
      path: '/home/gradient/projects/infra/apollos-brain',
      tasks: [
        { id: 'brain-logs', title: 'Summarize weekly repo activity into Logs', updatedAt: ago(26 * 60), status: 'complete' },
        { id: 'brain-prune', title: 'Prune stale pattern notes and dead links', updatedAt: ago(50 * 60), status: 'complete' },
      ],
    },
  ];

  /** @type {Map<string, TaskDetail>} */
  const tasks = new Map();

  // --- The selected task: mirrors the reference screenshot structure. ------
  tasks.set(GUI_TASK_ID, {
    id: GUI_TASK_ID,
    title: 'Build a static three-pane GUI shell for the wizard binary to serve and embed',
    workspace: 'wizard',
    path: '/home/gradient/projects/ai/wizard',
    model: 'glm-5.2',
    status: 'complete',
    workedFor: '3m 1s',
    progress: [
      { text: 'Scaffold the three-pane layout, top bar, and composer skeleton', done: true },
      { text: 'Style the sidebar task tree with workspace groups and selection', done: true },
      { text: 'Render transcript cards, tool rows, and file chips', done: true },
      { text: 'Build the git, goal, and progress context cards', done: true },
      { text: 'Add the mock API seam with streaming callbacks', done: true },
    ],
    git: {
      branch: 'feat/gui',
      additions: 734,
      deletions: 7,
      files: [
        { path: 'gui/assets/index.html', status: 'M', additions: 54, deletions: 0 },
        { path: 'gui/assets/style.css', status: 'M', additions: 361, deletions: 0 },
        { path: 'gui/assets/app.js', status: 'M', additions: 214, deletions: 0 },
        { path: 'gui/assets/api.js', status: 'M', additions: 98, deletions: 0 },
        { path: 'src/serve.rs', status: 'M', additions: 7, deletions: 7 },
      ],
    },
    transcript: [
      {
        type: 'user',
        text:
          'Build a static three-pane GUI shell for wizard: dark sidebar with the task list, ' +
          'streaming conversation in the center, and a git/plan context panel on the right. ' +
          'No frameworks — plain HTML, CSS, and ES modules the Rust binary can embed and serve.',
        // A replayed message shows what went up with it. (A fixture image would
        // have no bytes behind it; a real one is attached in the composer.)
        attachments: [
          {
            path: '/home/you/.wizard/attachments/wiz-gui-shell/gui-design-spec.md',
            name: 'gui-design-spec.md',
            mime: 'text/markdown',
            bytes: 8100,
            kind: 'file',
          },
        ],
      },
      { type: 'worked', label: 'Worked for 3m 1s' },
      {
        type: 'text',
        text:
          "I'm going to inspect the current wizard project structure first, then I'll identify what " +
          'already exists for GUI assets and static serving before wiring in the layout and mock data.',
      },
      { type: 'tool', tool: 'explore', id: 'c1', title: 'Explored', detail: '1 search, 1 file', status: 'failed' },
      {
        type: 'text',
        text:
          'The gui directory looks either empty or nearly empty. I’m checking the actual directory contents now ' +
          'and confirming whether there’s an existing shell to extend or whether I should build the layout from scratch.',
      },
      { type: 'tool', tool: 'run', id: 'c2', title: 'Ran', command: 'git status --short', status: 'failed' },
      {
        type: 'text',
        text:
          'There isn’t an existing shell here, so I’m building a self-contained implementation from scratch. I’m ' +
          'checking whether there are any local project conventions to respect, then I’ll add a framework-free ' +
          'three-pane layout with mock streaming, tool rows, and a git panel.',
      },
      {
        type: 'tool',
        tool: 'write',
        id: 'c3',
        title: 'Wrote',
        files: [
          { name: 'index.html', path: 'gui/assets/index.html' },
          { name: 'app.js', path: 'gui/assets/app.js' },
          { name: 'style.css', path: 'gui/assets/style.css' },
        ],
        diffstat: { additions: 733 },
        status: 'ok',
      },
    ],
    // Streaming tail: replayed through streamTask() so the shell exercises the
    // same code path a live WebSocket feed uses.
    tailEvents: textEvents(
      'The full UI and core logic are in place. I’m verifying the JavaScript parses cleanly and reviewing ' +
        'the layout for edge cases like narrow viewports, collapsed panels, long file names, and streaming ' +
        'transcripts that outgrow the scrollback.'
    ),
  });

  // --- Secondary task with a partially complete plan (undone styling). -----
  tasks.set('wiz-provider', {
    id: 'wiz-provider',
    title: 'Refine provider picker copy and key entry',
    workspace: 'wizard',
    path: '/home/gradient/projects/ai/wizard',
    model: 'glm-5.2',
    status: 'complete',
    workedFor: '1m 12s',
    progress: [
      { text: 'Audit current provider menu strings for tone drift', done: true },
      { text: 'Rewrite picker labels as dry one-liners', done: true },
      { text: 'Move key entry inline under the selected provider', done: true },
      { text: 'Persist keys to ~/.wizard/credentials.toml on submit', done: false },
      { text: 'Add round-trip test for credential save/load', done: false },
    ],
    git: { branch: 'main', additions: 41, deletions: 18, files: [] },
    transcript: [
      { type: 'user', text: 'Refine the provider picker copy — dry one-liners, no hype — and make key entry inline instead of a separate screen.' },
      { type: 'worked', label: 'Worked for 1m 12s' },
      { type: 'text', text: 'I’m sweeping the picker strings first to find everything that narrates instead of stating, then I’ll fold the key prompt into the provider row itself.' },
      { type: 'tool', tool: 'run', id: 'p1', title: 'Ran', command: 'rg -n "provider" src/tui/menu.rs', status: 'ok' },
      { type: 'text', text: 'The copy edits are in. Key entry now opens inline under the selected provider; persistence and the round-trip test are still open.' },
    ],
  });

  // --- Remaining tasks share a compact generic shape. -----------------------
  const simple = (id, ws, branch, prompt, worked, closing, git) => {
    const summary = workspaces.flatMap((w) => w.tasks).find((t) => t.id === id);
    tasks.set(id, {
      id,
      title: summary ? summary.title : id,
      workspace: ws.name,
      path: ws.path,
      model: 'glm-5.2',
      status: 'complete',
      workedFor: worked,
      git: { files: [], ...git, branch },
      transcript: [
        { type: 'user', text: prompt },
        { type: 'worked', label: `Worked for ${worked}` },
        { type: 'text', text: closing },
      ],
    });
  };

  const [wiz, site, brain] = workspaces;
  simple('wiz-toolrows', wiz, 'main',
    'Render tool calls in the TUI as structured rows (explore / run / write) instead of raw text blocks.',
    '4m 40s',
    'Structured rows land for the three core tools; unknown tools fall back to a generic label row. Snapshot tests updated.',
    { additions: 210, deletions: 64 });
  simple('wiz-softwrap', wiz, 'main',
    'Soft-wrap the composer instead of horizontal scrolling, keeping cursor math correct across wraps.',
    '2m 05s',
    'Composer now wraps at the pane edge and the cursor tracks logical columns. Added wrap-boundary unit tests.',
    { additions: 96, deletions: 31 });
  simple('wiz-update', wiz, 'feat/self-update',
    'Add rollback tests for wizard self-update: corrupt download, failed sanity check, and .bak restore.',
    '3m 33s',
    'All three failure paths are covered; the .bak restore test exercises --rollback end to end.',
    { additions: 122, deletions: 4 });

  simple('site-pinning', site, 'main',
    'Fix bottom pinning on the landing page when the demo gif finishes loading late.',
    '1m 48s',
    'The gif now reserves its box via aspect-ratio, so late loads no longer shove the fold. Verified at three widths.',
    { additions: 12, deletions: 6 });
  simple('site-hero', site, 'main',
    'Refresh the hero wording and spacing — keep it dry, one line, no adjectives.',
    '58s',
    'Hero copy is down to a single factual line and the spacing grid is back on the 8px rhythm.',
    { additions: 9, deletions: 11 });
  simple('site-bench', site, 'main',
    'Tighten the homepage bench section copy to match the measured numbers.',
    '1m 21s',
    'Bench copy now quotes the recorded harness numbers verbatim and links the methodology page.',
    { additions: 17, deletions: 22 });
  simple('site-breakpoints', site, 'main',
    'Tune hero breakpoints for tablet widths; the wordmark clips between 768 and 900px.',
    '2m 10s',
    'Added an intermediate breakpoint at 840px; the wordmark scales instead of clipping.',
    { additions: 24, deletions: 9 });
  simple('site-faq', site, 'main',
    'Add pricing FAQ content with anchor links from the pricing table.',
    '3m 02s',
    'Six FAQ entries added with stable anchors; the pricing table rows deep-link to them.',
    { additions: 88, deletions: 0 });
  simple('site-docs-search', site, 'main',
    'Improve docs search highlighting: match on headings and highlight the matched term in results.',
    '4m 15s',
    'Search results now bold the matched term and prefer heading matches. Index rebuild stays under 40ms.',
    { additions: 61, deletions: 18 });

  simple('brain-logs', brain, 'main',
    'Summarize this week’s repo activity into Logs/ with per-repo bullet points.',
    '2m 51s',
    'Weekly log written with per-repo sections and links back to the commits it summarizes.',
    { additions: 140, deletions: 0 });
  simple('brain-prune', brain, 'main',
    'Prune stale pattern notes and fix dead links across the vault.',
    '3m 44s',
    'Removed four superseded patterns and repaired eleven dead links; the graph has no orphans left.',
    { additions: 23, deletions: 187 });

  return { workspaces, tasks };
}

/**
 * Split a paragraph into word-level text events (keeps trailing whitespace).
 * @param {string} text
 */
function textEvents(text) {
  return text.split(/(?<=\s)/).map((delta) => ({ type: 'text', delta }));
}

/* ------------------------------------------------------------------------ */
/* MockApi                                                                   */
/* ------------------------------------------------------------------------ */

let idCounter = 0;
const nextId = (prefix) => `${prefix}-${Date.now().toString(36)}-${++idCounter}`;

/**
 * The mock's `/` palette. The real menu is derived from `commands::COMMANDS`, so
 * this mirrors that table — every built-in, in its display order, with the
 * `where` the backend assigns it — plus one custom `prompt` command, the kind a
 * workspace's `.wizard/commands/*.md` adds. All four `where` kinds appear, which
 * is what makes each path drivable headlessly.
 */
const MOCK_COMMANDS = [
  { name: 'model', args: '[tag]', detail: 'pick or switch the model', where: 'server' },
  { name: 'mode', args: '[genie|sovereign]', detail: 'pick or switch personality mode', where: 'server' },
  { name: 'genie', detail: 'switch to genie mode', where: 'server' },
  { name: 'sovereign', detail: 'switch to sovereign mode', where: 'server' },
  { name: 'effort', args: '[low|medium|high|default]', detail: 'set reasoning effort (Grok 4.x, OpenAI o-series / gpt-5)', where: 'server' },
  { name: 'plan', detail: 'toggle plan mode: read-only until a plan is approved', where: 'server' },
  { name: 'omakase', detail: "toggle omakase: chef's-choice plan mode, the agent decides", where: 'server' },
  { name: 'rewind', args: '[turn]', detail: 'rewind files and conversation to before a turn', where: 'server' },
  { name: 'resume', detail: 'reopen and continue a past session', where: 'client' },
  { name: 'compact', detail: 'summarize older history into a progress note now', where: 'server' },
  { name: 'agents', detail: 'browse subagents and delegate to one', where: 'server' },
  { name: 'subagents', detail: 'monitor the subagents running in this session', where: 'client' },
  { name: 'evolve', args: '[--deep] <desc>', detail: 'self-extend: add a skill, tool, or MCP server', where: 'server' },
  { name: 'publish', args: '[branch]', detail: 'fork & publish your Wizard, get a one-line installer', where: 'server' },
  { name: 'provider', detail: 'add or switch LLM providers (interactive)', where: 'client' },
  { name: 'fusion', args: '[config]', detail: 'toggle model fusion, or configure the panel', where: 'server' },
  { name: 'server', args: '[status|start|stop]', detail: 'manage the local llama-server', where: 'server' },
  { name: 'login', args: '<xai>', detail: 'sign in to a provider account (xAI OAuth)', where: 'client' },
  { name: 'diff', detail: 'toggle the git diff sidebar', where: 'client' },
  { name: 'todos', detail: 'toggle the todo overlay above the input', where: 'client' },
  { name: 'dashboard', detail: 'session manager: all live wizard sessions on this machine', where: 'client' },
  { name: 'cost', detail: 'show session token usage and cost', where: 'server' },
  { name: 'memory', detail: 'show saved project memories', where: 'server' },
  { name: 'status', detail: 'show session status: model, usage, todos, tasks', where: 'server' },
  { name: 'bashes', detail: 'list background tasks: id, status, command', where: 'server' },
  { name: 'goal', args: '[text]', detail: 'show or set the standing mission goal', where: 'server' },
  { name: 'settings', detail: 'open the settings menu (change config anytime)', where: 'client' },
  { name: 'vim', detail: 'toggle vim-style modal editing of the input line', where: 'unavailable' },
  { name: 'doctor', detail: 'diagnose config, providers, MCP, hooks, state dirs', where: 'server' },
  { name: 'reload', detail: 'reload skills, scripted tools, and MCP servers', where: 'server' },
  { name: 'clear', detail: 'clear the conversation', where: 'client' },
  { name: 'help', detail: 'show available commands and keys', where: 'server' },
  { name: 'quit', detail: 'exit wizard', where: 'unavailable' },
  { name: 'exit', detail: 'exit wizard', where: 'unavailable' },
  { name: 'review', detail: 'review the diff (.wizard/commands/review.md)', where: 'prompt', args: '<args>' },
];

/** What the mock's `server` commands answer with — a notice, as the real ones do.
 *  `/rewind` is not here: it truncates, and answers with `transcript_reset`. */
const MOCK_NOTICES = {
  model: (a) => `Model set to ${a || '(no model given)'}.`,
  mode: (a) => `Mode set to ${a || '(no mode given)'}.`,
  genie: () => 'Mode set to genie.',
  sovereign: () => 'Mode set to sovereign.',
  effort: (a) => `Reasoning effort set to ${a || 'default'}.`,
  plan: () => 'Plan mode on: the next turn investigates read-only and presents a plan.',
  omakase: () => 'Omakase on: the agent decides when the plan is ready.',
  compact: () => 'History compacted into a progress note.',
  agents: () => 'Subagents: researcher, reviewer, tester.',
  evolve: (a) => `Evolve: ${a || '(nothing to build)'} — skill written and registered.`,
  publish: (a) => `Published to ${a || 'main'}.`,
  fusion: () => 'Fusion on: every turn now runs through the panel.',
  server: (a) => `Local llama-server: ${a === 'stop' ? 'stopped' : a === 'start' ? 'running on :8080' : 'not running'}.`,
  cost: () => 'Session spend: 24,310 prompt · 3,120 completion tokens.',
  memory: () => 'Project memories: 3 notes in .wizard/memory.md.',
  status: () => 'glm-5.2 · zai · sovereign · effort default · no step limit · 24,310 tokens next turn.',
  bashes: () => 'No background tasks.',
  goal: (a) => (a ? `Goal set: ${a}` : 'No standing goal in this workspace.'),
  doctor: () => 'Config, providers, MCP, hooks and state dirs all check out.',
  reload: () => 'Reloaded: 2 skills, 1 scripted tool, 0 MCP servers.',
  help: () => `Commands: ${MOCK_COMMANDS.map((c) => `/${c.name}`).join(' ')}`,
};

/** The presets the mock's Settings page offers (a subset of the real ones). */
const MOCK_PRESETS = [
  { name: 'anthropic', label: 'Anthropic', kind: 'anthropic', base_url: 'https://api.anthropic.com', model: 'claude-fable-5', needs_key: true, needs_base_url: false, hint: 'Claude models, straight from Anthropic.' },
  { name: 'openai', label: 'OpenAI', kind: 'openai', base_url: 'https://api.openai.com/v1', model: 'gpt-5.2', needs_key: true, needs_base_url: false, hint: 'GPT models, or any OpenAI-compatible endpoint.' },
  { name: 'ollama', label: 'Ollama', kind: 'ollama', base_url: 'http://127.0.0.1:11434', model: 'qwen3:8b', needs_key: false, needs_base_url: false, hint: 'A local model served by Ollama. No key needed.' },
];

export class MockApi {
  constructor() {
    this._data = buildMockData();
    /** @type {Map<string, Set<StreamCallbacks>>} */
    this._streams = new Map();
    /** @type {Set<number>} */
    this._timers = new Set();
    this._mockProviders = [
      { name: 'zai', kind: 'openai', base_url: 'https://api.z.ai/v1', model: 'GLM-5.2', key: 'stored' },
      { name: 'anthropic', kind: 'anthropic', base_url: 'https://api.anthropic.com', model: 'claude-sonnet-4-5', key: 'env' },
    ];
    /** What the next turn would carry; grows with the conversation, and drops
     *  when /compact or /rewind takes a scythe to it. @type {ContextSize} */
    this._context = { tokens: 24310, window: 200000 };
    /** The GUI's step budget. 0 = no limit, as v1.2's default now is. */
    this._maxSteps = 0;
    /** Session-unique subagent run ids, as the server's are. */
    this._runs = 0;
    /** The sign-in in flight, for `/login` and the Settings sign-in rows. */
    this._signIn = null;
  }

  /** @returns {Promise<Workspace[]>} */
  async listTasks() {
    return this._data.workspaces;
  }

  /** @returns {Promise<WorkspaceRef>} */
  async home() {
    const ws = this._data.workspaces[0];
    return { cwd: ws.path, name: ws.name };
  }

  /**
   * @param {string} id
   * @returns {Promise<TaskDetail>}
   */
  async getTask(id) {
    const task = this._data.tasks.get(id);
    if (!task) throw new Error(`unknown task: ${id}`);
    return task;
  }

  /**
   * @param {string} id
   * @param {StreamCallbacks} callbacks
   * @returns {StreamHandle}
   */
  streamTask(id, callbacks) {
    let set = this._streams.get(id);
    if (!set) {
      set = new Set();
      this._streams.set(id, set);
    }
    set.add(callbacks);
    const t = setTimeout(() => {
      this._timers.delete(t);
      if (!set.has(callbacks)) return;
      if (callbacks.onOpen) callbacks.onOpen();
      // The meter has a reading as soon as a chat is open, not only once the
      // first turn of it has run.
      if (callbacks.onContext) callbacks.onContext(this._context);
    }, 0);
    this._timers.add(t);

    const task = this._data.tasks.get(id);
    if (task && task.tailEvents && task.tailEvents.length) {
      // Replay the streaming tail for this subscriber only, then finish.
      // The resolver honors close(): once unsubscribed, no more deliveries.
      this._play(() => (set.has(callbacks) ? [callbacks] : []), task.tailEvents, {
        state: task.status,
        elapsedLabel: task.workedFor,
      });
    } else if (task) {
      const t = setTimeout(() => {
        this._timers.delete(t);
        if (set.has(callbacks) && callbacks.onStatus) {
          callbacks.onStatus({ state: task.status, elapsedLabel: task.workedFor });
        }
      }, 0);
      this._timers.add(t);
    }

    return {
      close: () => {
        set.delete(callbacks);
      },
    };
  }

  /**
   * @param {string} id
   * @param {string} text
   * @param {{model?: string, images?: string[], files?: string[]}} [opts]
   */
  async sendMessage(id, text, opts = {}) {
    const subscribers = () => Array.from(this._streams.get(id) ?? []);
    const callId = nextId('call');
    const subCallId = nextId('call');
    const run = ++this._runs;
    const attached = [
      (opts.images || []).length && `${opts.images.length} image(s)`,
      (opts.files || []).length && `${opts.files.length} file(s)`,
    ].filter(Boolean).join(' and ');
    const opening = attached
      ? `Looking at the ${attached} you attached, and at the files they point to. `
      : 'Taking another pass at that now. I’m rechecking the affected files and rerunning the quick checks before reporting back. ';
    const closing =
      'Done — the follow-up change is in place. The shell still renders cleanly and the mock stream replays as expected' +
      (opts.model ? ` (model: ${opts.model}).` : '.');
    // The prompt grew by what was said and what came with it; the reply grows it
    // again. The meter reads this, the Goal card's lifetime total reads `usage`.
    const promptTokens = 1200 + Math.round(text.length / 4);
    this._context = { tokens: this._context.tokens + promptTokens + 480, window: 200000 };

    // The mock's session file: what `getTask` reads back, and therefore what the
    // refetch a `transcript_reset` forces will show. A turn that streamed but was
    // never recorded would silently vanish at the next `/rewind`, which is the
    // one thing a rewind has to get right.
    const task = this._data.tasks.get(id);
    if (task) {
      task.transcript.push({ type: 'user', text });
      task.transcript.push({ type: 'worked', label: 'Worked for 21s' });
      task.transcript.push({ type: 'text', text: opening + closing });
    }

    const todo = (n) => [
      { text: 'Re-read the files the change touched', done: n > 0, active: n === 0 },
      { text: 'Rerun the quick checks', done: n > 1, active: n === 1 },
      { text: 'Report back', done: n > 2, active: n === 2 },
    ];
    const events = [
      { type: 'status', status: { state: 'working' } },
      { type: 'context', context: this._context },
      { type: 'todo', items: todo(0) },
      ...textEvents(opening),
      {
        type: 'tool_call',
        call: { id: callId, name: 'execute', tool: 'run', title: 'Ran', command: 'node --check gui/assets/app.js', status: 'pending' },
      },
      { type: 'tool_result', result: { callId, status: 'ok' } },
      { type: 'todo', items: todo(1) },
      // One delegation, streamed as its own run: the panel's Subagents section —
      // what `/subagents` reveals — is empty until something has run in the chat.
      { type: 'subagent_run', info: { run, bg: null, name: 'reviewer', task: 'Check the diff for regressions' } },
      { type: 'subagent_text', run, text: 'Reading the changed files against HEAD.' },
      {
        type: 'subagent_tool_call',
        run,
        call: { id: subCallId, name: 'read_file', tool: 'explore', title: 'Explored', detail: 'gui/assets/app.js', status: 'pending' },
      },
      { type: 'subagent_tool_result', run, result: { callId: subCallId, status: 'ok', summary: 'gui/assets/app.js (2,900 lines)' } },
      { type: 'subagent_step', run, step: 2 },
      {
        type: 'subagent_done',
        run,
        result: { completed: true, output: 'No regressions in the diff.', stepsUsed: 2, error: null },
      },
      ...textEvents(closing),
      { type: 'todo', items: todo(3) },
      { type: 'usage', usage: { promptTokens, completionTokens: 480 } },
    ];
    this._play(subscribers, events, { state: 'complete' }, 'completed');
    return { ok: true };
  }

  /**
   * The server half of a `/command`, answered with the frame kinds the protocol
   * already has: a notice, a fresh reading for the meter where the command moved
   * the context, and — for `/rewind <turn>` — the `transcript_reset` that says
   * the session file itself was truncated.
   *
   * The refusals are here too. A `client` or `unavailable` command should never
   * reach this method; if a page bug sends one, it is answered exactly as the
   * server answers it, so the bug is visible under `?mock=1` and not only
   * against a live backend.
   */
  sendCommand(id, name, args) {
    const subscribers = () => Array.from(this._streams.get(id) ?? []);
    const refuse = (message) => this._play(subscribers, [{ type: 'error', message }], { state: 'idle' });
    const spec = MOCK_COMMANDS.find((c) => c.name === name);
    if (!spec) return refuse(`unknown command: /${name}`);
    if (spec.where === 'client') return refuse(`/${name} is run by the page, not the server`);
    if (spec.where === 'unavailable') return refuse(`/${name} runs in the terminal; a browser has nowhere to put it`);
    if (spec.where === 'prompt') return refuse(`/${name} is a prompt: send it as a message`);
    if (name === 'rewind') return this._play(subscribers, this._rewind(id, args), { state: 'idle' });

    const say = MOCK_NOTICES[name];
    if (!say) return refuse(`/${name} is not implemented in the mock`);
    const events = [{ type: 'notice', text: say(args) }];
    if (name === 'compact') {
      this._context = { tokens: 4200, window: 200000 };
      events.push({ type: 'context', context: this._context });
    }
    this._play(subscribers, events, { state: 'idle' });
  }

  /**
   * `/rewind`: bare, it lists the turns there is something to go back to; with a
   * turn, it truncates the session before it and says so with a `transcript_reset`
   * — which the client answers by re-reading the session, since the file is the
   * only copy of the history. The reset comes first and the notice after, the
   * order the live server sends them in.
   */
  _rewind(id, args) {
    const task = this._data.tasks.get(id);
    if (!task) return [{ type: 'error', message: `unknown task: ${id}` }];
    const items = task.transcript;
    const starts = items.reduce((acc, item, i) => (item.type === 'user' ? [...acc, i] : acc), []);
    if (!args) {
      return [{
        type: 'notice',
        text: starts.length
          ? `Turns you can go back to: ${starts.map((_, i) => i + 1).join(', ')}.`
          : 'Nothing to rewind: this chat has taken no turns.',
      }];
    }
    const turn = Number(args);
    if (!Number.isInteger(turn) || turn < 1 || turn > starts.length) {
      return [{ type: 'error', message: `no turn ${args} to rewind to` }];
    }
    task.transcript = items.slice(0, starts[turn - 1]);
    this._context = { tokens: 1200 * (turn - 1), window: 200000 };
    return [
      { type: 'transcript_reset', turn },
      { type: 'notice', text: `Rewound to before turn ${turn}. Files and conversation restored.` },
      { type: 'context', context: this._context },
    ];
  }

  /** @returns {Promise<SlashCommand[]>} */
  async commands() {
    return normalizeCommands(MOCK_COMMANDS);
  }

  /**
   * An upload with no disk behind it: the paths are the ones the real server
   * would have written to (an image under `~/.wizard/images/`, anything else
   * under `~/.wizard/attachments/`), and `kind` is decided here from the bytes'
   * media type — the one thing the client is never allowed to decide for real.
   * @param {string} id
   * @param {File[]} files
   * @returns {Promise<Attachment[]>}
   */
  async upload(id, files) {
    await new Promise((r) => setTimeout(r, 140)); // the round trip the real one has
    return Array.from(files).map((file) => {
      const image = /^image\//.test(file.type || '');
      const name = file.name || (image ? 'pasted.png' : 'file');
      return {
        path: `/home/you/.wizard/${image ? 'images' : 'attachments'}/${id}/${nextId('up')}-${name}`,
        name,
        mime: file.type || 'application/octet-stream',
        bytes: file.size || 0,
        kind: image ? 'image' : 'file',
      };
    });
  }

  planVerdict() {}

  interviewAnswers() {}

  cancel() {}

  /**
   * @param {{id: string, path: string}} task
   * @returns {Promise<GitInfo>}
   */
  async gitStatus(task) {
    const detail = this._data.tasks.get(task.id);
    if (!detail) throw new Error(`unknown task: ${task.id}`);
    return detail.git ?? { branch: 'main', additions: 0, deletions: 0, files: [] };
  }

  /**
   * A synthetic diff for one of the fixture's changed files: as many `+`/`-`
   * lines as its counts claim (bounded — the fixtures claim hundreds), so the
   * diff pane has something to render with no git repo behind it.
   * @param {{id: string, path: string}} task
   * @returns {Promise<FileDiff>}
   */
  async fileDiff(task, path) {
    const detail = this._data.tasks.get(task.id);
    const file = ((detail && detail.git && detail.git.files) || []).find((f) => f.path === path);
    if (!file) throw new Error(`'${path}' is not a changed file in this workspace`);
    const shown = (n) => Math.min(n || 0, 12);
    const deletions = shown(file.deletions);
    const additions = shown(file.additions);
    const lines = [
      { kind: 'ctx', text: ' fn main() {' },
      ...Array.from({ length: deletions }, (_, i) => ({ kind: 'del', text: `-    let was_${i + 1} = ();` })),
      ...Array.from({ length: additions }, (_, i) => ({ kind: 'add', text: `+    let now_${i + 1} = ();` })),
      { kind: 'ctx', text: ' }' },
    ];
    return {
      path,
      status: file.status || 'M',
      additions: file.additions || 0,
      deletions: file.deletions || 0,
      binary: false,
      truncated: additions < (file.additions || 0) || deletions < (file.deletions || 0),
      hunks: [{ header: `@@ -1,${deletions + 2} +1,${additions + 2} @@ fn main()`, lines }],
    };
  }

  /** @returns {Promise<WorkspaceRef[]>} */
  async workspaces() {
    return this._data.workspaces.map((ws, i) => ({
      cwd: ws.path,
      name: ws.name,
      taskCount: ws.tasks.length,
      home: i === 0,
    }));
  }

  async branches(task) {
    const detail = this._data.tasks.get(task.id);
    const current = (detail && detail.git && detail.git.branch) || 'main';
    return { current, branches: [current, 'main', 'feat/gui', 'fix/stream-timeouts'].filter((b, i, all) => all.indexOf(b) === i) };
  }

  async checkout(task, branch) {
    const detail = this._data.tasks.get(task.id);
    if (detail && detail.git) detail.git.branch = branch;
    return branch;
  }

  /** @returns {Promise<ModelInfo[]>} */
  async listModels() {
    return [
      { value: null, label: 'GLM-5.2', provider: 'zai', isDefault: true },
      { value: 'claude-sonnet-4-5', label: 'Sonnet 4.5', provider: 'anthropic' },
      { value: 'grok-4', label: 'Grok 4', provider: 'xai' },
    ];
  }

  /** Mock settings; `?mock=1&first-run=1` exercises the onboarding path. */
  async settings() {
    const firstRun = new URLSearchParams(window.location.search).has('first-run');
    return {
      first_run: firstRun || this._mockProviders.length === 0,
      config_path: '/home/you/.wizard/config.toml',
      credentials_path: '/home/you/.wizard/credentials.toml',
      active: this._mockProviders[0] ? this._mockProviders[0].name : null,
      max_steps: this._maxSteps,
      providers: this._mockProviders.map((p, i) => ({ ...p, active: i === 0 })),
      presets: MOCK_PRESETS,
    };
  }

  async saveSettings(patch) {
    // 0 is a value, not an absence: it is how the step limit is turned off.
    if (patch && Number.isInteger(patch.max_steps) && patch.max_steps >= 0) {
      this._maxSteps = patch.max_steps;
    }
    return this.settings();
  }

  async saveProvider({ name, kind, baseUrl, model }) {
    this._mockProviders = this._mockProviders.filter((p) => p.name !== name);
    this._mockProviders.unshift({ name, kind, base_url: baseUrl, model, key: 'stored' });
    return { settings: await this.settings(), probe: { ok: true, models: [model, `${model}-mini`] } };
  }

  async testProvider(name) {
    void name;
    return { ok: true, models: ['mock-model'] };
  }

  async activateProvider(name) {
    const i = this._mockProviders.findIndex((p) => p.name === name);
    if (i > 0) this._mockProviders.unshift(...this._mockProviders.splice(i, 1));
    return this.settings();
  }

  async removeProvider(name) {
    this._mockProviders = this._mockProviders.filter((p) => p.name !== name);
    return this.settings();
  }

  /**
   * A sign-in with no provider behind it. The URL goes nowhere on purpose — a
   * mock must not send a browser to a real consent screen — and the flow lands
   * as an OAuth provider on the second poll, which is enough to drive `/login`
   * and the Settings sign-in rows headlessly.
   */
  async beginSignIn(provider) {
    this._signIn = { state: 'pending', provider, polls: 0 };
    return `about:blank#mock-sign-in-${encodeURIComponent(provider)}`;
  }

  async signInStatus() {
    const flow = this._signIn;
    if (!flow) return { state: 'idle' };
    if (flow.polls++ < 1) return { state: 'pending', provider: flow.provider };
    if (flow.state === 'pending') {
      flow.state = 'done';
      const oauth = flow.provider === 'xai'
        ? { kind: 'xaioauth', base_url: 'https://api.x.ai/v1', model: 'grok-4' }
        : { kind: 'chatgptoauth', base_url: 'https://chatgpt.com/backend-api', model: 'gpt-5.2' };
      this._mockProviders = this._mockProviders.filter((p) => p.name !== flow.provider);
      this._mockProviders.unshift({ name: flow.provider, ...oauth, key: 'oauth' });
    }
    return { state: 'done', provider: flow.provider };
  }

  /**
   * An empty chat in the mock's own workspace, mirroring `RealApi.newChat`.
   * @returns {Promise<{id: string, cwd: string, workspace: string}>}
   */
  async newChat() {
    const ws = this._data.workspaces[0];
    const id = nextId('chat');
    this._data.tasks.set(id, {
      id,
      title: NEW_CHAT_TITLE,
      workspace: ws.name,
      path: ws.path,
      model: 'glm-5.2',
      status: 'idle',
      git: { branch: 'main', additions: 0, deletions: 0, files: [] },
      transcript: [],
    });
    return { id, cwd: ws.path, workspace: ws.name };
  }

  /**
   * Drip events to callbacks on a timer, then emit a final status (and, when
   * `doneReason` is given, a `done`), mirroring the live frame order.
   * @param {StreamCallbacks[] | (() => StreamCallbacks[])} targets
   * @param {Array<Object>} events
   * @param {TaskStatus} doneStatus
   * @param {string} [doneReason]
   */
  _play(targets, events, doneStatus, doneReason) {
    const resolve = () => (typeof targets === 'function' ? targets() : targets);
    let i = 0;
    const tick = () => {
      this._timers.delete(timer);
      const cbs = resolve();
      if (i >= events.length) {
        for (const cb of cbs) {
          if (doneReason && cb.onDone) cb.onDone(doneReason);
          if (cb.onStatus) cb.onStatus(doneStatus);
        }
        return;
      }
      const ev = events[i++];
      for (const cb of cbs) {
        if (ev.type === 'text' && cb.onText) cb.onText(ev.delta);
        else if (ev.type === 'tool_call' && cb.onToolCall) cb.onToolCall(ev.call);
        else if (ev.type === 'tool_result' && cb.onToolResult) cb.onToolResult(ev.result);
        else if (ev.type === 'status' && cb.onStatus) cb.onStatus(ev.status);
        else if (ev.type === 'context' && cb.onContext) cb.onContext(ev.context);
        else if (ev.type === 'usage' && cb.onUsage) cb.onUsage(ev.usage);
        else if (ev.type === 'todo' && cb.onTodo) cb.onTodo(ev.items);
        else if (ev.type === 'notice' && cb.onNotice) cb.onNotice(ev.text);
        else if (ev.type === 'transcript_reset' && cb.onTranscriptReset) cb.onTranscriptReset(ev.turn);
        else if (ev.type === 'error' && cb.onError) cb.onError(ev.message);
        else if (ev.type === 'subagent_run' && cb.onSubagentRun) cb.onSubagentRun(ev.info);
        else if (ev.type === 'subagent_text' && cb.onSubagentText) cb.onSubagentText(ev.run, ev.text);
        else if (ev.type === 'subagent_tool_call' && cb.onSubagentToolCall) cb.onSubagentToolCall(ev.run, ev.call);
        else if (ev.type === 'subagent_tool_result' && cb.onSubagentToolResult) cb.onSubagentToolResult(ev.run, ev.result);
        else if (ev.type === 'subagent_step' && cb.onSubagentStep) cb.onSubagentStep(ev.run, ev.step);
        else if (ev.type === 'subagent_done' && cb.onSubagentDone) cb.onSubagentDone(ev.run, ev.result);
      }
      timer = setTimeout(tick, ev.type === 'text' ? 26 : 220);
      this._timers.add(timer);
    };
    let timer = setTimeout(tick, 120);
    this._timers.add(timer);
  }
}

/* ------------------------------------------------------------------------ */
/* Factory                                                                   */
/* ------------------------------------------------------------------------ */

/**
 * The API implementation for this page load: `RealApi` by default,
 * `MockApi` with `?mock=1`.
 */
export function createApi() {
  const params = new URLSearchParams(window.location.search);
  return params.get('mock') === '1' ? new MockApi() : new RealApi();
}
