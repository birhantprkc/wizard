// Wizard GUI — app entry. Framework-free: the whole UI is rendered from a
// state object into the semantic skeleton in index.html. All data flows
// through the Api seam (api.js): RealApi (HTTP + one WebSocket per open
// task) by default, MockApi with `?mock=1`.

import { NEW_CHAT_TITLE, createApi } from './api.js';
import { icons } from './icons.js';
import { $, MOD_KEY, h, icon, iconBtn, relAge, fmtDur } from './dom.js';
import { renderMarkdownInto, newMarkdownView, pushMarkdown } from './markdown.js';
import {
  transcriptInner, chat, autoScroll, breakFlow, endStream, collapseThinking,
  appendPromptCard, appendWorkedSection, appendMessage, appendThinkingBlock,
  appendSystemRow, appendToolCall, onToolResult, appendImages,
} from './transcript.js';
import { openPane, closePane } from './pane.js';
import { renderContextPanel, updateContextMeter, updateGoal, updateProgress, refreshGit } from './context.js';
import {
  touchRun, onSubagentRun, onSubagentText, onSubagentToolCall, onSubagentToolResult,
  onSubagentImages, onSubagentStep, onSubagentDone,
} from './subagents.js';
import { initDropTarget, renderAttachTray } from './attach.js';
import { loadCommands, closePalette } from './palette.js';
import {
  renderComposer, updateModelChip, updateSendButton, focusComposer,
  activeDir, openDirMenu, openBranchMenu,
} from './composer.js';
import { openSettings, openOnboarding } from './settings.js';

export const api = createApi();

export const state = {
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
   *  @type {import('./attach.js').StagedFile[]} */
  attachments: [],
  /** @type {import('./api.js').GitInfo | null} */
  git: null,
  /** Elapsed label of the last finished turn ("3m 1s"). */
  lastWorked: null,
  /** This task's subagent runs, oldest first (the panel lists them the other
   *  way up). @type {import('./subagents.js').SubagentRunView[]} */
  subagents: [],
};

/** @type {import('./api.js').StreamHandle | null} */
let streamHandle = null;
/** The in-flight turn's collapsible section: {section, body, labelEl, startedAt}. */
export let liveTurn = null;
/** Turn start captured when the user hits send (beats the state frame). */
let pendingTurnStart = null;
/** True between socket open and the first frame: a `working` state then
 *  marks the start of a mid-turn buffer replay. */
let replayPending = false;
const reconnect = { attempts: 0, timer: null };
/** Transient "Retrying…" row, removed on the next frame. */
let transientNote = null;
let gitPoll = null;
export let selectSeq = 0;
/** The post-rewind refetch in flight; a later one wins. */
let resetSeq = 0;
/** System rows that arrive during that refetch, held for the transcript it is
 *  rebuilding. Null when no refetch is in flight. @type {Array<{text:string,cls:string|undefined}>|null} */
let resetRows = null;

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

export function renderTopbar() {
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

/** Ticks the elapsed clock of the runs still going. */
let paneClock = null;

/** Keep the elapsed time honest while a run is going; stop when none is. */
export function syncPaneClock() {
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

/** A chat's title: its first message, one line, bounded. */
function titleFrom(text) {
  const line = text.split('\n', 1)[0].trim() || text.trim();
  return line.length > 90 ? `${line.slice(0, 89)}…` : line;
}

/**
 * @param {string} text
 * @param {import('./attach.js').StagedFile[]} [staged]  what was in the composer's tray
 */
export async function send(text, staged = []) {
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
export async function newChat(cwd) {
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
export function newChatHere() {
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

export async function refreshTaskList() {
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
export async function bootChat() {
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
