// Wizard GUI — DOM helpers: the hyperscript element builder and the small
// formatters every module shares.

import { icons } from './icons.js';

/* ------------------------------------------------------------------------ */
/* DOM helpers                                                               */
/* ------------------------------------------------------------------------ */

export const $ = (id) => document.getElementById(id);

/** Shortcut hints follow the platform: ⌘N on macOS, Ctrl-N everywhere else. */
export const MOD_KEY = /mac/i.test(navigator.userAgentData?.platform || navigator.platform || '') ? '⌘' : 'Ctrl-';

/**
 * Hyperscript-style element builder.
 * @param {string} tag
 * @param {Object} [attrs] `class`, `dataset`, `html`, `on<event>` handlers, or plain attributes
 * @param {...(Node|string|Array|null|undefined|false)} children
 * @returns {HTMLElement}
 */
export function h(tag, attrs = {}, ...children) {
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
export const icon = (name, cls = 'icon') => h('span', { class: cls, html: icons[name] || '', 'aria-hidden': 'true' });

export const iconBtn = (name, label, onclick, cls = '') =>
  h('button', { class: `icon-btn ${cls}`.trim(), type: 'button', title: label, 'aria-label': label, onclick }, icon(name));

/** Relative age label: 2m, 42m, 5h, 2d. */
export function relAge(ts) {
  const mins = Math.max(1, Math.round((Date.now() - ts) / 60000));
  if (mins < 60) return `${mins}m`;
  const hours = Math.round(mins / 60);
  if (hours < 24) return `${hours}h`;
  return `${Math.round(hours / 24)}d`;
}

/** "42s", "3m 1s", "1h 12m". */
export function fmtDur(secs) {
  if (secs < 60) return `${secs}s`;
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  if (m < 60) return s ? `${m}m ${s}s` : `${m}m`;
  const hr = Math.floor(m / 60);
  const mm = m % 60;
  return mm ? `${hr}h ${mm}m` : `${hr}h`;
}

/** "812", "89K", "1.2M". */
export function fmtTokens(n) {
  const short = (x) => (x < 10 ? x.toFixed(1).replace(/\.0$/, '') : String(Math.round(x)));
  if (n < 1000) return String(n);
  if (n < 1e6) return `${short(n / 1000)}K`;
  return `${short(n / 1e6)}M`;
}

/** A path for a box that elides from the front (`direction: rtl`, so the file
 *  name — the part you are here for — survives). The mark pins the leading "/"
 *  of an absolute path, which the bidi algorithm otherwise carries to the far
 *  end of the line and renders as a trailing slash. */
export const elidedPath = (path) => `\u200e${path}`;

/** "512 B", "50 KB", "2.4 MB" — a file size, as a file manager writes it. */
export function fmtBytes(n) {
  if (n < 1024) return `${n} B`;
  const kb = n / 1024;
  if (kb < 1024) return `${Math.round(kb)} KB`;
  const mb = kb / 1024;
  return `${mb < 10 ? mb.toFixed(1) : Math.round(mb)} MB`;
}

export const stateLabel = (s) =>
  ({ working: 'Working…', needs_input: 'Needs input', idle: 'Idle', complete: 'Complete', failed: 'Failed', connecting: '…' })[s] || s;

/** `replaceChildren` renders a `null` child as the text "null"; `h()` drops it.
 *  Menus are built conditionally, so they go through this. */
export function fillWith(node, ...children) {
  node.replaceChildren(...children.flat(Infinity).filter(Boolean));
}
