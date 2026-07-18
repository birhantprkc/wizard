// Wizard GUI — the context panel: git changes, the goal card, the context
// meter, the subagent list's box, and progress.

import { $, h, icon, fmtDur, fmtTokens, stateLabel } from './dom.js';
import { openDiffPane } from './pane.js';
import { renderSubagentList } from './subagents.js';
import { api, state, liveTurn, renderTopbar } from './app.js';

/* ------------------------------------------------------------------------ */
/* Context panel                                                             */
/* ------------------------------------------------------------------------ */

/** Live refs into the context panel so streams patch it in place instead of
 *  re-rendering (a rebuild would collapse the expanded changed-file list and
 *  throw away the panel's scroll position mid-turn). */
export let ctx = null;

export function renderContextPanel() {
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
  // and the two are deliberately not the same readout.
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
export function updateContextMeter() {
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

export function updateGoal() {
  if (!ctx || !state.task) return;
  ctx.goalText.textContent = state.task.title;
  ctx.goalStatus.textContent = stateLabel(state.taskState);
  ctx.goalMeta.textContent = goalMetaText();
}

export function updateProgress() {
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

let gitSeq = 0;

export async function refreshGit() {
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
