// Wizard GUI — app entry. Framework-free: the whole UI is rendered from a
// state object into the semantic skeleton in index.html. All data flows
// through the Api seam (api.js): RealApi (HTTP + one WebSocket per open
// task) by default, MockApi with `?mock=1`.

import { NEW_CHAT_TITLE, applyToolSummary, createApi, imageUrl, rememberImage } from './api.js';
import { icons, fileIconSvg } from './icons.js';

const api = createApi();

const state = {
  /** @type {import('./api.js').Workspace[]} */
  workspaces: [],
  /** The directory `wizard gui` was launched from; a new chat opens there. */
  home: { cwd: '', name: '' },
  /** Last `GET /api/settings`: providers, presets, first-run flag. */
  settings: null,
  /** @type {import('./api.js').ModelInfo[]} */
  models: [],
  /** Model override sent with the next user_message; null = task default. */
  modelId: null,
  modelLabel: null,
  selectedTaskId: null,
  /** @type {import('./api.js').TaskDetail | null} */
  task: null,
  /** @type {import('./api.js').TaskStatus['state']} */
  taskState: 'idle',
  /** @type {Array<{text:string,done:boolean,active?:boolean}>} */
  todos: [],
  /** `/todos` hid the progress section; it stays hidden until it is toggled back. */
  todosHidden: false,
  /** Session lifetime spend (the `usage` frames), which the Goal card reports. */
  usage: { prompt: 0, completion: 0 },
  /** Latest `context` frame: what the NEXT model call carries. A different
   *  number from `usage` above, and shown apart from it.
   *  @type {import('./api.js').ContextSize | null} */
  context: null,
  /** The `/` palette's commands, for this chat's workspace.
   *  @type {import('./api.js').SlashCommand[]} */
  commands: [],
  /** Files staged in the composer, to go up with the next message.
   *  @type {StagedFile[]} */
  attachments: [],
  /** @type {import('./api.js').GitInfo | null} */
  git: null,
  /** Elapsed label of the last finished turn ("3m 1s"). */
  lastWorked: null,
  /** This task's subagent runs, oldest first (the panel lists them the other
   *  way up). @type {SubagentRunView[]} */
  subagents: [],
};

/** @type {import('./api.js').StreamHandle | null} */
let streamHandle = null;
/** The in-flight turn's collapsible section: {section, body, labelEl, startedAt}. */
let liveTurn = null;
/** Turn start captured when the user hits send (beats the state frame). */
let pendingTurnStart = null;
/** True between socket open and the first frame: a `working` state then
 *  marks the start of a mid-turn buffer replay. */
let replayPending = false;
const reconnect = { attempts: 0, timer: null };
/** Transient "Retrying…" row, removed on the next frame. */
let transientNote = null;
let gitPoll = null;
let gitSeq = 0;
let selectSeq = 0;
/** The post-rewind refetch in flight; a later one wins. */
let resetSeq = 0;
/** System rows that arrive during that refetch, held for the transcript it is
 *  rebuilding. Null when no refetch is in flight. @type {Array<{text:string,cls:string|undefined}>|null} */
let resetRows = null;
/** Composer refs. */
let composerInput = null;
let modelLabelEl = null;
let attachTray = null;
let fileInput = null;
/** The open `/` palette: `{el, list, matches, index}`, or null. */
let palette = null;
/** The one open dropdown (model / directory / branch), if any. */
let menuEl = null;

/* ------------------------------------------------------------------------ */
/* DOM helpers                                                               */
/* ------------------------------------------------------------------------ */

const $ = (id) => document.getElementById(id);

/** Shortcut hints follow the platform: ⌘N on macOS, Ctrl-N everywhere else. */
const MOD_KEY = /mac/i.test(navigator.userAgentData?.platform || navigator.platform || '') ? '⌘' : 'Ctrl-';

/**
 * Hyperscript-style element builder.
 * @param {string} tag
 * @param {Object} [attrs] `class`, `dataset`, `html`, `on<event>` handlers, or plain attributes
 * @param {...(Node|string|Array|null|undefined|false)} children
 * @returns {HTMLElement}
 */
function h(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (value == null || value === false) continue;
    if (key === 'class') node.className = value;
    else if (key === 'dataset') Object.assign(node.dataset, value);
    else if (key === 'html') node.innerHTML = value;
    else if (key.startsWith('on') && typeof value === 'function') node.addEventListener(key.slice(2), value);
    else node.setAttribute(key, value === true ? '' : value);
  }
  for (const child of children.flat(Infinity)) {
    if (child == null || child === false) continue;
    node.append(child.nodeType ? child : document.createTextNode(child));
  }
  return node;
}

/** Inline SVG icon wrapped in a span so CSS can size/color it. */
const icon = (name, cls = 'icon') => h('span', { class: cls, html: icons[name] || '', 'aria-hidden': 'true' });

const iconBtn = (name, label, onclick, cls = '') =>
  h('button', { class: `icon-btn ${cls}`.trim(), type: 'button', title: label, 'aria-label': label, onclick }, icon(name));

/** Relative age label: 2m, 42m, 5h, 2d. */
function relAge(ts) {
  const mins = Math.max(1, Math.round((Date.now() - ts) / 60000));
  if (mins < 60) return `${mins}m`;
  const hours = Math.round(mins / 60);
  if (hours < 24) return `${hours}h`;
  return `${Math.round(hours / 24)}d`;
}

/** "42s", "3m 1s", "1h 12m". */
function fmtDur(secs) {
  if (secs < 60) return `${secs}s`;
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  if (m < 60) return s ? `${m}m ${s}s` : `${m}m`;
  const hr = Math.floor(m / 60);
  const mm = m % 60;
  return mm ? `${hr}h ${mm}m` : `${hr}h`;
}

/** "812", "89K", "1.2M". */
function fmtTokens(n) {
  const short = (x) => (x < 10 ? x.toFixed(1).replace(/\.0$/, '') : String(Math.round(x)));
  if (n < 1000) return String(n);
  if (n < 1e6) return `${short(n / 1000)}K`;
  return `${short(n / 1e6)}M`;
}

/** A path for a box that elides from the front (`direction: rtl`, so the file
 *  name — the part you are here for — survives). The mark pins the leading "/"
 *  of an absolute path, which the bidi algorithm otherwise carries to the far
 *  end of the line and renders as a trailing slash. */
const elidedPath = (path) => `\u200e${path}`;

/** "512 B", "50 KB", "2.4 MB" — a file size, as a file manager writes it. */
function fmtBytes(n) {
  if (n < 1024) return `${n} B`;
  const kb = n / 1024;
  if (kb < 1024) return `${Math.round(kb)} KB`;
  const mb = kb / 1024;
  return `${mb < 10 ? mb.toFixed(1) : Math.round(mb)} MB`;
}

const stateLabel = (s) =>
  ({ working: 'Working…', needs_input: 'Needs input', idle: 'Idle', complete: 'Complete', failed: 'Failed', connecting: '…' })[s] || s;

/* ------------------------------------------------------------------------ */
/* Sidebar                                                                   */
/* ------------------------------------------------------------------------ */

function resortWorkspaces() {
  for (const ws of state.workspaces) ws.tasks.sort((a, b) => b.updatedAt - a.updatedAt);
  state.workspaces.sort((a, b) => ((b.tasks[0] && b.tasks[0].updatedAt) || 0) - ((a.tasks[0] && a.tasks[0].updatedAt) || 0));
}

/** Patch a sidebar summary in place and re-render (live state dot / re-sort). */
function updateTaskSummary(id, { status, bump, title } = {}) {
  for (const ws of state.workspaces) {
    const task = ws.tasks.find((t) => t.id === id);
    if (!task) continue;
    if (status) task.status = status;
    if (title) task.title = title;
    if (bump) task.updatedAt = Date.now();
    break;
  }
  resortWorkspaces();
  renderSidebar();
}

function taskDot(t, selected) {
  if (t.status === 'working') return h('span', { class: 'task-dot working' });
  if (t.status === 'needs_input') return h('span', { class: 'task-dot attn' });
  if (t.status === 'failed') return h('span', { class: 'task-dot failed' });
  return selected ? h('span', { class: 'task-dot' }) : null;
}

function renderSidebar() {
  $('sidebar-top').replaceChildren(
    h('span', { class: 'brand' }, 'wizard'),
    iconBtn('gear', 'Settings', () => openSettings()),
  );

  $('sidebar-actions').replaceChildren(
    h('button', { class: 'side-row', type: 'button', onclick: () => newChatHere() },
      icon('plusSquare', 'icon side-row-icon'),
      h('span', { class: 'side-row-label' }, 'New Chat'),
      h('span', { class: 'side-row-hint' }, `${MOD_KEY}N`)),
  );

  $('tasks-header').replaceChildren(h('span', { class: 'tasks-title' }, 'Chats'));

  $('task-tree').replaceChildren(
    ...state.workspaces.map((ws) =>
      h('div', { class: 'ws-group' },
        h('div', { class: 'ws-head' }, icon('folder', 'icon ws-icon'), h('span', { class: 'ws-name', title: ws.path }, ws.name)),
        h('div', { class: 'ws-tasks' },
          ws.tasks.map((t) => {
            const selected = t.id === state.selectedTaskId;
            return h('button', {
              class: 'task-row' + (selected ? ' selected' : ''),
              type: 'button',
              title: t.title,
              onclick: () => selectTask(t.id),
            },
              h('span', { class: 'task-gutter' }, taskDot(t, selected)),
              h('span', { class: 'task-title' }, t.title),
              h('span', { class: 'task-age' }, relAge(t.updatedAt)));
          })))),
  );
}

/* ------------------------------------------------------------------------ */
/* Top bar                                                                   */
/* ------------------------------------------------------------------------ */

function renderTopbar() {
  const t = state.task;
  const branch = state.git && state.git.branch;

  const dirAnchor = h('span', { class: 'menu-anchor' });
  dirAnchor.append(h('button', {
    class: 'chip chip-repo', type: 'button',
    title: `${t ? t.path : ''}\nOpen a chat in another directory`,
    onclick: (e) => { e.stopPropagation(); openDirMenu(dirAnchor); },
  },
    icon('folder', 'icon chip-icon'),
    h('span', { class: 'chip-label' }, t ? t.workspace : '—'),
    icon('chevronDown', 'icon chip-caret')));

  const branchAnchor = h('span', { class: 'menu-anchor' });
  branchAnchor.append(h('button', {
    class: 'chip chip-branch', type: 'button', title: 'Switch branch',
    onclick: (e) => { e.stopPropagation(); openBranchMenu(branchAnchor); },
  },
    icon('branch', 'icon chip-icon'),
    h('span', { class: 'chip-label' }, branch),
    icon('chevronDown', 'icon chip-caret')));

  $('topbar').replaceChildren(
    h('div', { class: 'topbar-left' },
      iconBtn('panelLeft', 'Toggle chat list', () => $('app').classList.toggle('sidebar-collapsed')),
      h('h1', { class: 'topbar-title' }, t ? t.title : 'Wizard'),
      t && dirAnchor,
      // No branch chip outside a git repo: there is nothing to switch.
      t && branch && branchAnchor,
    ),
    h('div', { class: 'topbar-right' },
      iconBtn('panelRight', 'Toggle context panel', () => $('app').classList.toggle('panel-collapsed')),
    ),
  );
}

/* ------------------------------------------------------------------------ */
/* Transcript: shared row builders                                           */
/* ------------------------------------------------------------------------ */

const transcriptInner = () => $('transcript').querySelector('.transcript-inner');

/**
 * One streaming surface: where new content lands, plus the live refs a later
 * frame patches — the message mid-stream, the tool group being aggregated,
 * the rows waiting on their result, the card an `images` frame belongs on. The
 * main chat is one flow; an open subagent pane is another, so a subagent's rows
 * never land in — or aggregate with — the parent's.
 * @param {HTMLElement} scroller the element this flow scrolls in
 */
function newFlow(scroller) {
  return { scroller, target: null, md: null, think: null, group: null, rows: new Map(), lastTool: null };
}

/** The main chat's flow: the center transcript. */
const chat = newFlow($('transcript'));

function autoScroll(flow, force = false) {
  const scroller = flow.scroller;
  const nearBottom = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 180;
  if (force || nearBottom) scroller.scrollTop = scroller.scrollHeight;
}

/** Break the streaming text/thinking/tool-group flow (before a new block). */
function breakFlow(flow) {
  endStream(flow);
  flow.group = null;
  flow.lastTool = null;
  collapseThinking(flow);
}

/** Close the assistant message being streamed, if there is one: the last delta
 *  is landed now rather than on a frame that may never come. */
function endStream(flow) {
  if (!flow.md) return;
  endMarkdown(flow.md);
  flow.md = null;
}

function collapseThinking(flow) {
  if (flow.think) {
    flow.think.block.classList.add('collapsed');
    flow.think = null;
  }
}

/** A non-image attachment: what it is, on the message it went out with. */
function attachedFileChip(file) {
  return h('span', { class: 'file-chip', title: `${file.path || file.name} · ${fmtBytes(file.bytes || 0)}` },
    h('span', { class: 'file-chip-icon', html: fileIconSvg(file.name), 'aria-hidden': 'true' }),
    h('span', { class: 'file-chip-name' }, file.name));
}

/**
 * What you said, and what you sent with it. The images ride on the card rather
 * than in a strip below it: they were part of the message, not something the
 * turn produced.
 * @param {string} text
 * @param {import('./api.js').Attachment[]} [attachments]
 */
function appendPromptCard(text, attachments = []) {
  breakFlow(chat);
  const inner = transcriptInner();
  const images = attachments.filter((a) => a.kind === 'image' && a.path);
  const files = attachments.filter((a) => a.kind !== 'image' && a.path);
  // `h()` drops a null child, but would render a `0` — so these are ternaries,
  // not `length &&`.
  const card = h('div', { class: 'prompt-card' },
    text ? h('div', { class: 'prompt-text' }, text) : null,
    images.length ? h('div', { class: 'prompt-images' }, images.map(imageTile)) : null,
    files.length ? h('div', { class: 'prompt-files' }, files.map(attachedFileChip)) : null);
  inner.append(card);
  chat.target = inner;
}

/** Collapsible "Worked …" divider; returns its body (the new append target). */
function appendWorkedSection(label, live = false) {
  const labelEl = h('span', {}, label);
  const body = h('div', { class: 'worked-body' });
  const section = h('section', { class: 'worked-section' + (live ? ' live' : '') },
    h('button', {
      class: 'worked-head', type: 'button',
      onclick: () => section.classList.toggle('collapsed'),
    }, labelEl, icon('chevronDown', 'icon worked-caret')),
    h('div', { class: 'worked-rule' }),
    body);
  transcriptInner().append(section);
  chat.target = body;
  return { section, body, labelEl };
}

/** One whole assistant message: a replayed one, or a subagent's — which
 *  arrive per step rather than as deltas. Same markdown as the live stream. */
function appendMessage(flow, text) {
  breakFlow(flow);
  const root = h('div', { class: 'msg-text md' });
  renderMarkdownInto(root, text);
  flow.target.append(root);
}

function appendThinkingBlock(flow, text, collapsed) {
  const body = h('div', { class: 'thinking-body' }, text || '');
  const block = h('div', { class: 'thinking-block' + (collapsed ? ' collapsed' : '') },
    h('button', {
      class: 'thinking-head', type: 'button',
      onclick: () => block.classList.toggle('collapsed'),
    }, icon('chevronDown', 'icon thinking-caret'), h('span', {}, 'Thinking')),
    body);
  flow.target.append(block);
  return { block, body };
}

function appendSystemRow(flow, text, cls = '') {
  endStream(flow);
  flow.group = null;
  collapseThinking(flow);
  const row = h('div', { class: `system-row ${cls}`.trim() }, text);
  flow.target.append(row);
  autoScroll(flow);
  return row;
}

/* --- Tool rows ------------------------------------------------------------ */

const NOUN_PLURALS = { file: 'files', listing: 'listings', search: 'searches', 'git check': 'git checks' };

function countsText(counts) {
  return Array.from(counts)
    .map(([noun, n]) => `${n} ${n === 1 ? noun : NOUN_PLURALS[noun] || `${noun}s`}`)
    .join(', ');
}

function startExploreGroup(flow) {
  const counts = new Map();
  const sublist = h('div', { class: 'tool-sublist hidden' });
  const detail = h('span', { class: 'tool-args' }, '');
  const status = h('span', { class: 'tool-status hidden' }, 'Failed');
  const row = h('button', {
    class: 'tool-row tool-row-btn', type: 'button', title: 'Show the individual calls',
    onclick: () => sublist.classList.toggle('hidden'),
  }, icon('magnifier', 'icon tool-icon'), h('span', { class: 'tool-name' }, 'Explored'), detail, status);
  const card = h('div', { class: 'tool-group' }, row, sublist);
  flow.target.append(card);
  return { kind: 'explore', parent: flow.target, card, counts, sublist, detail, status };
}

function addExploreCall(flow, group, call) {
  const textEl = h('span', { class: 'subline-text' }, call.detail || '');
  const line = h('div', {
    class: 'tool-subline' + (call.status === 'pending' ? ' pending' : '') + (call.status === 'failed' ? ' failed' : ''),
  }, h('span', { class: 'subline-noun' }, call.noun), textEl);
  group.sublist.append(line);
  group.counts.set(call.noun, (group.counts.get(call.noun) || 0) + 1);
  group.detail.textContent = countsText(group.counts);
  if (call.status === 'failed') group.status.classList.remove('hidden');
  flow.rows.set(call.id, {
    update(result) {
      line.classList.remove('pending');
      if (result.summary) textEl.textContent = result.summary;
      if (result.status === 'failed') {
        line.classList.add('failed');
        group.status.classList.remove('hidden');
      }
    },
  });
}

function startWriteGroup(flow) {
  const addEl = h('span', { class: 'diffstat-add hidden' });
  const delEl = h('span', { class: 'diffstat-del hidden' });
  const status = h('span', { class: 'tool-status hidden' }, 'Failed');
  const row = h('div', { class: 'tool-row' },
    icon('pencil', 'icon tool-icon'), h('span', { class: 'tool-name' }, 'Wrote'), addEl, delEl, status);
  flow.target.append(row);
  const group = { kind: 'write', parent: flow.target, row, addEl, delEl, status, totals: { add: 0, del: 0 } };
  group.bump = (diffstat) => {
    if (!diffstat) return;
    group.totals.add += diffstat.additions || 0;
    group.totals.del += diffstat.deletions || 0;
    if (group.totals.add) { addEl.textContent = `+${group.totals.add}`; addEl.classList.remove('hidden'); }
    if (group.totals.del) { delEl.textContent = `-${group.totals.del}`; delEl.classList.remove('hidden'); }
  };
  return group;
}

function addWriteCall(flow, group, call) {
  const files = call.files && call.files.length ? call.files : [{ name: call.name || 'file' }];
  const chips = files.map((f) =>
    h('button', {
      class: 'file-chip' + (call.status === 'pending' ? ' pending' : '') + (call.status === 'failed' ? ' failed' : ''),
      type: 'button', title: f.path || f.name,
    },
      h('span', { class: 'file-chip-icon', html: fileIconSvg(f.name), 'aria-hidden': 'true' }),
      h('span', { class: 'file-chip-name' }, f.name)));
  for (const chip of chips) group.status.before(chip);
  group.bump(call.diffstat);
  if (call.status === 'failed') group.status.classList.remove('hidden');
  flow.rows.set(call.id, {
    update(result) {
      for (const chip of chips) chip.classList.remove('pending');
      if (result.status === 'failed') {
        for (const chip of chips) chip.classList.add('failed');
        group.status.classList.remove('hidden');
      }
    },
  });
}

/** One tool row of its own. Returns the row: the card its images land on. */
function appendStandaloneTool(flow, call) {
  const row = h('div', { class: 'tool-row' + (call.status === 'pending' ? ' pending' : ''), dataset: { callId: call.id || '' } });
  let detailEl = null;
  if (call.tool === 'run') {
    row.append(h('span', { class: 'tool-name' }, call.title || 'Ran'), h('code', { class: 'tool-cmd' }, call.command || ''));
  } else if (call.tool === 'search') {
    detailEl = h('span', { class: 'tool-args' }, call.detail || '');
    row.append(icon('globe', 'icon tool-icon'), h('span', { class: 'tool-name' }, call.title || 'Searched'), detailEl);
  } else if (call.tool === 'delegate') {
    detailEl = h('span', { class: 'tool-args' }, call.detail || '');
    row.append(icon('agents', 'icon tool-icon'), h('span', { class: 'tool-name' }, call.title || 'Delegated'), detailEl);
  } else if (call.tool === 'explore') {
    // Pre-aggregated row (mock fixtures): one line, no sublist.
    detailEl = h('span', { class: 'tool-args' }, call.detail || '');
    row.append(icon('magnifier', 'icon tool-icon'), h('span', { class: 'tool-name' }, call.title || 'Explored'), detailEl);
  } else if (call.tool === 'write') {
    row.append(icon('pencil', 'icon tool-icon'), h('span', { class: 'tool-name' }, call.title || 'Wrote'));
    for (const f of call.files || []) {
      row.append(h('button', { class: 'file-chip', type: 'button', title: f.path || f.name },
        h('span', { class: 'file-chip-icon', html: fileIconSvg(f.name), 'aria-hidden': 'true' }),
        h('span', { class: 'file-chip-name' }, f.name)));
    }
  } else {
    detailEl = h('span', { class: 'tool-args' }, call.detail || '');
    row.append(h('span', { class: 'tool-name mono-name' }, call.title || call.name || 'tool'), detailEl);
  }
  const status = h('span', { class: 'tool-status' + (call.status === 'failed' ? '' : ' hidden') }, 'Failed');
  row.append(status);
  flow.target.append(row);
  flow.rows.set(call.id, {
    update(result) {
      row.classList.remove('pending');
      if (detailEl && result.summary) detailEl.textContent = result.summary;
      if (result.status === 'failed') status.classList.remove('hidden');
    },
  });
  return row;
}

/**
 * Append one tool call, aggregating consecutive explore calls into a single
 * "Explored" row (counts + expandable sublist) and consecutive writes into a
 * single "Wrote" row (file chips + running diffstat).
 * @param {Object} flow
 * @param {import('./api.js').ToolCall} call
 */
function appendToolCall(flow, call) {
  collapseThinking(flow);
  endStream(flow);
  let card;
  if (call.tool === 'explore' && call.noun) {
    if (!(flow.group && flow.group.kind === 'explore' && flow.group.parent === flow.target)) {
      flow.group = startExploreGroup(flow);
    }
    addExploreCall(flow, flow.group, call);
    card = flow.group.card;
  } else if (call.tool === 'write' && call.name) {
    if (!(flow.group && flow.group.kind === 'write' && flow.group.parent === flow.target)) {
      flow.group = startWriteGroup(flow);
    }
    addWriteCall(flow, flow.group, call);
    card = flow.group.row;
  } else {
    flow.group = null;
    card = appendStandaloneTool(flow, call);
  }
  // Images a tool returns arrive right after its `tool_finished` — the protocol
  // orders them that way rather than giving them a call id — so the card just
  // laid down is the one they belong on.
  flow.lastTool = { name: call.name, card };
  autoScroll(flow);
}

function onToolResult(flow, result) {
  const row = flow.rows.get(result.callId);
  if (row) row.update(result);
}

/* --- Images --------------------------------------------------------------- */

/**
 * One image, as a thumbnail that opens it full size. The box is sized by CSS
 * before the file arrives — the frame carries no dimensions, only bytes — so a
 * 2048px render neither blows the column nor shoves the transcript down as it
 * decodes. A file that is gone says so and names itself, rather than leaving a
 * blank square behind.
 * @param {import('./api.js').ImageRef} image
 */
function imageTile(image) {
  const img = h('img', {
    class: 'image-thumb-img', src: imageUrl(image), alt: '', loading: 'lazy', decoding: 'async',
  });
  const tile = h('button', {
    class: 'image-thumb', type: 'button', title: `${image.path} — open full size`,
    onclick: () => openImagePane(image),
  }, img);
  img.addEventListener('error', () => {
    tile.classList.add('broken');
    tile.disabled = true;
    tile.title = image.path;
    tile.replaceChildren(
      icon('image', 'icon image-broken-icon'),
      h('span', {}, 'Image missing'),
      h('span', { class: 'image-broken-path mono' }, elidedPath(image.path)));
  });
  return tile;
}

/**
 * An `images` frame (or its replayed item): the model's own images go inline
 * where its text is; a tool's go on that tool's card, which the frame's arrival
 * order identifies. A batch whose tool card is not the last one laid down — a
 * transcript truncated mid-turn, a tool row the client hides — still shows its
 * images, in the flow, rather than dropping them.
 * @param {Object} flow
 * @param {import('./api.js').ImageBatch} batch
 */
function appendImages(flow, batch) {
  const onCard = batch.source === 'tool' && flow.lastTool && flow.lastTool.name === batch.tool;
  const strip = h('div', { class: 'image-strip' + (onCard ? ' tool-images' : '') },
    batch.images.map(imageTile));
  if (onCard) {
    flow.lastTool.card.after(strip);
  } else {
    endStream(flow);
    collapseThinking(flow);
    flow.target.append(strip);
  }
  autoScroll(flow);
}

/* ------------------------------------------------------------------------ */
/* Markdown                                                                  */
/* ------------------------------------------------------------------------ */

/* The one markdown renderer: assistant messages, subagent messages, plan and
   interview cards all come through here.

   Injection-safe by construction. Every element is built with `h()` and every
   scrap of model text lands in a text node — `innerHTML` is never handed model
   output — so a reply containing `<script>alert(1)</script>` renders as those
   characters. Link targets are model output too, and are checked against a
   scheme allowlist before they are ever put in an `href`.

   The parse is block-first, and every block keeps the exact source lines it came
   from (`.src`). That is what makes the streaming path cheap: a delta re-parses
   the message, but only the blocks whose source actually changed get redrawn.
   See `syncMarkdown`. */

/** ``` or ~~~, up to three spaces in, with an optional info string (the language). */
const FENCE_RE = /^ {0,3}(`{3,}|~{3,})[ \t]*(\S*)/;
const HEADING_RE = /^ {0,3}(#{1,6})[ \t]+(.*?)[ \t]*#*[ \t]*$/;
const HR_RE = /^ {0,3}(?:-{3,}|\*{3,}|_{3,})[ \t]*$/;
const QUOTE_RE = /^ {0,3}> ?(.*)$/;
/** A bullet (`-`, `*`, `+`) or a number (`1.`, `1)`), and what follows it. */
const ITEM_RE = /^([ \t]*)(?:([-*+])|(\d{1,9})[.)])(?:[ \t]+(.*))?$/;

/** Leading whitespace in columns, a tab being four of them. */
function indentOf(line) {
  let cols = 0;
  for (const ch of line) {
    if (ch === ' ') cols += 1;
    else if (ch === '\t') cols += 4;
    else break;
  }
  return cols;
}

/** Drop up to `cols` columns of leading whitespace. */
function dedent(line, cols) {
  let i = 0;
  let col = 0;
  while (i < line.length && col < cols) {
    if (line[i] === ' ') col += 1;
    else if (line[i] === '\t') col += 4;
    else break;
    i += 1;
  }
  return line.slice(i);
}

/** A table row's cells: split on unescaped pipes, minus the optional fencing
 *  pipes at either end, which delimit rather than open a cell. */
function splitRow(line) {
  const s = line.trim();
  const cells = [];
  let cur = '';
  for (let i = 0; i < s.length; i += 1) {
    if (s[i] === '\\' && s[i + 1] === '|') {
      cur += '|';
      i += 1;
    } else if (s[i] === '|') {
      cells.push(cur);
      cur = '';
    } else {
      cur += s[i];
    }
  }
  cells.push(cur);
  if (cells.length > 1 && s.startsWith('|') && !cells[0].trim()) cells.shift();
  if (cells.length > 1 && s.endsWith('|') && !cells[cells.length - 1].trim()) cells.pop();
  return cells.map((c) => c.trim());
}

/** The row of dashes under a header is what makes a pipe table a table. */
function isAlignRow(line) {
  if (!line.includes('-') || !/^[\s|:-]+$/.test(line)) return false;
  const cells = splitRow(line);
  return cells.length > 0 && cells.every((c) => /^:?-+:?$/.test(c));
}

/** `:--` left, `--:` right, `:-:` center, `---` unset. */
function cellAlign(spec) {
  const left = spec.startsWith(':');
  const right = spec.endsWith(':');
  if (left && right) return 'center';
  if (right) return 'right';
  if (left) return 'left';
  return '';
}

/** True if a line would open some block other than a paragraph — i.e. it ends
 *  the paragraph above it even without a blank line between them. */
function startsBlock(lines, i) {
  const line = lines[i];
  if (FENCE_RE.test(line) || HEADING_RE.test(line) || HR_RE.test(line)) return true;
  if (QUOTE_RE.test(line) || ITEM_RE.test(line)) return true;
  return line.includes('|') && i + 1 < lines.length && isAlignRow(lines[i + 1]);
}

/**
 * Markdown source → blocks. Each block carries the exact source it was parsed
 * from, so two parses can be diffed against each other cheaply.
 * @param {string} md
 * @returns {Array<Object>}
 */
function parseBlocks(md) {
  const lines = String(md).replace(/\r\n?/g, '\n').split('\n');
  const blocks = [];
  let i = 0;
  const since = (from) => lines.slice(from, i).join('\n');

  while (i < lines.length) {
    if (!lines[i].trim()) { i += 1; continue; } // blank lines only separate blocks
    const start = i;
    const line = lines[i];

    const fence = FENCE_RE.exec(line);
    if (fence) {
      // Closed by a fence of the same character, at least as long as the opener.
      const close = new RegExp(`^ {0,3}${fence[1][0] === '`' ? '`' : '~'}{${fence[1].length},}[ \t]*$`);
      const body = [];
      i += 1;
      while (i < lines.length && !close.test(lines[i])) body.push(lines[i++]);
      if (i < lines.length) i += 1; // the closing fence — absent mid-stream
      blocks.push({ kind: 'code', lang: fence[2], code: body.join('\n'), src: since(start) });
      continue;
    }

    const heading = HEADING_RE.exec(line);
    if (heading) {
      i += 1;
      blocks.push({ kind: 'heading', level: heading[1].length, text: heading[2], src: since(start) });
      continue;
    }

    if (HR_RE.test(line)) {
      i += 1;
      blocks.push({ kind: 'hr', src: since(start) });
      continue;
    }

    if (QUOTE_RE.test(line)) {
      const inner = [];
      while (i < lines.length && lines[i].trim() && !HR_RE.test(lines[i])) {
        const m = QUOTE_RE.exec(lines[i]);
        inner.push(m ? m[1] : lines[i]); // a lazy continuation still belongs to the quote
        i += 1;
      }
      // The quote's content is markdown in its own right.
      blocks.push({ kind: 'quote', inner: inner.join('\n'), src: since(start) });
      continue;
    }

    if (line.includes('|') && i + 1 < lines.length && isAlignRow(lines[i + 1])) {
      const head = splitRow(line);
      const align = splitRow(lines[i + 1]).map(cellAlign);
      const rows = [];
      i += 2;
      while (i < lines.length && lines[i].trim() && lines[i].includes('|') && !isAlignRow(lines[i])) {
        rows.push(splitRow(lines[i]));
        i += 1;
      }
      blocks.push({ kind: 'table', head, align, rows, src: since(start) });
      continue;
    }

    if (ITEM_RE.test(line)) {
      const base = indentOf(line);
      const numbered = !ITEM_RE.exec(line)[2];
      // Switching marker — a `-` list under a `1.` list — starts a new list, not
      // another item of this one.
      const switched = (l) => {
        const m = ITEM_RE.exec(l);
        return m && indentOf(l) <= base && !m[2] !== numbered;
      };
      while (i < lines.length) {
        const l = lines[i];
        if (!l.trim()) {
          // A blank line ends the list unless the list carries on beneath it.
          const next = lines[i + 1];
          if (!next || !next.trim()) break;
          if (!ITEM_RE.test(next) && indentOf(next) <= base) break;
          if (switched(next)) break;
          i += 1;
          continue;
        }
        if (switched(l)) break;
        const item = ITEM_RE.exec(l);
        if (item ? indentOf(l) < base : indentOf(l) <= base) break;
        i += 1;
      }
      blocks.push({ kind: 'list', ...parseList(lines.slice(start, i)), src: since(start) });
      continue;
    }

    const para = [];
    while (i < lines.length && lines[i].trim()) {
      if (para.length && startsBlock(lines, i)) break;
      para.push(lines[i]);
      i += 1;
    }
    blocks.push({ kind: 'para', text: para.join('\n'), src: since(start) });
  }
  return blocks;
}

/**
 * A list block's items. An item's body is everything under its marker —
 * continuation lines and nested lists alike — dedented to the marker's content
 * column, which makes it a little markdown document of its own. Rendering
 * recurses into it, and that is how nesting (and a code block inside a bullet)
 * comes out right.
 */
function parseList(lines) {
  const first = ITEM_RE.exec(lines[0]);
  const ordered = !first[2];
  const base = indentOf(lines[0]);
  const items = [];
  let cur = null;
  let content = 0;
  for (const line of lines) {
    const m = ITEM_RE.exec(line);
    if (m && indentOf(line) <= base) {
      cur = [m[4] || ''];
      items.push(cur);
      content = indentOf(line) + (m[2] ? m[2].length : m[3].length + 1) + 1;
      continue;
    }
    if (cur) cur.push(dedent(line, content));
  }
  return { ordered, start: ordered ? Number(first[3]) : 1, items: items.map((l) => l.join('\n')) };
}

/* --- Inline --------------------------------------------------------------- */

const ESCAPABLE = /[\\`*_{}[\]()#+\-.!|~<>]/;
const CODE_SPAN = /^(`+)([\s\S]*?)\1(?!`)/;
const STRONG_STAR = /^\*\*(?=\S)([\s\S]*?\S)\*\*/;
const STRONG_UNDER = /^__(?=\S)([\s\S]*?\S)__/;
const EM_STAR = /^\*(?=\S)([\s\S]*?\S)\*(?!\*)/;
const EM_UNDER = /^_(?=\S)([\s\S]*?\S)_(?!_)/;
const STRIKE = /^~~(?=\S)([\s\S]*?\S)~~/;
/** `[text](url)`, or `![alt](url)`. The URL may carry balanced parens. */
const LINK = /^(!?)\[((?:\\.|[^\][\\])*)\]\([ \t]*((?:[^\s()\\]|\\.|\([^\s()]*\))*)(?:[ \t]+"([^"]*)")?[ \t]*\)/;
/** A bare URL in prose. It stops before trailing punctuation, which is a
 *  sentence's, not the URL's. */
const AUTOLINK = /^https?:\/\/[^\s<>[\]()"'`]+[^\s<>[\]()"'`.,;:!?]/;

/** The schemes we will hand a browser. Anything else — `javascript:`, `data:`,
 *  `vbscript:` — is not a link, and its source text is shown instead. */
const SAFE_SCHEME = /^(?:https?:\/\/|mailto:|tel:)/i;

/**
 * A link target out of a model, or null if it is not one we will follow.
 * @param {string} raw
 * @returns {string|null}
 */
function safeHref(raw) {
  const url = raw.trim().replace(/^<([\s\S]*)>$/, '$1');
  // `java&#9;script:` and friends: a browser strips control characters before
  // resolving the scheme, so the check has to look at what it will actually see.
  const probe = url.replace(/[\u0000-\u0020\u00a0]/g, '');
  return SAFE_SCHEME.test(probe) ? url : null;
}

/**
 * Inline markdown, in one left-to-right pass. Code spans win over everything
 * (their content is literal), then links, then emphasis.
 * @param {string} text
 * @param {boolean} [inLink] inside an `<a>` already: no nested links
 * @returns {Array<Node|string>} children for the enclosing block
 */
function inlineNodes(text, inLink = false) {
  const src = String(text);
  const out = [];
  let buf = '';
  const flush = () => {
    if (buf) out.push(buf);
    buf = '';
  };
  let i = 0;
  while (i < src.length) {
    const c = src[i];
    const rest = src.slice(i);

    // A backslash escape is the author saying "this one is a literal".
    if (c === '\\' && ESCAPABLE.test(src[i + 1] || '')) {
      buf += src[i + 1];
      i += 2;
      continue;
    }

    // A newline inside a paragraph is a line break. CommonMark would make it a
    // space, but a model that writes lines means lines — collapsing them into
    // one run-on paragraph is the bug this renderer exists to kill.
    if (c === '\n') {
      flush();
      out.push(h('br'));
      i += 1;
      continue;
    }

    if (c === '`') {
      const m = CODE_SPAN.exec(rest);
      if (m && m[2]) {
        flush();
        // One padding space either side is a fence for backticks, not content.
        const body = /^ .* $/.test(m[2]) ? m[2].slice(1, -1) : m[2];
        out.push(h('code', { class: 'md-code' }, body));
        i += m[0].length;
        continue;
      }
    }

    if (!inLink && (c === '[' || (c === '!' && src[i + 1] === '['))) {
      const m = LINK.exec(rest);
      if (m) {
        flush();
        const href = safeHref(m[3]);
        if (href) {
          // Model output: it opens in its own tab and cannot reach back into ours.
          // An image (`![alt](…)`) links rather than loads — a transcript does not
          // fetch from wherever a model points it.
          out.push(h('a', {
            class: 'md-link', href, target: '_blank', rel: 'noopener noreferrer',
            title: m[4] || null,
          }, ...inlineNodes(m[2], true)));
        } else {
          out.push(m[0]); // not a scheme we will follow: it says what it says
        }
        i += m[0].length;
        continue;
      }
    }

    if (!inLink && c === 'h' && AUTOLINK.test(rest)) {
      const url = AUTOLINK.exec(rest)[0];
      flush();
      out.push(h('a', { class: 'md-link', href: url, target: '_blank', rel: 'noopener noreferrer' }, url));
      i += url.length;
      continue;
    }

    if (c === '~') {
      const m = STRIKE.exec(rest);
      if (m) {
        flush();
        out.push(h('del', {}, ...inlineNodes(m[1], inLink)));
        i += m[0].length;
        continue;
      }
    }

    if (c === '*' || c === '_') {
      // `snake_case` is a word, not emphasis: an underscore mid-word is a character.
      const intraword = c === '_' && /\w/.test(src[i - 1] || '');
      if (!intraword) {
        const strong = (c === '*' ? STRONG_STAR : STRONG_UNDER).exec(rest);
        if (strong) {
          flush();
          out.push(h('strong', {}, ...inlineNodes(strong[1], inLink)));
          i += strong[0].length;
          continue;
        }
        const em = (c === '*' ? EM_STAR : EM_UNDER).exec(rest);
        if (em) {
          flush();
          out.push(h('em', {}, ...inlineNodes(em[1], inLink)));
          i += em[0].length;
          continue;
        }
      }
    }

    buf += c;
    i += 1;
  }
  flush();
  return out;
}

/* --- Blocks → DOM --------------------------------------------------------- */

/** A fenced block. The info string is labelled rather than dropped: it is the
 *  one part of a code block that says what you are looking at. */
function renderCode(block) {
  const pre = h('pre', { class: 'md-pre' }, h('code', {}, block.code));
  if (!block.lang) return pre;
  return h('div', { class: 'md-code-block' },
    h('div', { class: 'md-code-lang mono' }, block.lang),
    pre);
}

/** A GFM pipe table. It scrolls inside its own box — a wide table must not push
 *  the transcript column sideways. Cells past the header's width are dropped and
 *  missing ones filled, which is what the alignment row promised. */
function renderTable(block) {
  const cell = (tag, text, i) =>
    h(tag, { class: block.align[i] ? `md-al-${block.align[i]}` : null }, ...inlineNodes(text || ''));
  return h('div', { class: 'md-table-wrap' },
    h('table', { class: 'md-table' },
      h('thead', {}, h('tr', {}, ...block.head.map((c, i) => cell('th', c, i)))),
      h('tbody', {}, ...block.rows.map((row) =>
        h('tr', {}, ...block.head.map((_, i) => cell('td', row[i], i)))))));
}

/** One list item. Its body is markdown, so it can hold a nested list or a code
 *  block; a plain one is put straight into the `<li>` rather than wrapped in a
 *  `<p>` that would space the whole list out. */
function renderListItem(src) {
  const li = h('li');
  const blocks = parseBlocks(src);
  if (blocks.length && blocks[0].kind === 'para') {
    li.append(...inlineNodes(blocks.shift().text));
  }
  for (const block of blocks) li.append(renderBlock(block));
  return li;
}

function renderBlock(block) {
  switch (block.kind) {
    case 'heading':
      return h(`h${block.level}`, {}, ...inlineNodes(block.text));
    case 'hr':
      return h('hr');
    case 'code':
      return renderCode(block);
    case 'table':
      return renderTable(block);
    case 'quote': {
      const quote = h('blockquote');
      for (const inner of parseBlocks(block.inner)) quote.append(renderBlock(inner));
      return quote;
    }
    case 'list': {
      const list = h(block.ordered ? 'ol' : 'ul', { start: block.start !== 1 ? String(block.start) : null });
      for (const item of block.items) list.append(renderListItem(item));
      return list;
    }
    default:
      return h('p', {}, ...inlineNodes(block.text));
  }
}

/**
 * Render markdown into `root`, replacing whatever was there. `root` must carry
 * the `md` class for the stylesheet to reach it.
 * @param {HTMLElement} root
 * @param {string} md
 */
function renderMarkdownInto(root, md) {
  root.replaceChildren(...parseBlocks(md).map(renderBlock));
}

/* --- Streaming ------------------------------------------------------------ */

/**
 * A message still being written: the markdown source received so far, and the
 * blocks currently on screen. A delta re-renders only the blocks whose source
 * changed — in a stream that is the last one — so finished paragraphs, tables
 * and code blocks keep the very same DOM nodes, and a selection inside them
 * survives the next token.
 * @param {HTMLElement} target where the message goes
 * @param {string} cls the root's classes
 */
function newMarkdownView(target, cls) {
  const view = { root: h('div', { class: cls }), src: '', blocks: [], frame: 0, after: null };
  target.append(view.root);
  return view;
}

/** Take a delta. The repaint is one per animation frame, not one per token. */
function pushMarkdown(view, delta, after) {
  view.src += delta;
  view.after = after;
  if (view.frame) return;
  view.frame = requestAnimationFrame(() => {
    view.frame = 0;
    syncMarkdown(view);
    if (view.after) view.after();
  });
}

/** Reconcile the DOM with the source: keep the leading blocks whose source has
 *  not changed, redraw from the first that has. */
function syncMarkdown(view) {
  const next = parseBlocks(view.src);
  const prev = view.blocks;
  let keep = 0;
  while (keep < next.length && keep < prev.length && next[keep].src === prev[keep].src) {
    next[keep].node = prev[keep].node; // untouched: its DOM node, and any selection in it, stands
    keep += 1;
  }
  for (let i = keep; i < prev.length; i += 1) prev[i].node.remove();
  for (let i = keep; i < next.length; i += 1) {
    next[i].node = renderBlock(next[i]);
    view.root.append(next[i].node);
  }
  view.blocks = next;
}

/** The message is complete: land the last delta and stop. */
function endMarkdown(view) {
  if (view.frame) cancelAnimationFrame(view.frame);
  view.frame = 0;
  syncMarkdown(view);
  view.root.classList.remove('streaming');
}

/* --- Plan review / interview cards ---------------------------------------- */

function onPlan(plan) {
  breakFlow(chat);
  const id = state.selectedTaskId;
  const body = h('div', { class: 'plan-body md' });
  renderMarkdownInto(body, plan);
  const note = h('div', { class: 'note hidden' });
  const feedback = h('textarea', {
    class: 'input plan-feedback hidden', rows: '2',
    placeholder: 'What should change? (sent back to the agent)',
  });
  const approveBtn = h('button', { class: 'btn primary', type: 'button' }, 'Approve');
  const rejectBtn = h('button', { class: 'btn ghost', type: 'button' }, 'Reject');
  const actions = h('div', { class: 'card-actions' }, approveBtn, rejectBtn);
  const card = h('div', { class: 'gate-card plan-card' },
    h('div', { class: 'gate-head' }, icon('clipboard', 'icon gate-icon'), h('span', {}, 'Plan — awaiting approval')),
    body, feedback, actions, note);
  const resolve = (label) => {
    actions.remove();
    feedback.remove();
    note.textContent = label;
    note.classList.remove('hidden');
    card.classList.add('resolved');
  };
  const fail = (err) => {
    note.textContent = String((err && err.message) || err);
    note.classList.remove('hidden');
  };
  approveBtn.onclick = () => {
    try { api.planVerdict(id, true); resolve('Plan approved'); } catch (err) { fail(err); }
  };
  rejectBtn.onclick = () => {
    if (feedback.classList.contains('hidden')) {
      feedback.classList.remove('hidden');
      rejectBtn.textContent = 'Send rejection';
      feedback.focus();
      return;
    }
    try { api.planVerdict(id, false, feedback.value.trim()); resolve('Plan rejected'); } catch (err) { fail(err); }
  };
  chat.target.append(card);
  autoScroll(chat, true);
}

function onInterview(questions) {
  breakFlow(chat);
  const id = state.selectedTaskId;
  const inputs = questions.map((q) =>
    h('textarea', { class: 'input iv-answer', rows: '1', placeholder: 'Your answer (optional)', 'aria-label': q }));
  const rows = questions.map((q, i) =>
    h('div', { class: 'iv-q' }, h('div', { class: 'iv-question' }, q), inputs[i]));
  const note = h('div', { class: 'note hidden' });
  const sendBtn = h('button', { class: 'btn primary', type: 'button' }, 'Send answers');
  const skipBtn = h('button', { class: 'btn ghost', type: 'button' }, 'Skip');
  const actions = h('div', { class: 'card-actions' }, sendBtn, skipBtn);
  const card = h('div', { class: 'gate-card interview-card' },
    h('div', { class: 'gate-head' }, icon('question', 'icon gate-icon'), h('span', {}, 'The agent has questions')),
    ...rows, actions, note);
  const resolve = (label) => {
    actions.remove();
    for (const input of inputs) input.setAttribute('disabled', '');
    note.textContent = label;
    note.classList.remove('hidden');
    card.classList.add('resolved');
  };
  const fail = (err) => {
    note.textContent = String((err && err.message) || err);
    note.classList.remove('hidden');
  };
  sendBtn.onclick = () => {
    try { api.interviewAnswers(id, inputs.map((i) => i.value.trim())); resolve('Answers sent'); } catch (err) { fail(err); }
  };
  skipBtn.onclick = () => {
    try { api.interviewAnswers(id, null); resolve('Interview skipped'); } catch (err) { fail(err); }
  };
  chat.target.append(card);
  autoScroll(chat, true);
}

/* ------------------------------------------------------------------------ */
/* Transcript: replay + live turn                                            */
/* ------------------------------------------------------------------------ */

/** Reset the chat flow onto a fresh transcript body. */
function resetChatFlow(inner) {
  // Anything mid-stream belongs to the transcript being replaced: cancel its
  // pending frame rather than paint it into a tree nobody will see again.
  if (chat.md && chat.md.frame) cancelAnimationFrame(chat.md.frame);
  chat.target = inner;
  chat.md = null;
  chat.think = null;
  chat.group = null;
  chat.lastTool = null;
  chat.rows = new Map();
  liveTurn = null;
}

function renderTranscript() {
  const scroller = $('transcript');
  const inner = h('div', { class: 'transcript-inner' });
  scroller.replaceChildren(inner);
  resetChatFlow(inner);
  if (!state.task) return;

  for (const item of state.task.transcript) {
    if (item.type === 'user') {
      appendPromptCard(item.text, item.attachments);
    } else if (item.type === 'worked') {
      breakFlow(chat);
      appendWorkedSection(item.label || 'Worked');
    } else if (item.type === 'text') {
      appendMessage(chat, item.text);
    } else if (item.type === 'thinking') {
      endStream(chat);
      chat.group = null;
      appendThinkingBlock(chat, item.text, true);
    } else if (item.type === 'tool') {
      appendToolCall(chat, item);
    } else if (item.type === 'images') {
      appendImages(chat, item);
    } else if (item.type === 'notice') {
      appendSystemRow(chat, item.text);
    }
  }
  breakFlow(chat);
  scroller.scrollTop = scroller.scrollHeight;
}

/** Drop everything after the last prompt card (the in-flight turn's partial
 *  replay); the WebSocket buffer replay rebuilds it. */
function truncateAfterLastPrompt() {
  const inner = transcriptInner();
  if (!inner) return;
  const cards = inner.querySelectorAll(':scope > .prompt-card');
  const last = cards[cards.length - 1];
  if (!last) {
    inner.replaceChildren();
  } else {
    while (last.nextSibling) last.nextSibling.remove();
  }
  resetChatFlow(inner);
}

function beginLiveTurn() {
  liveTurn = null; // any previous section was finalized or truncated
  breakFlow(chat);
  const { section, body, labelEl } = appendWorkedSection('Working…', true);
  liveTurn = { section, body, labelEl, startedAt: pendingTurnStart || Date.now() };
  pendingTurnStart = null;
  chat.rows = new Map();
  autoScroll(chat, true);
}

const DONE_SUFFIX = { cancelled: ' (cancelled)', max_steps: ' (step limit)', error: ' (error)' };

function finalizeLiveTurn(reason) {
  if (!liveTurn) return;
  const secs = Math.max(1, Math.round((Date.now() - liveTurn.startedAt) / 1000));
  state.lastWorked = fmtDur(secs);
  liveTurn.labelEl.textContent = `Worked for ${state.lastWorked}${DONE_SUFFIX[reason] || ''}`;
  liveTurn.section.classList.remove('live');
  liveTurn = null;
}

/* --- Stream callbacks ------------------------------------------------------ */

function onText(delta) {
  collapseThinking(chat);
  chat.group = null;
  if (!chat.md || !chat.md.root.isConnected) {
    chat.md = newMarkdownView(chat.target, 'msg-text md streaming');
  }
  // The repaint lands on the next frame, and touches only the block the delta
  // changed — so a selection in the paragraphs above it survives the stream.
  pushMarkdown(chat.md, delta, () => autoScroll(chat));
}

function onThinking(delta) {
  endStream(chat);
  chat.group = null;
  if (!chat.think || !chat.think.body.isConnected) {
    chat.think = appendThinkingBlock(chat, '', false);
  }
  chat.think.body.textContent += delta;
  autoScroll(chat);
}

function onStatus(status) {
  const first = replayPending;
  replayPending = false;
  const wire = status.state;
  if (wire === 'working') {
    if (first) {
      truncateAfterLastPrompt();
      beginLiveTurn();
    } else if (!liveTurn) {
      beginLiveTurn();
    }
  }
  state.taskState = wire;
  if (status.elapsedLabel) state.lastWorked = status.elapsedLabel;
  updateSendButton();
  updateTaskSummary(state.selectedTaskId, {
    status: wire,
    bump: wire === 'working' || wire === 'needs_input',
  });
  updateGoal();
  syncGitPoll();
}

function onTodo(items) {
  state.todos = items.map((i) => ({ text: i.text, done: !!i.done, active: !!i.active }));
  updateProgress();
  updateGoal();
}

function onUsage(usage) {
  state.usage.prompt += usage.promptTokens;
  state.usage.completion += usage.completionTokens;
  updateGoal();
}

/** The `context` frame: a reading, not a running total — it replaces. */
function onContext(context) {
  state.context = context;
  updateContextMeter();
}

/**
 * `/rewind <turn>` truncated the session on disk: every turn from `turn` on is
 * gone, and what is on screen is a record of turns that no longer exist. The
 * session file is the only copy of the history, so the only correct redraw is to
 * read it back — not to pick DOM nodes off the end and hope the count matches.
 */
async function onTranscriptReset(turn) {
  const id = state.selectedTaskId;
  const seq = ++resetSeq;
  // The notice describing the rewind arrives while this refetch is in flight, and
  // belongs to the transcript being rebuilt — not to the one being thrown away.
  resetRows = [];
  const flush = () => {
    const rows = resetRows || [];
    resetRows = null;
    for (const row of rows) appendSystemRow(chat, row.text, row.cls);
  };

  let task;
  try {
    task = await api.getTask(id);
  } catch (err) {
    if (seq !== resetSeq || state.selectedTaskId !== id) return;
    appendSystemRow(chat, `Rewound${turn == null ? '' : ` to turn ${turn}`}, but the transcript could not be re-read: ${String((err && err.message) || err)}`, 'error');
    flush();
    return;
  }
  if (seq !== resetSeq || state.selectedTaskId !== id) return; // a later reset, or another chat, owns the screen now

  state.task = task;
  renderTranscript();
  updateGoal(); // the title is the first prompt, which a rewind to turn 1 removes
  flush();
  refreshGit(); // a rewind restores the files too: the diff on screen is a turn old
}

/** A system row, held back while a post-rewind refetch is in flight: appending it
 *  now would put it in the transcript that refetch is about to replace. */
function systemRow(text, cls) {
  if (resetRows) resetRows.push({ text, cls });
  else appendSystemRow(chat, text, cls);
}

function onRetrying(attempt) {
  transientNote = h('div', { class: 'system-row retrying' },
    h('span', { class: 'spinner-icon spinning', html: icons.spinner, 'aria-hidden': 'true' }),
    ` Retrying (attempt ${attempt})…`);
  chat.target.append(transientNote);
  autoScroll(chat);
}

function onDone(reason) {
  // The turn is over: land the message's last delta now, rather than leaving it
  // to whatever frame or flow break happens to come next.
  endStream(chat);
  finalizeLiveTurn(reason);
  updateSendButton();
  updateTaskSummary(state.selectedTaskId, { bump: true });
  updateGoal();
  refreshGit();
}

function clearTransient() {
  if (transientNote) {
    transientNote.remove();
    transientNote = null;
  }
}

/* ------------------------------------------------------------------------ */
/* Context panel                                                             */
/* ------------------------------------------------------------------------ */

/** Live refs into the context panel so streams patch it in place instead of
 *  re-rendering (a rebuild would collapse the expanded changed-file list and
 *  throw away the panel's scroll position mid-turn). */
let ctx = null;

function renderContextPanel() {
  const root = $('context-panel');
  root.replaceChildren();
  ctx = null;
  const t = state.task;
  if (!t) return;
  ctx = {};

  // --- Git tools ---
  ctx.gitAdd = h('span', { class: 'add' }, '+0');
  ctx.gitDel = h('span', { class: 'del' }, '-0');
  ctx.gitCount = h('span', { class: 'ctx-sub' }, '');
  ctx.gitBranch = h('span', { class: 'ctx-label' }, '—');
  ctx.gitFileList = h('div', { class: 'git-files hidden' });
  ctx.gitSection = h('section', { class: 'ctx-section hidden' },
    h('div', { class: 'ctx-header' }, h('span', {}, 'Git tools')),
    h('button', {
      class: 'ctx-row', type: 'button', title: 'Show changed files',
      onclick: () => ctx.gitFileList.classList.toggle('hidden'),
    },
      icon('diff', 'icon ctx-icon'), h('span', { class: 'ctx-label' }, 'Changes'), ctx.gitCount,
      h('span', { class: 'ctx-right' }, ctx.gitAdd, ctx.gitDel)),
    ctx.gitFileList,
    h('div', { class: 'ctx-row static' }, icon('branch', 'icon ctx-icon'), ctx.gitBranch));

  // --- Goal ---
  ctx.goalStatus = h('span', { class: 'ctx-header-right' }, '');
  ctx.goalText = h('div', { class: 'goal-text' }, t.title);
  ctx.goalMeta = h('div', { class: 'goal-meta' }, '');
  const goalSection = h('section', { class: 'ctx-section' },
    h('div', { class: 'ctx-header' }, h('span', {}, 'Goal'), ctx.goalStatus),
    h('div', { class: 'goal-row' },
      icon('target', 'icon goal-icon'),
      h('div', { class: 'goal-main' }, ctx.goalText, ctx.goalMeta)));

  // --- Context ---
  // The tokens the NEXT model call will carry, from the `context` frame. The
  // Goal card's `N tokens` above is the session's lifetime spend, from `usage`,
  // and the two are deliberately not the same readout: reporting one as the
  // other is the bug main just fixed in the TUI.
  ctx.meterPct = h('span', { class: 'ctx-header-right' }, '');
  ctx.meterFill = h('span', { class: 'meter-fill' });
  ctx.meterBar = h('span', { class: 'meter' }, ctx.meterFill);
  ctx.meterText = h('div', { class: 'meter-text' }, '');
  ctx.contextSection = h('section', { class: 'ctx-section hidden' },
    h('div', { class: 'ctx-header' }, h('span', {}, 'Context'), ctx.meterPct),
    h('div', { class: 'meter-row' }, ctx.meterBar, ctx.meterText));

  // --- Subagents ---
  ctx.subagentList = h('div', { class: 'subagent-list' });
  ctx.subagentSection = h('section', { class: 'ctx-section hidden' },
    h('div', { class: 'ctx-header' }, h('span', {}, 'Subagents')),
    ctx.subagentList);
  /** run id -> the row's live refs, patched as the run streams. */
  ctx.subagentRows = new Map();

  // --- Progress ---
  ctx.progressList = h('div', { class: 'progress-list' });
  ctx.progressSection = h('section', { class: 'ctx-section hidden' },
    h('div', { class: 'ctx-header' }, h('span', {}, 'Progress')),
    ctx.progressList);

  root.append(h('div', { class: 'context-card' },
    ctx.gitSection, goalSection, ctx.contextSection, ctx.subagentSection, ctx.progressSection));
  updateGitCard();
  updateGoal();
  updateContextMeter();
  renderSubagentList();
  updateProgress();
}

/**
 * The context meter: how full the next model call is. Without a window (the
 * provider does not report one) there is nothing to fill, so the bar goes and
 * the count stays — a bar against an invented denominator would be a guess
 * dressed up as a measurement.
 */
function updateContextMeter() {
  if (!ctx) return;
  const c = state.context;
  if (!c || !c.tokens) {
    ctx.contextSection.classList.add('hidden');
    return;
  }
  ctx.contextSection.classList.remove('hidden');
  const pct = c.window ? Math.min(100, Math.round((c.tokens / c.window) * 100)) : null;
  ctx.meterBar.classList.toggle('hidden', pct == null);
  ctx.meterPct.textContent = pct == null ? '' : `${pct}%`;
  ctx.meterFill.className = 'meter-fill' + (pct != null && pct >= 85 ? ' high' : '');
  ctx.meterFill.style.width = `${pct || 0}%`;
  ctx.meterText.textContent = c.window
    ? `${fmtTokens(c.tokens)} of ${fmtTokens(c.window)} next turn`
    : `${fmtTokens(c.tokens)} next turn`;
}

function updateGitCard() {
  if (!ctx) return;
  const g = state.git;
  if (!g) {
    ctx.gitSection.classList.add('hidden');
    return;
  }
  ctx.gitSection.classList.remove('hidden');
  const files = g.files || [];
  ctx.gitAdd.textContent = `+${g.additions || 0}`;
  ctx.gitDel.textContent = `-${g.deletions || 0}`;
  ctx.gitCount.textContent = files.length ? `${files.length} ${files.length === 1 ? 'file' : 'files'}` : 'clean';
  ctx.gitBranch.textContent = g.branch || '—';
  ctx.gitFileList.replaceChildren(
    ...files.map((f) =>
      h('button', {
        class: 'git-file', type: 'button', title: `${f.path}\nShow this file's diff`,
        onclick: () => openDiffPane(f),
      },
        h('span', { class: 'git-file-path' }, f.path),
        h('span', { class: 'git-file-stat' },
          h('span', { class: 'add' }, `+${f.additions || 0}`),
          h('span', { class: 'del' }, `-${f.deletions || 0}`)))),
  );
}

function goalMetaText() {
  const parts = [];
  const items = state.todos.length ? state.todos : (state.task && state.task.progress) || [];
  if (items.length) parts.push(`${items.filter((i) => i.done).length}/${items.length}`);
  const elapsed = liveTurn
    ? fmtDur(Math.max(1, Math.round((Date.now() - liveTurn.startedAt) / 1000)))
    : state.lastWorked || (state.task && state.task.workedFor);
  if (elapsed) parts.push(elapsed);
  const tokens = state.usage.prompt + state.usage.completion;
  if (tokens) parts.push(`${fmtTokens(tokens)} tokens`);
  return parts.join(' · ');
}

function updateGoal() {
  if (!ctx || !state.task) return;
  ctx.goalText.textContent = state.task.title;
  ctx.goalStatus.textContent = stateLabel(state.taskState);
  ctx.goalMeta.textContent = goalMetaText();
}

function updateProgress() {
  if (!ctx) return;
  const items = state.todos.length ? state.todos : (state.task && state.task.progress) || [];
  // `/todos` hid it: a later todo frame updates it, it does not reopen it.
  if (!items.length || state.todosHidden) {
    ctx.progressSection.classList.add('hidden');
    return;
  }
  ctx.progressSection.classList.remove('hidden');
  ctx.progressList.replaceChildren(
    ...items.map((p) => h('div', { class: 'progress-item' },
      icon(p.done ? 'checkCircle' : p.active ? 'circleDot' : 'circle',
        'icon check-icon' + (p.done ? ' done' : p.active ? ' active' : '')),
      h('span', { class: 'progress-text' + (p.done ? ' done' : '') }, p.text))),
  );
}

/* --- Git polling ------------------------------------------------------------ */

async function refreshGit() {
  const t = state.task;
  if (!t || !t.path) return;
  const seq = ++gitSeq;
  const id = t.id;
  try {
    const git = await api.gitStatus(t);
    if (state.selectedTaskId !== id || seq !== gitSeq) return;
    const hadBranch = state.git && state.git.branch;
    state.git = git;
    updateGitCard();
    if ((git && git.branch) !== hadBranch) renderTopbar();
  } catch {
    if (state.selectedTaskId === id && seq === gitSeq) {
      state.git = null; // not a git repo (or backend refused): hide the card
      updateGitCard();
    }
  }
}

/** Poll /api/git every 3s while a turn is streaming; stop when it isn't. */
function syncGitPoll() {
  const wants = state.taskState === 'working';
  if (wants && !gitPoll) {
    gitPoll = setInterval(() => {
      refreshGit();
      updateGoal(); // keep the live elapsed time fresh
    }, 3000);
  } else if (!wants && gitPoll) {
    clearInterval(gitPoll);
    gitPoll = null;
  }
}

/* ------------------------------------------------------------------------ */
/* Subagents: the panel's list of runs, and one run's own pane               */
/* ------------------------------------------------------------------------ */

/**
 * One subagent run, in the vocabulary the TUI's rail already uses for the same
 * events — run, name, task, status, transcript, steps, unread — so the two
 * surfaces describe a run the same way.
 * @typedef {Object} SubagentRunView
 * @property {number} run              Session-unique run id (frames are scoped to it).
 * @property {number|null} bg          Background-registry id; null when the parent waits on it.
 * @property {string} name
 * @property {string} task
 * @property {'running'|'done'|'failed'|'budget'} status
 * @property {RunEntry[]} transcript   Its own messages and tool cards.
 * @property {number} steps            Model round-trips completed.
 * @property {number} startedAt
 * @property {number|null} finishedAt  Set when it ends; freezes the elapsed clock.
 * @property {number} unread           Entries appended since its pane was last open.
 */

/**
 * One entry of a run's transcript: a message it wrote, a tool card, or a
 * closing notice.
 * @typedef {Object} RunEntry
 * @property {'text'|'tool'|'images'|'notice'} type
 * @property {string} [text]
 * @property {string} [cls]                          Row modifier for a notice ('error').
 * @property {import('./api.js').ToolCall} [call]
 * @property {import('./api.js').ImageBatch} [batch]
 */

/**
 * What is open in the main content area in place of the chat, if anything:
 * `{kind: 'subagent', run, flow, status, meta}` or `{kind: 'diff', path}`. Both
 * views are built by the same pane plumbing further down.
 */
let openPane = null;
/** Ticks the elapsed clock of the runs still going. */
let paneClock = null;

/** What the pane header calls each status; the dot is colored by it too. */
const RUN_STATUS = { running: 'Running', done: 'Done', failed: 'Failed', budget: 'Step limit' };

const findRun = (id) => state.subagents.find((run) => run.run === id);

/** A tool card as one line: what it did, to what. */
function callLine(call) {
  const subject = call.command || call.detail || (call.files || []).map((f) => f.name).join(', ');
  return subject ? `${call.title} ${subject}` : call.title;
}

/** What the subagent is doing right now: the tool it is in the middle of, else
 *  its latest message, else the task it was handed. */
function runActivity(run) {
  for (let i = run.transcript.length - 1; i >= 0; i -= 1) {
    const entry = run.transcript[i];
    // A call still running is the most specific thing to say — but only while
    // the run is going; a finished run is described by what it concluded.
    if (run.status === 'running' && entry.type === 'tool' && entry.call.status === 'pending') {
      return callLine(entry.call);
    }
    if (entry.type === 'text' && entry.text.trim()) return entry.text;
  }
  return run.task;
}

/** "3 steps · 12s" — the clock frozen once the run ends. */
function runMeta(run) {
  const secs = Math.max(1, Math.round(((run.finishedAt || Date.now()) - run.startedAt) / 1000));
  return `${run.steps} ${run.steps === 1 ? 'step' : 'steps'} · ${fmtDur(secs)}`;
}

/**
 * Rebuild the list of runs, most recent first. Called when a run starts (or a
 * task loads) — never per streamed frame: what a run is *doing* is patched
 * into the row it already has, through the refs kept here.
 */
function renderSubagentList() {
  if (!ctx) return;
  ctx.subagentRows = new Map();
  // No box when there is nothing in it.
  ctx.subagentSection.classList.toggle('hidden', !state.subagents.length);

  const rows = [];
  for (const run of [...state.subagents].reverse()) {
    const dot = h('span', { class: 'sub-dot' });
    const badge = h('span', { class: 'sub-badge hidden' });
    const activity = h('div', { class: 'sub-activity' });
    const meta = h('div', { class: 'sub-meta' });
    const row = h('button', {
      class: 'ctx-row subagent-row', type: 'button', title: `${run.name} — ${run.task}`,
      onclick: () => openSubagentPane(run.run),
    },
      dot,
      h('div', { class: 'sub-main' },
        h('div', { class: 'sub-line' }, h('span', { class: 'sub-name' }, run.name), badge),
        activity,
        meta));
    ctx.subagentRows.set(run.run, { row, dot, badge, activity, meta });
    rows.push(row);
  }
  ctx.subagentList.replaceChildren(...rows);
  for (const run of state.subagents) updateSubagentRow(run);
  syncPaneClock();
}

/** Patch one run's row in place: its dot, what it is doing, steps and time. */
function updateSubagentRow(run) {
  const row = ctx && ctx.subagentRows.get(run.run);
  if (!row) return;
  row.dot.className = `sub-dot ${run.status}`;
  row.activity.textContent = runActivity(run);
  row.meta.textContent = runMeta(run);
  row.badge.textContent = String(run.unread);
  row.badge.classList.toggle('hidden', !run.unread);
  row.row.classList.toggle('open', !!openPane && openPane.run === run);
}

/** A run changed: patch its row, and its pane's header when it is the open one. */
function touchRun(run) {
  updateSubagentRow(run);
  if (openPane && openPane.run === run) updatePaneHead();
}

/** Keep the elapsed time honest while a run is going; stop when none is. */
function syncPaneClock() {
  const wants = state.subagents.some((run) => run.status === 'running');
  if (wants && !paneClock) {
    paneClock = setInterval(() => {
      for (const run of state.subagents) {
        if (run.status === 'running') touchRun(run);
      }
    }, 1000);
  } else if (!wants && paneClock) {
    clearInterval(paneClock);
    paneClock = null;
  }
}

/* --- The run's stream ------------------------------------------------------ */

/** `subagent_run_started`. A run already listed was re-announced on attach —
 *  a background run outliving the turn that spawned it — so its row stands. */
function onSubagentRun(info) {
  if (findRun(info.run)) return;
  state.subagents.push({
    ...info,
    status: 'running',
    transcript: [],
    steps: 0,
    startedAt: Date.now(),
    finishedAt: null,
    unread: 0,
  });
  renderSubagentList();
}

/** Render one entry with the components the main chat uses, so a subagent's
 *  messages, tool cards and images read exactly like the parent's. */
function renderRunEntry(flow, entry) {
  if (entry.type === 'text') appendMessage(flow, entry.text);
  else if (entry.type === 'tool') appendToolCall(flow, entry.call);
  else if (entry.type === 'images') appendImages(flow, entry.batch);
  else appendSystemRow(flow, entry.text, entry.cls || '');
}

/** Append to a run: into its transcript always, into its pane when open —
 *  and onto its unread badge when the user is looking somewhere else. */
function appendToRun(run, entry) {
  run.transcript.push(entry);
  if (openPane && openPane.run === run) {
    renderRunEntry(openPane.flow, entry);
    autoScroll(openPane.flow);
  } else {
    run.unread += 1;
  }
}

function onSubagentText(id, text) {
  const run = findRun(id);
  if (!run || !text.trim()) return;
  appendToRun(run, { type: 'text', text });
  touchRun(run);
}

function onSubagentToolCall(id, call) {
  const run = findRun(id);
  if (!run) return;
  appendToRun(run, { type: 'tool', call });
  touchRun(run);
}

function onSubagentToolResult(id, result) {
  const run = findRun(id);
  if (!run) return;
  const entry = run.transcript.find((e) => e.type === 'tool' && e.call.id === result.callId);
  if (entry) {
    entry.call.status = result.status;
    applyToolSummary(entry.call, result.summary);
  }
  if (openPane && openPane.run === run) onToolResult(openPane.flow, result);
  touchRun(run);
}

/** Images from inside a run: they belong in that run's pane, where its tool
 *  cards and messages are — not in the parent's chat. */
function onSubagentImages(id, batch) {
  const run = findRun(id);
  if (!run) return;
  appendToRun(run, { type: 'images', batch });
  touchRun(run);
}

function onSubagentStep(id, step) {
  const run = findRun(id);
  if (!run) return;
  run.steps = step;
  touchRun(run);
}

function onSubagentDone(id, result) {
  const run = findRun(id);
  if (!run) return;
  run.status = result.error ? 'failed' : result.completed ? 'done' : 'budget';
  run.finishedAt = Date.now();
  if (result.stepsUsed) run.steps = result.stepsUsed;
  // The subagent's final message is the step that made no tool call, so the
  // sub-loop ends on it without streaming it: it arrives here, as the report.
  // Without this the pane would show all of the work and none of the outcome.
  const last = run.transcript[run.transcript.length - 1];
  const reported = last && last.type === 'text' && last.text === result.output;
  if (result.output.trim() && !reported) {
    appendToRun(run, { type: 'text', text: result.output });
  }
  if (result.error) {
    appendToRun(run, { type: 'notice', text: `failed: ${result.error}`, cls: 'error' });
  } else if (!result.completed) {
    appendToRun(run, { type: 'notice', text: 'hit its step budget' });
  }
  touchRun(run);
  syncPaneClock();
}

/* --- The subagent pane ------------------------------------------------------ */

/**
 * Open one run's own view in the main content area: its messages and its tool
 * cards, streaming on while it runs.
 */
function openSubagentPane(id) {
  const run = findRun(id);
  if (!run) return;
  const body = h('div', { class: 'transcript-inner' });
  const scroll = h('div', { class: 'transcript pane-scroll' }, body);
  const flow = newFlow(scroll);
  flow.target = body;

  const status = h('span', { class: 'pane-status' });
  const meta = h('span', { class: 'pane-meta' });
  const head = paneHead(
    [icon('agents', 'icon pane-icon'),
      h('span', { class: 'pane-name' }, run.name),
      status,
      h('span', { class: 'pane-spacer' }),
      meta],
    h('div', { class: 'pane-task' }, run.task));

  showPane({ kind: 'subagent', run, flow, status, meta }, 'subagent-pane', head, scroll);
  for (const entry of run.transcript) renderRunEntry(flow, entry);
  breakFlow(flow);
  run.unread = 0;
  updatePaneHead();
  updateSubagentRow(run);
  scroll.scrollTop = scroll.scrollHeight;
}

/** The subagent pane's header: what the run is, and where it has got to. */
function updatePaneHead() {
  if (!openPane) return;
  const run = openPane.run;
  openPane.status.className = `pane-status ${run.status}`;
  openPane.status.textContent = RUN_STATUS[run.status];
  openPane.meta.textContent = runMeta(run);
}

/* ------------------------------------------------------------------------ */
/* The pane: a second view where the chat is                                 */
/* ------------------------------------------------------------------------ */

/**
 * The pane's header: the one-press way back to the chat, then the view's own
 * title row, and an optional line under it.
 * @param {Array<Node|false|null>} row  what this view puts beside the back button
 * @param {Node} [sub]                  a second header line
 */
function paneHead(row, sub) {
  return h('header', { class: 'pane-head' },
    h('div', { class: 'pane-head-row' },
      h('button', {
        class: 'btn quiet btn-sm pane-back', type: 'button', title: 'Back to the chat (Esc)',
        onclick: () => closePane(),
      }, icon('chevronLeft', 'icon pane-back-icon'), 'Chat'),
      row),
    sub);
}

/**
 * Show `nodes` in the main content area, in place of the chat. The chat is only
 * hidden, never torn down, so it keeps streaming behind the pane and is one
 * press away — the back control, or Escape.
 * @param {Object} pane  what is open; `closePane` and the stream handlers read it
 * @param {string} cls   the view's own class ('subagent-pane', 'diff-pane')
 */
function showPane(pane, cls, ...nodes) {
  const view = $('pane');
  view.className = `pane ${cls}`;
  view.replaceChildren(...nodes);
  $('transcript').classList.add('hidden');
  openPane = pane;
}

/** Back to the chat — the pane's DOM goes with it, so no two panes ever share
 *  a row or a tool group. */
function closePane() {
  if (!openPane) return;
  const { run } = openPane;
  openPane = null;
  const view = $('pane');
  view.replaceChildren();
  view.className = 'pane hidden';
  $('transcript').classList.remove('hidden');
  if (run) updateSubagentRow(run); // its row is no longer the open one
  autoScroll(chat, true);
}

/* ------------------------------------------------------------------------ */
/* The image pane: one image, at full size                                   */
/* ------------------------------------------------------------------------ */

/**
 * Open an image where the chat is: the file at its own size, what it is, and
 * how big. The same pane the diff and the subagent runs open in, so it closes
 * the same way — the back control, or Escape.
 * @param {import('./api.js').ImageRef} image
 */
function openImagePane(image) {
  const img = h('img', { class: 'image-full', src: imageUrl(image), alt: '', decoding: 'async' });
  const body = h('div', { class: 'image-body' }, img);
  const scroll = h('div', { class: 'pane-scroll' }, body);
  const head = paneHead([
    icon('image', 'icon pane-icon'),
    h('span', { class: 'pane-path', title: image.path }, elidedPath(image.path)),
    h('span', { class: 'pane-spacer' }),
    h('span', { class: 'pane-meta' }, `${image.mime} · ${fmtBytes(image.bytes)}`),
  ]);
  // The thumbnail loaded from the same URL, so this rarely fires — but a
  // session cleaned up between the two is a blank pane unless it says so.
  img.addEventListener('error', () => {
    body.replaceChildren(h('div', { class: 'image-note error' }, `The file is gone: ${image.path}`));
  });
  showPane({ kind: 'image', path: image.path }, 'image-pane', head, scroll);
}

/* ------------------------------------------------------------------------ */
/* The diff pane: one changed file, against HEAD                             */
/* ------------------------------------------------------------------------ */

/** Line kind → the class that colors its row. */
const DIFF_LINE_CLASS = { add: 'diff-add', del: 'diff-del', meta: 'diff-meta' };

/**
 * Open a changed file's diff where the chat is: every hunk of it against HEAD,
 * staged and unstaged changes together — the same working-tree state the file's
 * `+N -M` in the panel counts.
 *
 * The diff is a snapshot taken now, and the agent may edit the file a second
 * later; clicking the row again refetches rather than reopening this one.
 * @param {{path: string, additions: number, deletions: number}} file
 */
async function openDiffPane(file) {
  const task = state.task;
  if (!task) return;
  const body = h('div', { class: 'diff-body' }, h('div', { class: 'diff-note' }, 'Loading…'));
  const scroll = h('div', { class: 'pane-scroll' }, body);
  const add = h('span', { class: 'add' }, `+${file.additions || 0}`);
  const del = h('span', { class: 'del' }, `-${file.deletions || 0}`);
  const head = paneHead([
    icon('diff', 'icon pane-icon'),
    h('span', { class: 'pane-path', title: file.path }, file.path),
    h('span', { class: 'pane-spacer' }),
    h('span', { class: 'diff-stat' }, add, del),
  ]);

  const pane = { kind: 'diff', path: file.path };
  showPane(pane, 'diff-pane', head, scroll);
  try {
    const diff = await api.fileDiff(task, file.path);
    if (openPane !== pane) return; // closed, or another file opened, while we fetched
    // The counts the backend just measured, not the ones the row was showing:
    // the poll behind the panel may be a few seconds old.
    add.textContent = `+${diff.additions}`;
    del.textContent = `-${diff.deletions}`;
    renderDiff(body, diff);
  } catch (err) {
    if (openPane !== pane) return;
    body.replaceChildren(h('div', { class: 'diff-note error' }, String((err && err.message) || err)));
  }
}

/**
 * The hunks, as plain rows: one element per line, no per-character work, so a
 * 5000-line diff opens as fast as a 5-line one. Every case git can hand back
 * says something — a binary file, a change with no lines in it (a mode, a
 * rename), a diff too long to ship whole — rather than sitting on a spinner.
 */
function renderDiff(body, diff) {
  if (diff.binary) {
    body.replaceChildren(h('div', { class: 'diff-note' }, 'Binary file — no line diff to show.'));
    return;
  }
  if (!diff.hunks.length) {
    body.replaceChildren(h('div', { class: 'diff-note' },
      'No line changes — git records this file as changed in its mode or name only.'));
    return;
  }
  const rows = [];
  let lines = 0;
  for (const hunk of diff.hunks) {
    rows.push(h('div', { class: 'diff-hunk' }, hunk.header));
    for (const line of hunk.lines) {
      rows.push(h('div', { class: `diff-line ${DIFF_LINE_CLASS[line.kind] || 'diff-ctx'}` }, line.text || ' '));
      lines += 1;
    }
  }
  if (diff.truncated) {
    rows.push(h('div', { class: 'diff-note' }, `Truncated after ${lines.toLocaleString()} lines.`));
  }
  body.replaceChildren(...rows);
}

/* ------------------------------------------------------------------------ */
/* Attachments: files staged for the next message                            */
/* ------------------------------------------------------------------------ */

/**
 * One file staged in the composer. It uploads the moment it is attached, so by
 * the time the message is sent the server has already written it and said what
 * it is; `path` is what goes out on the wire.
 * @typedef {Object} StagedFile
 * @property {string} key           Identity for the chip, before a path exists.
 * @property {string} name
 * @property {number} bytes
 * @property {string} mime
 * @property {'image'|'file'} kind  Guessed locally to draw the chip; the SERVER decides it for real.
 * @property {string|null} preview  Object URL, images only.
 * @property {string|null} path     Where the server wrote it; null until it has.
 * @property {string|null} error    Why it did not land.
 * @property {boolean} pending
 * @property {Promise<void>} upload
 */

let attachSeq = 0;

/**
 * Stage files: the chips appear at once and the uploads run behind them, so a
 * 4MB screenshot does not freeze the composer while it goes up.
 * @param {File[]} files
 */
function attachFiles(files) {
  const task = state.task;
  if (!task || !files.length) return;
  for (const file of files) {
    const image = /^image\//.test(file.type || '');
    /** @type {StagedFile} */
    const item = {
      key: `att-${++attachSeq}`,
      name: file.name || (image ? 'pasted image.png' : 'file'),
      bytes: file.size || 0,
      mime: file.type || '',
      kind: image ? 'image' : 'file',
      preview: image ? URL.createObjectURL(file) : null,
      path: null,
      error: null,
      pending: true,
      upload: null,
    };
    item.upload = uploadStaged(task.id, file, item);
    state.attachments.push(item);
  }
  renderAttachTray();
}

/** Put one file where the server can serve it back, and take its word for what
 *  it is: the server sniffs the bytes, and the file name here is only a label. */
async function uploadStaged(taskId, file, item) {
  try {
    const [saved] = await api.upload(taskId, [file]);
    if (!saved || !saved.path) throw new Error('the server saved nothing');
    item.path = saved.path;
    item.name = saved.name || item.name;
    item.mime = saved.mime || item.mime;
    item.bytes = saved.bytes || item.bytes;
    item.kind = saved.kind === 'image' ? 'image' : 'file';
    // The bytes are already here; the thumbnail need not fetch them back.
    if (item.kind === 'image' && item.preview) rememberImage(item.path, item.preview);
  } catch (err) {
    item.error = String((err && err.message) || err);
  } finally {
    item.pending = false;
    if (state.attachments.includes(item)) renderAttachTray();
  }
}

function removeAttachment(item) {
  state.attachments = state.attachments.filter((a) => a !== item);
  renderAttachTray();
}

/** One staged file: a thumbnail for an image, its type for anything else. */
function attachChip(item) {
  const face = item.kind === 'image' && item.preview
    ? h('img', { class: 'attach-thumb', src: item.preview, alt: '' })
    : h('span', { class: 'attach-icon', html: fileIconSvg(item.name), 'aria-hidden': 'true' });
  return h('div', {
    class: 'attach-chip' + (item.pending ? ' pending' : '') + (item.error ? ' failed' : ''),
    title: item.error ? `${item.name} — ${item.error}` : `${item.name} · ${fmtBytes(item.bytes)}`,
  },
    face,
    h('span', { class: 'attach-name' }, item.name),
    h('span', { class: 'attach-size' }, item.error ? 'failed' : fmtBytes(item.bytes)),
    h('button', {
      class: 'attach-remove', type: 'button',
      title: 'Remove', 'aria-label': `Remove ${item.name}`,
      onclick: () => removeAttachment(item),
    }, icon('close')));
}

function renderAttachTray() {
  if (!attachTray) return;
  attachTray.classList.toggle('hidden', !state.attachments.length);
  attachTray.replaceChildren(...state.attachments.map(attachChip));
}

/**
 * Files dropped on the chat — the transcript or the composer — attach to the
 * next message, exactly as the paperclip does. The document-level handlers are
 * what stop a file dropped anywhere else from navigating the page to it.
 */
function initDropTarget() {
  const zone = $('conversation');
  const form = $('composer');
  const hasFiles = (e) => Array.from((e.dataTransfer && e.dataTransfer.types) || []).includes('Files');
  // A drag over a child fires `dragleave` on the parent: count the crossings
  // rather than trusting one leave to mean the pointer really left.
  let depth = 0;
  const leave = () => {
    depth = 0;
    form.classList.remove('dropping');
  };
  zone.addEventListener('dragenter', (e) => {
    if (!hasFiles(e)) return;
    depth += 1;
    form.classList.add('dropping');
  });
  zone.addEventListener('dragover', (e) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = 'copy';
  });
  zone.addEventListener('dragleave', (e) => {
    if (!hasFiles(e)) return;
    depth -= 1;
    if (depth <= 0) leave();
  });
  zone.addEventListener('drop', (e) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    leave();
    attachFiles(Array.from(e.dataTransfer.files || []));
  });
  for (const type of ['dragover', 'drop']) {
    document.addEventListener(type, (e) => {
      if (hasFiles(e)) e.preventDefault();
    });
  }
}

/* ------------------------------------------------------------------------ */
/* Slash commands: the palette, and who runs what                            */
/* ------------------------------------------------------------------------ */

/** The workspace's commands. Custom ones live in its `.wizard/commands/`, so
 *  the list is per-chat and is reloaded when one is opened. */
async function loadCommands(task) {
  const seq = selectSeq;
  try {
    const commands = await api.commands(task.path);
    if (seq === selectSeq) state.commands = commands;
  } catch {
    if (seq === selectSeq) state.commands = []; // no palette, typing still works
  }
  // The list arrives after the composer does. Someone who typed `/` while it
  // was in flight filtered an empty list and got no palette — and, since the
  // palette only re-syncs on input, no palette until they typed another
  // character. Re-sync once it lands.
  if (seq === selectSeq) syncPalette();
}

/** A leading `/name`, with the rest as its arguments — and only when the name
 *  is one we were given. A message can legitimately start with a slash: a path
 *  (`/home/…`) is not a command, and is sent as what it is. */
function matchCommand(text) {
  const m = /^\/([A-Za-z0-9_-]+)(?:\s+([\s\S]*))?$/.exec(text);
  if (!m) return null;
  const def = state.commands.find((c) => c.name === m[1]);
  return def ? { def, args: (m[2] || '').trim() } : null;
}

/**
 * Run a command that is not a `prompt` one (those are messages, and go out
 * through `send`). A `server` command becomes a `command` frame and is answered
 * with the frames the protocol already has; a `client` one is handled here; an
 * `unavailable` one is answered here and never sent, because the only thing the
 * server would send back is the same refusal.
 */
function runCommand({ def, args }) {
  const line = args ? `/${def.name} ${args}` : `/${def.name}`;
  appendSystemRow(chat, line);
  if (def.where === 'unavailable') {
    explainUnavailable(def);
    return;
  }
  if (def.where === 'client') {
    runClientCommand(def, args, line);
    return;
  }
  try {
    api.sendCommand(state.task.id, def.name, args);
  } catch (err) {
    appendSystemRow(chat, String((err && err.message) || err), 'error');
  }
}

/** A terminal-only command: say what it is and why it is not here. Nothing is
 *  sent — there is no browser surface for it to land on, at either end. */
function explainUnavailable(def) {
  closePalette();
  appendSystemRow(chat, `/${def.name} runs in the terminal, not here — ${def.detail}.`);
}

/**
 * The commands the page owns. Each is a window this UI already has: there is no
 * second panel, list or overlay built for a slash command, because a command
 * that opened one of its own would be a different feature wearing its name.
 */
function runClientCommand(def, args, line) {
  switch (def.name) {
    case 'diff': {
      const files = (state.git && state.git.files) || [];
      const file = args
        ? files.find((f) => f.path === args || f.path.endsWith(`/${args}`))
        : files[0];
      if (!file) {
        appendSystemRow(chat, args ? `${args} is not a changed file.` : 'Nothing has changed in the working tree.');
        return;
      }
      openDiffPane(file);
      return;
    }
    case 'todos':
      state.todosHidden = !state.todosHidden;
      updateProgress();
      return;
    // A chat and its session file are the same object here, and clearing the
    // history rotates that file — so the honest clear is a new chat, in the
    // same directory. The old one stays in the sidebar rather than being wiped.
    case 'clear':
      newChatHere();
      return;
    case 'subagents':
      revealSubagents();
      return;
    case 'dashboard':
      showRunningChats();
      return;
    case 'resume':
      focusChatList();
      return;
    case 'settings':
      openSettings();
      return;
    case 'provider':
      openSettings({ focus: 'providers' });
      return;
    case 'login':
      openSettings({ focus: 'signin', signIn: SIGN_INS.some((s) => s.id === args) ? args : null });
      return;
    default:
      appendSystemRow(chat, `${line} is not implemented in the GUI.`, 'error');
  }
}

/** Draw the eye to a part of the context panel that is already on screen: a
 *  command that reveals a panel still has to say which part of it. */
function flashSection(section) {
  if (!section) return;
  section.scrollIntoView({ block: 'nearest' });
  section.classList.remove('flash');
  void section.offsetWidth; // restart the animation when the same section is flashed twice
  section.classList.add('flash');
}

/** `/subagents`: the panel's Subagents section is where a run is watched from —
 *  each row opens that run's own pane. */
function revealSubagents() {
  $('app').classList.remove('panel-collapsed');
  if (!state.subagents.length) {
    appendSystemRow(chat, 'No subagent has run in this chat.');
    return;
  }
  flashSection(ctx && ctx.subagentSection);
}

/**
 * `/dashboard`: what is running. The sidebar's chat list is that list — its
 * state dots are `/api/tasks`, which merges the same running registry the TUI's
 * session manager reads — so it is re-read and revealed rather than duplicated
 * into a second view that could disagree with it.
 */
async function showRunningChats() {
  $('app').classList.remove('sidebar-collapsed');
  await refreshTaskList();
  const tasks = state.workspaces.flatMap((ws) => ws.tasks);
  const counts = [
    ['working', tasks.filter((t) => t.status === 'working').length],
    ['waiting for input', tasks.filter((t) => t.status === 'needs_input').length],
    ['failed', tasks.filter((t) => t.status === 'failed').length],
  ].filter(([, n]) => n).map(([label, n]) => `${n} ${label}`);
  appendSystemRow(chat, counts.length
    ? `${counts.join(', ')} — in the chat list.`
    : `Nothing running. ${tasks.length} ${tasks.length === 1 ? 'chat' : 'chats'} in the list.`);
}

/** `/resume`: the chat list is the session picker. There is no second one to
 *  open, so this re-reads it and puts the keyboard in it. */
async function focusChatList() {
  $('app').classList.remove('sidebar-collapsed');
  await refreshTaskList();
  const tree = $('task-tree');
  const row = tree.querySelector('.task-row.selected') || tree.querySelector('.task-row');
  if (!row) {
    appendSystemRow(chat, 'No other chat to resume.');
    return;
  }
  row.scrollIntoView({ block: 'nearest' });
  row.focus();
}

/** Rebuild the palette from what it is currently offering. */
function renderPalette() {
  if (!palette) return;
  palette.list.replaceChildren(...palette.matches.map((cmd, i) => {
    // A terminal-only command is listed, dimmed, with what it is: someone who
    // knows it from the TUI should read why it is not here rather than wonder
    // whether they mistyped it. It cannot be picked, so it is not `aria-selected`.
    const dead = cmd.where === 'unavailable';
    return h('button', {
      class: 'palette-item' + (i === palette.index ? ' active' : '') + (dead ? ' unavailable' : ''),
      type: 'button', role: 'option', 'aria-selected': !dead && i === palette.index ? 'true' : 'false',
      'aria-disabled': dead ? 'true' : null,
      // mousedown, not click: the composer must not lose focus before the pick,
      // or the palette closes on blur and the click lands on nothing.
      onmousedown: (e) => {
        e.preventDefault();
        pickCommand(cmd, true);
      },
      onmousemove: () => {
        if (palette.index === i) return;
        palette.index = i;
        renderPalette();
      },
    },
      h('span', { class: 'palette-name mono' }, `/${cmd.name}`),
      cmd.args ? h('span', { class: 'palette-args mono' }, cmd.args) : null,
      h('span', { class: 'palette-detail' }, cmd.detail),
      // Where a command comes from, when it is not simply wizard's own: a custom
      // one is the workspace's, a terminal-only one is not this UI's to run.
      cmd.where === 'prompt' && h('span', { class: 'tag' }, 'custom'),
      dead && h('span', { class: 'tag' }, 'terminal only'));
  }));
  const active = palette.list.children[palette.index];
  if (active) active.scrollIntoView({ block: 'nearest' });
}

function openPalette(matches, index = 0) {
  if (!palette) {
    const list = h('div', { class: 'palette-list' });
    palette = { el: h('div', { class: 'palette', role: 'listbox', 'aria-label': 'Commands' }, list), list, matches: [], index: 0 };
    $('composer').append(palette.el);
  }
  palette.matches = matches;
  palette.index = Math.min(Math.max(index, 0), matches.length - 1);
  renderPalette();
}

function closePalette() {
  if (!palette) return;
  palette.el.remove();
  palette = null;
}

/**
 * Follow the composer: a `/` in the first column offers what it could become.
 * Once a space is typed the user is on to arguments, and the palette is done.
 */
function syncPalette() {
  const m = composerInput && /^\/(\S*)$/.exec(composerInput.value);
  if (!m) {
    closePalette();
    return;
  }
  const query = m[1].toLowerCase();
  const matches = state.commands.filter((c) => c.name.toLowerCase().startsWith(query));
  if (!matches.length) {
    closePalette(); // a path, or a typo: neither is a menu worth holding open
    return;
  }
  openPalette(matches);
}

/**
 * Complete the composer to the picked command. Enter runs it right there —
 * unless it takes arguments, which are typed, not guessed.
 */
function pickCommand(cmd, run) {
  if (!cmd || !composerInput) return;
  // There is nothing to complete to: it does not run here. Say so on the pick
  // rather than completing the composer to a command that goes nowhere.
  if (cmd.where === 'unavailable') {
    if (run) explainUnavailable(cmd);
    return;
  }
  const takesArgs = !!cmd.args;
  composerInput.value = `/${cmd.name}${takesArgs ? ' ' : ''}`;
  closePalette();
  composerInput.focus();
  if (run && !takesArgs) $('composer').requestSubmit();
}

/** The palette's own keys, while it is open. Returns true when it took the key. */
function paletteKeydown(e) {
  if (!palette) return false;
  const n = palette.matches.length;
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    palette.index = (palette.index + (e.key === 'ArrowDown' ? 1 : n - 1)) % n;
    renderPalette();
    return true;
  }
  if (e.key === 'Escape') {
    e.preventDefault();
    e.stopPropagation(); // Escape closes the palette, not the pane behind it
    closePalette();
    return true;
  }
  if (e.key === 'Tab' || (e.key === 'Enter' && !e.shiftKey)) {
    e.preventDefault();
    pickCommand(palette.matches[palette.index], e.key === 'Enter');
    return true;
  }
  return false;
}

/* ------------------------------------------------------------------------ */
/* Composer                                                                  */
/* ------------------------------------------------------------------------ */

function defaultModelLabel() {
  // The task's own model wins (it may have been created with an override);
  // the configured default is the fallback before any task loads.
  if (state.task && state.task.model) return state.task.model;
  const def = state.models.find((m) => m.isDefault);
  return def ? def.label : '—';
}

function updateModelChip() {
  if (modelLabelEl) modelLabelEl.textContent = state.modelLabel || defaultModelLabel();
}

function closeMenu() {
  if (menuEl) {
    menuEl.remove();
    menuEl = null;
    document.removeEventListener('click', onDocClickForMenu, true);
  }
}

function onDocClickForMenu(e) {
  if (menuEl && !menuEl.contains(e.target) && !e.target.closest('.menu-anchor')) closeMenu();
}

/** `replaceChildren` renders a `null` child as the text "null"; `h()` drops it.
 *  Menus are built conditionally, so they go through this. */
function fillWith(node, ...children) {
  node.replaceChildren(...children.flat(Infinity).filter(Boolean));
}

/**
 * Open a dropdown under `anchor` and let `fill` populate it — possibly after
 * an await, so every menu here can load what it offers at open time rather
 * than trusting whatever was fetched at boot. A menu the user has closed (or
 * replaced with another) in the meantime is never written into: `fill` gets a
 * `live()` predicate to check after each await.
 *
 * Clicking the same chip again toggles the menu shut.
 */
async function openMenu(anchor, cls, loading, fill) {
  const wasOpen = menuEl && anchor.contains(menuEl);
  closeMenu();
  if (wasOpen) return;
  const menu = h('div', { class: `menu ${cls}`, role: 'menu' }, h('div', { class: 'menu-note' }, loading));
  anchor.append(menu);
  menuEl = menu;
  document.addEventListener('click', onDocClickForMenu, true);
  const live = () => menuEl === menu;
  try {
    await fill(menu, live);
  } catch (err) {
    if (live()) menu.replaceChildren(h('div', { class: 'menu-note error' }, String((err && err.message) || err)));
  }
}

/** A menu row. */
function menuItem(label, { hint, title, selected, onclick } = {}) {
  return h('button', {
    class: 'menu-item' + (selected ? ' selected' : ''),
    type: 'button', role: 'menuitem', title, onclick,
  },
    h('span', { class: 'menu-item-label' }, label),
    hint && h('span', { class: 'menu-item-hint' }, hint),
    selected && h('span', { class: 'menu-check', html: icons.check, 'aria-hidden': 'true' }));
}

/**
 * The model menu, reloaded on open: providers can be added in Settings, and a
 * local backend that was down when the page loaded may be up now — a menu
 * built once at boot goes stale either way.
 */
function openModelMenu(anchor) {
  return openMenu(anchor, 'model-menu', 'Loading models…', async (menu, live) => {
    const models = await api.listModels();
    if (!live()) return;
    state.models = models;
    fillModelMenu(menu);
  });
}

function fillModelMenu(menu) {
  const choose = (m) => {
    state.modelId = m.value;
    state.modelLabel = m.label;
    updateModelChip();
    closeMenu();
  };
  const manage = h('button', {
    class: 'menu-item menu-manage', type: 'button', role: 'menuitem',
    onclick: () => { closeMenu(); openSettings(); },
  }, h('span', { class: 'menu-item-label' }, 'Manage providers…'));

  if (!state.models.length) {
    menu.replaceChildren(
      h('div', { class: 'menu-note' }, 'No provider is configured.'),
      manage,
    );
    return;
  }

  menu.replaceChildren();
  let lastProvider = null;
  for (const m of state.models) {
    if (m.provider !== lastProvider) {
      menu.append(h('div', { class: 'menu-head' }, m.provider));
      lastProvider = m.provider;
    }
    const selected = state.modelId === m.value || (state.modelId == null && m.isDefault);
    menu.append(menuItem(m.label, {
      hint: m.isDefault ? 'default' : null,
      selected,
      onclick: () => choose(m),
    }));
  }
  menu.append(manage);
}

/* --- Workspace + branch menus (the topbar chips) --------------------------- */

/** The directory a new chat opens in: the one you are looking at, else the
 *  directory `wizard gui` runs in. */
function activeDir() {
  return (state.task && state.task.path) || state.home.cwd;
}

/**
 * The folder chip: open a chat in another directory. A chat's working
 * directory is fixed when its session is created — it is written into the
 * session file and is where every command it has run took effect — so this
 * starts a new chat there rather than pretending to move this one.
 */
function openDirMenu(anchor) {
  return openMenu(anchor, 'dir-menu', 'Loading directories…', async (menu, live) => {
    const dirs = await api.workspaces();
    if (!live()) return;
    const here = activeDir();

    const pathInput = h('input', {
      class: 'input menu-input', type: 'text', spellcheck: 'false',
      placeholder: '/absolute/path', 'aria-label': 'Open a chat in this directory',
    });
    const err = h('div', { class: 'menu-note error hidden' });
    const open = async (cwd) => {
      err.classList.add('hidden');
      try {
        await newChat(cwd);
        closeMenu();
      } catch (e) {
        // The menu stays open on failure — a mistyped path is worth fixing in
        // place rather than starting over.
        err.textContent = String((e && e.message) || e);
        err.classList.remove('hidden');
      }
    };
    pathInput.addEventListener('keydown', (e) => {
      if (e.key !== 'Enter') return;
      e.preventDefault();
      const cwd = pathInput.value.trim();
      if (cwd) open(cwd);
    });

    fillWith(menu,
      h('div', { class: 'menu-head' }, 'New chat in'),
      dirs.map((d) => menuItem(d.name, {
        hint: d.home ? 'here' : null,
        title: d.cwd,
        selected: d.cwd === here,
        onclick: () => open(d.cwd),
      })),
      h('div', { class: 'menu-foot' }, pathInput),
      err,
    );
  });
}

/** The branch chip: check a branch out in this chat's workspace. */
function openBranchMenu(anchor) {
  return openMenu(anchor, 'branch-menu', 'Loading branches…', async (menu, live) => {
    const task = state.task;
    if (!task) return;
    const { current, branches } = await api.branches(task);
    if (!live()) return;

    const err = h('div', { class: 'menu-note error hidden' });
    const fail = (e) => {
      err.textContent = String((e && e.message) || e);
      err.classList.remove('hidden');
    };
    const switchTo = async (branch, create) => {
      err.classList.add('hidden');
      try {
        const now = await api.checkout(task, branch, create);
        closeMenu();
        appendSystemRow(chat, `Switched to ${now}`);
        await refreshGit();
        renderTopbar();
      } catch (e) {
        fail(e);
      }
    };

    const newInput = h('input', {
      class: 'input menu-input', type: 'text', spellcheck: 'false',
      placeholder: 'new branch name', 'aria-label': 'Create and check out a branch',
    });
    newInput.addEventListener('keydown', (e) => {
      if (e.key !== 'Enter') return;
      e.preventDefault();
      const name = newInput.value.trim();
      if (name) switchTo(name, true);
    });

    fillWith(menu,
      h('div', { class: 'menu-head' }, 'Branch'),
      // The backend refuses this too; saying so up front beats a failed click.
      state.taskState === 'working'
        && h('div', { class: 'menu-note' }, 'The agent is working in this tree — stop the turn to switch.'),
      branches.map((b) => menuItem(b, {
        selected: b === current,
        onclick: () => switchTo(b, false),
      })),
      h('div', { class: 'menu-foot' }, newInput),
      err,
    );
  });
}

/** Composer refs for the send/stop button. */
let sendBtn = null;

/**
 * One button, two jobs: send while idle, stop while the agent is working. The
 * thing you want to press mid-turn is exactly where you last pressed send, and
 * an idle spinner sitting next to it only ever read as "loading forever".
 */
function updateSendButton() {
  if (!sendBtn) return;
  const working = state.taskState === 'working';
  sendBtn.classList.toggle('working', working);
  sendBtn.title = working ? 'Stop the agent' : 'Send';
  sendBtn.setAttribute('aria-label', sendBtn.title);
  sendBtn.replaceChildren(icon(working ? 'stop' : 'sendArrow'));
}

function stopTurn() {
  if (state.taskState !== 'working' || !state.selectedTaskId) return;
  try {
    api.cancel(state.selectedTaskId);
    appendSystemRow(chat, 'Stopping…');
  } catch {
    /* the socket is gone; the turn is not ours to stop */
  }
}

function focusComposer() {
  if (composerInput) composerInput.focus();
}

function renderComposer() {
  const form = $('composer');
  const input = h('textarea', {
    class: 'composer-input', rows: '1',
    placeholder: 'Ask wizard to change something, or / for a command', 'aria-label': 'Message wizard',
  });
  composerInput = input;
  input.addEventListener('input', () => {
    input.style.height = 'auto';
    input.style.height = `${Math.min(input.scrollHeight, 160)}px`;
    syncPalette();
  });
  input.addEventListener('blur', () => closePalette());
  input.addEventListener('keydown', (e) => {
    if (paletteKeydown(e)) return;
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      form.requestSubmit();
    }
  });
  // A screenshot on the clipboard is bytes, not a path: it is attached, and
  // uploaded, like any other file. A text paste falls through untouched.
  input.addEventListener('paste', (e) => {
    const files = Array.from((e.clipboardData && e.clipboardData.files) || []);
    if (!files.length) return;
    e.preventDefault();
    attachFiles(files);
  });

  attachTray = h('div', { class: 'attach-tray hidden' });
  fileInput = h('input', {
    class: 'hidden', type: 'file', multiple: true, 'aria-hidden': 'true', tabindex: '-1',
  });
  fileInput.addEventListener('change', () => {
    attachFiles(Array.from(fileInput.files || []));
    fileInput.value = ''; // the same file, picked twice in a row, still fires
  });
  const attachBtn = iconBtn('paperclip', 'Attach files', () => fileInput.click());

  modelLabelEl = h('span', { class: 'chip-label' }, defaultModelLabel());
  const modelAnchor = h('span', { class: 'menu-anchor' },
    h('button', {
      class: 'chip ghost-chip model-select', type: 'button', title: 'Model for the next message',
      onclick: (e) => { e.stopPropagation(); openModelMenu(modelAnchor); },
    }, modelLabelEl, icon('chevronDown', 'icon chip-caret')));

  // Not a submit button: while the agent is working this same button stops it.
  sendBtn = h('button', {
    class: 'send-btn', type: 'button', title: 'Send', 'aria-label': 'Send',
    onclick: () => (state.taskState === 'working' ? stopTurn() : form.requestSubmit()),
  }, icon('sendArrow'));

  form.replaceChildren(
    input,
    attachTray,
    fileInput,
    h('div', { class: 'composer-row' },
      attachBtn,
      h('span', { class: 'composer-spacer' }),
      modelAnchor,
      sendBtn,
    ),
  );
  updateSendButton();
  renderAttachTray();

  form.onsubmit = (e) => {
    e.preventDefault();
    closePalette();
    const text = input.value.trim();
    if (!state.task) return;
    if (!text && !state.attachments.length) return;

    // A `prompt` command is a message — the server expands it, through the same
    // preprocess that resolves @file refs — so it goes down the send path. The
    // other two kinds are not messages at all, and take no attachments with them.
    const command = matchCommand(text);
    // A `client` command touches nothing but this page, and an `unavailable` one
    // is answered with an explanation and nothing else, so a turn in flight is no
    // reason to refuse either. Everything that reaches the agent is: one turn at
    // a time, and the backend refuses a second — so do not pretend otherwise.
    const local = command && (command.def.where === 'client' || command.def.where === 'unavailable');
    if (state.taskState === 'working' && !local) return;
    input.value = '';
    input.style.height = 'auto';
    if (command && command.def.where !== 'prompt') {
      runCommand(command);
      return;
    }
    const staged = state.attachments;
    state.attachments = [];
    renderAttachTray();
    send(text, staged);
  };
}

/** A chat's title: its first message, one line, bounded. */
function titleFrom(text) {
  const line = text.split('\n', 1)[0].trim() || text.trim();
  return line.length > 90 ? `${line.slice(0, 89)}…` : line;
}

/**
 * @param {string} text
 * @param {StagedFile[]} [staged]  what was in the composer's tray
 */
async function send(text, staged = []) {
  const task = state.task;
  // A file still going up is part of this message: wait for it, rather than
  // sending the prompt without the thing the prompt is about. The uploads
  // started when the file was attached, so this is all but always already done.
  await Promise.all(staged.map((a) => a.upload).filter(Boolean));
  // The chat was switched while a file uploaded; its socket went with it.
  if (!state.task || state.task.id !== task.id) return;

  const sent = staged.filter((a) => a.path);
  const images = sent.filter((a) => a.kind === 'image');
  const files = sent.filter((a) => a.kind !== 'image');

  appendPromptCard(text, sent);
  for (const bad of staged.filter((a) => !a.path)) {
    appendSystemRow(chat, `${bad.name} was not attached: ${bad.error || 'the upload failed'}`, 'error');
  }
  pendingTurnStart = Date.now();
  autoScroll(chat, true);
  // The first message in an empty chat names it, everywhere it is shown.
  if (task.title === NEW_CHAT_TITLE) {
    task.title = titleFrom(text) || (sent[0] && sent[0].name) || NEW_CHAT_TITLE;
    if (draft && draft.id === task.id) draft.title = task.title;
    updateTaskSummary(task.id, { title: task.title });
    renderTopbar();
    updateGoal();
  }
  try {
    await api.sendMessage(task.id, text, {
      model: state.modelId || undefined,
      images: images.map((a) => a.path),
      files: files.map((a) => a.path),
    });
  } catch (err) {
    appendSystemRow(chat, String((err && err.message) || err), 'error');
  }
}

/* ------------------------------------------------------------------------ */
/* Overlays: Settings and first-run onboarding                               */
/* ------------------------------------------------------------------------ */

/** Where a provider's key comes from, as one plain phrase. Only the state that
 *  needs acting on is colored. */
const KEY_STATE = {
  stored: { text: 'key stored' },
  env: { text: 'key from env' },
  oauth: { text: 'signed in' },
  not_needed: { text: 'local' },
  missing: { text: 'no key', tone: 'warn' },
};

/** The host of a base URL — enough to tell providers apart, and short enough
 *  not to wrap (Cloudflare's is a path template with an account-id slot). */
function endpointHost(url) {
  const bare = String(url || '').replace(/^https?:\/\//, '').replace(/\/+$/, '');
  return bare.split('/', 1)[0];
}

function closeOverlay() {
  $('overlay-root').replaceChildren();
  document.removeEventListener('keydown', onOverlayKeydown);
}

function onOverlayKeydown(e) {
  if (e.key === 'Escape') closeOverlay();
}

/** Mount `panel` in a dimmed overlay. `dismissable` false = onboarding, which
 *  has nothing behind it worth clicking. */
function showOverlay(panel, { dismissable = true } = {}) {
  const overlay = h('div', {
    class: 'overlay',
    onclick: dismissable ? (e) => { if (e.target === overlay) closeOverlay(); } : null,
  }, panel);
  $('overlay-root').replaceChildren(overlay);
  if (dismissable) document.addEventListener('keydown', onOverlayKeydown);
  return overlay;
}

/** A labelled field: micro-label above the input. */
function field(label, input, hint) {
  return h('label', { class: 'field' },
    h('span', { class: 'field-label' }, label),
    input,
    hint && h('span', { class: 'field-hint' }, hint));
}

const textInput = (attrs) => h('input', { class: 'input mono', type: 'text', spellcheck: 'false', ...attrs });

/** The custom-provider pseudo-preset: any OpenAI-compatible endpoint. */
const CUSTOM_PRESET = {
  name: '', label: 'Custom', kind: 'openai', base_url: '', model: '',
  needs_key: true, custom: true,
};

/**
 * The provider form, shared by onboarding and Settings.
 * @param {Object} preset  a preset, or an existing provider to edit
 */
function providerForm(preset, { submitLabel = 'Save', onSaved, onCancel } = {}) {
  const editing = !!preset.editing;
  const local = preset.kind === 'ollama' || preset.kind === 'llamacpp';
  const needsKey = preset.needs_key !== false && !local;

  const nameInput = textInput({ value: preset.name || '', placeholder: 'name', readonly: editing || null });
  const baseInput = textInput({ value: preset.base_url || '', placeholder: 'https://…' });
  const modelInput = textInput({ value: preset.model || '', placeholder: 'model tag' });
  const keyInput = h('input', {
    class: 'input mono', type: 'password', spellcheck: 'false', autocomplete: 'off',
    placeholder: editing ? 'unchanged' : 'sk-…',
  });
  const note = h('div', { class: 'note error hidden' });
  const submit = h('button', { class: 'btn primary', type: 'submit' }, submitLabel);

  const form = h('form', { class: 'form' },
    ...[
      !editing && preset.custom && field('Name', nameInput),
      (preset.needs_base_url || preset.custom || editing) && field('Base URL', baseInput),
      field('Model', modelInput),
      needsKey && field('API key', keyInput, 'stored in ~/.wizard/credentials.toml'),
    ].filter(Boolean),
    note,
    h('div', { class: 'form-actions' },
      submit,
      onCancel && h('button', { class: 'btn quiet', type: 'button', onclick: onCancel }, 'Cancel')));

  form.onsubmit = async (e) => {
    e.preventDefault();
    note.className = 'note error hidden';
    submit.setAttribute('disabled', '');
    submit.textContent = 'Checking…';
    try {
      const { settings, probe } = await api.saveProvider({
        name: (nameInput.value || preset.name).trim(),
        kind: preset.kind,
        baseUrl: baseInput.value.trim() || preset.base_url,
        model: modelInput.value.trim(),
        apiKey: keyInput.value.trim() || undefined,
      });
      state.settings = settings;
      // Saved either way: a provider that does not answer is still worth
      // keeping on the page so its key or URL can be fixed.
      if (onSaved) onSaved({ settings, probe });
    } catch (err) {
      note.className = 'note error';
      note.textContent = String((err && err.message) || err);
    } finally {
      submit.removeAttribute('disabled');
      submit.textContent = submitLabel;
    }
  };
  return form;
}

/** The provider list both overlays choose from: a name and where it points. */
function presetList(presets, onPick) {
  const row = (p, endpoint) =>
    h('button', { class: 'row row-pick', type: 'button', onclick: () => onPick(p) },
      h('span', { class: 'row-name' }, p.label),
      h('span', { class: 'row-meta mono' }, endpoint));
  return h('div', { class: 'rows' },
    ...presets.map((p) => row(p, endpointHost(p.base_url))),
    row(CUSTOM_PRESET, 'OpenAI-compatible'));
}

/* --- Subscription sign-in (OAuth) ----------------------------------------- */

/** The plans you can sign in to, rather than paste a key for. */
const SIGN_INS = [
  { id: 'chatgpt', label: 'ChatGPT', plan: 'Plus / Pro / Team subscription' },
  { id: 'xai', label: 'xAI', plan: 'SuperGrok subscription' },
];

/**
 * Sign in to a subscription: the browser goes to the provider, which redirects
 * back to the loopback listener that flow bound for itself (never a route this
 * server serves — a provider only redirects to the address registered with its
 * client id), and we poll /api/login for the outcome.
 *
 * The popup is opened synchronously from the click — a browser blocks a window
 * opened after an await — and pointed at the authorize URL once we have it.
 */
function signInRow(id, label, plan, { onDone, onStatus }) {
  const say = onStatus || (() => {});
  return h('button', {
    class: 'row row-pick row-signin', type: 'button',
    dataset: { provider: id }, // `/login <plan>` focuses the row it names
    onclick: async () => {
      const tab = window.open('', '_blank');
      try {
        const url = await api.beginSignIn(id);
        if (tab) tab.location = url;
        else window.location = url; // popups blocked: use this tab
        say(`Waiting for ${label} in the other tab…`);
        await waitForSignIn();
        say(null);
        if (onDone) onDone();
      } catch (err) {
        if (tab) tab.close();
        say(String((err && err.message) || err), true);
      }
    },
  },
    h('span', { class: 'row-name' }, `Sign in with ${label}`),
    h('span', { class: 'row-meta' }, plan));
}

/** Poll until the sign-in in flight finishes, one way or the other. */
async function waitForSignIn({ timeoutMs = 5 * 60 * 1000 } = {}) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    await new Promise((r) => setTimeout(r, 1000));
    const status = await api.signInStatus();
    if (status.state === 'done') return status;
    if (status.state === 'failed') throw new Error(status.error || 'sign-in failed');
    // `idle` means the flow was dropped — the server restarted under us.
    if (status.state === 'idle') throw new Error('the sign-in was not completed');
    if (Date.now() > deadline) throw new Error('the sign-in timed out');
  }
}

/* --- Onboarding ----------------------------------------------------------- */

/** First run: no provider is configured, so wizard cannot answer anything yet. */
function openOnboarding(settings) {
  const body = h('div', { class: 'sheet-body' });
  const sheet = h('div', { class: 'sheet onboard', role: 'dialog', 'aria-label': 'Set up wizard' },
    h('div', { class: 'sheet-head' },
      h('h2', { class: 'sheet-title' }, 'Set up wizard'),
      h('p', { class: 'sheet-sub' }, 'Pick a provider to run the agent on.')),
    body);

  const skip = (label) => h('button', {
    class: 'btn quiet', type: 'button',
    title: 'Wizard cannot answer until a provider is configured',
    onclick: () => { closeOverlay(); bootChat(); },
  }, label);

  const pickStep = () => {
    const note = h('div', { class: 'note hidden' });
    const say = (text, bad) => {
      note.textContent = text || '';
      note.className = `note${bad ? ' error' : ''}${text ? '' : ' hidden'}`;
    };
    const signedIn = async () => {
      state.settings = await api.settings();
      closeOverlay();
      bootChat();
    };
    fillWith(body,
      // A subscription first: it is what most people already have, and it needs
      // no key to paste.
      h('div', { class: 'rows' },
        ...SIGN_INS.map((s) => signInRow(s.id, s.label, s.plan, { onDone: signedIn, onStatus: say }))),
      note,
      h('div', { class: 'block-title with-rule' }, 'or use an API key'),
      presetList(settings.presets, formStep),
      h('div', { class: 'form-actions end' }, skip('Skip')),
    );
  };

  const formStep = (preset) => {
    const done = ({ probe }) => {
      if (!probe.ok) {
        fillWith(body,
          h('div', { class: 'note error' },
            `Saved, but ${preset.label} did not answer: ${probe.error || 'unknown error'}`),
          h('div', { class: 'form-actions' },
            h('button', { class: 'btn primary', type: 'button', onclick: () => formStep(preset) }, 'Try again'),
            skip('Continue anyway')),
        );
        return;
      }
      closeOverlay();
      bootChat();
    };
    fillWith(body,
      h('div', { class: 'block-title' }, preset.label),
      providerForm(preset, { submitLabel: 'Connect', onSaved: done, onCancel: pickStep }),
    );
  };

  pickStep();
  showOverlay(sheet, { dismissable: false });
}

/* --- Settings ------------------------------------------------------------- */

/**
 * Settings. `focus` is what `/provider` and `/login` come in on: this is the one
 * sheet either of them names, so they open it on the part they mean rather than
 * on a page of their own.
 * @param {{focus?: 'providers'|'signin'|null, signIn?: string|null}} [opts]
 *   `signIn` names the plan `/login <xai>` asked for, whose row is focused —
 *   focused, not clicked: the consent window has to open on the user's own press
 *   or the browser blocks it.
 */
async function openSettings({ focus = null, signIn = null } = {}) {
  const body = h('div', { class: 'sheet-body' });
  const foot = h('div', { class: 'sheet-foot mono' });
  const sheet = h('div', { class: 'sheet settings', role: 'dialog', 'aria-label': 'Settings' },
    h('div', { class: 'sheet-head row-between' },
      h('h2', { class: 'sheet-title' }, 'Settings'),
      iconBtn('close', 'Close', closeOverlay)),
    body, foot);
  showOverlay(sheet);
  body.append(h('div', { class: 'note' }, 'Loading…'));
  try {
    state.settings = await api.settings();
  } catch (err) {
    body.replaceChildren(h('div', { class: 'note error' }, String((err && err.message) || err)));
    return;
  }
  renderSettings(body, foot, { focus, signIn });
}

function renderSettings(body, foot, { focus = null, signIn = null } = {}) {
  const s = state.settings;
  const rerender = () => renderSettings(body, foot);

  const providerRow = (p) => {
    const key = KEY_STATE[p.key] || KEY_STATE.missing;
    const status = h('div', { class: 'row-status hidden' });
    const say = (text, bad) => {
      status.textContent = text;
      status.classList.remove('hidden');
      status.classList.toggle('error', !!bad);
    };
    const act = async (fn) => {
      try {
        const out = await fn();
        if (out && out.providers) state.settings = out;
        rerender();
      } catch (err) {
        say(String((err && err.message) || err), true);
      }
    };
    const test = async () => {
      say('Testing…');
      try {
        const probe = await api.testProvider(p.name);
        say(probe.ok ? `Answered — ${probe.models.length || 'no'} models` : probe.error || 'no answer', !probe.ok);
      } catch (err) {
        say(String((err && err.message) || err), true);
      }
    };
    const edit = () => {
      fillWith(body,
        h('div', { class: 'block' },
          h('div', { class: 'block-title' }, p.name),
          providerForm({ ...p, editing: true, needs_key: p.key !== 'not_needed' }, {
            onSaved: rerender, onCancel: rerender,
          })));
    };
    const action = (label, onclick, cls = '') =>
      h('button', { class: `link ${cls}`.trim(), type: 'button', onclick }, label);

    return h('div', { class: 'row row-provider' + (p.active ? ' is-active' : '') },
      h('div', { class: 'row-main' },
        h('div', { class: 'row-line' },
          h('span', { class: 'row-name' }, p.name),
          p.active && h('span', { class: 'tag' }, 'active')),
        h('div', { class: 'row-meta mono' },
          `${p.kind} · ${p.model} · `,
          h('span', { class: key.tone || '' }, key.text)),
        status),
      h('div', { class: 'row-actions' },
        !p.active && action('Use', () => act(() => api.activateProvider(p.name))),
        action('Test', test),
        action('Edit', edit),
        action('Remove', () => act(() => api.removeProvider(p.name)), 'danger')));
  };

  // The picker lives inside the Providers block: it is the same list, one
  // step further in, not a second section competing with it.
  const add = h('div', { class: 'add-provider' });
  const resetAdd = () => {
    fillWith(add, h('button', {
      class: 'row row-add', type: 'button',
      onclick: () => showChoices(),
    }, h('span', { class: 'row-name' }, '+  Add provider')));
  };
  const showChoices = () => {
    const note = h('div', { class: 'note hidden' });
    const say = (text, bad) => {
      note.textContent = text || '';
      note.className = `note${bad ? ' error' : ''}${text ? '' : ' hidden'}`;
    };
    fillWith(add,
      h('div', { class: 'rows' },
        ...SIGN_INS.map((si) => signInRow(si.id, si.label, si.plan, { onDone: rerender, onStatus: say }))),
      note,
      h('div', { class: 'block-title with-rule' }, 'or use an API key'),
      presetList(s.presets, pickPreset),
      h('div', { class: 'form-actions end' },
        h('button', { class: 'btn quiet', type: 'button', onclick: resetAdd }, 'Cancel')),
    );
  };
  const pickPreset = (preset) => {
    fillWith(add,
      h('div', { class: 'block-title' }, `Add ${preset.label}`),
      providerForm(preset, { onSaved: rerender, onCancel: resetAdd }));
  };
  resetAdd();

  // Persists when the field is left, or on Enter. A number with a Save button
  // beside it is one control more than the job needs.
  const steps = h('input', {
    class: 'input num', type: 'number', min: '0', max: '1000', value: String(s.max_steps),
  });
  const agentNote = h('span', { class: 'note inline hidden' });
  steps.addEventListener('keydown', (e) => { if (e.key === 'Enter') steps.blur(); });
  steps.addEventListener('change', async () => {
    const raw = steps.value.trim();
    const value = Number(raw);
    // 0 is a value — it is how the limit is turned off — but an empty box is
    // not: clearing the field must not be read as a request for no limit.
    if (!raw || !Number.isInteger(value) || value < 0 || value === s.max_steps) {
      steps.value = String(s.max_steps);
      return;
    }
    try {
      state.settings = await api.saveSettings({ max_steps: value });
      s.max_steps = state.settings.max_steps;
      agentNote.className = 'note inline';
      agentNote.textContent = s.max_steps === 0 ? 'Saved — no limit' : 'Saved';
    } catch (err) {
      steps.value = String(s.max_steps);
      agentNote.className = 'note inline error';
      agentNote.textContent = String((err && err.message) || err);
    }
  });

  fillWith(body,
    h('div', { class: 'block' },
      h('div', { class: 'block-title' }, 'Providers'),
      s.providers.length
        ? h('div', { class: 'rows' }, ...s.providers.map(providerRow))
        : h('div', { class: 'note' }, 'None configured — wizard cannot answer until one is.'),
      add),
    h('div', { class: 'block' },
      h('div', { class: 'block-title' }, 'Agent'),
      h('div', { class: 'setting' },
        h('div', { class: 'setting-main' },
          h('div', { class: 'setting-name' }, 'Step limit'),
          h('div', { class: 'setting-help' }, 'Tool calls one chat may make per turn. 0 is no limit.')),
        h('div', { class: 'setting-control' }, agentNote, steps))),
  );
  foot.textContent = s.config_path;

  // `/provider` and `/login` open this sheet on the part of it they name: the
  // same picker the "Add provider" row opens, since a command that opened one of
  // its own would be a second way to do this that could drift from the first.
  if (focus) {
    showChoices();
    if (focus !== 'signin') {
      add.scrollIntoView({ block: 'nearest' });
    } else {
      const row = add.querySelector(signIn ? `.row-signin[data-provider="${signIn}"]` : '.row-signin');
      if (row) {
        row.scrollIntoView({ block: 'nearest' });
        row.focus(); // the press that opens the consent window has to be the user's own
      }
    }
  }

  // The composer's model chip reflects whatever the active provider is now.
  api.listModels().then((models) => {
    state.models = models;
    if (state.modelId && !models.some((m) => m.value === state.modelId)) {
      state.modelId = null;
      state.modelLabel = null;
    }
    updateModelChip();
  }).catch(() => { /* the chip keeps its last label */ });
}

/* ------------------------------------------------------------------------ */
/* New chat                                                                  */
/* ------------------------------------------------------------------------ */

/**
 * Open an empty chat and focus the composer; the first message starts the
 * first turn. Without a `cwd` it lands in the directory you are already in
 * (the open chat's, else the one `wizard gui` runs in).
 *
 * Errors are raised, not swallowed: the folder chip's menu shows a bad path
 * in place, while the sidebar button reports it in the transcript.
 * @param {string} [cwd] absolute path of the workspace to open it in
 */
async function newChat(cwd) {
  const created = await api.newChat(cwd || activeDir());
  draft = {
    id: created.id,
    title: NEW_CHAT_TITLE,
    path: created.cwd || state.home.cwd,
    workspace: created.workspace || state.home.name,
  };
  mergeDraft();
  await selectTask(created.id);
  focusComposer();
}

/** `newChat` for the buttons that have nowhere better to show a failure. */
function newChatHere() {
  newChat().catch((err) => {
    appendSystemRow(chat, `Could not start a new chat: ${String((err && err.message) || err)}`, 'error');
  });
}

/** A chat the backend does not list yet: `/api/tasks` only reports sessions
 *  that have messages, so an untouched new chat lives here until it does. */
let draft = null;

/** Splice `draft` into the sidebar until the backend knows about it. */
function mergeDraft() {
  if (!draft) return;
  if (state.workspaces.some((ws) => ws.tasks.some((t) => t.id === draft.id))) {
    draft = null;
    return;
  }
  let ws = state.workspaces.find((w) => w.path === draft.path);
  if (!ws) {
    ws = { name: draft.workspace, path: draft.path, tasks: [] };
    state.workspaces.push(ws);
  }
  ws.tasks.push({ id: draft.id, title: draft.title, updatedAt: Date.now(), status: 'idle' });
}

async function refreshTaskList() {
  try {
    state.workspaces = await api.listTasks();
    mergeDraft();
    resortWorkspaces();
    renderSidebar();
  } catch {
    /* sidebar keeps its last known contents */
  }
}

/* ------------------------------------------------------------------------ */
/* Task selection + streaming lifecycle                                      */
/* ------------------------------------------------------------------------ */

function closeStream() {
  if (streamHandle) {
    streamHandle.close();
    streamHandle = null;
  }
  if (reconnect.timer) {
    clearTimeout(reconnect.timer);
    reconnect.timer = null;
  }
}

function scheduleReconnect(id) {
  streamHandle = null;
  const delay = Math.min(500 * 2 ** reconnect.attempts, 5000);
  reconnect.attempts += 1;
  reconnect.timer = setTimeout(() => {
    reconnect.timer = null;
    if (state.selectedTaskId === id) selectTask(id, { reload: true });
  }, delay);
}

function makeCallbacks(id) {
  // `guard` drops frames for a task the user has moved away from and clears
  // the transient "Retrying…" note; `content` additionally marks the buffer
  // replay as consumed (any content frame means the head state frame passed).
  const guard = (fn) => (...args) => {
    if (state.selectedTaskId !== id) return;
    clearTransient();
    fn(...args);
  };
  const content = (fn) => guard((...args) => {
    replayPending = false;
    fn(...args);
  });
  return {
    onOpen: () => {
      if (state.selectedTaskId !== id) return;
      reconnect.attempts = 0;
      replayPending = true;
    },
    onClose: () => {
      if (state.selectedTaskId !== id) return;
      scheduleReconnect(id);
    },
    onText: content(onText),
    onThinking: content(onThinking),
    onToolCall: content((call) => appendToolCall(chat, call)),
    onToolResult: content((result) => onToolResult(chat, result)),
    onImages: content((batch) => appendImages(chat, batch)),
    onStatus: guard(onStatus), // reads + clears replayPending itself
    onTodo: content(onTodo),
    onUsage: content(onUsage),
    onContext: content(onContext),
    onPlan: content(onPlan),
    onInterview: content(onInterview),
    onNotice: content((text) => systemRow(text)),
    onError: content((message) => systemRow(message, 'error')),
    onTranscriptReset: content(onTranscriptReset),
    onRetrying: content(onRetrying),
    onDone: content(onDone),
    onSubagentRun: content(onSubagentRun),
    onSubagentText: content(onSubagentText),
    onSubagentToolCall: content(onSubagentToolCall),
    onSubagentToolResult: content(onSubagentToolResult),
    onSubagentImages: content(onSubagentImages),
    onSubagentStep: content(onSubagentStep),
    onSubagentDone: content(onSubagentDone),
  };
}

async function selectTask(id, { reload = false } = {}) {
  const seq = ++selectSeq;
  closeStream();
  closePane();
  if (gitPoll) {
    clearInterval(gitPoll);
    gitPoll = null;
  }
  if (paneClock) {
    clearInterval(paneClock);
    paneClock = null;
  }
  closePalette();
  // A rewind's refetch belongs to the chat it was issued in: strand it, and drop
  // the rows it was holding, rather than let them land in the one being opened.
  resetSeq += 1;
  resetRows = null;
  const sameTask = reload && state.task && state.task.id === id;
  state.selectedTaskId = id;
  state.todos = [];
  state.todosHidden = false;
  state.usage = { prompt: 0, completion: 0 };
  state.context = null;
  state.git = null;
  state.lastWorked = null;
  state.subagents = [];
  state.taskState = 'connecting';
  if (!sameTask) {
    state.modelId = null;
    state.modelLabel = null;
    // Files staged for a message that was never sent belong to the chat they
    // were staged in, not to the one being opened.
    state.attachments = [];
    state.commands = [];
    renderAttachTray();
  }
  renderSidebar();

  let task;
  try {
    task = await api.getTask(id);
  } catch (err) {
    if (seq !== selectSeq) return;
    if (!sameTask) {
      state.task = null;
      renderTopbar();
      const scroller = $('transcript');
      const inner = h('div', { class: 'transcript-inner' });
      scroller.replaceChildren(inner);
      resetChatFlow(inner);
      appendSystemRow(chat, `Could not load the task: ${String((err && err.message) || err)}`, 'error');
      renderContextPanel();
    }
    scheduleReconnect(id); // the backend may just be restarting
    return;
  }
  if (seq !== selectSeq) return; // stale response; user moved on

  state.task = task;
  state.taskState = task.status || 'idle';
  renderTopbar();
  renderTranscript();
  renderContextPanel();
  updateModelChip();
  refreshGit();
  loadCommands(task);
  replayPending = true;
  streamHandle = api.streamTask(id, makeCallbacks(id));
}

/* ------------------------------------------------------------------------ */
/* Boot                                                                      */
/* ------------------------------------------------------------------------ */

/** Land in the newest chat of the directory wizard runs in; with none, open a
 *  fresh one there — `wizard gui` in a repo is about that repo. */
async function bootChat() {
  const here = state.workspaces.find((ws) => ws.path === state.home.cwd);
  if (here && here.tasks.length) await selectTask(here.tasks[0].id);
  else newChatHere();
}

async function init() {
  renderComposer();
  initDropTarget();
  renderSidebar();
  renderTopbar();
  try {
    const [workspaces, models, home, settings] = await Promise.all([
      api.listTasks(), api.listModels(), api.home(), api.settings(),
    ]);
    state.workspaces = workspaces;
    state.models = models;
    state.home = home;
    state.settings = settings;
  } catch (err) {
    const scroller = $('transcript');
    const inner = h('div', { class: 'transcript-inner' });
    scroller.replaceChildren(inner);
    resetChatFlow(inner);
    appendSystemRow(chat, `Could not reach the wizard backend: ${String((err && err.message) || err)}`, 'error');
    return;
  }
  resortWorkspaces();
  renderSidebar();
  updateModelChip();

  // Keep the relative ages in the sidebar fresh.
  setInterval(() => renderSidebar(), 60000);

  window.addEventListener('keydown', (e) => {
    if ((e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === 'n') {
      e.preventDefault();
      newChatHere();
    }
    // One press back out of a pane — unless an overlay is up, whose own Escape
    // closes it first.
    if (e.key === 'Escape' && openPane && !$('overlay-root').firstChild) {
      e.preventDefault();
      closePane();
    }
  });

  // Nothing to send a message to yet: set up a provider before opening a chat.
  if (state.settings.first_run) openOnboarding(state.settings);
  else await bootChat();
}

init();
