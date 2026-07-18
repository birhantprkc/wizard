// Wizard GUI — attachments: files staged in the composer for the next
// message, and the drop target that feeds them.

import { rememberImage } from './api.js';
import { fileIconSvg } from './icons.js';
import { $, h, icon, fmtBytes } from './dom.js';
import { attachTray } from './composer.js';
import { api, state } from './app.js';

/* ------------------------------------------------------------------------ */
/* Attachments: files staged for the next message                            */
/* ------------------------------------------------------------------------ */

/**
 * One file staged in the composer. It uploads the moment it is attached, so by
 * the time the message is sent the server has already written it and said what
 * it is; `path` is what goes out on the wire.
 * @typedef {Object} StagedFile
 * @property {string} key           Identity for the chip, before a path exists.
 * @property {string} name
 * @property {number} bytes
 * @property {string} mime
 * @property {'image'|'file'} kind  Guessed locally to draw the chip; the SERVER decides it for real.
 * @property {string|null} preview  Object URL, images only.
 * @property {string|null} path     Where the server wrote it; null until it has.
 * @property {string|null} error    Why it did not land.
 * @property {boolean} pending
 * @property {Promise<void>} upload
 */

let attachSeq = 0;

/**
 * Stage files: the chips appear at once and the uploads run behind them, so a
 * 4MB screenshot does not freeze the composer while it goes up.
 * @param {File[]} files
 */
export function attachFiles(files) {
  const task = state.task;
  if (!task || !files.length) return;
  for (const file of files) {
    const image = /^image\//.test(file.type || '');
    /** @type {StagedFile} */
    const item = {
      key: `att-${++attachSeq}`,
      name: file.name || (image ? 'pasted image.png' : 'file'),
      bytes: file.size || 0,
      mime: file.type || '',
      kind: image ? 'image' : 'file',
      preview: image ? URL.createObjectURL(file) : null,
      path: null,
      error: null,
      pending: true,
      upload: null,
    };
    item.upload = uploadStaged(task.id, file, item);
    state.attachments.push(item);
  }
  renderAttachTray();
}

/** Put one file where the server can serve it back, and take its word for what
 *  it is: the server sniffs the bytes, and the file name here is only a label. */
async function uploadStaged(taskId, file, item) {
  try {
    const [saved] = await api.upload(taskId, [file]);
    if (!saved || !saved.path) throw new Error('the server saved nothing');
    item.path = saved.path;
    item.name = saved.name || item.name;
    item.mime = saved.mime || item.mime;
    item.bytes = saved.bytes || item.bytes;
    item.kind = saved.kind === 'image' ? 'image' : 'file';
    // The bytes are already here; the thumbnail need not fetch them back.
    if (item.kind === 'image' && item.preview) rememberImage(item.path, item.preview);
  } catch (err) {
    item.error = String((err && err.message) || err);
  } finally {
    item.pending = false;
    if (state.attachments.includes(item)) renderAttachTray();
  }
}

function removeAttachment(item) {
  state.attachments = state.attachments.filter((a) => a !== item);
  renderAttachTray();
}

/** One staged file: a thumbnail for an image, its type for anything else. */
function attachChip(item) {
  const face = item.kind === 'image' && item.preview
    ? h('img', { class: 'attach-thumb', src: item.preview, alt: '' })
    : h('span', { class: 'attach-icon', html: fileIconSvg(item.name), 'aria-hidden': 'true' });
  return h('div', {
    class: 'attach-chip' + (item.pending ? ' pending' : '') + (item.error ? ' failed' : ''),
    title: item.error ? `${item.name} — ${item.error}` : `${item.name} · ${fmtBytes(item.bytes)}`,
  },
    face,
    h('span', { class: 'attach-name' }, item.name),
    h('span', { class: 'attach-size' }, item.error ? 'failed' : fmtBytes(item.bytes)),
    h('button', {
      class: 'attach-remove', type: 'button',
      title: 'Remove', 'aria-label': `Remove ${item.name}`,
      onclick: () => removeAttachment(item),
    }, icon('close')));
}

export function renderAttachTray() {
  if (!attachTray) return;
  attachTray.classList.toggle('hidden', !state.attachments.length);
  attachTray.replaceChildren(...state.attachments.map(attachChip));
}

/**
 * Files dropped on the chat — the transcript or the composer — attach to the
 * next message, exactly as the paperclip does. The document-level handlers are
 * what stop a file dropped anywhere else from navigating the page to it.
 */
export function initDropTarget() {
  const zone = $('conversation');
  const form = $('composer');
  const hasFiles = (e) => Array.from((e.dataTransfer && e.dataTransfer.types) || []).includes('Files');
  // A drag over a child fires `dragleave` on the parent: count the crossings
  // rather than trusting one leave to mean the pointer really left.
  let depth = 0;
  const leave = () => {
    depth = 0;
    form.classList.remove('dropping');
  };
  zone.addEventListener('dragenter', (e) => {
    if (!hasFiles(e)) return;
    depth += 1;
    form.classList.add('dropping');
  });
  zone.addEventListener('dragover', (e) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = 'copy';
  });
  zone.addEventListener('dragleave', (e) => {
    if (!hasFiles(e)) return;
    depth -= 1;
    if (depth <= 0) leave();
  });
  zone.addEventListener('drop', (e) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    leave();
    attachFiles(Array.from(e.dataTransfer.files || []));
  });
  for (const type of ['dragover', 'drop']) {
    document.addEventListener(type, (e) => {
      if (hasFiles(e)) e.preventDefault();
    });
  }
}
