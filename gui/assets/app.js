// Wizard GUI — app entry. Framework-free: the whole UI is rendered from a
// state object into the semantic skeleton in index.html. All data flows
// through the Api seam (api.js): RealApi (HTTP + one WebSocket per open
// task) by default, MockApi with `?mock=1`.

import { createApi } from './api.js';
import { icons, fileIconSvg } from './icons.js';

const api = createApi();

const state = {
  /** @type {import('./api.js').Workspace[]} */
  workspaces: [],
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
  user: { name: 'Teddy', initial: 'T' },
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
/** Prompt of a just-created task, until the backend persists it. */
let pendingPrompt = null;
/** Composer refs. */
let modelLabelEl = null;
let modelMenuEl = null;

/* ------------------------------------------------------------------------ */
/* DOM helpers                                                               */
/* ------------------------------------------------------------------------ */

const $ = (id) => document.getElementById(id);

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
function updateTaskSummary(id, { status, bump } = {}) {
  for (const ws of state.workspaces) {
    const task = ws.tasks.find((t) => t.id === id);
    if (!task) continue;
    if (status) task.status = status;
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
  $('sidebar-nav').replaceChildren(
    iconBtn('panelLeft', 'Toggle sidebar'),
    iconBtn('arrowLeft', 'Back'),
    iconBtn('arrowRight', 'Forward'),
  );

  const sideRow = (name, label, hint, onclick) =>
    h('button', { class: 'side-row', type: 'button', onclick },
      icon(name, 'icon side-row-icon'),
      h('span', { class: 'side-row-label' }, label),
      hint && h('span', { class: 'side-row-hint' }, hint));

  $('sidebar-actions').replaceChildren(
    sideRow('plusSquare', 'New Task', '⌘N', () => openNewTaskModal()),
    sideRow('folder', 'Open Workspace', null, () => openNewTaskModal()),
    sideRow('wand', 'Skills'),
  );

  $('tasks-header').replaceChildren(
    h('span', { class: 'tasks-title' }, 'Tasks'),
    iconBtn('filter', 'Filter tasks', null, 'tasks-filter'),
  );

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

  $('user-row').replaceChildren(
    h('span', { class: 'avatar' }, state.user.initial),
    h('span', { class: 'user-name' }, state.user.name),
    iconBtn('gear', 'Settings', null, 'user-gear'),
  );
}

/* ------------------------------------------------------------------------ */
/* Top bar                                                                   */
/* ------------------------------------------------------------------------ */

function renderTopbar() {
  const t = state.task;
  const branch = state.git && state.git.branch;
  $('topbar').replaceChildren(
    h('div', { class: 'topbar-left' },
      h('h1', { class: 'topbar-title' }, t ? t.title : 'Wizard'),
      t && h('span', { class: 'chip chip-repo', title: t.path },
        icon('folder', 'icon chip-icon'), h('span', { class: 'chip-label' }, t.workspace)),
      t && branch && h('span', { class: 'chip chip-branch', title: 'Current branch' },
        icon('branch', 'icon chip-icon'), h('span', { class: 'chip-label' }, branch)),
      iconBtn('ellipsis', 'More actions'),
    ),
    h('div', { class: 'topbar-right' },
      h('button', { class: 'model-chip', type: 'button', title: t && t.model ? `Model: ${t.model}` : 'Agent' },
        h('span', { class: 'model-avatar', html: icons.sparkles }), icon('chevronDown', 'icon chip-caret')),
      iconBtn('notesPanel', 'Notes'),
      iconBtn('terminal', 'Terminal'),
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
  const note = h('div', { class: 'card-note hidden' });
  const feedback = h('textarea', {
    class: 'text-input plan-feedback hidden', rows: '2',
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
    h('textarea', { class: 'text-input iv-answer', rows: '1', placeholder: 'Your answer (optional)', 'aria-label': q }));
  const rows = questions.map((q, i) =>
    h('div', { class: 'iv-q' }, h('div', { class: 'iv-question' }, q), inputs[i]));
  const note = h('div', { class: 'card-note hidden' });
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
  updateSpinner();
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
  updateSpinner();
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
    class: 'text-input commit-input', type: 'text', placeholder: 'Commit message',
    onkeydown: (e) => { if (e.key === 'Enter') { e.preventDefault(); doCommit(); } },
  });
  ctx.commitGo = h('button', { class: 'btn primary btn-sm', type: 'button', onclick: () => doCommit() }, 'Commit');
  ctx.commitErr = h('div', { class: 'card-note error hidden' });
  ctx.commitBox = h('div', { class: 'commit-editor hidden' },
    ctx.commitInput,
    h('div', { class: 'card-actions' },
      ctx.commitGo,
      h('button', {
        class: 'btn ghost btn-sm', type: 'button',
        onclick: () => { ctx.commitBox.classList.add('hidden'); ctx.commitErr.classList.add('hidden'); },
      }, 'Cancel')),
    ctx.commitErr);
  ctx.commitNote = h('div', { class: 'card-note hidden' });
  ctx.gitSection = h('section', { class: 'ctx-section hidden' },
    h('div', { class: 'ctx-header' }, h('span', {}, 'Git tools')),
    h('button', {
      class: 'ctx-row', type: 'button', title: 'Show changed files',
      onclick: () => ctx.gitFileList.classList.toggle('hidden'),
    },
      icon('diff', 'icon ctx-icon'), h('span', { class: 'ctx-label' }, 'Changes'), ctx.gitCount,
      h('span', { class: 'ctx-right' }, ctx.gitAdd, ctx.gitDel)),
    ctx.gitFileList,
    h('div', { class: 'ctx-row static' },
      icon('branch', 'icon ctx-icon'), ctx.gitBranch, icon('chevronDown', 'icon ctx-caret')),
    h('button', {
      class: 'ctx-row', type: 'button', title: 'Commit all changes',
      onclick: () => {
        ctx.commitBox.classList.toggle('hidden');
        ctx.commitNote.classList.add('hidden');
        if (!ctx.commitBox.classList.contains('hidden')) ctx.commitInput.focus();
      },
    },
      icon('commitNode', 'icon ctx-icon'), h('span', { class: 'ctx-label' }, 'Commit'),
      icon('ellipsis', 'icon ctx-caret')),
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

function closeModelMenu() {
  if (modelMenuEl) {
    modelMenuEl.remove();
    modelMenuEl = null;
    document.removeEventListener('click', onDocClickForMenu, true);
  }
}

function onDocClickForMenu(e) {
  if (modelMenuEl && !modelMenuEl.contains(e.target) && !e.target.closest('.model-select')) closeModelMenu();
}

function toggleModelMenu(anchor) {
  if (modelMenuEl) {
    closeModelMenu();
    return;
  }
  if (!state.models.length) return;
  const menu = h('div', { class: 'menu model-menu', role: 'menu' });
  let lastProvider = null;
  const choose = (m) => {
    state.modelId = m.value;
    state.modelLabel = m.label;
    updateModelChip();
    closeModelMenu();
  };
  for (const m of state.models) {
    if (m.provider !== lastProvider) {
      menu.append(h('div', { class: 'menu-head' }, m.provider));
      lastProvider = m.provider;
    }
    const selected = state.modelId === m.value || (state.modelId == null && m.isDefault);
    menu.append(h('button', {
      class: 'menu-item' + (selected ? ' selected' : ''),
      type: 'button', role: 'menuitem',
      onclick: () => choose(m),
    },
      h('span', { class: 'menu-item-label' }, m.label),
      m.isDefault && h('span', { class: 'menu-item-hint' }, 'default'),
      selected && h('span', { class: 'menu-check', html: icons.check, 'aria-hidden': 'true' })));
  }
  anchor.append(menu);
  modelMenuEl = menu;
  document.addEventListener('click', onDocClickForMenu, true);
}

function updateSpinner() {
  const spin = document.querySelector('.spinner-icon.composer-spin');
  const btn = document.querySelector('.spinner-btn');
  const working = state.taskState === 'working';
  if (spin) spin.classList.toggle('spinning', working);
  if (btn) {
    btn.classList.toggle('active', working);
    btn.title = working ? 'Stop the current turn' : 'Idle';
  }
}

function renderComposer() {
  const form = $('composer');
  const input = h('textarea', {
    class: 'composer-input', rows: '1',
    placeholder: 'Ask for follow-up changes', 'aria-label': 'Ask for follow-up changes',
  });
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
      onclick: (e) => { e.stopPropagation(); toggleModelMenu(modelAnchor); },
    }, modelLabelEl, icon('chevronDown', 'icon chip-caret')));

  form.replaceChildren(
    input,
    h('div', { class: 'composer-row' },
      iconBtn('plus', 'Attach', null, 'composer-plus'),
      // Wizard has no permission gating: this chip states the agent mode.
      h('span', { class: 'chip ghost-chip mode-chip', title: 'Agent mode — GUI sessions run autonomously' },
        icon('wand', 'icon chip-icon'), h('span', { class: 'chip-label' }, 'Sovereign')),
      h('span', { class: 'composer-spacer' }),
      h('button', {
        class: 'icon-btn spinner-btn', type: 'button', title: 'Idle',
        onclick: () => {
          if (state.taskState === 'working' && state.selectedTaskId) {
            try { api.cancel(state.selectedTaskId); } catch { /* not connected */ }
          }
        },
      }, h('span', { class: 'spinner-icon composer-spin', html: icons.spinner, 'aria-hidden': 'true' })),
      modelAnchor,
      h('button', { class: 'send-btn', type: 'submit', title: 'Send', 'aria-label': 'Send' }, icon('sendArrow')),
    ),
  );

  form.onsubmit = (e) => {
    e.preventDefault();
    const text = input.value.trim();
    if (!text || !state.task) return;
    input.value = '';
    input.style.height = 'auto';
    send(text);
  };
}

async function send(text) {
  appendPromptCard(text);
  pendingTurnStart = Date.now();
  autoScroll(true);
  try {
    await api.sendMessage(state.task.id, text, { model: state.modelId || undefined });
  } catch (err) {
    appendSystemRow(String((err && err.message) || err), 'error');
  }
}

/* ------------------------------------------------------------------------ */
/* New Task modal                                                            */
/* ------------------------------------------------------------------------ */

function closeModal() {
  $('modal-root').replaceChildren();
  document.removeEventListener('keydown', onModalKeydown);
}

function onModalKeydown(e) {
  if (e.key === 'Escape') closeModal();
}

async function openNewTaskModal() {
  const root = $('modal-root');
  if (root.childElementCount) return;

  const wsSelect = h('select', { class: 'text-input select' });
  const pathInput = h('input', {
    class: 'text-input', type: 'text', spellcheck: 'false',
    placeholder: '/absolute/path/to/workspace',
  });
  const promptInput = h('textarea', { class: 'text-input', rows: '4', placeholder: 'What should the agent do?' });
  const modelSelect = h('select', { class: 'text-input select' });
  const errEl = h('div', { class: 'card-note error hidden' });
  const createBtn = h('button', { class: 'btn primary', type: 'submit' }, 'Create task');

  wsSelect.append(h('option', { value: '' }, 'Custom path…'));
  wsSelect.addEventListener('change', () => {
    if (wsSelect.value) pathInput.value = wsSelect.value;
  });
  modelSelect.append(h('option', { value: '' }, 'Default model'));
  for (const m of state.models) {
    if (m.isDefault) continue;
    modelSelect.append(h('option', { value: m.value || '' }, `${m.label} (${m.provider})`));
  }

  const form = h('form', { class: 'modal-form' },
    h('div', { class: 'form-row' }, h('label', { class: 'form-label' }, 'Workspace'), wsSelect, pathInput),
    h('div', { class: 'form-row' }, h('label', { class: 'form-label' }, 'Prompt'), promptInput),
    h('div', { class: 'form-row' }, h('label', { class: 'form-label' }, 'Model'), modelSelect),
    errEl,
    h('div', { class: 'card-actions modal-actions' },
      createBtn,
      h('button', { class: 'btn ghost', type: 'button', onclick: closeModal }, 'Cancel')));

  form.onsubmit = async (e) => {
    e.preventDefault();
    const cwd = pathInput.value.trim();
    const prompt = promptInput.value.trim();
    errEl.classList.add('hidden');
    if (!cwd.startsWith('/')) {
      errEl.textContent = 'The workspace must be an absolute path.';
      errEl.classList.remove('hidden');
      return;
    }
    if (!prompt) {
      errEl.textContent = 'The prompt must not be empty.';
      errEl.classList.remove('hidden');
      return;
    }
    createBtn.setAttribute('disabled', '');
    createBtn.textContent = 'Creating…';
    try {
      const { id } = await api.newTask({ cwd, prompt, model: modelSelect.value || undefined });
      pendingPrompt = { id, text: prompt };
      closeModal();
      await refreshTaskList();
      await selectTask(id);
    } catch (err) {
      errEl.textContent = String((err && err.message) || err);
      errEl.classList.remove('hidden');
      createBtn.removeAttribute('disabled');
      createBtn.textContent = 'Create task';
    }
  };

  const overlay = h('div', {
    class: 'modal-overlay',
    onclick: (e) => { if (e.target === overlay) closeModal(); },
  },
    h('div', { class: 'modal', role: 'dialog', 'aria-label': 'New Task' },
      h('div', { class: 'modal-head' },
        h('span', { class: 'modal-title' }, 'New Task'),
        iconBtn('close', 'Close', closeModal)),
      form));
  root.append(overlay);
  document.addEventListener('keydown', onModalKeydown);
  promptInput.focus();

  try {
    const workspaces = await api.workspaces();
    for (const ws of workspaces) {
      wsSelect.append(h('option', { value: ws.cwd }, `${ws.name} — ${ws.cwd}`));
    }
    if (workspaces.length) {
      wsSelect.value = workspaces[0].cwd;
      if (!pathInput.value) pathInput.value = workspaces[0].cwd;
    }
  } catch {
    /* workspaces are a convenience; the path field still works */
  }
}

async function refreshTaskList() {
  try {
    state.workspaces = await api.listTasks();
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

  // A just-created task may not have persisted its first prompt yet.
  if (pendingPrompt && pendingPrompt.id === id) {
    if (!task.transcript.some((i) => i.type === 'user')) {
      task.transcript = [{ type: 'user', text: pendingPrompt.text }, ...task.transcript];
      if (!task.title || task.title === id) {
        task.title = pendingPrompt.text.length > 90 ? `${pendingPrompt.text.slice(0, 89)}…` : pendingPrompt.text;
      }
      pendingTurnStart = pendingTurnStart || Date.now();
    } else {
      pendingPrompt = null;
    }
  }

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

async function init() {
  renderComposer();
  renderSidebar();
  renderTopbar();
  try {
    const [workspaces, models] = await Promise.all([api.listTasks(), api.listModels()]);
    state.workspaces = workspaces;
    state.models = models;
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
  const mostRecent = state.workspaces.flatMap((ws) => ws.tasks).sort((a, b) => b.updatedAt - a.updatedAt)[0];
  if (mostRecent) await selectTask(mostRecent.id);

  // Keep the relative ages in the sidebar fresh.
  setInterval(() => renderSidebar(), 60000);

  window.addEventListener('keydown', (e) => {
    if ((e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === 'n') {
      e.preventDefault();
      openNewTaskModal();
    }
  });
}

init();
