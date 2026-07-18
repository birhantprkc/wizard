// Wizard GUI — the composer: the input, its send/stop button, the model chip,
// and the dropdown menus (model / directory / branch).

import { icons } from './icons.js';
import { $, h, icon, iconBtn, fillWith } from './dom.js';
import { chat, appendSystemRow } from './transcript.js';
import { refreshGit } from './context.js';
import { attachFiles, renderAttachTray } from './attach.js';
import { matchCommand, runCommand, closePalette, syncPalette, paletteKeydown } from './palette.js';
import { openSettings } from './settings.js';
import { api, state, send, newChat, renderTopbar } from './app.js';

/** Composer refs. */
export let composerInput = null;
let modelLabelEl = null;
export let attachTray = null;
let fileInput = null;
/** The one open dropdown (model / directory / branch), if any. */
let menuEl = null;

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

export function updateModelChip() {
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
export function activeDir() {
  return (state.task && state.task.path) || state.home.cwd;
}

/**
 * The folder chip: open a chat in another directory. A chat's working
 * directory is fixed when its session is created — it is written into the
 * session file and is where every command it has run took effect — so this
 * starts a new chat there rather than pretending to move this one.
 */
export function openDirMenu(anchor) {
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
export function openBranchMenu(anchor) {
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
export function updateSendButton() {
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

export function focusComposer() {
  if (composerInput) composerInput.focus();
}

export function renderComposer() {
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
