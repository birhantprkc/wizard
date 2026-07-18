// Wizard GUI — transcript building blocks: the streaming flow, the shared row
// builders (messages, tool groups, images), used by the chat and the panes.

import { imageUrl } from './api.js';
import { fileIconSvg } from './icons.js';
import { $, h, icon, fmtBytes, elidedPath } from './dom.js';
import { renderMarkdownInto, endMarkdown } from './markdown.js';
import { openImagePane } from './pane.js';

/* ------------------------------------------------------------------------ */
/* Transcript: shared row builders                                           */
/* ------------------------------------------------------------------------ */

export const transcriptInner = () => $('transcript').querySelector('.transcript-inner');

/**
 * One streaming surface: where new content lands, plus the live refs a later
 * frame patches — the message mid-stream, the tool group being aggregated,
 * the rows waiting on their result, the card an `images` frame belongs on. The
 * main chat is one flow; an open subagent pane is another, so a subagent's rows
 * never land in — or aggregate with — the parent's.
 * @param {HTMLElement} scroller the element this flow scrolls in
 */
export function newFlow(scroller) {
  return { scroller, target: null, md: null, think: null, group: null, rows: new Map(), lastTool: null };
}

/** The main chat's flow: the center transcript. */
export const chat = newFlow($('transcript'));

export function autoScroll(flow, force = false) {
  const scroller = flow.scroller;
  const nearBottom = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 180;
  if (force || nearBottom) scroller.scrollTop = scroller.scrollHeight;
}

/** Break the streaming text/thinking/tool-group flow (before a new block). */
export function breakFlow(flow) {
  endStream(flow);
  flow.group = null;
  flow.lastTool = null;
  collapseThinking(flow);
}

/** Close the assistant message being streamed, if there is one: the last delta
 *  is landed now rather than on a frame that may never come. */
export function endStream(flow) {
  if (!flow.md) return;
  endMarkdown(flow.md);
  flow.md = null;
}

export function collapseThinking(flow) {
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
export function appendPromptCard(text, attachments = []) {
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
export function appendWorkedSection(label, live = false) {
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
export function appendMessage(flow, text) {
  breakFlow(flow);
  const root = h('div', { class: 'msg-text md' });
  renderMarkdownInto(root, text);
  flow.target.append(root);
}

export function appendThinkingBlock(flow, text, collapsed) {
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

export function appendSystemRow(flow, text, cls = '') {
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
export function appendToolCall(flow, call) {
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

export function onToolResult(flow, result) {
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
export function appendImages(flow, batch) {
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
