// Wizard GUI — subagents: the panel's list of runs, their stream handlers,
// and one run's own pane.

import { applyToolSummary } from './api.js';
import { h, icon, fmtDur } from './dom.js';
import { newFlow, autoScroll, breakFlow, appendMessage, appendToolCall, appendImages, appendSystemRow, onToolResult } from './transcript.js';
import { openPane, paneHead, showPane } from './pane.js';
import { ctx } from './context.js';
import { state, syncPaneClock } from './app.js';

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
export function renderSubagentList() {
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
export function updateSubagentRow(run) {
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
export function touchRun(run) {
  updateSubagentRow(run);
  if (openPane && openPane.run === run) updatePaneHead();
}

/* --- The run's stream ------------------------------------------------------ */

/** `subagent_run_started`. A run already listed was re-announced on attach —
 *  a background run outliving the turn that spawned it — so its row stands. */
export function onSubagentRun(info) {
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

export function onSubagentText(id, text) {
  const run = findRun(id);
  if (!run || !text.trim()) return;
  appendToRun(run, { type: 'text', text });
  touchRun(run);
}

export function onSubagentToolCall(id, call) {
  const run = findRun(id);
  if (!run) return;
  appendToRun(run, { type: 'tool', call });
  touchRun(run);
}

export function onSubagentToolResult(id, result) {
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
export function onSubagentImages(id, batch) {
  const run = findRun(id);
  if (!run) return;
  appendToRun(run, { type: 'images', batch });
  touchRun(run);
}

export function onSubagentStep(id, step) {
  const run = findRun(id);
  if (!run) return;
  run.steps = step;
  touchRun(run);
}

export function onSubagentDone(id, result) {
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
