// Wizard GUI — slash commands: the `/` palette, and who runs what.

import { $, h } from './dom.js';
import { chat, appendSystemRow } from './transcript.js';
import { openDiffPane } from './pane.js';
import { ctx, updateProgress } from './context.js';
import { composerInput } from './composer.js';
import { SIGN_INS, openSettings } from './settings.js';
import { api, state, selectSeq, newChatHere, refreshTaskList } from './app.js';

/** The open `/` palette: `{el, list, matches, index}`, or null. */
let palette = null;

/* ------------------------------------------------------------------------ */
/* Slash commands: the palette, and who runs what                            */
/* ------------------------------------------------------------------------ */

/** The workspace's commands. Custom ones live in its `.wizard/commands/`, so
 *  the list is per-chat and is reloaded when one is opened. */
export async function loadCommands(task) {
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
export function matchCommand(text) {
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
export function runCommand({ def, args }) {
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

export function closePalette() {
  if (!palette) return;
  palette.el.remove();
  palette = null;
}

/**
 * Follow the composer: a `/` in the first column offers what it could become.
 * Once a space is typed the user is on to arguments, and the palette is done.
 */
export function syncPalette() {
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
export function paletteKeydown(e) {
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
