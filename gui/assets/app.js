// Wizard GUI — app entry. Framework-free: the whole UI is rendered from a
// state object into the semantic skeleton in index.html. All data flows
// through the Api seam (api.js): RealApi (HTTP + one WebSocket per open
// task) by default, MockApi with `?mock=1`.

import { NEW_CHAT_TITLE, createApi } from './api.js';
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
  usage: { prompt: 0, completion: 0 },
  /** @type {import('./api.js').GitInfo | null} */
  git: null,
  /** Elapsed label of the last finished turn ("3m 1s"). */
  lastWorked: null,
};

/** @type {import('./api.js').StreamHandle | null} */
let streamHandle = null;
/** Element new streamed content is appended into (live worked-body or transcript root). */
let appendTarget = null;
/** Paragraph currently receiving streamed text deltas. */
let streamPara = null;
/** Thinking block currently receiving thinking deltas: {block, body}. */
let streamThink = null;
/** The in-flight turn's collapsible section: {section, body, labelEl, startedAt}. */
let liveTurn = null;
/** Turn start captured when the user hits send (beats the state frame). */
let pendingTurnStart = null;
/** True between socket open and the first frame: a `working` state then
 *  marks the start of a mid-turn buffer replay. */
let replayPending = false;
const reconnect = { attempts: 0, timer: null };
/** call_id -> row updater for live tool_finished frames. */
let toolRows = new Map();
/** Active aggregation group for consecutive explore/write calls. */
let toolGroup = null;
/** Transient "Retrying…" row, removed on the next frame. */
let transientNote = null;
let gitPoll = null;
let gitSeq = 0;
let selectSeq = 0;
/** Composer refs. */
let composerInput = null;
let modelLabelEl = null;
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
    h('span', { class: 'home-dir', title: state.home.cwd || 'Working directory' },
      icon('folder', 'icon home-icon'),
      h('span', { class: 'home-name' }, state.home.name || 'Wizard')),
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

function autoScroll(force = false) {
  const scroller = $('transcript');
  const nearBottom = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 180;
  if (force || nearBottom) scroller.scrollTop = scroller.scrollHeight;
}

/** Break the streaming text/thinking/tool-group flow (before a new block). */
function breakFlow() {
  streamPara = null;
  toolGroup = null;
  collapseThinking();
}

function collapseThinking() {
  if (streamThink) {
    streamThink.block.classList.add('collapsed');
    streamThink = null;
  }
}

function appendPromptCard(text) {
  breakFlow();
  const inner = transcriptInner();
  inner.append(h('div', { class: 'prompt-card' }, text));
  appendTarget = inner;
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
  appendTarget = body;
  return { section, body, labelEl };
}

function appendThinkingBlock(text, collapsed) {
  const body = h('div', { class: 'thinking-body' }, text || '');
  const block = h('div', { class: 'thinking-block' + (collapsed ? ' collapsed' : '') },
    h('button', {
      class: 'thinking-head', type: 'button',
      onclick: () => block.classList.toggle('collapsed'),
    }, icon('chevronDown', 'icon thinking-caret'), h('span', {}, 'Thinking')),
    body);
  appendTarget.append(block);
  return { block, body };
}

function appendSystemRow(text, cls = '') {
  streamPara = null;
  toolGroup = null;
  collapseThinking();
  const row = h('div', { class: `system-row ${cls}`.trim() }, text);
  appendTarget.append(row);
  autoScroll();
  return row;
}

/* --- Tool rows ------------------------------------------------------------ */

const NOUN_PLURALS = { file: 'files', listing: 'listings', search: 'searches', 'git check': 'git checks' };

function countsText(counts) {
  return Array.from(counts)
    .map(([noun, n]) => `${n} ${n === 1 ? noun : NOUN_PLURALS[noun] || `${noun}s`}`)
    .join(', ');
}

function startExploreGroup() {
  const counts = new Map();
  const sublist = h('div', { class: 'tool-sublist hidden' });
  const detail = h('span', { class: 'tool-args' }, '');
  const status = h('span', { class: 'tool-status hidden' }, 'Failed');
  const row = h('button', {
    class: 'tool-row tool-row-btn', type: 'button', title: 'Show the individual calls',
    onclick: () => sublist.classList.toggle('hidden'),
  }, icon('magnifier', 'icon tool-icon'), h('span', { class: 'tool-name' }, 'Explored'), detail, status);
  appendTarget.append(h('div', { class: 'tool-group' }, row, sublist));
  return { kind: 'explore', parent: appendTarget, counts, sublist, detail, status };
}

function addExploreCall(group, call) {
  const textEl = h('span', { class: 'subline-text' }, call.detail || '');
  const line = h('div', {
    class: 'tool-subline' + (call.status === 'pending' ? ' pending' : '') + (call.status === 'failed' ? ' failed' : ''),
  }, h('span', { class: 'subline-noun' }, call.noun), textEl);
  group.sublist.append(line);
  group.counts.set(call.noun, (group.counts.get(call.noun) || 0) + 1);
  group.detail.textContent = countsText(group.counts);
  if (call.status === 'failed') group.status.classList.remove('hidden');
  toolRows.set(call.id, {
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

function startWriteGroup() {
  const addEl = h('span', { class: 'diffstat-add hidden' });
  const delEl = h('span', { class: 'diffstat-del hidden' });
  const status = h('span', { class: 'tool-status hidden' }, 'Failed');
  const row = h('div', { class: 'tool-row' },
    icon('pencil', 'icon tool-icon'), h('span', { class: 'tool-name' }, 'Wrote'), addEl, delEl, status);
  appendTarget.append(row);
  const group = { kind: 'write', parent: appendTarget, row, addEl, delEl, status, totals: { add: 0, del: 0 } };
  group.bump = (diffstat) => {
    if (!diffstat) return;
    group.totals.add += diffstat.additions || 0;
    group.totals.del += diffstat.deletions || 0;
    if (group.totals.add) { addEl.textContent = `+${group.totals.add}`; addEl.classList.remove('hidden'); }
    if (group.totals.del) { delEl.textContent = `-${group.totals.del}`; delEl.classList.remove('hidden'); }
  };
  return group;
}

function addWriteCall(group, call) {
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
  toolRows.set(call.id, {
    update(result) {
      for (const chip of chips) chip.classList.remove('pending');
      if (result.status === 'failed') {
        for (const chip of chips) chip.classList.add('failed');
        group.status.classList.remove('hidden');
      }
    },
  });
}

function appendStandaloneTool(call) {
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
  appendTarget.append(row);
  toolRows.set(call.id, {
    update(result) {
      row.classList.remove('pending');
      if (detailEl && result.summary) detailEl.textContent = result.summary;
      if (result.status === 'failed') status.classList.remove('hidden');
    },
  });
}

/**
 * Append one tool call, aggregating consecutive explore calls into a single
 * "Explored" row (counts + expandable sublist) and consecutive writes into a
 * single "Wrote" row (file chips + running diffstat).
 * @param {import('./api.js').ToolCall} call
 */
function appendToolCall(call) {
  collapseThinking();
  streamPara = null;
  if (call.tool === 'explore' && call.noun) {
    if (!(toolGroup && toolGroup.kind === 'explore' && toolGroup.parent === appendTarget)) {
      toolGroup = startExploreGroup();
    }
    addExploreCall(toolGroup, call);
  } else if (call.tool === 'write' && call.name) {
    if (!(toolGroup && toolGroup.kind === 'write' && toolGroup.parent === appendTarget)) {
      toolGroup = startWriteGroup();
    }
    addWriteCall(toolGroup, call);
  } else {
    toolGroup = null;
    appendStandaloneTool(call);
  }
  autoScroll();
}

function onToolResult(result) {
  const row = toolRows.get(result.callId);
  if (row) row.update(result);
}

/* --- Plan review / interview cards ---------------------------------------- */

/** Minimal, injection-safe markdown-ish renderer for plan cards. */
function renderMarkdownInto(root, md) {
  const inline = (text) => {
    const out = [];
    const re = /(`[^`]+`|\*\*[^*]+\*\*)/g;
    let last = 0;
    let m;
    while ((m = re.exec(text))) {
      if (m.index > last) out.push(text.slice(last, m.index));
      const tok = m[0];
      if (tok.startsWith('`')) out.push(h('code', { class: 'md-code' }, tok.slice(1, -1)));
      else out.push(h('strong', {}, tok.slice(2, -2)));
      last = m.index + tok.length;
    }
    if (last < text.length) out.push(text.slice(last));
    return out;
  };
  let list = null;
  let para = [];
  let pre = null;
  const flushPara = () => {
    if (para.length) {
      root.append(h('p', {}, ...inline(para.join(' '))));
      para = [];
    }
  };
  for (const line of String(md).split('\n')) {
    if (pre) {
      if (/^```/.test(line)) { root.append(pre); pre = null; } else pre.textContent += `${line}\n`;
      continue;
    }
    if (/^```/.test(line)) { flushPara(); list = null; pre = h('pre', { class: 'md-pre' }); continue; }
    const heading = /^(#{1,4})\s+(.*)/.exec(line);
    if (heading) {
      flushPara(); list = null;
      root.append(h('div', { class: `md-h md-h${heading[1].length}` }, ...inline(heading[2])));
      continue;
    }
    const li = /^\s*(?:[-*]|\d+[.)])\s+(.*)/.exec(line);
    if (li) {
      flushPara();
      if (!list) { list = h('ul', { class: 'md-list' }); root.append(list); }
      list.append(h('li', {}, ...inline(li[1])));
      continue;
    }
    if (!line.trim()) { flushPara(); list = null; continue; }
    para.push(line.trim());
  }
  flushPara();
  if (pre) root.append(pre);
}

function onPlan(plan) {
  breakFlow();
  const id = state.selectedTaskId;
  const body = h('div', { class: 'plan-body' });
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
  appendTarget.append(card);
  autoScroll(true);
}

function onInterview(questions) {
  breakFlow();
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
  appendTarget.append(card);
  autoScroll(true);
}

/* ------------------------------------------------------------------------ */
/* Transcript: replay + live turn                                            */
/* ------------------------------------------------------------------------ */

function renderTranscript() {
  const scroller = $('transcript');
  const inner = h('div', { class: 'transcript-inner' });
  scroller.replaceChildren(inner);
  streamPara = null;
  streamThink = null;
  toolGroup = null;
  liveTurn = null;
  toolRows = new Map();
  appendTarget = inner;
  if (!state.task) return;

  for (const item of state.task.transcript) {
    if (item.type === 'user') {
      appendPromptCard(item.text);
    } else if (item.type === 'worked') {
      breakFlow();
      appendWorkedSection(item.label || 'Worked');
    } else if (item.type === 'text') {
      breakFlow();
      appendTarget.append(h('p', { class: 'msg-text' }, item.text));
    } else if (item.type === 'thinking') {
      streamPara = null;
      toolGroup = null;
      appendThinkingBlock(item.text, true);
    } else if (item.type === 'tool') {
      appendToolCall(item);
    } else if (item.type === 'notice') {
      appendSystemRow(item.text);
    }
  }
  breakFlow();
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
  appendTarget = inner;
  streamPara = null;
  streamThink = null;
  toolGroup = null;
  liveTurn = null;
  toolRows = new Map();
}

function beginLiveTurn() {
  liveTurn = null; // any previous section was finalized or truncated
  breakFlow();
  const { section, body, labelEl } = appendWorkedSection('Working…', true);
  liveTurn = { section, body, labelEl, startedAt: pendingTurnStart || Date.now() };
  pendingTurnStart = null;
  toolRows = new Map();
  autoScroll(true);
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
  collapseThinking();
  toolGroup = null;
  if (!streamPara || !streamPara.isConnected) {
    streamPara = h('p', { class: 'msg-text streaming' });
    appendTarget.append(streamPara);
  }
  streamPara.textContent += delta;
  autoScroll();
}

function onThinking(delta) {
  streamPara = null;
  toolGroup = null;
  if (!streamThink || !streamThink.body.isConnected) {
    streamThink = appendThinkingBlock('', false);
  }
  streamThink.body.textContent += delta;
  autoScroll();
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

function onRetrying(attempt) {
  transientNote = h('div', { class: 'system-row retrying' },
    h('span', { class: 'spinner-icon spinning', html: icons.spinner, 'aria-hidden': 'true' }),
    ` Retrying (attempt ${attempt})…`);
  appendTarget.append(transientNote);
  autoScroll();
}

function onDone(reason) {
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

/** Live refs into the context panel so streams update without re-rendering
 *  (a full re-render would eat the commit editor mid-keystroke). */
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
  ctx.commitInput = h('input', {
    class: 'input commit-input', type: 'text', placeholder: 'Commit message',
    onkeydown: (e) => { if (e.key === 'Enter') { e.preventDefault(); doCommit(); } },
  });
  ctx.commitGo = h('button', { class: 'btn primary btn-sm', type: 'button', onclick: () => doCommit() }, 'Commit');
  ctx.commitErr = h('div', { class: 'note error hidden' });
  ctx.commitBox = h('div', { class: 'commit-editor hidden' },
    ctx.commitInput,
    h('div', { class: 'card-actions' },
      ctx.commitGo,
      h('button', {
        class: 'btn ghost btn-sm', type: 'button',
        onclick: () => { ctx.commitBox.classList.add('hidden'); ctx.commitErr.classList.add('hidden'); },
      }, 'Cancel')),
    ctx.commitErr);
  ctx.commitNote = h('div', { class: 'note hidden' });
  ctx.gitSection = h('section', { class: 'ctx-section hidden' },
    h('div', { class: 'ctx-header' }, h('span', {}, 'Git tools')),
    h('button', {
      class: 'ctx-row', type: 'button', title: 'Show changed files',
      onclick: () => ctx.gitFileList.classList.toggle('hidden'),
    },
      icon('diff', 'icon ctx-icon'), h('span', { class: 'ctx-label' }, 'Changes'), ctx.gitCount,
      h('span', { class: 'ctx-right' }, ctx.gitAdd, ctx.gitDel)),
    ctx.gitFileList,
    h('div', { class: 'ctx-row static' }, icon('branch', 'icon ctx-icon'), ctx.gitBranch),
    h('button', {
      class: 'ctx-row', type: 'button', title: 'Commit all changes',
      onclick: () => {
        ctx.commitBox.classList.toggle('hidden');
        ctx.commitNote.classList.add('hidden');
        if (!ctx.commitBox.classList.contains('hidden')) ctx.commitInput.focus();
      },
    },
      icon('commitNode', 'icon ctx-icon'), h('span', { class: 'ctx-label' }, 'Commit'),
      icon('chevronDown', 'icon ctx-caret')),
    ctx.commitBox,
    ctx.commitNote);

  // --- Goal ---
  ctx.goalStatus = h('span', { class: 'ctx-header-right' }, '');
  ctx.goalText = h('div', { class: 'goal-text' }, t.title);
  ctx.goalMeta = h('div', { class: 'goal-meta' }, '');
  const goalSection = h('section', { class: 'ctx-section' },
    h('div', { class: 'ctx-header' }, h('span', {}, 'Goal'), ctx.goalStatus),
    h('div', { class: 'goal-row' },
      icon('target', 'icon goal-icon'),
      h('div', { class: 'goal-main' }, ctx.goalText, ctx.goalMeta)));

  // --- Progress ---
  ctx.progressList = h('div', { class: 'progress-list' });
  ctx.progressSection = h('section', { class: 'ctx-section hidden' },
    h('div', { class: 'ctx-header' }, h('span', {}, 'Progress')),
    ctx.progressList);

  root.append(h('div', { class: 'context-card' }, ctx.gitSection, goalSection, ctx.progressSection));
  updateGitCard();
  updateGoal();
  updateProgress();
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
      h('div', { class: 'git-file', title: f.path },
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
  if (!items.length) {
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

async function doCommit() {
  if (!ctx || !state.task) return;
  const message = ctx.commitInput.value.trim();
  if (!message) {
    ctx.commitInput.focus();
    return;
  }
  ctx.commitGo.setAttribute('disabled', '');
  ctx.commitGo.textContent = 'Committing…';
  ctx.commitErr.classList.add('hidden');
  try {
    const out = await api.commit(state.task, message);
    ctx.commitInput.value = '';
    ctx.commitBox.classList.add('hidden');
    ctx.commitNote.textContent = `Committed ${String(out.sha || '').slice(0, 7)}`;
    ctx.commitNote.classList.remove('hidden');
    refreshGit();
  } catch (err) {
    ctx.commitErr.textContent = String((err && err.message) || err);
    ctx.commitErr.classList.remove('hidden');
  } finally {
    ctx.commitGo.removeAttribute('disabled');
    ctx.commitGo.textContent = 'Commit';
  }
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
        appendSystemRow(`Switched to ${now}`);
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
    appendSystemRow('Stopping…');
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
    placeholder: 'Ask wizard to change something', 'aria-label': 'Message wizard',
  });
  composerInput = input;
  input.addEventListener('input', () => {
    input.style.height = 'auto';
    input.style.height = `${Math.min(input.scrollHeight, 160)}px`;
  });
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      form.requestSubmit();
    }
  });

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
    h('div', { class: 'composer-row' },
      // Wizard has no permission gating: this chip states the agent mode.
      h('span', { class: 'chip ghost-chip mode-chip', title: 'Agent mode — GUI sessions run autonomously' },
        icon('wand', 'icon chip-icon'), h('span', { class: 'chip-label' }, 'Sovereign')),
      h('span', { class: 'composer-spacer' }),
      modelAnchor,
      sendBtn,
    ),
  );
  updateSendButton();

  form.onsubmit = (e) => {
    e.preventDefault();
    const text = input.value.trim();
    // One turn at a time: the backend refuses a second, so do not pretend.
    if (!text || !state.task || state.taskState === 'working') return;
    input.value = '';
    input.style.height = 'auto';
    send(text);
  };
}

/** A chat's title: its first message, one line, bounded. */
function titleFrom(text) {
  const line = text.split('\n', 1)[0].trim() || text.trim();
  return line.length > 90 ? `${line.slice(0, 89)}…` : line;
}

async function send(text) {
  appendPromptCard(text);
  pendingTurnStart = Date.now();
  autoScroll(true);
  // The first message in an empty chat names it, everywhere it is shown.
  if (state.task && state.task.title === NEW_CHAT_TITLE) {
    state.task.title = titleFrom(text);
    if (draft && draft.id === state.task.id) draft.title = state.task.title;
    updateTaskSummary(state.task.id, { title: state.task.title });
    renderTopbar();
    updateGoal();
  }
  try {
    await api.sendMessage(state.task.id, text, { model: state.modelId || undefined });
  } catch (err) {
    appendSystemRow(String((err && err.message) || err), 'error');
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
  { id: 'xai', label: 'xAI', plan: 'SuperGrok subscription' },
];

/**
 * Sign in to a subscription: the browser goes to the provider, the provider
 * redirects back to a route this server owns, and we watch for the outcome.
 *
 * The popup is opened synchronously from the click — a browser blocks a window
 * opened after an await — and pointed at the authorize URL once we have it.
 */
function signInRow(id, label, plan, { onDone, onStatus }) {
  const say = onStatus || (() => {});
  return h('button', {
    class: 'row row-pick row-signin', type: 'button',
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
    // `idle` means the flow was dropped (server restarted, or it expired).
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

async function openSettings() {
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
  renderSettings(body, foot);
}

function renderSettings(body, foot) {
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
    class: 'input num', type: 'number', min: '1', max: '1000', value: String(s.max_steps),
  });
  const agentNote = h('span', { class: 'note inline hidden' });
  steps.addEventListener('keydown', (e) => { if (e.key === 'Enter') steps.blur(); });
  steps.addEventListener('change', async () => {
    const value = Number(steps.value);
    if (!value || value === s.max_steps) return;
    try {
      state.settings = await api.saveSettings({ max_steps: value });
      s.max_steps = state.settings.max_steps;
      agentNote.className = 'note inline';
      agentNote.textContent = 'Saved';
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
          h('div', { class: 'setting-help' }, 'Tool calls one chat may make per turn.')),
        h('div', { class: 'setting-control' }, agentNote, steps))),
  );
  foot.textContent = s.config_path;

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
    appendSystemRow(`Could not start a new chat: ${String((err && err.message) || err)}`, 'error');
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
    onToolCall: content(appendToolCall),
    onToolResult: content(onToolResult),
    onStatus: guard(onStatus), // reads + clears replayPending itself
    onTodo: content(onTodo),
    onUsage: content(onUsage),
    onPlan: content(onPlan),
    onInterview: content(onInterview),
    onNotice: content((text) => appendSystemRow(text)),
    onError: content((message) => appendSystemRow(message, 'error')),
    onRetrying: content(onRetrying),
    onDone: content(onDone),
  };
}

async function selectTask(id, { reload = false } = {}) {
  const seq = ++selectSeq;
  closeStream();
  if (gitPoll) {
    clearInterval(gitPoll);
    gitPoll = null;
  }
  const sameTask = reload && state.task && state.task.id === id;
  state.selectedTaskId = id;
  state.todos = [];
  state.usage = { prompt: 0, completion: 0 };
  state.git = null;
  state.lastWorked = null;
  state.taskState = 'connecting';
  if (!sameTask) {
    state.modelId = null;
    state.modelLabel = null;
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
      appendTarget = inner;
      appendSystemRow(`Could not load the task: ${String((err && err.message) || err)}`, 'error');
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
    appendTarget = inner;
    appendSystemRow(`Could not reach the wizard backend: ${String((err && err.message) || err)}`, 'error');
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
  });

  // Nothing to send a message to yet: set up a provider before opening a chat.
  if (state.settings.first_run) openOnboarding(state.settings);
  else await bootChat();
}

init();
