// Wizard GUI — the pane: a second view shown where the chat is. The plumbing
// (open/close, the shared header) plus the image and diff views; the subagent
// view lives with the rest of the subagent code in subagents.js.

import { imageUrl } from './api.js';
import { $, h, icon, elidedPath, fmtBytes } from './dom.js';
import { chat, autoScroll } from './transcript.js';
import { updateSubagentRow } from './subagents.js';
import { api, state } from './app.js';

/**
 * What is open in the main content area in place of the chat, if anything:
 * `{kind: 'subagent', run, flow, status, meta}` or `{kind: 'diff', path}`. Both
 * views are built by the same pane plumbing further down.
 */
export let openPane = null;

/* ------------------------------------------------------------------------ */
/* The pane: a second view where the chat is                                 */
/* ------------------------------------------------------------------------ */

/**
 * The pane's header: the one-press way back to the chat, then the view's own
 * title row, and an optional line under it.
 * @param {Array<Node|false|null>} row  what this view puts beside the back button
 * @param {Node} [sub]                  a second header line
 */
export function paneHead(row, sub) {
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
export function showPane(pane, cls, ...nodes) {
  const view = $('pane');
  view.className = `pane ${cls}`;
  view.replaceChildren(...nodes);
  $('transcript').classList.add('hidden');
  openPane = pane;
}

/** Back to the chat — the pane's DOM goes with it, so no two panes ever share
 *  a row or a tool group. */
export function closePane() {
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
export function openImagePane(image) {
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
export async function openDiffPane(file) {
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
